//! `/provider/{profileName}/{upstreamPath}` 的解析器。
//!
//! 拼接只有一条规则（`docs/03-managed-provider-mode.md` §5）：
//!
//! ```text
//! target = rstrip(base_url, '/') + '/' + lstrip_once(rest_path, '/')
//! ```
//!
//! 「只有一条」是刻意的。任何「智能」拼接（发现两边都有 `/v1` 就去掉一个）
//! 都会在某个 `base_url` 上猜错，而猜错的表现是上游 404 —— 日志里一切正常。

use argus_core::{ObserveMode, ProxyError, ResolvedTarget, TargetOrigin};
use http::Uri;

use super::{
    MANAGED_PREFIX, ProfileRegistry, TargetResolver, ensure_absolute, invalid_profile_name,
    path::{is_valid_profile_name, split_first_segment, strip_one_leading_slash},
    push_query,
};

/// Managed 模式解析器。无状态，随手构造。
#[derive(Copy, Clone, Debug, Default)]
pub struct ManagedResolver;

impl TargetResolver for ManagedResolver {
    /// # Errors
    ///
    /// | 情况 | 错误 | status |
    /// |---|---|---|
    /// | path 不以 `/provider/` 开头 | `ProviderNotFound` | 404 |
    /// | profile 名不合字符集（含 `%6Fpenai` 这类绕过写法） | `InvalidRequest` | 400 |
    /// | `/provider/{name}` 后面什么都没有 | `EmptyUpstreamPath` | 400 |
    /// | Profile 不存在 | `ProviderNotFound` | 404 |
    /// | Profile 被禁用 | `ProviderDisabled` | 503 |
    /// | `base_url` 拼不出绝对 URL | `ProviderMisconfigured` | 503 |
    fn resolve(
        &self,
        parts: &http::request::Parts,
        profiles: &dyn ProfileRegistry,
    ) -> Result<ResolvedTarget, ProxyError> {
        let tail = parts
            .uri
            .path()
            .strip_prefix(MANAGED_PREFIX)
            .ok_or(ProxyError::ProviderNotFound)?;

        // 没有第二个 '/' 有两种可能，报不同的错才好诊断：
        //   /provider/openai   → 名字合法但没给 path，说明客户端 base_url 配错了
        //   /provider/OpenAI   → 名字本身就不合法
        let Some((name, rest)) = split_first_segment(tail) else {
            return Err(if is_valid_profile_name(tail) {
                ProxyError::EmptyUpstreamPath
            } else {
                invalid_profile_name()
            });
        };

        if !is_valid_profile_name(name) {
            return Err(invalid_profile_name());
        }
        let profile = profiles.lookup(name).ok_or(ProxyError::ProviderNotFound)?;
        if !profile.enabled {
            return Err(ProxyError::ProviderDisabled);
        }

        let uri = join(profile.base_url, rest, parts.uri.query())?;

        Ok(ResolvedTarget {
            attempt: 0,
            origin: TargetOrigin::Managed {
                profile: profile.id,
                profile_name: profile.name.into(),
            },
            uri,
            credential: profile.credential.clone(),
            // 暂定值。R04 收到响应 header 后可以把它收窄成 Generic*
            observer: profile.observer,
            timeouts: profile.timeouts,
            retry: std::sync::Arc::clone(profile.retry),
            // 采样决策不在 R02 范围内（→ R04 / F20）
            observe: ObserveMode::default(),
        })
    }
}

/// `docs/03` §5 的拼接规则，唯一实现。
///
/// 两边的处理**刻意不对称**：
/// - `base_url` 去掉尾部**全部**的 `/` —— `https://x.com`、`https://x.com/`、
///   `https://x.com//` 三种写法等价，用户不会因为多打一个斜杠收到 404。
/// - rest 只去掉**一个**前导 `/` —— `/provider/x//foo` 里那个 `//` 是客户端
///   发出来的字节，折叠它就是替上游做决定（R5）。
///
/// # Errors
///
/// 拼出来的串不是绝对 URL（`base_url` 配错）时返回 `ProviderMisconfigured`。
fn join(base_url: &str, rest: &str, query: Option<&str>) -> Result<Uri, ProxyError> {
    let base = base_url.trim_end_matches('/');
    let rest = strip_one_leading_slash(rest);

    let mut out =
        String::with_capacity(base.len() + 1 + rest.len() + query.map_or(0, |q| q.len() + 1));
    out.push_str(base);
    out.push('/');
    out.push_str(rest);
    push_query(&mut out, query);

    let uri = Uri::try_from(out).map_err(|_| ProxyError::ProviderMisconfigured)?;
    ensure_absolute(uri, ProxyError::ProviderMisconfigured)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use super::*;

    fn joined(base: &str, rest: &str) -> String {
        join(base, rest, None).expect("固定的合法值").to_string()
    }

    /// `docs/03` §5：三种尾斜杠写法必须等价。
    #[test]
    fn join_base_url_with_any_number_of_trailing_slashes_produces_one_slash() {
        for base in ["https://x.com", "https://x.com/", "https://x.com//"] {
            assert_eq!(joined(base, "/v1/messages"), "https://x.com/v1/messages");
        }
    }

    /// `docs/03` §5.1：`base_url` 自带前缀时整体作为前缀，不被 `join` 语义吃掉。
    #[test]
    fn join_base_url_carrying_a_path_prefix_keeps_the_whole_prefix() {
        assert_eq!(
            joined("https://gw.corp.com/ai/v1", "/chat/completions"),
            "https://gw.corp.com/ai/v1/chat/completions"
        );
    }

    /// `docs/03` §5.2：尾斜杠请求转发到 `base_url` 的根路径。
    #[test]
    fn join_rest_that_is_just_a_slash_targets_the_base_root() {
        assert_eq!(joined("https://x.com", "/"), "https://x.com/");
    }

    /// R5：`//` 不折叠，两个位置都要验 —— 中间的和紧跟在 profile 名后面的。
    #[test]
    fn join_rest_containing_double_slashes_preserves_all_of_them() {
        assert_eq!(joined("https://x.com", "/v1//foo"), "https://x.com/v1//foo");
        assert_eq!(joined("https://x.com", "//foo"), "https://x.com//foo");
    }

    #[test]
    fn join_query_present_appends_it_byte_for_byte() {
        let uri =
            join("https://x.com", "/v1", Some("a=1&b=a%2Bb&empty=&flag")).expect("固定的合法值");
        assert_eq!(uri.to_string(), "https://x.com/v1?a=1&b=a%2Bb&empty=&flag");
    }

    /// 光秃秃的 `?` 也是客户端发出来的字节。
    #[test]
    fn join_query_that_is_empty_still_emits_the_question_mark() {
        let uri = join("https://x.com", "/v1", Some("")).expect("固定的合法值");
        assert_eq!(uri.to_string(), "https://x.com/v1?");
    }

    /// `Uri` 会把 `/v1/x` 当成合法的 origin-form —— 一个写错的 `base_url` 就能
    /// 静默地让 Argus 往自己身上发请求。
    #[test]
    fn join_base_url_without_scheme_returns_provider_misconfigured() {
        for base in ["", "/", "api.openai.com"] {
            assert_eq!(
                join(base, "/v1", None).unwrap_err(),
                ProxyError::ProviderMisconfigured,
                "base_url={base:?} 应该被判为配置错误"
            );
        }
    }
}
