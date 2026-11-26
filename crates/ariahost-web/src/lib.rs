//! AriaHost 网页管理页（需求第 9 条）：浏览器能开的本地任务页，**浏览器永远拿不到 `rpc-secret`**。
//!
//! ## 全貌（D4 定版：同源反代注入 token）
//!
//! ```text
//! 浏览器 ──GET /  ────────────────► 本模块（127.0.0.1:<web.port>，默认 6875）
//!        ──POST /jsonrpc {method} ─► 校验方法白名单 → 丢掉浏览器的 token: → 注入服务端 token:
//!                                     └─► 127.0.0.1:6800/jsonrpc（aria2）→ 原样返回 JSON
//! ```
//!
//! 为什么这么设计：aria2 的 `rpc-secret` 是「全权钥匙」，一旦落到浏览器（页面源码、localStorage、
//! 书签、截图）就等于把下载器的控制权交出去。反过来让服务端代理，浏览器只有「白名单里那 12 个方法」
//! 的权限，且**物理上拿不到 secret**。
//!
//! ## 技术选择的理由
//!
//! - **零异步运行时**：手写阻塞 HTTP/1.1 + 每连接一线程。本仓库的 HTTP 栈已经有一条（`ureq` +
//!   手写传输层），再引 axum/hyper/tokio 只会让二进制、CI 时间与攻击面一起变大。
//! - **零构建前端**：`assets/` 里的三个文件用 `include_str!` 内嵌，vanilla ESM，
//!   不需要 node/pnpm，也不需要 `include_dir` 这种宏依赖。
//! - **只绑环回**：非 `127.0.0.1` 的绑定请求直接拒绝（[`WebError::NotLoopback`]）。
//!
//! ## 安全边界（每一条都有单测，见本文件的 `tests`）
//!
//! | 边界 | 行为 |
//! |---|---|
//! | 方法白名单 | 白名单外一律拒（[`rpc::ALLOWED_METHODS`]） |
//! | `shutdown`/`forceShutdown` | 拦截并返回「请在面板操作」（[`rpc::Reject::Shutdown`]） |
//! | `system.multicall` | 整体拒绝（它能打包任意方法，是白名单旁路） |
//! | 浏览器传的 `token:` | 一律丢弃，注入服务端的 |
//! | `Host` 头 | 只认 `127.0.0.1[:port]` / `localhost[:port]`（防 DNS rebinding） |
//! | 请求体 | 上限 1 MiB，超了 413 |
//! | 响应头 | `CSP: default-src 'self'` + `nosniff` + `no-referrer`；**不发** CORS 头 |
//!
//! 本模块**不碰** `aria2.conf` 的 `log=`、不改 aria2 的启动参数、不写配置。

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;

use ariahost_common::ErrorCode;

pub mod assets;
pub mod http;
pub mod rpc;
pub mod server;

pub use http::{request_once, RawResponse};
pub use rpc::{
    check_method, is_allowed, rewrite_call, Proxy, Reject, ALLOWED_METHODS, ALWAYS_DENIED,
};
pub use server::{
    handle_connection, host_allowed, serve_listener, Context, BIND_ADDR, MAX_BODY_BYTES,
};

/// 网页管理页的配置（端口 + 上游 RPC + secret）。
#[derive(Debug, Clone)]
pub struct WebConfig {
    /// 绑定地址。只接受环回地址（`127.0.0.1` / `localhost` / `::1` 的 IPv4 形式）。
    pub bind: String,
    /// 网页端口（`web.port`，默认 6875）。
    pub port: u16,
    /// aria2 JSON-RPC 端口（`aria2.rpc.port`，默认 6800）。
    pub rpc_port: u16,
    /// `rpc-secret`（**只在服务端内存里**，绝不回给浏览器）。
    pub secret: String,
}

impl WebConfig {
    /// 从配置层组装（`web.port` + `aria2.rpc.port` + `aria2.rpc.secret`）。
    ///
    /// secret 为空时说明还没生成（首次运行），此时网页能打开但反代会失败——
    /// 由调用方决定是否先生成 secret（面板/CLI 的职责，不在本模块）。
    pub fn from_config(config: &arihost_config::Config) -> Self {
        Self {
            bind: BIND_ADDR.to_string(),
            port: config.web.port,
            rpc_port: config.aria2.rpc.port,
            secret: config.aria2.rpc.secret.clone(),
        }
    }

