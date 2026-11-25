//! 服务管理页的**纯逻辑**（M3-b / task-46）：六态文案、按钮可用性、等待态、错误码 → 人话。
//!
//! 这里不碰 egui、不碰 SCM：所有函数都是纯的，可以在 Linux 上直接单测。
//! 渲染与收集 SCM 数据分别在 `app.rs` 与 `worker.rs`，提权在 `elevate.rs`。

use std::time::{Duration, Instant};

use ariahost_common::ErrorCode;
use ariahost_svc::{ServiceStartMode, ServiceState};

/// 「安装」的等待预算：SCM 注册通常秒级，30s 是给慢盘/杀软留下的余量（task-46 指定）。
pub const INSTALL_BUDGET: Duration = Duration::from_secs(30);
/// 「启动」的等待预算。
pub const START_BUDGET: Duration = Duration::from_secs(30);
/// 「停止」的等待预算：架构 §4.6 是 15s 优雅 + 10s 强制 = 25s（task-46 指定）。
pub const STOP_BUDGET: Duration = Duration::from_secs(25);
/// 优雅停止段落的长度（架构 §4.6 第一段）。
pub const STOP_GRACEFUL: Duration = Duration::from_secs(15);

/// 六态的中文短标签（卡片标题行）。
pub const fn state_label(state: ServiceState) -> &'static str {
    match state {
        ServiceState::NotInstalled => "未安装",
        ServiceState::Stopped => "已停止",
        ServiceState::StartPending => "启动中",
        ServiceState::Running => "运行中",
        ServiceState::StopPending => "停止中",
        ServiceState::Unknown => "未知",
    }
}

/// 状态行下面的补充说明（一句话解释「这意味着什么」）。
pub const fn state_hint(state: ServiceState) -> &'static str {
    match state {
        ServiceState::NotInstalled => "还没有把 aria2 装成 Windows 服务：装好后可以开机自启、面板关掉也能继续下载。",
        ServiceState::Stopped => "服务已注册但当前没在跑：点「启动服务」由 SCM 拉起它（面板可以关）。",
        ServiceState::StartPending => "SCM 正在拉起服务，稍等一下（一般 1–2 秒）。",
        ServiceState::Running => "服务正在运行，aria2 监听本机 RPC；面板关不关都不影响下载。",
        ServiceState::StopPending => "SCM 正在停止服务；aria2 会先把会话与队列落盘再退出。",
        ServiceState::Unknown => "SCM 报了一个本产品不该出现的状态（如暂停系列）：请刷新，或用 `sc query AriaHost` 核对。",
    }
}

/// 启动方式的中文标签（`None` = 未安装）。
pub fn start_mode_label(mode: Option<ServiceStartMode>) -> &'static str {
    match mode {
        None => "—",
        Some(ServiceStartMode::Demand) => "手动触发（需要时由面板/SCM 启动）",
        Some(ServiceStartMode::Auto) => "随系统自动启动",
        Some(ServiceStartMode::Disabled) => "已禁用（需要先启用才能启动）",
        Some(ServiceStartMode::System) => "驱动：系统启动时加载（异常）",
        Some(ServiceStartMode::Boot) => "驱动：引导时加载（异常）",
    }
}

/// 用户点的那三个操作（停止/启动算两个，共四个动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// 安装为服务（一次 UAC）。
    Install,
    /// 卸载服务（一次 UAC；服务在跑时由后端先停再删）。
    Uninstall,
    /// 启动服务。
    Start,
    /// 停止服务。
    Stop,
}

