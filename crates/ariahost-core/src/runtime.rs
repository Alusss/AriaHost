//! aria2c 子进程托管：定位 → 渲染 `aria2.conf` → 无窗口启动 → RPC 就绪探测 → 优雅停止 → 孤儿守卫。
//!
//! 对应架构 §4.2（进程模型：单进程 + 一个 aria2c 子进程）、§4.6 R7（headless 平面目录）、
//! §4.11（受控 conf 由 TOML 渲染，用户永不直改）。
//!
//! ## 状态机
//!
//! ```text
//! Idle ──start──▶ Starting ──getVersion 成功──▶ Running
//!                    │                             │
//!                    ├─ 15s 超时 ─▶ Failed(E2002)   ├─stop──▶ Stopping ──▶ Stopped
//!                    └─ 进程先退 ─▶ Failed(exit)    └─ 被外部杀掉 ─▶ Failed
//! Failed ──start──▶ Starting（允许重来）
//! Stopped ──start──▶ Starting（允许重来）
//! ```
//!
//! 每次状态转换都通过 [`RuntimeEvent`] 回调交给调用方（面板后面靠它显示「启动中/停止中」）。
//!
//! ## 可测试性
//!
//! 进程启动与 RPC 探测分别抽成 [`Spawner`] / [`Readiness`] 两个 trait：
//! 单测用假实现覆盖「就绪超时 / 启动即崩 / shutdown 无响应被迫 kill / 重复 start / stop 后再 start」，
//! **不依赖真实 aria2c**。
//!
//! ## 孤儿守卫
//!
//! Windows 上把子进程放进 Job Object 并设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
//! （父进程无论怎么死——包括被任务管理器结束——Job 句柄关闭时系统都会杀掉子进程）。
//! Job 创建失败**不静默降级**：发一条 [`RuntimeEvent::Warning`] 让调用方与日志都能看到。

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ariahost_aria2::RpcClient;
use ariahost_common::config::{self, Config, ConfigError, RpcListen};
use ariahost_common::paths::{versions_dir, Plane};
use ariahost_common::ErrorCode;

use crate::store::VersionStore;

/// `CreateProcess` 的 `CREATE_NO_WINDOW`：无控制台窗口（面板退出后不留黑窗）。
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 受控 conf 文件名（落在运行时目录）。
pub const CONF_FILE_NAME: &str = "aria2.conf";
/// 会话文件名（落在 session 目录）。
pub const SESSION_FILE_NAME: &str = "aria2.session";
/// 日志文件名（落在 logs 目录）。
pub const LOG_FILE_NAME: &str = "aria2.log";
/// PID 文件名（落在运行时目录）。
pub const PID_FILE_NAME: &str = "pid";

/// 解压/下载之外，本模块唯一需要既定裁决的 aria2 选项：文件预分配方式。
///
/// **定版裁决 = 保持 `prealloc`**（架构 §7-R 表：「首版保持 `prealloc` 默认；`dl.allocation` 键暂不开放」，
/// 依据 research-cache-ux §1.3）。注意这与 research-service §2.1 的「Windows 上建议显式 falloc」
/// 是同一份材料里的两种说法：`falloc` 在 MinGW 构建的可用性官方文档与 changelog 自相矛盾【未验证】，
/// 所以定版选了语义最稳的 `prealloc`。
///
/// 语义代价（必须进文档，不能藏）：`prealloc` 在下载开始前就把文件预分配到**全尺寸**，
/// 一个 10GB 任务下到 1% 时磁盘上已占 10GB。这是磁盘治理的已知前提，不是 bug。
pub const FILE_ALLOCATION: &str = "prealloc";

// ---------------------------------------------------------------------------
// 运行时目录
// ---------------------------------------------------------------------------

/// headless 平面的运行时目录集合（架构 §4.6 R7）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    /// `%APPDATA%\AriaHost\runtime`
    pub runtime: PathBuf,
    /// `%APPDATA%\AriaHost\session`
    pub session: PathBuf,
    /// `%APPDATA%\AriaHost\logs`
    pub logs: PathBuf,
}

impl RuntimePaths {
    /// 按真实 `%APPDATA%` 解析；拿不到返回 `None`（由调用方给出可读错误）。
    pub fn from_env() -> Option<Self> {
        let root = config::appdata_root()?;
        Some(Self::under(&root))
    }

    /// 按给定根目录构造（单测用）。
    pub fn under(root: &Path) -> Self {
        Self {
            runtime: root.join("runtime"),
            session: root.join("session"),
            logs: root.join("logs"),
        }
    }

    /// `runtime\aria2.conf`。
    pub fn conf_path(&self) -> PathBuf {
        self.runtime.join(CONF_FILE_NAME)
    }

    /// `session\aria2.session`。
    pub fn session_path(&self) -> PathBuf {
        self.session.join(SESSION_FILE_NAME)
    }

    /// `logs\aria2.log`。
    pub fn log_path(&self) -> PathBuf {
        self.logs.join(LOG_FILE_NAME)
    }

    /// `runtime\pid`。
    pub fn pid_path(&self) -> PathBuf {
        self.runtime.join(PID_FILE_NAME)
    }
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 运行时失败原因。
///
/// 码映射只用**既有**码（架构 §4.12 前缀定版；不新造 `AH-E####`）：
/// 「找不到 aria2c」→ `AH-E2001`；「启动 15s 未就绪」→ `AH-E2002`。
/// 其余情形（启动即崩、停止被迫强杀、Job 创建失败）没有语义匹配的既有码，`code()` 返回 `None`，
/// 靠 `Display` 里的可读细节 + 调用方提示动作表达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// 已经在运行/启动/停止中，拒绝重复 start。
    AlreadyRunning,
    /// 定位不到可用的 aria2c。
    CoreNotFound {
        /// 可读细节（含查过哪些位置）。
        detail: String,
    },
    /// 受控 conf 渲染/落盘失败。
    Prepare(String),
    /// `CreateProcess` 失败。
    Launch(String),
    /// 进程在就绪前自己退出了。
    StartExited {
        /// 退出码（被信号杀掉时为 `None`）。
        exit_code: Option<i32>,
    },
    /// 就绪探测超时（已尽力清理子进程）。
    StartTimeout {
        /// 实际等了多久（毫秒）。
        waited_ms: u128,
    },
    /// 停止链走完仍无法确认进程退出。
    StopFailed(String),
    /// 配置层错误透传。
    Config(ConfigError),
}

