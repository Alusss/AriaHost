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

x86 包自 `v0.1.3`（M4-1）起已在 `release.yml` 的发布矩阵里，与 x64 同 tag 出包、缺腿不发 Release（`fail-fast: false`）；全机变体（`-machine-` 段）仍在 M4 之后。判据以**实机运行**为准，不以「能编译」为准。矩阵与门禁见 [ci.md](ci.md) §3、[release.md](release.md) §2。

### 3.1 本地出包的两条路径（2026-11-27 实测）

**先给判据：能不能链接，只看一条。**

```powershell
# 有任何命中 → 本地 msvc 目标可能能链接；一个都没有 → 必然失败在链接
Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Lib' -Recurse -Filter kernel32.lib -ErrorAction SilentlyContinue |
  Select-Object -First 1 -ExpandProperty FullName
```

**本机实测结论（`hp zhan66pro`，2026-11-27）**：

| 项 | 事实 |
|---|---|
| VS Build Tools | 在 `F:\BuildTools\VS2022`（17.14.379），**MSVC 14.44.35207 齐全**，`link.exe`、`vcvars64.bat` 都在 |
| Windows SDK | **缺失**：`Windows Kits\10` 下 `Include`/`Lib` 为空，**全盘搜不到 `kernel32.lib`** |
| 结果 | `cargo build --target x86_64-pc-windows-msvc` / `--release` **必定失败在链接**（cargo/rustc 能在，clippy、fmt、`cargo check` 不受影响） |

所以本机**出不了 MSVC 的 exe**——这不是代码问题，是环境缺 SDK。补装 SDK 需要 UAC 提权，而**本会话被策略拒绝提权**（无交互桌面时 UAC 直接返回「操作已被取消」），必须由用户手工双击执行一次安装，装完再用上面的命令复核。

**可用路径：WSL 里的 GNU 交叉（已实测出 x64 包）**。

```bash
# 在 WSL 内（不是 PowerShell）。首次需装 mingw 链接器与目标
sudo apt-get install -y gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --locked --target x86_64-pc-windows-gnu
```

产物是对应 target 目录下的 `ariahost.exe`（PE），能用于「本地确认真的是个能跑链接结果的 Windows 二进制」；但它**不是**发布口径——发布包一律由 CI 的 msvc 腿产出（含 `+crt-static`，见 §7）。

**口诀**：只改 Rust 逻辑 → 本地跑 GNU 交叉 + `cargo test` 够用；涉及 PE 版本资源、静态 CRT、NSIS 打包、真机安装/卸载 → **只能等 Windows CI**，本地给不出结论。

## 4. 四种运行方式

CLI 契约见架构 §7.1（R9），落地时若有微调，必须同步本节与 `installer/ariahost.nsi`。

```powershell
# ① 面板（默认）：M2 起真的开窗（eframe/egui + wgpu）；M2-1 起显示真实服务/aria2 状态
.\target\release\ariahost.exe
.\target\release\ariahost.exe --backend dx12           # 只允许 D3D12（产品主路径的强制验证）
.\target\release\ariahost.exe --backend vulkan         # 只允许 Vulkan（多数机器上 wgpu 的自选项）
.\target\release\ariahost.exe --backend auto           # 默认：wgpu 自选，并尊重 WGPU_BACKEND

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
.\target\release\ariahost.exe --panel-selftest --backend dx12         # 钉死后端（DX12/Vulkan 只能二选一）
# 输出示例：
# SELFTEST adapter=NVIDIA_GeForce_RTX_4060 atlas=2048x128 atlas_cap=2048 surface=3840x2160 window=3840x2160 backend=Dx12 frames=120 ok=1 wgpu_errors=0 private_kb=48213
```

- `backend=` = **实际选中的**后端（`Adapter::get_info().backend`），不是 `--backend` 的回显：`--backend auto` 时它会如实报出 wgpu 选了谁；`--backend dx12` 在无 D3D12 的机器上会以 `SELFTEST failed reason=...` + 非 0 退出，这是预期。

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

