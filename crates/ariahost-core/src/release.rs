//! GitHub release 渠道：载荷解析、资产名白名单、架构选择、更新状态。
//!
//! 这里只做**纯逻辑**（不联网、不落盘），联网在 [`crate::http`]，编排在 [`crate::fetch`]。
//!
//! 白名单**不用正则 crate**：模式是固定的五段结构
//! （`aria2-<数字>.<数字>.<数字>-win-<32|64>bit-build<数字>.zip`），手写解析既零依赖又能
//! 精确表达「多一段就不匹配」——这正好把 `-debug`、`-minimal` 之类变体挡在外面，
//! 而正则里的 `.*` 很容易放它们进来。

use ariahost_aria2::{parse_version_number, Aria2Version};
use serde::Deserialize;

use crate::error::CoreError;

/// 最低支持的 aria2 版本（架构 §4.11/R1：能力用运行时探测，但低于此版本直接拒绝）。
pub const MIN_SUPPORTED: Aria2Version = Aria2Version::new(1, 19, 1);

/// GitHub releases API（一次返回全部 24 个版本，无翻页；未认证限速 60 次/时/IP）。
pub const RELEASES_API: &str = "https://api.github.com/repos/aria2/aria2/releases?per_page=100";

/// 从 release tag 取出 aria2 版本号：`release-1.37.0` → `1.37.0`。
///
/// 标签前缀是 `release-`（GitHub 上 aria2 的历史标签都如此）；不带前缀时也接受纯版本号，
/// 解析失败返回 `None`——**上游多出一个怪标签不该让整次检查失败**，调用方跳过即可。
pub fn release_version(tag: &str) -> Option<Aria2Version> {
    let text = tag.trim();
    let text = text.strip_prefix("release-").unwrap_or(text);
    parse_version_number(text).ok()
}

/// 匹配到的 Windows 资产（白名单形状 + 版本 + 位数 + build 号）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinAsset {
    /// 资产文件名（原样）。
    pub name: String,
    /// 资产里的 aria2 版本。
    pub version: Aria2Version,
    /// 32 或 64。
    pub bits: u32,
    /// build 号（`build1`/`build2`…；**不假定为 1**，1.23.0 只有 build2）。
    pub build: u32,
}

/// 白名单匹配：`aria2-<ver>-win-<32|64>bit-build<n>.zip`，且**不允许任何多余段**。
///
/// 多余段一律不匹配，因此 `...-build1-debug.zip` 这类调试包天然被过滤掉。
pub fn match_win_asset(name: &str) -> Option<WinAsset> {
    let stem = name.strip_suffix(".zip")?;
    let rest = stem.strip_prefix("aria2-")?;
    let mut parts = rest.split('-');

    let version_text = parts.next()?;
    if parts.next()? != "win" {
        return None;
    }
    let bits_text = parts.next()?;
    let build_text = parts.next()?;
    // 多出任何一段（-debug/-minimal/...）都不算白名单。
    if parts.next().is_some() {
        return None;
    }

    let bits = match bits_text {
        "32bit" => 32,
        "64bit" => 64,
        _ => return None,
    };
    let build = build_text.strip_prefix("build")?.parse::<u32>().ok()?;
    if build == 0 {
        return None;
    }
    let version = parse_version_number(version_text).ok()?;

    Some(WinAsset {
        name: name.to_owned(),
        version,
        bits,
        build,
    })
}

/// 本机构的位数（`cfg(target_pointer_width)`：32 位进程取 32 位包）。
pub const fn host_bits() -> u32 {
    if cfg!(target_pointer_width = "64") {
        64
    } else {
        32
    }
}

/// GitHub release 里的一个资产（只取用得上的字段）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseAsset {
    /// 文件名。
    pub name: String,
    /// 字节数（用于和 Content-Length/实收长度对账）。
    #[serde(default)]
    pub size: u64,
    /// 下载地址。
    #[serde(rename = "browser_download_url")]
    pub browser_download_url: String,
}

