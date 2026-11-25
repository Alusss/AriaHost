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

/// 仅取证用的「强制走哪条退路」开关（环境变量名）。
///
/// 真机 1/6 的「不出帧」是随机态，验收要求**可重复地**证明两条退路都走得通，所以留一个
/// 显式的注入点。取值：
/// - `fallback`：等价于「2s 内没等到出帧回执」→ 直接由 ctl 线程收尾（第一张网）；
/// - `close-swallowed`：帧路径照常接住并走完停止链，但**关窗不生效**（模拟系统吞掉
///   `ViewportCommand::Close`）→ 由第二张网收尾。
///
/// 它只改变**退出走哪条路**，不改任何业务状态；设了会写一条显眼的 WARN 日志，不会静默生效。
pub const SIMULATE_ENV: &str = "ARIAHOST_QUIT_SIMULATE";

/// 旧开关名（只认「设了就算数」的布尔语义，一律折算成 [`Simulate::Fallback`]）。
///
/// `task-59` 的取证脚本用它；合并成统一开关后仍然认，避免老脚本静默失效——
/// 静默失效最坏的结果是「以为在测 B，其实走的是 A」，读数全废还不报警。
/// 新脚本请用 [`SIMULATE_ENV`]。
pub const SIMULATE_ENV_LEGACY: &str = "ARIAHOST_QUIT_FALLBACK_SIMULATE";

/// 取证注入点的取值（`ARIAHOST_QUIT_SIMULATE` 的值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Simulate {
    /// 强制第一张网：跳过 2s 回执等待，直接由 ctl 线程收尾。
    Fallback,
    /// 强制第二张网：帧路径正常接住并走完停止链，但关窗不生效。
    CloseSwallowed,
}

impl Simulate {
    /// 字符串 → 取值（纯函数，便于单测）。
    ///
    /// 认不出的值返回 `None`（**不猜**）：取证开关拼错时宁可当没设，也不要悄悄走另一条路。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "fallback" => Some(Self::Fallback),
            "close-swallowed" => Some(Self::CloseSwallowed),
            _ => None,
        }
    }

    /// 反方向：用于日志里原样回显取值。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::CloseSwallowed => "close-swallowed",
        }
    }
}

/// 读环境变量得到注入点（没设 / 值认不出 → `None`）。
///
/// 兼容旧开关：`ARIAHOST_QUIT_FALLBACK_SIMULATE` 只要设了（任意非空值）就按
/// [`Simulate::Fallback`] 处理，与 `task-59` 脚本的语义一致。
pub fn simulate() -> Option<Simulate> {
    resolve(
        std::env::var(SIMULATE_ENV).ok().as_deref(),
        std::env::var(SIMULATE_ENV_LEGACY).ok().as_deref(),
    )
}

/// 两个环境变量取值 → 注入点（纯函数，便于单测：不碰进程环境）。
///
/// 优先级：新开关认得出来就用它；新开关设了但认不出，也仍然回落到旧开关
/// （取证时改错值不至于把整轮验证变成「什么都没开」而无人察觉）。
pub fn resolve(current: Option<&str>, legacy: Option<&str>) -> Option<Simulate> {
    if let Some(kind) = current.and_then(Simulate::parse) {
        return Some(kind);
    }
    match legacy {
        Some(raw) if !raw.trim().is_empty() => Some(Simulate::Fallback),
        _ => None,
    }
}

/// 当前是否强制「关窗被吞」（app 的帧路径据此截断 `Close`）。
pub fn simulating_close_swallowed() -> bool {
    simulate() == Some(Simulate::CloseSwallowed)
}

/// 客户端支持的指令（M4-b 起两条：退出 / 唤出）。
pub const QUIT: &str = "quit";
/// 请已有面板把窗口唤到前台（第二实例用；**幂等**，没有副作用）。
///
/// 为什么需要它：用户点 X 进托盘后，再双击开始菜单/桌面图标会起第二个进程。
/// 旧行为只是打印一句「已在运行」然后 exit 1 —— 屏幕上什么都不发生，
/// 用户看到的现象就是「双击没反应」（task-77 的真机缺陷）。有了这条指令，
/// 第二个进程能把已有面板叫到前台，然后正常退出。
pub const SHOW: &str = "show";
/// 单行请求上限：读到这里还没换行就按未知指令处理（防客户端半截请求把线程挂住）。
pub const MAX_REQUEST_BYTES: usize = 64;

