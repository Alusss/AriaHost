# 发布流程

文件：`.github/workflows/release.yml` + `installer/ariahost.nsi`。
**状态：发布链路已验证（2026-11-25）。** tag `v0.1.0` 移到修复后的 commit `0ebed31`，Release run **45454312274** 全绿（wall clock 2.4 分钟：`version` 8s / `build` ≈100s / `publish` ≈30s），产物已发布到 <https://github.com/doaio/wasm/releases/tag/v0.1.0>：`AriaHost-0.1.0-x64-setup.exe` **196078 字节**、`AriaHost-0.1.0-x64-manifest.json` **503 字节**、`SHA256SUMS.txt` **95 字节**；下载回来重算 SHA-256 与 `SHA256SUMS.txt` 完全一致（见 §3）。首次尝试（run **45451494558**）曾失败在 `File` 载荷路径，根因与修复见 §5.1。

> **已验证的边界**：编译 → 打包 → 校验和 → 发布这条路走通了。**没有验证的**：没有任何人在真机上点过这个安装向导（`/S` 静默、AUMID 写入、服务注册助手都还没有实测记录）。

## 1. 触发与输入

| 方式 | 触发 | 结果 |
|---|---|---|
| 推 tag | `git tag v0.1.0 && git push origin v0.1.0` | 出安装包 + 创建 GitHub Release |
| 手动 | Actions → Release → Run workflow，可选填 `version` | 只出 artifact，**不**创建 Release |

手动运行时版本号来源优先级：输入框 → `v*` tag → 根 `Cargo.toml` 的 `[workspace.package] version`；三者都空则报错退出。版本号必须形如 `0.1.0` 或 `0.1.0-beta.1`（不带 `v` 前缀）。

解析出发布号之后还有一道**一致性校验**：`Cargo.toml` 存在时，其 `version` 必须与本次发布号相同，否则 `exit 1`——防止发出「tag 写 `v0.1.0`、二进制里是 0.2.0」名实不符的包。`Cargo.toml` 尚不存在时跳过并打 warning（M1 前预期）。

## 2. Job 流程

```text
version（解析版本 + 元数据自检，ubuntu）
  └─ build（x64 per-user 单包，windows）
        └─ publish（仅 tag 触发；gh release create）
```

| job | 关键动作 |
|---|---|
| `version` | 解析 `version` 与 `vi_version`（四段数字 `x.y.z.0`，pre-release 后缀被截断，供 `VIProductVersion` 用）；校验发布号 == `Cargo.toml` 版本；自检 `rust-toolchain.toml` 的 `channel`、workflow 里 `toolchain:` 输入与之相等、`.cargo/config.toml` 的 `crt-static`、`.nsi` 的 `LegalCopyright` 不是 TODO |
| `build` | `cargo build --release --locked --target x86_64-pc-windows-msvc` → 暂存 `dist/x64/ariahost.exe`（**给 makensis 的路径必须绝对 + 全反斜杠**）→ 签名占位①（签 exe）→ `choco install nsis` → `makensis` 出安装包 → 签名占位②（签 setup）→ `SHA256SUMS.txt` + `manifest.json` → `upload-artifact`（7 天）。`timeout-minutes: 30`（首版压过，挂住时最坏计费 60 分钟而非 120） |
| `publish` | 下载 artifact（**不** merge，避免同名 `SHA256SUMS.txt` 互相覆盖）→ 只捞 `*-x64-setup.exe` 与 `*manifest.json` 到 `out/` → **校验 x64 安装包必须存在**（缺则失败、不发 Release）→ 重算校验和 → 生成 notes → `gh release create` |

**双架构构建（M4-1 起）**：`build` job 是一张 `matrix.include` 表，两条腿（x64 / x86）并行、`fail-fast: false`，产物分别为 `AriaHost-<ver>-x64-setup.exe` 与 `AriaHost-<ver>-x86-setup.exe`，各自带 `SHA256SUMS.txt` 与 `-manifest.json`；`publish` 硬门禁要求**两个包都在**才发 Release。
仓库是 private，Windows runner 按 2× 分钟计费（2000 分钟/月 ≈ 1000 分钟实际构建），所以：一条腿失败**不**用 `continue-on-error` 遮掩（失败 job 一样烧分钟，静默吞掉更坏）；日常 push 的 CI 只跑 x64，32 位腿（clippy + test + build + dumpbin）放在每周定时 / 手动 `run_x86` / 发布矩阵三条时机上真跑，账单见 [dev/ci](ci.md) §2。分钟基线待首次 tag 跑完后按日志回填。

`publish` 因此可以回到默认门禁（`needs` 全部成功），不再需要 `!cancelled()` 兜底。

## 3. 产物

```text
AriaHost-<ver>-x64-setup.exe         安装包（x64，per-user）
AriaHost-<ver>-x86-setup.exe         安装包（x86，per-user）——M4-1 起随 tag 发布
AriaHost-<ver>-<arch>-manifest.json  版本清单（product/version/arch/target/variant/commit/built_at/files）
SHA256SUMS.txt                       校验和；build 与 publish 各生成一次，覆盖范围不同（见下）
```

**v0.1.3 的实测产物（run 45695433791 全绿，双架构首个正式版；tag → commit `520ce9875079d8abbc3fb37907b27c4ad4427e6c`）**：

| 附件 | 字节 | SHA-256（下载回来独立重算，与 `SHA256SUMS.txt` 逐字一致） |
|---|---|---|
| `AriaHost-0.1.3-x64-setup.exe` | 8277700 | `70b546dabc7000d8296fc32bfff37a8d52bfaac59e5278ffe6c4e81e1d50b5b0` |
| `AriaHost-0.1.3-x86-setup.exe` | 8069194 | `fdbfcc458ca32d08a85a7447940e714f13ee53a0539b81ee26ef7866f325e1e5` |

**v0.1.0 的实测产物（run 45454312274，已发布）**：

| 附件 | 字节 | SHA-256（下载回来重算，与 `SHA256SUMS.txt` 一致） |
|---|---|---|
| `AriaHost-0.1.0-x64-setup.exe` | 196078 | `7e4a3c13b1bb6b28d3e5c6f7ee3f99483fab71c35776b99ee3ac65207816749a` |
| `ariahost.exe`（包内载荷） | 240640 | `be509a71bbc97654b3c657b293ea8ea25e2a7f53407612f8ae1418023a6c1bd1` |
| `AriaHost-0.1.0-x64-manifest.json` | 503 | —（`files` 字段带上两行的校验和） |
| `SHA256SUMS.txt` | 95 | —（`publish` 版只含 setup 一行） |

