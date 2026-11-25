//! 面板单实例（EG-12）。
//!
//! ## 为什么从「只有文件锁」改成「互斥体为主 + 文件锁兜底」（task-71 判据 B）
//!
//! v0.1.2 真机上出现过**两个面板并存**，其中一个抢不到网页端口而报 `AH-E3001`。
//! 根因不在「文件锁本身不生效」，而在它有一条**静默放行**的路：
//! [`LockOutcome::Unavailable`]（目录不可写 / 权限 / IO 错）时调用方按「不阻塞启动」继续跑，
//! 于是第二个实例照常开窗，两个进程再一起抢 `127.0.0.1:6875`。
//!
//! 现在按架构 §4.1.4 的定稿机制收口：
//!
//! - **Windows 主判据 = 命名互斥体**（`CreateMutexW` + `ERROR_ALREADY_EXISTS`）。
//!   它不依赖任何目录或环境变量，不存在「打开失败就放行」的路。
//! - **`%LOCALAPPDATA%\AriaHost\panel.lock` 的文件锁**（`std::fs::File::try_lock`）保留：
//!   非 Windows 的唯一实现；Windows 上作为**尽力而为的第二层**（跨登录会话/跨用户目录），
//!   拿不到只记一行日志，不影响「本会话唯一」这个结论。
//! - 进程退出（含崩溃）时句柄由系统关闭、锁与互斥体都自动释放，不留「幽灵实例」。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Windows 命名互斥体名（`Local\` 前缀 = 每登录会话一个）。
///
/// 与文件锁的粒度一致（都是「同一用户会话内唯一」）：多用户/RDP 各开一个面板是允许的，
/// 跨会话管道隔离由 task-56 负责，两者不冲突。
#[cfg(windows)]
pub const MUTEX_NAME: &str = "Local\\AriaHost-panel-instance";

/// 命名互斥体（EG-12 定稿机制）的最小封装。句柄统一存 `isize`，
/// 这样 [`InstanceLock`] 不必把 windows-sys 类型泄漏到跨平台结构里。
#[cfg(windows)]
mod mutex_lock {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;

    use super::MUTEX_NAME;

    /// 原始 `HANDLE`。
    pub type Handle = isize;

    /// 创建/打开**产品**命名互斥体，返回 `(句柄, 是否已经存在)`。
    pub fn create() -> Result<(Handle, bool), String> {
        create_named(MUTEX_NAME)
    }

    /// 创建/打开指定名字的互斥体（单测用独立名字，免得撞上本机真在跑的面板）。
    pub fn create_named(name: &str) -> Result<(Handle, bool), String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // 只在「已存在」时 `CreateMutexW` 才设 `ERROR_ALREADY_EXISTS`；先把 last error
        // 清零，否则会把之前任何一次调用的残留错误码误读成「已存在」。
        unsafe { SetLastError(0) };
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(format!("CreateMutexW 失败（Win32 {}）", unsafe {
                GetLastError()
            }));
        }
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        Ok((handle as Handle, existed))
    }

    /// 释放句柄（进程退出时系统也会回收，这里只是让 `Drop` 语义及时）。
    pub fn release(handle: Handle) {
        unsafe { CloseHandle(handle as _) };
    }
}

/// 单实例锁的获取结果。
pub enum LockOutcome {
    /// 拿到锁（持有 [`InstanceLock`] 期间其他实例拿不到）。
    Acquired(InstanceLock),
    /// 已有实例持锁：本次启动应打印提示后退出。
    AlreadyRunning {
        /// 锁文件路径（提示用）。
        path: PathBuf,
    },
    /// 锁不可用（环境变量缺失/权限/IO 错误）：按「不阻塞启动」处理，由调用方记日志。
    Unavailable {
        /// 预期锁文件路径。
        path: PathBuf,
        /// 失败原因（中文，写日志用）。
        reason: String,
    },
}

/// 持有单实例锁；`Drop` 时释放。
pub struct InstanceLock {
    path: PathBuf,
    /// 文件锁句柄（Windows 上是第二层，拿不到也不阻塞，故为 `Option`）。
    _file: Option<File>,
    /// Windows 命名互斥体句柄（主判据）。
    #[cfg(windows)]
    mutex: Option<mutex_lock::Handle>,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(handle) = self.mutex.take() {
            mutex_lock::release(handle);
        }
    }
}

impl InstanceLock {
    /// 锁路径（Windows 上互斥体到手时，这里是文件锁的预期路径；提示与日志用）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 面板日志/自检显示用的一行描述（中文）：**哪几层真的拿到了**，写清楚。
    pub fn describe(&self) -> String {
        #[cfg(windows)]
        if self.mutex.is_some() {
            return match self._file {
                Some(_) => format!("命名互斥体 {MUTEX_NAME} + 文件锁 {}", self.path.display()),
                None => format!("命名互斥体 {MUTEX_NAME}（文件锁未拿到，仅第一层生效）"),
            };
        }
        format!("文件锁 {}", self.path.display())
    }
}

/// 锁文件路径：`%LOCALAPPDATA%\AriaHost\panel.lock`；取不到环境变量时返回 `None`。
pub fn lock_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join("AriaHost").join("panel.lock"))
}

/// 获取单实例锁。
///
/// Windows：先命名互斥体（定稿判据），再尽力拿一层文件锁；非 Windows：只走文件锁。
pub fn acquire() -> LockOutcome {
    #[cfg(windows)]
    if let Some(outcome) = acquire_mutex() {
        return outcome;
    }
    acquire_file_lock()
}

