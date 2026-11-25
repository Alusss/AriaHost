//! AriaHost 入口（架构 §4.1.4）。
//!
//! **面板入口自 M2 起真的起窗**（`crates/ariahost-ui`：eframe/egui + wgpu，只做技术闸门切片，
//! 不接服务/RPC/任务列表）。`--service` 与三个短命助手仍是桩。
//!
//! 分派顺序硬约束：先解析 argv，再决定是否进入单实例锁；短命助手不参与单实例。
//! 面板的单实例锁在 `ariahost_ui::run_panel` 内部获取（`%LOCALAPPDATA%\AriaHost\panel.lock`）。

mod aumid;
mod cli;

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::process::exit(cli::run(&args));
}
