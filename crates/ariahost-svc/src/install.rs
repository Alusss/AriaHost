//! 安装/卸载/启停的**纯逻辑**前置：`binPath` 引号、失败动作、Win32 错误 → 结果分类。
//!
//! 真正的 SCM 调用在 `scm.rs`（仅 Windows）。这里放的全是能在 Linux 上跑单测的决定：
//! 引号怎么加、哪些 Win32 码算「幂等成功」、哪些算「需要管理员」。

use std::path::Path;
use std::time::Duration;

use ariahost_common::ErrorCode;

use crate::plan::{FAILURE_DELAYS, FAILURE_RESET_PERIOD};
use crate::{InstallOutcome, ServiceError, ServiceState, UninstallOutcome};

/// 服务形态启动参数（SCM `binPath` 的尾部）。
pub const SERVICE_ARGUMENT: &str = "--service";

// ---- Win32 错误码（只列本模块要区分的） ----
/// `ERROR_ACCESS_DENIED`：需要管理员。
pub const ERROR_ACCESS_DENIED: i32 = 5;
/// `ERROR_SERVICE_ALREADY_RUNNING`：已经在跑。
pub const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
/// `ERROR_SERVICE_DOES_NOT_EXIST`：没装。
pub const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// `ERROR_SERVICE_NOT_ACTIVE`：本来就没在跑。
pub const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`：不是 SCM 拉起的（手工双击 `--service` 会撞这个）。
pub const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;
/// `ERROR_SERVICE_MARKED_FOR_DELETE`：已被标记删除，句柄还没全关。
pub const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
/// `ERROR_SERVICE_EXISTS`：同名服务已存在。
pub const ERROR_SERVICE_EXISTS: i32 = 1073;

/// SCM 存的服务命令行：`"C:\path with space\ariahost.exe" --service`。
///
/// 三个要点：
/// 1. **exe 路径必须加引号**：不加的话 `C:\Program Files\AriaHost\ariahost.exe` 会被按空格切分，
///    SCM 会去找 `C:\Program.exe`——这是 Windows 服务最经典的坑；
/// 2. 参数放在引号**外面**；
/// 3. 路径里的 `"` 不可能出现（Windows 文件名不允许），所以这里不做转义。
///
/// 注：真正注册时走 `windows-service` 的 `ServiceInfo`（它内部用 `escape_wide` 做同样的加引号），
/// 这个函数是**被钉住的期望值**：用于日志/诊断，以及 `#[ignore]` 的手工核对（读回 `query_config`）。
pub fn service_binary_path(exe: &Path) -> String {
    format!("\"{}\" {SERVICE_ARGUMENT}", exe.display())
}

/// 失败动作的等待时长（架构 §5：restart backoff 1/2/4 秒）。
pub fn failure_delays() -> &'static [Duration; 3] {
    &FAILURE_DELAYS
}

/// 失败计数的重置周期。
pub fn failure_reset_period() -> Duration {
    FAILURE_RESET_PERIOD
}

/// 服务描述（`ChangeServiceConfig2W` 的 `SERVICE_CONFIG_DESCRIPTION`）。
pub const SERVICE_DESCRIPTION: &str = "AriaHost — aria2 下载服务（由 SCM 托管 aria2c 子进程）";

/// `install` 的结果分类（`raw` 是 Win32 错误码；`None` = 调用成功）。
///
/// 幂等口径：同名服务已存在**不算错误**（安装器重跑必须安全），返回
/// [`InstallOutcome::AlreadyExisted`]，由调用方决定是否提示。
pub fn classify_install(
    raw: Option<i32>,
    detail: impl Into<String>,
) -> Result<InstallOutcome, ServiceError> {
    let detail = detail.into();
    match raw {
        None => Ok(InstallOutcome::Created),
        Some(ERROR_SERVICE_EXISTS) | Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {
            Ok(InstallOutcome::AlreadyExisted)
        }
        Some(ERROR_ACCESS_DENIED) => Err(ServiceError::NeedsAdmin { detail }),
        Some(other) => Err(ServiceError::RegisterPrereq {
            detail: format!("{detail}（Win32 错误 {other}）"),
        }),
    }
}

