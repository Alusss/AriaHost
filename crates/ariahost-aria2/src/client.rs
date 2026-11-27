//! aria2 JSON-RPC 客户端（`127.0.0.1:6800`）。
//!
//! ## 传输
//!
//! 走 `crate::http` 的手写最小 HTTP/1.1：`POST /jsonrpc`，每次调用一条 `Connection: close`
//! 连接。**本库不做重试/退避**：`AH-E2003` 的「5s×3 + 退避 2/5/10s」是调用方的重连策略
//! （M2-1 落地），库只把单次尝试的结果原样报出来，避免策略在两处各写一份、日后漂移。
//!
//! ## token 注入
//!
//! aria2 要求 `--rpc-secret` 存在时**每次调用的 params 第一项**都是 `token:<secret>`
//! （aria2 manual「JSON-RPC Interface」示例：`aria2.addUri("token:$$secret$$", [...])`）。
//! [`RpcClient`] 在 [`RpcClient::call`] 内统一注入：语义化方法不要把 token 写进参数，
//! 任何调用路径也不会漏注入。
//!
//! ## 错误码映射（只映射到 `ariahost-common::ErrorCode` 已有码，不新增码）
//!
//! | 情况 | 变体 | `ErrorCode` | 依据 |
//! |---|---|---|---|
//! | 连接被拒 / 不可达 | [`RpcCallError::Connect`] | `AH-E2003` | 「RPC 握手失败」＝ 一次握手尝试没建立起来 |
//! | 已连接但写/读失败（含读超时） | [`RpcCallError::Transport`] | `AH-E2003` | 同上，握手阶段没拿到响应 |
//! | 状态行/头部/JSON 结构不合法 | [`RpcCallError::Protocol`] | `AH-E2003` | 对端没给出可用的 JSON-RPC 响应 |
//! | HTTP 403 / 404 | [`RpcCallError::HttpStatus`] | `AH-E3003` | 该码定义即「HTTP 403/404」 |
//! | HTTP 其它非 2xx | [`RpcCallError::HttpStatus`] | 无（保留状态码原文） | 既有码里没有对应项，硬套 E3003 会谎报「403/404」 |
//! | JSON-RPC `error` 其它码 | [`RpcCallError::Remote`] | 无（保留 code/message 原文） | 同上：`E2###` 无「aria2 语义错误」码 |
//! | JSON-RPC `-32601` | [`RpcCallError::MethodNotFound`] | 无（能力探测信号） | 见下 |
//!
//! `-32601` 不映射成 `AH-E6004`（核心版本过低）是刻意的：能力探测（「你支持
//! `aria2.saveSession` 吗」）在探测语境下不是错误，若库直接判成「版本过低」，每次探测都会
//! 变成告警。**必需**方法缺失时由调用方（M2-1）自己映射到 `AH-E6004`。

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ariahost_common::ErrorCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http::{self, HttpError};
use crate::rpc::{RpcError, RpcRequest, RpcResponse};

/// aria2 RPC 默认端口（架构 §3.3：数据面 `127.0.0.1:6800`）。
pub const DEFAULT_PORT: u16 = 6800;

/// aria2 RPC 固定路径（HTTP 通道）。
pub const RPC_PATH: &str = "/jsonrpc";

/// 默认单次 socket 操作超时（架构 §4.12 的「5s」口径）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// JSON-RPC 标准码：方法不存在（`Method not found`）。
const METHOD_NOT_FOUND_CODE: i64 = -32601;

const METHOD_GET_VERSION: &str = "aria2.getVersion";
const METHOD_GET_GLOBAL_STAT: &str = "aria2.getGlobalStat";
const METHOD_TELL_ACTIVE: &str = "aria2.tellActive";
const METHOD_TELL_WAITING: &str = "aria2.tellWaiting";
const METHOD_TELL_STOPPED: &str = "aria2.tellStopped";
const METHOD_ADD_URI: &str = "aria2.addUri";
const METHOD_PAUSE: &str = "aria2.pause";
const METHOD_UNPAUSE: &str = "aria2.unpause";
const METHOD_REMOVE: &str = "aria2.remove";
const METHOD_SAVE_SESSION: &str = "aria2.saveSession";

/// `aria2.getVersion` 的返回结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// 版本号字符串，例如 `1.37.0`（格式与 `aria2c --version` 首行的版本段一致）。
    pub version: String,
    /// 编译期启用的功能列表，例如 `["BitTorrent", "GZip"]`；旧版本可能没有该字段。
    #[serde(rename = "enabledFeatures", default)]
    pub enabled_features: Vec<String>,
}

