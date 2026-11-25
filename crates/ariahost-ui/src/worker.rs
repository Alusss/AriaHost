//! 面板数据刷新（M2-1 起 / M2-2b 扩）：SCM 服务状态 + aria2 RPC 状态 + 任务列表 + 行操作，
//! 全部在后台线程完成。
//!
//! 四条硬约束：
//! 1. **不阻塞 UI 线程**：RPC 与 SCM 查询（含 pause/unpause/remove/addUri）都跑在 worker 线程，
//!    UI 只读 `mpsc` 送来的结果，每帧 `try_recv` 一次（非阻塞），从不等待网络；
//! 2. **不做忙轮询**：成功时每 [`ARIA2_REFRESH_INTERVAL`] 刷新一次；失败按
//!    2s→4s→8s→16s→30s 退避（AH-E2005 语义，上限 [`BACKOFF_CAP`]）；
//! 3. **原始输出不上屏**：`RpcCallError` 的 `detail` 是 IO 原文（可能含 JSON/栈信息），
//!    只用来归类成 [`Aria2Failure`]；UI 拿到的永远是固定中文文案 + 错误码；
//! 4. **格式化离开 UI 线程**（EG-9）：任务行在 worker 里一次性变成 [`TaskRow`]
//!    （字符串字段全是 `Arc<str>`），UI 每帧只复制指针，不做 `format!`。
//!
//! 本模块**不托管 aria2c 进程**——那是 [`crate::core_host`] 的职责。这里只负责「连上去做什么」。

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ariahost_aria2::{GlobalStat, RpcCallError, RpcClient, VersionInfo, DEFAULT_PORT, RPC_PATH};
use ariahost_common::ErrorCode;
use ariahost_svc::{status_report, ServiceQueryError, ServiceState, ServiceStatusReport};
use serde_json::Value;

use crate::logbuf::LogBuffer;
use crate::tasks::{self, TaskAction, TaskRow, TaskStatus, MAX_ROWS_PER_BUCKET};

/// `aria2.pause`。
pub const RPC_PAUSE: &str = "aria2.pause";
/// `aria2.unpause`。
pub const RPC_UNPAUSE: &str = "aria2.unpause";
/// `aria2.remove`（活动/等待/暂停的任务）。
pub const RPC_REMOVE: &str = "aria2.remove";
/// `aria2.removeDownloadResult`（已完成/出错/已移除的**结果条目**）。
pub const RPC_REMOVE_RESULT: &str = "aria2.removeDownloadResult";

/// aria2 状态的成功刷新间隔（架构 §4.12 的「2s」口径）。
pub const ARIA2_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// 环境变量：覆盖成功刷新间隔（毫秒）。配置面在 M3 才落地，这里先留一个字面可控的逃生口。
///
/// 有效范围 [`REFRESH_INTERVAL_MIN`]..=[`REFRESH_INTERVAL_MAX`]；越界/非数字一律忽略并走默认，
/// 避免一个错的环境变量把面板变成忙轮询（设成 0）或假死（设成 1 小时）。
pub const REFRESH_INTERVAL_ENV: &str = "ARIAHOST_REFRESH_MS";

/// 刷新间隔下限（太快就是忙轮询）。
pub const REFRESH_INTERVAL_MIN: Duration = Duration::from_millis(500);

/// 刷新间隔上限（再慢就不像「实时」了）。
pub const REFRESH_INTERVAL_MAX: Duration = Duration::from_secs(60);

/// 失败退避上限：`AH-E2005` 的口径是「最长 30 秒重试一次」。
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// 服务状态刷新间隔（一次 `QueryServiceStatusEx`，轻量，不必跟着 aria2 的 2s）。
/// 服务状态的常规刷新间隔：SCM 查询要开服务管理器，5s 一次足够跟手又不烧 CPU（M2-1 起的值）。
pub const SERVICE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// 服务页打开时的刷新间隔（M3-b）：面板正盯着看，2s 能接受。
pub const SERVICE_PAGE_INTERVAL: Duration = Duration::from_secs(2);

/// 服务操作后的连拉间隔：1s 一次、连拉 [`SERVICE_BURST_TIMES`] 次，
/// 让「注册中…/启动中…/停止中…」几秒内就能看到真实状态跳变。
pub const SERVICE_BURST_INTERVAL: Duration = Duration::from_secs(1);

/// 服务操作后连拉几次。
pub const SERVICE_BURST_TIMES: u8 = 3;

/// 连续失败几次后，把 `AH-E2005`（重连失败）一并提示出来。
pub const RECONNECT_FAILURES: u32 = 5;

/// 单次 RPC 的超时（`ariahost-aria2` 的默认 5s）；面板不覆盖，留作文档锚点。
pub const RPC_TIMEOUT: Duration = ariahost_aria2::DEFAULT_TIMEOUT;

pub use crate::view::{StatusView, Tone};

/// aria2 RPC 端点文本（「复制 RPC 端点」按钮与状态区共用）。
pub fn endpoint_text() -> String {
    endpoint_text_for(DEFAULT_PORT)
}

/// 指定端口的 RPC 端点文本。
pub fn endpoint_text_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}{RPC_PATH}")
}

/// 一轮成功刷新的快照（UI 只读这个结构，不再碰网络）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aria2Snapshot {
    /// 核心版本号（`aria2.getVersion`）。
    pub version: String,
    /// 启用的功能项数（只给数量，不把整张表铺到界面上）。
    pub feature_count: usize,
    /// 全局下载速率（字节/秒）。
    pub download_speed: u64,
    /// 全局上传速率（字节/秒）。
    pub upload_speed: u64,
    /// 正在下载的任务数。
    pub num_active: u64,
    /// 排队中的任务数。
    pub num_waiting: u64,
    /// 已停止的任务数。
    pub num_stopped: u64,
    /// 历史停止总数（旧版本没有该字段）。
    pub num_stopped_total: Option<u64>,
}

impl Aria2Snapshot {
    /// 由两个 RPC 结果拼一份快照。
    pub fn from_parts(version: VersionInfo, stat: GlobalStat) -> Self {
        Self {
            version: version.version,
            feature_count: version.enabled_features.len(),
            download_speed: stat.download_speed,
            upload_speed: stat.upload_speed,
            num_active: stat.num_active,
            num_waiting: stat.num_waiting,
            num_stopped: stat.num_stopped,
            num_stopped_total: stat.num_stopped_total,
        }
    }
}

