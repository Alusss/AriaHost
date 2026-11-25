//! `cargo run -p ariahost-web --example web_selftest`
//!
//! 起真实的网页管理页（读 `config.toml` 的 `web.port` / `aria2.rpc.port`），
//! 逐项自检并把 `SELFTEST ...` 行打到 stdout，然后继续提供服务（Ctrl+C 退出）。
//!
//! 等价于将来的 `ariahost --web-selftest`（CLI 由 Lead 接；本批不许改 `cli.rs`）。

fn main() {
    println!("web_selftest：{:?}", ariahost_web::selftest());
}
