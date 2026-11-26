//! ServiceHost：SCM 托管 + aria2c 子进程托管（架构 §3.2 / §4.2.3 / §5）。
//!
//! ## 分工
//!
//! - [`plan`]：控制命令 → 状态上报序列的状态机、[`plan::drive_service`] 驱动骨架、
//!   超时预算、服务特定退出码。**纯逻辑，Linux 单测全覆盖**。
//! - [`install`]：`binPath` 引号、失败动作、Win32 错误 → 结果分类。**纯逻辑，单测全覆盖**。
//! - [`status_file`]：服务侧状态文件 `%ProgramData%\AriaHost\status.json` 的原子写入
//!   （M5-e）。单向：服务只写、面板与诊断包只读；内容不含机密；写不出去只记 WARN。
//! - `scm`（仅 Windows）：dispatcher、控制处理器、`CreateServiceW`/`DeleteServiceW`/启停的哑胶水。
//!
//! ## 非阻塞纪律（架构 §4.2.4）
//!
//! SCM 回调线程里**只许登记命令**：`service_control_handler` 的闭包只做 `send`，
//! 所有阻塞动作（起 aria2、等停止）都在 `service_main` 的工作线程里由 [`plan::drive_service`] 完成。
//! 回调里阻塞会让 SCM 判服务失去响应（1053）。
//!
//! ## 平面（R7/R9）
//!
//! 服务账户的 `%APPDATA%` 是 **systemprofile**，所以服务模式的一切落盘走
//! `%ProgramData%\AriaHost\`（`RuntimePaths::service()`、`Plane::Service` 版本库）。

use std::fmt;
use std::path::Path;

use ariahost_common::ErrorCode;

pub mod install;
pub mod log;
pub mod plan;
pub mod status_file;

#[cfg(windows)]
mod scm;
#[cfg(not(windows))]
#[path = "scm_stub.rs"]
mod scm;

/// SCM 服务名（架构 §3.3：服务名与显示名同名 `AriaHost`）。
pub const SERVICE_NAME: &str = "AriaHost";

/// SCM 显示名。
pub const SERVICE_DISPLAY_NAME: &str = "AriaHost";

/// 服务在面板/CLI 视角下的状态（[`query_state`] 的输出）。
///
/// 只保留面板真正要区分的 5 态 + 兜底：AriaHost 不会让服务进入 `Paused` 系列，
/// 所以 SCM 的 `Paused`/`ContinuePending`/`PausePending` 统一归 [`ServiceState::Unknown`]，
/// 而不是硬编成 Stopped 或 Running（猜错的代价是面板给出错误的「启动/停止」按钮）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceState {
    /// 未安装：SCM 里没有这个服务名（正常状态，不是错误）。
    NotInstalled,
    /// 已安装，当前已停止。
    Stopped,
    /// 正在启动（SCM `SERVICE_START_PENDING`）。
    StartPending,
    /// 正在运行（SCM `SERVICE_RUNNING`）。
    Running,
    /// 正在停止（SCM `SERVICE_STOP_PENDING`）。
    StopPending,
    /// 无法识别的状态（含 SCM 的暂停系列与未知状态码）。
    Unknown,
}

impl ServiceState {
    /// 日志/诊断包里的稳定字符串（不随 UI 文案变化）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::Stopped => "stopped",
            Self::StartPending => "start-pending",
            Self::Running => "running",
            Self::StopPending => "stop-pending",
            Self::Unknown => "unknown",
        }
    }

    /// 是否处于「正在运行」。
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }

    /// 是否「有服务注册」（未安装之外的状态都算已注册）。
    pub const fn is_installed(self) -> bool {
        !matches!(self, Self::NotInstalled)
    }
}

/// 安装结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// 本次新建。
    Created,
    /// 同名服务已存在（安装器重跑必须安全，不算错误）。
    AlreadyExisted,
}

impl InstallOutcome {
    /// 稳定文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::AlreadyExisted => "already-existed",
        }
    }
}