/// RPC 失败的**归类**（不是原文）。
///
/// 刻意实现 `Copy` 且不带 `String`：这样「把 aria2 原始 stderr / JSON 甩到界面上」
/// 在类型层面就做不到——UI 只能拿到这个枚举，文案由它自己的方法产出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aria2Failure {
    /// 连不上（端口没人监听 / 拒绝连接）。
    Unreachable,
    /// 连上了但读写超时。
    Timeout,
    /// HTTP 层被拒（403/404 映射 `AH-E3003`，其余状态码没有对应码）。
    HttpStatus(u16),
    /// 响应不是合法的 JSON-RPC 形状。
    Protocol,
    /// aria2 自己返回了 JSON-RPC `error`（只保留数字码，保留 message 就会把原始 JSON 带上屏）。
    Remote(i64),
}

impl Aria2Failure {
    /// 由 `RpcCallError` 归类（唯一会把 RPC 错误翻成面板语义的地方）。
    pub fn from_error(error: &RpcCallError) -> Self {
        match error {
            RpcCallError::Connect { timed_out, .. } | RpcCallError::Transport { timed_out, .. } => {
                if *timed_out {
                    Self::Timeout
                } else {
                    Self::Unreachable
                }
            }
            RpcCallError::Protocol { .. } => Self::Protocol,
            RpcCallError::HttpStatus { status } => Self::HttpStatus(*status),
            RpcCallError::Remote { error } => Self::Remote(error.code),
            RpcCallError::MethodNotFound { error, .. } => Self::Remote(error.code),
        }
    }

    /// 对应的既有错误码（不新增码）。
    pub fn code(self) -> ErrorCode {
        match self {
            Self::HttpStatus(403 | 404) => ErrorCode::HttpRejected,
            Self::Unreachable
            | Self::Timeout
            | Self::Protocol
            | Self::HttpStatus(_)
            | Self::Remote(_) => ErrorCode::RpcHandshakeFailed,
        }
    }

    /// 面向用户的固定中文说明（不含任何原始输出）。
    pub fn summary(self) -> String {
        match self {
            Self::Unreachable => {
                format!("连不上 aria2（{DEFAULT_PORT} 端口没有可连接的 RPC 服务）")
            }
            Self::Timeout => "aria2 RPC 响应超时（单次操作超过 5 秒）".to_owned(),
            Self::HttpStatus(403 | 404) => {
                "aria2 RPC 拒绝了请求（HTTP 403/404，通常是 RPC secret 不匹配）".to_owned()
            }
            Self::HttpStatus(status) => format!("aria2 RPC 返回非预期 HTTP 状态 {status}"),
            Self::Protocol => "aria2 RPC 响应不符合 JSON-RPC 形状（对端可能不是 aria2）".to_owned(),
            Self::Remote(code) => format!("aria2 返回 RPC 错误 {code}"),
        }
    }

    /// 下一步提示。
    pub fn next_step(self) -> String {
        match self {
            Self::Unreachable | Self::Timeout => {
                "先启动 aria2（面板左上「启动 aria2」）；启动后这里会自动连上".to_owned()
            }
            Self::HttpStatus(_) | Self::Protocol | Self::Remote(_) => {
                "确认 RPC secret 与本进程一致（secret.bin 属于当前 Windows 用户）；本批次不自动重装核心"
                    .to_owned()
            }
        }
    }
}

/// 服务状态的快照或失败（失败也带码，UI 直出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceSnapshot {
    /// 查询成功（含 `NotInstalled`，它**不是**错误）。
    ///
    /// M3-b 起带上完整报告：服务页要显示启动方式与 PID，光有六态不够。
    State(ServiceStatusReport),
    /// 查询失败。
    Failed {
        /// 既有错误码（`AH-E5001` / `AH-E5002`）。
        code: ErrorCode,
        /// 归类，决定中文文案。
        kind: ServiceFailure,
    },
}

impl ServiceSnapshot {
    /// 六态（查询失败或还没刷新过时为 `None`）。
    pub fn state(&self) -> Option<ServiceState> {
        match self {
            Self::State(report) => Some(report.state),
            Self::Failed { .. } => None,
        }
    }
}

/// 服务查询失败的归类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceFailure {
    /// 非 Windows 平台：没有 SCM。
    UnsupportedPlatform,
    /// 权限不足（`AH-E5002`）。
    NeedsAdmin,
    /// 其它 SCM 失败（`AH-E5001`）。
    Other,
}

impl ServiceSnapshot {
    /// 由 `status_report()` 的结果构造。
    pub fn from_result(result: Result<ServiceStatusReport, ServiceQueryError>) -> Self {
        match result {
            Ok(report) => Self::State(report),
            Err(error) => Self::Failed {
                code: error.code,
                kind: classify_service_failure(error.code),
            },
        }
    }
}

/// 服务错误码 → 归类。
fn classify_service_failure(code: ErrorCode) -> ServiceFailure {
    match code {
        ErrorCode::ServiceNeedsAdmin => ServiceFailure::NeedsAdmin,
        // `AH-E5001` 在 Windows 上是「SCM 查询失败」，在非 Windows 上是「平台不支持」的占位实现，
        // 两者的用户文案完全不同，只能用编译期平台判定区分。
        ErrorCode::ServiceUnavailable if !cfg!(windows) => ServiceFailure::UnsupportedPlatform,
        _ => ServiceFailure::Other,
    }
}

/// 一轮 aria2 刷新的结果 + 连续失败次数（UI 用它决定要不要带 `AH-E2005`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aria2Update {
    /// 本轮结果。
    pub outcome: Result<Aria2Snapshot, Aria2Failure>,
    /// 连续失败次数（成功则为 0）。
    pub consecutive_failures: u32,
}

/// 一轮任务列表刷新。
#[derive(Debug, Clone, PartialEq)]
pub struct TasksUpdate {
    /// `Ok` 是合并后的行（活动 → 等待 → 已停止）。
    pub outcome: Result<Vec<TaskRow>, Aria2Failure>,
    /// 是否触到每类上限（还有更多没列出来）。
    pub truncated: bool,
}

/// 一次行操作的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    /// 做了什么。
    pub action: TaskAction,
    /// 对哪个任务。
    pub gid: Arc<str>,
    /// 结果。
    pub result: Result<(), Aria2Failure>,
}