#### 4.1.2 面板数据刷新（M2-1）

- **全部在后台线程**：`crates/ariahost-ui/src/worker.rs` 的 `Worker` 持有 `RpcClient`，用 `std::sync::mpsc` 把快照送回 UI；UI 每帧 `try_recv` 一次，从不等待网络（RPC 单次超时 5s，见 `ariahost-aria2` 的 `DEFAULT_TIMEOUT`）。
- **刷新节奏**：aria2 每 2s（`ARIA2_REFRESH_INTERVAL`）；失败按 2→4→8→16→30s 退避（`BACKOFF_CAP`，AH-E2005 语义），成功即清零。服务状态每 5s 读一次 SCM。**没有忙轮询**：交互模式靠 `request_repaint_after(500ms)` 定时问一次通道。
- **调用面**：`getVersion` + `getGlobalStat`（队列数三个字段 `getGlobalStat` 已给，不再额外打 `tell*`，少两次可能超时的 RPC）。
- **错误上屏规则**：`RpcCallError` 的 `detail`（IO 原文/JSON）**永不进 UI**，只归类成 `Aria2Failure`（不可达/超时/HTTP 状态/协议/远端码）→ 固定中文 + `AH-E2003`（403/404 走 `AH-E3003`）。连续 5 次失败后追加 `AH-E2005` 提示。`AH-E2003` 的下一步固定是「aria2 核心尚未接入（M1-3 提供）」，服务未安装固定是「一键注册在 M1-2B」。
- **窗口状态**：`%LOCALAPPDATA%\AriaHost\window.json`，单位**点**（不是物理像素），原子写（`window.json.tmp` → `rename`）；越界回中心、尺寸仍受 EG-11 的 8191 夹住；`--panel-selftest` 既不读也不写（否则一次 4K 自测会把面板永久拉成 4K）。

调试顺序：`sc query AriaHost` 看状态 → `%APPDATA%\AriaHost\logs\ariahost.log` 与 `aria2.log` 看原因 → `sc stop AriaHost` 验证停止链是否在 25s 内收尾。

退出与 AUMID（安装器会用到，手工也可验证）：

```powershell
.\target\release\ariahost.exe --quit                          # 通知已运行实例优雅退出（0=已退出 1=无实例 2=超时）
.\target\release\ariahost.exe --write-aumid "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\AriaHost\AriaHost.lnk"
```

### 4.2 渲染后端与内存闸门（M2 门禁口径）

**闸门 = 100 MB 私用工作集，两种后端都要过**（`auto`/Vulkan 与显式 `dx12` 各跑一遍）。窗口必须真实打开；数字一律取**外部采样**。

```powershell
# 60 秒、每 6s 一次，私用工作集（不是 Private Bytes，也不是程序自报）
$s = Get-Counter '\Process(ariahost)\Working Set - Private' -SampleInterval 6 -MaxSamples 10
$s.CounterSamples | Measure-Object -Property CookedValue -Minimum -Maximum
```

**本机实测**（Intel Iris Xe / 1920×1080、100% 缩放 / release / windows-latest 产物 / 均含开窗）：

| 场景 | 外部采样 60s（每 6s） | 余量 |
|---|---|---|
| M2-0b 默认路径 | 67.7–67.8 MB（零增长） | 32 MB |
| M2-1 `auto`（本机选中 Vulkan） | 72.1–72.7 MB | 28 MB |
| M2-1 显式 `--backend dx12` | **89.0–89.6 MB** | **10 MB** |
| M2-1 `--backend dx12 --size 3840x2160` | selftest `private_kb=90920`（与 1080p 基本持平，自报值仅参考） | — |