    /// 给用户点的地址（架构 §4.5：URL 一律由 `web.port` 渲染，禁止硬编码 6875）。
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// 上游 JSON-RPC 地址。
    pub fn upstream(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.rpc_port))
    }
}

/// 起服务失败的原因（都映射到既有 `AH-` 码，u 面拿得到「为什么 + 怎么办」）。
#[derive(Debug)]
pub enum WebError {
    /// 试图绑非环回地址：只允许本机。
    NotLoopback {
        /// 被拒绝的地址。
        addr: String,
    },
    /// 端口被占用（含权限不足等 bind 失败）。
    PortInUse {
        /// 端口。
        port: u16,
        /// 底层原因。
        detail: String,
    },
}

impl WebError {
    /// 对应错误码。
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::NotLoopback { .. } => ErrorCode::Usage,
            Self::PortInUse { .. } => ErrorCode::PortConflict,
        }
    }

    /// 可读细节（进日志/面板提示）。
    pub fn detail(&self) -> String {
        match self {
            Self::NotLoopback { addr } => format!(
                "网页管理页只能监听本机环回地址，拒绝绑定 {addr}（架构 §3.3：只许 127.0.0.1）"
            ),
            Self::PortInUse { port, detail } => {
                format!("网页页端口被占用（127.0.0.1:{port}）：{detail}")
            }
        }
    }

    /// 给用户的下一步建议（面板直接显示）。
    pub const fn hint(&self) -> &'static str {
        match self {
            Self::NotLoopback { .. } => "把监听地址改回 127.0.0.1（网页管理页只在本机可用）",
            Self::PortInUse { .. } => {
                "换个端口（config.toml 的 [web] port），或先关掉占用该端口的程序"
            }
        }
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.code().code(), self.detail())
    }
}

impl std::error::Error for WebError {}

/// 正在提供服务的网页管理页。
///
/// 后台线程跑 accept 循环；`Server` 活着服务就活着（进程退出时随之结束）。
/// 想要「前台阻塞」用 [`serve_blocking`]。
pub struct Server {
    port: u16,
    context: Arc<Context>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Server")
            .field("port", &self.port)
            .field("context", &self.context)
            .finish()
    }
}

impl Server {
    /// 绑定并开始服务（后台线程）。
    pub fn start(config: WebConfig) -> Result<Self, WebError> {
        let listener = bind(&config)?;
        let port = listener
            .local_addr()
            .map_err(|error| WebError::PortInUse {
                port: config.port,
                detail: error.to_string(),
            })?
            .port();
        let context = Arc::new(Context::new(
            port,
            Proxy::new(config.upstream(), config.secret),
        ));
        let shared = Arc::clone(&context);
        // 线程 detached：服务寿命 = 进程寿命（面板/CLI 想停就退出进程）。
        std::thread::spawn(move || {
            let _ = serve_listener(listener, shared);
        });
        Ok(Self { port, context })
    }

    /// 实际端口（`port == 0` 时是内核分配的）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 给用户点的地址。
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// 地址。
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }

    /// 反代（selftest 会用到；也给面板做「测试连接」留口子）。
    pub fn proxy(&self) -> &Proxy {
        &self.context.proxy
    }
}

/// 绑定监听（只许环回；端口 0 表示让内核挑一个）。
///
/// 端口占用返回 [`WebError::PortInUse`]（`AH-E3001`），**不**静默换端口
/// （架构 §5：静默回落到 6875 会让用户的「一键打开」开到别人的服务上）。
pub fn bind(config: &WebConfig) -> Result<TcpListener, WebError> {
    if !is_loopback(&config.bind) {
        return Err(WebError::NotLoopback {
            addr: config.bind.clone(),
        });
    }
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, config.port))).map_err(|error| {
        WebError::PortInUse {
            port: config.port,
            detail: error.to_string(),
        }
    })
}

/// 这个地址字符串是不是环回。
///
/// 先整串比对（`::1` 里也有冒号，不能先按端口拆），再尝试拆 `name:port`。
fn is_loopback(addr: &str) -> bool {
    let addr = addr.trim().to_ascii_lowercase();
    if matches!(addr.as_str(), "127.0.0.1" | "localhost" | "::1") {
        return true;
    }
    match addr.rsplit_once(':') {
        // `127.0.0.1:6875` 这种也接受（但端口会被忽略，端口以 `config.port` 为准）。
        Some((name, port)) if !port.is_empty() && port.parse::<u16>().is_ok() => {
            matches!(name, "127.0.0.1" | "localhost")
        }
        _ => false,
    }
}

