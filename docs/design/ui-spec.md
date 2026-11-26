# AriaHost 界面设计系统规格（UI Spec v1）

> **已被 `ui-spec-v2.md` 取代（v2，2026-11-26）。** 本文保留为历史审计记录；施工以 v2 为准。
> v2 修订了四件事：字体家族（Segoe UI + YaHei UI 逐字形回退）、字号阶梯（对齐 Fluent 2 的 12/14/16/20）、
> 颜色令牌（新增正绿 `ok.dot/ok.text/ok.ring`，状态色全部改为令牌）与卡片化分隔（删除页面内分割线）。

> 适用范围：`crates/ariahost-ui`（eframe / egui 0.37，纯立即模式，无 CSS、无样式表）。
> 本文件是**规格**（要改成什么样），不是现状描述；现状逐条问题见 [ui-audit.md](ui-audit.md)。
> 所有颜色对比度由 `.scratch/design/contrast_report.py` 实算，实测表见 `.scratch/design/contrast-current.md`（现状）与 `.scratch/design/contrast-tokens.md`（本规格提案，全部 PASS）。
> 基线：`6561e02`（2026-11-26）。行号会随并行改动漂移，**定位请以「函数名 + 代码片段」为准**，行号只作路标。

## 0. 三条硬结论（先看这段，后面全是它的展开）

1. **现状没有样式层。** 全 `ariahost-ui` 与 `ariahost` 里没有任何 `set_style` / `set_visuals` / `TextStyle` 配置（`Select-String 'set_style|set_visuals|install_style|theme'` 0 命中），界面跑在 egui 出厂默认上：字号阶梯只有 `9 / 13 / 18`，另有 3 处 `.size(20.0)` 与 3 处 `.heading()` 直接绕过阶梯。这是「字号大小不一」的根因，不是观感问题。
2. **有一处功能性缺陷，不是审美问题。** 面板根 `Ui` 没有铺底，只有 `Panel` / `CentralPanel` 会自己画底。于是**顶栏那一行**（产品名、`服务`、`设置`）和**整个设置页内容区**落在交换链清屏色上——本机实测像素 `#080808`，而文字仍是浅色主题的 `gray(80)`：对比度 **2.48:1**（顶栏），设置页正文更差（`.weak()` 项 **1.23:1**）。截图 `shots/01b-top-zoom.png`、`shots/07b-settings-download.png` 可复现。
3. **「加粗」在 egui 里不存在。** `RichText::strong()` 只把颜色换成 `widgets.active.text_color()`（`widget_text.rs:252-255,474-480`），字重不变；系统字体只有一个字重。所以层级只能靠**字号 + 颜色 + 间距 + 位置**建立，`strong()` 不能当 bold 用。

## 1. 设计原则（可判定，不是口号）

| 编号 | 原则 | 判定方式 |
|---|---|---|
| P1 | 一屏一个页面标题，一个主操作 | 代码里每屏 `Heading` 出现次数 = 1；`accent` 底色按钮 ≤ 1 |
| P2 | 颜色只从 token 取 | `grep -n "Color32::from_" crates/ariahost-ui/src` 只允许命中 `theme.rs`（新建） |
| P3 | 字号只从阶梯取 | `grep -n "\.size(" crates/ariahost-ui/src` 命中数 = 0 |
| P4 | 正文对比度 ≥ 4.5:1，大字 ≥ 3:1，UI 边界 ≥ 3:1 | `contrast_report.py` 全 PASS |
| P5 | 同一语义在六屏里长得一样 | 同一组件只有一处实现（见 §5 组件表） |
| P6 | 任何信息不靠颜色单独表达 | 状态同时有文字词（如「已暂停」），颜色只是冗余编码 |

## 2. 字体

### 2.1 语义阶梯（唯一允许的字号来源）

尺寸单位是 egui 的 point（100% DPI 下 1pt = 1px；150% 下自动 ×1.5）。

| 语义角色 | `TextStyle` | `FontId` | 行高 | 用途 | 禁止 |
|---|---|---|---|---|---|
| 页面标题 Title | `TextStyle::Heading` | `proportional(20.0)` | 28 | 每屏唯一标题（「下载」「服务」「设置」） | 不得用于卡片内标题 |
| 分区标题 Subtitle | `TextStyle::Name("AriaSubtitle")` | `proportional(16.0)` | 22 | 卡片标题、分区标题、空态标题 | 不得连续出现两级以上 |
| 正文 Body | `TextStyle::Body` / `TextStyle::Button` | `proportional(14.0)` | 20 | 正文、按钮文字、表格单元格、输入框、对话框 | — |
| 次要 Small | `TextStyle::Small` | `proportional(12.0)` | 17 | 说明行、时间、单位、派生信息、表头辅助 | **中文字号下限 12pt**，不得再小 |
| 数值 Numeric（大） | `TextStyle::Name("AriaNumber")` | `monospace(16.0)` | 22 | 汇总处的大数值：速度合计、可回收空间、任务计数 | 等宽族保证数字不跳动；只用于数字+单位 |
| 等宽 Mono（小） | `TextStyle::Monospace` | `monospace(13.0)` | 18 | 路径、错误码、PID、端口、日志、表格数值列 | 不得用于整句中文 |

**中文字号下限 = 12pt。** 依据：100% DPI 下 9pt（现 `TextStyle::Small` 出厂值）汉字横画与字腔粘连，「繁 / 籍 / 隙」这类 12 画以上的字糊成色块；本机实测 9pt 行距只有 12px（`shots/12-firstrun-nocore.png` 的状态明细四行，y=95/107/119/131），行距与字高比 1.33，挤压到无法扫读。12pt 是 Windows 中文界面的通用下限（Fluent 正文 14、caption 12）。

### 2.2 字号使用规则

1. 只允许出现 12 / 13（等宽） / 14 / 16 / 20 五档字号，跨两种字族（比例 / 等宽）。**新增档位必须先改本节**。
2. `RichText::size()`（即 `.size(20.0)`）**全库禁用**：现状 `app.rs` 的服务状态徽章用 `.size(20.0)` 绕过了阶梯，与 16/18 撞车。徽章改用 §5.6 的 chip 规格。
3. `.small()` 从不用于承载**唯一信息**：它只能做补充。任何「用户必须读到才能做对事」的文字（下一步、错误原因）必须 ≥ 14pt 且颜色 ≥ `text2`。
4. `.weak()` **不作为「次要文字色」的手段**（egui 的 `.weak()` = `gamma_multiply(0.6)`，在浅底上把 7.59:1 压到 3.72:1）。次要文字用 `text2`，禁用文字用 `text3`。
5. 多行段落显式设行高（`RichText::line_height(Some(20.0))` 或对应值）；单行标签不设，避免与控件高度打架。

