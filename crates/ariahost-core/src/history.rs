//! 任务历史持久化（M2-8）：把「已完成/失败/已移除」的下载记录落到磁盘。
//!
//! ## 为什么需要
//!
//! aria2 的 `tellStopped` 只保留内存里最近 `--max-download-result`（默认 1000）条，
//! **面板一重启就全没了**。需求要求「下载进度、配置缓存等要合理管理」，验收里也有
//! 「面板重启后仍能看到已完成任务」。这里补的就是这一条。
//!
//! ## 存储
//!
//! `%APPDATA%\AriaHost\history\downloads.jsonl`（用户平面）/
//! `%ProgramData%\AriaHost\history\downloads.jsonl`（服务平面）。
//!
//! **JSONL，一行一条，追加写为主**。不每次全量重写——几千条全量写会把 UI 卡住。
//! 只有轮转（超过 [`MAX_ENTRIES`] 条）时才重写一次。
//!
//! ## 记录 schema
//!
//! | 字段 | 类型 | 说明 |
//! |---|---|---|
//! | `gid` | string | aria2 任务 id（同 gid 后写覆盖先写） |
//! | `name` | string | 文件名（UTF-8 原样往返） |
//! | `dir` | string | 保存目录 |
//! | `uri` | string | **落盘前已脱敏**（`token:`/`token=`/`rpc-secret=`/`urn:btih:`/URL 密码） |
//! | `total_bytes` / `completed_bytes` | u64 | 大小 |
//! | `status` | `complete` / `error` / `removed` | 终态 |
//! | `error_code` | string? | `AH-E####` 或 aria2 的 errorCode；成功时不写 |
//! | `finished_at` | string | UTC RFC3339；**固定格式，可按字符串排序** |
//! | `source` | `user` / `service` | 哪个平面写的 |
//!
//! `finished_at` 由调用方给（本模块不再加第三份时间格式化实现；排序只依赖
//! 「RFC3339 UTC 定长字符串」这个性质）。
//!
//! ## 幂等
//!
//! 同一 gid 可以被记录多次（例如先 `complete` 后补一次 `removed`）。**读时以文件里
//! 最后一条为准**，不会出现两条「完成」。所以 `record` 永远只追加，不做原地改。
//!
//! ## 轮转
//!
//! 行数超过上限（默认 [`MAX_ENTRIES`] 5000，常量、不开配置键）时：读回全部记录 →
//! 取最近 5000 条 → 写临时文件 → 旧文件改名成 `downloads.jsonl.1`（上一代，只留一份）→
//! 临时文件改名成正式文件。轮转失败**不影响刚写入的那条记录**，只把失败原因放在
//! [`RecordOutcome::rotation_error`] 里（历史会暂时超过上限，下次再试）。
//!
//! ## 损坏容忍（本批最容易漏的点）
//!
//! 掉电/半行/非法 JSON/非法 UTF-8/空行**一律跳过该行继续读**，不整体失败；条数记在
//! [`LoadOutcome::skipped_lines`]。另外追加前会检查文件末尾是否有换行，没有就先补一个
//! ——否则新记录会和断掉的半行粘成一条坏记录，等于把新数据也弄丢。
//!
//! ## 并发：只支持单进程
//!
//! 面板进程独占这份文件。**多进程并发写未处理**：两个进程同时轮转会互相覆盖 `.1`。
//! 不加锁文件（不想为它引依赖，也没有可靠的跨平台 flock）；真要支持并发，得先定
//! 「谁写历史」这一条所有权规则。
//!
//! ## 脱敏归属
//!
//! 规则落在 [`ariah​ost_common::redact`]（`common`），服务日志与任务历史共用**同一份**实现。
//! 本模块在落盘前**再脱一次**（调用方忘了脱也兜住）。

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ariahost_common::error::ErrorCode;
use ariahost_common::paths::{self, Plane};
use ariahost_common::redact::redact_uri;
use serde::{Deserialize, Serialize};

/// 历史文件名。
pub const HISTORY_FILE: &str = "downloads.jsonl";

/// 上一代文件名（轮转产生的唯一一份备份）。
pub const HISTORY_ROTATED: &str = "downloads.jsonl.1";

/// 轮转时用的临时文件名。
pub const HISTORY_TMP: &str = "downloads.jsonl.tmp";

/// 默认保留条数。**常量，不开配置键**（与 `clean.tmp_hours` 同批将来处理）。
pub const MAX_ENTRIES: usize = 5000;