/// 管道 DACL（SDDL）：先 deny 网络登录，再 allow SYSTEM / 管理员 / 所有者。
pub const PIPE_SDDL: &str = "D:(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

/// 解析后的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// 通知面板退出。
    Quit,
    /// 把已有面板的窗口唤到前台（第二实例）。
    Show,
    /// 不认识的指令（原样带回，便于排障）。
    Unknown(String),
}

/// 解析一行请求（纯函数，跨平台可测）。
pub fn parse_request(line: &str) -> Request {
    match line.trim() {
        QUIT => Request::Quit,
        SHOW => Request::Show,
        other => Request::Unknown(other.to_owned()),
    }
}

/// 构造应答行（纯函数）。`ok` 带服务端 pid，客户端据此等进程真的退出。
///
/// `show` 也回同样形状的 `ok pid=`：客户端只关心「服务端真的处理了这一条」，
/// 复用一个形状比再发明一种应答更不容易漂移。
pub fn ack_line(request: &Request, server_pid: u32) -> String {
    match request {
        Request::Quit | Request::Show => format!("ok pid={server_pid}\n"),
        Request::Unknown(command) => format!("refused unknown-command:{command}\n"),
    }
}

/// 从 `ok pid=1234` 这类应答里取 pid（客户端用；纯函数，跨平台可测）。
pub fn parse_ack_pid(ack: &str) -> Option<u32> {
    ack.split_whitespace()
        .find_map(|part| part.strip_prefix("pid="))
        .and_then(|value| value.trim().parse::<u32>().ok())
}

/// 第二实例「唤出已有面板」的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShowAttempt {
    /// 已有面板应答了 `ok`：窗口已被叫到前台。
    Awakened,
    /// 管道连上了、请求送到位，但服务端拒绝（或应答看不懂）。
    Refused(String),
    /// 没连上 / 连上了没有应答（超预算）。调用方据此写日志并给非零退出码。
    Unreachable(String),
}

/// 第二实例唤出的结果——**一行可抄进日志/报障**的说法（纯函数，跨平台可测）。
impl ShowAttempt {
    pub fn summary(&self) -> String {
        match self {
            Self::Awakened => "已唤出（服务端应答 ok）".to_owned(),
            Self::Refused(detail) => format!("被拒绝：{detail}"),
            Self::Unreachable(detail) => format!("未送达：{detail}"),
        }
    }

    /// 是否算成功（成败口径只在这里定义，调用方不要各自 `matches!`）。
    pub fn is_awakened(&self) -> bool {
        matches!(self, Self::Awakened)
    }
}

/// 第二实例等应答的预算。
///
/// 为什么是 1.5s：用户双击图标到窗口出现，超过 2s 的停顿就会被当成「卡死了」；
/// 服务端处理 `show` 只做三次 Win32 调用（微秒级），1.5s 全是余量。
pub const SHOW_BUDGET: Duration = Duration::from_millis(1500);

/// 把「读到的应答」判成结果（纯函数）。
///
/// 单独拆出来的原因：**超时**与**ack 缺失**这两条分支在真机上很难稳定复现
/// （要恰好赶上服务端在 `CreateNamedPipeW` 与 `ConnectNamedPipe` 之间），
/// 做成纯函数才能用单测把两条分支钉死。
pub fn classify_show_ack(ack: Option<&str>) -> ShowAttempt {
    match ack {
        None => ShowAttempt::Unreachable("没等到应答（连上了但服务端没回话）".to_owned()),
        Some(text) => {
            let text = text.trim();
            if text.is_empty() {
                return ShowAttempt::Unreachable("应答为空（对端可能已退出）".to_owned());
            }
            match parse_ack_pid(text) {
                Some(_) => ShowAttempt::Awakened,
                None => ShowAttempt::Refused(text.to_owned()),
            }
        }
    }
}

