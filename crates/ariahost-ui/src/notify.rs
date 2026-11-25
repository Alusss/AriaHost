//! 通知通道（M3-c）：**一处入口 + 免打扰规则 + 降级链**。
//!
//! ## 为什么入口放在 `ariahost-ui`（而不是 common/core）
//!
//! 发系统级提示必须拿到**托盘图标的身份**（`Shell_NotifyIconW` 的 `hWnd` + `uID`），
//! 而那个图标由本 crate 的 [`crate::tray`] 创建。放到 `ariahost-common` 会让底层反向依赖 UI，
//! 放到 `ariahost-core` 更糟（core 是纯逻辑 + 下载，不该知道托盘）。
//! 所以：**core/common 只产数据（`CleanReport` 等），本模块只负责「要不要打扰用户」与「怎么发」**。
//!
//! ## 降级链（架构 §7.1 裁决三 / 需求 F10）
//!
//! | 级别 | 通道 | 本批状态 |
//! |---|---|---|
//! | 1 | 面板内横幅（面板已开时优先，避免系统级打扰） | **只留判定接口**，实际接线在 `app.rs`（task-46 之后，由 Lead 排期） |
//! | 2 | 托盘气泡 `Shell_NotifyIconW` + `NIF_INFO` | **本批必做**（见 [`crate::tray::Tray::balloon`]），不需要 AUMID |
//! | 3 | Toast（`ToastNotificationManager`，需 AUMID + WinRT） | **本批不做**；需 AUMID 快捷方式 + WinRT 绑定，M4 评估 |
//!
//! 任何一级失败都**不得静默丢弃**：失败要能拿到 `false`/`Suppressed{reason}` 并落日志
//! （用户以为下载卡死，比多一条提示更糟）。
//!
//! ## 免打扰（本批硬要求）
//!
//! - 同 `kind` 冷却 [`COOLDOWN`]（30 分钟）；
//! - 同 `kind` 每天最多 [`DAILY_CAP`]（5）次，隔天（UTC 日界线）重置；
//! - [`Notifier::mute`]（面板「本次忽略」）= 本进程内该类静默直到退出。
//!
//! **配置来源的诚实说明**：定版 schema（52 键）里通知只有 `notify.on_complete`（默认 true）、
//! `notify.on_error`（默认 true）、`notify.backend`（默认 `auto`）三个键，**没有**冷却/每日上限键。
//! 所以冷却与上限在这里是常量，并通过 [`Notifier::set_policy`] 承接那两个真实存在的开关；
//! 「要不要给 schema 加 `notify.cooldown_minutes`」已上报 Lead 裁决，**本模块不自造配置键**。
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

/// 同 `kind` 的通知冷却期（配置 schema 无对应键，见模块文档）。
pub const COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// 同 `kind` 每天最多发几条（超限静默，只落日志）。
pub const DAILY_CAP: u32 = 5;

/// 通知类别。`as_str()` 的取值会出现在日志与抑制原因里，**是稳定契约**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotifyKind {
    /// 磁盘可回收空间达到打扰线（清理引擎产出 `CleanReport`）。
    Clean,
    /// 任务下载完成（默认关闭，由 `notify.on_complete` 控制）。
    DownloadDone,
    /// 服务/运行时异常（如 aria2c 反复崩溃进入 `Failed`）。
    ServiceError,
    /// 有新版本可用（M4 才接真实数据源）。
    UpdateAvailable,
}

impl NotifyKind {
    /// 稳定字符串（日志/抑制原因/契约测试都用它）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::DownloadDone => "download_done",
            Self::ServiceError => "service_error",
            Self::UpdateAvailable => "update_available",
        }
    }
}

/// 通知的语气（映射到气泡的 `dwInfoFlags`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyLevel {
    Info,
    Warn,
    Error,
}

impl NotifyLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// 一条待发通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotifyKind,
    pub level: NotifyLevel,
    pub title: String,
    pub body: String,
}

impl Notification {
    /// 便捷构造（标题/正文都是给人看的中文，不放错误码）。
    pub fn new(
        kind: NotifyKind,
        level: NotifyLevel,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            level,
            title: title.into(),
            body: body.into(),
        }
    }
}

