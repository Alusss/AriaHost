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
    local_appdata_dir().map(|dir| dir.join("cache"))
}

/// 服务运行时根：`%ProgramData%\AriaHost`（conf/session/logs/secret 都落在这里）。
pub fn service_runtime_dir() -> Option<PathBuf> {
    env_dir("ProgramData").map(|dir| dir.join("AriaHost"))
}

/// 用户平面根：`%APPDATA%\AriaHost`（roaming：配置、日志、session、runtime）。
fn appdata_dir() -> Option<PathBuf> {
    env_dir("APPDATA").map(|dir| dir.join("AriaHost"))
}

/// 用户平面本地根：`%LOCALAPPDATA%\AriaHost`（不漫游：cache、tmp）。
fn local_appdata_dir() -> Option<PathBuf> {
    env_dir("LOCALAPPDATA").map(|dir| dir.join("AriaHost"))
}

/// 日志目录：用户平面 `%APPDATA%\AriaHost\logs`，服务平面 `%ProgramData%\AriaHost\logs`。
///
/// 两种平面的日志互不可见（服务账户下 `%APPDATA%` 会解析到 systemprofile），
/// 所以清理器必须按平面分别扫描，不能只扫其中一个。
pub fn logs_dir(plane: Plane) -> Option<PathBuf> {
    match plane {
        Plane::User => appdata_dir().map(|dir| dir.join("logs")),
        Plane::Service => service_runtime_dir().map(|dir| dir.join("logs")),
    }
}

/// session 目录：用户平面 `%APPDATA%\AriaHost\session`，服务平面 `%ProgramData%\AriaHost\session`。
///
/// 里面是 `aria2.session` 与 `.bak`。**清理器永远不删这个目录里的任何东西**
/// （删了等于丢全部未完成任务），这里只提供路径给调用方读。
pub fn session_dir(plane: Plane) -> Option<PathBuf> {
    match plane {
        Plane::User => appdata_dir().map(|dir| dir.join("session")),
        Plane::Service => service_runtime_dir().map(|dir| dir.join("session")),
    }
}

/// 临时目录：用户平面 `%LOCALAPPDATA%\AriaHost\tmp`，服务平面 `%ProgramData%\AriaHost\tmp`。
///
/// 为什么不放 roaming（`%APPDATA%`）：临时产物是机器本地、可随时丢弃的，
/// 漫游目录会被同步到域账户服务器，既浪费流量又会把别处的过期文件带回来。
pub fn tmp_dir(plane: Plane) -> Option<PathBuf> {
    match plane {
        Plane::User => local_appdata_dir().map(|dir| dir.join("tmp")),
        Plane::Service => service_runtime_dir().map(|dir| dir.join("tmp")),
    }
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

    #[test]
    fn per_plane_dirs_split_roaming_from_local() {
        // 用户平面：日志/session 在 roaming，tmp 在本地（不漫游）。
        if let Some(path) = logs_dir(Plane::User) {
            assert!(
                path.ends_with(Path::new("AriaHost").join("logs")),
                "{path:?}"
            );
        }
        if let Some(path) = session_dir(Plane::User) {
            assert!(
                path.ends_with(Path::new("AriaHost").join("session")),
                "{path:?}"
            );
        }
        if let Some(path) = tmp_dir(Plane::User) {
            assert!(
                path.ends_with(Path::new("AriaHost").join("tmp")),
                "{path:?}"
            );
            // tmp 不得落在 cache 之下（cache 由版本库独占），也不得等于 cache 本身。
            assert!(!path.ends_with(Path::new("AriaHost").join("cache")));
        }
        // 服务平面：三个目录都在 ProgramData 下（按平面分开，绝不共用 user 平面路径）。
        for (dir, expected) in [
            (logs_dir(Plane::Service), "logs"),
            (session_dir(Plane::Service), "session"),
            (tmp_dir(Plane::Service), "tmp"),
        ] {
            if let Some(path) = dir {
                assert!(
                    path.ends_with(Path::new("AriaHost").join(expected)),
                    "{path:?}"
                );
            }
        }
        // 两平面同名目录不得相等。
        if let (Some(user), Some(service)) = (logs_dir(Plane::User), logs_dir(Plane::Service)) {
            assert_ne!(user, service, "两平面的日志目录不能是同一个");
        }
    }

    #[test]
    fn user_plane_dirs_are_distinct() {
        let mut seen = Vec::new();
        for path in [
            config_file(),
            cache_dir(),
            logs_dir(Plane::User),
            session_dir(Plane::User),
            tmp_dir(Plane::User),
            versions_dir(Plane::User),
        ]
        .into_iter()
        .flatten()
        {
            assert!(!seen.contains(&path), "目录重复：{path:?}");
            seen.push(path);
        }
    }
}
