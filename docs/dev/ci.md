# CI 流水线

文件：`.github/workflows/ci.yml`（质量门禁）、`.github/workflows/release.yml`（发布，见 [release.md](release.md)）。

**状态：M0 骨架，从未在 GitHub 上跑过。** 两个 YAML 只做过**本地结构自检**（`.scratch/wasm-design/yamlcheck-builder-docs.mjs`：顶层键/缩进/行尾空格/同层重复键/`${{ }}` 配平/作业结构断言；本机无 Python/`yaml` 库，**不是真解析**）。首次真跑结果以仓库第一次 push 的 Actions 日志为准。

**工具链版本的事实源 = `rust-toolchain.toml`**（`channel = "1.95"` + `rustfmt`/`clippy` + `x86_64-pc-windows-msvc`）。但**「不传输入 action 就按文件装」是错的**：`dtolnay/rust-toolchain` 的 `toolchain` 输入默认值就是 **`stable`**（分支名），不传时它先装一份 stable，随后 rustup 又按 `rust-toolchain.toml` 补装 1.95 → 每个含工具链的 job 白下一次完整工具链（≈1.5–3 分钟 ×2 计费），且 `steps.toolchain.outputs.cachekey` 是 **stable** 的哈希（pin 升级不换缓存 key）。所以三处 action 都**显式**传 `toolchain: "1.95"`，另有 `meta` 的检查 4 与 release `version` job 的自检断言「toml 的 `channel` == workflow 里的值」，两处不会漂。`components`/`targets` 不传，由 `rust-toolchain.toml` 声明、rustup 首次调用时按需补装（日志里会看到 `installing component ...`）。

## 1. 配额前提与省分钟机制

仓库是 **private**：Windows runner 按 **2× 分钟**计费，GitHub Free 的 2000 分钟/月 ≈ **1000 分钟**实际 Windows 构建时间。首版是单 arch、无可选特性的小项目，一分钟都不该浪费在重复编译上。

| 机制 | 落点 | 省什么 |
|---|---|---|
| 单 arch 单包 | `ci.yml` 只跑 x64；`release.yml` 只编 x64 per-user 一个包；x86 job 整段注释在 `release.yml` 里（M4 恢复，见 Q3/Q15） | 直接砍掉一半 Windows 分钟与一次 `makensis` |
| Windows 只留 1 个 job | `ci.yml` 的 `windows` job 内串行 clippy → test → build，共用一次 checkout / 工具链安装 / `actions/cache` | 省掉每个 job 约 1.5–3 分钟的安装与缓存传输（这些也按 2× 计费） |
| `concurrency` | 两个 workflow 都用 `group: ${{ github.workflow }}-${{ github.ref }}` + `cancel-in-progress: true` | 同一 ref 连续 push 只留最新一次，旧 run 立刻取消 |
| 路径判定 | `ci.yml` 的 `changes` job：纯 `docs/**`、`*.md`、`.scratch/**`、`LICENSE*`、`.gitignore`、`.editorconfig` 改动 → `code=false` → `preflight`/`fmt`/`windows`/`nsi-syntax` 全部 skip，只剩 `changes` + `meta` + `gate` 三个 ubuntu job（合计约 1–2 分钟） | 文档 PR 从「3 个 Windows job」降到「3 个共约 1–2 分钟的 ubuntu job」 |
| artifact 保留 | 所有 `upload-artifact` 都是 `retention-days: 7` | 存储配额，不占分钟但占配额 |
| NSIS 预检挪到 Linux | `ci.yml` 的 `nsi-syntax` job：ubuntu + `apt-get install nsis` + 1 字节 stub 冒充 `ariahost.exe` 只跑 `makensis`（1× 计费，约 1 分钟） | 把安装脚本的**编译期**错误挡在 Windows 之前（真实 `makensis` 排在 release 的整条 `cargo build` 之后，语法错会让第一次打 tag 先烧 10–25 Windows 分钟再失败） |

**为什么不用 `on.push.paths-ignore`**：`paths-ignore` 命中时整个 workflow 根本不触发，PR 上连一个 CI 状态都没有 → 分支保护的 `ci-gate` 永远 pending，PR 卡死。现在的做法是「workflow 总是触发，编译 job 按判定 skip」，语义等价但状态始终存在。加新顶层目录（如 `vendor/`）时，记得同步 `changes` job 里的白名单正则。

**`release.yml` 没有路径过滤**：tag 推送本身就是一次显式的发布决定，用它去过滤只会在「漏改某文件」时造出「打了 tag 但没有 Release」。省分钟靠「只编一个包」，不靠跳过发布。

> `concurrency` 在 `release.yml` 上还有残留风险：同一 ref 上再点一次 `workflow_dispatch` 会取消正在跑的发布；`cancel-in-progress: true` 是配额优先的取舍，发布时别重复触发。