impl Op {
    /// 按钮文案。
    pub const fn button(self) -> &'static str {
        match self {
            Self::Install => "安装为服务",
            Self::Uninstall => "卸载服务",
            Self::Start => "启动服务",
            Self::Stop => "停止服务",
        }
    }

    /// 等待态第一行（进度感）。
    pub const fn progress(self) -> &'static str {
        match self {
            Self::Install => "注册中…",
            Self::Uninstall => "卸载中…",
            Self::Start => "启动中…",
            Self::Stop => "停止中…",
        }
    }

    /// 等待态第二行（告诉用户「这期间别重复点、大概多久」）。
    pub const fn waiting_hint(self) -> &'static str {
        match self {
            Self::Install => "正在向 Windows 服务管理器注册服务，一般 1–3 秒（最长 30 秒）。",
            Self::Uninstall => "先停服务再删注册，服务在跑时要等它把队列落盘（可能 10–30 秒）。",
            Self::Start => "SCM 拉起服务并等 RPC 就绪，一般 1–3 秒（最长 30 秒）。",
            Self::Stop => "aria2 先把会话与队列落盘再退出：前 15 秒优雅停止，之后才是强制结束。",
        }
    }

    /// 该操作的等待预算。
    pub const fn budget(self) -> Duration {
        match self {
            Self::Install => INSTALL_BUDGET,
            Self::Uninstall | Self::Start => START_BUDGET,
            Self::Stop => STOP_BUDGET,
        }
    }
}

/// 当前状态下四个按钮各自能不能点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buttons {
    /// 安装为服务。
    pub install: bool,
    /// 卸载服务。
    pub uninstall: bool,
    /// 启动服务。
    pub start: bool,
    /// 停止服务。
    pub stop: bool,
}

impl Buttons {
    /// 一个都不能点（等待态/状态未知时用）。
    pub const NONE: Self = Self {
        install: false,
        uninstall: false,
        start: false,
        stop: false,
    };
}

/// 按六态决定按钮可用性。**过渡态一律不点**，避免用户把 SCM 搅成竞态。
pub const fn buttons(state: ServiceState) -> Buttons {
    match state {
        ServiceState::NotInstalled => Buttons {
            install: true,
            uninstall: false,
            start: false,
            stop: false,
        },
        ServiceState::Stopped => Buttons {
            install: false,
            uninstall: true,
            start: true,
            stop: false,
        },
        ServiceState::Running => Buttons {
            install: false,
            uninstall: true,
            start: false,
            stop: true,
        },
        // 过渡态与未知态：只让用户刷新，不提供会撞车/靠猜的操作。
        ServiceState::StartPending | ServiceState::StopPending | ServiceState::Unknown => {
            Buttons::NONE
        }
    }
}

/// 卡片当前处在什么阶段。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stage {
    /// 空闲：只显示状态与按钮。
    #[default]
    Idle,
    /// 正在做某个操作（`since` 用来算「已等 X 秒」）。
    Waiting {
        /// 正在做的操作。
        op: Op,
        /// 开始时刻。
        since: Instant,
    },
}

impl Stage {
    /// 开始等待。
    pub fn begin(op: Op) -> Self {
        Self::Waiting {
            op,
            since: Instant::now(),
        }
    }

    /// 正在做的操作（空闲为 `None`）。
    pub const fn op(self) -> Option<Op> {
        match self {
            Self::Idle => None,
            Self::Waiting { op, .. } => Some(op),
        }
    }

    /// 已经等了多久。
    pub fn elapsed(self, now: Instant) -> Duration {
        match self {
            Self::Idle => Duration::ZERO,
            Self::Waiting { since, .. } => now.saturating_duration_since(since),
        }
    }

    /// 是否已超出该操作的预算。
    pub fn over_budget(self, now: Instant) -> bool {
        match self {
            Self::Idle => false,
            Self::Waiting { op, .. } => self.elapsed(now) > op.budget(),
        }
    }
}

/// 等待态的三行文案：`progress` / `hint` / `counter`（第三行可能没有）。
///
/// 停止是**两段式**（架构 §4.6）：前 [`STOP_GRACEFUL`] 报「优雅停止」，
/// 之后报「强制结束」——用户能看出系统在按顺序做两件事，而不是卡死了。
pub fn waiting_text(stage: Stage, now: Instant) -> Option<(String, String, Option<String>)> {
    let op = stage.op()?;
    let waited = stage.elapsed(now);
    let seconds = format!("{:.1}", waited.as_secs_f32());

    let progress = match op {
        Op::Stop if waited <= STOP_GRACEFUL => "停止中…（正在优雅停止）".to_owned(),
        Op::Stop if waited <= STOP_BUDGET => "停止中…（优雅超时，正在强制结束）".to_owned(),
        Op::Stop => "停止中…（已超出 25 秒预算，仍在等 SCM 回报）".to_owned(),
        other => other.progress().to_owned(),
    };

    let counter = if stage.over_budget(now) {
        format!("已等 {seconds} 秒（超出预算，可能仍在后台完成；可点「刷新状态」核对）")
    } else {
        format!("已等 {seconds} 秒")
    };

    Some((progress, op.waiting_hint().to_owned(), Some(counter)))
}

