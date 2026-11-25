//! 下载线路：模板拼接 + 逐线路聚合失败（**不静默降级**）。
//!
//! 线路语义（架构 §4.7 / R5；M4-0 起四类来源）：`Cdn`（用户 CDN，**用户机器首选**）→
//! `Github`（直连官方 release，**CI 首选**）；`Managed` / `External` 是「本机已有核心」，
//! 不在本模块（见 [`crate::runtime`] 的 `resolve_core`）。
//!
//! 顺序来自 `net.source_order`（见 [`lines_from`]）：用户机器默认 `cdn,github`——真实用户网络里
//! GitHub 的 `release-assets.githubusercontent.com` 实测超时（真机 v0.1.2 首次运行失败的直接原因）。
//!
//! [`CI_SOURCE_ORDER`]（`github,cdn`）**目前没有生产调用者**：`release.yml` 的「内置 aria2c 核心」
//! 那一步是自带的 PowerShell 步骤，直接下载官方 zip 并对 zip 做 SHA-256 硬校验，不走本模块。
//! 保留 `ci_lines()` 是为了 CI 侧将来跑「应用内下载」时按 `github → cdn` 排序（runner 在 GitHub
//! 内网，直连最可信），**不要**在说明里写成「release.yml 已经在用」。
//!
//! 测速口径（2026-11-26，本机 rustls 直连实测，见 `.scratch/m4/`）：CDN 3266 ms / GitHub 直连
//! 2431 ms——两者都能通，说明「GitHub 必超时」只成立于用户那侧网络，不能当普遍结论。
//!
//! 任何一条线路失败都必须留下痕迹：成功时进 [`DownloadOutcome::warnings`]（UI 可见信息条），
//! 全部失败时进 [`CoreError::AllLinesFailed`]（`AH-E6007`）。

use crate::error::{CoreError, LineAttempt};
use crate::http::{self, HttpClient};

/// 直连 GitHub 的线路名。
pub const DIRECT_LINE_NAME: &str = "直连 GitHub";

/// 用户 CDN 的线路名。
pub const CDN_LINE_NAME: &str = "CDN ah.335772.xyz";

/// 用户 CDN 的默认前缀（与 `ariahost-common` 的 `[net] cdn_base` 默认值**同字面量**）。
///
/// 为什么不从这里引用到 config：依赖方向是 common ← core，config 不能反向依赖 core。
pub const DEFAULT_CDN_BASE: &str = "https://ah.335772.xyz/releases/";

/// 用户机器运行时的默认顺序：CDN 优先。
pub const DEFAULT_SOURCE_ORDER: &str = "cdn,github";

/// CI 构建期的顺序：GitHub 优先（runner 在 GitHub 内网，比 CDN 快一个数量级）。
pub const CI_SOURCE_ORDER: &str = "github,cdn";

/// `{url}` 占位符：整条资产 URL。
pub const URL_PLACEHOLDER: &str = "{url}";

/// `{file}` 占位符：资产 URL 的**最后一段**（CDN 上的文件名与官方资产完全同名）。
pub const FILE_PLACEHOLDER: &str = "{file}";

/// 一条下载线路。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadLine {
    /// 显示名（进日志/提示，例如「直连 GitHub」）。
    pub name: String,
    /// URL 模板，必须包含 [`URL_PLACEHOLDER`] 或 [`FILE_PLACEHOLDER`]。
    pub template: String,
}

impl DownloadLine {
    /// 直连线路：`{url}` → 原样。
    pub fn direct() -> Self {
        Self {
            name: DIRECT_LINE_NAME.to_owned(),
            template: URL_PLACEHOLDER.to_owned(),
        }
    }

    /// 自定义模板线路。
    pub fn new(name: impl Into<String>, template: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            template: template.into(),
        }
    }

    /// 用户 CDN 线路：`<cdn_base>/<资产文件名>`（末尾斜杠缺了会自动补）。
    pub fn cdn(base: &str) -> Self {
        let base = base.trim();
        let normalized = if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        };
        Self {
            name: CDN_LINE_NAME.to_owned(),
            template: format!("{normalized}{FILE_PLACEHOLDER}"),
        }
    }

    /// 模板是否带可用占位符（不带占位符的模板拼不出目标地址，配置校验用）。
    pub fn has_placeholder(&self) -> bool {
        self.template.contains(URL_PLACEHOLDER) || self.template.contains(FILE_PLACEHOLDER)
    }

    /// 把模板里的占位符替换成真实地址。
    ///
    /// 注意：**不做 URL 归一化/转义**——`{url}` 形态（前缀 + 完整 GitHub URL）本来就靠
    /// 原样拼接，转义反而会把它拼坏。
    ///
    /// 旧镜像线路（前缀式域名）已废弃并**删除**，勿再引入。
    pub fn join(&self, url: &str) -> String {
        let joined = self.template.replace(URL_PLACEHOLDER, url);
        if joined.contains(FILE_PLACEHOLDER) {
            joined.replace(FILE_PLACEHOLDER, asset_file_name(url))
        } else {
            joined
        }
    }
}

