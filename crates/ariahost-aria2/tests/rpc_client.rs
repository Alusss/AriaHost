//! [`RpcClient`] 的假服务器测试。
//!
//! 本机与 CI 都**不拉起真实 aria2**：用线程内 `TcpListener` 当对端，逐字节控制响应，
//! 才能覆盖「Content-Length 优先 / 无长度读到 EOF / 读超时 / 非 JSON / HTTP 非 2xx」这些
//! 真实进程很难稳定复现的分支。
//!
//! 覆盖的验收项（task-28 A.5）：token 注入、正常响应、error 响应、MethodNotFound、超时。

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ariahost_aria2::{GlobalStat, RpcCallError, RpcClient, VersionInfo};
use ariahost_common::ErrorCode;
use serde_json::{json, Value};

/// HTTP 头结束标记。
const HEADER_END: &[u8] = b"\r\n\r\n";

/// 客户端单次调用用的超时（比 100ms 的注入超时宽，避免正常用例偶发超时）。
const CLIENT_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// 假服务器
// ---------------------------------------------------------------------------

/// 假服务器：接受 `count` 条连接，每条都交给 `respond` 生成**原始响应字节**。
///
/// 故意不做「按需计数」的魔法：调用方必须让 `count` 与实际调用次数一致，
/// `finish()` 会在服务器线程结束后交出收到的全部请求。
struct FakeServer {
    addr: SocketAddr,
    requests: Receiver<Value>,
    handle: Option<JoinHandle<()>>,
}

impl FakeServer {
    fn start<F>(count: usize, respond: F) -> Self
    where
        F: Fn(Value) -> String + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定假服务器失败");
        let addr = listener.local_addr().expect("取假服务器地址失败");
        let (sender, requests) = mpsc::channel();

        let handle = thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().expect("接受连接失败");
                let body = read_request(&mut stream);
                let request: Value = serde_json::from_slice(&body).expect("请求体必须是 JSON");
                let response = respond(request.clone());
                sender.send(request).expect("回传请求失败");
                stream.write_all(response.as_bytes()).expect("写响应失败");
                let _ = stream.flush();
                // 离开作用域即关闭连接：没有 Content-Length 的用例靠这个 EOF 收尾。
            }
        });

        Self {
            addr,
            requests,
            handle: Some(handle),
        }
    }

    fn one<F>(respond: F) -> Self
    where
        F: Fn(Value) -> String + Send + 'static,
    {
        Self::start(1, respond)
    }

    fn client(&self, secret: Option<&str>) -> RpcClient {
        RpcClient::with_timeout(self.addr, secret.map(str::to_owned), CLIENT_TIMEOUT)
    }

    /// 收齐请求并等服务器线程退出。
    fn finish(mut self) -> Vec<Value> {
        let requests: Vec<Value> = self.requests.iter().collect();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("假服务器线程 panic");
        }
        requests
    }
}

/// 从连接里读出一个完整 HTTP 请求，返回 body（即 JSON-RPC 请求体）。
fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(head_end) = find(&buffer, HEADER_END) {
            let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
            let length = content_length(&head);
            let body_start = head_end + HEADER_END.len();
            if buffer.len() >= body_start + length {
                return buffer[body_start..body_start + length].to_vec();
            }
        }
        let read = stream.read(&mut chunk).expect("假服务器读请求失败");
        assert!(read > 0, "客户端在请求发完前就断开了");
        buffer.extend_from_slice(&chunk[..read]);
        assert!(buffer.len() < 1 << 20, "请求体异常大");
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse().expect("Content-Length 必须是数字");
            }
        }
    }
    panic!("客户端请求必须带 Content-Length：{head}");
}

// ---------------------------------------------------------------------------
// 响应构造
// ---------------------------------------------------------------------------

/// 200 + Content-Length 的标准响应。
fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn jsonrpc_ok(id: &Value, result: Value) -> String {
    http_ok(&json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

fn jsonrpc_error(id: &Value, code: i64, message: &str) -> String {
    http_ok(
        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            .to_string(),
    )
}

fn http_status(status: u16, reason: &str) -> String {
    format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n")
}

/// 只读走请求、不回响应并保持连接的服务器：用来验证读超时。
fn serve_silent(hold: Duration) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定假服务器失败");
    let addr = listener.local_addr().expect("取假服务器地址失败");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("接受连接失败");
        let _ = read_request(&mut stream);
        thread::sleep(hold);
        // 只有测试自己等够时间后连接才会关闭：客户端必须靠自己的超时先返回。
    });
    (addr, handle)
}

// ---------------------------------------------------------------------------
// A. token 注入与成功响应
// ---------------------------------------------------------------------------

