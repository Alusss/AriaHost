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

/// M5-f：安装器传「安装用户的配置路径」用的环境变量名。
///
/// 为什么走环境变量而不是命令行参数：`--install-service` 的**参数契约在
/// `crates/ariahost/src/cli.rs`**（本批写域之外），而安装器早已用 cmd 包了一层提权调用——
/// 在那里 `set AH_CONFIG_PATH=<安装用户算好的字面路径> && ...` 是最小改动。
///
/// 为什么必须是「安装用户算好的字面路径」：提权后进程的 `%APPDATA%` 可能属于**另一个**
/// 管理员账户；助手自己展开会指到别人家里去（那正是 M5-f 这个 P0 的同类问题）。
pub const CONFIG_PATH_ENV: &str = "AH_CONFIG_PATH";

/// 读安装器传进来的配置路径（绝对路径才算数；其它形态当没传）。
pub fn requested_config_path() -> Option<std::path::PathBuf> {
    config_path_from(std::env::var_os(CONFIG_PATH_ENV).as_deref())
}

/// [`requested_config_path`] 的判据本体（拆出来才能在 Linux 门禁里单测：
/// 单测改环境变量在并行测试下不安全，Rust 2024 起还是 `unsafe`）。
pub fn config_path_from(raw: Option<&std::ffi::OsStr>) -> Option<std::path::PathBuf> {
    let raw = raw?;
    let text = raw.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 只接受绝对路径：相对路径在这里没有任何合理含义（助手的工作目录是 system32）。
    //
    // 判据与 `arihost_common::config::is_absolute_str` 同一套口径：`Path::is_absolute()
    // || 盘符绝对 || UNC`。**为什么不能只认盘符/UNC**：Linux 门禁要用 `/tmp/...` 跑
    // 「env 合法」那条分支，只认 Windows 形态会让那条分支永远测不到（假绿）。Windows 上
    // `Path::is_absolute()` 对 `C:\x`、`\\server\share` 本来也为真，语义不变。
    let bytes = trimmed.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = trimmed.starts_with("\\\\");
    if !(std::path::Path::new(trimmed).is_absolute() || drive_absolute || unc) {
        return None;
    }
    Some(std::path::PathBuf::from(trimmed))
}

// ---------------------------------------------------------------------------
// M6-bA：装服务前的配置准备（自算 / 自建 / fail-closed 校验）
// ---------------------------------------------------------------------------

/// 装服务前准备好的配置：要写进 `Parameters\ConfigPath` 的路径 + 可读过程记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfigReady {
    /// 配置文件的绝对路径（`Parameters\ConfigPath` 就写它）。
    pub path: std::path::PathBuf,
    /// 是不是本次**自建**的（true = 刚从零落了一份最小 TOML）。
    pub created: bool,
    /// 过程记录（成功也要带回日志：让 service.log 说清「这份配置哪来的」）。
    pub notes: Vec<String>,
}

/// 拒绝装服务的原因类别。**面板只负责显示 [`ServiceConfigProblem::message`]**，
/// 不许在 UI 里再抄一份文案（两份实现必然漂移，本项目已经复发过一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceConfigProblemKind {
    /// 连「用户的配置目录」都算不出来（`%APPDATA%` 缺失）。
    PathUnavailable,
    /// 自建默认配置失败（目录不可写等）。
    CannotCreate,
    /// 文件在，但读不动或 TOML 坏。
    Unreadable,
    /// `dl.dir` 过不了服务侧守卫（`download_dir_for_service`）。
    ForbiddenDownloadDir,
}

/// 装前校验失败：带类别 + 路径 + 细节，`message()` 是**唯一**的用户文案来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfigProblem {
    /// 类别。
    pub kind: ServiceConfigProblemKind,
    /// 出问题的文件（若有）。
    pub path: Option<std::path::PathBuf>,
    /// 技术细节（进日志；也可作为文案的一部分）。
    pub detail: String,
}

