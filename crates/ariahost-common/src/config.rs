//! 配置层：`%APPDATA%\AriaHost\config.toml` 的读写（架构 §4.11 D11）。
//!
//! ## 定版 schema
//!
//! 键名、分节与默认值**逐条照抄**架构 §4.11.1 的 52 键清单（`schema_version` 不计入）。
//! 不在这里自创键名：改名等于迁移，必须有 §7-R 的裁决。
//!
//! ## 三条硬要求
//!
//! 1. **缺失文件 = 全默认**：首次启动不写盘，等用户真的改了才落盘。
//! 2. **损坏文件不静默覆盖**：解析失败一律返回错误并保留原文件（用户手写的注释/取值不能被吞掉）。
//! 3. **未知键原样保留**：每一层都有 `#[serde(flatten)] extra`，老/新版本互不破坏。
//!
//! ## 原子写
//!
//! 同目录临时文件 → 写满 → `sync_all` → 先把现存文件另存为 `config.toml.bak` → `rename` 覆盖。
//! 任何一步失败都返回错误（不 panic），临时文件在失败路径上尽力删除。
//!
//! ## `aria2.rpc.secret`
//!
//! 配置里该键默认为空字符串，表示「自动生成」。生效值按以下顺序解析：
//!
//! 1. `config.toml` 里非空 → 直接用（用户显式指定）；
//! 2. 否则读 `<appdata>\AriaHost\secret.bin`（DPAPI 用户作用域）；读不到/解不开 → 重新生成
//!    16 字节密码学随机 → 32 位 hex → DPAPI 保护后原子写回 `secret.bin`。
//!
//! 生成随机数**不引新依赖**：Windows 用 `BCryptGenRandom`（系统首选 RNG），
//! 其它平台读 `/dev/urandom`（仅 WSL 单测路径走到，见下方 `random_secret_hex` 的 cfg 分支）。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// 定版 schema 版本号；迁移链的入口（架构 §4.11）。
pub const SCHEMA_VERSION: u32 = 1;

/// 配置文件名（`%APPDATA%\AriaHost\config.toml`）。
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// secret 文件名（与 `config.toml` 同目录；DPAPI 用户作用域加密）。
pub const SECRET_FILE_NAME: &str = "secret.bin";

/// 未知键容器：原样读入、原样写回。
pub type Extra = BTreeMap<String, toml::Value>;

// ---------------------------------------------------------------------------
// 结构定义（键名 = 文档键名）
// ---------------------------------------------------------------------------

