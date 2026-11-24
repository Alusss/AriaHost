//! 内部日志环形缓冲（EG-9：日志缓冲必须有上限，本切片取 500 行）。
//!
//! 面板不落盘、不引 logger 门面：这里只保证「启动过程可见」与「缓冲有界」两件事，
//! 需要长期日志的落盘策略随 M2-1 的日志模块一起做。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 环形缓冲保留的最大行数（EG-9 要求有界，本切片取 500 行）。
pub const CAPACITY: usize = 500;

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

    /// 追加一行；超过 [`CAPACITY`] 时丢弃最旧的一行。
    pub fn push(&self, line: impl Into<String>) {
        let elapsed = format_elapsed(self.start.elapsed());
        let mut line = line.into();
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
}