/// 提权被取消（`ERROR_CANCELLED`）时给用户的那句话——**这不是错误**，不许弹红条。
///
/// 两种情形要分开说，否则不诚实：
/// - 用户真的点了取消（很快就返回）；
/// - 这个会话根本看不到授权框（锁屏/无人值守时 `ShellExecuteExW` 会一直等，
///   实测约 122 秒后返回 `ERROR_CANCELLED`）——那时说「你取消了授权」是冤枉用户。
pub fn elevation_cancelled_message(waited: Duration) -> String {
    if waited >= Duration::from_secs(60) {
        // 仍然保留「你取消了授权」这个说法，便于验收脚本/文档统一检索，
        // 但把「也可能是看不到授权框」如实讲出来。
        "你取消了授权（也可能是当前会话无法显示授权框），服务未变更。".to_owned()
    } else {
        "你取消了授权，服务未变更。".to_owned()
    }
}

/// 错误码 → 「一句话结论 + 下一步」。**不出现裸错误码在前台**（码放括号里，便于对照文档）。
pub fn hint_for(code: ErrorCode) -> (&'static str, &'static str) {
    match code {
        ErrorCode::ServiceNeedsAdmin => (
            "需要管理员权限：点一下会自动弹出 Windows 授权框（UAC）。",
            "在授权框里点「是」。若不想再看到它，可以右键面板 →「以管理员身份运行」。",
        ),
        ErrorCode::ServiceRegisterPrereq => (
            "服务注册前置条件没满足（通常是 %ProgramData%\\AriaHost 权限或核心缺失）。",
            "点「查看服务日志」看具体哪一步；必要时先以管理员身份运行一次面板。",
        ),
        ErrorCode::ServiceUnavailable => (
            "Windows 服务管理器（SCM）当前不可用。",
            "确认不是精简系统/被策略禁用，再点「刷新状态」；仍失败请看服务日志。",
        ),
        ErrorCode::Aria2NotFound => (
            "服务装上了，但服务侧找不到 aria2 核心。",
            "先在面板里完成「核心未就绪」引导（安装核心），再回来启动服务。",
        ),
        ErrorCode::Aria2StartTimeout => (
            "服务已启动，但 aria2 在预算时间内没就绪。",
            "点「查看服务日志」看 aria2c 的启动行；常见原因是端口被占或 secret 不匹配。",
        ),
        _ => (
            "操作没有完成。",
            "点「查看服务日志」看具体原因；错误码在括号里，可对照文档。",
        ),
    }
}

/// 卡片右上角那条「操作结果」提示的语气。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeTone {
    /// 成功（绿）。
    Ok,
    /// 信息（灰）：例如「你取消了授权」。
    Info,
    /// 失败（红）。
    Bad,
}

/// 一次操作结束后的提示：文案 + 语气。
pub fn notice_after(op: Op, outcome: NoticeOutcome) -> (NoticeTone, String) {
    match outcome {
        NoticeOutcome::Done => match op {
            Op::Install => (
                NoticeTone::Ok,
                "已注册为 Windows 服务（手动触发）。点「启动服务」让它开始工作。".to_owned(),
            ),
            Op::Uninstall => (NoticeTone::Ok, "服务已卸载，注册信息已删除。".to_owned()),
            Op::Start => (
                NoticeTone::Ok,
                "服务已启动（运行中）。现在关掉面板也不影响下载。".to_owned(),
            ),
            Op::Stop => (NoticeTone::Ok, "服务已停止；会话与队列已落盘。".to_owned()),
        },
        NoticeOutcome::TimedOut => (
            NoticeTone::Bad,
            format!(
                "{}没有在预算内完成（可能仍在后台继续）。点「刷新状态」再核对一次。",
                op.progress().trim_end_matches('…')
            ),
        ),
    }
}