/// 前台阻塞式服务（CLI 用；`Ctrl+C` 结束）。
pub fn serve_blocking(config: WebConfig) -> Result<(), WebError> {
    let listener = bind(&config)?;
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(config.port);
    let context = Arc::new(Context::new(
        port,
        Proxy::new(config.upstream(), config.secret),
    ));
    serve_listener(listener, context).map_err(|error| WebError::PortInUse {
        port,
        detail: error.to_string(),
    })
}

// ---------------------------------------------------------------------------
// 自检（真起一个服务，用自己的最小客户端打它）
// ---------------------------------------------------------------------------

/// 自检：按配置起服务、逐项打点、把结果打到 stdout，**跑完即返回，服务随之关闭**。
///
/// 给 `ariahost --web-selftest` 用。自检的语义是「验完就走」；要前台常驻服务请用
/// [`serve_blocking`]。
///
/// 历史教训（2026-11-25，CI run 45524691482）：这里曾经以 `loop { sleep(3600s) }`
/// 常驻，导致任何调用方——包括 `crates/ariahost/src/cli.rs` 里那条
/// `web_selftest_is_parsed_and_runs` 单测——**永不返回**。CI 上 6875 端口空闲，
/// 自检真的把服务起起来了，于是测试挂到 job 超时；本地之所以"通过"，只是那台机器
/// 的 6875 被残留进程占着，`Server::start` 提前失败返回，纯属撞运气。
/// **自检函数不得包含无限阻塞。**
pub fn selftest() -> Result<(), String> {
    let server = selftest_report()?;
    println!(
        "SELFTEST done url={}（自检完成；本函数返回后服务关闭，要常驻请用 serve_blocking）",
        server.url()
    );
    Ok(())
}