/// 为什么没发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    /// 该类在配置里被关掉（`notify.on_complete` / `notify.on_error`）。
    Disabled,
    /// 用户点了「本次忽略」。
    Muted,
    /// 冷却期内。
    Cooldown,
    /// 今天的次数用完了。
    DailyCap,
}

impl SuppressReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Muted => "muted",
            Self::Cooldown => "cooldown",
            Self::DailyCap => "daily-cap",
        }
    }
}

/// 一次投递的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// 已进队列，等 UI 线程发出去。
    Queued,
    /// 被免打扰规则拦下（**不是错误**，只落日志）。
    Suppressed {
        kind: NotifyKind,
        reason: SuppressReason,
    },
}

impl NotifyOutcome {
    /// 抑制时的日志行（验收要的固定格式：`notify suppressed kind=clean reason=cooldown`）。
    pub fn log_line(self) -> Option<String> {
        match self {
            Self::Queued => None,
            Self::Suppressed { kind, reason } => Some(format!(
                "notify suppressed kind={} reason={}",
                kind.as_str(),
                reason.as_str()
            )),
        }
    }
}

/// 计数（自检/诊断用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NotifyStats {
    pub queued: u32,
    pub suppressed: u32,
}

/// 通知中心：只做**判定 + 排队**，不发系统调用（发由 UI 线程做）。
///
/// 这样拆分的原因：判定逻辑必须能在 Linux/CI 上单测（托盘与 `Shell_NotifyIconW` 只在
/// Windows 上存在），而 `Shell_NotifyIconW` 又只能由持有托盘图标的那条线程发。
pub struct Notifier {
    /// 真实存在的两个配置开关（`notify.on_complete` / `notify.on_error`）。
    on_complete: bool,
    on_error: bool,
    /// 每个类别上次**真的**发出去的时间。
    last: HashMap<NotifyKind, SystemTime>,
    /// 每个类别当日计数 + 当日编号（UTC 天）。
    today: HashMap<NotifyKind, (u64, u32)>,
    /// 面板里点过「本次忽略」的类别。
    muted: HashSet<NotifyKind>,
    /// 待发队列（UI 线程每帧 `drain` 一次）。
    outbox: Vec<Notification>,
    stats: NotifyStats,
}

impl Notifier {
    /// 默认策略 = 架构 §4.11 的默认值（`on_complete=true`、`on_error=true`）。
    pub fn new() -> Self {
        Self {
            on_complete: true,
            on_error: true,
            last: HashMap::new(),
            today: HashMap::new(),
            muted: HashSet::new(),
            outbox: Vec::new(),
            stats: NotifyStats::default(),
        }
    }

    /// 承接配置：`notify.on_complete` / `notify.on_error`（**真实键名**，见模块文档）。
    pub fn set_policy(&mut self, on_complete: bool, on_error: bool) {
        self.on_complete = on_complete;
        self.on_error = on_error;
    }

    /// 「本次忽略」：该类在本进程内静默。
    pub fn mute(&mut self, kind: NotifyKind) {
        self.muted.insert(kind);
    }

    /// 取消静默（面板里再点一次）。
    pub fn unmute(&mut self, kind: NotifyKind) {
        self.muted.remove(&kind);
    }

    pub fn is_muted(&self, kind: NotifyKind) -> bool {
        self.muted.contains(&kind)
    }

    pub fn stats(&self) -> NotifyStats {
        self.stats
    }

    /// 投递（用当前时钟）。
    pub fn offer(&mut self, notice: Notification) -> NotifyOutcome {
        self.offer_at(notice, SystemTime::now())
    }

    /// 投递（显式时钟，单测用；生产走 [`Self::offer`]）。
    pub fn offer_at(&mut self, notice: Notification, now: SystemTime) -> NotifyOutcome {
        let kind = notice.kind;
        if let Some(reason) = self.gate(kind, now) {
            self.stats.suppressed += 1;
            return NotifyOutcome::Suppressed { kind, reason };
        }
        let day = day_index(now);
        let entry = self.today.entry(kind).or_insert((day, 0));
        if entry.0 != day {
            *entry = (day, 0);
        }
        entry.1 += 1;
        self.last.insert(kind, now);
        self.outbox.push(notice);
        self.stats.queued += 1;
        NotifyOutcome::Queued
    }