/// 整份配置。字段顺序与文档一致，便于人工对照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// schema 版本；只有迁移才改。
    pub schema_version: u32,
    pub aria2: Aria2Section,
    pub web: WebSection,
    pub service: ServiceSection,
    pub dl: DlSection,
    pub net: NetSection,
    pub queue: QueueSection,
    pub ui: UiSection,
    pub notify: NotifySection,
    pub clean: CleanSection,
    pub lifecycle: LifecycleSection,
    /// 未知顶层键（新版本加的节 / 用户手写残留）。
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[aria2]`：核心来源与更新（5 键 + 4 键 rpc 子表）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Aria2Section {
    /// `managed`（版本库托管）或 `external`（用户自己的 aria2c）。
    pub source: Aria2Source,
    /// `source = "external"` 时的 aria2c 路径（支持 `~` 展开）。
    pub external_path: String,
    pub update_channel: String,
    pub check_update_on_start: bool,
    /// 恒定 false：升级永远显式确认（架构 §4.11 注释）。
    pub auto_update: bool,
    pub rpc: RpcSection,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[aria2.rpc]`：取代 v1 的 `[rpc]` 组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcSection {
    /// `localhost` | `lan` | `off`。
    pub listen: RpcListen,
    pub port: u16,
    /// 空 = 自动生成（见模块文档）。
    pub secret: String,
    pub allow_origin_all: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[web]`：内置网页管理页；仅监听 127.0.0.1。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSection {
    pub port: u16,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[service]`：SCM 服务形态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceSection {
    pub mode: ServiceMode,
    pub start_type: String,
    pub recovery: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[dl]`：下载落盘行为。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DlSection {
    /// 默认 `~/Downloads`；渲染 `aria2.conf` 前必须展开成绝对路径。
    pub dir: String,
    pub auto_file_renaming: bool,
    pub allow_overwrite: bool,
    /// TOML 键名是保留字 `continue`，字段名加后缀。
    #[serde(rename = "continue")]
    pub continue_dl: bool,
    pub always_resume: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[net]`：连接与限速。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetSection {
    pub split: u8,
    pub max_connection_per_server: u8,
    pub min_split_size: String,
    pub max_overall_download_limit: String,
    pub max_download_limit: String,
    pub max_overall_upload_limit: String,
    pub connect_timeout: u32,
    pub max_tries: u8,
    pub proxy: String,
    /// 面板文案必须写「内存缓存」（架构 §4.11 注释）。
    pub disk_cache: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[queue]`：队列与结果集。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueSection {
    pub max_concurrent: u8,
    pub max_download_result: u32,
    pub keep_unfinished_result: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[ui]`：界面行为（本批只存不执行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSection {
    pub language: String,
    pub theme: String,
    pub renderer: String,
    pub minimize_to_tray: bool,
    pub start_minimized: bool,
    pub close_action: String,
    pub start_with_windows: bool,
    pub task_manager_target: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[notify]`：通知（本批只存不执行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifySection {
    pub on_complete: bool,
    pub on_error: bool,
    pub backend: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[clean]`：清理阈值（本批只存不执行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CleanSection {
    pub scan_on_start: bool,
    pub scan_interval_hours: u32,
    pub aria2_idle_days: u32,
    pub log_max_size_mb: u32,
    pub log_keep_files: u32,
    pub cache_max_mb: u32,
    pub min_free_space_gb: u32,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `[lifecycle]`：启停交互策略（本批只存不执行）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleSection {
    pub on_start_unfinished: String,
    pub on_close_active: String,
    pub quit_default: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `aria2.source` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aria2Source {
    /// 版本库托管（默认；架构裁决 R1）。
    Managed,
    /// 用户自带 aria2c。
    External,
}

impl Aria2Source {
    /// 文本形态（写日志/诊断包用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::External => "external",
        }
    }
}

/// `aria2.rpc.listen` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RpcListen {
    /// 仅回环（默认，安全）。
    Localhost,
    /// 监听所有网卡（需 MSG-U08 提示）。
    Lan,
    /// 不开 RPC。
    Off,
}

impl RpcListen {
    /// 文本形态。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Localhost => "localhost",
            Self::Lan => "lan",
            Self::Off => "off",
        }
    }

    /// `--rpc-listen-all` 的取值（`Off` 由调用方处理，不产生该参数）。
    pub fn listen_all(self) -> Option<bool> {
        match self {
            Self::Localhost => Some(false),
            Self::Lan => Some(true),
            Self::Off => None,
        }
    }
}

/// `service.mode` 取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceMode {
    /// 无服务，直接跑（默认）。
    #[serde(rename = "headless")]
    Headless,
    /// 已注册为 Windows 服务（裁决 R6/R9）。
    #[serde(rename = "windows-service")]
    WindowsService,
}

impl ServiceMode {
    /// 文本形态。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
            Self::WindowsService => "windows-service",
        }
    }
}

// ---------------------------------------------------------------------------
// 默认值（逐条对应架构 §4.11.1 的「默认值」列）
// ---------------------------------------------------------------------------

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            aria2: Aria2Section::default(),
            web: WebSection::default(),
            service: ServiceSection::default(),
            dl: DlSection::default(),
            net: NetSection::default(),
            queue: QueueSection::default(),
            ui: UiSection::default(),
            notify: NotifySection::default(),
            clean: CleanSection::default(),
            lifecycle: LifecycleSection::default(),
            extra: Extra::new(),
        }
    }
}

impl Default for Aria2Section {
    fn default() -> Self {
        Self {
            source: Aria2Source::Managed,
            external_path: String::new(),
            update_channel: "stable".to_owned(),
            check_update_on_start: true,
            auto_update: false,
            rpc: RpcSection::default(),
            extra: Extra::new(),
        }
    }
}

impl Default for RpcSection {
    fn default() -> Self {
        Self {
            listen: RpcListen::Localhost,
            port: 6800,
            secret: String::new(),
            allow_origin_all: false,
            extra: Extra::new(),
        }
    }
}

impl Default for WebSection {
    fn default() -> Self {
        Self {
            port: 6875,
            extra: Extra::new(),
        }
    }
}

impl Default for ServiceSection {
    fn default() -> Self {
        Self {
            mode: ServiceMode::Headless,
            start_type: "manual".to_owned(),
            recovery: "restart".to_owned(),
            extra: Extra::new(),
        }
    }
}

impl Default for DlSection {
    fn default() -> Self {
        Self {
            dir: "~/Downloads".to_owned(),
            auto_file_renaming: true,
            allow_overwrite: false,
            continue_dl: true,
            always_resume: true,
            extra: Extra::new(),
        }
    }
}

impl Default for NetSection {
    fn default() -> Self {
        Self {
            split: 5,
            max_connection_per_server: 1,
            min_split_size: "20M".to_owned(),
            max_overall_download_limit: "0".to_owned(),
            max_download_limit: "0".to_owned(),
            max_overall_upload_limit: "0".to_owned(),
            connect_timeout: 60,
            max_tries: 5,
            proxy: String::new(),
            disk_cache: "16M".to_owned(),
            extra: Extra::new(),
        }
    }
}

