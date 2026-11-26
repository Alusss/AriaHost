//! 服务状态机与控制命令（**纯逻辑**，不含任何 SCM 调用）。
//!
//! 为什么单独一层：SCM 交互（`windows-service` 的 dispatcher / 控制处理器 / `CreateServiceW`）
//! 在 Linux 侧编译不到、也测不了；把「收到控制命令 → 该上报哪些状态」抽成纯函数，
//! 才能在 WSL 的 `cargo test` 里把 §5 的状态机逐条钉住，Windows 侧只剩一层哑胶水。
//!
//! 规格来源：架构 §5 服务生命周期与 §4.2.4 状态机；控制命令到状态的映射见 [`ServiceLoop::handle`]。

use std::sync::mpsc::Receiver;
use std::time::Duration;

use ariahost_common::time::utc_stamp;
use ariahost_common::ErrorCode;

use crate::log::{Level, LogSink};
use crate::ServiceError;

/// `StartPending` / `StopPending` 的 `wait_hint`：SCM 启动预算 30s（架构 §5 ST-01）。
pub const START_WAIT_HINT: Duration = Duration::from_secs(30);

/// 停止预算：`shutdown` 15s + `forceShutdown` 10s = ≤25s（架构 §5 ST-05）。
pub const STOP_WAIT_HINT: Duration = Duration::from_secs(25);

/// 崩溃退避：1s → 2s → 4s（架构 §5「restart backoff 1/2/4/8s」的前三档，第四次起交给 SCM 放弃）。
pub const FAILURE_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// 失败计数重置周期：1 小时无失败才清零（防「偶发一次就永久降级」）。
pub const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(3600);

/// 服务可能收到的控制命令（只保留本产品响应的四种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// SCM `StartService`。
    Start,
    /// SCM `ControlService(STOP)`。
    Stop,
    /// 系统关机（与 `Stop` 同处理）。
    Shutdown,
    /// SCM 催问状态：**不做事**（架构 §4.2.4 要求）。
    Interrogate,
}

impl ControlCommand {
    /// 日志/事件里的稳定文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Shutdown => "shutdown",
            Self::Interrogate => "interrogate",
        }
    }
}

/// 要把 aria2 托管起来的那一层（生产实现是 `ariahost_core::runtime`，单测用假件）。
pub trait Supervisor {
    /// 启动并等到就绪；成功返回探到的 aria2 版本（进日志/诊断）。
    fn start(&mut self) -> Result<String, SupervisorFailure>;
    /// 优雅停止（超时/强杀由实现决定，调用方只关心「停完没」）。
    fn stop(&mut self) -> Result<(), SupervisorFailure>;

    /// M5-e：写 `status.json` 用的**核心来源**（默认 `None`：「这个实现不报告来源」）。
    ///
    /// 为什么给默认实现：这个值只影响观测文件，不参与托管决策。给它默认值后，
    /// 既有的单测假件与 `scm_stub` 都不用改，也不会因为「没实现」而写错数据。
    fn core_source(&self) -> crate::status_file::CoreSource {
        crate::status_file::CoreSource::None
    }

    /// M5-e：当前 aria2c 子进程 PID（没在跑就是 `None`）。
    ///
    /// 每次调用都应**现读**（不是构造时缓存），否则服务停止后文件里会留一个已死的 PID。
    fn aria2_pid(&self) -> Option<u32> {
        None
    }
}

/// 托管失败（带既有错误码 + 可读原因）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorFailure {
    /// 既有错误码。
    pub code: ErrorCode,
    /// 可读原因（中文，进日志/事件）。
    pub hint: String,
}

impl SupervisorFailure {
    /// 构造。
    pub fn new(code: ErrorCode, hint: impl Into<String>) -> Self {
        Self {
            code,
            hint: hint.into(),
        }
    }
}

/// 需要上报给 SCM 的一条状态（Windows 胶水把它翻成 `ServiceStatus`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceReport {
    /// 正在启动。
    StartPending {
        /// SCM 等待预算。
        wait_hint: Duration,
    },
    /// 已就绪。
    Running {
        /// 探到的 aria2 版本（只用于日志）。
        aria2_version: String,
    },
    /// 正在停止。
    StopPending {
        /// SCM 等待预算。
        wait_hint: Duration,
    },
    /// 已停止；`exit_code` 非空表示「服务因某错误退出」（服务特定错误码）。
    Stopped {
        /// 服务特定退出码（0/`None` = 正常停止）。
        exit_code: Option<u32>,
    },
    /// 启动失败，随后按 Stopped 收尾。
    Failed {
        /// 既有错误码。
        code: ErrorCode,
        /// 可读原因。
        hint: String,
    },
}

impl ServiceReport {
    /// 该上报对应的服务态（测试与日志用）。
    pub const fn state_name(&self) -> &'static str {
        match self {
            Self::StartPending { .. } => "start-pending",
            Self::Running { .. } => "running",
            Self::StopPending { .. } => "stop-pending",
            Self::Stopped { .. } => "stopped",
            Self::Failed { .. } => "failed",
        }
    }
}

