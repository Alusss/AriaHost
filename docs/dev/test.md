# 测试策略

> 适用：M1 起。自动化目标已在 CI 跑绿（`fmt` / `clippy -D warnings` / `test` / 静态 CRT 快检 / `makensis`），但**没有任何一条验收被人工签字**；本文定义「怎么写、怎么跑、按什么验收」。
> 验收条目以[需求分析](../requirements-and-design/2026-11-22-wasm-需求分析.md) **§6 的 21 行**（15 条编号，#3/#15 各拆 a/b，另 #16–#19）为准；阈值（30s/25s/60MB/100MB 等）同样只引用定版文档，本文不另立数字。

## 1. 三层测试

| 层 | 位置 | 跑法 | 覆盖什么 | 门槛 |
|---|---|---|---|---|
| 单元 | 各 crate 内 `#[cfg(test)] mod tests` | `cargo test --workspace` | 配置解析/迁移、错误码映射、路径与端口校验、状态机迁移表、URL 模板与回退链排序 | 必须全绿，CI 阻断 |
| 集成 | `crates/*/tests/`（进程内 + 真 aria2 子进程） | `cargo test --workspace -- --include-ignored`（需 aria2c） | RPC client 与真 aria2 的握手、saveSession/shutdown 停止链、同源反代与 secret 不外泄、版本库切换与回滚 | 本地手工触发；M2 起进 CI 可选 job |
| 手工验收 | 真机 + 安装包 | 见 §3 清单 | 安装器、UAC、托盘、服务、IME、通知、内存档位等无法自动化的行为 | 每个里程碑结束前逐条签字 |

集成测试需要真实 aria2 二进制，通过环境变量指定，**未设置时跳过而不是假通过**：

```powershell
$env:ARIAHOST_ARIA2C = "C:\path\to\aria2c.exe"
cargo test --workspace -- --include-ignored
```

## 2. 不做什么

- 不设覆盖率百分比硬指标（首版代码量小，覆盖率数字容易变成装饰）。
- 不写「mock 一切」的集成测试：RPC 行为以真 aria2 为准，mock 只用于错误分支。
- 不在测试里断言耗时/内存的绝对值；性能只在验收 #11 与 N1 的口径下实测并记录环境。

### 2.1 单测禁改系统状态

`crates/ariahost/src/cli.rs` 的单测曾经**真的往 SCM 注册服务**：`--install-service` 在管理员上下文执行成功、退出码 0，而断言写死 `1` → 测试红，且**宿主机服务表被污染**。更危险的是 `--uninstall-service`——在装了 AriaHost 的机器上跑一次 `cargo test` 会**删掉用户真实的服务**。现改为纯函数测试（`parse_args` / 服务状态 JSON 只比形状，见 `service_status_json_shape_is_stable`）。

**规则**：凡是会注册/删除服务、写注册表、改系统目录、占用真实端口的命令，单测**只能测参数解析、输出形状与错误映射**，不许产生副作用；要覆盖副作用就放进需显式开关的集成测试（`cargo test -- --include-ignored`）。

## 3. 验收 21 行（15 条编号）→ 验证手段映射

来源：需求分析 §6（v2.1 定版）。**每条都必须能指到一个具体动作**，不能只写「人工确认」。既有编号 1–15 一律不重排；`3a/3b`、`15a/15b` 只是拆条目、口径不变；`#16–#19` 是 v2.1 新增（第 9 条三形态 + N8 文档完备性）。