安装包 PE 版本资源实测正确：`FileVersion`/`ProductVersion` = `0.1.0`，`ProductName`/`CompanyName` = `AriaHost`，`LegalCopyright` = `Copyright (c) 2026 Alusss`，`FileDescription` = `AriaHost 安装程序`（中文正常，说明 `/INPUTCHARSET UTF8` 生效）。

**两份 `SHA256SUMS.txt` 覆盖范围不同（别当成一致）**：

- `build` job（进 artifact）：覆盖 **setup + `ariahost.exe`** 两行。
- `publish` job（挂到 Release 附件）：在 `out/` 里只有安装包（`find` 只捞 `*-x64-setup.exe` 与 `*manifest.json`），`sha256sum ./*.exe` **只覆盖安装包**。要让 exe 的校验和也对外可见，看同目录的 `manifest.json` —— 它的 `files` 字段里带着 build 时算的两行。

**v0.1.0 只出 x64 一个包**（run 45454312274）；**v0.1.3 起 x86 进发布矩阵**，同一 tag 出 `x64` 与 `x86` 两个 per-user 包。全机变体（`-machine-` 段）仍在 M4 后，见 [ci.md](ci.md) §3。

安装包命名由 `installer/ariahost.nsi` 的 `OUTFILE` 决定，版本/架构经 `/DVERSION`、`/DARCH` 注入，**不要在脚本里写死版本号**；CI 另用 `/DOUTFILE=<绝对路径>` 覆盖默认名，避免依赖 makensis 的工作目录假设。

**M4 加变体时的 glob 陷阱**：per-user 是 `AriaHost-<ver>-x64-setup.exe`，全机变体是 `AriaHost-<ver>-x64-machine-setup.exe`。`build` 的 SHA256SUMS 步骤与 `publish` 的 `find` 现在都只匹配 `*-x64-setup.exe`（全机包不会被命中）；一旦为了「省事」改成 `*-setup.exe`，同一发布会一次捞出多个包，校验和与 Release 附件会重名/互相覆盖。`build` 里另有一道 `machine` 名字断言，命中即失败。

### 3.1 v0.1.3 发布实录（双架构首个正式版，2026-11-26）

**矩阵**：`release.yml` 两条腿各出一个 per-user 包（`fail-fast: false`），x64 腿 `x86_64-pc-windows-msvc`、x86 腿 `i686-pc-windows-msvc`，同一 tag、同一 commit `520ce98`。

**x86 腿的构建证据**（run 45695433791 的「构建 x86 per-user」job 日志原文）：

| 项 | 日志原文 |
|---|---|
| 构建命令 | `cargo build --workspace --release --locked --target i686-pc-windows-msvc` |
| 工具链 | `OK：rustc 与 rust-toolchain.toml 一致` |
| 静态 CRT | `OK：x86 产物只依赖系统 DLL（无 VCRUNTIME/MSVCP）`（`dumpbin`）——**32 位下静态 CRT 同样成立** |
| 核心资产 | `aria2 zip SHA-256 = 35f6514cc5dd7e98a87b3c4c2d25a0754b9b063dbe59bc0f22d483464f61e5b6`（32 位 zip，对照 64 位 `67d01530…`）；`随包 aria2c.exe SHA-256 = b9cd71b2…` |
| 许可 | `许可原文对账通过：COPYING SHA-256 = 8177F97513213526DF2CF6184D8FF986C675AFB514D4E68A404010521B880643` |

**PE 机器类型核验（外部探针，不采信 manifest 自述）**：按偏移 `e_lfanew` 读出 `Signature`，确认等于 `0x00004550`（即 `PE\0\0`）后，再读 `Machine` 字段判架构。

| 包内文件 | `Machine` | 字节 | SHA-256 |
|---|---|---|---|
| x64 `ariahost.exe` | `0x8664` | 20052992 | `98c42fe3b0688895419d03d2879412c732e3e5d103467169745297fd8fab47cd` |
| x86 `ariahost.exe` | `0x014C` | 16903168 | `888aec4ae559bdcc7d3ed38a9296036884ea7b0c8d0d3141d4a3472d688b4028` |
| x64 `aria2c.exe` | `0x8664` | 5649408 | `be2099c214f63a3cb4954b09a0becd6e2e34660b886d4c898d260febfe9d70c2` |
| x86 `aria2c.exe` | `0x014C` | 6006798 | `b9cd71b275af11b63c33457b0f43f2f2675937070c563e195f223efd7fa4c74b` |

两个 arch 的 `manifest.json` 里 `files[].sha256` 与上表**逐字一致**——但 manifest 是产物自述，**结论以上表的独立读数为准**。

**仍未验证**：x86 包的真机安装与运行（尤其 egui/wgpu 在 i686 上的 DX12 设备创建、4 GB 地址空间下图集与缓冲是否够用）；x64/x86 两包在同一台机上的互斥行为（同版本不同架构不能并存，未实测）。

## 4. 首次发布 checklist

1. `Cargo.toml` 的 `[workspace.package] version` 改成目标版本，提交。
2. 本地四条命令全绿（见 [ci.md](ci.md) §6）；`main` 上 `ci-gate` 是绿的。
3. **工具链 pin 生效**：`rustc -V` 打印 `1.95.x`（CI 日志里 `build` job 的「校验工具链 pin 生效」步骤也必须过）。不是 1.95.x 就别发——先看三处 action 的 `toolchain:` 输入与 `rust-toolchain.toml` 的 `channel` 是否漂了（`version` job 的元数据自检与 `ci.yml` 的 `meta` 检查 4 都会断言这一点）。
4. **安装器元数据**：`installer/ariahost.nsi` 的 `VIAddVersionKey "LegalCopyright"` 是 `Copyright (c) 2026 Alusss`（不是 TODO），且安装向导**没有许可页/EULA**（双许可不需要接受条款，Q13/S3 已定——不要自行加 `MUI_LICENSEPAGE`）。`version` job 的元数据自检会挡住 TODO 回归。
5. `git tag -a v0.1.0 -m "AriaHost 0.1.0"` → `git push origin v0.1.0`。
6. 盯 `build` job：`makensis` 步骤现在**不再是首次编译**——`ci.yml` 的 `windows` job 每次 PR 都会用真实 `ariahost.exe` 跑一遍安装脚本编译门禁，能编译通过不等于能打 tag，但编译期错误会在合并前暴露。首次真跑时仍可能踩运行期问题（载荷路径、`choco` 装的 NSIS 版本、中文代码页），修的时候先看日志里 `File: "…"` 的路径形态。
7. 下载 x64 安装包在干净虚拟机里跑一遍[手工验收](../dev/test.md) §3 的 #1/#2。
8. 校验 `SHA256SUMS.txt` 与 Release 附件一致（v0.1.0 实测一致，哈希见 §3）；notes 里的 commit 对得上；`manifest.json` 的 `variant` 是 `per-user`、`U+FFFD`=0 且 `ConvertFrom-Json` 可解析。**坑**：Windows PowerShell 5.1 直接 `Get-Content` UTF-8 **无 BOM** 的 `manifest.json` 会按 GBK 解码显示乱码——那是**显示假象**，文件本身有效；核对请用 `[IO.File]::ReadAllBytes()` + `[Text.Encoding]::UTF8.GetString()`。