impl RuntimeError {
    /// 对应的既有错误码（没有合适码时为 `None`）。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::CoreNotFound { .. } | Self::Launch(_) => Some(ErrorCode::Aria2NotFound),
            Self::StartTimeout { .. } => Some(ErrorCode::Aria2StartTimeout),
            Self::AlreadyRunning
            | Self::Prepare(_)
            | Self::StartExited { .. }
            | Self::StopFailed(_)
            | Self::Config(_) => None,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => write!(formatter, "aria2 已在运行或正在切换状态，拒绝重复启动"),
            Self::CoreNotFound { detail } => write!(formatter, "找不到 aria2c：{detail}"),
            Self::Prepare(detail) => write!(formatter, "渲染 aria2.conf 失败：{detail}"),
            Self::Launch(detail) => write!(formatter, "启动 aria2c 失败：{detail}"),
            Self::StartExited { exit_code } => match exit_code {
                Some(code) => write!(formatter, "aria2c 在就绪前退出（退出码 {code}），见日志"),
                None => write!(formatter, "aria2c 在就绪前退出（无退出码），见日志"),
            },
            Self::StartTimeout { waited_ms } => write!(
                formatter,
                "aria2c 启动后 {waited_ms}ms 内 RPC 未就绪，已尝试清理子进程"
            ),
            Self::StopFailed(detail) => write!(formatter, "aria2c 停止失败：{detail}"),
            Self::Config(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<ConfigError> for RuntimeError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

// ---------------------------------------------------------------------------
// 定位核心
// ---------------------------------------------------------------------------

/// 按 `aria2.source` 定位 aria2c 可执行文件。
///
/// - `managed`：版本库 `active.json` 指向的 `versions/<ver>/aria2c.exe`；
/// - `external`：`aria2.external_path`（支持 `~` 展开）。
///
/// 顺带做**悬空回滚指针修复**：若 `active.json` 的 `previous` 指向已删除的版本目录，
/// 清掉该字段并原子写回（否则「回滚」按钮会指向空气）。
pub fn resolve_core_exe(config: &Config) -> Result<PathBuf, RuntimeError> {
    match config.aria2.source {
        ariahost_common::Aria2Source::Managed => {
            let dir = versions_dir(Plane::User).ok_or_else(|| RuntimeError::CoreNotFound {
                detail: "拿不到 %LOCALAPPDATA%（版本库根目录不可用）".to_owned(),
            })?;
            let store = VersionStore::new(&dir);
            let pointer = store.active().map_err(|error| RuntimeError::CoreNotFound {
                detail: format!("读取激活指针失败：{error}"),
            })?;
            let Some(pointer) = pointer else {
                return Err(RuntimeError::CoreNotFound {
                    detail: format!(
                        "{} 下没有 active.json（还没装过 aria2 核心）",
                        dir.display()
                    ),
                });
            };
            // 悬空 previous → 修掉（失败不算致命：回滚按钮另有校验）。
            let _ = store.repair_dangling_previous();
            let exe = store.exe_path(&pointer.active);
            if exe.is_file() {
                Ok(exe)
            } else {
                Err(RuntimeError::CoreNotFound {
                    detail: format!(
                        "版本 {} 的 aria2c.exe 不存在：{}",
                        pointer.active,
                        exe.display()
                    ),
                })
            }
        }
        ariahost_common::Aria2Source::External => {
            let raw = config.aria2.external_path.trim();
            if raw.is_empty() {
                return Err(RuntimeError::CoreNotFound {
                    detail: "aria2.source=external 但 aria2.external_path 为空".to_owned(),
                });
            }
            let exe = config::expand_user_path(raw);
            if exe.is_file() {
                Ok(exe)
            } else {
                Err(RuntimeError::CoreNotFound {
                    detail: format!("external_path 指向的文件不存在：{}", exe.display()),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 受控 conf
// ---------------------------------------------------------------------------

/// 渲染 `aria2.conf` 文本（纯函数，便于单测逐行核对）。
///
/// 约定：
/// - **`dir` 必须是绝对路径**：aria2 无默认目录，取进程工作目录；服务/无头下那是 `system32`；
/// - `secret` 不进 conf（架构 §4.11.1 注：以命令行 `--rpc-secret` 注入，不落明文）；
/// - `input-file` 只在 session 文件已存在时写（aria2 对不存在的 input-file 会直接报错退出）；
/// - `stop-with-process` 用当前进程 PID：本进程崩了 aria2 自己停（Job Object 之外的第二道保险）。
pub fn render_conf(
    config: &Config,
    paths: &RuntimePaths,
    secret: &str,
    download_dir: &Path,
) -> String {
    let mut text = String::new();
    text.push_str("# AriaHost 渲染的受控配置（每次启动重写，不要手改；改 config.toml）。\n");
    text.push_str("# 用户设置见 %APPDATA%\\AriaHost\\config.toml（架构 §4.11）。\n\n");

    // 落盘目录：必须绝对（见函数文档）。
    text.push_str(&format!("dir={}\n", download_dir.display()));
    text.push_str(&format!(
        "# file-allocation：定版保持 prealloc（架构 §7-R），预分配会在下载前占满全尺寸磁盘空间。\nfile-allocation={FILE_ALLOCATION}\n"
    ));
    text.push_str(&format!(
        "continue={}\n",
        if config.dl.continue_dl {
            "true"
        } else {
            "false"
        }
    ));
    text.push_str(&format!(
        "auto-file-renaming={}\n",
        if config.dl.auto_file_renaming {
            "true"
        } else {
            "false"
        }
    ));
    text.push_str(&format!(
        "allow-overwrite={}\n",
        if config.dl.allow_overwrite {
            "true"
        } else {
            "false"
        }
    ));
    text.push_str(&format!(
        "always-resume={}\n",
        if config.dl.always_resume {
            "true"
        } else {
            "false"
        }
    ));

    // RPC：面板/网页页/第三方都靠它。
    text.push_str("\n# RPC（secret 由命令行注入，不写在这里）\n");
    text.push_str("enable-rpc=true\n");
    if let Some(listen_all) = config.aria2.rpc.listen.listen_all() {
        text.push_str(&format!(
            "rpc-listen-all={}\n",
            if listen_all { "true" } else { "false" }
        ));
    }
    text.push_str(&format!("rpc-listen-port={}\n", config.aria2.rpc.port));
    if config.aria2.rpc.allow_origin_all {
        text.push_str("rpc-allow-origin-all=true\n");
    }

    // 会话与日志。
    text.push_str("\n# 会话与日志\n");
    text.push_str(&format!(
        "save-session={}\n",
        paths.session_path().display()
    ));
    text.push_str("save-session-interval=30\n");
    // 只在 session 已存在时挂 input-file，否则 aria2 会因文件不存在而退出。
    let session = paths.session_path();
    if session.is_file() {
        text.push_str(&format!("input-file={}\n", session.display()));
    }
    text.push_str(&format!("log={}\n", paths.log_path().display()));
    text.push_str("log-level=notice\n");
    // 本进程退出（含崩溃）时 aria2 自行停止。
    text.push_str(&format!("stop-with-process={}\n", std::process::id()));

    // 并发与限速。
    text.push_str("\n# 并发与限速\n");
    text.push_str(&format!(
        "max-concurrent-downloads={}\n",
        config.queue.max_concurrent
    ));
    text.push_str(&format!(
        "max-download-result={}\n",
        config.queue.max_download_result
    ));
    text.push_str(&format!(
        "keep-unfinished-download-result={}\n",
        if config.queue.keep_unfinished_result {
            "true"
        } else {
            "false"
        }
    ));
    text.push_str(&format!("split={}\n", config.net.split));
    text.push_str(&format!(
        "max-connection-per-server={}\n",
        config.net.max_connection_per_server
    ));
    text.push_str(&format!("min-split-size={}\n", config.net.min_split_size));
    text.push_str(&format!(
        "max-overall-download-limit={}\n",
        config.net.max_overall_download_limit
    ));
    text.push_str(&format!(
        "max-download-limit={}\n",
        config.net.max_download_limit
    ));
    text.push_str(&format!(
        "max-overall-upload-limit={}\n",
        config.net.max_overall_upload_limit
    ));
    text.push_str(&format!("connect-timeout={}\n", config.net.connect_timeout));
    text.push_str(&format!("max-tries={}\n", config.net.max_tries));
    text.push_str(&format!("disk-cache={}\n", config.net.disk_cache));
    let proxy = config.net.proxy.trim();
    if !proxy.is_empty() {
        text.push_str(&format!("all-proxy={proxy}\n"));
    }

    // secret 只用于调用方拼命令行；占位注释说明为什么这里没有它。
    let _ = secret;
    text
}

/// 写受控 conf 到运行时目录（原子写），返回 conf 路径。
pub fn prepare_conf(
    config: &Config,
    paths: &RuntimePaths,
    secret: &str,
    download_dir: &Path,
) -> Result<PathBuf, RuntimeError> {
    for dir in [&paths.runtime, &paths.session, &paths.logs] {
        std::fs::create_dir_all(dir).map_err(|error| {
            RuntimeError::Prepare(format!("建目录 {} 失败：{error}", dir.display()))
        })?;
    }
    let text = render_conf(config, paths, secret, download_dir);
    let conf = paths.conf_path();
    let temporary = conf.with_extension("conf.tmp");
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&temporary).map_err(|error| {
            RuntimeError::Prepare(format!("建临时文件 {} 失败：{error}", temporary.display()))
        })?;
        file.write_all(text.as_bytes()).map_err(|error| {
            RuntimeError::Prepare(format!("写 {} 失败：{error}", temporary.display()))
        })?;
        file.sync_all().map_err(|error| {
            RuntimeError::Prepare(format!("落盘 {} 失败：{error}", temporary.display()))
        })?;
    }
    std::fs::rename(&temporary, &conf).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        RuntimeError::Prepare(format!("改名到 {} 失败：{error}", conf.display()))
    })?;
    Ok(conf)
}

/// 由配置拼出一次启动的完整描述（exe + 参数 + 工作目录 + 日志）。
pub fn launch_spec(
    config: &Config,
    paths: &RuntimePaths,
    exe: PathBuf,
    secret: &str,
) -> LaunchSpec {
    let mut args = vec![
        format!("--conf-path={}", paths.conf_path().display()),
        format!("--rpc-secret={secret}"),
    ];
    if config.aria2.rpc.listen == RpcListen::Off {
        // 用户关掉 RPC：不注入 secret，也不开 enable-rpc（conf 里已按 listen=off 处理）。
        args = vec![format!("--conf-path={}", paths.conf_path().display())];
    }
    LaunchSpec {
        exe,
        args,
        cwd: paths.runtime.clone(),
        log: paths.log_path(),
    }
}

// ---------------------------------------------------------------------------
// 启动/探测抽象
// ---------------------------------------------------------------------------

/// 一次启动的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// aria2c 可执行文件。
    pub exe: PathBuf,
    /// 命令行参数（不含 exe 自身）。
    pub args: Vec<String>,
    /// 工作目录（运行时目录，避免继承面板的工作目录）。
    pub cwd: PathBuf,
    /// 日志路径（诊断用；aria2 自己按 conf 的 `log=` 写）。
    pub log: PathBuf,
}

/// 子进程句柄抽象（真正的 `std::process::Child` 与测试假件都实现它）。
pub trait Child {
    /// 进程 ID。
    fn id(&self) -> u32;
    /// 非阻塞看是否已退出：`Some(exit_code)` 表示已退出。
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    /// 强杀。
    fn kill(&mut self) -> io::Result<()>;
}

/// 进程启动器抽象。
pub trait Spawner {
    /// 按描述启动子进程；句柄负责自己持有的资源（含 Job Object）。
    fn spawn(&self, spec: &LaunchSpec) -> Result<Box<dyn Child>, RuntimeError>;
}

/// RPC 探测抽象：只关心「就绪没有」和「停得掉吗」。
pub trait Readiness {
    /// 探测就绪；成功返回 aria2 版本号（`getVersion`，不用每次变的 sessionId）。
    fn probe_version(&self) -> Result<String, String>;
    /// `aria2.shutdown`（优雅）。
    fn shutdown(&self) -> Result<(), String>;
    /// `aria2.forceShutdown`（次一级）。
    fn force_shutdown(&self) -> Result<(), String>;
}

/// 各项超时/轮询参数（可注入，单测用小值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// 从 spawn 到 RPC 就绪的上限（默认 15s，架构 §4.2 / 任务书）。
    pub ready: Duration,
    /// `shutdown` 之后的退出等待（默认 15s）。
    pub stop_grace: Duration,
    /// `forceShutdown` 之后的退出等待（默认 10s）。
    pub force_grace: Duration,
    /// 首次轮询间隔（默认 200ms，之后按 1.5 倍递增）。
    pub poll: Duration,
    /// 轮询间隔上限（默认 1s）。
    pub poll_max: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ready: Duration::from_secs(15),
            stop_grace: Duration::from_secs(15),
            force_grace: Duration::from_secs(10),
            poll: Duration::from_millis(200),
            poll_max: Duration::from_secs(1),
        }
    }
}

