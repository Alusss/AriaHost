//! 最小 HTTP/1.1 客户端（只为 aria2 JSON-RPC 服务）。
//!
//! ## 为什么手写而不是引 reqwest/ureq/hyper
//!
//! aria2 的 RPC 端点固定为 `POST /jsonrpc`、本机明文（`127.0.0.1:6800`，无 TLS、无重定向、
//! 不走系统代理），需要的只是「连上去 → 写一段 JSON → 读一段 JSON」。引一个 HTTP 栈会给
//! 静态 CRT 的单 exe 增加 TLS/DNS/连接池全套重量，还要多维护一套与 aria2 无关的错误语义。
//!
//! ## 支持的子集（刻意不做的部分）
//!
//! - 请求固定 `HTTP/1.1` + `Connection: close`，不做 keep-alive：每次调用一条连接，
//!   本机 loopback 的握手成本远小于维持连接池的复杂度；
//! - 响应体收尾只认两种：`Content-Length` 定长，或「无 `Content-Length` 时读到 EOF」；
//! - `Transfer-Encoding: chunked` **显式报错**而不是猜：aria2 的 RPC 服务器对 JSON-RPC
//!   一律带 `Content-Length`，真遇到 chunked 说明对端不是预期的服务器，报错比猜错安全；
//! - 超时是**每次 socket 操作**的超时（connect / 单次 read），不是整次调用的总时长上限。
//!   对端持续低速滴字节时总耗时可以超过 timeout；需要总时长上限的调用方自己计时
//!   （M2-1 的探测循环）。
//!
//! 本模块是 `client` 的实现细节，不对外暴露。

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// 响应体上限（防对端灌内存）。aria2 的 `tell*` 响应到 `--max-download-result` 量级
/// （默认 1000 条）也只有数 MB，64 MiB 是「明显异常」的判据而不是容量预算。
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// 单次 `read` 的缓冲区大小。
const READ_CHUNK: usize = 8 * 1024;

/// 响应头结束标记（HTTP/1.1 规定 CRLF）。
const HEADER_END: &[u8] = b"\r\n\r\n";

/// 传输层错误：区分「连不上」「连上后读写失败」「非 2xx」「响应结构不合法」。
///
/// 这四类都由 `client` 映射到 [`crate::RpcCallError`]；这里保留原始 `io::Error`（含
/// `ErrorKind`），因为 RPC 层需要判断「是不是超时」。
#[derive(Debug)]
pub enum HttpError {
    /// 建立 TCP 连接失败（拒绝/超时/不可达）。
    Connect(io::Error),
    /// 连接已建立，但写请求或读响应失败（含读超时、对端提前关闭）。
    Transport(io::Error),
    /// 非 2xx 状态行。
    Status(u16),
    /// 状态行/头部/长度/编码不合法。
    Malformed(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(error) => {
                write!(formatter, "建立连接失败（{:?}）：{error}", error.kind())
            }
            Self::Transport(error) => write!(formatter, "传输失败（{:?}）：{error}", error.kind()),
            Self::Status(status) => write!(formatter, "HTTP 状态码 {status}"),
            Self::Malformed(detail) => write!(formatter, "响应不合法：{detail}"),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) | Self::Transport(error) => Some(error),
            Self::Status(_) | Self::Malformed(_) => None,
        }
    }
}

/// 解析后的响应。
pub struct HttpResponse {
    /// 状态码（只可能是 2xx：非 2xx 已在读取阶段变成 [`HttpError::Status`]）。
    pub status: u16,
    /// 响应体原始字节。
    pub body: Vec<u8>,
}

/// 响应头解析结果。
struct Head {
    /// 状态码。
    status: u16,
    /// `Content-Length`；缺省表示「读到 EOF」。
    content_length: Option<usize>,
    /// 响应体在缓冲区中的起点（头结束标记之后）。
    body_start: usize,
}