/// `uninstall` 的结果分类。
///
/// 幂等口径：「本来就没装」不算错误，返回 [`UninstallOutcome::NotInstalled`]。
pub fn classify_uninstall(
    raw: Option<i32>,
    detail: impl Into<String>,
) -> Result<UninstallOutcome, ServiceError> {
    let detail = detail.into();
    match raw {
        None => Ok(UninstallOutcome::Removed),
        Some(ERROR_SERVICE_DOES_NOT_EXIST) => Ok(UninstallOutcome::NotInstalled),
        Some(ERROR_ACCESS_DENIED) => Err(ServiceError::NeedsAdmin { detail }),
        Some(other) => Err(ServiceError::Unavailable {
            detail: format!("{detail}（Win32 错误 {other}）"),
        }),
    }
}

/// `start` / `stop` 的结果分类。
///
/// 幂等口径：已运行（`start`）与已停止（`stop`）都算成功——面板连点不该报错。
pub fn classify_control(
    target: ServiceState,
    raw: Option<i32>,
    detail: impl Into<String>,
) -> Result<ServiceState, ServiceError> {
    let detail = detail.into();
    match (target, raw) {
        (_, None) => Ok(target),
        (ServiceState::Running, Some(ERROR_SERVICE_ALREADY_RUNNING)) => Ok(ServiceState::Running),
        (ServiceState::Stopped, Some(ERROR_SERVICE_NOT_ACTIVE)) => Ok(ServiceState::Stopped),
        (ServiceState::Stopped, Some(ERROR_SERVICE_ALREADY_RUNNING)) => Ok(ServiceState::Running),
        (_, Some(ERROR_SERVICE_DOES_NOT_EXIST)) => Err(ServiceError::NotInstalled { detail }),
        (_, Some(ERROR_SERVICE_MARKED_FOR_DELETE)) => Err(ServiceError::Unavailable {
            detail: format!("{detail}（服务已被标记删除，等句柄释放后重试）"),
        }),
        (_, Some(ERROR_ACCESS_DENIED)) => Err(ServiceError::NeedsAdmin { detail }),
        (_, Some(other)) => Err(ServiceError::Unavailable {
            detail: format!("{detail}（Win32 错误 {other}）"),
        }),
    }
}

/// 「不是 SCM 拉起来的」错误（1063）：手工跑 `ariahost --service` 会撞上。
pub fn not_from_scm(detail: impl Into<String>) -> ServiceError {
    ServiceError::NotFromScm {
        detail: detail.into(),
    }
}