impl Default for QueueSection {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            max_download_result: 1000,
            keep_unfinished_result: true,
            extra: Extra::new(),
        }
    }
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            language: "system".to_owned(),
            theme: "system".to_owned(),
            renderer: "auto".to_owned(),
            minimize_to_tray: true,
            start_minimized: true,
            close_action: "tray".to_owned(),
            start_with_windows: false,
            task_manager_target: "internal".to_owned(),
            extra: Extra::new(),
        }
    }
}

impl Default for NotifySection {
    fn default() -> Self {
        Self {
            on_complete: true,
            on_error: true,
            backend: "auto".to_owned(),
            extra: Extra::new(),
        }
    }
}

impl Default for CleanSection {
    fn default() -> Self {
        Self {
            scan_on_start: true,
            scan_interval_hours: 24,
            aria2_idle_days: 7,
            log_max_size_mb: 10,
            log_keep_files: 5,
            cache_max_mb: 500,
            min_free_space_gb: 5,
            extra: Extra::new(),
        }
    }
}

impl Default for LifecycleSection {
    fn default() -> Self {
        Self {
            on_start_unfinished: "ask".to_owned(),
            on_close_active: "ask".to_owned(),
            quit_default: "pause".to_owned(),
            extra: Extra::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// 配置读写失败原因。
///
/// 每个 kind 对应一个既有 E1 段错误码（`AH-E1001`–`AH-E1005`），
/// 让 UI 能按码选主按钮（尤其是 `UnsupportedSchemaVersion` 与 `Parse` 的**建议不同**：
/// 前者是「升级/备份后重置」，后者才是「修复或删除」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// 拿不到配置根目录（`%APPDATA%` 缺失）→ `AH-E1001`。
    PathUnavailable,
    /// 读/写/改名失败 → `AH-E1003`。
    Io,
    /// TOML 解析失败（含 schema 字段类型不符）→ `AH-E1002`。
    Parse,
    /// `schema_version` 比本程序新，拒绝按旧规则改写 → `AH-E1005`。
    UnsupportedSchemaVersion,
    /// secret 生成/加解密失败 → `AH-E1004`。
    Secret,
}

/// 配置读写错误（可读中文信息 + 可选既有错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    kind: ConfigErrorKind,
    path: Option<PathBuf>,
    detail: String,
}

impl ConfigError {
    fn new(kind: ConfigErrorKind, path: Option<&Path>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.map(Path::to_path_buf),
            detail: detail.into(),
        }
    }

    /// 失败类别。
    pub fn kind(&self) -> ConfigErrorKind {
        self.kind
    }

    /// 出问题的文件（若有）。
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// 对应的错误码（E1 配置段）。
    ///
    /// 末尾的 `_` 分支是**刻意保留的兜底位**：将来新增 `ConfigErrorKind` 变体时，
    /// 若还没来得及在架构 §4.12 里申请到码，这里返回 `None` 而不是被编译器逼着
    /// 硬塞进某个既有码（`None` 的调用方会退回「按 kind 给提示」）。
    #[allow(unreachable_patterns)]
    pub fn code(&self) -> Option<ErrorCode> {
        match self.kind {
            ConfigErrorKind::PathUnavailable => Some(ErrorCode::Usage),
            ConfigErrorKind::Parse => Some(ErrorCode::ConfigParseFailed),
            ConfigErrorKind::Io => Some(ErrorCode::ConfigIoFailed),
            ConfigErrorKind::Secret => Some(ErrorCode::CredentialCryptoFailed),
            ConfigErrorKind::UnsupportedSchemaVersion => Some(ErrorCode::ConfigSchemaTooNew),
            _ => None,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head = match self.kind {
            ConfigErrorKind::PathUnavailable => "配置目录不可用",
            ConfigErrorKind::Io => "配置读写失败",
            ConfigErrorKind::Parse => "配置文件解析失败（原文件未改动）",
            ConfigErrorKind::UnsupportedSchemaVersion => "配置 schema 版本过新，拒绝改写",
            ConfigErrorKind::Secret => "RPC secret 处理失败",
        };
        match &self.path {
            Some(path) => write!(formatter, "{head}：{}：{}", path.display(), self.detail),
            None => write!(formatter, "{head}：{}", self.detail),
        }
    }
}

impl std::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// 路径约定
// ---------------------------------------------------------------------------

/// `%APPDATA%\AriaHost`：配置/日志/session/runtime/secret 的根（headless 平面）。
pub fn appdata_root() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(|dir| PathBuf::from(dir).join("AriaHost"))
}

