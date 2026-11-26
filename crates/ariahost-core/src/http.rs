//! HTTP 客户端（ureq + rustls），以及 API/安装包两类载荷的断言。
//!
//! ## 为什么是 ureq（而不是 WinHTTP / reqwest）
//!
//! - **全仓只允许一条 HTTP/TLS 栈**。`ureq 3.x` 的 `rustls` feature 自带
//!   ring（C 后端，静态链接）+ `webpki-roots`（编译期内置 Mozilla 根证书），不碰系统证书库；
//!   默认关闭 gzip（HTTP 层不解压，避免把「压缩炸弹」变成我们的内存问题）。
//! - **WinHTTP 被否的具体理由**（不是「不安全」，而是验证成本）：
//!   WinHTTP 只能是 `#[cfg(windows)]` 代码，而本仓库的**本地验证通道是 Linux 侧的
//!   `cargo clippy/test`**（WSL），Windows 专属分支在本地一行都编译不到——M1-2A 已经吃过
//!   「本机无链接器、只能靠 CI 试错」的教训（5 轮推送）。用 ureq 则同一份代码在 Linux 上
//!   就被 clippy/假服务器测试覆盖。它的代价是：不读 Windows 证书库（企业 MITM 代理的证书
//!   校验会失败）——按设计文档，下载线路本来就允许失败并给出 `AH-E6001` 可见提示，
//!   不需要为本功能引入 WinHTTP 的 ~300 行 unsafe。
//! - 明确不引 reqwest（tokio 异步栈与整仓单线程模型不匹配，且体积远大于 ureq）。
//!
//! ## 测试如何做到「不依赖真实网络」
//!
//! [`HttpClient::with_options`] 可以被测试改成允许明文 HTTP，于是测试全部指向
//! `127.0.0.1:<随机端口>` 的线程内假服务器（`tests/` 里）。生产默认 `https_only(true)`。

use std::time::Duration;

use ureq::Agent;

use crate::error::CoreError;
use crate::net::ProbeTransport;

/// 单次请求的总超时（连接 + 收响应 + 收 body）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认跟随的重定向次数（GitHub release 资产会 302 到 objects.githubusercontent.com）。
pub const DEFAULT_MAX_REDIRECTS: u32 = 5;

/// 响应体硬上限（防对端灌内存）。
pub const DEFAULT_MAX_BODY: u64 = 64 * 1024 * 1024;

/// releases API 的合理上限：24 个 release 的 JSON 只有几十 KB，8 MiB 已是异常。
pub const MAX_JSON_BODY: u64 = 8 * 1024 * 1024;

/// zip 魔数（本地文件头）。
pub const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";

/// HTTP 客户端选项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpOptions {
    /// 是否允许明文 HTTP（**只给测试用**；生产必须 false）。
    pub allow_plain_http: bool,
    /// 单次请求总超时。
    pub timeout: Duration,
    /// 最大重定向次数。
    pub max_redirects: u32,
    /// 响应体硬上限。
    pub max_body: u64,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            allow_plain_http: false,
            timeout: DEFAULT_TIMEOUT,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_body: DEFAULT_MAX_BODY,
        }
    }
}

/// 一次响应（状态码 + 关心的头 + body）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    /// 状态码。
    pub status: u16,
    /// `Content-Type` 原文。
    pub content_type: Option<String>,
    /// `Content-Length`（有则用于长度对账）。
    pub content_length: Option<u64>,
    /// 响应体。
    pub body: Vec<u8>,
}

impl HttpReply {
    /// 状态码是否 2xx。
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// 非 2xx 时给出错误：429 或 403+rate limit 文案 → 限流；其余 → HTTP 状态。
    pub fn failure(&self) -> CoreError {
        let text = String::from_utf8_lossy(&self.body);
        let lowered = text.to_ascii_lowercase();
        if self.status == 429 || (self.status == 403 && lowered.contains("rate limit")) {
            return CoreError::RateLimited {
                detail: format!("HTTP {} {}", self.status, truncate(&text)),
            };
        }
        CoreError::HttpStatus {
            status: self.status,
        }
    }
}

/// HTTP 客户端。
///
/// 内部持有可复用的 [`Agent`]（连接池对它自己管）与选项；`Clone` 成本很低。
#[derive(Debug, Clone)]
pub struct HttpClient {
    agent: Agent,
    options: HttpOptions,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    /// 生产默认：HTTPS-only、30s 超时、最多 5 次重定向。
    pub fn new() -> Self {
        Self::with_options(HttpOptions::default())
    }

