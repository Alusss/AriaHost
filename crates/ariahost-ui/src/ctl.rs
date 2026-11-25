//! 控制管道**服务端**（架构 §3.1）：`\\.\pipe\ariahost-ctl`。
//!
//! ## 为什么需要它
//!
//! 安装器的卸载/覆盖升级流程会先跑 `ariahost.exe --quit`，等面板退出后才敢删 `$INSTDIR`
//! 下的 exe。`--quit` 长期是桩（永远回「无实例」），于是卸载时面板还开着、文件被占用，
//! 「卸载残留 / 升级写不进新文件」就是这么来的。本模块是那个桩的服务端。
//!
//! ## 协议（一行一问一答，UTF-8）
//!
//! ```text
//! 客户端 → quit\n
//! 服务端 → ok pid=1234\n        收到，退出流程已交给面板
//! 服务端 → refused unknown-command:xxx\n
//! ```
//!
//! `ok` 后面带**服务端 pid**：客户端据此等那个进程真的消失，而不是「发完就算成功」。
//!
//! ## 收到 quit 之后做什么
//!
//! **不** `process::exit`，也不自己停 aria2c——只做两件事：
//! 1. 置位 [`take_quit_request`] 的标记；
//! 2. `ctx.request_repaint()` 把面板从空闲里叫醒（egui 只在有输入/重绘请求时出帧，
//!    不叫醒的话这条请求要等到用户动鼠标才被看见）。
//!
//! 真正的退出（保存会话 → 停 aria2c → 关窗）由面板既有的 exit 档完成：
//! `app.rs` 里 `begin_quit(ctx, false)` 那条路径。**故意不在这里 `ViewportCommand::Close`**：
//! 那会被 `ui.close_action` 的三档拦下（tray 档会变成「藏进托盘」，ask 档会弹对话框），
//! 而卸载要的是「真退」。
//!
//! ## DACL
//!
//! [`PIPE_SDDL`]：显式拒绝网络登录（`NU`），允许 SYSTEM（`SY`）、本机管理员（`BA`）、
//! 以及对象所有者（`OW` = 建管道的当前用户）。管道名在 `\\.\pipe\` 下本身只能本机连接，
//! 再加一条 deny 是为了「即使将来被 `\\机器名\pipe\` 访问也进不来」。
//! 安全描述符构造失败时**降级**为系统默认 DACL（不阻断功能，日志里记一条）。
//!
//! ## 并发
//!
//! 单线程串行 accept：一个连接处理完立刻 `DisconnectNamedPipe` 并新建实例。
//! 多个客户端（安装器 + 用户在面板点退出）同时连不会死锁，只是排队——每条请求的处理
//! 只有「读一行 + 写一行」，微秒级。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::logbuf::LogBuffer;

/// 客户端唯一支持的指令。
pub const QUIT: &str = "quit";
/// 单行请求上限：读到这里还没换行就按未知指令处理（防客户端半截请求把线程挂住）。
pub const MAX_REQUEST_BYTES: usize = 64;

/// 管道 DACL（SDDL）：先 deny 网络登录，再 allow SYSTEM / 管理员 / 所有者。
pub const PIPE_SDDL: &str = "D:(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

/// 解析后的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// 通知面板退出。
    Quit,
    /// 不认识的指令（原样带回，便于排障）。
    Unknown(String),
}

/// 解析一行请求（纯函数，跨平台可测）。
pub fn parse_request(line: &str) -> Request {
    match line.trim() {
        QUIT => Request::Quit,
        other => Request::Unknown(other.to_owned()),
    }
}

/// 构造应答行（纯函数）。`ok` 带服务端 pid，客户端据此等进程真的退出。
pub fn ack_line(request: &Request, server_pid: u32) -> String {
    match request {
        Request::Quit => format!("ok pid={server_pid}\n"),
        Request::Unknown(command) => format!("refused unknown-command:{command}\n"),
    }
}

/// 从 `ok pid=1234` 这类应答里取 pid（客户端用；纯函数，跨平台可测）。
pub fn parse_ack_pid(ack: &str) -> Option<u32> {
    ack.split_whitespace()
        .find_map(|part| part.strip_prefix("pid="))
        .and_then(|value| value.trim().parse::<u32>().ok())
}

