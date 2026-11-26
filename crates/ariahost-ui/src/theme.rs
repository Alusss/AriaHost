//! 设计 token 与 `Style` 安装（`docs/design/ui-spec.md` §2–§8 的代码化）。
//!
//! ## 为什么要有这个文件
//!
//! 视觉审计（`docs/design/ui-audit.md`）量出来的问题都是**同一个病**：颜色、字号、
//! 间距散在每个绘制点里，各自拍脑袋取值。结果「顶栏黑底灰字 2.48:1」「状态绿 1.82:1」
//! 「`.weak()` 正文 3.72:1」——浅色主题下更是黑底深字。
//!
//! 所以这里做三件事，且**只在这里**做：
//! 1. 两套主题的 token（`DARK` / `LIGHT`），一个值只有一个定义处；
//! 2. 把 token 装进 egui 的 `Style`（字号阶梯、间距、`Visuals`）；
//! 3. 给绘制点提供访问器（[`tokens`] / [`text2`] / [`card`] / [`chip`] …）。
//!
//! 规则（§8.5，会直接判缺陷）：绘制点不许再出现 `Color32::from_rgb/gray` 字面量、
//! 不许写字号字面量、不许用 `.weak()` 表达「次要文字」（用 [`text2`]）。
//!
//! ## 一个必须记住的坑
//!
//! `override_text_color` 只影响**没有显式颜色**的文字；`RichText::color(..)` 仍优先。
//! 这正好是我们要的：语义色（状态/危险）继续由 token 决定，正文统一由 Style 决定。

use egui::{Color32, CornerRadius, FontFamily, FontId, Frame, Stroke, TextStyle};

// ---------------------------------------------------------------------------
// token
// ---------------------------------------------------------------------------

/// 一套主题的全部颜色 token（§4.1 表，值与表逐字一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    /// 本 token 集是否为深色（`ui.visuals().dark_mode` 的镜像，绘制点别自己猜）。
    pub dark: bool,
    /// 页面底；窗口清屏色也用这个（不铺底就会露出交换链的 `#080808`）。
    pub bg_base: Color32,
    /// 卡片、表格容器、壳。
    pub surface: Color32,
    /// 表格偶数行。
    pub stripe: Color32,
    /// hover 底、次按钮底。
    pub surface_hover: Color32,
    /// 按下态；也是输入框聚焦时的底。
    pub surface_active: Color32,
    /// 输入框、进度条槽。
    pub input_bg: Color32,
    /// 装饰性分隔线（1px）。**不要**拿它画控件边界（对比不足）。
    pub border: Color32,
    /// 控件边界（输入框、勾选框），≥3:1。
    pub border_strong: Color32,
    /// 主文字。
    pub text: Color32,
    /// 次要文字、hint、页脚。
    pub text2: Color32,
    /// 禁用文字（仍要可读，≥3:1）。
    pub text3: Color32,
    /// 主按钮底、焦点环、进度填充。
    pub accent: Color32,
    /// 主按钮 hover（深色变亮、浅色变暗，两个方向都过 4.5）。
    pub accent_hover: Color32,
    /// 链接、可点文字。
    pub accent_text: Color32,
    /// 主按钮上的字。
    pub on_accent: Color32,
    /// 成功底/边框/图标。
    pub success: Color32,
    /// 成功文字。
    pub success_text: Color32,
    /// 警告底/边框/图标。
    pub warn: Color32,
    /// 警告文字。
    pub warn_text: Color32,
    /// 危险底/边框/图标。
    pub danger: Color32,
    /// 危险文字。
    pub danger_text: Color32,
    /// 进行中/信息。
    pub info: Color32,
    /// 信息文字。
    pub info_text: Color32,
    /// chip 底的 alpha（深色 16% / 浅色 10%）。
    pub chip_alpha: u8,
    /// ok.dot：运行中指示点本体（**只做填充**，必须配 ok_ring 描边才够 3:1）。
    pub ok_dot: Color32,
    /// ok.ring：指示点 1px 描边，承担图形 3:1 边界。
    pub ok_ring: Color32,
    /// ok.text：「运行中 / 下载中 / 已完成」文字（浅色加深、深色用正绿）。
    pub ok_text: Color32,
}