| # | 验收要点（压缩，原文见 §6） | 验证动作 | 里程碑 | 自动化 |
|---|---|---|---|---|
| 1 | per-user 全新安装全程无 UAC；未装 VC++ 的干净系统同样可启动（与 #15a/#15b 联测）；卸载后数据默认保留、服务平面数据按 F1 提示处理 | 干净 Win10/11 虚拟机（无管理员会话）跑 `AriaHost-<ver>-x64-setup.exe`，全程录屏点 UAC（须 0 次）；装完丢一个 HTTP 链接下载；卸载后确认 `%APPDATA%\AriaHost` 仍在 | M3 | 否 |
| 2 | 断网/双线路全失败装完不阻断，面板进引导态（MSG-U06），补装可走通 | 断网安装 → 面板应进引导态；恢复网络后从引导页补装 aria2 → 能下载 | M3 | 否 |
| 3a | **M1**：服务注册助手**本体**（不依赖安装器与面板 UI）——提权执行 `ariahost.exe --install-service` → `sc query AriaHost` 为 `Manual`，`QueryServiceStatus` 30s 内进 Running 或报 `AH-E5001`；取消 UAC（1223）→ 配置不变、不落半状态、报 `AH-E5002`；助手失败可回滚并报 `AH-E5003` | 管理员会话里直接跑 `ariahost.exe --install-service`；`sc qc AriaHost` 看 START_TYPE；计时到 Running；再取消一次 UAC 比对 `config.toml` 未变 | M1 | 部分（`sc` 可脚本查，UAC 取消需人工） |
| 3b | **M2→M3**：设置页按钮→**同一条助手**；M2 段从源码目录直接跑 exe（不需安装器）点按钮 → 一次 UAC → `service.mode` 切 `windows-service`，含**面板运行中触发**（助手不得被单实例逻辑拦成 `AH-E9001`）；M3 段从 per-user 安装目录触发并与 #1 联测 | 面板点「一键装服务」并录屏点 UAC 次数（须 1 次）；面板运行中再点一次；装完从安装目录重复一遍 | M2/M3 | 部分（UI 点击手工） |
| 4 | 有活动任务点 X 默认隐藏托盘且任务继续；真退出必弹 MSG-U04；`taskkill /F` 后重启可续传 | 起大文件任务 → 点 X → 任务管理器确认进程在、进度在涨；托盘真退出看弹窗；强杀后重启看续传 | M2 | 否 |
| 5 | 同机双实例：第二个实例激活已有窗口并退出（`AH-E9001`）；6800 双开被拦截 | 双击两次 exe；再用第二个进程 `--headless` 抢 6800，应报错而非静默 | M2 | 部分 |
| 6 | 清理跑批后硬边界全部完好；回执与实际释放量一致 | 构造孤儿 `.aria2` + 缓存垃圾，跑清理，逐项核对「已完成文件/活动任务/配置/session/secret/激活版核心」未动；比对回执 KB 与资源管理器差值 | M2 | 部分（回执可断言） |
| 7 | 检查更新四态各有独立文案；1.37.0 下显示「已是最新」而非报错 | 无网 / 代理返回非 JSON / 伪造 429 / 正常四态各跑一次；再用 1.37.0 manifest 跑「无更新」 | M2 | 部分（四态判定可单测） |
| 8 | 升级成功后回滚由单次操作完成（`active.json` 原子回拨 + 旧版重启 + 三一致校验）；失败报 `AH-E6008` 并有手工出口；耗时 ≤ 25s+30s+15s | 升级 → 点一次回滚 → 校验进程版本 == `active.json` == RPC `getVersion`；人为破坏新版启动 → 看 `AH-E6008` 与人工出口 | M3 | 部分 |
| 9 | RPC 默认不可从局域网访问；lan 开启有确认弹窗与防火墙规则 | 另一台机 `curl http://<本机IP>:6800/jsonrpc` 应失败；本机 `127.0.0.1` 应成功；开 lan 看确认弹窗 + `netsh advfirewall firewall show rule` | M2 | 部分（内外网探测可脚本化） |
| 10 | 界面全部中文；错误提示全部来自统一模型（抽查 aria2 stderr 无直弹） | 遍历全部页面与托盘菜单截屏归档；构造 5 类错误（用户可修复/环境缺失/网络/权限/aria2 内部）看文案模板；抓一次 aria2 stderr 确认未直弹 | M2 | 否 |
| 11 | 内存三档（release / 60s 稳态 / `PrivateWorkingSetSize` / 含开窗）：①托盘无窗 ≤60MB ②开窗闲置 60s <100MB ③下载中开窗 <100MB。**硬门禁仅 ②③**，①是报告项+归因；任一硬门禁超标即不通过并给档位归因。**夹具**：③=`--max-download-result=5000` 注入并跑完 5000 条 1KB 本地任务（形成 5000 行列表）+ 3 个 64MB 本地文件以 1MB/s 限速持续下载 + 速率图开，1h 采样、每 5min 记一次取最大值；②=同一列表 20 行、无任务、开窗静止 60s 单点；①=关窗后 60s 单点 | 每档跑完用任务管理器或 `Get-Process \| Select PrivateMemorySize64`（口径以 `PrivateWorkingSetSize` 为准）记录三档数值；记录机器型号/系统版本/构建号 + 截图；超标项注明三前提（`MemoryHints::MemoryUsage`、字体图集 2048²、虚拟滚动）哪条缺失 | M2 | 否（可脚本取样，判定须人工归因） |
| 12 | CI 出包：`v*` tag 触发 `release.yml` → 产出 `AriaHost-<ver>-x64-setup.exe` **与** `AriaHost-<ver>-x86-setup.exe` + 各自的 `SHA256SUMS.txt` / `-manifest.json`，notes 列包名与校验和；`publish` 缺任一腿即红（M4-1 起双架构转正）。改动范围判定用 `changes` job 而**不是** `on.push.paths-ignore`（后者命中时 workflow 根本不触发，PR 上连状态都没有） | 打 tag 后看 Actions 两条腿都绿；下载两份 artifact 与 Release 附件，`Get-FileHash -Algorithm SHA256` 与 `SHA256SUMS.txt` 逐个比对；确认 PR 上纯文档改动时编译 job 是 skipped 而 `ci-gate` 仍绿 | M4-1 | 是（流水线自身即证据） |
| 13 | IME 真机（M2 门禁）：面板**必含三处文本输入**（新建任务 URL、设置-下载目录、列表搜索框——架构 §4.5 v2.1 裁定）× 微软拼音/搜狗 × 连续输入中文 30 字 → 不丢字、候选框跟随、Shift 中英切换正常、点别处正确打断；**同类覆盖**：设置页其余文本键（镜像 URL 模板、RPC 端口、代理）各抽 10 字；**网页页 IME 不计入本项**（见 #17）；且源码零每帧 `request_focus` | 按组合逐个手测并录屏；`git grep -n "request_focus"` 逐处复核是否在每帧路径（`update()` 内无条件调用即违例） | M2 | 部分（源码扫描可脚本化，输入效果必须真机） |
| 14 | 通知降级：AUMID 已注册 → Toast 显示「AriaHost」；未注册/写失败 → 降级到托盘气泡或面板条幅，**不静默丢弃**；`notify.backend` 强制值生效。**M2 用手工 `--write-aumid` 夹具覆盖 AUMID 已注册主路径；M3 由安装器真路径复验** | 正常装一次看 Toast 名称；手工 `ariahost.exe --write-aumid "<测试.lnk>"` 后从该 .lnk 启动看 Toast；删 AUMID 或用未注册路径装一次看降级；把 `notify.backend` 依次强制为各值，抽查 3 次下载完成 + 1 次失败 | M2/M3 | 否 |
| 15a | **M1**：依赖自足 + 非桌面环境——`dumpbin /dependents ariahost.exe` 无 `VCRUNTIME140.dll`/`MSVCP140.dll`；未装任何 VC++ Redistributable 的 **Server Core** 上 `--service` 与 headless 起得来、6800 可调、6875 可访问（**不测双击**——Server Core 无桌面）；桌面 Win10 1809/21H2/22H2 与 Win11 各跑一次 `--service`/headless。失败则退 D13-B 重新拍板 | `dumpbin /dependents target\release\ariahost.exe` 贴输出；干净 Server Core VM 起 `--service` + `curl 127.0.0.1:6800` + 打开 6875 | M1 | 部分（依赖项检查**已设为 CI 门禁**：`ci.yml` 的 `windows` job 用 vswhere 找 `dumpbin`，找不到或命中 `VCRUNTIME140`/`MSVCP140` 即失败，不再是 warning；Server Core 启动仍须人工） |
| 15b | **M2**：桌面首次启动——Win10 1809/21H2/22H2 与 Win11 干净环境（未装 VC++ Redistributable）双击 `ariahost.exe`/安装目录 exe → 出面板、托盘图标可见、可打开设置；**Server Core 不在本条范围**（见 #15a） | 每台干净 VM 双击 exe 一次，核对面板/托盘/设置页三项 | M2 | 否 |
| 16 | **第 9 条·面板形态**：列表对 3 个任务暂停/继续/删除，状态与 6800 查询一致；冷启动默认最小化到托盘（`ui.start_minimized=false` 时显示主窗）；双击托盘图标恢复面板；面板只读态（aria2 未就绪）不崩溃且给一键启动 | 建 3 个任务做三种操作，每步 `aria2.tellStatus` 对账；改 `ui.start_minimized` 两值各起一次；卸载 aria2 后启动看只读态 | M2 | 否 |
| 17 | **第 9 条·网页页形态**：`http://127.0.0.1:<web.port>` 添加 URL → 暂停 → 删除 → 历史可见；批量（≥3）暂停/继续；每任务高级选项改一项并生效；改 `web.port` 后一键打开指向新端口；端口被占时报 `AH-E3001`（web 子字段）且可改端口；secret 不出现在浏览器（页面源码与 devtools 网络面板检索无明文） | 浏览器逐项操作并对账 6800；用 `netstat -ano \| findstr :6875` 先占端口再启动；devtools Network 全局搜 secret | M2 | 否 |
| 18 | **第 9 条·第三方 RPC 形态（M1）**：用设置页 secret（headless 首启生成的 secret）连 `127.0.0.1:6800`，`aria2.getGlobalStat`/`addUri`/`tellStatus`/`pause` 均成功；错误 secret 与无 secret 均被拒；`aria2.rpc.listen=off` 时连接被拒 | 用 curl/Postman 带 `token:` 发 4 个方法；再发错 token 与空 token；最后切 `off` 重试 | M1 | 是（可脚本化，作为 M1 冒烟） |
| 19 | **N8 文档完备性（M3）**：`docs/dev/{setup,build,ci,release,style,test}.md` 六篇存在；`docs/README.md` 索引全链可达；`docs/dev/`、`README.md`、`docs/README.md` 无旧前缀/「验收 11 条」/「150MB」等过期口径残留（定版文档的历史修订记录除外）；新人按 `setup.md`→`build.md` 在干净环境 ≤30min 跑起来；未验证项均带标记 | 逐链点开；`Select-String` 扫过期口径；找一位未接触过仓库的人计时录屏走一遍 | M3 | 部分（链接与禁词可脚本化） |

