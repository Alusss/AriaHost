//! 监听、路由、每连接一线程的阻塞服务端。
//!
//! 决策（都写下来，避免后来者以为是漏了）：
//! - **每连接一个请求**，响应带 `Connection: close`，然后关连接。不实现 keep-alive：
//!   省掉「读超时/流水线/半关闭」一整类坑，浏览器每次多一次 TCP 握手（本机环回，代价可忽略）。
//! - **每连接一个线程**而不是线程池：本页最多几个并发连接；SCM 面板开一个标签页而已。
//!   线程有读写超时兜底，不会出现「慢速连接把线程占死」。
//! - `Host` 头**每个请求都校验**（防 DNS rebinding）：只认 `127.0.0.1[:port]` 与 `localhost[:port]`。

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::assets;
use crate::http::{self, ReadError, Request, Response};
use crate::rpc::Proxy;

/// 只允许绑在环回地址上（架构 §3.3）。
pub const BIND_ADDR: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// 请求体上限：1 MiB（一个 `addUri` 撑死几百字节，1 MiB 是浪费都算不上的余量）。
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 读/写超时：慢速连接不会永久占着线程。
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// 一次请求需要的全部上下文（连接间共享）。
pub struct Context {
    /// 本服务的端口（`Host` 校验用）。
    pub port: u16,
    /// JSON-RPC 反代。
    pub proxy: Proxy,
}

impl Context {
    /// 组装上下文。
    pub fn new(port: u16, proxy: Proxy) -> Self {
        Self { port, proxy }
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Context")
            .field("port", &self.port)
            .field("proxy", &self.proxy)
            .finish()
    }
}

/// `Host` 头是否可接受。
///
/// 只接受 `127.0.0.1[:port]`、`localhost[:port]`（大小写不敏感）。`port` 给 0 表示不校验端口
/// （只在单测里用，生产恒传真实端口）。
///
/// 为什么校验：DNS rebinding 的经典打法是把 `evil.example` 解析到 `127.0.0.1`，
/// 让浏览器把本机服务当成「同源」去读——`Host` 头会是 `evil.example`，这里直接拦掉。
pub fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.trim().to_ascii_lowercase();
    // 拆出主机名与端口（IPv6 字面量不在支持范围：我们只绑 IPv4 环回）。
    let (name, declared_port) = match host.rsplit_once(':') {
        Some((name, port_text)) => match port_text.parse::<u16>() {
            Ok(parsed) => (name.to_owned(), Some(parsed)),
            // `127.0.0.1:abc` 这种直接拒。
            Err(_) => return false,
        },
        None => (host, None),
    };
    let name_ok = matches!(name.as_str(), "127.0.0.1" | "localhost");
    if !name_ok {
        return false;
    }
    match declared_port {
        None => true,
        Some(declared) => port == 0 || declared == port,
    }
}

/// 处理一个已经 accept 的连接（读完一个请求 → 写一个响应 → 关闭）。
pub fn handle_connection(mut stream: TcpStream, context: &Context) {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let request = match http::read_request(&mut stream, MAX_BODY_BYTES) {
        Ok(request) => request,
        Err(error) => {
            // 对端直接断开：没什么可回的，安静收场（不刷日志）。
            if matches!(error, ReadError::Io(_)) {
                return;
            }
            let response = match error {
                ReadError::BodyTooLarge => Response::new(
                    413,
                    "text/plain; charset=utf-8",
                    "请求体太大（上限 1 MiB）。\n",
                ),
                _ => Response::new(
                    400,
                    "text/plain; charset=utf-8",
                    "请求不合法（只支持 HTTP/1.1 的最小 GET/POST）。\n",
                ),
            };
            let _ = response.write_to(&mut stream, false);
            return;
        }
    };

    let response = route(&request, context);
    let head_only = request.method == "HEAD";
    let _ = response.write_to(&mut stream, head_only);
}

/// 路由：静态资源 / 健康探针 / JSON-RPC 反代。
fn route(request: &Request, context: &Context) -> Response {
    // Host 校验对所有请求都做（包括静态资源：rebinding 读页面同样是信息泄露）。
    let host = request.header("host");
    if host.is_none() {
        return Response::new(400, "text/plain; charset=utf-8", "缺少 Host 头。\n");
    }
    if !host_allowed(host, context.port) {
        return Response::new(
            403,
            "text/plain; charset=utf-8",
            "Host 不被接受（只允许 127.0.0.1 与 localhost）。\n",
        );
    }

    match request.method.as_str() {
        "GET" | "HEAD" => {
            if request.path == "/healthz" {
                let body = serde_json::json!({
                    "status": "ok",
                    "version": ariahost_common::VERSION,
                    "mode": "web",
                });
                return Response::json(body.to_string());
            }
            if request.path == "/favicon.ico" {
                return Response::no_content();
            }
            match assets::lookup(&request.path) {
                Some(asset) => Response::new(200, asset.content_type, asset.body),
                None => Response::not_found(),
            }
        }
        "POST" => {
            if request.path == "/jsonrpc" {
                context.proxy.handle(&request.body)
            } else {
                Response::not_found()
            }
        }
        _ => Response::new(
            405,
            "text/plain; charset=utf-8",
            "只支持 GET / HEAD / POST。\n",
        ),
    }
}

/// 接受循环（阻塞；`listener` 被 drop 或出错时返回）。
pub fn serve_listener(listener: TcpListener, context: Arc<Context>) -> std::io::Result<()> {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let context = Arc::clone(&context);
                // 一连接一线程；线程 panic 不会带走进程（最坏是这个连接没响应）。
                std::thread::spawn(move || handle_connection(stream, &context));
            }
            // 单个连接 accept 失败（例如对端在握手前就断了）不该终止整个服务。
            Err(_) => continue,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    #[test]
    fn host_allowed_accepts_loopback_forms() {
        for host in [
            "127.0.0.1:6875",
            "localhost:6875",
            "LOCALHOST:6875",
            "127.0.0.1",
            "localhost",
            " 127.0.0.1:6875 ",
        ] {
            assert!(host_allowed(Some(host), 6875), "{host} 应被接受");
        }
    }

    #[test]
    fn host_allowed_rejects_rebinding_attempts() {
        for host in [
            "evil.example:6875",
            "evil.example",
            "127.0.0.1:6800", // 端口不对（别的本机服务）
            "localhost:9999",
            "127.0.0.1.evil.example:6875",
            "192.168.1.10:6875",
            "[::1]:6875",
            "127.0.0.1:abc",
            "",
        ] {
            assert!(!host_allowed(Some(host), 6875), "{host} 必须被拒绝");
        }
        assert!(!host_allowed(None, 6875), "缺 Host 头必须拒绝");
    }

    #[test]
    fn bind_addr_is_loopback() {
        assert_eq!(BIND_ADDR, Ipv4Addr::LOCALHOST);
        assert_eq!(
            SocketAddr::from((BIND_ADDR, 6875)).to_string(),
            "127.0.0.1:6875"
        );
    }
}