/// 卸载结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// 本次删除。
    Removed,
    /// 本来就没装（幂等成功，不算错误）。
    NotInstalled,
}

impl UninstallOutcome {
    /// 稳定文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Removed => "removed",
            Self::NotInstalled => "not-installed",
        }
    }
}

/// SCM 启动类型（只留本产品会遇到的 5 种；[`Self::as_str`] 是稳定机器可读文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStartMode {
    /// 随系统自动启动。
    Auto,
    /// 手动/按需启动（`DemandStart`，本产品的默认，需求第 5 条）。
    Demand,
    /// 已禁用。
    Disabled,
    /// 驱动：系统启动时加载（服务不该出现，保留以便如实上报而不是猜）。
    System,
    /// 驱动：引导时加载（同上）。
    Boot,
}

impl ServiceStartMode {
    /// 稳定文本：面板/脚本按它判断，**不要**解析中文。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Demand => "demand",
            Self::Disabled => "disabled",
            Self::System => "system",
            Self::Boot => "boot",
        }
    }
}

/// 只读服务状态报告（`--service-status` 与面板轮询共用同一份数据）。
///
/// 「未安装」不是错误：`installed == false` 时 `state` 恒为 [`ServiceState::NotInstalled`]、
/// `start_type`/`pid` 都是 `None`。这样调用方既不用把「没装」当异常，也不用解析中文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceStatusReport {
    /// 是否有服务注册。
    pub installed: bool,
    /// 当前状态。
    pub state: ServiceState,
    /// 启动类型（未安装时 `None`）。
    pub start_type: Option<ServiceStartMode>,
    /// 服务进程 pid（只有运行中且 SCM 给出时才是 `Some`）。
    pub pid: Option<u32>,
}

/// 服务状态查询失败（[`ServiceState::NotInstalled`] 不是错误，不走这个类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceQueryError {
    /// 映射到的既有错误码（本批不新增码）。
    pub code: ErrorCode,
    /// 诊断原文（含 Win32 错误码；不上屏，进日志/诊断包）。
    pub detail: String,
}

impl ServiceQueryError {
    /// 构造。
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for ServiceQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.code.code(), self.detail)
    }
}

impl std::error::Error for ServiceQueryError {}

/// 服务操作的失败。
///
/// 每个变体都带**可执行建议所需的分类**：`NeedsAdmin` 要求提权，`NotInstalled` 要先装，
/// `RegisterPrereq` 是注册前置失败（ProgramData 权限/版本库准备）。裸 Win32 码只出现在
/// `detail` 里，不上屏（架构 §4.12：禁止把 Windows 错误原样丢给用户）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// 非 Windows 平台：本产品只发 Windows。
    Unsupported,
    /// 手工在非服务上下文跑 `--service`（Win32 1063）。
    NotFromScm {
        /// 可读原因。
        detail: String,
    },
    /// 需要管理员权限（Win32 5）。
    NeedsAdmin {
        /// 可读原因。
        detail: String,
    },
    /// 服务未安装（Win32 1060）。
    NotInstalled {
        /// 可读原因。
        detail: String,
    },
    /// 通用失败（SCM 打不开、控制被拒、超时等）。
    Unavailable {
        /// 可读原因。
        detail: String,
    },
    /// 注册前置失败（ProgramData 权限、服务创建被拒）。
    RegisterPrereq {
        /// 可读原因。
        detail: String,
    },
    /// aria2 核心启动失败（透传上游码，如 `AH-E2001`/`AH-E2002`）。
    StartupFailed {
        /// 上游错误码。
        code: ErrorCode,
        /// 可读原因。
        detail: String,
    },
}