### 3.1 真机取证手段（锁屏 / 无桌面也能用）

- **UIA 关窗可替代手点 X**：对窗口发 UI Automation 的 `WindowPattern.Close` **确实会走到 eframe 的 `close_requested`**（不是被 winit 吞掉）。探针实测：调完 **0.3s 内**进程退出、`window.json` 被写出（该文件只在 close 分支写）。因此 `ui.close_action` 三档可在锁屏下验收：`tray` → 进程仍在、窗口 `Visible=false`；`ask` → UIA 树里出现对话框文本；`exit` → 约 **4s** 内 `ariahost` 与 `aria2c` 一起消失。方法细节见 [build.md](build.md) §4.3。
- **内存只认外部采样**（口径不动）：release 构建、窗口真实打开、启动后 **60s 稳态**，用 `K32GetProcessMemoryInfo → PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize`（**按 PID 打开的句柄**，每 6s 一次取序列）。**不要**拿 `Get-Counter '\Process(ariahost)\Working Set - Private'` 当主口径——它按名字取实例，多实例时会取到别人的进程（见 §6.9）。**不得用程序自报的 `private_kb` 当闸门**——同一状态两者差 3 MB 量级，见 [build.md](build.md) §4.2。

## 4. 手工验收记录模板

每次验收在 `.scratch/` 或 PR 描述里贴这张表（不入 `docs/`，避免文档过期）：

