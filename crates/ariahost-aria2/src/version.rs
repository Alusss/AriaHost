//! `aria2c --version` 输出解析。
//!
//! 只解析首行，形如：
//! ```text
//! aria2 version 1.37.0
//! Copyright (C) 2006, 2019 Tatsuhiro Tsujikawa
//! ```
//!
//! 版本比较用派生 `Ord`（按 major/minor/patch 字典序），不引入 semver 依赖。

use std::fmt;

/// `aria2c --version` 首行的固定前缀（含尾随空格）。
const VERSION_PREFIX: &str = "aria2 version ";

/// aria2 版本号（三段数字）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Aria2Version {
    /// 主版本号。
    pub major: u32,
    /// 次版本号。
    pub minor: u32,
    /// 修订号。
    pub patch: u32,
}

impl Aria2Version {
    /// 构造版本号。
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl fmt::Display for Aria2Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 版本行解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionParseError {
    /// 不是 aria2 的版本行。
    #[error("不是 aria2 版本行（缺少 `aria2 version ` 前缀）：{line}")]
    MissingPrefix {
        /// 实际收到的整行。
        line: String,
    },
    /// 版本号不是三段数字。
    #[error("版本号格式非法（期望三段数字，如 1.37.0）：{value}")]
    Malformed {
        /// 前缀之后的内容。
        value: String,
    },
}

/// 解析 `aria2c --version` 的首行，例如 `aria2 version 1.37.0`。
///
/// 容忍首尾空白与 CRLF；版本号后不允许再跟后缀（`1.37.0-rc1` 视为非法），
/// 因为 aria2 正式版输出只有三段数字，带后缀说明输出被改动过，应进诊断而不是猜。
pub fn parse_version_line(line: &str) -> Result<Aria2Version, VersionParseError> {
    let line = line.trim();
    let value =
        line.strip_prefix(VERSION_PREFIX)
            .ok_or_else(|| VersionParseError::MissingPrefix {
                line: line.to_owned(),
            })?;
    parse_version_number(value)
}

/// 解析纯版本号（不带 `aria2 version ` 前缀），例如 `1.37.0`。
///
/// 供「GitHub release tag」「配置里的版本值」等复用同一套数字规则：M1-3 的
/// release tag `release-1.37.0` 去掉前缀后走这里，避免第二套版本解析。
pub fn parse_version_number(value: &str) -> Result<Aria2Version, VersionParseError> {
    let value = value.trim();
    let mut parts = value.split('.');
    let major = parse_component(&mut parts, value)?;
    let minor = parse_component(&mut parts, value)?;
    let patch = parse_component(&mut parts, value)?;
    if parts.next().is_some() {
        return Err(VersionParseError::Malformed {
            value: value.to_owned(),
        });
    }
    Ok(Aria2Version::new(major, minor, patch))
}

/// 解析一个十进制段；缺失或非数字都归为 `Malformed`。
fn parse_component(
    parts: &mut std::str::Split<'_, char>,
    value: &str,
) -> Result<u32, VersionParseError> {
    parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .ok_or_else(|| VersionParseError::Malformed {
            value: value.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_first_line() {
        let version = parse_version_line("aria2 version 1.37.0").expect("应能解析标准版本行");
        assert_eq!(version, Aria2Version::new(1, 37, 0));
        assert_eq!(version.to_string(), "1.37.0");
    }

    #[test]
    fn tolerates_whitespace_and_crlf() {
        let version = parse_version_line("  aria2 version 1.36.0\r\n").expect("应容忍空白与 CRLF");
        assert_eq!(version, Aria2Version::new(1, 36, 0));
    }

    #[test]
    fn rejects_missing_prefix() {
        let error = parse_version_line("aria2c version 1.37.0").expect_err("前缀不符应报错");
        assert!(matches!(error, VersionParseError::MissingPrefix { .. }));
    }

    #[test]
    fn rejects_malformed_versions() {
        for line in [
            "aria2 version 1.37",
            "aria2 version 1.37.0-rc1",
            "aria2 version 1.37.0.1",
            "aria2 version x.y.z",
            "aria2 version 1..0",
        ] {
            let error = parse_version_line(line).expect_err("非法版本号应报错");
            assert!(
                matches!(error, VersionParseError::Malformed { .. }),
                "{line} 应归为 Malformed"
            );
        }
    }

    #[test]
    fn missing_version_value_is_missing_prefix_after_trim() {
        // 行尾空白会被 trim，因此 `aria2 version ` 等价于 `aria2 version`，属于缺少前缀。
        for line in ["aria2 version", "aria2 version   "] {
            let error = parse_version_line(line).expect_err("缺少版本号应报错");
            assert!(
                matches!(error, VersionParseError::MissingPrefix { .. }),
                "{line} 应归为 MissingPrefix"
            );
        }
    }

    #[test]
    fn version_ordering_is_numeric() {
        assert!(Aria2Version::new(1, 36, 0) < Aria2Version::new(1, 37, 0));
        assert!(Aria2Version::new(1, 37, 0) < Aria2Version::new(2, 0, 0));
    }

    #[test]
    fn parses_bare_version_numbers() {
        // M1-3 的 GitHub tag `release-1.37.0` 去掉前缀后走这条路径。
        assert_eq!(
            parse_version_number("1.37.0").expect("应能解析纯版本号"),
            Aria2Version::new(1, 37, 0)
        );
        assert_eq!(
            parse_version_number(" 1.19.1 ").expect("应容忍空白"),
            Aria2Version::new(1, 19, 1)
        );
        for value in ["", "1", "1.37", "1.37.0.1", "1.37.0-rc1", "v1.37.0"] {
            assert!(
                matches!(
                    parse_version_number(value),
                    Err(VersionParseError::Malformed { .. })
                ),
                "{value:?} 应归为 Malformed"
            );
        }
    }
}