/// 自检的「只报告不阻塞」版本：起服务、逐项断言并把 `SELFTEST` 行打到 stdout，
/// 全部通过则返回仍在服务的 [`Server`]（调用方自己决定何时退出）。
pub fn selftest_report() -> Result<Server, String> {
    let config = selftest_config();
    println!(
        "SELFTEST config web.port={} rpc.port={} secret={}",
        config.port,
        config.rpc_port,
        if config.secret.is_empty() {
            "<空>（反代会被 aria2 拒绝，属于配置缺失）"
        } else {
            "<已配置，不显示>"
        }
    );
    let server = Server::start(config).map_err(|error| {
        println!(
            "SELFTEST start ok=0 code={} detail={} hint={}",
            error.code().code(),
            error.detail(),
            error.hint()
        );
        error.to_string()
    })?;
    let addr = server.addr();
    let port = server.port();
    let host = format!("127.0.0.1:{port}");
    let mut failures = 0u32;
    let mut check = |name: &str, ok: bool, detail: &str| {
        if ok {
            println!("SELFTEST check={name} ok=1 {detail}");
        } else {
            failures += 1;
            println!("SELFTEST check={name} ok=0 {detail}");
        }
    };

    // 1) 首页 + 安全头
    match request_once(addr, "GET", "/", &host, b"") {
        Ok(response) => {
            let text = response.text();
            check(
                "index",
                response.status == 200
                    && response.header("content-type") == Some("text/html; charset=utf-8")
                    && response
                        .header("content-security-policy")
                        .is_some_and(|value| value.contains("default-src 'self'"))
                    && response.header("access-control-allow-origin").is_none()
                    && text.contains("AriaHost 下载任务"),
                &format!(
                    "status={} content-type={:?} csp={:?} body={}B",
                    response.status,
                    response.header("content-type"),
                    response.header("content-security-policy"),
                    response.body.len()
                ),
            );
        }
        Err(error) => check("index", false, &error.to_string()),
    }

    // 1b) favicon（task-103）：两条独立的二进制资产。报字节数，方便一眼看出
    // 「是不是把 361 KB 的多档 ICO 塞进来了」。
    for (name, path, kind, magic) in [
        (
            "favicon-ico",
            "/favicon.ico",
            "image/x-icon",
            &[0x00u8, 0x00, 0x01, 0x00][..],
        ),
        (
            "favicon-png",
            "/favicon-32.png",
            "image/png",
            &[0x89u8, 0x50, 0x4e, 0x47][..],
        ),
    ] {
        match request_once(addr, "GET", path, &host, b"") {
            Ok(response) => check(
                name,
                response.status == 200
                    && response.header("content-type") == Some(kind)
                    && response.body.starts_with(magic),
                &format!(
                    "status={} content-type={:?} body={}B cache-control={:?}",
                    response.status,
                    response.header("content-type"),
                    response.body.len(),
                    response.header("cache-control")
                ),
            ),
            Err(error) => check(name, false, &error.to_string()),
        }
    }

    // 2) 静态资源
    for (path, expected) in [
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/style.css", "text/css; charset=utf-8"),
    ] {
        match request_once(addr, "GET", path, &host, b"") {
            Ok(response) => check(
                path,
                response.status == 200 && response.header("content-type") == Some(expected),
                &format!("status={} {}B", response.status, response.body.len()),
            ),
            Err(error) => check(path, false, &error.to_string()),
        }
    }

    // 3) 未知路径 404
    match request_once(addr, "GET", "/nope", &host, b"") {
        Ok(response) => check(
            "404",
            response.status == 404,
            &format!("status={}", response.status),
        ),
        Err(error) => check("404", false, &error.to_string()),
    }

    // 4) Host 头防 rebinding
    match request_once(addr, "GET", "/", "evil.example:6875", b"") {
        Ok(response) => check(
            "host-rebinding",
            response.status == 403,
            &format!("status={}", response.status),
        ),
        Err(error) => check("host-rebinding", false, &error.to_string()),
    }

    // 5) shutdown 必须被拦（不需要 aria2 在跑）
    let shutdown = br#"{"jsonrpc":"2.0","id":1,"method":"aria2.shutdown","params":[]}"#;
    match request_once(addr, "POST", "/jsonrpc", &host, shutdown) {
        Ok(response) => {
            let text = response.text();
            check(
                "shutdown-blocked",
                response.status == 200 && text.contains("-32001") && text.contains("面板"),
                &format!("status={} body={text}", response.status),
            );
        }
        Err(error) => check("shutdown-blocked", false, &error.to_string()),
    }

    // 6) 白名单与反代（aria2 没起时应是 503 + 人话）
    let version = br#"{"jsonrpc":"2.0","id":2,"method":"aria2.getVersion","params":[]}"#;
    match request_once(addr, "POST", "/jsonrpc", &host, version) {
        Ok(response) => {
            let text = response.text();
            let ok = response.status == 200 || (response.status == 503 && text.contains("面板"));
            check(
                "proxy",
                ok,
                &format!("status={} body={}", response.status, truncate(&text, 160)),
            );
        }
        Err(error) => check("proxy", false, &error.to_string()),
    }

    // 7) 白名单外的方法
    let forbidden = br#"{"jsonrpc":"2.0","id":3,"method":"aria2.changeGlobalOption","params":[]}"#;
    match request_once(addr, "POST", "/jsonrpc", &host, forbidden) {
        Ok(response) => check(
            "whitelist",
            response.status == 200 && response.text().contains("-32601"),
            &format!(
                "status={} body={}",
                response.status,
                truncate(&response.text(), 160)
            ),
        ),
        Err(error) => check("whitelist", false, &error.to_string()),
    }

    println!("SELFTEST url={} failures={failures}", server.url());
    if failures == 0 {
        Ok(server)
    } else {
        Err(format!("{failures} 项自检失败"))
    }
}

/// 自检用的配置：读 `config.toml`，失败就用默认值（selftest 不该被配置问题挡住）。
fn selftest_config() -> WebConfig {
    match arihost_config::load() {
        Ok(config) => WebConfig::from_config(&config),
        Err(error) => {
            println!("SELFTEST warn 读配置失败（用默认值）：{error}");
            WebConfig {
                bind: BIND_ADDR.to_string(),
                port: 6875,
                rpc_port: 6800,
                secret: String::new(),
            }
        }
    }
}

