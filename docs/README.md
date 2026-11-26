# 文档地图

本目录分两层：**定版文档**（需求/架构，改版需同步两文）与**执行文档**（开发/CI/发布，随代码走）。

## 1. 阅读顺序

| 你是 | 按这个顺序读 |
|---|---|
| 第一次接触项目 | [需求分析](requirements-and-design/2026-11-22-wasm-需求分析.md) §1–§6 → [架构设计](requirements-and-design/2026-11-22-wasm-架构设计.md) §3–§4 → 本页 |
| 要写代码 | [dev/setup](dev/setup.md) → [dev/style](dev/style.md) → [dev/build](dev/build.md) → [dev/test](dev/test.md) |
| 要发版 | [dev/ci](dev/ci.md) → [dev/release](dev/release.md) |
| 只想装来用 | 仓库根 [README](../README.md) |

## 2. 文档地图

### 定版层（`requirements-and-design/`）

| 文档 | 作用 | 状态 |
|---|---|---|
| `2026-11-22-wasm-需求分析.md` | 10 条用户原话 → F1–F12 / N1–N8 / **验收 21 行（15 条编号）** / Q1–Q19 已拍板 | v2.17（M0 定版 v2.2） |
| `2026-11-22-wasm-架构设计.md` | 进程拓扑、IPC、状态机、配置总表、错误码、R1–R11 裁决、里程碑 | v2.17（M0 定版 v2.2） |

这两份是**单一事实源**：编号（F/N/Q/验收 #n）、端口、路径、阈值一律以它们为准；执行文档只做落地与索引，不另立数字。

### 设计层（`design/`，界面规格）

| 文档 | 作用 | 状态 |
|---|---|---|
| [design/ui-spec-v2.md](design/ui-spec-v2.md) | 界面设计系统 v2：字体链/字号阶梯/颜色令牌/间距/组件规格，**施工以此为准**（自包含，可直接照做） | 现行 |
| [design/ui-spec.md](design/ui-spec.md) | 设计系统 v1（历史审计记录） | **已被 v2 取代** |
| [design/ui-audit.md](design/ui-audit.md) | 逐屏审计：现状有什么问题、违反哪条规格、怎么改（基线 `6561e02`） | 参考 |

界面规格与架构文档的分工：**规格数字在 `design/`，架构只登记「界面权威规格指向这里」**（架构 §4.5）；两处冲突以 `design/` 为准并回改架构。

### 执行层（`dev/`）

| 文档 | 回答什么问题 |
|---|---|
| [dev/setup.md](dev/setup.md) | 我这台机器要装什么、装完长什么样、目录怎么摆 |
| [dev/build.md](dev/build.md) | 怎么编、怎么编 x86、产物在哪、三种形态怎么跑、**本地绿到什么程度才算数**、怎么本地打安装包 |
| [dev/test.md](dev/test.md) | 测什么、怎么测、21 行验收各自怎么验证 |
| [dev/style.md](dev/style.md) | 代码怎么写、错误怎么抛、日志怎么打、commit 怎么写、分支怎么开 |
| [dev/ci.md](dev/ci.md) | CI 有哪些 job、缓存怎么算 key、失败了怎么查、怎么本地复现 |
| [dev/release.md](dev/release.md) | 发一版要按什么顺序做什么、怎么校验、出问题怎么回滚 |
| [dev/known-limitations.md](dev/known-limitations.md) | **哪些是真缺陷但本阶段有意不修**、为什么、什么条件下该修 |

### 仓库根

`README.md`（对外门面）、`CONTRIBUTING.md`（贡献流程）、`LICENSE-MIT` / `LICENSE-APACHE`（MIT OR Apache-2.0 双许可；`LICENSE` 只是一行指针，Apache 全文待 M1 补齐）。

## 3. 文档规则（写新文档前先看）

1. **一篇 ≤150 行**：结论与命令优先，不写散文；详细论证放 `requirements-and-design/` 或 `.scratch/`。
2. **中文正文，命令与路径原样**：命令、路径、注册表键、配置键一律不翻译、不改写。
3. **数字必须可溯源**：性能/体积/耗时只能引用定版文档里的值并带【估算】【未验证】标记；禁止自造 benchmark。
4. **不写未发生的验证**：没跑过的构建、没截过的图、没测过的指标，一律写「未验证」并给出验证点。
5. **状态要标**：文档顶部写明适用于哪个里程碑（M0–M4），过期即改，不留「历史正确、现在误导」的内容。