/// `aria2.getGlobalStat` 的返回结构。
///
/// aria2 的 JSON-RPC 响应里**数值一律是字符串**（manual 示例 `"downloadSpeed":"21849"`），
/// 这里用下面的 `flex` 助手同时接受字符串与 JSON 数字，统一归一到 `u64`：字符串是既定事实，
/// 数字是对版本差异的容忍，两种都不该让面板解析失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalStat {
    /// 全局下载速率（字节/秒）。
    #[serde(rename = "downloadSpeed", deserialize_with = "flex::u64")]
    pub download_speed: u64,
    /// 全局上传速率（字节/秒）。
    #[serde(rename = "uploadSpeed", deserialize_with = "flex::u64")]
    pub upload_speed: u64,
    /// 正在下载的任务数。
    #[serde(rename = "numActive", deserialize_with = "flex::u64")]
    pub num_active: u64,
    /// 排队中的任务数。
    #[serde(rename = "numWaiting", deserialize_with = "flex::u64")]
    pub num_waiting: u64,
    /// 已停止的任务数（受 `--max-download-result` 上限约束，默认 1000）。
    #[serde(rename = "numStopped", deserialize_with = "flex::u64")]
    pub num_stopped: u64,
    /// 历史上停止过的任务总数（不受上限约束；旧版本没有该字段）。
    #[serde(
        rename = "numStoppedTotal",
        default,
        deserialize_with = "flex::opt_u64"
    )]
    pub num_stopped_total: Option<u64>,
}

// 「字符串或数字 → `u64`」的 serde 助手（外层用普通注释：同一 item 不能既有 outer 又有 inner 文档）。
mod flex {
    //! 只服务于 [`super::GlobalStat`] 的字段反序列化。

    use serde::de::{Deserializer, Error as DeError};
    use serde::Deserialize;
    use serde_json::Value;

    /// 把 JSON 值转成 `u64`；字符串按十进制解析。
    pub(super) fn u64_value<E: DeError>(value: &Value) -> Result<u64, E> {
        match value {
            Value::String(raw) => raw
                .parse::<u64>()
                .map_err(|_| E::custom(format!("期望十进制非负整数，实际 {raw:?}"))),
            Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| E::custom(format!("期望非负整数，实际 {number}"))),
            other => Err(E::custom(format!("期望字符串或数字，实际 {other}"))),
        }
    }

    /// 必填字段：字符串或数字。
    pub(super) fn u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let value = Value::deserialize(deserializer)?;
        u64_value(&value)
    }

    /// 可选字段：缺失为 `None`，存在则同 [`u64`]。
    pub(super) fn opt_u64<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        let value = Option::<Value>::deserialize(deserializer)?;
        value.as_ref().map(u64_value::<D::Error>).transpose()
    }
}

/// 一次 RPC 调用的失败。
///
/// 每个变体都保留足够原文用于诊断；[`RpcCallError::code`] 只返回**语义上真的对得上**的
/// 既有 `ErrorCode`（见模块文档的映射表），没有对应码时返回 `None` 而不是硬套。
///
/// 只派生 `PartialEq`（不派生 `Eq`）：内嵌的 [`RpcError`] 只实现 `PartialEq`，
/// 而在错误类型上强推全序相等没有实际用途。
#[derive(Debug, Clone, PartialEq)]
pub enum RpcCallError {
    /// 连接阶段失败（拒绝 / 超时 / 不可达）→ `AH-E2003`。
    Connect {
        /// 原始 IO 错误文本（含 `ErrorKind`）。
        detail: String,
        /// 是否属于超时（`TimedOut` / `WouldBlock`）。
        timed_out: bool,
    },
    /// 连接已建立，但写请求或读响应失败（含读超时）→ `AH-E2003`。
    Transport {
        /// 原始 IO 错误文本（含 `ErrorKind`）。
        detail: String,
        /// 是否属于超时。
        timed_out: bool,
    },
    /// 响应结构不合法（状态行/头部/JSON/字段形状）→ `AH-E2003`。
    Protocol {
        /// 具体哪里不合法。
        detail: String,
    },
    /// HTTP 非 2xx：403/404 → `AH-E3003`，其它码保留原文且无对应错误码。
    HttpStatus {
        /// 实际状态码。
        status: u16,
    },
    /// 远端返回 JSON-RPC `error`（保留 aria2 的 `code`/`message` 原文）。
    Remote {
        /// 原始错误对象。
        error: RpcError,
    },
    /// 远端报告方法不存在（`-32601`），供运行时能力探测。
    MethodNotFound {
        /// 被调用的方法名。
        method: String,
        /// 原始错误对象（`code` 固定为 `-32601`）。
        error: RpcError,
    },
}

