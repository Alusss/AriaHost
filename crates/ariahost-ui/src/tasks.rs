//! 任务列表模型（M2）：aria2 `tell*` 的 JSON → 可直接上屏的行。
//!
//! ## 为什么要这一层
//!
//! EG-9 要求「200 行不卡、不每帧 clone」。做法是**把格式化全部提到 worker 线程**：
//! RPC 回来的 `Value` 在这里一次性变成 [`TaskRow`]，所有字符串字段都是 `Arc<str>`，
//! UI 每帧只是复制若干 `Arc` 指针（8 字节），不做 `format!`、不做 `to_owned`。
//!
//! ## aria2 的字段约定（易错点）
//!
//! `aria2.tellStatus` / `tellActive` / `tellWaiting` / `tellStopped` 返回的数值字段
//! **全是字符串**（`"totalLength": "1048576"`），`errorCode` 也是字符串。
//! 这里两种都容忍（字符串与数字），统一用 [`u64_of`] 读。
//!
//! 名称优先取 `bittorrent.info.name`，其次 `files[0].path` 的文件名，再次 `files[0].uris[0].uri`；
//! 磁力「元数据下载」阶段 aria2 给的是 `[METADATA]xxx`，这里把前缀去掉再显示。

use std::sync::Arc;

use serde_json::Value;

/// 每类（活动 / 等待 / 已停止）最多取多少条。
///
/// aria2 的 `tellWaiting`/`tellStopped` 必须显式给 `num`；默认 `max-download-result=1000`，
/// 全取回来会让一轮刷新变成 1000 条 × 若干 RPC 字段的解析。200 已远超一屏，
/// 而 `--panel-selftest --seed-rows 200` 的内存验收也按这个量级做。
pub const MAX_ROWS_PER_BUCKET: u32 = 200;

/// 任务状态（aria2 的 `status` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 正在下载/做种。
    Active,
    /// 排队中。
    Waiting,
    /// 已暂停。
    Paused,
    /// 已完成。
    Complete,
    /// 出错停止。
    Error,
    /// 已移除。
    Removed,
    /// 认不出来的状态（不猜语义）。
    Unknown,
}

impl TaskStatus {
    /// 由 aria2 的英文状态词构造。
    pub fn from_raw(raw: &str) -> Self {
        match raw {
            "active" => Self::Active,
            "waiting" => Self::Waiting,
            "paused" => Self::Paused,
            "complete" => Self::Complete,
            "error" => Self::Error,
            "removed" => Self::Removed,
            _ => Self::Unknown,
        }
    }

    /// 中文状态词。
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "下载中",
            Self::Waiting => "等待中",
            Self::Paused => "已暂停",
            Self::Complete => "已完成",
            Self::Error => "出错",
            Self::Removed => "已移除",
            Self::Unknown => "未知",
        }
    }

    /// 是否已经结束（在 `tellStopped` 里的状态）。
    ///
    /// 结束态的任务要做的是「清掉结果条目」(`aria2.removeDownloadResult`)，
    /// 而不是 `aria2.remove`（后者只对活动/等待/暂停有效）。
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Complete | Self::Error | Self::Removed)
    }

    /// 能否暂停。
    pub fn can_pause(self) -> bool {
        matches!(self, Self::Active | Self::Waiting)
    }

    /// 能否继续。
    pub fn can_resume(self) -> bool {
        matches!(self, Self::Paused)
    }

    /// 能否移除。
    pub fn can_remove(self) -> bool {
        matches!(
            self,
            Self::Active
                | Self::Waiting
                | Self::Paused
                | Self::Complete
                | Self::Error
                | Self::Removed
        )
    }

    /// 移除时该调哪个 RPC：结束态清结果条目，其余走 `aria2.remove`。
    pub fn remove_method(self) -> &'static str {
        if self.is_finished() {
            super::worker::RPC_REMOVE_RESULT
        } else {
            super::worker::RPC_REMOVE
        }
    }
}

/// 列表里的一行（所有字符串都已经格式化好，UI 只读）。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    /// aria2 的 GID（行标识，也是所有行操作的入参）。
    pub gid: Arc<str>,
    /// 显示名（文件名 / 磁力名 / URI）。
    pub name: Arc<str>,
    /// 总大小（已格式化，如 `1.2 MiB`；未知为 `—`）。
    pub size: Arc<str>,
    /// 进度 0.0..=1.0。
    pub progress: f32,
    /// 下载速率（已格式化，如 `2.5 MiB/s`；空闲为 `—`）。
    pub speed: Arc<str>,
    /// 剩余时间（已格式化，如 `3 分 20 秒`；未知为 `—`）。
    pub eta: Arc<str>,
    /// 中文状态词（[`TaskStatus::label`]）。
    pub status: Arc<str>,
    /// 状态枚举（决定行尾按钮）。
    pub kind: TaskStatus,
    /// aria2 的错误码（0 = 无错）。
    pub error_code: u64,
}

