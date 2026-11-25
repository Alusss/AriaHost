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
//! ## 收到 quit 之后做什么（task-59：两条路，绝不依赖 UI 出帧）
//!
//! 1. 置位 [`take_quit_request`] 的标记 + `ctx.request_repaint()` 叫醒面板；
//!    立刻回 `ok pid=` 并放开管道（客户端的 10s 预算从发出请求起算，不能被我们的等待吃掉）。
//! 2. **等 UI 回执**（[`mark_quit_consumed`] 由 `draw()` 在消费后置位）最多 [`CONSUME_WAIT`]。
//!    - 等到 → 帧路径接手，行为与以前完全一致（`app.rs` 里 `begin_quit(ctx, false)`）。
//!    - 等不到 → **本线程自己收尾**（[`fallback_quit`]）：请托管线程优雅停一次
//!      （复用同一条停止链：保存会话 → `aria2.shutdown` → 三段式），最多等 [`FALLBACK_STOP_WAIT`]，
//!      然后 `process::exit(0)`。aria2c 还有 Job Object（`KILL_ON_JOB_CLOSE`）兜底，不留孤儿。
//!
//! 为什么必须有第二条路：`ctx.request_repaint()` 从非 UI 线程发起**不保证一定出帧**
//! （托盘隐藏 / 窗口被遮挡 / 空闲定时器没排上），而 `take_quit_request` 只在 `draw()` 里被调用。
//! 真机实测 1/6 次「收到 quit 之后 50 秒没有任何消费行」，安装器的 `--quit` 于是拿到 `timeout`
//! → 覆盖升级中止并报「仍在运行」。**不做**「直接 exit 不保存」那种更省事的写法：那会丢掉未完成会话。
//!
//! 两条路都写文件日志且 target 不同：帧路径是 `panel`（`走退出档`），兜底是 `ctl.fallback`
//! （`开始收尾` / `停止完成` / `没有可用的托管线程`），排障时看 target 就能判断走了哪条。
//!
//! **故意不在这里 `ViewportCommand::Close`**：那会被 `ui.close_action` 的三档拦下
//! （tray 档会变成「藏进托盘」、ask 档会弹对话框），而卸载要的是「真退」。
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
use std::time::Duration;

#[cfg(windows)]
use ariahost_common::logfile;

use crate::logbuf::LogBuffer;

/// 兜底路径的日志 target：文件日志里必须能一眼区分「帧路径」与「兜底路径」。
pub const FALLBACK_TARGET: &str = "ctl.fallback";
/// ctl 线程等 UI「已消费 quit」回执的上限；超时即走兜底（task-59）。
///
/// 为什么是 2s：正常帧路径的实测是 100–240ms（含一次 repaint 唤醒），2s 已留 8 倍余量；
/// 而安装器的 `--quit` 预算是 10s，兜底（停止链实测 4.1–4.3s）必须留得下。
pub const CONSUME_WAIT: Duration = Duration::from_secs(2);
/// 兜底路径等托管线程把停止做完的上限。
///
/// `CoreRuntime::stop` 自己分三段（优雅 → force → 强杀），实测 4.1–4.3s；15s 是它的兜底上限，
/// 与 `core_host::STOP_EXPECTED_SECS` 的口径一致——**不缩短**，缩短会在慢盘上丢掉会话保存。
pub const FALLBACK_STOP_WAIT: Duration = Duration::from_secs(15);

/// 仅取证用的强制兜底开关（环境变量名）。
///
/// 真机 1/6 的「不出帧」是随机态，验收要求**可重复地**证明兜底路径走得通，所以留一个
/// 显式的注入点：设了它，ctl 线程就当作「回执没等到」，直接走兜底。
/// 它只改变**退出走哪条路**，不改任何业务状态；设了会写一条显眼的 WARN 日志，不会静默生效。
pub const SIMULATE_FALLBACK_ENV: &str = "ARIAHOST_QUIT_FALLBACK_SIMULATE";

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
/// 「UI 已消费掉这次 quit」的回执位（task-59）。
///
/// 存在的理由：`ctx.request_repaint()` 从非 UI 线程发起**不保证一定出帧**
/// （托盘隐藏 / 窗口被遮挡 / 空闲定时器没被排上），而 `take_quit_request` 只在 `draw()` 里被调用。
/// 一旦那一帧没发生，标志就永远躺着、进程永不退出（真机实测 1/6 复现，日志可证：
/// 「收到 quit，已交给面板退出流程」之后 50 秒没有消费行）。
/// 所以：ctl 线程置位后**等这个回执**，等不到就自己收尾——退出路径不再以「一定会出帧」为前提。
static QUIT_CONSUMED: AtomicBool = AtomicBool::new(false);

