//! 渲染后端选择（M2-1 的 `--backend`）。
//!
//! 产品只发 Windows，但本机 wgpu 自选的是 Vulkan（见 task-27 的实测：`backend=Vulkan`），
//! 所以 DX12 路径一直没被机器验证过。本模块把「选哪个 wgpu 后端」变成显式开关：
//! `--backend auto|dx12|vulkan` → [`eframe::wgpu::InstanceDescriptor::backends`]。
//!
//! 语义边界：
//! - `auto` = wgpu 默认（`Backends::default()`，即全部后端），并且**尊重** `WGPU_BACKEND`
//!   环境变量（`Backends::with_env`，与 wgpu 自己 `InstanceDescriptor::default()` 的行为一致）；
//! - `dx12` / `vulkan` = **只**允许该后端。该后端在此机器上不存在时启动会失败，
//!   `--panel-selftest` 会打印 `SELFTEST failed reason=...` 并以非 0 退出——这是预期的
//!   「如实报错」，不是缺陷（见 M2-1 验收：DX12 不可用要写清楚）。

use eframe::wgpu;

/// 面板渲染后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// wgpu 自选（默认，全部后端，尊重 `WGPU_BACKEND`）。
    #[default]
    Auto,
    /// 只允许 D3D12。
    Dx12,
    /// 只允许 Vulkan。
    Vulkan,
}

impl Backend {
    /// 解析 `--backend` 的取值（大小写不敏感，两端空白容忍）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "dx12" | "d3d12" => Some(Self::Dx12),
            "vulkan" | "vk" => Some(Self::Vulkan),
            _ => None,
        }
    }

    /// 稳定的字符串（日志/测试用，与 `--backend` 的取值一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dx12 => "dx12",
            Self::Vulkan => "vulkan",
        }
    }

    /// 日志里的一行中文说明（比 `Debug` 打印后端位集更可读：后者会带 NOOP/METAL 之类的位名）。
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Auto => "wgpu 自选，并尊重 WGPU_BACKEND 环境变量",
            Self::Dx12 => "只允许 D3D12（Windows 产品主路径）",
            Self::Vulkan => "只允许 Vulkan（多数 Windows 机器上 wgpu 的自选项）",
        }
    }

    /// 映射到 wgpu 的后端位集。
    pub fn backends(self) -> wgpu::Backends {
        match self {
            // `with_env`：显式没指定时，`WGPU_BACKEND` 仍能覆盖——与 wgpu 默认行为对齐，
            // 免得「面板 auto」与「aria2 无关工具链的 wgpu auto」语义不一致。
            Self::Auto => wgpu::Backends::default().with_env(),
            Self::Dx12 => wgpu::Backends::DX12,
            Self::Vulkan => wgpu::Backends::VULKAN,
        }
    }
}

/// 单个后端的探测结论。
///
/// 两种失败要分开记日志：**后端没编进来**（本构建不具备能力）与**后端在但没有显卡**
/// 是完全不同的事故，混成一句「不可用」以后没法排障。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// 该后端可用（至少枚举到一个 adapter）。
    Ready,
    /// 这个 wgpu 构建里没有该后端。
    Unsupported,
    /// 后端在，但一个 adapter 都没有。
    NoAdapter,
}

/// `auto` 的串行探测顺序：**Vulkan → DX12 → GL**（内存优先，GL 兜底）。
///
/// 两个实测事实（同一产物 sha256=12db9ce2…ce3f30、K32 句柄口径、30 采样 / 60s / 单实例）：
/// 1. **枚举范围本身是成本**：全集 `auto` = 72.0MB，单后端显式 `vulkan` = 51.6MB，差 **20.4MB**。
///    所以 `auto` 不能一次给 `Backends::all()`，必须串行探测、只开一个。
/// 2. **后端本身是更大的一笔**：显式 `dx12` = **116.4MB**，已破 100MB 闸门（EG-4）。
///
/// 因此顺序不能按「Windows 主路径优先」把 dx12 放第一位——那样 `auto` 在有 D3D12 的
/// 机器上会直接选中 116MB 的那条路，闸门必破。**内存闸门是硬要求，DX12 只作兜底**：
/// 只有 Vulkan 不可用时才走它，并在日志里如实说明为什么退到 DX12。
/// 想改回 DX12 优先：只改这个常量的顺序即可（有单测锁语义，不锁具体顺序）。
pub const AUTO_PROBE_ORDER: [(&str, wgpu::Backends); 3] = [
    ("vulkan", wgpu::Backends::VULKAN),
    ("dx12", wgpu::Backends::DX12),
    ("gl", wgpu::Backends::GL),
];

