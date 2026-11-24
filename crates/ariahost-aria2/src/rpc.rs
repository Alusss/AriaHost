//! aria2 JSON-RPC 2.0 的最小数据结构。
//!
//! 本模块只固定**传输层形状**（请求/响应/错误对象），不发请求、不定义方法清单。
//! 发请求、token 注入与语义化方法在 [`crate::client`]（M1-2A）；重连/退避策略仍待 M2-1。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 协议版本固定值。
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC 请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcRequest {
    /// 协议版本，固定 `"2.0"`。
    pub jsonrpc: String,
    /// 请求 id（aria2 用字符串形式）。
    pub id: String,
    /// 方法名，例如 `aria2.addUri`。
    pub method: String,
    /// 位置参数数组。
    pub params: Vec<Value>,
}

impl RpcRequest {
    /// 构造一个标准 JSON-RPC 2.0 请求。
    pub fn new(id: impl Into<String>, method: impl Into<String>, params: Vec<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 错误对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    /// aria2 错误码。
    pub code: i64,
    /// 错误描述（只进日志/诊断包，不直接弹给用户）。
    pub message: String,
}

/// JSON-RPC 响应：`result` 与 `error` 二选一。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcResponse {
    /// 协议版本；部分客户端省略时按 `"2.0"` 处理。
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,
    /// 响应 id（通知类响应可能缺失）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 成功结果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 错误对象。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

fn default_jsonrpc() -> String {
    JSONRPC_VERSION.to_owned()
}

impl RpcResponse {
    /// 是否带错误对象。
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_as_jsonrpc_2() {
        let request = RpcRequest::new(
            "req-1",
            "aria2.addUri",
            vec![json!(["https://example.com/a"])],
        );
        let value = serde_json::to_value(&request).expect("请求应可序列化");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["method"], "aria2.addUri");
        assert_eq!(value["params"][0][0], "https://example.com/a");
    }

    #[test]
    fn parses_success_response() {
        let raw = r#"{"id":"req-1","jsonrpc":"2.0","result":"2089b05ecca3d829"}"#;
        let response: RpcResponse = serde_json::from_str(raw).expect("成功响应应可解析");
        assert!(!response.is_error());
        assert_eq!(response.result, Some(json!("2089b05ecca3d829")));
        assert_eq!(response.id.as_deref(), Some("req-1"));
    }

    #[test]
    fn parses_error_response() {
        let raw = r#"{"id":"req-1","jsonrpc":"2.0","error":{"code":1,"message":"Unauthorized"}}"#;
        let response: RpcResponse = serde_json::from_str(raw).expect("错误响应应可解析");
        assert!(response.is_error());
        assert_eq!(
            response.error,
            Some(RpcError {
                code: 1,
                message: "Unauthorized".to_owned(),
            })
        );
    }

    #[test]
    fn response_defaults_jsonrpc_when_omitted() {
        let response: RpcResponse =
            serde_json::from_str(r#"{"result":"ok"}"#).expect("缺 jsonrpc 字段应可解析");
        assert_eq!(response.jsonrpc, "2.0");
    }
}