## 2. `ci.yml` 的 job

```text
changes ─┬─ meta                       (ubuntu，永远跑)
         ├─ preflight ─┬─ fmt         (ubuntu)
         │             ├─ windows     (windows，clippy + test + build 串行)
         │             └─ nsi-syntax  (ubuntu，stub + makensis)
         └────────────────────────────→ gate (always, 汇总红/绿)
```

| job | runner | 命令 | 失败意味着 |
|---|---|---|---|
| `changes` | ubuntu | `git diff --name-only` + 白名单正则 → `code` | 取不到 base commit 时默认 `code=true`（宁可多跑） |
| `meta` | ubuntu | ①扫 `U+FFFD`（错误编码读写留下的替换字符）②扫 YAML 里的制表符 ③ruby `Psych.parse_file` 解析两个 workflow ④断言 workflow 的 `toolchain:` 输入 == `rust-toolchain.toml` 的 `channel` | 文件被以错误编码写过（典型：`pwsh Set-Content/Out-File` 写 UTF-8 中文）、YAML 结构坏了，或工具链 pin 漂了 |
| `preflight` | ubuntu | 检查 `Cargo.toml` 存在、`Cargo.lock` 存在（缺则 warning） | **M1 前必然红，这是设计意图**：没有代码就不发绿灯 |
| `fmt` | ubuntu | `cargo fmt --all -- --check` | 有未格式化代码，本地 `cargo fmt --all` 即可 |
| `windows` | windows | `rustc -V` 校验 1.95.x → 断言 `x86_64-pc-windows-msvc` target 已装 → `cargo clippy --workspace --all-targets --locked -- -D warnings` → `cargo test --workspace --locked` → `cargo build --workspace --release --locked --target x86_64-pc-windows-msvc` → 产物存在性 + `dumpbin /dependents` **门禁** | 工具链 pin 没生效 / target 缺失 / 有 warning / 测试失败 / `Cargo.lock` 过期 / 没产出 `ariahost.exe` / 产物仍依赖动态 CRT |
| `nsi-syntax` | ubuntu | `apt-get install nsis` → 造 1 字节 `dist/x64/ariahost.exe` → `makensis -WX -INPUTCHARSET UTF8 -D…` | 安装脚本有**编译期**错误（未定义常量、卸载段调普通函数、路径拼错、字符集问题） |
| `gate` | ubuntu | `if: always()`，`needs` 结果里出现 `failure`/`timed_out`/`cancelled` 即 `exit 1`；`skipped` 视为通过 | 分支保护只看这一个 job |

**分支保护配置**：`main` 的 required status check 只勾 `ci-gate`。新增 job 时记得同步加进 `gate.needs`，否则它红了也不会拦合并。

`dumpbin` 检查已经是**门禁**（不是提示）：用 `vswhere -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64` 定位 VS 安装路径，再找 `VC\Tools\MSVC\<ver>\bin\Hostx64\x64\dumpbin.exe`；**找不到 dumpbin 或命中 `VCRUNTIME140`/`MSVCP140` 都 `exit 1`**。以前只打 `::warning::` 时，`ci-gate` 绿并不等于验收 #15a 过——那是最实在的一处假绿。权威判定仍是 #15a 的整项验收（含 Server Core 启动），见 §5。

**`nsi-syntax` 的代价与边界**：约 1 分钟 / 1× 计费。它证明的只是「`.nsi` 在 NSIS 3.x 下能编译」；**证明不了**真机安装/卸载行为、per-user 与 `/DPER_MACHINE` 的运行时差异，也不等于 Windows 版 `makensis`（choco 装的 3.11）与 Ubuntu 版（3.08/3.09）行为完全一致。stub 方案的两个坑已在 `.nsi` 侧处理：① `File` 读编译期真实文件，所以用 stub 顶上；② 路径分隔符——`"${SRCDIR}\ariahost.exe"` 在 Linux 上会被当成「文件名含反斜杠」而必然找不到，因此 `.nsi` 的**编译期**路径统一用 `/`（Windows 的 makensis 同样接受 `/`）。

## 3. `release.yml` 的 job

```text
version（解析版本 + 元数据自检，ubuntu）
  └─ build（x64 per-user 单包，windows）
        └─ publish（仅 tag 触发；gh release create）
```

