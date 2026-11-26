//! 获取编排：API → 选资产 → 双线路下载 → 哈希 → 解压 → 装库 → 激活。
//!
//! ## 哈希口径（**不得假装校验过**）
//!
//! aria2 官方 release **不提供任何哈希**。本批唯一可信的交叉参考是 Scoop 清单里的
//! 1.37.0 两包（见 [`KNOWN_SHA256`]），因此：
//! - 有已知值 → **硬校验**，不匹配直接 [`CoreError::PackageInvalid`]（`AH-E6003`），
//!   **且在装库之前失败**，旧版本原封不动；
//! - 没有已知值 → [`HashState::Unverified`] + 一条进 [`FetchOutcome::warnings`] 的告警。
//!   返回值里带着这个状态，UI/日志就不能声称「已校验」。
//!
//! ## 顺序
//!
//! 所有校验（长度/魔数/哈希/解压）都在 [`crate::store::VersionStore::install_core`] 之前完成，
//! 所以任何一步失败都不会留下半装版本，也不会改 `active.json`。

use ariahost_aria2::Aria2Version;
use ariahost_common::config::{CoreProxyMode, NetSection, DEFAULT_CORE_PROXY_URL};
use sha2::{Digest, Sha256};

use crate::download::{download_zip, lines_from, DownloadLine};
use crate::error::CoreError;
use crate::http::{self, HttpClient};
use crate::release::{
    check_update, host_bits, ReleaseAsset, ReleaseCatalog, UpdateState, MIN_SUPPORTED,
};
use crate::store::{ActivePointer, VersionStore};
use crate::zip_extract::extract_core;

/// 已知的交叉参考 SHA-256（版本, 位数, 十六进制小写）。
///
/// 来源：Scoop 清单（2026-11-22 侦察）。**只有这两个**——其它版本一律
/// [`HashState::Unverified`]，不要为了「看起来更严」去编造校验值。
pub const KNOWN_SHA256: &[(&str, u32, &str)] = &[
    (
        "1.37.0",
        64,
        "67d015301eef0b612191212d564c5bb0a14b5b9c4796b76454276a4d28d9b288",
    ),
    (
        "1.37.0",
        32,
        "35f6514cc5dd7e98a87b3c4c2d25a0754b9b063dbe59bc0f22d483464f61e5b6",
    ),
];

/// 查已知哈希。
pub fn known_sha256(version: Aria2Version, bits: u32) -> Option<&'static str> {
    let text = version.to_string();
    KNOWN_SHA256
        .iter()
        .find(|(known_version, known_bits, _)| *known_version == text && *known_bits == bits)
        .map(|(_, _, sha)| *sha)
}

/// 解析命令行/配置里的版本参数：`latest`（大小写不敏感）→ `None`，其余按版本号解析。
pub fn parse_version_arg(arg: &str) -> Result<Option<Aria2Version>, CoreError> {
    let text = arg.trim();
    if text.eq_ignore_ascii_case("latest") {
        return Ok(None);
    }
    ariahost_aria2::parse_version_number(text)
        .map(Some)
        .map_err(|_| CoreError::BadVersion {
            value: text.to_owned(),
        })
}

/// 本次下载的哈希状态（**必须能区分「校验过」与「没法校验」**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashState {
    /// 已用已知交叉参考值校验通过。
    Verified {
        /// 期望值（小写十六进制）。
        sha256: String,
    },
    /// 无法校验：上游不提供哈希，且本版本没有已知交叉参考值。
    Unverified {
        /// 原因（进日志/告警）。
        reason: String,
    },
}

impl HashState {
    /// 是否真的校验过。
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// 一次成功获取的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    /// 装好并激活的版本。
    pub version: Aria2Version,
    /// 激活后的 `aria2c.exe` 路径。
    pub exe_path: std::path::PathBuf,
    /// 哈希状态（`Unverified` 时 UI 不得声称已校验）。
    pub hash: HashState,
    /// 命中的下载线路名。
    pub line: String,
    /// 命中线路之前的失败线路说明 + 未校验哈希告警（**必须展示**）。
    pub warnings: Vec<String>,
    /// 写盘后的激活指针。
    pub pointer: ActivePointer,
}

