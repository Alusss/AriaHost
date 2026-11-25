//! 手写 HTTP/1.1 的服务端最小实现（只覆盖本页面需要的那一小块）。
//!
//! 为什么不引 axum/hyper/tokio：本仓库其余部分（`aria2` 客户端的传输层、核心下载）
//! 都是手写阻塞 HTTP + `ureq`，网页管理页只在**本机环回**上服务一个小页面，
//! 多引一套异步 HTTP 栈只会让二进制变大、CI 变慢、攻击面变大（依赖即攻击面）。
//!
//! 刻意**只做**：
//! - `GET`/`HEAD` 取静态资源，`POST` 打 JSON-RPC 反代；
//! - 每连接**一个请求**后关闭（不实现 keep-alive 状态机，省掉超时/流水线的一堆坑）；
//! - 请求行与每个头都有长度上限，请求体上限由调用方给（网页端是 1 MiB）。
//!
//! 不做（明确写下来，免得后来者以为是漏了）：`Transfer-Encoding: chunked`、
//! 100-continue、HTTP/2、压缩、`Range`、Cookie。

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

/// 请求行 + 单行的长度上限（超了当畸形请求，别让对端用超长行吃内存）。
pub const MAX_LINE_BYTES: u64 = 8 * 1024;

/// 全部头的总长度上限。
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// 一个已解析的请求。
#[derive(Debug, Clone)]
pub struct Request {
    /// 原始方法（大写，如 `GET`）。
    pub method: String,
    /// 原始请求目标（含查询串）。
    pub target: String,
    /// 路径部分（不含查询串）。
    pub path: String,
    /// 头：名字统一小写，值原样（已去掉首尾空白）。
    pub headers: Vec<(String, String)>,
    /// 请求体。
    pub body: Vec<u8>,
}

impl Request {
    /// 取一个头（大小写不敏感，因为解析时名字已小写）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// 从头里取 `Content-Length`（没有就是 0）。
    pub fn content_length(&self) -> Option<usize> {
        self.header("content-length")
            .and_then(|value| value.trim().parse::<usize>().ok())
    }
}

/// 读请求时的失败原因（每一种都对应一个明确的 HTTP 响应）。
#[derive(Debug)]
pub enum ReadError {
    /// 底层 IO 出错（对端断开、超时）。
    Io(io::Error),
    /// 请求行/头超长。
    TooLong,
    /// 语法不对（缺版本号、头没有冒号等）。
    Malformed,
    /// 请求体超过调用方给的配额。
    BodyTooLarge,
    /// 用了本服务不支持的传输方式（`Transfer-Encoding`）。
    Unsupported,
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 从连接里读一个完整请求。
///
/// `max_body` 是请求体上限：`Content-Length` 超了立刻 `BodyTooLarge`（**不读**那一 MiB 之后的内容）。
pub fn read_request(stream: &mut TcpStream, max_body: usize) -> Result<Request, ReadError> {
    let mut reader = BufReader::new(stream);
    let request_line = read_line(&mut reader)?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ReadError::Malformed)?
        .to_ascii_uppercase();
    let target = parts.next().ok_or(ReadError::Malformed)?.to_owned();
    let version = parts.next().ok_or(ReadError::Malformed)?;
    if !version.starts_with("HTTP/1.") {
        return Err(ReadError::Malformed);
    }
    // 只认 `GET /x HTTP/1.1` 这种三段式，多余的段是畸形。
    if parts.next().is_some() {
        return Err(ReadError::Malformed);
    }

    let mut headers = Vec::new();
    let mut header_bytes = 0usize;
    loop {
        let line = read_line(&mut reader)?;
        if line.is_empty() {
            break;
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(ReadError::TooLong);
        }
        let (name, value) = line.split_once(':').ok_or(ReadError::Malformed)?;
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(ReadError::Malformed);
        }
        headers.push((name, value.trim().to_owned()));
    }

    let mut request = Request {
        method,
        path: target.split('?').next().unwrap_or("/").to_owned(),
        target,
        headers,
        body: Vec::new(),
    };

    if request.header("transfer-encoding").is_some() {
        return Err(ReadError::Unsupported);
    }
    let length = request.content_length().unwrap_or(0);
    if length > max_body {
        return Err(ReadError::BodyTooLarge);
    }
    if length > 0 {
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        request.body = body;
    }
    Ok(request)
}

/// 读一行（去掉行尾 `\r\n`/`\n`），带长度上限。
fn read_line(reader: &mut BufReader<&mut TcpStream>) -> Result<String, ReadError> {
    let mut buffer = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE_BYTES + 1)
        .read_until(b'\n', &mut buffer)?;
    if read == 0 {
        return Err(ReadError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "对端在请求行/头之前就关闭了连接",
        )));
    }
    if buffer.len() as u64 > MAX_LINE_BYTES {
        return Err(ReadError::TooLong);
    }
    while matches!(buffer.last(), Some(b'\n' | b'\r')) {
        buffer.pop();
    }
    String::from_utf8(buffer).map_err(|_| ReadError::Malformed)
}

