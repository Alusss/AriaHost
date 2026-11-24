//! 网页管理页骨架（架构 §3.2/§3.3/§4.4）。
//!
//! 本批只有：`GET /healthz` 与 `serve(port)`。
//! **不做**静态资产内嵌、同源反代、token 注入与 shutdown 拦截（M2 落地）。

use axum::{routing::get, Json, Router};
use serde_json::{json, Value};

/// 构造路由表。M2 起在这里挂静态资源与同源反代。
pub fn router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

/// `GET /healthz` 的响应体（纯函数，便于单测）。
fn health_body() -> Value {
    json!({
        "status": "ok",
        "version": ariahost_common::VERSION,
    })
}

/// `GET /healthz`：进程存活探针，安装器与 CI 用。
async fn healthz() -> Json<Value> {
    Json(health_body())
}

/// 在 `127.0.0.1:<port>` 上提供网页管理页（架构 §3.3：仅本机监听）。
///
/// 本批是骨架：端口绑定失败原样返回 `io::Error`，由调用方映射错误码。
pub async fn serve(port: u16) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    axum::serve(listener, router()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_body_reports_ok_and_version() {
        let body = health_body();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], ariahost_common::VERSION);
    }
}