impl ServiceError {
    /// 对应错误码（`StartupFailed` 透传上游码，其余落在 E5 段）。
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Unsupported | Self::NotFromScm { .. } | Self::NotInstalled { .. } => {
                ErrorCode::ServiceUnavailable
            }
            Self::NeedsAdmin { .. } => ErrorCode::ServiceNeedsAdmin,
            Self::Unavailable { .. } => ErrorCode::ServiceUnavailable,
            Self::RegisterPrereq { .. } => ErrorCode::ServiceRegisterPrereq,
            Self::StartupFailed { code, .. } => *code,
        }
    }

    /// 诊断细节（没有时为空串）。
    pub fn detail(&self) -> &str {
        match self {
            Self::Unsupported => "",
            Self::NotFromScm { detail }
            | Self::NeedsAdmin { detail }
            | Self::NotInstalled { detail }
            | Self::Unavailable { detail }
            | Self::RegisterPrereq { detail }
            | Self::StartupFailed { detail, .. } => detail,
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code().code();
        match self {
            Self::Unsupported => write!(formatter, "{code} 非 Windows 平台不支持服务模式"),
            Self::NotFromScm { detail } => write!(formatter, "{code} {detail}"),
            Self::NeedsAdmin { detail } => {
                write!(formatter, "{code} 需要管理员权限（UAC）：{detail}")
            }
            Self::NotInstalled { detail } => {
                write!(formatter, "{code} AriaHost 服务未安装：{detail}")
            }
            Self::Unavailable { detail } => write!(formatter, "{code} {detail}"),
            Self::RegisterPrereq { detail } => {
                write!(formatter, "{code} 服务注册前置失败：{detail}")
            }
            Self::StartupFailed { code, detail } => write!(formatter, "{} {detail}", code.code()),
        }
    }
}

impl std::error::Error for ServiceError {}

#[cfg(windows)]
mod state {
    //! Windows 实现：`windows-service` 0.8.1（API 以依赖源码为准，见各调用的签名说明）。

    use std::ffi::OsStr;

    use ariahost_common::ErrorCode;
    use windows_service::service::ServiceAccess;
    use windows_service::service::ServiceStartType;
    use windows_service::service::ServiceState as WinServiceState;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    use super::{ServiceQueryError, ServiceStartMode, ServiceState, ServiceStatusReport};
    use crate::install::{ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST};

    /// 查询指定服务名的状态。
    ///
    /// 调用链（全部来自 windows-service-0.8.1 源码）：
    /// - `ServiceManager::local_computer(None::<&OsStr>, CONNECT)` → `OpenSCManagerW`
    ///   （`service_manager.rs:76-81`）；
    /// - `manager.open_service(name, QUERY_STATUS)` → `OpenServiceW`
    ///   （`service_manager.rs:197-217`）；
    /// - `service.query_status()` → `QueryServiceStatusEx`（`service.rs:1539`）。
    pub fn query_state_of(name: &str) -> Result<ServiceState, ServiceQueryError> {
        let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
            .map_err(|error| map_error(&error, "打开本机 SCM"))?;

        let service = match manager.open_service(name, ServiceAccess::QUERY_STATUS) {
            Ok(service) => service,
            // 「没装」是面板第一屏的常态，不是错误。
            Err(error) if is_not_installed(&error) => return Ok(ServiceState::NotInstalled),
            Err(error) => return Err(map_error(&error, "打开服务")),
        };

        let status = service
            .query_status()
            .map_err(|error| map_error(&error, "查询服务状态"))?;
        Ok(map_state(status.current_state))
    }

