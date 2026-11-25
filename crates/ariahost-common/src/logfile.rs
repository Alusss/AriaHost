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
//! 3. **轮转编号**：[`rotated_index`] 与 `arihost-core::clean` 的 `rotated_index` 同一语义——
//!    `.1` 是**最近**一份，`.4` 最老，`.5` 起算超限。清理器与写入器对「第几份」理解不一致，
//!    就会删错文件（这也是本模块把该函数**公开**出来的原因：下一步 core 应改用它，别再留副本）。
//!
//! ## 行格式（与 `service.log` 对齐）
//!
//! ```text
//! 2026-11-25T03:12:45.000Z INFO  ui: 面板启动
//! 2026-11-25T03:12:45.100Z ERROR up: 控制管道创建失败（Win32 5）
//! ```
//!
//! 前两个空格分隔的字段是**时间戳与级别**（定宽、可 grep），其余是正文；正文以 `target:` 开头，
//! 便于按来源过滤。`service.log` 的行没有 `target:` 前缀，但前两个字段相同，同一套解析器能吃两份。
//!
//! ## 失败姿态：调用即安全
//!
//! [`write`] 永不 panic、永不返回错误、永不让调用方等 IO：
//! 目录不存在就尝试建一次；建不出来/磁盘满/只读盘 → 静默降级为「不落盘」，向 stderr 警告**一次**。
//! 日志是诊断设施，**不能反过来把主流程 drag 死**。
//!
//! 全局状态是 `OnceLock<Mutex<Option<FileLog>>>`：锁被毒化也不 panic（取回内部值继续）。

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
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
            Ok(file) => {
                self.bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
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
}