/// 操作结果（业务层，不含提权细节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeOutcome {
    /// 达到目标状态。
    Done,
    /// 超预算。
    TimedOut,
}

/// 操作完成后，「达到目标状态」的判定：把目标状态摊平成谓词，便于单测。
///
/// 只看状态、不看 pid：SCM 有时不给 pid（例如服务刚进入 Running 的那一瞬间），
/// 不能因为少一个 pid 就判「没成功」。
pub const fn reached(op: Op, state: ServiceState) -> bool {
    match op {
        Op::Install => state.is_installed(),
        Op::Uninstall => !state.is_installed(),
        Op::Start => state.is_running(),
        Op::Stop => matches!(state, ServiceState::Stopped | ServiceState::NotInstalled),
    }
}

/// 服务管理页的**状态机**（不碰 egui）：等待态、操作结果提示、何时算完成。
///
/// 分工：`worker` 负责「定期去问 SCM」并把快照推给 UI；`app` 负责把点击变成提权调用；
/// 这个结构只回答三个问题——现在在等什么、要不要报超时、这次操作算不算成功。
#[derive(Debug, Clone, Default)]
pub struct ServiceCard {
    stage: Stage,
    pending: Option<Op>,
    notice: Option<(NoticeTone, String)>,
}

impl ServiceCard {
    /// 新建（空闲）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前阶段。
    pub const fn stage(&self) -> Stage {
        self.stage
    }

    /// 正在等哪个操作（空闲为 `None`）。
    pub const fn pending(&self) -> Option<Op> {
        self.pending
    }

    /// 最近一条结果提示。
    pub fn notice(&self) -> Option<&(NoticeTone, String)> {
        self.notice.as_ref()
    }

    /// 开始一次操作：进等待态，清掉上一条提示。
    pub fn begin(&mut self, op: Op) {
        self.stage = Stage::begin(op);
        self.pending = Some(op);
        self.notice = None;
    }

    /// 直接给一条提示并回到空闲（提权被取消、启动失败等「已经有结论」的路径用）。
    pub fn settle(&mut self, tone: NoticeTone, message: impl Into<String>) {
        self.stage = Stage::Idle;
        self.pending = None;
        self.notice = Some((tone, message.into()));
    }

    /// 观察 SCM 快照：达到目标状态就收尾并返回该报的结果。
    ///
    /// 只看状态、不看提权子进程的退出码：SCM 是真的（用户也可能在别处点了启动），
    /// 以 SCM 为准比以退出码为准更不容易骗人。
    pub fn observe(&mut self, state: Option<ServiceState>) -> Option<(Op, NoticeOutcome)> {
        let op = self.pending?;
        let state = state?;
        if !reached(op, state) {
            return None;
        }
        let outcome = NoticeOutcome::Done;
        self.finish(op, outcome);
        Some((op, outcome))
    }

    /// 超出预算就收尾（返回一次，之后不再重复报）。
    pub fn timeout_if_due(&mut self, now: Instant) -> Option<(Op, NoticeOutcome)> {
        let op = self.pending?;
        if !self.stage.over_budget(now) {
            return None;
        }
        let outcome = NoticeOutcome::TimedOut;
        self.finish(op, outcome);
        Some((op, outcome))
    }