### 2.3 层级怎么建立（替代 bold）

| 手段 | 用法 |
|---|---|
| 字号 | 20 / 16 / 14 / 12 四级足够 |
| 颜色 | `text` → `text2` → `text3` 三级 |
| 间距 | 上一级与下一级之间用 `SP_4 = 16`，同级之间 `SP_2 = 8` |
| 位置 | 标题左对齐同一条基线（`SP_5 = 24` 页边距） |
| 底线/背景 | 只有「当前页 / 选中行」用 accent 底色或左侧 2px 条 |

如需视觉强调：用 `text` 全色 + 14pt（对比 `text2` 12pt），不要叠加 `strong()`。

## 3. 间距、栅格与断点

### 3.1 间距 token（4pt 基准）

| token | 值 | 用在哪 |
|---|---|---|
| `SP_1` | 4 | 图标与文字之间、chip 上下内边距 |
| `SP_2` | 8 | 同一组内的控件之间、卡片标题与正文之间 |
| `SP_3` | 12 | 卡片内边距（紧凑卡：表格容器、提示条） |
| `SP_4` | 16 | 卡片内边距（标准）、卡片与卡片之间 |
| `SP_5` | 24 | 分区之间、页面左右边距 |
| `SP_6` | 32 | 页面上下留白、空态上留白 |
| `SP_7` | 48 | 空态图标与文案之间 |

### 3.2 `Spacing` 字段赋值（照抄）

| 字段 | 现值（出厂） | 规格值 | 理由 |
|---|---|---|---|
| `item_spacing` | `(8,3)` | `(8,6)` | 3 太挤，中文行贴在一起 |
| `window_margin` | `6` | `16` | 内容贴边是「排版丑」的第一观感来源 |
| `menu_margin` | `6` | `8` | — |
| `button_padding` | `(8,2)` | `(12,6)` | 按钮高度从 17 提到 32（见 §5.1） |
| `interact_size` | `(40,20)` | `(48,28)` | 点击区下限（§7.4） |
| `indent` | `18` | `20` | 与 12pt 字高对齐 |
| `slider_width` | `100` | `140` | — |
| `slider_rail_height` | `8` | `6` | — |
| `combo_width` | `100` | `160` | 中文选项普遍 4–8 字 |
| `text_edit_width` | `280` | `280` | 保持；路径类 400、数字类 120（局部覆盖） |
| `icon_width` / `icon_width_inner` / `icon_spacing` | `14/8/4` | `16/10/6` | 与 14pt 文字视觉居中 |
| `tooltip_width` | `500` | `420` | 超过 420 的提示应改写而不是换行 |
| `menu_width` | `400` | `320` | — |
| `combo_height` | `200` | `240` | — |
| `scroll.bar_width` | 出厂 | `10` | 细滚动条难点中 |
| `scroll.floating` | 出厂 | `false` | 浮层滚动条会盖住表格最后一列 |
| `Interaction::interact_radius` | `5` | `6` | 与圆角一致 |
| `Interaction::tooltip_delay` | `0.5` | `0.35` | 0.5s 让人以为没提示 |

### 3.3 页面栅格

- 页面左右内边距 = `SP_5 (24)`；内容最大宽 = **1080**（超出居中，避免 1920 屏一行 1800px 的扫读疲劳）。
- 卡片之间纵向 = `SP_4 (16)`；卡片内边距 = `SP_4 (16)`。
- 设置页/服务页的「标签列」固定宽：设置 160，服务 96；值列从标签列 + `SP_3 (12)` 起。
- 不引入 12 列栅格系统：立即模式下自适应列宽比栅格更省事，也避免窄窗塌陷。

### 3.4 窗口尺寸与断点

| 项 | 值 | 说明 |
|---|---|---|
| 默认窗口 | 1200 × 760 | 保持（`window.rs: DEFAULT_WIDTH/HEIGHT`） |
| 最小窗口 | 760 × 480 | 保持（`MIN_WIDTH/MIN_HEIGHT`），但**布局必须在该尺寸成立** |
| 断点 A | W ≥ 1000 | 表格 7 列全显 |
| 断点 B | 760 ≤ W < 1000 | 表格隐藏「剩余」列（并入状态 tooltip）；壳右侧按钮收进「⋯」菜单；状态卡按钮换行 |
| 断点 C | W < 760 | 不允许（窗口最小宽已限死） |

断点用 `ui.available_width()` 判定，不用 `ctx.screen_rect()`（多视口下后者不可靠）。

## 4. 颜色 token

### 4.1 token 表（两套主题都要实现）

实现方式：`ctx.style_mut_of(Theme::Dark, ...)` 与 `style_mut_of(Theme::Light, ...)` 各装一遍（API 已确认存在：`context.rs:2661,2691`）。主题跟随系统（`ThemePreference::System` 是 egui 默认），**但现在必须先修「未铺底」问题**，否则浅色主题下永远是黑底深字。

| token | 深色（默认） | 浅色 | 用在哪 |
|---|---|---|---|
| `bg_base` | `#15181B` | `#F6F7F9` | 页面底（窗口清屏色也用这个） |
| `surface` | `#1C2024` | `#FFFFFF` | 卡片、表格容器、壳 |
| `stripe` | `#202529` | `#F5F7F9` | 表格偶数行 |
| `surface_hover` | `#242A2F` | `#EEF1F5` | hover 底、次按钮底 |
| `surface_active` | `#2B3238` | `#E3E8EE` | 按下态 |
| `input_bg` | `#111417` | `#FFFFFF` | 输入框、进度条槽 |
| `border` | `#353C43` | `#D6DBE1` | 装饰性分隔线（1px） |
| `border_strong` | `#6B757E` | `#8A929C` | **控件边界**（输入框、勾选框），≥3:1 |
| `text` | `#E6E9EC` | `#1B1F23` | 主文字 |
| `text2` | `#A7B0B8` | `#59626B` | 次要文字、hint |
| `text3` | `#838C94` | `#7E878F` | 禁用文字 |
| `accent` | `#4C8DF6` | `#1F6FEB` | 主按钮底、焦点环、进度填充 |
| `accent_hover` | `#5C9CFF` | `#185FD1` | 主按钮 hover（深色变亮、浅色变暗，两个方向都过 4.5） |
| `accent_text` | `#86B4FF` | `#0A58CA` | 链接、可点文字 |
| `on_accent` | `#0B121C` | `#FFFFFF` | 主按钮上的字 |
| `success` | `#4CC26B` | `#1A7F37` | 成功底/边框/图标 |
| `success_text` | `#6FD98C` | `#146C2E` | 成功文字 |
| `warn` | `#E0A93B` | `#8A5A00` | 警告底/边框/图标 |
| `warn_text` | `#F0C05A` | `#7A4F00` | 警告文字 |
| `danger` | `#F0616D` | `#C42B1C` | 危险底/边框/图标 |
| `danger_text` | `#FF929B` | `#B42318` | 危险文字 |
| `info` | `#4FA8F5` | `#0969DA` | 进行中/信息 |
| `info_text` | `#7CC4FF` | `#0550AE` | 信息文字 |
| chip 底透明度 | `16%` | `10%` | 徽章底 = tone 色按该 alpha 叠在 `surface` 上 |

