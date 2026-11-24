//! AriaHost 公共基础（架构 §3.2）：产品常量、错误码注册表、路径约定。
//!
//! 本批只提供纯函数与常量：不读文件、不建目录、不做配置迁移。
//! 配置 schema、MSG/ST 文案表、Win32 胶水在后续批次落地。

pub mod error;
pub mod paths;

pub use error::{ErrorCategory, ErrorCode};
pub use paths::Plane;

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