/// 一个响应。安全头在这里统一加，避免某条路径漏掉。
#[derive(Debug, Clone)]
pub struct Response {
    /// 状态码。
    pub status: u16,
    /// `Content-Type`（`204` 时为空串）。
    pub content_type: &'static str,
    /// 响应体。
    pub body: Vec<u8>,
}

impl Response {
    /// 用给定的状态与类型造响应。
    pub fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            body: body.into(),
        }
    }

    /// `text/html; charset=utf-8`。
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    /// `application/json`。
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, "application/json", body)
    }

    /// JSON-RPC 错误（HTTP 200 + 标准错误信封）。
    pub fn rpc_error(code: i64, message: &str, ah_code: Option<&str>) -> Self {
        let mut error = serde_json::Map::new();
        error.insert("code".to_owned(), serde_json::Value::from(code));
        error.insert(
            "message".to_owned(),
            serde_json::Value::from(message.to_owned()),
        );
        if let Some(ah) = ah_code {
            error.insert("data".to_owned(), serde_json::json!({ "ah_code": ah }));
        }
        let mut envelope = serde_json::Map::new();
        envelope.insert("jsonrpc".to_owned(), serde_json::Value::from("2.0"));
        envelope.insert("error".to_owned(), serde_json::Value::Object(error));
        envelope.insert("id".to_owned(), serde_json::Value::Null);
        Self::json(serde_json::Value::Object(envelope).to_string())
    }

    /// `404 Not Found`（纯文本，人看得懂就行）。
    pub fn not_found() -> Self {
        Self::new(
            404,
            "text/plain; charset=utf-8",
            "没有这个页面。回首页：/\n",
        )
    }

    /// `204 No Content`（`/favicon.ico`：省掉浏览器控制台的 404 噪音）。
    pub fn no_content() -> Self {
        Self::new(204, "", Vec::new())
    }

    /// 状态码对应的原因短语。
    pub const fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Unknown",
        }
    }

    /// 写响应。`head_only` 为真时只发头（`HEAD` 请求）。
    ///
    /// 安全头一律**由这里统一追加**（CSP / nosniff / no-referrer），
    /// 并且**永远不发** `Access-Control-Allow-Origin`：同源反代不需要 CORS，
    /// 发了反而等于允许第三方站点直接读本页数据。
    pub fn write_to(&self, stream: &mut TcpStream, head_only: bool) -> io::Result<()> {
        let mut head = String::with_capacity(320);
        head.push_str(&format!("HTTP/1.1 {} {}\r\n", self.status, self.reason()));
        if !self.content_type.is_empty() {
            head.push_str(&format!("Content-Type: {}\r\n", self.content_type));
        }
        head.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        head.push_str("Content-Security-Policy: default-src 'self'\r\n");
        head.push_str("X-Content-Type-Options: nosniff\r\n");
        head.push_str("Referrer-Policy: no-referrer\r\n");
        head.push_str("Cache-Control: no-store\r\n");
        head.push_str("Connection: close\r\n");
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        if !head_only {
            stream.write_all(&self.body)?;
        }
        stream.flush()
    }
}

/// 一次客户端请求的原始结果（selftest 与单测共用，避免两份「读响应」的代码）。
#[derive(Debug, Clone)]
pub struct RawResponse {
    /// 状态码。
    pub status: u16,
    /// 头（名字已小写）。
    pub headers: Vec<(String, String)>,
    /// 响应体。
    pub body: Vec<u8>,
}