    /// 收尾：把 `op` 的结果写成提示并回到空闲。
    pub fn finish(&mut self, op: Op, outcome: NoticeOutcome) {
        let (tone, message) = notice_after(op, outcome);
        self.stage = Stage::Idle;
        self.pending = None;
        self.notice = Some((tone, message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [ServiceState; 6] = [
        ServiceState::NotInstalled,
        ServiceState::Stopped,
        ServiceState::StartPending,
        ServiceState::Running,
        ServiceState::StopPending,
        ServiceState::Unknown,
    ];

    #[test]
    fn six_state_labels_are_distinct_and_nonempty() {
        let mut seen = Vec::new();
        for state in ALL_STATES {
            let label = state_label(state);
            assert!(!label.is_empty(), "{state:?} 的文案不能为空");
            assert!(!seen.contains(&label), "{label} 重复了：六态必须互不混淆");
            seen.push(label);
            assert!(!state_hint(state).is_empty());
        }
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn buttons_follow_the_state_machine() {
        // 未安装：只能装。
        let b = buttons(ServiceState::NotInstalled);
        assert!(b.install && !b.uninstall && !b.start && !b.stop);
        // 已停止：可卸可启，不能再装。
        let b = buttons(ServiceState::Stopped);
        assert!(!b.install && b.uninstall && b.start && !b.stop);
        // 运行中：可停可卸，不能再启。
        let b = buttons(ServiceState::Running);
        assert!(!b.install && b.uninstall && !b.start && b.stop);
        // 过渡态/未知：什么都不给点（避免把 SCM 搅成竞态）。
        for state in [
            ServiceState::StartPending,
            ServiceState::StopPending,
            ServiceState::Unknown,
        ] {
            assert_eq!(buttons(state), Buttons::NONE, "{state:?} 不该给可点按钮");
        }
    }

    #[test]
    fn stop_waiting_text_has_two_phases() {
        let stage = Stage::begin(Op::Stop);
        let now = match stage {
            Stage::Waiting { since, .. } => since,
            Stage::Idle => unreachable!(),
        };
        // 第 5 秒：优雅段。
        let (progress, hint, counter) =
            waiting_text(stage, now + Duration::from_secs(5)).expect("等待态必须有文案");
        assert!(progress.contains("优雅停止"), "实际：{progress}");
        assert!(hint.contains("15 秒"), "提示里要讲清楚两段：{hint}");
        assert!(counter.expect("应有计时").contains("已等 5.0 秒"));

        // 第 20 秒：强制段（>15s 且 <=25s）。
        let (progress, _, _) =
            waiting_text(stage, now + Duration::from_secs(20)).expect("仍在等待");
        assert!(progress.contains("强制结束"), "实际：{progress}");

        // 第 30 秒：超预算。
        let (progress, _, counter) =
            waiting_text(stage, now + Duration::from_secs(30)).expect("仍在等待");
        assert!(progress.contains("超出 25 秒预算"), "实际：{progress}");
        assert!(counter.expect("应有计时").contains("超出预算"));
    }

    #[test]
    fn other_ops_use_their_own_progress_words() {
        for (op, expected) in [
            (Op::Install, "注册中…"),
            (Op::Uninstall, "卸载中…"),
            (Op::Start, "启动中…"),
        ] {
            let stage = Stage::begin(op);
            let now = match stage {
                Stage::Waiting { since, .. } => since,
                Stage::Idle => unreachable!(),
            };
            let (progress, _, _) = waiting_text(stage, now).expect("必须有文案");
            assert_eq!(progress, expected);
        }
        // 空闲态没有等待文案。
        assert!(waiting_text(Stage::Idle, Instant::now()).is_none());
    }

    #[test]
    fn budgets_match_the_task_contract() {
        assert_eq!(Op::Install.budget(), Duration::from_secs(30));
        assert_eq!(Op::Start.budget(), Duration::from_secs(30));
        // 架构 §4.6：15s 优雅 + 10s 强制。
        assert_eq!(Op::Stop.budget(), Duration::from_secs(25));
        assert_eq!(STOP_GRACEFUL, Duration::from_secs(15));
    }

    #[test]
    fn cancelled_elevation_is_not_an_error_and_says_so() {
        // 用户很快点了取消：直说取消。
        let quick = elevation_cancelled_message(Duration::from_secs(2));
        assert!(quick.contains("你取消了授权"));
        assert!(!quick.contains("失败"));
        // 锁屏/无人值守：ShellExecuteExW 会等约两分钟后返回取消，不能冤枉用户。
        let slow = elevation_cancelled_message(Duration::from_secs(122));
        assert!(slow.contains("你取消了授权"));
        assert!(slow.contains("无法显示授权框"), "实际：{slow}");
        // 无论是哪一种，「服务未变更」都必须说清楚。
        assert!(quick.contains("服务未变更") && slow.contains("服务未变更"));
    }

    #[test]
    fn needs_admin_hint_tells_the_user_what_will_happen() {
        let (headline, next) = hint_for(ErrorCode::ServiceNeedsAdmin);
        assert!(headline.contains("管理员"));
        assert!(headline.contains("授权框"));
        assert!(next.contains("以管理员身份运行"));
    }

    #[test]
    fn reached_matches_each_goal() {
        assert!(reached(Op::Install, ServiceState::Stopped));
        assert!(reached(Op::Install, ServiceState::Running));
        assert!(!reached(Op::Install, ServiceState::NotInstalled));
        assert!(reached(Op::Uninstall, ServiceState::NotInstalled));
        assert!(!reached(Op::Uninstall, ServiceState::Stopped));
        assert!(reached(Op::Start, ServiceState::Running));
        assert!(!reached(Op::Start, ServiceState::StartPending));
        assert!(reached(Op::Stop, ServiceState::Stopped));
        assert!(reached(Op::Stop, ServiceState::NotInstalled));
        assert!(!reached(Op::Stop, ServiceState::StopPending));
    }

    #[test]
    fn start_mode_labels_cover_all_cases() {
        assert_eq!(start_mode_label(None), "—");
        assert!(start_mode_label(Some(ServiceStartMode::Demand)).contains("手动"));
        assert!(start_mode_label(Some(ServiceStartMode::Auto)).contains("自动"));
        assert!(start_mode_label(Some(ServiceStartMode::Disabled)).contains("禁用"));
    }

    #[test]
    fn card_finishes_when_scm_reaches_the_goal() {
        let mut card = ServiceCard::new();
        card.begin(Op::Install);
        assert_eq!(card.pending(), Some(Op::Install));
        assert_eq!(card.stage().op(), Some(Op::Install));
        // 还没装好：不报完成。
        assert!(card.observe(Some(ServiceState::NotInstalled)).is_none());
        // 装好了（SCM 报已停止）：收尾 + 给出「装好了，去启动」的提示。
        let done = card
            .observe(Some(ServiceState::Stopped))
            .expect("应判定完成");
        assert_eq!(done, (Op::Install, NoticeOutcome::Done));
        assert_eq!(card.pending(), None);
        assert_eq!(card.stage(), Stage::Idle);
        let (tone, message) = card.notice().expect("必须有提示");
        assert_eq!(*tone, NoticeTone::Ok);
        assert!(message.contains("启动服务"), "实际：{message}");
    }

    #[test]
    fn card_times_out_once_and_says_so() {
        let mut card = ServiceCard::new();
        card.begin(Op::Stop);
        let now = match card.stage() {
            Stage::Waiting { since, .. } => since,
            Stage::Idle => unreachable!(),
        };
        // 预算内：不收尾。
        assert!(card.timeout_if_due(now + Duration::from_secs(20)).is_none());
        // 超预算：收尾一次。
        let first = card
            .timeout_if_due(now + Duration::from_secs(26))
            .expect("应报超时");
        assert_eq!(first.1, NoticeOutcome::TimedOut);
        // 不重复报。
        assert!(card.timeout_if_due(now + Duration::from_secs(40)).is_none());
    }

    #[test]
    fn card_ignores_unknown_state_until_it_knows_something() {
        let mut card = ServiceCard::new();
        card.begin(Op::Start);
        // 还没有快照：继续等，不能凭空判成功/失败。
        assert!(card.observe(None).is_none());
        assert_eq!(card.pending(), Some(Op::Start));
        // 过渡态也不算成功。
        assert!(card.observe(Some(ServiceState::StartPending)).is_none());
        assert!(card.observe(Some(ServiceState::Running)).is_some());
    }

    #[test]
    fn settle_is_used_for_non_error_paths_like_cancelled_uac() {
        let mut card = ServiceCard::new();
        card.begin(Op::Install);
        card.settle(
            NoticeTone::Info,
            elevation_cancelled_message(Duration::from_secs(3)),
        );
        assert_eq!(card.pending(), None);
        let (tone, message) = card.notice().expect("必须有提示");
        assert_eq!(*tone, NoticeTone::Info, "取消授权不是错误，不能红条");
        assert!(message.contains("服务未变更"));
    }
}