/// 串行探测：按顺序一次只试**一个**后端，成功即返回它，失败先丢再试下一个。
///
/// `attempt` 必须「试完就把实例丢掉」——返回值里的 guard 在函数内被 `drop`，
/// 所以同一时刻最多只有一个后端被实例化。这一点由单测用 Drop 计数器证明，
/// 不靠注释保证。
/// 返回 `(胜出的后端, 可取证轨迹)`。
///
/// **轨迹**是给现场支持用的：形如 `vulkan:no_adapter>dx12:ready`，会以
/// `SELFTEST_NOTE probe=…` 打出去，也能在面板日志里逐行看到「尝试 X → 失败（原因）→ 最终 Y」。
/// 没有这条轨迹，「用户说内存高」就只能靠猜是哪个后端。
pub fn probe_in_order<P, G>(
    order: &[(&'static str, wgpu::Backends)],
    mut attempt: P,
    log: &mut Vec<String>,
) -> (Option<wgpu::Backends>, String)
where
    P: FnMut(wgpu::Backends) -> (Probe, G),
{
    let mut trace: Vec<String> = Vec::new();
    // (label, 失败原因)：回退到更贵后端时要如实说出「为什么不是第一个」。
    let mut failures: Vec<(&'static str, &'static str)> = Vec::new();

    for (label, backends) in order {
        let (probe, guard) = attempt(*backends);
        let reason = match probe {
            Probe::Ready => None,
            Probe::Unsupported => Some("本构建没编入该后端"),
            Probe::NoAdapter => Some("后端在，但枚举不到可用显卡适配器"),
        };

        let Some(reason) = reason else {
            trace.push(format!("{label}:ready"));
            log.push(format!(
                "后端探测：尝试 {label} → 可用；最终后端 = {label}（只开这一个，不走全集）"
            ));
            if !failures.is_empty() {
                let why = failures
                    .iter()
                    .map(|(l, r)| format!("{l} 不可用（{r}）"))
                    .collect::<Vec<_>>()
                    .join("；");
                log.push(format!(
                    "后端探测：注意——{why}，已回退到 {label}；DX12 实测私用工作集 116.4MB，比 Vulkan 的 51.6MB 高，内存会更高"
                ));
            }
            drop(guard);
            return (Some(*backends), trace.join(">"));
        };

        trace.push(format!("{label}:{reason}"));
        failures.push((label, reason));
        log.push(format!(
            "后端探测：尝试 {label} → 失败（{reason}）→ 试下一个"
        ));
        // 先丢实例再试下一个：同时持有两个等于把 20MB 又加回来。
        drop(guard);
    }

    log.push("后端探测：没有一个后端可用 → 退回 wgpu 默认（建议用 --backend 明确指定）".to_owned());
    (None, trace.join(">"))
}

/// 把后端位集写成中文日志（避免把 `Backends(DX12 | VULKAN)` 这种 Debug 串打到用户日志里）。
pub fn bits_label(bits: wgpu::Backends) -> String {
    if bits.is_empty() {
        return "（空集）".to_owned();
    }
    let mut names = Vec::new();
    for (bit, name) in [
        (wgpu::Backends::DX12, "dx12"),
        (wgpu::Backends::VULKAN, "vulkan"),
        (wgpu::Backends::GL, "gl"),
        (wgpu::Backends::METAL, "metal"),
        (wgpu::Backends::BROWSER_WEBGPU, "webgpu"),
    ] {
        if bits.contains(bit) {
            names.push(name);
        }
    }
    if names.is_empty() {
        format!("{bits:?}")
    } else {
        names.join(" + ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Alive(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for Alive {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn probe_stops_at_first_ready_and_never_holds_two_instances() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let alive = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut log = Vec::new();

        let chosen = {
            let (alive, peak, calls) = (alive.clone(), peak.clone(), calls.clone());
            probe_in_order(
                &[
                    ("dx12", wgpu::Backends::DX12),
                    ("vulkan", wgpu::Backends::VULKAN),
                    ("gl", wgpu::Backends::GL),
                ],
                move |_backends| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let now = alive.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // 第一个后端：假装没有显卡；第二个：成功。
                    let probe = if calls.load(Ordering::SeqCst) == 1 {
                        Probe::NoAdapter
                    } else {
                        Probe::Ready
                    };
                    (probe, Alive(alive.clone()))
                },
                &mut log,
            )
        };

        let (chosen, trace) = chosen;
        assert_eq!(chosen, Some(wgpu::Backends::VULKAN), "应落到第二个后端");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "成功之后不许继续探测");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "同一时刻最多一个实例存活（同时持有两个 = 白修那 20MB）"
        );
        assert_eq!(alive.load(Ordering::SeqCst), 0, "返回前必须把实例丢掉");
        // 轨迹要能回答「为什么不是第一个」：这是现场支持唯一的取证入口。
        assert_eq!(trace, "dx12:后端在，但枚举不到可用显卡适配器>vulkan:ready");
        assert!(
            log.iter()
                .any(|line| line.contains("尝试 dx12 → 失败")
                    && line.contains("枚举不到可用显卡适配器")),
            "失败原因要写清楚：{log:?}"
        );
        assert!(
            log.iter().any(|line| line.contains("已回退到 vulkan")),
            "回退时必须说清「为什么不是第一个 + 内存代价」：{log:?}"
        );
        assert!(
            log.iter().any(|line| line.contains("最终后端 = vulkan")),
            "必须有一行给出最终后端：{log:?}"
        );
        assert!(
            log.last().is_some_and(|line| line.contains("内存会更高")),
            "回退警示压在最末尾（最容易看到的位置）：{log:?}"
        );
    }

    #[test]
    fn probe_reports_unsupported_apart_from_no_adapter() {
        let mut log = Vec::new();
        let (chosen, trace) = probe_in_order(
            &[("dx12", wgpu::Backends::DX12)],
            |_| (Probe::Unsupported, ()),
            &mut log,
        );
        assert_eq!(chosen, None);
        assert_eq!(trace, "dx12:本构建没编入该后端");
        assert!(log[0].contains("没编入该后端"), "{log:?}");
        assert!(log[1].contains("没有一个后端可用"), "{log:?}");
    }

    #[test]
    fn parses_the_three_documented_values() {
        assert_eq!(Backend::parse("auto"), Some(Backend::Auto));
        assert_eq!(Backend::parse("dx12"), Some(Backend::Dx12));
        assert_eq!(Backend::parse("vulkan"), Some(Backend::Vulkan));
    }

    #[test]
    fn parse_is_case_insensitive_and_tolerates_spaces() {
        assert_eq!(Backend::parse(" DX12 "), Some(Backend::Dx12));
        assert_eq!(Backend::parse("D3D12"), Some(Backend::Dx12));
        assert_eq!(Backend::parse("Vk"), Some(Backend::Vulkan));
        assert_eq!(Backend::parse("AUTO"), Some(Backend::Auto));
    }

    #[test]
    fn rejects_unknown_values() {
        assert_eq!(Backend::parse(""), None);
        assert_eq!(Backend::parse("metal"), None);
        assert_eq!(Backend::parse("gl"), None);
        assert_eq!(Backend::parse("dx11"), None);
    }

    #[test]
    fn explicit_backends_are_exclusive() {
        // 显式指定必须只留一个后端位：这是「用 DX12 跑一遍」的唯一含义。
        assert_eq!(Backend::Dx12.backends(), wgpu::Backends::DX12);
        assert_eq!(Backend::Vulkan.backends(), wgpu::Backends::VULKAN);
        assert!(!Backend::Dx12.backends().contains(wgpu::Backends::VULKAN));
        assert!(!Backend::Vulkan.backends().contains(wgpu::Backends::DX12));
    }

    #[test]
    fn auto_probe_order_tries_exactly_one_backend_at_a_time() {
        // auto 不再一次给全集：每个候选必须是**单个**后端位，
        // 否则「同时实例化两个后端」会把实测约 20MB 的枚举成本加回来。
        for (label, bits) in AUTO_PROBE_ORDER {
            assert!(!label.is_empty());
            assert_eq!(
                bits.into_iter().count(),
                1,
                "{label} 必须是单个后端，实际 {bits:?}"
            );
        }
        // 顺序只锁「内存优先」这一条语义：第一个必须是 vulkan（单后端实测 51.6MB，
        // 而 DX12 是 116.4MB，已破闸门），DX12 只能作兜底。
        assert_eq!(AUTO_PROBE_ORDER[0].1, wgpu::Backends::VULKAN);
        let rest: Vec<wgpu::Backends> = AUTO_PROBE_ORDER[1..].iter().map(|(_, b)| *b).collect();
        assert!(
            rest.contains(&wgpu::Backends::DX12),
            "DX12 必须仍在兜底链里"
        );
        assert!(rest.contains(&wgpu::Backends::GL), "GL 必须兜底");
    }

    #[test]
    fn bits_label_names_the_backends_in_chinese_log_friendly_way() {
        assert_eq!(bits_label(wgpu::Backends::DX12), "dx12");
        assert_eq!(bits_label(wgpu::Backends::VULKAN), "vulkan");
        assert_eq!(
            bits_label(wgpu::Backends::DX12 | wgpu::Backends::VULKAN),
            "dx12 + vulkan"
        );
        assert_eq!(bits_label(wgpu::Backends::empty()), "（空集）");
    }

    #[test]
    fn as_str_roundtrips_with_parse() {
        for backend in [Backend::Auto, Backend::Dx12, Backend::Vulkan] {
            assert_eq!(Backend::parse(backend.as_str()), Some(backend));
        }
    }

    #[test]
    fn describe_is_chinese_and_names_the_backend() {
        assert!(Backend::Auto.describe().contains("自选"));
        assert!(Backend::Dx12.describe().contains("D3D12"));
        assert!(Backend::Vulkan.describe().contains("Vulkan"));
        for backend in [Backend::Auto, Backend::Dx12, Backend::Vulkan] {
            let text = backend.describe();
            // 不要把 wgpu 的位集 Debug 串（NOOP/METAL/…）直接打到日志里。
            assert!(!text.contains("NOOP"), "{text}");
            assert!(!text.contains("Backends("), "{text}");
        }
    }
}
