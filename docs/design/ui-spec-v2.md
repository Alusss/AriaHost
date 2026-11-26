# AriaHost 界面设计系统 v2（自包含 · 可直接施工）

> **本文取代 `ui-spec.md`。** v1 保留为历史审计记录；施工一律以本文为准。
> 适用：`crates/ariahost-ui`（eframe/egui 0.37，纯立即模式）与 `crates/ariahost-web`（零构建 vanilla 静态页）。
> 基线：`4705087`（v0.1.3 之后）。本文里的每个数字都能被复算：脚本 `.scratch/m5/contrast_v2.py`、截图 `.scratch/m5/shots/`。
> 写完本文时**没有改任何代码**；本文只做规格。

## §0 三句话结论

1. **「中文看着别扭」不是字体选错，是两套字体在混排**：egui 的 `Proportional` 默认第一位是内置 `Ubuntu-Light`（拉丁），CJK 走 `egui_system_font_provider` 的 Han fallback（本机 = Microsoft YaHei）。拉丁与中文来自两个不同设计的字体，字重/字宽/基线不一致——这是「别扭」的机械原因，不是审美口味。
2. **「绿色被过于淡化」是可测事实**：任务状态用的还是 `view.rs::tone_color` 里的旧硬编码色（`#6ECD82 / #F0BE5A / #EB6E6E / #96BEF0 / #8CC8A0`），在浅色页面底 `#F6F7F9` 上实测 **1.60–2.80:1**（要求 ≥4.5）。theme.rs 的令牌层没接管这几处。
3. **「分割线太多」是 13 处的事实**（`app.rs` 内）：4 处是壳与操作条边界（保留）、2 处本来就 `show_separator_line(false)`、**7 处是页面内分隔**（删 3 / 改间距 2 / 改卡片 2）。清查表见 §4.3。

## §1 现状测量（先量再改）

### 1.1 当前真实字号（读代码，不是印象）

字号已经集中到 `theme.rs::text_styles()`（`theme.rs:240-255`），这一点 v1 已落地；`.size()` 字面量在 `crates/ariahost-ui/src` 里已归零（`Select-String '\.size\(\d'` 0 命中）。

| 位置 | 取值来源 | 当前 pt | 行高（pt） | 备注 |
|---|---|---|---|---|
| 页面标题（「AriaHost · 下载」/「· 服务」/「· 设置」） | `TextStyle::Heading` | **20.0** | ≈26.6（字体度量 1.33×） | `theme.rs:247` |
| 卡片/分区标题 | `TextStyle::Name("AriaSubtitle")` | **16.0** | ≈21.3 | `theme.rs:248`；目前只有 `app.rs:3678` 一处显式用 |
| 大数值（合计/计数） | `TextStyle::Name("AriaNumber")` | **16.0**（等宽） | ≈21.3 | `theme.rs:249` |
| 正文 / 按钮 / 表格单元格 | `TextStyle::Body` / `Button` | **14.0** | ≈18.6 | `theme.rs:250-251` |
| 次要说明 / 表头 / 页脚 | `TextStyle::Small` | **12.0** | ≈16.0 | `theme.rs:252` |
| 等宽（路径/GID/端口/日志） | `TextStyle::Monospace` | **13.0** | ≈17.3 | `theme.rs:253` |
| 任务表行高 | `ROW_HEIGHT` | **32.0**（截图实测行距 32px） | — | 已从 22 提到 32 |
| 表头行高 | `TableBuilder::header(...)` | **32.0** | — | — |
| 任务状态文字（12pt 时） | `RichText::new(status).small()` | **12.0** | — | 颜色见 1.3 |

**判断**：阶梯本身没有「散乱」问题，问题在**层级差**与**字体家族**：
- 16 与 14 只差 1.14×，靠字号看不出两级（Fluent 的 Subtitle 20 / Body 14 = 1.43×）；
- 12pt 的次要文字在浅色底上若用 `text3` 只有 3.65:1，承载信息就偏弱；
- 拉丁/数字与中文来自不同字体（§2.1）。

### 1.2 当前颜色（截图取样 + 代码对照）

取样方法：`.scratch/m5/sample_colors.py`（Pillow，取区域内「离底色最远」的像素作为字形实心色，避开抗锯齿边缘）。

| 位置 | 截图取样 | 代码里的来源 | 说明 |
|---|---|---|---|
| 页面底 | `#F6F7F9` | `theme::LIGHT.bg_base` | 一致 |
| 卡片/表格底 | `#FFFFFF` | `LIGHT.surface` | 一致 |
| 表格偶行 | `#F5F7F9` | `LIGHT.stripe` | 与底 1.07:1，几乎看不出斑马纹 |
| 正文 | `#1F2327`（取样含抗锯齿） | `LIGHT.text = #1B1F23` | 一致 |
| 次要文字 | `#59626B` | `LIGHT.text2` | 一致（5.79:1） |
| 「下载中」绿 | `#6FCD83` | `view::tone_color(Good) = #6ECD82` | **旧色，未走令牌** |
| 「等待中」蓝 | `#97BEF0` | `app.rs::status_color` 硬编码 `(150,190,240)` | **旧色** |
| 「已暂停」黄 | `#F0BF5C` | `tone_color(Warn) = #F0BE5A` | **旧色** |
| 「已完成」绿 | `#8EC9A1` | `status_color` 硬编码 `(140,200,160)` | **旧色** |
| 「出错」红 | `#EC7A7A` | `tone_color(Bad) = #EB6E6E` | **旧色** |
| 进度条填充 | `#DCE8FC` | `selection.bg_fill = accent@40/255` 合成 | **过淡**（1.24:1） |
| 服务页横线 | 1px `#D6DBE1` | `LIGHT.border` | 与页面底 1.30:1（装饰线，仅记录） |

### 1.3 当前对比度（算出来的，谁低于 4.5:1）

公式：WCAG 2.1 相对亮度 `(L1+0.05)/(L2+0.05)`，全表见 `.scratch/m5/contrast-v2.md` 的 A 段。

