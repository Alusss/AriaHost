//! M1-3 的错误类型与既有错误码映射。
//!
//! 与 M1-2A 的口径一致：**只映射语义上真的对得上的既有码**，没有对应码时 [`CoreError::code`]
//! 返回 `None`，由调用方用 [`Display`] 的原文展示，不硬套（硬套会让 UI 谎报错误类别）。
//!
//! | 情况 | 变体 | `ErrorCode` | 依据 |
//! |---|---|---|---|
//! | DNS/连接/TLS/超时等线路故障 | [`CoreError::NetworkUnreachable`] | `AH-E6001` | 「更新网络失败」 |
//! | 载荷不是预期格式（Content-Type 或首字符不对） | [`CoreError::BadPayload`] | `AH-E6001` | 架构 §4.7：BadPayload 与网络故障同码、文案区分 |
//! | GitHub 限流（429 / 403+rate limit 文案） | [`CoreError::RateLimited`] | `AH-E6002` | 「更新请求被限流（429）」 |
//! | HTTP 403/404 | [`CoreError::HttpStatus`] | `AH-E3003` | 该码定义即「HTTP 403/404」 |
//! | HTTP 其它非 2xx | [`CoreError::HttpStatus`] | `AH-E6001` | 更新链路上的 5xx 归网络失败 |
//! | 长度不符/zip 魔数/哈希不符/解压失败 | [`CoreError::PackageInvalid`] | `AH-E6003` | 「更新包校验失败」 |
//! | 请求的版本不在版本库 | [`CoreError::VersionNotFound`] | 无 | 既有码里没有「版本未下载」 |
//! | 该版本低于最低支持（1.19.1） | [`CoreError::VersionTooOld`] | `AH-E6004` | 「核心版本过低」 |
//! | 没有匹配本机架构的资产 | [`CoreError::NoAssetForArch`] | 无 | 不在既有码语义内 |
//! | 下载线路全部失败 | [`CoreError::AllLinesFailed`] | `AH-E6007` | 「下载源全部不可用」 |
//! | 目标 `aria2c.exe` 被占用 | [`CoreError::ExeInUse`] | `AH-E6005` | 「exe 被占用」 |
//! | 回滚失败（没有上一代） | [`CoreError::RollbackFailed`] | `AH-E6008` | 「回滚自身失败」 |
//! | 版本库读写失败 | [`CoreError::StoreIo`] | 无 | 既有码无「本地版本库 IO」 |
//! | 输入/上游版本号非法 | [`CoreError::BadVersion`] | `AH-E1001` | 命令行入口按用法错误处理 |
//! | 手工 tag 非法（含路径穿越形态） | [`CoreError::BadTag`] | `AH-E1001` | 用户输入，与 `BadVersion` 同码 |
//! | 该 tag 下 3 个 build 全 404 | [`CoreError::NoReleaseAsset`] | 无 | 既非网络故障也非包损坏 |
//! | 下载包 `--version` 与 tag 不符 | [`CoreError::VersionMismatch`] | `AH-E6003` | 「更新包校验失败」 |

use std::fmt;

use ariahost_common::ErrorCode;

/// 一条下载线路的失败记录（`AllLinesFailed` 里逐条保留，供 UI 明确展示，不静默降级）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAttempt {
    /// 线路名（例如「直连 GitHub」「CDN」；旧镜像前缀已废弃并删除）。
    pub line: String,
    /// 该线路上实际请求的 URL。
    pub url: String,
    /// 失败原文。
    pub detail: String,
}

/// aria2 核心获取/版本管理过程中的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// 线路/API 网络不可达（DNS、连接、TLS、超时）。
    NetworkUnreachable {
        /// 诊断原文。
        detail: String,
    },
    /// 载荷不是预期格式。
    BadPayload {
        /// 诊断原文（含 Content-Type 与实际首字节）。
        detail: String,
    },
    /// GitHub 限流。
    RateLimited {
        /// 诊断原文。
        detail: String,
    },
    /// HTTP 非 2xx 且不属于限流。
    HttpStatus {
        /// 状态码。
        status: u16,
    },
    /// 包校验失败（长度、魔数、哈希、解压）。
    PackageInvalid {
        /// 诊断原文。
        detail: String,
    },
    /// 请求的版本不在版本库。
    VersionNotFound {
        /// 版本号字符串。
        version: String,
    },
    /// 上游 release 列表里没有这个版本。
    ReleaseNotFound {
        /// 版本号字符串。
        version: String,
        /// 目录里可用的版本数（诊断用）。
        available: usize,
    },
    /// 版本低于最低支持。
    VersionTooOld {
        /// 版本号字符串。
        version: String,
        /// 最低支持版本。
        minimum: String,
    },
    /// 没有匹配本机架构的资产。
    NoAssetForArch {
        /// 诊断原文。
        detail: String,
    },
    /// 下载线路全部失败。
    AllLinesFailed {
        /// 逐条线路的失败记录。
        attempts: Vec<LineAttempt>,
    },
    /// 目标 `aria2c.exe` 被占用/无法取得写权限。
    ExeInUse {
        /// 目标路径。
        path: String,
    },
    /// 回滚失败。
    RollbackFailed {
        /// 诊断原文。
        detail: String,
    },
    /// 版本库读写失败。
    StoreIo {
        /// 诊断原文。
        detail: String,
    },
    /// 版本号字符串非法。
    BadVersion {
        /// 原始取值。
        value: String,
    },
    /// 手工指定的 tag 非法（tag 会进下载 URL 与版本目录名，**必须严格**）。
    BadTag {
        /// 具体原因（来自 [`crate::tag::TagError`]）。
        reason: String,
    },
    /// 该 tag 下没有本机架构的发布包（`build1`–`build3` 全 404）。
    NoReleaseAsset {
        /// 归一化后的版本号。
        tag: String,
        /// 位宽。
        bits: u32,
        /// 试过的 build 数。
        builds: u32,
    },
    /// 下载包的 `aria2c --version` 与 tag 不一致（包被替换 / build 名对不上）。
    VersionMismatch {
        /// 期望版本（归一化后的 tag）。
        expected: String,
        /// 实测版本。
        actual: String,
    },
}

