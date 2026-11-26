//! 中文字体接入（EG-5 / EG-6）。
//!
//! 两条硬约束：
//! 1. 中文一律走**系统字体**，不内嵌 CJK（内嵌 = +5–10MB，直接吃掉 <100MB 的余量）；
//! 2. 必须显式 `.with_locale(Some("zh-CN"))`——`fontique` 对汉字（Han）的默认 locale
//!    经常落到日文字形，简体字会缺字或字形错（EG-5 原文）。
//!
//! eframe 0.37 的 `system_fonts` feature 会自动挂一个 `SystemFontProvider::new()`
//! （不带 locale），所以面板把 `system_font_fallback` 关掉、改在这里自己注册（见 lib.rs）。
//!
//! ## 逐字形：谁在供应字形（2026-11-26 实测纠正）
//!
//! 原本这里按 [`CJK_CHAIN`] 逐族提问，得到「每族给不给汉字」，再据此报「汉字用的是
//! 微软雅黑 UI」。**那是错的**：`egui_system_font_provider` 的 `font_for` 把
//! `FontFamily::Name(_)` 直接映射成 `GenericFamily::SansSerif`，再叠加脚本回退
//! （`egui_system_font_provider-0.37.0/src/lib.rs:115`），**命名族本身不参与选择**。
//! 于是逐族提问得到的答案必然相同——名字是我们自己传进去的，属于**回声式假证据**。
//!
//! 由此还有一条更硬的结论：在本 provider 上，`CJK_CHAIN` **接不了线**，它只影响
//! 「我们问谁」，不影响渲染。真正决定字形的是 `locale=zh-CN` + fontique 的默认选择。
//! （要按族名接线只能另走 `FontInsert`/`FontPriority` 显式注入字体数据，那是另一批活，
//! 不在本文件假装已实现。）
//!
//! 所以现在只报**系统真正给出的族名**（`insert.name`，形如 `system:Microsoft YaHei:400:0`）；
//! 拿不到就是 `None`，展示层写 `miss`，**绝不回填我们请求的名字**。

use egui::{FallbackRequest, FontFamily, FontProvider as _};
use egui_system_font_provider::SystemFontProvider;

use crate::logbuf::LogBuffer;

/// 字体回退 locale：简体中文（EG-5 指定值）。
pub const LOCALE: &str = "zh-CN";

/// 汉字探测字：会触发 Han 脚本的系统字体回退查询。
const HAN_CLUSTER: &str = "直";

/// 拉丁探测字：用来分辨「这一次给的是拉丁字形还是汉字字形」。
const LATIN_CLUSTER: &str = "A";

/// 字体偏好的声明顺序（ui-spec-v2 §2.1，Lead 裁决 1）。
///
/// **注意**：这只是一份「我们希望的顺序」，在本 provider 上**不产生选择效果**
/// （见模块文档：命名族被忽略）。它留着有两个用途：一是记录设计意图与排除理由，
/// 二是一旦将来换成能按族名接线的方案（`FontInsert`），顺序就是现成的输入。
///
/// 排除 `Noto Sans SC`（用户级安装，换机就没有）与 `Segoe UI Variable`
/// （Win10 没有，变量字体光学尺寸会让 Win10/11 字宽不一致）的理由不变。
pub const CJK_CHAIN: [&str; 3] = ["Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"];

/// 等宽链（Lead 裁决 2）：`Consolas` 打头。
///
/// `Cascadia Mono` 不进链：本机注册表 0 项、字体目录无文件，装了 Windows Terminal
/// 也不带它——写进必需链等于在多数机器上白探测一次。
pub const MONO_CHAIN: [&str; 2] = ["Consolas", "Courier New"];

/// 系统**实际**用哪一族供应这三类字形。
///
/// 全部 `None` = 查询没给出结果（非 Windows 目标，或 provider 未就绪），
/// 展示层必须写 `miss`；**不要**用它回填请求过的族名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Supply {
    /// 拉丁探测字（`A`）落到的字体。
    pub latin: Option<String>,
    /// 汉字探测字（`直`）落到的字体。
    pub han: Option<String>,
    /// `Monospace` 泛族落到的字体（「等宽用的是谁」）。
    pub mono: Option<String>,
}

