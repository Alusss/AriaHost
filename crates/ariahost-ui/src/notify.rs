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
//! - 同 `kind` **24 小时滚动窗口**内最多 [`RATE_CAP`]（5）条——**不是** UTC 日历日
//!   （Lead 裁决 2026-11-25：用户理解的是「最近这段时间别烦我」，UTC+8 下按日历日会在
//!   早 8 点前后各刷 5 条，与「每天」文案对不上；滚动窗口零依赖、零时区语义）；
//! - [`Notifier::mute`]（面板「本次忽略」）= 本进程内该类静默直到退出。
//!
//! **配置来源的诚实说明**：定版 schema（52 键）里通知只有 `notify.on_complete`（默认 true）、
//! `notify.on_error`（默认 true）、`notify.backend`（默认 `auto`）三个键，**没有**冷却/上限键
//! （Lead 裁决：不加键，走常量；M4 评估是否开键）。所以冷却是常量，频率上限是常量，
//! 真实存在的两个开关由 [`Notifier::set_policy`] 承接——**本模块不自造配置键**。
//!
//! ## 通道可用性（给关闭逻辑用）
//!
//! [`ChannelAvailability`] 是「气泡到底能不能发」的可查询状态：`tray-icon` 把
//! `Shell_NotifyIconW(NIM_ADD)` 失败当成功（本仓实测：`build()` 返回 `Ok` 而托盘图标并不存在），
//! 所以关闭逻辑**不能**假设「藏进托盘」一定能找回来——气泡通道不可用时要降级为
//! 最小化到任务栏 / 提示后退出（M3-d 待办，本模块只提供判据）。
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

/// 同 `kind` 的通知冷却期（配置 schema 无对应键，见模块文档）。
pub const COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// 频率上限的窗口长度：**滚动 24 小时**（不是日历日）。
pub const RATE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// 同一 `kind` 在 [`RATE_WINDOW`] 内最多发几条（超限静默，只落日志）。
pub const RATE_CAP: usize = 5;

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
    /// 滚动 24 小时窗口内的次数用完了。
    WindowCap,
}

impl SuppressReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Muted => "muted",
            Self::Cooldown => "cooldown",
            Self::WindowCap => "window-cap",
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

/// 通知通道的可用性（**给关闭逻辑用**，见模块文档最后一节）。
///
/// 为什么要这个类型：`tray-icon` 不把 `NIM_ADD` 失败当错误，所以「托盘建好了」
/// 与「托盘真的在」是两件事。`ui.close_action=tray` 的「藏进托盘」必须排在
/// `balloon == true` 之后，否则用户会连图标都找不到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelAvailability {
    /// 托盘气泡是否可用（能拿到图标身份**且** `NIM_MODIFY` 被接受过）。
    balloon: bool,
    /// 不可用的原因（可用时为 `None`）：落日志与面板告警要能说清「为什么没通知」。
    reason: Option<&'static str>,
}

impl ChannelAvailability {
    /// 什么都不通且**说不出原因**（兜底；优先用 [`Self::unavailable`]）。
    pub const fn none() -> Self {
        Self::unavailable("no-channel")
    }

    /// 不可用 + 具体原因（`not-windows` / `tray-missing` / `shell-refused` …）。
    pub const fn unavailable(reason: &'static str) -> Self {
        Self {
            balloon: false,
            reason: Some(reason),
        }
    }

    /// 气泡可用。
    pub const fn balloon() -> Self {
        Self {
            balloon: true,
            reason: None,
        }
    }

    pub const fn is_balloon(self) -> bool {
        self.balloon
    }