/// 服务当前态（与 [`crate::ServiceState`] 的 Running/StartPending/… 对齐，但去掉了 NotInstalled）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopState {
    /// 已停止。
    Stopped,
    /// 正在启动（只可能在本对象内部短暂出现）。
    Starting,
    /// 运行中。
    Running,
    /// 正在停止。
    Stopping,
}

/// 控制命令 → 状态上报序列的状态机。
pub struct ServiceLoop<S: Supervisor> {
    supervisor: S,
    state: LoopState,
}

impl<S: Supervisor> ServiceLoop<S> {
    /// 新建（初始态 `Stopped`：`service_main` 一进来就该是已停止，等 SCM 发 `Start`）。
    pub fn new(supervisor: S) -> Self {
        Self {
            supervisor,
            state: LoopState::Stopped,
        }
    }

    /// 当前态。
    pub fn state(&self) -> LoopState {
        self.state
    }

    /// 借一眼被托管的实现（M5-e：写状态文件时现取核心来源与 PID）。
    ///
    /// 只读借用：写状态文件**不改变**托管状态，也不允许借它去 `start`/`stop`。
    pub fn supervisor(&self) -> &S {
        &self.supervisor
    }

    /// 处理一条控制命令，返回**按顺序**要上报的状态列表。
    ///
    /// 映射表（架构 §5）：
    ///
    /// | 命令 | 当前态 | 上报序列 |
    /// |---|---|---|
    /// | `Start` | `Stopped` | `StartPending` → `Running`（成功）/ `Failed`（失败） |
    /// | `Start` | `Running`/`Starting` | 空（SCM 不会重发；重复 start 幂等） |
    /// | `Stop`/`Shutdown` | 任意 | `StopPending` → `Stopped` |
    /// | `Interrogate` | 任意 | 空（不做事） |
    ///
    /// 停止携带的 `exit_code` 取「最后一次失败」的服务特定码（正常停止为 `None`）。
    pub fn handle(&mut self, command: ControlCommand) -> Vec<ServiceReport> {
        match command {
            ControlCommand::Interrogate => Vec::new(),
            ControlCommand::Start => {
                if matches!(self.state, LoopState::Running | LoopState::Starting) {
                    return Vec::new();
                }
                self.state = LoopState::Starting;
                let mut reports = vec![ServiceReport::StartPending {
                    wait_hint: START_WAIT_HINT,
                }];
                match self.supervisor.start() {
                    Ok(version) => {
                        self.state = LoopState::Running;
                        reports.push(ServiceReport::Running {
                            aria2_version: version,
                        });
                    }
                    Err(failure) => {
                        self.state = LoopState::Stopped;
                        reports.push(ServiceReport::Failed {
                            code: failure.code,
                            hint: failure.hint,
                        });
                    }
                }
                reports
            }
            ControlCommand::Stop | ControlCommand::Shutdown => {
                self.state = LoopState::Stopping;
                let mut reports = vec![ServiceReport::StopPending {
                    wait_hint: STOP_WAIT_HINT,
                }];
                let exit_code = match self.supervisor.stop() {
                    Ok(()) => None,
                    // 停止失败也要报 Stopped（否则 SCM 会一直等）；把错误码塞进服务特定退出码。
                    Err(failure) => Some(exit_code_for(failure.code)),
                };
                self.state = LoopState::Stopped;
                reports.push(ServiceReport::Stopped { exit_code });
                reports
            }
        }
    }
}

/// 错误码 → 服务特定退出码（`AH-E5001` → `5001`）。
///
/// 为什么不直接用 Win32 码：`AH-Exxxx` 是产品自己的码表，用户在事件查看器里看到
/// 「服务特定错误 5001」能直接对上 `AH-E5001`，比裸 Win32 码可诊断。
pub fn exit_code_for(code: ErrorCode) -> u32 {
    code.code()
        .trim_start_matches("AH-E")
        .parse::<u32>()
        .unwrap_or(0)
}

/// 状态上报的去处（Windows 实现翻成 `ServiceStatus` 交给 SCM；单测用假件记账）。
pub trait StatusSink {
    /// 上报一条状态；失败返回可读原因（由调用方决定是致命还是仅记日志）。
    fn report(&self, report: &ServiceReport) -> Result<(), String>;
}

/// 一条上报的可读描述（进日志；用户贴 issue 时能直接看懂，不必翻译 SCM 术语）。
pub fn describe_report(report: &ServiceReport) -> String {
    match report {
        ServiceReport::StartPending { wait_hint } => {
            format!("StartPending（wait_hint {}s）", wait_hint.as_secs())
        }
        ServiceReport::Running { aria2_version } => {
            format!("Running（aria2c {aria2_version} 就绪）")
        }
        ServiceReport::StopPending { wait_hint } => {
            format!("StopPending（wait_hint {}s）", wait_hint.as_secs())
        }
        ServiceReport::Stopped { exit_code: None } => "Stopped".to_owned(),
        ServiceReport::Stopped {
            exit_code: Some(code),
        } => format!("Stopped（服务特定退出码 {code}）"),
        ServiceReport::Failed { code, hint } => format!("Failed（{}：{hint}）", code.code()),
    }
}

