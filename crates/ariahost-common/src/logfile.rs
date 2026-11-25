//! `ariahost.log` 落盘（需求 F11 的「双日志」里客户端那一半）。
//!
//! ## 为什么要有这个模块
//!
//! 面板的日志原先只在内存环形缓冲里（`ariahost-ui::LogBuffer`，给界面看的），**进程一退就全丢**。
//! 用户报「点 X 之后卡住」时，我们让他导诊断包，包里只有 `aria2.log`，面板自己的行为一行都没有。
//! 这个模块把面板动作写到 `%APPDATA%\AriaHost\logs\ariahost.log`（服务平面是
//! `%ProgramData%\AriaHost\logs\`），清理引擎（`arihost-core::clean`）才有东西可轮转/可删。
//!
//! ## 三条「不造第二套规则」的约束
//!
//! 1. **时间戳**：用 [`crate::time::utc_stamp`]（定宽 24 字节 `YYYY-MM-DDTHH:MM:SS.mmmZ`），
//!    与 `service.log` 同一份实现——这里不自己 format。
//! 2. **脱敏**：写入前过 [`crate::redact::redact`]（`token:` / `--rpc-secret=` / 磁力 infohash /
//!    代理密码）。**脱敏在写盘前做**，不是读的时候做：磁盘上的字节必须已经是安全的。
//! 3. **轮转编号**：[`rotated_index`] 是「第几份」的**唯一实现**：`arihost-core::clean`
//!    与这里的写入器都调它（`.1` 是**最近**一份，`.4` 最老，`.5` 起算超限）。清理器与
//!    写入器对「第几份」理解不一致，就会删错文件——所以它先是公开的，后来又把副本删了。
//!
//! ## 行格式（与 `service.log` 对齐）
//!
//! ```text
//! 2026-11-25T03:12:45.000Z INFO  ui: 面板启动
//! 2026-11-25T03:12:45.100Z ERROR up: 控制管道创建失败（Win32 5）
//! ```
//!
//! 前两个空格分隔的字段是**时间戳与级别**（定宽、可 grep），其余是正文；正文以 `target:` 开头，
//! 便于按来源过滤。`service.log` 的行没有 `target:` 前缀，但前两个字段相同，同一套解析器
//! （[`parse_line`]）能吃两份——**一个写（[`format_line`]）、一个读（[`parse_line`]），
//! 成对放在同一个模块里**，格式一改两边一起改，且 round-trip 测试会立刻红。
//!
//! ## 失败姿态：调用即安全
//!
//! [`write`] 永不 panic、永不返回错误、永不让调用方等 IO：
//! 目录不存在就尝试建一次；建不出来/磁盘满/只读盘 → 静默降级为「不落盘」，向 stderr 警告**一次**。
//! 日志是诊断设施，**不能反过来把主流程 drag 死**。
//!
//! 全局状态是 `OnceLock<Mutex<Option<FileLog>>>`：锁被毒化也不 panic（取回内部值继续）。

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::paths::{logs_dir, Plane};
use crate::redact::redact;
use crate::time::utc_stamp;

/// 客户端日志文件名（需求 F11）。
pub const LOG_FILE_NAME: &str = "ariahost.log";

/// 轮转标记：`ariahost.log.3` 里的 `.log.`（与 `clean.rs` 的 `LOG_ROTATION_MARKER` 同字面）。
pub const ROTATION_MARKER: &str = ".log.";

/// 单文件上限 10 MiB（需求 F11「按大小轮转 10MB×5 份」）。
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 保留总数（当前文件 + `.1`..=`.4`）。
pub const DEFAULT_KEEP_TOTAL: u32 = 5;

/// 日志级别。
///
/// 为什么四档而不是 `service.log` 的三档：面板有「仅排障时想看」的细节（渲染后端探测、
/// 窗口几何恢复），归到 `Debug`。`as_str` 与 `service.log` 的三档同名，同一套 grep 都吃得下。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// 出错（启动失败、退出流程失败、外部命令失败）。
    Error,
    /// 可继续但需要注意（降级、重试、端口被占）。
    Warn,
    /// 正常节点（启动、命令、状态迁移）。
    Info,
    /// 排障细节（默认不落 `Info` 以下的内容由调用方决定，本模块不额外过滤）。
    Debug,
}

