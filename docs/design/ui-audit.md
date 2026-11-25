# AriaHost 界面逐屏审计（基线 6561e02）

> 配套：[ui-spec.md](ui-spec.md)（设计系统规格）。本文件只描述**现状有什么问题、违反哪条规格、怎么改**。
> 全部结论都基于两类证据：① 源码（`crates/ariahost-ui/src/**`，基线 `6561e02`；审计期间 builder-ui 的 task-71 已合入，涉及窗口标题/打开目录/关窗判定，与本文件条目无冲突，凡受影响处已就地标注）；② 真实窗口截图（`.scratch/design/shots/`）。
> 行号会随并行改动漂移，**定位以「函数名 + 代码片段」为准**。

## 0. 结论先行

1. **这不是审美问题，是三处功能缺陷叠在审美上**（5 条 blocker 全部落在下面三处）：
   - 面板根 `Ui` 未铺底 → 顶栏与整个设置页落在交换链清屏色 `#080808` 上，而文字仍是浅色主题的 `#505050`／`.weak()` 的 `#202020`：实测 **2.48:1** 与 **1.23:1**，等于看不见（G-1、P-1）。
   - 状态主文案不折行 → 一条长错误串把右边界顶穿，并把「启动 aria2」主按钮挤出屏幕（M-1，截图 `13-core-guide.png`）。
   - 760×480 最小窗下，工具栏按钮组溢出右缘，最右「退出」被窗口边缘裁切（M-9，截图 `14b-toolbar-zoom.png`）。
   - （第 5 条 blocker 是根因性的：全库没有样式层，G-2。）
2. **「字号大小不一」有确定根因**：全库没有任何样式层（G-2），字号只有 egui 出厂的 `9 / 13 / 18`，另有 3 处 `.size(20.0)` 与 3 处 `.heading()` 绕过阶梯；`.small()` 用了 56 次，其中大量承载**唯一信息**（表头、状态词、错误原因、下一步）。9pt 汉字在本机实测行距仅 12px，属于糊。
3. **「颜色对比差」是可量化的事实**：18 处硬编码色在浅色主题下全部落在 **1.53–2.83:1**（需要 3.0–4.5）；Banner 文字与它自己的 14% 底色只有 **1.22–2.10:1**；`.weak()` 当次要文字色把 7.59:1 压到 **3.72:1**。逐项数字见 `.scratch/design/contrast-current.md`。
4. 全局统一性差在「同一语义多种实现」：状态徽章有 3 套画法（`[文字]` 彩色 / 20pt 大数字 / 9pt 小字），按钮宽度 104/116/自适应 三种，标题 18pt 出现两次，卡片边界靠 `faint_bg_color`（浅色下 `#FDFDFD`，与页面底 1.04:1，等于没有边界）。

统计：**blocker 5 / major 24 / minor 21（共 50 条）**。

## 1. 证据与可信度

| 项 | 内容 |
|---|---|
| 截图来源 | `.scratch/m3/artifact-v012-final/ariahost.exe`（CI 产物，19,955,712 B，2026-11-25 21:38，版本 0.1.2） |
| 抓取方式 | `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2)` 抓窗口位图，未裁剪、未调色（脚本：`.scratch/design/shot.ps1`） |
| 运行参数 | ①`--panel-selftest --frames 100000 --seed-rows 200`（主界面 200 行假数据）；②同上无 `--seed-rows`（空态）；③`APPDATA`/`LOCALAPPDATA` 指向空目录（首屏「核心未就绪」） |
| 环境 | Windows 浅色主题（`HKCU\...\Personalize\AppsUseLightTheme=1`），客户区 1200×760 |
| 像素采样 | 用 `.scratch/design/wcag.py` + 直接 `Bitmap.GetPixel` 取样（文中引用的 hex 均为实测像素） |
| 行号口径 | 本文每条都给了「函数名 + 代码片段」，**行号只是路标**：它取自读取时刻的工作副本（2026-11-26 00:15–00:35，`app.rs` 当时 4013 行）。此后 builder-ui 并行改动让 `app.rs` 整体位移（`git diff` 显示 +204/−24），请用函数名定位 |
| **未验证** | ①深色主题未截屏（改系统设置才能稳定复现，没动用户的系统设置）；②服务页只截到「读取中」（SELFTEST 不连 SCM）；③设置页只截到「下载」组；④改后截图无法本机产出（无 MSVC 链接器，GUI 只能在 CI 产物上验） |