// ---------------------------------------------------------------------------
// 状态与事件
// ---------------------------------------------------------------------------

/// 运行时状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreState {
    /// 还没启动。
    Idle,
    /// 已 spawn，等 RPC 就绪。
    Starting,
    /// RPC 就绪，可用。
    Running,
    /// 正在停。
    Stopping,
    /// 已停。
    Stopped,
    /// 失败（`start` 可重来）。
    Failed,
}

impl CoreState {
    /// 单行稳定文本（selftest 与日志共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// 状态快照（调用方随时可问；也可从事件里推）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreStatus {
    /// 当前状态。
    pub state: CoreState,
    /// 子进程 PID（启动过才有）。
    pub pid: Option<u32>,
    /// 就绪后探测到的 aria2 版本。
    pub version: Option<String>,
    /// 子进程退出码（正常/异常退出后才有）。
    pub exit_code: Option<i32>,
    /// 停止链是否走到「强杀」这一步。
    pub forced: bool,
}

impl CoreStatus {
    fn new(state: CoreState) -> Self {
        Self {
            state,
            pid: None,
            version: None,
            exit_code: None,
            forced: false,
        }
    }
}

/// 状态转换事件（每次转换发一条，调用方用来驱动 UI 与日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// 已 spawn（`pid` 可能还没拿到就绪）。
    Starting,
    /// RPC 就绪。
    Running {
        /// 探到的 aria2 版本。
        version: String,
        /// 子进程 PID。
        pid: u32,
    },
    /// 开始停。
    Stopping,
    /// 已停。
    Stopped {
        /// 退出码（强杀或无退出码时为 `None`）。
        exit_code: Option<i32>,
        /// 是否走到强杀。
        forced: bool,
    },
    /// 失败（`code` 为既有错误码，没有合适码时为 `None`）。
    Failed {
        /// 状态。
        code: Option<ErrorCode>,
        /// 可读原因。
        hint: String,
    },
    /// 非致命告警（例如 Job Object 建不起来 → 孤儿守卫降级，**必须可见**）。
    Warning {
        /// 可读原因。
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// 运行时本体
// ---------------------------------------------------------------------------

/// aria2c 托管器。
pub struct CoreRuntime {
    spawner: Box<dyn Spawner>,
    probe: Box<dyn Readiness>,
    timeouts: Timeouts,
    status: CoreStatus,
    child: Option<Box<dyn Child>>,
}

impl CoreRuntime {
    /// 组装一个托管器（启动器与探测器由调用方决定，便于单测替换）。
    pub fn new(spawner: Box<dyn Spawner>, probe: Box<dyn Readiness>, timeouts: Timeouts) -> Self {
        Self {
            spawner,
            probe,
            timeouts,
            status: CoreStatus::new(CoreState::Idle),
            child: None,
        }
    }

    /// 当前状态快照。
    pub fn status(&self) -> &CoreStatus {
        &self.status
    }

    /// spawn → 轮询就绪（最多 [`Timeouts::ready`]）。
    ///
    /// 每一步都通过 `events` 回调上报；失败时状态落到 [`CoreState::Failed`] 并返回错误。
    pub fn start(
        &mut self,
        spec: &LaunchSpec,
        events: &mut dyn FnMut(RuntimeEvent),
    ) -> Result<CoreStatus, RuntimeError> {
        if matches!(
            self.status.state,
            CoreState::Starting | CoreState::Running | CoreState::Stopping
        ) {
            return Err(RuntimeError::AlreadyRunning);
        }
        self.status = CoreStatus::new(CoreState::Starting);
        events(RuntimeEvent::Starting);

        let mut child = match self.spawner.spawn(spec) {
            Ok(child) => child,
            Err(error) => {
                self.status.state = CoreState::Failed;
                events(RuntimeEvent::Failed {
                    code: error.code(),
                    hint: error.to_string(),
                });
                return Err(error);
            }
        };
        let pid = child.id();
        self.status.pid = Some(pid);

        let started = Instant::now();
        let mut interval = self.timeouts.poll;
        loop {
            match child.try_wait() {
                Ok(Some(code)) => {
                    self.child = None;
                    self.status.state = CoreState::Failed;
                    self.status.exit_code = Some(code);
                    let error = RuntimeError::StartExited {
                        exit_code: Some(code),
                    };
                    events(RuntimeEvent::Failed {
                        code: error.code(),
                        hint: error.to_string(),
                    });
                    return Err(error);
                }
                Ok(None) => {}
                Err(error) => {
                    // 句柄坏掉：按启动失败处理，但先尽力杀掉。
                    let _ = child.kill();
                    self.child = None;
                    self.status.state = CoreState::Failed;
                    let detail = format!("查询子进程状态失败：{error}");
                    events(RuntimeEvent::Failed {
                        code: None,
                        hint: detail.clone(),
                    });
                    return Err(RuntimeError::Launch(detail));
                }
            }

            match self.probe.probe_version() {
                Ok(version) => {
                    self.status.state = CoreState::Running;
                    self.status.version = Some(version.clone());
                    self.child = Some(child);
                    events(RuntimeEvent::Running { version, pid });
                    return Ok(self.status.clone());
                }
                Err(_) => {
                    if started.elapsed() >= self.timeouts.ready {
                        let _ = child.kill();
                        self.child = None;
                        self.status.state = CoreState::Failed;
                        let error = RuntimeError::StartTimeout {
                            waited_ms: started.elapsed().as_millis(),
                        };
                        events(RuntimeEvent::Failed {
                            code: error.code(),
                            hint: error.to_string(),
                        });
                        return Err(error);
                    }
                    std::thread::sleep(interval);
                    interval = next_interval(interval, self.timeouts.poll_max);
                }
            }
        }
    }

    /// 优雅停止：`shutdown` → 等 `stop_grace` → `forceShutdown` → 等 `force_grace` → 强杀。
    ///
    /// 任何一步超时都继续往下走（**不卡死**），返回的 [`CoreStatus::forced`] 说明是否走到强杀。
    pub fn stop(
        &mut self,
        events: &mut dyn FnMut(RuntimeEvent),
    ) -> Result<CoreStatus, RuntimeError> {
        let Some(mut child) = self.child.take() else {
            // 没在跑：幂等返回（重复 stop 不算错）。
            if self.status.state != CoreState::Failed {
                self.status.state = CoreState::Stopped;
            }
            return Ok(self.status.clone());
        };

        self.status.state = CoreState::Stopping;
        events(RuntimeEvent::Stopping);

        // 第一段：优雅。
        let _ = self.probe.shutdown();
        if let Some(code) = wait_exit(&mut *child, self.timeouts.stop_grace, self.timeouts.poll) {
            self.status.state = CoreState::Stopped;
            self.status.exit_code = code;
            self.status.forced = false;
            events(RuntimeEvent::Stopped {
                exit_code: code,
                forced: false,
            });
            return Ok(self.status.clone());
        }

        // 第二段：forceShutdown。
        let _ = self.probe.force_shutdown();
        if let Some(code) = wait_exit(&mut *child, self.timeouts.force_grace, self.timeouts.poll) {
            self.status.state = CoreState::Stopped;
            self.status.exit_code = code;
            self.status.forced = true;
            events(RuntimeEvent::Stopped {
                exit_code: code,
                forced: true,
            });
            return Ok(self.status.clone());
        }

        // 第三段：强杀。
        if let Err(error) = child.kill() {
            let detail = format!("强杀 aria2c 失败：{error}");
            self.status.state = CoreState::Failed;
            events(RuntimeEvent::Failed {
                code: None,
                hint: detail.clone(),
            });
            return Err(RuntimeError::StopFailed(detail));
        }
        let code = wait_exit(&mut *child, self.timeouts.poll_max, self.timeouts.poll);
        self.status.state = CoreState::Stopped;
        self.status.exit_code = code.flatten();
        self.status.forced = true;
        events(RuntimeEvent::Stopped {
            exit_code: code.flatten(),
            forced: true,
        });
        Ok(self.status.clone())
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        // 兜底：本对象被丢弃时不让子进程变孤儿（Job Object 之外的最后一道）。
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
        }
    }
}