impl RpcCallError {
    /// 语义上能对上的既有错误码；无对应码时返回 `None`（此时必须用 [`Display`] 的原文）。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Connect { .. } | Self::Transport { .. } | Self::Protocol { .. } => {
                Some(ErrorCode::RpcHandshakeFailed)
            }
            Self::HttpStatus { status: 403 | 404 } => Some(ErrorCode::HttpRejected),
            Self::HttpStatus { .. } | Self::Remote { .. } | Self::MethodNotFound { .. } => None,
        }
    }

    /// 是否由超时导致（连接超时或读超时）。
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Connect { timed_out, .. } | Self::Transport { timed_out, .. } => *timed_out,
            Self::Protocol { .. }
            | Self::HttpStatus { .. }
            | Self::Remote { .. }
            | Self::MethodNotFound { .. } => false,
        }
    }

    /// 远端错误对象（`Remote` / `MethodNotFound` 之外为 `None`）。
    pub fn remote_error(&self) -> Option<&RpcError> {
        match self {
            Self::Remote { error } | Self::MethodNotFound { error, .. } => Some(error),
            Self::Connect { .. }
            | Self::Transport { .. }
            | Self::Protocol { .. }
            | Self::HttpStatus { .. } => None,
        }
    }
}

impl fmt::Display for RpcCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { detail, timed_out } => write!(
                formatter,
                "{} 连接 aria2 RPC 失败{}：{detail}",
                ErrorCode::RpcHandshakeFailed.code(),
                if *timed_out { "（超时）" } else { "" }
            ),
            Self::Transport { detail, timed_out } => write!(
                formatter,
                "{} aria2 RPC 传输失败{}：{detail}",
                ErrorCode::RpcHandshakeFailed.code(),
                if *timed_out { "（超时）" } else { "" }
            ),
            Self::Protocol { detail } => write!(
                formatter,
                "{} aria2 RPC 响应不合法：{detail}",
                ErrorCode::RpcHandshakeFailed.code()
            ),
            Self::HttpStatus { status } if matches!(status, 403 | 404) => write!(
                formatter,
                "{} aria2 RPC 返回 HTTP {status}",
                ErrorCode::HttpRejected.code()
            ),
            Self::HttpStatus { status } => {
                write!(formatter, "aria2 RPC 返回 HTTP {status}")
            }
            Self::Remote { error } => {
                write!(
                    formatter,
                    "aria2 RPC 错误 {}：{}",
                    error.code, error.message
                )
            }
            Self::MethodNotFound { method, error } => write!(
                formatter,
                "aria2 RPC 不支持方法 {method}（{}：{}）",
                error.code, error.message
            ),
        }
    }
}

impl std::error::Error for RpcCallError {}

/// aria2 JSON-RPC 客户端。
///
/// 不可变配置：地址 / secret / 超时；调用之间唯一的可变状态是请求 id 计数器
/// （`&self` 可并发调用，`Send + Sync`）。
pub struct RpcClient {
    addr: SocketAddr,
    secret: Option<String>,
    timeout: Duration,
    next_id: AtomicU64,
}

impl RpcClient {
    /// 用默认超时（[`DEFAULT_TIMEOUT`]）构造。
    pub fn new(addr: SocketAddr, secret: Option<String>) -> Self {
        Self::with_timeout(addr, secret, DEFAULT_TIMEOUT)
    }

    /// 指定超时构造（超时同时用于 connect、写、单次读）。
    pub fn with_timeout(addr: SocketAddr, secret: Option<String>, timeout: Duration) -> Self {
        Self {
            addr,
            secret,
            timeout,
            next_id: AtomicU64::new(0),
        }
    }

