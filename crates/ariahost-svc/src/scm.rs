//! Windows SCM 胶水（仅 `cfg(windows)`）：dispatcher、控制处理器、安装/卸载/启停。
//!
//! 这一层刻意做得很薄——所有能测的决定（状态机、引号、错误分类）都在 `plan.rs`/`install.rs`，
//! 在 Linux 上被单测覆盖。这里只剩「把 Win32 调通」的哑代码。
//!
//! **依赖源码核对**（windows-service 0.8.1）：
//! - `service_dispatcher::start(name, extern "system" fn(u32, *mut *mut u16))`（`service_dispatcher.rs:34`）；
//! - 控制处理器 `register(name, FnMut(ServiceControl) -> ServiceControlHandlerResult)`
//!   → `ServiceStatusHandle`（`service_control_handler.rs:31`）；**没有** `Start` 事件，
//!   `service_main` 被调用本身就是启动；
//! - `Service::start/stop/delete/set_description/update_failure_actions`
//!   （`service.rs:1476/1505/1565/1808/1773`）；
//! - `ServiceInfo.executable_path` 的加引号由 crate 内部 `escape_wide` 完成
//!   （`service.rs` 的 `RawServiceInfo::new`），注册时不必自己拼字符串；
//!   [`crate::service_binary_path`] 是被钉住的**期望值**，用于诊断与手工核对。

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ariahost_common::config as config_layer;
use ariahost_common::paths::Plane;
use ariahost_common::ErrorCode;
use ariahost_core::resolve_core_in;
use ariahost_core::runtime::{
    self, CoreRuntime, LaunchSpec, ProcessSpawner, RpcProbe, RuntimePaths, Timeouts,
};
use windows_service::service::{
    Service, ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState as WinServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::install::{self, classify_control, classify_install, classify_uninstall};
use crate::log::ServiceLog;
use crate::plan::{
    drive_service_with_status, exit_code_for, ControlCommand, ServiceReport, StatusSink,
    StatusTarget, Supervisor, SupervisorFailure,
};
use crate::status_file::CoreSource;
use crate::{
    InstallOutcome, ServiceError, ServiceState, UninstallOutcome, SERVICE_DISPLAY_NAME,
    SERVICE_NAME,
};

// ---------------------------------------------------------------------------
// 服务主循环
// ---------------------------------------------------------------------------

/// 服务入口：连 SCM dispatcher。**非服务上下文调用会返回 1063**（手工跑 `--service`）。
pub fn run() -> Result<(), ServiceError> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|error| {
        if win32_code(&error) == Some(install::ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) {
            install::not_from_scm(format!(
                "未被 SCM 拉起（Win32 1063）；服务模式必须由 SCM 启动，请用服务管理器或 `sc start AriaHost`。{}",
                describe(&error)
            ))
        } else {
            ServiceError::Unavailable {
                detail: format!("连接 SCM 分发器失败：{}", describe(&error)),
            }
        }
    })
}

windows_service::define_windows_service!(ffi_service_main, service_main);

/// SCM 回调线程：只做「登记命令 + 立刻返回」，绝不在这里干活。
///
/// **返回 `()` 是刻意的**：`define_windows_service!` 展开成
/// `$service_main_handler(arguments);`——返回值会被**丢弃**，所以这里返回 `Result`
/// 会触发 `unused_must_use`（CI 的 `-D warnings` 直接红）。
/// 启动失败已经通过 `drive_service` → [`ScmStatusSink`] 上报成
/// `Stopped` + 服务特定退出码（事件查看器可见），这里只把文本写 stderr 兜底。
///
/// 日志：**不能**在控制处理器里写（那是 SCM 的回调线程，任何阻塞都会让 SCM 判服务无响应），
/// 所以命令只在工作线程被取出时记一行。
fn service_main(_arguments: Vec<OsString>) {
    // Windows 服务没有控制台：stdout/stderr 没有接收者，日志必须自己写文件。
    let log = ServiceLog::open_service_plane();
    log.info(&format!(
        "service_main 进入（pid {}）：服务无控制台，日志写 {}",
        std::process::id(),
        log.path().display()
    ));
    match establish_and_drive(&log) {
        Ok(()) => log.info("service_main 正常退出"),
        Err(error) => {
            log.error(&format!(
                "service_main 异常退出：{} {}",
                error.code().code(),
                error.detail()
            ));
            // 兜底：日志文件都写不了时，至少让 stderr（调试器/前台运行时）看到。
            eprintln!("{} {}", error.code().code(), error.detail());
        }
    }
}

