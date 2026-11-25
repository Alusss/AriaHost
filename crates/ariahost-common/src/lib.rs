//! AriaHost 公共基础（架构 §3.2）：产品常量、错误码注册表、路径约定、配置层。
//!
//! 配置层（[`config`]）自 M2-2a 起落地：schema 定版 TOML 的读写、原子写 + `.bak`、
//! 未知键保留、`rpc.secret` 的生成与 DPAPI 保护。MSG/ST 文案表与更大的 Win32 胶水仍留后续批次。
//!
//! 跨模块共用的三个「只能有一份实现」的东西：[`redact`]（秘密值脱敏）、[`time`]
//! （UTC 时间戳）与 [`logfile`]（`ariahost.log` 落盘 + 轮转编号）。服务日志、任务历史、
//! 面板日志都指向这里，别再各写一份。

pub mod config;
pub mod error;
pub mod logfile;
pub mod paths;
pub mod redact;
pub mod time;

pub use config::{Aria2Source, Config, ConfigError, ConfigErrorKind, RpcListen, ServiceMode};
pub use error::{ErrorCategory, ErrorCode};
pub use logfile::{Level, LOG_FILE_NAME};
pub use paths::Plane;
pub use redact::{redact, redact_uri, REDACTED};
pub use time::{utc_stamp, STAMP_LEN};

/// 产品显示名（窗口标题、日志、安装器共用）。
pub const PRODUCT_NAME: &str = "AriaHost";

/// 可执行文件名（不含 `.exe`，命令行输出与管道名共用）。
pub const BINARY_NAME: &str = "ariahost";

/// 当前版本，来自 workspace 的 `version`，只此一处。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 退出码：成功。
pub const EXIT_OK: i32 = 0;

/// 退出码：一般失败（含参数用法错误）。
pub const EXIT_ERR: i32 = 1;

/// 退出码：`--quit` 与控制管道握手超时。
pub const EXIT_TIMEOUT: i32 = 2;
