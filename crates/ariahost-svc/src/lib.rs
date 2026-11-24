//! ServiceHost 骨架（架构 §3.2/§4.2.3）。
//!
//! 本批（M1-2A）只新增**只读**能力：[`query_state`] / [`query_state_of`] 打开本机 SCM、
//! 读一次服务状态、关句柄。**不安装、不卸载、不启停、不改配置**——那些属 M1-2B。
//! 读状态不需要管理员权限（`SC_MANAGER_CONNECT` + `SERVICE_QUERY_STATUS`）。
//!
//! [`run`] 仍是 M1 占位：只返回 [`ServiceError::Unsupported`]，由 `ariahost --service`
//! 打印 `AH-E5001` 并以退出码 1 结束。M2 在此接入 `windows-service` 的 `service_dispatcher`、
//! 控制处理器与 Job Object。

use std::fmt;

use ariahost_common::ErrorCode;

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

#[cfg(windows)]
mod state {
    //! Windows 实现：`windows-service` 0.8.1（API 以依赖源码为准，见各调用的签名说明）。

    use std::ffi::OsStr;

    use ariahost_common::ErrorCode;
    use windows_service::service::ServiceAccess;
    use windows_service::service::ServiceState as WinServiceState;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    use super::{ServiceQueryError, ServiceState};

    /// `ERROR_SERVICE_DOES_NOT_EXIST`（Win32 1060）：SCM 里没有这个名字的服务。
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    /// `ERROR_ACCESS_DENIED`（Win32 5）：没有查询权限（正常用户读状态不需要管理员，
    /// 但被策略收紧的机器/AACL 改动会走到这里）。
    const ERROR_ACCESS_DENIED: i32 = 5;

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
            Err(error) => {
                // 「没装」是面板第一屏的常态，不是错误。
                if win32_code(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) {
                    return Ok(ServiceState::NotInstalled);
                }
                return Err(map_error(&error, "打开服务"));
            }
        };

        let status = service
            .query_status()
            .map_err(|error| map_error(&error, "查询服务状态"))?;
        Ok(map_state(status.current_state))
    }

    /// 取 Win32 错误码。`windows_service::Error` 只有 `Winapi` 变体带 `io::Error`。
    fn win32_code(error: &windows_service::Error) -> Option<i32> {
        match error {
            windows_service::Error::Winapi(io_error) => io_error.raw_os_error(),
            _ => None,
        }
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

    use super::{ServiceQueryError, ServiceState};

    /// 非 Windows 平台没有 SCM，直接报「服务不可用」。
    pub fn query_state_of(_name: &str) -> Result<ServiceState, ServiceQueryError> {
        Err(ServiceQueryError::new(
            ErrorCode::ServiceUnavailable,
            "非 Windows 平台不支持 SCM 查询".to_owned(),
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

/// ServiceHost 启动错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    /// M1 占位：ServiceHost 尚未实现。
    Unsupported,
}

impl ServiceError {
    /// 对应错误码（M1 占位复用 `AH-E5001`：SCM 启动失败/服务不可用）。
    pub const fn code(self) -> ErrorCode {
        ErrorCode::ServiceUnavailable
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(
                formatter,
                "{} 服务宿主尚未实现（M1 骨架，M2 接入 SCM）",
                self.code().code()
            ),
        }
    }
}

impl std::error::Error for ServiceError {}

#[cfg(windows)]
mod platform {
    use super::ServiceError;

    /// 服务入口（M1 桩）：不调用 `service_dispatcher::start`，直接返回 Unsupported。
    ///
    /// 原因：本批不接管 SCM；在非服务上下文调用 `start` 会得到 1063（服务未连接），
    /// 既不是本批要验证的行为，也会让本地 `--service` 冒烟测试出现误导性错误。
    pub fn run() -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::ServiceError;

    /// 非 Windows 平台占位：保证 workspace 在非 Windows 上也能编译（CI 只编译 Windows）。
    pub fn run() -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported)
    }
}

pub use platform::run;

/// 构造「已停止」的 SCM 服务状态，M2 的控制循环直接复用。
///
/// 本批只作为 `windows-service` 依赖的骨架锚点：**不调用任何 SCM API**，
/// 真正的 `service_dispatcher::start` 与控制处理器在 M2 接入。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_returns_unsupported_with_service_code() {
        let error = run().expect_err("M1 的服务入口必须是 Unsupported 桩");
        assert_eq!(error, ServiceError::Unsupported);
        assert_eq!(error.code().code(), "AH-E5001");
        assert!(error.to_string().starts_with("AH-E5001"));
    }

    #[test]
    fn service_names_are_pinned() {
        assert_eq!(SERVICE_NAME, "AriaHost");
        assert_eq!(SERVICE_DISPLAY_NAME, "AriaHost");
    }

    #[cfg(windows)]
    #[test]
    fn stopped_status_reports_stopped() {
        use windows_service::service::ServiceState as WinServiceState;

        assert_eq!(stopped_status().current_state, WinServiceState::Stopped);
    }

    // ---- M1-2A：只读状态查询 ----

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
        // 前提：CI runner 上未安装 AriaHost 服务（M1-2B 之前不存在任何安装路径）。
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
    #[ignore = "手工用：需本机已安装 AriaHost 服务（M1-2B 之后才能造出这种环境）"]
    fn query_state_reports_installed_service_manually() {
        let state = query_state().expect("查询 AriaHost 服务状态不应报错");
        assert!(
            state.is_installed(),
            "本机应已安装 AriaHost 服务，实际 {}",
            state.as_str()
        );
    }
}
