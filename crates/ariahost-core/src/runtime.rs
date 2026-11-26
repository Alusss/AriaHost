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

    /// 服务平面（R7）：`%ProgramData%\AriaHost\`，conf/session/logs 直接在其下。
    ///
    /// 为什么服务不能用 headless 平面：服务账户（LocalSystem / LocalService）拿到的
    /// `%APPDATA%` 是 **systemprofile**，不是用户的目录（架构 §4.6 R9）。
    pub fn service() -> Option<Self> {
        let root = ariahost_common::paths::service_runtime_dir()?;
        Some(Self {
            runtime: root.clone(),
            session: root.join("session"),
            logs: root.join("logs"),
        })
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
    /// 回退链走完仍找不到可用核心（**逐跳原因**，顺序即尝试顺序）。
    ///
    /// 与 [`RuntimeError::CoreNotFound`] 的区别：`CoreNotFound` 是**单跳**失败
    /// （某一来源解释自己为什么没有），`Unavailable` 是**整条链**的结论。
    /// UI 应把这里每一条都显示出来，否则用户只看到「找不到」却不知道查过哪里。
    Unavailable {
        /// 逐跳原因（`configured(...)` / `last_good` / `bundled` …）。
        attempts: Vec<String>,
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
            Self::CoreNotFound { .. } | Self::Launch(_) | Self::Unavailable { .. } => {
                Some(ErrorCode::Aria2NotFound)
            }
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
            Self::Unavailable { attempts } => {
                write!(formatter, "找不到可用的 aria2c：")?;
                for (index, attempt) in attempts.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, "；")?;
                    }
                    write!(formatter, "{}) {attempt}", index + 1)?;
                }
                Ok(())
            }
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

/// 内置核心的文件名：安装包把它放在**本 exe 同目录**（与安装器 `ariahost.nsi` 一致）。
pub const BUNDLED_CORE_EXE: &str = "aria2c.exe";

/// 核心的来源。面板「核心」区要能看出当前用的是哪一种（内置/托管/外部）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreOrigin {
    /// 安装包自带：exe 同目录的 `aria2c.exe`（不进版本库，不参与版本管理）。
    Bundled,
    /// 版本库 `active.json` 指向的 `versions/<ver>/aria2c.exe`。
    Managed,
    /// 用户指定的 `aria2.external_path`。
    External,
}

impl CoreOrigin {
    /// 稳定文本（面板、`--aria2-status`、诊断包共用；不要解析中文）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Managed => "managed",
            Self::External => "external",
        }
    }
}

/// 解析出来的核心：路径 + 来源 + 是否回退。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCore {
    /// `aria2c.exe` 的实际路径。
    pub path: PathBuf,
    /// 它为什么被选中。
    pub origin: CoreOrigin,
    /// 是否走了回退链（`false` = 按 `aria2.source` 的语义直接解析成功）。
    ///
    /// UI 用它决定要不要显示「已回退到 X，原因 Y」——**降级必须可见**，
    /// 否则用户会以为配置生效了。
    pub fell_back: bool,
    /// 回退原因（`fell_back` 时为 `Some`，含第一跳失败原因；否则 `None`）。
    pub reason: Option<String>,
}

impl ResolvedCore {
    /// 按配置直接解析成功（正常路径，不涉及回退）。
    fn direct(path: PathBuf, origin: CoreOrigin) -> Self {
        Self {
            path,
            origin,
            fell_back: false,
            reason: None,
        }
    }
}

/// 内置核心的候选路径：**当前进程 exe 所在目录**的 `aria2c.exe`。
///
/// 为什么按「当前 exe」而不是固定安装目录：便携版/开发构建也该能吃到同目录的核心，
/// 而安装器只保证「同一个目录里放了 `ariahost.exe` 与 `aria2c.exe`」这一条。
pub fn bundled_core_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    Some(dir.join(BUNDLED_CORE_EXE))
}