/// 深色（默认）。
pub const DARK: Tokens = Tokens {
    dark: true,
    bg_base: Color32::from_rgb(0x15, 0x18, 0x1B),
    surface: Color32::from_rgb(0x1C, 0x20, 0x24),
    stripe: Color32::from_rgb(0x20, 0x25, 0x29),
    surface_hover: Color32::from_rgb(0x24, 0x2A, 0x2F),
    surface_active: Color32::from_rgb(0x2B, 0x32, 0x38),
    input_bg: Color32::from_rgb(0x11, 0x14, 0x17),
    border: Color32::from_rgb(0x35, 0x3C, 0x43),
    border_strong: Color32::from_rgb(0x6B, 0x75, 0x7E),
    text: Color32::from_rgb(0xE6, 0xE9, 0xEC),
    text2: Color32::from_rgb(0xA7, 0xB0, 0xB8),
    text3: Color32::from_rgb(0x83, 0x8C, 0x94),
    accent: Color32::from_rgb(0x4C, 0x8D, 0xF6),
    accent_hover: Color32::from_rgb(0x5C, 0x9C, 0xFF),
    accent_text: Color32::from_rgb(0x86, 0xB4, 0xFF),
    on_accent: Color32::from_rgb(0x0B, 0x12, 0x1C),
    success: Color32::from_rgb(0x00, 0xC8, 0x53),
    success_text: Color32::from_rgb(0x00, 0xC8, 0x53),
    warn: Color32::from_rgb(0xE0, 0xA9, 0x3B),
    warn_text: Color32::from_rgb(0xE0, 0xA9, 0x3B),
    danger: Color32::from_rgb(0xF0, 0x61, 0x6D),
    danger_text: Color32::from_rgb(0xF0, 0x61, 0x6D),
    info: Color32::from_rgb(0x4F, 0xA8, 0xF5),
    info_text: Color32::from_rgb(0x4F, 0xA8, 0xF5),
    chip_alpha: 41, // 16% of 255
    ok_dot: Color32::from_rgb(0x00, 0xC8, 0x53),
    ok_ring: Color32::from_rgb(0x0B, 0x6B, 0x34),
    ok_text: Color32::from_rgb(0x00, 0xC8, 0x53),
};

/// 浅色。
pub const LIGHT: Tokens = Tokens {
    dark: false,
    bg_base: Color32::from_rgb(0xF6, 0xF7, 0xF9),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    stripe: Color32::from_rgb(0xF5, 0xF7, 0xF9),
    surface_hover: Color32::from_rgb(0xEE, 0xF1, 0xF5),
    surface_active: Color32::from_rgb(0xE3, 0xE8, 0xEE),
    input_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    border: Color32::from_rgb(0xD6, 0xDB, 0xE1),
    border_strong: Color32::from_rgb(0x8A, 0x92, 0x9C),
    text: Color32::from_rgb(0x1B, 0x1F, 0x23),
    text2: Color32::from_rgb(0x59, 0x62, 0x6B),
    text3: Color32::from_rgb(0x7E, 0x87, 0x8F),
    accent: Color32::from_rgb(0x1F, 0x6F, 0xEB),
    accent_hover: Color32::from_rgb(0x18, 0x5F, 0xD1),
    accent_text: Color32::from_rgb(0x0A, 0x58, 0xCA),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    success: Color32::from_rgb(0x15, 0x80, 0x3D),
    success_text: Color32::from_rgb(0x15, 0x80, 0x3D),
    warn: Color32::from_rgb(0x8A, 0x5A, 0x00),
    warn_text: Color32::from_rgb(0x8A, 0x5A, 0x00),
    danger: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    danger_text: Color32::from_rgb(0xC4, 0x2B, 0x1C),
    info: Color32::from_rgb(0x09, 0x69, 0xDA),
    info_text: Color32::from_rgb(0x09, 0x69, 0xDA),
    chip_alpha: 26, // 10% of 255
    ok_dot: Color32::from_rgb(0x00, 0xC8, 0x53),
    ok_ring: Color32::from_rgb(0x0B, 0x6B, 0x34),
    ok_text: Color32::from_rgb(0x15, 0x80, 0x3D),
};