| job | 做什么 | 失败意味着 |
|---|---|---|
| `version` | 解析 `version` 与四段 `vi_version`，并校验「本次发布号 == `Cargo.toml` 的 `[workspace.package] version`」（不一致 `exit 1`；`Cargo.toml` 尚不存在时跳过并 warning）；自检 `rust-toolchain.toml` 有合法 `channel`、workflow 的 `toolchain:` 输入与之相等、`.cargo/config.toml` 含 `crt-static`、`.nsi` 的 `LegalCopyright` 不是 TODO | 版本号来源缺失/非 semver/与 Cargo.toml 不一致，或三处易回归的元数据被改坏 |
| `build` | `cargo build --release --locked --target x86_64-pc-windows-msvc` → 暂存 `dist/x64` → 签名占位①（签 exe，无 secret 即跳过）→ `choco install nsis` → `makensis -INPUTCHARSET UTF8` → 签名占位②（签 setup）→ `SHA256SUMS.txt` + `manifest.json`（`variant = per-user`）→ `upload-artifact`（7 天）。`timeout-minutes: 30` | 编译、打包或校验和失败 |
| `publish` | 仅 tag：下载 artifact（**不** merge）→ 只捞 `*-x64-setup.exe` 与 `*manifest.json` → 硬校验 x64 包存在 → 重算校验和 → `gh release create --verify-tag` | 缺 `contents: write` 或 tag 不存在 |

**x86 在 M4 之前不启用**：`release.yml` 里整段注释掉了 `build-x86`（**不留 `continue-on-error`**——失败 job 一样烧 2× 分钟，且失败被静默吞掉更坏）。恢复时要同时改四处：解开 job、`publish.needs` 加它、`rust-toolchain.toml` 的 `targets` 加 `i686-pc-windows-msvc`、`.cargo/config.toml` 取消 i686 段注释。全机变体（`/DPER_MACHINE`、产物名带 `-machine-`）同样排在 M4。

## 4. 缓存

`windows`（ci）与 `build`（release）各自缓存 `~/.cargo/registry/{index,cache}`、`~/.cargo/git/db`、`target`：

```yaml
key: ${{ runner.os }}-x64-${{ hashFiles('**/Cargo.lock') }}-${{ steps.toolchain.outputs.cachekey }}
restore-keys: ${{ runner.os }}-x64-
```

- key 里同时有 `Cargo.lock` 指纹和 dtolnay/rust-toolchain 输出的 `cachekey`。注意 `cachekey` 的语义：它反映**action 实际安装的那套工具链**——因为这里显式传了 `toolchain: "1.95"`，所以它就是 1.95 的哈希；若哪天有人把 `toolchain:` 输入删掉，它就会退回 stable 的哈希，`rust-toolchain.toml` 升版不再换 key（`meta` 的检查 4 会拦住这种改动）。
- ci 与 release 的 key 前缀不同（`-x64-` / `-release-x64-`），避免 release 的 `target` 把 ci 的缓存挤掉。
- 仓库 10 GB 缓存上限下，单个 Windows `target` 通常已占数百 MB；开始被逐出时先缩小 `target` 缓存范围（只缓存 `target/release` 或干脆去掉 `build` 的那份）。

## 5. 工具链 pin 与静态 CRT

- `rust-toolchain.toml`（仓库根）= **版本与组件的事实源**：`channel = "1.95"`、`components = ["rustfmt","clippy"]`、`targets = ["x86_64-pc-windows-msvc"]`。升级时改这里的 `channel`，**同时**改三处 action 的 `toolchain:` 输入（`ci.yml` 的 `fmt`/`windows`、`release.yml` 的 `build`）。
- 三处 action 都**显式**传 `toolchain: "1.95"`：不传时它的默认值是 `stable`，会先装 stable 再让 rustup 补装 1.95（每个 job 多下一次完整工具链，cachekey 也变成 stable 的）。`components`/`targets` 仍由 `rust-toolchain.toml` 提供，rustup 首次调用时按需补装。
- 还有两道 **pin 校验步骤**（`fmt`/`windows`/`build` 里各一道 `rustc -V` 断言 1.95.x），以及两道**一致性断言**（`meta` 检查 4、release `version` job 自检）：断言「workflow 里的 `toolchain:` 值 == toml 的 `channel`」。前者防 pin 被覆盖，后者防两处漂移。
- `.cargo/config.toml` = 静态 CRT 落点（D13-A）：`[target.x86_64-pc-windows-msvc] rustflags = ["-C","target-feature=+crt-static"]`。验收 #15a 的判定方式是 `dumpbin /dependents target\x86_64-pc-windows-msvc\release\ariahost.exe` 里不出现 `VCRUNTIME140*.dll` / `MSVCP140*.dll`。
- **注意**：`RUSTFLAGS` 环境变量优先于 `.cargo/config.toml`；如果哪天为了别的目的加了 `RUSTFLAGS`，`+crt-static` 会被整段替换掉。