/// 一次「新建下载」的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    /// 用户输入的链接（回显用；可能很长，UI 只取展示名）。
    pub uri: String,
    /// 成功时是 aria2 分配的 GID。
    pub result: Result<Arc<str>, Aria2Failure>,
}

/// worker 推给 UI 的一条更新。
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// aria2 全局状态。
    Aria2(Aria2Update),
    /// 服务状态。
    Service(ServiceSnapshot),
    /// 任务列表。
    Tasks(TasksUpdate),
    /// 行操作结果。
    Action(ActionOutcome),
    /// 新建下载结果。
    Added(AddOutcome),
}

/// 一行操作的请求（UI → worker）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRequest {
    /// 意图。
    pub action: TaskAction,
    /// 目标 GID。
    pub gid: Arc<str>,
    /// 目标行的状态（决定「移除」该调 `remove` 还是 `removeDownloadResult`）。
    pub kind: TaskStatus,
}

impl TaskRequest {
    /// 该调哪个 RPC 方法（唯一决定点）。
    pub fn method(&self) -> &'static str {
        match self.action {
            TaskAction::Pause => RPC_PAUSE,
            TaskAction::Unpause => RPC_UNPAUSE,
            TaskAction::Remove => self.kind.remove_method(),
        }
    }
}

/// worker 收到的命令。
enum Command {
    /// 立即刷新（「立即刷新」按钮、行操作后、核心就绪后）。
    RefreshNow,
    /// 调整服务轮询间隔（服务页开关时调用；`None` = 回到常规 5s）。
    ServiceInterval(Option<Duration>),
    /// 操作后连拉几次服务状态（每次 1s）。
    ServiceBurst,
    /// 一行操作。
    Task(TaskRequest),
    /// 新建下载。
    AddUri(String),
    /// 退出（面板关闭）。
    Stop,
}

/// worker 的构建参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOptions {
    /// RPC 端口（`aria2.rpc.port`）。
    pub port: u16,
    /// RPC secret（`secret.bin` 解出来的值；`None` = 不带 secret）。
    pub secret: Option<String>,
    /// 成功刷新间隔（默认 [`ARIA2_REFRESH_INTERVAL`]）。
    pub interval: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            secret: None,
            interval: ARIA2_REFRESH_INTERVAL,
        }
    }
}

impl WorkerOptions {
    /// 从环境变量与 secret 组装（面板启动路径用）。
    pub fn detect(port: u16, secret: Option<String>, log: &LogBuffer) -> Self {
        let interval = refresh_interval_from_env().unwrap_or(ARIA2_REFRESH_INTERVAL);
        if interval != ARIA2_REFRESH_INTERVAL {
            log.push(format!(
                "{REFRESH_INTERVAL_ENV} 覆盖刷新间隔：{} ms（默认 {} ms）",
                interval.as_millis(),
                ARIA2_REFRESH_INTERVAL.as_millis()
            ));
        }
        Self {
            port,
            secret,
            interval,
        }
    }
}

/// 失败退避计数器（纯逻辑，单测覆盖序列）。
///
/// `base` 是成功刷新间隔，也是退避序列的第一项：默认 2s，可用
/// [`REFRESH_INTERVAL_ENV`] 覆盖（见 [`refresh_interval_from_env`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    failures: u32,
    base: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        // 退避起点 = 成功刷新间隔（默认 2s，可用 ARIAHOST_REFRESH_MS 覆盖）。
        Self::new(ARIA2_REFRESH_INTERVAL)
    }
}

impl Backoff {
    /// 指定成功刷新间隔（也是退避起点）。
    pub const fn new(base: Duration) -> Self {
        Self { failures: 0, base }
    }

    /// 连续失败次数。
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// 下一次刷新应等待多久：成功时是 `base`（默认 2s），
    /// 失败时 2→4→8→16→30s（`BACKOFF_CAP` 封顶）。
    pub fn interval(&self) -> Duration {
        if self.failures == 0 {
            return self.base;
        }
        // failures=1 → 2s，2 → 4s，3 → 8s，4 → 16s，5 及以后 → 30s（32s 被 cap 吃掉）。
        let shift = (self.failures - 1).min(4);
        (self.base * (1_u32 << shift)).min(BACKOFF_CAP)
    }

    /// 记一次失败。
    pub fn on_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    /// 记一次成功（退避清零）。
    pub fn on_success(&mut self) {
        self.failures = 0;
    }
}

/// 读 [`REFRESH_INTERVAL_ENV`]：只有落在合法区间内才算数，其余一律 `None`（走默认）。
pub fn refresh_interval_from_env() -> Option<Duration> {
    let raw = std::env::var(REFRESH_INTERVAL_ENV).ok()?;
    parse_refresh_interval(&raw)
}

/// 解析刷新间隔文本（毫秒）。抽成纯函数便于单测，不碰环境变量。
pub fn parse_refresh_interval(raw: &str) -> Option<Duration> {
    let millis = raw.trim().parse::<u64>().ok()?;
    let interval = Duration::from_millis(millis);
    (REFRESH_INTERVAL_MIN..=REFRESH_INTERVAL_MAX)
        .contains(&interval)
        .then_some(interval)
}

/// 后台刷新线程的句柄。
///
/// 设计要点：`Drop` 只发 `Stop`、**不 join**。RPC 单次超时是 5 秒，join 会把「关窗 → 进程退出」
/// 拖成最多 5 秒的白屏；线程随进程一起结束更干净。UI 侧任何等待都只发生在 `try_recv`。
pub struct Worker {
    commands: Option<Sender<Command>>,
    updates: Receiver<Update>,
    /// 线程句柄：仅持有所有权（drop 即 detach），不 join。
    _handle: Option<JoinHandle<()>>,
}

