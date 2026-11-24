//! 统一错误码注册表（架构 §4.12）。
//!
//! 前缀定版 `AH-E####`，码段固定：`E1###` 配置存储/命令行｜`E2###` aria2 核心｜
//! `E3###` 网络与任务｜`E4###` 磁盘｜`E5###` Windows 服务｜`E6###` 版本与升级｜
//! `E9###` 通用。
//!
//! 本批只落「码 → 分类」的静态映射；逐码文案模板与动作注册表（retry/open_logs/…）
//! 在 M5 与 MSG/ST 表一起落地。新增码必须同时补 [`ErrorCode`] 与 `code()`。

/// 错误分类（架构 §4.12 的六类，供 UI 决定主按钮与文案模板）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// 用户可自行修复（改设置、换端口、重试等）。
    UserFixable,
    /// 环境缺失（找不到 aria2、磁盘空间、系统路径失效等）。
    EnvMissing,
    /// 网络问题（下载、更新、RPC 传输）。
    Network,
    /// 权限问题（管理员/UAC、ACL）。
    Permission,
    /// aria2 内部故障（进程、RPC 语义）。
    Aria2Internal,
    /// 无法归因，进诊断包。
    Unknown,
}

impl ErrorCategory {
    /// 稳定字符串，供日志/诊断包使用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserFixable => "USER_FIXABLE",
            Self::EnvMissing => "ENV_MISSING",
            Self::Network => "NETWORK",
            Self::Permission => "PERMISSION",
            Self::Aria2Internal => "ARIA2_INTERNAL",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// 错误码（`AH-E####`）。
///
/// 变体顺序按码段排列，便于人工核对；`code()` 是唯一对外字符串来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    // ---- E1### 配置存储 / 命令行 ----
    /// AH-E1001：命令行用法错误（缺参数、未知参数）。
    ///
    /// 本批 CLI 契约指定用此码；架构 §4.12 未单列该码，故语义按「配置/用法类」归类。
    Usage,
    /// AH-E1002：配置解析/校验失败（TOML 语法坏了、字段类型不符）。
    ///
    /// 可执行建议：**修复它，或删除后重新生成**（文件已经坏了，删掉不心疼）。
    ConfigParseFailed,
    /// AH-E1003：配置读写 IO 失败（无权限、磁盘满、文件被占用）。
    ///
    /// 可执行建议：检查文件/目录权限与磁盘空间后重试。
    ConfigIoFailed,
    /// AH-E1004：凭据加解密失败（DPAPI / `secret.bin`）。
    ///
    /// 可执行建议：重新生成 RPC secret（`secret.bin` 损坏或不属于当前用户 profile）。
    CredentialCryptoFailed,
    /// AH-E1005：配置 schema 版本过新（本程序比文件旧，通常是用户降级了 AriaHost）。
    ///
    /// **与 [`Self::ConfigParseFailed`] 的差别是可执行建议**：文件没坏，只是比本程序新。
    /// 建议是「把 AriaHost 升级回去」或「先备份再重置配置」——
    /// **绝不能建议「删掉配置」**，否则用户一键丢掉全部设置（架构 §4.12 主按钮口径）。
    ConfigSchemaTooNew,

    // ---- E2### aria2 核心 ----
    /// AH-E2001：未找到 aria2c（→ MSG-U06）。
    Aria2NotFound,
    /// AH-E2002：aria2 子进程启动 10s 超时。
    Aria2StartTimeout,
    /// AH-E2003：RPC 握手失败（5s×3，退避 2/5/10s）。
    RpcHandshakeFailed,
    /// AH-E2005：RPC 重连 5 次失败（退避 2s→60s）。
    RpcReconnectFailed,

    // ---- E3### 网络与任务 ----
    /// AH-E3001：端口被占/疑似另一个 aria2。
    PortConflict,
    /// AH-E3002：下载哈希校验失败。
    HashMismatch,
    /// AH-E3003：HTTP 403/404。
    HttpRejected,

