//! 面板单实例（EG-12 的本切片实现）。
//!
//! 设计定稿是 `CreateMutexW` + `ERROR_ALREADY_EXISTS` + 命令行转发（架构 §4.1.4），
//! 那是 M2-1 的活。本切片只需要「第二个实例不要开第二个窗口」，所以用 std 的文件锁：
//! `std::fs::File::try_lock`（Rust 1.89 起稳定，工具链 1.95 可用），
//! 不引入 `single-instance` crate，也不为此新增任何 Windows API 绑定。
//!
//! 锁文件：`%LOCALAPPDATA%\AriaHost\panel.lock`。进程退出（含崩溃）时句柄关闭、锁自动释放，
//! 所以不会留「幽灵实例」——这正是文件锁相对手写 mutex 的一个便宜之处。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

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

/// 持有单实例锁；`Drop` 时随文件句柄关闭自动释放。
pub struct InstanceLock {
    path: PathBuf,
    /// 只为持有句柄：显式 drop 会在字段顺序里最后发生，锁随句柄关闭释放。
    _file: File,
}

impl InstanceLock {
    /// 锁文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 面板信息区显示用的一行描述（中文）。
    pub fn describe(&self) -> String {
        format!("已获取（{}）", self.path.display())
    }
}

/// 锁文件路径：`%LOCALAPPDATA%\AriaHost\panel.lock`；取不到环境变量时返回 `None`。
pub fn lock_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join("AriaHost").join("panel.lock"))
}

/// 按约定路径获取单实例锁。
pub fn acquire() -> LockOutcome {
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
        Ok(()) => LockOutcome::Acquired(InstanceLock { path, _file: file }),
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
}