/// 获取器（HTTP 客户端 + 线路顺序）。
#[derive(Debug, Clone)]
pub struct Fetcher {
    client: HttpClient,
    lines: Vec<DownloadLine>,
    /// 造线路时产生的告警（非法 `source_order` 项、回退等），随每次 `fetch` 一并上报。
    line_warnings: Vec<String>,
    /// releases API 地址（默认 [`crate::release::RELEASES_API`]；测试指向本机假服务器）。
    api_url: String,
    /// 「AriaHost 自己访问 GitHub」的线路模式（`net.core_proxy_mode`）。
    ///
    /// 手工 tag 路径（[`crate::tag`]）不走 `lines`，而是用这一对设置经
    /// [`crate::net::github_via`] 拼出实际 URL——同一份判定，不另写一套。
    core_proxy_mode: CoreProxyMode,
    /// 前缀代理基址（`net.core_proxy_url`），仅 `proxy` 模式生效。
    core_proxy_url: String,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    /// 生产默认：HTTPS-only 客户端 + 用户机器默认线路顺序（`cdn → github`）。
    pub fn new() -> Self {
        Self::with_order(
            crate::download::DEFAULT_SOURCE_ORDER,
            crate::download::DEFAULT_CDN_BASE,
        )
    }

    /// 指定线路顺序：`order` 见 [`lines_from`]（`"cdn,github"` / `"github,cdn"` …）。
    ///
    /// CI 用 `github,cdn`（runner 在 GitHub 内网）；用户机器用默认值（CDN 优先）。
    pub fn with_order(order: &str, cdn_base: &str) -> Self {
        let (lines, line_warnings) = lines_from(order, cdn_base);
        Self {
            client: HttpClient::new(),
            lines,
            line_warnings,
            api_url: crate::release::RELEASES_API.to_owned(),
            core_proxy_mode: CoreProxyMode::Direct,
            core_proxy_url: DEFAULT_CORE_PROXY_URL.to_owned(),
        }
    }

    /// 按配置的 `[net]` 段造线路（`net.source_order` + `net.cdn_base` + 代理两键）。
    pub fn with_net(net: &NetSection) -> Self {
        Self::with_order(&net.source_order, &net.cdn_base)
            .with_core_proxy(net.core_proxy_mode, net.core_proxy_url.clone())
    }

    /// 注入客户端与线路（测试用：允许明文 HTTP 指向假服务器）。
    pub fn with_parts(client: HttpClient, lines: Vec<DownloadLine>) -> Self {
        Self {
            client,
            lines,
            line_warnings: Vec::new(),
            api_url: crate::release::RELEASES_API.to_owned(),
            core_proxy_mode: CoreProxyMode::Direct,
            core_proxy_url: DEFAULT_CORE_PROXY_URL.to_owned(),
        }
    }

    /// 覆盖「AriaHost 自己访问 GitHub」的线路（手工 tag 路径用；测试也靠它指向假服务器）。
    pub fn with_core_proxy(mut self, mode: CoreProxyMode, url: impl Into<String>) -> Self {
        self.core_proxy_mode = mode;
        self.core_proxy_url = url.into();
        self
    }

    /// 当前代理模式（`net.core_proxy_mode`）。
    pub fn core_proxy_mode(&self) -> CoreProxyMode {
        self.core_proxy_mode
    }

    /// 当前前缀代理基址（`net.core_proxy_url`）。
    pub fn core_proxy_url(&self) -> &str {
        &self.core_proxy_url
    }

    /// 内部客户端（同 crate 的 `tag` 模块要用它发下载请求；**不对外暴露**）。
    pub(crate) fn client(&self) -> &HttpClient {
        &self.client
    }

    /// 改 releases API 地址（测试用；也可用于企业内镜像）。
    pub fn with_api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    /// 当前 API 地址。
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// 当前线路。
    pub fn lines(&self) -> &[DownloadLine] {
        &self.lines
    }