/// 状态文件写到哪里（`None` = 本形态不写状态文件，例如单测与前台自检）。
///
/// 只有服务形态才写：`%ProgramData%\AriaHost`（服务账户的 `%APPDATA%` 是 systemprofile，
/// 见 lib.rs 的平面说明）。
#[derive(Debug, Clone, Copy)]
pub struct StatusTarget<'a> {
    /// 目标目录（`%ProgramData%\AriaHost`）。
    pub dir: &'a std::path::Path,
    /// 服务二进制版本（写进 `service_version`）。
    pub service_version: &'a str,
}

/// 生命周期里写 `status.json` 的那一层（M5-e）。
///
/// 四个挂钩点（每个都写**当前时刻的真实值**，不缓存核心三项）：
///
/// | 点 | 时机 | 写出的内容 |
/// |---|---|---|
/// | 1 | 服务进入主循环、核心尚未拉起 | `core_source=none`、版本与 PID 为 `null` |
/// | 2 | 核心就绪（`Running` 已上报） | 真实来源 + 探到的版本 + 真实 PID |
/// | 3 | 核心已停（`Stopped` 已上报） | 回到 `none` |
/// | 4 | 收到停止命令、**核心还活着** | 仍是「有核心」（面板能看见停服前的最后一眼） |
///
/// `started_at` 是这四项里唯一的会话常量：一次服务启动只定一次，面板靠它区分「同一次启动」。
/// 它不属于「缓存旧值」——核心三项每次都是现取的。
struct StatusWriter<'a> {
    dir: &'a std::path::Path,
    service_version: &'a str,
    log: &'a dyn LogSink,
    started_at: String,
    /// 最近一次探到的核心版本（点 4 要写「仍有核心」时的版本；来源与 PID 仍现取）。
    core_version: Option<String>,
}

impl<'a> StatusWriter<'a> {
    fn new(target: StatusTarget<'a>, log: &'a dyn LogSink) -> Self {
        Self {
            dir: target.dir,
            service_version: target.service_version,
            log,
            started_at: utc_stamp(std::time::SystemTime::now()),
            core_version: None,
        }
    }

    /// 落一份快照并盖上「现在」。写失败只留 WARN（[`status_file::write_or_warn`] 内部处理）。
    fn commit(&mut self, status: crate::status_file::ServiceStatus) {
        let status = status.stamped_at(utc_stamp(std::time::SystemTime::now()));
        crate::status_file::write_or_warn(self.dir, &status, self.log);
    }

    /// 点 1（与启动失败时）：服务在跑或即将退出，但没有核心。
    fn core_absent(&mut self) {
        self.core_version = None;
        let status = crate::status_file::ServiceStatus::starting(
            self.service_version,
            self.started_at.clone(),
        );
        self.commit(status);
    }

    /// 点 2：核心就绪。
    fn core_ready(
        &mut self,
        source: crate::status_file::CoreSource,
        version: String,
        pid: Option<u32>,
    ) {
        self.core_version = Some(version.clone());
        let status = crate::status_file::ServiceStatus::starting(
            self.service_version,
            self.started_at.clone(),
        )
        .with_running_core(source, Some(version), pid);
        self.commit(status);
    }

    /// 点 4：马上要停核心——仍按「有核心」写，版本来源与 PID 现取。
    fn before_stop(&mut self, source: crate::status_file::CoreSource, pid: Option<u32>) {
        let status = crate::status_file::ServiceStatus::starting(
            self.service_version,
            self.started_at.clone(),
        )
        .with_running_core(source, self.core_version.clone(), pid);
        self.commit(status);
    }

    /// 点 3：核心已停。
    fn core_stopped(&mut self) {
        self.core_version = None;
        let status = crate::status_file::ServiceStatus::starting(
            self.service_version,
            self.started_at.clone(),
        );
        self.commit(status);
    }
}

/// 服务主循环的**驱动骨架**：启动一次，然后等停止命令。
///
/// 等价于 [`drive_service_with_status`] 且 `status = None`（不写状态文件）。
///
/// 为什么分成两个函数：既有的六个单测与 `--panel-selftest` 之类的前台路径不该被迫
/// 提供状态文件目标；新增能力用可选参数，不破坏既有契约。
pub fn drive_service<S: Supervisor>(
    supervisor: S,
    sink: &dyn StatusSink,
    commands: &Receiver<ControlCommand>,
    log: &dyn LogSink,
) -> Result<(), ServiceError> {
    drive_service_with_status(supervisor, sink, commands, log, None)
}