/// 字体注册结果，面板信息区显示「字体是否来自系统」与「实际供应族」。
#[derive(Debug, Clone, Default)]
pub struct FontState {
    /// 系统字体解析结果（形如 `system:Microsoft YaHei:400:0`），`None` 表示没找到。
    pub resolved: Option<String>,
    /// 三类字形的实际供应族。
    pub supply: Supply,
}

impl FontState {
    /// 一行摘要，日志 / 信息区 / 诊断包共用同一份措辞。
    ///
    /// 空结果写「未探测」——那是「我们没问到」，**不等于**「这台机器没有中文字体」。
    pub fn supply_summary(&self) -> String {
        if self.supply == Supply::default() {
            return "（未探测：本目标没有系统字体枚举）".to_owned();
        }
        let show = |value: &Option<String>| match value {
            Some(name) => name.clone(),
            None => "miss".to_owned(),
        };
        format!(
            "实际供应 拉丁={} · 汉字={} · 等宽={}",
            show(&self.supply.latin),
            show(&self.supply.han),
            show(&self.supply.mono)
        )
    }

    /// 真正给出拉丁字形的族（系统返回的名字，不是我们请求的名字）。
    pub fn latin_supplier(&self) -> Option<&str> {
        self.supply.latin.as_deref()
    }

    /// 真正给出汉字字形的族（系统返回的名字，不是我们请求的名字）。
    pub fn han_supplier(&self) -> Option<&str> {
        self.supply.han.as_deref()
    }

    /// 中文是否确实由系统字体提供（EG-5 的可见证据）。
    pub fn is_system_font(&self) -> bool {
        self.resolved
            .as_deref()
            .is_some_and(|name| name.starts_with("system:"))
    }
}

/// 首帧前注册系统字体 provider，并做一次预热查询。
///
/// 为什么预热（EG-6）：`SystemFontProvider::new()` 在后台线程枚举系统字体（可能数百毫秒），
/// 未完成时的查询会**阻塞**。提前查一次能让枚举在首帧前结束，避免第一帧卡顿；
/// 同时也顺手拿到「系统字体是否可用」这个事实给面板显示。
pub fn install(ctx: &egui::Context, log: &LogBuffer) -> FontState {
    // 必须包 `Arc`：`Context::add_font_provider` 收的是 `Arc<dyn FontProvider>`，
    // 而探测要在注册前对**同一个实例**做完（预热，EG-6），否则会二次枚举系统字体。
    let provider = std::sync::Arc::new(SystemFontProvider::new().with_locale(Some(LOCALE)));
    // 1) 三类字形各问一次：问到的是系统**实际**给我们的族名。
    let supply = probe_supply(&provider);
    log.push(format!("字体{}", supply_line(&supply)));
    // 同时把「声明顺序」写进日志：它不影响选择（见模块文档），但用户报「中文显示怪」
    // 时需要知道我们**希望**用哪几族，以及等宽链打头的是谁。写进日志也让这两个常量
    // 在非测试构建里真的被用到，不至于变成只能靠注释解释的死代码。
    log.push(format!(
        "字体声明顺序（仅记录意图，本 provider 不按族名选择）：汉字 {}；等宽 {}",
        CJK_CHAIN.join(" → "),
        MONO_CHAIN.join(" → "),
    ));
    // 2) 兜底：连汉字都没问到，也要给面板一个「默认族」的答案（与 eframe 常规路径一致）。
    let request = FallbackRequest {
        cluster: HAN_CLUSTER,
        family: &FontFamily::Proportional,
    };
    let resolved = supply
        .han
        .clone()
        .or_else(|| provider.font_for(&request).map(|insert| insert.name));
    match &resolved {
        Some(name) => log.push(format!(
            "系统字体已接入（locale={LOCALE}）：「{HAN_CLUSTER}」→ {name}"
        )),
        None => log.push(format!(
            "系统字体未解析到「{HAN_CLUSTER}」（locale={LOCALE}）：中文可能显示为方块（EG-5）"
        )),
    }
    // 注册的 provider 必须是同一个实例：预热已经让它把字体枚举跑完，避免二次枚举。
    ctx.add_font_provider(provider);
    let state = FontState {
        resolved,
        supply: supply.clone(),
    };
    // 落点只经由 `latin_supplier`/`han_supplier` 这两条**同一个判据**输出：日志与面板
    // 因此不可能各说一套（别在这里再写一遍 `map(...)`）。
    log.push(format!(
        "字体落点：拉丁→{}；汉字→{}",
        state.latin_supplier().unwrap_or("（未解析）"),
        state.han_supplier().unwrap_or("（未解析）"),
    ));
    state
}