    /// 造线路时的告警（`source_order` 里有非法项 / 回退到默认顺序等）。
    ///
    /// 这些告警与「下载阶段的线路失败」不是一回事：它们说明**配置没被完全照办**，
    /// 调用方应把它们并进用户可见的告警里（`FetchOutcome::warnings`）。
    pub fn line_warnings(&self) -> &[String] {
        &self.line_warnings
    }

    /// 拉 releases 目录。
    pub fn releases(&self) -> Result<ReleaseCatalog, CoreError> {
        let body = http::fetch_json(&self.client, &self.api_url)?;
        ReleaseCatalog::parse_json(&body)
    }

    /// 检查更新（四态里的两态；网络/限流由调用方按错误处理）。
    pub fn check_update(&self, current: Aria2Version) -> Result<UpdateState, CoreError> {
        Ok(check_update(&self.releases()?, current))
    }

    /// 获取并激活指定版本（`None` = 最新版）。
    pub fn fetch(
        &self,
        store: &VersionStore,
        wanted: Option<Aria2Version>,
    ) -> Result<FetchOutcome, CoreError> {
        if let Some(version) = wanted {
            if version < MIN_SUPPORTED {
                return Err(CoreError::VersionTooOld {
                    version: version.to_string(),
                    minimum: MIN_SUPPORTED.to_string(),
                });
            }
        }

        let catalog = self.releases()?;
        let entry = match wanted {
            Some(version) => catalog
                .find(version)
                .ok_or_else(|| CoreError::ReleaseNotFound {
                    version: version.to_string(),
                    available: catalog.len(),
                })?,
            None => catalog.latest().ok_or_else(|| CoreError::ReleaseNotFound {
                version: "latest".to_owned(),
                available: 0,
            })?,
        };
        let version = entry.version;

        let asset = entry
            .asset_for(host_bits())
            .ok_or_else(|| CoreError::NoAssetForArch {
                detail: format!("aria2 {version} 的 release 里没有 {} 位 zip", host_bits()),
            })?;
        let expected_size = if asset.size > 0 {
            Some(asset.size)
        } else {
            None
        };

        let download = download_zip(
            &self.client,
            &self.lines,
            &asset.browser_download_url,
            expected_size,
        )?;

        let (hash, mut warnings) = verify_hash(version, host_bits(), &download.bytes)?;
        warnings.extend(download.warnings);
        // 线路构造阶段的告警（配置没被完全照办）也要让用户看见，不能只留在 Fetcher 里。
        warnings.extend(self.line_warnings.iter().cloned());

        let extracted = extract_core(&download.bytes)?;
        store.install_core(&version.to_string(), &extracted)?;
        // 安装证据（M5-c）：与手工 tag 路径同一口径 —— 让 `list_cores` 能回答
        // 「这份 exe 与安装时是否逐字节一致」。写失败只进告警，不阻断安装。
        let provenance = crate::store::Provenance {
            asset_name: asset.name.clone(),
            line: download.line.clone(),
            zip_sha256: sha256_hex(&download.bytes),
            exe_sha256: sha256_hex(&extracted.aria2c),
            known_sha256_verified: hash.is_verified(),
            installed_at: ariahost_common::time::utc_stamp(std::time::SystemTime::now()),
        };
        if let Err(error) = store.write_provenance(&version.to_string(), &provenance) {
            warnings.push(format!(
                "写安装证据失败（hash_verified 会显示 false）：{error}"
            ));
        }
        let pointer = store.activate(&version.to_string())?;

        Ok(FetchOutcome {
            version,
            exe_path: store.exe_path(&version.to_string()),
            hash,
            line: download.line,
            warnings,
            pointer,
        })
    }

    /// 该版本 + 本机位数的资产（给 CLI 打印用；不下载）。
    pub fn asset_for(&self, version: Aria2Version) -> Result<ReleaseAsset, CoreError> {
        let text = version.to_string();
        let catalog = self.releases()?;
        catalog
            .asset_for(version, host_bits())
            .cloned()
            .ok_or_else(|| CoreError::ReleaseNotFound {
                version: text,
                available: catalog.len(),
            })
    }
}