- ⚠️ **上表是历史口径（`Get-Counter` 按进程名取数），已被 §4.2.1 取代**：按句柄口径重测，DX12 是 **116.4 MB**（不是 89.6），`auto` 是 **72.0 MB**（不是 72.x 那组的 96.7）。旧读数不删，作为「名口径会骗人」的反面样例留着；**任何判断以 §4.2.1 为准**。
- **自报值不能当闸门**：同一进程同一状态下，`--panel-selftest` 的 `private_kb`（自报 75.5 MB）与外部计数器（72.3 MB）差 3 MB 量级。程序内外采样点不同（`GetProcessMemoryInfo → PrivateWorkingSetSize` 是结束瞬间的单点）。**闸门一律用进程外 `Get-Counter` 的 60s 采样**，自报值只用于快速自查。
- 交换链三档（M2-0b，captain 复现）：`1920x1080`、`2560x1440`、`3840x2160` 均 `ok=1`、`wgpu_errors=0`、`atlas=2048x64`、`atlas_cap=2048`。
- **DX12 首次被机器验证**（2026-11-25）：本机 `--backend dx12` 起窗成功。此前 `auto` 一直选 Vulkan，DX12 路径从未真正跑过——`--backend dx12` 现在是产品主路径的强制验证入口。
- **已归因（见 §4.2.1）**：`auto` 与单后端 Vulkan 的差确实是**枚举范围**（实测 20.4 MB，怀疑成立）；但「DX12 只比 Vulkan 多 ~20 MB」**不成立**——按句柄口径是 **+65 MB**（116.4 vs 51.6），仍未逐项拆开（驱动侧工作集 + 交换链）。

#### 4.2.1 三后端口径复测（M3-l，2026-11-25）

口径升级：**`Get-Counter` 按进程名取数已被废弃**（同名残留实例下只回 1 个样本、无 PID，无法证明测的是谁）。现行口径是 `K32GetProcessMemoryInfo → PrivateWorkingSetSize`，**按被测进程句柄**取数，30 采样 / 60 s，采样期强制单实例（实例数≠1 判 INCONCLUSIVE）。

同一产物（sha256=`12db9ce2…ce3f30`，Intel Iris Xe / 1920×1080 / 100% 缩放）：

| 场景 | PWS（60 s，30 采样） | 到 100 MB 的余量 |
|---|---|---|
| 全集 `auto`（wgpu 自选 Vulkan） | 72.0 MB（71.7–73.9） | 28 MB |
| 显式 `--backend vulkan`（单后端） | 51.6 MB（50.6–52.8） | 48 MB |
| 显式 `--backend dx12` | **116.4 MB（116.3–116.6）** | **−16 MB（破闸门）** |

**条件元数据（必带）**：本机该轮的条件是 `锁屏=否 / 主屏=OrayIddDriver 虚拟 IDD 1920×1080 / DPI=96 / 采样=30×60s / backend 实际=Vulkan`。同一个 `dx12` 在**锁屏**条件下读 **116.4 MB**、在「未锁屏 + 虚拟 IDD 主屏」条件下读 **88.8 MB**——**两条条件，绿红互换**。所以判定行一律写成：

```
PWS max = NNN,NNN KB（PID=NNNN，余量=NNN,NNN KB）→ PASS|FAIL|INCONCLUSIVE
条件：锁屏=否 / 主屏=<名称, 分辨率, DPI> / 采样=N / backend 实际=<Vulkan|Dx12>
```

**缺元数据的读数按 INCONCLUSIVE 处理**——`auto`/`vulkan` 稳定不变、只有 `dx12` 随条件漂 27.6 MB 这件事说明：不带条件的读数不能跨轮次比较。

**A（收窄枚举）生效的同会话 A/B 对照**（同机、同口径，唯一差别是产物）：

| 产物 | `auto` | 显式 `vulkan`（对照组） |
|---|---|---|
| 无 A（`12db9ce2`） | 71.6 MB | 51.6 MB |
| 有 A（`d8f11c33`） | **51.4 MB** | 51.6 MB |

`auto` 降 20.2 MB 而显式 `vulkan` 纹丝不动 → 省下来的就是被砍掉的枚举范围，不是环境漂移。

