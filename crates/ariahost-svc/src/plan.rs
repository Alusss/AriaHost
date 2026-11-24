//! 服务状态机与控制命令（**纯逻辑**，不含任何 SCM 调用）。
//!
//! 为什么单独一层：SCM 交互（`windows-service` 的 dispatcher / 控制处理器 / `CreateServiceW`）
//! 在 Linux 侧编译不到、也测不了；把「收到控制命令 → 该上报哪些状态」抽成纯函数，
//! 才能在 WSL 的 `cargo test` 里把 §5 的状态机逐条钉住，Windows 侧只剩一层哑胶水。
//!
//! 规格来源：架构 §5 服务生命周期与 §4.2.4 状态机；控制命令到状态的映射见 [`ServiceLoop::handle`]。

use std::sync::mpsc::Receiver;
use std::time::Duration;

use ariahost_common::ErrorCode;

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

/// 服务主循环的**驱动骨架**：启动一次，然后等停止命令。
///
/// 为什么放在这一层（而不是 Windows 胶水里）：这部分逻辑决定了「失败会不会卡住 SCM」，
/// 必须在 Linux 单测里覆盖。SCM 只负责提供 `commands` 通道与实现 [`StatusSink`]。
///
/// 行为：
/// 1. 先跑一次 `Start`（SCM 不会为「启动」发控制事件——`service_main` 被调用本身就是启动）；
/// 2. 启动失败 → 上报 `Failed` 并返回错误（SCM 会把服务标成已停止 + 特定退出码）；
/// 3. 启动成功 → 阻塞等 `Stop`/`Shutdown`，收到后按状态机上报并返回；
/// 4. 通道断掉（理论上只会发生在进程要退出时）→ 当作停止处理，绝不悬挂。
pub fn drive_service<S: Supervisor>(
    supervisor: S,
    sink: &dyn StatusSink,
    commands: &Receiver<ControlCommand>,
) -> Result<(), ServiceError> {
    let mut state = ServiceLoop::new(supervisor);

    let startup = state.handle(ControlCommand::Start);
    for report in &startup {
        sink.report(report)
            .map_err(|detail| ServiceError::Unavailable {
                detail: format!("上报启动状态失败：{detail}"),
            })?;
    }
    if let Some(ServiceReport::Failed { code, hint }) = startup.last() {
        return Err(ServiceError::StartupFailed {
            code: *code,
            detail: hint.clone(),
        });
    }

    loop {
        let command = match commands.recv() {
            Ok(command) => command,
            Err(_) => ControlCommand::Stop,
        };
        let stopping = matches!(command, ControlCommand::Stop | ControlCommand::Shutdown);
        for report in &state.handle(command) {
            sink.report(report)
                .map_err(|detail| ServiceError::Unavailable {
                    detail: format!("上报停止状态失败：{detail}"),
                })?;
        }
        if stopping {
            return Ok(());
        }
    }
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
                None => Ok("1.37.0".to_owned()),
                Some((code, hint)) => Err(SupervisorFailure::new(code, hint)),
            }
        }

        fn stop(&mut self) -> Result<(), SupervisorFailure> {
            self.stops += 1;
            if self.stop_fails {
                Err(SupervisorFailure::new(
                    ErrorCode::ServiceUnavailable,
                    "停止超时",
                ))
            } else {
                Ok(())
            }
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

        drive_service(FakeSupervisor::ok(), &sink, &receiver).expect("应正常结束");
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

        drive_service(FakeSupervisor::ok(), &sink, &receiver).expect("应正常结束");
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
        drive_service(FakeSupervisor::ok(), &sink, &receiver).expect("通道关闭当停止");
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
        let error =
            drive_service(FakeSupervisor::ok(), &sink, &receiver).expect_err("上报失败必须冒泡");
        assert!(
            matches!(error, ServiceError::Unavailable { .. }),
            "{error:?}"
        );
    }
}