```text
构建：AriaHost 0.1.0 (x64, commit abc1234)   系统：Win11 23H2 / Win10 22H2 / Server Core 2022
#1 无UAC+干净机：…    #2 引导态：…      #3a 助手本体 Manual+30s：…  #3b 设置页按钮：…
#4 托盘续传：…        #5 双实例：…      #6 清理硬边界：…
#7 四态文案：…        #8 回滚三一致：…  #9 lan 隔离：…
#10 中文/错误模型：…  #11 内存三档：托盘__MB / 开窗__MB / 下载中__MB（含归因）
#12 CI 出包+SHA256：… #13 IME：…        #14 通知降级：…
#15a 依赖+非桌面：…   #15b 桌面双击：…
#16 面板CRUD+托盘：…  #17 网页页+端口：…  #18 RPC secret：…  #19 文档：…
备注（失败项的现象、日志路径、截图名）：
```

## 5. CI 里的测试

- `ci.yml` 的 `windows` job 里的「单元 + 集成测试」步骤跑 `cargo test --workspace --locked`（单元 + 不需要外部二进制的集成）。**没有独立的 `test` job**：clippy/test/build 串行在同一个 Windows job 里，共用一次工具链安装与缓存。
- `makensis` 编译门禁跑在**同一个 `windows` job** 里、用真实载荷（真 exe + 真 ICO）。**没有独立的 `nsi-syntax`（ubuntu）job**——它当年用 1 字节 stub 在 Linux 上编译，给出的是假绿，已删除。
- 需要 aria2c 的集成测试不进默认 CI（runner 上没有受控的 aria2 版本）；M2 起可评估下载固定版本 aria2 后开启。
- 验收 #12 的「CI 出包」只能由 `release.yml` 证明：本地无法替代（本机无 makensis/工具链）。
- `clippy -D warnings` 与 `fmt --check` 是同级门禁：测试过了但 clippy 红，PR 一样不能合。
- **已跑绿的是自动化那部分**（2026-11-25）：`fmt` / `clippy` / `cargo test --workspace --locked` / 静态 CRT 快检（`dumpbin`）/ `makensis` 真实载荷编译 / `release.yml` 出包 + `SHA256SUMS.txt`。**验收本身按人工签字算**：#1、#19 待真机，#13 IME 待真机，#15b 待干净 VM——「CI 绿」不等于「验收通过」。

## 6. 观测纪律（多 agent 共享同一工作区时的硬规则）

这一节来自三次真实误判，不是预防性说教。

### 6.1 「红/绿」必须声明观测对象

报告里必须写清是哪一种：

| 观测对象 | 可信度 | 写法 |
|---|---|---|
| **隔离 worktree @ `<sha>`** | 高（只含该提交） | `worktree @ dbde5b0：cargo check --workspace --all-targets --locked → exit 0` |
| **CI runner @ `<sha>`** | 高（唯一真 MSVC） | `CI run 45542650461 @ c6e50bc → success` |
| **当前工作区** | **低**（混着别人未提交的改动） | 只有在 `git status --short` **为空**时才可作为证据 |

**只要工作区里有别人的未提交文件，任何"红"和"绿"都不作数。** 实例：`cargo test --workspace` 在脏工作区报 `E0515`，被归因到已提交的 `0344914`；隔离 worktree 复核该提交 **52 passed / 0 failed**，红的是别人正在改的 `logfile.rs`。

### 6.2 长期不可编译的半成品放自己的 worktree

共享工作区只允许**能编译的中间态**。一个未跟踪且编译不过的文件（如半成品 `diagnose.rs` 里三处 `ariyahost_*` 拼写）会让**所有人**无法编译，且错误信息指向"某个提交"，误导归因。

> 超过 ~30 分钟仍无法编译的半成品 → `git worktree add` 到自己名下，别留在共享树。

### 6.3 GUI 子系统程序取退出码必须用 `Start-Process -Wait`

`ariahost.exe` 是 `Subsystem=2 (GUI)`。PowerShell 对 GUI 程序**不等待**：

```powershell
& $exe --quit          # ← $LASTEXITCODE 是空值！不是 0，也不是 1
```

正确写法：

```powershell
$p = Start-Process -FilePath $exe -ArgumentList '--quit' -NoNewWindow -Wait -PassThru `
      -RedirectStandardOutput q.out -RedirectStandardError q.err
$p.ExitCode            # ← 真退出码
```

**所有自检脚本统一这个写法**；`cmd /c` 也不可靠（同样不回填）。

### 6.4 Windows 目标的 lint 必须单独跑一遍（Linux 目标看不见 `#[cfg(windows)]`）

已被咬过两次：`tray.rs` 的 `manual_range_contains`、`diagnose.rs` 的四条（`manual_c_str_literals` / `field_reassign_with_default` ×2 / `unnecessary_lazy_evaluations`）。两次都是 **CI 才红**，本地全绿——因为那些代码在 `#[cfg(windows)]` 里，Linux 目标根本不编译。

本地闸门（WSL 内，需要 `x86_64-pc-windows-gnu` 交叉工具链）：

```bash
ARIAHOST_SKIP_ICON=1 \
cargo clippy --workspace --all-targets --locked \
  --target x86_64-pc-windows-gnu -- -D warnings
```

