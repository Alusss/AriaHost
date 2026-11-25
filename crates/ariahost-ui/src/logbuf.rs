//! 内部日志环形缓冲（EG-9：日志缓冲必须有上限，本切片取 500 行）。
//!
//! **面板日志的落盘唯一入口就在这里**（M3-m / task-58 之前，面板日志只在内存里，
//! `--diagnose` 的诊断包里因此没有面板行为，见 `docs/dev/known-limitations.md` 的 L2）。
//!
//! 两条通道、一次调用：
//! 1. 内存环形缓冲（有界 500 行）——面板左下角可见，供现场「刚才发生了什么」；
//! 2. `arihost_common::logfile`（10MiB×5 轮转、写入前自动脱敏）——供 `--diagnose` 打包。
//!
//! 为什么不每帧写盘：这里的 `push`/[`LogBuffer::push_level`] 只在**产生事件**时被调用
//! （按钮点击、状态迁移、命令结果），渲染循环里没有任何调用点；`docs/dev/build.md`
//! 「面板日志落盘」一节记着这条约束，改渲染代码时别破坏它。
//!
//! 写盘失败对 UI **无影响**：`logfile::write` 的公共签名不返回结果（它是
//! 「调用即安全、不 panic、不阻塞」的契约），失败由 `logfile` 内部 `warn_once`
//! 往 stderr 记一次（`[ariahost.log] 写入失败…`），面板侧无从得知、也不该假装知道。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ariahost_common::logfile::{self, Level};

/// 环形缓冲保留的最大行数（EG-9 要求有界，本切片取 500 行）。
pub const CAPACITY: usize = 500;

/// 落盘 target：面板行为（短、稳定、不含可变数据；`logfile::MAX_TARGET_LEN = 24`）。
pub const TARGET_PANEL: &str = "panel";

/// 落盘 target：渲染后端探测轨迹（排障最需要的那段，见 task-57/58）。
pub const TARGET_PROBE: &str = "panel.probe";

/// 线程安全的有界日志缓冲：写入时丢弃最旧行，读取返回一份快照。
pub struct LogBuffer {
    lines: Mutex<VecDeque<String>>,
    start: Instant,
}

