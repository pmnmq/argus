//! `/raw/…` 的解析器，两种 URL 形式（`docs/04-raw-proxy-mode.md` §4）。
//!
//! ```text
//! 直接形式  /raw/https://api.openai.com/v1/responses
//! 分段形式  /raw/https/api.openai.com/v1/responses
//! ```
//!
//! 分段形式存在的唯一理由是：整条 URL 里没有连续的两个斜杠，因此能穿过
//! 任何会做 `merge_slashes` 的前置组件（§6）。两种形式必须产出**逐字节
//! 相同**的 target，否则「换个部署方式就打到别的地方」这种事只会在生产里
//! 才被发现。

use std::sync::{Arc, LazyLock};

use argus_core::{
    CredentialMode, ObserveMode, ObserverKind, ProxyError, ResolvedTarget, RetryPolicy,
    TargetOrigin, Timeouts,
};
use http::{Uri, uri::Scheme};
use memchr::memchr;

use super::{
    ProfileRegistry, RAW_PREFIX, TargetResolver, ensure_absolute, path::strip_prefix_ascii_ci,
    push_query,
};

/// Raw 模式下 target 的两种写法。判别结果只影响怎么切串，不影响产出。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RawForm {
    /// `/raw/{scheme}://{authority}{path}`
    Direct,
    /// `/raw/{scheme}/{authority}{path}`
    Segmented,
}

impl RawForm {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Segmented => "segmented",
        }
    }
}

/// 按 `docs/04` §4.3 判别 Raw 的两种形式。
///
/// 返回 `(形式, scheme, scheme 之后的剩余串)`。scheme 按 ASCII 大小写不敏感
/// 匹配（RFC 3986 §3.1），产出一律小写。
///
/// **不做猜测性补全**：`/raw/api.openai.com/v1` 缺 scheme 直接 400，不默认补
/// `https` —— 那会让「用户以为在用 TLS，实际是明文」成为可能。
///
/// # Errors
///
/// 四种前缀都不匹配时返回 [`ProxyError::InvalidRawTarget`]。
pub fn detect_raw_form(rest: &str) -> Result<(RawForm, Scheme, &str), ProxyError> {
    // 长的在前只是为了读起来顺；四个前缀两两不可能同时匹配
    const CANDIDATES: [(RawForm, &str, bool); 4] = [
        (RawForm::Direct, "https://", true),
        (RawForm::Direct, "http://", false),
        (RawForm::Segmented, "https/", true),
        (RawForm::Segmented, "http/", false),
    ];

    for (form, prefix, secure) in CANDIDATES {
        if let Some(after) = strip_prefix_ascii_ci(rest, prefix) {
            let scheme = if secure { Scheme::HTTPS } else { Scheme::HTTP };
            return Ok((form, scheme, after));
        }
    }
    Err(ProxyError::InvalidRawTarget)
}

/// 在第一个 `/` 处把 authority 与 path 分开。
///
/// IPv6 字面量的 `:` 全在方括号里、不含 `/`，所以 `[2001:db8::1]:8000` 走这条
/// 规则也是对的 —— 需要小心的是别去找 `:`，不是别用 `/`。
///
/// # Errors
///
/// authority 为空或含 userinfo 时返回 [`ProxyError::InvalidRawTarget`]
/// （`docs/04` §4.4）。
fn split_authority(after_scheme: &str) -> Result<(&str, &str), ProxyError> {
    let (authority, path) = match memchr(b'/', after_scheme.as_bytes()) {
        Some(i) => after_scheme.split_at(i),
        // `/raw/https://api.openai.com` —— 没有 path，转发到根
        None => (after_scheme, ""),
    };

    if authority.is_empty() {
        return Err(ProxyError::InvalidRawTarget);
    }
    // `https://api.openai.com@evil.com/v1` 读起来像在打 OpenAI，实际 authority
    // 是 evil.com。这个歧义会让人眼和 allowlist 得出不同结论，而 allowlist
    // 正是 SSRF 防线的一部分（`docs/13` §9）。
    if memchr(b'@', authority.as_bytes()).is_some() {
        return Err(ProxyError::InvalidRawTarget);
    }
    Ok((authority, path))
}