| 组合 | 前景 | 背景 | 对比度 | 结论 |
|---|---|---|---|---|
| 任务「下载中」 | `#6ECD82` | `#F6F7F9` | **1.83:1** | **FAIL** |
| 任务「已暂停」 | `#F0BE5A` | `#F6F7F9` | **1.60:1** | **FAIL** |
| 任务「出错」 | `#EB6E6E` | `#F6F7F9` | **2.80:1** | **FAIL** |
| 任务「等待中」 | `#96BEF0` | `#F6F7F9` | **1.79:1** | **FAIL** |
| 任务「已完成」 | `#8CC8A0` | `#F6F7F9` | **1.80:1** | **FAIL** |
| 任务「未知/已移除」 | `#AAAAAA` | `#F6F7F9` | **2.17:1** | **FAIL** |
| 状态徽章 `[文字]` | `#6ECD82` | `#F6F7F9` | **1.83:1** | **FAIL** |
| 正文 | `#1B1F23` | `#F6F7F9` | 15.46:1 | PASS |
| 次要文字 | `#59626B` | `#F6F7F9` | 5.79:1 | PASS |
| 禁用文字 | `#7E878F` | `#FFFFFF` | 3.65:1 | PASS（禁用态） |
| 控件边框 | `#8A929C` | `#FFFFFF` | 3.15:1 | PASS |
| 装饰分隔线 | `#D6DBE1` | `#F6F7F9` | 1.30:1 | 装饰线不需要 3:1（但见 §4） |
| 进度填充 vs 卡片 | `#DCE8FC` | `#FFFFFF` | 1.24:1 | 只记录：**看起来「没在动」** |

**结论：7 个状态色全部不达标**，且都是「浅底上放浅色字」。这正是用户说的「绿色被过于淡化」——不是观感，是 1.8:1。

### 1.4 缺陷表（10 条，带图注）

截图：`.scratch/m5/shots/v013-{main-200,service,settings}.png`（v0.1.3 CI 产物 `.scratch/m4/rel-arts/AriaHost-0.1.3-x64/ariahost.exe`，`--panel-selftest --frames 100000 --seed-rows 200`，Windows 浅色主题，客户区 1200×760，`PrintWindow` 抓取未调色）。

| # | 位置（截图） | 问题 | v2 目标 |
|---|---|---|---|
| D1 | `v013-main-200` 操作列（右缘） | 三个行内按钮横向溢出，「移除」被窗口右缘裁掉一半 | 操作列固定 3 槽位 × 36pt，空槽占位；列宽 120（§5.1） |
| D2 | `v013-main-200` 状态列 | 6 种状态色 1.60–2.80:1，12pt 小字，浅底浅字 | 状态用 chip（12pt + 语义底），文字 ≥4.5:1（§3.4） |
| D3 | `v013-main-200` 进度列 | 填充是 accent@16% → `#DCE8FC`，1.24:1，看起来像空条 | 填充改实色 `accent`（4.63:1），条高 8（§5.1） |
| D4 | `v013-main-200` 整行 | 「暂停/继续/移除」三个按钮同时出现（语义互斥） | 按状态只出现其一；三槽位仍在（§5.1） |
| D5 | `v013-main-200` 文件名与标题 | 拉丁/数字是 Ubuntu-Light（细），中文是 YaHei，字重与字宽不齐 | 拉丁/数字 → Segoe UI；中文 → YaHei UI（§2.1） |
| D6 | `v013-main-200` 表格斑马纹 | `#F5F7F9` vs `#FFFFFF` = 1.07:1，等于没有 | 取消斑马纹，改行 hover + 32 行高（§4.1 规则 2） |
| D7 | `v013-settings` 单选组（右侧） | 「关闭按钮行为」三项横排，第三项「直接退出…」被右缘裁切 | 单选竖排，每项一行 28（§5.3） |
| D8 | `v013-settings`「配置文件」行 | 路径与「打开配置文件所在目录」按钮挤一行，按钮紧贴路径末字 | 路径独占一行（13pt 等宽，超长省略），按钮另起（§5.3） |
| D9 | `v013-service` 页面中部 | 状态卡下方 1 条横线、底部操作条上方 1 条横线，且区块之间无卡片包裹 | 删 2 条线，改 2 张卡片（§4.3） |
| D10 | `v013-service` 底部 | 4 个按钮等权（安装/启动/停止/卸载），卸载无危险色、与安装相邻 | 卸载移到最右 + danger 样式（§5.2） |

## §2 字体与字号（Type Scale）

### 2.1 字体家族链（已在本机核实存在性）

**本机环境**：Windows 11 Pro `10.0.26200`。核实手段：`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts` + `HKCU\...\Fonts` + `C:\Windows\Fonts` 文件。

| 角色 | 家族链（从左到右逐字形回退） | 本机证据 | 判定 |
|---|---|---|---|
| **拉丁 / 数字**（首选） | `Segoe UI` | `Segoe UI (TrueType) = segoeui.ttf`（959,752 B）；粗体 `segoeuib.ttf` | ✅ 存在，全 Windows 内置 |
| **中文** | `Microsoft YaHei UI` → `Microsoft YaHei` | `Microsoft YaHei & Microsoft YaHei UI (TrueType) = msyh.ttc`（19,704,352 B）；粗体 `msyhbd.ttc` | ✅ 存在（两者在同一个 TTC 里，是两个 face） |
| **等宽**（GID/速率/大小/端口/日志） | `Consolas` | `Consolas (TrueType) = consola.ttf`（453,088 B） | ✅ 存在，全 Windows 内置 |
| 等宽（可选，**不依赖**） | ~~`Cascadia Mono`~~ | 注册表 **0 项**；`C:\Windows\Fonts` 与用户字体目录**无文件**；本机装了 Windows Terminal 1.25 但**不带**该字体 | ❌ **从必需链移除**（Lead 建议的 Cascadia Mono 本机不存在，干净 Win10/11 也不能假定有） |
| 中文兜底（**不依赖**） | ~~`Noto Sans SC`~~ | 有，但来自 **HKCU 用户安装**（`HKCU\...\Fonts: Noto Sans SC (TrueType) = C:\WINDOWS\Fonts\Noto Sans SC (TrueType).otf`），不是 Windows 内置 | ❌ 不进必需链（只有它存在时才作为最后兜底） |
| 拉丁（**决定不用**） | ~~`Segoe UI Variable`~~ | 本机有 `SegUIVar.ttf`（Win11）；**Win10 没有**（Win11 21H2 起引入） | ❌ 不用：变量字体带光学尺寸，Win10/Win11 字宽会不一致，代价大于收益 |

**最终生效的 `Proportional` 链（按优先级）**：

```
Segoe UI  →  Microsoft YaHei UI  →  Microsoft YaHei  →  (egui 内置 Ubuntu-Light/emoji)  →  (系统 Han fallback: 简体中文)
```

为什么把 `Segoe UI` 放在**中文之前**：egui 的字体家族是**逐字形**回退（`FontPriority::Highest/Lowest` 的语义就是「这个字形在前者里找不到才用后者」，见 `epaint/text/font_definitions.rs:87-99`）。所以顺序决定的是「谁先被问」，不是「谁覆盖谁」：Segoe UI 没有汉字 → 汉字自动落到 YaHei UI；拉丁与数字则留在 Segoe UI。这正是 Windows 自身的中英混排做法（Segoe UI + YaHei UI 是系统级的成对设计）。