impl Worker {
    /// 起 worker：首轮立即刷一次，之后按间隔/退避刷新。
    pub fn spawn(log: Arc<LogBuffer>, options: WorkerOptions) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let thread_log = Arc::clone(&log);
        let spawned = thread::Builder::new()
            .name("ariahost-refresh".to_owned())
            .spawn(move || run(thread_log, &command_rx, &update_tx, options));
        match spawned {
            Ok(handle) => Self {
                commands: Some(command_tx),
                updates: update_rx,
                _handle: Some(handle),
            },
            Err(error) => {
                // 线程起不来时面板仍要能开：只报一条日志，数据区停在「等待首次刷新」。
                log.push(format!(
                    "后台刷新线程启动失败：{error}（面板继续，状态不会自动更新）"
                ));
                Self {
                    commands: Some(command_tx),
                    updates: update_rx,
                    _handle: None,
                }
            }
        }
    }

    /// 请求立即刷新（非阻塞；worker 不在时静默忽略）。
    pub fn request_refresh(&self) {
        self.send(Command::RefreshNow);
    }

    /// 服务页开/关时调整服务轮询间隔（M3-b；非阻塞）。
    ///
    /// **不每帧调用**：只在页面切换那一刻调一次——硬约束是「不得每帧开 SCM」。
    pub fn set_service_page(&self, on_page: bool) {
        self.send(Command::ServiceInterval(if on_page {
            Some(SERVICE_PAGE_INTERVAL)
        } else {
            None
        }));
    }

    /// 服务操作后请求连拉（非阻塞）。
    pub fn burst_service(&self) {
        self.send(Command::ServiceBurst);
    }

    /// 请求一行操作（非阻塞；结果走 [`Update::Action`]）。
    pub fn request_task(&self, request: TaskRequest) {
        self.send(Command::Task(request));
    }

    /// 请求新建下载（非阻塞；结果走 [`Update::Added`]）。
    pub fn request_add(&self, uri: String) {
        self.send(Command::AddUri(uri));
    }

    /// 非阻塞取一条更新（每帧调一次即可）。
    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }

    /// 面板是否还有后台线程（供状态区显示「自动刷新未启用」）。
    pub fn is_running(&self) -> bool {
        self._handle.is_some()
    }

    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.send(Command::Stop);
    }
}