/// 当前主题的 token（按 `ui` 的 `dark_mode` 选，不缓存、不猜）。
pub fn tokens(ui: &egui::Ui) -> &'static Tokens {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// 当前主题的 token（按 `ctx` 选；对话框/面板这类没有 `Ui` 的地方用）。
pub fn tokens_of(ctx: &egui::Context) -> &'static Tokens {
    // egui 0.37 没有 `ctx.style()`：当前主题用 `ctx.theme()` 查（`Theme::{Dark,Light}`）。
    if ctx.theme() == egui::Theme::Dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// 次要素材文字色（替代 `.weak()` 表达「次要」）。
pub fn text2(ui: &egui::Ui) -> Color32 {
    tokens(ui).text2
}

/// 禁用文字色。
pub fn text3(ui: &egui::Ui) -> Color32 {
    tokens(ui).text3
}

/// 主文字色（需要显式写的时候用；正文一般交给 Style 的 `override_text_color`）。
pub fn text(ui: &egui::Ui) -> Color32 {
    tokens(ui).text
}

// ---------------------------------------------------------------------------
// Frame 工厂
// ---------------------------------------------------------------------------

/// 顶栏：`surface` 底 + 左右 16 / 上下 8 内边距（§6.1，条高做到 48）。
///
/// 那条 1px 分隔线不在这里画：`Frame::stroke` 只能画**整圈**，画出来像卡片。
/// 交给 `Panel::show_separator_line(true)`——它按面板所在边画单边线，正是壳要的效果。
pub fn top_bar_frame(ui: &egui::Ui) -> Frame {
    let t = tokens(ui);
    Frame::NONE
        .fill(t.surface)
        .inner_margin(egui::Margin::symmetric(16, 8))
        .corner_radius(CornerRadius::ZERO)
}

/// 页脚：与顶栏同底，内边距更紧（上下 4），少占纵向空间。
pub fn bottom_bar_frame(ui: &egui::Ui) -> Frame {
    let t = tokens(ui);
    Frame::NONE
        .fill(t.surface)
        .inner_margin(egui::Margin::symmetric(16, 4))
        .corner_radius(CornerRadius::ZERO)
}

/// 页面区：`bg_base` 铺底 + 左右 24 / 上下 12（§3.2 外边距）。
///
/// **必须铺底**：不铺底时 egui 露出交换链清屏色（近黑），浅色主题下就是
/// 「黑底深字 2.48:1」——审计 P0 的原始成因，不是文字色的问题。
pub fn page_frame(ui: &egui::Ui) -> Frame {
    let t = tokens(ui);
    Frame::NONE
        .fill(t.bg_base)
        .inner_margin(egui::Margin::symmetric(24, 12))
        .corner_radius(CornerRadius::ZERO)
}

/// 卡片：`surface` 底 + 1px `border` + 圆角 8 + 内边距 16（§5.7）。
pub fn card(ui: &egui::Ui) -> Frame {
    let t = tokens(ui);
    Frame::NONE
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(16))
}

/// 紧凑卡（内边距 12）。
pub fn card_tight(ui: &egui::Ui) -> Frame {
    card(ui).inner_margin(egui::Margin::same(12))
}

/// 语义色按 chip 透明度叠在 `surface` 上得到徽章底（§5.6）。
///
/// 用 `from_rgba_unmultiplied` 而不是 `gamma_multiply`：后者会把底色算成怪色，
/// 文字压在上面时对比度不可预测（§4.3 规则 3）。
pub fn chip_bg(t: &Tokens, tone: Color32) -> Color32 {
    let a = t.chip_alpha;
    Color32::from_rgba_unmultiplied(tone.r(), tone.g(), tone.b(), a)
}

