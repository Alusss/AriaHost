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

/// 字体注册结果，面板信息区显示「字体是否来自系统」。
#[derive(Debug, Clone, Default)]
pub struct FontState {
    /// 系统字体解析结果（形如 `system:Microsoft YaHei:...`），`None` 表示没找到。
    pub resolved: Option<String>,
}

impl FontState {
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
    let request = FallbackRequest {
        cluster: WARMUP_CLUSTER,
        family: &FontFamily::Proportional,
    };
    let resolved = provider.font_for(&request).map(|insert| insert.name);
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
    FontState { resolved }
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
        };
        assert!(state.is_system_font());
        assert!(!FontState::default().is_system_font());
        assert!(!FontState {
            resolved: Some("内嵌字体".to_owned())
        }
        .is_system_font());
    }
}