/// 「行数未知」的哨兵值。
const UNKNOWN_LINES: usize = usize::MAX;

// ---------------------------------------------------------------------------
// 记录
// ---------------------------------------------------------------------------

/// 任务终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    /// 下载完成。
    Complete,
    /// 下载失败。
    Error,
    /// 被移除（用户删任务 / 队列清理）。
    Removed,
}

impl HistoryStatus {
    /// JSON 里的字面量（写盘/比对共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Error => "error",
            Self::Removed => "removed",
        }
    }

    /// 中文短标签（UI 用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Complete => "已完成",
            Self::Error => "失败",
            Self::Removed => "已移除",
        }
    }
}

/// 记录来源平面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// 面板/无头直跑（用户平面）。
    User,
    /// 服务模式（服务平面）。
    Service,
}

impl HistorySource {
    /// 对应平面。
    pub fn plane(self) -> Plane {
        match self {
            Self::User => Plane::User,
            Self::Service => Plane::Service,
        }
    }
}

/// 一条任务历史记录（一行 JSONL）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// aria2 任务 id。
    pub gid: String,
    /// 文件名。
    #[serde(default)]
    pub name: String,
    /// 保存目录。
    #[serde(default)]
    pub dir: String,
    /// 源 URI（**落盘前脱敏**）。
    #[serde(default)]
    pub uri: String,
    /// 文件总大小。
    #[serde(default)]
    pub total_bytes: u64,
    /// 已完成字节。
    #[serde(default)]
    pub completed_bytes: u64,
    /// 终态。
    pub status: HistoryStatus,
    /// 错误码（`AH-E####` 或 aria2 的 errorCode）；成功时不写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// 结束时间（UTC RFC3339，定长可排序）。
    pub finished_at: String,
    /// 哪个平面写的。
    pub source: HistorySource,
}

impl HistoryEntry {
    /// 最小构造：其余字段用默认值/构建器补。
    pub fn new(
        gid: impl Into<String>,
        name: impl Into<String>,
        dir: impl Into<String>,
        uri: impl Into<String>,
        status: HistoryStatus,
        finished_at: impl Into<String>,
    ) -> Self {
        Self {
            gid: gid.into(),
            name: name.into(),
            dir: dir.into(),
            uri: uri.into(),
            total_bytes: 0,
            completed_bytes: 0,
            status,
            error_code: None,
            finished_at: finished_at.into(),
            source: HistorySource::User,
        }
    }

    /// 补大小。
    pub fn with_bytes(mut self, total: u64, completed: u64) -> Self {
        self.total_bytes = total;
        self.completed_bytes = completed;
        self
    }

    /// 补错误码。
    pub fn with_error_code(mut self, code: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self
    }

    /// 补来源平面。
    pub fn with_source(mut self, source: HistorySource) -> Self {
        self.source = source;
        self
    }
}

/// 读取结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadOutcome {
    /// 记录（按 `finished_at` 倒序、同 gid 已去重取最后一条）。
    pub entries: Vec<HistoryEntry>,
    /// 跳过的坏行数（半行、非法 JSON、非法 UTF-8、空行）。
    pub skipped_lines: usize,
}

/// 写入结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordOutcome {
    /// 写入后文件里的记录条数（轮转后是保留条数）。
    pub total_lines: usize,
    /// 是否触发了轮转。
    pub rotated: bool,
    /// 轮转失败的原因（记录本身已写成功；历史会暂时超过上限）。
    pub rotation_error: Option<String>,
}

/// 统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryStats {
    /// 记录总数（同 gid 只算一次）。
    pub total: usize,
    /// 完成数。
    pub completed: usize,
    /// 失败数。
    pub failed: usize,
    /// 已移除数。
    pub removed: usize,
    /// 这些记录声明的文件总大小之和。
    pub total_bytes: u64,
    /// 读取时跳过的坏行数。
    pub skipped_lines: usize,
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 历史文件操作的错误。
#[derive(Debug)]
pub enum HistoryError {
    /// 文件读写失败。
    Io {
        /// 哪一步。
        action: &'static str,
        /// 出问题的路径。
        path: PathBuf,
        /// 底层错误。
        error: io::Error,
    },
    /// 序列化失败（理论上不会发生：结构都是可序列化的）。
    Serialize {
        /// 哪条记录。
        gid: String,
        /// 底层错误。
        error: serde_json::Error,
    },
}

impl HistoryError {
    /// 对应错误码：历史文件与 `config.toml` 同在用户数据目录，IO 失败归 `AH-E1003`。
    pub fn code(&self) -> ErrorCode {
        ErrorCode::ConfigIoFailed
    }