/// 面板每帧调用一次：取走「收到过 quit」这件事（只返回一次 `true`）。
///
/// 与 [`serve`] 同一份进程内状态，所以 UI 线程读它不需要锁。
pub fn take_quit_request() -> bool {
    QUIT_REQUESTED.swap(false, Ordering::SeqCst)
}

/// UI 消费掉 quit 后调用（`app.rs::draw` 里紧跟 [`take_quit_request`]）：给 ctl 线程回执。
pub fn mark_quit_consumed() {
    QUIT_CONSUMED.store(true, Ordering::SeqCst);
}

/// 等 UI 回执，最多 `timeout`。`true` = 帧路径已接手，ctl 线程不必兜底。
///
/// 用固定小步轮询而不是条件变量：ctl 线程本来就在串行 accept 里阻塞，20ms 轮询既不耗 CPU，
/// 也不引入新的同步原语——这条路径只在真的来了 `--quit` 时才会走到。
pub fn wait_quit_consumed(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if QUIT_CONSUMED.load(Ordering::SeqCst) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 「帧路径已经发出关窗」的回执位（task-59 第二张网）。
///
/// 第一版只覆盖「2s 内没人消费」。真机矩阵抓到了更靠后的一种卡法：
/// 帧路径**接住了** quit、也把 aria2 停干净了，然后 UI 不再出帧
/// （窗口被遮挡 / 系统吞掉 `ViewportCommand::Close`），进程于是永远停在「已停止」。
/// 所以 ctl 线程还要确认「关窗真的发出去了」，否则照样自己收尾。
static QUIT_DONE: AtomicBool = AtomicBool::new(false);

/// 帧路径发出关窗后的宽限：这段时间里进程该自己退掉了（关窗到进程消失是毫秒级）。
pub const FRAME_EXIT_WAIT: Duration = Duration::from_secs(2);
/// 等「帧路径已发出关窗」的上限。要够长到覆盖最慢的停止链（实测 4.1–5.0s），
/// 又要短到让客户端 10s 预算里还剩余量——[`FRAME_EXIT_WAIT`] 之后再退也在预算内。
pub const FRAME_CLOSE_GRACE: Duration = Duration::from_secs(6);

/// 帧路径发出关窗后调用（`app.rs::pump_quit` 里紧跟在 `send_viewport_cmd(Close)` 之后）。
pub fn mark_quit_done() {
    QUIT_DONE.store(true, Ordering::SeqCst);
}

/// 等「帧路径已发出关窗」，最多 `timeout`。`true` = 帧路径走到了关窗那一步。
pub fn wait_quit_done(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if QUIT_DONE.load(Ordering::SeqCst) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 兜底收尾（task-59）：ctl 线程自己把会话存好、把 aria2 停掉，然后退出。
///
/// **不做**「直接 exit 不保存」：那会把未完成会话丢掉。若根本没有托管的运行时
/// （面板没起过 aria2 / 线程没起来 / 自测），没有会话要存，直接退即可。
///
/// 退出码恒为 0：对客户端来说「进程消失了」就是成功；真正的失败信息在日志里，
/// 客户端契约（成功 0 / 拒绝 1 / 超时 2）不因兜底而变。
#[cfg(windows)]
fn fallback_quit(log: &std::sync::Arc<LogBuffer>) {
    log.push_level(
        logfile::Level::Warn,
        FALLBACK_TARGET,
        "ctl.fallback: 开始收尾（保存会话 → aria2.shutdown → exit 0）".to_owned(),
    );
    if !crate::core_host::request_stop_without_ui() {
        log.push_level(
            logfile::Level::Warn,
            FALLBACK_TARGET,
            "ctl.fallback: 没有可用的托管线程（aria2 未托管）→ 直接退出，无会话需保存".to_owned(),
        );
        std::process::exit(0);
    }
    log.push_level(
        logfile::Level::Info,
        FALLBACK_TARGET,
        format!("ctl.fallback: 已请求托管线程优雅停止，最多等 {FALLBACK_STOP_WAIT:?}"),
    );
    if crate::core_host::wait_stop_done(FALLBACK_STOP_WAIT) {
        log.push_level(
            logfile::Level::Info,
            FALLBACK_TARGET,
            "ctl.fallback: 停止完成（会话已保存、aria2.shutdown 已发）→ exit 0".to_owned(),
        );
    } else {
        log.push_level(
            logfile::Level::Warn,
            FALLBACK_TARGET,
            "ctl.fallback: 等满预算仍未停完 → exit 0（aria2c 由 Job Object KILL_ON_JOB_CLOSE 兜底）"
                .to_owned(),
        );
    }
    std::process::exit(0);
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
            let mut quit = false;
            match &request {
                Request::Quit => {
                    quit = true;
                    QUIT_CONSUMED.store(false, Ordering::SeqCst);
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
            // 先回答、先放开管道：客户端的 10s 预算是从发出请求起算的，
            // 不能让它把 2s 的回执等待也算进去。
            let ack = ack_line(&request, std::process::id());
            if !write_line(handle, &ack) {
                log.push("控制管道：应答写不出去（客户端可能已断开）".to_owned());
            }
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
            // task-59：等 UI 回执。等到 = 帧路径接手（行为与以前一致）；
            // 等不到 = 那一帧没发生（实测 1/6），由 ctl 线程自己收尾，绝不把退出挂在出帧上。
            if quit {
                let simulated = std::env::var_os(SIMULATE_FALLBACK_ENV).is_some();
                if simulated {
                    log.push_level(
                        logfile::Level::Warn,
                        FALLBACK_TARGET,
                        format!(
                            "ctl.fallback: 环境变量 {SIMULATE_FALLBACK_ENV} 已设置——强制走兜底（仅取证用）"
                        ),
                    );
                }
                if !simulated && wait_quit_consumed(CONSUME_WAIT) {
                    log.push_level(
                        logfile::Level::Info,
                        FALLBACK_TARGET,
                        format!("ctl.fallback: 未启用（帧路径已在 {CONSUME_WAIT:?} 内接手）"),
                    );
                    // 第二张网（task-59 真机矩阵补的）：接手 ≠ 会退出。
                    // 实测下载态里帧路径把 aria2 停干净后就再没出过帧（窗口被遮挡 /
                    // Close 被系统吞掉），进程永远停在「已停止」，客户端拿到 `timeout`。
                    if wait_quit_done(FRAME_CLOSE_GRACE) {
                        log.push_level(
                            logfile::Level::Info,
                            FALLBACK_TARGET,
                            format!(
                                "ctl.fallback: 帧路径已发出关窗，宽限 {FRAME_EXIT_WAIT:?} 等进程自己退"
                            ),
                        );
                        std::thread::sleep(FRAME_EXIT_WAIT);
                        // 走到这里说明关窗没生效（正常关窗时进程早没了，下面的代码根本不会执行）。
                        log.push_level(
                            logfile::Level::Warn,
                            FALLBACK_TARGET,
                            format!(
                                "ctl.fallback: 关窗后 {FRAME_EXIT_WAIT:?} 进程仍未退出 → 直接 exit 0"
                            ),
                        );
                        std::process::exit(0);
                    }
                    log.push_level(
                        logfile::Level::Warn,
                        FALLBACK_TARGET,
                        format!(
                            "ctl.fallback: 帧路径接住了但 {FRAME_CLOSE_GRACE:?} 内没发出关窗 → 由 ctl 线程收尾"
                        ),
                    );
                } else if !simulated {
                    log.push_level(
                        logfile::Level::Warn,
                        FALLBACK_TARGET,
                        format!("ctl.fallback: {CONSUME_WAIT:?} 内没有出帧回执，改由 ctl 线程收尾"),
                    );
                }
                fallback_quit(&log);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_quit_done_is_observable_by_wait() {
        // 轮询回执位：ctl 第二张网靠它判断「帧路径有没有真的发出关窗」。
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            mark_quit_done();
        });
        assert!(wait_quit_done(Duration::from_secs(2)));
    }

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