/// 客户端：请已有实例把面板唤到前台（第二实例的 `AlreadyRunning` 分支专用）。
///
/// 非 Windows：没有命名管道，直接报不可达（与 `quit` 客户端同一口径）。
#[cfg(not(windows))]
pub fn request_show(_budget: Duration) -> ShowAttempt {
    classify_show_ack(None)
}

/// 客户端：请已有实例把面板唤到前台（Windows：连命名管道 → 发一行 → 读应答）。
#[cfg(windows)]
pub fn request_show(budget: Duration) -> ShowAttempt {
    match platform::client_roundtrip(ariahost_common::paths::control_pipe_name(), SHOW, budget) {
        Ok(ack) => classify_show_ack(Some(&ack)),
        Err(detail) => ShowAttempt::Unreachable(detail),
    }
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
    use std::time::{Duration, Instant};

    use ariahost_common::paths::control_pipe_name;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, BOOL, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
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

    /// 客户端：连上管道 → 发一行 → 读一行应答（总耗时不超过 `budget`）。
    ///
    /// 返回 `Ok(应答行)` / `Err(没送达的原因)`。**没送达**与**送达但被拒**必须分开：
    /// 前者是「已有实例可能卡住了」，后者是「已有实例明确拒绝」，日志与排障口径不同。
    pub fn client_roundtrip(
        pipe_name: &str,
        command: &str,
        budget: Duration,
    ) -> Result<String, String> {
        let name = wide(pipe_name);
        let deadline = Instant::now() + budget;
        let handle = loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }
            let code = unsafe { GetLastError() };
            // ERROR_PIPE_BUSY：所有实例都被占用（服务端正忙着上一次请求）。等一会儿再试。
            if code != ERROR_PIPE_BUSY {
                return Err(format!("连不上控制管道（Win32 {code}）"));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("控制管道忙，等不到空闲实例".to_owned());
            }
            let wait_ms = (left.as_millis().min(50) as u32).max(1);
            unsafe { WaitNamedPipeW(name.as_ptr(), wait_ms) };
        };
        if !write_line(handle, &format!("{command}\n")) {
            unsafe { CloseHandle(handle) };
            return Err("请求写不进管道".to_owned());
        }
        // 读应答用「有界轮询」而不是直接 `ReadFile`：对端若已经卡死不出帧，
        // 阻塞读会让第二个进程也挂住 —— 那正好是这条指令要修的病。
        let mut ack = String::new();
        loop {
            let mut available = 0u32;
            let ok = unsafe {
                PeekNamedPipe(
                    handle,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                break;
            }
            if available > 0 {
                let mut buffer = [0u8; 128];
                let mut read = 0u32;
                let ok = unsafe {
                    ReadFile(
                        handle,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut read,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read == 0 {
                    break;
                }
                ack.push_str(&String::from_utf8_lossy(&buffer[..read as usize]));
                if ack.contains('\n') {
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { CloseHandle(handle) };
        if ack.is_empty() {
            Err("连上了但没有应答（对端可能已退出）".to_owned())
        } else {
            Ok(ack)
        }
    }

    /// `show` 的实体结果（只用于写日志：说明这次唤出**改变了什么**）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WakeOutcome {
        /// 唤出之前窗口是隐藏的（在托盘里）。
        pub was_hidden: bool,
        /// 唤出之前窗口是最小化的。
        pub was_minimized: bool,
    }

    /// 枚举回调的入参（只带回本进程的面板窗口句柄）。
    struct FindState {
        pid: u32,
        found: HWND,
    }

    /// `EnumWindows` 回调：只认「本进程 + 标题以产品名开头」的顶层窗口。
    ///
    /// 为什么要判标题：托盘也建了一个隐藏的顶层窗口（类名/标题是 `tray_icon_app`），
    /// 若只按 pid 找，可能把托盘消息窗口当成面板去 `ShowWindow`，用户仍然看不到面板。
    unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam as *mut FindState) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != state.pid {
            return 1; // 继续枚举
        }
        let mut title = [0u16; 160];
        let len = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        let len = len.max(0) as usize;
        let title = String::from_utf16_lossy(&title[..len]);
        if title.starts_with(ariahost_common::PRODUCT_NAME) {
            state.found = hwnd;
            return 0; // 找到了，停
        }
        1
    }

    /// 找本进程的面板窗口（找不到返回 `None`）。
    fn find_panel_window() -> Option<HWND> {
        let mut state = FindState {
            pid: std::process::id(),
            found: std::ptr::null_mut(),
        };
        unsafe {
            EnumWindows(
                Some(enum_windows_cb),
                &mut state as *mut FindState as LPARAM,
            );
        }
        if state.found.is_null() {
            None
        } else {
            Some(state.found)
        }
    }

    /// 把本进程的面板窗口叫到前台（`show` 指令的实体）。
    ///
    /// 为什么用 Win32 而不是 egui 的 `ViewportCommand::Visible(true)`：
    /// 1. 那条指令要等帧线程跑一帧才生效，而「第二实例唤出」恰恰要能对付
    ///    「帧线程被系统挂住/没被叫醒」的场面（`--quit` 的两张网就是同一课）；
    /// 2. 它改不了面板自己的 `window_hidden` 状态，本批不改 app.rs 的状态机。
    ///
    /// `ShowWindow(SW_SHOW)` 是幂等的：窗口本来就可见时不会闪烁。
    pub fn wake_panel() -> Result<WakeOutcome, String> {
        let hwnd = find_panel_window().ok_or_else(|| "找不到本进程的面板窗口".to_owned())?;
        let was_minimized = unsafe { IsIconic(hwnd) } != 0;
        let was_hidden = unsafe { IsWindowVisible(hwnd) } == 0;
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            if was_minimized {
                ShowWindow(hwnd, SW_RESTORE);
            }
            // 前台化是**尽力而为**：前台锁可能拒绝（返回 0），但窗口已可见，
            // 这不代表唤出失败，所以不当作错误、也不写进返回值。
            SetForegroundWindow(hwnd);
        }
        Ok(WakeOutcome {
            was_hidden,
            was_minimized,
        })
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
                Request::Show => {
                    // 幂等：本来可见/在前台时什么都不变，也不打扰用户。
                    match wake_panel() {
                        Ok(outcome) => {
                            let before = if outcome.was_hidden {
                                "原本在托盘里"
                            } else if outcome.was_minimized {
                                "原本最小化"
                            } else {
                                "原本已可见"
                            };
                            log.push(format!(
                                "控制管道：收到 show → 已把面板叫到前台（{before}）"
                            ));
                            // 帧线程若被挂住，这一步也顺手把它叫醒（尽力而为）。
                            ctx.request_repaint();
                        }
                        Err(detail) => log.push(format!(
                            "控制管道：收到 show，但唤不出窗口：{detail}（面板可能尚未建窗）"
                        )),
                    }
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
                let sim = simulate();
                let simulated = sim == Some(Simulate::Fallback);
                if let Some(kind) = sim {
                    log.push_level(
                        logfile::Level::Warn,
                        FALLBACK_TARGET,
                        format!(
                            "ctl.fallback: 取证开关 {SIMULATE_ENV}={} 已设置——只改变退出走哪条路（仅取证用）",
                            kind.as_str()
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
    fn simulate_parses_only_known_values() {
        // 取证开关：认得的两种取值各到位（task-62）。
        assert_eq!(Simulate::parse("fallback"), Some(Simulate::Fallback));
        assert_eq!(
            Simulate::parse("close-swallowed"),
            Some(Simulate::CloseSwallowed)
        );
        // 前后空白容忍（环境变量常被随手带空格）。
        assert_eq!(Simulate::parse("  fallback  "), Some(Simulate::Fallback));
        // 拼错不猜：当没设，宁可少走一条取证分支，也不要悄悄改变退出路径。
        for garbage in ["", "1", "true", "close_swallowed", "Fallback", "fallback2"] {
            assert_eq!(Simulate::parse(garbage), None, "不该认 {garbage:?}");
        }
        // 反方向回显与解析互为逆运算（日志里原样打这个值）。
        for kind in [Simulate::Fallback, Simulate::CloseSwallowed] {
            assert_eq!(Simulate::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn simulate_env_is_unset_in_tests() {
        // 单测里不允许被取证开关影响：若外部真设了，这里会失败，提醒把它去掉再跑。
        assert_eq!(simulate(), None);
        assert!(!simulating_close_swallowed());
    }

    #[test]
    fn resolve_prefers_new_switch_and_keeps_legacy_working() {
        // 新开关认得出来 → 用它。
        assert_eq!(
            resolve(Some("close-swallowed"), None),
            Some(Simulate::CloseSwallowed)
        );
        assert_eq!(resolve(Some("fallback"), None), Some(Simulate::Fallback));
        // 旧开关只要非空 → Fallback（老脚本语义：设了就算数）。
        assert_eq!(resolve(None, Some("1")), Some(Simulate::Fallback));
        assert_eq!(resolve(None, Some("true")), Some(Simulate::Fallback));
        // 新开关设了但认不出 → 仍回落到旧开关，不让整轮取证悄悄变成「什么都没开」。
        assert_eq!(
            resolve(Some("close_swallowed"), Some("1")),
            Some(Simulate::Fallback)
        );
        // 都空 / 都认不出 → 不生效。
        assert_eq!(resolve(None, None), None);
        assert_eq!(resolve(Some("typo"), None), None);
        assert_eq!(resolve(Some("typo"), Some("  ")), None);
    }

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
    fn two_commands_are_recognised_and_nothing_else() {
        assert_eq!(parse_request("quit"), Request::Quit);
        assert_eq!(parse_request("quit\n"), Request::Quit);
        assert_eq!(parse_request("  quit  "), Request::Quit);
        // M4-b：第二实例唤出已有面板。
        assert_eq!(parse_request("show"), Request::Show);
        assert_eq!(parse_request("show\r\n"), Request::Show);
        assert_eq!(
            parse_request("shutdown"),
            Request::Unknown("shutdown".to_owned())
        );
    }

    #[test]
    fn ack_carries_server_pid_for_quit_and_show() {
        assert_eq!(ack_line(&Request::Quit, 4321), "ok pid=4321\n");
        // 同一个应答形状，客户端只关心「服务端真的处理了」。
        assert_eq!(ack_line(&Request::Show, 4321), "ok pid=4321\n");
    }

    #[test]
    fn show_ack_is_classified_into_three_honest_outcomes() {
        // 正常：服务端应答 ok。
        assert_eq!(
            classify_show_ack(Some("ok pid=42\n")),
            ShowAttempt::Awakened
        );
        // 送达但被拒：用于排障，原文要带回去。
        assert_eq!(
            classify_show_ack(Some("refused unknown-command:show\n")),
            ShowAttempt::Refused("refused unknown-command:show".to_owned())
        );
        // 没应答 / 空应答：两种都得是 Unreachable，不能当成成功。
        assert!(matches!(
            classify_show_ack(None),
            ShowAttempt::Unreachable(_)
        ));
        assert!(matches!(
            classify_show_ack(Some("   \n")),
            ShowAttempt::Unreachable(_)
        ));
    }

    /// 判据 B 的口径：第二实例的预算必须**短于**用户感觉「卡住」的阈值。
    #[test]
    fn show_budget_stays_below_the_ux_threshold() {
        assert!(SHOW_BUDGET <= Duration::from_millis(2000));
        assert!(SHOW_BUDGET >= Duration::from_millis(500));
    }

    #[test]
    fn show_attempt_summary_and_success_flag_agree_with_the_variant() {
        let ok = ShowAttempt::Awakened;
        assert!(ok.is_awakened());
        assert!(ok.summary().contains("已唤出"));

        let refused = ShowAttempt::Refused("refused unknown-command:show".to_owned());
        assert!(!refused.is_awakened());
        // 失败一定要把原文带出来，否则用户报障时无从下手。
        assert!(refused.summary().contains("refused unknown-command:show"));

        let unreachable = ShowAttempt::Unreachable("2s 内没有应答".to_owned());
        assert!(!unreachable.is_awakened());
        assert!(unreachable.summary().contains("2s 内没有应答"));
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
