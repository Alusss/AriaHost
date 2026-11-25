//! 同源反代（D4 定版）：浏览器 → 本页 → `127.0.0.1:6800/jsonrpc`，**由服务端注入 `token:`**。
//!
//! ## 为什么必须这样
//!
//! aria2 的 RPC 只要拿到 `rpc-secret` 就能做任何事（加任务、删任务、改全局选项）。
//! 若把 secret 交给浏览器（哪怕写进页面里），任何能读到页面的东西都能接管下载器。
//! 所以：**secret 只存在服务端进程内存里**，浏览器发来的请求由本模块改写后再转发。
//!
//! ## 四条安全边界（每一条都有单测）
//!
//! 1. **方法白名单**：白名单外一律拒（`changeGlobalOption`/`saveSession` 这些不给网页端）。
//! 2. **`shutdown`/`forceShutdown` 拦截**：浏览器一键关掉下载服务＝拒绝服务。
//! 3. **`system.multicall` 整体拒绝**：它能一次打包任意方法，等于白名单的旁路。
//! 4. **浏览器传的 `token:` 一律丢弃**：否则它可以用错的 token 试探、或塞第二个 token 玩参数错位。
//!
//! 注入策略：先删掉所有「以 `token:` 开头的顶层字符串参数」，再把服务端的 token 插到**第 0 位**
//! （aria2 约定 token 为首参）。这样「不传 / 传假 / 传两个」三种脏输入都收敛成一种干净形态。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

use crate::http::Response;

/// 网页端**允许**调用的方法（白名单即全部权限，别处不要偷偷加）。
///
/// 取舍原则：只放「看任务 / 加任务 / 暂停 / 继续 / 移除」这一组；
/// 任何会改全局配置、写会话文件、影响 aria2 生死的都不放。
pub const ALLOWED_METHODS: &[&str] = &[
    // 查询
    "aria2.getVersion",
    "aria2.getGlobalStat",
    "aria2.tellStatus",
    "aria2.tellActive",
    "aria2.tellWaiting",
    "aria2.tellStopped",
    "aria2.getFiles",
    // 添加
    "aria2.addUri",
    // 控制（只对单个任务，不提供 *All 变体：那等于一条命令停掉全部下载）
    "aria2.pause",
    "aria2.unpause",
    "aria2.remove",
    "aria2.removeDownloadResult",
];

/// 即使将来有人误加进白名单，这些也必须**永远**被拒（先判 deny，再判 allow）。
pub const ALWAYS_DENIED: &[&str] = &[
    "aria2.shutdown",
    "aria2.forceShutdown",
    "aria2.shutdownAll",
    "aria2.forceShutdownAll",
    "system.multicall",
    "aria2.changeGlobalOption",
    "aria2.changeGlobalOptions",
    "aria2.saveSession",
    "aria2.purgeDownloadResult",
    "aria2.pauseAll",
    "aria2.unpauseAll",
    "aria2.changeOption",
    "aria2.changePosition",
    "system.listMethods",
    "system.listNotifications",
];

/// 被拒绝的原因（决定 HTTP 状态与 JSON-RPC 错误码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    /// 关服务类方法：给「去面板操作」的友好提示。
    Shutdown,
    /// 白名单外的方法。
    NotAllowed(String),
    /// 请求结构不对（不是对象、没有 method、params 不是数组）。
    Malformed(&'static str),
}

impl Reject {
    /// JSON-RPC 错误码：`-32601` 是标准的「方法不可用」，关服务用一个应用错误码。
    pub const fn rpc_code(&self) -> i64 {
        match self {
            Self::Shutdown => -32001,
            Self::NotAllowed(_) | Self::Malformed(_) => -32601,
        }
    }

    /// 给用户看的中文说明（前端直接显示）。
    pub fn message(&self) -> String {
        match self {
            Self::Shutdown => {
                "出于安全考虑，网页端不允许关闭 aria2；请在 AriaHost 面板里操作。".to_owned()
            }
            Self::NotAllowed(method) => {
                format!("网页端不允许调用该方法（不在白名单内）：{method}")
            }
            Self::Malformed(detail) => format!("请求格式不对：{detail}"),
        }
    }