截图清单（`.scratch/design/shots/`）：

| 文件 | 内容 |
|---|---|
| `01-main-panel.png` | 主界面 + 200 行假数据（工具栏/表格/斑马纹/行内按钮） |
| `01b-top-zoom.png` | 主界面顶栏 4× 放大：`#080808` 底 + `#505050` 字（G-1 铁证） |
| `07b-settings-download.png` | 设置页（整页黑底） |
| `07d-settings-top-zoom.png` | 设置页顶部 2× 放大（同上，看清文字颜色） |
| `11-service.png` | 服务页「读取中」态 |
| `12-firstrun-nocore.png` | 主界面空态（核心可用、aria2 已停止） |
| `13-core-guide.png` | 首屏「核心未就绪」卡片（长文本破版） |
| `14-main-minsize.png` | 760×480 最小窗（工具栏重叠/裁切） |

## 2. 全局问题（G）

| ID | 严重度 | 位置 | 现象（可观测） | 违反 | 修法 |
|---|---|---|---|---|---|
| G-1 | **blocker** | `app.rs::PanelApp::draw` 开头 `ui.horizontal(\|ui\| { ui.heading(format!("{PRODUCT_NAME} {VERSION}")); … })`——在 `Panel::top` 之前直接画在根 `Ui` 上；`show_settings` 整个页面也直接画在根 `Ui` | 顶栏区域实测像素 `#080808`，文字 `#505050`（2.48:1）、`.weak()` 部分 `#202020`（1.23:1）；设置页 90% 面积同样如此 | spec §6.1 规则 1、§8.3 | 用 `Frame::NONE.fill(bg_base)` 包住整个根内容（或把顶栏改成 `Panel::top`），两套主题都铺底；验收：截图里不再出现 `#080808` |
| G-2 | **blocker** | 全 `crates/ariahost-ui`：`set_style` / `set_visuals` / `TextStyle` **0 命中** | 字号只有 9/13/18 + `.size(20.0)`×3；没有主题色、没有间距 token；任何「统一」都无从谈起 | spec §2/§3/§4/§8.1 | 新建 `theme.rs::install(ctx)`，按 §8.1 骨架装两套 `Style` |
| G-3 | major | 15 处 `.strong()`（如 `show_status` 的 `RichText::new(&view.headline).strong()`、`show_task_table` 表头） | egui 的 `strong()` 只换颜色不改字重（`widget_text.rs:474-480`），所以「加粗」= 变深一档灰；标题与正文的差别全靠字号跳 | spec §0-3、§2.3 | 不再用 `strong()` 表达层级；按 §2.3 用字号/颜色/间距/位置建层级 |
| G-4 | major | 42 处 `.weak()` | 次要文字被 `gamma_multiply(0.6)` 压暗：`#808080` on `#F8F8F8` = **3.72:1**（正文需 4.5）；在黑条上 1.23:1 | spec §2.2 规则 4 | 次要文字改 `text2` token；`.weak()` 仅留装饰 |
| G-5 | major | `view.rs::tone_color`（4 个 tone 色）+ `app.rs` 里 13 处 `Color32::from_rgb` 与 1 处 `Color32::GRAY`：`show_service_card`（状态色/结果提示/查询失败）、`show_tasks`（操作提示）、`status_color`、`service_state_color` | 全部是按深色底调的浅色，浅色主题下实测 1.53–2.83:1，全部 FAIL | spec §4.3 规则 1 | 全部收进 `theme.rs` 的语义 token（§4.1），按 tone 取 `*_text`；清单用 `Select-String 'Color32::from_' crates\ariahost-ui\src` 自查 |
| G-6 | major | 无 `set_theme`；`ThemePreference` 默认 `System`（`egui/memory/theme.rs:75-76`） | 深/浅两套 `Visuals` 都是出厂默认，而硬编码色只对深底成立 → 浅色主题下「半成品」 | spec §4.1 | `install()` 里显式 `set_theme(System)` 并 `style_mut_of(Dark/Light)` 各装一遍 |
| G-7 | minor | `.small()` 56 处，含表头（`RichText::new(title).strong().small()`）、状态词、错误原因、下一步 | 9pt 中文：本机实测行距 12px（`12-firstrun-nocore.png` 状态明细 y=95/107/119/131），汉字糊 | spec §2.1（下限 12pt） | 12pt 兜底；承载信息的文字另提到 14pt |
| G-8 | minor | `add_space(2.0/4.0/6.0/8.0/10.0/24.0)` 共 30 处，值随手取 | 垂直节奏没有基准，同屏出现 2/4/6/8/10 五种间距 | spec §3.1 | 统一到 `SP_1..SP_7` |

