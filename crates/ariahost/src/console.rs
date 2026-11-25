//! GUI 子系统下的「按需控制台」（架构 §4.1.4）。
//!
//! ## 为什么需要这个模块
//!
//! `main.rs` 加了 `#![cfg_attr(windows, windows_subsystem = "windows")]`（**GUI 子系统**）之后，
//! 双击 exe **不再弹黑窗**——但代价是 Windows 不再给进程接任何控制台，`println!` 的输出
//! 会**静默丢弃**（不报错、不 panic、不影响退出码）。而 CLI 必须让人看得见：
//! `--service-status` 要吐 JSON，`--install-service` 要吐 `AH-E` 码。
//!
//! 所以分两步走，顺序不能颠倒：
//!
//! 1. **先看现有 stdout 能不能用**（[`probe_stdout`]）。`> out.txt`、`| more`、CI 捕获这些
//!    情形句柄**本来就有效**，这时一个字节都别动——重定向优先，绝不能为了「接控制台」
//!    把输出抢走（那正是最常见的翻车方式）。
//! 2. 只有 stdout 无效时，才 [`AttachConsole`] 附着到**父进程已有的**控制台（cmd/PowerShell
//!    窗口）。附着之后 `GetStdHandle(STD_OUTPUT_HANDLE)` 仍返回坏句柄，必须用 `CONOUT$`
//!    重开并 `SetStdHandle` 换掉，否则输出照样丢。
//!
//! **绝不新开控制台**：附着失败（Explorer 双击启动）就安静地什么都不做。
//! `--service` 由 SCM 在 Session 0 拉起，那里连父控制台都不存在，所以**连探测都跳过**
//! （见 [`cli::run`] 的分派）。
//!
//! ## 面板为什么不附着
//!
//! 面板是 GUI 常驻程序，附着到用户的控制台会带来「关掉 cmd 窗口 → `CTRL_CLOSE_EVENT`
//! → 面板被连带杀掉」的老问题（正是本批要修的用户抱怨之一）。所以面板**不调用**这里。
//!
//! ## 排障开关
//!
//! 设 `ARIAHOST_CONSOLE_DEBUG=1` 会把探测/附着结果打到 **stderr**（一行）：
//!
//! ```text
//! [console] stdout=pipe attached=false win32=0
//! ```
//!
//! 默认不打印：`--service-status` 的 stdout 是稳定 JSON 契约，不能被诊断行污染。

// 非 Windows 目标上这套「探测 + 附着」只有单测会用到（Linux 没有「stdout 不存在」这回事），
// 生产路径不会构造 `Pipe`/`File`/`Missing`，也不会有 `ConsolePlan` 之外的决策。
// 与其把这些类型塞进 `#[cfg(test)]`（那样 Windows 侧的单测反而测不到同一份定义），
// 不如让 lint 在非 Windows 目标上闭嘴——定义与判据必须**跨平台同一份**。
#![cfg_attr(not(windows), allow(dead_code))]

/// 现有 stdout 指向什么（只看句柄类型，不看内容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutKind {
    /// 真实控制台（`FILE_TYPE_CHAR`）。
    Console,
    /// 管道：`| more`、CI 捕获、被父进程接走的匿名管道。
    Pipe,
    /// 磁盘文件：`> out.txt`。
    File,
    /// 没有可用句柄：GUI 子系统 + 双击启动 / 句柄是坏的。
    Missing,
}

impl StdoutKind {
    /// 稳定字符串（日志与排障用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Pipe => "pipe",
            Self::File => "file",
            Self::Missing => "none",
        }
    }
}

/// 拿到 stdout 之前要不要去附着父控制台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolePlan {
    /// 保持现状：句柄本来就有效（重定向 / 已有控制台），一个字节都别动。
    Keep,
    /// 附着父控制台，然后用 `CONOUT$` 重开 stdout/stderr。
    AttachParent,
}

/// **纯决策**：给定 stdout 现状，决定要不要附着。
///
/// 单独抽出来是为了能在 Linux/CI 上单测——真正的 Win32 调用在 Windows 上，
/// 而在 Linux 上根本不存在「没有 stdout」这回事。
pub const fn plan(kind: StdoutKind) -> ConsolePlan {
    match kind {
        StdoutKind::Console | StdoutKind::Pipe | StdoutKind::File => ConsolePlan::Keep,
        StdoutKind::Missing => ConsolePlan::AttachParent,
    }
}

/// 探测与附着的结果（只用于排障与测试，不参与业务判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConsoleOutcome {
    /// 探测到的 stdout 类型。
    pub kind: Option<StdoutKind>,
    /// 是否成功附着到父控制台。
    pub attached: bool,
    /// 附着失败时的 Win32 错误码（成功/未尝试为 0）。
    pub last_error: u32,
}

impl ConsoleOutcome {
    /// 稳定字符串（`console` / `pipe` / `file` / `attached` / `none`）。
    pub const fn summary(self) -> &'static str {
        if self.attached {
            return "attached";
        }
        match self.kind {
            Some(kind) => kind.as_str(),
            None => "unknown",
        }
    }
}

/// 按需为 CLI 输出准备好 stdout/stderr。
///
/// 幂等：重复调用只会重复探测（`Pipe`/`File`/`Console` 一律 `Keep`）。
/// 绝不 panic、绝不新开窗口：这里是**失败即降级**的路径。
#[cfg(windows)]
pub fn ensure_for_cli_output() -> ConsoleOutcome {
    let kind = probe_stdout();
    let outcome = match plan(kind) {
        ConsolePlan::Keep => ConsoleOutcome {
            kind: Some(kind),
            attached: false,
            last_error: 0,
        },
        ConsolePlan::AttachParent => attach_parent(kind),
    };
    report(outcome);
    outcome
}