/// 服务主循环的**驱动骨架**，并可选地把生命周期写进 `status.json`（M5-e）。
///
/// 为什么放在这一层（而不是 Windows 胶水里）：这部分逻辑决定了「失败会不会卡住 SCM」，
/// 必须在 Linux 单测里覆盖。SCM 只负责提供 `commands` 通道与实现 [`StatusSink`]。
///
/// 行为：
/// 1. 先跑一次 `Start`（SCM 不会为「启动」发控制事件——`service_main` 被调用本身就是启动）；
/// 2. 启动失败 → 上报 `Failed` 并返回错误（SCM 会把服务标成已停止 + 特定退出码）；
/// 3. 启动成功 → 阻塞等 `Stop`/`Shutdown`，收到后按状态机上报并返回；
/// 4. 通道断掉（理论上只会发生在进程要退出时）→ 当作停止处理，绝不悬挂。
///
/// `log` 的写入**永不失败**（[`LogSink`] 的实现自己吞错误），所以它不参与错误传播。
/// 状态文件同理：写不出来只记 WARN，绝不影响服务能否 `RUNNING`。
pub fn drive_service_with_status<S: Supervisor>(
    supervisor: S,
    sink: &dyn StatusSink,
    commands: &Receiver<ControlCommand>,
    log: &dyn LogSink,
    status: Option<StatusTarget<'_>>,
) -> Result<(), ServiceError> {
    let mut state = ServiceLoop::new(supervisor);
    let mut writer = status.map(|target| StatusWriter::new(target, log));

    log.record(
        Level::Info,
        "服务主循环启动：先自启一次 aria2c（SCM 不为「启动」发控制事件，service_main 被调用本身就是启动）",
    );
    // 挂钩点 1：服务已进入主循环、核心还没拉起。
    if let Some(writer) = writer.as_mut() {
        writer.core_absent();
    }
    let startup = state.handle(ControlCommand::Start);
    report_all(sink, &startup, log).map_err(|detail| ServiceError::Unavailable {
        detail: format!("上报启动状态失败：{detail}"),
    })?;
    if let Some(ServiceReport::Failed { code, hint }) = startup.last() {
        // 启动失败也是「没有核心」的一种终态：落一份，面板下次能看到服务曾失败退出。
        if let Some(writer) = writer.as_mut() {
            writer.core_absent();
        }
        log.record(
            Level::Error,
            &format!("aria2c 起不来，服务以 {} 失败退出：{hint}", code.code()),
        );
        return Err(ServiceError::StartupFailed {
            code: *code,
            detail: hint.clone(),
        });
    }
    // 挂钩点 2：核心就绪（来源与 PID 现取，版本用刚上报的那个）。
    if let (Some(writer), Some(ServiceReport::Running { aria2_version })) =
        (writer.as_mut(), startup.last())
    {
        writer.core_ready(
            state.supervisor().core_source(),
            aria2_version.clone(),
            state.supervisor().aria2_pid(),
        );
    }

    loop {
        let command = match commands.recv() {
            Ok(command) => command,
            Err(_) => {
                log.record(
                    Level::Warn,
                    "控制通道已断开（进程正在退出）：按 stop 处理，不留悬挂状态",
                );
                ControlCommand::Stop
            }
        };
        log.record(Level::Info, &format!("收到控制命令 {}", command.as_str()));
        let stopping = matches!(command, ControlCommand::Stop | ControlCommand::Shutdown);
        // 挂钩点 4：服务 STOP **前**——核心还活着，按「有核心」写最后一眼。
        if stopping {
            if let Some(writer) = writer.as_mut() {
                writer.before_stop(
                    state.supervisor().core_source(),
                    state.supervisor().aria2_pid(),
                );
            }
        }
        let reports = state.handle(command);
        report_all(sink, &reports, log).map_err(|detail| ServiceError::Unavailable {
            detail: format!("上报停止状态失败：{detail}"),
        })?;
        if stopping {
            // 挂钩点 3：核心已停。
            if let Some(writer) = writer.as_mut() {
                writer.core_stopped();
            }
            log.record(Level::Info, "服务主循环退出（已上报 Stopped）");
            return Ok(());
        }
    }
}