    /// 只读状态报告：`QUERY_STATUS | QUERY_CONFIG` 一次拿全状态、pid 与启动类型。
    ///
    /// 为什么合在一次调用里：面板要轮询它，分三次打开服务句柄会让「状态/pid/启动类型」
    /// 来自三个不同瞬间，出现「已停止但 pid 还在」这类自相矛盾的报告。
    pub fn status_report_of(name: &str) -> Result<ServiceStatusReport, ServiceQueryError> {
        let manager = ServiceManager::local_computer(None::<&OsStr>, ServiceManagerAccess::CONNECT)
            .map_err(|error| map_error(&error, "打开本机 SCM"))?;

        let service = match manager.open_service(
            name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        ) {
            Ok(service) => service,
            // 「没装」是常态，不是错误：给一份自洽的报告而不是 Err。
            Err(error) if is_not_installed(&error) => {
                return Ok(ServiceStatusReport {
                    installed: false,
                    state: ServiceState::NotInstalled,
                    start_type: None,
                    pid: None,
                });
            }
            Err(error) => return Err(map_error(&error, "打开服务")),
        };

        let status = service
            .query_status()
            .map_err(|error| map_error(&error, "查询服务状态"))?;
        let state = map_state(status.current_state);
        let config = service
            .query_config()
            .map_err(|error| map_error(&error, "查询服务配置"))?;
        Ok(ServiceStatusReport {
            installed: true,
            state,
            start_type: Some(map_start_type(config.start_type)),
            // 只有运行时 pid 才有意义：停止态 SCM 仍会给出上次的 pid，照抄会误导面板。
            pid: if state.is_running() {
                status.process_id
            } else {
                None
            },
        })
    }

    /// `windows_service::ServiceStartType` → 本 crate 的稳定枚举。
    fn map_start_type(start_type: ServiceStartType) -> ServiceStartMode {
        match start_type {
            ServiceStartType::AutoStart => ServiceStartMode::Auto,
            ServiceStartType::OnDemand => ServiceStartMode::Demand,
            ServiceStartType::Disabled => ServiceStartMode::Disabled,
            ServiceStartType::SystemStart => ServiceStartMode::System,
            ServiceStartType::BootStart => ServiceStartMode::Boot,
        }
    }

    /// 取 Win32 错误码。`windows_service::Error` 只有 `Winapi` 变体带 `io::Error`。
    fn win32_code(error: &windows_service::Error) -> Option<i32> {
        match error {
            windows_service::Error::Winapi(io_error) => io_error.raw_os_error(),
            _ => None,
        }
    }

    /// 是否「服务不存在」（Win32 1060）。
    fn is_not_installed(error: &windows_service::Error) -> bool {
        win32_code(error) == Some(ERROR_SERVICE_DOES_NOT_EXIST)
    }

    /// 错误码映射：只用既有 `E5###`。
    ///
    /// - `ERROR_ACCESS_DENIED` → `AH-E5002`（服务操作需管理员/UAC）；
    /// - 其余（SCM 打不开、服务句柄拿不到、状态查询失败）→ `AH-E5001`（服务不可用）：
    ///   既有码段里没有「查询失败」专用码，E5001 是唯一语义对得上的；详情原文照样保留。
    fn map_error(error: &windows_service::Error, action: &str) -> ServiceQueryError {
        let code = if win32_code(error) == Some(ERROR_ACCESS_DENIED) {
            ErrorCode::ServiceNeedsAdmin
        } else {
            ErrorCode::ServiceUnavailable
        };
        ServiceQueryError::new(code, format!("{action}失败：{}", describe(error)))
    }

    /// `windows_service::Error` 的 `Display` 对 `Winapi` 只给「IO error in winapi call」
    /// （源码 `lib.rs:206-222`），诊断价值为零，所以这里自己拼 Win32 码与系统消息。
    fn describe(error: &windows_service::Error) -> String {
        match error {
            windows_service::Error::Winapi(io_error) => match io_error.raw_os_error() {
                Some(code) => format!("Win32 错误 {code}（{io_error}）"),
                None => io_error.to_string(),
            },
            other => other.to_string(),
        }
    }

    /// `windows_service` 的状态 → 面板态（映射理由见 [`ServiceState`] 文档）。
    pub(super) fn map_state(state: WinServiceState) -> ServiceState {
        match state {
            WinServiceState::Stopped => ServiceState::Stopped,
            WinServiceState::StartPending => ServiceState::StartPending,
            WinServiceState::StopPending => ServiceState::StopPending,
            WinServiceState::Running => ServiceState::Running,
            WinServiceState::ContinuePending
            | WinServiceState::PausePending
            | WinServiceState::Paused => ServiceState::Unknown,
        }
    }
}