#[test]
fn injects_token_as_first_param_and_parses_version() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_ok(
            &id,
            json!({"version": "1.37.0", "enabledFeatures": ["BitTorrent", "GZip"]}),
        )
    });

    let version: VersionInfo = server
        .client(Some("s3cr3t"))
        .get_version()
        .expect("getVersion 应成功");
    assert_eq!(version.version, "1.37.0");
    assert_eq!(
        version.enabled_features,
        vec!["BitTorrent".to_owned(), "GZip".to_owned()]
    );

    let requests = server.finish();
    assert_eq!(requests.len(), 1, "一次调用只应发一条请求");
    let request = &requests[0];
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["method"], "aria2.getVersion");
    assert_eq!(
        request["params"],
        json!(["token:s3cr3t"]),
        "secret 必须自动插到 params 第一项"
    );
    assert!(
        request["id"].is_string(),
        "id 必须是字符串：{}",
        request["id"]
    );
}

#[test]
fn omits_token_when_no_secret_configured() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_ok(&id, json!({"version": "1.37.0"}))
    });

    let version = server
        .client(None)
        .get_version()
        .expect("无 secret 时也应成功");
    assert!(
        version.enabled_features.is_empty(),
        "缺 enabledFeatures 时按空列表处理"
    );

    let requests = server.finish();
    assert_eq!(
        requests[0]["params"],
        json!([]),
        "无 secret 时不得注入 token"
    );
}

// ---------------------------------------------------------------------------
// B. getGlobalStat：字符串与数字两种线形状
// ---------------------------------------------------------------------------

#[test]
fn parses_global_stat_from_string_fields() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_ok(
            &id,
            json!({
                "downloadSpeed": "21849",
                "uploadSpeed": "0",
                "numActive": "1",
                "numWaiting": "2",
                "numStopped": "3",
                "numStoppedTotal": "1024"
            }),
        )
    });

    let stat: GlobalStat = server
        .client(None)
        .get_global_stat()
        .expect("getGlobalStat 应成功");
    assert_eq!(stat.download_speed, 21_849);
    assert_eq!(stat.upload_speed, 0);
    assert_eq!(stat.num_active, 1);
    assert_eq!(stat.num_waiting, 2);
    assert_eq!(stat.num_stopped, 3);
    assert_eq!(stat.num_stopped_total, Some(1_024));
    server.finish();
}

#[test]
fn accepts_numeric_global_stat_fields_and_missing_total() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_ok(
            &id,
            json!({
                "downloadSpeed": 7,
                "uploadSpeed": 0,
                "numActive": 0,
                "numWaiting": 0,
                "numStopped": 0
            }),
        )
    });

    let stat = server
        .client(None)
        .get_global_stat()
        .expect("数字字段也应可解析");
    assert_eq!(stat.download_speed, 7);
    assert_eq!(stat.num_stopped_total, None, "旧版本没有 numStoppedTotal");
    server.finish();
}

// ---------------------------------------------------------------------------
// C. error 响应与 MethodNotFound
// ---------------------------------------------------------------------------

#[test]
fn maps_remote_error_and_preserves_aria2_code_and_message() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_error(&id, 1, "Unauthorized")
    });

    let error = server
        .client(Some("wrong-secret"))
        .get_version()
        .expect_err("error 响应必须报错");

    match &error {
        RpcCallError::Remote { error } => {
            assert_eq!(error.code, 1);
            assert_eq!(error.message, "Unauthorized");
        }
        other => panic!("期望 Remote，实际 {other:?}"),
    }
    assert_eq!(
        error.code(),
        None,
        "远端语义错误没有对应既有错误码，不得硬套"
    );
    let text = error.to_string();
    assert!(
        text.contains("Unauthorized"),
        "message 原文必须保留：{text}"
    );
    assert!(text.contains('1'), "code 原文必须保留：{text}");
    server.finish();
}

#[test]
fn detects_method_not_found_for_capability_probe() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        jsonrpc_error(&id, -32601, "Method not found")
    });

    let error = server
        .client(None)
        .save_session()
        .expect_err("-32601 必须被识别出来");

    match &error {
        RpcCallError::MethodNotFound { method, error } => {
            assert_eq!(method, "aria2.saveSession");
            assert_eq!(error.code, -32601);
            assert_eq!(error.message, "Method not found");
        }
        other => panic!("期望 MethodNotFound，实际 {other:?}"),
    }
    assert_eq!(error.code(), None, "能力探测信号不应映射成既有错误码");
    assert!(error.to_string().contains("aria2.saveSession"), "{error}");
    server.finish();
}

// ---------------------------------------------------------------------------
// D. HTTP 状态码映射
// ---------------------------------------------------------------------------

