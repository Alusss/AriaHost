//! UAC 提权封装（M3-b / task-46）：`ShellExecuteExW` + `runas`。
//!
//! 只用于服务管理（装/卸/启/停都要改 SCM，非管理员必失败）。
//!
//! ## 为什么必须放到后台线程
//!
//! 实测（锁屏/无人值守的真机）：`ShellExecuteExW(verb="runas")` **会阻塞 122 秒**才返回
//! `FALSE` + `GetLastError()=1223`（`ERROR_CANCELLED`）。这条阻塞发生在**调用它的线程**上，
//! 没有任何超时参数可设——所以绝不能放在 UI 线程，否则点一下面板就假死两分钟。
//! 本模块只负责「一次提权调用 + 等它结束」，调用方负责把它丢进线程。
//!
//! ## 为什么不用 `runas` 以外的办法
//!
//! 硬约束（task-46）：只用 `ShellExecuteW/ExW` + `runas`，不引入新依赖。
//! `windows-sys` 已在依赖里，本模块只要求它多开一个 `Win32_System_Registry` feature
//! （`SHELLEXECUTEINFOW` 的 `hkeyClass` 字段被它门控），不新增任何 crate。

use std::path::Path;
use std::time::Duration;

/// 一次提权调用的结果。
///
/// 非 Windows 目标上这些变体不会被构造（提权实现是占位），这里的 allow 属性只为了让
/// cargo clippy -D warnings 在本机 Linux 侧也能过。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevateOutcome {
    /// 提权进程跑完并给出退出码（0 才是成功，具体含义由被调用的子命令定义）。
    Completed {
        /// 子进程退出码。
        exit_code: u32,
        /// 从发起到结束的耗时（含用户看授权框的时间）。
        waited: Duration,
    },
    /// 用户点了「否」，或当前会话根本看不到授权框（`ERROR_CANCELLED`）。
    Cancelled {
        /// 从发起到返回的耗时：>=60s 基本可以判定是「会话看不到授权框」。
        waited: Duration,
    },
    /// 授权框没能弹出来（没有交互桌面、策略禁止等）。
    LaunchFailed {
        /// Win32 错误码（0 表示平台不支持等非 Win32 原因）。
        error: u32,
        /// 可读原因（进日志，不上屏）。
        detail: String,
    },
    /// 子进程没在预算时间内结束。
    WaitTimedOut {
        /// 已等待时长。
        waited: Duration,
    },
}

/// 等待子进程时的轮询粒度：既能及时响应，也不会把 CPU 烧起来。
#[cfg_attr(not(windows), allow(dead_code))]
const POLL: Duration = Duration::from_millis(120);