impl Level {
    /// 行内固定文本（定宽、大写、可 grep）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

/// 轮转参数（单测直接构造，线上用 [`Limits::default`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 单文件上限（字节）。超过就轮转。
    pub max_bytes: u64,
    /// 保留总数（含当前文件）。`1` 表示只留当前文件、轮转即覆盖。
    pub keep_total: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            keep_total: DEFAULT_KEEP_TOTAL,
        }
    }
}

/// `ariahost.log.3` → `Some(3)`；不是轮转文件返回 `None`。
///
/// **语义与 `arihost-core::clean` 的 `rotated_index` 逐字一致**：按最后一个 `.log.` 切分，
/// 后缀必须是非空纯 ASCII 数字。这里公开它，是为了让清理引擎改用它、而不是再维护一份副本。
pub fn rotated_index(name: &str) -> Option<u32> {
    let (_, suffix) = name.rsplit_once(ROTATION_MARKER)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok()
}

/// 拼一行的**纯函数**（时间戳与脱敏都在这里定型，便于单测断言整行文本）。
pub fn format_line(stamp: &str, level: Level, target: &str, message: &str) -> String {
    format!("{stamp} {} {target}: {}\n", level.as_str(), redact(message))
}

/// 一行日志的解析结果（[`parse_line`]）。
///
/// 只切「时间戳 / 级别 / 其余」三个定宽可判的字段，**不猜正文结构**：
/// `ariahost.log` 的正文是 `target: 文本`，`service.log` 的正文没有 `target:` 前缀，
/// 想看 `target` 的用 [`LogLine::target`] 显式切。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLine<'a> {
    /// 24 字节定宽时间戳（`YYYY-MM-DDTHH:MM:SS.mmmZ`）。
    pub stamp: &'a str,
    /// 级别。
    pub level: Level,
    /// 级别之后到行尾的全部内容（不含换行符）。
    pub rest: &'a str,
}

/// `target` 名的最大长度（超过就不当 `target:`，按正文处理）。
pub const MAX_TARGET_LEN: usize = 24;

impl<'a> LogLine<'a> {
    /// 把 [`Self::rest`] 切成 `target` 与正文（`service.log` 这类没有前缀的行返回 `None`）。
    ///
    /// 判据是**保守的**：`target` 必须非空、全 ASCII 字母数字/`_`/`-`/`.`，且不超过
    /// [`MAX_TARGET_LEN`]。中文正文里常见的 `注意：` 是全角冒号，天然不命中；万一正文
    /// 以 `abc: 中文` 起步，最多把 `abc` 当 target，正文一字不丢。
    pub fn target(&self) -> Option<(&'a str, &'a str)> {
        let (name, message) = self.rest.split_once(": ")?;
        let plausible = !name.is_empty()
            && name.len() <= MAX_TARGET_LEN
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        plausible.then_some((name, message))
    }
}

/// 解析一行（[`format_line`] 的逆操作，`service.log` 的行也能吃）。
///
/// 返回 `None` 表示「不是我们写出来的形态」：时间戳不是 24 字节定宽、级别不认识、
/// 或者连两个空格分隔字段都不够。**不 panic、不丢字节**：成功时
/// `format!("{} {} {}\n", line.stamp, line.level.as_str(), line.rest)` 与输入逐字相同。
pub fn parse_line(line: &str) -> Option<LogLine<'_>> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (stamp, tail) = line.split_once(' ')?;
    if !is_fixed_stamp(stamp) {
        return None;
    }
    let (level, rest) = tail.split_once(' ')?;
    let level = Level::parse(level)?;
    Some(LogLine { stamp, level, rest })
}

impl Level {
    /// 行内固定文本 → 级别（[`Level::as_str`] 的逆操作）。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ERROR" => Some(Self::Error),
            "WARN" => Some(Self::Warn),
            "INFO" => Some(Self::Info),
            "DEBUG" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// 是不是 24 字节定宽时间戳（只查形态，不查日历合法性——那是 [`crate::time`] 的事）。
fn is_fixed_stamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    if bytes.len() != crate::time::STAMP_LEN {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b'T'
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19)
        && bytes[19] == b'.'
        && digits(20..23)
        && bytes[23] == b'Z'
}

