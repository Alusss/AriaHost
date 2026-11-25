//! 状态区的最小视图模型（徽章 + 语气 + 错误码 + 主文案 + 详情 + 下一步）。
//!
//! M2-1 时它长在 `worker.rs` 里（那时只有 aria2 RPC 与服务两条线）；
//! M2 起 `core_host`（本进程托管的 aria2c）也要产出同一形状，所以抽成独立模块，
//! 避免「core_host 反过来 import worker」这种方向倒置。
//!
//! 硬约束：**这里永远不出现 aria2/SCM 的原始输出**（`RpcCallError::detail` 是 IO 原文，
//! 可能含 JSON 与栈信息）。`code` 只放既有 `AH-E####`，没有合适码时是 `None`——
//! 宁可不显示码，也不编一个。

/// 状态区的语气（决定颜色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// 一切正常。
    Good,
    /// 需要用户注意，但不算错误（未安装核心、未安装服务）。
    Warn,
    /// 明确失败。
    Bad,
    /// 还没拿到数据 / 空闲。
    Idle,
}

/// 状态区的一块（badge + 主文案 + 详情 + 下一步）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusView {
    /// 短状态词（「运行中」「未连接」…）。
    pub badge: String,
    /// 语气。
    pub tone: Tone,
    /// 既有错误码（正常状态为 `None`）。
    pub code: Option<&'static str>,
    /// 主文案。
    pub headline: String,
    /// 详情行。
    pub details: Vec<String>,
    /// 下一步提示（失败语境下必须有可执行建议）。
    pub next_step: Option<String>,
}

impl StatusView {
    /// 「还没数据」的占位视图。
    pub fn idle(badge: &str, headline: String) -> Self {
        Self {
            badge: badge.to_owned(),
            tone: Tone::Idle,
            code: None,
            headline,
            details: Vec::new(),
            next_step: None,
        }
    }
}

/// 语气 → 颜色（深色主题下也够对比）。
pub fn tone_color(tone: Tone) -> egui::Color32 {
    use egui::Color32;
    match tone {
        Tone::Good => Color32::from_rgb(110, 205, 130),
        Tone::Warn => Color32::from_rgb(240, 190, 90),
        Tone::Bad => Color32::from_rgb(235, 110, 110),
        Tone::Idle => Color32::from_rgb(170, 170, 170),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_view_is_neutral_and_codeless() {
        let view = StatusView::idle("等待", "正在读取…".to_owned());
        assert_eq!(view.tone, Tone::Idle);
        assert!(view.code.is_none());
        assert!(view.details.is_empty());
        assert!(view.next_step.is_none());
    }

    #[test]
    fn tones_map_to_distinct_colors() {
        let tones = [Tone::Good, Tone::Warn, Tone::Bad, Tone::Idle];
        let mut colors = Vec::new();
        for tone in tones {
            colors.push(tone_color(tone));
        }
        for (index, color) in colors.iter().enumerate() {
            assert!(
                !colors[..index].contains(color),
                "第 {index} 个语气与前面的颜色重复了"
            );
        }
    }
}