/// Raw 模式默认的重试策略。每请求 `Arc::new` 一次纯属浪费，共享一份。
static DEFAULT_RETRY: LazyLock<Arc<RetryPolicy>> =
    LazyLock::new(|| Arc::new(RetryPolicy::default()));

/// Raw 模式解析器。无状态，随手构造。
#[derive(Copy, Clone, Debug, Default)]
pub struct RawResolver;

impl TargetResolver for RawResolver {
    /// Raw 模式不查 Profile，`profiles` 参数用不到 —— target 完全由客户端在
    /// URL 里给出，这正是它与 Managed 的分界。
    ///
    /// # Errors
    ///
    /// | 情况 | 错误 | status |
    /// |---|---|---|
    /// | path 不以 `/raw/` 开头 | `ProviderNotFound` | 404 |
    /// | 缺 scheme / scheme 不在白名单 | `InvalidRawTarget` | 400 |
    /// | authority 为空或含 userinfo | `InvalidRawTarget` | 400 |
    /// | 拼出来的串不是合法绝对 URL | `InvalidRawTarget` | 400 |
    fn resolve(
        &self,
        parts: &http::request::Parts,
        _profiles: &dyn ProfileRegistry,
    ) -> Result<ResolvedTarget, ProxyError> {
        let rest = parts.uri.path().strip_prefix(RAW_PREFIX).ok_or(ProxyError::ProviderNotFound)?;

        let (_form, scheme, after_scheme) = detect_raw_form(rest)?;
        let (authority, upstream_path) = split_authority(after_scheme)?;
        let uri = assemble(&scheme, authority, upstream_path, parts.uri.query())?;

        Ok(ResolvedTarget {
            attempt: 0,
            origin: TargetOrigin::Raw,
            uri,
            // `docs/04` §9：Raw 默认 passthrough。绑定 Argus 保管的凭据需要
            // Key 上的额外权限 + target allowlist，那是 F13/R03 的事
            credential: CredentialMode::Passthrough,
            // 认不出协议就先按非流式 JSON 兜底，R04 会按 host/path/响应
            // Content-Type 收窄。「认不出协议」≠「不观测」
            observer: ObserverKind::GenericJson,
            timeouts: Timeouts::default(),
            retry: Arc::clone(&DEFAULT_RETRY),
            observe: ObserveMode::default(),
        })
    }
}