/// 一个日志文件（含轮转状态）。
///
/// 线上由 [`write`] 通过全局单例驱动；单测直接 `FileLog::new(临时目录, …)`，
/// 这样不需要为了测试去动全局 `OnceLock`（一个进程只能 init 一次）。
pub struct FileLog {
    dir: PathBuf,
    limits: Limits,
    file: Option<File>,
    bytes: u64,
    warned: bool,
}

/// 文件末尾是不是换行（只读最后 1 字节）。
///
/// 读不到（文件不存在/被占用/长度为 0）时返回 `true`：空文件与「读不出来」都不需要补换行
/// ——补了反而会在干净文件开头留一行空行。
fn ends_with_newline(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return true;
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if length == 0 {
        return true;
    }
    if file.seek(SeekFrom::End(-1)).is_err() {
        return true;
    }
    let mut byte = [0u8; 1];
    matches!(file.read(&mut byte), Ok(1)) && byte[0] == b'\n'
}

impl FileLog {
    /// 建一个写入器（**不立刻建目录/开文件**：第一次写入才碰磁盘）。
    pub fn new(dir: impl Into<PathBuf>, limits: Limits) -> Self {
        Self {
            dir: dir.into(),
            limits,
            file: None,
            bytes: 0,
            warned: false,
        }
    }

    /// 日志目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 当前文件路径。
    pub fn path(&self) -> PathBuf {
        self.dir.join(LOG_FILE_NAME)
    }

    /// 第 `index` 份轮转文件（`index >= 1`；`.1` 是最近一份）。
    pub fn rotated_path(&self, index: u32) -> PathBuf {
        self.dir.join(format!("{LOG_FILE_NAME}.{index}"))
    }

    /// 写一行（时间取自系统时钟）。
    pub fn write(&mut self, level: Level, target: &str, message: &str) -> bool {
        self.write_at(SystemTime::now(), level, target, message)
    }

    /// 指定时间戳写一行（单测用，好断言整行文本）。
    pub fn write_at(
        &mut self,
        instant: SystemTime,
        level: Level,
        target: &str,
        message: &str,
    ) -> bool {
        let line = format_line(&utc_stamp(instant), level, target, message);
        let bytes = line.as_bytes();
        let needed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

        // 超限就轮转：先关句柄（Windows 上句柄占用会让改名失败），再滚链。
        let over_limit = self.file.is_some()
            && self.bytes.saturating_add(needed) > self.limits.max_bytes
            && self.bytes > 0;
        if over_limit && !self.rotate() {
            return false;
        }

        if !self.ensure_open() {
            return false;
        }
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        if file.write_all(bytes).is_err() {
            // 写失败（磁盘满/管道断）：丢掉句柄，下次重开，避免一直往坏句柄写。
            self.file = None;
            self.warn_once("写入失败（磁盘满/权限/被占用），本次起跳过落盘");
            return false;
        }
        self.bytes = self.bytes.saturating_add(needed);
        true
    }