// ---------------------------------------------------------------------------
// 安装
// ---------------------------------------------------------------------------

/// 字号阶梯（§2.1）：**只有这五档** + 等宽两档。
///
/// 返回 `[(TextStyle, FontId)]`，`apply` 直接灌进 `style.text_styles`。
pub fn text_styles() -> Vec<(TextStyle, FontId)> {
    let prop = |pt: f32| FontId::new(pt, FontFamily::Proportional);
    let mono = |pt: f32| FontId::new(pt, FontFamily::Monospace);
    vec![
        (TextStyle::Heading, prop(20.0)),
        (TextStyle::Name("AriaSubtitle".into()), prop(16.0)),
        (TextStyle::Name("AriaNumber".into()), mono(16.0)),
        (TextStyle::Body, prop(14.0)),
        (TextStyle::Button, prop(14.0)),
        (TextStyle::Small, prop(12.0)),
        (TextStyle::Monospace, mono(13.0)),
    ]
}

/// 装两套主题（跟随系统）。**必须在 `PanelApp::build` 最前面调用一次**：
/// 晚于首帧就会看到一帧黑底，早于字体注册则会和 `fonts::install` 抢 `text_styles`。
pub fn install(ctx: &egui::Context) {
    // 跟随系统；用户没得选（本批不提供主题开关，P3 再说）。
    ctx.set_theme(egui::ThemePreference::System);
    ctx.style_mut_of(egui::Theme::Dark, |style| apply(style, &DARK));
    ctx.style_mut_of(egui::Theme::Light, |style| apply(style, &LIGHT));
}

/// 把一套 token 灌进 `Style`（§8.1 骨架，逐项对齐）。
pub fn apply(style: &mut egui::Style, c: &Tokens) {
    style.text_styles = text_styles().into_iter().collect();

    // —— 间距（§3.2）——
    let sp = &mut style.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.window_margin = egui::Margin::same(16);
    sp.menu_margin = egui::Margin::same(8);
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
    w.inactive.weak_bg_fill = c.surface_hover; // 次按钮底
    w.inactive.bg_fill = c.surface_hover; // 勾选框底
    w.inactive.bg_stroke = Stroke::new(1.0, c.border_strong);
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
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(
        c.accent.r(),
        c.accent.g(),
        c.accent.b(),
        if c.dark { 72 } else { 40 },
    );
    v.selection.stroke = Stroke::new(1.0, c.accent);
    v.disabled_alpha = 1.0; // 禁用态用 text3，不再整体压 alpha
    v.striped = false; // 表格自己控制斑马纹（§5.4），避免全局生效
}

// ---------------------------------------------------------------------------
// 对比度（把「可读」变成可判定的数，而不是审美争论）
// ---------------------------------------------------------------------------