### 4.2 对比度（实测，节选；全表见 `.scratch/design/contrast-tokens.md`）

| 组合 | 深色 | 浅色 | 门限 |
|---|---|---|---|
| 正文 / 页面底 | 14.62:1 | 15.46:1 | ≥4.5 |
| 次要文字 / 卡片底 | 7.45:1 | 6.21:1 | ≥4.5 |
| 禁用文字 / 卡片底 | 4.79:1 | 3.65:1 | ≥3.0 |
| 输入框边框 / 卡片底 | 3.49:1 | 3.15:1 | ≥3.0 |
| 焦点环 / 卡片底 | 5.03:1 | 4.63:1 | ≥3.0 |
| 主按钮文字 / 按钮底 | 5.77:1 | 4.63:1 | ≥4.5 |
| success 文字 / chip 底 | 6.96:1 | 5.70:1 | ≥4.5 |
| warn 文字 / chip 底 | 7.10:1 | 6.17:1 | ≥4.5 |
| danger 文字 / chip 底 | 6.23:1 | 5.62:1 | ≥4.5 |
| info 文字 / chip 底 | 6.63:1 | 6.59:1 | ≥4.5 |

**对照现状**（同脚本，浅色主题）：`.weak()` 正文 3.72:1、状态绿 1.82:1、状态蓝 1.81:1、停止黄 1.53:1、操作提示蓝 1.67:1、Banner 文字 / 自身底色 1.22–2.10:1、顶栏黑条 2.48:1。全部低于门限，这就是用户说的「颜色对比等视觉效果很差」的可测证据。

### 4.3 颜色使用规则

1. 绘制点**只允许**取 token（`theme::text2(&ui)` 这类访问器），禁止 `Color32::from_rgb(...)` 字面量。现状 18 处硬编码全部要收进 `theme.rs`。
2. `.weak()` 只允许用在「可有可无」的装饰文字上；用它替代 `text2` 的 42 处逐一改掉。
3. `gamma_multiply` 只允许做**底色**（chip / banner），不允许做文字色。
4. 状态一律「文字词 + 颜色」双编码（P6）：色弱用户也要能读。
5. 语义色的选用：故障/失败 = `danger`；需要用户注意但可继续 = `warn`；进行中/等待 = `info`；完成/正常 = `success`；中性/未安装 = `text2`。

### 4.4 状态 → token 映射（六态服务 + 七态任务）

| 状态 | 徽章底 | 文字 | 图标 |
|---|---|---|---|
| 服务：未安装 | `text3` 12% | `text2` | ○ |
| 服务：已停止 | `warn` 16%/10% | `warn_text` | ■ |
| 服务：启动中 / 停止中 | `info` 16%/10% | `info_text` | ◐（配 spinner） |
| 服务：运行中 | `success` 16%/10% | `success_text` | ● |
| 服务：未知 | `text3` 12% | `text2` | ? |
| 任务：下载中/做种 | `success` 16%/10% | `success_text` | ↓ |
| 任务：等待中 | `info` 16%/10% | `info_text` | ⋯ |
| 任务：已暂停 | `warn` 16%/10% | `warn_text` | ‖ |
| 任务：已完成 | `success` 16%/10% | `success_text` | ✓ |
| 任务：出错 | `danger` 16%/10% | `danger_text` | ! |
| 任务：已移除 / 未知 | `text3` 12% | `text2` | – |

## 5. 组件规格

单位均为 point。所有组件**只有一处实现**（放 `crates/ariahost-ui/src/widgets.rs`），绘制点只调用它。

### 5.1 按钮

| 类型 | 高度 | 水平内边距 | 圆角 | 底 | 字 | 边框 | 用途 |
|---|---|---|---|---|---|---|---|
| Primary | 32 | 16 | 6 | `accent` | `on_accent` 14pt | 无 | 每屏 ≤1：启动 aria2 / 开始 / 保存服务操作 |
| Secondary | 32 | 12 | 6 | `surface`（卡片上）/ `surface_hover`（页面底） | `text` 14pt | 1px `border` | 其余常规动作 |
| Danger | 32 | 12 | 6 | 透明 | `danger_text` 14pt | 1px `danger` | 卸载服务、移除任务（不可恢复时） |
| Icon | 32×32 | — | 6 | 透明 | 图标 16 | 无 | 关闭、刷新、更多；必须 `on_hover_text` |
| RowAction | 24 | 8 | 4 | 透明 | `text2` 12pt | 1px `border` | 表格行内（暂停/继续/移除） |

状态：

| 状态 | 表现 |
|---|---|
| hover | 底 `accent_hover`（Primary）/ `surface_active`（Secondary）；文字不变 |
| pressed | 底再压 8%（Primary 用 `#3E7BDC` 深色 / `#1554B8` 浅色，实测 ≥4.5） |
| disabled | 底 `surface_hover`，字 `text3`，无边框；**禁用原因必须写在 `on_hover_text`** |
| focus | 外扩 1px `accent` 描边（键盘可见） |

规则：
- 同一行按钮**等高**；主操作按钮统一 `min_size((120, 32))`，避免「启动 aria2 / 打开下载目录」104 与 116 两种宽度（现状）。
- 宽高由文字撑开导致的「同行不等宽」判为缺陷（审计 M-11）。
- 一行 ≥3 个按钮时，第三个起收进「⋯」菜单（`menu_icon_button`）。

### 5.2 输入框

| 项 | 值 |
|---|---|
| 高度 | 32 |
| 底 / 边框 | `input_bg` / 1px `border_strong`，圆角 6 |
| 内边距 | `(10, 6)` |
| 字 | Body 14pt `text`；占位符 `text2` |
| 宽度 | 默认 280；路径类 400；数字类 120；可用宽度不足时收缩到 ≥120 |
| focus | 边框换 `accent` + 外扩 1px |
| 错误 | 边框 `danger` + 下方 `danger_text` 12pt 一句原因（就地，不弹窗） |

禁止：把 aria2 参数名当主标签（现状「重名文件自动改名（auto-file-renaming，默认 true）」），主标签只写中文，参数名放 `on_hover_text`。