/// 校验下载包的 SHA-256。
///
/// - **有已知值且不匹配 → `Err(PackageInvalid)`**（`AH-E6003`）：调用方在装库前就失败，
///   旧版本与指针都不会被动到；
/// - 有已知值且匹配 → `Ok(HashState::Verified)`；
/// - 没有已知值 → `Ok(HashState::Unverified)` + 一条告警（**绝不假装校验过**）。
pub fn verify_hash(
    version: Aria2Version,
    bits: u32,
    bytes: &[u8],
) -> Result<(HashState, Vec<String>), CoreError> {
    let actual = sha256_hex(bytes);
    match known_sha256(version, bits) {
        Some(expected) if expected == actual => {
            Ok((HashState::Verified { sha256: actual }, Vec::new()))
        }
        Some(expected) => Err(CoreError::package(format!(
            "SHA-256 不符：期望 {expected}，实际 {actual}"
        ))),
        None => {
            let reason = format!(
                "未校验哈希：aria2 官方 release 不提供 SHA-256，{version}（{bits} 位）也没有已知交叉参考值（实际 sha256={actual}）"
            );
            Ok((
                HashState::Unverified {
                    reason: reason.clone(),
                },
                vec![reason],
            ))
        }
    }
}

/// SHA-256 → 小写十六进制。
///
/// `pub(crate)`：`list_cores` 要实测版本库里的 exe 与已知交叉参考值是否一致，
/// 不能再写第二份哈希实现。
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hash_table_has_exactly_the_documented_pairs() {
        assert_eq!(
            known_sha256(Aria2Version::new(1, 37, 0), 64),
            Some("67d015301eef0b612191212d564c5bb0a14b5b9c4796b76454276a4d28d9b288")
        );
        assert_eq!(
            known_sha256(Aria2Version::new(1, 37, 0), 32),
            Some("35f6514cc5dd7e98a87b3c4c2d25a0754b9b063dbe59bc0f22d483464f61e5b6")
        );
        // 其它版本/位数没有已知值：绝不能编造。
        assert_eq!(known_sha256(Aria2Version::new(1, 36, 0), 64), None);
        assert_eq!(known_sha256(Aria2Version::new(1, 37, 0), 16), None);
    }

    #[test]
    fn version_arg_parsing_handles_latest_and_numbers() {
        assert_eq!(parse_version_arg("latest").expect("latest 合法"), None);
        assert_eq!(parse_version_arg("LATEST").expect("大小写不敏感"), None);
        assert_eq!(
            parse_version_arg(" 1.37.0 ").expect("应容忍空白"),
            Some(Aria2Version::new(1, 37, 0))
        );
        for bad in ["", "1.37", "v1.37.0", "newest"] {
            let error = parse_version_arg(bad).expect_err("非法参数应报错");
            assert!(matches!(error, CoreError::BadVersion { .. }), "{error}");
            assert_eq!(error.code(), Some(ariahost_common::ErrorCode::Usage));
        }
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // 空串的 SHA-256（公开测试向量）。
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // "abc" 的 SHA-256。
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn unverified_state_carries_a_warning() {
        let (state, warnings) =
            verify_hash(Aria2Version::new(1, 36, 0), 64, b"payload").expect("没有已知值不是错误");
        assert!(!state.is_verified());
        assert_eq!(warnings.len(), 1, "未校验必须留一条告警");
        assert!(warnings[0].contains("未校验哈希"), "{}", warnings[0]);
        match state {
            HashState::Unverified { reason } => {
                assert!(reason.contains("sha256="), "实际值要进原因：{reason}");
            }
            other => panic!("期望 Unverified，实际 {other:?}"),
        }
    }

    #[test]
    fn mismatched_known_hash_is_rejected_before_install() {
        // 1.37.0 有已知值，但内容不是那个包 → 必须直接拒绝（不是「未校验」放行）。
        let error =
            verify_hash(Aria2Version::new(1, 37, 0), 64, b"wrong").expect_err("哈希不符必须报错");
        assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
        assert_eq!(
            error.code(),
            Some(ariahost_common::ErrorCode::UpdatePackageInvalid)
        );
        assert!(error.to_string().contains("SHA-256 不符"), "{error}");
    }
}