三条结论（都改代码，不是改文案）：

1. **枚举范围本身是成本，约 20.4 MB**：全集 72.0 vs 单后端 51.6。所以 `auto` 不再一次给 `Backends::all()`，改为**串行探测、只开一个后端**（`crates/ariahost-ui/src/backend.rs` 的 `AUTO_PROBE_ORDER` + `probe_in_order`，同一时刻最多一个实例，有单测用 Drop 计数器证明）。
2. **DX12 是破闸门的那条路**：116.4 MB，超 100 MB **16 MB**。因此 `auto` 的探测顺序是 **Vulkan → DX12 → GL**（内存优先），DX12 只作兜底并在日志里说明为什么退到它。想改回 DX12 优先就改这一个常量的顺序——但那条路当前过不了 EG-4。
3. **旧读数更正**：本文件早前记的 `auto` 96.7 MB、服务页 99.1 MB、以及「DX12 只多 ~20 MB」都建立在名口径 / 多实例读数上，**按本产物重测不再成立**；上表是新基准。DX12 为何比 Vulkan 多 65 MB 仍未逐项归因（驱动侧工作集 + 交换链，未拆）。

> 闸门仍未过的是 **DX12 这条路**，不是产品主路径（`auto` 在本机走 Vulkan = 72.0 MB）。若某台机器只有 D3D12，它会以 116 MB 起窗——这条已记入 `known-limitations.md`（L9）。

### 4.3 无人值守取证（锁屏 / 无头机）

- **不要指望截图**：锁屏时截出来是锁屏界面，无头环境根本没有桌面。改用 **UI Automation 读 egui（accesskit）暴露的文本节点**——M2-1 实测读到 **46 个可见文本节点**，覆盖服务状态、aria2 版本 / 速率 / 任务计数与四个按钮文案。这足以证明「文案与结构对」，也是 #16 面板形态的取证手段。
- **反面教训**：假 aria2 把 JSON-RPC `id` 回成数字时，客户端每轮 `getVersion` 失败并按 2s 退避重试。这**恰恰证明退避与失败归类在工作**——取证时要记录调用间隔（实测 **2.1s** = 2s 节流 + 30ms 往返），别把「一直失败」直接当缺陷。
- **能力边界**：UI Automation 证明不了**字形正确、中文渲染正常、输入法能上屏**。`selftest` 里 `font=Microsoft_YaHei_UI` 只说明字体解析成功 ≠ 字形正确 ≠ 能输入——这三件事必须人眼 + 真机输入法，见验收 #13（**仍未通过**）。
- **能验「关闭行为」**（2026-11-25 实测）：对窗口发 `WindowPattern.Close`（UIA）**确实会走到 eframe 的 `close_requested`**，不是被 winit 直接吞掉。判据是 `window.json` 被写盘——它只在 close 分支里写。因此 `ui.close_action` 三档（`tray`/`ask`/`exit`）可以在锁屏下真机验证：`tray` 之后进程仍在且窗口 `Visible=false`；`ask` 之后 UIA 树里出现对话框文本；`exit` 之后约 4 秒进程与 `aria2c` 都消失。

### 4.4 面板日志落盘（M3-m / task-58）

**面板的行为日志现在真的写到磁盘**，不再只活在内存环形缓冲里：

