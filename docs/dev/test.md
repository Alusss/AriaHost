# 测试策略

> 适用：M1 起。当前仓库没有代码，**没有任何测试被执行过**；本文定义「怎么写、怎么跑、按什么验收」。
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
| 12 | CI 出包：`v*` tag 触发 `release.yml` → 产出 `AriaHost-<ver>-x64-setup.exe` + `SHA256SUMS.txt`，notes 列包名与校验和。**x86 在 M4 之前不进流水线**（`release.yml` 里整个 job 注释掉，**不留 `continue-on-error`**：失败 job 一样烧 2× 分钟，且静默吞掉比不跑更坏）；改动范围判定用 `changes` job 而**不是** `on.push.paths-ignore`（后者命中时 workflow 根本不触发，PR 上连状态都没有）。首次达成 = M3 | 打 tag 后看 Actions 绿；下载 artifact 与 Release 附件，`Get-FileHash -Algorithm SHA256` 与 `SHA256SUMS.txt` 逐个比对；确认 PR 上纯文档改动时编译 job 是 skipped 而 `ci-gate` 仍绿 | M3 | 是（流水线自身即证据） |
| 13 | IME 真机（M2 门禁）：面板**必含三处文本输入**（新建任务 URL、设置-下载目录、列表搜索框——架构 §4.5 v2.1 裁定）× 微软拼音/搜狗 × 连续输入中文 30 字 → 不丢字、候选框跟随、Shift 中英切换正常、点别处正确打断；**同类覆盖**：设置页其余文本键（镜像 URL 模板、RPC 端口、代理）各抽 10 字；**网页页 IME 不计入本项**（见 #17）；且源码零每帧 `request_focus` | 按组合逐个手测并录屏；`git grep -n "request_focus"` 逐处复核是否在每帧路径（`update()` 内无条件调用即违例） | M2 | 部分（源码扫描可脚本化，输入效果必须真机） |
| 14 | 通知降级：AUMID 已注册 → Toast 显示「AriaHost」；未注册/写失败 → 降级到托盘气泡或面板条幅，**不静默丢弃**；`notify.backend` 强制值生效。**M2 用手工 `--write-aumid` 夹具覆盖 AUMID 已注册主路径；M3 由安装器真路径复验** | 正常装一次看 Toast 名称；手工 `ariahost.exe --write-aumid "<测试.lnk>"` 后从该 .lnk 启动看 Toast；删 AUMID 或用未注册路径装一次看降级；把 `notify.backend` 依次强制为各值，抽查 3 次下载完成 + 1 次失败 | M2/M3 | 否 |
| 15a | **M1**：依赖自足 + 非桌面环境——`dumpbin /dependents ariahost.exe` 无 `VCRUNTIME140.dll`/`MSVCP140.dll`；未装任何 VC++ Redistributable 的 **Server Core** 上 `--service` 与 headless 起得来、6800 可调、6875 可访问（**不测双击**——Server Core 无桌面）；桌面 Win10 1809/21H2/22H2 与 Win11 各跑一次 `--service`/headless。失败则退 D13-B 重新拍板 | `dumpbin /dependents target\release\ariahost.exe` 贴输出；干净 Server Core VM 起 `--service` + `curl 127.0.0.1:6800` + 打开 6875 | M1 | 部分（依赖项检查**已设为 CI 门禁**：`ci.yml` 的 `windows` job 用 vswhere 找 `dumpbin`，找不到或命中 `VCRUNTIME140`/`MSVCP140` 即失败，不再是 warning；Server Core 启动仍须人工） |
| 15b | **M2**：桌面首次启动——Win10 1809/21H2/22H2 与 Win11 干净环境（未装 VC++ Redistributable）双击 `ariahost.exe`/安装目录 exe → 出面板、托盘图标可见、可打开设置；**Server Core 不在本条范围**（见 #15a） | 每台干净 VM 双击 exe 一次，核对面板/托盘/设置页三项 | M2 | 否 |
| 16 | **第 9 条·面板形态**：列表对 3 个任务暂停/继续/删除，状态与 6800 查询一致；冷启动默认最小化到托盘（`ui.start_minimized=false` 时显示主窗）；双击托盘图标恢复面板；面板只读态（aria2 未就绪）不崩溃且给一键启动 | 建 3 个任务做三种操作，每步 `aria2.tellStatus` 对账；改 `ui.start_minimized` 两值各起一次；卸载 aria2 后启动看只读态 | M2 | 否 |
| 17 | **第 9 条·网页页形态**：`http://127.0.0.1:<web.port>` 添加 URL → 暂停 → 删除 → 历史可见；批量（≥3）暂停/继续；每任务高级选项改一项并生效；改 `web.port` 后一键打开指向新端口；端口被占时报 `AH-E3001`（web 子字段）且可改端口；secret 不出现在浏览器（页面源码与 devtools 网络面板检索无明文） | 浏览器逐项操作并对账 6800；用 `netstat -ano \| findstr :6875` 先占端口再启动；devtools Network 全局搜 secret | M2 | 否 |
| 18 | **第 9 条·第三方 RPC 形态（M1）**：用设置页 secret（headless 首启生成的 secret）连 `127.0.0.1:6800`，`aria2.getGlobalStat`/`addUri`/`tellStatus`/`pause` 均成功；错误 secret 与无 secret 均被拒；`aria2.rpc.listen=off` 时连接被拒 | 用 curl/Postman 带 `token:` 发 4 个方法；再发错 token 与空 token；最后切 `off` 重试 | M1 | 是（可脚本化，作为 M1 冒烟） |
| 19 | **N8 文档完备性（M3）**：`docs/dev/{setup,build,ci,release,style,test}.md` 六篇存在；`docs/README.md` 索引全链可达；`docs/dev/`、`README.md`、`docs/README.md` 无旧前缀/「验收 11 条」/「150MB」等过期口径残留（定版文档的历史修订记录除外）；新人按 `setup.md`→`build.md` 在干净环境 ≤30min 跑起来；未验证项均带标记 | 逐链点开；`Select-String` 扫过期口径；找一位未接触过仓库的人计时录屏走一遍 | M3 | 部分（链接与禁词可脚本化） |

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
- 另有 `nsi-syntax`（ubuntu）：用 1 字节 stub 只跑 `makensis`，挡安装脚本的编译期错误，约 1 分钟 1× 计费。它不在验收编号里，属预防性闸门。
- 需要 aria2c 的集成测试不进默认 CI（runner 上没有受控的 aria2 版本）；M2 起可评估下载固定版本 aria2 后开启。
- 验收 #12 的「CI 出包」只能由 `release.yml` 证明：本地无法替代（本机无 makensis/工具链）。
- `clippy -D warnings` 与 `fmt --check` 是同级门禁：测试过了但 clippy 红，PR 一样不能合。
- 目前**没有任何一条验收被执行过**（无代码、无安装包）；#1/#12 的首次真跑在 **M3**，#15a 在 **M1**，#15b 在 **M2**。本节所有结论**未经 cargo/makensis 验证**。