/// 注册控制处理器 → 组装 supervisor → 跑状态机。
fn establish_and_drive(log: &ServiceLog) -> Result<(), ServiceError> {
    let (command_sender, command_receiver) = mpsc::channel::<ControlCommand>();
    let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
        // 关键：这里只 send，不做任何阻塞动作（回调里阻塞会让 SCM 判服务无响应）。
        match control {
            ServiceControl::Stop => send_command(&command_sender, ControlCommand::Stop),
            ServiceControl::Shutdown => send_command(&command_sender, ControlCommand::Shutdown),
            // 架构 §4.2.4：Interrogate 不做事。
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })
    .map_err(|error| ServiceError::Unavailable {
        detail: format!("注册控制处理器失败：{}", describe(&error)),
    })?;
    log.info("控制处理器已注册（回调只登记命令，不阻塞 SCM 线程）");

    let supervisor = Aria2Supervisor::from_env(log)?;
    let sink = ScmStatusSink {
        handle: status_handle,
    };
    // M5-e：服务形态把生命周期写进 `%ProgramData%\AriaHost\status.json`（单向、不含机密）。
    //
    // 目录取不到 → `None`：不写状态文件，面板退回「服务（运行中）」的旧口径。
    // 这是**观测能力降级**，不阻断服务启动（写不出状态文件绝不能让服务失败）。
    let status_dir = ariahost_common::paths::service_runtime_dir();
    if status_dir.is_none() {
        log.warn(&format!(
            "拿不到 %ProgramData%（服务平面根目录不可用）：不写 {}，面板将看不到服务侧核心信息",
            crate::status_file::STATUS_FILE_NAME
        ));
    }
    let target = status_dir.as_deref().map(|dir| StatusTarget {
        dir,
        service_version: env!("CARGO_PKG_VERSION"),
    });
    drive_service_with_status(supervisor, &sink, &command_receiver, log, target)
}

/// 送一条命令进工作线程；通道断了（进程正在退出）如实回服务不可用，不 panic。
fn send_command(
    sender: &mpsc::Sender<ControlCommand>,
    command: ControlCommand,
) -> ServiceControlHandlerResult {
    match sender.send(command) {
        Ok(()) => ServiceControlHandlerResult::NoError,
        Err(_) => ServiceControlHandlerResult::Other(exit_code_for(ErrorCode::ServiceUnavailable)),
    }
}

/// 把 [`ServiceReport`] 翻成 SCM 状态。
struct ScmStatusSink {
    handle: ServiceStatusHandle,
}

impl StatusSink for ScmStatusSink {
    fn report(&self, report: &ServiceReport) -> Result<(), String> {
        let status = match report {
            ServiceReport::StartPending { wait_hint } => status_for(
                WinServiceState::StartPending,
                ServiceControlAccept::empty(),
                *wait_hint,
            ),
            ServiceReport::Running { .. } => status_for(
                WinServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                Duration::ZERO,
            ),
            ServiceReport::StopPending { wait_hint } => status_for(
                WinServiceState::StopPending,
                ServiceControlAccept::empty(),
                *wait_hint,
            ),
            ServiceReport::Stopped { exit_code } => {
                let mut status = status_for(
                    WinServiceState::Stopped,
                    ServiceControlAccept::empty(),
                    Duration::ZERO,
                );
                if let Some(code) = exit_code {
                    status.exit_code = ServiceExitCode::ServiceSpecific(*code);
                }
                status
            }
            ServiceReport::Failed { code, .. } => {
                let mut status = status_for(
                    WinServiceState::Stopped,
                    ServiceControlAccept::empty(),
                    Duration::ZERO,
                );
                // 事件查看器里会显示「服务特定错误 2002/5001…」，能直接对上 AH-Exxxx。
                status.exit_code = ServiceExitCode::ServiceSpecific(exit_code_for(*code));
                status
            }
        };
        self.handle
            .set_service_status(status)
            .map_err(|error| describe(&error))
    }
}

/// 组装一条 `ServiceStatus`。
fn status_for(
    state: WinServiceState,
    accepted: ServiceControlAccept,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    }
}

// ---------------------------------------------------------------------------
// 真实 Supervisor：托管 aria2c（复用 ariahost-core::runtime，服务平面）
// ---------------------------------------------------------------------------

/// 把 `ariahost_core::runtime` 包成 [`Supervisor`]，并强制使用服务平面（R7）。
struct Aria2Supervisor {
    spec: LaunchSpec,
    core: CoreRuntime,
    /// M5-e：核心来源，**解析时一次定下**（写 `status.json` 用）。
    ///
    /// 为什么定在构造期：来源是「这次为什么选了这个核心」的判断结果，启动后再去猜
    /// （例如按路径反推）可能与真实解析结论不一致；解析器已经给了 `origin`，直接取。
    source: CoreSource,
}