/// 默认配置路径（复用 [`crate::paths::config_file`]，不另造一套）。
pub fn default_path() -> Option<PathBuf> {
    crate::paths::config_file()
}

/// 运行时目录 `%APPDATA%\AriaHost\runtime`（放 `aria2.conf`、`pid`）。
pub fn runtime_dir() -> Option<PathBuf> {
    appdata_root().map(|dir| dir.join("runtime"))
}

/// session 目录 `%APPDATA%\AriaHost\session`（放 `aria2.session`）。
pub fn session_dir() -> Option<PathBuf> {
    appdata_root().map(|dir| dir.join("session"))
}

/// 日志目录 `%APPDATA%\AriaHost\logs`。
pub fn logs_dir() -> Option<PathBuf> {
    appdata_root().map(|dir| dir.join("logs"))
}

/// 默认 secret 路径（与 `config.toml` 同目录的 `secret.bin`）。
pub fn default_secret_path() -> Option<PathBuf> {
    appdata_root().map(|dir| dir.join(SECRET_FILE_NAME))
}

/// 把配置里的 `~`/`~/xxx` 展开成用户目录绝对路径。
///
/// 只处理前缀 `~`：其它形式原样返回（由调用方给出可读错误）。
pub fn expand_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let rest = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
        .or_else(|| (trimmed == "~").then_some(""));
    match rest {
        Some(rest) => {
            let home = user_home().unwrap_or_default();
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(trimmed),
    }
}

/// 用户主目录（`USERPROFILE`，缺失时退 `HOME`）。
pub fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

/// 下载目录的绝对路径（`aria2.conf` 的 `dir=` 必须绝对）。
pub fn download_dir_absolute(config: &Config) -> Result<PathBuf, ConfigError> {
    let raw = config.dl.dir.trim();
    if raw.is_empty() {
        return Err(ConfigError::new(
            ConfigErrorKind::PathUnavailable,
            None,
            "dl.dir 为空；请在设置页填写下载目录",
        ));
    }
    let expanded = expand_user_path(raw);
    // `~` 开头的展开结果由主目录保证绝对；其余按**字符串**判定 —— 产品只跑 Windows，
    // 而 Linux（WSL 单测通道）上 `Path::is_absolute()` 不认 `D:\...`，不能拿它当唯一判据。
    let absolute = if raw.starts_with('~') {
        expanded.is_absolute()
    } else {
        is_absolute_str(raw)
    };
    if absolute {
        Ok(expanded)
    } else {
        Err(ConfigError::new(
            ConfigErrorKind::PathUnavailable,
            Some(Path::new(raw)),
            "dl.dir 必须是绝对路径（或 ~/ 开头）；aria2 的 dir 若为相对路径会落到进程工作目录",
        ))
    }
}

/// 按「Windows 或宿主平台」判绝对路径（`C:\x`、`C:/x`、`\\server\share`、`/unix/x`）。
fn is_absolute_str(raw: &str) -> bool {
    if Path::new(raw).is_absolute() {
        return true;
    }
    let bytes = raw.as_bytes();
    let drive_with_root = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = raw.starts_with("\\\\") || raw.starts_with("//");
    drive_with_root || unc
}

// ---------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------

/// 读默认路径的配置；文件不存在 → 全默认。
pub fn load() -> Result<Config, ConfigError> {
    let path = default_path().ok_or_else(|| {
        ConfigError::new(ConfigErrorKind::PathUnavailable, None, "拿不到 %APPDATA%")
    })?;
    load_from(&path)
}

/// 读指定路径的配置；文件不存在 → 全默认。
pub fn load_from(path: &Path) -> Result<Config, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => {
            return Err(ConfigError::new(
                ConfigErrorKind::Io,
                Some(path),
                error.to_string(),
            ))
        }
    };
    // 允许空文件（等价默认值），但不允许坏 TOML。
    if text.trim().is_empty() {
        return Ok(Config::default());
    }
    let config: Config = toml::from_str(&text)
        .map_err(|error| ConfigError::new(ConfigErrorKind::Parse, Some(path), error.to_string()))?;
    if config.schema_version > SCHEMA_VERSION {
        return Err(ConfigError::new(
            ConfigErrorKind::UnsupportedSchemaVersion,
            Some(path),
            format!(
                "文件 schema_version={}，本程序支持到 {SCHEMA_VERSION}",
                config.schema_version
            ),
        ));
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// 写（原子 + .bak）
// ---------------------------------------------------------------------------

/// 写默认路径（`%APPDATA%\AriaHost\config.toml`），目录不存在会创建。
pub fn save(config: &Config) -> Result<(), ConfigError> {
    let path = default_path().ok_or_else(|| {
        ConfigError::new(ConfigErrorKind::PathUnavailable, None, "拿不到 %APPDATA%")
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(parent), error.to_string())
        })?;
    }
    save_to(config, &path)
}