## 5. NSIS 自检清单

`installer/ariahost.nsi` 的**编译期**已在 windows-latest 通过（2026-11-25），本清单转为**改脚本后回归**与**真机安装**时的逐条核对（顺序即排错顺序；#11–#18 是改对齐架构 v2.0 时新增的 A1–A7 项）：

| # | 检查点 | 失败症状 |
|---|---|---|
| 1 | `makensis /INPUTCHARSET UTF8 /DVERSION=x /DVI_VERSION=x.y.z.0 /DARCH=x64 /DSRCDIR=<绝对路径>\dist\x64 /DOUTFILE=<绝对路径>\installer\AriaHost-x-x64-setup.exe installer\ariahost.nsi` 参数齐全（Windows 版用 `/` 前缀选项，路径用 `\`） | `!define` 缺省值生效，装出 `0.0.0-dev` 版本包；或产物落在意外目录 |
| 1a | **`/INPUTCHARSET UTF8` 必须带**：`installer/ariahost.nsi` 是 UTF-8 **无 BOM**、含大量中文字面量（MUI 文案、VersionKey、按钮文案）。en-US runner 的 ANSI 代码页会把它们读成乱码 | 安装向导中文变问号/乱码，或 `!define` 里的中文串编译报错 |
| 2 | `!include` 路径存在：`MUI2.nsh`、`LogicLib.nsh`、`FileFunc.nsh`、`x64.nsh`、`WinVer.nsh` | `!include: could not find` |
| 3 | `VIProductVersion` 为四段数字（`${VI_VERSION}`） | `invalid VIProductVersion format` |
| 4 | `"${SRCDIR}\ariahost.exe"` 存在，且 `SRCDIR` 是**全反斜杠的绝对路径**（实测 run 45452855606：Windows 版 `makensis` 只认这个形态——`D:/a/.../x64/ariahost.exe` 正斜杠 exit 1，相对 `dist\x64\ariahost.exe` 按**脚本所在目录**解析、同样 exit 1；`D:\a\...\x64\ariahost.exe` exit 0。**不要为了跨平台改回 `/`**，Linux 版接受 `/` 但 Windows 不接受，这正是首发失败的根因） | `File: "…" -> no files found`（即使文件确实存在） |
| 5 | `${GetSize}` / `IntFmt` 的用法（`EstimatedSize` 为 DWORD） | `Invalid command: ${GetSize}` 或写入注册表报类型错 |
| 6 | `WriteUninstaller "$INSTDIR\uninst.exe"` 在 `Section` 内（架构 §4.10 清单要求 `uninst.exe`，旧名 `uninstall.exe` 已在卸载段一并 Delete） | 卸载程序缺失 |
| 7 | 卸载段先 `Call un.QuitRunningInstance`（`--quit`，卸载器只能调 `un.` 前缀函数；安装器侧对应 `QuitRunningInstance`），再 `--uninstall-service`；per-user 提权只能 `ExecShell "runas"` | 卸载后 `sc query AriaHost` 仍有服务 / 文件占用删不掉 / 首次编译报函数未定义 |
| 8 | 中文界面：`Unicode true` + `MUI_LANGUAGE "SimpChinese"` | 安装向导中文乱码 |
| 9 | `RequestExecutionLevel user`（默认）/ `admin`（`/DPER_MACHINE`） | 提权行为与验收 #1 冲突 |
| 10 | `x64.nsh` 的 `${RunningX64}` 判断在 `.onInit` 内 | 32 位系统装 x64 包不报错但无法启动 |
| 11 | **A1 路径**：卸载清理的是 `$APPDATA\AriaHost`（配置/日志/session/secret）与 `$LOCALAPPDATA\AriaHost\cache`（dht、`aria2\versions\<ver>`、`staging`）；不得出现 `$LOCALAPPDATA\AriaHost\versions` 这种旧路径 | 用户数据删不干净，或误删到下载目录 |
| 12 | **A2 服务平面**：`%ProgramData%\AriaHost`（`aria2.conf`/`session`/`logs`/`secret`/`aria2\versions`/`staging`）先注销服务成功再清；per-user 无权限时**保留并提示**（AH-E5003 文案），不静默留垃圾 | 残留目录被当"已清理" |
| 13 | **A3 真等结果**：全机变体用 `nsExec::ExecToLog … --uninstall-service` 取退出码；per-user 用 `runas` + 轮询服务注册表键消失（≤30s）。失败时提示手工 `sc delete AriaHost` | 只 `Sleep` 假同步 → 服务仍在却继续删文件 |
| 14 | **A4 升级不停进程**：覆盖前 `Call QuitRunningInstance`；`--quit` 返回 0/1 继续，其余值 MessageBox「面板正在运行，已中止」+ `Abort` | 直接 `File` 覆盖失败，用户看到"拒绝访问" |
| 15 | **A5 AUMID**：建完开始菜单 `.lnk` 后 `ExecWait … --write-aumid "<lnk>"`；非 0 只 `DetailPrint` 告警、**不阻断**（降级判定交运行时） | Toast 永远不显示（AUMID 缺失） |
| 16 | **A6 组件可见性**：per-user 包隐藏服务组件，实现位置是 `Section "…" SEC_SERVICE` **之后**的 `Function HidePerUserServiceSection`（`SectionSetText ${SEC_SERVICE} ""`），`.onInit` 只发一次 `Call`；全机变体可勾选并直接 `--install-service`（已是 admin，不再二次 UAC） | per-user 用户勾了服务必然失败（设计上不允许）；或首次编译报 `unknown variable/constant "${SEC_SERVICE}"` |
| 17 | **A7 产物名**：per-user `AriaHost-<ver>-<arch>-setup.exe`、全机 `AriaHost-<ver>-<arch>-machine-setup.exe`，两变体不得同名 | 同 x64 两包同名，发布时互相覆盖 |
| 18 | 动态命令的可用性：`${AtLeastBuild} 17763`（`WinVer.nsh`，见 #19）、`${FileExists}` 用于目录、`${LoopWhile}` 循环、`SectionSetText` 在 Section 之后的函数里调用 | 首次编译报 `Invalid command` / 隐藏失效 |
| 19 | **系统基线**：`${IfNot} ${AtLeastBuild} 17763` 拒装并给 `AH-E1001`。**不能用 `${AtLeastWin10}`**——它只比 major/minor（10.0.10240），会放过 Windows Server 2016（10.0.14393）与 Win10 1803 及更早，与需求 N6/Q16、架构 §4.10/§13 的「安装器卡 10.0.17763」冲突 | Server 2016 装上去了；或编译报 `unknown variable/constant "AtLeastBuild"`（老版 `WinVer.nsh`） |

### 5.1 安装脚本编译门禁（Windows，真实载荷）

为什么不在 Release 里第一次编译 `.nsi`：绕到整条 `cargo build` 之后才发现语法错，第一次打 tag 就先烧 10–25 Windows 分钟（按 2× 计费）。所以编译期闸门放在 `ci.yml` 的 `windows` job 里，和 clippy/test/build 同一个 runner：

- 载荷是 **`cargo build` 的真实产物**（`target\x86_64-pc-windows-msvc\release\ariahost.exe`），不是 stub。
- `/DSRCDIR` 传**全反斜杠的绝对路径**，`/DOUTFILE` 同理；参数集与 `release.yml` 的 makensis 步骤一致，避免「CI 能编、发布编不过」。
- 产物有下限守卫（<100000 字节即失败）：真 exe 是 240KB 量级，能挡住「载荷为空/stub」这类退化。

**为什么不在 Linux 上跑（历史教训，别重犯）**：这里原有一个 ubuntu job `nsi-syntax`，`apt-get install nsis` + 1 字节 stub 只跑 `makensis`，长期显示绿色。但首发 tag 的 Release 依然失败（run 45451494558），探针 run 45452855606 证明 Windows 版只接受全反斜杠绝对路径——Linux 版接受 `/`，所以它的绿对 Windows **零证明力**。该 job 已删除，不要以任何形式恢复（包括「顺手加个跨平台路径兼容」）。

编译通过不等于能装：**必须在干净虚拟机（无管理员、无已有安装）实测**，见 `docs/dev/test.md` 验收 #1–#3。

### 5.2 makensis 从哪来（两条线路，2026-11-26 定）

历史：`v0.1.3` 的第一次发布 **两条腿同时红在「安装 NSIS」这一步**，上游是 `community.chocolatey.org` 先 503 再 504。红点在整条 Windows 编译**之后**——私有仓按 2× 分钟计费，等于一次上游抖动烧掉两轮全量编译。现在的顺序是三条，前两条都不依赖 chocolatey.org：

**runner 实证（2026-11-26，run 45693192261）**：日志原文 `NSIS 就绪（SourceForge 便携包，SHA-256 已核对；makensis 在 Bin\ 下）`——便携包直接命中（`2,363,925 B`、SHA-256 `ba63dffc…b3d8`），**choco 那条线路一次都没被用到**。

| 顺序 | 来源 | 失败即转下一条 | 证据 |
|---|---|---|---|
| 1 | `actions/cache`（键 `nsis-3.13.0-<os>`，缓存目录 `C:\Program Files (x86)\NSIS`） | 未命中 | 命中时零网络 |
| 2 | **SourceForge 官方便携 zip**：`https://downloads.sourceforge.net/project/nsis/NSIS%203/3.13/nsis-3.13.zip`，钉 SHA-256 `ba63dffc…b3d8` | 下载失败 / 哈希不符（各重试 2 次） | 本机实测：`curl -L` exit 0、2,363,925 B、哈希一致、解包树含 `Bin/`+`Stubs/`+`Include/`、用它编出 6,334,401 B 安装包 |
| 3 | `choco install nsis --version=3.13.0`（钉版本 + 10/30/60 s 三次退避） | 三次全败 → 本步 fail-closed | 2026-11-26 实测可恢复 |