反过来（YaHei UI 在前）也能显示中文，但**拉丁会变成 YaHei 自带的拉丁**（字宽更宽、字重更轻），与 Windows 其他界面不一致——不采用。

**`Monospace` 链**：

```
Consolas  →  (egui 内置 Hack)  →  (系统 fallback：中文用 YaHei)
```

**粗体**：egui 不做字重合成。要「真粗」，必须把**粗体面**单独注册成一个命名家族：

```
FontFamily::Name("AriaBold")  =  Segoe UI Bold  →  Microsoft YaHei UI Bold  →  Microsoft YaHei Bold
```

使用范围仅限：`text.h1`、`text.h2`、表格表头、主按钮文字。正文一律 normal（中文粗体在小字号下发糊，且会破坏行距一致性）。

**加载方式（施工用的 API 与参数；不写实现代码）**

| 步骤 | API / 参数 | 值 |
|---|---|---|
| 1. 依赖 | `crates/ariahost-ui/Cargo.toml` 增加直接依赖 | `fontique = "0.11.1"` —— 它**已在 `Cargo.lock`**（`egui_system_font_provider` 的依赖），新增直接依赖**不引入新 crate**、不改 lock 解析 |
| 2. 枚举系统字体 | `fontique::Collection::new(CollectionOptions { shared: false, system_fonts: true })` | 一次，启动时；与 `fonts::install` 同处 |
| 3. 按家族名查面 | `collection.query(&mut cache)` → `query.set_families([QueryFamily::Named("Segoe UI".into())])` → 取第一个能渲染的 `QueryFont` | 依次查 `Segoe UI`、`Microsoft YaHei UI`、`Microsoft YaHei`、`Segoe UI Bold`、`Microsoft YaHei UI Bold`、`Consolas`；查不到就**跳过**（不 panic） |
| 4. 注入 egui | `egui::Context::add_font(FontInsert::new(name, FontData::from_blob(qf.blob, qf.index), vec![InsertFontFamily { family, priority: FontPriority::Highest }]))` | blob 是 fontique 的 **mmap**（不复制进堆）；`qf.index` 是 TTC 里的 face 索引（msyh.ttc 有多个 face，**不要硬编码 0**） |
| 5. 顺序 | ⚠️ `Highest` = 插到该家族最前；**后插的更靠前**（文档示例用 `insert(0, ...)`，`font_definitions.rs:36`） | 要得到 `[Segoe UI, YaHei UI]`，**先插 YaHei UI，再插 Segoe UI**。此为【推断】：编译后若顺序反了，交换两次 `add_font` 的调用顺序即可（1 分钟可验） |
| 6. 保留系统 fallback | `ctx.add_font_provider(Arc::new(SystemFontProvider::new().with_locale(Some("zh-CN"))))` 保持不变 | `zh-CN` 是对的：provider 内部会把简体中文 locale 映射到 `zh-SG` 的 Han fallback，正是为了避开日文字形（见 `egui_system_font_provider-0.37.0/src/lib.rs:184-212`） |

**内存代价**：三者 mmap 合计约 19.7MB（msyh.ttc）+ 0.96MB（segoeui.ttf）+ 0.45MB（consola.ttf）的**文件映射**，不是堆分配；egui 只会把实际用到的字形栅格化进图集。对 100MB 闸门的实测影响需 builder-ui 出一次 `--panel-selftest` 的 `private_kb` 对比（见 §7）。

### 2.2 字号阶梯（v2，唯一允许的字号来源）

| 令牌 | `TextStyle` | 字号 pt | 行高 pt | 字重 | 用途 | 示例文案 |
|---|---|---|---|---|---|---|
| `text.h1` | `Heading` | **20** | 28 | **bold**（`AriaBold` 家族） | 每屏唯一页面标题 | 「AriaHost · 下载」 |
| `text.h2` | `Name("AriaSubtitle")` | **16** | 22 | **bold** | 卡片标题、分区标题 | 「Windows 服务」「下载核心未就绪」 |
| `text.body` | `Body` / `Button` | **14** | 20 | normal | 正文、按钮、输入框、对话框 | 「把链接粘到上面，回车开始」 |
| `text.label` | `Body`（同 14）+ `text2` 色 | **14** | 20 | normal | 表单标签（只靠颜色区分，不缩字号） | 「下载目录」「启动方式」 |
| `text.caption` | `Small` | **12** | 17 | normal | 表头、页脚、单位、时间、辅助说明 | 「32 分 52 秒」「v0.1.3」 |
| `text.mono` | `Monospace` | **13** | 18 | normal | 路径、GID、端口、日志、表格数值 | `127.0.0.1:6800` |
| `text.metric` | `Name("AriaNumber")` | **16** | 22 | normal | 汇总处大数值（合计速度、可回收空间） | 「12.8 MiB/s」 |

**基线 = 14pt 的理由**：中文 12pt 是**下限**（再小汉字糊），14pt 是 Windows Fluent 2 的 Body 档，也是中文 100% DPI 下笔画最清楚的一档；egui 出厂 13pt 夹在中间，既不在任何成体系的阶梯上、又比 Fluent Body 小一号。**取 14，不取 13**。

**为什么不用 12/14/16/20/24**：Fluent 2 的阶梯是 12 / 14 / 16 / 20 / 28（Caption / Body / Body Large / Subtitle / Title），**没有 24**。24 既不是系统档位，又会把 48pt 的顶栏顶到 56–64，压缩任务表可视行数（1200×760 下少 1–2 行）。28 更大，只适合「关于」页这类整屏文字。所以 v2 用 **12 / 14 / 16 / 20**（+ 等宽 13 / 16），与 Fluent 对齐。
- 层级差：20/16 = 1.25×（标题→卡片标题），16/14 = 1.14×（**靠 bold + 上方 24pt 间距**拉开，不靠字号），14/12 = 1.17×。
- 一屏最多两级标题：`text.h1` 只出现 1 次（页面标题），其余一律 `text.h2`。

### 2.3 中英混排的四条硬规则

1. **字号用 pt，不用 px**：egui 的 `FontId.size` 是逻辑 pt，DPI 缩放由 `pixels_per_point` 处理。禁止为了「看起来大一点」写 15.5 / 17 这类非档位值。
2. **行高固定**：`text.body` 的行高 20（1.43×），`text.caption` 17（1.42×）。多行段落显式 `RichText::line_height(Some(20.0))`；单行标签不设（跟随字体度量）。
3. **数字用等宽 + tabular**：所有「会变的数字列」（大小/速度/剩余/进度百分比/GID）用 `text.mono`；`RichText` 里不要混排中文与数字的同一段落（中文用 YaHei 的字宽，数字会是 Segoe 的宽度，视觉上会「跳」）。
4. **中英之间加半角空格**（文案层）：`aria2 核心`、`AH-E5002 需要管理员`——中文与拉丁/数字之间留一个半角空格，是 Windows 中文界面的惯例；**不要**用 letter-spacing 模拟（egui 只有 `extra_letter_spacing`，会把整句拉散，且中文不支持字距）。