## 3. 主界面（任务列表）

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| M-1 | **blocker** | `app.rs::show_status`：`ui.horizontal(\|ui\| { badge(ui,&view); ui.label(RichText::new(&view.headline).strong()); … right_to_left(自右画按钮) })` | 主文案在水平布局里**不折行**：长串只被窗口裁掉（`13-core-guide.png` 里 `…versions\ 下没有 adu` 断在右缘），右侧「启动 aria2」按钮被挤出可视区 | spec §6.5 规则 3 | 先画右对齐按钮，再画文本并 `.wrap()`；长句被迫折成 2 行而不是溢出 |
| M-2 | major | `show_status` 尾部：`for line in &view.details { ui.label(RichText::new(line).weak().small()) }`、`"已等待 {:.1} 秒"`、`"下一步：{next}"` | 核心/下载目录/RPC/下一步四行全是 9pt + 3.72:1；「下一步」是必须读的信息，却比按钮小一号 | spec §2.2 规则 3、§5.9 | 明细改 12pt `text2`；「下一步」14pt `text2`；或用 `Notice` |
| M-3 | major | `draw` 壳标题：`ui.heading(format!("{PRODUCT_NAME} {VERSION}"))` + `RichText::new("M2-2b 下载面板").weak()`（`app.rs:3503-3504`，最新工作副本） | 产品名与版本在**壳内 heading** 里再出现一次（`title_text()` 已按 task-71 去掉窗口标题栏的版本号，但壳内这行没动）；「M2-2b」是内部里程碑号，不该进用户界面 | spec §6.1 规则 2、§7.7 规则 2 | 壳标题只留「AriaHost · 下载」；版本移到页脚；删里程碑串 |
| M-4 | major | `show_tasks`：`ui.heading(format!("任务（{total}）"))` | 同屏两个 18pt 标题（壳标题 + 任务标题），中间没有 16pt 层级；主次不分 | spec §2.1 | 壳标题 20pt/任务标题 16pt（`AriaSubtitle`） |
| M-5 | major | `show_task_table` header：`ui.label(RichText::new(title).strong().small())` | 表头 9pt bold（bold 实际只是换色），比 13pt 正文还小，7 列中文在 1200 宽下勉强可读 | spec §5.4 表头 12pt | 表头 12pt `text2`，行高 32 |
| M-6 | major | `const ROW_HEIGHT: f32 = 22.0`；行内 `ui.small_button("暂停")` | 22px 行里塞进度条（默认高 ≈14）+ 2 个 `small_button`（实测可点高 ≈21px，`13pt×1.33 + 2×2`），垂直几乎贴边；行内点击目标 < 24 下限 | spec §5.4、§7.4 | 行高 32；行内按钮 24 高 |
| M-7 | major | `show_task_table` 操作列：`if row.kind.can_pause() { … } if row.kind.can_resume() { … } if row.kind.can_remove() { … }` 顺序堆叠 | 实测量化（`01-main-panel.png`，操作列像素扫描）：两按钮的行 `暂停/继续` 在 x=1080..1121、`移除` 在 1130..1171；**只有「移除」的行，移除落在 1080..1121**——同一个动作在不同行横移 50px，而它左边紧邻的正是「暂停」。肌肉记忆失效，误点即暂停/移除 | spec §5.4 固定槽位 | 三槽位固定宽（暂停槽 40 / 继续槽 40 / 移除槽 40），空槽 `allocate_space` 占位，保证每行同一动作同一 x |
| M-8 | major | `TableBuilder::striped(true)`（`01-main-panel.png` 实测 253/248） | 斑马纹 **1.04:1**：既没提供分组线索，又制造轻微色噪 | spec §5.4 stripe | 用 `stripe` token（1.06–1.07:1，并配合 32 行高）或干脆不用斑马纹改用行 hover |
| M-9 | major | `show_tasks` 首行：`ui.heading(任务)` + `with_layout(right_to_left)` 塞 5 个按钮（立即刷新/复制 RPC 端点/打开配置目录/在浏览器打开/退出） | 760×480 下按钮组**溢出右缘**：最右「退出」被窗口边缘裁掉一截（`14-main-minsize.png` + 4× 放大 `14b-toolbar-zoom.png`）；没有折行机制 | spec §6.2 规则 4、§3.4 断点 B | 窄窗折成两行；按钮收进「刷新」+「⋯」 |
| M-10 | major | 同上，「退出」与「立即刷新」等权同排 | 破坏性操作与高频操作零隔离、无危险样式，误点即触发退出流程 | spec §6.7 | 「退出」移进「⋯」菜单或页脚，Danger 样式 + 确认 |
| M-11 | minor | `main_buttons`：`min_size(egui::vec2(104.0, 26.0))` 与 `(116.0, 26.0)` | 同一行按钮宽度不等（104/116），高度 26 与系统 `interact_size.y=20` 冲突 | spec §5.1 | 主操作统一 120×32，其余自适应同高 |
| M-12 | minor | `show_task_table`：`ProgressBar::new(row.progress).desired_width(96.0).show_percentage()` | 百分比画在条内，颜色取 `visuals.selection.stroke.color`（随主题变）；半填充行上文字一半压蓝一半压白 | spec §5.5 | 百分比独立列（13pt Mono 右对齐）；条内不画字 |
| M-13 | minor | `status_color()`（`TaskStatus`→色）+ `RichText::new(&*row.status).small().color(...)` | 状态词 9pt + 硬编码色（1.81–2.05:1），颜色成了唯一强编码，读不出来 | spec §4.4、§5.6 | 状态列用 chip（12pt + 语义底） |
| M-14 | minor | `show_empty_tasks`：`add_space(24.0)` + `vertical_centered` + 13pt 标题 | 空态只有两行文字，标题 13pt（与正文同级），没有主操作入口 | spec §5.12 | 16pt 标题 + 12pt 说明 + 〔新建下载〕引导 |
| M-15 | minor | `show_footer`：`RichText::new(format!("{PRODUCT_NAME} {VERSION}")).small().weak()` + `"·"` + 核心摘要(9pt weak) | 页脚两段 9pt + 1.23–3.72:1，且产品名与壳标题重复 | spec §6.1/§6.6 | 页脚 12pt `text2` 单行：「AriaHost 0.1.2 · 核心 aria2 1.37.0（托管）」 |

