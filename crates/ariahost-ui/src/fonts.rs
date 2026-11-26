//! 中文字体接入（EG-5 / EG-6）。
//!
//! 两条硬约束：
//! 1. 中文一律走**系统字体**，不内嵌 CJK（内嵌 = +5–10MB，直接吃掉 <100MB 的余量）；
//! 2. 必须显式 `.with_locale(Some("zh-CN"))`——`fontique` 对汉字（Han）的默认 locale
//!    经常落到日文字形，简体字会缺字或字形错（EG-5 原文）。
//!
//! eframe 0.37 的 `system_fonts` feature 会自动挂一个 `SystemFontProvider::new()`
//! （不带 locale），所以面板把 `system_font_fallback` 关掉、改在这里自己注册（见 lib.rs）。

use std::sync::Arc;

use egui::{FallbackRequest, FontFamily, FontProvider as _};
use egui_system_font_provider::SystemFontProvider;

use crate::logbuf::LogBuffer;

/// 字体回退 locale：简体中文（EG-5 指定值）。
pub const LOCALE: &str = "zh-CN";

/// 预热用的探测字：汉字（Han 脚本）会触发系统字体回退查询。
const WARMUP_CLUSTER: &str = "直";

/// 中文**逐字形链**的优先顺序（ui-spec-v2 §2.3）：按此顺序逐个探测系统字体，
/// 第一个能给出字形的就是实际来源；**全部失败才**回落到 fontique 的默认选择。
///
/// 为什么要显式写顺序而不是全交给 fontique：默认选择在装了多套中文字体的机器上
/// 可能落到等线/宋体一类笔形差异很大的字体上，同一个界面在不同机器上字形不一致；
/// 而且**没有可复现的日志**——用户报「中文显示怪」时无法判断到底用了哪一套。
pub const CJK_CHAIN: [&str; 4] = [
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "SimHei",
    "Noto Sans CJK SC",
];

/// 一条链上单个候选字体的探测结果。
#[derive(Debug, Clone)]
pub struct ChainHit {
    /// 候选字体名（[`CJK_CHAIN`] 的某一项）。
    pub family: &'static str,
    /// 解析到的实际字体（形如 `system:Microsoft YaHei:...`）；`None` = 该候选不可用。
    pub resolved: Option<String>,
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
    /// 只陈述探测到的事实：命中项写出实际解析值，未命中项标「无」。空链说明
    /// 一次都没探测（非 Windows 目标），**不**写成「没有中文字体」——那是两件事。
    pub fn chain_summary(&self) -> String {
        if self.chain.is_empty() {
            return "（未探测：本目标没有系统字体枚举）".to_owned();
        }
        self.chain
            .iter()
            .map(|hit| match &hit.resolved {
                Some(resolved) => format!("{}={resolved}", hit.family),
                None => format!("{}=无", hit.family),
            })
            .collect::<Vec<_>>()
            .join(" → ")
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
    // 1) 逐字形链：按 CJK_CHAIN 逐个候选字体问一次「这个字你有没有」。
    let chain = probe_chain(&provider, WARMUP_CLUSTER);
    let first_hit = chain.iter().find_map(|hit| hit.resolved.clone());
    let mut trail = String::new();
    for hit in &chain {
        trail.push_str(&format!(
            "{} → {}；",
            hit.family,
            hit.resolved.as_deref().unwrap_or("未解析")
        ));
    }
    log.push(format!("中文字体链（locale={LOCALE}）：{trail}"));
    // 2) 兜底：即使链上全失败，也给面板一个「默认族」的答案（与 eframe 常规路径一致）。
    let request = FallbackRequest {
        cluster: WARMUP_CLUSTER,
        family: &FontFamily::Proportional,
    };
    let resolved = first_hit.or_else(|| provider.font_for(&request).map(|insert| insert.name));
    match &resolved {
        Some(name) => log.push(format!(
            "系统字体已接入（locale={LOCALE}）：「{WARMUP_CLUSTER}」→ {name}"
        )),
        None => log.push(format!(
            "系统字体未解析到「{WARMUP_CLUSTER}」（locale={LOCALE}）：中文可能显示为方块（EG-5）"
        )),
    }
    // 注册的 provider 必须是同一个实例：预热已经让它把字体枚举跑完，避免二次枚举。
    ctx.add_font_provider(provider);
    FontState { resolved, chain }
}

/// 按 [`CJK_CHAIN`] 逐个候选字体探测一个汉字字形（纯探测，不改任何全局状态）。
fn probe_chain(provider: &SystemFontProvider, cluster: &'static str) -> Vec<ChainHit> {
    CJK_CHAIN
        .iter()
        .map(|family| {
            let name: Arc<str> = Arc::from(*family);
            let request = FallbackRequest {
                cluster,
                family: &FontFamily::Name(name),
            };
            ChainHit {
                family,
                resolved: provider.font_for(&request).map(|insert| insert.name),
            }
        })
        .collect()
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
    fn cjk_chain_prefers_yahei_ui_and_is_not_empty() {
        assert!(!CJK_CHAIN.is_empty());
        assert_eq!(CJK_CHAIN[0], "Microsoft YaHei UI");
        // 链上不许有重复项（重复只会白问一次）。
        let mut seen: Vec<&str> = CJK_CHAIN.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "CJK_CHAIN 有重复候选");
    }

    #[test]
    fn chain_hit_keeps_family_name_when_unresolved() {
        let hit = ChainHit {
            family: "SimHei",
            resolved: None,
        };
        assert_eq!(hit.family, "SimHei");
        assert!(hit.resolved.is_none());
        assert!(FontState::default().chain.is_empty());
    }
}
