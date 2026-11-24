//! aria2 集成（架构 §3.2）。
//!
//! 模块：
//! - [`version`]：`aria2c --version` 首行解析；
//! - [`rpc`]：JSON-RPC 2.0 的请求/响应/错误对象（传输层形状）；
//! - [`client`]：零依赖手写 HTTP/1.1 的 [`RpcClient`]（M1-2A），token 自动注入 + 错误码映射。
//!
//! `http` 是 `client` 的实现细节，不对外暴露。
//!
//! 边界：本 crate **不拉起 aria2c 进程、不读配置、不做重连策略**；RPC 只连本机明文端点
//! （`127.0.0.1:6800`）。进程托管与重连属于 M2-1。

pub mod client;
mod http;
pub mod rpc;
pub mod version;

pub use client::{
    GlobalStat, RpcCallError, RpcClient, VersionInfo, DEFAULT_PORT, DEFAULT_TIMEOUT, RPC_PATH,
};
pub use rpc::{RpcError, RpcRequest, RpcResponse};
pub use version::{parse_version_line, parse_version_number, Aria2Version, VersionParseError};

/// aria2 核心来源（配置键 `aria2.source`，默认 `managed`，见架构 §4.11/R1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    /// AriaHost 自行下载/校验/切换的版本（默认）。
    #[default]
    Managed,
    /// 用户指定的外部 aria2c（仍由 AriaHost 渲染受控 conf）。
    External,
    /// 随安装包内嵌的版本（首版不提供，预留）。
    Bundled,
}

impl Source {
    /// 配置/日志里的稳定字符串（与 TOML 键值一致）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::External => "external",
            Self::Bundled => "bundled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_source_is_managed() {
        assert_eq!(Source::default(), Source::Managed);
        assert_eq!(Source::Managed.as_str(), "managed");
        assert_eq!(Source::External.as_str(), "external");
        assert_eq!(Source::Bundled.as_str(), "bundled");
    }
}