## 4. 服务管理

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| S-1 | minor | `show_service_page` 头部：`ui.heading("Windows 服务")` + `RichText::new("把 aria2 装成系统服务：开机自启，面板关掉也能继续下载").weak()` | 18pt 标题与 13pt 弱色长句**同行**，说明句 27 字，视觉上像标题的尾巴而不是独立说明；信息层级只有一档 | spec §6.1/§6.3 | 标题 20pt；说明降级为正文块（14pt `text2`，单独一行可折行），或删掉（字段已自解释） |
| S-2 | major | `show_service_card`：`RichText::new(label).strong().size(20.0).color(color)` | 唯一 `.size(20.0)`：与 18pt heading、16pt 子标题形成 18/20 两个近邻字号（视觉上「差半档」最难受）；「读取中…」用 `Color32::GRAY`（3.72:1） | spec §2.2 规则 2、§5.6 | 状态值 16pt `AriaSubtitle` + 语义 `*_text`；徽章化 |
| S-3 | major | `egui::Grid::new("ariahost_service_facts").spacing([12.0, 4.0])` + 标签 `.weak()` | 行距 4、标签弱色、标签列没有固定宽 → 标签与值的基线看着不齐（`11-service.png`「启动方式」与「—」） | spec §3.2/§6.3 | Grid spacing `[12, 8]`；标签列固定 96，标签 `text2` 12pt |
| S-4 | major | `service::start_mode_label` 返回值直接上界面：「手动触发（需要时由面板/SCM 启动）」「—（未运行或 SCM 未给）」 | 值列里塞了整句解释，还带内部缩写（SCM），表格行被撑长 | spec §6.3 规则 1、§7.7 | 值只留「手动触发」；解释进 `on_hover_text` |
| S-5 | minor | `show_service_card` 结果提示：`(140,200,160) / (150,190,240) / (230,140,140)` | 三种硬编码色在浅底 1.82/1.90/2.33:1，且与 tone 体系重复 | spec §4.3、§5.9 | 改 `Notice` + 语义 token |
| S-6 | minor | `show_service_card` 权限说明：`"当前以普通权限运行：…想免掉它，可右键面板 →「以管理员身份运行」。").weak().small()` | 必须读的说明用 9pt + 3.72:1，且 62 字长句 | spec §2.2 规则 3 | 12pt `text2`，拆两句 |
| S-7 | minor | `show_service_buttons`：4 个按钮等权横排（安装为服务/启动/停止/卸载服务） | 「卸载服务」与「安装为服务」相邻，同为 Secondary；等待中整体禁用但无说明 | spec §6.3 规则 2、§6.7 | 卸载移到操作条最右 + Danger + 确认；禁用时 `on_hover_text` 给原因 |