- **路径**：用户平面 `%APPDATA%\AriaHost\logs\ariahost.log`（服务平面 `%ProgramData%\AriaHost\logs\`），按 10MB×5 份轮转。
- **唯一收口点**：`LogBuffer::push` 一律转发（`crates/ariahost-ui/src/logbuf.rs`），所以 `app.rs` 里几十处 `self.log.push(...)` 不需要各自挂一遍——新增的行自动落盘。
- **分级**：内联错误条按语气落盘，`Tone::Bad → ERROR`、`Tone::Warn → WARN`（`app.rs::banner_level`，有单测）；`Good`/`Idle` 不写，避免噪音。
- **脱敏**：写入侧已有唯一实现 `logfile::format_line → redact`（`common/src/logfile.rs`），面板**不重复脱敏**；`token:`/`--rpc-secret=`/磁力 infohash 等模式在落盘前被替换。
- **后端探测轨迹**：`auto` 的逐次尝试与最终选择写在 `panel.probe` 目标下，例：
  `INFO panel.probe: 后端探测：尝试 vulkan → 可用；最终后端 = vulkan（只开这一个，不走全集）`
  —— 诊断包里据此可直接判定「真的用了哪个后端」，不必再跑 selftest。
- **边界**：`--panel-selftest` **不**初始化文件日志（短命自检不污染用户日志），要验写入器本身用 `--log-selftest`。

**已实测（Linux 真面板、隔离 `APPDATA`、45 s）**：`ariahost.log` 25 行 / 3530 B，`main`×1 + `panel`×23 + `panel.probe`×1，`LEAK`/`SK-SHOULD-NOT-APPEAR` 命中 0。

**这 25 行怎么来的（可复述的账）**：

| 类别 | 行数 | 说明 |
|---|---|---|
| `main` 启动 | 1 | `main.rs` 在 `logfile::init` 之后写的第一行 |
| 启动期一次性 `panel` | 18 | 后端选择 / 窗口状态 / 单实例锁 / 渲染配置 / 配置读取 / secret / 核心未就绪 / 界面行为 / 渲染器 / 图标 / 控制管道 / 系统字体 / 中文字体 / 图集上限 / 渲染设备 / 前置条件 / 后台刷新启动 … 每条只写一次 |
| `panel.probe` | 1 | 后端探测轨迹（只有 `auto` 才有这行） |
| 事件驱动 `panel` | 5 | 4 次 `aria2 刷新失败 AH-E2003`（退避 4 s→8 s→16 s）+ 1 次 `服务状态查询失败 AH-E5001`（非 Windows 的一次性事件） |

**定频写盘 = 0 行**：worker 的 2 s 刷新**成功时不写日志**（只在失败时写），`RawInput.max_texture_side` 每帧改的是内存字段、不进日志。所以「零交互面板长时间挂着」的写盘量只由**退避重试**决定：45 s 内退避只到 16 s 档，30 s 窗口里正好只多 2 行（8 s、16 s 两次重试）。若真在渲染循环里写，30 s ≈ 60 fps × 30 = **1800 行量级**——与实测差三个数量级。

**判据演化（写明，避免被当成无声放宽）**：task-58 的判据 2 原文是「用 `--panel-selftest` 跑一次，再检查落盘文件」。实现上 `--panel-selftest` **有意不落盘**（`cli::inits_file_log` 只对 `Panel | Service` 返回真）：这是**安全不变量**——并发 CI 自检会互踩同一个文件，短命自检会污染用户日志。因此该判据拆成两条替代证据：**A** 用 `--log-selftest --dir <隔离目录>` 验写入器与脱敏（exit 0、500 行、`leak=false`、落盘可见 `token:***` 与 `--rpc-secret=***`）；**B** 用真面板（隔离 `APPDATA`、45 s）验探针轨迹上盘 + 写盘有界（即上表）。拆法的覆盖面**大于**原判据：原判据只要求「轨迹进了文件」，B 还证明了「日志不会把磁盘写爆」，并顺带否掉「每帧写盘」这个真实风险。

### 4.5 内置拉丁字体 Inter（UI-7 / task-105）

**为什么随包**：`Inter` 不进系统，浏览器/系统菜单里看到的效果和面板里不一样；而「整套界面字体统一」是需求原文的一条。随包只放**拉丁**两个字重（Regular 309,828 B / SemiBold 315,756 B，合计 ≈ 0.6 MB），汉字仍走系统（内嵌 CJK 面一个就 10–17 MB，直接吃掉 <100 MB 的余量）。

**装到哪**（安装器 `installer/ariahost.nsi`）：

```
$INSTDIR\fonts\Inter-Regular.ttf
$INSTDIR\fonts\Inter-SemiBold.ttf
$INSTDIR\licenses\OFL.txt      # SIL OFL-1.1 原文，随包必备
```

源文件在 `installer\fonts\`（已入库；sha256 见 [THIRD-PARTY-NOTICES.md](../../THIRD-PARTY-NOTICES.md)）。`nsi` 用 `!if /FileExists "${__FILEDIR__}\fonts\Inter-Regular.ttf"` 门禁：**文件缺失是编译期 `!error`**，不会出一个「静默少字体」的包。卸载段**逐文件 `Delete` 再 `RMDir "$INSTDIR\fonts"`**（故意不用 `RMDir /r`：字体就在 exe 旁边、目录名可猜，可执行文件旁的递归删除风险比省两行代码大——注释在 nsi 卸载段）。

**解析顺序**（`fonts.rs::font_dirs_with`，顺序即优先级）：

1. `<exe 所在目录>\fonts\`（随包 → 绿色版/解压即用也生效）
2. `%LOCALAPPDATA%\Microsoft\Windows\Fonts`（用户装的字体）
3. `%WINDIR%\Fonts`（系统字体）

命中的面以 `FontPriority::Highest` 注入 `FontFamily::Proportional`，并额外登记一个命名族 `AriaBold` 专供粗体（`theme::bold()` 是唯一消费者）；SemiBold 与 Regular **同一个族名**，靠族内字重区分——这是 `egui 0.37` 的 `FontInsert` 唯一可行路径（provider 不按 family/weight 选面）。

**可观测**：`--panel-selftest` 的 `fonts=` 字段是唯一权威口径（格式见 `fonts.rs::fonts_line`）：

```powershell
.\ariahost.exe --panel-selftest --frames 60
# fonts=latin=<实际族>|han=<实际族>|mono=<实际族>|resolved=<预热查询>|bold=yes|inject=Inter:309828|budget=309828/17825792|inject_skip=none
```

- `latin=` / `han=` / `mono=`：**实际**落到的族名（系统 provider 返回的名字），未命中写 `miss`，**绝不回填我们请求的声明名**；
- `bold=yes|no`：命名族 `AriaBold` 是否真注入成功（`no` 时 `theme::bold()` 自动退回常规字重并落一条告警）；
- `inject=<族名>:<字节>`（多条 `;` 分隔）/ `inject=miss`：注入到底成没成；
- `budget=已用/上限`：单文件 17 MB 预算（`INJECT_BUDGET`，防两个大面一起注入）；
- `inject_skip=<角色>:<原因>`（多条 `;` 分隔）/ `none`：每个没注入的面**为什么**没注入（文件不存在 / 超预算 / 读失败），不再静默。

> 在 WSL 上跑同一条命令，`latin=`/`han=` 会落到 `system:DejaVu_Sans…` / `system:WenQuanYi_Zen_Hei…`，且 `inject=miss`——因为测试二进制的 exe 旁没有 `fonts\` 目录。**这是预期**，不是缺陷；Windows 侧要看的是 `latin=` 里出现 `Inter`、`inject=Inter:…`、`bold=yes`。

**汉字图集容量实测**（`fonts.rs` 的 `#[ignore]` 探针 `probe_inject_and_atlas_capacity`，默认不跑；需要时 `cargo test -p ariahost-ui -- --ignored --nocapture probe_inject_and_atlas_capacity`）：

