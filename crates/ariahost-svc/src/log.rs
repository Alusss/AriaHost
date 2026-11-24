//! 服务平面日志：`%ProgramData%\AriaHost\logs\service.log`（F11 双日志里的服务侧）。
//!
//! ## 为什么不是「重定向 stderr」
//!
//! Windows 服务由 SCM 拉起，**没有控制台**——stdout/stderr 没有接收者，写进去就丢了。
//! 所以服务进程必须**自己写文件**：所有关键节点都显式调 [`ServiceLog`] 记一行，
//! 而不是指望「把 stderr 接到文件」。重定向只在我们真的能继承句柄时才成立，
//! 而 SCM 拉起时并不给。
//!
//! ## 三条硬约束
//!
//! 1. **日志永远不能拖垮服务**：打开/写入/轮转任何一步失败都只降级（stderr 告警**一次**），
//!    绝不让 [`ServiceLog`] 返回错误给业务、绝不 panic。
//! 2. **脱敏**：[`redact`] 抹掉 `token:`、`token=`、`--rpc-secret=`、磁力 infohash、URL 密码。
//! 3. **大小轮转**：默认 10MB × 5 份（`service.log` + `service.1.log`…`service.4.log`），
//!    同进程内做，不加依赖。阈值可注入，单测用小阈值验滚动。
//!
//! 与 `aria2.log` 的分工：那是 aria2c 自己写的下载日志（`log=` 选项），本模块只管
//! **服务进程自身**的事件，不去改 aria2 的 `log=` 指向（避免两边互相覆盖）。

use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 日志级别。只留三档：服务日志是给排障看的，不需要 TRACE/DEBUG 的花活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 正常节点（进入、命令、状态迁移、就绪）。
    Info,
    /// 可继续但需要注意（降级、轮转失败、幂等重试）。
    Warn,
    /// 出错（启动失败、停止失败、上报失败）。
    Error,
}

impl Level {
    /// 行内固定文本（对齐、可 grep）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// 日志去处。抽象出来是为了能在 Linux 单测里断言「该记的都记了」。
///
/// 取 `&self` 而非 `&mut self`：状态机与状态上报是两处独立代码，
/// 它们共享同一份日志却不需要互相传递可变借用（内部用 `RefCell`）。
pub trait LogSink {
    /// 记一行。实现**不得** panic、不得返回错误（失败自己吞）。
    fn record(&self, level: Level, message: &str);
}

/// 丢弃一切的日志。[`drive_service`](crate::plan::drive_service) 的测试默认用它。
pub struct NoopLog;

impl LogSink for NoopLog {
    fn record(&self, _level: Level, _message: &str) {}
}

/// 共享的「什么都不记」实例，省得每处 `&NoopLog` 都新建。
pub static NOOP: NoopLog = NoopLog;

/// 轮转参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 单个文件上限（字节）。超过就轮转。
    pub max_bytes: u64,
    /// 保留份数（含当前文件）。5 = `service.log` + `.1`…`.4`。
    pub keep: usize,
}

/// 10MB（F11 口径）。
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// 5 份（F11 口径）。
pub const DEFAULT_KEEP: usize = 5;

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            keep: DEFAULT_KEEP,
        }
    }
}

/// 服务日志文件（`<dir>/service.log`）。
///
/// 构造不碰磁盘（除了 [`open_service_plane`](Self::open_service_plane) 也不建目录）：
/// 真正的打开/建目录发生在第一次写入，失败就降级——这样「打不开日志」永远不会
/// 挡在服务启动路径上。
pub struct ServiceLog {
    path: PathBuf,
    limits: Limits,
    state: RefCell<State>,
}

/// 可变部分：当前句柄、已写字节数、是否已经告警过。
struct State {
    file: Option<File>,
    size: u64,
    warned: bool,
    warnings: u32,
}