## 5. 设置

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| P-1 | **blocker** | `show_settings`：整个函数体直接画在根 `Ui`（没有 `CentralPanel` / 没有 `Frame` 底色），只有底部 `Panel::bottom` 自带底色 | 整页 `#080808`，标题/说明/输入框标签全是浅色主题的深灰字（`07b`、`07d`）：正文 2.48:1、`.weak()` 1.23:1 —— 设置页在浅色主题下**不可读** | spec §8.3、§6.4 规则 1 | 与 G-1 同一修复：设置页包 `Frame::NONE.fill(bg_base)` |
| P-2 | major | `show_settings` 顶栏：`ui.label(RichText::new("配置文件").weak())` + `ui.monospace(path.display().to_string())` + `ui.button("打开配置文件所在目录")` 全在**一行** | 实测（`07e-config-path-zoom.png`）：路径按内容宽度占位，按钮紧贴路径末字、没有间距策略；路径长度取决于用户名，**更长时整行会溢出并把按钮推出右缘**（推断，未在 760 宽下截屏） | spec §6.4 规则 1、§5.2 | 路径单独一行（13pt Mono，`truncate` + `on_hover_text` 全文），按钮另起一行右对齐 |
| P-3 | minor | `settings_text_row`：`ui.add_sized([150.0, 18.0], egui::Label::new(label))` | 标签格固定高 18，同行输入框实测高 ≈22（`07b`）：标签在格内垂直居中 → 视觉上比输入框文字偏高 2–3px；另 150 与规格的 160 不一致 | spec §6.4 规则 2 | 标签列 160 + 行高 40，`Label` 不设死高（跟随行高垂直居中） |
| P-4 | major | `settings_note`：`effect.text()` 按 `Effect` 上三色（`tone_color(Good/Warn/Bad)`） | 每行挂一条彩色小字（「立即生效」绿/「重启后生效」黄/「本批只保存，行为待接线」红），整页三四行彩色噪音，用户无法区分「必须处理」与「说明」 | spec §6.4 规则 3 | 只对「需重启」项挂 12pt `text2` 小字；未接线的项**不显示** |
| P-5 | major | `show_settings_interface`：三个 `ui.radio_value` 横排在 `ui.horizontal` 里（关闭按钮行为 / 渲染器 / 退出默认动作） | 三档选项标签长（「直接退出（按退出流程停 aria2）」12 个全角字符 ≈ 156pt），三项横排在 760 宽必然溢出（代码走查，该组未截屏） | spec §5.3 RadioGroup 竖排 | 竖排，每项一行 28 高 |
| P-6 | major | 8 处把 aria2 参数名写进主标签：`"重名文件自动改名（auto-file-renaming，默认 true）"`、`"断点续传（continue，默认 true）"`、`"内存缓存 disk-cache"` 等 | 主标签混中英 + 默认值，一眼看过去全是括号；且「默认 true」这种信息应在字段右侧统一呈现 | spec §5.2 禁止项、§7.7 规则 1 | 主标签只写中文；默认值与参数名放右侧 12pt `text2` / `on_hover_text` |
| P-7 | minor | `show_settings` 底部操作条：`保存 / 撤销更改 / 恢复默认 / 重新配置下载核心 / 状态文字` 全在一行 + 两个 `separator` | 主操作（保存）与「重新配置下载核心」等权；「没有改动」这类状态字挤在按钮后面 | spec §6.4 底部条、§5.1 | 左：保存(primary)/撤销/恢复默认；中：状态 12pt；右：「重新配置下载核心」收进「⋯」 |
| P-8 | minor | `if self.settings.leaving { ui.horizontal_wrapped(… 保存并离开 / 放弃改动并离开 / 取消) }` | 离开确认横排在内容区顶部，三个等权按钮，且会被后续内容挤走 | spec §6.4 规则 5、§5.10 | 改对话框（§6.4 规则 5 文案） |
| P-9 | minor | 左导航：`ui.allocate_ui_with_layout(vec2(96.0, avail_h))` + `Button::new(label).selected(selected)` | 96 宽在 12pt 下够用，但选中态依赖 `selection.bg_fill`（浅色 `#90D1FF`），与正文色对比 4.6:1 勉强；分组少于 4 个时留白过大 | spec §5.3/§4.1 | 导航 160 宽（与标签列对齐），选中态 = accent 12% 底 + 左侧 2px accent 条 |