`ARIAHOST_SKIP_ICON=1` 的来历：`build.rs` 用 `winresource` 嵌图标，而交叉到 windows 目标时它会把路径里的 `/` 转成 `\`，`windres` 因此打不开 ICO、整个 gate 被一个**与被测代码无关**的资源步骤挡住。该变量只跳过资源嵌入，**不影响任何 Rust 代码的编译与 lint**；正式构建不设它。

> 这条闸门不替代 CI：`-gnu` 与 `-msvc` 仍有差异（`cfg(target_env)` 分支）。它的价值是**把"只有 Windows 才编的代码"提前拉到本地**，把一轮 10 分钟 + 2× 计费的 CI 往返变成 40 秒本地循环。

---

### 6.5 自检入口不得提供可变后门

隐藏自检（`--panel-selftest`、`--log-selftest`、`--panel-selftest --drive …`）**只能读状态、只能注入输入事件**。

- ✅ 允许：`settings_state()` 只读、`RawInput` 注入文本/点击（走真正的 `ui()` 路径）
- ❌ 禁止：直接 `draft_mut()` 改值、直接调 `save()`

理由：测试若能用后门改状态，它就**证明不了 UI 接线**——而 UI 接线正是这类测试存在的唯一目的。

### 6.6 安全与"错误值"类缺陷只认受控实验

**"跑一遍看起来正常"证明不了脱敏、权限、路径解析、版本上报这几类是对的。** 必须人为构造最坏输入，再对**产物**做机械扫描。

做法：构造一个受控环境（伪 `%APPDATA%` / `%LOCALAPPDATA%` / `%ProgramData%`），把凭据塞满所有可能的位置，然后：

```powershell
# 1) 跑一次真实操作（例如 --diagnose）
# 2) 对产物逐文件机械扫描原始秘密串，而不是人眼看输出
$zip = "AriaHost-diagnose-....zip"
Expand-Archive $zip -DestinationPath $tmp -Force
Get-ChildItem $tmp -Recurse -File | ForEach-Object {
    $hit = Select-String -Path $_.FullName -Pattern 'SUPERSECRET1234567890','PROXYPASS99','TRACKERKEY777' -SimpleMatch
    if ($hit) { "LEAK: $($_.Name):$($hit.LineNumber)" }
}
```

**已经用这个方法抓到两条真实漏洞**（2026-11-25）：

1. `aria2` 回显的 `secret=…` 形状**没被脱敏**——原有三种形态的单测全绿，漏的是第四种。修法：`common::redact` 补 `secret:` / `secret=` / `passkey=` 标记，诊断侧再按"已知字面值"scrub 一遍。
2. `os_version` 报 **6.2/9200（Windows 8）**——`GetVersionExW` 读的是**进程 manifest**，本程序没声明就谎报。改动态解析 `ntdll!RtlGetVersion` 后报 `10.0.26200`。

第 2 条值得单独记：**"错值"比"没值"更坏**——用户贴上来我们会被引向完全错误的方向。

**适用范围**：脱敏/权限/路径解析/版本上报/环境探测。这几类改动**必须配一个受控实验**，单测不代替它。

### 6.7 被测 exe 必须从工作区**外**启动（本沙箱会伪造「起不来」）

**现象（2026-11-25 双重实测）**：同一份 `ariahost.exe`

| 启动位置 | `Start-Process` 结果 |
|---|---|
| 仓库内（`F:\git_project\wasm\.scratch\…`） | **立刻失败**，`The operation was canceled by the user`（ShellExecuteEx 返回 1223），约 0.8s |
| `%TEMP%\ahwr-probe\`（同一份二进制，sha256 一致） | 正常起窗，`alive=True`，pid 正常 |

交叉排除：`F:` 是本地盘（DriveType=3）、目录无隐藏属性、exe 无内嵌 manifest、未签名、无 MOTW；**旧产物**复制进 `.scratch` 同样失败 → 不是产物问题，是路径问题。

**根因**：DSH 沙箱对「从受管工作区发起的 ShellExecute 提权/启动」有拦截，表现为 `1223`（`ERROR_CANCELLED`）——**和用户点了取消是同一个错误码**（见 `known-limitations` L8）。

**纪律**：
1. GUI/ShellExecute 类验收，先把 exe 拷到 `%TEMP%`（或工作区外的本地盘）再启动，**并核对拷贝前后的 sha256**。
2. 直接调用（`& exe --selftest`）不受影响，别因为直接调用能跑就以为 `Start-Process` 也能跑。
3. 看到 `1223` 先怀疑这条，别记成「产品起不来」。

### 6.8 控制管道名是全机唯一的：并发验收会互相杀进程

`\\.\pipe\ariahost-ctl` **不含用户/会话后缀**（架构 §3.1 的定版名字）。因此：

- 同一台机上只要有两个会话在跑验收，A 的 `--quit` 会命中 B 的面板（实测：某轮面板被外部结束两次、无崩溃记录、静置探针完好）；
- 生产上意味着**多用户（RDP / 快速用户切换）**场景下会串——`%LOCALAPPDATA%\AriaHost\panel.lock` 是每用户的，两个用户可各跑一个面板，但管道只有一个。这是待修缺陷（已立单），不是「测试环境噪音」。

**并发验收协议（临时）**：动手前在 `.scratch/m3/panel-lock/` 建 `<你的名字>.lock`（内容写用途与起始时间），用完删除；需要独占面板/管道的用例，先确认目录里没有别人的锁。**这不能修产品缺陷，只能避免互相污染证据。**

### 6.9 内存必须按进程句柄取数（禁用按名字的计数器）

**事故（2026-11-25）**：一份验收脚本用

```powershell
Get-Counter '\Process(ariahost)\Working Set - Private'
```

取私用工作集。该计数器**按实例名**返回，不带 PID；机器上有残留实例时 `[0]` 可能是**别的进程**的数。据此报出「服务页 110.6MB 超 100MB 闸门」。

**这个数被修正了两次才收敛**（这条比结论本身值钱）：

| 轮次 | 口径 | 读数 | 问题 |
|---|---|---|---|
| 1 | 按名计数器 | 110.6MB | 取到别人的进程 → 假失败 |
| 2 | 按句柄 + 但机器上仍有残留实例 | 99.1MB | 仍有污染 → 仍不准 |
| 3 | 按句柄 + 采样期实例数恒 1 | **72.0MB** | 收敛 |

教训：**换口径不足以解决污染，还要同时控制「采样期只有一个实例」**，并把实例数写进输出。

**规则**：

1. 一律用 `K32GetProcessMemoryInfo → PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize`，且句柄来自**你启动的那个进程**（`OpenProcess(pid)` / `Start-Process -PassThru`），不是按名字找。

   **结构体必须是 `EX2`**（`MEMORY_COUNTERS_EX2`，Win8+）。`PROCESS_MEMORY_COUNTERS_EX` **没有** `PrivateWorkingSetSize` 字段——照 `EX` 的字段表顺手改读 `WorkingSetSize`，拿到的是**总工作集**（含 Vulkan/Intel 驱动映射、字体文件映射等共享页），量级差 2–3 倍。本文旧版第 233 行就误写成 `EX`，2026-11-26 我照它抄出一次 **177 MB 的「破闸门」**，实为总工作集，当场误发给成员当回归处理。参考实现：`.scratch/m3/acceptance/membase-three-scenarios.ps1` 第 25–51 行。

   **取数后必须交叉核对：`PrivateWorkingSetSize ≤ PrivateUsage`**。不成立就说明结构体尺寸/字段偏移读错了，先修脚本再谈结论——不要把矛盾数字报出去。
2. 采样期必须记录**同时存在的同名实例数**；`>1` 就 INCONCLUSIVE。
3. 口径写死在输出里：`PWS max = XXX KB（句柄 PID=NNNN）/ 余量 = XXX KB`。
4. **「没采到数据」永远不是「通过」**：采样数 < N、进程中途退出、变量为空 → 判 `INCONCLUSIVE`，**不得**判 PASS（已发现 4 个脚本犯过 `$null -lt 闸门值 → True` 这类空判）。

### 6.10 自检/演示模式的数字不能当产品基线

`--seed-rows N`（自检模式）跳过 config/RPC/web/log/tray 等整套常驻设施，运行时**不是同一套**，内存必然偏低。因此：

- **闸门判定只认「正常启动的 idle 面板」**；
- 自检模式的数字只能在**同模式内**横向比较（例如同一模式加 200 行 vs 0 行，验证虚拟化是否吃内存）。

**当前已验证的产品基线（2026-11-25，`K32` 按句柄、实例数恒 1、60s、同一产物）**：

| 配置 | PWS max | 说明 |
|---|---|---|
| `auto`（串行探测，胜出 Vulkan） | 72.0MB | 窄化枚举**之前**的旧产物 |
| 显式单后端 Vulkan | **51.6MB** | 枚举范围本身值 **20.4MB** |
| 显式单后端 DX12 | **116.4MB** | **破闸门**；无 Vulkan ICD 的机器会走到这里 |

→ 结论：**「后端」与「枚举范围」是两个独立开销源**；`auto` 必须窄化枚举，且 DX12 路径的内存仍需单独压缩（见 `known-limitations`）。任一条没做完，都不能说「内存闸门达标」。

### 6.11 判据演化必须写进文档，不许无声放宽

判据在执行中被证明**写错了形态**时，正确处理是四步：① 写清原判据的**原文**；② 写清它为什么不可满足（环境限制 / 实现约束 / 安全不变量，三者性质不同）；③ 给出替代证据，并说明替代证据的覆盖面**不窄于**原判据；④ 落在文档里，不能只在聊天里达成一致。

**实例（task-58，2026-11-25）**：原判据 2 是「用 `--panel-selftest` 跑一次，再检查落盘日志」。但 `--panel-selftest` **有意不落盘**（`cli::inits_file_log` 只对 `Panel | Service` 返回真）——这是**安全不变量**：并发 CI 自检会互踩同一个日志文件，短命自检会污染用户日志。替代证据拆成两条：`--log-selftest --dir <隔离目录>` 验写入器与脱敏（exit 0、500 行、`leak=false`、落盘可见 `token:***`）；真面板（隔离 `APPDATA`、45 s）验探针轨迹上盘 + 写盘有界。**替代证据比原判据更强**：原判据只要求「轨迹在文件里」，后一条还证明「日志不会把磁盘写爆」（30 s 只多 2 行，每帧写则约 1800 行）。逐项账见 `build.md` §4.4。

### 6.12 退出有两条路径，并发时靠「幂等 + 单点 exit」而不是靠互斥

`--quit` 的收尾有两条路：**帧路径**（`draw()` 消费 `take_quit_request()` 后停核心、关窗）与**兜底路径**（ctl 线程等 2s 没等到消费就自己收尾）。两者**可能同时命中**——最典型的是取证开关 `ARIAHOST_QUIT_SIMULATE=fallback`（旧名 `ARIAHOST_QUIT_FALLBACK_SIMULATE=1` 仍兼容）：它短路的是「那 2s 等待」，不是「让 UI 永不出帧」，所以日志里会**同时**出现兜底段与 `走退出档`。

看到双份日志**不是竞态**，别当 bug 修。语义是：

- `CoreRuntime::stop` **幂等**：第二次调用看到已停止就直接返回，不会二次 `save session`、不会二次 kill；
- `exit(0)` **只由兜底路径负责**（帧路径只在没有兜底时才会走到 `ViewportCommand::Close`），所以进程退出既不会被提前掐断，也不会有人替它决定退出码；
- **会话不丢**：兜底走的是同一条 `CoreRuntime::stop`（三段优雅停止），不是 `kill`。

生产路径下两者**不会**并发：兜底只在「2s 内没人消费」时触发，而那意味着帧路径没在跑。所以这条只影响取证，不影响用户。

### 6.13 真实条件难出现时，构造等价条件——「未复现」不算证据

两类场景，同一条纪律：

1. **条件可以人工造**（安装器覆盖写竞态，task-64）：自然竞态在 20 连跑里一次没出现（守卫命中 **0/20**，平均 2887 ms）。于是用**独占锁**把「目标 exe 被别人持有写句柄」这个等价条件造出来 → `exit 1`、**8830 ms**（≈ 8 s 上限）、**目标字节未变**；解锁后 `exit 0`。**「没抓到」只说明概率低，不说明实现正确。**
2. **现象像缺陷但环境不允许复现**（安装器注册表清除，task-66）：本沙箱的 NSIS 写不进注册表。这时先做**最小探针**（独立脚本只跑 `WriteRegStr HKCU` / `HKLM`，两者均 ERR）证明这是**环境限制**，再把它记为「**未取证**」，而不是「缺陷」——否则会动手去改一段本来就正确的实现。

**判据**：

| 你手上的证据 | 该写什么 | 不许写什么 |
|---|---|---|
| 构造出等价条件并实测 | `已用 <构造方式> 取证` | —— |
| 跑 N 次没出现 | `N 次未自然复现；风险仍开放` | 「应该没问题」 |
| 最小探针证明环境不允许 | `未取证（环境限制已用最小探针证明）` | 「缺陷」「已修」 |
| 代码看着对但没跑过 | `未验证` | 「已验证」「理论正确」 |

延伸：**「未取证 ≠ 缺陷」**与「未验证 ≠ 已通过」是同一条纪律的两种措辞，见于 `known-limitations.md` 的 L11/L12。

### 6.14 判据点名的字符串，必须能在界面上被看见

判据里写「应显示 X」时，X 必须落在**用户视线能读到的那一行**里的字面内容上——不是日志里，也不是另一个页面里的完整原因。紧凑行做截断是正常的，但截断恰好把这轮判据点名的那一段吃掉，这个判据就等于没测过。

**实例（task-54 判据 R3，2026-11-25）**：R3 要看 `external` 指向不存在路径时的可见提示。实际 `short_reason` 在第一个「：」处断句，紧凑行只剩 `未就绪（找不到 aria2c）`——**点名要看的 `external_path: C:\...\NoSuchDir\aria2c.exe` 被丢掉了**（日志与首屏引导卡里倒是有）。于是同一条场景必须分两层给结论：

| 层 | 结论 | 依据 |
|---|---|---|
| 语义 | PASS | 显示「未就绪」，没有静默降级，日志带 `AH-E2001` |
| 文案 | FAIL → 已修 `61aa6ca` | `short_reason` 不再把「：」当断句（只按换行 / `；` / `。` 断，仍 40 字符硬截）；回归测试 `missing_external_core_keeps_the_external_path_qualifier` |

**操作含义**：写判据时把点名要看的那一段**原样贴进判据**；复验时先用 UI Automation 抓那一行文本做子串比对，再谈其它。

### 6.15 自述型字段要「值对」+「来源对」，缺一不算过

诊断类字段（`os_version`、后端选择、核心来源、权限态）自带 `source` / `branch` 这类**来源标记**。判据必须同时看两件：

1. **值**是否符合预期；
2. **来源**是否是你想走的那条路。

只对值不对来源，会出现两种假绿：**碰巧对**（`GetVersionExW` 在某些系统上也报真实 build）与**没走新路**（改动没编进去 / 解析失败落回退，而回退值恰好与期望相同）。

**实例（task-67，2026-11-25）**：`os_version` 在 MSVC 产物上报 `{6,2,9200, fallback-getversionex}`，真因是**我们自己把窄串当宽串传**（`c"ntdll.dll".as_ptr().cast::<u16>()`），`GetModuleHandleW` 恒失败 → 永远落回退。修复后在 MSVC 产物上验的是**两件事**：

```json
"os_version": { "major": 10, "minor": 0, "build": 26200, "source": "ntdll-rtlgetversion" }
```

```
[osver-probe] handle=0x7ffa1ba40000 handle_err=Some(0) proc=Some(0x7ffa1baea620) proc_err=Some(0) rtl_status=0 branch=ntdll
```

`branch=ntdll` 而不是 `fallback`，才算过；只看 `build=26200` 无法证明走的不是碰巧报对的回退。

**操作含义**：凡「多路回退 + 自述来源」的实现，**判据里必须点名来源值**；探针日志（哪怕是 stderr 一次性输出）要留在脚本里，别为了干净删掉——它是唯一能证伪「碰巧对」的东西。

---


### 6.16 每个**架构目标**都要真编一次；packed 结构体字段禁取引用

两条同源教训，都来自 2026-11-26 首次 32 位真跑（release run `45663151807`）。

**（1）x64 绿不代表 i686 绿，且 `check` 就能抓住一部分**
同一次双腿构建里，`构建 x64 per-user` 全绿、`构建 x86 per-user` 红在 Rust 编译。**架构目标是独立的观测对象**，与 §6.4 的「目标 OS 单独跑」并列：`x86_64-pc-windows-msvc` 通过不证明 `i686-pc-windows-msvc` 通过。而这类错误（`E0793`）在 `cargo check --target i686` 阶段就会报——**所以「只做 check 不做 build」并不必然漏掉它，漏掉它的是「根本没对 i686 编过」**。别把「我们跑了 check」当成「我们验过 32 位」，也别把「32 位要跑 test」当成能替代「32 位要编一次」——两者是不同关卡。

**（2）同一类型在不同 target 上的 `repr` 可能不同——packed 只在 32 位成立**
本节初稿把这条写成了「两种架构上都是 1 字节对齐，x64 上是静默 UB」——**这是错的，已被成员按 crate 原文证伪并更正**。实证（`windows-sys 0.59.0`，`src/Windows/Win32/UI/Shell/mod.rs`，两条同名定义并排存在）：

```rust
#[repr(C)]                #[cfg(any(target_arch="aarch64", target_arch="arm64ec", target_arch="x86_64"))]  // L5680–5681
pub struct NOTIFYICONDATAW { … }