/// WCAG 2.1 的相对亮度（sRGB 线性化）。只服务 [`contrast_ratio`]，不对外。
fn relative_luminance(color: Color32) -> f32 {
    fn channel(value: u8) -> f32 {
        let c = f32::from(value) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// 两色的 WCAG 对比度（1.0–21.0，越大越清楚）。
///
/// 为什么放进产品代码而不是只写在测试里：验收要能复述「主文字对页面底 X:1」，
/// 而口径必须是同一份实现，否则下次换色又是各自估。
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// token 值必须与 §4.1 表逐字一致：这是「一处定义」的锚点，
    /// 谁想改颜色就得改这张表，不能偷偷在绘制点写别的值。
    #[test]
    fn tokens_are_pinned_to_the_spec_table() {
        assert_eq!(DARK.bg_base, Color32::from_rgb(0x15, 0x18, 0x1B));
        assert_eq!(DARK.surface, Color32::from_rgb(0x1C, 0x20, 0x24));
        assert_eq!(DARK.text, Color32::from_rgb(0xE6, 0xE9, 0xEC));
        assert_eq!(DARK.text2, Color32::from_rgb(0xA7, 0xB0, 0xB8));
        assert_eq!(DARK.border_strong, Color32::from_rgb(0x6B, 0x75, 0x7E));
        assert_eq!(DARK.accent, Color32::from_rgb(0x4C, 0x8D, 0xF6));
        assert_eq!(LIGHT.bg_base, Color32::from_rgb(0xF6, 0xF7, 0xF9));
        assert_eq!(LIGHT.surface, Color32::from_rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(LIGHT.text, Color32::from_rgb(0x1B, 0x1F, 0x23));
        assert_eq!(LIGHT.text2, Color32::from_rgb(0x59, 0x62, 0x6B));
        // 两套主题的 `dark` 标志是常量事实，放进 const 块才是它的正确形态
        // （否则 clippy 的 `assertions_on_constants` 会在 `-D warnings` 下拦下来）。
        const { assert!(DARK.dark && !LIGHT.dark) };
        // 浅色不能让页面底和卡片底同色，否则卡片边界全靠描边，视觉上会糊成一片。
        assert_ne!(LIGHT.bg_base, LIGHT.surface);
    }

    /// 「黑底灰字 2.48:1」就是这批要消掉的缺陷本身：两套主题的主/次文字对页面底、
    /// 以及主文字对卡片底，都必须过 WCAG AA 的正文口径（≥ 4.5:1）。
    #[test]
    fn text_on_surfaces_meets_wcag_aa_in_both_themes() {
        for (name, c) in [("dark", DARK), ("light", LIGHT)] {
            let primary = contrast_ratio(c.text, c.bg_base);
            assert!(
                primary >= 4.5,
                "{name} 主题主文字对页面底只有 {primary:.2}:1（要求 ≥ 4.5）"
            );
            let on_card = contrast_ratio(c.text, c.surface);
            assert!(
                on_card >= 4.5,
                "{name} 主题主文字对卡片底只有 {on_card:.2}:1（要求 ≥ 4.5）"
            );
            let secondary = contrast_ratio(c.text2, c.bg_base);
            assert!(
                secondary >= 4.5,
                "{name} 主题次文字对页面底只有 {secondary:.2}:1（要求 ≥ 4.5）"
            );
        }
        // 自检：闸门必须能真的拦下不合格配色——拿一组「近黑底 + 中灰字」当对照样本，
        // 它若也能过 4.5:1，说明上面的断言其实没在算东西。
        let weak = contrast_ratio(
            Color32::from_rgb(0x6B, 0x75, 0x7E),
            Color32::from_rgb(0x12, 0x16, 0x1B),
        );
        assert!(weak < 4.5, "对照样本本应不达标，实际 {weak:.2}:1");
    }

    /// 字号阶梯只允许五档（§2.1 / §9.2 第 1 条的语义版）。
    #[test]
    fn font_ladder_has_exactly_five_distinct_sizes() {
        let mut sizes: Vec<u32> = text_styles()
            .iter()
            .map(|(_, font)| font.size.to_bits())
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        assert_eq!(sizes.len(), 5, "字号只允许五档，实际：{sizes:?}");
    }

    /// 装上以后：页面底、主文字、弱文字都必须来自 token，而不是 egui 默认色。
    #[test]
    fn apply_installs_tokens_into_visuals() {
        let mut style = egui::Style::default();
        apply(&mut style, &LIGHT);
        assert!(!style.visuals.dark_mode);
        assert_eq!(style.visuals.panel_fill, LIGHT.bg_base);
        assert_eq!(style.visuals.override_text_color, Some(LIGHT.text));
        assert_eq!(style.visuals.weak_text_color, Some(LIGHT.text2));
        // 弱文字不能再被 alpha 压暗（那正是 3.72:1 的来源）
        assert_eq!(style.visuals.weak_text_alpha, 1.0);
        assert_eq!(style.visuals.extreme_bg_color, LIGHT.input_bg);
        // 禁用态不许整体压 alpha（禁用文字仍要 ≥3:1）
        assert_eq!(style.visuals.disabled_alpha, 1.0);
        assert!(!style.visuals.striped, "斑马纹由表格自己控制");
    }

    /// 间隔类字段的取值与 §3.2 一致：这类值最容易在各处被随手改成别的数。
    #[test]
    fn spacing_matches_the_spec() {
        let mut style = egui::Style::default();
        apply(&mut style, &DARK);
        assert_eq!(style.spacing.item_spacing, egui::vec2(8.0, 6.0));
        assert_eq!(style.spacing.button_padding, egui::vec2(12.0, 6.0));
        assert_eq!(style.spacing.interact_size, egui::vec2(48.0, 32.0));
        assert_eq!(style.spacing.scroll.bar_width, 10.0);
        assert!(!style.spacing.scroll.floating);
    }

    /// `tokens()` 跟着 `dark_mode` 走：浅色主题下必须真的换一套，而不是仍取深色。
    #[test]
    fn tokens_follow_the_active_theme() {
        let ctx = egui::Context::default();
        let dark = egui::Ui::new(
            ctx.clone(),
            egui::Id::unique("theme-test"),
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(100.0, 100.0),
            )),
        );
        // 默认视觉就是深色
        assert_eq!(tokens(&dark).bg_base, DARK.bg_base);
    }

    /// chip 底：语义色带 alpha 叠上去，色相不变、只有不透明度变。
    ///
    /// 口径：`Color32` **存储就是预乘的**（`from_rgba_unmultiplied` 会立刻把 rgb 按
    /// `c * a / 255` 缩掉），所以不能直接比 `bg.r() == success.r()`；要比就比
    /// `to_srgba_unmultiplied()` 回读回来的未预乘值（±2 是预乘/反预乘的量化误差）。
    #[test]
    fn chip_bg_keeps_the_hue_and_only_changes_alpha() {
        let bg = chip_bg(&DARK, DARK.success);
        let back = bg.to_srgba_unmultiplied();
        let want = DARK.success.to_srgba_unmultiplied();
        for (name, idx) in [("r", 0usize), ("g", 1), ("b", 2)] {
            let delta = (i32::from(back[idx]) - i32::from(want[idx])).abs();
            assert!(
                delta <= 2,
                "{name} 回读偏了 {delta}：got {} want {}",
                back[idx],
                want[idx]
            );
        }
        // 不透明度只有 token 一个来源。
        assert_eq!(bg.a(), DARK.chip_alpha, "alpha 必须来自 chip_alpha");
        // 预乘存储下的 rgb 必须正好是 `原色 * a / 255`（±1 舍入）：`gamma_multiply`
        // 只在 gamma 空间缩放，得到的是另一组值，所以这一条能区分两种实现。
        // 注意不能写成「每个通道都严格变小」：token 里 r 分量可能本来就是 0，0 再乘也还是 0。
        let a = u32::from(DARK.chip_alpha);
        for (name, idx, got) in [("r", 0usize, bg.r()), ("g", 1, bg.g()), ("b", 2, bg.b())] {
            let exact = (u32::from(want[idx]) * a / 255) as i32;
            let delta = (i32::from(got) - exact).abs();
            assert!(
                delta <= 1,
                "{name} 不是按 alpha 预乘的：got {got} 期望 {exact}（原色 {}）",
                want[idx]
            );
        }
    }

    /// 卡片/壳的 frame 必须真的带 fill（否则又是「未铺底」）。
    #[test]
    fn frame_helpers_carry_a_fill() {
        let ctx = egui::Context::default();
        let ui = egui::Ui::new(
            ctx,
            egui::Id::unique("theme-test"),
            egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(100.0, 100.0),
            )),
        );
        assert_eq!(card(&ui).fill, DARK.surface);
        assert_eq!(top_bar_frame(&ui).fill, DARK.surface);
        assert_eq!(page_frame(&ui).fill, DARK.bg_base);
    }
}