实测序列（不同汉字数 × 4 档语义字号 20/16/14/12，同一进程逐级扩大；测于 Linux/WSL + 系统汉字面 `WenQuanYi Zen Hei`）：

| 不同汉字数 | 图集尺寸 | 填充率 |
|---|---|---|
| 500 | 2048 × 256 | 8.8% |
| 1000 | 2048 × 512 | 17.3% |
| 1500 | 2048 × 1024 | 26.4% |
| 2000 | 2048 × 1024 | 34.8% |
| 2500 | 2048 × 1024 | 43.9% |
| 3000 | 2048 × 2048 | 52.8% |
| 3500 | 2048 × 2048 | 61.2% |
| 4000 | 2048 × 2048 | 69.8% |

结论与代价（这两条是**判据**，不是观感）：

- **容量**：图集宽被 `max_texture_side`（EG-7 的 2048）钉住，只有高度在翻倍。按线性外推，2048×2048 装满约 **5700 个不同汉字**（4 档字号同时用到）。面板实际文案是数百字量级，常态占用个位数百分比；`--panel-selftest` 里看到的 `atlas=2048x64` 就是常态。
- **代价**：图集是 RGBA8，2048×2048 = **16.8 MB**（GPU 纹理 + 一份 CPU 侧 `ColorImage`）。所以「把几千行日志铺满屏」这类改法会让图集翻到最大档、多背 ~17–34 MB，**可能吃掉 100 MB 闸门的余量**——改日志视图时必须重跑内存闸门（§4.2）。
- 溢出 / 失败字：0 / 0（到 4000 字为止没有出现 atlas overflow）。