/// GitHub releases API 的单个 release（只取用得上的字段）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitHubRelease {
    /// 形如 `release-1.37.0`。
    pub tag_name: String,
    /// 发布时间（RFC3339 字符串，原样保留；本批不引时间库）。
    #[serde(default)]
    pub published_at: Option<String>,
    /// 资产列表。
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// 归一化后的一个版本条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseEntry {
    /// 版本号。
    pub version: Aria2Version,
    /// 原始 tag。
    pub tag_name: String,
    /// 发布时间原文。
    pub published_at: Option<String>,
    /// 原始资产列表（含非 Windows 资产，选择时再过滤）。
    pub assets: Vec<ReleaseAsset>,
}

impl ReleaseEntry {
    fn from_github(release: GitHubRelease) -> Option<Self> {
        let version = release_version(&release.tag_name)?;
        if version < MIN_SUPPORTED {
            return None;
        }
        Some(Self {
            version,
            tag_name: release.tag_name,
            published_at: release.published_at,
            assets: release.assets,
        })
    }

    /// 该版本里匹配本机位数的资产；多个 build 时取 **build 号最大** 的那个。
    pub fn asset_for(&self, bits: u32) -> Option<&ReleaseAsset> {
        self.assets
            .iter()
            .filter_map(|asset| match_win_asset(&asset.name).map(|win| (win, asset)))
            .filter(|(win, _)| win.version == self.version && win.bits == bits)
            .max_by_key(|(win, _)| win.build)
            .map(|(_, asset)| asset)
    }
}

/// 版本目录（按版本号降序）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleaseCatalog {
    entries: Vec<ReleaseEntry>,
}

impl ReleaseCatalog {
    /// 解析 API 返回的 JSON 数组。
    ///
    /// 非数组/非法 JSON → [`CoreError::BadPayload`]（这正是「代理返回非 JSON」那一路）。
    pub fn parse_json(body: &[u8]) -> Result<Self, CoreError> {
        let releases: Vec<GitHubRelease> =
            serde_json::from_slice(body).map_err(|error| CoreError::BadPayload {
                detail: format!("releases JSON 解析失败（{} 字节）：{error}", body.len()),
            })?;
        let mut entries: Vec<ReleaseEntry> = releases
            .into_iter()
            .filter_map(ReleaseEntry::from_github)
            .collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.version));
        Ok(Self { entries })
    }

    /// 目录里的全部版本（降序）。
    pub fn versions(&self) -> impl Iterator<Item = Aria2Version> + '_ {
        self.entries.iter().map(|entry| entry.version)
    }

    /// 条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（clippy 的 `len_without_is_empty` 要求成对出现）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 最新版本（目录已按版本降序）。
    pub fn latest(&self) -> Option<&ReleaseEntry> {
        self.entries.first()
    }

    /// 查某个版本。
    pub fn find(&self, version: Aria2Version) -> Option<&ReleaseEntry> {
        self.entries.iter().find(|entry| entry.version == version)
    }

    /// 查某个版本 + 位数的资产。
    pub fn asset_for(&self, version: Aria2Version, bits: u32) -> Option<&ReleaseAsset> {
        self.find(version).and_then(|entry| entry.asset_for(bits))
    }
}

/// 「检查更新」的四态结果（架构 §4.7：四态文案绝不合并）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateState {
    /// 没有更新（**常态**：aria2 上游发布极慢）。
    NoUpdate {
        /// 当前版本。
        current: Aria2Version,
        /// 上游最新版本（可能等于 `current`）。
        latest: Aria2Version,
    },
    /// 有更新。
    UpdateAvailable {
        /// 当前版本。
        current: Aria2Version,
        /// 上游最新版本。
        latest: Aria2Version,
    },
}

impl UpdateState {
    /// 是否有更新。
    pub const fn has_update(self) -> bool {
        matches!(self, Self::UpdateAvailable { .. })
    }
}

