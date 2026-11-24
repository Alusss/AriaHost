//! 非 Windows 占位（保证 workspace 在非 Windows 上也能编译；CI 与发布只针对 Windows）。
//!
//! 这里每个入口都返回 [`ServiceError::Unsupported`]——**不是静默降级**：
//! 调用方能拿到明确错误与 `AH-E5001`，而不是「假装成功」。

use std::path::Path;

use crate::{InstallOutcome, ServiceError, ServiceState, UninstallOutcome};

/// 非 Windows 没有 SCM，服务模式不可用。
pub fn run() -> Result<(), ServiceError> {
    Err(ServiceError::Unsupported)
}

/// 非 Windows 不支持安装服务。
pub fn install(_exe_path: &Path) -> Result<InstallOutcome, ServiceError> {
    Err(ServiceError::Unsupported)
}

/// 非 Windows 不支持卸载服务。
pub fn uninstall() -> Result<UninstallOutcome, ServiceError> {
    Err(ServiceError::Unsupported)
}

/// 非 Windows 不支持启动服务。
pub fn start() -> Result<ServiceState, ServiceError> {
    Err(ServiceError::Unsupported)
}

/// 非 Windows 不支持停止服务。
pub fn stop() -> Result<ServiceState, ServiceError> {
    Err(ServiceError::Unsupported)
}