impl CoreError {
    /// 语义上能对上的既有错误码；无对应码时返回 `None`。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::NetworkUnreachable { .. } | Self::BadPayload { .. } | Self::HttpStatus { .. } => {
                Some(ErrorCode::UpdateNetworkFailed)
            }
            Self::RateLimited { .. } => Some(ErrorCode::UpdateRateLimited),
            Self::PackageInvalid { .. } => Some(ErrorCode::UpdatePackageInvalid),
            Self::VersionTooOld { .. } => Some(ErrorCode::CoreVersionTooOld),
            Self::AllLinesFailed { .. } => Some(ErrorCode::AllSourcesUnavailable),
            Self::ExeInUse { .. } => Some(ErrorCode::ExeInUse),
            Self::RollbackFailed { .. } => Some(ErrorCode::RollbackFailed),
            Self::BadVersion { .. } | Self::BadTag { .. } => Some(ErrorCode::Usage),
            Self::VersionMismatch { .. } => Some(ErrorCode::UpdatePackageInvalid),
            Self::VersionNotFound { .. }
            | Self::ReleaseNotFound { .. }
            | Self::NoAssetForArch { .. }
            | Self::NoReleaseAsset { .. }
            | Self::StoreIo { .. } => None,
        }
    }

    /// 构造网络错误（`io::Error` 只留文本，便于 `Clone`/比较）。
    pub fn network(detail: impl Into<String>) -> Self {
        Self::NetworkUnreachable {
            detail: detail.into(),
        }
    }

    /// 构造包校验错误。
    pub fn package(detail: impl Into<String>) -> Self {
        Self::PackageInvalid {
            detail: detail.into(),
        }
    }

    /// 构造版本库 IO 错误。
    pub fn store_io(detail: impl Into<String>) -> Self {
        Self::StoreIo {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NetworkUnreachable { detail } => write!(
                formatter,
                "{} aria2 核心下载失败（网络不可达）：{detail}",
                ErrorCode::UpdateNetworkFailed.code()
            ),
            Self::BadPayload { detail } => write!(
                formatter,
                "{} 下载线路返回的不是预期内容：{detail}",
                ErrorCode::UpdateNetworkFailed.code()
            ),
            Self::RateLimited { detail } => write!(
                formatter,
                "{} GitHub 限流：{detail}",
                ErrorCode::UpdateRateLimited.code()
            ),
            Self::HttpStatus { status } => {
                // 403/404 有专用码（E3003，语义就是「HTTP 403/404」）；其它状态码归更新网络失败。
                let code = if matches!(*status, 403 | 404) {
                    ErrorCode::HttpRejected
                } else {
                    ErrorCode::UpdateNetworkFailed
                };
                write!(
                    formatter,
                    "{} aria2 核心下载返回 HTTP {status}",
                    code.code()
                )
            }
            Self::PackageInvalid { detail } => write!(
                formatter,
                "{} 下载包校验失败：{detail}",
                ErrorCode::UpdatePackageInvalid.code()
            ),
            Self::VersionNotFound { version } => {
                write!(formatter, "版本库里没有 aria2 {version}")
            }
            Self::ReleaseNotFound { version, available } => write!(
                formatter,
                "GitHub 上找不到 aria2 {version} 的 release（目录里有 {available} 个可用版本）"
            ),
            Self::VersionTooOld { version, minimum } => write!(
                formatter,
                "{} aria2 {version} 低于最低支持版本 {minimum}",
                ErrorCode::CoreVersionTooOld.code()
            ),
            Self::NoAssetForArch { detail } => {
                write!(formatter, "该版本没有匹配本机架构的安装包：{detail}")
            }
            Self::AllLinesFailed { attempts } => {
                write!(
                    formatter,
                    "{} 全部下载线路失败：",
                    ErrorCode::AllSourcesUnavailable.code()
                )?;
                for (index, attempt) in attempts.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, "；")?;
                    }
                    write!(formatter, "{}（{}）", attempt.line, attempt.detail)?;
                }
                Ok(())
            }
            Self::ExeInUse { path } => write!(
                formatter,
                "{} 目标 aria2c.exe 被占用，无法切换：{path}",
                ErrorCode::ExeInUse.code()
            ),
            Self::RollbackFailed { detail } => write!(
                formatter,
                "{} 回滚失败：{detail}",
                ErrorCode::RollbackFailed.code()
            ),
            Self::StoreIo { detail } => write!(formatter, "版本库读写失败：{detail}"),
            Self::BadVersion { value } => {
                write!(formatter, "{} 版本号非法：{value}", ErrorCode::Usage.code())
            }
            Self::BadTag { reason } => write!(
                formatter,
                "{} 手工指定的 aria2 tag 非法：{reason}",
                ErrorCode::Usage.code()
            ),
            Self::NoReleaseAsset { tag, bits, builds } => write!(
                formatter,
                "该 tag 下没有 {bits} 位的 aria2 发布包：{tag}（build1–build{builds} 全部 404）——\
                 请到 {} 核对版本号与本机架构",
                crate::tag::ARIA2_RELEASES_PAGE
            ),
            Self::VersionMismatch { expected, actual } => write!(
                formatter,
                "{} 下载包版本与 tag 不一致：期望 {expected}，实测 {actual}（包可能被替换，或 build 名对不上）",
                ErrorCode::UpdatePackageInvalid.code()
            ),
        }
    }
}