/// 当前进程是否已经是管理员（提权后就无需再弹框）。
#[cfg(windows)]
pub fn is_elevated() -> bool {
    // SAFETY: `IsUserAnAdmin` 无参数、无副作用，只读当前令牌。
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// 非 Windows：本产品只发 Windows，这里只为让 `cargo check` 通过。
#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// 用 `runas` 拉起一个**已经提权**的子进程，并等它结束。
///
/// `timeout` 只约束「等子进程」这一段；`ShellExecuteExW` 自身在某些会话下会阻塞很久
/// （见模块文档），那一段无法设上限——调用方必须在后台线程里调用本函数。
#[cfg(windows)]
pub fn run_elevated(file: &Path, args: &[String], timeout: Duration) -> ElevateOutcome {
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_CANCELLED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };

    let verb = wide("runas");
    let file_w = wide(&file.to_string_lossy());
    let params_w = wide(&join_parameters(args));

    // SAFETY: 结构体先清零再逐字段填，`cbSize` 必须是结构体大小（Win32 契约）。
    let started = std::time::Instant::now();
    let info = unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file_w.as_ptr();
        info.lpParameters = params_w.as_ptr();
        // 提权后的 CLI 不需要窗口：结果走退出码 + 状态轮询。
        info.nShow = 0; // SW_HIDE
        if ShellExecuteExW(&mut info) == 0 {
            let error = GetLastError();
            return if error == ERROR_CANCELLED {
                ElevateOutcome::Cancelled {
                    waited: started.elapsed(),
                }
            } else {
                ElevateOutcome::LaunchFailed {
                    error,
                    detail: format!("ShellExecuteExW(runas) 失败，Win32={error}"),
                }
            };
        }
        info
    };

    let handle = info.hProcess;
    if handle.is_null() {
        return ElevateOutcome::LaunchFailed {
            error: 0,
            detail: "ShellExecuteExW 成功但没有返回进程句柄（SEE_MASK_NOCLOSEPROCESS 未生效）"
                .to_owned(),
        };
    }

    // SAFETY: 句柄由 ShellExecuteExW 交给我们，用完必须 CloseHandle。
    unsafe {
        loop {
            let waited_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            match WaitForSingleObject(handle, POLL.as_millis() as u32) {
                WAIT_OBJECT_0 => {
                    let mut code: u32 = 0;
                    if GetExitCodeProcess(handle, &mut code) == 0 {
                        let error = GetLastError();
                        windows_sys::Win32::Foundation::CloseHandle(handle);
                        return ElevateOutcome::LaunchFailed {
                            error,
                            detail: format!("GetExitCodeProcess 失败，Win32={error}"),
                        };
                    }
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                    return ElevateOutcome::Completed {
                        exit_code: code,
                        waited: started.elapsed(),
                    };
                }
                WAIT_TIMEOUT => {
                    // WAIT_TIMEOUT：还在跑，看总预算。
                    if started.elapsed() >= timeout {
                        windows_sys::Win32::Foundation::CloseHandle(handle);
                        return ElevateOutcome::WaitTimedOut {
                            waited: started.elapsed(),
                        };
                    }
                    let _ = waited_ms;
                }
                other => {
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                    return ElevateOutcome::LaunchFailed {
                        error: other,
                        detail: format!("WaitForSingleObject 返回 {other}"),
                    };
                }
            }
        }
    }
}

/// 非 Windows 占位：本产品只发 Windows。
#[cfg(not(windows))]
pub fn run_elevated(_file: &Path, _args: &[String], _timeout: Duration) -> ElevateOutcome {
    ElevateOutcome::LaunchFailed {
        error: 0,
        detail: "非 Windows 平台没有 UAC".to_owned(),
    }
}

/// 参数串拼接：含空格/引号的参数要加引号，否则 `runas` 会把它们拆开。
#[cfg(windows)]
fn join_parameters(args: &[String]) -> String {
    let mut out = String::new();
    for arg in args {
        if !out.is_empty() {
            out.push(' ');
        }
        if arg.is_empty() || arg.contains(' ') || arg.contains('"') {
            out.push('"');
            out.push_str(&arg.replace('"', "\\\""));
            out.push('"');
        } else {
            out.push_str(arg);
        }
    }
    out
}

/// Rust 字符串 → 以 NUL 结尾的 UTF-16。
#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    buffer.push(0);
    buffer
}

/// 面板该用哪个可执行文件做提权助手：优先自己（同一份二进制，子命令区分行为）。
pub fn helper_exe() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok()
}

/// `sc.exe` 的绝对路径（启动/停止服务没有对应的 CLI 子命令，这一批用系统自带的 sc）。
pub fn sc_exe() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    Some(std::path::Path::new(&root).join("System32").join("sc.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_paths_are_absolute_when_available() {
        if let Some(exe) = helper_exe() {
            assert!(exe.is_absolute(), "提权助手必须是绝对路径：{exe:?}");
        }
        #[cfg(windows)]
        {
            let sc = sc_exe().expect("Windows 必有 SystemRoot");
            assert!(sc.is_absolute());
            assert!(sc.to_string_lossy().ends_with("sc.exe"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn parameters_are_quoted_when_needed() {
        assert_eq!(
            join_parameters(&["--install-service".to_owned()]),
            "--install-service"
        );
        assert_eq!(
            join_parameters(&["start".to_owned(), "AriaHost".to_owned()]),
            "start AriaHost"
        );
        // 带空格的路径（用户可能把面板装在 "C:\Program Files\..." 下）。
        assert_eq!(
            join_parameters(&["--write-aumid".to_owned(), "C:\\a b\\x.lnk".to_owned()]),
            "--write-aumid \"C:\\a b\\x.lnk\""
        );
    }
}