/// 截断长文本（selftest 打印用）。
fn truncate(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

/// 配置层的 re-export（本文件用 [[`arihost_config`]] 指代 `ariahost_common::config`）。
use ariahost_common::config as arihost_config;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::Mutex;
    use std::thread;

    /// 假上游：记录收到的请求体，回一个固定的 JSON-RPC 成功响应。
    struct FakeUpstream {
        addr: SocketAddr,
        bodies: Arc<Mutex<Vec<String>>>,
    }

    impl FakeUpstream {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定假上游");
            let addr = listener.local_addr().expect("地址");
            let bodies = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&bodies);
            thread::spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(mut stream) = incoming else { continue };
                    let Ok(request) = crate::http::read_request(&mut stream, 4 * 1024 * 1024)
                    else {
                        continue;
                    };
                    recorded
                        .lock()
                        .expect("锁")
                        .push(String::from_utf8_lossy(&request.body).into_owned());
                    let body = br#"{"jsonrpc":"2.0","id":1,"result":{"version":"1.37.0"}}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body);
                }
            });
            Self { addr, bodies }
        }

        fn received(&self) -> Vec<String> {
            self.bodies.lock().expect("锁").clone()
        }
    }

    /// 起一个指向假上游的服务（端口 0 = 内核分配）。
    fn server_with_fake_upstream(secret: &str) -> (Server, FakeUpstream) {
        let upstream = FakeUpstream::start();
        let config = WebConfig {
            bind: "127.0.0.1".to_owned(),
            port: 0,
            rpc_port: upstream.addr.port(),
            secret: secret.to_owned(),
        };
        let server = Server::start(config).expect("起服务");
        (server, upstream)
    }

    fn host_of(server: &Server) -> String {
        format!("127.0.0.1:{}", server.port())
    }

    #[test]
    fn static_assets_are_served_with_security_headers() {
        let (server, _upstream) = server_with_fake_upstream("s3cret");
        let host = host_of(&server);

        let index = request_once(server.addr(), "GET", "/", &host, b"").expect("GET /");
        assert_eq!(index.status, 200);
        assert_eq!(
            index.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(
            index.header("content-security-policy"),
            Some("default-src 'self'")
        );
        assert_eq!(index.header("x-content-type-options"), Some("nosniff"));
        assert!(
            index.header("access-control-allow-origin").is_none(),
            "同源反代不许发 CORS 头"
        );
        assert!(index.text().contains("AriaHost 下载任务"));
        // 浏览器拿不到 secret：页面里不该出现它。
        assert!(!index.text().contains("s3cret"));

        let js = request_once(server.addr(), "GET", "/app.js", &host, b"").expect("GET /app.js");
        assert_eq!(js.status, 200);
        assert_eq!(
            js.header("content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert!(!js.text().contains("s3cret"), "脚本里也不许有 secret");

        let css =
            request_once(server.addr(), "GET", "/style.css", &host, b"").expect("GET /style.css");
        assert_eq!(css.status, 200);
        assert_eq!(css.header("content-type"), Some("text/css; charset=utf-8"));
    }

    #[test]
    fn healthz_stays_cheap_and_favicon_is_really_served() {
        let (server, _upstream) = server_with_fake_upstream("s3cret");
        let host = host_of(&server);
        let health = request_once(server.addr(), "GET", "/healthz", &host, b"").expect("healthz");
        assert_eq!(health.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&health.body).expect("JSON");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], ariahost_common::VERSION);

        // WEB-3（task-103）：`/favicon.ico` 过去返回 204（标签页无图标，用户反馈）。
        // 现在必须真给字节——**不许**为了测试变绿改回 204。
        let icon = request_once(server.addr(), "GET", "/favicon.ico", &host, b"").expect("图标");
        assert_eq!(icon.status, 200);
        assert_eq!(icon.header("content-type"), Some("image/x-icon"));
        assert!(
            !icon.body.is_empty(),
            "图标体不许为空（204 的老行为已废弃）"
        );
        assert!(icon.body.len() <= 64 * 1024, "小尺寸图标不许超过 64 KiB");
        // 图标给浏览器缓存：不是 no-store（那是数据接口的口径）。
        let cache = icon.header("cache-control").unwrap_or_default();
        assert!(cache.contains("max-age"), "图标应可缓存，实际：{cache}");
    }

    #[test]
    fn unknown_path_is_404_and_wrong_method_is_405() {
        let (server, _upstream) = server_with_fake_upstream("s3cret");
        let host = host_of(&server);
        assert_eq!(
            request_once(server.addr(), "GET", "/nope", &host, b"")
                .expect("404")
                .status,
            404
        );
        assert_eq!(
            request_once(server.addr(), "PUT", "/jsonrpc", &host, b"{}")
                .expect("405")
                .status,
            405
        );
    }

    #[test]
    fn host_header_is_enforced() {
        let (server, _upstream) = server_with_fake_upstream("s3cret");
        let wrong = request_once(server.addr(), "GET", "/", "evil.example:6875", b"")
            .expect("rebinding 请求");
        assert_eq!(wrong.status, 403, "DNS rebinding 必须被拒");

        // 缺 Host 头：直接手写原始请求。
        let mut stream = TcpStream::connect(server.addr()).expect("连接");
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").expect("写请求");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).expect("读响应");
        assert!(raw.starts_with("HTTP/1.1 400"), "{raw}");
    }

    #[test]
    fn proxy_injects_the_server_token_and_drops_browser_tokens() {
        let (server, upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        // 浏览器塞了一个假 token（想试探），还带了自己的参数。
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"aria2.tellActive","params":["token:guess",["gid"]]}"#;
        let response = request_once(server.addr(), "POST", "/jsonrpc", &host, body).expect("反代");
        assert_eq!(response.status, 200);
        assert_eq!(
            response.text(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"version":"1.37.0"}}"#,
            "上游响应必须原样返回"
        );

        let received = upstream.received();
        assert_eq!(received.len(), 1, "上游应收到一次请求");
        let forwarded = &received[0];
        assert!(
            forwarded.contains(r#""params":["token:real-secret",["gid"]]"#),
            "必须注入服务端 token 且保留业务参数：{forwarded}"
        );
        assert!(
            !forwarded.contains("guess"),
            "浏览器传的假 token 必须被丢掉：{forwarded}"
        );
    }

    #[test]
    fn proxy_works_when_browser_sends_no_token_at_all() {
        let (server, upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion"}"#;
        assert_eq!(
            request_once(server.addr(), "POST", "/jsonrpc", &host, body)
                .expect("反代")
                .status,
            200
        );
        let forwarded = upstream.received().join("\n");
        assert!(
            forwarded.contains(r#""params":["token:real-secret"]"#),
            "{forwarded}"
        );
    }

    #[test]
    fn shutdown_is_intercepted_and_never_reaches_aria2() {
        let (server, upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        for method in ["aria2.shutdown", "aria2.forceShutdown"] {
            let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#);
            let response = request_once(server.addr(), "POST", "/jsonrpc", &host, body.as_bytes())
                .expect("反代");
            assert_eq!(response.status, 200, "{method}");
            let value: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
            assert_eq!(value["error"]["code"], -32001, "{method}");
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("面板"),
                "{method}：{}",
                value["error"]["message"]
            );
            assert_eq!(value["error"]["data"]["ah_code"], "AH-E3003");
        }
        assert!(
            upstream.received().is_empty(),
            "拦截必须发生在上游之前（一条都不许转发）"
        );
    }

    #[test]
    fn multiplexed_and_dangerous_methods_are_rejected() {
        let (server, upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        let cases = [
            "aria2.changeGlobalOption",
            "aria2.saveSession",
            "aria2.purgeDownloadResult",
            "aria2.pauseAll",
            "system.multicall",
            "system.listMethods",
        ];
        for method in cases {
            let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":[]}}"#);
            let response = request_once(server.addr(), "POST", "/jsonrpc", &host, body.as_bytes())
                .expect("反代");
            let value: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
            assert_eq!(value["error"]["code"], -32601, "{method} 应被拒");
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("白名单"),
                "{method}"
            );
        }
        assert!(upstream.received().is_empty(), "被拒的方法一条都不许转发");
    }

    #[test]
    fn batch_request_with_one_denied_call_is_rejected_wholesale() {
        let (server, upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        let body = br#"[
            {"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":[]},
            {"jsonrpc":"2.0","id":2,"method":"aria2.shutdown","params":[]}
        ]"#;
        let response = request_once(server.addr(), "POST", "/jsonrpc", &host, body).expect("批量");
        let value: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(value["error"]["code"], -32001, "批量里有一条越权就整批拒绝");
        assert!(upstream.received().is_empty(), "fail-closed：整批不转发");
    }

    #[test]
    fn not_json_gets_400() {
        let (server, _upstream) = server_with_fake_upstream("real-secret");
        let host = host_of(&server);
        let response =
            request_once(server.addr(), "POST", "/jsonrpc", &host, b"not json").expect("请求");
        assert_eq!(response.status, 400);
        let value: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        assert_eq!(value["error"]["code"], -32700);
    }

    #[test]
    fn oversized_body_gets_413_without_reading_it() {
        let (server, _upstream) = server_with_fake_upstream("real-secret");
        // 只发头（声明 2 MiB），不发体：服务端应在读体之前就 413。
        let mut stream = TcpStream::connect(server.addr()).expect("连接");
        let head = format!(
            "POST /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n\r\n",
            server.port(),
            2 * 1024 * 1024
        );
        stream.write_all(head.as_bytes()).expect("写头");
        let mut raw = String::new();
        stream.read_to_string(&mut raw).expect("读响应");
        assert!(raw.starts_with("HTTP/1.1 413"), "{raw}");
    }

    #[test]
    fn upstream_down_gives_a_friendly_503() {
        // 指向一个没人监听的端口：反代应给人话而不是 500。
        let free = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("找空闲端口");
            listener.local_addr().expect("地址").port()
        };
        let config = WebConfig {
            bind: "127.0.0.1".to_owned(),
            port: 0,
            rpc_port: free,
            secret: "real-secret".to_owned(),
        };
        let server = Server::start(config).expect("起服务");
        let host = host_of(&server);
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"aria2.getVersion","params":[]}"#;
        let response = request_once(server.addr(), "POST", "/jsonrpc", &host, body).expect("反代");
        assert_eq!(response.status, 503);
        let value: serde_json::Value = serde_json::from_slice(&response.body).expect("JSON");
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("请先在 AriaHost 面板启动"), "{message}");
        assert_eq!(value["error"]["data"]["ah_code"], "AH-E2003");
    }

    #[test]
    fn non_loopback_bind_is_refused() {
        for bind in ["0.0.0.0", "192.168.1.10", "::", "evil.example"] {
            let config = WebConfig {
                bind: bind.to_owned(),
                port: 0,
                rpc_port: 6800,
                secret: "x".to_owned(),
            };
            let error = Server::start(config).expect_err("必须拒绝");
            assert!(
                matches!(error, WebError::NotLoopback { .. }),
                "{bind} → {error:?}"
            );
            assert_eq!(error.code().code(), "AH-E1001");
        }
    }

    #[test]
    fn port_in_use_is_reported_with_a_hint() {
        let squatter = TcpListener::bind("127.0.0.1:0").expect("占位");
        let port = squatter.local_addr().expect("地址").port();
        let config = WebConfig {
            bind: "127.0.0.1".to_owned(),
            port,
            rpc_port: 6800,
            secret: "x".to_owned(),
        };
        let error = Server::start(config).expect_err("端口被占必须报错");
        assert!(matches!(error, WebError::PortInUse { .. }), "{error:?}");
        assert_eq!(error.code().code(), "AH-E3001");
        assert!(
            error.detail().contains("网页页端口被占用"),
            "{}",
            error.detail()
        );
        assert!(error.hint().contains("端口"), "{}", error.hint());
    }

    #[test]
    fn url_is_rendered_from_the_actual_port_only() {
        // 架构 §4.5：URL 一律由配置/实际端口渲染，不许出现硬编码 6875。
        let (server, _upstream) = server_with_fake_upstream("x");
        assert_eq!(server.url(), format!("http://127.0.0.1:{}/", server.port()));
        assert!(server.port() > 0);
    }

    #[test]
    fn loopback_detection_accepts_the_three_forms() {
        for addr in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "127.0.0.1:6875",
            " ::1 ",
        ] {
            assert!(is_loopback(addr), "{addr}");
        }
        for addr in ["0.0.0.0", "10.0.0.1", "::", "127.0.0.2", "example.com"] {
            assert!(!is_loopback(addr), "{addr}");
        }
    }

    #[test]
    fn browser_cannot_reach_the_secret_through_any_response() {
        // 全站扫一遍：任何响应体/响应头都不许出现 secret。
        let (server, _upstream) = server_with_fake_upstream("top-secret-value");
        let host = host_of(&server);
        for path in ["/", "/index.html", "/app.js", "/style.css", "/healthz"] {
            let response = request_once(server.addr(), "GET", path, &host, b"").expect("GET");
            assert!(
                !response.text().contains("top-secret-value"),
                "{path} 的响应体泄露了 secret"
            );
            let headers = format!("{:?}", response.headers);
            assert!(
                !headers.contains("top-secret-value"),
                "{path} 的响应头泄露了 secret：{headers}"
            );
        }
    }
}