impl LogBuffer {
    /// 新建一个空缓冲（时间戳从此刻起算）。
    pub fn new() -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            start: Instant::now(),
        }
    }

    /// 追加一行（`Info` 级、target = [`TARGET_PANEL`]）；超过 [`CAPACITY`] 时丢弃最旧的一行。
    pub fn push(&self, line: impl Into<String>) {
        self.push_level(Level::Info, TARGET_PANEL, line);
    }

    /// 追加一行并同步落盘（**面板日志的唯一漏斗**：内存缓冲 + 文件两处一起写）。
    ///
    /// `level` 只影响文件那一行（内存缓冲不显示级别）；`target` 必须是稳定短标识，
    /// 别把 gid/路径/版本塞进去——它要能被 `grep 'panel.probe:'` 稳定命中。
    pub fn push_level(&self, level: Level, target: &str, line: impl Into<String>) {
        let raw = line.into();
        // 先落盘：磁盘保留**未截断**的原文（排障时 4KB 以上的错误链也有价值）。
        logfile::write(level, target, &raw);

        let elapsed = format_elapsed(self.start.elapsed());
        let mut line = raw;
        if line.len() > MAX_LINE_BYTES {
            // 按字符边界截断，避免把 UTF-8 多字节字符切成半个（中文日志会立刻踩到）。
            let mut cut = MAX_LINE_BYTES;
            while cut > 0 && !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
            line.push('…');
        }
        let rendered = format!("[{elapsed}] {line}");
        let mut lines = self
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.len() == CAPACITY {
            lines.pop_front();
        }
        lines.push_back(rendered);
    }

    /// 当前行数（用于测试与面板显示）。
    pub fn len(&self) -> usize {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 取一份快照（面板绘制时用；不持有锁做渲染）。
    pub fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// 单行字节上限（约 4KB，够放一条完整错误链）。
const MAX_LINE_BYTES: usize = 4096;

/// `[  12.345s]` 形式的相对时间戳：面板是启动过程日志，相对时间比墙钟更好读。
fn format_elapsed(elapsed: Duration) -> String {
    format!("{:>9.3}s", elapsed.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_at_most_capacity_lines() {
        let log = LogBuffer::new();
        for index in 0..(CAPACITY + 10) {
            log.push(format!("第 {index} 行"));
        }
        assert_eq!(log.len(), CAPACITY);
        let snapshot = log.snapshot();
        // 最旧的 10 行被丢掉，第一条应该是「第 10 行」。
        assert!(
            snapshot
                .first()
                .is_some_and(|line| line.contains("第 10 行")),
            "{:?}",
            snapshot.first()
        );
        assert!(
            snapshot
                .last()
                .is_some_and(|line| line.contains("第 509 行")),
            "{:?}",
            snapshot.last()
        );
    }

    #[test]
    fn truncates_overlong_line_on_char_boundary() {
        let log = LogBuffer::new();
        log.push("中".repeat(MAX_LINE_BYTES));
        let snapshot = log.snapshot();
        assert_eq!(snapshot.len(), 1);
        // 截断后仍是合法 UTF-8（能取到字符串即说明没切坏），且带省略号。
        assert!(snapshot[0].ends_with('…'));
        assert!(snapshot[0].chars().count() < MAX_LINE_BYTES);
    }

    #[test]
    fn empty_buffer_starts_clean() {
        let log = LogBuffer::new();
        assert!(log.is_empty());
        assert_eq!(log.len(), 0);
    }

    /// 文件日志的写入器是**进程级** `OnceLock`（`logfile::init_at` 只生效一次），
    /// 所以本模块的落盘测试共用一个临时目录；各测试用唯一 tag 行断言，互不干扰。
    fn disk_dir() -> &'static std::path::Path {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let dir = std::env::temp_dir().join("ariahost-ui-logbuf-disk");
            let _ = std::fs::remove_dir_all(&dir);
            logfile::init_at(dir.clone(), logfile::Limits::default());
            dir
        })
    }

    fn disk_text() -> String {
        let path = logfile::current_path().expect("init_at 之后必有路径");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn push_level_writes_a_line_to_disk_with_target() {
        disk_dir();
        let log = LogBuffer::new();
        log.push_level(Level::Info, TARGET_PROBE, "probe-disk-断言行-A");
        let text = disk_text();
        assert!(
            text.contains("probe-disk-断言行-A"),
            "落盘内容里没有这条日志：{text}"
        );
        // target 稳定可 grep（`logfile::format_line` 的格式是 `时间 级别 target: message`）。
        assert!(text.contains("panel.probe:"), "target 没写进日志行：{text}");
        // 内存那一份同样在（两条通道都写）。
        assert!(log.snapshot()[0].contains("probe-disk-断言行-A"));
    }

    #[test]
    fn disk_line_is_redacted_before_write() {
        disk_dir();
        let log = LogBuffer::new();
        // 假凭据：`token:` 是 `common::redact` 认的形态之一。落盘必须是 `***`。
        //
        // 注意形态：脱敏规则是「`token:` 后面**紧跟**的值」，值遇空格即止。所以
        // `token: 值`（带空格）不会命中——面板侧因此别把凭据写成「键 + 空格 + 值」，
        // 要写就写 `token=值` / `token:值`。这条限制来自 `common::redact`（不在本 crate
        // 的写作用域），已作为发现上报。
        log.push_level(
            Level::Warn,
            TARGET_PANEL,
            "redact-断言行-B token:LEAK-SK-SHOULD-NOT-APPEAR",
        );
        let text = disk_text();
        assert!(
            text.contains("redact-断言行-B"),
            "落盘内容里没有这条日志：{text}"
        );
        assert!(
            !text.contains("LEAK-SK-SHOULD-NOT-APPEAR"),
            "假凭据被原样写进日志了：{text}"
        );
        assert!(text.contains("***"), "没有看到脱敏占位符：{text}");
    }
}
