# 构建与运行

> 适用：M1 起。**当前仓库没有 `Cargo.toml`**，以下命令在 M1 落地前均会失败——本文描述的是目标形态，落地后逐条复核。
> 本文命令**未在本仓库实测**（开发机无 Rust 工具链），首次真跑以 CI 日志（`.github/workflows/ci.yml`）为准。

## 1. 常用命令

```powershell
cargo build --workspace                                  # debug 构建
cargo build --workspace --release                        # release 构建（出包用这个）
cargo build --workspace --release --locked               # CI 用的严格模式（需已提交 Cargo.lock）
cargo test --workspace                                   # 单元 + 集成测试
cargo clippy --workspace --all-targets -- -D warnings    # 与 CI 一致
cargo fmt --all                                          # 格式化（CI 用 --check 只校验）
```

只编主程序：`cargo build --release -p ariahost-ui`。

## 2. 产物路径

| 目标 | 路径 |
|---|---|
| 主程序（唯一 exe，含 `--service` 服务形态） | `target\release\ariahost.exe` |
| x86 主程序 | `target\i686-pc-windows-msvc\release\ariahost.exe` |
| 安装包（本地打包） | `installer\AriaHost-<版本>-<arch>-setup.exe` |
| 全机安装包（M4） | `installer\AriaHost-<版本>-<arch>-machine-setup.exe` |

`target/`、`dist/` 均在 `.gitignore` 内，不入库。服务形态不产出第二个 exe：`ariahost-svc` 是 crate，编进同一个 `ariahost.exe`，由 `--service` 子命令进入（架构 §7.1）。

## 3. x86 交叉构建

Rust 1.95 起 `i686-pc-windows-msvc` 是 **Tier 1 without host tools**：目标平台不提供宿主工具，但 x64 宿主可交叉编译（std 仍预编译）。

```powershell
rustup target add i686-pc-windows-msvc
cargo build --release --locked --target i686-pc-windows-msvc
```

x86 包在 M4 之前**不进 CI**：`release.yml` 里整段 `build-x86` job 被注释掉，**不留 `continue-on-error`**——失败 job 一样烧 2× Windows 分钟，而且失败被静默吞掉后没人会发现 x86 其实一直没出包，比「不跑」更坏（Q3/Q15）。M4 转正；验收以实机运行为准，不以「能编译」为准。恢复步骤见 `release.yml` 注释块与 [ci.md](ci.md) §3。

## 4. 四种运行方式

CLI 契约见架构 §7.1（R9），落地时若有微调，必须同步本节与 `installer/ariahost.nsi`。

```powershell
# ① 面板（默认）：托盘常驻，启动后按配置最小化
.\target\release\ariahost.exe

# ② headless：面板进程直接托管 aria2，关闭面板即停止（无 SCM）
.\target\release\ariahost.exe --headless

# ③ 服务模式调试：注册 → SCM 拉起 → 查状态（注册需管理员终端，一次 UAC）
.\target\release\ariahost.exe --install-service     # 建 ProgramData、复制版本库、CreateService（幂等）
sc start AriaHost
sc query AriaHost
.\target\release\ariahost.exe --uninstall-service   # 停止 → 删服务 → 清 ProgramData
```

④ 仅以 SCM 方式运行时由系统拉起 `ariahost.exe --service`（安装器不直接调它）。

调试顺序：`sc query AriaHost` 看状态 → `%APPDATA%\AriaHost\logs\ariahost.log` 与 `aria2.log` 看原因 → `sc stop AriaHost` 验证停止链是否在 25s 内收尾。

退出与 AUMID（安装器会用到，手工也可验证）：

```powershell
.\target\release\ariahost.exe --quit                          # 通知已运行实例优雅退出（0=已退出 1=无实例 2=超时）
.\target\release\ariahost.exe --write-aumid "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\AriaHost\AriaHost.lnk"
```

## 5. 本地打安装包

前置：`dist\<arch>\ariahost.exe` 已存在，且 `makensis` 在 PATH（安装见 [setup.md](setup.md)）。

```powershell
New-Item -ItemType Directory -Force dist\x64 | Out-Null
Copy-Item target\release\ariahost.exe dist\x64\ -Force
makensis /DVERSION=0.1.0 /DVI_VERSION=0.1.0.0 /DARCH=x64 /DSRCDIR=..\dist\x64 installer\ariahost.nsi
```

产物：`installer\AriaHost-0.1.0-x64-setup.exe`。
全机变体（M4，首版不发布）：加 `/DPER_MACHINE` 重编一次，产物名自动带 `-machine-setup.exe`（A7：两个变体不能同名，否则发布时互相覆盖）。

> **NSIS 脚本未经本地编译验证**（开发机无 `makensis`）。首次编译请预留排错时间，并按 [dev/release.md](release.md) §5 自检清单逐条核对；编译错误以 M3 的 CI 日志为准。

## 6. 版本号与静态链接

- 唯一来源：根 `Cargo.toml` 的 `[workspace.package] version`（各 crate 用 `version.workspace = true` 继承）。
- 安装包版本由 CI 从该值解析后通过 `/DVERSION`、`/DVI_VERSION` 注入 NSIS，**不在 `.nsi` 里手写**。
- 改版本 = 改 `Cargo.toml` 一处 + 打 tag；发版流程见 [dev/release.md](release.md)。
- **静态 CRT**（架构 §4.13 D13，验收 #15a）：M1 需在仓库根新增 `.cargo/config.toml` 设 `rustflags = ["-C","target-feature=+crt-static"]`，确保干净机器无需 `VCRUNTIME140.dll`。该文件**尚未创建**（属架构落地项，不在文档交付范围）；注意环境变量 `RUSTFLAGS` 会整体覆盖这里的配置，不要混用。

## 7. 清理与排错

```powershell
cargo clean                       # 全清（下次冷构建很慢）
Remove-Item -Recurse -Force dist  # 清暂存产物
```

| 现象 | 处理 |
|---|---|
| `warning: unused` 却被 CI 判红 | CI 开了 `-D warnings`，本地也加 `-- -D warnings` 复现 |
| `--locked` 失败 | 本地 `cargo build`（不加 `--locked`）后提交 `Cargo.lock` |
| `makensis: command not found` | 装 NSIS 或把 `C:\Program Files (x86)\NSIS` 加进 PATH |
| 安装包能装但启动即退 | 先在终端直接跑 `ariahost.exe` 看 stderr；托盘程序无控制台时用 `RUST_LOG=debug` + 日志文件 |
| `link.exe not found` | 缺 VS Build Tools 的「使用 C++ 的桌面开发」工作负载，见 [setup.md](setup.md) |
