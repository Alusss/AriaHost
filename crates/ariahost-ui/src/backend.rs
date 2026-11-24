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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn auto_keeps_more_than_one_backend() {
        // auto 不能退化成单后端，否则「默认路径」等于偷偷钉死某个后端。
        let auto = Backend::Auto.backends();
        assert!(auto.contains(wgpu::Backends::DX12) || auto.contains(wgpu::Backends::VULKAN));
        assert_ne!(auto, wgpu::Backends::empty());
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