    /// 指定选项（测试用 `allow_plain_http: true` 指向本地假服务器）。
    pub fn with_options(options: HttpOptions) -> Self {
        let config = Agent::config_builder()
            .timeout_global(Some(options.timeout))
            .timeout_connect(Some(options.timeout))
            .https_only(!options.allow_plain_http)
            .max_redirects(options.max_redirects)
            // 4xx/5xx 不转成 Error：我们要读响应体判断是不是限流。
            .http_status_as_error(false)
            .build();
        Self {
            agent: Agent::new_with_config(config),
            options,
        }
    }

    /// 当前选项。
    pub fn options(&self) -> &HttpOptions {
        &self.options
    }

    /// 发一次 GET，返回状态、头与 body（**不**对状态码报错，由调用方分类）。
    pub fn get(&self, url: &str) -> Result<HttpReply, CoreError> {
        let mut response = self.agent.get(url).call().map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        let content_type = header_text(response.headers().get("content-type"));
        let content_length = header_text(response.headers().get("content-length"))
            .and_then(|value| value.trim().parse::<u64>().ok());
        let body = response
            .body_mut()
            .with_config()
            .limit(self.options.max_body)
            .read_to_vec()
            .map_err(map_ureq_error)?;
        Ok(HttpReply {
            status,
            content_type,
            content_length,
            body,
        })
    }

    /// 发一次 HEAD，只取状态码（`设置页「测一下」`的可用性探测用）。
    ///
    /// 为什么单独一个方法 + 单独的超时：探活要比常规请求短得多——用户按「测一下」
    /// 不该等 30s；而且探活**绝不能**读 body（镜像站可能返回 HTML 拦截页，
    /// 但探活只关心「能不能到达」，格式判断交给真正的下载路径）。
    pub fn head_status(&self, url: &str, timeout: Duration) -> Result<u16, ProbeTransport> {
        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .timeout_connect(Some(timeout))
            .timeout_recv_response(Some(timeout))
            .https_only(!self.options.allow_plain_http)
            .max_redirects(self.options.max_redirects)
            .http_status_as_error(false)
            .build();
        let agent = Agent::new_with_config(config);
        match agent.head(url).call() {
            Ok(response) => Ok(response.status().as_u16()),
            Err(error) => Err(map_probe_error(error)),
        }
    }
}

/// GET 一个 JSON 端点（GitHub releases API）。
///
/// 断言链：2xx → 长度对账 → `Content-Type` 含 `json`（缺失则退化为首字符判定）→ 首字符 `{`/`[`。
pub fn fetch_json(client: &HttpClient, url: &str) -> Result<Vec<u8>, CoreError> {
    let reply = client.get(url)?;
    if !reply.is_success() {
        return Err(reply.failure());
    }
    if let Some(length) = reply.content_length {
        if length as usize != reply.body.len() {
            return Err(CoreError::BadPayload {
                detail: format!(
                    "Content-Length 声明 {length} 字节，实收 {} 字节",
                    reply.body.len()
                ),
            });
        }
    }
    if let Some(content_type) = &reply.content_type {
        if !content_type.to_ascii_lowercase().contains("json") {
            return Err(CoreError::BadPayload {
                detail: format!("Content-Type 是 {content_type}，不是 JSON（多半是代理/镜像页）"),
            });
        }
    }
    let first = first_non_space(&reply.body);
    match first {
        Some(b'{') | Some(b'[') => Ok(reply.body),
        other => Err(CoreError::BadPayload {
            detail: format!("响应首字节不是 {{ 或 [，而是 {other:?}"),
        }),
    }
}

/// GET 一个 aria2 Windows 安装包。
///
/// 断言链（顺序即 task-30 要求的校验顺序）：2xx → Content-Type 不是 html →
/// `Content-Length`/API 声明大小与实收一致 → zip 魔数 `PK\x03\x04`。
/// **哈希校验不在这里**：需要版本号与位数，由 [`crate::fetch`] 在解压前做。
pub fn fetch_zip(
    client: &HttpClient,
    url: &str,
    expected_size: Option<u64>,
) -> Result<Vec<u8>, CoreError> {
    let reply = client.get(url)?;
    if !reply.is_success() {
        return Err(reply.failure());
    }
    if let Some(content_type) = &reply.content_type {
        if content_type.to_ascii_lowercase().contains("html") {
            return Err(CoreError::BadPayload {
                detail: format!("Content-Type 是 {content_type}：线路返回的是网页而不是 zip"),
            });
        }
    }
    if let Some(length) = reply.content_length {
        if length as usize != reply.body.len() {
            return Err(CoreError::package(format!(
                "Content-Length 声明 {length} 字节，实收 {} 字节",
                reply.body.len()
            )));
        }
        if let Some(expected) = expected_size {
            if length != expected {
                return Err(CoreError::package(format!(
                    "Content-Length {length} 与 release 资产大小 {expected} 不一致"
                )));
            }
        }
    }
    if let Some(expected) = expected_size {
        if reply.body.len() as u64 != expected {
            return Err(CoreError::package(format!(
                "实际收到 {} 字节，release 资产大小是 {expected} 字节",
                reply.body.len()
            )));
        }
    }
    if !reply.body.starts_with(ZIP_MAGIC) {
        return Err(CoreError::package(format!(
            "magic 不是 PK\\x03\\x04（前 4 字节：{:02x?}）",
            &reply.body[..reply.body.len().min(4)]
        )));
    }
    Ok(reply.body)
}