## 6. 核心引导（首屏卡片）

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| C-1 | **blocker** | `show_core_guide`：`ui.horizontal(在线下载（{line.label()}） / 改用{other.label()} / 稍后再说)` + 下一行 `"已有 aria2c：" + TextEdit(available_width - 92) + 校验并使用` | 按钮文案双层括号（`在线下载（线路2（直连 GitHub））`）；输入框按 `available_width()-92` 撑满，把「校验并使用」挤出可视区（`13-core-guide.png` 里该按钮不见） | spec §6.5 规则 1 | 按钮文案短化（在线下载/改用代理线路/稍后再说），细节进 tooltip；输入框固定 400，按钮不参与宽度竞争 |
| C-2 | major | `egui::Frame::group(ui.style()).fill(ui.style().visuals.faint_bg_color)` | 浅色主题下 `faint_bg_color` = `#FDFDFD`，与页面 `#F8F8F8` 只差 1.04:1 → 卡片看不出边界，内容像直接浮在页面上 | spec §5.7 | `surface` 底 + 1px `border` + 圆角 8 |
| C-3 | major | 卡片内 6 处 `.weak().small()`：`"（AriaHost 需要一个 aria2c 才能下载东西）"`、`"可以这样处理：{hint}"`、`"下一步：换 X 再试"`、`"已请求取消：…"`、`"已经 45 秒没有任何进展…"` | 全是「必须读到才能做对事」的指引，却用 9pt + 3.72:1 | spec §2.2 规则 3 | 12–14pt `text2`；失败原因用 `danger_text` |
| C-4 | minor | `"已就绪：aria2 {version}（{line_note}；{hash_note}）"` | 一句话里塞版本、线路、哈希三件事，9pt，括号内还有分号 | spec §7.7 | 拆两行：结论 14pt / 附注 12pt `text2` |
| C-5 | minor | `"已请求取消：当前步骤完成后停止（下载并校验是一整步，不能中途掐断）"` | 9pt，且用括号解释机制 | spec §7.6 | 结论 14pt + 机制说明进 tooltip |