#[cfg(not(windows))]
mod state {
    //! 非 Windows 占位：保证 workspace 在非 Windows 上也能编译（CI 只编译 Windows）。

    use ariahost_common::ErrorCode;

    use super::{ServiceQueryError, ServiceState, ServiceStatusReport};

    /// 非 Windows 平台没有 SCM，直接报「服务不可用」。
    pub fn query_state_of(_name: &str) -> Result<ServiceState, ServiceQueryError> {
        Err(ServiceQueryError::new(
            ErrorCode::ServiceUnavailable,
            "非 Windows 平台不支持 SCM 查询".to_owned(),
        ))
    }

    /// 非 Windows 平台没有 SCM，状态报告同样如实报错（不能伪造一台机器的服务状态）。
    pub fn status_report_of(_name: &str) -> Result<ServiceStatusReport, ServiceQueryError> {
        Err(ServiceQueryError::new(
            ErrorCode::ServiceUnavailable,
            "非 Windows 平台没有 SCM，无法查询服务状态".to_owned(),
        ))
    }
}

/// 查询本产品服务（[`SERVICE_NAME`]）的当前状态。
///
/// 未安装返回 `Ok(ServiceState::NotInstalled)`；真正的失败（SCM 打不开、权限不足等）
/// 返回带既有错误码的 [`ServiceQueryError`]。
pub fn query_state() -> Result<ServiceState, ServiceQueryError> {
    query_state_of(SERVICE_NAME)
}

/// 查询任意服务名的状态（[`query_state`] 的实现入口）。
///
/// 面板的「模式收敛」判断需要查别的名字（例如旧版本残留的服务），以及测试需要一个
/// 保证不存在的名字，所以把名字作为参数暴露出来。
pub fn query_state_of(name: &str) -> Result<ServiceState, ServiceQueryError> {
    state::query_state_of(name)
}

/// 查询本产品服务的只读状态报告（`--service-status` 的实现，面板轮询也用它）。
///
/// 与 [`query_state`] 的区别：这里额外给出 `start_type` 与运行中的 `pid`，
/// 且**未安装不是错误**（`installed: false`）。
pub fn status_report() -> Result<ServiceStatusReport, ServiceQueryError> {
    status_report_of(SERVICE_NAME)
}

/// 查询任意服务名的只读状态报告（[`status_report`] 的实现入口）。
pub fn status_report_of(name: &str) -> Result<ServiceStatusReport, ServiceQueryError> {
    state::status_report_of(name)
}

pub use scm::{install, run, start, stop, uninstall};

/// 构造「已停止」的 SCM 服务状态（M1 骨架锚点，保留给后续控制循环复用）。
#[cfg(windows)]
pub fn stopped_status() -> windows_service::service::ServiceStatus {
    use windows_service::service::{
        ServiceControlAccept, ServiceExitCode, ServiceState as WinServiceState, ServiceStatus,
        ServiceType,
    };

    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: WinServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::NO_ERROR,
        checkpoint: 0,
        wait_hint: std::time::Duration::from_secs(0),
        process_id: None,
    }
}

/// 供 CLI/安装器判断平台能力（非 Windows 恒 `false`）。
pub const fn is_supported_platform() -> bool {
    cfg!(windows)
}

