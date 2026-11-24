//! ServiceHost 骨架（架构 §3.2/§4.2.3）。
//!
//! 本批**不托管 aria2c、不接管 SCM 控制循环**：`run()` 只返回 [`ServiceError::Unsupported`]，
//! 由 `ariahost --service` 打印 `AH-E5001` 并以退出码 1 结束。
//! M2 在此接入 `windows-service` 的 `service_dispatcher`、控制处理器与 Job Object。

use std::fmt;

use ariahost_common::ErrorCode;

/// SCM 服务名（架构 §3.3：服务名与显示名同名 `AriaHost`）。
pub const SERVICE_NAME: &str = "AriaHost";

/// SCM 显示名。
pub const SERVICE_DISPLAY_NAME: &str = "AriaHost";

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
        ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
    };

    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Stopped,
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
        use windows_service::service::ServiceState;

        assert_eq!(stopped_status().current_state, ServiceState::Stopped);
    }
}