impl RawResponse {
    /// 取一个头（大小写不敏感）。
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// 响应体当 UTF-8 文本（读不出来就空串）。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 打一次 HTTP/1.1 请求并读完整响应（`Connection: close` 语义）。
///
/// 只给 selftest 与单测用：生产路径里浏览器是客户端，服务端没有「对外发请求」的需求
/// （反代打上游用的是 [`crate::rpc::Proxy::post_json`]）。
pub fn request_once(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    body: &[u8],
) -> io::Result<RawResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(5))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "没有状态码"))?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let mut body = Vec::new();
    match length {
        Some(length) => {
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            reader.read_to_end(&mut body)?;
        }
    }
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    /// 起一个只回一次响应的假客户端：把 `raw` 写进去，再读回服务端响应的字节。
    fn roundtrip(raw: &[u8], max_body: usize) -> Result<Request, ReadError> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
        let addr = listener.local_addr().expect("地址");
        let payload = raw.to_vec();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("连接");
            stream.write_all(&payload).expect("写请求");
            stream.shutdown(std::net::Shutdown::Write).ok();
        });
        let (mut stream, _) = listener.accept().expect("接受");
        let result = read_request(&mut stream, max_body);
        client.join().expect("客户端线程");
        result
    }

    #[test]
    fn parses_a_minimal_get() {
        let request = roundtrip(
            b"GET /app.js HTTP/1.1\r\nHost: 127.0.0.1:6875\r\n\r\n",
            1024,
        )
        .expect("应解析成功");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/app.js");
        assert_eq!(request.target, "/app.js");
        assert_eq!(request.header("host"), Some("127.0.0.1:6875"));
        assert!(request.body.is_empty());
    }

    #[test]
    fn keeps_query_string_in_target_but_not_in_path() {
        let request = roundtrip(b"GET /x?a=1&b=2 HTTP/1.1\r\nHost: h\r\n\r\n", 1024).expect("解析");
        assert_eq!(request.target, "/x?a=1&b=2");
        assert_eq!(request.path, "/x");
    }

    #[test]
    fn parses_post_body_and_lowercases_header_names() {
        let request = roundtrip(
            b"POST /jsonrpc HTTP/1.1\r\nHOST: 127.0.0.1:6875\r\nContent-Length: 5\r\n\r\nhello",
            1024,
        )
        .expect("解析");
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, b"hello");
        assert_eq!(request.content_length(), Some(5));
        assert_eq!(
            request.header("Host"),
            Some("127.0.0.1:6875"),
            "头名大小写不敏感"
        );
    }

    #[test]
    fn rejects_body_over_the_limit_without_reading_it() {
        let error = roundtrip(
            b"POST /jsonrpc HTTP/1.1\r\nContent-Length: 9999999\r\n\r\n",
            1024,
        )
        .expect_err("必须拒绝");
        assert!(matches!(error, ReadError::BodyTooLarge), "{error:?}");
    }

    #[test]
    fn rejects_chunked_encoding() {
        let error = roundtrip(
            b"POST /jsonrpc HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            1024,
        )
        .expect_err("必须拒绝");
        assert!(matches!(error, ReadError::Unsupported), "{error:?}");
    }

    #[test]
    fn rejects_malformed_request_lines() {
        for raw in [
            &b"GET /x\r\n\r\n"[..],
            &b"GET /x HTTP/1.1 extra\r\n\r\n"[..],
            &b"GET /x SPDY/3\r\n\r\n"[..],
        ] {
            let error = roundtrip(raw, 1024).expect_err("必须拒绝");
            assert!(matches!(error, ReadError::Malformed), "{error:?}");
        }
    }

    #[test]
    fn rejects_oversized_header_line() {
        let mut raw = b"GET /x HTTP/1.1\r\nX-Big: ".to_vec();
        raw.extend(std::iter::repeat_n(b'a', MAX_LINE_BYTES as usize + 10));
        raw.extend_from_slice(b"\r\n\r\n");
        let error = roundtrip(&raw, 1024).expect_err("必须拒绝");
        assert!(matches!(error, ReadError::TooLong), "{error:?}");
    }

    #[test]
    fn response_carries_security_headers_and_no_cors() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
        let addr = listener.local_addr().expect("地址");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接受");
            Response::html("<html></html>")
                .write_to(&mut stream, false)
                .expect("写响应");
        });
        let mut stream = TcpStream::connect(addr).expect("连接");
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("读响应");
        server.join().expect("服务端线程");

        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("Content-Type: text/html; charset=utf-8\r\n"),
            "{text}"
        );
        assert!(
            text.contains("Content-Security-Policy: default-src 'self'\r\n"),
            "{text}"
        );
        assert!(
            text.contains("X-Content-Type-Options: nosniff\r\n"),
            "{text}"
        );
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(
            !text
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "同源反代不需要也不允许 CORS 头：{text}"
        );
        assert!(text.ends_with("<html></html>"), "{text}");
    }

    #[test]
    fn head_only_writes_headers_without_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定");
        let addr = listener.local_addr().expect("地址");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接受");
            Response::html("<html>body</html>")
                .write_to(&mut stream, true)
                .expect("写响应");
        });
        let mut stream = TcpStream::connect(addr).expect("连接");
        let mut text = String::new();
        stream.read_to_string(&mut text).expect("读响应");
        server.join().expect("服务端线程");
        assert!(text.contains("Content-Length: 17\r\n"), "{text}");
        assert!(
            !text.contains("<html>body</html>"),
            "HEAD 不许带响应体：{text}"
        );
    }

    #[test]
    fn rpc_error_envelope_is_standard_jsonrpc() {
        let response = Response::rpc_error(-32001, "不许关", Some("AH-E3003"));
        assert_eq!(response.status, 200);
        let value: serde_json::Value = serde_json::from_slice(&response.body).expect("应是 JSON");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["error"]["code"], -32001);
        assert_eq!(value["error"]["message"], "不许关");
        assert_eq!(value["error"]["data"]["ah_code"], "AH-E3003");
        assert!(value["id"].is_null());
    }

    #[test]
    fn reason_phrases_cover_used_statuses() {
        for (status, reason) in [
            (200, "OK"),
            (204, "No Content"),
            (400, "Bad Request"),
            (403, "Forbidden"),
            (404, "Not Found"),
            (405, "Method Not Allowed"),
            (413, "Payload Too Large"),
            (503, "Service Unavailable"),
        ] {
            assert_eq!(
                Response::new(status, "text/plain", Vec::new()).reason(),
                reason
            );
        }
    }
}