impl std::error::Error for CoreError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_and_payload_share_update_network_code() {
        assert_eq!(
            CoreError::network("连接超时").code(),
            Some(ErrorCode::UpdateNetworkFailed)
        );
        assert_eq!(
            CoreError::BadPayload {
                detail: "text/html".to_owned()
            }
            .code(),
            Some(ErrorCode::UpdateNetworkFailed)
        );
    }

    #[test]
    fn rate_limit_and_package_and_rollback_codes() {
        assert_eq!(
            CoreError::RateLimited {
                detail: "API rate limit exceeded".to_owned()
            }
            .code(),
            Some(ErrorCode::UpdateRateLimited)
        );
        assert_eq!(
            CoreError::package("哈希不符").code(),
            Some(ErrorCode::UpdatePackageInvalid)
        );
        assert_eq!(
            CoreError::RollbackFailed {
                detail: "没有上一代".to_owned()
            }
            .code(),
            Some(ErrorCode::RollbackFailed)
        );
        assert_eq!(
            CoreError::ExeInUse {
                path: "C:\\x\\aria2c.exe".to_owned()
            }
            .code(),
            Some(ErrorCode::ExeInUse)
        );
        assert_eq!(
            CoreError::AllLinesFailed {
                attempts: Vec::new()
            }
            .code(),
            Some(ErrorCode::AllSourcesUnavailable)
        );
    }

    #[test]
    fn unmapped_variants_return_none() {
        for error in [
            CoreError::VersionNotFound {
                version: "1.37.0".to_owned(),
            },
            CoreError::NoAssetForArch {
                detail: "只有 64 位".to_owned(),
            },
            CoreError::store_io("写指针失败"),
        ] {
            assert_eq!(error.code(), None, "{error} 不应硬套错误码");
        }
    }

    #[test]
    fn http_status_mapping_splits_403_404_from_other_codes() {
        assert_eq!(
            CoreError::HttpStatus { status: 404 }.code(),
            Some(ErrorCode::UpdateNetworkFailed)
        );
        // 403/404 走 E3003；Display 里必须出现对应码。
        let message = CoreError::HttpStatus { status: 403 }.to_string();
        assert!(message.contains("AH-E3003"), "{message}");
        let message = CoreError::HttpStatus { status: 502 }.to_string();
        assert!(message.contains("AH-E6001"), "{message}");
    }

    #[test]
    fn display_starts_with_code_when_mapped() {
        let message = CoreError::package("zip 魔数不符").to_string();
        assert!(message.starts_with("AH-E6003"), "{message}");
        let message = CoreError::RateLimited {
            detail: "429".to_owned(),
        }
        .to_string();
        assert!(message.starts_with("AH-E6002"), "{message}");
    }

    #[test]
    fn all_lines_failed_lists_every_attempt() {
        let error = CoreError::AllLinesFailed {
            attempts: vec![
                LineAttempt {
                    line: "直连 GitHub".to_owned(),
                    url: "https://github.com/x.zip".to_owned(),
                    detail: "连接超时".to_owned(),
                },
                LineAttempt {
                    line: "镜像".to_owned(),
                    url: "https://mirror.invalid/https://github.com/x.zip".to_owned(),
                    detail: "HTTP 404".to_owned(),
                },
            ],
        };
        let message = error.to_string();
        assert!(message.contains("直连 GitHub"), "{message}");
        assert!(message.contains("镜像"), "{message}");
        assert!(message.contains("AH-E6007"), "{message}");
    }
}
