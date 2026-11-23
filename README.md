# AriaHost

> Windows 上的 aria2 服务管理器：把命令行下载器 aria2 装进「可选 Windows 服务 + 中文托盘面板 + 浏览器网页管理页」三件套，装好即用。

- 产品名 **AriaHost**，可执行文件 `ariahost.exe`，服务名 `AriaHost`；仓库代号 `wasm` 为历史名保留（不影响用户可见标识）。
- 技术栈：Rust + [egui/eframe](https://github.com/emilk/egui) 原生 UI（无 WebView2 依赖）、自写 NSIS 安装器、GitHub Actions 出包。
- 当前状态：**设计定版阶段（M0）**。仓库里只有文档与 CI/安装脚本骨架，**没有任何 Rust 代码**（无 `Cargo.toml`、无 `crates/`）。CI 与安装脚本尚未真跑过——首次真跑以 M1/M3 的 Actions 日志为准。

## 特性（首版范围）

- **三形态并存**：托盘面板（速览/启停/暂停/删除）＋ 浏览器网页管理页（完整任务管理）＋ 本地 RPC 供 AriaNg/Motrix 等第三方客户端接入。
- **服务三模式**：`windows-service` / `headless` / `off`；默认手动触发，不随开机自动拉起 aria2。
- **优雅停止**：`saveSession` → `shutdown` → 15s → `forceShutdown` → 10s → 强杀，全程 ≤25s 且状态可见。
- **核心版本管理**：在线拉取受支持的 24 个版本（1.19.1→1.37.0）或指定本地文件；升级失败自动回滚上一版。
- **磁盘治理**：先检测后提示，可配置阈值与一键清理，硬边界（已完成文件/活动任务/配置/密钥）绝不自动删。
- **出厂安全**：RPC 仅本机、secret 自动生成并存 DPAPI、局域网档需二次确认、日志统一脱敏。
- **中文界面**：首版单语，错误提示走统一模型，不把 aria2 stderr 裸弹给用户。

范围外（首版不做）：BT/Metalink 管理、多语言、aria2 自动升级、公网管理页、aria2 全部选项的平铺暴露。详见[需求分析](docs/requirements-and-design/2026-11-22-wasm-需求分析.md) §9。

## 安装

- 当前**没有可下载的安装包**（首个可安装版本计划在 M3 之后）。发布后从 GitHub Releases 下载 `AriaHost-<版本>-x64-setup.exe`，用 `SHA256SUMS.txt` 校验后再运行。
- 系统要求：**Windows 10 1809（10.0.17763）或更高**，或 **Windows Server 2019 及以上**，**x64**。Windows 10 1803 及更早、Windows Server 2016 不被支持——安装器用 `WinVer` 卡内部版本 `17763`，低于该版本**安装即拒**并提示 `AH-E1001`。首版**只发 x64 per-user 单包**；x86 包与全机（all-users）变体在 M4 之前不进流水线。
- 首版安装包**未做代码签名**，SmartScreen 可能提示「未知发布者」——这是已知取舍，以 SHA-256 校验值为准（见 [docs/dev/release.md](docs/dev/release.md)）。

## 截图

（留空，M2 面板完成后补入 `docs/images/panel.png`；在真实截图出现之前不放任何示意图。）

## 开发快速开始

> 前提：已装 Rust stable + VS Build Tools（C++ 工作负载）；详见 [docs/dev/setup.md](docs/dev/setup.md)。
> 仓库根的 `rust-toolchain.toml` 把工具链 pin 在 **Rust 1.95**（唯一事实源），在本目录内跑 `cargo` 时 rustup 会自动切版本（首次需联网下载）。
> **仓库当前没有 `Cargo.toml`，下面三条命令在 M1 落地前会直接报错**，这是预期行为。

```powershell
cargo fmt --all -- --check      # 格式
cargo build --release --workspace   # 构建
cargo test --workspace          # 测试
```

CI 与发布流水线：`.github/workflows/ci.yml`（PR 门禁）、`.github/workflows/release.yml`（`v*` tag 出安装包）。

## 目录结构

```text
crates/                 Rust workspace 成员（M1 起陆续落地）
  ariahost-common/      配置、错误码、日志、路径约定
  ariahost-aria2/       aria2 进程管理与 RPC 客户端
  ariahost-svc/         ServiceHost（SCM 注册、停止链、状态机）
  ariahost-web/         网页管理页资产与同源反代
  ariahost-ui/          egui/eframe 面板 + 托盘（产出 ariahost.exe）
installer/ariahost.nsi  自写 NSIS 安装脚本
docs/                   文档（见下）
.github/workflows/      CI 与发布
```

> crate 名与目录以架构文档为准；`ariahost-*` 前缀为当前约定，若 architect 调整会同步到本文与 `docs/dev/`。

## 文档索引

| 文档 | 内容 |
|---|---|
| [docs/README.md](docs/README.md) | 文档地图、阅读顺序、文档规则 |
| [需求分析](docs/requirements-and-design/2026-11-22-wasm-需求分析.md) | 10 条用户原话 → F1–F12、N1–N8、21 行验收（15 条编号）、Q1–Q16 已拍板（v2.2） |
| [架构设计](docs/requirements-and-design/2026-11-22-wasm-架构设计.md) | 进程拓扑、IPC、状态机、配置表、错误码段位、里程碑 |
| [docs/dev/setup.md](docs/dev/setup.md) | 环境准备、IDE、目录说明 |
| [docs/dev/build.md](docs/dev/build.md) | 本地构建、x86 交叉、三种运行方式、本地打包 |
| [docs/dev/test.md](docs/dev/test.md) | 测试分层与 21 行验收（15 条编号）的映射 |
| [docs/dev/style.md](docs/dev/style.md) | Rust 规范、提交信息、分支模型 |
| [docs/dev/ci.md](docs/dev/ci.md) | workflow 各 job、缓存、本地复现、排错 |
| [docs/dev/release.md](docs/dev/release.md) | 发版 checklist、校验、回滚、NSIS 自检清单 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献流程（精简版） |

## 许可证

本项目采用 **MIT OR Apache-2.0 双许可**（使用者可任选其一）：

- [LICENSE-MIT](LICENSE-MIT) — MIT 全文，版权行 `Copyright (c) 2026 Alusss`。
- [LICENSE-APACHE](LICENSE-APACHE) — **待补齐**：Apache-2.0 全文需从 <https://www.apache.org/licenses/LICENSE-2.0.txt> 原样下载后替换该文件（当前文件只有一行 TODO 指针，**禁止手抄正文**——手抄出错比缺文件更糟）。补齐时点：M1 首次可联网提交（属交付完整性项 #19，与错误码无关）。
- [LICENSE](LICENSE) — 只有一行指针，不放正文。

安装包**不含许可页/EULA**：双许可不需要用户先接受条款（Q13/S3 已定），安装向导里的 `LegalCopyright` 为 `Copyright (c) 2026 Alusss`。

## 免责声明

- aria2 本身以 **GPLv2**（含 OpenSSL 例外）分发。AriaHost **不链接 aria2**、不修改其源码，只以子进程方式启动它并通过 RPC 调度，因此不构成衍生作品；但**分发 aria2 二进制时**必须随附其许可证文本与源码获取途径（见架构设计 §8 风险表）。
- AriaHost 不托管、不转售任何下载内容；用户须自行确保下载行为合法。