impl ServiceConfigProblem {
    fn new(
        kind: ServiceConfigProblemKind,
        path: Option<&std::path::Path>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            path: path.map(std::path::Path::to_path_buf),
            detail: detail.into(),
        }
    }

    /// **面向用户的四段文案**（面板直接显示这一段，不要改写/复制）。
    ///
    /// 四段固定：① 确切键名与文件路径 ② 为什么不行（服务是 SYSTEM，不能写用户目录或
    /// 受保护目录）③ 建议值 ④ 下一步动作（先在面板设置里保存）。
    pub fn message(&self) -> String {
        let file = self
            .path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "（未知路径）".to_owned());
        let head = match self.kind {
            ServiceConfigProblemKind::PathUnavailable => format!(
                "找不到用户的配置目录（%APPDATA% 不可用），无法为服务准备配置：{}",
                self.detail
            ),
            ServiceConfigProblemKind::CannotCreate => {
                format!("无法创建服务要读取的配置文件：{file}")
            }
            ServiceConfigProblemKind::Unreadable => {
                format!("配置文件无法解析（键 [dl] dir 读不出来）：{file}")
            }
            ServiceConfigProblemKind::ForbiddenDownloadDir => {
                format!("配置文件里的 [dl] dir 不能给服务用：{file}")
            }
        };
        format!(
            "{head}\n\
             \n\
             原因：服务以 SYSTEM 账户运行，它不能（也不该）把文件写进你的用户目录或系统保留目录，\
             否则下载会落到你既看不见也改不动的位置。\n\
             建议：把 [dl] dir 改成 %ProgramData%\\AriaHost\\downloads（或任意你自己的绝对目录）。\n\
             下一步：先在面板「设置」里把下载目录保存好，再回来装服务。\n\
             细节：{}",
            self.detail
        )
    }
}

impl std::fmt::Display for ServiceConfigProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

/// 装服务前把配置准备到位：**自算路径 → 缺文件就自建 → 装前 fail-closed 校验**。
///
/// 这是 `--install-service` 的**唯一**配置入口（A+B 一体）。以前只有
/// [`requested_config_path`]（只认外部喂），面板「一键装服务」不传 `AH_CONFIG_PATH`，
/// 于是注册表 `Parameters\ConfigPath` 不写 → 服务侧第 1/2 级全空 → 落到内置默认 →
/// `dl.dir` 被按 SYSTEM 的 profile 展开 → M5-f 守卫拦下 → **装完起不来**。
///
/// 三步（与任务书的 A 一致）：
/// 1. `AH_CONFIG_PATH` 有值且（绝对 + 文件存在）→ 用它（安装器路径，行为不变）；
/// 2. 有值但非法 → **忽略 + 记一条**（兜住 NSIS 老引号 bug 写进注册表的垃圾值），继续 3；
/// 3. 自算 `%APPDATA%\AriaHost\config.toml`；文件不存在就**落一份最小 TOML**并记一条。
///
/// # 与安装器的成对实现（改一处必须改另一处）
///
/// 这一步与 `installer/ariahost.nsi` 的 `EnsureUserConfig` 是**同一个行为的两份实现**：
/// 同样的最小 TOML 内容（`schema_version = 1` + `[dl] dir`）、同样的默认目录
/// （`<%USERPROFILE%>\Downloads`）、同样只在文件缺失时才创建。安装器那条路为的是
/// 「安装时就绪」，这里为的是「面板/手工 `--install-service` 时也就绪」。**改内容或
/// 改默认目录时两处必须一起改**（NSIS 侧由 builder-svc 维护）。
pub fn resolve_service_config() -> Result<ServiceConfigReady, ServiceConfigProblem> {
    let env = std::env::var_os(CONFIG_PATH_ENV);
    let default_path = ariahost_common::config::default_path();
    resolve_service_config_with(env.as_deref(), default_path.as_deref())
}

/// [`resolve_service_config`] 的可注入版本（单测用：不改进程环境变量）。
pub fn resolve_service_config_with(
    env: Option<&std::ffi::OsStr>,
    user_config: Option<&Path>,
) -> Result<ServiceConfigReady, ServiceConfigProblem> {
    let mut notes = Vec::new();

    // ---- 步 1/2：安装器传进来的路径 ----
    if let Some(raw) = env {
        match config_path_from(Some(raw)) {
            Some(path) if path.is_file() => {
                validate_service_config(&path)?;
                notes.push(format!(
                    "服务配置沿用安装器传入的路径：{}（{}）",
                    path.display(),
                    CONFIG_PATH_ENV
                ));
                return Ok(ServiceConfigReady {
                    path,
                    created: false,
                    notes,
                });
            }
            Some(path) => notes.push(format!(
                "忽略安装器传入的 {CONFIG_PATH_ENV}={:?}：文件不存在（{}）——改用自算路径",
                raw.to_string_lossy(),
                path.display()
            )),
            None => notes.push(format!(
                "忽略安装器传入的 {CONFIG_PATH_ENV}={:?}：不是绝对路径（可能来自引号拼接错误）——改用自算路径",
                raw.to_string_lossy()
            )),
        }
    }

    // ---- 步 3：自算 + 缺文件就自建 ----
    let path = user_config.map(Path::to_path_buf).ok_or_else(|| {
        ServiceConfigProblem::new(
            ServiceConfigProblemKind::PathUnavailable,
            None,
            "拿不到 %APPDATA%（服务需要一个绝对路径来定位用户的 config.toml）",
        )
    })?;

    let mut created = false;
    if !path.is_file() {
        create_default_config(&path)?;
        created = true;
        notes.push(format!(
            "自建了服务要读取的默认配置：{}（下载目录 {}）",
            path.display(),
            default_download_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "（拿不到 %USERPROFILE%）".to_owned())
        ));
    } else {
        notes.push(format!("服务配置使用自算路径：{}", path.display()));
    }

    // ---- B：装前 fail-closed 校验 ----
    validate_service_config(&path)?;
    Ok(ServiceConfigReady {
        path,
        created,
        notes,
    })
}