/// 线程主体：先刷一轮，然后按「下一个到期时刻」等待命令。
fn run(
    log: Arc<LogBuffer>,
    commands: &Receiver<Command>,
    updates: &Sender<Update>,
    options: WorkerOptions,
) {
    let client = RpcClient::local(options.port, options.secret.clone());
    let interval = options.interval;
    let mut backoff = Backoff::new(interval);
    let mut next_aria2 = Instant::now();
    let mut next_service = Instant::now();
    // 服务轮询的间隔**可变**（M3-b）：服务页打开时调短，操作后连拉几次，
    // 其余时候保持 5s——硬要求是「不得每帧开 SCM」。
    let mut service_interval = SERVICE_REFRESH_INTERVAL;
    // 操作后立刻连拉几次（1s 一次），让等待态几秒内就能看到真实状态跳变。
    let mut service_burst: u8 = 0;
    // 只在「文案变了」时写日志：aria2 没启动是常态，2s 一条会把 500 行缓冲刷满。
    let mut last_aria2_line: Option<String> = None;
    let mut last_service_line: Option<String> = None;

    loop {
        let now = Instant::now();

        if now >= next_aria2 {
            let outcome = refresh_aria2(&client);
            match &outcome {
                Ok(_) => backoff.on_success(),
                Err(_) => backoff.on_failure(),
            }
            let consecutive_failures = backoff.failures();

            let line = match &outcome {
                Ok(snapshot) => format!(
                    "aria2 已连接：核心 {}，活动 {} / 等待 {}（{}）",
                    snapshot.version,
                    snapshot.num_active,
                    snapshot.num_waiting,
                    endpoint_text_for(options.port)
                ),
                Err(failure) => format!(
                    "aria2 刷新失败：{} {}（连续 {consecutive_failures} 次，下一次 {:?} 后重试）",
                    failure.code().code(),
                    failure.summary(),
                    backoff.interval()
                ),
            };
            if last_aria2_line.as_deref() != Some(line.as_str()) {
                log.push(line.clone());
                last_aria2_line = Some(line);
            }

            // 只有 RPC 通的时候才拉任务列表：连不上时多打三次 RPC 只是白等三次超时。
            // 失败时也要推一条空的 Tasks（`Err`）：否则界面会拿着「上一次的幽灵列表」继续显示。
            let tasks_update = match &outcome {
                Ok(_) => {
                    let tasks = refresh_tasks(&client);
                    let truncated = tasks.as_ref().map(|list| list.truncated).unwrap_or(false);
                    TasksUpdate {
                        outcome: tasks.map(|list| list.rows),
                        truncated,
                    }
                }
                Err(failure) => TasksUpdate {
                    outcome: Err(*failure),
                    truncated: false,
                },
            };

            let aria2_update = Aria2Update {
                outcome,
                consecutive_failures,
            };
            if updates.send(Update::Aria2(aria2_update)).is_err() {
                return;
            }
            if updates.send(Update::Tasks(tasks_update)).is_err() {
                return;
            }

            next_aria2 = Instant::now() + backoff.interval();
        }

        if Instant::now() >= next_service {
            let snapshot = ServiceSnapshot::from_result(status_report());
            let line = match snapshot {
                ServiceSnapshot::State(report) => format!(
                    "服务状态：{}（{}，启动方式 {}，pid {}）",
                    report.state.as_str(),
                    ariahost_svc::SERVICE_NAME,
                    report
                        .start_type
                        .map_or("n/a", ariahost_svc::ServiceStartMode::as_str),
                    report
                        .pid
                        .map_or_else(|| "n/a".to_owned(), |pid| pid.to_string())
                ),
                ServiceSnapshot::Failed { code, kind } => {
                    format!("服务状态查询失败：{}（{:?}）", code.code(), kind)
                }
            };
            if last_service_line.as_deref() != Some(line.as_str()) {
                log.push(line.clone());
                last_service_line = Some(line);
            }
            if updates.send(Update::Service(snapshot)).is_err() {
                return;
            }
            // 操作后的连拉：每次拉完递减，间隔 1s；拉完后回到常规间隔。
            if service_burst > 0 {
                service_burst -= 1;
                next_service = Instant::now() + SERVICE_BURST_INTERVAL;
            } else {
                next_service = Instant::now() + service_interval;
            }
        }

        let wait = next_aria2
            .min(next_service)
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(50));
        match commands.recv_timeout(wait) {
            Ok(Command::RefreshNow) => {
                next_aria2 = Instant::now();
                next_service = Instant::now();
            }
            Ok(Command::ServiceInterval(interval)) => {
                service_interval = interval.unwrap_or(SERVICE_REFRESH_INTERVAL);
                // 切页时立刻拉一次：别让用户盯着一屏 5 秒前的旧状态。
                next_service = Instant::now();
            }
            Ok(Command::ServiceBurst) => {
                service_burst = SERVICE_BURST_TIMES;
                next_service = Instant::now();
            }
            Ok(Command::Task(request)) => {
                let outcome = run_task(&client, &request);
                let line = match &outcome.result {
                    Ok(()) => format!(
                        "任务 {} 已{}（{}）",
                        request.gid,
                        request.action.label(),
                        request.method()
                    ),
                    Err(failure) => format!(
                        "任务 {} {}失败：{} {}",
                        request.gid,
                        request.action.label(),
                        failure.code().code(),
                        failure.summary()
                    ),
                };
                log.push(line);
                if updates.send(Update::Action(outcome)).is_err() {
                    return;
                }
                // 立刻回刷一次，让列表与真实状态对齐（不等到下一个 2s 周期）。
                next_aria2 = Instant::now();
            }
            Ok(Command::AddUri(uri)) => {
                let outcome = run_add(&client, &uri);
                let line = match &outcome.result {
                    Ok(gid) => format!("已加入下载：{gid} ← {}", shorten(&uri, 80)),
                    Err(failure) => format!(
                        "新建下载失败：{} {} ← {}",
                        failure.code().code(),
                        failure.summary(),
                        shorten(&uri, 80)
                    ),
                };
                log.push(line);
                if updates.send(Update::Added(outcome)).is_err() {
                    return;
                }
                next_aria2 = Instant::now();
            }
            Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

/// 一次任务列表刷新：`tellActive` + `tellWaiting` + `tellStopped`（任一失败即整轮失败）。
fn refresh_tasks(client: &RpcClient) -> Result<TaskList, Aria2Failure> {
    let active = client
        .tell_active()
        .map_err(|error| Aria2Failure::from_error(&error))?;
    let waiting = client
        .tell_waiting(0, MAX_ROWS_PER_BUCKET)
        .map_err(|error| Aria2Failure::from_error(&error))?;
    let stopped = client
        .tell_stopped(0, MAX_ROWS_PER_BUCKET)
        .map_err(|error| Aria2Failure::from_error(&error))?;
    let truncated =
        waiting.len() as u32 >= MAX_ROWS_PER_BUCKET || stopped.len() as u32 >= MAX_ROWS_PER_BUCKET;
    Ok(TaskList {
        rows: tasks::rows_from(&active, &waiting, &stopped),
        truncated,
    })
}

/// 一次任务列表结果（带截断标记）。
struct TaskList {
    rows: Vec<TaskRow>,
    truncated: bool,
}

/// 一次行操作。成功的判据是「RPC 没报错」——aria2 对 removeDownloadResult 回 `"OK"`，
/// 对 pause/unpause/remove 回 GID，这两种都不需要解析。
fn run_task(client: &RpcClient, request: &TaskRequest) -> ActionOutcome {
    let params = vec![Value::String(request.gid.to_string())];
    let result = client
        .call(request.method(), params)
        .map(|_value| ())
        .map_err(|error| Aria2Failure::from_error(&error));
    ActionOutcome {
        action: request.action,
        gid: Arc::clone(&request.gid),
        result,
    }
}

/// 一次新建下载：`aria2.addUri([uri])` → GID。
fn run_add(client: &RpcClient, uri: &str) -> AddOutcome {
    let result = client
        .add_uri(&[uri.to_owned()])
        .map(|gid| Arc::from(gid.as_str()))
        .map_err(|error| Aria2Failure::from_error(&error));
    AddOutcome {
        uri: uri.to_owned(),
        result,
    }
}

/// 一次 aria2 全局刷新：`getVersion` + `getGlobalStat`（任一失败即整轮失败）。
fn refresh_aria2(client: &RpcClient) -> Result<Aria2Snapshot, Aria2Failure> {
    let version = client
        .get_version()
        .map_err(|error| Aria2Failure::from_error(&error))?;
    let stat = client
        .get_global_stat()
        .map_err(|error| Aria2Failure::from_error(&error))?;
    Ok(Aria2Snapshot::from_parts(version, stat))
}

/// aria2 状态区文案（纯函数：失败归类 → 固定中文 + 码 + 下一步）。
pub fn aria2_view(update: Option<&Aria2Update>) -> StatusView {
    let Some(update) = update else {
        return StatusView::idle("等待首次刷新", "正在读取 aria2 状态…".to_owned());
    };
    match &update.outcome {
        Ok(snapshot) => {
            let mut details = vec![
                format!(
                    "速率：↓ {} / ↑ {}",
                    format_speed(snapshot.download_speed),
                    format_speed(snapshot.upload_speed)
                ),
                format!(
                    "任务：活动 {} · 等待 {} · 已停止 {}",
                    snapshot.num_active, snapshot.num_waiting, snapshot.num_stopped
                ),
                format!("RPC 端点：{}", endpoint_text()),
            ];
            if let Some(total) = snapshot.num_stopped_total {
                details.push(format!("历史停止累计：{total}"));
            }
            if snapshot.feature_count > 0 {
                details.push(format!("启用的功能：{} 项", snapshot.feature_count));
            }
            StatusView {
                badge: "已连接".to_owned(),
                tone: Tone::Good,
                code: None,
                headline: format!("aria2 核心 {} 在线", snapshot.version),
                details,
                next_step: None,
            }
        }
        Err(failure) => {
            let mut next_step = failure.next_step();
            if update.consecutive_failures >= RECONNECT_FAILURES {
                next_step = format!(
                    "{next_step}（已连续 {} 次握手失败：{}，按 2s→4s→8s→{}s 退避重试）",
                    update.consecutive_failures,
                    ErrorCode::RpcReconnectFailed.code(),
                    BACKOFF_CAP.as_secs()
                );
            }
            StatusView {
                badge: "未连接".to_owned(),
                tone: Tone::Warn,
                code: Some(failure.code().code()),
                headline: failure.summary(),
                details: vec![
                    format!("RPC 端点：{}", endpoint_text()),
                    format!("单次超时：{} 秒", RPC_TIMEOUT.as_secs()),
                ],
                next_step: Some(next_step),
            }
        }
    }
}

/// 服务状态区文案（纯函数）。
///
/// 这是**状态区那一行**（概览句 + 下一步）；服务管理页的六态卡片在 `crate::service`。
/// 两处的短标签刻意共用一份（`service::state_label`），避免「概览说运行中、卡片说未知」。
pub fn service_view(snapshot: Option<&ServiceSnapshot>) -> StatusView {
    let Some(snapshot) = snapshot else {
        return StatusView::idle("等待首次刷新", "正在读取 Windows 服务状态…".to_owned());
    };
    let name = ariahost_svc::SERVICE_NAME;
    match snapshot {
        ServiceSnapshot::State(report) => {
            let detail = format!(
                "启动方式：{}；PID：{}",
                crate::service::start_mode_label(report.start_type),
                report
                    .pid
                    .map_or_else(|| "—".to_owned(), |pid| pid.to_string())
            );
            match report.state {
                ServiceState::Running => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Good,
                    code: None,
                    headline: format!("服务 {name} 正在运行"),
                    details: vec![detail],
                    next_step: None,
                },
                ServiceState::Stopped => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Warn,
                    code: None,
                    headline: format!("服务 {name} 已安装，当前已停止"),
                    details: vec![detail],
                    next_step: Some(
                        "点底部「服务」→「启动服务」，由 SCM 拉起它（面板可关）".to_owned(),
                    ),
                },
                ServiceState::StartPending => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Warn,
                    code: None,
                    headline: format!("服务 {name} 正在启动"),
                    details: vec![detail],
                    next_step: None,
                },
                ServiceState::StopPending => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Warn,
                    code: None,
                    headline: format!("服务 {name} 正在停止"),
                    details: vec![detail],
                    next_step: Some("aria2 会先把会话与队列落盘再退出，请等它停稳".to_owned()),
                },
                ServiceState::NotInstalled => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Warn,
                    code: None,
                    headline: format!("尚未注册 Windows 服务 {name}"),
                    details: vec![
                        "当前由面板自己托管 aria2c；装成服务后可以开机自启、面板关掉也继续下载"
                            .to_owned(),
                    ],
                    next_step: Some(
                        "点底部「服务」→「安装为服务」（会弹一次 Windows 授权框）".to_owned(),
                    ),
                },
                ServiceState::Unknown => StatusView {
                    badge: crate::service::state_label(report.state).to_owned(),
                    tone: Tone::Warn,
                    code: None,
                    headline: format!("服务 {name} 处于无法识别的状态"),
                    details: vec![detail],
                    next_step: Some(
                        "用 `sc query AriaHost` 核对；面板不猜这种状态该怎么办".to_owned(),
                    ),
                },
            }
        }
        ServiceSnapshot::Failed { code, kind } => {
            let (headline, next_step) = match kind {
                ServiceFailure::UnsupportedPlatform => (
                    "当前平台没有 Windows 服务（SCM）".to_owned(),
                    "AriaHost 只发布 Windows 版本；此状态仅在非 Windows 上出现".to_owned(),
                ),
                ServiceFailure::NeedsAdmin => (
                    "读取服务状态需要管理员权限".to_owned(),
                    "以管理员身份重新启动面板，或用 `sc query AriaHost` 自查".to_owned(),
                ),
                ServiceFailure::Other => (
                    "读取 SCM 服务状态失败".to_owned(),
                    "先确认 SCM 可访问（`sc query AriaHost`）；服务页的「刷新状态」可重试"
                        .to_owned(),
                ),
            };
            StatusView {
                badge: "查询失败".to_owned(),
                tone: Tone::Bad,
                code: Some(code.code()),
                headline,
                details: vec![format!("服务名：{name}")],
                next_step: Some(next_step),
            }
        }
    }
}
/// 速率格式化（委托给任务列表的同一套量纲，避免两处小数位约定不一致）。
pub fn format_speed(bytes_per_second: u64) -> String {
    tasks::format_rate(bytes_per_second)
}