### 5.3 下拉 / 单选 / 复选

| 控件 | 规格 |
|---|---|
| ComboBox | 高 32，宽 160；展开项高 28；选中项左侧 2px `accent` 条 |
| RadioGroup | **竖排**，每项高 28，项间距 `SP_1`；右侧可带 12pt 说明。禁止横排（中文标签长，760 宽必溢出，现状实证） |
| Checkbox | 框 16×16，圆角 3，1px `border_strong`；选中底 `accent` + 白勾；标签 14pt，与框间距 `SP_2` |

### 5.4 表格（任务列表）

| 项 | 值 |
|---|---|
| 容器 | `surface` 底，1px `border`，圆角 8，内边距 `SP_3` |
| 表头高 | 32；字 12pt `text2`；底 `surface`；下边框 1px `border` |
| 行高 | **32**（现 22，行内塞了进度条与两个按钮，读不动） |
| 斑马纹 | 偶数行 `stripe` |
| 行 hover | `surface_hover` |
| 选中行 | `accent` 12% 底 + 左侧 2px `accent` 条 |
| 单元格内边距 | 左右 `SP_2 (8)`，垂直居中 |
| 列宽 | 名称 `remainder().at_least(240)`；大小 96；进度 160；速度 96；剩余 96；状态 96；操作 132 |
| 对齐 | 名称/状态左对齐；大小/速度/剩余**右对齐**（等宽字体，数字不跳动）；进度条左对齐 |
| 操作列 | 三个固定槽位，每槽 40 宽，**空槽用 `ui.allocate_space` 占位**，保证「暂停/继续/移除」在每行同一 x |
| 虚拟化 | 必须继续用 `body().rows(32.0, total, ...)`，**不允许**为美观改成逐行 `ui.horizontal`（内存闸门） |

### 5.5 进度条

| 项 | 值 |
|---|---|
| 槽高 | 8，圆角 4，底 `input_bg`，1px `border` |
| 填充 | `accent`；完成态 `success` |
| 文字 | **不在条内画百分比**：百分比单独一列，13pt Mono，右对齐，`text` |
| 不确定态 | 同槽 + 2px 宽 30% 的 `info` 滑块（不做动画，靠 500ms 重绘推进） |

理由：条内文字现在取 `visuals.selection.stroke.color`（浅色主题 `#00537D`），在**半填充**的行上文字一半压蓝一半压白，且肤色随主题而变（`progress_bar.rs:176-180`）；改为独立文字列可一次解决对比与对齐。

### 5.6 状态徽章（chip）

| 项 | 值 |
|---|---|
| 高 | 22；内边距 `(8, 2)`；圆角 11（胶囊） |
| 底 | 语义色 @ `chip 透明度` 叠在 `surface` 上 |
| 字 | 12pt，颜色用对应 `*_text` |
| 内容 | 可选 12px 图标 + 状态词（中文），**不加方括号**（现状 `[{badge}]` 是命令行风格） |
| 大号场景 | 服务页顶部：用 16pt Subtitle + `*_text`，不加 chip 底（避免大色块） |

### 5.7 卡片

| 项 | 值 |
|---|---|
| 底 / 边框 | `surface` / 1px `border`，圆角 8 |
| 内边距 | `SP_4 (16)`（紧凑卡 `SP_3 (12)`） |
| 结构 | 标题行（16pt `text` + 右侧可选动作）→ `SP_2` → 正文 / 控件 |
| 卡片之间 | `SP_4 (16)` |
| 禁止 | 卡片套卡片（超过两层）；卡片内再画 `separator` 当分区（用 `SP_3` 间距代替） |

### 5.8 Banner（横幅，替代现在的「内联错误条」）

| 项 | 值 |
|---|---|
| 结构 | 左侧 3px 语义色实心条 + 正文区 + 右上关闭按钮 24×24 |
| 底 | 语义色 @ 12% 叠在 `surface` 上（**不是 `gamma_multiply` 出来的怪色**） |
| 标题 | 14pt `text`（不再用语义色写字，现状 1.22:1） |
| 第二行 | 12pt `text2`，「下一步 / 怎么做」 |
| 错误码 | 13pt Mono `text2`，放在标题行尾 |
| 高度 | 自适应，最小 48 |
| 消失 | 成功/信息 8s 自动消失；错误**不自动消失**，需点关闭（保留现状语义） |

### 5.9 Notice（行内提示，一次性反馈）

单行、无底色：`SP_1` 图标（12）+ 12pt `text2`；成功用 `success_text` 只给图标上色。3s 后淡出（不做动画则直接消失，避免额外重绘）。

### 5.10 对话框

| 项 | 值 |
|---|---|
| 宽度 | 420（最小 360，最大 560） |
| 圆角 / 阴影 | 10 / 沿用 `popup_shadow` |
| 遮罩 | 黑 40%（`Color32::from_black_alpha(102)`） |
| 标题 | 16pt `text`，左对齐 |
| 正文 | 14pt `text`，行高 20 |
| 按钮 | 右对齐：`[取消] [主操作]`；危险操作主按钮用 Danger 样式 |
| 焦点 | 打开时焦点在「取消」；`Esc` = 取消 |

### 5.11 折叠区 / 分隔线

- 折叠头高 32，箭头 16，字 14pt `text2`；展开后内容左缩进 `SP_4`。
- 分隔线：1px `border`，上下各 `SP_2`；**不用 `separator()` 做分区**（脏线多），分区用间距。

### 5.12 空态

| 项 | 值 |
|---|---|
| 结构 | 48px 图标（可选）→ `SP_4` → 16pt `text` 标题 → `SP_2` → 12pt `text2` 说明 → `SP_4` → 主操作（若有） |
| 垂直位置 | 内容区上 1/3 处（`SP_6` + 上边距），不居中到底 |
| 文案 | 说明**下一步做什么**，不写「暂无数据」 |

## 6. 布局规格（逐屏）

### 6.1 全局壳（所有页面共用）

```
┌───────────────────────────────────────────────────────────────────┐ 壳：surface 底，高 48，下边框 1px border
│ AriaHost · 下载                      [服务] [设置]                │
├───────────────────────────────────────────────────────────────────┤ 页面区：bg_base 底，左右 24，内容最大宽 1080 居中
│                                                                   │
│  <页面内容>                                                        │
│                                                                   │
├───────────────────────────────────────────────────────────────────┤ 页脚：surface 底，高 32，上边框 1px border
│ AriaHost 0.1.2 · 核心 1.37.0（托管）                    [运行信息 ▾]│
└───────────────────────────────────────────────────────────────────┘
```