/// 服务侧启动失败时写进事件/状态的码：优先用 `AH-E5###`，其余沿用实现给的码。
pub fn startup_failure_code(error: &ServiceError) -> ErrorCode {
    error.code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_path_quotes_the_executable() {
        // 经典坑：路径含空格时不加引号 → SCM 去执行 `C:\Program.exe`。
        assert_eq!(
            service_binary_path(Path::new(r"C:\Program Files\AriaHost\ariahost.exe")),
            r#""C:\Program Files\AriaHost\ariahost.exe" --service"#
        );
        // 不含空格也照样加引号：行为统一，免得「换台机器就炸」。
        assert_eq!(
            service_binary_path(Path::new(r"C:\ariahost\ariahost.exe")),
            r#""C:\ariahost\ariahost.exe" --service"#
        );
        // 参数在引号外，且只有一个。
        let command = service_binary_path(Path::new(r"D:\a b\ariahost.exe"));
        assert!(command.ends_with("\" --service"), "{command}");
        assert_eq!(command.matches('"').count(), 2, "{command}");
    }

    #[test]
    fn install_is_idempotent_when_service_exists() {
        assert_eq!(
            classify_install(None, "").expect("首次"),
            InstallOutcome::Created
        );
        assert_eq!(
            classify_install(Some(ERROR_SERVICE_EXISTS), "已存在").expect("重跑不报错"),
            InstallOutcome::AlreadyExisted
        );
        assert_eq!(
            classify_install(Some(ERROR_SERVICE_MARKED_FOR_DELETE), "待删除").expect("同上"),
            InstallOutcome::AlreadyExisted
        );
    }

    #[test]
    fn install_maps_access_denied_to_needs_admin() {
        let error = classify_install(Some(ERROR_ACCESS_DENIED), "创建服务").expect_err("要管理员");
        assert!(
            matches!(error, ServiceError::NeedsAdmin { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::ServiceNeedsAdmin);
        assert!(error.to_string().starts_with("AH-E5002"), "{error}");
    }

    #[test]
    fn install_other_errors_are_register_prereq() {
        let error = classify_install(Some(87), "创建服务").expect_err("其他错误");
        assert!(
            matches!(error, ServiceError::RegisterPrereq { .. }),
            "{error:?}"
        );
        assert_eq!(error.code(), ErrorCode::ServiceRegisterPrereq);
    }

    #[test]
    fn uninstall_is_idempotent_when_absent() {
        assert_eq!(
            classify_uninstall(None, "").expect("删成功"),
            UninstallOutcome::Removed
        );
        assert_eq!(
            classify_uninstall(Some(ERROR_SERVICE_DOES_NOT_EXIST), "没装").expect("幂等"),
            UninstallOutcome::NotInstalled
        );
    }

    #[test]
    fn uninstall_needs_admin_and_reports_unavailable() {
        assert!(matches!(
            classify_uninstall(Some(ERROR_ACCESS_DENIED), "删除").expect_err("要管理员"),
            ServiceError::NeedsAdmin { .. }
        ));
        let error = classify_uninstall(Some(1075), "删除").expect_err("其他错误");
        assert!(
            matches!(error, ServiceError::Unavailable { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn control_treats_duplicate_state_as_success() {
        assert_eq!(
            classify_control(
                ServiceState::Running,
                Some(ERROR_SERVICE_ALREADY_RUNNING),
                "启动"
            )
            .expect("已在跑算成功"),
            ServiceState::Running
        );
        assert_eq!(
            classify_control(
                ServiceState::Stopped,
                Some(ERROR_SERVICE_NOT_ACTIVE),
                "停止"
            )
            .expect("已停算成功"),
            ServiceState::Stopped
        );
        // 想停却发现已经在跑（SCM 竞态）：如实返回真实状态。
        assert_eq!(
            classify_control(
                ServiceState::Stopped,
                Some(ERROR_SERVICE_ALREADY_RUNNING),
                "停止"
            )
            .expect("竞态"),
            ServiceState::Running
        );
    }

    #[test]
    fn control_reports_not_installed_and_needs_admin() {
        let error = classify_control(
            ServiceState::Running,
            Some(ERROR_SERVICE_DOES_NOT_EXIST),
            "启动",
        )
        .expect_err("没装");
        assert!(
            matches!(error, ServiceError::NotInstalled { .. }),
            "{error:?}"
        );

        let error = classify_control(ServiceState::Stopped, Some(ERROR_ACCESS_DENIED), "停止")
            .expect_err("要管理员");
        assert!(
            matches!(error, ServiceError::NeedsAdmin { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn failure_actions_are_one_two_four_seconds() {
        let delays: Vec<u64> = failure_delays()
            .iter()
            .map(|delay| delay.as_secs())
            .collect();
        assert_eq!(delays, vec![1, 2, 4], "架构 §5：1s → 2s → 4s");
        assert_eq!(failure_reset_period(), Duration::from_secs(3600));
        assert!(!SERVICE_DESCRIPTION.is_empty());
    }
}
