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
//! ## 逐字形链（ui-spec-v2 §2.1 / Lead 裁决 1）
//!
//! 中文不是「挑一个字体全用」而是**逐字形**：拉丁字母与数字留在 Segoe UI（字形窄、
//! 数字等宽感好），汉字自动落到 Microsoft YaHei UI。所以链就是这三段，按序探测：
//! `Segoe UI → Microsoft YaHei UI → Microsoft YaHei`。
//!
//! 不进必需链的两个候选，理由都是「本机不是系统内置」：
//! - `Noto Sans SC`：本机是 HKCU 用户级安装，换台机器就没有；
//! - `Segoe UI Variable`：Win10 没有，且变量字体的光学尺寸会让 Win10/11 字宽不一致。

use std::sync::Arc;

use egui::{FallbackRequest, FontFamily, FontProvider as _};
use egui_system_font_provider::SystemFontProvider;

use crate::logbuf::LogBuffer;

/// 字体回退 locale：简体中文（EG-5 指定值）。
pub const LOCALE: &str = "zh-CN";

/// 汉字探测字：会触发 Han 脚本的系统字体回退查询。
const HAN_CLUSTER: &str = "直";

/// 拉丁探测字：用来分辨「这一族给的是拉丁字形还是汉字字形」。
const LATIN_CLUSTER: &str = "A";

/// 字体**逐字形链**的优先顺序（ui-spec-v2 §2.1，Lead 裁决 1）。
///
/// 按此顺序逐族探测；`Segoe UI` 只出拉丁/数字，汉字由 `Microsoft YaHei UI` 起接管，
/// `Microsoft YaHei` 是非 UI 变体的兜底。**全部失败才**回落到 fontique 的默认选择。
///
/// 为什么要显式写顺序而不是全交给 fontique：默认选择在装了多套中文字体的机器上
/// 可能落到等线/宋体一类笔形差异很大的字体上，同一个界面在不同机器上字形不一致；
/// 而且**没有可复现的日志**——用户报「中文显示怪」时无法判断到底用了哪一套。
pub const CJK_CHAIN: [&str; 3] = ["Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei"];

/// 等宽链（Lead 裁决 2）：`Consolas` 打头。
///
/// `Cascadia Mono` 不进链：本机注册表 0 项、字体目录无文件，装了 Windows Terminal
/// 也不带它——写进必需链等于在多数机器上白探测一次。
pub const MONO_CHAIN: [&str; 2] = ["Consolas", "Courier New"];

/// 一条链上单个候选字体的探测结果。
#[derive(Debug, Clone)]
pub struct ChainHit {
    /// 候选字体名（[`CJK_CHAIN`] 的某一项）。
    pub family: &'static str,
    /// 该族对**拉丁**探测字的解析结果；`None` = 该族给不出拉丁字形。
    pub latin: Option<String>,
    /// 该族对**汉字**探测字的解析结果；`None` = 该族给不出汉字字形。
    pub han: Option<String>,
}

impl ChainHit {
    /// 这一族是否真的提供了汉字字形（逐字形链的核心判据）。
    pub fn provides_han(&self) -> bool {
        self.han.is_some()
    }

    /// 一行摘要：`族（拉丁=…, 汉字=…）`，未命中写「无」。
    pub fn summary(&self) -> String {
        let show = |value: &Option<String>| value.clone().unwrap_or_else(|| "无".to_owned());
        format!(
            "{}（拉丁={}, 汉字={}）",
            self.family,
            show(&self.latin),
            show(&self.han)
        )
    }
}

/// 字体注册结果，面板信息区显示「字体是否来自系统」。
#[derive(Debug, Clone, Default)]
pub struct FontState {
    /// 系统字体解析结果（形如 `system:Microsoft YaHei:...`），`None` 表示没找到。
    pub resolved: Option<String>,
    /// 逐字形链的逐项探测结果（顺序与 [`CJK_CHAIN`] 一致）。
    pub chain: Vec<ChainHit>,
}

impl FontState {
    /// 逐字形链的命中摘要（一行，日志与诊断包用）。
    ///
    /// 只陈述探测到的事实：每族分别给拉丁与汉字两条结果，未命中标「无」。
    /// 空链说明一次都没探测（非 Windows 目标），**不**写成「没有中文字体」——那是两件事。
    pub fn chain_summary(&self) -> String {
        if self.chain.is_empty() {
            return "（未探测：本目标没有系统字体枚举）".to_owned();
        }
        self.chain
            .iter()
            .map(ChainHit::summary)
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// 第一个真正给出汉字字形的族名（那才是「汉字用的是谁」）。
    pub fn han_family(&self) -> Option<&'static str> {
        self.chain
            .iter()
            .find(|hit| hit.provides_han())
            .map(|hit| hit.family)
    }

