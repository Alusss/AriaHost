# 开发环境准备

> 适用：M1 起的本地开发。当前仓库**没有 Rust 工程**，装完环境后还没有东西可编——这是正常的。
> 开发机实测：本仓库作者机器上 `cargo`/`rustc`/`rustup`/`pnpm`/`makensis` **全都没有**，因此历史产出只能靠 CI 验证。请按本文把环境补齐。

## 1. 必备组件

| 组件 | 版本口径 | 用途 | 缺失后果 |
|---|---|---|---|
| Rust（rustup） | stable 通道。M1 起用 `rust-toolchain.toml` 钉死（TODO：版本号待定） | 编译 | 什么都编不了 |
| Visual Studio Build Tools 2022 | 工作负载「使用 C++ 的桌面开发」+ Windows 10/11 SDK | MSVC 链接器、`link.exe` | `link.exe not found` |
| Git for Windows | 2.40+ | 版本控制 | — |
| NSIS | 3.x（含 `MultiUser.nsh`，本仓库暂未使用） | 只打安装包时需要 | `makensis` 找不到 |

> 不需要 Node.js / pnpm：UI 是 egui/eframe 原生渲染，没有前端构建链。CI 里出现的 `node` 只用于本地 YAML 自检，不是项目依赖。

## 2. 安装

```powershell
# Rust（含 rustup、cargo、rustfmt、clippy）
winget install --id Rustlang.Rustup -e
# VS Build Tools：装完在 Visual Studio Installer 里勾「使用 C++ 的桌面开发」
winget install --id Microsoft.VisualStudio.2022.BuildTools -e
# NSIS（仅有出包需求时）
winget install --id NSIS.NSIS -e
# 若 winget 没有上述 id：
# choco install rust-msvc visualstudio2022buildtools nsis -y
```

安装后**重开终端**（PATH 才生效），然后：

```powershell
rustup default stable
rustup component add rustfmt clippy
rustup target add x86_64-pc-windows-msvc
rustup target add i686-pc-windows-msvc   # x86 试验性包需要
```

## 3. 自检

```powershell
rustc -V; cargo -V; cargo fmt -V; cargo clippy -V; makensis /VERSION
```

期望：前四行输出 stable 版本号，最后一行输出 NSIS 版本（不装 NSIS 则报「无法识别」，不影响编译）。
CI 与本地一致性靠 `rust-toolchain.toml`（M1 落地时提交）；在此之前本地 stable 与 CI 的 stable 可能不同版本——**格式/ clippy 差异先以 CI 为准**。

## 4. IDE 建议

| 选择 | 说明 |
|---|---|
| VS Code + `rust-analyzer` + `CodeLLDB` | 推荐。`rust-analyzer` 走 `cargo check`，与 CI 的 clippy 互补；`CodeLLDB` 用于断点调试 |
| RustRover | 开箱即用，重；机器吃紧时选上一个 |

建议的 `.vscode/settings.json`（**不入库**，见 `.gitignore`）：

```json
{ "rust-analyzer.check.command": "clippy", "editor.formatOnSave": true }
```

egui 应用调试的两个实用开关（M2 起）：

```powershell
$env:RUST_LOG = "ariahost=debug"        # tracing 日志级别（tracing_subscriber::EnvFilter）
$env:WGPU_BACKEND = "dx12"              # 图形后端排障用；默认由 wgpu 自选
```

## 5. 目录结构（M1 之后的目标形态）

```text
wasm/                       仓库根（代号保留）
├─ Cargo.toml               workspace 定义；版本号唯一来源 [workspace.package] version
├─ Cargo.lock               必须提交（CI 用 --locked）
├─ rust-toolchain.toml      TODO（M1）：钉 stable 版本 + components + targets
├─ rustfmt.toml  .editorconfig  .gitignore
├─ crates/
│  ├─ ariahost-common/      配置 schema、错误码、日志、路径
│  ├─ ariahost-aria2/       进程管理、RPC client、版本库
│  ├─ ariahost-svc/         ServiceHost（SCM、停止链、状态机）
│  ├─ ariahost-web/         网页页资产 + 同源反代
│  └─ ariahost-ui/          egui/eframe 面板 + 托盘 → ariahost.exe
├─ installer/ariahost.nsi   自写 NSIS 脚本
├─ dist/<arch>/             CI/本地暂存产物（git 忽略）
├─ docs/                    文档
└─ .github/workflows/       ci.yml / release.yml
```

## 6. 常见问题

| 现象 | 处理 |
|---|---|
| `error: linker 'link.exe' not found` | 没装 VS Build Tools 的 C++ 工作负载，或没在「x64 Native Tools」环境；重装工作负载后重开终端 |
| `can't find crate for core` / `target not installed` | 漏了 `rustup target add i686-pc-windows-msvc` |
| `cargo build` 报 `--locked` 相关错误 | `Cargo.lock` 缺失或过期：先在无 `--locked` 的情况下构建并提交 `Cargo.lock` |
| 路径含中文/空格导致链接失败 | 把仓库放到纯 ASCII 路径（如 `F:\git_project\wasm`） |
| 公司网络拉 crates.io 失败 | 配 `~/.cargo/config.toml` 镜像源；**不要**把带凭据的配置提交进仓库 |
