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

**为什么只有一个构建 job**：仓库是 private，Windows runner 按 2× 分钟计费（2000 分钟/月 ≈ 1000 分钟实际构建）。x86 在 M4 之前整段注释掉、**不留 `continue-on-error`**——失败 job 一样烧分钟，且失败被静默吞掉比不跑更坏。恢复步骤写在 `release.yml` 的注释块里（四处要一起改）。

`publish` 因此可以回到默认门禁（`needs` 全部成功），不再需要 `!cancelled()` 兜底。

## 3. 产物

```text
AriaHost-<ver>-x64-setup.exe         安装包（x64，per-user，首版唯一产物）
AriaHost-<ver>-x64-manifest.json     版本清单（product/version/arch/target/variant/commit/built_at/files）
SHA256SUMS.txt                       校验和；build 与 publish 各生成一次，覆盖范围不同（见下）
```

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

首版**只出这一个包**：x86（`-x86-setup.exe`）与全机变体（`-machine-` 段）都在 M4 才进流水线，见 [ci.md](ci.md) §3。

安装包命名由 `installer/ariahost.nsi` 的 `OUTFILE` 决定，版本/架构经 `/DVERSION`、`/DARCH` 注入，**不要在脚本里写死版本号**；CI 另用 `/DOUTFILE=<绝对路径>` 覆盖默认名，避免依赖 makensis 的工作目录假设。

**M4 加变体时的 glob 陷阱**：per-user 是 `AriaHost-<ver>-x64-setup.exe`，全机变体是 `AriaHost-<ver>-x64-machine-setup.exe`。`build` 的 SHA256SUMS 步骤与 `publish` 的 `find` 现在都只匹配 `*-x64-setup.exe`（全机包不会被命中）；一旦为了「省事」改成 `*-setup.exe`，同一发布会一次捞出多个包，校验和与 Release 附件会重名/互相覆盖。`build` 里另有一道 `machine` 名字断言，命中即失败。

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

## 6. 代码签名（首版不启用）

Q8 已定：v1 发布**不签名**，Release notes 与 README 都明说「未知发布者」。启用步骤（`release.yml` 已有占位步骤，`if: env.SIGN_CERT != ''`）：

1. 仓库 Secrets 配 `SIGN_CERT_PFX_BASE64`（pfx 的 base64）与 `SIGN_CERT_PASSWORD`。
2. 把**两处**占位步骤里的 `Write-Host` 换成真实的 `signtool sign /f ... /p ... /fd sha256 /tr <时间戳URL> /td sha256`。
3. 顺序必须是：先签 `ariahost.exe` → 再 `makensis` 打包 → 最后签 `setup.exe`。签名会改变文件字节，**签名后必须重算 SHA256SUMS**。现在 `release.yml` 已经有对应的两个占位步骤：`makensis` **之前**的「代码签名 exe」和 `makensis` **之后**的「代码签名 setup」，而校验和步骤排在两者之后——顺序已对，**不要**把 exe 签名挪到打包之后（那时 setup 里嵌的 payload 仍未签名）。
4. 不要在任何脚本里硬编码证书路径或密码。

## 7. aria2 核心的分发口径

默认**不打包** `aria2c.exe`（体积 + GPL 分发义务，见 README 免责声明）。`release.yml` 的暂存步骤注释里留了钩子：只要 `dist/<arch>/aria2c.exe` 存在，`ariahost.nsi` 里的 `!if /FileExists` 就会把它一并装进安装目录。

若改为随包分发，必须同时：附 aria2 的 GPLv2 许可文本、提供对应源码获取路径、在面板「关于」里标注其版本与来源。这条属于法务口径，**改之前先确认**。

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
- 静态 CRT（验收 #15a）的落点是 `.cargo/config.toml` 的 `[target.x86_64-pc-windows-msvc] rustflags = ["-C","target-feature=+crt-static"]`（文件已建）。是否真的无 `VCRUNTIME140*.dll` / `MSVCP140*.dll`，**以 `ci.yml` 的 `dumpbin /dependents` 门禁日志为准**——本文不单独声明该项已通过；「proc-macro 不受 target rustflags 影响」仍属【推断】。