impl TaskRow {
    /// 一行「等待中」的占位（`addUri` 成功但下一轮刷新还没回来的那 2 秒）。
    pub fn optimistic(gid: &str, uri: &str) -> Self {
        let name = uri_name(uri);
        Self {
            gid: Arc::from(gid),
            name: Arc::from(name.as_str()),
            size: Arc::from("—"),
            progress: 0.0,
            speed: Arc::from("—"),
            eta: Arc::from("—"),
            status: Arc::from(TaskStatus::Waiting.label()),
            kind: TaskStatus::Waiting,
            error_code: 0,
        }
    }

    /// 由 aria2 的一条状态对象构造；少了 `gid` 就返回 `None`（没有身份的行没法操作）。
    pub fn from_value(value: &Value) -> Option<Self> {
        let gid = text_of(value, "gid")?;
        let raw_status = text_of(value, "status").unwrap_or_default();
        let kind = TaskStatus::from_raw(&raw_status);

        let total = total_length(value);
        let completed = completed_length(value);
        let speed = u64_of(value, "downloadSpeed");
        let progress = progress_ratio(completed, total, kind);
        let eta_seconds = eta_seconds(total, completed, speed, kind);

        Some(Self {
            gid: Arc::from(gid.as_str()),
            name: Arc::from(display_name(value, &gid).as_str()),
            size: Arc::from(if total == 0 {
                "—".to_owned()
            } else {
                format_bytes(total)
            }),
            progress,
            speed: Arc::from(if speed == 0 {
                "—".to_owned()
            } else {
                format_rate(speed)
            }),
            eta: Arc::from(format_eta(eta_seconds)),
            status: Arc::from(kind.label()),
            kind,
            error_code: u64_of(value, "errorCode"),
        })
    }

    /// 自测用的假行（`--panel-selftest --seed-rows N`）：内容稳定、覆盖各种状态与列宽。
    ///
    /// 刻意让名称长度、数值量级、状态词都不同，这样截图/内存测试同时也在压列宽与省略号。
    pub fn sample(index: usize) -> Self {
        const STATUSES: [TaskStatus; 5] = [
            TaskStatus::Active,
            TaskStatus::Waiting,
            TaskStatus::Paused,
            TaskStatus::Complete,
            TaskStatus::Error,
        ];
        let kind = STATUSES[index % STATUSES.len()];
        let total = 64 * 1024 * 1024 * (1 + (index as u64 % 97));
        let completed = match kind {
            TaskStatus::Complete => total,
            TaskStatus::Active => total / 3,
            _ => total / (2 + (index as u64 % 5)),
        };
        let speed = match kind {
            TaskStatus::Active => 1024 * (128 + (index as u64 % 4096)),
            _ => 0,
        };
        let name = format!(
            "种子-{index:03}-{}.bin",
            match index % 3 {
                0 => "ubuntu-24.04-desktop-amd64",
                1 => "synthetic-test-payload-with-a-very-long-file-name-to-force-ellipsis",
                _ => "archive",
            }
        );
        Self {
            gid: Arc::from(format!("{:016x}", 0x2089_b05e_cca3_d829_u64 + index as u64).as_str()),
            name: Arc::from(name.as_str()),
            size: Arc::from(format_bytes(total).as_str()),
            progress: progress_ratio(completed, total, kind),
            speed: Arc::from(if speed == 0 {
                "—".to_owned()
            } else {
                format_rate(speed)
            }),
            eta: Arc::from(format_eta(eta_seconds(total, completed, speed, kind)).as_str()),
            status: Arc::from(kind.label()),
            kind,
            error_code: if kind == TaskStatus::Error { 1 } else { 0 },
        }
    }
}

/// 合并三路 `tell*` 结果：活动 → 等待 → 已停止（面板里最关心正在下的）。
pub fn rows_from(active: &[Value], waiting: &[Value], stopped: &[Value]) -> Vec<TaskRow> {
    let mut rows = Vec::with_capacity(active.len() + waiting.len() + stopped.len());
    for bucket in [active, waiting, stopped] {
        for value in bucket {
            if let Some(row) = TaskRow::from_value(value) {
                rows.push(row);
            }
        }
    }
    rows
}