/// 日志用的单行摘要（拉丁 / 汉字 / 等宽，未命中写 `miss`）。
fn supply_line(supply: &Supply) -> String {
    let show = |value: &Option<String>| match value {
        Some(name) => name.clone(),
        None => "miss".to_owned(),
    };
    format!(
        "实际供应：拉丁={} 汉字={} 等宽={}",
        show(&supply.latin),
        show(&supply.han),
        show(&supply.mono)
    )
}

/// 问系统「这三类字形实际用谁」（纯探测，不改任何全局状态）。
///
/// 只问**泛族**（`Proportional` / `Monospace`）——这正是渲染时 egui 真正会传的族；
/// 用 `FontFamily::Name(_)` 问出来的是同一个答案，问三遍只会制造「逐族命中」的错觉。
fn probe_supply(provider: &SystemFontProvider) -> Supply {
    let ask = |cluster: &'static str, family: &FontFamily| {
        provider
            .font_for(&FallbackRequest { cluster, family })
            .map(|insert| insert.name)
    };
    Supply {
        latin: ask(LATIN_CLUSTER, &FontFamily::Proportional),
        han: ask(HAN_CLUSTER, &FontFamily::Proportional),
        mono: ask(LATIN_CLUSTER, &FontFamily::Monospace),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_is_simplified_chinese() {
        assert_eq!(LOCALE, "zh-CN");
    }

    #[test]
    fn system_font_detection_requires_system_prefix() {
        let state = FontState {
            resolved: Some("system:Microsoft YaHei:0:0".to_owned()),
            ..FontState::default()
        };
        assert!(state.is_system_font());
        assert!(!FontState::default().is_system_font());
        assert!(!FontState {
            resolved: Some("内嵌字体".to_owned()),
            ..FontState::default()
        }
        .is_system_font());
    }

    #[test]
    fn cjk_chain_is_the_three_name_glyph_chain() {
        // Lead 裁决 1：声明顺序必须是这三段，且按此序。
        assert_eq!(
            CJK_CHAIN,
            ["Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"]
        );
        // 不进必需链的两个候选，写死断言防止有人「顺手加回来」。
        assert!(!CJK_CHAIN.contains(&"Noto Sans CJK SC"));
        assert!(!CJK_CHAIN.iter().any(|name| name.contains("Variable")));
        let mut seen: Vec<&str> = CJK_CHAIN.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "CJK_CHAIN 有重复候选");
    }

    #[test]
    fn mono_chain_starts_with_consolas() {
        // Lead 裁决 2：Consolas 打头，Cascadia Mono 不在链上。
        assert_eq!(MONO_CHAIN[0], "Consolas");
        assert!(!MONO_CHAIN.contains(&"Cascadia Mono"));
    }

    #[test]
    fn summary_never_echoes_the_requested_family() {
        // 这是本文件最重要的一条回归：曾经 `zh` 显示成「Segoe UI」，
        // 而 Segoe UI 根本不含汉字——名字来自我们自己的提问，不是系统事实。
        let state = FontState {
            resolved: Some("system:Microsoft YaHei:400:0".to_owned()),
            supply: Supply {
                latin: Some("system:Segoe UI:400:0".to_owned()),
                han: Some("system:Microsoft YaHei:400:0".to_owned()),
                mono: None,
            },
        };
        let text = state.supply_summary();
        assert!(text.contains("汉字=system:Microsoft YaHei:400:0"), "{text}");
        // 汉字那一段绝不能是我们请求过的族名。
        assert!(!text.contains("汉字=Segoe UI"), "{text}");
        // 未命中的档位必须显式写 miss，而不是留空。
        assert!(text.contains("等宽=miss"), "{text}");
        assert_eq!(state.han_supplier(), Some("system:Microsoft YaHei:400:0"));
        assert_eq!(state.latin_supplier(), Some("system:Segoe UI:400:0"));
    }

    #[test]
    fn empty_supply_says_not_probed() {
        assert!(FontState::default().supply_summary().contains("未探测"));
        assert_eq!(FontState::default().han_supplier(), None);
    }
}