## §3 颜色令牌（含对比度契约）

### 3.1 令牌表（浅 / 深 / 用途 / 实测对比度）

门槛：**正文类 ≥4.5:1**；**大字（≥16pt bold 或 ≥20pt）与图标/图形 ≥3:1**。低于门槛的**不进表**。全表见 `.scratch/m5/contrast-v2.md` B 段。

| 令牌 | 浅色 | 深色 | 用途 | 对比度（前景/背景） | 结论 |
|---|---|---|---|---|---|
| `bg.base` | `#F6F7F9` | `#15181B` | 页面底、窗口清屏色 | —（底色） | — |
| `bg.card` | `#FFFFFF` | `#1C2024` | 卡片、表格容器、顶栏/页脚 | —（底色） | — |
| `bg.hover` | `#EEF1F5` | `#242A2F` | 行 hover、次按钮底 | —（底色） | — |
| `bg.input` | `#FFFFFF` | `#111417` | 输入框、进度槽 | —（底色） | — |
| `text.body` | `#1B1F23` | `#E6E9EC` | 正文 | 16.58:1 / 13.45:1（对 card） | PASS |
| `text.caption` | `#59626B` | `#A7B0B8` | 次要文字 | 6.21:1 / 7.45:1 | PASS |
| `text.disabled` | `#7E878F` | `#838C94` | 禁用 | 3.65:1 / 4.79:1 | PASS（≥3:1） |
| `line.border` | `#D6DBE1` | `#353C43` | 卡片 1px 边框、**仅此用途** | 1.30:1（装饰，只做层级暗示） | 记录 |
| `line.control` | `#8A929C` | `#6B757E` | 输入框/勾选框边界 | 3.15:1 / 3.49:1 | PASS |
| `accent` | `#1F6FEB` | `#4C8DF6` | 主按钮底、进度实色、焦点环 | 白字 4.63:1 / 深字 5.77:1 | PASS |
| `accent.text` | `#0A58CA` | `#86B4FF` | 链接、可点文字 | 6.44:1 / 7.79:1 | PASS |
| **`ok.dot`** | **`#00C853`** | **`#00C853`** | **运行中指示点（正绿本体）** | 深 7.32:1；浅 2.24:1 → **必须配 `ok.ring`** | 见下注 |
| **`ok.ring`** | **`#0B6B34`** | `#0B6B34` | 指示点 1px 描边（承担 3:1 边界） | 6.63:1 / 6.19:1 | PASS |
| **`ok.text`** | **`#15803D`** | **`#00C853`** | 「运行中」「下载中」「已完成」文字 | 5.02:1（card）/ 4.68:1（base）；深 7.32:1 | PASS |
| `warn.text` | `#8A5A00` | `#E0A93B` | 警告文字 | 5.93:1 / 7.73:1 | PASS |
| `danger.text` | `#C42B1C` | `#F0616D` | 危险文字 | 5.66:1 / 5.18:1 | PASS |
| `info.text` | `#0969DA` | `#4FA8F5` | 信息/进行中文字 | 5.19:1 / 6.44:1 | PASS |
| `chip.ok` | `ok.dot` @10%（浅）/16%（深）叠 card | 同左 | 状态胶囊底 | 文字 `ok.text` 对该底 4.60:1（浅） | PASS |

> **`ok.dot` 的唯一用法**：`#00C853` 只做**填充**，且**必须**带 1px `ok.ring` 描边。理由：#00C853 在浅色卡片上是 2.24:1，作为「图形对象」不达 3:1；描边 `#0B6B34` 与卡片 6.63:1，边界达标，而填充保留「正绿不淡化」的观感。深色主题下 `#00C853` 本身 7.32:1，描边可选。

**状态色映射（六态服务 + 七态任务）**

| 状态 | 文字令牌 | 点/胶囊底 | 说明 |
|---|---|---|---|
| 服务：运行中 | `ok.text` | `ok.dot` + `ok.ring` | 正绿点 |
| 服务：启动中 / 停止中 | `info.text` | `info.text` @10% | 配 spinner |
| 服务：已停止 | `warn.text` | `warn.text` @10% | — |
| 服务：未安装 / 未知 | `text.caption` | `text.caption` @12% | 灰 |
| 任务：下载中 / 做种 | `ok.text` | `ok.dot` + ring | 正绿点 |
| 任务：等待中 | `info.text` | — | — |
| 任务：已暂停 | `warn.text` | — | — |
| 任务：已完成 | `ok.text` | — | — |
| 任务：出错 | `danger.text` | — | — |
| 任务：已移除 / 未知 | `text.caption` | — | — |

### 3.2 旧 → 新映射（builder-ui 照这张表改）

| 现在（v0.1.3） | 改成 | 备注 |
|---|---|---|
| `view.rs::tone_color(Good)` = `(110,205,130)` | `ok.text` | 浅 `#15803D` / 深 `#00C853` |
| `tone_color(Warn)` = `(240,190,90)` | `warn.text` | — |
| `tone_color(Bad)` = `(235,110,110)` | `danger.text` | — |
| `tone_color(Idle)` = `(170,170,170)` | `text.caption` | — |
| `app.rs::status_color` 里的 `(150,190,240)` | `info.text` | 等待中 |
| `status_color` 里的 `(140,200,160)` | `ok.text` | 已完成 |
| `theme::LIGHT/DARK.success` = `#1A7F37` / `#4CC26B` | 拆成 `ok.dot`（`#00C853`）+ `ok.text`（`#15803D` / `#00C853`） | **一个绿不许两种用途** |
| `theme::*_success_text` = `#146C2E` / `#6FD98C` | `ok.text` | 深色下 `#6FD98C` 仍可留作「已完成」的浅一档，但**运行中一律 `#00C853`**（用户点名） |
| `v.selection.bg_fill = accent@40/255`（进度条默认填充） | 进度条显式 `fill(accent)` 实色 | 现状 1.24:1，看着像空条 |
| `view.rs::badge()` 的 `[文字]` 方括号写法 | chip（§3.4） | 命令行风格，不进 GUI |

**硬规则**：`tone_color` / `status_color` 这两个函数在 v2 里**只允许返回 token**，函数体内不得出现 `Color32::from_rgb`。

## §4 卡片化分隔

### 4.1 三条判定规则（按顺序问）