    /// 指向本机 `127.0.0.1:{port}`（默认超时）。
    pub fn local(port: u16, secret: Option<String>) -> Self {
        Self::new(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), secret)
    }

    /// 指向本机 `127.0.0.1:{port}`，指定超时（测试注入短超时走这个入口）。
    pub fn local_with_timeout(port: u16, secret: Option<String>, timeout: Duration) -> Self {
        Self::with_timeout(
            SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            secret,
            timeout,
        )
    }

    /// 目标地址。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 单次 socket 操作超时。
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// 是否配置了 secret（不回显 secret 本身）。
    pub fn has_secret(&self) -> bool {
        self.secret.is_some()
    }

    /// 发一次 JSON-RPC 调用，返回 `result`。
    ///
    /// 参数顺序由调用方给定；secret 由本方法自动插到**第一项**（见模块文档）。
    pub fn call(&self, method: &str, params: Vec<Value>) -> Result<Value, RpcCallError> {
        let id = self.next_request_id();
        let request = RpcRequest::new(id, method, self.prepare_params(params));
        let request_body =
            serde_json::to_vec(&request).map_err(|error| RpcCallError::Protocol {
                detail: format!("请求序列化失败：{error}"),
            })?;

        let response_body = http::post_json(self.addr, RPC_PATH, &request_body, self.timeout)
            .map_err(|error| self.map_http_error(error))?;

        let parsed: RpcResponse =
            serde_json::from_slice(&response_body).map_err(|error| RpcCallError::Protocol {
                detail: format!(
                    "响应体不是合法 JSON-RPC（{} 字节）：{error}",
                    response_body.len()
                ),
            })?;

        if let Some(error) = parsed.error {
            // -32601 单独成型：能力探测要靠它区分「版本旧」与「真错误」。
            return Err(if error.code == METHOD_NOT_FOUND_CODE {
                RpcCallError::MethodNotFound {
                    method: method.to_owned(),
                    error,
                }
            } else {
                RpcCallError::Remote { error }
            });
        }

        parsed.result.ok_or_else(|| RpcCallError::Protocol {
            detail: "响应既没有 result 也没有 error".to_owned(),
        })
    }

    /// `aria2.getVersion`：核心版本与启用的功能（心跳/就绪探测的首选）。
    pub fn get_version(&self) -> Result<VersionInfo, RpcCallError> {
        self.call_typed(METHOD_GET_VERSION, Vec::new())
    }

    /// `aria2.getGlobalStat`：全局速率与队列长度。
    pub fn get_global_stat(&self) -> Result<GlobalStat, RpcCallError> {
        self.call_typed(METHOD_GET_GLOBAL_STAT, Vec::new())
    }

    /// `aria2.tellActive`：正在下载的任务，元素是 aria2 原始 status 对象。
    ///
    /// 刻意不把 20+ 个 status 字段定成结构体：面板真正需要哪些字段属于 M2-1 的产品决定，
    /// 现在定死只会制造返工。需要收窄时用 aria2 的 `keys` 参数，或在面板侧建视图结构。
    pub fn tell_active(&self) -> Result<Vec<Value>, RpcCallError> {
        self.call_typed(METHOD_TELL_ACTIVE, Vec::new())
    }

    /// `aria2.tellWaiting`：排队中的任务。
    ///
    /// `offset` 可为负（`-1` = 队尾倒数第一，aria2 manual 的 `tellWaiting` 语义）。
    pub fn tell_waiting(&self, offset: i64, num: u32) -> Result<Vec<Value>, RpcCallError> {
        self.call_typed(METHOD_TELL_WAITING, vec![json!(offset), json!(num)])
    }

    /// `aria2.tellStopped`：已停止的任务。
    ///
    /// 结果是 FIFO 队列，长度受 `--max-download-result` 约束（默认 1000），所以**不能**当作
    /// 完整历史：面板的「已停止」列表必须自建持久化，RPC 只当增量来源。
    ///
    /// 负 offset = 从最新倒数，与 `tell_waiting` 语义一致（`worker.rs` 用 `-N, N` 取最近 N 条）。
    pub fn tell_stopped(&self, offset: i64, num: u32) -> Result<Vec<Value>, RpcCallError> {
        self.call_typed(METHOD_TELL_STOPPED, vec![json!(offset), json!(num)])
    }

    /// `aria2.addUri`：按 URI 列表新建任务，返回 GID。
    pub fn add_uri(&self, uris: &[String]) -> Result<String, RpcCallError> {
        let value = self.call(METHOD_ADD_URI, vec![json!(uris)])?;
        expect_gid(value, METHOD_ADD_URI)
    }

    /// `aria2.pause`：暂停单个任务（按 GID），返回 GID。
    pub fn pause(&self, gid: &str) -> Result<String, RpcCallError> {
        let value = self.call(METHOD_PAUSE, vec![json!(gid)])?;
        expect_gid(value, METHOD_PAUSE)
    }

    /// `aria2.unpause`：继续单个任务（按 GID），返回 GID。
    pub fn unpause(&self, gid: &str) -> Result<String, RpcCallError> {
        let value = self.call(METHOD_UNPAUSE, vec![json!(gid)])?;
        expect_gid(value, METHOD_UNPAUSE)
    }

    /// `aria2.remove`：移除单个任务（按 GID），返回 GID。
    pub fn remove(&self, gid: &str) -> Result<String, RpcCallError> {
        let value = self.call(METHOD_REMOVE, vec![json!(gid)])?;
        expect_gid(value, METHOD_REMOVE)
    }

    /// `aria2.saveSession`：把 error/unfinished 列表落盘（停止链第一段）。
    ///
    /// aria2 成功时返回字符串 `"OK"`。老版本/精简构建可能没有这个方法，此时得到
    /// [`RpcCallError::MethodNotFound`]，调用方据此降级（不能当成成功）。
    pub fn save_session(&self) -> Result<(), RpcCallError> {
        let value = self.call(METHOD_SAVE_SESSION, Vec::new())?;
        match value {
            Value::String(text) if text == "OK" => Ok(()),
            other => Err(RpcCallError::Protocol {
                detail: format!("{METHOD_SAVE_SESSION} 期望返回 \"OK\"，实际 {other}"),
            }),
        }
    }

    /// 调一次方法并把 `result` 反序列化成 `T`。
    fn call_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, RpcCallError> {
        let value = self.call(method, params)?;
        serde_json::from_value(value).map_err(|error| RpcCallError::Protocol {
            detail: format!("{method} 返回值形状不符合预期：{error}"),
        })
    }

    /// 把 secret 插到 params 第一项（`token:<secret>`）。
    fn prepare_params(&self, params: Vec<Value>) -> Vec<Value> {
        match &self.secret {
            Some(secret) => {
                let mut prepared = Vec::with_capacity(params.len() + 1);
                prepared.push(Value::String(format!("token:{secret}")));
                prepared.extend(params);
                prepared
            }
            None => params,
        }
    }

    /// 请求 id：进程内自增，形如 `ariahost-1`（aria2 只要求与响应回显一致）。
    fn next_request_id(&self) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        format!("ariahost-{sequence}")
    }

    /// 传输层错误 → RPC 错误（映射表见模块文档）。
    fn map_http_error(&self, error: HttpError) -> RpcCallError {
        match error {
            HttpError::Connect(io_error) => RpcCallError::Connect {
                detail: format!(
                    "{}：{io_error}（目标 {}）",
                    describe_kind(&io_error),
                    self.addr
                ),
                timed_out: is_timeout_kind(&io_error),
            },
            HttpError::Transport(io_error) => RpcCallError::Transport {
                detail: format!(
                    "{}：{io_error}（目标 {}，单次操作超时 {:?}）",
                    describe_kind(&io_error),
                    self.addr,
                    self.timeout
                ),
                timed_out: is_timeout_kind(&io_error),
            },
            HttpError::Status(status) => RpcCallError::HttpStatus { status },
            HttpError::Malformed(detail) => RpcCallError::Protocol { detail },
        }
    }
}

