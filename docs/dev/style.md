# 代码风格约定

> 适用：M1 起。当前无可执行代码，本文是**约定**；CI 的 `fmt` / `clippy` 两道门禁是强制项，其余部分是评审关注点。
> 基线：rustfmt 默认风格 + clippy 默认 lint（`-D warnings`）。只有下文明确列出的地方才偏离默认。

## 1. 自动化管到哪

| 规则 | 工具 | 是否阻断 |
|---|---|---|
| 缩进/换行/导入分组 | `cargo fmt --all -- --check` | 阻断 |
| clippy 默认 + `-D warnings` | `cargo clippy --workspace --all-targets -- -D warnings` | 阻断 |
| 命名/文档注释/模块划分 | 人工评审 | 评审意见 |

提交前固定动作：

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

## 2. rustfmt（`rustfmt.toml`，只用 stable 支持的键）

```toml
edition = "2021"
max_width = 100
use_field_init_shorthand = true
use_try_shorthand = true
```

不要引入需要 nightly 的键（`imports_granularity`、`group_imports` 等）——CI 用 stable 工具链，加了会直接报错。

## 3. 命名

| 对象 | 约定 | 例 |
|---|---|---|
| crate | `ariahost-` 前缀 + 单词 | `ariahost-aria2`、`ariahost-svc` |
| 模块/文件 | `snake_case`，按职责而非技术分层 | `process.rs`、`rpc.rs`、`versions.rs` |
| 类型/trait | `UpperCamelCase` | `Aria2Process`、`VersionStore` |
| 函数/变量 | `snake_case` | `spawn_aria2()` |
| 常量 | `SCREAMING_SNAKE_CASE` | `DEFAULT_RPC_PORT: u16 = 6800` |
| 错误码 | `AH-E####` 字面量集中在 `ariahost-common::errors` | `AH-E5001` |
| 用户可见文案 | 中文，集中在文案模块，不散落在逻辑里 | `MSG-U04` |

内部标识（结构体字段、日志 key、配置项）一律英文；**面向用户的一切文字必须是中文**（验收 #10）。

## 4. 错误处理

- 库 crate：`thiserror` 定义具体错误枚举，`?` 传播；不用 `anyhow`。
- 应用层（`ariahost-ui`、`ariahost-svc`）与跨层边界：允许 `anyhow::Result`，但**必须带上下文**（`.context("启动 aria2 失败")`）。
- 可失败路径禁止 `unwrap()` / `expect()`：仅「逻辑上不可能失败」（如 `Mutex` 中毒后重建、常量解析）可例外，并在紧邻处写一行注释说明为什么不可能失败。
- 禁止 `panic!` 作为错误出口；用户可见失败的唯一出口是统一错误模型 → 映射到 `AH-E####` 或 `MSG-U##`。
- 不允许把 aria2 的原始 stderr 直接弹给用户（验收 #10）：必须经错误模型翻译，原文只进日志。
- 新增错误码：先在 `ariahost-common::errors` 占用段位，再在 PR 描述里列出「码 / 触发条件 / 用户文案」，由架构负责人在定版文档登记。

## 5. 日志

```rust
tracing::info!(pid = child.id(), "aria2 已启动");
```

- 用 `tracing`，不用 `println!`/`eprintln!`（GUI 子系统下 stdout 不可见）。
- 级别口径：`error` 用户可见失败、`warn` 自愈/降级、`info` 生命周期事件、`debug` 状态迁移与 RPC 概要、`trace` 逐包/逐行。
- **敏感值永不落日志**：RPC secret/token、完整 URL 的 token 段、用户代理凭据。打 URL 时只留 scheme+host+path。
- 日志文件：`%APPDATA%\AriaHost\logs\ariahost.log`、`aria2.log`，单文件 10MB × 5 份轮转（架构 §4.11）。

## 6. 并发与异步

- 托盘/UI 线程不做阻塞 IO：所有 aria2 交互走 `tokio` 任务，UI 只读通道消息（egui 的 `update()` 每帧必须快速返回）。
- 共享状态优先 `Arc<Mutex<T>>` + **短临界区**，禁止在持锁期间 `.await` 或调用外部进程。
- 跨线程消息用 `std::sync::mpsc` 或 `tokio::sync::mpsc`；不用全局可变静态（`static mut` 一律禁止）。
- 进程等待/超时必须有上限：停止链 25s、启动 30s 等阈值不写成魔法数，集中在 `ariahost-common::timings`。

## 7. 文件与模块规范

- 单文件建议 ≤500 行；超过就按职责拆（不是按「工具函数/结构体」拆）。
- `lib.rs`/`main.rs` 只做装配与再导出，不写业务逻辑。
- 公共 API 用 `///` 文档注释，说明失败条件；内部函数按需。
- 新增依赖前在 PR 里写一句「为什么 std 不行」；依赖数量增长要能解释。

## 8. 提交信息与分支

- 提交信息：Conventional Commits，正文中文，type/scope 用词见 [CONTRIBUTING.md](../../CONTRIBUTING.md) §3。
- 分支：`main` 受保护（只接受 PR，`ci-gate` 必须绿）；开发分支 `feat/<主题>`、`fix/<主题>`、`docs/<主题>`，合入前 squash。
- 一个 PR 只做一件事；文档改动与代码改动分 PR（除非是改命令/参数导致的连带同步）。

## 9. 与文档的一致性

改代码时顺手检查：

- 命令/参数变了 → 同一个 PR 里改 `docs/dev/build.md`、`installer/ariahost.nsi`。
- 错误码/文案变了 → 同一个 PR 里列出差异；`docs/requirements-and-design/` 由架构负责人改，不要自己动。
- 版本号只在 `Cargo.toml` 的 `[workspace.package] version`，其余地方出现硬编码版本号即视为 bug。