    /// 给用户看的一句话。
    pub fn message(&self) -> String {
        match self {
            Self::Io {
                action,
                path,
                error,
            } => format!(
                "{} {action}失败：{}（{error}）",
                self.code().code(),
                path.display()
            ),
            Self::Serialize { gid, error } => format!(
                "{} 历史记录 {gid} 序列化失败（{error}）",
                self.code().code()
            ),
        }
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::Serialize { error, .. } => Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// 存储
// ---------------------------------------------------------------------------

/// 一份历史文件（一个平面一份）。
#[derive(Debug)]
pub struct History {
    path: PathBuf,
    max_entries: usize,
    cached_lines: AtomicUsize,
}

impl History {
    /// 用默认上限构造。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_max_entries(path, MAX_ENTRIES)
    }

    /// 指定上限（**只给单测用**；生产路径用 [`MAX_ENTRIES`]）。
    pub fn with_max_entries(path: impl Into<PathBuf>, max_entries: usize) -> Self {
        Self {
            path: path.into(),
            max_entries: max_entries.max(1),
            cached_lines: AtomicUsize::new(UNKNOWN_LINES),
        }
    }

    /// 按平面取标准路径：`<数据根>\history\downloads.jsonl`。
    pub fn default_path(plane: Plane) -> Option<PathBuf> {
        match plane {
            Plane::User => paths::config_file()?
                .parent()
                .map(|root| root.join("history").join(HISTORY_FILE)),
            Plane::Service => {
                paths::service_runtime_dir().map(|root| root.join("history").join(HISTORY_FILE))
            }
        }
    }

    /// 按平面构造（拿不到环境变量时为 `None`）。
    pub fn for_plane(plane: Plane) -> Option<Self> {
        Self::default_path(plane).map(Self::new)
    }

    /// 历史文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 保留上限。
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// 追加一条记录（同 gid 再去一条，读时后者胜）。
    ///
    /// 只有「追加本身」失败才返回 `Err`；轮转失败放在
    /// [`RecordOutcome::rotation_error`] 里，不影响这条记录已落盘的事实。
    pub fn record(&self, entry: &HistoryEntry) -> Result<RecordOutcome, HistoryError> {
        let mut safe = entry.clone();
        // 兜底再脱一次：调用方忘了脱敏也不该把 token 写进磁盘。
        safe.uri = redact_uri(&entry.uri);

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| self.io_error("创建历史目录", parent.to_path_buf(), error))?;
        }

        let before = self.line_count()?;
        let line = serde_json::to_string(&safe).map_err(|error| HistoryError::Serialize {
            gid: safe.gid.clone(),
            error,
        })?;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');