    // ---- E4### 磁盘 ----
    /// AH-E4001：下载盘空间不足。
    DiskFull,

    // ---- E5### Windows 服务 ----
    /// AH-E5001：SCM 启动 30s 超时/服务映像路径失效。
    ///
    /// M1 的 `--service` 桩复用此码表示 ServiceHost 尚未实现。
    ServiceUnavailable,
    /// AH-E5002：服务操作需管理员/UAC 被取消（ERROR_CANCELLED=1223）。
    ServiceNeedsAdmin,
    /// AH-E5003：服务注册前置失败（ProgramData 权限或版本库准备失败）。
    ServiceRegisterPrereq,

    // ---- E6### 版本与升级 ----
    /// AH-E6001：更新网络失败。
    UpdateNetworkFailed,
    /// AH-E6002：更新请求被限流（429）。
    UpdateRateLimited,
    /// AH-E6003：更新包校验失败。
    UpdatePackageInvalid,
    /// AH-E6004：核心版本过低，缺少所需 RPC 能力。
    CoreVersionTooOld,
    /// AH-E6005：exe 被占用（升级前必须停核心）。
    ExeInUse,
    /// AH-E6006：切换失败，已自动回滚。
    SwitchRolledBack,
    /// AH-E6007：下载源全部不可用（= version 文档 E_ALL_LINES_DOWN）。
    AllSourcesUnavailable,
    /// AH-E6008：回滚自身失败（= E_ROLLBACK）。
    RollbackFailed,

    // ---- E9### 通用 ----
    /// AH-E9001：重复实例（面板单实例互斥）。
    DuplicateInstance,
    /// AH-E9002：session 损坏（已自动用 `.bak`）。
    SessionCorrupted,
    /// AH-E9003：内置网页页加载失败。
    BuiltinPageFailed,
    /// AH-E9004：无默认浏览器。
    NoDefaultBrowser,
}