## 6. 本地复现 CI

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --release --locked --target x86_64-pc-windows-msvc
dumpbin /dependents target\x86_64-pc-windows-msvc\release\ariahost.exe   # 静态 CRT 自检
```

四条全绿再推。`--locked` 必须带：它在本地就能暴露 `Cargo.lock` 未提交/过期的问题，而不是等 CI 红。`rustc -V` 应打印 `1.95.x`（在仓库目录内 rustup 按 `rust-toolchain.toml` 生效；本地首次会自动下载 1.95，且 rustfmt/clippy/target 会按需补装）。`dumpbin` 不在 PATH 时的找法见 §2 的 `nsi-syntax`/`dumpbin` 说明，或直接开「x64 Native Tools Command Prompt」。

## 7. 已知限制（本节内容全部没在 GitHub 上跑过）

- **从未在 GitHub 上执行过**：仓库 remote 上没有任何 Actions 运行记录。已有的只是本地结构自检——它证明「不明显写错」，**不等于** GitHub 的解析器会接受，也不等于任何 job 曾跑通。
- 单 arch 策略意味着 CI 覆盖不到 x86：i686 的编译问题会在 M4 恢复时才暴露（`rust-toolchain.toml` 只声明了 x64 target，本地交叉编译需自己 `rustup target add`）。
- target 三元组 `x86_64-pc-windows-msvc` 在 `ci.yml` 与 `release.yml` 各以**字面量**出现（不再有矩阵变量）：工具链装的 target 来自 `rust-toolchain.toml`，`cargo build --target` 来自 workflow。两边改一处漏一处 → 编译到未安装的 target 会报 `can't find crate for 'std'`，所以 M4 加 i686 时两处都要动。
- 验收 #12（CI 出包 + `SHA256SUMS`）只能由 `release.yml` 在 GitHub 上证明；首次达成时点 = M3。
- 不跑覆盖率、不跑 `cargo audit`、不做二进制体积回归（首版口径，后续按需加）。
- **`nsi-syntax` 同样从未跑过**：它假设 ①ubuntu 镜像自带 Ruby（`Psych.parse_file` 做 YAML 语法解析）②`apt-get install nsis` 装的 3.08/3.09 版 `makensis` 支持 `-WX`/`-INPUTCHARSET`/`-D` 与 `WinVer.nsh` 里的 `${AtLeastBuild}`。这些在 NSIS 3.x 上应当成立，但**没有实测**；若首次 run 就红，先看日志属于「脚本真错」还是「工具版本差异」，别急着摘 `-WX`。

## 8. 排障

| 现象 | 原因/处理 |
|---|---|
| `preflight` 红，报「仓库尚无 Cargo.toml」 | 预期。M1 落地 workspace 后自动转绿 |
| `fmt`/`windows` 红，报「rustc 版本与 rust-toolchain.toml 不符」 | 三处 action 的 `toolchain:` 输入与 toml 的 `channel` 漂了，或本地规则没生效（`meta` 检查 4 会先一步报出这类漂移） |
| `meta` 红，列出若干文件含 `U+FFFD` | 有文件被以错误编码写过（几乎都是 `pwsh Set-Content/Out-File`）。用编辑器/`write` 工具重写该文件，别再对着乱码改 |
| `meta` 红，报「runner 上没有 ruby」 | 镜像变了：YAML 解析检查依赖 runner 自带的 Ruby/Psych。**不要退回 `pip install pyyaml`**——ubuntu 24.04 的 PEP 668 会让它直接失败，`meta` 一红整条 gate 就红 |
| `nsi-syntax` 红 | 安装脚本编译期错误（未定义常量、卸载段调了普通函数、路径拼错、中文被读成 ANSI）。注意 `-WX` 把 warning 也当错误——修 warning，不要摘 `-WX` |
| 纯文档 PR 的编译 job 全是 skipped | 正常：`changes` 判定为 `code=false`。`meta`、`nsi-syntax` 与 `gate` 仍是绿的 |
| 文档 PR 一直没有 `ci-gate` 状态 | 只有两种可能：workflow 根本没触发（`on:` 配置/权限），或 workflow 文件被 GitHub 判定解析失败（此时 Actions 页会显示 workflow 错误、连 job 都不会有）。`gate` 是 `if: always()`，上游失败它照样会跑并判红——所以「pending」不是 `changes` 挂了造成的 |
| 本地 clippy 绿、CI 红 | 版本不同。比对 `rustc -V`；本地在仓库内会自动切到 1.95，去掉 `cd` 到仓库外就回到系统默认 |
| `--locked` 报 lock file needs update | 本地 `cargo build`（不带 `--locked`）后提交 `Cargo.lock` |
| 缓存始终 miss | 检查是否刚改过 `Cargo.lock`/`rust-toolchain.toml`；`restore-keys` 命中时会打 `Cache restored from key` |
| `build` 报「缺少 ariahost.exe」 | 没有 crate 用 `[[bin]] name = "ariahost"`，或产物路径随 target 变化 |