/// 比对当前版本与目录里的最新版本。
pub fn check_update(catalog: &ReleaseCatalog, current: Aria2Version) -> UpdateState {
    let latest = catalog.latest().map_or(current, |entry| entry.version);
    if latest > current {
        UpdateState::UpdateAvailable { current, latest }
    } else {
        UpdateState::NoUpdate { current, latest }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_release_asset_names() {
        let win = match_win_asset("aria2-1.37.0-win-64bit-build1.zip").expect("应匹配");
        assert_eq!(win.version, Aria2Version::new(1, 37, 0));
        assert_eq!(win.bits, 64);
        assert_eq!(win.build, 1);
        assert_eq!(win.name, "aria2-1.37.0-win-64bit-build1.zip");

        // 1.23.0 只有 build2：不得假定 build1。
        let win = match_win_asset("aria2-1.23.0-win-64bit-build2.zip").expect("build2 应匹配");
        assert_eq!(win.build, 2);
        let win = match_win_asset("aria2-1.19.1-win-32bit-build1.zip").expect("32 位应匹配");
        assert_eq!(win.bits, 32);
    }

    #[test]
    fn rejects_non_whitelisted_names() {
        for name in [
            // 调试包：多一段，必须挡掉。
            "aria2-1.37.0-win-64bit-build1-debug.zip",
            // 源码包/源码归档。
            "aria2-1.37.0.tar.gz",
            "aria2-1.37.0.tar.bz2",
            // 位数写法不对。
            "aria2-1.37.0-win-x64-build1.zip",
            "aria2-1.37.0-win-64bit.zip",
            // build 段缺失/非数字/为 0。
            "aria2-1.37.0-win-64bit-build.zip",
            "aria2-1.37.0-win-64bit-buildx.zip",
            "aria2-1.37.0-win-64bit-build0.zip",
            // 版本段不合法。
            "aria2-1.37-win-64bit-build1.zip",
            "aria2-v1.37.0-win-64bit-build1.zip",
            // 前缀/后缀不对。
            "aria2c-1.37.0-win-64bit-build1.zip",
            "aria2-1.37.0-win-64bit-build1.zip.sig",
            "",
        ] {
            assert!(match_win_asset(name).is_none(), "{name} 不应匹配");
        }
    }

    #[test]
    fn parses_release_tags() {
        assert_eq!(
            release_version("release-1.37.0"),
            Some(Aria2Version::new(1, 37, 0))
        );
        assert_eq!(release_version("1.19.1"), Some(Aria2Version::new(1, 19, 1)));
        assert_eq!(release_version("release-1.37"), None);
        assert_eq!(release_version("nightly"), None);
    }

    const SAMPLE_JSON: &str = r#"[
      {
        "tag_name": "release-1.37.0",
        "published_at": "2023-11-15T13:05:48Z",
        "assets": [
          {"name": "aria2-1.37.0.tar.gz", "size": 100, "browser_download_url": "https://example.invalid/src.tar.gz"},
          {"name": "aria2-1.37.0-win-32bit-build1.zip", "size": 111, "browser_download_url": "https://example.invalid/32.zip"},
          {"name": "aria2-1.37.0-win-64bit-build1.zip", "size": 222, "browser_download_url": "https://example.invalid/64.zip"},
          {"name": "aria2-1.37.0-win-64bit-build1-debug.zip", "size": 333, "browser_download_url": "https://example.invalid/64-debug.zip"}
        ]
      },
      {
        "tag_name": "release-1.36.0",
        "published_at": "2022-08-01T00:00:00Z",
        "assets": [
          {"name": "aria2-1.36.0-win-64bit-build1.zip", "size": 444, "browser_download_url": "https://example.invalid/136.zip"}
        ]
      },
      {
        "tag_name": "release-1.23.0",
        "published_at": "2018-06-01T00:00:00Z",
        "assets": [
          {"name": "aria2-1.23.0-win-64bit-build2.zip", "size": 555, "browser_download_url": "https://example.invalid/123.zip"}
        ]
      },
      {
        "tag_name": "release-1.18.0",
        "assets": [
          {"name": "aria2-1.18.0-win-64bit-build1.zip", "size": 666, "browser_download_url": "https://example.invalid/118.zip"}
        ]
      },
      {
        "tag_name": "not-a-version",
        "assets": []
      }
    ]"#;

    #[test]
    fn parses_catalog_sorted_and_filtered() {
        let catalog = ReleaseCatalog::parse_json(SAMPLE_JSON.as_bytes()).expect("应可解析");
        // 1.18.0 低于最低支持、not-a-version 无法解析 → 都被过滤。
        assert_eq!(catalog.len(), 3);
        assert_eq!(
            catalog.versions().collect::<Vec<_>>(),
            vec![
                Aria2Version::new(1, 37, 0),
                Aria2Version::new(1, 36, 0),
                Aria2Version::new(1, 23, 0)
            ]
        );
        assert_eq!(
            catalog.latest().map(|entry| entry.version),
            Some(Aria2Version::new(1, 37, 0))
        );
    }

    #[test]
    fn selects_asset_by_arch_and_skips_debug() {
        let catalog = ReleaseCatalog::parse_json(SAMPLE_JSON.as_bytes()).expect("应可解析");
        let asset = catalog
            .asset_for(Aria2Version::new(1, 37, 0), 64)
            .expect("64 位应有资产");
        assert_eq!(asset.name, "aria2-1.37.0-win-64bit-build1.zip");
        assert_eq!(asset.size, 222);

        let asset = catalog
            .asset_for(Aria2Version::new(1, 37, 0), 32)
            .expect("32 位应有资产");
        assert_eq!(asset.name, "aria2-1.37.0-win-32bit-build1.zip");

        // 1.23.0 只有 build2，仍应选中。
        let asset = catalog
            .asset_for(Aria2Version::new(1, 23, 0), 64)
            .expect("1.23.0 的 build2 应被选中");
        assert_eq!(asset.name, "aria2-1.23.0-win-64bit-build2.zip");

        // 没有的版本 → None。
        assert!(catalog.asset_for(Aria2Version::new(1, 20, 0), 64).is_none());
    }

    #[test]
    fn picks_highest_build_when_several_exist() {
        let json = r#"[{
            "tag_name": "release-1.37.0",
            "assets": [
              {"name": "aria2-1.37.0-win-64bit-build1.zip", "size": 1, "browser_download_url": "https://example.invalid/1.zip"},
              {"name": "aria2-1.37.0-win-64bit-build3.zip", "size": 3, "browser_download_url": "https://example.invalid/3.zip"},
              {"name": "aria2-1.37.0-win-64bit-build2.zip", "size": 2, "browser_download_url": "https://example.invalid/2.zip"}
            ]
        }]"#;
        let catalog = ReleaseCatalog::parse_json(json.as_bytes()).expect("应可解析");
        let asset = catalog
            .asset_for(Aria2Version::new(1, 37, 0), 64)
            .expect("应有资产");
        assert_eq!(asset.name, "aria2-1.37.0-win-64bit-build3.zip");
    }

    #[test]
    fn rejects_non_array_and_non_json_payloads() {
        for body in [
            &br#"{"message":"API rate limit exceeded"}"#[..],
            b"<html>404</html>",
            b"",
            b"[",
        ] {
            let error = ReleaseCatalog::parse_json(body).expect_err("必须报 BadPayload");
            assert!(
                matches!(error, CoreError::BadPayload { .. }),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn update_state_distinguishes_both_cases() {
        let catalog = ReleaseCatalog::parse_json(SAMPLE_JSON.as_bytes()).expect("应可解析");

        let state = check_update(&catalog, Aria2Version::new(1, 36, 0));
        assert_eq!(
            state,
            UpdateState::UpdateAvailable {
                current: Aria2Version::new(1, 36, 0),
                latest: Aria2Version::new(1, 37, 0)
            }
        );
        assert!(state.has_update());

        let state = check_update(&catalog, Aria2Version::new(1, 37, 0));
        assert_eq!(
            state,
            UpdateState::NoUpdate {
                current: Aria2Version::new(1, 37, 0),
                latest: Aria2Version::new(1, 37, 0)
            }
        );
        assert!(!state.has_update());

        // 当前版本比目录里的还新（本地私有构建）→ 无更新。
        let state = check_update(&catalog, Aria2Version::new(1, 38, 0));
        assert!(!state.has_update());
    }

    #[test]
    fn host_bits_matches_pointer_width() {
        let bits = host_bits();
        assert!(bits == 32 || bits == 64, "{bits}");
        assert_eq!(bits, u32::from(cfg!(target_pointer_width = "64")) * 32 + 32);
    }
}