/// Windows 命名互斥体路径。返回 `None` = 互斥体本身不可用（极端环境），调用方退回文件锁。
#[cfg(windows)]
fn acquire_mutex() -> Option<LockOutcome> {
    let expected = lock_path().unwrap_or_else(|| PathBuf::from(MUTEX_NAME));
    match mutex_lock::create() {
        Ok((handle, true)) => {
            // 已经有一个面板持有它：本次启动要退出，句柄立刻还回去（不留悬空引用）。
            mutex_lock::release(handle);
            Some(LockOutcome::AlreadyRunning {
                path: PathBuf::from(MUTEX_NAME),
            })
        }
        Ok((handle, false)) => {
            // 互斥体到手 = 本会话唯一。文件锁只做「跨目录/只读配置目录」的第二层兜底：
            // 拿不到不阻塞（否则「配置目录不可写」会变成「面板起不来」），但要留痕。
            let file = lock_path().and_then(best_effort_file_lock);
            Some(LockOutcome::Acquired(InstanceLock {
                path: expected,
                _file: file,
                mutex: Some(handle),
            }))
        }
        Err(reason) => {
            eprintln!("[ariahost] 命名互斥体不可用，退回文件锁：{reason}");
            None
        }
    }
}

/// 尽力拿文件锁：成功返回句柄；任何失败（目录/权限/已被同进程另一实例占住）返回 `None`。
#[cfg(windows)]
fn best_effort_file_lock(path: PathBuf) -> Option<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

/// 文件锁实现（非 Windows 的唯一实现；Windows 上作为兜底）。
fn acquire_file_lock() -> LockOutcome {
    match lock_path() {
        Some(path) => acquire_at(path),
        None => LockOutcome::Unavailable {
            path: PathBuf::from(r"%LOCALAPPDATA%\AriaHost\panel.lock"),
            reason: "环境变量 LOCALAPPDATA 缺失或为空".to_owned(),
        },
    }
}

/// 在指定路径上获取单实例锁（测试直接调这个，避免改进程环境变量）。
pub fn acquire_at(path: PathBuf) -> LockOutcome {
    if let Some(parent) = path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            return LockOutcome::Unavailable {
                path,
                reason: format!("创建锁文件目录失败：{error}"),
            };
        }
    }
    let file = match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            return LockOutcome::Unavailable {
                path,
                reason: format!("打开锁文件失败：{error}"),
            };
        }
    };
    match file.try_lock() {
        Ok(()) => LockOutcome::Acquired(InstanceLock {
            path,
            _file: Some(file),
            #[cfg(windows)]
            mutex: None,
        }),
        Err(TryLockError::WouldBlock) => LockOutcome::AlreadyRunning { path },
        Err(TryLockError::Error(error)) => LockOutcome::Unavailable {
            path,
            reason: format!("try_lock 失败：{error}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同时只允许一个持有者；释放后可以重新获取。
    #[test]
    fn second_acquire_is_rejected_until_first_is_dropped() {
        let path = std::env::temp_dir()
            .join("ariahost-ui-instance-test")
            .join("panel.lock");
        let _ = std::fs::remove_file(&path);

        let first = acquire_at(path.clone());
        let lock = match first {
            LockOutcome::Acquired(lock) => lock,
            LockOutcome::AlreadyRunning { .. } => panic!("首次获取不应是 AlreadyRunning"),
            LockOutcome::Unavailable { reason, .. } => panic!("首次获取失败：{reason}"),
        };
        assert!(lock.describe().contains("panel.lock"));

        match acquire_at(path.clone()) {
            LockOutcome::AlreadyRunning { .. } => {}
            LockOutcome::Acquired(_) => panic!("第二次获取竟然成功了：文件锁没生效"),
            LockOutcome::Unavailable { reason, .. } => panic!("第二次获取异常：{reason}"),
        }

        drop(lock);
        match acquire_at(path.clone()) {
            LockOutcome::Acquired(_) => {}
            other => panic!(
                "释放后应能重新获取，实际：{}",
                match other {
                    LockOutcome::AlreadyRunning { .. } => "AlreadyRunning",
                    LockOutcome::Unavailable { .. } => "Unavailable",
                    LockOutcome::Acquired(_) => unreachable!(),
                }
            ),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lock_path_ends_with_expected_suffix() {
        if let Some(path) = lock_path() {
            assert!(
                path.ends_with(Path::new("AriaHost").join("panel.lock")),
                "{path:?}"
            );
        }
    }

    /// Windows 命名互斥体：第二次创建必须看到 `ERROR_ALREADY_EXISTS`。
    ///
    /// 这就是「第二个实例不开第二个窗口」的判据来源（task-71 B）。用独立名字，
    /// 避免撞上本机真在跑的面板或别的测试进程。
    #[cfg(windows)]
    #[test]
    fn named_mutex_second_create_reports_already_exists() {
        let name = format!("Local\\AriaHost-test-mutex-{}", std::process::id());
        let (first, existed) = mutex_lock::create_named(&name).expect("首次创建命名互斥体");
        assert!(!existed, "首次创建不应看到 ERROR_ALREADY_EXISTS");
        let (second, existed_again) = mutex_lock::create_named(&name).expect("再次创建命名互斥体");
        assert!(
            existed_again,
            "同进程第二次创建必须看到 ERROR_ALREADY_EXISTS"
        );
        mutex_lock::release(second);
        mutex_lock::release(first);
    }
}