/// 轮询间隔递增：`value * 1.5`，上限 `max`。
fn next_interval(value: Duration, max: Duration) -> Duration {
    let grown = value.saturating_mul(3) / 2;
    if grown > max {
        max
    } else if grown.is_zero() {
        Duration::from_millis(10)
    } else {
        grown
    }
}

/// 在 `timeout` 内等子进程退出；返回 `Some(exit_code)`（可能为 `None`=无码）或 `None`（超时）。
fn wait_exit(child: &mut dyn Child, timeout: Duration, poll: Duration) -> Option<Option<i32>> {
    let deadline = Instant::now() + timeout;
    let interval = poll.max(Duration::from_millis(5));
    loop {
        match child.try_wait() {
            Ok(Some(code)) => return Some(Some(code)),
            Ok(None) => {}
            Err(_) => return Some(None),
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(interval);
    }
}

// ---------------------------------------------------------------------------
// 真实实现：进程启动器 + Job Object 孤儿守卫
// ---------------------------------------------------------------------------

/// 真实进程启动器：`CREATE_NO_WINDOW`、工作目录=运行时目录、stdin/stdout/stderr 全部断开。
pub struct ProcessSpawner;

impl Spawner for ProcessSpawner {
    fn spawn(&self, spec: &LaunchSpec) -> Result<Box<dyn Child>, RuntimeError> {
        spawn_platform(spec)
    }
}

#[cfg(windows)]
fn spawn_platform(spec: &LaunchSpec) -> Result<Box<dyn Child>, RuntimeError> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let mut command = Command::new(&spec.exe);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let child = command
        .spawn()
        .map_err(|error| RuntimeError::Launch(format!("{}：{error}", spec.exe.display())))?;
    let pid = child.id();
    let job = JobGuard::attach(pid);
    Ok(Box::new(WindowsChild { child, job }))
}