/// 取头部的文本值（非 UTF-8 或缺失都当没有）。
fn header_text(value: Option<&ureq::http::HeaderValue>) -> Option<String> {
    value
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// 第一个非空白字节。
fn first_non_space(body: &[u8]) -> Option<u8> {
    body.iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

/// 截断超长文本（错误信息里塞整页 HTML 没有意义）。
fn truncate(text: &str) -> String {
    const LIMIT: usize = 200;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…")
}

/// `ureq::Error` → 探测传输失败分类。
///
/// 超时与「连不上」必须分开：前者多半是代理不可用（建议换地址），
/// 后者多半是压根没走对线路（建议切换直连/代理）。混成一个会给出错误建议。
fn map_probe_error(error: ureq::Error) -> ProbeTransport {
    let text = error.to_string();
    match error {
        ureq::Error::Timeout(_) => ProbeTransport::Timeout(text),
        _ => ProbeTransport::Unreachable(text),
    }
}

/// `ureq::Error` → [`CoreError`]。
fn map_ureq_error(error: ureq::Error) -> CoreError {
    match error {
        // 被响应体上限截断：这是载荷异常，不是网络。
        ureq::Error::BodyExceedsLimit(limit) => {
            CoreError::package(format!("响应体超过上限 {limit} 字节"))
        }
        ureq::Error::StatusCode(status) => CoreError::HttpStatus { status },
        ureq::Error::HostNotFound => CoreError::network("域名解析失败"),
        ureq::Error::RequireHttpsOnly(url) => {
            CoreError::network(format!("请求被强制 HTTPS 拒绝：{url}"))
        }
        other => CoreError::network(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16, body: &str) -> HttpReply {
        HttpReply {
            status,
            content_type: None,
            content_length: None,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn non_2xx_classification_splits_rate_limit() {
        let error = reply(429, "too many requests").failure();
        assert!(matches!(error, CoreError::RateLimited { .. }), "{error}");

        // GitHub 的限流实际是 403 + message
        let error = reply(403, r#"{"message":"API rate limit exceeded for 1.2.3.4"}"#).failure();
        assert!(matches!(error, CoreError::RateLimited { .. }), "{error}");

        // 普通 403（无 rate limit 文案）→ HTTP 状态
        let error = reply(403, "Forbidden").failure();
        assert!(
            matches!(error, CoreError::HttpStatus { status: 403 }),
            "{error}"
        );

        let error = reply(503, "unavailable").failure();
        assert!(
            matches!(error, CoreError::HttpStatus { status: 503 }),
            "{error}"
        );
    }

    #[test]
    fn success_predicate_covers_only_2xx() {
        assert!(reply(200, "").is_success());
        assert!(reply(204, "").is_success());
        assert!(!reply(301, "").is_success());
        assert!(!reply(404, "").is_success());
    }

    #[test]
    fn first_non_space_skips_whitespace() {
        assert_eq!(first_non_space(b"  \r\n\t{\"a\":1}"), Some(b'{'));
        assert_eq!(first_non_space(b"["), Some(b'['));
        assert_eq!(first_non_space(b"   "), None);
        assert_eq!(first_non_space(b""), None);
    }

    #[test]
    fn json_payload_assertions() {
        // 好载荷：首字节 { ，Content-Type 缺失也接受。
        let body = b"{\"ok\":true}";
        assert_eq!(first_non_space(body), Some(b'{'));

        // html 首字节 → 不是 JSON。
        assert_ne!(first_non_space(b"<html>"), Some(b'{'));
        assert_ne!(first_non_space(b"<html>"), Some(b'['));
    }

    #[test]
    fn truncate_keeps_short_text_and_bounds_long_text() {
        assert_eq!(truncate("short"), "short");
        let long = "x".repeat(500);
        let cut = truncate(&long);
        assert!(cut.chars().count() <= 201, "{}", cut.chars().count());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn default_options_are_https_only_with_bounded_limits() {
        let options = HttpOptions::default();
        assert!(!options.allow_plain_http);
        assert_eq!(options.timeout, DEFAULT_TIMEOUT);
        assert_eq!(options.max_redirects, DEFAULT_MAX_REDIRECTS);
        assert!(options.max_body >= 32 * 1024 * 1024, "要装得下 ~5MB 的 zip");
    }
}