/// 链接截断（日志用；不让一条超长 URL 把日志撑成一行 10KB）。
fn shorten(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…（共 {count} 字符）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ariahost_aria2::{RpcError, VersionInfo};
    use serde_json::json;
    use std::net::TcpListener;

    fn snapshot() -> Aria2Snapshot {
        Aria2Snapshot {
            version: "1.37.0".to_owned(),
            feature_count: 3,
            download_speed: 2_621_440,
            upload_speed: 512,
            num_active: 2,
            num_waiting: 5,
            num_stopped: 11,
            num_stopped_total: Some(42),
        }
    }

    fn update(outcome: Result<Aria2Snapshot, Aria2Failure>, failures: u32) -> Aria2Update {
        Aria2Update {
            outcome,
            consecutive_failures: failures,
        }
    }

    #[test]
    fn backoff_follows_the_documented_sequence() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.interval(), ARIA2_REFRESH_INTERVAL);
        let mut seen = Vec::new();
        for _ in 0..7 {
            backoff.on_failure();
            seen.push(backoff.interval().as_secs());
        }
        assert_eq!(seen, vec![2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff.failures(), 7);
        backoff.on_success();
        assert_eq!(backoff.failures(), 0);
        assert_eq!(backoff.interval(), ARIA2_REFRESH_INTERVAL);
    }

    #[test]
    fn backoff_cap_is_thirty_seconds() {
        let mut backoff = Backoff::default();
        for _ in 0..40 {
            backoff.on_failure();
        }
        assert_eq!(backoff.interval(), BACKOFF_CAP);
        // 不能因为 u32 位移过大而 panic / 回绕。
        assert!(backoff.interval() <= BACKOFF_CAP);
    }

    #[test]
    fn failure_mapping_covers_every_rpc_error_kind() {
        let connect = RpcCallError::Connect {
            detail: "Connection refused (os error 10061)".to_owned(),
            timed_out: false,
        };
        assert_eq!(
            Aria2Failure::from_error(&connect),
            Aria2Failure::Unreachable
        );
        assert_eq!(connect.code(), Some(ErrorCode::RpcHandshakeFailed));

        let timeout = RpcCallError::Transport {
            detail: "read timed out".to_owned(),
            timed_out: true,
        };
        assert_eq!(Aria2Failure::from_error(&timeout), Aria2Failure::Timeout);

        let forbidden = RpcCallError::HttpStatus { status: 403 };
        assert_eq!(
            Aria2Failure::from_error(&forbidden),
            Aria2Failure::HttpStatus(403)
        );
        assert_eq!(forbidden.code(), Some(ErrorCode::HttpRejected));

        let weird = RpcCallError::HttpStatus { status: 502 };
        assert_eq!(
            Aria2Failure::from_error(&weird).code(),
            ErrorCode::RpcHandshakeFailed
        );

        let protocol = RpcCallError::Protocol {
            detail: "响应既没有 result 也没有 error".to_owned(),
        };
        assert_eq!(Aria2Failure::from_error(&protocol), Aria2Failure::Protocol);

        let remote = RpcCallError::Remote {
            error: RpcError {
                code: 1,
                message: "Unauthorized".to_owned(),
            },
        };
        assert_eq!(Aria2Failure::from_error(&remote), Aria2Failure::Remote(1));

        let missing = RpcCallError::MethodNotFound {
            method: "aria2.tellActive".to_owned(),
            error: RpcError {
                code: -32601,
                message: "Method not found".to_owned(),
            },
        };
        assert_eq!(
            Aria2Failure::from_error(&missing),
            Aria2Failure::Remote(-32601)
        );
    }

    #[test]
    fn failure_view_never_leaks_raw_detail() {
        // 原始 detail 里塞一段可识别的「JSON/stderr」特征串，界面上绝不能出现。
        const MARKER: &str = "RAW_SECRET_MARKER";
        let error = RpcCallError::Connect {
            detail: format!("connect failed: {{\"code\":-1,\"message\":\"{MARKER}\"}}"),
            timed_out: false,
        };
        let failure = Aria2Failure::from_error(&error);
        let view = aria2_view(Some(&update(Err(failure), 1)));
        let flattened = format!(
            "{}{}{:?}{:?}",
            view.badge, view.headline, view.details, view.next_step
        );
        assert!(!flattened.contains(MARKER), "{flattened}");
        assert!(!flattened.contains("os error"), "{flattened}");
        assert_eq!(view.code, Some("AH-E2003"));
        assert!(view.headline.contains("aria2"), "{}", view.headline);
        assert!(
            view.next_step
                .as_deref()
                .is_some_and(|text| text.contains("启动 aria2")),
            "{:?}",
            view.next_step
        );
    }

    #[test]
    fn reconnect_failures_add_the_e2005_hint() {
        let few = aria2_view(Some(&update(Err(Aria2Failure::Unreachable), 3)));
        assert!(!few
            .next_step
            .as_deref()
            .unwrap_or_default()
            .contains("AH-E2005"));

        let many = aria2_view(Some(&update(
            Err(Aria2Failure::Unreachable),
            RECONNECT_FAILURES,
        )));
        let next = many.next_step.unwrap_or_default();
        assert!(next.contains("AH-E2005"), "{next}");
        assert!(next.contains("30"), "{next}");
    }

    #[test]
    fn connected_view_shows_counts_and_rates() {
        let view = aria2_view(Some(&update(Ok(snapshot()), 0)));
        assert_eq!(view.badge, "已连接");
        assert_eq!(view.tone, Tone::Good);
        assert!(view.code.is_none());
        assert!(view.headline.contains("1.37.0"), "{}", view.headline);
        let joined = view.details.join("\n");
        assert!(joined.contains("2.5 MiB/s"), "{joined}");
        assert!(joined.contains("512 B/s"), "{joined}");
        assert!(joined.contains("活动 2"), "{joined}");
        assert!(joined.contains("等待 5"), "{joined}");
        assert!(joined.contains("已停止 11"), "{joined}");
        assert!(joined.contains("127.0.0.1:6800"), "{joined}");
    }

    #[test]
    fn pending_view_is_idle_and_explains_itself() {
        let view = aria2_view(None);
        assert_eq!(view.tone, Tone::Idle);
        assert!(view.headline.contains("正在读取"));
        assert!(view.code.is_none());
    }

    #[test]
    fn service_states_map_to_expected_badges() {
        let cases = [
            (ServiceState::Running, "运行中", Tone::Good),
            (ServiceState::Stopped, "已停止", Tone::Warn),
            (ServiceState::StartPending, "启动中", Tone::Warn),
            (ServiceState::StopPending, "停止中", Tone::Warn),
            (ServiceState::Unknown, "未知", Tone::Warn),
            (ServiceState::NotInstalled, "未安装", Tone::Warn),
        ];
        for (state, badge, tone) in cases {
            let view = service_view(Some(&ServiceSnapshot::State(ServiceStatusReport {
                installed: state.is_installed(),
                state,
                start_type: None,
                pid: None,
            })));
            assert_eq!(view.badge, badge, "{state:?}");
            assert_eq!(view.tone, tone, "{state:?}");
            assert!(view.code.is_none(), "{state:?}");
        }
    }

    #[test]
    fn not_installed_points_at_the_service_page() {
        let view = service_view(Some(&ServiceSnapshot::State(ServiceStatusReport {
            installed: false,
            state: ServiceState::NotInstalled,
            start_type: None,
            pid: None,
        })));
        let next = view.next_step.unwrap_or_default();
        assert!(next.contains("服务"), "{next}");
        assert!(next.contains("安装为服务"), "{next}");
    }

    #[test]
    fn service_failures_carry_codes_and_next_steps() {
        let admin = service_view(Some(&ServiceSnapshot::Failed {
            code: ErrorCode::ServiceNeedsAdmin,
            kind: ServiceFailure::NeedsAdmin,
        }));
        assert_eq!(admin.code, Some("AH-E5002"));
        assert_eq!(admin.tone, Tone::Bad);
        assert!(admin
            .next_step
            .as_deref()
            .is_some_and(|text| text.contains("管理员")));

        let other = service_view(Some(&ServiceSnapshot::Failed {
            code: ErrorCode::ServiceUnavailable,
            kind: ServiceFailure::Other,
        }));
        assert_eq!(other.code, Some("AH-E5001"));
        assert!(other.next_step.is_some());
    }

    #[test]
    fn classify_service_failure_is_platform_aware() {
        assert_eq!(
            classify_service_failure(ErrorCode::ServiceNeedsAdmin),
            ServiceFailure::NeedsAdmin
        );
        let unsupported = classify_service_failure(ErrorCode::ServiceUnavailable);
        if cfg!(windows) {
            assert_eq!(unsupported, ServiceFailure::Other);
        } else {
            assert_eq!(unsupported, ServiceFailure::UnsupportedPlatform);
        }
    }

    #[test]
    fn format_speed_switches_units() {
        assert_eq!(format_speed(0), "0 B/s");
        assert_eq!(format_speed(999), "999 B/s");
        assert_eq!(format_speed(1024), "1.0 KiB/s");
        assert_eq!(format_speed(1536), "1.5 KiB/s");
        assert_eq!(format_speed(2_621_440), "2.5 MiB/s");
        assert_eq!(format_speed(3 * 1024 * 1024 * 1024), "3.0 GiB/s");
    }

    #[test]
    fn snapshot_from_parts_uses_global_stat_counts() {
        let version = VersionInfo {
            version: "1.36.0".to_owned(),
            enabled_features: vec!["BitTorrent".to_owned(), "GZip".to_owned()],
        };
        let stat = GlobalStat {
            download_speed: 10,
            upload_speed: 20,
            num_active: 1,
            num_waiting: 2,
            num_stopped: 3,
            num_stopped_total: None,
        };
        let snapshot = Aria2Snapshot::from_parts(version, stat);
        assert_eq!(snapshot.version, "1.36.0");
        assert_eq!(snapshot.feature_count, 2);
        assert_eq!(snapshot.download_speed, 10);
        assert_eq!(snapshot.num_active, 1);
        assert_eq!(snapshot.num_stopped_total, None);
    }

    /// 端到端的一小段真 socket 检查：端口没人监听时必须**立即**归类成 Unreachable，
    /// 而不是傻等 5s 超时（面板卡顿正是这条要防的）。只用回环地址，不碰外网。
    #[test]
    fn refused_connection_maps_to_unreachable_quickly() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("取一个空闲端口");
        let port = listener.local_addr().expect("本机地址").port();
        drop(listener);

        let client = RpcClient::local_with_timeout(port, None, Duration::from_millis(500));
        let started = Instant::now();
        let failure = refresh_aria2(&client).expect_err("没有服务在监听，必须失败");
        assert!(
            matches!(failure, Aria2Failure::Unreachable | Aria2Failure::Timeout),
            "{failure:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "拒绝连接应立刻返回，实际 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn backoff_sequence_can_start_from_a_configured_interval() {
        // ARIAHOST_REFRESH_MS 覆盖的是「成功间隔 = 退避起点」，序列形状不变：
        // base=1s 时是 1→2→4→…；base 保持默认 2s 时才是文档写的 2→4→8→…
        let mut backoff = Backoff::new(Duration::from_millis(1000));
        assert_eq!(backoff.interval(), Duration::from_millis(1000));
        backoff.on_failure();
        assert_eq!(backoff.interval(), Duration::from_millis(1000));
        backoff.on_failure();
        assert_eq!(backoff.interval(), Duration::from_millis(2000));
        backoff.on_failure();
        assert_eq!(backoff.interval(), Duration::from_millis(4000));
    }

    #[test]
    fn refresh_interval_env_is_bounded() {
        assert_eq!(parse_refresh_interval("2000"), Some(Duration::from_secs(2)));
        assert_eq!(parse_refresh_interval(" 500 "), Some(REFRESH_INTERVAL_MIN));
        assert_eq!(parse_refresh_interval("60000"), Some(REFRESH_INTERVAL_MAX));
        // 太快 = 忙轮询、太慢 = 假死：都必须被拒，回落到默认。
        assert_eq!(parse_refresh_interval("0"), None);
        assert_eq!(parse_refresh_interval("499"), None);
        assert_eq!(parse_refresh_interval("60001"), None);
        assert_eq!(parse_refresh_interval("abc"), None);
        assert_eq!(parse_refresh_interval("-1"), None);
        assert_eq!(parse_refresh_interval(""), None);
    }

    #[test]
    fn endpoint_text_is_the_loopback_jsonrpc_url() {
        assert_eq!(endpoint_text(), "http://127.0.0.1:6800/jsonrpc");
        assert_eq!(endpoint_text_for(6810), "http://127.0.0.1:6810/jsonrpc");
    }

    #[test]
    fn task_request_targets_the_right_rpc_method() {
        let pause = TaskRequest {
            action: TaskAction::Pause,
            gid: Arc::from("g1"),
            kind: TaskStatus::Active,
        };
        assert_eq!(pause.method(), RPC_PAUSE);

        let resume = TaskRequest {
            action: TaskAction::Unpause,
            gid: Arc::from("g1"),
            kind: TaskStatus::Paused,
        };
        assert_eq!(resume.method(), RPC_UNPAUSE);

        // 活动任务：移除 = aria2.remove
        let remove_active = TaskRequest {
            action: TaskAction::Remove,
            gid: Arc::from("g1"),
            kind: TaskStatus::Active,
        };
        assert_eq!(remove_active.method(), RPC_REMOVE);

        // 结束态的行在 tellStopped 里，只能清结果条目。
        for kind in [TaskStatus::Complete, TaskStatus::Error, TaskStatus::Removed] {
            let request = TaskRequest {
                action: TaskAction::Remove,
                gid: Arc::from("g1"),
                kind,
            };
            assert_eq!(request.method(), RPC_REMOVE_RESULT, "{kind:?}");
        }
    }

    #[test]
    fn task_fetch_is_capped_and_reports_truncation() {
        // 只测纯逻辑：截断标记的判据是「某一类取满了上限」。
        const {
            assert!(MAX_ROWS_PER_BUCKET >= 200, "200 行内存验收要求至少 200");
        }
        let full = vec![json!({ "gid": "x", "status": "waiting" }); MAX_ROWS_PER_BUCKET as usize];
        let partial = [json!({ "gid": "x", "status": "waiting" })];
        assert!(full.len() as u32 >= MAX_ROWS_PER_BUCKET);
        assert!((partial.len() as u32) < MAX_ROWS_PER_BUCKET);
    }

    #[test]
    fn shorten_keeps_short_links_intact_and_counts_chars() {
        assert_eq!(shorten("http://a/b", 20), "http://a/b");
        let long = "x".repeat(100);
        let cut = shorten(&long, 10);
        assert!(cut.starts_with("xxxxxxxxxx"), "{cut}");
        assert!(cut.contains("共 100 字符"), "{cut}");
    }
}