impl fmt::Debug for RpcClient {
    /// secret 一律脱敏成 `token:***`：日志与诊断包不得出现 secret 明文（架构 §4.12）。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpcClient")
            .field("addr", &self.addr)
            .field("secret", &self.secret.as_ref().map(|_| "token:***"))
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// `io::ErrorKind` 的可读名（诊断用；比直接打印 Debug 更稳定）。
fn describe_kind(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::TimedOut => "超时 TimedOut",
        std::io::ErrorKind::WouldBlock => "超时 WouldBlock",
        std::io::ErrorKind::ConnectionRefused => "连接被拒绝 ConnectionRefused",
        std::io::ErrorKind::ConnectionReset => "连接被重置 ConnectionReset",
        std::io::ErrorKind::ConnectionAborted => "连接被中止 ConnectionAborted",
        std::io::ErrorKind::NotConnected => "未连接 NotConnected",
        std::io::ErrorKind::AddrNotAvailable => "地址不可用 AddrNotAvailable",
        std::io::ErrorKind::PermissionDenied => "权限不足 PermissionDenied",
        _ => "IO 错误",
    }
}

/// 是否属于超时（Windows 的读超时是 `TimedOut`，部分平台报 `WouldBlock`）。
fn is_timeout_kind(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// 期望 `result` 是 GID 字符串（`addUri`/`pause`/`unpause`/`remove` 的共同返回形状）。
fn expect_gid(value: Value, method: &str) -> Result<String, RpcCallError> {
    match value {
        Value::String(gid) => Ok(gid),
        other => Err(RpcCallError::Protocol {
            detail: format!("{method} 期望返回 GID 字符串，实际 {other}"),
        }),
    }
}
