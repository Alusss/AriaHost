//! 秘密值脱敏（日志与落盘共用的**唯一实现**）。
//!
//! 为什么放在 `common`：同一份规则原来有两处需求（服务平面日志、任务历史里的 `uri`），
//! 一旦各写一份，将来必然漂移——一处补了 `&token=`，另一处没补，就是泄漏。
//! 现在服务日志（`ariahost-svc`）与任务历史（`ariahost-core::history`）指向同一份规则。
//!
//! 覆盖面（对应架构 §4.6.1 的威胁模型与 §6 日志策略）：
//!
//! | 形态 | 例子 | 脱敏后 |
//! |---|---|---|
//! | aria2 RPC token | `token:s3cr3t` | `token:***` |
//! | 查询参数 token | `?token=abc&id=7` | `?token=***&id=7` |
//! | 命令行 rpc-secret | `--rpc-secret=topsecret` | `--rpc-secret=***` |
//! | 磁力 infohash | `urn:btih:0123abcd…` | `urn:btih:***` |
//! | URL 里的密码 | `https://user:pw@h/p` | `https://user:***@h/p` |
//!
//! 不做的事：不删用户名（保留便于排障）、不改 URL 其它部分、不动长度以外的结构。
//!
//! 注意：**不是「所有像秘密的字符串」都认得**。`Authorization: Bearer xxx`、
//! 自定义头之类不在本规则内——落盘前请不要把整段原始请求塞进历史。

/// 脱敏后的占位符。
pub const REDACTED: &str = "***";

/// 值的终止字符（出现在这些字符之前的连续片段才算「秘密值」）。
const VALUE_END: &[char] = &[
    ' ', '\t', '\r', '\n', '"', '\'', '&', ',', ';', ')', ']', '}', '>', '<', '`', '|',
];

/// 把文本里出现秘密值的片段替换成 [`REDACTED`]。
pub fn redact(message: &str) -> String {
    let text = with_values_redacted(message, "token:", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "token=", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "rpc-secret=", |ch| !VALUE_END.contains(&ch));
    let text = with_values_redacted(&text, "urn:btih:", |ch| ch.is_ascii_alphanumeric());
    redact_url_password(&text)
}

/// 落盘前给 URI 用的别名（语义相同，独立名字是为了调用点读起来明确）。
pub fn redact_uri(uri: &str) -> String {
    redact(uri)
}

/// 把 `needle` 之后的「连续值字符」替换成 [`REDACTED`]（大小写不敏感，保留 `needle` 原文）。
fn with_values_redacted<F>(message: &str, needle: &str, is_value: F) -> String
where
    F: Fn(char) -> bool,
{
    let lower = message.to_ascii_lowercase();
    let mut out = String::with_capacity(message.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find(needle) {
        let start = search + offset;
        let value_start = start + needle.len();
        let mut value_end = value_start;
        for (index, ch) in message[value_start..].char_indices() {
            if !is_value(ch) {
                break;
            }
            value_end = value_start + index + ch.len_utf8();
        }
        out.push_str(&message[copied..value_start]);
        if value_end > value_start {
            out.push_str(REDACTED);
        }
        copied = value_end;
        // 空值也要前进，否则死循环。
        search = value_end.max(value_start);
    }
    out.push_str(&message[copied..]);
    out
}

/// `scheme://user:password@host/...` 里的密码 → [`REDACTED`]（用户名保留，便于排障）。
fn redact_url_password(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let mut out = String::with_capacity(message.len());
    let mut copied = 0usize;
    let mut search = 0usize;
    while let Some(offset) = lower[search..].find("://") {
        let authority_start = search + offset + 3;
        // 权限段到下一个 `/`、`?`、`#`、空白或引号为止。
        let mut authority_end = message.len();
        for (index, ch) in message[authority_start..].char_indices() {
            if matches!(ch, '/' | '?' | '#' | ' ') || VALUE_END.contains(&ch) {
                authority_end = authority_start + index;
                break;
            }
        }
        let authority = &message[authority_start..authority_end];
        if let Some(at) = authority.rfind('@') {
            if let Some(colon) = authority[..at].find(':') {
                let password_start = authority_start + colon + 1;
                let password_end = authority_start + at;
                out.push_str(&message[copied..password_start]);
                if password_end > password_start {
                    out.push_str(REDACTED);
                }
                copied = password_end;
            }
        }
        search = authority_end.max(authority_start);
        if search >= message.len() {
            break;
        }
        // 跳过已经处理过的权限段，继续找下一个 `://`。
        search += 1;
    }
    out.push_str(&message[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_aria2_token_prefix() {
        assert_eq!(redact("token:abc123"), "token:***");
        assert_eq!(
            redact("调用失败 params=[token:s3cr3t, [\"url\"]]"),
            "调用失败 params=[token:***, [\"url\"]]"
        );
    }

    #[test]
    fn redacts_query_token() {
        assert_eq!(
            redact("GET http://h/api?token=abc123&id=7"),
            "GET http://h/api?token=***&id=7"
        );
        assert_eq!(redact("&token=deadbeef"), "&token=***");
    }

    #[test]
    fn redacts_rpc_secret_in_both_forms() {
        assert_eq!(
            redact("aria2c --rpc-secret=topsecret --enable-rpc"),
            "aria2c --rpc-secret=*** --enable-rpc"
        );
        assert_eq!(redact("rpc-secret=another"), "rpc-secret=***");
    }

    #[test]
    fn redacts_magnet_infohash() {
        let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x";
        assert_eq!(redact(magnet), "magnet:?xt=urn:btih:***&dn=x");
    }

    #[test]
    fn redacts_url_password_but_keeps_user() {
        assert_eq!(
            redact("https://alice:hunter2@example.com/a/b"),
            "https://alice:***@example.com/a/b"
        );
        // 没有 @ 的普通 URL 原样
        assert_eq!(
            redact("https://example.com/a/b?token=***"),
            "https://example.com/a/b?token=***"
        );
    }

    #[test]
    fn multiple_secrets_in_one_line() {
        assert_eq!(
            redact("uri=https://u:p@h/p?token=t1&x=1 token:t2 end"),
            "uri=https://u:***@h/p?token=***&x=1 token:*** end"
        );
    }

    #[test]
    fn plain_text_is_untouched() {
        for text in [
            "下载完成：aria2-1.37.0-win-64bit.zip",
            "",
            "gid=abc123 status=complete",
            "https://example.com/file.zip",
        ] {
            assert_eq!(redact(text), text, "{text}");
        }
    }

    #[test]
    fn utf8_is_safe_and_length_can_shrink_or_grow() {
        // 秘密值里含中文：按字符边界计算，不能切碎 UTF-8。
        assert_eq!(redact("token:中文秘密值 done"), "token:*** done");
        // 不得产生非法 UTF-8（String 保证，这里只是显式过一遍）。
        let out = redact("token:密码&x=1");
        assert!(out.is_char_boundary(out.len()));
        assert_eq!(out, "token:***&x=1");
    }

    #[test]
    fn empty_value_does_not_hang() {
        assert_eq!(redact("token:"), "token:");
        assert_eq!(redact("token:&x=1"), "token:&x=1");
        assert_eq!(redact("token= "), "token= ");
    }
}
