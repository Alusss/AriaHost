//! AriaHost 入口（架构 §4.1.4）。
//!
//! **面板入口自 M2 起真的起窗**（`crates/ariahost-ui`：eframe/egui + wgpu，只做技术闸门切片，
//! 不接服务/RPC/任务列表）。`--service` 与三个短命助手仍是桩。
//!
//! 分派顺序硬约束：先解析 argv，再决定是否进入单实例锁；短命助手不参与单实例。
//! 面板的单实例锁在 `ariahost_ui::run_panel` 内部获取（`%LOCALAPPDATA%\AriaHost\panel.lock`）。
//!
//! **子系统**：Windows 上是 GUI 子系统（双击不弹黑窗）。代价是 stdout 默认没接控制台，
//! 所以 CLI 分支在输出前按需附着父控制台——见 [`console`]。Cargo 的 `[[bin]]` 没有
//! 「按构建 profile 选子系统」的能力，只能在 crate 根下这个属性，两条路径靠代码弥合。

// GUI 子系统：Windows 双击 exe 不再先弹一个控制台黑窗（本批 task-48 的起因）。
// 非 Windows 一律不加（那是 Windows-only 属性）。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod aumid;
mod cli;
mod console;

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::process::exit(cli::run(&args));
}
