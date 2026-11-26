//! 「AriaHost 自己访问 GitHub」的线路层（M5-b）：URL 拼接 / 故障三分类 / 可用性探测。
//!
//! # 为什么要单独一层
//!
//! 用户原话要求三件事，且**判定只能有一处**：
//! 1. 直连不好能切代理（前缀式，如 `https://ghproxy.net/`）；
//! 2. 切换要带格式校验 + 可用性测试；
//! 3. 失败时要能说「下一步该干什么」——直连失败建议切代理，代理失败建议换代理地址，
//!    而**载荷坏/限流绝不能错误地建议换代理**（越换越糟，还掩盖真因）。
//!
//! 所以：URL 拼在这一层、故障分类在这一层、hint 也由这一层出；
//! 设置页/CLI/引导页只**取用**，不许各写一套判断。
//!
//! # 与 `net.proxy` / `net.cdn_base` 的分工
//!
//! | 键 | 作用域 |
//! |---|---|
//! | `net.core_proxy_mode` + `net.core_proxy_url` | **本模块**：AriaHost 自己拉版本列表/资产/探活时的前缀代理 |
//! | `net.proxy` | aria2c 自己下载文件时用的代理（`aria2.conf` 渲染），与核心获取无关 |
//! | `net.cdn_base` | aria2 二进制的镜像前缀（`{base}{资产名}`），与通用 gh 代理形态不同 |

use std::time::{Duration, Instant};

use ariahost_common::config::{normalize_proxy_base, CoreProxyMode, DEFAULT_CORE_PROXY_URL};

use crate::error::CoreError;

/// 可用性探测的默认端点：GitHub 的「最新 release」稳定跳转端点。
///
/// 为什么是它而不是某个具体资产：资产名随版本变，而 `releases/latest` 只要求仓库存在；
/// 探测只关心「这条线路能不能到 GitHub」，不关心下什么。
pub const PROBE_URL: &str = "https://github.com/aria2/aria2/releases";

/// 探测默认超时（用户可注入更短的值，测试用假传输）。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------
// 1) URL 拼接：唯一判定点
// ---------------------------------------------------------------------------

/// 把 GitHub 原始 URL 按线路配置改成实际请求 URL。
///
/// - `Direct` → 原样返回（一个字节都不改）；
/// - `Proxy` → `{base}{完整 GitHub URL}`；`base` 用 [`normalize_proxy_base`] 兜底，
///   非法或空时回落 [`DEFAULT_CORE_PROXY_URL`]（保存时本应已规范化，这里只是末尾防线）。
pub fn github_via(mode: CoreProxyMode, proxy_url: &str, github_url: &str) -> String {
    match mode {
        CoreProxyMode::Direct => github_url.to_owned(),
        CoreProxyMode::Proxy => {
            let base = normalize_proxy_base(proxy_url)
                .unwrap_or_else(|_| DEFAULT_CORE_PROXY_URL.to_owned());
            format!("{base}{github_url}")
        }
    }
}

// ---------------------------------------------------------------------------
// 2) 故障三分类 + hint
// ---------------------------------------------------------------------------

/// 线路故障的**处置分类**（注意：不是错误码分类，错误码只描述"发生了什么"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineFault {
    /// 直连不通 → 建议切到代理。
    DirectUnreachable,
    /// 代理不通（连接被拒绝/DNS/TLS 等）→ 建议更换代理地址。
    ProxyUnreachable,
    /// 代理超时 → 建议更换代理地址（超时多半是该代理不可用）。
    ProxyTimeout,
    /// 返回内容不是预期（HTML 拦截页、截断、格式不对）→ **不要建议换线路**。
    BadPayload,
    /// GitHub 限流 → 稍后再试，换代理无用。
    RateLimited,
    /// 其它（包校验、磁盘、状态码、全线路失败聚合等）→ 按原错误排查。
    Other,
}

impl LineFault {
    /// 给用户看的「下一步」。**固定文案**，UI/CLI 直接取用。
    pub const fn hint(self) -> &'static str {
        match self {
            Self::DirectUnreachable => {
                "直连 GitHub 不通：建议在设置里把「获取核心」切到代理（默认 https://ghproxy.net/）"
            }
            Self::ProxyUnreachable => "代理不通：建议更换代理地址（检查是否还能访问 GitHub）",
            Self::ProxyTimeout => "代理超时：建议更换代理地址，或稍后重试",
            Self::BadPayload => {
                "返回内容不是预期格式：与线路无关，按原错误排查（不要换代理掩盖问题）"
            }
            Self::RateLimited => "GitHub 限流：稍等再试，换代理不会绕过限流",
            Self::Other => "稍后重试；反复失败请导出诊断包发给开发者",
        }
    }
}