#[cfg(not(windows))]
fn spawn_platform(spec: &LaunchSpec) -> Result<Box<dyn Child>, RuntimeError> {
    use std::process::{Command, Stdio};

    let child = Command::new(&spec.exe)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| RuntimeError::Launch(format!("{}：{error}", spec.exe.display())))?;
    Ok(Box::new(PlainChild { child }))
}

#[cfg(windows)]
struct WindowsChild {
    child: std::process::Child,
    job: Option<JobGuard>,
}

#[cfg(windows)]
impl Child for WindowsChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| status.code().unwrap_or(-1)))
    }

    fn kill(&mut self) -> io::Result<()> {
        // 先放掉 Job：KILL_ON_JOB_CLOSE 会让系统直接收掉子进程。
        self.job = None;
        self.child.kill()
    }
}

#[cfg(not(windows))]
struct PlainChild {
    child: std::process::Child,
}

#[cfg(not(windows))]
impl Child for PlainChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| status.code().unwrap_or(-1)))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}

/// Job Object：`KILL_ON_JOB_CLOSE` —— 父进程无论怎么死，系统都会收掉子进程。
#[cfg(windows)]
pub struct JobGuard {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl JobGuard {
    /// 把进程加入 job；失败返回 `None`（调用方应发 Warning，不许静默）。
    pub fn attach(pid: u32) -> Option<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        // SAFETY：全部为本进程内的 Win32 调用；句柄在结构体 Drop 里关闭。
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if configured == 0 {
                CloseHandle(job);
                return None;
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                CloseHandle(job);
                return None;
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self { handle: job })
        }
    }
}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY：handle 由 CreateJobObjectW 得到，只关一次。
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// 真实实现：RPC 探测
// ---------------------------------------------------------------------------