#[repr(C, packed(1))]     #[cfg(target_arch = "x86")]                                                      // L5709–5710
pub struct NOTIFYICONDATAW { … }
```

于是同一行 `fill_wide(&mut data.szTip, …)`：**x64 上完全合法**（那份定义是 `repr(C)`，不 packed），**x86 上直接 `error[E0793]`**。C 头文件里 `#include <pshpack1.h>` 的「按架构条件打包」被 windows-rs 忠实映射成了 `cfg` + 两种 `repr`。

**结论仍然是「不对 packed 字段取引用」，但理由要写准**：不是为了「消除 x64 的 UB」（x64 本来没有 UB），而是①一份源码要在两个 target 上都成立；②同族结构体未必都有这种分层（可能某个全架构 packed），这个写法不会再踩一次。禁止 `#[allow(...)]` 绕过；**「x64 编得过」不能当「写法安全」的证据**——它只说明 x64 那份定义不是 packed。

**禁用模式扫描**（新代码合入前跑一次，命中即人工核对）：
```bash
grep -nE '&(mut )?(data|nid|nidw|obj)\.[a-zA-Z_]+' crates/ariahost-ui/src/
```

### 6.17 每条机验判据必须带「改前基线」——基线已达标的判据是假绿

派活前写下判据时，必须同时写**改前实测值**。若改前就已经满足，这条判据**证明不了任何事**：要么删掉，要么换成能区分「改前/改后」的新口径。

