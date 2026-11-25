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
}