/// 非 Windows：stdout 天然可用（控制台/管道/文件之一），什么都不用做。
#[cfg(not(windows))]
pub fn ensure_for_cli_output() -> ConsoleOutcome {
    let outcome = ConsoleOutcome {
        kind: Some(StdoutKind::Console),
        attached: false,
        last_error: 0,
    };
    report(outcome);
    outcome
}

/// 排障开关：`ARIAHOST_CONSOLE_DEBUG=1` 时把结果打到 stderr。
fn report(outcome: ConsoleOutcome) {
    if std::env::var_os("ARIAHOST_CONSOLE_DEBUG").is_some() {
        eprintln!(
            "[console] stdout={} attached={} win32={}",
            outcome.summary(),
            outcome.attached,
            outcome.last_error
        );
    }
}

/// 读 `GetStdHandle(STD_OUTPUT_HANDLE)` 并判类型。
#[cfg(windows)]
fn probe_stdout() -> StdoutKind {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileType, FILE_TYPE_CHAR, FILE_TYPE_DISK, FILE_TYPE_PIPE,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_OUTPUT_HANDLE};

    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return StdoutKind::Missing;
    }
    match unsafe { GetFileType(handle) } {
        FILE_TYPE_CHAR => StdoutKind::Console,
        FILE_TYPE_PIPE => StdoutKind::Pipe,
        FILE_TYPE_DISK => StdoutKind::File,
        // `FILE_TYPE_UNKNOWN`（0）：句柄在但读不出类型，当作不可用。
        _ => StdoutKind::Missing,
    }
}

/// 附着父控制台，并把 stdout/stderr 换成 `CONOUT$` 的句柄。
///
/// 为什么必须重开：`AttachConsole` 只把进程挂到父控制台上，**不会**修好已有的
/// std 句柄（GUI 子系统下它们是空/坏的）。`CONOUT$` 是当前控制台的输出设备名，
/// 打开它拿到的句柄才是能写的控制台句柄，再 `SetStdHandle` 换进去，
/// 之后 `println!` 才会走到控制台。
#[cfg(windows)]
fn attach_parent(kind: StdoutKind) -> ConsoleOutcome {
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        let code = unsafe { GetLastError() };
        // 双击启动的正常路径：没有父控制台（ERROR_INVALID_HANDLE=6）。
        // 什么都不做——**绝不** AllocConsole，那正是我们要消灭的黑窗。
        return ConsoleOutcome {
            kind: Some(kind),
            attached: false,
            last_error: code,
        };
    }

    // 打开两次：stdout 与 stderr 各一个句柄（同一设备，句柄要各自持有）。
    for (name, std_handle) in [
        ("CONOUT$", STD_OUTPUT_HANDLE),
        ("CONOUT$", STD_ERROR_HANDLE),
    ] {
        match std::fs::OpenOptions::new().write(true).open(name) {
            Ok(file) => {
                use std::os::windows::io::AsRawHandle;
                let handle = file.as_raw_handle() as HANDLE;
                // 句柄要活到进程结束：`SetStdHandle` 只存值，不接管所有权。
                std::mem::forget(file);
                unsafe {
                    SetStdHandle(std_handle, handle);
                }
            }
            Err(_) => {
                // 控制台在但没有 CONOUT$（极少见）：保持原样，输出仍可能丢失，
                // 但这不是崩溃路径——CLI 的退出码比输出更重要。
            }
        }
    }

    ConsoleOutcome {
        kind: Some(StdoutKind::Console),
        attached: true,
        last_error: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_wins_over_attach() {
        // 重定向到文件/管道时**必须** Keep：这正是「不许因为 attach 失败丢输出」的判据。
        assert_eq!(plan(StdoutKind::File), ConsolePlan::Keep);
        assert_eq!(plan(StdoutKind::Pipe), ConsolePlan::Keep);
    }

    #[test]
    fn existing_console_is_kept() {
        assert_eq!(plan(StdoutKind::Console), ConsolePlan::Keep);
    }

    #[test]
    fn only_missing_stdout_triggers_attach() {
        assert_eq!(plan(StdoutKind::Missing), ConsolePlan::AttachParent);
        // 反向断言：没有任何「其他类型」也去 attach（那会把重定向输出抢走）。
        for kind in [StdoutKind::Console, StdoutKind::Pipe, StdoutKind::File] {
            assert_ne!(plan(kind), ConsolePlan::AttachParent);
        }
    }

    #[test]
    fn stdout_kind_strings_are_pinned() {
        assert_eq!(StdoutKind::Console.as_str(), "console");
        assert_eq!(StdoutKind::Pipe.as_str(), "pipe");
        assert_eq!(StdoutKind::File.as_str(), "file");
        assert_eq!(StdoutKind::Missing.as_str(), "none");
    }

    #[test]
    fn outcome_summary_is_stable() {
        assert_eq!(ConsoleOutcome::default().summary(), "unknown");
        assert_eq!(
            ConsoleOutcome {
                kind: Some(StdoutKind::Pipe),
                attached: false,
                last_error: 0
            }
            .summary(),
            "pipe"
        );
        assert_eq!(
            ConsoleOutcome {
                kind: Some(StdoutKind::Console),
                attached: true,
                last_error: 0
            }
            .summary(),
            "attached",
            "附着成功要能一眼看出走的是控制台而不是继承来的句柄"
        );
    }

    #[test]
    fn ensure_is_safe_to_call_on_any_platform() {
        // 本机（CI 是 Linux）必须有可用的 stdout，且不该尝试 attach。
        let outcome = ensure_for_cli_output();
        assert!(!outcome.attached);
        assert_eq!(outcome.last_error, 0);
    }
}