1. **这是两个不同「功能区」吗？** → 用**卡片**（`bg.card` + 1px `line.border` + 圆角 8 + 内边距 16），卡片之间 16 间距。例：下载页「状态卡 / 新建下载卡 / 任务卡」。
2. **这是同一个功能区里的两组内容吗？** → 用**间距**（12–16pt）+ 小标题（`text.h2`），**不要**用线。例：卡片内「核心版本 / RPC 地址」两行。
3. **只有以下三种情况才允许线**：① 壳边界（顶栏下沿、页脚上沿、底部操作条上沿）；② 表格的**行**分隔（且只在密集模式，默认不用）；③ 菜单项之间的分组。**页面内的水平线一根都不许有。**

### 4.2 卡片规格

| 属性 | 值 | 来源 |
|---|---|---|
| 底 / 边框 | `bg.card` / 1pt `line.border` | `theme::card(ui)`（已存在，`theme.rs:213-220`） |
| 圆角 | 8 | 同上 |
| 内边距 | 16（紧凑卡 12） | `theme::card` / `theme::card_tight` |
| 卡片间距 | 16（纵向） | `SP_4` |
| 标题行 | `text.h2` + bold；右侧允许 1 个次要动作（`text.caption` 文字按钮） | §2.2 |
| 卡片内分区 | 间距 12 + `text.caption` 小标题；不加线 | 规则 2 |

### 4.3 分割线清查表（`app.rs` 13 处：保留 4、已是 false 2、页面内 7 → 删 3 / 改间距 2 / 改卡片 2）

扫描方法：`grep -n 'ui.separator()\|show_separator_line(' crates/ariahost-ui/src/*.rs`（行号为 4705087 时刻）。

| # | 位置 | 现状 | v2 处置 |
|---|---|---|---|
| 1 | `app.rs:3638` 主壳 `Panel::top`（`top_bar_frame`） | `show_separator_line(true)` | **保留**（壳边界，规则 3①） |
| 2 | `app.rs:3686` 主壳 `Panel::bottom`（页脚） | `true` | **保留** |
| 3 | `app.rs:1148` 服务页底部操作条 | `true` | **保留**（操作条边界） |
| 4 | `app.rs:1353` 设置页底部操作条 | `true` | **保留** |
| 5 | `app.rs:3670` 主壳页面区（`page_frame`） | `false` | 保持关闭 |
| 6 | `app.rs:1135` 服务页头部 | `false` | 保持关闭 |
| 7 | `app.rs:1275` `show_service_card` 内（「查看服务日志/刷新状态」上方） | `ui.separator()` | **删**：把这两个按钮并进状态卡底部动作区（间距 12） |
| 8 | `app.rs:1377` 设置页「未保存离开」块之后 | `ui.separator()` | **删**：改成对话框（§5.3），不再有块内分隔 |
| 9 | `app.rs:1390` 设置页左导航与右内容之间 | `ui.separator()`（竖线） | **改间距**：导航宽 160 + 右内容左内边距 24；导航自己用 `bg.card` 竖条承载 |
| 10 | `app.rs:1486` 设置页底部条：`恢复默认` / `重新配置下载核心` | `ui.separator()` | **改间距**（24）；不同权重的按钮不用竖线切 |
| 11 | `app.rs:1509` 设置页底部条：`重新配置下载核心` / 状态文字 | `ui.separator()` | **删**：状态文字用 `text.caption`，靠颜色退后 |
| 12 | `app.rs:3121` `show_footer` 诊断区：facts / probe 之间 | `ui.separator()` | **改卡片**：诊断区改成 3 张紧凑卡（运行信息 / 输入法自检 / 日志） |
| 13 | `app.rs:3123` 诊断区：probe / log 之间 | `ui.separator()` | 同上 |
| — | `tray.rs:143-151` 托盘菜单两项分隔 | `PredefinedMenuItem::separator()` | **保留**（规则 3③，系统菜单惯例） |

**改完的效果**：页面内 0 根线；层级完全由「卡片 + 间距 + 字号」表达。

## §5 四屏线框

> 记号：`▣` 主操作（唯一，accent 实色）；`▢` 次操作；`·` 辅助信息（`text.caption`）；`[chip]` 状态胶囊；`——` 卡片边界。

### 5.1 下载页

```
┌ 顶栏（bg.card, 高 48, 下沿 1px line.border）────────────────────────────────┐
│ AriaHost · 下载                                        ▢ 服务   ▢ 设置      │
└────────────────────────────────────────────────────────────────────────────┘
┌ 页面区（bg.base, 左右 24, 上 16）──────────────────────────────────────────┐
│ ┌ 状态卡 ─────────────────────────────────────────────────────────────────┐ │
│ │ [● 已停止]  aria2 核心未启动                          ▣ 启动 aria2  ▢ 打开目录│ │
│ │ · 核心 1.37.0（托管）· RPC 127.0.0.1:6800                                │ │
│ │ · 下一步：点「启动 aria2」开始下载                                       │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
│ ┌ 新建下载 ───────────────────────────────────────────────────────────────┐ │
│ │ [链接输入框 400 ............................]  ▢ 开始                    │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
│ ┌ 任务（200）─────────────────────────────── ▢ 刷新   ▢ ⋯ ────────────────┐ │
│ │ 名称                    大小      进度      速度     剩余    状态   操作  │ │
│ │ …                      64.0 MiB  ▰▰▱▱ 33%  12.8KiB/s 5:42  [下载中] ▢ ▢ ▢ │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────────┘
┌ 页脚（bg.card, 高 32, 上沿 1px）──────────────────────────────────────────┐
│ · AriaHost 0.1.3 · 核心 aria2 1.37.0（托管）                    ▢ 运行信息 ▾ │
└────────────────────────────────────────────────────────────────────────────┘
```

规则：
- 一屏一个 `▣`（启动 aria2）。「打开下载目录」「刷新」「⋯」全部是 `▢`。
- 任务表：行高 32，表头 12pt `text.caption`，单元格 14pt；名称列 `remainder().at_least(240)`；大小/速度/剩余**右对齐 + `text.mono`**；进度列 160（条 8 高，百分比在条右侧 13pt 等宽）。
- 操作列：固定 3 槽 × 36，空槽占位（解决 D1/D4）；按钮只有 hover 才显示描边，常态无边框（降低视觉噪音）。
- 状态列用 `[chip]`（12pt，§3.1），不再用裸色字。
- 删除斑马纹（D6）；行 hover 用 `bg.hover`。

### 5.2 服务页