规则：
1. **壳必须自己铺底**（`Frame::NONE.fill(bg_base)` 包住整个 `CentralPanel`），这是修 §0-2 的唯一改动点。
2. 产品名只在壳里出现一次；不再画「M2-2b 下载面板」这类里程碑串（现 `app.rs` 的 `ui.heading(format!("{PRODUCT_NAME} {VERSION}"))` 与 `"M2-2b 下载面板"`）。
3. 版本号只在页脚与「关于」里出现，不进标题。
4. 页面导航（服务/设置/返回）用 `Secondary` 按钮，右侧对齐，与系统标题栏按钮**纵向错开**（壳高 48 ≥ 标题栏可点区）。
5. 返回类按钮统一文案「← 返回」，不再用「返回主界面」（4 字更长的同时表达更弱）。

### 6.2 主界面（下载）

```
┌ 壳 ──────────────────────────────────────────────────────────────┐
│ AriaHost · 下载                                  [服务] [设置]    │
├──────────────────────────────────────────────────────────────────┤
│ ┌ 状态卡 (surface, SP_4) ───────────────────────────────────────┐ │
│ │ [● 已停止]  核心未启动                    [启动 aria2] [目录] │ │
│ │ 核心 1.37.0（托管）· RPC 127.0.0.1:6800                       │ │
│ │ 下一步：点「启动 aria2」开始下载                    ← 14pt text2│ │
│ └───────────────────────────────────────────────────────────────┘ │
│ ┌ 新建下载 (surface) ───────────────────────────────────────────┐ │
│ │ [链接输入框 400 ............] [开始]                          │ │
│ └───────────────────────────────────────────────────────────────┘ │
│ 任务（3）                                          [刷新] [⋯]     │ 16pt Subtitle
│ ┌ 表格容器 ─────────────────────────────────────────────────────┐ │
│ │ 名称                  大小     进度        速度   剩余  状态  │ │
│ │ ...                                                           │ │
│ └───────────────────────────────────────────────────────────────┘ │
├──────────────────────────────────────────────────────────────────┤
│ AriaHost 0.1.2 · 核心 1.37.0（托管）                     [运行信息] │
└──────────────────────────────────────────────────────────────────┘
```

规则：
1. 状态卡是**唯一**展示「核心/服务/下一步」的地方；现在的状态明细四行 9pt 全塞进顶栏，扫描成本极高。
2. 「启动/停止 aria2」= 唯一 Primary；「打开下载目录」= Secondary，固定 120 宽。
3. 工具栏 5 个按钮 → 保留「刷新」+「⋯」菜单（复制 RPC 端点 / 打开配置目录 / 在浏览器打开 / 退出）；「退出」是破坏性操作，必须在菜单里而不是工具栏最右（现状与任务表同排，误点代价大）。
4. 任务标题与工具栏同一行；窄窗（<1000）工具栏折成两行，不与标题抢宽度（现状 760 宽时按钮组溢出右缘、最右按钮被裁，见审计 M-9 与 `shots/14b-toolbar-zoom.png`）。

### 6.3 服务管理

```
┌ 壳 ──────────────────────────────────────────────────────────────┐
│ AriaHost · 服务                                  [← 返回]         │
├──────────────────────────────────────────────────────────────────┤
│ ┌ 状态卡 ───────────────────────────────────────────────────────┐ │
│ │ 服务状态                                     [刷新状态]       │ │
│ │  已停止（16pt success/warn_text）                              │ │
│ │  服务名   AriaHost                                             │ │
│ │  启动方式 手动触发（需要时由面板/SCM 启动）                     │ │
│ │  PID      —                                                    │ │
│ └───────────────────────────────────────────────────────────────┘ │
│ ┌ 说明 ─────────────────────────────────────────────────────────┐ │
│ │ 当前以普通权限运行：装/卸/启停各弹一次授权框。                 │ │
│ │ 服务日志：%ProgramData%\AriaHost\logs（需管理员）              │ │
│ └───────────────────────────────────────────────────────────────┘ │
├ 底部固定操作条（surface，高 56） ─────────────────────────────────┤
│ [安装为服务] [启动] [停止]                        [卸载服务]      │
└──────────────────────────────────────────────────────────────────┘
```

规则：
1. 标签列 96，值列左对齐；`—` 表空值而不是「（未运行或 SCM 未给）」这种长句（长句移到 `on_hover_text`）。
2. 卸载固定在操作条最右、间距 `SP_5`，用 Danger 样式 + 二次确认（对话框写明「会停止正在进行的下载」）。
3. 等待态（启动中/停止中）在状态卡内显示 spinner + 「停止中，最多 25 秒」+ 已等待秒数（12pt `text2`）。
4. 结果提示（成功/取消授权/失败）用 Notice，不用彩色长句；「你取消了授权」不落红条（现有语义要保留）。

### 6.4 设置

```
┌ 壳 ──────────────────────────────────────────────────────────────┐
│ AriaHost · 设置                                  [← 返回]         │
├──────────────────────────────────────────────────────────────────┤
│ 配置文件 C:\Users\…\AriaHost\config.toml        [打开目录]        │ 12pt text2
│ ┌ 左导航 160 ┬ 右内容（最大宽 720） ────────────────────────────┐ │
│ │ 下载        │  下载设置                                        │ │
│ │ 网络        │  标签(160)   控件(280)              默认 5      │ │
│ │ 界面        │  下载目录    [.................]               │ │
│ │             │  ⟳ 重启 aria2 后生效                            │ │
│ └─────────────┴────────────────────────────────────────────────┘ │
├ 底部固定操作条（surface，高 56） ─────────────────────────────────┤
│ [保存] [撤销更改] [恢复默认]        有未保存的改动                │
└──────────────────────────────────────────────────────────────────┘
```

规则：
1. **设置页必须有底板**（现状整页黑底，见 §0-2）。
2. 标签列 160 固定、单行不换行（超出用 `on_hover_text`）；控件列从 184 起，行高 40。
3. 「生效时机」不再每行一条彩色小字（现状 `Immediate/Restart/NotWired` 三色混排），改成：**只对「需要重启」的项**在该行右侧挂 12pt `text2` 的「重启后生效」小字；「本批未接线」的项**不出现在界面上**（要么能生效，要么不露出）。
4. 三档单选（关闭行为 / 渲染器 / 退出默认动作）竖排，每项一行，标签完整（现状三条横排在 760 宽会挤）。
5. 未保存离开用对话框（`[保存并离开] [放弃并离开] [取消]`），不用横排文字按钮挤在内容区顶部。
6. 常量字号：路径与 PID 用 13pt Mono。

### 6.5 核心引导（首屏卡片）