impl ErrorCode {
    /// 返回 `AH-E####` 形式的错误码字符串。
    pub const fn code(self) -> &'static str {
        match self {
            Self::Usage => "AH-E1001",
            Self::ConfigParseFailed => "AH-E1002",
            Self::ConfigIoFailed => "AH-E1003",
            Self::CredentialCryptoFailed => "AH-E1004",
            Self::ConfigSchemaTooNew => "AH-E1005",
            Self::Aria2NotFound => "AH-E2001",
            Self::Aria2StartTimeout => "AH-E2002",
            Self::RpcHandshakeFailed => "AH-E2003",
            Self::RpcReconnectFailed => "AH-E2005",
            Self::PortConflict => "AH-E3001",
            Self::HashMismatch => "AH-E3002",
            Self::HttpRejected => "AH-E3003",
            Self::DiskFull => "AH-E4001",
            Self::ServiceUnavailable => "AH-E5001",
            Self::ServiceNeedsAdmin => "AH-E5002",
            Self::ServiceRegisterPrereq => "AH-E5003",
            Self::UpdateNetworkFailed => "AH-E6001",
            Self::UpdateRateLimited => "AH-E6002",
            Self::UpdatePackageInvalid => "AH-E6003",
            Self::CoreVersionTooOld => "AH-E6004",
            Self::ExeInUse => "AH-E6005",
            Self::SwitchRolledBack => "AH-E6006",
            Self::AllSourcesUnavailable => "AH-E6007",
            Self::RollbackFailed => "AH-E6008",
            Self::DuplicateInstance => "AH-E9001",
            Self::SessionCorrupted => "AH-E9002",
            Self::BuiltinPageFailed => "AH-E9003",
            Self::NoDefaultBrowser => "AH-E9004",
        }
    }

    /// 错误分类，决定 UI 的主按钮与文案模板。
    pub const fn category(self) -> ErrorCategory {
        match self {
            Self::Usage
            | Self::ConfigParseFailed
            | Self::ConfigIoFailed
            | Self::CredentialCryptoFailed
            | Self::ConfigSchemaTooNew
            | Self::PortConflict
            | Self::CoreVersionTooOld
            | Self::ExeInUse
            | Self::DuplicateInstance => ErrorCategory::UserFixable,
            Self::Aria2NotFound
            | Self::DiskFull
            | Self::ServiceUnavailable
            | Self::NoDefaultBrowser => ErrorCategory::EnvMissing,
            Self::HashMismatch
            | Self::HttpRejected
            | Self::UpdateNetworkFailed
            | Self::UpdateRateLimited
            | Self::UpdatePackageInvalid
            | Self::AllSourcesUnavailable => ErrorCategory::Network,
            Self::ServiceNeedsAdmin | Self::ServiceRegisterPrereq => ErrorCategory::Permission,
            Self::Aria2StartTimeout | Self::RpcHandshakeFailed | Self::RpcReconnectFailed => {
                ErrorCategory::Aria2Internal
            }
            Self::SwitchRolledBack
            | Self::RollbackFailed
            | Self::SessionCorrupted
            | Self::BuiltinPageFailed => ErrorCategory::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 注册表全集：新增变体时必须同步，防止漏映射。
    const ALL: &[ErrorCode] = &[
        ErrorCode::Usage,
        ErrorCode::ConfigParseFailed,
        ErrorCode::ConfigIoFailed,
        ErrorCode::CredentialCryptoFailed,
        ErrorCode::ConfigSchemaTooNew,
        ErrorCode::Aria2NotFound,
        ErrorCode::Aria2StartTimeout,
        ErrorCode::RpcHandshakeFailed,
        ErrorCode::RpcReconnectFailed,
        ErrorCode::PortConflict,
        ErrorCode::HashMismatch,
        ErrorCode::HttpRejected,
        ErrorCode::DiskFull,
        ErrorCode::ServiceUnavailable,
        ErrorCode::ServiceNeedsAdmin,
        ErrorCode::ServiceRegisterPrereq,
        ErrorCode::UpdateNetworkFailed,
        ErrorCode::UpdateRateLimited,
        ErrorCode::UpdatePackageInvalid,
        ErrorCode::CoreVersionTooOld,
        ErrorCode::ExeInUse,
        ErrorCode::SwitchRolledBack,
        ErrorCode::AllSourcesUnavailable,
        ErrorCode::RollbackFailed,
        ErrorCode::DuplicateInstance,
        ErrorCode::SessionCorrupted,
        ErrorCode::BuiltinPageFailed,
        ErrorCode::NoDefaultBrowser,
    ];

    #[test]
    fn codes_have_expected_shape() {
        for code in ALL {
            let raw = code.code();
            assert_eq!(raw.len(), 8, "错误码长度必须是 8：{raw}");
            assert!(raw.starts_with("AH-E"), "错误码前缀必须是 AH-E：{raw}");
            assert!(
                raw[4..].chars().all(|c| c.is_ascii_digit()),
                "错误码后四位必须是数字：{raw}"
            );
        }
    }

    #[test]
    fn codes_are_unique() {
        for (index, code) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert_ne!(code.code(), other.code(), "错误码重复：{}", code.code());
            }
        }
    }

    #[test]
    fn cli_and_service_codes_match_contract() {
        assert_eq!(ErrorCode::Usage.code(), "AH-E1001");
        assert_eq!(ErrorCode::ServiceUnavailable.code(), "AH-E5001");
        assert_eq!(ErrorCode::Usage.category(), ErrorCategory::UserFixable);
        assert_eq!(
            ErrorCode::ServiceNeedsAdmin.category(),
            ErrorCategory::Permission
        );
        assert_eq!(ErrorCategory::Network.as_str(), "NETWORK");
    }
}