/// 自建配置里的默认下载目录：`<%USERPROFILE%>\Downloads`。
fn default_download_dir() -> Option<std::path::PathBuf> {
    ariahost_common::config::user_home().map(|home| home.join("Downloads"))
}

/// 落一份**最小**合法 TOML（与 NSI 的 `EnsureUserConfig` 内容一致）。
///
/// 为什么是最小而不是完整默认值：完整默认值有五十多个键，写死一份等于把
/// `Config::default()` 复制到两个语言里——那才是真正会漂移的东西。最小文件让
/// 「其余键」永远由 Rust 侧 `Config` 的 serde 默认值决定。
///
/// 为什么默认目录写死成字面量路径而不是 `~/Downloads`：`~` 由**服务进程**展开会落到
/// SYSTEM 的 profile（正是那个 P0）；这里必须是一个真正的绝对路径。
fn create_default_config(path: &Path) -> Result<(), ServiceConfigProblem> {
    let download = default_download_dir().ok_or_else(|| {
        ServiceConfigProblem::new(
            ServiceConfigProblemKind::PathUnavailable,
            Some(path),
            "拿不到 %USERPROFILE%（无法为默认下载目录算出绝对路径）",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ServiceConfigProblem::new(
                ServiceConfigProblemKind::CannotCreate,
                Some(parent),
                format!("建目录失败：{error}"),
            )
        })?;
    }
    // TOML 字面量字符串（单引号）：Windows 路径里全是反斜杠，基本字符串要转义 `\U` 之类。
    let text = format!(
        "schema_version = 1\n\
         \n\
         # 由 --install-service 生成：服务（SYSTEM）要读这份配置。\n\
         # 下载目录必须是绝对路径：服务身份下 `~` 会展开到系统账户目录。\n\
         [dl]\n\
         dir = '{}'\n",
        download.display()
    );
    std::fs::write(path, text).map_err(|error| {
        ServiceConfigProblem::new(
            ServiceConfigProblemKind::CannotCreate,
            Some(path),
            format!("写文件失败：{error}"),
        )
    })
}