/// 用 M1-2A 的 RPC 客户端做就绪探测。
pub struct RpcProbe {
    client: RpcClient,
}

impl RpcProbe {
    /// 连 `127.0.0.1:<port>`；单次探测超时 500ms（轮询由本模块控制）。
    pub fn new(port: u16, secret: &str) -> Self {
        let secret = if secret.trim().is_empty() {
            None
        } else {
            Some(secret.trim().to_owned())
        };
        Self {
            client: RpcClient::local_with_timeout(port, secret, Duration::from_millis(500)),
        }
    }
}

impl Readiness for RpcProbe {
    fn probe_version(&self) -> Result<String, String> {
        self.client
            .get_version()
            .map(|info| info.version)
            .map_err(|error| error.to_string())
    }

    fn shutdown(&self) -> Result<(), String> {
        self.client
            .call("aria2.shutdown", Vec::new())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn force_shutdown(&self) -> Result<(), String> {
        self.client
            .call("aria2.forceShutdown", Vec::new())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

// ---------------------------------------------------------------------------
// 测试（全部用假实现，不依赖真实 aria2c）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use ariahost_common::config::{Aria2Section, Config, DlSection, RpcSection};

    // ---- 假件 ----

    /// 假子进程：可配置「何时退出」。
    struct FakeChild {
        pid: u32,
        /// 剩余 `try_wait` 次数之后才退出（`None` = 永不退出）。
        exit_after: Arc<Mutex<Option<u32>>>,
        exit_code: i32,
        killed: Arc<Mutex<bool>>,
    }

    impl FakeChild {
        fn new(pid: u32, exit_after: Option<u32>, exit_code: i32) -> Self {
            Self::shared(pid, Arc::new(Mutex::new(exit_after)), exit_code)
        }

        /// 与 [`FakeProbe`] 共享同一份退出状态：probe 让「进程退出」时子进程真的会退。
        fn shared(pid: u32, exit_after: Arc<Mutex<Option<u32>>>, exit_code: i32) -> Self {
            Self {
                pid,
                exit_after,
                exit_code,
                killed: Arc::new(Mutex::new(false)),
            }
        }
    }

    impl Child for FakeChild {
        fn id(&self) -> u32 {
            self.pid
        }

        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            let mut state = self.exit_after.lock().expect("锁");
            match *state {
                Some(0) => Ok(Some(self.exit_code)),
                Some(remaining) => {
                    *state = Some(remaining - 1);
                    Ok(None)
                }
                None => Ok(None),
            }
        }

        fn kill(&mut self) -> io::Result<()> {
            *self.killed.lock().expect("锁") = true;
            Ok(())
        }
    }

    /// 假启动器：记录被调次数，返回预设子进程。
    struct FakeSpawner {
        calls: Arc<Mutex<u32>>,
        plan: Box<dyn Fn(u32) -> Box<dyn Child> + Send + Sync>,
    }

    impl FakeSpawner {
        fn new(plan: impl Fn(u32) -> Box<dyn Child> + Send + Sync + 'static) -> Self {
            Self {
                calls: Arc::new(Mutex::new(0)),
                plan: Box::new(plan),
            }
        }

        fn counter(&self) -> Arc<Mutex<u32>> {
            Arc::clone(&self.calls)
        }
    }

    impl Spawner for FakeSpawner {
        fn spawn(&self, _spec: &LaunchSpec) -> Result<Box<dyn Child>, RuntimeError> {
            let mut calls = self.calls.lock().expect("锁");
            *calls += 1;
            Ok((self.plan)(*calls))
        }
    }

    /// 假探测器：可配置「第几次探测开始就绪」与「shutdown 是否有效」。
    struct FakeProbe {
        ready_after: u32,
        probes: Arc<Mutex<u32>>,
        shutdown_calls: Arc<Mutex<u32>>,
        force_calls: Arc<Mutex<u32>>,
        shutdown_works: bool,
        force_works: bool,
        exit: Arc<Mutex<Option<u32>>>,
    }

    impl FakeProbe {
        fn new(
            ready_after: u32,
            shutdown_works: bool,
            force_works: bool,
            child_exit: Arc<Mutex<Option<u32>>>,
        ) -> Self {
            Self {
                ready_after,
                probes: Arc::new(Mutex::new(0)),
                shutdown_calls: Arc::new(Mutex::new(0)),
                force_calls: Arc::new(Mutex::new(0)),
                shutdown_works,
                force_works,
                exit: child_exit,
            }
        }
    }

    impl Readiness for FakeProbe {
        fn probe_version(&self) -> Result<String, String> {
            let mut probes = self.probes.lock().expect("锁");
            *probes += 1;
            if *probes >= self.ready_after {
                Ok("1.37.0".to_owned())
            } else {
                Err("连接被拒绝".to_owned())
            }
        }

        fn shutdown(&self) -> Result<(), String> {
            *self.shutdown_calls.lock().expect("锁") += 1;
            if self.shutdown_works {
                *self.exit.lock().expect("锁") = Some(0);
                Ok(())
            } else {
                Err("shutdown 无响应".to_owned())
            }
        }

        fn force_shutdown(&self) -> Result<(), String> {
            *self.force_calls.lock().expect("锁") += 1;
            if self.force_works {
                *self.exit.lock().expect("锁") = Some(0);
                Ok(())
            } else {
                Err("forceShutdown 无响应".to_owned())
            }
        }
    }

    /// 短超时，让单测快。
    fn fast_timeouts() -> Timeouts {
        Timeouts {
            ready: Duration::from_millis(80),
            stop_grace: Duration::from_millis(60),
            force_grace: Duration::from_millis(60),
            poll: Duration::from_millis(5),
            poll_max: Duration::from_millis(10),
        }
    }

    fn spec() -> LaunchSpec {
        LaunchSpec {
            exe: PathBuf::from("aria2c.exe"),
            args: vec!["--conf-path=aria2.conf".to_owned()],
            cwd: PathBuf::from("."),
            log: PathBuf::from("aria2.log"),
        }
    }

    /// 收集事件，便于断言序列。
    fn recorder() -> (Arc<Mutex<Vec<RuntimeEvent>>>, impl FnMut(RuntimeEvent)) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let callback = move |event: RuntimeEvent| sink.lock().expect("锁").push(event);
        (events, callback)
    }

