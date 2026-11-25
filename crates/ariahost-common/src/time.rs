//! UTC 时间戳格式化（服务日志与任务历史落盘的**唯一实现**）。
//!
//! 为什么上移到 `common`：这批以前有两条链路各要一份时间戳——服务平面日志
//! `service.log` 的每行前缀（`ariahost-svc::log`）与任务历史 `downloads.jsonl` 的
//! `finished_at`（`ariahost-core::history`，由调用方给）。两处各写一份必然漂移
//! （一处给毫秒一处不给、一处 `Z` 一处 `+00:00`），而这两个字段都靠「字符串序即
//! 时间序」参与排序，格式一漂就把顺序弄错，且已经落盘的老日志不可比重排。
//!
//! ## 唯一格式（改动前先读这里）
//!
//! `YYYY-MM-DDTHH:MM:SS.mmmZ`，UTC，**定宽 24 字节**（[`STAMP_LEN`]）。
//!
//! - 毫秒三位：同一秒内的多条日志、多笔任务仍能分出先后；
//! - 结尾 `Z`：UTC，无时区偏移，跨机器可直接比较；
//! - 定宽：`&str` 的字典序就是时间序，排序不需要解析，也不依赖 locale。
//!
//! ## 为什么历史也用这个格式，而不是「更纯的」不带毫秒的 RFC3339
//!
//! `2023-11-14T22:13:20.000Z` 本身就是合法 RFC3339，所以不算改变语义。另外本批之前
//! 任务历史**没有任何生产调用点**（只有单测），磁盘上没有需要兼容的老文件，统一是免费的。
//! 反过来若保留两套（日志带毫秒、历史不带），同一条链路上就会混用两种长度：
//! `"…:20.500Z"` 与 `"…:20Z"` 逐字节比较时 `'.'`(0x2E) < `'Z'`(0x5A)，
//! 后者会被排到前者之后——**混用即错序**，这正是要避免的那类漂移。
//!
//! 曾经的服务日志实现（`svc::log::format_utc`）逐字搬到这里，输出字节完全一致，
//! 已有的 `service.log` 不受影响。

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 时间戳的固定长度（字节）。日志行前缀与 `finished_at` 都是这个宽度。
pub const STAMP_LEN: usize = 24;

/// `SystemTime` → `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC，定宽 24 字节，可直接按字节排序）。
///
/// 早于 1970-01-01 的时刻（`duration_since` 报错）**按纪元处理**，不 panic：
/// 日志要记录的是「程序看到的时刻」，为此崩掉不值得。这条行为有测试钉住。
pub fn utc_stamp(instant: SystemTime) -> String {
    let since_epoch = instant.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let seconds = i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX);
    let millis = since_epoch.subsec_millis();
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let mut out = String::with_capacity(STAMP_LEN);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    /// 外部核对过的定值：`date -u -d @<秒>` 的输出。
    #[test]
    fn matches_known_instants() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // `date -u -d @1700000000` = 2023-11-14T22:13:20Z
        assert_eq!(utc_stamp(at(1_700_000_000)), "2023-11-14T22:13:20.000Z");
        // `date -u -d @1793000000` = 2026-10-26T07:33:20Z
        assert_eq!(utc_stamp(at(1_793_000_000)), "2026-10-26T07:33:20.000Z");
        // 毫秒不被截断，且闰年 2 月 29 日正确（2024-02-29T12:00:00Z）。
        assert_eq!(
            utc_stamp(at(1_709_208_000) + Duration::from_millis(7)),
            "2024-02-29T12:00:00.007Z"
        );
    }

    /// 定宽的用途就是「字符串序 == 时间序」，这条测试是那个用途的守卫。
    #[test]
    fn stamps_sort_lexicographically_like_time() {
        let early = utc_stamp(at(1_700_000_000));
        let late = utc_stamp(at(1_700_000_001));
        assert!(
            early < late,
            "定宽 UTC 必须能按字符串排序：{early} / {late}"
        );
        // 同一秒内靠毫秒分先后（这条是「必须带毫秒」的理由）。
        let later_in_same_second = utc_stamp(at(1_700_000_000) + Duration::from_millis(500));
        assert!(utc_stamp(at(1_700_000_000)) < later_in_same_second);
        assert!(later_in_same_second < late);
    }

    /// 任何时刻都必须是 24 字节：`STAMP_LEN` 一旦被写进排序假设，长度漂移就是错序。
    #[test]
    fn every_stamp_is_exactly_24_bytes() {
        for stamp in [
            utc_stamp(UNIX_EPOCH),
            utc_stamp(at(1_000_000_000)),
            utc_stamp(at(1_700_000_000) + Duration::from_millis(999)),
            utc_stamp(at(4_102_444_800)), // 2100-01-01T00:00:00Z
        ] {
            assert_eq!(stamp.len(), STAMP_LEN, "{stamp}");
            assert!(stamp.is_ascii(), "只允许 ASCII，否则字节序不可靠：{stamp}");
            assert!(stamp.ends_with('Z'), "{stamp}");
        }
    }

    /// 早于纪元的时刻按纪元处理（不 panic、不返回负号字符串）。
    #[test]
    fn pre_epoch_clamps_to_epoch() {
        let before = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(utc_stamp(before), utc_stamp(UNIX_EPOCH));
        assert_eq!(utc_stamp(before), "1970-01-01T00:00:00.000Z");
    }
}