/// 装前 fail-closed 校验：文件在、能解析、`dl.dir` 过得了服务侧守卫。
///
/// 只**调用** `download_dir_for_service`（`crates/ariahost-common` 由 task-94 在加固，
/// 本任务不改它实现）——守卫规则要变时两边自然同步。
fn validate_service_config(path: &Path) -> Result<(), ServiceConfigProblem> {
    if !path.is_file() {
        return Err(ServiceConfigProblem::new(
            ServiceConfigProblemKind::Unreadable,
            Some(path),
            "配置文件不存在（服务起来后会退回内置默认值，而默认值过不了下载目录守卫）",
        ));
    }
    let config = ariahost_common::config::load_from(path).map_err(|error| {
        ServiceConfigProblem::new(
            ServiceConfigProblemKind::Unreadable,
            Some(path),
            error.to_string(),
        )
    })?;
    ariahost_common::config::download_dir_for_service(&config).map_err(|error| {
        ServiceConfigProblem::new(
            ServiceConfigProblemKind::ForbiddenDownloadDir,
            Some(path),
            error.to_string(),
        )
    })?;
    Ok(())
}

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

    /// 每个用例一个独立目录。
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join("ariahost-svc-install")
                .join(format!("{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn config(&self) -> std::path::PathBuf {
            self.0.join("config.toml")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 写一个 `dl.dir = <dir>` 的合法配置。
    fn write_config(path: &std::path::Path, dir: &str) {
        let text = format!("schema_version = 1\n[dl]\ndir = '{dir}'\n");
        std::fs::write(path, text).expect("写配置文件");
    }

    // ---- M6-bA：装前配置准备三分支 ----------------------------------------

    /// 分支 1：`AH_CONFIG_PATH` 合法且文件存在 → 直接用它（安装器路径，行为不变）。
    #[test]
    fn service_config_uses_valid_env_path_unchanged() {
        let dir = TestDir::new("cfg-env-ok");
        let config = dir.config();
        write_config(&config, r"D:\Downloads\aria2");
        let raw = std::ffi::OsString::from(config.display().to_string());

        let ready = resolve_service_config_with(Some(raw.as_os_str()), None).expect("应就绪");
        assert_eq!(ready.path, config);
        assert!(!ready.created, "沿用既有文件时不该标成自建");
        assert!(ready
            .notes
            .iter()
            .any(|note| note.contains(CONFIG_PATH_ENV)));
    }

    /// 分支 2：`AH_CONFIG_PATH` 非法（相对路径，兜住 NSIS 老引号 bug）→ 忽略 + 自算。
    #[test]
    fn service_config_ignores_illegal_env_and_falls_back() {
        let dir = TestDir::new("cfg-env-bad");
        let config = dir.config();
        write_config(&config, r"D:\Downloads\aria2");
        let bad = std::ffi::OsString::from("config.toml");

        let ready =
            resolve_service_config_with(Some(bad.as_os_str()), Some(&config)).expect("应就绪");
        assert_eq!(ready.path, config);
        assert!(
            ready.notes.iter().any(|note| note.contains("不是绝对路径")),
            "必须留下「忽略了非法 env」的记录：{:?}",
            ready.notes
        );
    }

    /// 分支 3：env 缺失 + 文件不存在 → 自算并**自建**，且自建的配置能被读回。
    #[test]
    fn service_config_self_creates_minimal_config_when_missing() {
        let dir = TestDir::new("cfg-create");
        let config = dir.config();
        assert!(!config.exists(), "前置条件：文件本来不存在");

        let ready = resolve_service_config_with(None, Some(&config)).expect("应就绪");
        assert_eq!(ready.path, config);
        assert!(ready.created, "缺文件时必须标成自建");
        assert!(config.is_file(), "必须真的落盘");

        // 自建的 TOML 要能按服务侧同一套解析读回，且 dir 就是 <USERPROFILE>\Downloads。
        let loaded = ariahost_common::config::load_from(&config).expect("自建配置必须可解析");
        let expected = default_download_dir().expect("本机应能算出 USERPROFILE/HOME");
        assert_eq!(
            loaded.dl.dir,
            expected.display().to_string(),
            "默认下载目录必须与 NSI 的 EnsureUserConfig 一致（<用户主目录>\\Downloads）"
        );
        // 更强的一层：走**服务侧那条链**（`load_for_service_from`）把刚建的文件读回来。
        // 写进注册表后，服务启动时正是按第 1 级（Parameters\ConfigPath）命中这份文件。
        let chain = ariahost_common::config::load_for_service_from(
            &ariahost_common::config::ServiceConfigCandidates {
                service_parameter: Some(config.display().to_string()),
                machine_wide: None,
            },
        );
        assert_eq!(
            chain.config.dl.dir,
            expected.display().to_string(),
            "服务侧必须在第 1 级就读到自建的那份配置：{:?}",
            chain.notes
        );
        assert!(
            matches!(
                chain.source,
                ariahost_common::config::ServiceConfigSource::ServiceParameter(_)
            ),
            "自建配置必须作为第 1 级命中，实际：{:?}",
            chain.source
        );
        // 必须是字面量字符串形态（单引号）：TOML 基本字符串里 `\U` 是非法转义。
        let text = std::fs::read_to_string(&config).expect("读回");
        assert!(
            text.contains(&format!("dir = '{}'", expected.display())),
            "{text}"
        );
        // 交付报告直接引用：自建内容 + 过程记录（哪来的、有没有自建）。
        eprintln!("CREATED_TOML_BEGIN\n{text}CREATED_TOML_END");
        eprintln!("CREATED_NOTES {:?}", ready.notes);
    }

    /// 已有配置文件时**不覆盖**（幂等：面板重装服务不该冲掉用户设置）。
    #[test]
    fn service_config_does_not_overwrite_existing_file() {
        let dir = TestDir::new("cfg-idempotent");
        let config = dir.config();
        write_config(&config, r"E:\MyDownloads");

        let ready = resolve_service_config_with(None, Some(&config)).expect("应就绪");
        assert!(!ready.created, "文件已在，不该标成自建");
        let loaded = ariahost_common::config::load_from(&config).expect("可解析");
        assert_eq!(loaded.dl.dir, r"E:\MyDownloads", "用户设置不许被覆盖");
    }

    /// B 条判据 2：`dl.dir` 过不了服务侧守卫 → 拒注册，且**文案四段齐全**。
    #[test]
    fn service_config_refuses_a_dir_the_service_guard_rejects() {
        let dir = TestDir::new("cfg-forbidden");
        let config = dir.config();
        write_config(
            &config,
            r"C:\WINDOWS\system32\config\systemprofile\Downloads",
        );

        let problem =
            resolve_service_config_with(None, Some(&config)).expect_err("systemprofile 必须拒注册");
        assert_eq!(problem.kind, ServiceConfigProblemKind::ForbiddenDownloadDir);

        let message = problem.message();
        // 四段：① 键名与路径 ② 为什么 ③ 建议值 ④ 下一步
        assert!(message.contains("[dl] dir"), "要点名确切键名：{message}");
        assert!(message.contains("SYSTEM"), "要说清为什么：{message}");
        assert!(
            message.contains(r"%ProgramData%\AriaHost\downloads"),
            "要给建议值：{message}"
        );
        assert!(
            message.contains("设置"),
            "要给下一步（先在面板设置里保存）：{message}"
        );
        // Display 必须与 message 同源（面板只显示一份文案）。
        assert_eq!(problem.to_string(), message);
        // `--nocapture` 时把原文打出来，供交付报告直接引用（不是断言，不影响通过）。
        eprintln!("REFUSAL_MESSAGE_BEGIN\n{message}\nREFUSAL_MESSAGE_END");
    }

    /// B 条判据 3：坏 TOML → 拒注册（不是「当默认值继续」）。
    #[test]
    fn service_config_refuses_a_broken_file() {
        let dir = TestDir::new("cfg-broken");
        let config = dir.config();
        std::fs::write(&config, "schema_version = = 1\n[[[").expect("写坏配置");

        let problem = resolve_service_config_with(None, Some(&config)).expect_err("坏 TOML 必须拒");
        assert_eq!(problem.kind, ServiceConfigProblemKind::Unreadable);
        assert!(
            problem.message().contains("无法解析"),
            "{}",
            problem.message()
        );
    }

    /// 连用户配置目录都算不出来 → 明确报 `PathUnavailable`（不是 panic）。
    #[test]
    fn service_config_reports_missing_user_config_path() {
        let problem = resolve_service_config_with(None, None).expect_err("%APPDATA% 缺失时必须拒");
        assert_eq!(problem.kind, ServiceConfigProblemKind::PathUnavailable);
        assert!(
            problem.message().contains("%APPDATA%"),
            "{}",
            problem.message()
        );
    }

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

    // ---- M5-f：安装器传进来的配置路径 --------------------------------------

    #[test]
    fn config_path_env_only_accepts_absolute_paths() {
        use std::ffi::OsStr;

        let ok = config_path_from(Some(OsStr::new(
            r"C:\Users\alice\AppData\Roaming\AriaHost\config.toml",
        )))
        .expect("盘符绝对路径必须接受");
        assert_eq!(
            ok,
            std::path::PathBuf::from(r"C:\Users\alice\AppData\Roaming\AriaHost\config.toml")
        );

        // 前后空白要吃掉（cmd 的 set 会把空格一起带进来）。
        let padded = config_path_from(Some(OsStr::new("  D:/AriaHost/config.toml  ")))
            .expect("前后空白要 trim");
        assert_eq!(padded, std::path::PathBuf::from("D:/AriaHost/config.toml"));

        // UNC 路径也接受。
        assert!(config_path_from(Some(OsStr::new(r"\\server\share\config.toml"))).is_some());

        // 相对路径 / 空 / 缺失一律当没传（助手的工作目录是 system32，相对路径等于随机位置）。
        for bad in ["config.toml", r"AriaHost\config.toml", "", "   "] {
            assert!(
                config_path_from(Some(OsStr::new(bad))).is_none(),
                "不该接受 {bad:?}"
            );
        }
        assert!(config_path_from(None).is_none());
    }
}