#[test]
fn maps_http_403_and_404_to_http_rejected() {
    for (status, reason) in [(403u16, "Forbidden"), (404, "Not Found")] {
        let server = FakeServer::one(move |_request| http_status(status, reason));
        let error = server
            .client(None)
            .get_version()
            .expect_err("非 2xx 必须报错");
        assert!(
            matches!(&error, RpcCallError::HttpStatus { status: actual } if *actual == status),
            "{status} 应映射成 HttpStatus，实际 {error:?}"
        );
        assert_eq!(error.code(), Some(ErrorCode::HttpRejected));
        assert_eq!(error.code().map(ErrorCode::code), Some("AH-E3003"));
        assert!(error.to_string().contains("AH-E3003"), "{error}");
        server.finish();
    }
}

#[test]
fn keeps_other_http_status_without_forcing_a_code() {
    let server = FakeServer::one(|_request| http_status(500, "Internal Server Error"));
    let error = server
        .client(None)
        .get_version()
        .expect_err("非 2xx 必须报错");
    assert!(matches!(&error, RpcCallError::HttpStatus { status } if *status == 500));
    assert_eq!(error.code(), None, "500 不是 403/404，不得谎报 AH-E3003");
    assert!(error.to_string().contains("500"), "{error}");
    server.finish();
}

// ---------------------------------------------------------------------------
// E. 传输层：超时与连接拒绝
// ---------------------------------------------------------------------------

#[test]
fn times_out_with_handshake_code_when_server_never_responds() {
    let (addr, handle) = serve_silent(Duration::from_millis(400));
    let client = RpcClient::with_timeout(addr, Some("tok".to_owned()), Duration::from_millis(100));

    let started = Instant::now();
    let error = client.get_version().expect_err("服务器不响应必须超时报错");
    let elapsed = started.elapsed();

    assert!(
        matches!(&error, RpcCallError::Transport { timed_out, .. } if *timed_out),
        "超时应归为 Transport+timed_out，实际 {error:?}"
    );
    assert_eq!(
        error.code(),
        Some(ErrorCode::RpcHandshakeFailed),
        "超时必须映射 AH-E2003"
    );
    assert!(error.to_string().contains("AH-E2003"), "{error}");
    assert!(
        elapsed >= Duration::from_millis(50),
        "应真的等超时而不是立即返回：{elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "必须在服务器关闭前按自己的超时返回：{elapsed:?}"
    );
    handle.join().expect("静默服务器线程 panic");
}

#[test]
fn maps_connection_refused_to_handshake_failed() {
    // 先占一个端口再放掉：连接该地址必定被拒。
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定失败");
    let addr = listener.local_addr().expect("取地址失败");
    drop(listener);

    let client = RpcClient::with_timeout(addr, None, Duration::from_millis(500));
    let error = client.get_version().expect_err("连接被拒必须报错");
    assert!(
        matches!(&error, RpcCallError::Connect { .. }),
        "应归为 Connect，实际 {error:?}"
    );
    assert_eq!(error.code(), Some(ErrorCode::RpcHandshakeFailed));
    assert!(error.to_string().contains("AH-E2003"), "{error}");
}

// ---------------------------------------------------------------------------
// F. 响应收尾规则：Content-Length 优先 / 无长度读到 EOF / 非 JSON
// ---------------------------------------------------------------------------

#[test]
fn honors_content_length_and_ignores_trailing_bytes() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        let body = json!({"jsonrpc": "2.0", "id": id, "result": {"version": "1.37.0"}}).to_string();
        // 长度按 body 声明，后面再灌垃圾：只有严格按 Content-Length 截断才能解析成功。
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}\r\nTRAILING-GARBAGE",
            body.len()
        )
    });

    let version = server
        .client(None)
        .get_version()
        .expect("必须只读 Content-Length 指定的字节");
    assert_eq!(version.version, "1.37.0");
    server.finish();
}

#[test]
fn reads_body_to_eof_when_content_length_absent() {
    let server = FakeServer::one(|request| {
        let id = request["id"].clone();
        let body = json!({"jsonrpc": "2.0", "id": id, "result": {"version": "1.36.0"}}).to_string();
        format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}")
    });

    let version = server
        .client(None)
        .get_version()
        .expect("无 Content-Length 应读到 EOF");
    assert_eq!(version.version, "1.36.0");
    server.finish();
}