impl ServiceLog {
    /// 服务平面默认位置：`%ProgramData%\AriaHost\logs\service.log`。
    ///
    /// 取不到 `%ProgramData%` 时返回一个**永远降级**的实例（写 stderr 一次告警），
    /// 不让路径解析失败变成服务启动失败。
    pub fn open_service_plane() -> Self {
        match ariahost_common::paths::service_runtime_dir() {
            Some(dir) => Self::open(dir.join("logs"), Limits::default()),
            None => Self::disabled(),
        }
    }

    /// 指定目录与阈值（单测用）。
    pub fn open(dir: impl Into<PathBuf>, limits: Limits) -> Self {
        Self {
            path: dir.into().join("service.log"),
            limits,
            state: RefCell::new(State {
                file: None,
                size: 0,
                warned: false,
                warnings: 0,
            }),
        }
    }

    /// 明确「用不了」的实例：任何写入都只返回 `false`。
    fn disabled() -> Self {
        let log = Self::open(PathBuf::from("<unresolved>"), Limits::default());
        log.note_degraded("拿不到 %ProgramData%（服务平面目录不可用）");
        log
    }

    /// 当前文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 生效的轮转参数。
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// 是否处于降级态（当前没有可用句柄）。降级不影响服务，只是这行日志丢了。
    pub fn is_degraded(&self) -> bool {
        self.state.borrow().file.is_none()
    }

    /// 累计告警次数——单测用它断言「只告警一次，不刷屏」。
    pub fn warnings(&self) -> u32 {
        self.state.borrow().warnings
    }

    /// INFO 一行。
    pub fn info(&self, message: &str) {
        let _ = self.write(Level::Info, message);
    }

    /// WARN 一行。
    pub fn warn(&self, message: &str) {
        let _ = self.write(Level::Warn, message);
    }

    /// ERROR 一行。
    pub fn error(&self, message: &str) {
        let _ = self.write(Level::Error, message);
    }

    /// 写一行，返回是否真的落盘。**永不 panic、永不返回 Err**。
    pub fn write(&self, level: Level, message: &str) -> bool {
        self.write_at(SystemTime::now(), level, message)
    }

    /// 指定时间戳写入（单测用，好断言整行文本）。
    fn write_at(&self, instant: SystemTime, level: Level, message: &str) -> bool {
        let Ok(mut state) = self.state.try_borrow_mut() else {
            return false;
        };
        let line = format!(
            "{} {} {}\n",
            format_utc(instant),
            level.as_str(),
            redact(message)
        );
        let bytes = line.as_bytes();

        if state.file.is_none() {
            if let Err(error) = self.ensure_open(&mut state) {
                self.note_degraded_with(&mut state, &error);
                return false;
            }
        }

        // 轮转判定：`size > 0` 保证「一行就超限」时也能写进去（否则小阈值会死循环）。
        if state.size > 0 && state.size.saturating_add(bytes.len() as u64) > self.limits.max_bytes {
            if let Err(error) = self.rotate(&mut state) {
                // 轮转失败不丢这一行：继续写进旧文件（宁可超限，也别把排障线索吃掉）。
                self.note_degraded_with(&mut state, &error);
            }
        }

        let written = match state.file.as_mut() {
            Some(file) => file.write_all(bytes),
            None => return false,
        };
        match written {
            Ok(()) => {
                state.size += bytes.len() as u64;
                true
            }
            Err(error) => {
                // 句柄坏了：丢掉它，下一行重新打开（那里若还是失败，会走告警）。
                state.file = None;
                state.size = 0;
                self.note_degraded_with(&mut state, &error);
                false
            }
        }
    }

