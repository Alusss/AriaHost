//! AriaHost 入口（架构 §4.1.4）。
//!
//! **面板入口待 M2**：本批只做 argv 解析与子命令分派。
//! 不建窗口、不初始化 wgpu、不连 RPC、不注册服务、不创建单实例 mutex。
//!
//! 分派顺序硬约束：先解析 argv，再决定是否进入单实例互斥；短命助手不参与 mutex。
//! 本批所有业务分支都是桩，只保证分支、错误码、退出码与 CLI 契约一致。

mod cli;

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    std::process::exit(cli::run(&args));
}