    /// 打开文件（追加模式）。目录不存在就建一次。
    fn ensure_open(&mut self) -> bool {
        if self.file.is_some() {
            return true;
        }
        if let Err(error) = fs::create_dir_all(&self.dir) {
            self.warn_once(&format!(
                "建不了日志目录 {}（{error}），本次起不落盘",
                self.dir.display()
            ));
            return false;
        }
        let path = self.path();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                let mut length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                // 上一次可能写一半被打断、被工具截断，或文件是外部程序建的：末尾没有 `\n` 时，
                // 我们 append 的第一行会**粘在最后一行尾巴上**，两行合成一行、时间戳就丢了
                // （解析器只会把整行当消息）。所以先补一个换行，再照常追加。
                if length > 0 && !ends_with_newline(&path) && file.write_all(b"\n").is_ok() {
                    length += 1;
                }
                self.bytes = length;
                self.file = Some(file);
                true
            }
            Err(error) => {
                self.warn_once(&format!(
                    "打不开日志文件 {}（{error}），本次起不落盘",
                    path.display()
                ));
                false
            }
        }
    }

    /// 滚链：删最老的一份 → 依次 `.i` → `.i+1` → 当前 → `.1`。
    ///
    /// 顺序反不得：`fs::rename` 在 Windows 上目标存在会失败，所以必须先删/先腾位。
    fn rotate(&mut self) -> bool {
        // 先关句柄：Windows 上「打开着的文件」不能改名。
        self.file = None;
        let current = self.path();
        let keep = self.limits.keep_total.max(1);
        if keep == 1 {
            // 只留一份：删掉旧的当前文件，下一次 ensure_open 会新建空的。
            if let Err(error) = fs::remove_file(&current) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    self.warn_once(&format!("轮转删不掉 {}（{error}）", current.display()));
                    return false;
                }
            }
            self.bytes = 0;
            return true;
        }
        // 最老的一份（`.keep-1`）直接删掉：保留总数里没有它的位置。
        let oldest = self.rotated_path(keep - 1);
        if let Err(error) = fs::remove_file(&oldest) {
            if error.kind() != std::io::ErrorKind::NotFound {
                self.warn_once(&format!("轮转删不掉 {}（{error}）", oldest.display()));
                return false;
            }
        }
        // `.keep-2` → `.keep-1`，…，`.1` → `.2`（倒序，保证目标位已腾空）。
        for index in (1..=keep.saturating_sub(2)).rev() {
            let from = self.rotated_path(index);
            if !from.exists() {
                continue;
            }
            let to = self.rotated_path(index + 1);
            if let Err(error) = fs::rename(&from, &to) {
                self.warn_once(&format!(
                    "轮转改名失败 {} → {}（{error}）",
                    from.display(),
                    to.display()
                ));
                return false;
            }
        }
        if current.exists() {
            let target = self.rotated_path(1);
            if let Err(error) = fs::rename(&current, &target) {
                self.warn_once(&format!(
                    "轮转改名失败 {} → {}（{error}）",
                    current.display(),
                    target.display()
                ));
                return false;
            }
        }
        self.bytes = 0;
        true
    }

    /// 降级时只朝 stderr 喊一次（日志设施自己坏了，不能靠日志通知）。
    fn warn_once(&mut self, message: &str) {
        if self.warned {
            return;
        }
        self.warned = true;
        eprintln!("[ariahost.log] {message}");
    }
}

/// 全局写入器（`init` 装一次，`write` 用它）。
static GLOBAL: OnceLock<Mutex<Option<FileLog>>> = OnceLock::new();

/// 按平面初始化（幂等，重复调用无副作用）。
///
/// 拿不到目录（例如 `%APPDATA%` 缺失的服务账户）就**什么都不做**：之后 [`write`] 静默 no-op，
/// 绝不在这种环境里制造噪音。
pub fn init(plane: Plane) {
    let Some(dir) = logs_dir(plane) else {
        return;
    };
    init_at(dir, Limits::default());
}

/// 指定目录初始化（自检/单测用；幂等）。
pub fn init_at(dir: impl Into<PathBuf>, limits: Limits) {
    let cell = GLOBAL.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(FileLog::new(dir, limits));
    }
}

/// 写一行（**调用即安全**：不 panic、不返回错误、不阻塞调用方等 IO）。
///
/// 未初始化时静默丢弃（CLI 的 `--version` 之类不值得在用户盘上建目录）。
pub fn write(level: Level, target: &str, message: &str) {
    let Some(cell) = GLOBAL.get() else {
        return;
    };
    let mut guard = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(log) = guard.as_mut() {
        let _ = log.write_at(SystemTime::now(), level, target, message);
    }
}