    /// 不可用的原因；可用时为 `None`。
    pub const fn reason(self) -> Option<&'static str> {
        self.reason
    }

    /// 稳定字符串（日志/面板告警用）：`balloon` 或 `none`。
    pub const fn summary(self) -> &'static str {
        if self.balloon {
            "balloon"
        } else {
            "none"
        }
    }
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
    /// 每个类别在滚动窗口内**真的**发出去的时间戳（旧的会被裁掉）。
    recent: HashMap<NotifyKind, Vec<SystemTime>>,
    /// 面板里点过「本次忽略」的类别。
    muted: HashSet<NotifyKind>,
    /// 通道可用性（由 tray 层写入，关闭逻辑读）。
    channel: ChannelAvailability,
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
            recent: HashMap::new(),
            muted: HashSet::new(),
            channel: ChannelAvailability::none(),
            outbox: Vec::new(),
            stats: NotifyStats::default(),
        }
    }

    /// 承接配置：`notify.on_complete` / `notify.on_error`（**真实键名**，见模块文档）。
    pub fn set_policy(&mut self, on_complete: bool, on_error: bool) {
        self.on_complete = on_complete;
        self.on_error = on_error;
    }

    /// 写入通道可用性（托盘建成/失败时由 UI 侧调用）。
    pub fn set_channel(&mut self, channel: ChannelAvailability) {
        self.channel = channel;
    }

    /// 当前通道可用性（关闭逻辑：气泡不可用时不许「藏进托盘」）。
    pub fn channel(&self) -> ChannelAvailability {
        self.channel
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
        self.last.insert(kind, now);
        let window = self.recent.entry(kind).or_default();
        prune_window(window, now);
        window.push(now);
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
        if let Some(window) = self.recent.get(&kind) {
            // 窗口内还留着几条，就是「最近 24 小时真的发过几条」。
            let in_window = window
                .iter()
                .filter(|sent| within_window(**sent, now))
                .count();
            if in_window >= RATE_CAP {
                return Some(SuppressReason::WindowCap);
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

/// 该时间戳是否还在「现在往前 [`RATE_WINDOW`]」的窗口里。
///
/// 时钟回拨时（`sent` 在 `now` 之后）**算在窗口内**：宁可少发一条，也别因为系统时间
/// 被改动而连发 5 条骚扰用户。
fn within_window(sent: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(sent) {
        Ok(elapsed) => elapsed < RATE_WINDOW,
        Err(_) => true,
    }
}

/// 裁掉窗口外的旧时间戳（防止 `Vec` 无限长）。
fn prune_window(window: &mut Vec<SystemTime>, now: SystemTime) {
    window.retain(|sent| within_window(*sent, now));
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

    /// 发满 5 条（每条错开 31 分钟避开冷却）。
    fn fill_window(notifier: &mut Notifier) {
        for index in 0..RATE_CAP as u64 {
            assert_eq!(
                notifier.offer_at(clean(), at(index * 31)),
                NotifyOutcome::Queued,
                "第 {index} 条应放行"
            );
        }
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
    fn window_cap_blocks_sixth_within_24h() {
        let mut notifier = Notifier::new();
        fill_window(&mut notifier);
        // 第 6 条：仍在 24h 窗口内（5 条最老的是 t=0，隔了 155 分钟）
        assert_eq!(
            notifier.offer_at(clean(), at(RATE_CAP as u64 * 31)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::WindowCap
            }
        );
    }

    /// 配额是**滑动窗口**，不是「UTC 日历日计数」。
    ///
    /// 判据取在跨零点这一刻：把 5 条全塞在 23:xx（同一天末），零点后来一条。
    /// - 日历日实现：计数器归零 → 放行（用户会在零点后 1 分钟被再骚扰一次）。
    /// - 滚动窗口实现：5 条都还在 24h 内 → 拦。
    ///
    /// `UNIX_EPOCH` 正好是 UTC 零点，所以第 2 天的零点 = 2880 分钟。
    /// 注意门的顺序是「开关 → 静默 → 冷却 → 配额」，所以探针要离最后一条 ≥30 分钟，
    /// 否则先被 `Cooldown` 拦下，测不到配额。
    #[test]
    fn window_slides_instead_of_resetting_at_midnight() {
        let midnight = 2880;
        let mut notifier = Notifier::new();
        for index in 0..RATE_CAP as u64 {
            // 2730/2761/2792/2823/2854 —— 全部落在零点前的同一天末段
            assert_eq!(
                notifier.offer_at(clean(), at(midnight - 150 + index * 31)),
                NotifyOutcome::Queued,
                "零点前的第 {index} 条应放行"
            );
        }
        // 零点后 31 分钟（冷却已过，配额未滑出）：日历日实现会归零放行，滚动窗口必须仍拦
        assert_eq!(
            notifier.offer_at(clean(), at(midnight + 31)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::WindowCap
            },
            "跨零点不该释放配额（那正是日历日实现的 bug）"
        );
        // 最老那条（2730）满 24h 出窗口：2730+1440=4170 分钟 → 窗口内剩 4 条 → 放行
        assert_eq!(
            notifier.offer_at(clean(), at(midnight - 150 + RATE_WINDOW.as_secs() / 60)),
            NotifyOutcome::Queued,
            "滑动窗口：最老条目滑出后应释放配额"
        );
    }

    #[test]
    fn window_cap_does_not_leak_across_kinds() {
        let mut notifier = Notifier::new();
        fill_window(&mut notifier);
        let update = Notification::new(
            NotifyKind::UpdateAvailable,
            NotifyLevel::Info,
            "有新版本",
            "1.38.0",
        );
        assert_eq!(
            notifier.offer_at(update, at(155)),
            NotifyOutcome::Queued,
            "配额是按类别算的"
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
    fn channel_availability_is_queryable_for_close_logic() {
        // 关默认不可用：关闭逻辑若在托盘建不起来时仍「藏进托盘」，用户就找不回来了。
        let mut notifier = Notifier::new();
        assert!(!notifier.channel().is_balloon());
        assert_eq!(notifier.channel().summary(), "none");
        assert_eq!(
            notifier.channel().reason(),
            Some("no-channel"),
            "兜底也要有原因"
        );
        notifier.set_channel(ChannelAvailability::balloon());
        assert!(notifier.channel().is_balloon());
        assert_eq!(notifier.channel().summary(), "balloon");
        assert_eq!(notifier.channel().reason(), None, "可用时不该有原因");
        notifier.set_channel(ChannelAvailability::unavailable("shell-refused"));
        assert!(!notifier.channel().is_balloon(), "降级要能写回去");
        assert_eq!(
            notifier.channel().reason(),
            Some("shell-refused"),
            "原因要能一路传到面板与日志"
        );
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
        assert_eq!(SuppressReason::WindowCap.as_str(), "window-cap");
        assert_eq!(NotifyLevel::Info.as_str(), "info");
        assert_eq!(NotifyLevel::Warn.as_str(), "warn");
        assert_eq!(NotifyLevel::Error.as_str(), "error");
    }

    #[test]
    fn policy_constants_are_pinned() {
        assert_eq!(COOLDOWN, Duration::from_secs(1800));
        assert_eq!(RATE_WINDOW, Duration::from_secs(24 * 60 * 60));
        assert_eq!(RATE_CAP, 5);
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let mut notifier = Notifier::new();
        assert_eq!(notifier.offer_at(clean(), at(120)), NotifyOutcome::Queued);
        // 时钟被拨回：按「还在窗口内」处理，不能 panic、不能放行成重复打扰
        assert_eq!(
            notifier.offer_at(clean(), at(0)),
            NotifyOutcome::Suppressed {
                kind: NotifyKind::Clean,
                reason: SuppressReason::Cooldown
            }
        );
    }
}