> 这是**容量口径**的证据（「数千汉字 × 4 档不会撑爆 2048 图集」），不是 Windows 字形观感的证据。Windows 侧的真实 `fonts=` 行（`latin=Inter…`、`bold=yes`）必须用 CI 产物复跑，见 §5.1 的顺序。

## 5. 本地绿的边界（必读）

这里的「绿」指本地命令跑过、退出码 0。**本机的绿只覆盖跨平台那部分**，不是 CI 绿的等价物。下表四条判据，按它决定「这条改动能不能在本地下结论」。

| # | 情形 | 本地为什么绿 | 该怎么做 |
|---|---|---|---|
| 1 | 改 `#[cfg(windows)]` 里的代码 | Linux 目标下整段不参与编译，clippy 看不到任何一行 | **不在本地下结论**，推上去等 Windows CI |
| 2 | 为了让绿而加 `#[allow(...)]` | lint 被自己关掉了 | **禁止**。CI 红是代码问题，不是配置问题 |
| 3 | 含固定端口的自检 / 测试 | 端口已被占 → `Server::start` 提前失败返回，测试 0.02s「通过」 | 先查端口占用再跑，跑完清进程 |
| 4 | 新增目标依赖但没让 `Cargo.lock` 落笔 | 本地不加 `--locked` 照样编过 | 跑一次 `cargo check` 让 lock 落笔再提交 |

三次真实翻车（本仓已发生）：

- **`tray.rs`**：每条语句都挂 `#[cfg(windows)]`，WSL 上 `cargo clippy --workspace --all-targets -- -D warnings` 报 **0 告警**；CI 上 `tray.rs:238` 的 `manual_range_contains` 立刻红（已改成 `RangeInclusive::contains`）。
- **6875 端口**：WSL 里一个孤儿测试进程占着端口，`Server::start` 提前失败返回，单测 **0.02 秒**「通过」；CI 上端口空闲 → 真起服务 → **挂死**。
- **`--locked`**：给 crate 加 `[target.'cfg(windows)'.dependencies]`（如 `windows-sys`）后没让 lock 落笔，CI 的 `--locked` 构建直接失败。

**禁止用 `#[allow]` 换绿**：等于把这道门禁关掉，下一处同类问题就没人挡。

### 5.1 本地能做什么

方法论：**本地尽力 → 推 → CI 出 exe → 本机真跑取证**。顺序不能反——`--panel-selftest` 的内存/交换链、托盘、三档关闭这些，必须有 exe 才有意义。

```powershell
# 本地唯一能全信的一步（不碰链接器）
cargo fmt --all -- --check

# 跨平台部分：WSL 交叉 check（已实测约 1m13s）
wsl -d debian13s1-claudecode -u root -- bash -c 'export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin; export CARGO_TARGET_DIR=/tmp/wasm-check; cd /mnt/f/git_project/wasm && cargo check -p ariahost --target x86_64-unknown-linux-gnu'
```