```
┌ 卡片 ─────────────────────────────────────────────────────────────┐
│ 下载核心未就绪（16pt）                                            │
│ 首次使用需要准备 aria2 核心。（14pt text2）                       │
│                                                                   │
│ [● 在线下载] [改用代理线路] [稍后再说]        ← 三个 Secondary     │
│                                                                   │
│ 已有 aria2c：[路径输入框 400 .........] [校验并使用]               │
│ ⚠ 说明 / 失败原因（12pt，danger_text 只在真失败时上色）           │
└───────────────────────────────────────────────────────────────────┘
```

规则：
1. 按钮**不换行溢出**：三个按钮总宽在 760 窗宽内必须放得下（当前「在线下载（线路2（直连 GitHub））」这种双层括号把按钮撑到 260 宽）。按钮文案：`在线下载`、`改用代理线路`、`稍后再说`，线路细节进 `on_hover_text`。
2. 状态行（核心是否就绪、错误码、下一步）在状态卡里，不重复放在引导卡。
3. **长文本必须折行**：现状 `RichText::new(&view.headline).strong()` 在 `ui.horizontal` 里不折行 → 一条长状态串直接顶出右边界并把「启动 aria2」按钮挤出屏幕（截图 `13-core-guide.png`）。规格：状态卡主文案区用 `ui.add(egui::Label::new(...).wrap())`，且**先画右对齐按钮、再画左对齐文本**，让文本按剩余宽度折行。
4. 段落与段落之间 `SP_3`；卡片内不再嵌套 `Frame::group`。

### 6.6 日志 / 诊断

```
│ 运行信息 ▾                                                        │
│  渲染后端      wgpu / dx12                                        │
│  核心          aria2 1.37.0（托管，激活版本）                     │
│  字形图集      4096 px（上限）                                    │
│  中文字体      system:Microsoft YaHei UI                          │
│  内存          51.4 MB（私有工作集）                              │
│  服务          AriaHost · 已停止                                  │
│ ────────────────────────────────                                 │
│ 输入法自检 [.....................] 已输入 0 字                    │
│ ────────────────────────────────                                 │
│ 启动日志（最近 500 行）                        [复制] [打开日志]  │
│ ┌ ─────────────────────────────────────────────────────────────┐  │
│ │ 12:01:02.123 面板启动 …（13pt Mono，行高 18）                │  │
│ └──────────────────────────────────────────────────────────────┘  │
```

规则：
1. 诊断信息用 2 列 Grid：标签 96 / 值自适应；值 13pt Mono（路径、版本、KB）。
2. 日志区高度 = `min(240, 可用高 × 0.4)`，内部滚动，`stick_to_bottom(true)` 保留。
3. 日志行 13pt Mono + `text2`；错误行用 `danger_text`（整行，不逐字上色）。
4. 「复制日志」按钮：复制当前缓冲全文（便于报障）。

### 6.7 主操作与破坏性操作的空间隔离规则

| 规则 | 具体要求 |
|---|---|
| 主操作为唯一 | 一屏一个 Primary；其余 Secondary |
| 破坏性操作位置 | 底部操作条最右，或收进「⋯」菜单；**不得与主操作相邻**（间距 ≥ `SP_5 = 24`） |
| 破坏性操作样式 | Danger（透明底 + danger 边框 + danger_text 字） |
| 破坏性操作确认 | 二次确认对话框，写明「影响」（例如「会停止正在下载的 3 个任务」）；默认焦点在「取消」 |
| 不可恢复操作 | 移除任务需确认；已完成的下载不得在 UI 里删除 |

## 7. 交互与人体工学

### 7.1 焦点与键序

| 项 | 规格 |
|---|---|
| Tab 顺序 | 壳右侧动作 → 页面内自上而下、自左而右；`egui` 默认按控件创建顺序，因此**创建顺序 = 视觉顺序**（现状有「按钮先画在右、文本后画」的倒序，需要调整） |
| 焦点可见 | 1px `accent` 外框（`Visuals::selection` 之外单独实现），失焦不保留 |
| Enter | 输入框内 = 该输入框的主操作（新建下载框 = 开始） |
| Esc | 关闭当前对话框 / 取消当前编辑 |
| Ctrl+S | 设置页保存（可选，P2） |
| 禁止 | 每帧 `request_focus`（会 `interrupt_ime()`，`egui` #8813，现已在 `app.rs` 注释里写明，保持不变） |

### 7.2 键盘可达

所有可点控件必须是 `Button`（或 `Response` 可聚焦），**`selectable_label` 不作为唯一交互**（accesskit 里不可执行，现状设置页左侧导航已因此改用 `Button::selected`，属于正确做法，规格固化）。

### 7.3 危险操作

| 操作 | 是否二次确认 | 文案要点 |
|---|---|---|
| 卸载服务 | 是 | 会停止 aria2 与服务，正在下载的任务会中断 |
| 退出并停止下载 | 是（`ask` 档） | 当前下载数 N；可改「收进托盘继续」 |
| 移除任务 | 是（进行中的任务） | 已下载部分文件保留/删除二选一 |
| 恢复默认设置 | 否，但给一次撤销（保存前） | — |

### 7.4 点击目标与间距

- 最小可点区 **24×24**（WCAG 2.2 Target Size (Minimum) 是 24），行内动作按此下限；主按钮 32 高。
- 相邻可点目标间距 ≥ `SP_2 (8)`；危险目标与安全目标间距 ≥ `SP_5 (24)`。
- 触摸/高 DPI 场景（`pixels_per_point ≥ 1.5`）：`interact_size` 提升到 `(48, 32)`。

### 7.5 长列表与滚动

- 任务表继续虚拟化（`TableBuilder::body().rows()`），行高 32 常数。
- 滚动位置保持：`ScrollArea` / `TableBuilder` 的 `id_salt` 必须稳定（现 `"ariahost_tasks"` 正确），**不得**把行数或筛选条件拼进 id。
- 每 2 秒刷新时**不得**重置滚动或选中行；行按 `gid` 做 key 而不是 index（`optimistic` 行插入时要保持稳定）。
- 表头条固定在滚动区外（`TableBuilder` 的 header 已固定）。

### 7.6 三态文案规范

| 态 | 规则 | 示例 |
|---|---|---|
| 空 | 说明下一步，不写「暂无数据」 | 「还没有下载任务 — 把链接粘到上面，回车开始」 |
| 加载 | <300ms 不显示；≥300ms 显示 spinner + 一句进行中的事 | 「正在读取服务状态…」 |
| 错误 | 码 + 一句人话 + 一个动作 | 「AH-E5002 · 需要管理员权限 · [以管理员重启]」 |

### 7.7 文案规范