    /// 打开（或重开）`service.log`，目录不存在就建。
    fn ensure_open(&self, state: &mut State) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // 续写已有文件时要把既有大小算进来，否则重启后永远不轮转。
        state.size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        state.file = Some(file);
        Ok(())
    }

    /// 轮转：`.4` 删除 → `.3`→`.4` → … → `service.log`→`.1` → 重开新文件。
    fn rotate(&self, state: &mut State) -> io::Result<()> {
        state.file = None;
        let last = self.limits.keep.saturating_sub(1).max(1);
        let _ = fs::remove_file(self.rotated_path(last));
        for index in (1..last).rev() {
            let _ = fs::rename(self.rotated_path(index), self.rotated_path(index + 1));
        }
        let _ = fs::rename(&self.path, self.rotated_path(1));
        self.ensure_open(state)
    }

    /// `<dir>/service.<index>.log`。
    fn rotated_path(&self, index: usize) -> PathBuf {
        let dir = self
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        dir.join(format!("service.{index}.log"))
    }

    /// 降级时把原因记一次（构造期没有 `State` 可借，单独一个入口）。
    fn note_degraded(&self, reason: &str) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            self.note_degraded_with(&mut state, reason);
        }
    }

    /// 降级告警：**只在第一次**说话，之后静默（服务里 stderr 反正也没人看）。
    fn note_degraded_with<S: std::fmt::Display>(&self, state: &mut State, reason: S) {
        if state.warned {
            return;
        }
        state.warned = true;
        state.warnings += 1;
        eprintln!(
            "WARN 服务日志不可用（{}）：{}；后续日志继续尝试写入，失败不再重复告警",
            self.path.display(),
            reason
        );
    }
}

impl LogSink for ServiceLog {
    fn record(&self, level: Level, message: &str) {
        let _ = self.write(level, message);
    }
}

// ---------------------------------------------------------------------------
// 时间戳
// ---------------------------------------------------------------------------

/// `SystemTime` → `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC，定宽，可直接按字节切）。
pub fn format_utc(instant: SystemTime) -> String {
    let since_epoch = instant.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX);
    let millis = since_epoch.subsec_millis();
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut out = String::with_capacity(24);
    // `write!` 到 String 不会失败，显式忽略结果以满足 clippy。
    let _ = write!(
        out,
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3_600,
        (of_day % 3_600) / 60,
        of_day % 60
    );
    out
}

/// 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1); // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        u32::try_from(month).unwrap_or(1),
        day,
    )
}

// ---------------------------------------------------------------------------
// 脱敏
// ---------------------------------------------------------------------------

/// 被抹掉的占位符。
const REDACTED: &str = "***";

/// 值的终止字符（出现在这些字符之前的部分才算「秘密值」）。
const VALUE_END: &[char] = &[
    ' ', '\t', '\r', '\n', '"', '\'', '&', ',', ';', ')', ']', '}', '>', '<', '`', '|',
];

/// 脱敏：`token:` / `token=` / `--rpc-secret=` / 磁力 infohash / URL 密码 **全部** 抹成 `***`。
///
/// 为什么必须做：服务日志会被用户贴进 issue，而 aria2 的 RPC secret 一旦泄露，
/// 局域网里任何人都能远程加任务/删文件（架构 §4.6.1 威胁模型）。
pub fn redact(message: &str) -> String {
    let text = with_values_redacted(message, "token:", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "token=", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "rpc-secret=", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "urn:btih:", |ch| ch.is_ascii_alphanumeric());
    redact_url_password(&text)
}