/// `POST {path}`，把 `body` 原样写出，读回一个 2xx 响应。
///
/// `timeout` 同时用于 connect、写、单次读。
pub fn post_json(
    addr: SocketAddr,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<HttpResponse, HttpError> {
    if timeout.is_zero() {
        return Err(HttpError::Malformed("超时必须大于 0".to_owned()));
    }

    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(HttpError::Connect)?;
    // 请求只有几百字节，不需要 Nagle 合并；失败不影响正确性，按连接错误上报。
    stream.set_nodelay(true).map_err(HttpError::Connect)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(HttpError::Connect)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(HttpError::Connect)?;

    let request = build_request(addr, path, body);
    stream.write_all(&request).map_err(HttpError::Transport)?;
    stream.flush().map_err(HttpError::Transport)?;

    read_response(&mut stream)
}

/// 组请求字节：头 + 空行 + body。
fn build_request(addr: SocketAddr, path: &str, body: &[u8]) -> Vec<u8> {
    let host = match addr {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: AriaHost/{version}\r\n\
         Accept: application/json\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {length}\r\n\
         Connection: close\r\n\
         \r\n",
        version = env!("CARGO_PKG_VERSION"),
        length = body.len(),
    );
    let mut request = Vec::with_capacity(head.len() + body.len());
    request.extend_from_slice(head.as_bytes());
    request.extend_from_slice(body);
    request
}

/// 读到「响应头已知长度且已收全」或 EOF 为止，然后切出响应体。
fn read_response(stream: &mut TcpStream) -> Result<HttpResponse, HttpError> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut head: Option<Head> = None;
    let mut chunk = [0u8; READ_CHUNK];

    loop {
        // 已知长度时先判断是否已收全，避免为一个已经完整的响应多做一次阻塞 read。
        if let Some(parsed) = &head {
            if let Some(length) = parsed.content_length {
                let total = parsed.body_start.checked_add(length).ok_or_else(|| {
                    HttpError::Malformed(format!("Content-Length 溢出：{length}"))
                })?;
                if buffer.len() >= total {
                    break;
                }
            }
        }

        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            // EINTR 不是错误：重试同一次 read。
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(HttpError::Transport(error)),
        }

        if buffer.len() > MAX_RESPONSE_BYTES {
            return Err(HttpError::Malformed(format!(
                "响应体超过上限 {MAX_RESPONSE_BYTES} 字节"
            )));
        }

        if head.is_none() {
            if let Some(head_end) = find_header_end(&buffer) {
                let parsed = parse_head(&buffer, head_end)?;
                if !(200..300).contains(&parsed.status) {
                    // 非 2xx 不必等 body：状态码本身就是结论。
                    return Err(HttpError::Status(parsed.status));
                }
                head = Some(parsed);
            }
        }
    }

    let head = head.ok_or_else(|| {
        HttpError::Malformed(
            "对端在响应头结束前关闭连接（收到的字节不足一个完整响应头）".to_owned(),
        )
    })?;

    let body = match head.content_length {
        Some(length) => {
            let end = head
                .body_start
                .checked_add(length)
                .ok_or_else(|| HttpError::Malformed(format!("Content-Length 溢出：{length}")))?;
            buffer
                .get(head.body_start..end)
                .ok_or_else(|| {
                    HttpError::Malformed(format!(
                        "响应体不足：声明 {length} 字节，实收 {} 字节",
                        buffer.len().saturating_sub(head.body_start)
                    ))
                })?
                .to_vec()
        }
        None => buffer
            .get(head.body_start..)
            .map_or_else(Vec::new, <[u8]>::to_vec),
    };

    Ok(HttpResponse {
        status: head.status,
        body,
    })
}

/// 找到响应头结束标记的起点；没有则返回 `None`（继续读）。
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(HEADER_END.len())
        .position(|window| window == HEADER_END)
}