#[test]
fn rejects_malformed_response_bodies() {
    // 200 但不是 JSON-RPC。
    let server = FakeServer::one(|_request| http_ok("not-json"));
    let error = server
        .client(None)
        .get_version()
        .expect_err("非 JSON 必须报错");
    assert!(matches!(&error, RpcCallError::Protocol { .. }), "{error:?}");
    assert_eq!(error.code(), Some(ErrorCode::RpcHandshakeFailed));
    server.finish();

    // 200 + JSON，但既无 result 也无 error。
    let server = FakeServer::one(|_request| http_ok(r#"{"jsonrpc":"2.0"}"#));
    let error = server
        .client(None)
        .get_version()
        .expect_err("缺 result/error 必须报错");
    assert!(matches!(&error, RpcCallError::Protocol { .. }), "{error:?}");
    server.finish();
}

// ---------------------------------------------------------------------------
// G. 语义化方法的方法名与参数形状（一次覆盖 10 个）
// ---------------------------------------------------------------------------

#[test]
fn semantic_methods_use_expected_wire_names_and_params() {
    let server = FakeServer::start(10, |request| {
        let id = request["id"].clone();
        let method = request["method"].as_str().expect("method 必须是字符串");
        let result = match method {
            "aria2.getVersion" => json!({"version": "1.37.0", "enabledFeatures": []}),
            "aria2.getGlobalStat" => json!({
                "downloadSpeed": "0",
                "uploadSpeed": "0",
                "numActive": "0",
                "numWaiting": "0",
                "numStopped": "0"
            }),
            "aria2.tellActive" | "aria2.tellWaiting" | "aria2.tellStopped" => json!([]),
            "aria2.addUri" | "aria2.pause" | "aria2.unpause" | "aria2.remove" => json!("gid-1"),
            "aria2.saveSession" => json!("OK"),
            other => panic!("未预期的方法 {other}"),
        };
        jsonrpc_ok(&id, result)
    });

    let client = server.client(Some("tok"));
    client.get_version().expect("getVersion");
    client.get_global_stat().expect("getGlobalStat");
    assert!(client.tell_active().expect("tellActive").is_empty());
    assert!(client.tell_waiting(-1, 50).expect("tellWaiting").is_empty());
    assert!(client.tell_stopped(0, 25).expect("tellStopped").is_empty());
    assert_eq!(
        client
            .add_uri(&["http://example.org/a.iso".to_owned()])
            .expect("addUri"),
        "gid-1"
    );
    assert_eq!(client.pause("gid-1").expect("pause"), "gid-1");
    assert_eq!(client.unpause("gid-1").expect("unpause"), "gid-1");
    assert_eq!(client.remove("gid-1").expect("remove"), "gid-1");
    client.save_session().expect("saveSession");

    let requests = server.finish();
    assert_eq!(requests.len(), 10, "每个语义方法各一条请求");

    let expected_methods = [
        "aria2.getVersion",
        "aria2.getGlobalStat",
        "aria2.tellActive",
        "aria2.tellWaiting",
        "aria2.tellStopped",
        "aria2.addUri",
        "aria2.pause",
        "aria2.unpause",
        "aria2.remove",
        "aria2.saveSession",
    ];
    for (request, expected) in requests.iter().zip(expected_methods) {
        assert_eq!(
            request["method"], expected,
            "方法名必须与 aria2 manual 一致"
        );
        assert_eq!(
            request["params"][0], "token:tok",
            "{expected} 必须自动注入 token"
        );
    }

    // 参数顺序与形状（aria2 按位置取参，顺序错了就是语义错误）。
    assert_eq!(requests[0]["params"], json!(["token:tok"]));
    assert_eq!(requests[3]["params"], json!(["token:tok", -1, 50]));
    assert_eq!(requests[4]["params"], json!(["token:tok", 0, 25]));
    assert_eq!(
        requests[5]["params"],
        json!(["token:tok", ["http://example.org/a.iso"]])
    );
    assert_eq!(requests[6]["params"], json!(["token:tok", "gid-1"]));
}

// ---------------------------------------------------------------------------
// H. 配置与脱敏
// ---------------------------------------------------------------------------

#[test]
fn local_endpoint_defaults_to_loopback_6800() {
    let client = RpcClient::local(6800, None);
    assert_eq!(client.addr().to_string(), "127.0.0.1:6800");
    assert_eq!(client.timeout(), Duration::from_secs(5));
    assert!(!client.has_secret());
}

#[test]
fn debug_redacts_secret() {
    let client = RpcClient::local(6800, Some("s3cr3t".to_owned()));
    let debug = format!("{client:?}");
    assert!(!debug.contains("s3cr3t"), "Debug 不得回显 secret：{debug}");
    assert!(
        debug.contains("token:***"),
        "secret 应显示为 token:***：{debug}"
    );
    assert!(debug.contains("127.0.0.1:6800"), "{debug}");
}