1. 主标签中文；aria2 参数名只放在 `on_hover_text` 或单个括号里，禁止括号套括号（现状「在线下载（线路2（直连 GitHub））」）。
2. 禁止在用户可见文案里出现里程碑/批次号（「M2-2b」「本批只保存」）。
3. 数字带单位与空格：「128.0 MiB」「12.8 KB/s」「1 小时 21 分」。
4. 状态词表固定：未安装 / 已停止 / 启动中 / 运行中 / 停止中 / 未知；下载中 / 等待中 / 已暂停 / 已完成 / 出错 / 已移除。
5. 错误码用「AH-E####」，显示在行尾等宽，不在正文里。

## 8. egui 落地

### 8.1 `install_style(ctx)` 骨架（可直接抄）

新建 `crates/ariahost-ui/src/theme.rs`，在 `PanelApp::build` 最前面调用一次：

```rust
//! 设计系统唯一入口：颜色 token + 字号阶梯 + 间距 + 控件视觉。
use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, Style, TextStyle, Visuals};

pub fn install(ctx: &egui::Context) {
    // 1) 主题：跟随系统（ThemePreference::System 是 egui 默认，显式写出来防止将来被改）
    ctx.set_theme(egui::ThemePreference::System);
    // 2) 两套视觉都要装，否则切到浅色主题时控件是「半个主题」
    ctx.style_mut_of(egui::Theme::Dark, |s| apply(s, &DARK));
    ctx.style_mut_of(egui::Theme::Light, |s| apply(s, &LIGHT));
}

fn apply(style: &mut Style, c: &Tokens) {
    // —— 字号阶梯（§2.1）：只允许这五档 ——
    let prop = |pt: f32| FontId::new(pt, FontFamily::Proportional);
    style.text_styles = [
        (TextStyle::Heading, prop(20.0)),
        (TextStyle::Name("AriaSubtitle".into()), prop(16.0)),
        (TextStyle::Name("AriaNumber".into()), FontId::new(16.0, FontFamily::Monospace)),
        (TextStyle::Body, prop(14.0)),
        (TextStyle::Button, prop(14.0)),
        (TextStyle::Small, prop(12.0)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ].into();

    // —— 间距（§3.2）——
    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.window_margin = Margin::same(16);
    sp.menu_margin = Margin::same(8);
    sp.button_padding = egui::vec2(12.0, 6.0);
    sp.interact_size = egui::vec2(48.0, 32.0);
    sp.indent = 20.0;
    sp.slider_width = 140.0;
    sp.slider_rail_height = 6.0;
    sp.combo_width = 160.0;
    sp.text_edit_width = 280.0;
    sp.icon_width = 16.0;
    sp.icon_width_inner = 10.0;
    sp.icon_spacing = 6.0;
    sp.tooltip_width = 420.0;
    sp.menu_width = 320.0;
    sp.combo_height = 240.0;
    sp.scroll.bar_width = 10.0;
    sp.scroll.floating = false;
    style.interaction.interact_radius = 6.0;
    style.interaction.tooltip_delay = 0.35;

    // —— 颜色（§4.1）——
    let v = &mut style.visuals;
    v.dark_mode = c.dark;
    v.panel_fill = c.bg_base;
    v.window_fill = c.bg_base;
    v.extreme_bg_color = c.input_bg;
    v.faint_bg_color = c.stripe;
    // 全局文字色：不要再靠 .weak() 压暗（§2.2 规则 4）
    v.override_text_color = Some(c.text);
    v.weak_text_alpha = 1.0;
    v.weak_text_color = Some(c.text2);
    v.hyperlink_color = c.accent_text;
    v.warn_fg_color = c.warn;
    v.error_fg_color = c.danger;
    v.window_stroke = Stroke::new(1.0, c.border);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = c.surface;
    w.noninteractive.weak_bg_fill = c.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, c.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, c.text);
    w.noninteractive.corner_radius = CornerRadius::same(6);
    w.inactive.weak_bg_fill = c.surface_hover;   // 次按钮底
    w.inactive.bg_fill = c.surface_hover;        // 勾选框底
    w.inactive.bg_stroke = Stroke::new(1.0, c.border);
    w.inactive.fg_stroke = Stroke::new(1.0, c.text);
    w.inactive.corner_radius = CornerRadius::same(6);
    w.hovered.weak_bg_fill = c.surface_active;
    w.hovered.bg_fill = c.surface_active;
    w.hovered.bg_stroke = Stroke::new(1.0, c.border_strong);
    w.hovered.fg_stroke = Stroke::new(1.0, c.text);
    w.hovered.corner_radius = CornerRadius::same(6);
    w.active.weak_bg_fill = c.surface_active;
    w.active.bg_fill = c.surface_active;
    w.active.bg_stroke = Stroke::new(1.0, c.accent);
    w.active.fg_stroke = Stroke::new(1.0, c.text);
    w.active.corner_radius = CornerRadius::same(6);
    w.open.weak_bg_fill = c.surface;
    w.open.bg_fill = c.surface;
    w.open.bg_stroke = Stroke::new(1.0, c.border);
    w.open.fg_stroke = Stroke::new(1.0, c.text);

    // 选中底色：accent 带 alpha（egui 没有 with_alpha，这里用未预乘构造）
    let a = if c.dark { 72 } else { 40 };
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(c.accent.r(), c.accent.g(), c.accent.b(), a);
    v.selection.stroke = Stroke::new(1.0, c.accent);
    v.disabled_alpha = 1.0;           // 禁用态用 text3，不再整体压 alpha
    v.striped = false;                // 表格自己控制斑马纹（§5.4），避免全局生效
}
```

> 注：
> - `weak_text_color` / `weak_text_alpha` 必须显式赋值而不是依赖默认；`override_text_color` 设为 `text`，未显式指定颜色的标签统一走主文字色。
> - 按上表，`strong_text_color()` 取 `widgets.active.text_color()` = `text`，与正文同色 —— **这是故意的**：`strong()` 从此不再有任何视觉差异，逼着实现者用字号/间距表达层级（§2.3）。
> - 现有 42 处 `.weak()` 会因为 `weak_text_color = text2` 立刻变成「12/13pt 的次要色」，视觉上先止血；但仍要按 §2.2 规则 4 逐个确认语义。

### 8.2 `Color32` 访问器

```rust
pub struct Tokens { /* 上表字段 */ pub dark: bool }
pub fn tokens(ui: &egui::Ui) -> &'static Tokens { /* 按 ui.visuals().dark_mode 返回 DARK/LIGHT */ }
```

绘制点写法：

```rust
ui.label(egui::RichText::new(text).color(theme::tokens(ui).text2));
```