    /// 判定是否该拦（不产生副作用）。
    fn gate(&self, kind: NotifyKind, now: SystemTime) -> Option<SuppressReason> {
        if !self.enabled(kind) {
            return Some(SuppressReason::Disabled);
        }
        if self.muted.contains(&kind) {
            return Some(SuppressReason::Muted);
        }
        if let Some(last) = self.last.get(&kind) {
            let elapsed = now.duration_since(*last).unwrap_or(Duration::ZERO);
            if elapsed < COOLDOWN {
                return Some(SuppressReason::Cooldown);
            }
        }
        if let Some((day, count)) = self.today.get(&kind).copied() {
            if day == day_index(now) && count >= DAILY_CAP {
                return Some(SuppressReason::DailyCap);
            }
        }
        None
    }

    /// 配置门：只有 `download_done` / `service_error` 有关闭开关（其余类别恒开）。
    fn enabled(&self, kind: NotifyKind) -> bool {
        match kind {
            NotifyKind::DownloadDone => self.on_complete,
            NotifyKind::ServiceError => self.on_error,
            NotifyKind::Clean | NotifyKind::UpdateAvailable => true,
        }
    }

    /// UI 线程取走待发通知（取完即空）。
    pub fn drain(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.outbox)
    }

    /// 队列长度（面板内横幅判定会用到）。
    pub fn pending(&self) -> usize {
        self.outbox.len()
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}