/// 资产 URL 的最后一段（GitHub release 资产名；官方资产 URL 不带 query）。
fn asset_file_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// 按 `net.source_order` + `net.cdn_base` 造线路。
///
/// 规则（与文档一致）：
/// - 只认 `cdn` / `github` 两个关键字，大小写不敏感；重复项只保留第一次出现；
/// - `cdn_base` 为空 → **跳过** CDN 线路（等价「只用 github」）；
/// - 未知关键字忽略并返回一条可见告警；
/// - 一个可用线路都没有 → 回退 GitHub 直连 + 一条可见告警（绝不静默降级成「无线路」）。
pub fn lines_from(order: &str, cdn_base: &str) -> (Vec<DownloadLine>, Vec<String>) {
    let base = cdn_base.trim();
    let mut lines: Vec<DownloadLine> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();

    for token in order.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.to_ascii_lowercase().as_str() {
            "cdn" => {
                if !base.is_empty() && !lines.iter().any(|line| line.name == CDN_LINE_NAME) {
                    lines.push(DownloadLine::cdn(base));
                }
            }
            "github" => {
                if !lines.iter().any(|line| line.name == DIRECT_LINE_NAME) {
                    lines.push(DownloadLine::direct());
                }
            }
            _ => unknown.push(token),
        }
    }

    let mut warnings = Vec::new();
    if !unknown.is_empty() {
        warnings.push(format!(
            "net.source_order 里的 {} 不是已知线路（只认 cdn / github），已忽略",
            unknown.join("、")
        ));
    }
    if lines.is_empty() {
        lines.push(DownloadLine::direct());
        warnings.push(format!(
            "net.source_order「{order}」里没有可用线路，已回退为 GitHub 直连"
        ));
    }

    (lines, warnings)
}

/// 用户机器运行时的默认线路：`cdn → github`。
pub fn default_lines() -> Vec<DownloadLine> {
    lines_from(DEFAULT_SOURCE_ORDER, DEFAULT_CDN_BASE).0
}

/// CI 构建期的线路：`github → cdn`（`release.yml` 的自带核心下载用）。
pub fn ci_lines() -> Vec<DownloadLine> {
    lines_from(CI_SOURCE_ORDER, DEFAULT_CDN_BASE).0
}

/// 一次成功的下载结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// zip 字节。
    pub bytes: Vec<u8>,
    /// 命中的线路名。
    pub line: String,
    /// 实际请求的 URL。
    pub url: String,
    /// 失败过但被后续线路兜住的线路说明（**必须展示**，不能丢）。
    pub warnings: Vec<String>,
}