**三个坑，都别再踩**（本机实测，不是推断）：

1. **便携包的 `Bin\makensis.exe` 没有版本资源**：`VersionInfo.FileVersion` 与 `ProductVersion` 都是空。原先「读 FileVersion 断言 3.13」的写法在便携包上会**假红**。现在以 `makensis /VERSION` 的运行时输出（`v3.13`）为主判据，FileVersion 仅兜底。
2. **不要把 `Bin\makensis.exe` 挪到安装根**去凑 choco 的布局：实测以 `0xC0000135`（`STATUS_DLL_NOT_FOUND`）退出。便携包必须整树落地，`makensis` 留在 `Bin\` 下；根的 2.5 KB `makensis.exe` 是官方转发器，`/VERSION` 正常，但**不要指望它能编译**。所以「找 makensis」的候选列表要同时认 `\NSIS\Bin\makensis.exe`（便携布局）与 `\NSIS\makensis.exe`（choco 布局）。
3. **本地跑 makensis 必须把 `TMP`/`TEMP` 指到工作区内**：本仓的取证 shell 在沙箱里，`%TEMP%` 下的写入/mmap 会被拒，makensis 报的是 `Internal compiler error #12345: error creating mmap the size of NNNNN` 或 `Can't open output file`——看起来像 makensis 的 bug，其实是环境。见 `.scratch/m4/local-nsis-gate.ps1`（跑之前先设 `$env:TEMP`）。

同理，**两份 workflow 的 pwsh 内联脚本在推送前必须做语法检查**：`.scratch/m4/psblocks/` 这套做法是先用 PyYAML 把所有 `shell: pwsh` 的 `run:` 抽出来，把 `${{ … }}` 替换成占位符，再用 `[Parser]::ParseInput` 过一遍（2026-11-26 实测 14 块全过）。`${{ }}` 不替换会必然假红。