/// UTC 天编号（`SystemTime` → 自 1970-01-01 起的整天数）。只用来做「每天」的桶，不涉及时区。
fn day_index(now: SystemTime) -> u64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() / 86_400)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Notification {
        Notification::new(
            NotifyKind::Clean,
            NotifyLevel::Info,
            "可回收 820 MB",
            "点此查看",
        )
    }

    fn at(minutes: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(minutes * 60)
    }

    #[test]
    fn first_notice_is_queued_and_drained_once() {
        let mut notifier = Notifier::new();
        assert_eq!(notifier.offer_at(clean(), at(0)), NotifyOutcome::Queued);
        assert_eq!(notifier.pending(), 1);
        let drained = notifier.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].title, "可回收 820 MB");
        assert_eq!(notifier.pending(), 0, "取走即空");
        assert_eq!(notifier.stats().queued, 1);
    }

    #[test]
    fn cooldown_blocks_second_notice_of_same_kind() {
        let mut notifier = Notifier::new();
        assert_eq!(notifier.offer_at(clean(), at(0)), NotifyOutcome::Queued);
        // 29 分钟：还在冷却期
        assert_eq!(
            notifier.offer_at(clean(), at(29)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::Cooldown
            }
        );
        // 30 分钟整：放行（边界写死，别让实现飘）
        assert_eq!(notifier.offer_at(clean(), at(30)), NotifyOutcome::Queued);
        assert_eq!(notifier.stats().suppressed, 1);
        assert_eq!(notifier.stats().queued, 2);
    }

    #[test]
    fn cooldown_is_per_kind() {
        let mut notifier = Notifier::new();
        assert_eq!(notifier.offer_at(clean(), at(0)), NotifyOutcome::Queued);
        let error = Notification::new(
            NotifyKind::ServiceError,
            NotifyLevel::Error,
            "下载服务已停止",
            "AH-E5001",
        );
        assert_eq!(
            notifier.offer_at(error, at(1)),
            NotifyOutcome::Queued,
            "别的类别不该被 clean 的冷却牵连"
        );
    }

    #[test]
    fn daily_cap_blocks_sixth_notice_then_resets_next_day() {
        let mut notifier = Notifier::new();
        // 每次错开 31 分钟，避开冷却（5 条需要 4×31 = 124 分钟）
        for index in 0..DAILY_CAP as u64 {
            let outcome = notifier.offer_at(clean(), at(index * 31));
            assert_eq!(outcome, NotifyOutcome::Queued, "第 {index} 条应放行");
        }
        let sixth = notifier.offer_at(clean(), at(DAILY_CAP as u64 * 31));
        assert_eq!(
            sixth,
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::DailyCap
            }
        );
        // 第二天（UTC 天界）重置
        let next_day = at(24 * 60);
        assert_eq!(
            notifier.offer_at(clean(), next_day),
            NotifyOutcome::Queued,
            "隔天要重置计数"
        );
    }

    #[test]
    fn muted_kind_is_silent_until_unmuted() {
        let mut notifier = Notifier::new();
        notifier.mute(NotifyKind::Clean);
        assert!(notifier.is_muted(NotifyKind::Clean));
        assert_eq!(
            notifier.offer_at(clean(), at(0)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::Muted
            }
        );
        notifier.unmute(NotifyKind::Clean);
        assert_eq!(notifier.offer_at(clean(), at(0)), NotifyOutcome::Queued);
    }

    #[test]
    fn policy_switches_follow_real_config_keys() {
        let mut notifier = Notifier::new();
        notifier.set_policy(false, false);
        let done = Notification::new(
            NotifyKind::DownloadDone,
            NotifyLevel::Info,
            "下载完成",
            "ubuntu.iso",
        );
        assert_eq!(
            notifier.offer_at(done, at(0)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::DownloadDone,
                reason: SuppressReason::Disabled
            },
            "notify.on_complete=false 必须真的关掉下载完成通知"
        );
        let error = Notification::new(
            NotifyKind::ServiceError,
            NotifyLevel::Error,
            "服务已停止",
            "",
        );
        assert!(matches!(
            notifier.offer_at(error, at(0)),
            NotifyOutcome::Suppressed {
                reason: SuppressReason::Disabled,
                ..
            }
        ));
        // clean 恒开（schema 里没有开关）
        assert_eq!(notifier.offer_at(clean(), at(0)), NotifyOutcome::Queued);
    }

    #[test]
    fn download_done_defaults_on_but_is_the_quietest_kind() {
        // 默认 on_complete=true（架构 §4.11 第 40 行），但它是「默认不打扰」的候选：
        // 关闭它只需配置一键，本测确认默认值与键语义没写反。
        let mut notifier = Notifier::new();
        let done = Notification::new(
            NotifyKind::DownloadDone,
            NotifyLevel::Info,
            "下载完成",
            "a.bin",
        );
        assert_eq!(notifier.offer_at(done, at(0)), NotifyOutcome::Queued);
    }

    #[test]
    fn suppress_log_line_is_pinned() {
        assert_eq!(
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::Cooldown
            }
            .log_line()
            .as_deref(),
            Some("notify suppressed kind=clean reason=cooldown")
        );
        assert_eq!(NotifyOutcome::Queued.log_line(), None);
    }

    #[test]
    fn kind_and_reason_strings_are_pinned() {
        assert_eq!(NotifyKind::Clean.as_str(), "clean");
        assert_eq!(NotifyKind::DownloadDone.as_str(), "download_done");
        assert_eq!(NotifyKind::ServiceError.as_str(), "service_error");
        assert_eq!(NotifyKind::UpdateAvailable.as_str(), "update_available");
        assert_eq!(SuppressReason::Disabled.as_str(), "disabled");
        assert_eq!(SuppressReason::Muted.as_str(), "muted");
        assert_eq!(SuppressReason::Cooldown.as_str(), "cooldown");
        assert_eq!(SuppressReason::DailyCap.as_str(), "daily-cap");
        assert_eq!(NotifyLevel::Info.as_str(), "info");
        assert_eq!(NotifyLevel::Warn.as_str(), "warn");
        assert_eq!(NotifyLevel::Error.as_str(), "error");
    }

    #[test]
    fn cooldown_length_is_thirty_minutes() {
        assert_eq!(COOLDOWN, Duration::from_secs(1800));
        assert_eq!(DAILY_CAP, 5);
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let mut notifier = Notifier::new();
        assert_eq!(notifier.offer_at(clean(), at(120)), NotifyOutcome::Queued);
        // 时钟被拨回：按 0 处理（视作刚发过），不能 panic、不能放行成重复打扰
        assert_eq!(
            notifier.offer_at(clean(), at(0)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::Cooldown
            }
        );
    }
}
