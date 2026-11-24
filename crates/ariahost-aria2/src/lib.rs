//! aria2 集成骨架（架构 §3.2）。
//!
//! 本批只有三件事：版本行解析、核心来源枚举、JSON-RPC 最小数据结构。
//! **不发任何网络请求、不拉起任何进程**；进程管理、RPC client、版本管理器在 M2/M3 落地。

pub mod rpc;
pub mod version;

pub use rpc::{RpcError, RpcRequest, RpcResponse};
pub use version::{parse_version_line, Aria2Version, VersionParseError};

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