/// 把 `needle` 之后的「连续值字符」替换成 `***`（大小写不敏感，保留 `needle` 原文）。
fn with_values_redacted<F>(message: &str, needle: &str, is_value: F) -> String
where
    F: Fn(char) -> bool,
{
    let lower = message.to_ascii_lowercase();
    let mut out = String::with_capacity(message.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find(needle) {
        let start = search + offset;
        let value_start = start + needle.len();
        let mut value_end = value_start;
        for (index, ch) in message[value_start..].char_indices() {
            if !is_value(ch) {
                break;
            }
            value_end = value_start + index + ch.len_utf8();
        }
        out.push_str(&message[copied..value_start]);
        if value_end > value_start {
            out.push_str(REDACTED);
        }
        copied = value_end;
        // 空值也要前进，否则死循环。
        search = value_end.max(value_start);
    }
    out.push_str(&message[copied..]);
    out
}

/// `scheme://user:password@host/...` 里的密码 → `***`（用户名保留，便于排障）。
fn redact_url_password(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let mut out = String::with_capacity(message.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find("://") {
        let authority_start = search + offset + 3;
        // 权限段到下一个 `/`、`?`、`#`、空白或引号为止。
        let mut authority_end = message.len();
        for (index, ch) in message[authority_start..].char_indices() {
            if matches!(ch, '/' | '?' | '#' | ' ') || VALUE_END.contains(&ch) {
                authority_end = authority_start + index;
                break;
            }
        }
        let authority = &message[authority_start..authority_end];
        if let Some(at) = authority.rfind('@') {
            if let Some(colon) = authority[..at].find(':') {
                let password_start = authority_start + colon + 1;
                let password_end = authority_start + at;
                out.push_str(&message[copied..password_start]);
                if password_end > password_start {
                    out.push_str(REDACTED);
                }
                copied = password_end;
            }
        }
        search = authority_end.max(authority_start);
        if search >= message.len() {
            break;
        }
        // 跳过已经处理过的权限段，继续找下一个 `://`。
        search += 1;
    }
    out.push_str(&message[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    /// 每个测试一个独立临时目录。
    fn temp_dir(tag: &str) -> PathBuf {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ariahost-log-{}-{}-{seq}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    // --- 时间戳 ---

    #[test]
    fn format_utc_matches_known_instants() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 外部核对：`date -u -d @1700000000` = 2023-11-14T22:13:20Z
        assert_eq!(format_utc(at(1_700_000_000)), "2023-11-14T22:13:20.000Z");
        // 外部核对：`date -u -d @1793000000` = 2026-10-26T07:33:20Z
        assert_eq!(format_utc(at(1_793_000_000)), "2026-10-26T07:33:20.000Z");
        // 毫秒与闰年 2 月 29 日（2024-02-29T12:00:00Z）。
        assert_eq!(
            format_utc(at(1_709_208_000) + Duration::from_millis(7)),
            "2024-02-29T12:00:00.007Z"
        );
    }

    #[test]
    fn format_utc_is_lexicographically_ordered() {
        let early = format_utc(at(1_700_000_000));
        let late = format_utc(at(1_700_000_001));
        assert!(
            early < late,
            "定宽 UTC 必须能按字符串排序：{early} / {late}"
        );
        assert_eq!(early.len(), late.len());
    }

    // --- 脱敏 ---

    #[test]
    fn redacts_aria2_token_prefix() {
        assert_eq!(redact("token:abc123"), "token:***");
        assert_eq!(
            redact("调用失败 params=[token:s3cr3t, [\"url\"]]"),
            "调用失败 params=[token:***, [\"url\"]]"
        );
    }

    #[test]
    fn redacts_query_token() {
        assert_eq!(
            redact("GET http://h/api?token=abc123&id=7"),
            "GET http://h/api?token=***&id=7"
        );
        assert_eq!(redact("&token=deadbeef"), "&token=***");
    }

    #[test]
    fn redacts_rpc_secret_in_both_forms() {
        assert_eq!(
            redact("aria2c --rpc-secret=topsecret --enable-rpc"),
            "aria2c --rpc-secret=*** --enable-rpc"
        );
        assert_eq!(redact("rpc-secret=another"), "rpc-secret=***");
    }

    #[test]
    fn redacts_magnet_infohash() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x";
        assert_eq!(
            redact(magnet),
            "magnet:?xt=urn:btih:***&dn=x",
            "infohash 是「谁在下什么」的证据，必须抹"
        );
        // base32 形式（32 字符）同样抹掉。
        assert_eq!(
            redact("urn:btih:ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"),
            "urn:btih:***"
        );
    }

    #[test]
    fn redacts_url_password_but_keeps_user_and_host() {
        assert_eq!(
            redact("下载失败 https://alice:hunter2@example.com/f.zip"),
            "下载失败 https://alice:***@example.com/f.zip"
        );
        // 没密码的 URL 不动。
        assert_eq!(
            redact("https://example.com/f.zip"),
            "https://example.com/f.zip"
        );
        // 端口形式。
        assert_eq!(
            redact("ftp://bob:pw@host:2121/x"),
            "ftp://bob:***@host:2121/x"
        );
    }

    #[test]
    fn keeps_clean_text_untouched() {
        let clean = "INFO 收到控制命令 start；上报 StartPending（wait_hint 30s）";
        assert_eq!(redact(clean), clean);
        // 敏感词作为普通单词出现时不该误伤（没有分隔符就没有值）。
        assert_eq!(redact("token 注入完成"), "token 注入完成");
    }

    #[test]
    fn redaction_handles_multiple_secrets_in_one_line() {
        let line = "args=[--rpc-secret=aaa, token:bbb] url=http://u:p@h/?token=ccc";
        assert_eq!(
            redact(line),
            "args=[--rpc-secret=***, token:***] url=http://u:***@h/?token=***"
        );
    }

    // --- 写入与格式 ---

    #[test]
    fn writes_line_with_utc_and_level() {
        let dir = temp_dir("format");
        let log = ServiceLog::open(&dir, Limits::default());
        assert!(log.write_at(at(1_700_000_000), Level::Info, "服务进入"));
        assert!(log.write_at(at(1_700_000_000), Level::Warn, "降级"));
        assert!(log.write_at(at(1_700_000_000), Level::Error, "AH-E5001 启动失败"));

        let text = read(log.path());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "2023-11-14T22:13:20.000Z INFO 服务进入");
        assert_eq!(lines[1], "2023-11-14T22:13:20.000Z WARN 降级");
        assert_eq!(lines[2], "2023-11-14T22:13:20.000Z ERROR AH-E5001 启动失败");
        // UTF-8 无 BOM、行尾统一 `\n`。
        let bytes = fs::read(log.path()).expect("读日志");
        assert_ne!(&bytes[..3], b"\xEF\xBB\xBF", "不许写 BOM");
        assert!(
            !bytes.windows(2).any(|pair| pair == b"\r\n"),
            "行尾统一 \\n"
        );
    }

    #[test]
    fn redaction_applies_to_written_lines() {
        let dir = temp_dir("redact-write");
        let log = ServiceLog::open(&dir, Limits::default());
        assert!(log.write_at(
            at(1_700_000_000),
            Level::Info,
            "启动 aria2c --rpc-secret=leak"
        ));
        let text = read(log.path());
        assert!(text.contains("--rpc-secret=***"), "{text}");
        assert!(!text.contains("leak"), "{text}");
    }

    #[test]
    fn creates_missing_directories() {
        let dir = temp_dir("mkdir").join("nested").join("logs");
        let log = ServiceLog::open(&dir, Limits::default());
        assert!(log.write_at(at(0), Level::Info, "hi"));
        assert!(dir.join("service.log").exists());
    }

    #[test]
    fn appends_to_existing_file_and_counts_prior_size() {
        let dir = temp_dir("append");
        fs::create_dir_all(&dir).expect("建目录");
        fs::write(dir.join("service.log"), "旧内容\n").expect("预置");
        let log = ServiceLog::open(&dir, Limits::default());
        assert!(log.write_at(at(0), Level::Info, "新内容"));
        let text = read(log.path());
        assert!(text.starts_with("旧内容\n"), "必须追加而不是覆盖：{text}");
        assert!(text.contains("新内容"));
    }

    // --- 轮转 ---

    #[test]
    fn rotates_at_threshold_and_keeps_five_files() {
        let dir = temp_dir("rotate");
        // 一行约 40 字节：阈值 100 → 每 2~3 行轮转一次。
        let log = ServiceLog::open(
            &dir,
            Limits {
                max_bytes: 100,
                keep: 5,
            },
        );
        for index in 0..40 {
            assert!(
                log.write_at(at(1_700_000_000), Level::Info, &format!("第 {index} 行")),
                "第 {index} 行必须写成功"
            );
        }
        for name in [
            "service.log",
            "service.1.log",
            "service.2.log",
            "service.3.log",
            "service.4.log",
        ] {
            assert!(dir.join(name).exists(), "缺 {name}");
        }
        assert!(!dir.join("service.5.log").exists(), "只保留 5 份");
        // 当前文件不超过阈值（写入前判定，单行超限才会略超）。
        let current = fs::metadata(dir.join("service.log")).expect("当前文件");
        assert!(current.len() <= 100, "当前文件超限：{} 字节", current.len());
        // 最新的行在 service.log，最旧的内容已经被挤掉。
        assert!(read(&dir.join("service.log")).contains("第 39 行"));
        let oldest = read(&dir.join("service.4.log"));
        assert!(!oldest.is_empty(), "第 5 份不该是空的");
        assert!(
            !oldest.contains("第 0 行"),
            "最旧的内容必须被丢弃：{oldest}"
        );
    }

    #[test]
    fn rotation_preserves_every_line_in_order() {
        let dir = temp_dir("rotate-order");
        let log = ServiceLog::open(
            &dir,
            Limits {
                max_bytes: 80,
                keep: 5,
            },
        );
        for index in 0..12 {
            assert!(log.write_at(at(1_700_000_000), Level::Info, &format!("L{index}")));
        }
        // 从最旧到最新拼起来，应恰好是最近 5 份里的内容，且行号递增。
        let mut seen = Vec::new();
        for slot in (1..5).rev() {
            for line in read(&dir.join(format!("service.{slot}.log"))).lines() {
                if let Some(rest) = line.split(" INFO L").nth(1) {
                    seen.push(rest.parse::<u32>().expect("行号"));
                }
            }
        }
        for line in read(&dir.join("service.log")).lines() {
            if let Some(rest) = line.split(" INFO L").nth(1) {
                seen.push(rest.parse::<u32>().expect("行号"));
            }
        }
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "{seen:?}");
        assert_eq!(seen.last().copied(), Some(11), "最新一行必须是最后写的");
    }

    // --- 降级 ---

    #[test]
    fn degrades_without_panicking_when_open_fails() {
        let dir = temp_dir("degrade");
        fs::create_dir_all(&dir).expect("建目录");
        // 用同名文件占住 `logs` 目录 → `create_dir_all` 必失败。
        let blocked = dir.join("logs");
        fs::write(&blocked, b"not a directory").expect("占位");

        let log = ServiceLog::open(&blocked, Limits::default());
        assert!(
            !log.write_at(at(0), Level::Info, "写不进去"),
            "该返回 false"
        );
        assert!(log.is_degraded(), "应处于降级态");
        assert_eq!(log.warnings(), 1, "第一次失败要告警一次");
        // 后续写入不再刷屏，但也不 panic。
        for _ in 0..5 {
            assert!(!log.write(Level::Warn, "还是不行"));
        }
        assert_eq!(log.warnings(), 1, "只告警一次");
    }

    #[test]
    fn write_returns_false_but_never_panics_when_disabled() {
        let log = ServiceLog::open_service_plane();
        // 环境里有没有 ProgramData 都不重要：这一行要么落盘、要么降级，都不许炸。
        let _ = log.write(Level::Info, "健康检查 token:hidden");
        assert_eq!(log.limits(), Limits::default());
        assert!(log.path().ends_with("service.log"));
    }

    #[test]
    fn noop_log_accepts_everything() {
        NOOP.record(Level::Error, "什么都别记");
        let log = NoopLog;
        log.record(Level::Info, "静默");
    }
}