    /// 对应既有 `AH-` 错误码（`AH-E3003` = 被拒）。
    pub const fn ah_code(&self) -> &'static str {
        "AH-E3003"
    }

    /// 转成 HTTP 响应：**HTTP 200 + JSON-RPC 错误信封**，前端按 `error` 分支显示。
    pub fn into_response(self) -> Response {
        Response::rpc_error(self.rpc_code(), &self.message(), Some(self.ah_code()))
    }
}

/// 这个方法在网页端是否**永远**被拒（先判 deny）。
pub fn is_always_denied(method: &str) -> bool {
    ALWAYS_DENIED
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(method))
}

/// 这个方法是否在白名单内。
pub fn is_allowed(method: &str) -> bool {
    ALLOWED_METHODS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// 白名单判定：先 deny、再 allow（顺序不能反，否则将来误加白名单会放开关服务）。
pub fn check_method(method: &str) -> Result<(), Reject> {
    if is_always_denied(method) {
        return Err(match method {
            _ if method.eq_ignore_ascii_case("aria2.shutdown")
                || method.eq_ignore_ascii_case("aria2.forceShutdown") =>
            {
                Reject::Shutdown
            }
            _ => Reject::NotAllowed(method.to_owned()),
        });
    }
    if is_allowed(method) {
        return Ok(());
    }
    Err(Reject::NotAllowed(method.to_owned()))
}

/// 把一条 JSON-RPC 调用就地改写成「带服务端 token 的版本」。
pub fn rewrite_call(call: &mut Value, secret: &str) -> Result<(), Reject> {
    let object = call
        .as_object_mut()
        .ok_or(Reject::Malformed("批量数组里每一项都必须是对象"))?;
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or(Reject::Malformed("缺少 method"))?
        .to_owned();
    check_method(&method)?;

    let token = format!("token:{secret}");
    match object.get_mut("params") {
        None => {
            object.insert("params".to_owned(), Value::Array(vec![Value::from(token)]));
        }
        Some(Value::Array(items)) => {
            // 丢掉浏览器塞进来的一切 token:（真 token / 假 token / 第二个 token）。
            items.retain(|item| !is_tokenish(item));
            items.insert(0, Value::from(token));
        }
        Some(_) => return Err(Reject::Malformed("params 必须是数组")),
    }
    Ok(())
}

/// 这个参数是不是一个「token:xxx」字符串。
fn is_tokenish(value: &Value) -> bool {
    value
        .as_str()
        .map(|text| text.trim_start().to_ascii_lowercase().starts_with("token:"))
        .unwrap_or(false)
}

/// 上游（aria2）的响应的最小形状。
#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    /// 状态码。
    pub status: u16,
    /// 上游的 `Content-Type`（仅诊断用；回给浏览器时一律按 `application/json`）。
    pub content_type: Option<String>,
    /// 响应体。
    pub body: Vec<u8>,
}

/// 上游连不上/超时。
#[derive(Debug)]
pub enum UpstreamError {
    /// 连不上（aria2 没起、端口不对）。
    Unreachable(String),
    /// 读到的响应不成形。
    Malformed(String),
}

/// 反代：浏览器请求体 → 校验/改写 → 打上游 → 原样把上游的 JSON 还回去。
pub struct Proxy {
    upstream: SocketAddr,
    secret: String,
    /// 建连超时：本机环回正常是毫秒级；3 秒足够，同时把「端口被防火墙 DROP」这类
    /// 黑洞环境（实测 WSL 上访问无人监听的端口会一直挂到超时）压在 3 秒内，
    /// 否则前端每 2 秒一次的轮询会排起长队。
    connect_timeout: Duration,
    /// 读写超时：aria2 回一个任务列表不该超过这个数。
    io_timeout: Duration,
    /// 上游响应体上限：任务列表最多 1000 条，8 MiB 很宽裕。
    max_response: usize,
}

/// 建连超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 读写超时。
pub const IO_TIMEOUT: Duration = Duration::from_secs(5);