/// 原子写指定路径：临时文件 → 落盘 → 旧文件转 `.bak` → rename。
pub fn save_to(config: &Config, path: &Path) -> Result<(), ConfigError> {
    let text = toml::to_string_pretty(config).map_err(|error| {
        ConfigError::new(
            ConfigErrorKind::Io,
            Some(path),
            format!("序列化失败：{error}"),
        )
    })?;
    let parent = path.parent().ok_or_else(|| {
        ConfigError::new(
            ConfigErrorKind::PathUnavailable,
            Some(path),
            "目标路径没有父目录",
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| ConfigError::new(ConfigErrorKind::Io, Some(parent), error.to_string()))?;

    let temporary = sibling(path, "tmp");
    {
        let mut file = fs::File::create(&temporary).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
        file.write_all(text.as_bytes()).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
        file.sync_all().map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
    }

    // 旧文件先留一份 .bak（存在才留）。
    if path.exists() {
        let backup = sibling(path, "bak");
        let _ = fs::remove_file(&backup);
        fs::rename(path, &backup).map_err(|error| {
            let _ = fs::remove_file(&temporary);
            ConfigError::new(
                ConfigErrorKind::Io,
                Some(path),
                format!("无法生成 .bak：{error}"),
            )
        })?;
    }

    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(ConfigError::new(
                ConfigErrorKind::Io,
                Some(path),
                error.to_string(),
            ))
        }
    }
}

/// 同目录下的兄弟路径（`config.toml` + 后缀）。
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| CONFIG_FILE_NAME.to_owned());
    path.with_file_name(format!("{file_name}.{suffix}"))
}

// ---------------------------------------------------------------------------
// secret
// ---------------------------------------------------------------------------

/// 解析「生效 RPC secret」：配置非空即用，否则读/生成 `secret.bin`。
///
/// 返回值即注入 `--rpc-secret=` 的字符串（32 位小写 hex，或用户自定义值）。
pub fn resolve_secret(config: &mut Config, secret_path: &Path) -> Result<String, ConfigError> {
    let explicit = config.aria2.rpc.secret.trim();
    if !explicit.is_empty() {
        return Ok(explicit.to_owned());
    }
    if let Some(text) = read_secret_file(secret_path) {
        return Ok(text);
    }
    // 没有/解不开 → 重新生成并落盘（密码学安全随机）。
    let generated = random_secret_hex();
    let protected = protect(generated.as_bytes())
        .map_err(|detail| ConfigError::new(ConfigErrorKind::Secret, Some(secret_path), detail))?;
    if let Some(parent) = secret_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(parent), error.to_string())
        })?;
    }
    let temporary = sibling(secret_path, "tmp");
    {
        let mut file = fs::File::create(&temporary).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
        file.write_all(&protected).map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
        file.sync_all().map_err(|error| {
            ConfigError::new(ConfigErrorKind::Io, Some(&temporary), error.to_string())
        })?;
    }
    fs::rename(&temporary, secret_path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        ConfigError::new(ConfigErrorKind::Io, Some(secret_path), error.to_string())
    })?;
    Ok(generated)
}

/// 生成 16 字节密码学随机 → 32 位小写 hex。
pub fn random_secret_hex() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes);
    let mut text = String::with_capacity(32);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0f) as usize] as char);
    }
    text
}

/// 读 `secret.bin` 并解出明文；任何一步不成立都返回 `None`（由调用方重新生成）。
fn read_secret_file(path: &Path) -> Option<String> {
    let protected = fs::read(path).ok()?;
    if protected.is_empty() {
        return None;
    }
    let plain = unprotect(&protected).ok()?;
    let text = String::from_utf8(plain).ok()?;
    let trimmed = text.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

// ---- 随机源：Windows 走 BCryptGenRandom，其它平台读 /dev/urandom ----

#[cfg(windows)]
fn fill_random(buffer: &mut [u8]) {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // 调用约定：halgorithm = 0 表示用系统首选 RNG 算法。
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    assert_eq!(status, 0, "BCryptGenRandom 失败（NTSTATUS={status}）");
}

#[cfg(not(windows))]
fn fill_random(buffer: &mut [u8]) {
    // 仅 WSL/CI 的 Linux 单测路径；产品只发 Windows。
    use std::io::Read as _;
    let mut handle = std::fs::File::open("/dev/urandom").expect("打开 /dev/urandom");
    handle.read_exact(buffer).expect("读 /dev/urandom");
}

// ---- 保护：Windows 走 DPAPI（用户作用域），其它平台明文（仅单测） ----

#[cfg(windows)]
fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(plain, true)
}

#[cfg(windows)]
fn unprotect(protected: &[u8]) -> Result<Vec<u8>, String> {
    dpapi(protected, false)
}

