//! path 切分与 profile 名校验。
//!
//! R02 里唯一的纯函数层，也是最容易写错的一层。这里每个函数都**只看原始
//! 字节**：不 decode、不规范化、不折叠 `//`、不改大小写（宪法 C35，
//! `docs/04-raw-proxy-mode.md` §5.3）。

use memchr::memchr;

/// profile 名的最大字节数，对应 `^[a-z0-9][a-z0-9-]{0,62}$` 的 1 + 62。
pub const MAX_PROFILE_NAME_LEN: usize = 63;

/// 按字节在第一个 `/` 处切开，返回 `(第一段, 含前导 '/' 的剩余部分)`。
///
/// 输入是**前缀之后**的尾串（例如 `openai/v1/messages`），不是完整 path。
/// 没有 `/` 时返回 `None` —— 调用方据此区分「有上游 path」与「没有」，
/// 这两种情况在 Managed 模式下的处理不同（`docs/03` §5.2）。
///
/// 切分点是一个 `/` 字节，天然落在 UTF-8 边界上；`%2F` 是三个字节
/// `%`、`2`、`F`，不含 `/`，因此**不会**被当成分隔符 —— 这正是不能先
/// decode 再切的原因。
#[must_use]
pub fn split_first_segment(tail: &str) -> Option<(&str, &str)> {
    memchr(b'/', tail.as_bytes()).map(|i| tail.split_at(i))
}

/// 只去掉**一个**前导 `/`。
///
/// 刻意不是 `trim_start_matches('/')`：`/provider/x//foo` 切出来的剩余部分是
/// `//foo`，全部剥掉会把它折叠成 `/foo`，那就是在替上游做决定了（R5）。
#[must_use]
pub fn strip_one_leading_slash(rest: &str) -> &str {
    rest.strip_prefix('/').unwrap_or(rest)
}

/// profile 名字符集校验：`^[a-z0-9][a-z0-9-]{0,62}$`，作用在**原始字节**上。
///
/// `%` 不在字符集里，所以这一条同时也是反绕过检查：`%6Fpenai` 直接不合法。
/// 反过来「先 decode 再校验」是错的 —— `%6Fpenai` 会解成 `openai` 并顺利
/// 通过（`docs/03-managed-provider-mode.md` §7）。
#[must_use]
pub fn is_valid_profile_name(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    match bytes.first() {
        None => false,
        Some(&first) => {
            bytes.len() <= MAX_PROFILE_NAME_LEN
                && (first.is_ascii_lowercase() || first.is_ascii_digit())
                && bytes[1..]
                    .iter()
                    .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }
    }
}

/// 大小写不敏感地剥掉一个 ASCII 前缀，剥不掉返回 `None`。
///
/// 只用于 scheme（RFC 3986 §3.1 规定 scheme 大小写不敏感）。**不要**拿它
/// 处理 path —— path 的大小写必须原样保留（R5）。
#[must_use]
pub fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| s.get(prefix.len()..)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first_segment_tail_with_upstream_path_keeps_the_leading_slash() {
        assert_eq!(
            split_first_segment("openai/v1/messages"),
            Some(("openai", "/v1/messages"))
        );
    }

    #[test]
    fn split_first_segment_tail_without_slash_returns_none() {
        assert_eq!(split_first_segment("openai"), None);
        assert_eq!(split_first_segment(""), None);
    }

    /// 尾斜杠与没有斜杠必须可区分：前者转发到根，后者 400（`docs/03` §5.2）。
    #[test]
    fn split_first_segment_tail_ending_with_slash_returns_an_empty_rest_slash() {
        assert_eq!(split_first_segment("openai/"), Some(("openai", "/")));
    }

    /// 编码后的斜杠不是分隔符。这一条错了，上游收到的就是另一个资源。
    #[test]
    fn split_first_segment_tail_containing_encoded_slash_does_not_split_on_it() {
        let (name, rest) = split_first_segment("x/v1/models/gpt%2F4").expect("有 '/'");
        assert_eq!(name, "x");
        assert_eq!(rest, "/v1/models/gpt%2F4");
    }

    #[test]
    fn split_first_segment_tail_with_double_slash_does_not_collapse_it() {
        assert_eq!(split_first_segment("x//foo"), Some(("x", "//foo")));
    }

    #[test]
    fn strip_one_leading_slash_rest_with_double_slash_leaves_the_second_one() {
        assert_eq!(strip_one_leading_slash("//foo"), "/foo");
        assert_eq!(strip_one_leading_slash("/v1//foo"), "v1//foo");
        assert_eq!(strip_one_leading_slash("/"), "");
        assert_eq!(strip_one_leading_slash("v1"), "v1");
    }

    #[test]
    fn is_valid_profile_name_names_from_the_docs_returns_true() {
        for name in ["openai", "openai-prod", "azure-eu", "a", "x9", "9x"] {
            assert!(is_valid_profile_name(name), "{name} 应该合法");
        }
    }

    /// 大写、下划线、点、斜杠、空段都不在字符集里。
    #[test]
    fn is_valid_profile_name_names_outside_the_charset_returns_false() {
        for name in [
            "", "-lead", "OpenAI", "open_ai", "open.ai", "a/b", "a b", "ä",
        ] {
            assert!(!is_valid_profile_name(name), "{name} 应该非法");
        }
    }

    /// R7 的核心：`%` 不在字符集里，所以任何百分号写法都进不来 ——
    /// 这才是拒绝 `%6Fpenai` 的那道门，而不是「decode 之后再校验」。
    #[test]
    fn is_valid_profile_name_percent_encoded_bypass_returns_false() {
        assert!(!is_valid_profile_name("%6Fpenai"));
        assert!(!is_valid_profile_name("open%2Dai"));
        assert!(!is_valid_profile_name("openai%00"));
        // 而它 decode 之后是完全合法的 —— 顺序反了就会放行
        assert!(is_valid_profile_name("openai"));
    }

    #[test]
    fn is_valid_profile_name_name_longer_than_63_bytes_returns_false() {
        assert!(is_valid_profile_name(&"a".repeat(MAX_PROFILE_NAME_LEN)));
        assert!(!is_valid_profile_name(
            &"a".repeat(MAX_PROFILE_NAME_LEN + 1)
        ));
    }

    #[test]
    fn strip_prefix_ascii_ci_prefix_in_any_case_returns_the_remainder() {
        assert_eq!(strip_prefix_ascii_ci("HTTPS://x", "https://"), Some("x"));
        assert_eq!(strip_prefix_ascii_ci("hTtPs://x", "https://"), Some("x"));
        assert_eq!(strip_prefix_ascii_ci("https://x", "https://"), Some("x"));
    }

    #[test]
    fn strip_prefix_ascii_ci_non_matching_or_short_input_returns_none() {
        assert_eq!(strip_prefix_ascii_ci("http://x", "https://"), None);
        assert_eq!(strip_prefix_ascii_ci("htt", "https://"), None);
        // 前缀长度落在多字节字符中间时必须返回 None 而不是 panic
        assert_eq!(strip_prefix_ascii_ci("ä", "h"), None);
    }
}