### 8.3 「未铺底」修复（P0，必做）

`PanelApp::draw` 的根布局改为：

```rust
egui::CentralPanel::default()
    .frame(egui::Frame::NONE.fill(theme::tokens(ui).bg_base))
    .show(ui, |ui| { /* 现有全部内容 */ });
```
或者：为 `settings_page` / `service_page` / 壳顶栏各自补 `Frame::NONE.fill(bg_base)`。**推荐前者**（一处修全部），并顺手把壳顶栏从「根 ui 直接 horizontal」改成 `Panel::top`（`Panel` 自带 fill）。

验收判据：截图里任何像素都不再是 `#080808`；像素取样取「壳顶栏中点」与「设置页中央」两处（用例见 §9.2 第 4–6 条）。

### 8.4 egui 做得到 / 做不到

| 需求 | egui 0.37 能力 | 退化方案 |
|---|---|---|
| 真粗体 | **做不到**（系统字体只有一个字重，`strong()` 只换色） | 靠字号/颜色/间距建立层级（§2.3） |
| 字间距微调 | `RichText::extra_letter_spacing` | 仅标题加 0.2，正文不加 |
| 多行段落行高 | `RichText::line_height(Some(h))` 支持 | 显式给 20 / 17 |
| 悬停/选中底色 | `Visuals.widgets` 全支持 | — |
| 圆角/描边 | `CornerRadius` / `Stroke` | — |
| 阴影 | `Visuals::window_shadow` / `popup_shadow` | 卡片不用阴影（性能 + 内存） |
| 描边渐变 / 模糊 / 半透明毛玻璃 | 不支持 | 不做 |
| 文本省略号 | `Label::truncate()`（单行）/ `TextWrapMode::Truncate` | 名称列用 truncate + `on_hover_text` 全文 |
| 表格列宽拖拽 | `TableBuilder::resizable(true)` | 保留，但设 `at_least` 防止拖没 |
| 表格行内多按钮对齐 | 需要手动占位 | 固定槽位（§5.4） |
| 富文本 Tooltip | 支持 `LayoutJob` | 提示最多两行 |
| 动画/过渡 | 有 `animate_bool` 等 | **托盘常驻禁止动画**（架构约束），页面内可用 ≤120ms 的淡入 |
| 系统级无障碍 | 只有 `Button` 可执行 | 交互控件一律 `Button`（§7.2） |

### 8.5 禁止事项（会直接判缺陷）

1. 绘制点出现 `Color32::from_rgb/from_gray` 字面量。
2. 出现 `.size(<字面量>)`。
3. 用 `.weak()` 表达「次要文字」。
4. 在 `ui.horizontal` 里画**可能很长**的单行文本（不折行会把右侧按钮顶出屏幕，现状已发生）。
5. 在每帧路径里 `format!` 长字符串或重建 `Style`（`ctx.style().clone()` 每帧一次 = 明显掉帧）。
6. 为「对齐好看」引入新依赖（`rfd`、图标库、CSS 类框架一律不加；图标继续用 `include_bytes!` 一次性纹理）。
7. 修改 `Visuals` 的 `disabled_alpha` 之前不看 §5.1 的禁用态定义（禁用必须仍然可读，`text3` ≥3:1）。

### 8.6 与内存闸门（100 MB）的关系

- 字号变大 **不增加** 内存：字体图集按需生长，上限已在 `raw_input_hook` 里钉住（`atlas_cap`）。
- 卡片背景、边框、徽章都是矢量 tessellation，不生成纹理。
- **不允许**为视觉引入位图背景 / 阴影纹理 / 大图标集（现图标 361 KB 的上限不变）。
- 表格行高从 22 → 32 会让可见行数变少，`body().rows()` 仍是常数行高虚拟化，不影响内存。

## 9. 实施顺序与验收

### 9.1 批次（每批可独立验收，不要一次全改）

| 批次 | 内容 | 影响面 | 判定 |
|---|---|---|---|
| **P0** | ①`theme.rs` + `install_style` 接上；②修「未铺底」（§8.3）；③删 `.size(20.0)`/`"M2-2b"` 文案 | `app.rs` 少量 + 新文件 | 截图不再有 `#080808`；字号只出现 5 档 |
| **P1** | 状态卡重排（长文本折行、按钮先画）；任务表行高 32 + 操作列占位 + 数字右对齐；服务页/设置页按 §6 重排 | 三屏 | 760×480 下无重叠、无按钮出界 |
| **P2** | 组件收敛到 `widgets.rs`（按钮/输入/徽章/卡片/Banner）；`.weak()` 全部替换 | 全 UI | `grep Color32::from_` 只剩 `theme.rs` |
| **P3** | 诊断区改 2 列 + 复制日志；对话框统一；浅色主题逐屏复核 | 全 UI | 双主题截图各 5 张 |

### 9.2 验收清单（可逐条打勾）

1. `grep -rn "\.size(" crates/ariahost-ui/src` → 0 命中。
2. `grep -rn "Color32::from_" crates/ariahost-ui/src` → 只命中 `theme.rs`。
3. `grep -rn "\.weak()" crates/ariahost-ui/src` → 只剩装饰性用途（≤3 处，且不在承载信息的文字上）。
4. 深/浅两套主题下，`contrast_report.py` 全 PASS（脚本已内置门限）。
5. 760×480：主界面、服务页、设置页各截一张，检查「无文本被窗口右边缘裁切」「无按钮与标题重叠」「无 3 个以上按钮挤在一行」。
6. 1200×760：同上，另检查「表格 7 列不重叠」「操作列每行 x 对齐」。
7. 键盘：Tab 走完一屏能到达每个可点控件；`Esc` 关闭对话框；焦点环可见。
8. 记忆：`--seed-rows 200` 下滚动到第 150 行，等 2 次刷新（4s），滚动位置与选中行不变。
9. 内存：`--panel-selftest --seed-rows 200 --frames 600` 的 `private_kb` 不超过 100 MB（口径见 `docs/dev/test.md`，本规格不改判据）。
10. 截图留证到 `.scratch/design/shots/`（`after-*.png`），与本次 `01/07b/11/12/13/14` 同名位置一一对照。

## 10. 术语

| 词 | 含义 |
|---|---|
| token | 一组具名数值（颜色 / 间距 / 字号），代码里只有一个定义处 |
| chip | 小胶囊状状态徽章（底 + 文字 + 可选图标） |
| Banner | 页面内横幅（可关闭，占整行） |
| Notice | 一次性行内提示（3–8s 消失，无底色） |
| 壳 / shell | 所有页面共用的外框（顶栏 + 内容区 + 页脚） |
| 断点 | 窗口宽度阈值，决定列数与按钮折叠 |