/// 按顺序尝试各线路下载同一个 GitHub 资产。
///
/// - 任一条成功即返回，**但前面失败过的线路全部写进 `warnings`**；
/// - 全失败 → [`CoreError::AllLinesFailed`]，逐条保留线路名/URL/原文（`AH-E6007`）；
///   但若**每一条**都是载荷校验失败（`AH-E6003`，如 magic 不是 zip、大小与 release 不符），
///   说明源是通的、拿到的东西不对——此时照实报 `AH-E6003`，不能冒充「源全部不可用」，
///   否则会把用户引去换镜像/查网络，排查方向就错了。
pub fn download_zip(
    client: &HttpClient,
    lines: &[DownloadLine],
    target_url: &str,
    expected_size: Option<u64>,
) -> Result<DownloadOutcome, CoreError> {
    let mut attempts: Vec<LineAttempt> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<CoreError> = Vec::new();

    for line in lines {
        let url = line.join(target_url);
        match http::fetch_zip(client, &url, expected_size) {
            Ok(bytes) => {
                return Ok(DownloadOutcome {
                    bytes,
                    line: line.name.clone(),
                    url,
                    warnings,
                });
            }
            Err(error) => {
                warnings.push(format!("下载线路「{}」失败：{error}", line.name));
                attempts.push(LineAttempt {
                    line: line.name.clone(),
                    url,
                    detail: error.to_string(),
                });
                errors.push(error);
            }
        }
    }

    if !errors.is_empty()
        && errors
            .iter()
            .all(|error| matches!(error, CoreError::PackageInvalid { .. }))
    {
        return Err(errors.swap_remove(0));
    }

    Err(CoreError::AllLinesFailed { attempts })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSET_URL: &str = "https://github.com/aria2/aria2/releases/download/release-1.37.0/aria2-1.37.0-win-64bit-build1.zip";
    const ASSET_FILE: &str = "aria2-1.37.0-win-64bit-build1.zip";

    #[test]
    fn direct_line_keeps_url_untouched() {
        let line = DownloadLine::direct();
        assert_eq!(line.join(ASSET_URL), ASSET_URL);
        assert!(line.has_placeholder());
    }

    /// M4-0 的核心：CDN 线路只换 host 与前缀，文件名与官方资产同名。
    #[test]
    fn cdn_line_replaces_only_the_file_name() {
        let line = DownloadLine::cdn(DEFAULT_CDN_BASE);
        assert!(line.has_placeholder());
        assert_eq!(
            line.join(ASSET_URL),
            format!("https://ah.335772.xyz/releases/{ASSET_FILE}")
        );
        // 缺尾斜杠要自动补，不能拼成 `...releasesaria2-...`。
        let no_slash = DownloadLine::cdn("https://ah.335772.xyz/releases");
        assert_eq!(no_slash.join(ASSET_URL), line.join(ASSET_URL));
    }

    #[test]
    fn template_without_placeholder_cannot_join() {
        let line = DownloadLine::new("坏模板", "https://example.invalid/fixed.zip");
        assert!(!line.has_placeholder());
        // join 原样返回（配置校验应提前拦掉），绝不 panik。
        assert_eq!(line.join(ASSET_URL), "https://example.invalid/fixed.zip");
    }

    #[test]
    fn default_lines_are_cdn_then_github() {
        let lines = default_lines();
        assert_eq!(lines.len(), 2, "默认只应有 cdn / github 两条");
        assert_eq!(lines[0].name, CDN_LINE_NAME);
        assert_eq!(lines[1].name, DIRECT_LINE_NAME);
    }

    #[test]
    fn ci_lines_are_github_then_cdn() {
        let lines = ci_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert_eq!(lines[1].name, CDN_LINE_NAME);
    }

    /// 旧镜像线路（`r.alusss.cn` 前缀模板）已删除，这条测试是**删除的守护**：
    /// 谁把镜像线路加回任何一条默认线路里，这里会先红（而不是等用户装不上才发现）。
    #[test]
    fn no_legacy_mirror_line_is_generated() {
        for order in ["cdn,github", "github,cdn", "cdn", "github"] {
            let (lines, _) = lines_from(order, "https://cdn.example.invalid/r/");
            assert!(!lines.is_empty(), "顺序 {order} 不该一条线路都没有");
            for line in &lines {
                assert!(
                    !line.template.contains("r.alusss.cn"),
                    "顺序 {order}：旧镜像域名不得再出现在线路模板里（{} => {}）",
                    line.name,
                    line.template
                );
            }
        }
    }

    /// `cdn_base` 为空 → 只走 github（任务第 5 条的单测之一）。
    #[test]
    fn empty_cdn_base_keeps_only_github() {
        let (lines, warnings) = lines_from("cdn,github", "   ");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert!(warnings.is_empty(), "跳过 CDN 是配置意图，不该告警");
    }

    /// 非法值 → 回退默认 + 一条告警（任务第 5 条的单测之一）。
    #[test]
    fn unknown_tokens_warn_and_are_ignored() {
        let (lines, warnings) = lines_from("gerrit, github", DEFAULT_CDN_BASE);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("gerrit"), "{}", warnings[0]);
    }

    /// 一个可用线路都没有 → 回退 github + 告警，绝不返回空线路表。
    #[test]
    fn empty_order_falls_back_to_github_with_warning() {
        let (lines, warnings) = lines_from(" , ", "");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("回退"), "{}", warnings[0]);
    }

    #[test]
    fn duplicate_tokens_are_deduped_in_order() {
        let (lines, warnings) = lines_from("github,cdn,github,github", DEFAULT_CDN_BASE);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert_eq!(lines[1].name, CDN_LINE_NAME);
        assert!(warnings.is_empty());
    }

    #[test]
    fn join_handles_multiple_placeholders() {
        let line = DownloadLine::new("双占位", "{url}?copy={url}");
        assert_eq!(line.join("https://x/y"), "https://x/y?copy=https://x/y");
    }
}