2026-11-26 一次任务书里连踩两条（同一批 UI 施工，我写的验收）：

| 我写的判据 | 真实基线 | 后果 |
|---|---|---|
| 「旧 hex 状态色（`#6ECD82` 等）命中 = 0」 | 改前就是 **0** —— 代码用的是 `Color32::from_rgb(110,205,130)`，源码里根本没有 hex 字面量 | 干活的人只要不写 hex 就"零命中"，颜色一个没改也算过 |
| 「字号字面量 `.size(` 命中 = 0」 | 改前就是 **0** —— 字号走 `FontId::new(pt, …)` 与 `TextStyle`，不用 `.size()` | 同上 |

**替换后的正确写法**（同一件事，能区分改前改后）：

```bash
# 令牌外泄：raw color 构造器（除 theme.rs 外应为 0；改前 17：app.rs 13 + view.rs 4）
grep -rnE 'Color32::from_(rgb|rgba_unmultiplied|rgba_premultiplied|gray)' crates/ariahost-ui/src/ | grep -v '/theme.rs:'
# 页面内水平线（改前 7 处，目标 0；面板壳的 .show_separator_line 不在此列）
grep -c 'ui\.separator()' crates/ariahost-ui/src/app.rs
```

**判定规则**：

1. 判据必须是「改前不满足 / 改后满足」的形式；写不出基线就先测一遍再写。
2. 结构性判据（「不许有 X 这种写法」）要说明**改前 X 是什么形态**，否则下一个人换一种写法就绕过去了。
3. 运行期才成立的东西（字体链、后端选择、内存）**不许用 grep 当判据**，要给运行期输出（见 §6.15 自述型字段要「值对」+「来源对」）。
4. 判据被证明形态写错时按 §6.11 处理：写清原判据错在哪、基线多少、新判据怎么区分，再改，不许无声换标准。