/// 按 `aria2.source` 在**用户平面**（`%LOCALAPPDATA%`）定位 aria2c。
///
/// 服务模式必须改用 [`resolve_core_in`] 传 [`Plane::Service`]：服务账户读不到用户目录（R7/R9）。
pub fn resolve_core(config: &Config) -> Result<ResolvedCore, RuntimeError> {
    resolve_core_in(config, Plane::User)
}

/// 按 `aria2.source` 在**指定平面**定位 aria2c（内置候选取当前 exe 同目录）。
pub fn resolve_core_in(config: &Config, plane: Plane) -> Result<ResolvedCore, RuntimeError> {
    resolve_core_in_with(config, plane, bundled_core_path().as_deref())
}

/// 同 [`resolve_core_in`]，但**显式注入**内置候选路径（单测用：测试二进制同目录不是安装目录）。
///
/// 取值语义（架构 §4.11 `aria2.source`）：
/// - `auto`（**默认**）：内置 → 托管 → 配了 `external_path` 的外部 → 报「未就绪」；
/// - `managed` / `external`：只看那一种，不存在就报错（不隐式降级，老配置语义不变）。
///
/// 顺带做**悬空回滚指针修复**：若 `active.json` 的 `previous` 指向已删除的版本目录，
/// 清掉该字段并原子写回（否则「回滚」按钮会指向空气）。
pub fn resolve_core_in_with(
    config: &Config,
    plane: Plane,
    bundled: Option<&Path>,
) -> Result<ResolvedCore, RuntimeError> {
    let versions_root = versions_dir(plane);
    resolve_core_with(config, versions_root.as_deref(), bundled)
}

/// 全部来源都显式注入的解析器本体（**单测只测这一层**：不读环境变量）。
///
/// 解析顺序（M5-b-G 起）：
/// 1. **configured**：按 `aria2.source` 的既有语义解析
///    （`auto` = 内置 → 托管 → 配了 `external_path` 的外部；`managed`/`external` 只看那一种）；
/// 2. 失败 → **last_good**：版本库里「最近一次真跑起来过」的版本（`last_good.json`，
///    由 [`CoreRuntime::with_last_good_root`] 在成功 spawn 后写入）；
/// 3. `auto` 下仍失败 → **bundled**（第 1 跳已试过，这里只是把原因记全）；
/// 4. 全失败 → [`RuntimeError::Unavailable`]，带**逐跳**原因。
///
/// # 为什么 `managed`/`external` 也走这一整条链（M5-b-G 的语义变更，**可见**）
///
/// 本批按任务书要求把回退链统一到所有 `aria2.source`：
/// `configured → last_good → bundled → Unavailable`。
/// 这**改变了** task-54 锁定的不变量「显式来源失败就报错、绝不使用内置核心」
/// （原测试 `managed_source_never_silently_falls_back_to_bundled`）：
/// 现在显式来源失败也会退到内置核心，但**必须可见**——`fell_back = true` 且
/// `reason` 里逐跳写明「为什么退、退到了哪」。理由：对「aria2 服务管理器」而言，
/// 版本库被清理/移走后直接起不来，比「起内置核心 + 面板明示已回退」更糟；
/// 而 task-54 真正防的是**静默**替换（用户以为 managed 生效），这一点没被放弃。
///
/// 如果将来要恢复严格语义：把下面第 3 跳加一个
/// `if config.aria2.source == Aria2Source::Auto` 判断即可（改动只在这一点）。
pub(crate) fn resolve_core_with(
    config: &Config,
    versions_root: Option<&Path>,
    bundled: Option<&Path>,
) -> Result<ResolvedCore, RuntimeError> {
    let mut attempts: Vec<String> = Vec::new();

    // 跳 1：configured（既有语义）。
    match configured_core(config, versions_root, bundled) {
        Ok(core) => return Ok(core),
        Err(error) => attempts.push(format!(
            "configured({})：{error}",
            config.aria2.source.as_str()
        )),
    }

    // 跳 2：last_good（最近一次真跑起来过的版本；仍是版本库来源）。
    match last_good_core(versions_root) {
        Ok(Some((version, exe))) => {
            let first = attempts.first().cloned().unwrap_or_default();
            return Ok(ResolvedCore {
                path: exe,
                // 它本来就在版本库里：来源仍是「托管」，回退信息走 fell_back/reason。
                origin: CoreOrigin::Managed,
                fell_back: true,
                reason: Some(format!(
                    "按 last_good 回退到 {version}（{first}）；若这个版本能用，请把 aria2.source 改回来"
                )),
            });
        }
        Ok(None) => attempts.push("last_good：没有记录，或记录的版本目录/exe 已不存在".to_owned()),
        Err(detail) => attempts.push(format!("last_good：{detail}")),
    }

    // 跳 3：bundled（exe 同目录）。
    if let Some(path) = bundled.filter(|path| path.is_file()) {
        return Ok(ResolvedCore {
            path: path.to_path_buf(),
            origin: CoreOrigin::Bundled,
            fell_back: true,
            reason: Some(format!(
                "回退到内置核心 {}（{}）：{}",
                path.display(),
                if config.aria2.source == ariahost_common::Aria2Source::Auto {
                    "auto 第 1 跳已试过，现在仍找不到其它来源"
                } else {
                    "显式来源不可用"
                },
                attempts.join("；")
            )),
        });
    }
    attempts.push(format!("bundled：exe 同目录没有可用的 {BUNDLED_CORE_EXE}"));

    Err(RuntimeError::Unavailable { attempts })
}