> 这次 check 的绿**只覆盖 `cfg(not(windows))` 分支**。含 `#[cfg(windows)]` 的代码（如 `ariahost-ui` 的托盘、`ariahost-svc` 的 SCM）在 Linux 目标下等于没被检查。

端口占用自查：WSL `ss -ltn | grep 6875`；Windows `Get-NetTCPConnection -LocalPort 6875 -ErrorAction SilentlyContinue`。

## 6. 本地打安装包

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

### 6.1 品牌图标管线（exe / 安装器 / 快捷方式共用一份 ICO）

- 源图 `assets/branding/ariahost-icon-256.png`（256×256 ARGB）→ `installer/ariahost.ico`：7 尺寸 16/24/32/48/64/128/256、32bpp；用 Pillow 生成，加 `bitmap_format="bmp"` 输出**未压缩 DIB 条目**（避开 NSIS / `rc.exe` 对 PNG 压缩图标的兼容风险），**不要手改 ICO**。
- exe 侧：`crates/ariahost/build.rs` 用 `winresource` 把该 ICO 与版本资源（ProductName / FileVersion / LegalCopyright 等）嵌进 `ariahost.exe`；MSVC 目标需要 Windows SDK 的 `rc.exe`（winresource 按 `RC_PATH` → `WindowsSdkVerBinPath` → 注册表 `KitsRoot10` 查找，CI 另有 `ilammy/msvc-dev-cmd` 兜底）。
- 安装器侧：`installer/ariahost.nsi` 用同一份 ICO 作 `MUI_ICON`/`MUI_UNICON`、`Icon`（setup.exe 自身），并装到 `$INSTDIR` 供开始菜单/桌面快捷方式引用；编译期路径一律 `${SRCDIR}\ariahost.ico`。
- CI 侧：`ci.yml` 的 makensis 门禁与 `release.yml` 的 `dist\x64` 暂存步骤都会先把 ICO 拷进 SRCDIR；`ci.yml` 还用 `VersionInfo` 断言 `ProductName`/`LegalCopyright`，证明资源真的进了 exe。

## 7. 版本号与静态链接

- 唯一来源：根 `Cargo.toml` 的 `[workspace.package] version`（各 crate 用 `version.workspace = true` 继承）。
- 安装包版本由 CI 从该值解析后通过 `/DVERSION`、`/DVI_VERSION` 注入 NSIS，**不在 `.nsi` 里手写**。
- 改版本 = 改 `Cargo.toml` 一处 + 打 tag；发版流程见 [dev/release.md](release.md)。
- **静态 CRT**（架构 §4.13 D13，验收 #15a）：M1 需在仓库根新增 `.cargo/config.toml` 设 `rustflags = ["-C","target-feature=+crt-static"]`，确保干净机器无需 `VCRUNTIME140.dll`。该文件**尚未创建**（属架构落地项，不在文档交付范围）；注意环境变量 `RUSTFLAGS` 会整体覆盖这里的配置，不要混用。

## 8. 清理与排错

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
| 自报 `private_kb` 与外部计数器差 3 MB 左右 | 正常：采样点不同（自报 = 结束瞬间单点，外部 = 60s 序列）。**结算一律用外部 `Get-Counter`**，见 §4.2 |
| 想截图取证却截到锁屏 / 黑屏 | 机器锁屏或无桌面，截图没有意义。改用 UI Automation 读文本节点，见 §4.3 |
| CI 的**许可逐字节对账**假红（18,431 B vs 18,092 B） | `actions/checkout` 按 autocrlf 把 LF 改写成 CRLF（差 339 B = 行数）。确认 `.gitattributes` 有 `installer/licenses/** -text`，并核 blob 侧应为 **18,092 B / `8177F975…B880643`**；本地复现用 `git -c core.autocrlf=true clone` 再量字节。**不要删这条 `-text`**，详见 [release.md](release.md) §7 |