## 6. 代码签名（首版不启用）

Q8 已定：v1 发布**不签名**，Release notes 与 README 都明说「未知发布者」。启用步骤（`release.yml` 已有占位步骤，`if: env.SIGN_CERT != ''`）：

1. 仓库 Secrets 配 `SIGN_CERT_PFX_BASE64`（pfx 的 base64）与 `SIGN_CERT_PASSWORD`。
2. 把**两处**占位步骤里的 `Write-Host` 换成真实的 `signtool sign /f ... /p ... /fd sha256 /tr <时间戳URL> /td sha256`。
3. 顺序必须是：先签 `ariahost.exe` → 再 `makensis` 打包 → 最后签 `setup.exe`。签名会改变文件字节，**签名后必须重算 SHA256SUMS**。现在 `release.yml` 已经有对应的两个占位步骤：`makensis` **之前**的「代码签名 exe」和 `makensis` **之后**的「代码签名 setup」，而校验和步骤排在两者之后——顺序已对，**不要**把 exe 签名挪到打包之后（那时 setup 里嵌的 payload 仍未签名）。
4. 不要在任何脚本里硬编码证书路径或密码。

## 7. aria2 核心的分发口径

**自 task-54 起改为随包分发**（定版变更）：`release.yml` 在打包前用 `scripts/fetch-aria2.ps1`（或等价步骤）下载 **aria2 1.37.0 win-64bit build1** 的官方 zip，**硬校验 zip 的 SHA-256**，解出 `aria2c.exe` 放到 `dist/<arch>/`，并把 `installer/licenses/aria2-COPYING.txt` 与仓库根 `THIRD-PARTY-NOTICES.md` 一起装进安装目录的 `licenses\`。

为什么改：双线路在线下载在真实用户网络下**不可用**（镜像全 404、直连 GitHub 抖断），首次运行引导必然失败 —— 装机即带核心是唯一可靠路径。

法务口径（三条必须同时满足，缺一不可）：

1. **许可全文**：`installer/licenses/aria2-COPYING.txt`（GPL-2.0 全文，取自官方 zip 内的 `COPYING`，18092 B，未删节）随包安装到 `<安装目录>\licenses\`；
2. **源码获取路径**：`THIRD-PARTY-NOTICES.md` 里写明上游仓库、release tag、与「上游同版本源码即对应源码」的说明（分发的就是上游构建产物，SHA-256 已记录）；
3. **版本与来源标注**：面板「关于」/诊断包 `env.json` 的 `core.origin` 标注 `bundled|managed|external` 与版本号（`core.origin` 已落地；「关于」页文案属 UI 批）。

**工程纪律（许可原文的换行符）**：`installer/licenses/**` 下的原文必须**逐字节等于上游发布包里的同名文件**，所以 `.gitattributes` 声明了 `installer/licenses/** -text`。踩过的坑：仓库 blob 是 **18,092 B / `8177F975…B880643`**（LF），`actions/checkout` 按 autocrlf 把它改写成 CRLF → **18,431 B / `189B1AF9…7167B`**（差 339 B = 行数），`release.yml` 的逐字节对账因此假红（run **45603199376**）。验证方式：`git -c core.autocrlf=true clone` 后核这两个数不变。**不要为了「统一 LF 策略」删掉这条 `-text`。**

`ariahost.nsi` 的 `!if /FileExists "${SRCDIR}\aria2c.exe"` 钩子保留（无核心时仍可编译出不含核心的包），但**发布包一律带核心**：`release.yml` 的 makensis 调用带 `/DBUNDLED_ARIA2_REQUIRED=1`，缺核心时编译期直接失败，避免发出「装完没核心」的包。

改这条口径（尤其是去掉核心或去掉许可文件）**必须先确认**。

### 7.1 在线下载线路（M4-0）

随包分发覆盖了「装完即用」，但**在线获取仍然存在**（32 位、`--aria2-fetch`、核心缺失时的首次运行引导）。线路口径在 M4-0 定版：

| 场景 | 顺序 | 依据 |
|---|---|---|
| 用户机（默认） | `cdn → github` | `net.source_order` 默认值；用户 CDN 在国内网络可达，直连 `release-assets.githubusercontent.com` 实测超时 |
| CI / 测试 | `github → cdn` | `CI_SOURCE_ORDER` / `ci_lines()`；`release.yml` 自带核心的下载步骤写死 GitHub release URL（第 232 行），**不依赖 CDN** |

URL 形态只有两种，不要混：

- CDN 行：`<net.cdn_base><文件名>`（基址默认 `https://ah.335772.xyz/releases/`，`net.cdn_base = ""` 即跳过该行）；
- 直连行：`{url}` 占位符替换成 GitHub 资产完整 URL。**直连还是走前缀代理**由 `net.core_proxy_mode`（`direct` / `proxy`，默认 `direct`）决定；`proxy` 时把 GitHub 资产**完整 URL** 拼在 `net.core_proxy_url`（默认 `https://ghproxy.net/`）后面。**旧的「前缀 + 完整 URL」镜像模板 `DEFAULT_MIRROR_TEMPLATE` 已由 task-76 从代码中删除**（`r.alusss.cn` 整体 404），不要再加回来；也不要把它和 `net.proxy`（aria2c 的下载代理）混为一谈。

哈希纪律（与随包分发同一套）：`KNOWN_SHA256` 锁死 **1.37.0 的 64 位与 32 位两个包**。命中则硬校验，不符即 `AH-E6003`，**且在装库之前失败**（旧版本原封不动）；没有已知哈希的版本走 `HashState::Unverified` + 一条进 `FetchOutcome::warnings` 的显式告警（UI 不得声称「已校验」），这是既定的诚实降级，**不要为了「看起来更严」去编造校验值**。新增线路或放宽哈希等于打开供应链口子，改之前必须确认。

## 8. 重发与回滚

- tag 已存在时 `gh release create --verify-tag` 会失败：要改内容就编辑 Release / 重传附件，不要移动 tag。
- 确实需要重发：删掉远端 tag 与 Release，提升 `[workspace.package] version` 后重新打 tag。**不要复用同一个版本号发内容不同的包**——用户端更新检查和校验和都会失真。
- 用户侧的版本回滚是产品功能（`active.json` 指针回拨，验收 #8），与这里的发布回滚是两件事。

## 9. 未验证声明

- **已验证**：源码 → 编译 → `makensis` 打包 → `SHA256SUMS`/manifest → `gh release create` 发布，全链路在 run **45454312274** 走通（产物与哈希见 §3，附件已下载回来独立重算）。**未验证**：安装向导本身——没有任何人在真机上点过它（`/S` 静默、AUMID 写入、`--install-service` 服务注册助手均无实测记录）。
- **以下三条原为静态假设，现状已变**：
  1. `${AtLeastBuild} 17763` 依赖所装 NSIS 的 `WinVer.nsh` 有该宏——编译期已通过，说明 choco 装的 3.11 里有它。
  2. ~~编译期路径可用 `/` 分隔~~ —— **已被证伪并修正**：实测 run 45452855606 证明 Windows 版 `makensis` 的 `File` 只接受全反斜杠绝对路径，正斜杠与相对路径必然 `no files found`。`.nsi` 已改 `\`，这是硬约束，不要为了跨平台改回去。
  3. `Function un.QuitRunningInstance` + `Function HidePerUserServiceSection` 的拆分——编译期已通过，运行时行为仍未验证。
- 版本注入链路（`version` job → `build` job → `makensis /D…`）已在 run 45454312274 验证生效（PE 版本资源为 `0.1.0`，与 tag 一致）。
- 剩余风险集中在运行期：真机安装/卸载、静默参数、服务注册与注销、AUMID 是否真的让 Toast 显示「AriaHost」。
- **两处口头契约未经验证，首次实测后必须回写本文与 `.nsi`**：
  1. `--quit` 的退出码（脚本假设 0=已退出、1=无实例、2=超时，其余非 0 即中止）——由 M1 的 `ariahost-cli` 实现，实际返回以代码为准；
  2. per-user 卸载时服务注销只能 `runas` 发起，**拿不到提权进程退出码**（`nsExec` 不能提权），所以改用「轮询 `HKLM\SYSTEM\CurrentControlSet\Services\AriaHost` 是否消失 ≤30s」判成败。
- 静态 CRT（验收 #15a）的落点是 `.cargo/config.toml` 的 `[target.x86_64-pc-windows-msvc] rustflags = ["-C","target-feature=+crt-static"]`（文件已建）。**已在 CI 证实**：run **45453817683** 的 `windows` job 里 `dumpbin /dependents`（`…\VC\Tools\MSVC\14.52.37025\bin\Hostx64\x64\dumpbin.exe`）输出 `OK：dumpbin 未发现 VCRUNTIME140 / MSVCP140 依赖（#15a 本次通过）`。注意这是**每次构建都会重跑**的门禁，不是一次性结论；「proc-macro 不受 target rustflags 影响」仍属【推断】。

## 10. 安装器运行期取证（`/DTRACE`）

安装器在 `/S` 静默模式下的判定（是否检测到进程、`--quit` 退出码、等了多少毫秒、为何中止）**没有 UI 可看**，所以脚本里内建了一条只在编译期开启的取证通道：`!macro TraceWrite`（`installer/ariahost.nsi`）。

- **默认构建不产生任何文件**：整段宏体被 `!ifdef TRACE` 包住，不给 `/DTRACE` 时宏展开为空。
- **开启方式**：`makensis … "/DTRACE=<绝对路径>" installer\ariahost.nsi`。CI 的 `makensis` 门禁里有一条**只编译不运行**的 `/DTRACE` 变体，用来保证这段代码不会腐烂。
- **路径规矩（都是实测教训，别改）**：
  1. 必须是 **Windows 绝对路径**，且**绝不指向仓库内**。2026-11-25 真实事故：`/DTRACE=F:\git_project\…\trace.txt` 经 bash/WSL 传参时反斜杠被吃掉，值退化成相对路径 `git_projectwasm…trace.txt`，白白落在仓库根。`.gitignore` 里 `/git_project*` 只是兜底。
  2. 文件内容是 **ANSI（中文系统为 GBK）**，不是 UTF-8。按 UTF-8 读会看到形如「锟斤拷」的替换字符与乱码——不代表取证失败，按 GBK 重读即可。
- **三个必须记住的 NSIS 坑（本次真机踩出来的，症状都很像"脚本没问题但行为不对"）**：
  1. `System::Call 'kernel32::GetTickCount()i .R0'` 的 **`.R0` 大写**才写 `$R0`；写成 `.r0` 写的是用户变量 `$0`，`$R0` 恒为空字符串 —— 预算比较 `$R1 >= 15000` 永不成立，等待循环变成死循环（实测卡 180s+）。隔离探针：大写差值 1203ms ≈ 1200ms ✓，小写读回为空。
  2. `nsExec::ExecToStack`（tasklist）的 stdout **以 CRLF 开头**（实测首字节 `0D 0A`），不 TrimEol 就永远比不中进程名 —— 修前「升级时检测不到正在运行的面板」，覆盖安装必然失败。
  3. `MessageBox` **不带 `/SD` 时静默安装也会弹窗并阻塞**（NSIS 语义），自动化里表现为"安装器挂了"。所有 MessageBox 都必须给 `/SD IDOK` / `/SD IDNO`。
- **实测口径（2026-11-25，`/S` 全静默）**：面板不退让（替身永不让出）时，升级中止耗时 **15204 ms**、退出码 **2**、TRACE 记 `等待结束：仍在运行（轮次=39，实测=15204 ms）`，且**一个文件都没覆盖**（先判后写）。

## 11. 安装器 / 卸载器退出码约定

静默模式（`/S`）没有 UI，退出码是**唯一**的失败信号，因此口径必须稳定、且**任何失败都不许返回 0**。下表每一行都有 2026-11-25 的真机实测支撑（官方 `0.1.1-x64` per-user 包）：

| 退出码 | 含义 | 触发点 | 实测证据 |
|---|---|---|---|
| `0` | 成功（安装完成 / 覆盖升级完成 / 卸载完成） | 脚本正常走完 | 安装（无实例）`0`，2.7 s；覆盖升级（面板在跑）`0`，面板 `--quit` 后 7 ms 内消失；卸载 `0` |
| `1` | **明确失败**（文件写不进、删除失败、服务注销回执非 0 等） | 先 `SetErrorLevel 1` 再 `Abort` | 目标 `ariahost.exe` 被独占锁住时安装 `1`、**8830 ms**（≈ 8 s 可写守卫上限，见 §11.2），**旧 exe 未被覆盖**；守卫落地前的同场景旧记录为 918 ms |
| `2` | **中止**（用户取消 / 内部 `Abort` 未带 `SetErrorLevel`） | `Abort` 默认口径 | 面板不让出 → 升级中止 `2`，15204 ms（15 s 预算时代），文件一个没动 |

排错建议：自动化里判 `-ne 0` 即失败，再按 `1 / 2` 分流——`1` 看权限、占用、磁盘与服务注册回执；`2` 多为交互取消或「进程赖着不退」。要定位到具体分支，用上一节的 `/DTRACE`。

### 11.1 「等面板退出」的三层预算（安装器 20 s < CLI 25 s < 面板 30 s）

三层各有一个常量，**安装器只控最外层**：

| 层 | 常量 / 位置 | 值 | 状态 |
|---|---|---|---|
| 安装器（等面板消失的**墙钟**） | `STOP_APP_BUDGET_MS`（[`installer/ariahost.nsi:367`](../../installer/ariahost.nsi)） | **20 s** | **已落地**（2026-11-25 由 15 s 抬起） |
| CLI（`--quit` 等目标进程消失） | `QUIT_BUDGET`（`crates/ariahost/src/cli.rs:600`） | **25 s**（原 10 s） | **已落地**（commit `11d25d2`） |
| 面板（自己收尾的上限） | `QUIT_TIMEOUT`（`crates/ariahost-ui/src/app.rs:56`） | **30 s** | 已落地 |

**大小关系与理由（设计意图）**：`20 < 25 < 30`——任何一层比它服务的那一层先放弃，就会把「慢但成功」报成「失败」。最内层的面板要保存会话 + 优雅停 aria2c，最外层的安装器只负责等。

**实测锚点**：`--quit` 无 aria2 = **3028–3052 ms**；有 aria2 在跑 = **7073–7079 ms**（内部 quit→exit 分别 **2.021 s** / **6.220 s**）；最坏档 `4a`「下载中、aria2 不信令」≈ **19 s**。19 s < 20 s，当前无冲突。

> **达成不了时谁先放弃（必读）**：安装器那 20 s 是**墙钟**、覆盖 `--quit` 整个阻塞调用（`kernel32!GetTickCount` 夹，见 §10）；CLI 25 s 是它自己的内部预算。代码注释已声明这是**知情接受**：25 s 不覆盖面板 30 s 的极端慢，靠安装器随后的复查与「最坏档 ≈19 s」的实际分布兜住。当安装器那 20 s 先到期而面板仍在收尾时，用户看到的是 `2`（中止）而不是 `0`——**当前实测最坏 19 s < 20 s，尚无冲突；25 s 落地后必须重测这一档**，测之前不要把 20 < 25 当成已证明安全。

**`--quit` 的 timeout 现在分三阶段**（commit `11d25d2`）：`TimeoutPhase::Handshake`（没收到应答，请求可能没送达）/ `ExitWait`（应答收到但没退完）/ `Watchdog`（外层看门狗到期）。三态给不同的**可执行**建议（都不建议强杀——强杀丢会话、留 aria2c 孤儿）。**对脚本可见的契约没变**：stdout 仍只有一行 `quit: timeout`，退出码仍 **2**；细分只进 stderr。

轮询间隔仍是 250 ms、判定仍只认 `tasklist`。TRACE 里打印 `墙钟（本步合计：--quit + 复查）= N ms`，因为 `--quit` 是阻塞调用，真正的等待发生在客户端内部，必须由安装器自己量。

`--quit` 三态与之的关系（客户端三态见 `crates/ariahost/src/cli.rs`）：

| `--quit` 结果 | 退出码 | 安装器随后做什么 |
|---|---|---|
| `no-instance` | `0` | 视作已退出，直接继续（安装器视为成功，不报错） |
| `accepted` | `0` | **仍要复查进程是否消失**——`accepted` 只表示请求已送达，不是「已退出」；复查走 250 ms 轮询直到预算耗尽 |
| `timeout` | `2` | 同样进入复查轮询；预算内消失则继续，仍活着才 `Abort` |

预算达成不了的情形是**有意 fail-closed**：aria2 拒绝退出（连 `forceShutdown` 也无效）这类病态场景，20 s 同样兜不住——此时安装器照旧 `Abort` 并提示用户手动结束进程，**不会**为了让安装继续而去 `taskkill /F IM ariahost.exe`（那会绕过面板的会话保存与优雅停止）。残留在 `$INSTDIR` 的 `aria2c.exe` 只在其映像路径位于 AriaHost 数据目录下时才强杀，其余一律报给用户。

### 11.2 覆盖写之前的「目标文件可写」守卫（`WaitForFileWritable`）

位置：[`installer/ariahost.nsi:688`](../../installer/ariahost.nsi) 起，在 `File "${SRCDIR}\${APP_EXE}"` **之前**。

- **只在目标已存在时生效**（升级/覆盖）——全新安装时 `$INSTDIR\ariahost.exe` 不存在，立即返回，不多等一毫秒。
- 上限 **32 × 250 ms = 8 s**；用 `FileOpen … "a"` 探独占可写性，成功即 `FileClose` 并继续。
- 超时**不静默继续**：明确中文文案 + `SetErrorLevel 1` + `Abort`，退出码 **1**（与 §11 的口径一致）。
- 为什么需要：**面板已退出 ≠ 文件句柄已释放**——杀软扫描、句柄拆除延迟、覆盖写共享冲突，都能让 `File` 在进程消失后仍然写不进去；没有守卫时这类失败表现为「安装莫名其妙中止」。
- 与 §11.1 的分工：**等进程消失**（`--quit` + `tasklist` 轮询）解决「谁在用」，**可写守卫**解决「文件是否已交还」，两件事不能互相替代。

**取证（两向）**：

| 场景 | 结果 |
|---|---|
| 加守卫前后各 20 连跑 | `Abort` **0/20**、平均 **2887 ms**、守卫命中 **0/20**（自然竞态没抓到） |
| **独占锁构造等价竞态** | `exit 1`、**8830 ms**（≈ 8 s 上限）、**目标字节未变**；解锁后重跑 → `exit 0` |

自然竞态「未复现」不构成证据，**以构造实验为准**——方法论见 [test.md](test.md) §6.13。

## 12. 控制管道名按用户 SID 隔离（`--quit` 的跨用户语义）

原先是固定名 `\\.\pipe\ariahost-ctl`：同一台机器上**任何**账户运行的面板都抢同一个名字，A 用户执行 `--quit` 有可能停掉 B 用户的面板。现改为按**当前进程令牌的用户 SID** 派名：

| 项 | 值 |
|---|---|
| 名字形态 | `\\.\pipe\ariahost-ctl-<SID>`，例如 `…-S-1-5-21-3909229984-2559032529-2968041387-1015` |
| 唯一实现 | `ariahost_common::paths::control_pipe_name()`（进程内 `OnceLock` 缓存，服务端与客户端取到**逐字相同**的值） |
| 取不到 SID 时 | 降级为 `\\.\pipe\ariahost-ctl-pid-<pid>`（含进程号，同样**不回退**旧固定名），并落一条 `WARN` 日志 |
| DACL | 未改动，仍是 `D:(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)`——拒绝网络登录，允许 SYSTEM / 本机管理员 / **所有者**；本来就不给 `Interactive` 全体。名字按 SID 隔离后，能猜到名字的前提已不存在 |

**三种情形分别怎么表现（别把第 2 条当 bug）**：

1. **同一 Windows 账户**（含 RDP 重连、多会话）：SID 相同 → 名字相同 → `--quit` 正常送达，退出码按 §11 的四态。
2. **另一个 Windows 账户**（各自跑自己的面板）：SID 不同 → 名字不同 → `--quit` 返回 `quit: no-instance`、退出码 **0**。这是**设计**：你停不了别人的面板，别人的安装器也不会误停你的。安装器把 `no-instance` 视作成功，所以卸载/升级不受影响。
3. **UAC 提权后的同一账户**：**用户 SID 不变**（变的只是完整性级别）。所以提权进程算出的管道名与面板一致，`--quit` 仍能送达——这点与「提权 = 另一个 SID」的直觉相反，按 token 里的 user SID 算即可；只有**换账户提权**（例如标准用户用另一个管理员账户的凭据）才是第 2 种情形。

**排错入口**：面板日志有一行 `控制管道已就绪：<完整管道名>`；`--quit` 失败时 stderr 会带上本次尝试的名字与 SID 来源（`sid` / `pid-fallback` 各一句）。`tasklist` / 任务管理器看不到跨用户的管道实例，别拿它们判断「管道在不在」。

**取证（2026-11-26 02:45，真机 Windows 11 build 26200，交叉构建的 x64 产物）**：

| 观测 | 值 |
|---|---|
| 面板实建的管道名 | `\\.\pipe\ariahost-ctl-S-1-5-21-3909229984-2559032529-2968041387-1001` |
| 枚举 `\\.\pipe\` 全表里旧固定名 `\\.\pipe\ariahost-ctl` | **0 个** |
| 同用户 `--quit` | `quit: accepted`、退出码 **0**、耗时 **353 ms** |
| 面板是否真退 | 15 s 内退出，退出码 **0**；退出后该管道名从枚举表消失 |
| 再执行一次 `--quit` | `quit: no-instance`、退出码 **0**（§11 口径） |
| 面板日志 | `INFO panel: 控制管道已就绪：\\.\pipe\ariahost-ctl-S-1-…-1001` + `DACL 已应用（拒绝网络登录；允许 SYSTEM/管理员/所有者）` |

## 13. 服务读取配置的注册表键 `Parameters\ConfigPath`（M5-f）

**新增的服务注册表键**（安装 `--install-service` 时写入，卸载时删除）：

| 项 | 值 |
|---|---|
| 键 | `HKLM\SYSTEM\CurrentControlSet\Services\AriaHost\Parameters` |
| 值名 | `ConfigPath` |
| 类型 | `REG_SZ`（值已是展开后的绝对路径；读取端也接受 `REG_EXPAND_SZ` 并自行展开 `%VAR%`） |
| 典型值 | `C:\Users\<安装用户>\AppData\Roaming\AriaHost\config.toml` |
| 写入者 | `--install-service` 助手（管理员上下文）。路径由安装器经环境变量 `AH_CONFIG_PATH` 传入——**必须**是安装用户算好的字面路径，助手不能自己展开 `%APPDATA%`（UAC 换账户时会指到另一个用户） |
| 卸载 | `--uninstall-service` 删除该子键；删不掉不阻断卸载 |

**服务读配置的三级顺序**（`config::load_for_service`，每级都写 `service.log`）：

1. 上面那个 `ConfigPath` → 展开 `%VAR%` → 必须是绝对路径 → 文件必须存在；
2. `%ProgramData%\AriaHost\config.toml`（机器级）；
3. 内置默认（`Config::default()`）——**记 WARN**，文案带出已查过的两个路径。

**为什么加这个键**：服务以 SYSTEM 运行，自己的 `%APPDATA%` 是 `…\systemprofile\AppData\Roaming`。没有这个键时它读不到用户那份 `config.toml`，`config::load()` 对 NotFound **静默返回默认值**，于是 `dir=` 被按 SYSTEM 的 `%USERPROFILE%` 展开成 `C:\WINDOWS\system32\config\systemprofile\Downloads` —— 用户设的下载目录/限速/并发在服务模式全部无效，文件落进系统目录（M5-f 的 P0）。

**配套护栏**（`config::download_dir_for_service`，fail-closed）：服务下发的下载目录若落在 `%SystemRoot%` 子树、任何含 `systemprofile` 组件的路径、或 `%ProgramData%` 根，则**拒绝启动 aria2c**，报 `AH-E1001` 并把原因写 `service.log` + `status.json`（`download_dir=null`）。判据是组件级 + 大小写不敏感（`C:\WINDOWS2` 不算 `C:\Windows` 子树）。

**`status.json`（schema_version = 2）新增两字段**：`config_path`（命中的那一级；内置默认时 `null`）、`download_dir`（实际下发给 aria2c 的目录；被护栏拒绝时 `null`）。面板据此提示「你的设置没生效」。

**取证口径（真机，由 Lead/用户执行）**：

```powershell
sc qc AriaHost
reg query "HKLM\SYSTEM\CurrentControlSet\Services\AriaHost\Parameters" /v ConfigPath
Get-Content C:\ProgramData\AriaHost\aria2.conf | Select-String '^dir='
Get-Content C:\ProgramData\AriaHost\status.json
```


「另一个 Windows 账户」这一路**没有**在真机上构造（本机只有一个可交互账户，另开账户需改系统设置，超出这批范围）：由 `paths::` 单测（两个不同 SID → 两个不同名、且都不等于旧固定名）与上面「旧固定名 0 个」共同保证，不在报告里冒充已验证。
