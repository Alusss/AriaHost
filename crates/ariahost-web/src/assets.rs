//! 内嵌静态资源（`include_str!`，零构建、零额外依赖）。
//!
//! 为什么不用 `include_dir`：一共三个文件、八个路由，手写一张 `match` 表就够；
//! 多一个宏 crate 只是多一份构建时间和供应链面。

/// 一个内嵌资源。
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// 文件内容（UTF-8）。
    pub body: &'static str,
    /// `Content-Type`。
    pub content_type: &'static str,
}

/// 首页。
pub const INDEX: &str = include_str!("../assets/index.html");
/// 前端脚本（vanilla ESM，无构建）。
pub const APP_JS: &str = include_str!("../assets/app.js");
/// 样式表。
pub const STYLE_CSS: &str = include_str!("../assets/style.css");

/// 路径 → 资源。`/` 与 `/index.html` 等价；其余要精确匹配。
pub fn lookup(path: &str) -> Option<Asset> {
    match path {
        "/" | "/index.html" => Some(Asset {
            body: INDEX,
            content_type: "text/html; charset=utf-8",
        }),
        "/app.js" => Some(Asset {
            body: APP_JS,
            content_type: "text/javascript; charset=utf-8",
        }),
        "/style.css" => Some(Asset {
            body: STYLE_CSS,
            content_type: "text/css; charset=utf-8",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_three_assets() {
        for (path, kind) in [
            ("/", "text/html; charset=utf-8"),
            ("/index.html", "text/html; charset=utf-8"),
            ("/app.js", "text/javascript; charset=utf-8"),
            ("/style.css", "text/css; charset=utf-8"),
        ] {
            let asset = lookup(path).unwrap_or_else(|| panic!("{path} 应命中"));
            assert_eq!(asset.content_type, kind, "{path}");
            assert!(!asset.body.is_empty(), "{path} 不该是空文件");
        }
    }

    #[test]
    fn unknown_paths_miss() {
        for path in [
            "/nope",
            "/app.js/",
            "/index",
            "/../etc/passwd",
            "/style.css.map",
        ] {
            assert!(lookup(path).is_none(), "{path} 不该命中");
        }
    }

    #[test]
    fn index_references_only_same_origin_assets_and_no_inline_code() {
        // CSP 是 default-src 'self'：内联脚本/样式会被浏览器直接拦掉，所以在源码层就挡住。
        assert!(INDEX.contains("src=\"/app.js\""), "必须用同源脚本");
        assert!(INDEX.contains("href=\"/style.css\""), "必须用同源样式");
        assert!(
            !INDEX.contains("<script>"),
            "不许内联脚本（CSP 会拦，且等于给自己留后门）"
        );
        assert!(!INDEX.contains("style="), "不许内联样式（CSP 会拦）");
        assert!(
            !INDEX.to_ascii_lowercase().contains("http://"),
            "页面不许引用任何外部源：{INDEX}"
        );
    }

    #[test]
    fn script_never_touches_innerhtml() {
        // 用 innerHTML 渲染 aria2 返回的 URL/文件名＝XSS 入口。
        assert!(
            !APP_JS.contains("innerHTML"),
            "前端不许用 innerHTML（改用 textContent）"
        );
        assert!(
            !APP_JS.to_ascii_lowercase().contains("document.write"),
            "前端不许用 document.write"
        );
    }

    #[test]
    fn script_only_calls_whitelisted_methods() {
        // 前端只许调白名单里的方法；写死一份列表防「页面偷偷调 changeGlobalOption」。
        for method in [
            "aria2.tellActive",
            "aria2.tellWaiting",
            "aria2.tellStopped",
            "aria2.getGlobalStat",
            "aria2.addUri",
            "aria2.pause",
            "aria2.unpause",
            "aria2.remove",
            "aria2.removeDownloadResult",
        ] {
            assert!(APP_JS.contains(method), "页面该用到 {method}");
            assert!(
                crate::rpc::is_allowed(method),
                "{method} 必须同时在服务端白名单里"
            );
        }
        for forbidden in ["shutdown", "changeGlobalOption", "saveSession", "multicall"] {
            assert!(
                !APP_JS.contains(forbidden),
                "页面不该出现 {forbidden}（服务端也会拒）"
            );
        }
    }

    // ---------------------------------------------------------------
    // UI-4（task-89）：令牌层 / 字体与几何 / 对比度（规格 §4.1、§5.4）
    // ---------------------------------------------------------------

    /// 取「`marker` 之后第一个 `{` 到与之配对的 `}`」之间的文本，用于分别读两套主题的令牌。
    fn block_after(css: &str, marker: &str) -> String {
        let start = css
            .find(marker)
            .unwrap_or_else(|| panic!("CSS 里找不到标记 {marker}"));
        let open = start + css[start..].find('{').expect("标记之后应有 '{'");
        let body = &css[open + 1..];
        let close = body.find('}').expect("块未闭合");
        body[..close].to_owned()
    }

    /// 解析 `--name: #rrggbb;` 形态的令牌。非颜色令牌（px、字体链）不在这里出现。
    fn color_tokens(block: &str) -> std::collections::BTreeMap<String, String> {
        let mut map = std::collections::BTreeMap::new();
        for line in block.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("--") else {
                continue;
            };
            let Some((name, value)) = rest.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_end_matches(';').trim();
            if value.starts_with('#') {
                map.insert(name.trim().to_owned(), value.to_owned());
            }
        }
        map
    }

    /// sRGB 单通道 → 线性光（WCAG 2.x 相对亮度）。
    fn linear(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(hex: &str) -> f64 {
        let digits = hex.trim_start_matches('#');
        assert_eq!(digits.len(), 6, "只接受 #rrggbb：{hex}");
        let part = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&digits[range], 16).unwrap_or_else(|_| panic!("非法色值：{hex}"))
        };
        0.2126 * linear(part(0..2)) + 0.7152 * linear(part(2..4)) + 0.0722 * linear(part(4..6))
    }

    /// WCAG 对比度（1:1 – 21:1）。
    fn contrast(fg: &str, bg: &str) -> f64 {
        let (a, b) = (luminance(fg), luminance(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn style_css_keeps_colors_inside_the_token_layer() {
        // 规格 §5.4 的硬约束：颜色只在令牌层定义一次，规则里一律 var(--x)。
        // 注释行与令牌定义行之外出现 `#` 就是有人绕过令牌层写死了颜色。
        let mut definitions = 0_usize;
        for (index, line) in STYLE_CSS.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }
            if trimmed.starts_with("--") {
                definitions += 1;
                continue;
            }
            assert!(
                !line.contains('#'),
                "第 {} 行有裸色值（改成 var(--x)）：{line}",
                index + 1
            );
            assert!(
                !line.contains(':') || !line.contains("rgb("),
                "第 {} 行用 rgb() 写死了颜色：{line}",
                index + 1
            );
        }
        assert!(
            definitions >= 24,
            "令牌定义只有 {definitions} 条，疑似令牌层被裁掉"
        );
    }

    #[test]
    fn style_css_typography_and_geometry_match_the_spec() {
        // §2.1：中文系统必须有 YaHei UI 变体，且以通用族收尾。
        assert!(
            STYLE_CSS.contains(r#""Segoe UI", "Microsoft YaHei UI""#),
            "字体链必须是 Segoe UI → Microsoft YaHei UI 的顺序"
        );
        assert!(
            STYLE_CSS.contains("system-ui") && STYLE_CSS.contains("sans-serif"),
            "字体链必须以 system-ui / sans-serif 兜底"
        );
        // §4.1：卡片圆角 8、控件圆角 6（与面板 theme.rs 同值）。
        assert!(
            STYLE_CSS.contains("--radius-card: 8px"),
            "卡片圆角必须是 8px"
        );
        assert!(
            STYLE_CSS.contains("--radius-ctl: 6px"),
            "控件圆角必须是 6px"
        );
        // 数字列右对齐 + 等宽数字（否则轮询时数字会左右跳）。
        assert!(STYLE_CSS.contains("text-align: right"), "数字列必须右对齐");
        assert!(
            STYLE_CSS.contains("tabular-nums"),
            "数字要用等宽数字，避免刷新时抖动"
        );
    }

    #[test]
    fn style_css_contrast_meets_wcag_in_both_themes() {
        // 对比度是量出来的，不是看出来的：深浅两套令牌逐对算 WCAG 比值。
        // 正文 7:1（AAA）、次要文字/状态字 4.5:1（AA）、
        // 非文字（焦点环、控件描边、进度条）3:1（1.4.11）。
        let cases = [
            ("light", block_after(STYLE_CSS, ":root {")),
            ("dark", block_after(STYLE_CSS, "prefers-color-scheme: dark")),
        ];
        for (theme, block) in cases {
            let tokens = color_tokens(&block);
            assert!(
                tokens.len() >= 12,
                "{theme} 主题只解析出 {} 个颜色令牌",
                tokens.len()
            );
            let pick = |name: &str| -> String {
                tokens
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| panic!("{theme} 缺令牌 --{name}"))
            };
            for (fg, bg, min) in [
                ("text", "card", 7.0),
                ("text", "bg", 7.0),
                ("muted", "card", 4.5),
                ("muted", "bg", 4.5),
                ("link", "card", 4.5),
                ("ok", "card", 4.5),
                ("bad", "card", 4.5),
            ] {
                let ratio = contrast(&pick(fg), &pick(bg));
                assert!(
                    ratio >= min,
                    "{theme}：--{fg} 叠在 --{bg} 上只有 {ratio:.2}:1，要求 ≥{min}:1"
                );
            }
            // 控件边界用 --border-strong（theme.rs 的 border_strong）：它只在 --card 上
            // 才过 3:1（浅 3.15 / 深 3.49）；压在 --bg 上会掉下去，所以控件一律放进卡片。
            for (fg, bg, min) in [("accent", "card", 3.0), ("border-strong", "card", 3.0)] {
                let ratio = contrast(&pick(fg), &pick(bg));
                assert!(
                    ratio >= min,
                    "{theme}：非文字色 --{fg} 对 --{bg} 只有 {ratio:.2}:1，要求 ≥{min}:1"
                );
            }
            let on_accent = contrast(&pick("on-accent"), &pick("accent"));
            assert!(
                on_accent >= 4.5,
                "{theme}：主按钮文字对 --accent 只有 {on_accent:.2}:1，要求 ≥4.5:1"
            );
        }
    }

    /// 色板只有一个来源：面板的 `theme.rs`。
    ///
    /// 网页页的令牌值必须**逐字**等于那里的 LIGHT / DARK 两套——
    /// task-89 首版就漂了三条（`--line` 用了 #d9dde3 而非 border 的 #d6dbe1，
    /// `--line-control` 在浅色用了 --text 的 #1b1f23 而非 border_strong，
    /// 于是控件边框看起来比面板重一倍）。两套界面分开维护，下次改色只会改一边，
    /// 用户在面板和网页页之间切换就会看到两种品牌色，所以这里把值钉死。
    #[test]
    fn style_css_palette_matches_the_panel_theme() {
        let light = color_tokens(&block_after(STYLE_CSS, ":root {"));
        let dark = color_tokens(&block_after(STYLE_CSS, "prefers-color-scheme: dark"));

        // LIGHT：theme.rs 的 LIGHT 全 15 条。
        let expected_light = [
            ("bg", "#f6f7f9"),
            ("card", "#ffffff"),
            ("hover", "#eef1f5"),
            ("border", "#d6dbe1"),
            ("border-strong", "#8a929c"),
            ("text", "#1b1f23"),
            ("muted", "#59626b"),
            ("link", "#0a58ca"),
            ("accent", "#1f6feb"),
            ("on-accent", "#ffffff"),
            ("ok", "#15803d"),
            ("bad", "#c42b1c"),
            ("warn", "#8a5a00"),
            ("ok-dot", "#00c853"),
            ("ok-ring", "#0b6b34"),
        ];
        for (name, want) in expected_light {
            assert_eq!(
                light.get(name).map(String::as_str),
                Some(want),
                "--{name} 与 theme.rs LIGHT 不一致（改色请改 theme.rs 再同步到这里）"
            );
        }
        assert_eq!(
            light.len(),
            expected_light.len(),
            "LIGHT 令牌条数与 theme.rs 对不上，多出或少了：{light:?}"
        );

        // DARK：只覆盖会变的那 13 条。--ok-dot / --ok-ring 在 theme.rs 里深浅同值，
        // 因此只在 :root 定义一次（不重复定义 = 不会漂）。
        let expected_dark = [
            ("bg", "#15181b"),
            ("card", "#1c2024"),
            ("hover", "#242a2f"),
            ("border", "#353c43"),
            ("border-strong", "#6b757e"),
            ("text", "#e6e9ec"),
            ("muted", "#a7b0b8"),
            ("link", "#86b4ff"),
            ("accent", "#4c8df6"),
            ("on-accent", "#0b121c"),
            ("ok", "#00c853"),
            ("bad", "#f0616d"),
            ("warn", "#e0a93b"),
        ];
        for (name, want) in expected_dark {
            assert_eq!(
                dark.get(name).map(String::as_str),
                Some(want),
                "--{name} 与 theme.rs DARK 不一致（改色请改 theme.rs 再同步到这里）"
            );
        }
        assert_eq!(
            dark.len(),
            expected_dark.len(),
            "DARK 令牌条数与 theme.rs 对不上，多出或少了：{dark:?}"
        );
    }

    /// 字号只有五档（`theme.rs::text_styles()`：20 / 16 / 14 / 13 / 12），
    /// 而且每一档都必须**真的被规则用到**——task-89 首版把 16px 定义出来却没人使用，
    /// 于是网页页没有「次级标题」这一层，视觉层级比面板少一级。
    #[test]
    fn style_css_font_sizes_follow_the_five_step_ladder() {
        for (token, px) in [
            ("heading", "20px"),
            ("subtitle", "16px"),
            ("body", "14px"),
            ("mono", "13px"),
            ("small", "12px"),
        ] {
            let line = format!("--fs-{token}: {px};");
            assert!(STYLE_CSS.contains(&line), "缺字号令牌 `{line}`");
            let uses = STYLE_CSS.matches(&format!("var(--fs-{token})")).count();
            assert!(uses > 0, "字号令牌 --fs-{token} 定义了却没有任何规则使用");
        }

        // `font:` 简写会把字号顶掉（`font: 14px/20px ...`），等于绕过令牌层。
        assert!(
            !STYLE_CSS.contains("font:"),
            "别用 `font:` 简写——它绕开字号令牌；请分开写 font-family / font-size / line-height"
        );

        // 规则区不许出现裸字号。
        for (index, line) in STYLE_CSS.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.is_empty()
                || trimmed.starts_with("/*")
                || trimmed.starts_with('*')
                || trimmed.starts_with("--")
            {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("font-size:") {
                assert!(
                    rest.trim_start().starts_with("var(--fs-"),
                    "第 {} 行字号没走令牌：{line}",
                    index + 1
                );
            }
        }
    }

    /// 窄屏（375px）下五列表放不下：**滚动必须发生在卡片内部**，页面本身不许横向溢出。
    ///
    /// 这条只钉结构（外包层 + `overflow-x: auto` + 表格最小宽度）；真实像素判据
    /// （`documentElement.scrollWidth <= clientWidth`）由浏览器实测覆盖，两者互补。
    #[test]
    fn table_scrolls_inside_the_card_on_narrow_screens() {
        // ① 表格外面必须有一层容器，且容器在 `<table>` 之前开、之后合。
        let open = INDEX
            .find(r#"<div class="table-wrap">"#)
            .expect("缺少 .table-wrap 容器");
        let table = INDEX.find("<table>").expect("缺少 <table>");
        let table_end = INDEX.find("</table>").expect("缺少 </table>");
        let close = INDEX.find("</div>").expect("缺少 </div>");
        assert!(open < table, "容器必须在表格之前开始");
        assert!(close > table_end, "容器必须在表格之后结束");

        // ② 容器自己带横向滚动。
        let block = block_after(STYLE_CSS, ".table-wrap");
        assert!(
            block.contains("overflow-x: auto"),
            ".table-wrap 必须 overflow-x: auto，实际块：{block}"
        );

        // ③ 表格有最小宽度，否则列被压扁而不是滚动（压扁会把「剩余」挤成两行）。
        let table_block = block_after(STYLE_CSS, "table {");
        assert!(
            table_block.contains("min-width: 620px"),
            "table 必须给 min-width，实际块：{table_block}"
        );

        // ④ 不「藏」横向溢出——那只是把症状盖住，实测判据会失真。
        assert!(
            !STYLE_CSS.contains("overflow-x: hidden"),
            "不许用 overflow-x: hidden 掩盖溢出"
        );
        let body_block = block_after(STYLE_CSS, "body {");
        assert!(
            !body_block.contains("overflow-x"),
            "body 不许设 overflow-x，横向滚动只该出现在 .table-wrap：{body_block}"
        );
    }
}