/// 两种形式在这里汇合：给定 scheme / authority / path，产出唯一的 `Uri`。
///
/// # Errors
///
/// 组不出合法绝对 URL 时返回 [`ProxyError::InvalidRawTarget`]。
fn assemble(
    scheme: &Scheme,
    authority: &str,
    upstream_path: &str,
    query: Option<&str>,
) -> Result<Uri, ProxyError> {
    let scheme = scheme.as_str();
    let mut out = String::with_capacity(
        scheme.len()
            + 3
            + authority.len()
            + upstream_path.len().max(1)
            + query.map_or(0, |q| q.len() + 1),
    );
    out.push_str(scheme);
    out.push_str("://");
    out.push_str(authority);
    // HTTP 没法表示「空 path」，request-target 至少是一个 `/`。这不是补全 ——
    // `curl https://api.openai.com` 发出去的也是 `GET /`
    if upstream_path.is_empty() {
        out.push('/');
    } else {
        out.push_str(upstream_path);
    }
    push_query(&mut out, query);

    let uri = Uri::try_from(out).map_err(|_| ProxyError::InvalidRawTarget)?;
    ensure_absolute(uri, ProxyError::InvalidRawTarget)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use super::*;

    #[test]
    fn detect_raw_form_direct_form_returns_direct_and_the_authority_onwards() {
        let (form, scheme, after) =
            detect_raw_form("https://api.openai.com/v1/responses").expect("合法");
        assert_eq!(form, RawForm::Direct);
        assert_eq!(scheme, Scheme::HTTPS);
        assert_eq!(after, "api.openai.com/v1/responses");
    }

    #[test]
    fn detect_raw_form_segmented_form_returns_the_same_remainder_as_direct() {
        let (form, scheme, after) =
            detect_raw_form("https/api.openai.com/v1/responses").expect("合法");
        assert_eq!(form, RawForm::Segmented);
        assert_eq!(scheme, Scheme::HTTPS);
        assert_eq!(after, "api.openai.com/v1/responses");
    }

    #[test]
    fn detect_raw_form_plaintext_scheme_returns_http() {
        assert_eq!(
            detect_raw_form("http://192.168.1.20:11434/api/chat").expect("合法").1,
            Scheme::HTTP
        );
        assert_eq!(
            detect_raw_form("http/192.168.1.20:11434/api/chat").expect("合法").1,
            Scheme::HTTP
        );
    }

    /// RFC 3986 §3.1：scheme 大小写不敏感。产出一律小写。
    #[test]
    fn detect_raw_form_scheme_in_upper_case_is_accepted_and_lowercased() {
        let (_, scheme, after) = detect_raw_form("HTTPS://api.openai.com/v1").expect("合法");
        assert_eq!(scheme.as_str(), "https");
        assert_eq!(after, "api.openai.com/v1");
    }

    /// 「不做猜测性修补」：缺 scheme 就是 400，不默认补 `https`。
    #[test]
    fn detect_raw_form_target_without_scheme_returns_invalid_raw_target() {
        for rest in [
            "api.openai.com/v1",
            "",
            "/api.openai.com",
            "//api.openai.com",
        ] {
            assert_eq!(
                detect_raw_form(rest).unwrap_err(),
                ProxyError::InvalidRawTarget,
                "{rest:?} 不该被接受"
            );
        }
    }

    /// 白名单之外的 scheme 一律 400，`file://` 这类尤其不能放行。
    #[test]
    fn detect_raw_form_scheme_outside_the_allowlist_returns_invalid_raw_target() {
        for rest in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "gopher://example.com/x",
            "ws://example.com/x",
            "file/etc/passwd",
        ] {
            assert_eq!(
                detect_raw_form(rest).unwrap_err(),
                ProxyError::InvalidRawTarget,
                "{rest:?} 不该被接受"
            );
        }
    }

    #[test]
    fn split_authority_host_with_path_splits_at_the_first_slash() {
        assert_eq!(
            split_authority("api.openai.com/v1/responses").expect("合法"),
            ("api.openai.com", "/v1/responses")
        );
    }

    /// IPv6 字面量里的 `:` 在方括号内，不能干扰切分。
    #[test]
    fn split_authority_ipv6_literal_with_port_keeps_the_brackets_intact() {
        assert_eq!(
            split_authority("[2001:db8::1]:8000/v1/chat").expect("合法"),
            ("[2001:db8::1]:8000", "/v1/chat")
        );
    }

    #[test]
    fn split_authority_target_without_path_returns_an_empty_path() {
        assert_eq!(
            split_authority("api.openai.com").expect("合法"),
            ("api.openai.com", "")
        );
    }

    #[test]
    fn split_authority_empty_authority_returns_invalid_raw_target() {
        assert_eq!(
            split_authority("/v1/responses").unwrap_err(),
            ProxyError::InvalidRawTarget
        );
        assert_eq!(
            split_authority("").unwrap_err(),
            ProxyError::InvalidRawTarget
        );
    }

    /// `https://api.openai.com@evil.com/v1` 的 authority 是 `evil.com`。
    #[test]
    fn split_authority_authority_carrying_userinfo_returns_invalid_raw_target() {
        for after in [
            "api.openai.com@evil.com/v1",
            "user:pass@example.com/v1",
            "@example.com/v1",
        ] {
            assert_eq!(
                split_authority(after).unwrap_err(),
                ProxyError::InvalidRawTarget,
                "{after:?} 不该被接受"
            );
        }
    }

    #[test]
    fn assemble_empty_upstream_path_targets_the_root() {
        let uri = assemble(&Scheme::HTTPS, "api.openai.com", "", None).expect("合法");
        assert_eq!(uri.to_string(), "https://api.openai.com/");
    }

    #[test]
    fn assemble_query_present_appends_it_byte_for_byte() {
        let uri = assemble(&Scheme::HTTPS, "x.com", "/v1", Some("b=a%2Bb&flag")).expect("合法");
        assert_eq!(uri.to_string(), "https://x.com/v1?b=a%2Bb&flag");
    }
}
