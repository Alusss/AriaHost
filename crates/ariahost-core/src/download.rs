//! 下载线路：模板拼接 + 逐线路聚合失败（**不静默降级**）。
//!
//! 线路语义（架构 §4.7 / R5）：默认**直连** GitHub；镜像前缀只作为**可配模板**存在
//! （`r.alusss.cn` 2026-11-22 实测整体不可用，不能当默认可用线路卖）。任何一条线路失败都
//! 必须留下痕迹：成功时进 [`DownloadOutcome::warnings`]（UI 可见信息条），全部失败时进
//! [`CoreError::AllLinesFailed`]（`AH-E6007`）。

use crate::error::{CoreError, LineAttempt};
use crate::http::{self, HttpClient};

/// 直连 GitHub 的线路名。
pub const DIRECT_LINE_NAME: &str = "直连 GitHub";

/// 镜像线路的默认模板（`{url}` 会被替换成完整 GitHub 资产 URL）。
///
/// **实测不可用**，只作为可配模板的默认值存在；不要把它当成「可用的备份线路」。
pub const DEFAULT_MIRROR_TEMPLATE: &str = "https://r.alusss.cn/{url}";

/// 镜像线路的默认显示名。
pub const MIRROR_LINE_NAME: &str = "镜像 r.alusss.cn";

/// `{url}` 占位符。
pub const URL_PLACEHOLDER: &str = "{url}";

/// 一条下载线路。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadLine {
    /// 显示名（进日志/提示，例如「直连 GitHub」）。
    pub name: String,
    /// URL 模板，必须包含 `{url}`。
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

    /// 默认镜像模板线路（可配入口的初始值）。
    pub fn default_mirror() -> Self {
        Self::new(MIRROR_LINE_NAME, DEFAULT_MIRROR_TEMPLATE)
    }

    /// 模板是否带 `{url}` 占位符（没有占位符的模板拼不出目标地址，配置校验用）。
    pub fn has_placeholder(&self) -> bool {
        self.template.contains(URL_PLACEHOLDER)
    }

    /// 把模板里的 `{url}` 替换成真实地址。
    ///
    /// 注意：**不做 URL 归一化/转义**——镜像前缀本来就是「前缀 + 完整 GitHub URL」的形态
    /// （`https://r.alusss.cn/https://github.com/...`），转义反而会把它拼坏。
    pub fn join(&self, url: &str) -> String {
        self.template.replace(URL_PLACEHOLDER, url)
    }
}

/// 默认线路顺序：直连优先，镜像其次。
pub fn default_lines() -> Vec<DownloadLine> {
    vec![DownloadLine::direct(), DownloadLine::default_mirror()]
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

    #[test]
    fn direct_line_keeps_url_untouched() {
        let line = DownloadLine::direct();
        assert_eq!(line.join(ASSET_URL), ASSET_URL);
        assert!(line.has_placeholder());
    }

    #[test]
    fn mirror_template_prefixes_full_url() {
        let line = DownloadLine::default_mirror();
        assert!(line.has_placeholder());
        assert_eq!(
            line.join(ASSET_URL),
            format!("https://r.alusss.cn/{ASSET_URL}")
        );
        // 前缀形态与侦察记录一致：镜像后面跟的是**完整** GitHub URL。
        assert!(line
            .join(ASSET_URL)
            .starts_with("https://r.alusss.cn/https://github.com/"));
    }

    #[test]
    fn template_without_placeholder_cannot_join() {
        let line = DownloadLine::new("坏模板", "https://example.invalid/fixed.zip");
        assert!(!line.has_placeholder());
        // join 原样返回（配置校验应提前拦掉），绝不 panik。
        assert_eq!(line.join(ASSET_URL), "https://example.invalid/fixed.zip");
    }

    #[test]
    fn default_lines_order_is_direct_then_mirror() {
        let lines = default_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, DIRECT_LINE_NAME);
        assert_eq!(lines[1].name, MIRROR_LINE_NAME);
    }

    #[test]
    fn join_handles_multiple_placeholders() {
        let line = DownloadLine::new("双占位", "{url}?copy={url}");
        assert_eq!(line.join("https://x/y"), "https://x/y?copy=https://x/y");
    }
}