/// 解析状态行与关心的头部。
fn parse_head(buffer: &[u8], head_end: usize) -> Result<Head, HttpError> {
    let text = std::str::from_utf8(&buffer[..head_end])
        .map_err(|_| HttpError::Malformed("响应头不是合法 UTF-8/ASCII".to_owned()))?;
    let mut lines = text.split("\r\n");

    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split_whitespace();
    let protocol = parts.next().unwrap_or_default();
    if !protocol.starts_with("HTTP/") {
        return Err(HttpError::Malformed(format!(
            "状态行不以 HTTP/ 开头：{status_line}"
        )));
    }
    let status_raw = parts
        .next()
        .ok_or_else(|| HttpError::Malformed(format!("状态行缺少状态码：{status_line}")))?;
    let status: u16 = status_raw
        .parse()
        .map_err(|_| HttpError::Malformed(format!("状态码不是数字：{status_line}")))?;

    let mut content_length: Option<usize> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Malformed(format!("响应头缺少冒号：{line}")));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                let length: usize = value.parse().map_err(|_| {
                    HttpError::Malformed(format!("Content-Length 不是数字：{value}"))
                })?;
                content_length = Some(length);
            }
            "transfer-encoding" => {
                if value.to_ascii_lowercase().contains("chunked") {
                    return Err(HttpError::Malformed(
                        "对端使用 chunked 编码；本客户端只支持 Content-Length 或读到 EOF"
                            .to_owned(),
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(Head {
        status,
        content_length,
        body_start: head_end + HEADER_END.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_line_and_content_length() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
        let head_end = find_header_end(raw).expect("应找到头结束标记");
        let head = parse_head(raw, head_end).expect("头部应可解析");
        assert_eq!(head.status, 200);
        assert_eq!(head.content_length, Some(2));
        assert_eq!(&raw[head.body_start..], b"{}");
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let raw = b"HTTP/1.1 200 OK\r\ncOnTeNt-LeNgTh: 7\r\n\r\n";
        let head =
            parse_head(raw, find_header_end(raw).expect("应找到头结束标记")).expect("头部应可解析");
        assert_eq!(head.content_length, Some(7));
    }

    #[test]
    fn rejects_chunked_encoding() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let error = parse_head(raw, find_header_end(raw).expect("应找到头结束标记"))
            .expect_err("chunked 必须报错");
        assert!(matches!(error, HttpError::Malformed(_)), "{error}");
    }

    #[test]
    fn rejects_non_http_status_line() {
        for raw in [
            &b"200 OK\r\n\r\n"[..],
            &b"HTTP/1.1 OK\r\n\r\n"[..],
            &b"\r\n"[..],
        ] {
            let head_end = find_header_end(raw).expect("应找到头结束标记");
            let error = parse_head(raw, head_end).expect_err("非法状态行必须报错");
            assert!(matches!(error, HttpError::Malformed(_)), "{error}");
        }
    }

    #[test]
    fn builds_post_request_with_content_length() {
        let body = br#"{"jsonrpc":"2.0"}"#;
        let addr: SocketAddr = "127.0.0.1:6800".parse().expect("地址应可解析");
        let request = build_request(addr, "/jsonrpc", body);
        let text = String::from_utf8_lossy(&request).into_owned();
        assert!(text.starts_with("POST /jsonrpc HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("\r\nHost: 127.0.0.1:6800\r\n"), "{text}");
        assert!(
            text.contains(&format!("\r\nContent-Length: {}\r\n", body.len())),
            "{text}"
        );
        assert!(text.contains("\r\nConnection: close\r\n"), "{text}");
        assert!(request.ends_with(body), "body 必须原样追加在头之后");
    }

    #[test]
    fn ipv6_host_is_bracketed() {
        let addr: SocketAddr = "[::1]:6800".parse().expect("地址应可解析");
        let request = build_request(addr, "/jsonrpc", b"{}");
        let text = String::from_utf8_lossy(&request).into_owned();
        assert!(text.contains("\r\nHost: [::1]:6800\r\n"), "{text}");
    }
}