## 7. 日志 / 诊断

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| L-1 | major | `show_facts`：`egui::Grid::new("ariahost_runtime_facts").spacing([14.0, 4.0])` + 7 行值全部 `.small()`；`show_log`：`RichText::new(line).monospace().small()` | 参数值（路径/版本/KB）9pt；日志行 = 9pt Mono。等宽族（egui 默认 Hack）没有 CJK 字形，中文会回退到系统字体 → 同一行里中英字形与宽度不一致，再压到 9pt 更难扫（行距实测仅 12px） | spec §6.6 | 值 13pt Mono；行距 8；日志 13pt Mono `text2` |
| L-2 | minor | `ScrollArea::vertical().max_height(150.0)` | 固定 150：480 高窗口里日志吃掉 1/3，1200×760 里又太短 | spec §6.6 规则 2 | `min(240, 可用高 × 0.4)` |
| L-3 | minor | `CollapsingHeader::new("运行信息、输入法自检与日志")` | 顿号把三样东西塞进一个标题；折叠头没有说明会展开什么 | spec §7.7 | 「运行信息」为主，其余在展开后用小标题分区 |
| L-4 | minor | `RichText::new(format!("错误码：{code}")).monospace().small()` | 错误码 9pt 混在小字里，反而最该被抄走的字符串最不显眼 | spec §6.6 | 13pt Mono `text2`，行尾独立显示 |

## 8. 对话框

| ID | 严重度 | 位置 | 现象 | 违反 | 修法 |
|---|---|---|---|---|---|
| D-1 | major | `show_close_prompt`：`ui.horizontal(最小化到托盘 / 退出并停止下载 / 取消)` | 三个按钮等权、无危险样式、无主次；危险动作（退出并停止）在中间，最容易误点；未定义默认焦点 | spec §5.10、§6.7 | 右对齐「取消 \| 退出并停止(Danger)」；默认焦点取消；`Esc` 取消 |
| D-2 | minor | 同上：正文 13pt + 第二行 `.weak().small()`；用 `egui::Window` 默认标题栏与壳风格不一致 | 对话框正文里「最小化到托盘可以让下载继续…」是决策关键信息，却是 9pt 弱色 | spec §5.10 | 正文 14pt；关键句与按钮文案合并（按钮本身就叫「收进托盘继续下载」） |

## 9. 修复顺序（按「用户能立刻感知」排序）

| 顺序 | 条目 | 理由 |
|---|---|---|
| 1 | G-1 / P-1（未铺底）、G-2（样式层） | 一改就「整个软件换了个样子」，且不依赖别的改动 |
| 2 | M-1（长文本破版）、M-9（窄窗重叠）、C-1（按钮出界） | 功能性破损，出现在首屏与最小窗两个必经路径 |
| 3 | G-4 / G-5 / G-6（颜色与对比） | 用户原话点名的「对比差」，有可测门禁 |
| 4 | M-5..M-8、S-3、P-3（表格/网格/表单的排版基线） | 影响「专业感」的主体面积 |
| 5 | M-10 / S-7 / D-1（破坏性操作隔离） | 误操作成本，改起来便宜 |
| 6 | 其余 minor | 一致性收尾 |

## 10. 与用户原话的对应

| 用户说的 | 对应条目 | 实测支撑 |
|---|---|---|
| 「各个界面的排版太丑」 | G-8、M-4、M-14、S-1、P-3、L-1 | 30 处随手 `add_space`；两个 18pt 标题；网格行距 4 |
| 「字号大小不一」 | G-2、G-7、S-2、M-5 | 9/13/18 + 20 四档混用；表头 9pt 正文 13pt |
| 「颜色对比等视觉效果很差」 | G-1、G-4、G-5、G-6、S-5、C-2 | 2.48 / 1.23 / 1.53–2.83 / 1.22–2.10 |
| 「对界面排版、字体、字号、对比、颜色、配色、主题逐项审查」 | 全文 | 规格 §2–§4 + 本文 50 条 |
| 「操作互动逻辑、人体工学逻辑」 | M-6、M-7、M-9、M-10、D-1、S-7 | 22px 行内 21px 按钮；按钮错位；窄窗重叠；破坏性无隔离 |
| 「确保各界面有机的全局统一」 | G-2、G-3、P-6、M-3 | 无样式层；三种徽章画法；中英混排标签 |