impl Aria2Supervisor {
    /// 从环境组装：ProgramData 平面 + 服务平面版本库 + machine-scope secret。
    fn from_env(log: &ServiceLog) -> Result<Self, ServiceError> {
        let mut config = config_layer::load().map_err(unavailable)?;
        let paths = RuntimePaths::service().ok_or_else(|| ServiceError::Unavailable {
            detail: "拿不到 %ProgramData%（服务平面运行时目录不可用）".to_owned(),
        })?;
        let secret_path = paths.runtime.join(config_layer::SECRET_FILE_NAME);
        let secret =
            config_layer::resolve_secret(&mut config, &secret_path).map_err(unavailable)?;
        let download_dir = config_layer::download_dir_absolute(&config).map_err(unavailable)?;
        // 用 `resolve_core_in` 而不是 `resolve_core_exe_in`：前者带 `origin`，
        // 状态文件要如实写「这个核心是内置/托管/外部来的」，不能事后按路径反推。
        let resolved = resolve_core_in(&config, Plane::Service).map_err(|error| {
            ServiceError::StartupFailed {
                code: error.code().unwrap_or(ErrorCode::Aria2NotFound),
                detail: error.to_string(),
            }
        })?;
        let source = CoreSource::from_origin(resolved.origin);
        let exe = resolved.path;
        runtime::prepare_conf(&config, &paths, &secret, &download_dir).map_err(unavailable)?;

        let port = config.aria2.rpc.port;
        log.info(&format!(
            "托管目标：核心 {}（来源 {}）、RPC 端口 {port}、服务平面运行目录 {}、下载目录 {}",
            exe.display(),
            source.as_str(),
            paths.runtime.display(),
            download_dir.display()
        ));
        let probe = RpcProbe::new(port, &secret);
        Ok(Self {
            spec: runtime::launch_spec(&config, &paths, exe, &secret),
            core: CoreRuntime::new(
                Box::new(ProcessSpawner),
                Box::new(probe),
                Timeouts::default(),
            ),
            source,
        })
    }
}

impl Supervisor for Aria2Supervisor {
    fn start(&mut self) -> Result<String, SupervisorFailure> {
        let mut failure_hint = String::new();
        let mut collect = |event: ariahost_core::RuntimeEvent| {
            if let ariahost_core::RuntimeEvent::Failed { hint, .. } = &event {
                failure_hint.clone_from(hint);
            }
        };
        self.core
            .start(&self.spec, &mut collect)
            .map_err(|error| SupervisorFailure {
                code: error.code().unwrap_or(ErrorCode::ServiceUnavailable),
                hint: if failure_hint.is_empty() {
                    error.to_string()
                } else {
                    failure_hint
                },
            })?;
        Ok(self
            .core
            .status()
            .version
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()))
    }

    fn stop(&mut self) -> Result<(), SupervisorFailure> {
        self.core
            .stop(&mut |_event| {})
            .map(|_status| ())
            .map_err(|error| SupervisorFailure {
                code: error.code().unwrap_or(ErrorCode::ServiceUnavailable),
                hint: error.to_string(),
            })
    }

    /// M5-e：写 `status.json` 用。
    fn core_source(&self) -> CoreSource {
        self.source
    }

    /// M5-e：现读 PID（`CoreStatus.pid` 在停止后是 `None`，所以不会留下已死的 PID）。
    fn aria2_pid(&self) -> Option<u32> {
        self.core.status().pid
    }
}

/// 配置层错误 → 服务不可用（配置读不出来时服务起不来，但这是「服务」视角的失败）。
fn unavailable(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Unavailable {
        detail: error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 安装 / 卸载 / 启停
// ---------------------------------------------------------------------------

/// 注册服务（手动触发 + 1/2/4 秒失败重启 + 描述）。
pub fn install(exe_path: &Path) -> Result<InstallOutcome, ServiceError> {
    let manager = ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| ServiceError::NeedsAdmin {
        detail: format!("打开本机 SCM（需要管理员）：{}", describe(&error)),
    })?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        // 需求原文：默认**手动触发**，不许改成 auto。
        start_type: ServiceStartType::OnDemand,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path.to_path_buf(),
        launch_arguments: vec![OsString::from(install::SERVICE_ARGUMENT)],
        dependencies: Vec::new(),
        account_name: None, // LocalSystem
        account_password: None,
    };

    let service = match manager.create_service(
        &service_info,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(error) => {
            return classify_install(
                win32_code(&error),
                format!("创建服务失败：{}", describe(&error)),
            );
        }
    };

    // 描述与失败动作都属「锦上添花」：写不进去不至于让安装失败（SCM 走默认行为）。
    let _ = service.set_description(install::SERVICE_DESCRIPTION);
    let _ = service.update_failure_actions(failure_actions());
    Ok(InstallOutcome::Created)
}