#[cfg(not(windows))]
fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    // 非 Windows（WSL 单测）没有 DPAPI：原样返回。产品只发 Windows，不走这条分支。
    Ok(plain.to_vec())
}

#[cfg(not(windows))]
fn unprotect(protected: &[u8]) -> Result<Vec<u8>, String> {
    Ok(protected.to_vec())
}

#[cfg(windows)]
fn dpapi(input: &[u8], encrypting: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let mut input_bytes = input.to_vec();
    let blob_in = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input_bytes.len()).map_err(|_| "secret 过长".to_owned())?,
        pbData: input_bytes.as_mut_ptr(),
    };
    let mut blob_out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY：输入/输出都是本栈上的有效 CRYPT_INTEGER_BLOB；输入缓冲在整个调用期间存活；
    // UI_FORBIDDEN 保证不弹窗（服务/无头进程没有交互桌面）。
    let ok = unsafe {
        if encrypting {
            CryptProtectData(
                &blob_in,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut blob_out,
            )
        } else {
            CryptUnprotectData(
                &blob_in,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut blob_out,
            )
        }
    };
    if ok == 0 {
        return Err(format!(
            "DPAPI 调用失败：{}",
            std::io::Error::last_os_error()
        ));
    }
    if blob_out.pbData.is_null() || blob_out.cbData == 0 {
        return Ok(Vec::new());
    }
    // SAFETY：DPAPI 成功时 pbData 指向 cbData 字节的 LocalAlloc 缓冲区，由我们 LocalFree。
    let copied = unsafe {
        let slice = std::slice::from_raw_parts(blob_out.pbData, blob_out.cbData as usize);
        let copied = slice.to_vec();
        LocalFree(blob_out.pbData as HLOCAL);
        copied
    };
    Ok(copied)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ariahost-config-{tag}-{}-{}",
                std::process::id(),
                next_seq()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn config_path(&self) -> PathBuf {
            self.0.join(CONFIG_FILE_NAME)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn next_seq() -> u32 {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        SEQ.fetch_add(1, Ordering::Relaxed)
    }

    /// 只改 `aria2.rpc.port` 的配置（避免 `field_reassign_with_default`）。
    fn with_rpc_port(port: u16) -> Config {
        Config {
            aria2: Aria2Section {
                rpc: RpcSection {
                    port,
                    ..RpcSection::default()
                },
                ..Aria2Section::default()
            },
            ..Config::default()
        }
    }

    /// 只改 `dl.dir` 的配置。
    fn with_dl_dir(dir: &str) -> Config {
        Config {
            dl: DlSection {
                dir: dir.to_owned(),
                ..DlSection::default()
            },
            ..Config::default()
        }
    }

    /// 只改 `aria2.rpc.secret` 的配置。
    fn with_secret(secret: &str) -> Config {
        Config {
            aria2: Aria2Section {
                rpc: RpcSection {
                    secret: secret.to_owned(),
                    ..RpcSection::default()
                },
                ..Aria2Section::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn defaults_match_the_52_key_schema() {
        let config = Config::default();
        assert_eq!(config.schema_version, SCHEMA_VERSION);
        assert_eq!(config.aria2.source, Aria2Source::Managed);
        assert!(config.aria2.external_path.is_empty());
        assert_eq!(config.aria2.update_channel, "stable");
        assert!(config.aria2.check_update_on_start);
        assert!(!config.aria2.auto_update);
        assert_eq!(config.aria2.rpc.listen, RpcListen::Localhost);
        assert_eq!(config.aria2.rpc.port, 6800);
        assert!(config.aria2.rpc.secret.is_empty());
        assert!(!config.aria2.rpc.allow_origin_all);
        assert_eq!(config.web.port, 6875);
        assert_eq!(config.service.mode, ServiceMode::Headless);
        assert_eq!(config.service.start_type, "manual");
        assert_eq!(config.service.recovery, "restart");
        assert_eq!(config.dl.dir, "~/Downloads");
        assert!(config.dl.auto_file_renaming);
        assert!(!config.dl.allow_overwrite);
        assert!(config.dl.continue_dl);
        assert!(config.dl.always_resume);
        assert_eq!(config.net.split, 5);
        assert_eq!(config.net.max_connection_per_server, 1);
        assert_eq!(config.net.min_split_size, "20M");
        assert_eq!(config.net.connect_timeout, 60);
        assert_eq!(config.net.max_tries, 5);
        assert_eq!(config.net.disk_cache, "16M");
        assert_eq!(config.queue.max_concurrent, 5);
        assert_eq!(config.queue.max_download_result, 1000);
        assert!(config.queue.keep_unfinished_result);
        assert_eq!(config.ui.renderer, "auto");
        assert!(config.ui.start_minimized);
        assert_eq!(config.ui.close_action, "tray");
        assert_eq!(config.notify.backend, "auto");
        assert_eq!(config.clean.log_max_size_mb, 10);
        assert_eq!(config.lifecycle.quit_default, "pause");
    }

    /// 架构 §4.11.1 的「计数断言」：序列化出来的叶子键必须正好 52 个。
    #[test]
    fn serialized_schema_has_exactly_52_leaf_keys() {
        let text = toml::to_string_pretty(&Config::default()).expect("序列化");
        let value: toml::Value = toml::from_str(&text).expect("回读");
        let mut count = 0usize;
        collect_leaf_keys(&value, &mut count);
        // schema_version 不计入 52 键。
        assert_eq!(count - 1, 52, "键数应与架构 §4.11.1 一致：\n{text}");
    }

    fn collect_leaf_keys(value: &toml::Value, count: &mut usize) {
        match value {
            toml::Value::Table(table) => {
                for (_key, child) in table {
                    collect_leaf_keys(child, count);
                }
            }
            _ => *count += 1,
        }
    }

    #[test]
    fn missing_file_yields_defaults_without_creating_anything() {
        let dir = TestDir::new("missing");
        let path = dir.config_path();
        let config = load_from(&path).expect("缺失文件应按默认值成功");
        assert_eq!(config, Config::default());
        assert!(!path.exists(), "读路径不应顺手建文件");
    }

    #[test]
    fn corrupt_file_reports_error_and_is_left_untouched() {
        let dir = TestDir::new("corrupt");
        let path = dir.config_path();
        let original = "[aria2]\nsource = managed\n";
        fs::write(&path, original).expect("写坏配置");

        let error = load_from(&path).expect_err("坏 TOML 必须报错");
        assert_eq!(error.kind(), ConfigErrorKind::Parse);
        assert_eq!(
            error.code(),
            Some(ErrorCode::ConfigParseFailed),
            "配置损坏必须给 AH-E1002（可删可修）"
        );
        assert!(error.to_string().contains("解析失败"), "{error}");
        // 关键：原文件一个字节都不许改。
        assert_eq!(fs::read_to_string(&path).expect("读回"), original);
    }

    #[test]
    fn unknown_keys_round_trip_at_every_level() {
        let dir = TestDir::new("unknown");
        let path = dir.config_path();
        let text = r#"
schema_version = 1
future_top = "keep me"

[aria2]
source = "external"
future_aria2 = 7

[aria2.rpc]
port = 6801
future_rpc = true

[dl]
dir = "D:\\dl"
future_dl = "x"

[brand_new_section]
alpha = 1
beta = "two"
"#;
        fs::write(&path, text).expect("写含未知键的配置");
        let config = load_from(&path).expect("未知键不该导致失败");
        assert_eq!(config.aria2.source, Aria2Source::External);
        assert_eq!(config.aria2.rpc.port, 6801);
        assert_eq!(config.dl.dir, "D:\\dl");
        assert!(config.extra.contains_key("future_top"));
        assert!(config.aria2.extra.contains_key("future_aria2"));
        assert!(config.aria2.rpc.extra.contains_key("future_rpc"));
        assert!(config.dl.extra.contains_key("future_dl"));
        assert!(config.extra.contains_key("brand_new_section"));

        // 原样写回：这些键都得在。
        save_to(&config, &path).expect("保存");
        let written = fs::read_to_string(&path).expect("读回");
        for needle in [
            "future_top",
            "future_aria2",
            "future_rpc",
            "future_dl",
            "brand_new_section",
            "alpha",
        ] {
            assert!(written.contains(needle), "写回丢了 {needle}：\n{written}");
        }
    }

    #[test]
    fn save_is_atomic_and_keeps_one_backup() {
        let dir = TestDir::new("atomic");
        let path = dir.config_path();

        let first = with_rpc_port(7001);
        save_to(&first, &path).expect("首次保存");
        assert!(path.exists());
        assert!(!sibling(&path, "tmp").exists(), "不能残留临时文件");
        assert!(
            !sibling(&path, "bak").exists(),
            "首次保存没有旧文件，不该有 .bak"
        );

        let second = with_rpc_port(7002);
        save_to(&second, &path).expect("二次保存");
        let backup = fs::read_to_string(sibling(&path, "bak")).expect("应有 .bak");
        assert!(backup.contains("7001"), "备份应是上一次的内容");
        let current = fs::read_to_string(&path).expect("读当前");
        assert!(current.contains("7002"));
        assert!(!sibling(&path, "tmp").exists());
    }

    /// 五种失败类别 → E1 段错误码的一对一映射（含「不给删」的那一条）。
    #[test]
    fn every_error_kind_maps_to_a_distinct_e1_code() {
        let build = |kind: ConfigErrorKind| {
            ConfigError::new(kind, Some(Path::new("config.toml")), "detail")
        };

        let path_unavailable = build(ConfigErrorKind::PathUnavailable);
        assert_eq!(path_unavailable.code(), Some(ErrorCode::Usage));
        assert_eq!(path_unavailable.code().expect("有码").code(), "AH-E1001");

        let parse = build(ConfigErrorKind::Parse);
        assert_eq!(parse.code(), Some(ErrorCode::ConfigParseFailed));
        assert_eq!(parse.code().expect("有码").code(), "AH-E1002");

        let io = build(ConfigErrorKind::Io);
        assert_eq!(io.code(), Some(ErrorCode::ConfigIoFailed));
        assert_eq!(io.code().expect("有码").code(), "AH-E1003");

        let secret = build(ConfigErrorKind::Secret);
        assert_eq!(secret.code(), Some(ErrorCode::CredentialCryptoFailed));
        assert_eq!(secret.code().expect("有码").code(), "AH-E1004");

        let too_new = build(ConfigErrorKind::UnsupportedSchemaVersion);
        assert_eq!(too_new.code(), Some(ErrorCode::ConfigSchemaTooNew));
        assert_eq!(too_new.code().expect("有码").code(), "AH-E1005");

        // 语义护栏：解析失败可以说「删掉重建」，schema 过新绝不能这么说
        //（否则用户一键丢掉全部设置）。这里用 code 的差异把这条契约钉住。
        assert_ne!(parse.code(), too_new.code());
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let dir = TestDir::new("schema");
        let path = dir.config_path();
        fs::write(&path, "schema_version = 99\n").expect("写");
        let error = load_from(&path).expect_err("更高版本必须拒绝");
        assert_eq!(error.kind(), ConfigErrorKind::UnsupportedSchemaVersion);
        assert_eq!(
            error.code(),
            Some(ErrorCode::ConfigSchemaTooNew),
            "schema 过新必须与「解析失败」区分：不能建议用户删配置"
        );
    }

    #[test]
    fn partial_file_fills_the_rest_with_defaults() {
        let dir = TestDir::new("partial");
        let path = dir.config_path();
        fs::write(&path, "[aria2.rpc]\nport = 7777\n").expect("写");
        let config = load_from(&path).expect("部分文件应成功");
        assert_eq!(config.aria2.rpc.port, 7777);
        assert_eq!(config.aria2.source, Aria2Source::Managed, "缺的键取默认");
        assert_eq!(config.net.split, 5);
    }

    #[test]
    fn tilde_and_relative_download_dirs() {
        // 相对路径：明确拒绝（服务/无头下相对 dir 会落到 system32）。
        let relative = with_dl_dir("relative/dir");
        let error = download_dir_absolute(&relative).expect_err("相对路径必须拒绝");
        assert_eq!(error.kind(), ConfigErrorKind::PathUnavailable);

        // 空值同样拒绝。
        assert!(download_dir_absolute(&with_dl_dir("   ")).is_err());

        // 绝对路径原样接受（不要求存在）。
        assert_eq!(
            download_dir_absolute(&with_dl_dir("D:\\downloads")).expect("绝对路径"),
            PathBuf::from("D:\\downloads")
        );
    }

    #[test]
    fn generated_secret_is_32_hex_and_persisted_once() {
        let dir = TestDir::new("secret");
        let secret_path = dir.0.join(SECRET_FILE_NAME);

        let mut config = Config::default();
        let first = resolve_secret(&mut config, &secret_path).expect("首次生成");
        assert_eq!(first.len(), 32, "16 字节 → 32 位 hex");
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()), "{first}");
        assert!(secret_path.exists(), "必须落盘");

        // 第二次必须读到同一个值（不重新生成）。
        let second = resolve_secret(&mut config, &secret_path).expect("再次读取");
        assert_eq!(first, second);
    }

    #[test]
    fn explicit_secret_in_config_wins_over_secret_bin() {
        let dir = TestDir::new("secret-explicit");
        let secret_path = dir.0.join(SECRET_FILE_NAME);
        fs::write(&secret_path, b"ignored").expect("写");

        let mut config = with_secret("user-chosen");
        assert_eq!(
            resolve_secret(&mut config, &secret_path).expect("用配置值"),
            "user-chosen"
        );
    }

    #[test]
    fn random_secrets_differ() {
        let first = random_secret_hex();
        let second = random_secret_hex();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
    }

    #[test]
    fn listen_flags_map_to_listen_all() {
        assert_eq!(RpcListen::Localhost.listen_all(), Some(false));
        assert_eq!(RpcListen::Lan.listen_all(), Some(true));
        assert_eq!(RpcListen::Off.listen_all(), None);
    }
}