/// 按顺序上报一串状态，逐条记日志（成功与失败都记，失败即中断）。
fn report_all(
    sink: &dyn StatusSink,
    reports: &[ServiceReport],
    log: &dyn LogSink,
) -> Result<(), String> {
    for report in reports {
        match sink.report(report) {
            Ok(()) => log.record(
                Level::Info,
                &format!("上报 {} 成功", describe_report(report)),
            ),
            Err(detail) => {
                log.record(
                    Level::Error,
                    &format!("上报 {} 失败：{detail}", describe_report(report)),
                );
                return Err(detail);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 假 Supervisor：可配置成功/失败，记录被调次数。
    #[derive(Default)]
    struct FakeSupervisor {
        start_result: Option<(ErrorCode, &'static str)>,
        stop_fails: bool,
        starts: u32,
        stops: u32,
        /// M5-e：写状态文件时报告的来源（默认为 `none`，与 trait 默认实现一致）。
        source: crate::status_file::CoreSource,
        /// 当前 PID（`start` 后变为 `Some(4242)`、`stop` 后清空——模拟真实生命周期）。
        pid: Option<u32>,
        /// 点 4 的取证钩子：`stop()` 一进来就把 `status.json` 原文抄到这个文件。
        ///
        /// 为什么用文件而不是字段：supervisor 被 move 进驱动循环后测试拿不回来，
        /// 而「STOP 前仍写着有核心」只有在这个瞬间才能观察到（紧接着会被点 3 覆盖）。
        stop_witness: Option<std::path::PathBuf>,
    }

    impl FakeSupervisor {
        fn ok() -> Self {
            Self::default()
        }

        fn failing(code: ErrorCode, hint: &'static str) -> Self {
            Self {
                start_result: Some((code, hint)),
                ..Self::default()
            }
        }
    }

    impl Supervisor for FakeSupervisor {
        fn start(&mut self) -> Result<String, SupervisorFailure> {
            self.starts += 1;
            match self.start_result {
                None => {
                    self.pid = Some(4242);
                    Ok("1.37.0".to_owned())
                }
                Some((code, hint)) => Err(SupervisorFailure::new(code, hint)),
            }
        }

        fn stop(&mut self) -> Result<(), SupervisorFailure> {
            self.stops += 1;
            // 点 4 的取证：此刻 `status.json` 应仍是「有核心」。
            if let Some(witness) = &self.stop_witness {
                if let Some(dir) = witness.parent() {
                    let live = crate::status_file::status_path(dir);
                    if let Ok(text) = std::fs::read_to_string(&live) {
                        let _ = std::fs::write(witness, text);
                    }
                }
            }
            self.pid = None;
            if self.stop_fails {
                Err(SupervisorFailure::new(
                    ErrorCode::ServiceUnavailable,
                    "停止超时",
                ))
            } else {
                Ok(())
            }
        }

        fn core_source(&self) -> crate::status_file::CoreSource {
            self.source
        }

        fn aria2_pid(&self) -> Option<u32> {
            self.pid
        }
    }

    fn names(reports: &[ServiceReport]) -> Vec<&'static str> {
        reports.iter().map(ServiceReport::state_name).collect()
    }

    #[test]
    fn start_reports_pending_then_running() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        assert_eq!(loop_.state(), LoopState::Stopped);

        let reports = loop_.handle(ControlCommand::Start);
        assert_eq!(names(&reports), vec!["start-pending", "running"]);
        assert_eq!(
            reports[0],
            ServiceReport::StartPending {
                wait_hint: START_WAIT_HINT
            },
            "启动预算必须是架构 §5 的 30s"
        );
        assert_eq!(
            reports[1],
            ServiceReport::Running {
                aria2_version: "1.37.0".to_owned()
            }
        );
        assert_eq!(loop_.state(), LoopState::Running);
    }

    #[test]
    fn start_failure_reports_failed_with_code_then_stays_stopped() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::failing(
            ErrorCode::Aria2StartTimeout,
            "aria2c 15s 未就绪",
        ));
        let reports = loop_.handle(ControlCommand::Start);
        assert_eq!(names(&reports), vec!["start-pending", "failed"]);
        match &reports[1] {
            ServiceReport::Failed { code, hint } => {
                assert_eq!(*code, ErrorCode::Aria2StartTimeout);
                assert!(hint.contains("15s"), "{hint}");
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
        assert_eq!(loop_.state(), LoopState::Stopped, "失败后必须回到可重试态");
    }

    #[test]
    fn stop_reports_pending_then_stopped() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        loop_.handle(ControlCommand::Start);
        let reports = loop_.handle(ControlCommand::Stop);
        assert_eq!(names(&reports), vec!["stop-pending", "stopped"]);
        assert_eq!(
            reports[0],
            ServiceReport::StopPending {
                wait_hint: STOP_WAIT_HINT
            },
            "停止预算必须是 15s+10s=25s"
        );
        assert_eq!(reports[1], ServiceReport::Stopped { exit_code: None });
        assert_eq!(loop_.state(), LoopState::Stopped);
    }

    #[test]
    fn shutdown_is_handled_exactly_like_stop() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        loop_.handle(ControlCommand::Start);
        let reports = loop_.handle(ControlCommand::Shutdown);
        assert_eq!(names(&reports), vec!["stop-pending", "stopped"]);
    }

    #[test]
    fn stop_failure_still_reports_stopped_with_service_exit_code() {
        let mut supervisor = FakeSupervisor::ok();
        supervisor.stop_fails = true;
        let mut loop_ = ServiceLoop::new(supervisor);
        loop_.handle(ControlCommand::Start);

        let reports = loop_.handle(ControlCommand::Stop);
        assert_eq!(names(&reports), vec!["stop-pending", "stopped"]);
        assert_eq!(
            reports[1],
            ServiceReport::Stopped {
                exit_code: Some(5001)
            },
            "停不下来也要报 Stopped，并把 AH-E5001 写进服务特定退出码"
        );
        assert_eq!(loop_.state(), LoopState::Stopped);
    }

    #[test]
    fn interrogate_does_nothing() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        assert!(loop_.handle(ControlCommand::Interrogate).is_empty());
        loop_.handle(ControlCommand::Start);
        assert!(loop_.handle(ControlCommand::Interrogate).is_empty());
        assert_eq!(loop_.state(), LoopState::Running, "Interrogate 不许改状态");
    }

    #[test]
    fn duplicate_start_while_running_is_ignored() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        loop_.handle(ControlCommand::Start);
        let again = loop_.handle(ControlCommand::Start);
        assert!(again.is_empty(), "运行中重复 start 不该再上报");
        assert_eq!(loop_.state(), LoopState::Running);
    }

    #[test]
    fn stop_then_start_works_again() {
        let mut loop_ = ServiceLoop::new(FakeSupervisor::ok());
        loop_.handle(ControlCommand::Start);
        loop_.handle(ControlCommand::Stop);
        let reports = loop_.handle(ControlCommand::Start);
        assert_eq!(names(&reports), vec!["start-pending", "running"]);
        assert_eq!(loop_.state(), LoopState::Running);
    }

    #[test]
    fn start_after_failure_is_allowed() {
        // 第一次失败，第二次仍会尝试（Failed 不是终态）。
        let mut loop_ = ServiceLoop::new(FakeSupervisor::failing(
            ErrorCode::Aria2NotFound,
            "没有 active.json",
        ));
        assert_eq!(names(&loop_.handle(ControlCommand::Start))[1], "failed");
        assert_eq!(names(&loop_.handle(ControlCommand::Start))[1], "failed");
        assert_eq!(loop_.state(), LoopState::Stopped);
    }

    #[test]
    fn exit_code_is_the_numeric_part_of_the_error_code() {
        assert_eq!(exit_code_for(ErrorCode::ServiceUnavailable), 5001);
        assert_eq!(exit_code_for(ErrorCode::ServiceNeedsAdmin), 5002);
        assert_eq!(exit_code_for(ErrorCode::Aria2StartTimeout), 2002);
        assert_eq!(exit_code_for(ErrorCode::CoreVersionTooOld), 6004);
    }

    #[test]
    fn control_command_strings_are_pinned() {
        assert_eq!(ControlCommand::Start.as_str(), "start");
        assert_eq!(ControlCommand::Stop.as_str(), "stop");
        assert_eq!(ControlCommand::Shutdown.as_str(), "shutdown");
        assert_eq!(ControlCommand::Interrogate.as_str(), "interrogate");
    }

    // ---- 驱动骨架：命令 → 上报序列（含失败与通道断开） ----

    /// 假 sink：把上报的名字记下来，可配置「上报失败」。
    struct FakeSink {
        seen: std::sync::Mutex<Vec<&'static str>>,
        fail_on: Option<&'static str>,
    }

    impl FakeSink {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
                fail_on: None,
            }
        }

        fn names(&self) -> Vec<&'static str> {
            self.seen.lock().expect("锁").clone()
        }
    }

    impl StatusSink for FakeSink {
        fn report(&self, report: &ServiceReport) -> Result<(), String> {
            let name = report.state_name();
            self.seen.lock().expect("锁").push(name);
            if self.fail_on == Some(name) {
                return Err("SCM 拒绝".to_owned());
            }
            Ok(())
        }
    }

    #[test]
    fn drive_service_start_then_stop_sequence() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("发停止");
        let sink = FakeSink::new();

        drive_service(FakeSupervisor::ok(), &sink, &receiver, &crate::log::NOOP)
            .expect("应正常结束");
        assert_eq!(
            sink.names(),
            vec!["start-pending", "running", "stop-pending", "stopped"]
        );
    }

    #[test]
    fn drive_service_interrogate_between_start_and_stop_is_harmless() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Interrogate).expect("催问");
        sender.send(ControlCommand::Stop).expect("停止");
        let sink = FakeSink::new();

        drive_service(FakeSupervisor::ok(), &sink, &receiver, &crate::log::NOOP)
            .expect("应正常结束");
        assert_eq!(
            sink.names(),
            vec!["start-pending", "running", "stop-pending", "stopped"],
            "Interrogate 不该产生上报"
        );
    }

    #[test]
    fn drive_service_returns_error_when_startup_fails() {
        let (_sender, receiver) = std::sync::mpsc::channel();
        let sink = FakeSink::new();

        let error = drive_service(
            FakeSupervisor::failing(ErrorCode::Aria2NotFound, "没有核心"),
            &sink,
            &receiver,
            &crate::log::NOOP,
        )
        .expect_err("启动失败必须返回错误");
        assert!(
            matches!(error, ServiceError::StartupFailed { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::Aria2NotFound, "保留上游码");
        assert_eq!(sink.names(), vec!["start-pending", "failed"]);
    }

    #[test]
    fn drive_service_treats_closed_channel_as_stop() {
        // 通道一关就退出（不会悬挂在 recv 上）。
        let (sender, receiver) = std::sync::mpsc::channel::<ControlCommand>();
        drop(sender);
        let sink = FakeSink::new();
        drive_service(FakeSupervisor::ok(), &sink, &receiver, &crate::log::NOOP)
            .expect("通道关闭当停止");
        assert_eq!(
            sink.names(),
            vec!["start-pending", "running", "stop-pending", "stopped"]
        );
    }

    #[test]
    fn drive_service_propagates_sink_failure() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("停止");
        let sink = FakeSink {
            seen: std::sync::Mutex::new(Vec::new()),
            fail_on: Some("running"),
        };
        let error = drive_service(FakeSupervisor::ok(), &sink, &receiver, &crate::log::NOOP)
            .expect_err("上报失败必须冒泡");
        assert!(
            matches!(error, ServiceError::Unavailable { .. }),
            "{error:?}"
        );
    }

    /// 录音机：把日志按 "级别 文本" 记下来，供断言。
    struct RecordingLog {
        lines: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingLog {
        fn new() -> Self {
            Self {
                lines: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().expect("锁").clone()
        }
    }

    impl LogSink for RecordingLog {
        fn record(&self, level: Level, message: &str) {
            self.lines
                .lock()
                .expect("锁")
                .push(format!("{} {message}", level.as_str()));
        }
    }

    #[test]
    fn drive_service_logs_commands_reports_and_exit() {
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("停止");
        let sink = FakeSink::new();
        let log = RecordingLog::new();

        drive_service(FakeSupervisor::ok(), &sink, &receiver, &log).expect("应正常结束");

        let lines = log.lines();
        let joined = lines.join("\n");
        for expected in [
            "INFO 服务主循环启动",
            "INFO 上报 StartPending（wait_hint 30s） 成功",
            "INFO 上报 Running（aria2c 1.37.0 就绪） 成功",
            "INFO 收到控制命令 stop",
            "INFO 上报 StopPending（wait_hint 25s） 成功",
            "INFO 上报 Stopped 成功",
            "INFO 服务主循环退出（已上报 Stopped）",
        ] {
            assert!(
                joined.contains(expected),
                "日志缺「{expected}」，实际：\n{joined}"
            );
        }
        // 顺序：启动 → 就绪 → 收到 stop → 停止 → 退出。
        let index_of = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("缺 {needle}"))
        };
        assert!(index_of("上报 Running") < index_of("收到控制命令 stop"));
        assert!(index_of("收到控制命令 stop") < index_of("服务主循环退出"));
    }

    #[test]
    fn drive_service_logs_startup_failure_at_error_level() {
        let (_sender, receiver) = std::sync::mpsc::channel();
        let log = RecordingLog::new();
        let _ = drive_service(
            FakeSupervisor::failing(ErrorCode::Aria2NotFound, "没有核心"),
            &FakeSink::new(),
            &receiver,
            &log,
        );
        let joined = log.lines().join("\n");
        assert!(
            joined.contains("ERROR aria2c 起不来，服务以 AH-E2001 失败退出：没有核心"),
            "{joined}"
        );
    }

    #[test]
    fn drive_service_logs_closed_channel_as_warning() {
        let (sender, receiver) = std::sync::mpsc::channel::<ControlCommand>();
        drop(sender);
        let log = RecordingLog::new();
        drive_service(FakeSupervisor::ok(), &FakeSink::new(), &receiver, &log).expect("当停止");
        let joined = log.lines().join("\n");
        assert!(joined.contains("WARN 控制通道已断开"), "{joined}");
    }

    #[test]
    fn describe_report_spells_out_status_and_codes() {
        assert_eq!(
            describe_report(&ServiceReport::StartPending {
                wait_hint: Duration::from_secs(30)
            }),
            "StartPending（wait_hint 30s）"
        );
        assert_eq!(
            describe_report(&ServiceReport::Running {
                aria2_version: "1.37.0".to_owned()
            }),
            "Running（aria2c 1.37.0 就绪）"
        );
        assert_eq!(
            describe_report(&ServiceReport::Stopped { exit_code: None }),
            "Stopped"
        );
        assert_eq!(
            describe_report(&ServiceReport::Stopped {
                exit_code: Some(5001)
            }),
            "Stopped（服务特定退出码 5001）"
        );
        assert_eq!(
            describe_report(&ServiceReport::Failed {
                code: ErrorCode::ServiceUnavailable,
                hint: "起不来".to_owned()
            }),
            "Failed（AH-E5001：起不来）"
        );
    }

    // ---- M5-e：状态文件挂钩（四点）-----------------------------------------

    /// 每个状态文件用例一个独立目录。
    struct StatusDir(std::path::PathBuf);

    impl StatusDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join("ariahost-svc-plan-status")
                .join(format!("{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn dir(&self) -> &std::path::Path {
            &self.0
        }

        fn live(&self) -> std::path::PathBuf {
            crate::status_file::status_path(&self.0)
        }

        fn read_json(&self) -> serde_json::Value {
            let text = std::fs::read_to_string(self.live()).expect("读 status.json");
            serde_json::from_str(&text).expect("合法 JSON")
        }
    }

    impl Drop for StatusDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 四个挂钩点的总装：预置 `Stop` 后一次跑完，按终态 + witness 逐点核对。
    ///
    /// - 点 2（核心就绪）与点 4（STOP 前）都写「有核心」，但只有点 4 能凭 witness
    ///   观察到（它紧接着被点 3 覆盖）。witness 里的 `core_version` 又只能来自点 2 的
    ///   缓存，所以这一条同时证明两点都跑过。
    /// - 点 3（核心停止后）是最终文件态。
    #[test]
    fn drive_service_writes_status_at_start_ready_before_stop_and_after_stop() {
        let dir = StatusDir::new("four-hooks");
        let witness = dir.dir().join("stop-witness.json");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("预置停止");

        let supervisor = FakeSupervisor {
            source: crate::status_file::CoreSource::Managed,
            stop_witness: Some(witness.clone()),
            ..FakeSupervisor::ok()
        };
        let target = StatusTarget {
            dir: dir.dir(),
            service_version: "0.1.3",
        };
        drive_service_with_status(
            supervisor,
            &FakeSink::new(),
            &receiver,
            &crate::log::NOOP,
            Some(target),
        )
        .expect("应正常结束");

        // 点 4 看到的最后一眼：有核心、来源真实、PID 是活的那个。
        let before = std::fs::read_to_string(&witness).expect("点 4 必须已落盘");
        let before: serde_json::Value = serde_json::from_str(&before).expect("合法 JSON");
        assert_eq!(before["core_source"], serde_json::json!("managed"));
        assert_eq!(before["aria2_pid"], serde_json::json!(4242));
        assert_eq!(
            before["core_version"],
            serde_json::json!("1.37.0"),
            "版本只能来自点 2（核心就绪时）的记录"
        );

        // 点 3：最终态是「没有核心」，且 schema/服务版本都在。
        let after = dir.read_json();
        assert_eq!(after["core_source"], serde_json::json!("none"));
        assert!(after["core_version"].is_null(), "{after}");
        assert!(after["aria2_pid"].is_null(), "停止后不许留已死的 PID");
        assert_eq!(after["schema_version"], serde_json::json!(1));
        assert_eq!(after["service_version"], serde_json::json!("0.1.3"));

        // started_at 是会话常量：点 4 与点 3 必须一致（面板靠它认「同一次启动」）。
        assert_eq!(
            before["started_at"], after["started_at"],
            "started_at 跨写入必须不变"
        );
        assert_ne!(
            after["updated_at"],
            serde_json::json!(""),
            "updated_at 必须每次现盖"
        );
    }

    /// 启动失败：**也要**落一份「没有核心」的终态，且不影响错误传播。
    #[test]
    fn drive_service_records_absent_core_when_startup_fails() {
        let dir = StatusDir::new("startup-failed");
        let (_sender, receiver) = std::sync::mpsc::channel::<ControlCommand>();
        let target = StatusTarget {
            dir: dir.dir(),
            service_version: "0.1.3",
        };

        let error = drive_service_with_status(
            FakeSupervisor::failing(ErrorCode::Aria2NotFound, "没有核心"),
            &FakeSink::new(),
            &receiver,
            &crate::log::NOOP,
            Some(target),
        )
        .expect_err("启动失败必须照旧返回错误");
        assert!(
            matches!(error, ServiceError::StartupFailed { .. }),
            "{error:?}"
        );

        let status = dir.read_json();
        assert_eq!(status["core_source"], serde_json::json!("none"));
        assert!(status["aria2_pid"].is_null());
    }

    /// 状态文件写不出来（目标目录是个文件）→ 服务照常跑完，只留 WARN。
    ///
    /// 这是任务书那条硬约束的接线验证：**写不出绝不能让服务失败**。
    #[test]
    fn drive_service_survives_unwritable_status_target() {
        let dir = StatusDir::new("unwritable");
        let blocker = dir.dir().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("造占位文件");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("预置停止");
        let log = RecordingLog::new();

        let outcome = drive_service_with_status(
            FakeSupervisor::ok(),
            &FakeSink::new(),
            &receiver,
            &log,
            Some(StatusTarget {
                dir: &blocker,
                service_version: "0.1.3",
            }),
        );
        assert!(outcome.is_ok(), "写不出状态文件不能拖垮服务：{outcome:?}");

        let warns: Vec<String> = log
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("WARN"))
            .collect();
        assert!(
            warns.iter().any(|line| line.contains("状态文件写不出去")),
            "必须留 WARN：{warns:?}"
        );
        // 服务仍然走完了完整生命周期。
        assert!(warns.iter().any(|line| line.contains("降级")), "{warns:?}");
    }

    /// 不传目标时（单测/前台形态）**一个字节都不写**。
    #[test]
    fn drive_service_without_target_writes_nothing() {
        let dir = StatusDir::new("no-target");
        let (sender, receiver) = std::sync::mpsc::channel();
        sender.send(ControlCommand::Stop).expect("预置停止");
        drive_service(
            FakeSupervisor::ok(),
            &FakeSink::new(),
            &receiver,
            &crate::log::NOOP,
        )
        .expect("应正常结束");
        let entries: Vec<_> = std::fs::read_dir(dir.dir())
            .expect("列目录")
            .map(|entry| entry.expect("项").file_name())
            .collect();
        assert!(entries.is_empty(), "不该产生文件：{entries:?}");
    }
}