```
┌ 顶栏 ──────────────────────────────────────────────────────────────────────┐
│ AriaHost · 服务                                                     ▢ ← 返回 │
└────────────────────────────────────────────────────────────────────────────┘
┌ 页面区 ────────────────────────────────────────────────────────────────────┐
│ ┌ 服务状态 ──────────────────────────────── ▢ 刷新状态 ──────────────────┐ │
│ │ ● 已停止            · 服务名 AriaHost                                  │ │
│ │ 启动方式  手动触发（需要时由面板/SCM 启动）                             │ │
│ │ PID       —                                                             │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
│ ┌ 权限与日志 ────────────────────────────────────────────────────────────┐ │
│ │ · 当前以普通权限运行：装/卸/启停各弹一次 Windows 授权框                 │ │
│ │ · 服务日志：%ProgramData%\AriaHost\logs（需管理员）   ▢ 查看服务日志      │ │
│ └─────────────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────────┘
┌ 底部操作条（bg.card, 高 56, 上沿 1px）────────────────────────────────────┐
│ ▢ 安装为服务   ▢ 启动服务   ▢ 停止服务                    ▣/⚠ 卸载服务     │
└────────────────────────────────────────────────────────────────────────────┘
```

规则：状态值 16pt（`text.h2`）但**不加粗**（状态词比标题更需要「读」而不是「强调」）；运行中用 `ok.dot` + `ok.text`；卸载固定最右、`danger` 样式、间距 ≥24（D10）。
- 引用 §4.3：删掉原状态卡下方那条线（#7），改成两张卡。

### 5.3 设置页

```
┌ 顶栏 ──────────────────────────────────────────────────────────────────────┐
│ AriaHost · 设置                                                     ▢ ← 返回 │
└────────────────────────────────────────────────────────────────────────────┘
┌ 页面区 ────────────────────────────────────────────────────────────────────┐
│ · 配置文件  C:\Users\…\AriaHost\config.toml        （13pt 等宽，超长省略）  │
│ ┌ 导航 160 ┬ 内容（最大宽 720）──────────────────────────────────────────┐ │
│ │ ▢ 下载    │ 下载设置                          ← text.h2 + bold           │ │
│ │ ▢ 网络    │ 下载目录  […………………………]   · 默认 ~/Downloads               │ │
│ │ ▢ 界面    │ 重名文件  ☑ 自动改名                                      │ │
│ │           │ · 这些值写进 aria2.conf，改完需重启 aria2                   │ │
│ └───────────┴─────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────────────────┘
┌ 底部操作条（高 56）────────────────────────────────────────────────────────┐
│ ▣ 保存    ▢ 撤销更改    ▢ 恢复默认                       · 没有改动        │
└────────────────────────────────────────────────────────────────────────────┘
```

规则：
- 「配置文件」路径独占一行（D8）；按钮 `▢ 打开目录` 放行尾。
- 表单行：标签列 160（`text.label` = 14pt + `text2`）、控件列 280、行高 40；错误提示 12pt `danger.text` 在控件下方。
- **三档单选竖排**（D7），每项一行 28。
- 生效时机只在需要重启的项上写「重启后生效」（12pt `text.caption`）；未接线的项**不显示**。
- 左导航用 `bg.card` 竖条 + 选中项左侧 2px `accent` 条（替代 #9 的竖线）。

### 5.4 浏览器管理页（`crates/ariahost-web`，零构建 vanilla）

现状：`assets/style.css` 是**只有深色**的一屏（`--bg:#14161a`），字体栈 `"Segoe UI", "Microsoft YaHei", system-ui`（缺 `UI` 变体），全用 `border-bottom` 做行分隔 → 与面板（跟系统主题、卡片化）不一致。

v2 要求：
1. **跟随系统主题**（`prefers-color-scheme`），与面板同源色值；
2. 字体栈补 `Microsoft YaHei UI`，并把等宽显式给出；
3. 行分隔线改为**卡片 + 行 hover**，`<table>` 只保留表头下沿 1px；
4. 状态用胶囊 + 正绿点，与面板同一套语义色。

**可直接抄的 CSS（替换 `style.css` 的对应段；变量段整段替换）**

