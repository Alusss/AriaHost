# 构建与运行

> 适用：M2 起（面板已经真的起窗）。
> **开发机没有 MSVC 链接器**：本地 `cargo build / check / test` 都会停在依赖的 build script 上，
> 编译验证只能靠 CI（`.github/workflows/ci.yml`）。本文命令以 CI 日志为准，本地跑不通不算缺陷。
> 能用本地跑的是 `cargo fmt --all -- --check`、`cargo metadata`、`cargo fetch` 这类不需要链接器的动作。

## 1. 常用命令

```powershell
cargo build --workspace                                  # debug 构建
cargo build --workspace --release                        # release 构建（出包用这个）
cargo build --workspace --release --locked               # CI 用的严格模式（需已提交 Cargo.lock）
cargo test --workspace                                   # 单元 + 集成测试
cargo clippy --workspace --all-targets -- -D warnings    # 与 CI 一致
cargo fmt --all                                          # 格式化（CI 用 --check 只校验）
```

只编主程序：`cargo build --release -p ariahost`。

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
# ① 面板（默认）：M2 起真的开窗（eframe/egui + wgpu，技术闸门切片；托盘属 M2-1）
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

### 4.1 面板自测入口（隐藏，供 M2 门禁量内存与交换链）

`--panel-selftest` 不出现在 `--help` 里，只给验收/自动化用：开窗 →（可选）把窗口调到指定物理像素 → 连续渲染 N 帧 → 打印一行机器可读结果 → 退出码 0。

```powershell
.\target\release\ariahost.exe --panel-selftest                        # 默认 120 帧
.\target\release\ariahost.exe --panel-selftest --frames 300           # 指定帧数（>0）
.\target\release\ariahost.exe --panel-selftest --size 3840x2160       # 物理像素，窗口可超出屏幕
# 输出示例：
# SELFTEST adapter=NVIDIA_GeForce_RTX_4060 atlas=2048x128 atlas_cap=2048 surface=3840x2160 window=3840x2160 backend=Dx12 frames=120 ok=1 wgpu_errors=0 private_kb=48213
```

- `atlas=宽x高` / `atlas_cap` = 字形图集纹理的**实际尺寸**与配置上限（EG-7）。
- `surface=` / `window=` = 窗口客户区物理像素，eframe 用它作为交换链 extent，故两者同值。
- `ok=1` = 没有 wgpu 未捕获错误，且（若给了 `--size`）窗口尺寸确实到达目标；`ok=0` 时另打一行 `SELFTEST_NOTE` 说明原因。
- `private_kb` = 结束时 `GetProcessMemoryInfo → PrivateWorkingSetSize`（EG-4 口径的参考值）；**真机 60s 稳态仍要按 §4.2.1 口径单独量**。
- 面板与自测共用 `%LOCALAPPDATA%\AriaHost\panel.lock` 单实例锁：已有面板在跑时自测会以退出码 1 结束（先关面板）。

#### 4.1.1 渲染上限：图集与交换链是两件事（M2-0b 结论）

- `DeviceDescriptor.max_texture_dimension_2d` 同时被 wgpu 用来校验交换链尺寸（`wgpu-core/src/device/surface_config.rs:44-58`）**和**被 egui-wgpu 当作图集上限（`egui-wgpu/src/winit.rs:333` 直接返回 `device.limits().max_texture_dimension_2d`）——压图集就会连坐压窗口，2048 会让 1440p/4K 变成黑窗。
- 现在的做法：设备上限取 **8192**（覆盖 4K 与 200% 缩放），图集上限另取 **2048**，经 `App::raw_input_hook`（`eframe/src/native/epi_integration.rs:423` 每帧调用）写回 `RawInput.max_texture_side`，落到 `TextOptions.max_texture_side`（`egui/src/context.rs:587`、`:628`）。
- 图集分配语义：宽度 = 上限，高度从 32 起按需翻倍（`epaint/src/text/glyph_atlas.rs:217-224`、`epaint/src/texture_atlas.rs:270-282`），2048 的稳态开销是「2048 × 实际高度 × 4B」，最坏 16.8MB；上限放 8192 会让宽度 ×4。
- `ARIAHOST_ATLAS_SIDE` 现在**只影响图集**（不再影响窗口），是临时放大图集的逃生口，默认 2048。

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
Copy-Item installer\ariahost.ico dist\x64\ -Force   # 图标也进 SRCDIR，否则 makensis 报 no files found
makensis /DVERSION=0.1.0 /DVI_VERSION=0.1.0.0 /DARCH=x64 /DSRCDIR=..\dist\x64 installer\ariahost.nsi
```

产物：`installer\AriaHost-0.1.0-x64-setup.exe`。
全机变体（M4，首版不发布）：加 `/DPER_MACHINE` 重编一次，产物名自动带 `-machine-setup.exe`（A7：两个变体不能同名，否则发布时互相覆盖）。

> **NSIS 脚本未经本地编译验证**（开发机无 `makensis`）。首次编译请预留排错时间，并按 [dev/release.md](release.md) §5 自检清单逐条核对；编译错误以 M3 的 CI 日志为准。

### 5.1 品牌图标管线（exe / 安装器 / 快捷方式共用一份 ICO）

- 源图 `assets/branding/ariahost-icon-256.png`（256×256 ARGB）→ `installer/ariahost.ico`：7 尺寸 16/24/32/48/64/128/256、32bpp；用 Pillow 生成，加 `bitmap_format="bmp"` 输出**未压缩 DIB 条目**（避开 NSIS / `rc.exe` 对 PNG 压缩图标的兼容风险），**不要手改 ICO**。
- exe 侧：`crates/ariahost/build.rs` 用 `winresource` 把该 ICO 与版本资源（ProductName / FileVersion / LegalCopyright 等）嵌进 `ariahost.exe`；MSVC 目标需要 Windows SDK 的 `rc.exe`（winresource 按 `RC_PATH` → `WindowsSdkVerBinPath` → 注册表 `KitsRoot10` 查找，CI 另有 `ilammy/msvc-dev-cmd` 兜底）。
- 安装器侧：`installer/ariahost.nsi` 用同一份 ICO 作 `MUI_ICON`/`MUI_UNICON`、`Icon`（setup.exe 自身），并装到 `$INSTDIR` 供开始菜单/桌面快捷方式引用；编译期路径一律 `${SRCDIR}\ariahost.ico`。
- CI 侧：`ci.yml` 的 makensis 门禁与 `release.yml` 的 `dist\x64` 暂存步骤都会先把 ICO 拷进 SRCDIR；`ci.yml` 还用 `VersionInfo` 断言 `ProductName`/`LegalCopyright`，证明资源真的进了 exe。

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