/// 服务端 pending 的「收到过 quit」标记。
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 服务端是否已启动（幂等保护，避免自测/重建窗口时起两条线程抢同一个管道）。
static SERVING: AtomicBool = AtomicBool::new(false);

/// 面板每帧调用一次：取走「收到过 quit」这件事（只返回一次 `true`）。
///
/// 与 [`serve`] 同一份进程内状态，所以 UI 线程读它不需要锁。
pub fn take_quit_request() -> bool {
    QUIT_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 是否已经起了服务端（自检与排障用）。
pub fn is_serving() -> bool {
    SERVING.load(Ordering::SeqCst)
}

/// 启动控制管道服务端（幂等）。
///
/// 返回 `true` 表示**已有**服务端在跑（含本次启动成功）；失败返回 `false` 并写日志。
/// 只在真实面板启动时调用（`--panel-selftest` 不调用：CI 并发跑多个自检会抢管道名）。
#[cfg(windows)]
pub fn serve(ctx: egui::Context, log: std::sync::Arc<LogBuffer>) -> bool {
    if SERVING.swap(true, Ordering::SeqCst) {
        return true;
    }
    let spawned = std::thread::Builder::new()
        .name("ariahost-ctl".to_owned())
        .spawn(move || platform::accept_loop(ctx, log));
    match spawned {
        Ok(_) => true,
        Err(error) => {
            SERVING.store(false, Ordering::SeqCst);
            log_on_start_failure(format!("控制管道线程起不来：{error}"));
            false
        }
    }
}

/// 非 Windows：没有命名管道，服务端不启动（客户端一路 `NoInstance`）。
#[cfg(not(windows))]
pub fn serve(_ctx: egui::Context, _log: std::sync::Arc<LogBuffer>) -> bool {
    false
}

/// 线程起不来时的兜底日志（没有 `LogBuffer` 可用时也不能 panic）。
#[cfg(windows)]
fn log_on_start_failure(message: String) {
    eprintln!("[ctl] {message}");
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;
    use std::time::Duration;

    use ariahost_common::paths::control_pipe_name;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, ReadFile, WriteFile, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    /// 管道实例数量上限：串行处理足够，这里只保证「第二个客户端不被拒绝」。
    const MAX_INSTANCES: u32 = PIPE_UNLIMITED_INSTANCES;
    /// 读/写缓冲（每条消息都是一行，4 KiB 足够）。
    const BUFFER_BYTES: u32 = 4096;

    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain([0]).collect()
    }

    /// 造安全描述符（失败返回 `None`，调用方退化为系统默认 DACL）。
    fn security_attributes() -> Option<*const SECURITY_ATTRIBUTES> {
        let mut sddl = wide(PIPE_SDDL);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_mut_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || descriptor.is_null() {
            return None;
        }
        // 故意泄漏到进程结束：`CreateNamedPipeW` 只读取它，之后释放会让描述符悬挂，
        // 而这是几十字节的进程级常量，不值得为它加一份引用计数。
        let attributes = Box::new(SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        });
        Some(Box::leak(attributes) as *const SECURITY_ATTRIBUTES)
    }

    /// 读一行（到 `\n` / EOF / 上限为止）。
    fn read_line(handle: HANDLE) -> String {
        let mut bytes: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1];
        while bytes.len() < MAX_REQUEST_BYTES {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    handle,
                    chunk.as_mut_ptr(),
                    1,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || read == 0 {
                break; // 对端关闭或出错：把已读到的当一条请求
            }
            if chunk[0] == b'\n' {
                break;
            }
            if chunk[0] != b'\r' {
                bytes.push(chunk[0]);
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// 写一行（写完 flush：管道是字节流，不刷会留在内核缓冲里）。
    fn write_line(handle: HANDLE, text: &str) -> bool {
        let bytes = text.as_bytes();
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return false;
        }
        unsafe { FlushFileBuffers(handle) != 0 }
    }

    pub fn accept_loop(ctx: egui::Context, log: Arc<LogBuffer>) {
        let name = wide(control_pipe_name());
        let attributes = security_attributes();
        match attributes {
            Some(_) => log.push(
                "控制管道：DACL 已应用（拒绝网络登录；允许 SYSTEM/管理员/所有者）".to_owned(),
            ),
            None => log.push(
                "控制管道：安全描述符构造失败，降级为系统默认 DACL（仅本机，同用户默认可连）"
                    .to_owned(),
            ),
        }
        let mut logged_ready = false;
        loop {
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    MAX_INSTANCES,
                    BUFFER_BYTES,
                    BUFFER_BYTES,
                    0,
                    attributes.unwrap_or(std::ptr::null()),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                let code = unsafe { GetLastError() };
                log.push(format!("控制管道：创建失败（Win32 {code}），1s 后重试"));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            if !logged_ready {
                logged_ready = true;
                log.push(format!("控制管道已就绪：{}", control_pipe_name()));
            }

            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if connected == 0 {
                let code = unsafe { GetLastError() };
                // ERROR_PIPE_CONNECTED = 客户端在 ConnectNamedPipe 之前就连上了，属正常竞争。
                if code != ERROR_PIPE_CONNECTED {
                    unsafe {
                        CloseHandle(handle);
                    }
                    continue;
                }
            }

            let line = read_line(handle);
            let request = parse_request(&line);
            match &request {
                Request::Quit => {
                    QUIT_REQUESTED.store(true, Ordering::SeqCst);
                    log.push(
                        "控制管道：收到 quit，已交给面板退出流程（保存会话 → 停 aria2c → 关窗）"
                            .to_owned(),
                    );
                    // egui 空闲时不发帧：不叫醒的话这条请求要等用户动鼠标才被看见。
                    ctx.request_repaint();
                }
                Request::Unknown(command) => {
                    log.push(format!("控制管道：未知指令 {command:?}，已拒绝"));
                }
            }
            let ack = ack_line(&request, std::process::id());
            if !write_line(handle, &ack) {
                log.push("控制管道：应答写不出去（客户端可能已断开）".to_owned());
            }
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_the_only_recognised_command() {
        assert_eq!(parse_request("quit"), Request::Quit);
        assert_eq!(parse_request("quit\n"), Request::Quit);
        assert_eq!(parse_request("  quit  "), Request::Quit);
        assert_eq!(
            parse_request("shutdown"),
            Request::Unknown("shutdown".to_owned())
        );
    }

    #[test]
    fn ack_carries_server_pid_for_quit() {
        assert_eq!(ack_line(&Request::Quit, 4321), "ok pid=4321\n");
    }

    #[test]
    fn unknown_command_is_refused_with_reason() {
        let ack = ack_line(&Request::Unknown("nope".to_owned()), 1);
        assert_eq!(ack, "refused unknown-command:nope\n");
    }

    #[test]
    fn ack_pid_round_trips() {
        let ack = ack_line(&Request::Quit, 987_654);
        assert_eq!(parse_ack_pid(&ack), Some(987_654));
    }

    #[test]
    fn parse_ack_pid_rejects_garbage() {
        assert_eq!(parse_ack_pid(""), None);
        assert_eq!(parse_ack_pid("ok"), None);
        assert_eq!(parse_ack_pid("ok pid=abc"), None);
        assert_eq!(parse_ack_pid("refused unknown-command:x"), None);
    }

    #[test]
    fn quit_flag_is_taken_exactly_once() {
        // 断言「取走」语义：面板处理完一次就不该再退出一次。
        QUIT_REQUESTED.store(true, Ordering::SeqCst);
        assert!(take_quit_request());
        assert!(!take_quit_request());
    }

    #[test]
    fn sddl_denies_network_and_allows_the_three_principals() {
        assert!(PIPE_SDDL.contains("(D;;GA;;;NU)"), "必须先拒绝网络登录");
        for principal in ["SY", "BA", "OW"] {
            assert!(
                PIPE_SDDL.contains(&format!("(A;;GA;;;{principal})")),
                "缺少允许项 {principal}"
            );
        }
    }
}