## 4. 当前状态与待同步事项

- **仓库已有可发布工程（2026-11-27）**：`Cargo.toml` / `crates/` / `Cargo.lock` 齐备，`v0.1.0`→`v0.1.5` 连续发布，最新 `v0.1.5` 双架构出包（见 [dev/release](dev/release.md) §3）。`docs/dev/*` 的构建命令已按实际代码复核。**但本地出不了 MSVC 的 exe**——缺 Windows SDK，判据与可用路径见 [dev/build](dev/build.md) §3.1。
- **CI 与安装脚本均已真跑**：`ci.yml` / `release.yml` 在 GitHub 上连跑通过（最新发布 run `45853691576`），`installer/ariahost.nsi` 已在 Windows runner 由 `makensis` 真实编译并出包。**仍未验证的是真机安装/卸载的点击流程**（没人跑过向导），以及通知降级、IME 等真机项——登记在 [dev/known-limitations](dev/known-limitations.md)。
- **2026-11-23 的历史静态修复（已过时，别照它复原）**：当时的处置见 `.scratch/wasm-design/response-v2.md`，其中「新增 `nsi-syntax` job（ubuntu stub）」**后来被删除**——Linux 版 `makensis` 会给出假绿，编译门禁已迁到 Windows runner，原因见 [dev/ci](dev/ci.md) §1。
- **v2.0 已销账**（v1.4 时代的四处口径漂移，架构/需求 v2.0 已改）：UI 栈改为 **egui/eframe**、安装器改为**自写 NSIS**、crate 前缀 **`ariahost-*`**、错误码前缀 **`AH-E####`**。执行层文档已全量对齐，不再保留旧写法。
- **v2.1 已销账**（第三轮评审）：验收体系改为 **21 行**（1–15 不重排，`#3→#3a/#3b`、`#15→#15a/#15b`，新增 `#16–#19`），内存口径三档 **60 / <100 / <100 MB**（硬门禁仅 ②③）；`docs/dev/test.md` 与本文已同步，旧的「15 条」「150MB」写法不再出现。
- **已定并落地（2026-11，执行层；均未经 CI 实跑验证）**：
  - **S1 许可证**：`MIT OR Apache-2.0` 双许可。`LICENSE-MIT` 为 MIT 全文，`LICENSE` 改成一行指针，`LICENSE-APACHE` 是 TODO 指针（全文待 M1 从 apache.org **原样下载**，禁止手抄）；`installer/ariahost.nsi` 的 `LegalCopyright` 已填 `Copyright (c) 2026 Alusss`，首版**无许可页/EULA**（Q13/S3）。
  - **S2 工具链 pin**：`rust-toolchain.toml` = Rust **1.95**（版本与组件的源）；三处 `dtolnay/rust-toolchain` action **必须显式传** `toolchain: "1.95"`——该输入默认值是 `stable`，不传会先装 stable 再让 rustup 补装 1.95（每个 job 多下一次完整工具链，缓存 key 也变成 stable 的）。另有 `rustc -V` 断言与两道「workflow 的 `toolchain:` == toml 的 `channel`」一致性检查兜底。
  - **`.cargo/config.toml`**：已建，x64 走静态 CRT（`+crt-static`，D13-A）；验收 #15a 仍需 `dumpbin /dependents` 实测确认。
  - **CI 与发布矩阵**：仓库已迁到 **`Alusss/AriaHost`（public）**，private 时代的「只跑 x64、Windows 单 job」口径不再成立；现在 `ci.yml` 同时编 x64 与 x86，发布矩阵 x64 + x86 双腿（缺腿不发 Release）。见 [dev/ci](dev/ci.md) §1、[dev/release](dev/release.md) §2。
- **仍未决 / 待落地**（不阻塞文档，阻塞「能跑」）：
  1. **全机变体（`/DPER_MACHINE`）未进流水线**：M4-1 起已出 x64 与 x86 两份 per-user 包；全机变体排在后续批次，恢复步骤见 [dev/ci](dev/ci.md) §3。
  2. **`--quit` 退出码契约**（0/1/2）是安装器脚本的假设，M1 实现 CLI 时确认。
- 端口与路径约定（两文一致，执行层直接引用）：RPC `localhost:6800`、网页页 `http://127.0.0.1:6875`、安装目录 `%LOCALAPPDATA%\Programs\AriaHost`、配置 `%APPDATA%\AriaHost\config.toml`、服务名 `AriaHost`。