```css
以下片段 = **`crates/ariahost-web/assets/style.css` 逐字副本**（task-89 起，由脚本维护：改样式先改代码，再整段回填本文，两处不再各写一份）。约束（CSP/零构建）：不引外部字体与框架、页面不含内联 `style=`。
/* AriaHost 网页管理页样式（视觉对齐 docs/design/ui-spec-v2.md §5.4）。
 *
 * 四条硬约束（都由 src/assets.rs 的单测守着）：
 * 1. **令牌层**：颜色/字号/圆角/字体只在这里定义一次；规则里一律 `var(--x)`，
 *    除变量定义行外不许出现裸色值、裸字号。改色 / 改字号 = 只改此处。
 * 2. **唯一色板是面板的 theme.rs**（`crates/ariahost-ui/src/theme.rs` 的 LIGHT / DARK）。
 *    网页页**不自带第二套值**：令牌逐个等于对应字段，改名只在下面这张表里发生。
 * 3. **字号五档对齐 `theme.rs::text_styles()`**：20 / 16 / 14 / 13 / 12。
 * 4. **零外部依赖**：不引框架、字体文件、图标库（CSP 是 default-src 'self'，
 *    引外站只会被拦成半张裸页）。
 *
 * 颜色令牌 ↔ theme.rs 字段（值必须逐字相等，单测 `style_css_palette_matches_the_panel_theme`
 * 把两套主题的期望值钉死；要改色请改 theme.rs，再同步搬到这里）：
 *
 *   --bg          ← bg_base          --border        ← border（只做装饰性分隔）
 *   --card        ← surface          --border-strong ← border_strong（控件边界，≥3:1）
 *   --hover       ← surface_hover    --text          ← text
 *   --muted       ← text2            --link          ← accent_text
 *   --accent      ← accent           --on-accent     ← on_accent
 *   --ok          ← success_text     --bad           ← danger_text
 *   --warn        ← warn_text        --ok-dot        ← ok_dot
 *   --ok-ring     ← ok_ring
 *
 * 注意：令牌行**不要写行尾注释**——单测按行解析 `--name: 值;`，行尾注释会被算进值里。
 */

:root {
  color-scheme: light dark;

  /* ---- 颜色：等于 theme.rs 的 LIGHT ---- */
  --bg: #f6f7f9;
  --card: #ffffff;
  --hover: #eef1f5;
  /* 装饰性分隔（卡片描边、表头下沿）：不承担对比度门槛，别拿它画控件边界。 */
  --border: #d6dbe1;
  /* 控件边界（输入框、按钮描边）：落在 --card 上 3.15:1，满足 WCAG 1.4.11 的 ≥3:1。
     注意它压在 --card 上才算得出 3.1；压在 --bg 上只有 2.93——所以控件一律放进卡片里。 */
  --border-strong: #8a929c;
  --text: #1b1f23;
  --muted: #59626b;
  --link: #0a58ca;
  --accent: #1f6feb;
  --on-accent: #ffffff;
  --ok: #15803d;
  --bad: #c42b1c;
  /* 以下两个在 theme.rs 里深浅两套取值完全相同，故只在 :root 定义一次。 */
  --ok-dot: #00c853;
  --ok-ring: #0b6b34;
  /* 当前页面还没有警示文案（.notice 用的是 --bad）；保留以对齐 theme.rs 的 warn_text。 */
  --warn: #8a5a00;

  /* ---- 字号：五档，值等于 theme.rs::text_styles() ---- */
  --fs-heading: 20px; /* Heading */
  --fs-subtitle: 16px; /* AriaSubtitle：卡片/区块标题这一层 */
  --fs-body: 14px; /* Body / Button */
  --fs-mono: 13px; /* Monospace：只给数字列，别拿去当正文字号 */
  --fs-small: 12px; /* Small：表头、状态字、说明行 */

  /* ---- 圆角（与 theme.rs 同值：卡片 8、控件 6）---- */
  --radius-card: 8px;
  --radius-ctl: 6px;

  /* ---- 字体链（中文系统必须有 YaHei UI 变体，见规格 §2.1）---- */
  --font-ui: "Segoe UI", "Microsoft YaHei UI", "Microsoft YaHei", system-ui, sans-serif;
  --font-mono: "Consolas", ui-monospace, SFMono-Regular, Menlo, monospace;
}

@media (prefers-color-scheme: dark) {
  :root {
    /* ---- 颜色：等于 theme.rs 的 DARK ---- */
    --bg: #15181b;
    --card: #1c2024;
    --hover: #242a2f;
    --border: #353c43;
    /* 压在 --card 上 3.49:1 */
    --border-strong: #6b757e;
    --text: #e6e9ec;
    --muted: #a7b0b8;
    --link: #86b4ff;
    --accent: #4c8df6;
    --on-accent: #0b121c;
    --ok: #00c853;
    --bad: #f0616d;
    --warn: #e0a93b;
  }
}

/* 内容列的**宽度上限与水平居中在 body**：顶部标题（h1）与卡片同处一个流里，
   两边留白用 auto，所以 360~1920 之间都是「内容列居中 + 左右各 24px 内边距」。
   为什么不放 main：main 只管内容区，header 在它外面，放 main 会只居中卡片、标题仍在左边。 */
html {
  /* body 收窄后页面两侧露出的画布仍要是页面底色（否则 1920 下出现白条）。 */
  background: var(--bg);
}

body {
  max-width: 1080px;
  margin: 0 auto;
  padding: 16px 24px 48px;
  background: var(--bg);
  color: var(--text);
  font-family: var(--font-ui);
  font-size: var(--fs-body);
  line-height: 20px;
  -webkit-font-smoothing: antialiased;
}

/* ---- 顶栏 ---- */
header {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 12px;
  padding: 8px 0 16px;
}
h1 {
  margin: 0;
  font-size: var(--fs-heading);
  line-height: 28px;
  font-weight: 600;
}
/* 区块标题：16px 这一档必须真的被用上，否则网页页没有「次级标题」层。 */
h2 {
  margin: 0 0 12px;
  font-size: var(--fs-subtitle);
  line-height: 24px;
  font-weight: 600;
}
.status {
  margin: 0;
  color: var(--muted);
  font-size: var(--fs-small);
  line-height: 17px;
}
.status.bad {
  color: var(--bad);
}

/* main 不再自带 max-width：宽度上限与居中统一由 body 负责（见上）。 */

/* ---- 卡片：替代页面级分割线（规格 §4.1）---- */
/* 控件必须放进卡片：--border-strong 只保证在 --card 上过 3:1。 */
.card {
  background: var(--card);
  border: 1px solid var(--border);
  border-radius: var(--radius-card);
  padding: 16px;
  margin-bottom: 16px;
}

/* ---- 添加行 ---- */
/* 不写 margin：外距统一由 `.card` 提供（表单同时挂了 `.add card`） */
.add {
  display: flex;
  gap: 8px;
}
.add input {
  flex: 1;
  min-width: 240px;
  padding: 8px 10px;
  color: var(--text);
  background: var(--card);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-ctl);
  font-family: var(--font-ui);
  font-size: var(--fs-body);
  line-height: 20px;
}
.add input:focus {
  outline: 2px solid var(--accent);
  outline-offset: 1px;
  border-color: var(--accent);
}

button {
  padding: 8px 14px;
  color: var(--text);
  background: var(--hover);
  border: 1px solid var(--border-strong);
  border-radius: var(--radius-ctl);
  font-family: var(--font-ui);
  font-size: var(--fs-body);
  line-height: 20px;
  cursor: pointer;
}
button:hover {
  background: var(--card);
  border-color: var(--accent);
}
.add button {
  background: var(--accent);
  color: var(--on-accent);
  border-color: var(--accent);
  font-weight: 600;
}
.add button:hover {
  filter: brightness(1.08);
}

a {
  color: var(--link);
}

/* ---- 表格：无行线，只留表头下沿 1px；行 hover ---- */
/* 窄屏（375px 手机）时五列表必然放不下：滚动**发生在卡片内部**（这层容器），
   而不是让整个页面横向溢出——`documentElement.scrollWidth <= clientWidth` 必须成立。
   `table` 的 `min-width` 保证列不被压扁成两行文字：小于它就在容器里横向滚。 */
.table-wrap {
  overflow-x: auto;
}
table {
  width: 100%;
  min-width: 620px;
  border-collapse: collapse;
}
thead th {
  padding: 8px 10px;
  color: var(--muted);
  font-size: var(--fs-small);
  line-height: 17px;
  font-weight: 500;
  text-align: left;
  border-bottom: 1px solid var(--border);
  white-space: nowrap;
}
tbody td {
  padding: 10px;
  vertical-align: middle;
  font-size: var(--fs-body);
  line-height: 20px;
}
tbody tr:hover {
  background: var(--hover);
}
td.num {
  font-family: var(--font-mono);
  font-size: var(--fs-mono);
  text-align: right;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

/* 任务名与它下面那行说明 */
.name {
  max-width: 460px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.meta {
  margin-top: 4px;
  color: var(--muted);
  font-size: var(--fs-small);
  line-height: 17px;
}
.actions {
  white-space: nowrap;
}
.actions button {
  margin-right: 6px;
  padding: 4px 10px;
  font-size: var(--fs-small);
}
.actions button:last-child {
  margin-right: 0;
}

/* ---- 状态：正绿点 + 深绿字（#00c853 只作填充，边界靠 --ok-ring 描边）---- */
.state {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: var(--fs-small);
  line-height: 17px;
  color: var(--muted);
}
.state.active {
  color: var(--ok);
}
.state.active::before {
  content: "";
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--ok-dot);
  box-shadow: 0 0 0 1px var(--ok-ring);
}
.state.error {
  color: var(--bad);
}

/* ---- 进度条：实色，不用半透明 ---- */
.bar {
  width: 160px;
  height: 8px;
  margin-bottom: 4px;
  background: var(--hover);
  border-radius: 4px;
  overflow: hidden;
}
.fill {
  height: 100%;
  width: 0;
  background: var(--accent);
}
.percent {
  color: var(--muted);
  font-family: var(--font-mono);
  font-size: var(--fs-mono);
  font-variant-numeric: tabular-nums;
}

/* ---- 通告与空态 ---- */
.notice {
  margin: 0 0 16px;
  padding: 10px 12px;
  border-radius: var(--radius-ctl);
  border-left: 3px solid var(--bad);
  background: var(--card);
  color: var(--text);
}
.empty {
  color: var(--muted);
  padding: 24px 4px;
}

@media (max-width: 760px) {
  .bar {
    width: 96px;
  }
  .name {
    max-width: 180px;
  }
}
```