/// 失败动作：重启 1s → 2s → 4s，之后 SCM 放弃（配合架构 §5 的防风暴）。
fn failure_actions() -> ServiceFailureActions {
    let actions = install::failure_delays()
        .iter()
        .map(|delay| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: *delay,
        })
        .collect::<Vec<_>>();
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(install::failure_reset_period()),
        reboot_msg: None,
        command: None,
        actions: Some(actions),
    }
}

/// 注销服务：先停后删；没装算幂等成功。
pub fn uninstall() -> Result<UninstallOutcome, ServiceError> {
    let manager = open_manager_for_write()?;
    let service = match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
    ) {
        Ok(service) => service,
        Err(error) => {
            return classify_uninstall(
                win32_code(&error),
                format!("打开服务失败：{}", describe(&error)),
            );
        }
    };

    // 在跑就先停（等它真停下再删，否则删不掉）。
    if let Ok(status) = service.query_status() {
        if status.current_state != WinServiceState::Stopped {
            let _ = service.stop();
            wait_for_state(&service, WinServiceState::Stopped, Duration::from_secs(30));
        }
    }

    match service.delete() {
        Ok(()) => Ok(UninstallOutcome::Removed),
        Err(error) => classify_uninstall(
            win32_code(&error),
            format!("删除服务失败：{}", describe(&error)),
        ),
    }
}

/// 启动服务并等到 Running（已在跑算幂等成功）。
pub fn start() -> Result<ServiceState, ServiceError> {
    let manager = open_manager_for_write()?;
    let service =
        open_service_for_control(&manager, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    if let Err(error) = service.start::<&OsStr>(&[]) {
        return classify_control(
            ServiceState::Running,
            win32_code(&error),
            format!("启动服务失败：{}", describe(&error)),
        );
    }
    wait_for_state(&service, WinServiceState::Running, Duration::from_secs(30));
    Ok(ServiceState::Running)
}

/// 停止服务并等到 Stopped（已停止算幂等成功）。
pub fn stop() -> Result<ServiceState, ServiceError> {
    let manager = open_manager_for_write()?;
    let service =
        open_service_for_control(&manager, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    if let Err(error) = service.stop() {
        return classify_control(
            ServiceState::Stopped,
            win32_code(&error),
            format!("停止服务失败：{}", describe(&error)),
        );
    }
    wait_for_state(&service, WinServiceState::Stopped, Duration::from_secs(30));
    Ok(ServiceState::Stopped)
}

fn open_manager_for_write() -> Result<ServiceManager, ServiceError> {
    ServiceManager::local_computer(
        None::<&OsStr>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .map_err(|error| ServiceError::NeedsAdmin {
        detail: format!("打开本机 SCM（需要管理员）：{}", describe(&error)),
    })
}

fn open_service_for_control(
    manager: &ServiceManager,
    access: ServiceAccess,
) -> Result<Service, ServiceError> {
    manager
        .open_service(SERVICE_NAME, access)
        .map_err(|error| match win32_code(&error) {
            Some(install::ERROR_SERVICE_DOES_NOT_EXIST) => ServiceError::NotInstalled {
                detail: describe(&error),
            },
            Some(install::ERROR_ACCESS_DENIED) => ServiceError::NeedsAdmin {
                detail: describe(&error),
            },
            _ => ServiceError::Unavailable {
                detail: describe(&error),
            },
        })
}

/// 轮询到目标状态或超时（超时不算错误：调用方只做尽力等待）。
fn wait_for_state(service: &Service, target: WinServiceState, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match service.query_status() {
            Ok(status) if status.current_state == target => return,
            Ok(_) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return,
        }
    }
}

// ---- 错误文本 ----

/// 取 Win32 错误码（`windows_service::Error` 只有 `Winapi` 变体带 `io::Error`）。
fn win32_code(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(io_error) => io_error.raw_os_error(),
        _ => None,
    }
}

/// 诊断文本：`windows_service::Error` 的 `Display` 对 Winapi 只说「IO error in winapi call」，
/// 没有诊断价值，所以自己拼 Win32 码。
fn describe(error: &windows_service::Error) -> String {
    match error {
        windows_service::Error::Winapi(io_error) => match io_error.raw_os_error() {
            Some(code) => format!("Win32 错误 {code}（{io_error}）"),
            None => io_error.to_string(),
        },
        other => other.to_string(),
    }
}