/// 把一个 [`CoreError`] 归到处置分类上。
///
/// 分类只认**错误的性质**，线路模式只影响「直连 vs 代理」的措辞：
/// 同一个 `NetworkUnreachable`，直连时叫「直连不通」，代理时叫「代理不通」。
pub fn fault_of(error: &CoreError, mode: CoreProxyMode) -> LineFault {
    match error {
        CoreError::NetworkUnreachable { .. } => match mode {
            CoreProxyMode::Direct => LineFault::DirectUnreachable,
            CoreProxyMode::Proxy => LineFault::ProxyUnreachable,
        },
        CoreError::BadPayload { .. } => LineFault::BadPayload,
        CoreError::RateLimited { .. } => LineFault::RateLimited,
        // 其它状态码/包校验/版本问题/全线路聚合：都不该靠"换代理"解决。
        _ => LineFault::Other,
    }
}

/// 便捷入口：错误 → 一句「下一步该干什么」。
pub fn hint_of(error: &CoreError, mode: CoreProxyMode) -> &'static str {
    fault_of(error, mode).hint()
}

// ---------------------------------------------------------------------------
// 3) 可用性探测（可注入假传输，单测不真发网络）
// ---------------------------------------------------------------------------

/// 探测成功的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOk {
    /// HTTP 状态码（2xx/3xx 才算通）。
    pub http_status: u16,
    /// 端到端耗时（毫秒）。
    pub latency_ms: u128,
    /// 实际请求的 URL（设置页要显示"到底打到哪"）。
    pub final_url: String,
}

/// 探测失败的分类（只保留探测能用到的四类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFault {
    DirectUnreachable,
    ProxyUnreachable,
    ProxyTimeout,
    BadPayload,
}

impl ProbeFault {
    /// 稳定文本（报告/日志用；**不要解析中文**）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectUnreachable => "direct-unreachable",
            Self::ProxyUnreachable => "proxy-unreachable",
            Self::ProxyTimeout => "proxy-timeout",
            Self::BadPayload => "bad-payload",
        }
    }

    /// 与 [`LineFault::hint`] 同源的固定文案。
    pub const fn hint(self) -> &'static str {
        match self {
            Self::DirectUnreachable => LineFault::DirectUnreachable.hint(),
            Self::ProxyUnreachable => LineFault::ProxyUnreachable.hint(),
            Self::ProxyTimeout => LineFault::ProxyTimeout.hint(),
            Self::BadPayload => LineFault::BadPayload.hint(),
        }
    }
}

/// 探测失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeErr {
    /// 分类（决定 hint）。
    pub fault: ProbeFault,
    /// 固定可执行提示。
    pub hint: &'static str,
    /// 诊断原文（不影响 hint）。
    pub detail: String,
}

impl std::fmt::Display for ProbeErr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}（{}）", self.hint, self.detail)
    }
}

/// 传输层失败（探针只关心「连不上 / 超时」这两种）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTransport {
    /// 超时（含读超时）。
    Timeout(String),
    /// 连接/DNS/TLS 等不可达。
    Unreachable(String),
}

/// 可注入的探测传输：只发一次轻量请求，返回状态码。
///
/// 生产实现是 [`HttpProber`]（HEAD）；单测传假实现，**不许真发网络**。
pub trait Prober {
    fn head(&self, url: &str, timeout: Duration) -> Result<u16, ProbeTransport>;
}

/// 生产探测传输：`ureq` 的 HEAD（不读 body，也不下大文件）。
#[derive(Debug, Clone, Default)]
pub struct HttpProber {
    /// 复用 `HttpClient` 的连接池与 TLS 配置。
    client: crate::http::HttpClient,
}

impl HttpProber {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用既有的客户端构造（单测要指向明文假服务器；生产用 [`HttpProber::new`]）。
    ///
    /// 为什么要这一条：`probe_tag_asset` 的 build-N 探测也走 HEAD，测试必须能把它
    /// 打到 `127.0.0.1` 的假服务器上，才能验证「真实请求路径长什么样」。
    pub fn with_client(client: crate::http::HttpClient) -> Self {
        Self { client }
    }
}