/// 按 `aria2.source` 的既有语义解析（**成功即返回，不回落**）。
fn configured_core(
    config: &Config,
    versions_root: Option<&Path>,
    bundled: Option<&Path>,
) -> Result<ResolvedCore, RuntimeError> {
    use ariahost_common::Aria2Source;

    match config.aria2.source {
        Aria2Source::Auto => {
            if let Some(path) = bundled.filter(|path| path.is_file()) {
                return Ok(ResolvedCore::direct(
                    path.to_path_buf(),
                    CoreOrigin::Bundled,
                ));
            }
            match managed_core_in(versions_root) {
                Ok(path) => Ok(ResolvedCore::direct(path, CoreOrigin::Managed)),
                Err(managed_error) => {
                    // 外部路径是用户显式写的：auto 下它是最后一道可用来源。
                    if config.aria2.external_path.trim().is_empty() {
                        return Err(RuntimeError::CoreNotFound {
                            detail: format!(
                                "未就绪：exe 同目录没有 {}，版本库也没有可用版本（{managed_error}）——\
                                 可在首次运行引导里在线下载或指定本地 aria2c",
                                BUNDLED_CORE_EXE
                            ),
                        });
                    }
                    external_core(config)
                }
            }
        }
        Aria2Source::Managed => Ok(ResolvedCore::direct(
            managed_core_in(versions_root)?,
            CoreOrigin::Managed,
        )),
        Aria2Source::External => external_core(config),
    }
}