    /// 第一个真正给出拉丁字形的族名（正常情况下是 `Segoe UI`）。
    pub fn latin_family(&self) -> Option<&'static str> {
        self.chain
            .iter()
            .find(|hit| hit.latin.is_some())
            .map(|hit| hit.family)
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
    let provider = Arc::new(SystemFontProvider::new().with_locale(Some(LOCALE)));
    // 1) 逐字形链：每族分别问「拉丁」和「汉字」两个探测字，结果分开记。
    let chain = probe_chain(&provider);
    for hit in &chain {
        log.push(format!("字体链：{}", hit.summary()));
    }
    // 2) 等宽链：同一个 provider 问一遍，只落日志（渲染侧仍用 `FontFamily::Monospace`，
    //    这里给出的是「Monospace 实际会落到哪一族」这一事实）。
    let mono = probe_mono(&provider);
    log.push(format!("等宽链：{}", mono.summary()));
    // 3) 兜底：即使链上全失败，也给面板一个「默认族」的答案（与 eframe 常规路径一致）。
    let request = FallbackRequest {
        cluster: HAN_CLUSTER,
        family: &FontFamily::Proportional,
    };
    let first_hit = chain.iter().find_map(|hit| hit.han.clone());
    let resolved = first_hit.or_else(|| provider.font_for(&request).map(|insert| insert.name));
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
    let state = FontState { resolved, chain };
    // 落点只经由 `han_family`/`latin_family` 这两条**同一个判据**输出：日志与面板因此
    // 不可能各说一套（别在这里再写一遍 `iter().find(...)`）。
    log.push(format!(
        "字体链落点：拉丁→{}；汉字→{}",
        state.latin_family().unwrap_or("（未解析）"),
        state.han_family().unwrap_or("（未解析）"),
    ));
    state
}

/// 按 [`CJK_CHAIN`] 逐族探测两个探测字（纯探测，不改任何全局状态）。
fn probe_chain(provider: &SystemFontProvider) -> Vec<ChainHit> {
    CJK_CHAIN
        .iter()
        .map(|family| ChainHit {
            family,
            latin: probe_cluster(provider, family, LATIN_CLUSTER),
            han: probe_cluster(provider, family, HAN_CLUSTER),
        })
        .collect()
}

/// 等宽链的探测：`Consolas` 在不在、`Monospace` 泛族实际落到哪一族。
fn probe_mono(provider: &SystemFontProvider) -> ChainHit {
    let named = probe_cluster(provider, MONO_CHAIN[0], LATIN_CLUSTER);
    let generic = provider
        .font_for(&FallbackRequest {
            cluster: LATIN_CLUSTER,
            family: &FontFamily::Monospace,
        })
        .map(|insert| insert.name);
    ChainHit {
        family: MONO_CHAIN[0],
        latin: named.or(generic),
        han: None,
    }
}

/// 问一次「`family` 这一族有没有 `cluster` 的字形」。
fn probe_cluster(
    provider: &SystemFontProvider,
    family: &'static str,
    cluster: &'static str,
) -> Option<String> {
    let name: Arc<str> = Arc::from(family);
    provider
        .font_for(&FallbackRequest {
            cluster,
            family: &FontFamily::Name(name),
        })
        .map(|insert| insert.name)
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
        // Lead 裁决 1：逐字形链必须是这三段，且按此序。
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
    fn chain_summary_reports_latin_and_han_separately() {
        let hit = ChainHit {
            family: "Segoe UI",
            latin: Some("system:Segoe UI:0:0".to_owned()),
            han: None,
        };
        assert_eq!(
            hit.summary(),
            "Segoe UI（拉丁=system:Segoe UI:0:0, 汉字=无）"
        );
        let state = FontState {
            resolved: None,
            chain: vec![
                hit,
                ChainHit {
                    family: "Microsoft YaHei UI",
                    latin: None,
                    han: Some("system:Microsoft YaHei:0:0".to_owned()),
                },
            ],
        };
        assert_eq!(state.latin_family(), Some("Segoe UI"));
        assert_eq!(state.han_family(), Some("Microsoft YaHei UI"));
        assert!(FontState::default().chain_summary().contains("未探测"));
    }
}