impl std::fmt::Debug for Proxy {
    /// 手写 `Debug`：secret **永远**不进化日志/panic 信息（哪怕有人 `dbg!` 了一把）。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Proxy")
            .field("upstream", &self.upstream)
            .field("connect_timeout", &self.connect_timeout)
            .field("io_timeout", &self.io_timeout)
            .field("max_response", &self.max_response)
            .field("secret", &"<已隐藏>")
            .finish()
    }
}

impl Proxy {
    /// 新建一个反代（`upstream` 一般是 `127.0.0.1:6800`）。
    pub fn new(upstream: SocketAddr, secret: impl Into<String>) -> Self {
        Self {
            upstream,
            secret: secret.into(),
            connect_timeout: CONNECT_TIMEOUT,
            io_timeout: IO_TIMEOUT,
            max_response: 8 * 1024 * 1024,
        }
    }

    /// 处理一个 `/jsonrpc` 请求体，返回给浏览器的响应。
    pub fn handle(&self, body: &[u8]) -> Response {
        let mut value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => {
                return Response::new(
                    400,
                    "application/json",
                    Response::rpc_error(-32700, "请求体不是合法 JSON", Some("AH-E3003")).body,
                );
            }
        };

        // 批量与单条分开处理：批量里只要有一条越权，整批拒绝（fail-closed）。
        let rewritten = match value {
            Value::Array(mut calls) => {
                for call in &mut calls {
                    if let Err(reject) = rewrite_call(call, &self.secret) {
                        return reject.into_response();
                    }
                }
                Value::Array(calls)
            }
            Value::Object(_) => {
                if let Err(reject) = rewrite_call(&mut value, &self.secret) {
                    return reject.into_response();
                }
                value
            }
            _ => {
                return Reject::Malformed("请求体必须是 JSON 对象或数组").into_response();
            }
        };

        let payload = match serde_json::to_vec(&rewritten) {
            Ok(payload) => payload,
            Err(_) => {
                return Response::new(
                    400,
                    "application/json",
                    Response::rpc_error(-32700, "改写请求失败", Some("AH-E3003")).body,
                );
            }
        };

        match self.post_json(&payload) {
            Ok(upstream) => {
                // 回给浏览器的一律是 JSON（aria2 只讲 JSON；上游的 Content-Type 只做诊断）。
                Response::new(upstream.status, "application/json", upstream.body)
            }
            Err(error) => {
                // 友好降级：前端据此显示「下载服务未启动，请先在 AriaHost 面板启动」。
                let detail = match &error {
                    UpstreamError::Unreachable(detail) | UpstreamError::Malformed(detail) => detail,
                };
                let message = format!(
                    "下载服务未启动或无法连接（{}）。请先在 AriaHost 面板启动 aria2。",
                    detail
                );
                Response::new(
                    503,
                    "application/json",
                    Response::rpc_error(-32000, &message, Some("AH-E2003")).body,
                )
            }
        }
    }

    /// 打一次上游 JSON-RPC（手写阻塞 HTTP，与仓库其余部分同一套做法）。
    pub fn post_json(&self, body: &[u8]) -> Result<UpstreamResponse, UpstreamError> {
        let mut stream = TcpStream::connect_timeout(&self.upstream, self.connect_timeout)
            .map_err(|error| UpstreamError::Unreachable(format!("{}：{error}", self.upstream)))?;
        stream
            .set_read_timeout(Some(self.io_timeout))
            .map_err(|error| UpstreamError::Unreachable(error.to_string()))?;
        stream
            .set_write_timeout(Some(self.io_timeout))
            .map_err(|error| UpstreamError::Unreachable(error.to_string()))?;

        let head = format!(
            "POST /jsonrpc HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.upstream,
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body))
            .and_then(|()| stream.flush())
            .map_err(|error| UpstreamError::Unreachable(error.to_string()))?;

        let mut reader = BufReader::new(stream);
        let status = read_status_line(&mut reader)?;
        let mut content_type = None;
        let mut content_length = None;
        let mut header_bytes = 0usize;
        loop {
            let mut line = String::new();
            let read = reader
                .by_ref()
                .take(8 * 1024)
                .read_line(&mut line)
                .map_err(|error| UpstreamError::Malformed(error.to_string()))?;
            if read == 0 {
                break;
            }
            header_bytes += line.len();
            if header_bytes > 16 * 1024 {
                return Err(UpstreamError::Malformed("上游响应头过长".to_owned()));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_owned();
                match name.as_str() {
                    "content-type" => content_type = Some(value),
                    "content-length" => content_length = value.parse::<usize>().ok(),
                    _ => {}
                }
            }
        }

        let mut body = Vec::new();
        match content_length {
            Some(length) => {
                if length > self.max_response {
                    return Err(UpstreamError::Malformed(format!(
                        "上游响应体过大（{length} 字节）"
                    )));
                }
                body.resize(length, 0);
                reader
                    .read_exact(&mut body)
                    .map_err(|error| UpstreamError::Malformed(error.to_string()))?;
            }
            None => {
                // 没有 Content-Length 就按「读到 EOF」处理，仍然有上限。
                let mut limited = reader.take(self.max_response as u64 + 1);
                limited
                    .read_to_end(&mut body)
                    .map_err(|error| UpstreamError::Malformed(error.to_string()))?;
                if body.len() > self.max_response {
                    return Err(UpstreamError::Malformed("上游响应体过大".to_owned()));
                }
            }
        }

        Ok(UpstreamResponse {
            status,
            content_type,
            body,
        })
    }
}