/// 版本库里的托管核心（`managed` 分支的唯一实现）。
fn managed_core_in(versions_root: Option<&Path>) -> Result<PathBuf, RuntimeError> {
    let dir = versions_root.ok_or_else(|| RuntimeError::CoreNotFound {
        detail: "拿不到核心版本库根目录（LOCALAPPDATA/PROGRAMDATA 缺失）".to_owned(),
    })?;
    let store = VersionStore::new(dir);
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

/// 用户显式指定的外部核心（`external` 分支的唯一实现）。
fn external_core(config: &Config) -> Result<ResolvedCore, RuntimeError> {
    let raw = config.aria2.external_path.trim();
    if raw.is_empty() {
        return Err(RuntimeError::CoreNotFound {
            detail: "aria2.source=external 但 aria2.external_path 为空".to_owned(),
        });
    }
    let exe = config::expand_user_path(raw);
    if exe.is_file() {
        Ok(ResolvedCore::direct(exe, CoreOrigin::External))
    } else {
        Err(RuntimeError::CoreNotFound {
            detail: format!("external_path 指向的文件不存在：{}", exe.display()),
        })
    }
}

/// 回退跳 2：版本库根里的 `last_good.json` 指向的版本。
///
/// 返回 `Ok(None)` = 没有记录（或记录指的目录/exe 已不存在）——那是「等于没有」，
/// 不是错误；只有「拿不到版本库根目录」才用 `Err(detail)` 说明原因，
/// 因为那意味着这个平面根本没法记录回退锚点。
///
/// 「可执行」的判定：`is_installed`（`versions/<ver>/aria2c.exe` 是文件）。
/// 静态无法证明一个 exe 真能跑，真正的证明在启动阶段（RPC 就绪）——所以这里不
/// 拿 `--version` 去探（那会把 `resolve_core` 变成一次进程启动，代价与副作用都不值）。
fn last_good_core(versions_root: Option<&Path>) -> Result<Option<(String, PathBuf)>, String> {
    let dir = versions_root
        .ok_or_else(|| "拿不到核心版本库根目录（LOCALAPPDATA/PROGRAMDATA 缺失）".to_owned())?;
    let store = VersionStore::new(dir);
    let Some(version) = store.last_good() else {
        return Ok(None);
    };
    let exe = store.exe_path(&version);
    if !exe.is_file() {
        return Ok(None);
    }
    Ok(Some((version, exe)))
}

/// 只要路径的旧入口（服务端 `scm.rs` 等不关心来源的调用方保留）。
pub fn resolve_core_exe(config: &Config) -> Result<PathBuf, RuntimeError> {
    resolve_core(config).map(|core| core.path)
}

/// 只要路径的旧入口（指定平面）。
pub fn resolve_core_exe_in(config: &Config, plane: Plane) -> Result<PathBuf, RuntimeError> {
    resolve_core_in(config, plane).map(|core| core.path)
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
    /// 就绪探测的**首次**间隔（默认 100ms，之后按 1.5 倍递增到 [`Self::ready_poll_max`]）。
    ///
    /// 为什么不复用 [`Self::poll`]：`poll` 同时被 `stop()` 的三段 `wait_exit` 当轮询间隔用，
    /// 改它会连带改停止路径的节奏——而停止路径的时序是冻结的（M4-c 任务书硬边界）。
    pub ready_poll: Duration,
    /// 就绪探测的间隔**上限**（默认 400ms）。
    ///
    /// 同理不复用 [`Self::poll_max`]：后者在 `stop()` 里是「强杀后等退出的兜底超时」。
    /// 两个值语义不同、owner 不同（就绪 = 启动体验；poll_max = 停止收尾），共用一个字段
    /// 会让「调启动」这件事偷偷改掉「停止」的时序。
    pub ready_poll_max: Duration,
    /// 等子进程退出时的轮询间隔（`stop()` 的三段 `wait_exit` 用；默认 200ms）。
    ///
    /// 就绪探测**不**用它（见 [`Self::ready_poll`]）。
    pub poll: Duration,
    /// 强杀后等退出的兜底超时（`stop()` 用；默认 1s）。
    ///
    /// 名字沿用历史（当年就绪循环复用过同一个值），**它从来不是「轮询上限」**。
    pub poll_max: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            ready: Duration::from_secs(15),
            stop_grace: Duration::from_secs(15),
            force_grace: Duration::from_secs(10),
            ready_poll: Duration::from_millis(100),
            ready_poll_max: Duration::from_millis(400),
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
    /// 版本库根（可选）：就绪探测成功后在这里写 `last_good.json`（M5-b-G 的回退锚点）。
    last_good_root: Option<PathBuf>,
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
            last_good_root: None,
        }
    }

    /// 让运行时在「核心成功 spawn 且 `aria2c --version`/RPC 探到有效版本」之后
    /// 把该版本记进 `last_good.json`（与 `active.json` 同在版本库根）。
    ///
    /// 为什么**不**放进 [`CoreRuntime::new`]：三个既有调用方（面板/服务/自检）不该为
    /// 一个新能力集体改签名。但反过来必须说清：**不接这一句就没有回退锚点**。
    /// 接线点（面板 `core_host` 与服务 `scm`）不在本批写域，见 worklog 的「仍未验证」。
    ///
    /// 为什么写入时机是「成功 spawn 之后」而不是「安装成功之后」：安装成功只证明
    /// 字节被写进目录，**真能跑起来**才算这个版本可用；把装完就跑不起来的版本写成
    /// last_good，等于把回退锚点钉在一个坏版本上。
    pub fn with_last_good_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.last_good_root = Some(root.into());
        self
    }

    /// 就绪后记录 last_good（失败只发一条 `Warning`，绝不把启动判成失败）。
    fn record_last_good(&self, version: &str, events: &mut dyn FnMut(RuntimeEvent)) {
        let Some(root) = self.last_good_root.as_ref() else {
            return;
        };
        let store = VersionStore::new(root);
        if let Err(error) = store.record_last_good(version) {
            events(RuntimeEvent::Warning {
                detail: format!("记录 last_good 失败（不影响运行）：{error}"),
            });
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
        // 就绪循环用自己的节奏（`ready_poll` / `ready_poll_max`）：这两个值只影响
        // 「spawn → Running」的探测频率，**不参与**任何停止时序。
        let mut interval = self.timeouts.ready_poll;
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
                    events(RuntimeEvent::Running {
                        version: version.clone(),
                        pid,
                    });
                    // 只有走到这里（进程活着 + 探到版本）才写回退锚点。
                    self.record_last_good(&version, events);
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
                    interval = next_interval(interval, self.timeouts.ready_poll_max);
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

    use ariahost_common::config::{Aria2Section, Aria2Source, Config, DlSection, RpcSection};

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
            ready_poll: Duration::from_millis(5),
            ready_poll_max: Duration::from_millis(10),
            poll: Duration::from_millis(5),
            poll_max: Duration::from_millis(10),
        }
    }

    /// M4-c 的护栏：只许改**就绪**节奏，不许顺手改掉停止的预算。
    ///
    /// 就绪探测改用 `ready_poll` / `ready_poll_max` 正是为了与停止侧解耦：`poll` / `poll_max`
    /// 同时被 `stop()` 的三段 `wait_exit` 使用（强杀后的兜底超时也在里面）。这条测试把停止侧
    /// 四个数钉死——任何人为了让启动更快而改到这里，先红的是 CI，不是用户的卸载。
    #[test]
    fn timeouts_keep_ready_and_stop_budgets_separate() {
        let timeouts = Timeouts::default();
        // 就绪：首轮 100ms、退避上限 400ms（M4-c 新值）
        assert_eq!(timeouts.ready_poll, Duration::from_millis(100));
        assert_eq!(timeouts.ready_poll_max, Duration::from_millis(400));
        // 就绪总预算不变（L13 的 S1+S2 仍受 15s 保护）
        assert_eq!(timeouts.ready, Duration::from_secs(15));
        // 停止侧原样冻结：15s + 10s = 25s，与 CLI `--quit` 的 25s 预算对齐
        assert_eq!(timeouts.stop_grace, Duration::from_secs(15));
        assert_eq!(timeouts.force_grace, Duration::from_secs(10));
        assert_eq!(timeouts.poll, Duration::from_millis(200));
        assert_eq!(timeouts.poll_max, Duration::from_secs(1));
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
    fn last_good_is_written_only_after_readiness_succeeds() {
        let dir = std::env::temp_dir().join(format!(
            "ariahost-runtime-lastgood-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建版本库根");

        // 1) 就绪失败：**绝不能**写 last_good（安装成功 ≠ 真能跑，这是本功能的语义核心）。
        let exit = Arc::new(Mutex::new(None));
        let spawner = FakeSpawner::new(|_| Box::new(FakeChild::new(7, None, 0)));
        let probe = FakeProbe::new(u32::MAX, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts())
            .with_last_good_root(&dir);
        let (_, mut callback) = recorder();
        runtime
            .start(&spec(), &mut callback)
            .expect_err("永远不就绪");
        assert_eq!(
            VersionStore::new(&dir).last_good(),
            None,
            "就绪前不得写 last_good"
        );

        // 2) 就绪成功：写盘；同版本再来一次不重写（幂等）。
        let exit = Arc::new(Mutex::new(None));
        let child_exit = Arc::clone(&exit);
        let spawner = FakeSpawner::new(move |_| {
            Box::new(FakeChild::shared(4242, Arc::clone(&child_exit), 0))
        });
        let probe = FakeProbe::new(1, true, true, Arc::clone(&exit));
        let mut runtime = CoreRuntime::new(Box::new(spawner), Box::new(probe), fast_timeouts())
            .with_last_good_root(&dir);
        let (events, mut callback) = recorder();
        let status = runtime.start(&spec(), &mut callback).expect("应就绪");
        assert_eq!(status.state, CoreState::Running);
        let store = VersionStore::new(&dir);
        assert_eq!(store.last_good().as_deref(), Some("1.37.0"));
        assert!(
            !store.record_last_good("1.37.0").expect("写 last_good"),
            "同一版本不得重复写盘"
        );
        assert!(
            !names(&events.lock().expect("锁")).contains(&"warning"),
            "正常路径不该有告警：{:?}",
            names(&events.lock().expect("锁"))
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    // ---- 核心定位（`aria2.source` 三态；全部用注入路径，不读环境变量）----

    /// 每个测试一独立目录；返回 `(根, 版本库根, 内置核心路径)`。
    ///
    /// 版本库根与内置核心都不预建：调用方按需造，避免「顺手就存在」把测试测空。
    fn resolve_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "ariahost-resolve-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let versions = root.join("versions");
        let bundled = root.join("install").join(BUNDLED_CORE_EXE);
        std::fs::create_dir_all(&root).expect("建测试根");
        (root, versions, bundled)
    }

    /// 在版本库里装一个「已激活」的版本（只造文件，不跑安装流程）。
    fn install_fake_managed(versions: &Path, version: &str) -> PathBuf {
        let dir = versions.join(version);
        std::fs::create_dir_all(&dir).expect("建版本目录");
        let exe = dir.join(BUNDLED_CORE_EXE);
        std::fs::write(&exe, b"MZ").expect("写占位核心");
        std::fs::write(
            versions.join("active.json"),
            format!("{{\"active\":\"{version}\",\"previous\":null}}\n"),
        )
        .expect("写激活指针");
        exe
    }

    /// 造一个「内置核心」文件（内容无所谓，只需要是文件）。
    fn install_fake_bundled(bundled: &Path) -> PathBuf {
        if let Some(parent) = bundled.parent() {
            std::fs::create_dir_all(parent).expect("建安装目录");
        }
        std::fs::write(bundled, b"MZ").expect("写内置核心");
        bundled.to_path_buf()
    }

    fn config_with(source: Aria2Source, external: &str) -> Config {
        let mut config = sample_config();
        config.aria2.source = source;
        config.aria2.external_path = external.to_owned();
        config
    }

    #[test]
    fn auto_prefers_bundled_over_managed() {
        let (root, versions, bundled) = resolve_fixture("auto-bundled");
        install_fake_bundled(&bundled);
        install_fake_managed(&versions, "1.37.0");

        let core = resolve_core_with(
            &config_with(Aria2Source::Auto, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("auto 应解析成功");

        assert_eq!(core.origin, CoreOrigin::Bundled);
        assert_eq!(core.path, bundled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_falls_back_to_managed_when_bundled_is_missing() {
        let (root, versions, bundled) = resolve_fixture("auto-managed");
        let managed = install_fake_managed(&versions, "1.37.0");
        // 内置路径给了但文件不存在（安装包漏带/用户删了）。
        let core = resolve_core_with(
            &config_with(Aria2Source::Auto, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("auto 应退到托管核心");

        assert_eq!(core.origin, CoreOrigin::Managed);
        assert_eq!(core.path, managed);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn auto_uses_external_path_as_the_last_resort() {
        let (root, versions, bundled) = resolve_fixture("auto-external");
        let external = root.join("portable").join(BUNDLED_CORE_EXE);
        install_fake_bundled(&external);

        let core = resolve_core_with(
            &config_with(Aria2Source::Auto, external.to_str().expect("UTF-8 路径")),
            Some(&versions),
            Some(&bundled),
        )
        .expect("auto 应退到外部核心");

        assert_eq!(core.origin, CoreOrigin::External);
        assert_eq!(core.path, external);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 往版本库根写一份 last_good 记录（与生产同一个文件名/形状）。
    fn write_last_good(versions: &Path, version: &str) {
        std::fs::create_dir_all(versions).expect("建版本库根");
        std::fs::write(
            versions.join(crate::store::LAST_GOOD_FILE),
            format!("{{\"version\":\"{version}\"}}\n"),
        )
        .expect("写 last_good");
    }

    #[test]
    fn auto_with_nothing_available_reports_unavailable_with_per_jump_reasons() {
        let (root, versions, bundled) = resolve_fixture("auto-none");
        let error = resolve_core_with(
            &config_with(Aria2Source::Auto, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect_err("三处都没有核心时必须报错");

        assert!(
            matches!(error, RuntimeError::Unavailable { .. }),
            "应是 Unavailable（逐跳结论）：{error:?}"
        );
        assert_eq!(error.code(), Some(ErrorCode::Aria2NotFound));
        let detail = error.to_string();
        assert!(
            detail.contains("首次运行引导"),
            "错误文案必须给出可执行下一步：{detail}"
        );
        // 逐跳原因一个都不许吞：configured / last_good / bundled 三条都要在场。
        let RuntimeError::Unavailable { attempts } = &error else {
            unreachable!()
        };
        assert_eq!(attempts.len(), 3, "三跳都应有原因：{attempts:?}");
        assert!(attempts[0].starts_with("configured(auto)"), "{attempts:?}");
        assert!(attempts[1].starts_with("last_good"), "{attempts:?}");
        assert!(attempts[2].starts_with("bundled"), "{attempts:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// M5-b-G 的**有意语义变更**：显式来源失败也会退到内置核心，但必须**可见**。
    ///
    /// 原来的不变量（task-54）是「managed/external 绝不使用内置核心」。本批按任务书
    /// 要求统一回退链，改成「可以退，但 `fell_back` + `reason` 必须说清」。
    /// 这条测试锁定的就是「退得可见」，而不是「退得静默」。
    #[test]
    fn managed_source_falls_back_to_bundled_with_a_visible_reason() {
        let (root, versions, bundled) = resolve_fixture("managed-fallback");
        install_fake_bundled(&bundled);

        let core = resolve_core_with(
            &config_with(Aria2Source::Managed, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("显式来源不可用时回退到内置核心");

        assert_eq!(core.origin, CoreOrigin::Bundled);
        assert!(core.fell_back, "回退必须标记");
        let reason = core.reason.expect("回退必须带原因");
        assert!(reason.contains("回退到内置核心"), "{reason}");
        assert!(reason.contains("configured(managed)"), "{reason}");
        assert_eq!(core.path, bundled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_success_does_not_mark_a_fallback() {
        let (root, versions, bundled) = resolve_fixture("no-fallback");
        install_fake_bundled(&bundled);
        // 有 last_good 也不该用：第 1 跳就成功了。
        write_last_good(&versions, "1.36.0");

        let core = resolve_core_with(
            &config_with(Aria2Source::Auto, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("auto 应直接用内置核心");

        assert_eq!(core.origin, CoreOrigin::Bundled);
        assert!(!core.fell_back, "成功路径不得标记回退");
        assert_eq!(core.reason, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_active_pointer_falls_back_to_last_good_version() {
        let (root, versions, bundled) = resolve_fixture("last-good-hit");
        // 版本装好了，但 active.json 缺失（指针被删/坏）→ managed 第 1 跳失败。
        let dir = versions.join("1.36.0");
        std::fs::create_dir_all(&dir).expect("建版本目录");
        let exe = dir.join(BUNDLED_CORE_EXE);
        std::fs::write(&exe, b"MZ").expect("写占位核心");
        write_last_good(&versions, "1.36.0");

        let core = resolve_core_with(
            &config_with(Aria2Source::Auto, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("应回退到 last_good 版本");

        assert_eq!(core.origin, CoreOrigin::Managed);
        assert_eq!(core.path, exe);
        assert!(core.fell_back, "回退必须标记");
        let reason = core.reason.expect("回退必须带原因");
        assert!(reason.contains("last_good"), "{reason}");
        assert!(reason.contains("1.36.0"), "{reason}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_last_good_record_falls_through_to_bundled() {
        let (root, versions, bundled) = resolve_fixture("last-good-stale");
        install_fake_bundled(&bundled);
        // 记录指向的版本目录已不存在 → 当「没有」，继续下一跳。
        write_last_good(&versions, "1.36.0");

        let core = resolve_core_with(
            &config_with(Aria2Source::Managed, ""),
            Some(&versions),
            Some(&bundled),
        )
        .expect("应继续退到内置核心");

        assert_eq!(core.origin, CoreOrigin::Bundled);
        assert!(core.fell_back);
        let reason = core.reason.expect("回退必须带原因");
        assert!(reason.contains("last_good"), "{reason}");
        assert!(reason.contains("回退到内置核心"), "{reason}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_source_requires_a_non_empty_existing_file() {
        let (root, versions, bundled) = resolve_fixture("external-strict");

        let empty = resolve_core_with(
            &config_with(Aria2Source::External, "   "),
            Some(&versions),
            Some(&bundled),
        )
        .expect_err("external 且路径为空必须报错");
        assert!(empty.to_string().contains("external_path 为空"), "{empty}");

        let missing = resolve_core_with(
            &config_with(Aria2Source::External, "C:\\nope\\aria2c.exe"),
            Some(&versions),
            Some(&bundled),
        )
        .expect_err("external 指向不存在的文件必须报错");
        assert!(
            missing
                .to_string()
                .contains("external_path 指向的文件不存在"),
            "{missing}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_versions_root_reports_a_readable_reason() {
        let (root, _versions, bundled) = resolve_fixture("no-root");
        let error = resolve_core_with(&config_with(Aria2Source::Auto, ""), None, Some(&bundled))
            .expect_err("拿不到版本库根目录时必须有可读原因");

        assert!(error.to_string().contains("版本库根目录"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn core_origin_strings_are_pinned() {
        // 面板/`--aria2-status`/诊断包都按这几个字符串判断，中文不许混进来。
        assert_eq!(CoreOrigin::Bundled.as_str(), "bundled");
        assert_eq!(CoreOrigin::Managed.as_str(), "managed");
        assert_eq!(CoreOrigin::External.as_str(), "external");
    }
}