        let mut file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.path)
            .map_err(|error| self.io_error("打开历史文件", self.path.clone(), error))?;
        // 掉电可能留下没有换行的半行；不补换行的话新记录会和它粘成一条坏记录。
        self.heal_trailing_newline(&mut file)?;
        file.write_all(&bytes)
            .map_err(|error| self.io_error("追加历史记录", self.path.clone(), error))?;
        drop(file);

        let total = before.saturating_add(1);
        self.cached_lines.store(total, Ordering::Relaxed);
        let mut outcome = RecordOutcome {
            total_lines: total,
            rotated: false,
            rotation_error: None,
        };
        if total > self.max_entries {
            match self.rotate() {
                Ok(kept) => {
                    outcome.rotated = true;
                    outcome.total_lines = kept;
                }
                Err(error) => outcome.rotation_error = Some(error.to_string()),
            }
        }
        Ok(outcome)
    }

    /// 读全部记录（已去重、按 `finished_at` 倒序）。
    pub fn load(&self) -> Result<LoadOutcome, HistoryError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::default())
            }
            Err(error) => return Err(self.io_error("打开历史文件", self.path.clone(), error)),
        };
        let mut reader = BufReader::new(file);
        let mut buf: Vec<u8> = Vec::new();
        let mut skipped = 0usize;
        let mut by_gid: BTreeMap<String, HistoryEntry> = BTreeMap::new();

        loop {
            buf.clear();
            let read = reader
                .read_until(b'\n', &mut buf)
                .map_err(|error| self.io_error("读取历史文件", self.path.clone(), error))?;
            if read == 0 {
                break;
            }
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            if buf.is_empty() {
                skipped += 1;
                continue;
            }
            // 非法 UTF-8 也不能整体失败。
            let Ok(text) = std::str::from_utf8(&buf) else {
                skipped += 1;
                continue;
            };
            match serde_json::from_str::<HistoryEntry>(text) {
                // 文件里最后一条同 gid 的胜出（幂等语义）。
                Ok(entry) => {
                    by_gid.insert(entry.gid.clone(), entry);
                }
                Err(_) => skipped += 1,
            }
        }

        let mut entries: Vec<HistoryEntry> = by_gid.into_values().collect();
        entries.sort_by(|left, right| {
            right
                .finished_at
                .cmp(&left.finished_at)
                .then_with(|| left.gid.cmp(&right.gid))
        });
        Ok(LoadOutcome {
            entries,
            skipped_lines: skipped,
        })
    }

    /// 最近 `limit` 条（按 `finished_at` 倒序）。
    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryEntry>, HistoryError> {
        let outcome = self.load()?;
        Ok(outcome.entries.into_iter().take(limit).collect())
    }

    /// 按 gid 找一条。
    pub fn find(&self, gid: &str) -> Result<Option<HistoryEntry>, HistoryError> {
        let outcome = self.load()?;
        Ok(outcome.entries.into_iter().find(|entry| entry.gid == gid))
    }

    /// 统计。
    pub fn stats(&self) -> Result<HistoryStats, HistoryError> {
        let LoadOutcome {
            entries,
            skipped_lines,
        } = self.load()?;
        let mut stats = HistoryStats {
            total: entries.len(),
            skipped_lines,
            ..HistoryStats::default()
        };
        for entry in &entries {
            match entry.status {
                HistoryStatus::Complete => stats.completed += 1,
                HistoryStatus::Error => stats.failed += 1,
                HistoryStatus::Removed => stats.removed += 1,
            }
            stats.total_bytes = stats.total_bytes.saturating_add(entry.total_bytes);
        }
        Ok(stats)
    }

    /// 轮转到最近 `max_entries` 条，并把上一代留成 `downloads.jsonl.1`。
    fn rotate(&self) -> Result<usize, HistoryError> {
        let LoadOutcome { entries, .. } = self.load()?;
        let keep = entries.len().min(self.max_entries);
        let kept = &entries[..keep];

        let tmp = self.path.with_file_name(HISTORY_TMP);
        {
            let mut file = File::create(&tmp)
                .map_err(|error| self.io_error("创建轮转临时文件", tmp.clone(), error))?;
            // 按 finished_at **升序**写：文件里「最后一条同 gid」才是最新的。
            for entry in kept.iter().rev() {
                let line =
                    serde_json::to_string(entry).map_err(|error| HistoryError::Serialize {
                        gid: entry.gid.clone(),
                        error,
                    })?;
                file.write_all(line.as_bytes())
                    .map_err(|error| self.io_error("写轮转临时文件", tmp.clone(), error))?;
                file.write_all(b"\n")
                    .map_err(|error| self.io_error("写轮转临时文件", tmp.clone(), error))?;
            }
            file.sync_all()
                .map_err(|error| self.io_error("落盘轮转临时文件", tmp.clone(), error))?;
        }

        let rotated = self.path.with_file_name(HISTORY_ROTATED);
        if self.path.exists() {
            // Windows 上 rename 到已存在的文件会失败，先删目标。
            if rotated.exists() {
                fs::remove_file(&rotated)
                    .map_err(|error| self.io_error("删除上一代历史", rotated.clone(), error))?;
            }
            fs::rename(&self.path, &rotated)
                .map_err(|error| self.io_error("历史文件改名为上一代", rotated.clone(), error))?;
        }
        fs::rename(&tmp, &self.path)
            .map_err(|error| self.io_error("轮转后落位历史文件", self.path.clone(), error))?;

        self.cached_lines.store(keep, Ordering::Relaxed);
        Ok(keep)
    }

    /// 行数（首次访问扫描一遍并缓存；同一进程内后续只累加）。
    fn line_count(&self) -> Result<usize, HistoryError> {
        let cached = self.cached_lines.load(Ordering::Relaxed);
        if cached != UNKNOWN_LINES {
            return Ok(cached);
        }
        let count = self.count_lines_on_disk()?;
        self.cached_lines.store(count, Ordering::Relaxed);
        Ok(count)
    }

    fn count_lines_on_disk(&self) -> Result<usize, HistoryError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(self.io_error("打开历史文件", self.path.clone(), error)),
        };
        let mut reader = BufReader::new(file);
        let mut buf: Vec<u8> = Vec::new();
        let mut lines = 0usize;
        loop {
            buf.clear();
            let read = reader
                .read_until(b'\n', &mut buf)
                .map_err(|error| self.io_error("统计历史行数", self.path.clone(), error))?;
            if read == 0 {
                break;
            }
            lines += 1;
        }
        Ok(lines)
    }

    fn heal_trailing_newline(&self, file: &mut File) -> Result<(), HistoryError> {
        let len = file
            .metadata()
            .map_err(|error| self.io_error("读取历史文件大小", self.path.clone(), error))?
            .len();
        if len == 0 {
            return Ok(());
        }
        file.seek(SeekFrom::End(-1))
            .map_err(|error| self.io_error("定位历史文件末尾", self.path.clone(), error))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)
            .map_err(|error| self.io_error("读取历史文件末字节", self.path.clone(), error))?;
        if last[0] != b'\n' {
            file.write_all(b"\n")
                .map_err(|error| self.io_error("补齐历史文件换行", self.path.clone(), error))?;
        }
        Ok(())
    }

    fn io_error(&self, action: &'static str, path: PathBuf, error: io::Error) -> HistoryError {
        HistoryError::Io {
            action,
            path,
            error,
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ariahost-history-{tag}-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("建临时目录");
            Self { root }
        }

        fn history(&self) -> History {
            History::new(self.root.join(HISTORY_FILE))
        }

        fn history_with_cap(&self, cap: usize) -> History {
            History::with_max_entries(self.root.join(HISTORY_FILE), cap)
        }

        fn file(&self) -> PathBuf {
            self.root.join(HISTORY_FILE)
        }

        fn rotated(&self) -> PathBuf {
            self.root.join(HISTORY_ROTATED)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn entry(gid: &str, status: HistoryStatus, finished_at: &str) -> HistoryEntry {
        HistoryEntry::new(
            gid,
            format!("{gid}.zip"),
            "D:/Downloads",
            format!("https://example.com/{gid}.zip"),
            status,
            finished_at,
        )
        .with_bytes(1000, 1000)
    }

    // ---- 正常往返 ----

    #[test]
    fn record_then_load_roundtrip() {
        let temp = TempDir::new("roundtrip");
        let history = temp.history();
        let mut item = entry("g1", HistoryStatus::Complete, "2026-11-25T01:00:00Z");
        item.name = "报告（中文）.zip".to_owned();

        let outcome = history.record(&item).expect("写");
        assert_eq!(outcome.total_lines, 1);
        assert!(!outcome.rotated);
        assert!(outcome.rotation_error.is_none());

        let loaded = history.load().expect("读");
        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(loaded.entries, vec![item]);
    }

    #[test]
    fn utf8_filename_roundtrips() {
        let temp = TempDir::new("utf8");
        let history = temp.history();
        let item = HistoryEntry::new(
            "g-utf8",
            "架构设计（v2.6）—最终版.zip",
            "D:/下载/项目",
            "https://example.com/a.zip",
            HistoryStatus::Complete,
            "2026-11-25T02:00:00Z",
        );
        history.record(&item).expect("写");
        let loaded = history.recent(10).expect("读");
        assert_eq!(loaded[0].name, item.name);
        assert_eq!(loaded[0].dir, item.dir);
    }

    #[test]
    fn append_does_not_rewrite_existing_bytes() {
        let temp = TempDir::new("append");
        let history = temp.history();
        history
            .record(&entry(
                "g1",
                HistoryStatus::Complete,
                "2026-11-25T01:00:00Z",
            ))
            .expect("写 1");
        let first_len = fs::metadata(temp.file()).expect("stat").len() as usize;
        let first_bytes = fs::read(temp.file()).expect("读原始");

        history
            .record(&entry(
                "g2",
                HistoryStatus::Complete,
                "2026-11-25T02:00:00Z",
            ))
            .expect("写 2");
        let all = fs::read(temp.file()).expect("读原始");
        assert!(all.len() > first_len, "只该追加");
        assert_eq!(
            &all[..first_bytes.len()],
            &first_bytes[..],
            "旧字节不得被改写"
        );
    }

    // ---- 幂等 ----

    #[test]
    fn same_gid_last_record_wins() {
        let temp = TempDir::new("idempotent");
        let history = temp.history();
        history
            .record(&entry(
                "g1",
                HistoryStatus::Complete,
                "2026-11-25T01:00:00Z",
            ))
            .expect("写完成");
        history
            .record(&entry("g1", HistoryStatus::Removed, "2026-11-25T03:00:00Z"))
            .expect("写移除");

        let loaded = history.load().expect("读");
        assert_eq!(loaded.entries.len(), 1, "同 gid 只该有一条");
        assert_eq!(loaded.entries[0].status, HistoryStatus::Removed);
        assert_eq!(loaded.entries[0].finished_at, "2026-11-25T03:00:00Z");

        let stats = history.stats().expect("统计");
        assert_eq!(stats.total, 1);
        assert_eq!(stats.completed, 0);
        assert_eq!(stats.removed, 1);
    }

    // ---- 轮转 ----

    #[test]
    fn exceeding_cap_rotates_and_keeps_backup() {
        let temp = TempDir::new("rotate");
        let history = temp.history_with_cap(5);
        let mut last = RecordOutcome::default();
        // 写 6 条：前 5 条不轮转，第 6 条写入后 6 > 5 → 轮转一次。
        for index in 0..6 {
            let finished = format!("2026-11-25T{index:02}:00:00Z");
            last = history
                .record(&entry(
                    &format!("g{index}"),
                    HistoryStatus::Complete,
                    &finished,
                ))
                .expect("写");
            if index < 5 {
                assert!(!last.rotated, "第 {index} 条不该轮转");
            }
        }
        assert!(last.rotated, "第 6 条写入后应轮转");
        assert!(last.rotation_error.is_none(), "{:?}", last.rotation_error);
        assert_eq!(last.total_lines, 5, "轮转后应只剩 5 条");

        let loaded = history.load().expect("读");
        let gids: Vec<&str> = loaded
            .entries
            .iter()
            .map(|item| item.gid.as_str())
            .collect();
        assert_eq!(gids, vec!["g5", "g4", "g3", "g2", "g1"], "保留最新 5 条");

        assert!(temp.rotated().exists(), "必须留一份 downloads.jsonl.1");
        let backup = fs::read_to_string(temp.rotated()).expect("读备份");
        assert!(
            backup.contains("\"gid\":\"g0\""),
            "备份应是轮转前的全量（含被挤掉的 g0）"
        );
        assert!(backup.contains("\"gid\":\"g5\""));
    }

    #[test]
    fn rotation_keeps_finished_at_order_for_later_reads() {
        let temp = TempDir::new("rotate-order");
        let history = temp.history_with_cap(3);
        // 故意乱序写入：后写的 finished_at 更早。轮转后仍应按 finished_at 排序，
        // 且同 gid 的覆盖语义不能被轮转破坏。
        for (gid, finished) in [
            ("a", "2026-11-25T05:00:00Z"),
            ("b", "2026-11-25T01:00:00Z"),
            ("c", "2026-11-25T04:00:00Z"),
            ("d", "2026-11-25T03:00:00Z"),
        ] {
            history
                .record(&entry(gid, HistoryStatus::Complete, finished))
                .expect("写");
        }
        let loaded = history.load().expect("读");
        assert_eq!(loaded.entries.len(), 3);
        let gids: Vec<&str> = loaded
            .entries
            .iter()
            .map(|item| item.gid.as_str())
            .collect();
        assert_eq!(gids, vec!["a", "c", "d"], "保留 finished_at 最新的三条");
    }

    // ---- 损坏容忍 ----

    #[test]
    fn bad_lines_are_skipped_not_fatal() {
        let temp = TempDir::new("badlines");
        let good = serde_json::to_string(&entry(
            "g-good",
            HistoryStatus::Complete,
            "2026-11-25T01:00:00Z",
        ))
        .expect("序列化");
        let raw = format!(
            "{good}\n\
             半行没有结尾\n\
             {{不是合法 JSON}}\n\
             \n\
             {good}\n"
        );
        fs::write(temp.file(), raw.as_bytes()).expect("写坏文件");

        let history = temp.history();
        let loaded = history.load().expect("坏行不该让整体失败");
        // 半行 + 非法 JSON + 空行 = 3 条坏行
        assert_eq!(loaded.skipped_lines, 3);
        // 两条合法行是同 gid → 去重后 1 条
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].gid, "g-good");
    }

    #[test]
    fn invalid_utf8_line_is_skipped() {
        let temp = TempDir::new("utf8bad");
        let good = serde_json::to_string(&entry(
            "g-ok",
            HistoryStatus::Complete,
            "2026-11-25T01:00:00Z",
        ))
        .expect("序列化");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\xff\xfe\x00broken\n");
        bytes.extend_from_slice(good.as_bytes());
        bytes.push(b'\n');
        fs::write(temp.file(), &bytes).expect("写坏文件");

        let loaded = temp.history().load().expect("读");
        assert_eq!(loaded.skipped_lines, 1);
        assert_eq!(loaded.entries.len(), 1);
    }

    #[test]
    fn half_written_tail_does_not_swallow_the_next_record() {
        let temp = TempDir::new("halftail");
        // 模拟掉电：文件以一条断掉的 JSON 结尾，没有换行。
        let first = serde_json::to_string(&entry(
            "g-before",
            HistoryStatus::Complete,
            "2026-11-25T01:00:00Z",
        ))
        .expect("序列化");
        fs::write(temp.file(), format!("{first}\n{{\"gid\":\"g-trunc")).expect("写半行文件");

        let history = temp.history();
        history
            .record(&entry(
                "g-after",
                HistoryStatus::Complete,
                "2026-11-25T02:00:00Z",
            ))
            .expect("写新记录");

        let loaded = history.load().expect("读");
        // 半行被跳过，但新记录必须完整（不能被粘在坏行后面一起坏掉）。
        assert_eq!(loaded.entries.len(), 2, "{:?}", loaded.entries);
        assert!(loaded.entries.iter().any(|item| item.gid == "g-after"));
        assert_eq!(loaded.skipped_lines, 1);
    }

    #[test]
    fn crlf_lines_are_accepted() {
        let temp = TempDir::new("crlf");
        let good = serde_json::to_string(&entry(
            "g-crlf",
            HistoryStatus::Complete,
            "2026-11-25T01:00:00Z",
        ))
        .expect("序列化");
        fs::write(temp.file(), format!("{good}\r\n")).expect("写 CRLF 文件");
        let loaded = temp.history().load().expect("读");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.skipped_lines, 0);
    }

    // ---- 查询 ----

    #[test]
    fn recent_returns_newest_first_and_honours_limit() {
        let temp = TempDir::new("recent");
        let history = temp.history();
        for (gid, finished) in [
            ("a", "2026-11-25T01:00:00Z"),
            ("b", "2026-11-25T03:00:00Z"),
            ("c", "2026-11-25T02:00:00Z"),
        ] {
            history
                .record(&entry(gid, HistoryStatus::Complete, finished))
                .expect("写");
        }
        let recent = history.recent(2).expect("读");
        let gids: Vec<&str> = recent.iter().map(|item| item.gid.as_str()).collect();
        assert_eq!(gids, vec!["b", "c"]);
    }

    #[test]
    fn find_hits_and_misses() {
        let temp = TempDir::new("find");
        let history = temp.history();
        history
            .record(&entry("g1", HistoryStatus::Error, "2026-11-25T01:00:00Z"))
            .expect("写");
        let found = history.find("g1").expect("读").expect("应有");
        assert_eq!(found.status, HistoryStatus::Error);
        assert!(history.find("nope").expect("读").is_none());
    }

    #[test]
    fn stats_counts_statuses_and_bytes() {
        let temp = TempDir::new("stats");
        let history = temp.history();
        history
            .record(
                &entry("done", HistoryStatus::Complete, "2026-11-25T01:00:00Z")
                    .with_bytes(500, 500),
            )
            .expect("写");
        history
            .record(
                &entry("fail", HistoryStatus::Error, "2026-11-25T02:00:00Z").with_bytes(300, 100),
            )
            .expect("写");
        history
            .record(
                &entry("gone", HistoryStatus::Removed, "2026-11-25T03:00:00Z").with_bytes(200, 0),
            )
            .expect("写");

        let stats = history.stats().expect("统计");
        assert_eq!(stats.total, 3);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.total_bytes, 1000);
        assert_eq!(stats.skipped_lines, 0);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let temp = TempDir::new("missing");
        let history = temp.history();
        assert!(history.load().expect("读").entries.is_empty());
        assert!(history.recent(10).expect("读").is_empty());
        assert_eq!(history.stats().expect("统计"), HistoryStats::default());
    }

    // ---- 脱敏 ----

    #[test]
    fn uri_is_redacted_before_hitting_disk() {
        let temp = TempDir::new("redact");
        let history = temp.history();
        let mut item = entry("g-secret", HistoryStatus::Complete, "2026-11-25T01:00:00Z");
        item.uri = "http://alice:hunter2@example.com/a.zip?token=s3cr3t&id=7".to_owned();
        history.record(&item).expect("写");

        let raw = fs::read_to_string(temp.file()).expect("读原始");
        for secret in ["hunter2", "s3cr3t", "alice:hunter2"] {
            assert!(!raw.contains(secret), "磁盘上不该出现 {secret}：{raw}");
        }
        assert!(raw.contains("***"), "应有脱敏占位：{raw}");

        let loaded = history.recent(1).expect("读");
        assert!(loaded[0].uri.contains("token=***"));
        assert!(loaded[0].uri.contains("alice:***@"));
    }

    #[test]
    fn magnet_infohash_is_redacted() {
        let temp = TempDir::new("magnet");
        let history = temp.history();
        let mut item = entry("g-magnet", HistoryStatus::Complete, "2026-11-25T01:00:00Z");
        item.uri = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x".to_owned();
        history.record(&item).expect("写");
        let raw = fs::read_to_string(temp.file()).expect("读原始");
        assert!(!raw.contains("0123456789abcdef"));
        assert!(raw.contains("urn:btih:***"));
    }

    // ---- 其它 ----

    #[test]
    fn source_and_error_code_roundtrip() {
        let temp = TempDir::new("fields");
        let history = temp.history();
        let item = entry("g-svc", HistoryStatus::Error, "2026-11-25T01:00:00Z")
            .with_error_code("AH-E3002")
            .with_source(HistorySource::Service);
        history.record(&item).expect("写");
        let loaded = history.recent(1).expect("读");
        assert_eq!(loaded[0].source, HistorySource::Service);
        assert_eq!(loaded[0].error_code.as_deref(), Some("AH-E3002"));
    }

    #[test]
    fn error_code_is_omitted_when_absent() {
        let temp = TempDir::new("nocode");
        let history = temp.history();
        history
            .record(&entry(
                "g1",
                HistoryStatus::Complete,
                "2026-11-25T01:00:00Z",
            ))
            .expect("写");
        let raw = fs::read_to_string(temp.file()).expect("读原始");
        assert!(!raw.contains("error_code"), "成功记录不该写 error_code");
    }

    #[test]
    fn default_paths_are_per_plane() {
        if let Some(path) = History::default_path(Plane::User) {
            assert!(path.ends_with(Path::new("AriaHost").join("history").join(HISTORY_FILE)));
        }
        if let Some(path) = History::default_path(Plane::Service) {
            assert!(path.ends_with(Path::new("AriaHost").join("history").join(HISTORY_FILE)));
        }
        if let (Some(user), Some(service)) = (
            History::default_path(Plane::User),
            History::default_path(Plane::Service),
        ) {
            assert_ne!(user, service, "两平面的历史文件不能是同一个");
        }
    }

    #[test]
    fn status_labels_are_pinned() {
        assert_eq!(HistoryStatus::Complete.as_str(), "complete");
        assert_eq!(HistoryStatus::Error.label(), "失败");
        assert_eq!(HistoryStatus::Removed.as_str(), "removed");
    }

    /// 手工性能探针：按真实规模（5000 条）量一次追加/读取/统计耗时。
    ///
    /// ```text
    /// cargo test -p ariahost-core history_perf_probe -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "手工性能探针（5000 条），需要 --ignored 才会跑"]
    fn history_perf_probe() {
        use std::time::Instant;

        let temp = TempDir::new("perf");
        let history = temp.history();
        let samples = 5000usize;

        let write_start = Instant::now();
        for index in 0..samples {
            let finished = format!(
                "2026-11-25T{:02}:{:02}:{:02}Z",
                index / 3600,
                (index / 60) % 60,
                index % 60
            );
            history
                .record(
                    &entry(
                        &format!("gid-{index:05}"),
                        HistoryStatus::Complete,
                        &finished,
                    )
                    .with_bytes(1024 * 1024, 1024 * 1024),
                )
                .expect("写");
        }
        let write_elapsed = write_start.elapsed();

        let read_start = Instant::now();
        let loaded = history.load().expect("读");
        let read_elapsed = read_start.elapsed();

        let stats_start = Instant::now();
        let stats = history.stats().expect("统计");
        let stats_elapsed = stats_start.elapsed();

        let bytes = fs::metadata(temp.file()).expect("stat").len();
        println!(
            "HISTORY_PERF writes={samples} write_total={write_elapsed:?} per_write={:?} \
             load={read_elapsed:?} stats={stats_elapsed:?} file_bytes={bytes} entries={} skipped={}",
            write_elapsed / samples as u32,
            loaded.entries.len(),
            loaded.skipped_lines
        );
        assert_eq!(stats.total, samples);
        assert_eq!(loaded.entries.len(), samples);
    }
}