/// 读状态行 → 状态码。
fn read_status_line(reader: &mut BufReader<TcpStream>) -> Result<u16, UpstreamError> {
    let mut line = String::new();
    reader
        .by_ref()
        .take(8 * 1024)
        .read_line(&mut line)
        .map_err(|error| UpstreamError::Malformed(error.to_string()))?;
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err(UpstreamError::Malformed("上游没有返回状态行".to_owned()));
    }
    let mut parts = trimmed.split_whitespace();
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(UpstreamError::Malformed(format!(
            "上游状态行异常：{trimmed}"
        )));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| UpstreamError::Malformed(format!("上游状态码异常：{trimmed}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
    }

    #[test]
    fn whitelist_matches_the_page_needs() {
        for method in [
            "aria2.addUri",
            "aria2.tellActive",
            "aria2.tellWaiting",
            "aria2.tellStopped",
            "aria2.tellStatus",
            "aria2.pause",
            "aria2.unpause",
            "aria2.remove",
            "aria2.removeDownloadResult",
            "aria2.getGlobalStat",
            "aria2.getVersion",
            "aria2.getFiles",
        ] {
            assert!(is_allowed(method), "{method} 应在白名单里");
            assert!(check_method(method).is_ok(), "{method} 应被放行");
        }
    }

    #[test]
    fn shutdown_is_rejected_with_the_friendly_reason() {
        for method in ["aria2.shutdown", "aria2.forceShutdown"] {
            let reject = check_method(method).expect_err("必须拒绝");
            assert_eq!(reject, Reject::Shutdown, "{method}");
            assert_eq!(reject.rpc_code(), -32001);
            assert!(
                reject.message().contains("请在 AriaHost 面板"),
                "{}",
                reject.message()
            );
        }
    }

    #[test]
    fn dangerous_methods_outside_whitelist_are_rejected() {
        for method in [
            "aria2.changeGlobalOption",
            "aria2.saveSession",
            "aria2.purgeDownloadResult",
            "aria2.pauseAll",
            "system.multicall",
            "system.listMethods",
        ] {
            let reject = check_method(method).expect_err("必须拒绝");
            assert!(
                matches!(reject, Reject::NotAllowed(_) | Reject::Shutdown),
                "{method} → {reject:?}"
            );
        }
    }

    #[test]
    fn denial_wins_even_if_it_were_also_whitelisted() {
        // 模拟「将来有人把 shutdown 误加进白名单」：deny 必须先判。
        assert!(is_always_denied("aria2.shutdown"));
        assert!(is_always_denied("ARIA2.FORCESHUTDOWN"), "大小写不敏感");
        assert_eq!(
            check_method("aria2.shutdown").expect_err("必须拒绝"),
            Reject::Shutdown
        );
    }

    #[test]
    fn injects_server_token_as_first_param() {
        let mut value = call("aria2.tellActive", serde_json::json!([]));
        rewrite_call(&mut value, "s3cret").expect("应放行");
        assert_eq!(value["params"], serde_json::json!(["token:s3cret"]));
    }

    #[test]
    fn discards_browser_supplied_tokens() {
        // 假 token、真 token、两个 token，全部丢掉后只留服务端的。
        for (params, expected) in [
            (
                serde_json::json!(["token:guess"]),
                serde_json::json!(["token:real"]),
            ),
            (
                serde_json::json!(["token:guess", "gid-1"]),
                serde_json::json!(["token:real", "gid-1"]),
            ),
            (
                serde_json::json!(["token:a", "token:b", "gid-1"]),
                serde_json::json!(["token:real", "gid-1"]),
            ),
        ] {
            let mut value = call("aria2.tellStatus", params);
            rewrite_call(&mut value, "real").expect("应放行");
            assert_eq!(
                value["params"], expected,
                "浏览器传的 token 必须被丢掉，业务参数必须保留"
            );
        }
    }

    #[test]
    fn token_like_values_inside_nested_arrays_are_kept() {
        // addUri 的第一个参数是 URI 数组：里面就算长得像 token 也不是 token，不该动。
        let mut value = call(
            "aria2.addUri",
            serde_json::json!([["http://example.com/a.zip"], { "dir": "/tmp" }]),
        );
        rewrite_call(&mut value, "real").expect("应放行");
        assert_eq!(
            value["params"],
            serde_json::json!([
                "token:real",
                ["http://example.com/a.zip"],
                { "dir": "/tmp" }
            ])
        );
    }

    #[test]
    fn missing_params_gets_created() {
        let mut value =
            serde_json::json!({ "jsonrpc": "2.0", "id": 7, "method": "aria2.getVersion" });
        rewrite_call(&mut value, "real").expect("应放行");
        assert_eq!(value["params"], serde_json::json!(["token:real"]));
    }

    #[test]
    fn non_array_params_are_malformed() {
        let mut value = call("aria2.tellActive", serde_json::json!({ "x": 1 }));
        let error = rewrite_call(&mut value, "real").expect_err("必须拒绝");
        assert!(matches!(error, Reject::Malformed(_)), "{error:?}");
    }

    #[test]
    fn call_without_method_is_malformed() {
        let mut value = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "params": [] });
        let error = rewrite_call(&mut value, "real").expect_err("必须拒绝");
        assert!(matches!(error, Reject::Malformed(_)), "{error:?}");
    }

    #[test]
    fn rewrite_is_idempotent_for_our_own_payload() {
        // 服务端改写过的请求再改一次，不该长出第二个 token。
        let mut value = call("aria2.tellActive", serde_json::json!(["token:real"]));
        rewrite_call(&mut value, "real").expect("第一次");
        rewrite_call(&mut value, "real").expect("第二次");
        assert_eq!(value["params"], serde_json::json!(["token:real"]));
    }

    #[test]
    fn upstream_timeouts_are_bounded_for_page_polling() {
        // 页面每 2 秒轮询一次：上游超时必须显著小于轮询间隔，否则请求会排队。
        assert!(CONNECT_TIMEOUT <= Duration::from_secs(3), "建连超时太长");
        assert!(IO_TIMEOUT <= Duration::from_secs(5), "读写超时太长");
        let proxy = Proxy::new("127.0.0.1:6800".parse().expect("地址"), "x");
        assert_eq!(proxy.connect_timeout, CONNECT_TIMEOUT);
        assert_eq!(proxy.io_timeout, IO_TIMEOUT);
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let proxy = Proxy::new("127.0.0.1:6800".parse().expect("地址"), "super-secret");
        let text = format!("{proxy:?}");
        assert!(
            !text.contains("super-secret"),
            "Debug 泄露了 secret：{text}"
        );
        assert!(text.contains("已隐藏"), "{text}");
    }
}