对比度（`.scratch/m5/contrast-v2.md` C 段）：浅色 `--text/--card` 16.58:1、`--muted` 6.21:1、`--link` 6.44:1、`--ok` 5.02:1、`--bad` 5.66:1、主按钮白字 4.63:1；深色全部 ≥7.3:1。`--ok-dot` 在浅色卡片上 2.24:1 → 靠 `box-shadow` 描边（`--ok-ring` 6.63:1）达标边界，与桌面端同一处理。

**HTML 侧只需要两处改动**（不算业务逻辑）：给表格容器套 `<section class="card">`，数值列 `<td class="num">`。

## §6 施工顺序与风险

### 6.1 批次（改动小 → 收益大）

| 批次 | 改什么 | 影响 | 回退 |
|---|---|---|---|
| **P0-1 字体** | `Cargo.toml` 加 `fontique`；`fonts.rs` 用 §2.1 的 6 个查询 + 5 次 `add_font`（Proportional 3 个、Monospace 1 个、AriaBold 2 个） | 全局观感一次性改变；内存 +mmap（非堆） | 删 Cargo 行 + 回退 `fonts.rs` 一个文件 |
| **P0-2 状态色** | `view.rs::tone_color` 与 `app.rs::status_color` 改为返回 token；`theme.rs` 增加 `ok.dot/ok.ring/ok.text` | 7 处 FAIL 对比度归零 | 两函数各 8 行，可单独回退 |
| **P0-3 进度条** | 进度条显式 `.fill(accent)`；行内百分比移到条右侧 | 1.24:1 → 4.63:1 | 一处调用 |
| **P1-1 卡片化** | 按 §4.3 删 8 处页面内分隔线，包 5 张卡片（状态/新建下载/任务/服务状态/权限与日志） | 层级变清楚，页面内无线 | 逐个区块改，可分批 |
| **P1-2 两页重排** | 服务页（D9/D10）、设置页（D7/D8）按 §5.2/§5.3 | 窄窗不再裁切 | 按函数回退 |
| **P2 Web** | 替换 `assets/style.css`；`index.html` 加两个 class | 浏览器页与面板一致 | 单文件回退 |

### 6.2 风险

| 风险 | 表现 | 对策 |
|---|---|---|
| 字体缺失 | 干净 Win10 无 `Segoe UI Variable`（已排除）、可能有极简精简版 Windows 缺 `msyh.ttc` | 每次查询失败都跳过而不是 panic；链尾保留 egui 内置字体与系统 fallback；日志记一条「字体回退」 |
| pt → 像素 | 125%/150% DPI 下 14pt = 17.5/21px，行高按比例放大，可能出现行高不是整数导致 1px 抖动 | 行高用 `RichText::line_height(Some(20.0))`（逻辑 pt，随 DPI 缩放）；不要写 px |
| 正绿在浅色底 | `#00C853` 单独用时 2.24:1 | 强制配 `ok.ring`；文字永远用 `ok.text`（`#15803D`），**不允许**把 `#00C853` 直接当文字色 |
| 卡片化后信息密度下降 | 每个卡片 16 内边距 + 16 间距，1200×760 下任务表可视行数可能少 2–3 行 | 任务卡用 `card_tight`（12）；状态卡与新建卡合并成一张（「下载控制」）可再省一格；验收时对比行数 |
| 粗体家族漏注册 | 标题要求 bold 但 `AriaBold` 家族为空 → 全部退化成 normal | `add_font` 时若粗体查询失败，日志告警并回退到 normal；**不要**用 `strong()` 假装加粗（它只换颜色） |

## §7 验收（可机验）

1. `grep -rn "Color32::from_" crates/ariahost-ui/src` → 只命中 `theme.rs`（视图层 0 命中）。
2. `grep -rn "tone_color\|status_color" crates/ariahost-ui/src` → 函数体里没有字面量颜色。
3. `grep -c "ui.separator()" crates/ariahost-ui/src/app.rs` → **0**（`show_separator_line` 只允许出现在 4 处壳/操作条）。
4. 对比度：用 `theme::contrast_ratio` 写一条测试遍历 §3.1 的每对（正文 ≥4.5、图形 ≥3.0），任一对低于门槛即测试失败——把契约钉进 CI。
5. 字体：`--panel-selftest --frames 600` 的日志里应出现 4 条字体解析记录（Segoe UI / YaHei UI / YaHei / Consolas），且 `private_kb` 与 v0.1.3 基线相比增量 ≤5MB。
6. 截图：1200×760 与 760×480 各 3 屏（下载/服务/设置），检查 ①无文本被右缘裁切 ②页面内 0 根线 ③状态色可读（肉眼 + 取样 hex 与 §3.1 一致）。
7. Web：`ariahost --web-selftest` 8 项仍全 ok；浏览器 devtools 计算样式中 `font-family` 以 `Segoe UI` 开头、`body` 的背景等于 `--bg`（浅/深各验一次）。

## §8 本文的证据与限制

- 字体存在性：注册表 + 字体文件（§2.1 表格里的文件名与字节数均为本机实测）。
- 字号：`theme.rs` 源码行号 + 截图行距（32px）。
- 颜色与对比度：`.scratch/m5/sample_colors.py`（截图取样）+ `.scratch/m5/contrast_v2.py`（WCAG 计算），全表 `contrast-v2.md`。
- 截图：`.scratch/m5/shots/` 三张（v0.1.3 CI 产物真跑，`PrintWindow` 抓取，未调色）。
- **未验证**：①深色主题未截屏（本机 `AppsUseLightTheme=1`，未改用户系统设置）；②`add_font` 的 `Highest` 先后顺序为推断（§2.1 步骤 5），需编译后看一眼字形归属即可确认；③字体 mmap 对 `private_kb` 的实际增量未测（需一次 selftest）。
