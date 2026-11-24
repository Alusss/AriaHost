//! aria2 核心获取与版本管理（M1-3）。
//!
//! 模块分工：
//! - [`release`]：GitHub releases API 的载荷解析、资产名白名单、架构选择、更新四态（纯逻辑）；
//! - [`http`]：唯一一条 HTTP/TLS 栈（ureq + rustls），以及 JSON/zip 两类载荷断言；
//! - [`download`]：下载线路模板与「逐线路失败可见」的聚合（不静默降级）；
//! - [`zip_extract`]：只取 `aria2c.exe`（含随包 DLL），带 zip slip / 炸弹防护；
//! - [`store`]：版本库布局、`active.json` 原子写、切换与单代回滚；
//! - [`fetch`]：把上面这些串起来的编排，以及哈希口径（[`KNOWN_SHA256`] / [`HashState`]）。
//!
//! ## TLS 依赖决策（本批唯一新增的 HTTP/TLS 栈）
//!
//! 结论：**`ureq 3.x` + `rustls`（ring 后端 + 编译期内置 `webpki-roots`）**，全仓只此一条。
//! - 不引 reqwest：tokio 异步栈与本仓单线程模型、单 exe 体积预算都不匹配；
//! - 不引第二套 TLS：`native-tls`/`schannel` 都不需要；
//! - **WinHTTP 被否的具体理由**：它只能是 `#[cfg(windows)]`，而本仓库的本地验证通道是
//!   Linux 侧 `cargo clippy/test`（WSL）。Windows 专属分支在本地编译不到、测不到，
//!   等于把这一块退回到「只能靠 CI 试错」（M1-2A 已经为此付过 5 轮推送的代价）。
//!   用 ureq 则同一份代码在 Linux 上就被 clippy 与假服务器测试覆盖。
//!   代价：不读 Windows 证书库（企业 MITM 代理会校验失败）——设计上线路失败本来就该
//!   给出 `AH-E6001` 可见提示，不需要为它引入约 300 行 unsafe。
//!
//! ## 边界
//!
//! 不碰面板、不装服务、不做命名管道；只提供库 API 与一个隐藏 CLI 入口
//! （`ariahost --aria2-fetch <latest|版本号>`）。

pub mod download;
pub mod error;
pub mod fetch;
pub mod http;
pub mod release;
pub mod store;
pub mod zip_extract;

pub use download::{
    default_lines, DownloadLine, DownloadOutcome, DEFAULT_MIRROR_TEMPLATE, DIRECT_LINE_NAME,
    MIRROR_LINE_NAME, URL_PLACEHOLDER,
};
pub use error::{CoreError, LineAttempt};
pub use fetch::{
    known_sha256, parse_version_arg, verify_hash, FetchOutcome, Fetcher, HashState, KNOWN_SHA256,
};
pub use http::{
    fetch_json, fetch_zip, HttpClient, HttpOptions, HttpReply, DEFAULT_MAX_BODY, DEFAULT_TIMEOUT,
    MAX_JSON_BODY, ZIP_MAGIC,
};
pub use release::{
    check_update, host_bits, match_win_asset, release_version, GitHubRelease, ReleaseAsset,
    ReleaseCatalog, ReleaseEntry, UpdateState, MIN_SUPPORTED, RELEASES_API,
};
pub use store::{ActivePointer, ExeProbe, VersionStore, ACTIVE_FILE};
pub use zip_extract::{extract_core, ExtractedCore};

/// 核心可执行文件名（版本库、解包、安装共用一处，避免第二份字面量）。
pub const CORE_EXE: &str = "aria2c.exe";

/// 随包 DLL 的后缀（小写比较）。
pub const DLL_SUFFIX: &str = ".dll";
