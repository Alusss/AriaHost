//! 路径与管道名约定（架构 §3.3）。
//!
//! 本模块只做路径拼装：不创建目录、不读文件、不校验存在性。
//! 目录来自 Windows 环境变量；取不到时返回 `None`，由调用方决定错误码与文案，
//! 这里不 panic、不猜盘符、不硬编码用户名。

use std::path::PathBuf;

/// 版本库所属平面。
///
/// R9-b/R11：用户平面与服务平面**各自持有**版本库与激活指针，
/// 一致性由比对/同步规则保证，不存在「单一共享版本库」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    /// 用户会话平面：`%LOCALAPPDATA%\AriaHost\cache\aria2\versions\<ver>\`。
    User,
    /// 服务平面：`%ProgramData%\AriaHost\aria2\versions\<ver>\`。
    Service,
}

/// `%APPDATA%\AriaHost\config.toml`（`.bak` 由调用方在同目录派生）。
pub fn config_file() -> Option<PathBuf> {
    env_dir("APPDATA").map(|dir| dir.join("AriaHost").join("config.toml"))
}

/// 用户平面缓存根：`%LOCALAPPDATA%\AriaHost\cache`。
pub fn cache_dir() -> Option<PathBuf> {
    env_dir("LOCALAPPDATA").map(|dir| dir.join("AriaHost").join("cache"))
}

/// 服务运行时根：`%ProgramData%\AriaHost`（conf/session/logs/secret 都落在这里）。
pub fn service_runtime_dir() -> Option<PathBuf> {
    env_dir("ProgramData").map(|dir| dir.join("AriaHost"))
}

/// 版本库根：按平面返回 `...\aria2\versions`。
///
/// 调用方再拼 `<ver>` 子目录；`active` 激活指针与版本库同层，由 M3 的版本管理器维护。
pub fn versions_dir(plane: Plane) -> Option<PathBuf> {
    match plane {
        Plane::User => cache_dir().map(|dir| dir.join("aria2").join("versions")),
        Plane::Service => service_runtime_dir().map(|dir| dir.join("aria2").join("versions")),
    }
}

/// 控制管道名：`\\.\pipe\ariahost-ctl`（DACL 见架构 §3.1）。
pub const fn control_pipe_name() -> &'static str {
    r"\\.\pipe\ariahost-ctl"
}

/// 读取非空环境变量目录。
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn config_file_ends_with_expected_suffix() {
        if let Some(path) = config_file() {
            assert!(
                path.ends_with(Path::new("AriaHost").join("config.toml")),
                "{path:?}"
            );
        }
    }

    #[test]
    fn cache_dir_ends_with_expected_suffix() {
        if let Some(path) = cache_dir() {
            assert!(
                path.ends_with(Path::new("AriaHost").join("cache")),
                "{path:?}"
            );
        }
    }

    #[test]
    fn service_runtime_dir_ends_with_product_dir() {
        if let Some(path) = service_runtime_dir() {
            assert!(path.ends_with("AriaHost"), "{path:?}");
        }
    }

    #[test]
    fn versions_dir_follows_plane() {
        if let Some(path) = versions_dir(Plane::User) {
            assert!(
                path.ends_with(
                    Path::new("AriaHost")
                        .join("cache")
                        .join("aria2")
                        .join("versions")
                ),
                "{path:?}"
            );
        }
        if let Some(path) = versions_dir(Plane::Service) {
            assert!(
                path.ends_with(Path::new("AriaHost").join("aria2").join("versions")),
                "{path:?}"
            );
        }
    }

    #[test]
    fn control_pipe_name_is_pinned() {
        assert_eq!(control_pipe_name(), r"\\.\pipe\ariahost-ctl");
    }
}