/// 服务二进制路径的期望命令行（见 [`install::service_binary_path`]）。
///
/// 单独暴露一个入口是为了让面板/M3 安装器不用知道 `--service` 这个字面量。
pub fn expected_binary_path(exe: &Path) -> String {
    install::service_binary_path(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn run_returns_unsupported_on_non_windows() {
        let error = run().expect_err("非 Windows 平台不可能跑起来");
        assert_eq!(error, ServiceError::Unsupported);
        assert_eq!(error.code().code(), "AH-E5001");
        assert!(error.to_string().starts_with("AH-E5001"));
    }

    #[test]
    fn service_names_are_pinned() {
        assert_eq!(SERVICE_NAME, "AriaHost");
        assert_eq!(SERVICE_DISPLAY_NAME, "AriaHost");
    }

    /// 启动类型的文本是面板/脚本的判断依据，改了就是破坏契约。
    #[test]
    fn start_modes_have_stable_texts() {
        assert_eq!(ServiceStartMode::Auto.as_str(), "auto");
        assert_eq!(ServiceStartMode::Demand.as_str(), "demand");
        assert_eq!(ServiceStartMode::Disabled.as_str(), "disabled");
        assert_eq!(ServiceStartMode::System.as_str(), "system");
        assert_eq!(ServiceStartMode::Boot.as_str(), "boot");
    }

    /// 「未安装」的报告必须自洽：installed=false 时不带 start_type/pid，状态就是 not-installed。
    #[test]
    fn not_installed_report_is_self_consistent() {
        let report = ServiceStatusReport {
            installed: false,
            state: ServiceState::NotInstalled,
            start_type: None,
            pid: None,
        };
        assert!(!report.installed);
        assert_eq!(report.state.as_str(), "not-installed");
        assert!(!report.state.is_installed());
        assert!(report.start_type.is_none());
        assert!(report.pid.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn status_report_returns_unsupported_on_non_windows() {
        let error = status_report().expect_err("非 Windows 上不该伪造服务状态");
        assert_eq!(error.code, ErrorCode::ServiceUnavailable);
        assert_eq!(error.code.code(), "AH-E5001");
        assert!(error.detail.contains("SCM"));
    }

    #[cfg(windows)]
    #[test]
    fn stopped_status_reports_stopped() {
        use windows_service::service::ServiceState as WinServiceState;

        assert_eq!(stopped_status().current_state, WinServiceState::Stopped);
    }

    #[test]
    fn service_state_strings_are_pinned() {
        assert_eq!(ServiceState::NotInstalled.as_str(), "not-installed");
        assert_eq!(ServiceState::Stopped.as_str(), "stopped");
        assert_eq!(ServiceState::StartPending.as_str(), "start-pending");
        assert_eq!(ServiceState::Running.as_str(), "running");
        assert_eq!(ServiceState::StopPending.as_str(), "stop-pending");
        assert_eq!(ServiceState::Unknown.as_str(), "unknown");
    }

    #[test]
    fn only_running_is_running_and_not_installed_is_not_installed() {
        assert!(ServiceState::Running.is_running());
        for state in [
            ServiceState::NotInstalled,
            ServiceState::Stopped,
            ServiceState::StartPending,
            ServiceState::StopPending,
            ServiceState::Unknown,
        ] {
            assert!(!state.is_running(), "{} 不应算运行中", state.as_str());
        }
        assert!(!ServiceState::NotInstalled.is_installed());
        assert!(ServiceState::Running.is_installed());
        assert!(ServiceState::Unknown.is_installed());
    }

    #[test]
    fn query_error_display_starts_with_code() {
        let error = ServiceQueryError::new(ErrorCode::ServiceUnavailable, "打开本机 SCM 失败");
        assert_eq!(
            error.to_string(),
            "AH-E5001 打开本机 SCM 失败",
            "诊断文本必须以错误码开头"
        );
    }

    #[test]
    fn outcomes_have_stable_strings() {
        assert_eq!(InstallOutcome::Created.as_str(), "created");
        assert_eq!(InstallOutcome::AlreadyExisted.as_str(), "already-existed");
        assert_eq!(UninstallOutcome::Removed.as_str(), "removed");
        assert_eq!(UninstallOutcome::NotInstalled.as_str(), "not-installed");
    }

    #[test]
    fn error_codes_follow_the_documented_mapping() {
        let needs_admin = ServiceError::NeedsAdmin {
            detail: "拒绝访问".to_owned(),
        };
        assert_eq!(needs_admin.code(), ErrorCode::ServiceNeedsAdmin);
        assert!(needs_admin.to_string().starts_with("AH-E5002"));
        assert!(
            needs_admin.to_string().contains("管理员"),
            "必须给可执行建议"
        );

        assert_eq!(
            ServiceError::RegisterPrereq {
                detail: "建 ProgramData 失败".to_owned()
            }
            .code(),
            ErrorCode::ServiceRegisterPrereq
        );
        assert_eq!(
            ServiceError::NotFromScm {
                detail: "手工运行".to_owned()
            }
            .code(),
            ErrorCode::ServiceUnavailable
        );
        assert_eq!(
            ServiceError::NotInstalled {
                detail: "没装".to_owned()
            }
            .code(),
            ErrorCode::ServiceUnavailable
        );
        // 启动失败透传上游码（真值优先，不硬塞 E5）。
        let startup = ServiceError::StartupFailed {
            code: ErrorCode::Aria2StartTimeout,
            detail: "15s 未就绪".to_owned(),
        };
        assert_eq!(startup.code(), ErrorCode::Aria2StartTimeout);
        assert!(startup.to_string().starts_with("AH-E2002"));
        assert_eq!(startup.detail(), "15s 未就绪");
    }

    #[test]
    fn expected_binary_path_matches_the_registration_form() {
        // M3 安装器会调这个函数拼期望值；和 install.rs 的单测保持一致。
        assert_eq!(
            expected_binary_path(Path::new(r"C:\Program Files\AriaHost\ariahost.exe")),
            r#""C:\Program Files\AriaHost\ariahost.exe" --service"#
        );
        assert_eq!(is_supported_platform(), cfg!(windows));
    }

    #[cfg(windows)]
    #[test]
    fn maps_windows_service_states_to_panel_states() {
        use windows_service::service::ServiceState as WinServiceState;

        assert_eq!(
            state::map_state(WinServiceState::Stopped),
            ServiceState::Stopped
        );
        assert_eq!(
            state::map_state(WinServiceState::StartPending),
            ServiceState::StartPending
        );
        assert_eq!(
            state::map_state(WinServiceState::StopPending),
            ServiceState::StopPending
        );
        assert_eq!(
            state::map_state(WinServiceState::Running),
            ServiceState::Running
        );
        // 本产品不暂停服务：暂停系列一律归 Unknown，不猜成 Stopped/Running。
        for paused in [
            WinServiceState::Paused,
            WinServiceState::PausePending,
            WinServiceState::ContinuePending,
        ] {
            assert_eq!(
                state::map_state(paused),
                ServiceState::Unknown,
                "{paused:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn absent_service_name_is_not_installed_not_an_error() {
        // 这个名字保证不存在：服务不存在时必须返回 NotInstalled，而不是 Err。
        let state = query_state_of("AriaHost-CI-Must-Not-Exist-4C1F").expect("服务不存在不是错误");
        assert_eq!(state, ServiceState::NotInstalled);
    }

    #[cfg(windows)]
    #[test]
    fn pinned_service_name_reports_not_installed_on_clean_machine() {
        // 前提：CI runner 上未安装 AriaHost 服务（M1-2B 不会在 CI 里安装）。
        // 这条用例就是「只读查询能跑通」的最小证据：读状态不需要管理员。
        let state = query_state().expect("查询 AriaHost 服务状态不应报错");
        assert_eq!(
            state,
            ServiceState::NotInstalled,
            "干净的 CI runner 上不应有 AriaHost 服务；实际 {}",
            state.as_str()
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "手工用：需管理员 + 本机已安装 AriaHost 服务"]
    fn query_state_reports_installed_service_manually() {
        let state = query_state().expect("查询 AriaHost 服务状态不应报错");
        assert!(
            state.is_installed(),
            "本机应已安装 AriaHost 服务，实际 {}",
            state.as_str()
        );
    }
}