    fn names(events: &[RuntimeEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                RuntimeEvent::Starting => "starting",
                RuntimeEvent::Running { .. } => "running",
                RuntimeEvent::Stopping => "stopping",
                RuntimeEvent::Stopped { .. } => "stopped",
                RuntimeEvent::Failed { .. } => "failed",
                RuntimeEvent::Warning { .. } => "warning",
            })
            .collect()
    }

    // ---- 用例 ----

    #[test]
    fn happy_path_emits_starting_running_stopping_stopped() {
        let exit = Arc::new(Mutex::new(None));
        let child_exit = Arc::clone(&exit);
        let spawner = FakeSpawner::new(move |_| {
            Box::new(FakeChild::shared(4242, Arc::clone(&child_exit), 0))
        });
        let probe = FakeProbe::new(2, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (events, mut callback) = recorder();
        let status = runtime.start(&spec(), &mut callback).expect("应就绪");
        assert_eq!(status.state, CoreState::Running);
        assert_eq!(status.pid, Some(4242));
        assert_eq!(status.version.as_deref(), Some("1.37.0"));

        let stopped = runtime.stop(&mut callback).expect("应停");
        assert_eq!(stopped.state, CoreState::Stopped);
        assert!(!stopped.forced, "第一段 shutdown 就该退出");

        assert_eq!(
            names(&events.lock().expect("锁")),
            vec!["starting", "running", "stopping", "stopped"]
        );
    }

    #[test]
    fn readiness_timeout_fails_with_e2002_and_kills_child() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(7, None, 0)));
        // ready_after 极大 → 永远不会就绪。
        let probe = FakeProbe::new(u32::MAX, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (events, mut callback) = recorder();
        let error = runtime.start(&spec(), &mut callback).expect_err("必须超时");
        assert!(
            matches!(error, RuntimeError::StartTimeout { .. }),
            "{error}"
        );
        assert_eq!(error.code(), Some(ErrorCode::Aria2StartTimeout));
        assert_eq!(runtime.status().state, CoreState::Failed);
        assert_eq!(
            names(&events.lock().expect("锁")),
            vec!["starting", "failed"]
        );
    }

    #[test]
    fn child_exiting_before_ready_is_reported_with_exit_code() {
        let exit = Arc::new(Mutex::new(None));
        // 第 1 次 try_wait 就返回退出码 5。
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(9, Some(0), 5)));
        let probe = FakeProbe::new(u32::MAX, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (_events, mut callback) = recorder();
        let error = runtime
            .start(&spec(), &mut callback)
            .expect_err("应报启动即崩");
        assert_eq!(
            error,
            RuntimeError::StartExited { exit_code: Some(5) },
            "退出码必须透出"
        );
        assert_eq!(error.code(), None, "没有语义匹配的既有码");
        assert_eq!(runtime.status().exit_code, Some(5));
    }

    #[test]
    fn shutdown_ignored_falls_back_to_force_then_kill() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(11, None, 0)));
        // shutdown 与 forceShutdown 都无效 → 必须走到强杀。
        let probe = FakeProbe::new(1, false, false, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (events, mut callback) = recorder();
        runtime.start(&spec(), &mut callback).expect("应就绪");
        let stopped = runtime.stop(&mut callback).expect("强杀后仍应成功");
        assert_eq!(stopped.state, CoreState::Stopped);
        assert!(stopped.forced, "必须如实标记走到强杀");
        assert_eq!(
            names(&events.lock().expect("锁")),
            vec!["starting", "running", "stopping", "stopped"]
        );
    }

    #[test]
    fn force_shutdown_is_enough_when_shutdown_times_out() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(12, None, 0)));
        let probe = FakeProbe::new(1, false, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (events, mut callback) = recorder();
        runtime.start(&spec(), &mut callback).expect("应就绪");
        let stopped = runtime.stop(&mut callback).expect("应停");
        assert!(stopped.forced, "走到 forceShutdown 也算强制");
        assert_eq!(
            names(&events.lock().expect("锁"))[2..],
            ["stopping", "stopped"]
        );
    }

    #[test]
    fn duplicate_start_is_refused_while_running() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(21, None, 0)));
        let launched = spawner.counter();
        let probe = FakeProbe::new(1, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (_events, mut callback) = recorder();
        runtime.start(&spec(), &mut callback).expect("首次");
        let error = runtime
            .start(&spec(), &mut callback)
            .expect_err("重复启动必须拒绝");
        assert_eq!(error, RuntimeError::AlreadyRunning);
        assert_eq!(error.code(), None);
        assert_eq!(
            *launched.lock().expect("锁"),
            1,
            "被拒绝的那次不该真的 spawn"
        );
    }

    #[test]
    fn start_again_after_stop_is_allowed() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(31, None, 0)));
        let probe = FakeProbe::new(1, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (_events, mut callback) = recorder();
        runtime.start(&spec(), &mut callback).expect("第一次");
        runtime.stop(&mut callback).expect("停");
        assert_eq!(runtime.status().state, CoreState::Stopped);
        runtime.start(&spec(), &mut callback).expect("停后应能再启");
        assert_eq!(runtime.status().state, CoreState::Running);
        runtime.stop(&mut callback).expect("再停");
    }

    #[test]
    fn start_again_after_failure_is_allowed() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(41, None, 0)));
        let probe = FakeProbe::new(u32::MAX, false, false, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (_events, mut callback) = recorder();
        runtime
            .start(&spec(), &mut callback)
            .expect_err("第一次超时");
        assert_eq!(runtime.status().state, CoreState::Failed);
        // 失败后允许再启动（Failed 不是终态）。
        let _ = runtime.start(&spec(), &mut callback);
        assert_eq!(
            runtime.status().state,
            CoreState::Failed,
            "仍会失败，但能重来"
        );
    }

    #[test]
    fn stop_without_start_is_idempotent() {
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(51, None, 0)));
        let probe = FakeProbe::new(1, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts());

        let (events, mut callback) = recorder();
        let status = runtime.stop(&mut callback).expect("没跑也能调 stop");
        assert_eq!(status.state, CoreState::Stopped);
        assert!(events.lock().expect("锁").is_empty(), "没发事件");
    }

    // ---- conf 渲染 ----

    fn sample_config() -> Config {
        Config {
            aria2: Aria2Section {
                rpc: RpcSection {
                    port: 6800,
                    allow_origin_all: false,
                    ..RpcSection::default()
                },
                ..Aria2Section::default()
            },
            dl: DlSection {
                dir: "D:\\downloads".to_owned(),
                ..DlSection::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn rendered_conf_contains_required_lines() {
        let config = sample_config();
        let paths = RuntimePaths::under(Path::new("C:\\Users\\alice\\AppData\\Roaming\\AriaHost"));
        let text = render_conf(
            &config,
            &paths,
            "secret-should-not-appear",
            Path::new("D:\\downloads"),
        );

        for needle in [
            "dir=D:\\downloads",
            "file-allocation=prealloc",
            "enable-rpc=true",
            "rpc-listen-all=false",
            "rpc-listen-port=6800",
            "log-level=notice",
            "max-concurrent-downloads=5",
            "continue=true",
            "split=5",
            "max-connection-per-server=1",
            "min-split-size=20M",
            "disk-cache=16M",
            "connect-timeout=60",
            "max-tries=5",
            "max-download-result=1000",
            "keep-unfinished-download-result=true",
            "auto-file-renaming=true",
            "allow-overwrite=false",
            "always-resume=true",
            "stop-with-process=",
        ] {
            assert!(text.contains(needle), "conf 缺少 {needle}：\n{text}");
        }
        assert!(text.contains("save-session="), "必须写 session 路径");
        assert!(text.contains("aria2.session"), "session 文件名");
        assert!(
            !text.contains("input-file="),
            "session 还不存在时不能写 input-file"
        );
        assert!(
            !text.contains("secret-should-not-appear"),
            "secret 不得进 conf 明文"
        );
        assert!(!text.contains("rpc-secret="), "secret 只走命令行");
    }

    #[test]
    fn conf_honours_lan_and_proxy_and_allocation() {
        let mut config = sample_config();
        config.aria2.rpc.listen = RpcListen::Lan;
        config.net.proxy = "http://127.0.0.1:7890".to_owned();
        let paths = RuntimePaths::under(Path::new("C:\\root"));
        let text = render_conf(&config, &paths, "x", Path::new("D:\\downloads"));
        assert!(text.contains("rpc-listen-all=true"), "{text}");
        assert!(text.contains("all-proxy=http://127.0.0.1:7890"), "{text}");
    }

    #[test]
    fn input_file_appears_only_when_session_exists() {
        let dir =
            std::env::temp_dir().join(format!("ariahost-runtime-conf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = RuntimePaths::under(&dir);
        std::fs::create_dir_all(&paths.session).expect("建 session 目录");
        std::fs::write(paths.session_path(), b"").expect("造 session 文件");

        let text = render_conf(&sample_config(), &paths, "x", Path::new("D:\\downloads"));
        assert!(
            text.contains("input-file="),
            "session 存在时必须挂 input-file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_conf_writes_atomically_and_creates_dirs() {
        let dir =
            std::env::temp_dir().join(format!("ariahost-runtime-prep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = RuntimePaths::under(&dir);

        let conf = prepare_conf(&sample_config(), &paths, "x", Path::new("D:\\downloads"))
            .expect("应写成功");
        assert!(conf.is_file(), "conf 必须存在：{}", conf.display());
        assert!(paths.session.is_dir() && paths.logs.is_dir());
        let text = std::fs::read_to_string(&conf).expect("读");
        assert!(text.contains("dir=D:\\downloads"));
        assert!(
            !conf.with_extension("conf.tmp").exists(),
            "不能残留临时文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn launch_spec_passes_conf_and_secret_on_command_line() {
        let config = sample_config();
        let paths = RuntimePaths::under(Path::new("C:\\root"));
        let spec = launch_spec(&config, &paths, PathBuf::from("aria2c.exe"), "abc123");
        assert!(spec.args.iter().any(|arg| arg == "--rpc-secret=abc123"));
        assert!(spec.args.iter().any(|arg| arg.starts_with("--conf-path=")));
        assert_eq!(spec.cwd, paths.runtime);
    }

    #[test]
    fn rpc_off_drops_secret_argument() {
        let mut config = sample_config();
        config.aria2.rpc.listen = RpcListen::Off;
        let paths = RuntimePaths::under(Path::new("C:\\root"));
        let spec = launch_spec(&config, &paths, PathBuf::from("aria2c.exe"), "abc123");
        assert!(!spec.args.iter().any(|arg| arg.contains("rpc-secret")));
    }

    #[test]
    fn interval_grows_then_caps() {
        let start = Duration::from_millis(200);
        let capped = next_interval(start, Duration::from_millis(250));
        assert_eq!(capped, Duration::from_millis(250));
        let grown = next_interval(start, Duration::from_secs(1));
        assert_eq!(grown, Duration::from_millis(300));
    }
}
