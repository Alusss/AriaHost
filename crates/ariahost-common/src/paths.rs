//! 路径与管道名约定（架构 §3.3）。
//!
//! 本模块只做路径拼装：不创建目录、不读文件、不校验存在性。
//! 目录来自 Windows 环境变量；取不到时返回 `None`，由调用方决定错误码与文案，
//! 这里不 panic、不猜盘符、不硬编码用户名。

use std::path::PathBuf;
use std::sync::OnceLock;

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

/// 控制管道名的前缀；后面接**当前进程令牌的用户 SID**（见 [`control_pipe_name`]）。
///
/// 为什么不再是一个固定名字（架构 §3.1）：固定名在**多用户 / RDP / 快速用户切换**下会让
/// `--quit` 打到别人的面板上——谁先连上谁被停，第一个能打开管道的会话就是受害者。
/// 名字按 SID 隔离后，「打不开」就等价于「当前用户没有面板在跑」，而不是去误杀别人。
const PIPE_PREFIX: &str = r"\\.\pipe\ariahost-ctl-";

/// 用户平面控制管道的名字：`\\.\pipe\ariahost-ctl-<用户 SID>`。
///
/// 语义（架构 §3.1 / §4.1.4）：
/// - **按用户 SID 隔离**，不用用户名（用户名可改、可重复、非 ASCII）；
/// - 进程内**只算一次**（`OnceLock`）：服务端与客户端必须逐字相同；
/// - **绝不回退到旧固定名**：那个名字现在只可能属于别的会话/别的用户，回退就是误杀；
/// - **服务平面（OS 服务模式）不用这个管道**，所以服务实例不参与这套隔离；
/// - 取不到 SID 时降级成含 PID 的名字（本进程内自洽，跨进程不再共享）并写一条 WARN。
pub fn control_pipe_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| match current_user_sid() {
        Some(sid) => pipe_name_for(&sid),
        None => {
            let pid = std::process::id();
            crate::logfile::write(
                crate::logfile::Level::Warn,
                "paths",
                &format!("取不到用户 SID，控制管道名降级为含 PID 的 {pid}（本进程内自洽）"),
            );
            pipe_name_for_pid(pid)
        }
    })
    .as_str()
}

/// 纯函数：SID → 管道名。单测直接打这里，不依赖真实令牌。
pub fn pipe_name_for(sid: &str) -> String {
    format!("{PIPE_PREFIX}{sid}")
}

/// 纯函数：取不到 SID 时的降级名（含 PID，进程内自洽）。
pub fn pipe_name_for_pid(pid: u32) -> String {
    format!("{PIPE_PREFIX}pid-{pid}")
}

/// 当前进程令牌里的用户 SID（`S-1-5-...` 形态）。
///
/// 失败一律返回 `None`，由调用方降级——**不 panic**：拿不到 SID 不该让面板起不来。
#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, TOKEN_INFORMATION_CLASS, TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `TOKEN_QUERY`（winnt.h）。
    const TOKEN_QUERY: u32 = 0x0008;

    /// 进程句柄不需要关；令牌句柄必须关。
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    let token = Token(token);
    let class: TOKEN_INFORMATION_CLASS = TokenUser;

    // 两次调用：第一次问需要多少字节（返回 0 + ERROR_INSUFFICIENT_BUFFER 属正常），
    // 第二次才真取。`needed` 在第一次之后就是缓冲区长度。
    let mut needed: u32 = 0;
    unsafe { GetTokenInformation(token.0, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    // TOKEN_USER 含指针字段，缓冲区必须按 `usize` 对齐——`Vec<u8>` 只有 1 字节对齐，别用。
    let words = (needed as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer: Vec<usize> = vec![0; words];
    if unsafe {
        GetTokenInformation(
            token.0,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return None;
    }

    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if sid.is_null() {
        return None;
    }
    let mut wide: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 || wide.is_null() {
        return None;
    }
    let mut len = 0usize;
    while unsafe { *wide.add(len) } != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    unsafe { LocalFree(wide as HLOCAL) };
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// 非 Windows：没有令牌 SID，直接降级（含 PID 的名字）。
#[cfg(not(windows))]
fn current_user_sid() -> Option<String> {
    None
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

    /// 管道名按 SID 隔离：名字里必须**逐字**带上 SID，且不是旧固定名。
    #[test]
    fn pipe_name_carries_the_sid_verbatim() {
        let sid = "S-1-5-21-1111-2222-3333-1001";
        assert_eq!(pipe_name_for(sid), format!(r"\\.\pipe\ariahost-ctl-{sid}"));
        assert!(pipe_name_for(sid).starts_with(PIPE_PREFIX));
    }

    /// 两个用户的管道名必须不同，且都**不等于**旧固定名（回退旧名 = 误杀别人的面板）。
    #[test]
    fn different_users_never_collide_and_never_fall_back_to_the_legacy_name() {
        let alice = pipe_name_for("S-1-5-21-1-2-3-1001");
        let bob = pipe_name_for("S-1-5-21-1-2-3-1002");
        assert_ne!(alice, bob);
        assert_ne!(alice, LEGACY_PIPE_NAME);
        assert_ne!(bob, LEGACY_PIPE_NAME);
    }

    /// 名字长度必须落在命名管道的上限内（`MAX_PATH` 260 / 实现按 256 卡）。
    ///
    /// 真实 SID 字符串 ~46 字符，Windows 文档给的最长形态也不超过 184；这里用「域式长 SID」
    /// 与 pid 降级名两头各钉一次，防止将来往名字里塞别的东西把长度顶爆。
    #[test]
    fn pipe_name_fits_in_the_named_pipe_limit() {
        for sid in [
            "S-1-5-21-3909229984-2559032529-2968041387-1015",
            "S-1-5-21-4294967295-4294967295-4294967295-4294967295",
        ] {
            let name = pipe_name_for(sid);
            assert!(
                name.chars().count() <= 256,
                "管道名超限（{} 字符）：{name}",
                name.chars().count()
            );
        }
        let pid = pipe_name_for_pid(u32::MAX);
        assert!(pid.chars().count() <= 256, "{pid}");
    }

    /// 取不到 SID 时的降级名：含 PID、进程内自洽、同样不回退旧名。
    #[test]
    fn pid_fallback_is_distinct_and_never_the_legacy_name() {
        assert_eq!(pipe_name_for_pid(4321), r"\\.\pipe\ariahost-ctl-pid-4321");
        assert_ne!(pipe_name_for_pid(4321), pipe_name_for_pid(4322));
        assert_ne!(pipe_name_for_pid(4321), LEGACY_PIPE_NAME);
    }

    /// 进程级名字只算一次（服务端与客户端必须逐字相同），且永不等于旧固定名。
    #[test]
    fn control_pipe_name_is_stable_within_the_process() {
        let first = control_pipe_name();
        let second = control_pipe_name();
        assert_eq!(first, second);
        assert!(first.starts_with(PIPE_PREFIX), "{first}");
        assert_ne!(first, LEGACY_PIPE_NAME);
    }

    /// 旧固定名（只用于断言「绝不再出现」）。
    const LEGACY_PIPE_NAME: &str = r"\\.\pipe\ariahost-ctl";

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