/// 行的三种操作（UI 只发意图，RPC 方法名在 [`TaskStatus::remove_method`] 里定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAction {
    /// `aria2.pause`
    Pause,
    /// `aria2.unpause`
    Unpause,
    /// `aria2.remove` / `aria2.removeDownloadResult`（看状态）
    Remove,
}

impl TaskAction {
    /// 中文动词（按钮与日志共用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "暂停",
            Self::Unpause => "继续",
            Self::Remove => "移除",
        }
    }
}

// ---------------------------------------------------------------------------
// 解析与格式化（纯函数，单测全覆盖）
// ---------------------------------------------------------------------------

/// 读一个数值字段：aria2 给字符串，本地假件/新版本可能给数字，两种都认。
fn u64_of(value: &Value, key: &str) -> u64 {
    match value.get(key) {
        Some(Value::String(text)) => text.trim().parse::<u64>().unwrap_or(0),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// 读一个非空字符串字段。
fn text_of(value: &Value, key: &str) -> Option<String> {
    let text = value.get(key)?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// 总长度：优先顶层 `totalLength`，退回 `files[].length` 之和。
fn total_length(value: &Value) -> u64 {
    let top = u64_of(value, "totalLength");
    if top > 0 {
        return top;
    }
    sum_files(value, "length")
}

/// 已完成长度：优先顶层 `completedLength`，退回 `files[].completedLength` 之和。
fn completed_length(value: &Value) -> u64 {
    let top = u64_of(value, "completedLength");
    if top > 0 {
        return top;
    }
    sum_files(value, "completedLength")
}

/// 累加 `files[].<key>`。
fn sum_files(value: &Value, key: &str) -> u64 {
    value
        .get("files")
        .and_then(Value::as_array)
        .map(|files| files.iter().map(|file| u64_of(file, key)).sum())
        .unwrap_or(0)
}

/// 进度：结束时恒定 100%，否则按已完成/总量；总量未知时为 0。
fn progress_ratio(completed: u64, total: u64, kind: TaskStatus) -> f32 {
    if matches!(kind, TaskStatus::Complete) {
        return 1.0;
    }
    if total == 0 {
        return 0.0;
    }
    (completed.min(total) as f32 / total as f32).clamp(0.0, 1.0)
}

/// 剩余秒数：只有「还没下完 + 速率 > 0」才有意义。
fn eta_seconds(total: u64, completed: u64, speed: u64, kind: TaskStatus) -> Option<u64> {
    if kind.is_finished() || speed == 0 || total <= completed {
        return None;
    }
    Some((total - completed).div_ceil(speed))
}

/// 显示名：`bittorrent.info.name` → `files[0].path` 的文件名 → `files[0].uris[0].uri` → `任务 <gid>`。
fn display_name(value: &Value, gid: &str) -> String {
    if let Some(name) = value
        .get("bittorrent")
        .and_then(|bt| bt.get("info"))
        .and_then(|info| info.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return strip_metadata_prefix(name).to_owned();
    }

    let first_file = value
        .get("files")
        .and_then(Value::as_array)
        .and_then(|files| files.first());

    if let Some(path) = first_file
        .and_then(|file| file.get("path"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if !base.is_empty() {
            return strip_metadata_prefix(base).to_owned();
        }
    }

    if let Some(uri) = first_file
        .and_then(|file| file.get("uris"))
        .and_then(Value::as_array)
        .and_then(|uris| uris.first())
        .and_then(|uri| uri.get("uri"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
    {
        return uri_name(uri);
    }

    format!("任务 {gid}")
}

/// URI → 显示名（取最后一段路径；磁力链接没有文件名，退回前缀）。
fn uri_name(uri: &str) -> String {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return "（未命名任务）".to_owned();
    }
    if trimmed.starts_with("magnet:") {
        // magnet 的 dn=（display name）才是人看的名字，没有就退回 hash。
        for part in trimmed.split('&') {
            if let Some(name) = part.strip_prefix("dn=") {
                let decoded = percent_decode(name);
                if !decoded.is_empty() {
                    return decoded;
                }
            }
        }
        return "磁力链接任务".to_owned();
    }
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let last = without_query.rsplit('/').find(|part| !part.is_empty());
    match last {
        Some(name) => name.to_owned(),
        None => trimmed.to_owned(),
    }
}

/// 磁力「元数据下载」阶段的 `[METADATA]` 前缀（aria2 自己的标记，不给人看）。
fn strip_metadata_prefix(name: &str) -> &str {
    name.strip_prefix("[METADATA]").unwrap_or(name)
}

/// 最小百分号解码（只处理 `%XX`；磁力 dn= 里的中文必须解出来才是人看的名字）。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 字节数 → 人类可读（1024 进制，一位小数；`0 B` 不加 `/s`）。
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let value = bytes as f64;
    if value >= TIB {
        format!("{:.1} TiB", value / TIB)
    } else if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// 速率 → 人类可读（`format_bytes` + `/s`）。
pub fn format_rate(bytes_per_second: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

/// 剩余时间 → 中文（`—` 表示未知/不适用）。
pub fn format_eta(seconds: Option<u64>) -> String {
    let Some(seconds) = seconds else {
        return "—".to_owned();
    };
    if seconds < 60 {
        format!("{seconds} 秒")
    } else if seconds < 3600 {
        format!("{} 分 {} 秒", seconds / 60, seconds % 60)
    } else {
        format!("{} 小时 {} 分", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn active_payload() -> Value {
        json!({
            "gid": "2089b05ecca3d829",
            "status": "active",
            "totalLength": "1048576",
            "completedLength": "262144",
            "downloadSpeed": "131072",
            "uploadSpeed": "0",
            "connections": "4",
            "dir": "C:\\Users\\Alusss\\Downloads",
            "errorCode": "0",
            "files": [{
                "index": "1",
                "path": "C:\\Users\\Alusss\\Downloads\\ubuntu.iso",
                "length": "1048576",
                "completedLength": "262144",
                "selected": "true",
                "uris": [{ "uri": "http://127.0.0.1:8080/ubuntu.iso", "status": "used" }]
            }]
        })
    }

    #[test]
    fn parses_active_row_into_formatted_cells() {
        let row = TaskRow::from_value(&active_payload()).expect("有 gid 就必须能成行");
        assert_eq!(&*row.gid, "2089b05ecca3d829");
        assert_eq!(&*row.name, "ubuntu.iso");
        assert_eq!(&*row.size, "1.0 MiB");
        assert_eq!(&*row.speed, "128.0 KiB/s");
        assert_eq!(&*row.status, "下载中");
        assert_eq!(row.kind, TaskStatus::Active);
        assert!((row.progress - 0.25).abs() < 1e-6, "{}", row.progress);
        // 786432 B / 131072 B/s = 6 秒
        assert_eq!(&*row.eta, "6 秒");
    }

    #[test]
    fn numeric_fields_as_numbers_are_tolerated() {
        let payload = json!({
            "gid": "abc",
            "status": "waiting",
            "totalLength": 2048,
            "completedLength": 1024,
            "downloadSpeed": 0,
            "files": [{ "path": "C:/tmp/a.zip" }]
        });
        let row = TaskRow::from_value(&payload).expect("数字字段也要能读");
        assert_eq!(&*row.name, "a.zip");
        assert_eq!(&*row.size, "2.0 KiB");
        assert_eq!(&*row.speed, "—");
        assert_eq!(&*row.eta, "—");
    }

    #[test]
    fn row_without_gid_is_dropped() {
        assert!(TaskRow::from_value(&json!({ "status": "active" })).is_none());
    }

    #[test]
    fn bitfield_name_wins_over_file_path() {
        let payload = json!({
            "gid": "b1",
            "status": "active",
            "totalLength": "10",
            "bittorrent": { "info": { "name": "debian-13.iso" } },
            "files": [{ "path": "C:\\dl\\[METADATA]debian", "length": "10" }]
        });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(&*row.name, "debian-13.iso");
    }

    #[test]
    fn metadata_prefix_is_stripped_from_file_names() {
        let payload = json!({
            "gid": "b2",
            "status": "active",
            "files": [{ "path": "[METADATA]ubuntu" }]
        });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(&*row.name, "ubuntu");
    }

    #[test]
    fn magnet_display_name_is_percent_decoded() {
        let payload = json!({
            "gid": "m1",
            "status": "active",
            "files": [{ "uris": [{ "uri": "magnet:?xt=urn:btih:abc&dn=%E4%B8%AD%E6%96%87.iso" }] }]
        });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(&*row.name, "中文.iso");
    }

    #[test]
    fn magnet_without_display_name_falls_back() {
        assert_eq!(uri_name("magnet:?xt=urn:btih:abc"), "磁力链接任务");
    }

    #[test]
    fn uri_name_strips_query_and_empty_segments() {
        assert_eq!(
            uri_name("http://127.0.0.1:8000/a/b/file.iso?token=1"),
            "file.iso"
        );
        assert_eq!(uri_name("http://127.0.0.1:8000/"), "127.0.0.1:8000");
    }

    #[test]
    fn finished_rows_are_full_and_have_no_eta() {
        let payload = json!({
            "gid": "c1",
            "status": "complete",
            "totalLength": "100",
            "completedLength": "100",
            "downloadSpeed": "0",
            "files": [{ "path": "a.bin" }]
        });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(row.progress, 1.0);
        assert_eq!(&*row.eta, "—");
        assert_eq!(&*row.status, "已完成");
        assert!(!row.kind.can_pause());
        assert!(row.kind.can_remove());
        assert_eq!(
            row.kind.remove_method(),
            super::super::worker::RPC_REMOVE_RESULT
        );
    }

    #[test]
    fn file_lengths_are_summed_when_top_level_missing() {
        let payload = json!({
            "gid": "s1",
            "status": "waiting",
            "files": [
                { "path": "a", "length": "100", "completedLength": "50" },
                { "path": "b", "length": "300", "completedLength": "50" }
            ]
        });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(&*row.size, "400 B");
        assert!((row.progress - 0.25).abs() < 1e-6, "{}", row.progress);
    }

    #[test]
    fn paused_row_can_resume_but_not_pause() {
        let payload = json!({ "gid": "p1", "status": "paused", "totalLength": "10" });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(row.kind, TaskStatus::Paused);
        assert!(row.kind.can_resume());
        assert!(!row.kind.can_pause());
        assert_eq!(row.kind.remove_method(), super::super::worker::RPC_REMOVE);
    }

    #[test]
    fn unknown_status_is_not_guessed() {
        let payload = json!({ "gid": "u1", "status": "seeding-what" });
        let row = TaskRow::from_value(&payload).expect("成行");
        assert_eq!(row.kind, TaskStatus::Unknown);
        assert_eq!(&*row.status, "未知");
        assert!(!row.kind.can_pause());
    }

    #[test]
    fn rows_from_orders_active_then_waiting_then_stopped() {
        let active = vec![json!({ "gid": "a", "status": "active" })];
        let waiting = vec![json!({ "gid": "b", "status": "waiting" })];
        let stopped = vec![json!({ "gid": "c", "status": "complete" })];
        let rows = rows_from(&active, &waiting, &stopped);
        let gids: Vec<&str> = rows.iter().map(|row| &*row.gid).collect();
        assert_eq!(gids, vec!["a", "b", "c"]);
    }

    #[test]
    fn format_bytes_switches_units_at_1024() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(2_621_440), "2.5 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        assert_eq!(format_bytes(2 * 1024_u64.pow(4)), "2.0 TiB");
    }

    #[test]
    fn format_eta_reads_naturally_in_chinese() {
        assert_eq!(format_eta(None), "—");
        assert_eq!(format_eta(Some(0)), "0 秒");
        assert_eq!(format_eta(Some(59)), "59 秒");
        assert_eq!(format_eta(Some(60)), "1 分 0 秒");
        assert_eq!(format_eta(Some(200)), "3 分 20 秒");
        assert_eq!(format_eta(Some(3600)), "1 小时 0 分");
        assert_eq!(format_eta(Some(7200 + 5 * 60)), "2 小时 5 分");
    }

    #[test]
    fn optimistic_row_is_a_waiting_placeholder() {
        let row = TaskRow::optimistic("g1", "http://127.0.0.1:8080/x.bin");
        assert_eq!(&*row.name, "x.bin");
        assert_eq!(&*row.status, "等待中");
        assert_eq!(row.progress, 0.0);
        assert_eq!(&*row.size, "—");
    }

    #[test]
    fn sample_rows_cover_every_status_and_are_stable() {
        let rows: Vec<TaskRow> = (0..200).map(TaskRow::sample).collect();
        assert_eq!(rows.len(), 200);
        for kind in [
            TaskStatus::Active,
            TaskStatus::Waiting,
            TaskStatus::Paused,
            TaskStatus::Complete,
            TaskStatus::Error,
        ] {
            assert!(rows.iter().any(|row| row.kind == kind), "{kind:?} 没有样本");
        }
        // 同一下标必须每次都一样（自测要可复现）。
        assert_eq!(TaskRow::sample(7), TaskRow::sample(7));
        assert_ne!(TaskRow::sample(7), TaskRow::sample(8));
        // gid 不重复（虚拟化表格按 gid 做行 id）。
        let mut gids: Vec<&str> = rows.iter().map(|row| &*row.gid).collect();
        let total = gids.len();
        gids.sort_unstable();
        gids.dedup();
        assert_eq!(gids.len(), total, "样本 gid 必须唯一");
    }

    #[test]
    fn percent_decode_keeps_plain_text() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("%41%42"), "AB");
        // 残缺的转义不能 panic，也不吞字符。
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}