/// 当前日志文件路径（未初始化返回 `None`；自检与诊断包用）。
pub fn current_path() -> Option<PathBuf> {
    let cell = GLOBAL.get()?;
    let guard = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().map(FileLog::path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    /// 每个测试一个独立临时目录（与仓库其它模块同风格，不引第三方 tempfile）。
    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ariahost-logfile-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// 定宽时间戳的测试基准（`utc_stamp` 的取值已由 common::time 自己钉住）。
    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn line_shape_matches_service_log_fields() {
        let line = format_line("2023-11-14T22:13:20.000Z", Level::Info, "ui", "面板启动");
        assert_eq!(line, "2023-11-14T22:13:20.000Z INFO ui: 面板启动\n");
        // 前两个空格分隔字段就是「定宽时间戳 + 级别」，与 service.log 一致。
        let mut fields = line.splitn(3, ' ');
        assert_eq!(fields.next(), Some("2023-11-14T22:13:20.000Z"));
        assert_eq!(fields.next(), Some("INFO"));
    }

    #[test]
    fn stamp_is_exactly_24_bytes() {
        let line = format_line(&utc_stamp(at(1_700_000_000)), Level::Warn, "ui", "降级");
        assert_eq!(&line[..24], "2023-11-14T22:13:20.000Z");
        assert_eq!(line.as_bytes()[24], b' ');
    }

    #[test]
    fn all_four_levels_have_fixed_width_text() {
        assert_eq!(Level::Error.as_str(), "ERROR");
        assert_eq!(Level::Warn.as_str(), "WARN");
        assert_eq!(Level::Info.as_str(), "INFO");
        assert_eq!(Level::Debug.as_str(), "DEBUG");
    }

    #[test]
    fn rotated_index_matches_clean_engine_semantics() {
        assert_eq!(rotated_index("ariahost.log.1"), Some(1));
        assert_eq!(rotated_index("ariahost.log.3"), Some(3));
        assert_eq!(rotated_index("ariahost.log"), None);
        assert_eq!(rotated_index("ariahost.log.abc"), None);
        assert_eq!(rotated_index("ariahost.log."), None);
        assert_eq!(rotated_index("aria2.log.2"), Some(2));
    }

    #[test]
    fn writes_lines_with_utc_and_target() {
        let dir = temp_dir("basic");
        let mut log = FileLog::new(&dir, Limits::default());
        assert!(log.write_at(at(1_700_000_000), Level::Info, "ui", "面板启动"));
        assert!(log.write_at(at(1_700_000_000), Level::Error, "up", "退出失败"));
        let text = read(&log.path());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "2023-11-14T22:13:20.000Z INFO ui: 面板启动");
        assert_eq!(lines[1], "2023-11-14T22:13:20.000Z ERROR up: 退出失败");
    }

    #[test]
    fn secrets_are_redacted_before_hitting_disk() {
        let dir = temp_dir("redact");
        let mut log = FileLog::new(&dir, Limits::default());
        // 三种形态（任务书要求）：token / rpc-secret / 磁力 infohash。
        log.write_at(
            at(1_700_000_000),
            Level::Info,
            "rpc",
            "call params=[\"token:supersecret123\"]",
        );
        log.write_at(
            at(1_700_000_000),
            Level::Warn,
            "conf",
            "aria2c --rpc-secret=topsecret456",
        );
        log.write_at(
            at(1_700_000_000),
            Level::Info,
            "task",
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x",
        );
        let text = read(&log.path());
        for secret in [
            "supersecret123",
            "topsecret456",
            "0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(!text.contains(secret), "落盘内容里还有明文 {secret}");
        }
        assert!(text.contains("***"), "脱敏占位符没出现");
    }

    #[test]
    fn proxy_password_in_url_is_redacted() {
        let dir = temp_dir("redact-proxy");
        let mut log = FileLog::new(&dir, Limits::default());
        log.write_at(
            at(1_700_000_000),
            Level::Info,
            "net",
            "proxy=http://user:pw123456@127.0.0.1:7890",
        );
        let text = read(&log.path());
        assert!(!text.contains("pw123456"));
    }

    #[test]
    fn crossing_the_size_limit_rotates_to_dot_one() {
        let dir = temp_dir("rotate");
        let limits = Limits {
            max_bytes: 200,
            keep_total: 5,
        };
        let mut log = FileLog::new(&dir, limits);
        // 每行约 40 字节：连写 6 行必然跨过 200。
        for index in 0..6 {
            assert!(log.write_at(
                at(1_700_000_000),
                Level::Info,
                "ui",
                &format!("第 {index} 行"),
            ));
        }
        assert!(log.path().exists(), "当前文件应存在");
        assert!(log.rotated_path(1).exists(), "跨过上限后应出现 .1");
        // `.1` 装的是轮转前的内容（第 0 行），当前文件里是新内容。
        assert!(read(&log.rotated_path(1)).contains("第 0 行"));
        assert!(read(&log.path()).contains("第 5 行"));
        assert!(!log.rotated_path(2).exists(), "只轮转过一次，不该有 .2");
    }

    #[test]
    fn keep_total_bounds_the_number_of_files() {
        let dir = temp_dir("keep");
        let limits = Limits {
            max_bytes: 100,
            keep_total: 3,
        };
        let mut log = FileLog::new(&dir, limits);
        // 写很多行，制造多次轮转（keep_total=3 → 当前 + .1 + .2）。
        for index in 0..40 {
            log.write_at(
                at(1_700_000_000),
                Level::Info,
                "ui",
                &format!("行 {index:02}"),
            );
        }
        let mut files: Vec<String> = fs::read_dir(&dir)
            .expect("读目录")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(
            files,
            vec![
                "ariahost.log".to_owned(),
                "ariahost.log.1".to_owned(),
                "ariahost.log.2".to_owned(),
            ],
            "保留总数必须是 keep_total"
        );
        // 超出份数的一份都不能存在（清理引擎按 `.N >= keep_total` 判超限）。
        for name in &files {
            if let Some(index) = rotated_index(name) {
                assert!(index < 3, "出现了超限的轮转文件 {name}");
            }
        }
    }

    #[test]
    fn older_files_hold_older_content() {
        // 「.1 是最近一份」的语义：编号越大内容越旧。
        let dir = temp_dir("order");
        let limits = Limits {
            max_bytes: 80,
            keep_total: 3,
        };
        let mut log = FileLog::new(&dir, limits);
        for index in 0..20 {
            log.write_at(
                at(1_700_000_000),
                Level::Info,
                "ui",
                &format!("标记 {index:02}"),
            );
        }
        let current = read(&log.path());
        let first = read(&log.rotated_path(1));
        let second = read(&log.rotated_path(2));
        let tail_of = |text: &str| -> String {
            text.lines()
                .last()
                .unwrap_or_default()
                .rsplit(' ')
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let current_tail: u32 = tail_of(&current).parse().unwrap_or(0);
        let first_tail: u32 = tail_of(&first).parse().unwrap_or(0);
        let second_tail: u32 = tail_of(&second).parse().unwrap_or(0);
        assert!(
            current_tail > first_tail && first_tail > second_tail,
            "编号越大应越旧：current={current_tail} .1={first_tail} .2={second_tail}"
        );
    }

    #[test]
    fn single_keep_rotates_by_replacing() {
        // keep_total = 1：只留当前文件，轮转等于删旧建新（不能出现 .1）。
        let dir = temp_dir("keep1");
        let limits = Limits {
            max_bytes: 60,
            keep_total: 1,
        };
        let mut log = FileLog::new(&dir, limits);
        for index in 0..5 {
            log.write_at(
                at(1_700_000_000),
                Level::Info,
                "ui",
                &format!("第 {index} 行"),
            );
        }
        assert!(log.path().exists());
        assert!(!log.rotated_path(1).exists(), "keep_total=1 不该产生 .1");
        assert!(read(&log.path()).contains("第 4 行"));
    }

    #[test]
    fn oversized_single_line_is_still_written() {
        // 一行就超过上限：不能死循环轮转，必须写进去。
        let dir = temp_dir("huge");
        let limits = Limits {
            max_bytes: 50,
            keep_total: 3,
        };
        let mut log = FileLog::new(&dir, limits);
        let long = "x".repeat(500);
        assert!(log.write_at(at(1_700_000_000), Level::Info, "ui", &long));
        assert_eq!(read(&log.path()).lines().count(), 1);
    }

    #[test]
    fn unwritable_dir_degrades_without_panicking() {
        // 用一个「父路径是文件」的目录，保证 create_dir_all 必失败（跨平台、无需 chmod）。
        let blocker = temp_dir("blocked");
        fs::create_dir_all(&blocker).expect("建临时目录");
        let blocker_file = blocker.join("not-a-dir");
        fs::write(&blocker_file, b"x").expect("写占位文件");
        let mut log = FileLog::new(blocker_file.join("logs"), Limits::default());
        // 不 panic、返回 false，且可以继续调用（每次都在失败里返回 false）。
        assert!(!log.write_at(at(1_700_000_000), Level::Info, "ui", "写不进去"));
        assert!(!log.write_at(at(1_700_000_000), Level::Info, "ui", "再来一次"));
    }

    #[test]
    fn appending_to_a_file_without_trailing_newline_inserts_one() {
        // 真实场景（2026-11-25 真机撞到）：日志文件可能是上次写一半被打断、被工具截断，
        // 或干脆是外部程序建的，末尾没有 `\n`。直接 append 会把我们第一行粘在它尾巴上，
        // 两行合成一行——解析器只能把整行当消息，时间戳当场丢失。
        let dir = temp_dir("no-trailing-newline");
        fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join(LOG_FILE_NAME);
        // 模拟现场：BOM + 外部写的一段，末尾**没有**换行。
        fs::write(&path, b"\xef\xbb\xbfpwsh-probe").expect("写无换行结尾的文件");

        let mut log = FileLog::new(&dir, Limits::default());
        assert!(log.write_at(at(1_700_000_000), Level::Info, "ui", "第一行"));

        let text = read(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "必须补换行分成两行：{text:?}");
        assert!(lines[0].contains("pwsh-probe"), "旧内容不许被动：{text:?}");
        // 关键断言：我们那行仍能被解析器**完整还原**（时间戳/级别/target 一个不少）。
        let parsed = parse_line(lines[1]).expect("写入行必须可解析");
        assert_eq!(parsed.stamp, "2023-11-14T22:13:20.000Z");
        assert_eq!(parsed.level, Level::Info);
        assert_eq!(parsed.target(), Some(("ui", "第一行")));
    }

    #[test]
    fn empty_log_file_gets_no_leading_blank_line() {
        // 补换行只针对「非空且末尾不是 \n」；空文件不能因此多一行空行。
        let dir = temp_dir("empty-file");
        fs::create_dir_all(&dir).expect("建目录");
        let path = dir.join(LOG_FILE_NAME);
        fs::write(&path, b"").expect("建空文件");
        let mut log = FileLog::new(&dir, Limits::default());
        assert!(log.write_at(at(1_700_000_000), Level::Info, "ui", "只有一行"));
        let text = read(&path);
        assert_eq!(text.lines().count(), 1, "空文件不该留空行：{text:?}");
        assert!(text.starts_with("2023-11-14T22:13:20.000Z"), "{text:?}");
    }

    #[test]
    fn global_write_is_safe_and_lands_under_the_initialised_path() {
        // 全局状态是进程级 `OnceLock`：本进程里谁先 init 就定谁（测试并行、顺序不定），
        // 所以这里不断言「目录是我给的那个」，只断言「初始化后 write 不 panic，且内容落在
        // current_path() 指的文件里」——这正好是线上唯一的用法契约。
        let dir = temp_dir("global");
        init_at(&dir, Limits::default());
        write(Level::Info, "ui", "全局写入-一号");
        let path = current_path().expect("初始化后应有路径");
        assert!(path.exists(), "写完后文件应存在：{}", path.display());
        let text = read(&path);
        assert!(text.contains("INFO ui: 全局写入-一号"), "实际内容：{text}");

        // init_at 幂等：再次调用换目录不生效（OnceLock 语义），路径不变。
        init_at(temp_dir("global-second"), Limits::default());
        assert_eq!(current_path().unwrap(), path);
    }

    #[test]
    fn format_line_and_parse_line_round_trip() {
        // 写/读成对存在，就必须能原样还原：四个级别 × 有无正文都过一遍。
        let stamp = "2026-11-25T03:12:45.000Z";
        let samples = [
            (Level::Info, "ui", "面板启动"),
            (Level::Warn, "up", "端口被占，退回随机端口"),
            (Level::Error, "svc", "控制管道创建失败（Win32 5）"),
            (Level::Debug, "gfx", "adapter=NVIDIA; backend=dx12"),
        ];
        for (level, target, message) in samples {
            let line = format_line(stamp, level, target, message);
            let parsed = parse_line(&line).unwrap_or_else(|| panic!("应解析成功：{line:?}"));
            assert_eq!(parsed.stamp, stamp);
            assert_eq!(parsed.level, level);
            assert_eq!(parsed.target(), Some((target, message)));
            // 逐字还原（含换行）：读取方拿到的字节就是写入方写下的字节。
            assert_eq!(
                format!(
                    "{} {} {}\n",
                    parsed.stamp,
                    parsed.level.as_str(),
                    parsed.rest
                ),
                line
            );
        }
    }

    #[test]
    fn parse_line_reads_service_log_lines_without_target() {
        // `service.log` 的行没有 `target:` 前缀：解析器要能吃，且不硬切出一段假 target。
        let line = "2026-11-25T03:12:45.100Z INFO 服务主循环进入\n";
        let parsed = parse_line(line).expect("服务日志行应可解析");
        assert_eq!(parsed.stamp, "2026-11-25T03:12:45.100Z");
        assert_eq!(parsed.level, Level::Info);
        assert_eq!(parsed.rest, "服务主循环进入");
        assert_eq!(parsed.target(), None);
        // 正文自带全角冒号也不该被当成 target 前缀。
        let path_like = parse_line("2026-11-25T03:12:45.100Z ERROR 写入失败：C:\\a b").unwrap();
        assert_eq!(path_like.target(), None);
    }

    #[test]
    fn parse_line_rejects_foreign_lines() {
        // 不是我们写的形态就返回 None（宁可不解析，也不编造字段）。
        for line in [
            "",
            "不是日志",
            "2026-11-25 03:12:45 INFO ui: 空格分隔的时间",
            "2026-11-25T03:12:45.000Z NOTICE ui: 级别不认识",
            "2026-11-25T03:12:45.000Z",
            "2026-11-25T03:12:45.000 INFO ui: 少了毫秒/Z",
        ] {
            assert!(parse_line(line).is_none(), "不该解析成功：{line:?}");
        }
    }

    #[test]
    fn parse_line_keeps_redacted_text_as_written() {
        // 脱敏在写入前做：读回来的就是磁盘上的安全字节，解析不负责还原原文。
        let line = format_line(
            "2026-11-25T03:12:45.000Z",
            Level::Info,
            "rpc",
            "token:abc123 与 --rpc-secret=xyz 都不该落盘",
        );
        assert!(!line.contains("abc123") && !line.contains("xyz"));
        let parsed = parse_line(&line).expect("应可解析");
        assert!(parsed.rest.contains("***"));
        assert_eq!(parsed.target().map(|(name, _)| name), Some("rpc"));
    }

    #[test]
    fn target_split_is_conservative() {
        // 返回 owned（解析结果借用行文本，闭包里现造的行活不过 return）。
        let split = |rest: &str| -> Option<(String, String)> {
            let line = format!("2026-11-25T03:12:45.000Z INFO {rest}");
            parse_line(&line).and_then(|parsed| {
                parsed
                    .target()
                    .map(|(name, message)| (name.to_owned(), message.to_owned()))
            })
        };
        assert_eq!(split("ui: x"), Some(("ui".to_owned(), "x".to_owned())));
        assert_eq!(
            split("a.b-c_d: x"),
            Some(("a.b-c_d".to_owned(), "x".to_owned()))
        );
        // 有空格/过长/空 → 不切
        assert_eq!(split("两个词: x"), None);
        assert_eq!(split(": x"), None);
        let long = format!("{}: x", "a".repeat(MAX_TARGET_LEN + 1));
        assert_eq!(split(&long), None);
        // 全角冒号不命中
        assert_eq!(split("注意：别切"), None);
    }
}