impl Prober for HttpProber {
    fn head(&self, url: &str, timeout: Duration) -> Result<u16, ProbeTransport> {
        self.client.head_status(url, timeout)
    }
}

/// 探测「当前线路能不能到 GitHub」。
pub fn probe_core_line(
    mode: CoreProxyMode,
    proxy_url: &str,
    timeout: Duration,
) -> Result<ProbeOk, ProbeErr> {
    probe_core_line_with(&HttpProber::new(), mode, proxy_url, timeout, PROBE_URL)
}

/// 带注入传输与端点的探测（单测入口）。
pub fn probe_core_line_with<P: Prober>(
    prober: &P,
    mode: CoreProxyMode,
    proxy_url: &str,
    timeout: Duration,
    probe_url: &str,
) -> Result<ProbeOk, ProbeErr> {
    let final_url = github_via(mode, proxy_url, probe_url);
    let started = Instant::now();
    let status = prober.head(&final_url, timeout).map_err(|transport| {
        let (fault, detail) = match (&transport, mode) {
            (ProbeTransport::Timeout(detail), CoreProxyMode::Proxy) => {
                (ProbeFault::ProxyTimeout, detail.clone())
            }
            (ProbeTransport::Timeout(detail), CoreProxyMode::Direct) => {
                (ProbeFault::DirectUnreachable, format!("直连超时：{detail}"))
            }
            (ProbeTransport::Unreachable(detail), CoreProxyMode::Proxy) => {
                (ProbeFault::ProxyUnreachable, detail.clone())
            }
            (ProbeTransport::Unreachable(detail), CoreProxyMode::Direct) => (
                ProbeFault::DirectUnreachable,
                format!("直连不可达：{detail}"),
            ),
        };
        ProbeErr {
            fault,
            hint: fault.hint(),
            detail,
        }
    })?;
    let latency_ms = started.elapsed().as_millis();

    if (200..400).contains(&status) {
        return Ok(ProbeOk {
            http_status: status,
            latency_ms,
            final_url,
        });
    }
    // 非 2xx/3xx：都是"通到了，但端点不认"——不是线路问题，别建议换代理。
    let detail = match status {
        403 | 404 => format!("探测端点返回 {status}（端点不存在/被拒，不是线路问题）"),
        429 => "探测端点返回 429（GitHub 限流）".to_owned(),
        _ => format!("探测端点返回 {status}"),
    };
    Err(ProbeErr {
        fault: ProbeFault::BadPayload,
        hint: ProbeFault::BadPayload.hint(),
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(status: u16) -> impl Prober {
        struct Fixed(u16);
        impl Prober for Fixed {
            fn head(&self, _url: &str, _timeout: Duration) -> Result<u16, ProbeTransport> {
                Ok(self.0)
            }
        }
        Fixed(status)
    }

    fn broken(transport: ProbeTransport) -> impl Prober {
        struct Broken(ProbeTransport);
        impl Prober for Broken {
            fn head(&self, _url: &str, _timeout: Duration) -> Result<u16, ProbeTransport> {
                Err(self.0.clone())
            }
        }
        Broken(transport)
    }

    // ---- URL 拼接（验收第 2 条 5 例）----

    #[test]
    fn direct_mode_keeps_url_verbatim() {
        let url = "https://github.com/aria2/aria2/releases/download/release-1.37.0/x.zip";
        assert_eq!(
            github_via(CoreProxyMode::Direct, "https://ghproxy.net/", url),
            url
        );
    }

    #[test]
    fn proxy_mode_prefixes_full_github_url() {
        assert_eq!(
            github_via(
                CoreProxyMode::Proxy,
                "https://ghproxy.net/",
                "https://github.com/a/b"
            ),
            "https://ghproxy.net/https://github.com/a/b"
        );
    }

    #[test]
    fn proxy_mode_tolerates_missing_trailing_slash() {
        assert_eq!(
            github_via(
                CoreProxyMode::Proxy,
                "https://ghproxy.net",
                "https://github.com/a"
            ),
            "https://ghproxy.net/https://github.com/a"
        );
    }

    #[test]
    fn proxy_mode_falls_back_to_default_when_base_is_invalid() {
        // 非法基址保存时本应已回落；这里验"末尾防线"不 panic、不拼出畸形 URL。
        assert_eq!(
            github_via(CoreProxyMode::Proxy, "not-a-url", "https://github.com/a"),
            format!("{DEFAULT_CORE_PROXY_URL}https://github.com/a")
        );
        assert_eq!(
            github_via(CoreProxyMode::Proxy, "  ", "https://github.com/a"),
            format!("{DEFAULT_CORE_PROXY_URL}https://github.com/a")
        );
    }

    #[test]
    fn proxy_mode_collapses_extra_slashes_and_keeps_path() {
        assert_eq!(
            github_via(
                CoreProxyMode::Proxy,
                "https://g.example.com///",
                "https://github.com/a"
            ),
            "https://g.example.com/https://github.com/a"
        );
        assert_eq!(
            github_via(
                CoreProxyMode::Proxy,
                "https://g.example.com/gh/",
                "https://github.com/a"
            ),
            "https://g.example.com/gh/https://github.com/a"
        );
    }

    // ---- 错误三分类（验收第 2 条各 1 例）----

    #[test]
    fn direct_network_failure_advises_switching_to_proxy() {
        let error = CoreError::network("dns failed");
        let fault = fault_of(&error, CoreProxyMode::Direct);
        assert_eq!(fault, LineFault::DirectUnreachable);
        assert!(fault.hint().contains("切到代理"), "{}", fault.hint());
    }

    #[test]
    fn proxy_failure_advises_changing_proxy_address() {
        let error = CoreError::network("connection refused");
        let fault = fault_of(&error, CoreProxyMode::Proxy);
        assert_eq!(fault, LineFault::ProxyUnreachable);
        assert!(fault.hint().contains("更换代理地址"), "{}", fault.hint());
    }

    #[test]
    fn payload_and_rate_limit_do_not_advise_switching_proxy() {
        for (error, expected) in [
            (
                CoreError::BadPayload {
                    detail: "<html>".to_owned(),
                },
                LineFault::BadPayload,
            ),
            (
                CoreError::RateLimited {
                    detail: "403".to_owned(),
                },
                LineFault::RateLimited,
            ),
        ] {
            for mode in [CoreProxyMode::Direct, CoreProxyMode::Proxy] {
                let fault = fault_of(&error, mode);
                assert_eq!(fault, expected);
                let hint = fault.hint();
                assert!(!hint.contains("更换代理地址"), "不该建议换代理：{hint}");
                assert!(!hint.contains("切到代理"), "不该建议切代理：{hint}");
            }
        }
    }

    // ---- 探测：假传输 2 例 ----

    #[test]
    fn probe_reports_ok_with_status_and_latency() {
        let ok = probe_core_line_with(
            &fake(200),
            CoreProxyMode::Proxy,
            "https://ghproxy.net/",
            Duration::from_secs(1),
            PROBE_URL,
        )
        .expect("200 应当算通");
        assert_eq!(ok.http_status, 200);
        assert_eq!(ok.final_url, format!("https://ghproxy.net/{PROBE_URL}"));
    }

    #[test]
    fn probe_transport_failures_map_to_mode_specific_faults() {
        let timeout = ProbeTransport::Timeout("8s".to_owned());
        let err = probe_core_line_with(
            &broken(timeout),
            CoreProxyMode::Proxy,
            "https://ghproxy.net/",
            Duration::from_secs(1),
            PROBE_URL,
        )
        .expect_err("超时必须报错");
        assert_eq!(err.fault, ProbeFault::ProxyTimeout);
        assert!(err.hint.contains("更换代理地址"));

        let err = probe_core_line_with(
            &broken(ProbeTransport::Unreachable("dns".to_owned())),
            CoreProxyMode::Direct,
            "https://ghproxy.net/",
            Duration::from_secs(1),
            PROBE_URL,
        )
        .expect_err("不可达必须报错");
        assert_eq!(err.fault, ProbeFault::DirectUnreachable);
        assert!(err.hint.contains("切到代理"));

        // 端点 404：线路是通的，不许建议换代理。
        let err = probe_core_line_with(
            &fake(404),
            CoreProxyMode::Proxy,
            "https://ghproxy.net/",
            Duration::from_secs(1),
            PROBE_URL,
        )
        .expect_err("404 必须报错");
        assert_eq!(err.fault, ProbeFault::BadPayload);
        assert!(!err.hint.contains("更换代理地址"));
    }
}
