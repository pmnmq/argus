//! 客户端 header → 上游 header（`docs/06-data-plane.md` §6.3–6.4）。
//!
//! 三条规则，方向刻意不对称：
//!
//! | | 处理 | 为什么 |
//! |---|---|---|
//! | hop-by-hop、`Host`、客户端凭据 | **必剥** | 前两类跨连接就没意义，第三类绝不能到 provider |
//! | provider 凭据、`Host` | **必注** | 上游要的是它自己的那份 |
//! | 其余**全部**（含没见过的） | **原样透传** | 见下 |
//!
//! 未知 header 默认透传是刻意的，也是这一层最重要的一条：Anthropic 与
//! `OpenAI` 经常用新 header 开启新能力（`anthropic-beta`、`OpenAI-Beta`……）。
//! 走白名单的话，每出一个新 header 就得给 Argus 发一次版 —— 网关会变成
//! 能力瓶颈，而这恰恰是宪法 C2「未知即透传」要避免的。
//!
//! 由此带来的风险（客户端借 header 往上游注入东西）由剥离黑名单兜住，
//! 特别是 `x-argus-*` 前缀一律剥掉，防止伪造内部标记。

use argus_core::{CredentialMode, ProxyError, ResolvedTarget};
use http::{
    HeaderMap, HeaderName, HeaderValue,
    header::{HOST, USER_AGENT},
};

/// 一律不从客户端带到上游的 header（`docs/06` §6.4）。
pub const STRIP_FROM_CLIENT: &[&str] = &[
    // 凭据：客户端用来认 Argus 的那把 key，与上游无关
    "authorization",
    "x-api-key",
    "api-key",
    // hop-by-hop：定义上就只在一跳内有效，转发过去会让上游误解连接语义
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    // Host 由上游 URI 决定，不是客户端说了算
    "host",
];

/// 内部标记的保留前缀。客户端发什么都剥掉，防伪造。
pub const RESERVED_PREFIX: &str = "x-argus-";

/// Argus 自报家门用的 header，方便上游与我们自己对账。
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-argus-request-id");

/// 组装发给上游的 header 集合。
///
/// # Errors
///
/// 上游 URI 没有 authority（拼不出 `Host`）时返回
/// [`ProxyError::InvalidRawTarget`]。
pub fn build_upstream_headers(
    client: &HeaderMap,
    target: &ResolvedTarget,
) -> Result<HeaderMap, ProxyError> {
    let mut out = HeaderMap::with_capacity(client.len() + 4);

    for (name, value) in client {
        if should_strip(name) {
            continue;
        }
        // append 而不是 insert：同名多值的 header（`accept`、`cookie`）
        // 合并成一条会改变语义，而宪法 C1 说的是**原样**
        out.append(name.clone(), value.clone());
    }

    let authority = target.uri.authority().ok_or(ProxyError::InvalidRawTarget)?.as_str();
    let host = HeaderValue::from_str(authority).map_err(|_| ProxyError::InvalidRawTarget)?;
    out.insert(HOST, host);

    // Passthrough 模式下客户端的上游凭据在哪？—— 不在这里。它走的是
    // 客户端自己发的那个 header，而那个 header 恰好在剥离名单上……
    // 所以 Passthrough 由 `preserve_client_credentials` 单独处理，见下
    match &target.credential {
        CredentialMode::Injected(cred) => {
            out.insert(cred.header_name().clone(), cred.header_value());
        }
        CredentialMode::Passthrough => preserve_client_credentials(client, &mut out),
        CredentialMode::None => {}
    }

    if !out.contains_key(USER_AGENT) {
        out.insert(USER_AGENT, HeaderValue::from_static(default_user_agent()));
    }
    Ok(out)
}

/// Passthrough：客户端自带上游凭据，Argus **既不看也不存**，原样带过去。
///
/// 这里要把刚才剥掉的凭据 header 放回来。看着绕，但两步分开是对的：
/// 默认剥离保证了「忘了处理」时凭据不会泄漏到上游，而放回来是一个需要
/// 显式声明 `Passthrough` 才发生的动作。
fn preserve_client_credentials(client: &HeaderMap, out: &mut HeaderMap) {
    for name in ["authorization", "x-api-key", "api-key"] {
        let Ok(header) = HeaderName::try_from(name) else {
            continue;
        };
        for value in client.get_all(&header) {
            out.append(header.clone(), value.clone());
        }
    }
}

/// 逐跳 header：定义上只在一跳内有效，转发过去会让对端误解连接语义。
///
/// 上行下行都要剥。下行漏了 `transfer-encoding` 尤其致命 —— hyper 会
/// 按自己的方式重新分块，两个 `transfer-encoding` 撞在一起就是响应截断。
#[must_use]
pub fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    )
}

fn should_strip(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.starts_with(RESERVED_PREFIX) || STRIP_FROM_CLIENT.contains(&name)
}

const fn default_user_agent() -> &'static str {
    concat!("argus/", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use std::sync::Arc;

    use argus_core::{
        CredentialKind, ObserveMode, ObserverKind, ResolvedCredential, RetryPolicy, TargetOrigin,
        Timeouts, ids::CredentialId,
    };
    use http::Uri;

    use super::*;

    const CLIENT_SECRET: &str = "sk-client-should-never-leave";
    const PROVIDER_SECRET: &str = "sk-provider-0123456789";

    fn target(credential: CredentialMode) -> ResolvedTarget {
        ResolvedTarget {
            attempt: 0,
            origin: TargetOrigin::Raw,
            uri: Uri::from_static("https://api.example.com/v1/messages"),
            credential,
            observer: ObserverKind::AnthropicMessages,
            timeouts: Timeouts::default(),
            retry: Arc::new(RetryPolicy::default()),
            observe: ObserveMode::Full,
        }
    }

    fn injected() -> CredentialMode {
        let cred = ResolvedCredential::new(
            CredentialId::generate(),
            CredentialKind::ApiKeyHeader,
            HeaderName::from_static("x-api-key"),
            PROVIDER_SECRET,
        )
        .expect("固定的合法值");
        CredentialMode::Injected(Arc::new(cred))
    }

    fn client_headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static(CLIENT_SECRET));
        h.insert("x-api-key", HeaderValue::from_static("client-key"));
        h.insert("host", HeaderValue::from_static("argus.internal"));
        h.insert("connection", HeaderValue::from_static("keep-alive"));
        h.insert("x-argus-internal", HeaderValue::from_static("forged"));
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        h.insert("anthropic-beta", HeaderValue::from_static("tools-2024"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h
    }

    /// 客户端用来认 Argus 的那把 key **绝不能**到 provider 手上。
    #[test]
    fn build_upstream_headers_strips_every_client_credential() {
        let out = build_upstream_headers(&client_headers(), &target(injected())).expect("可构建");
        let rendered = format!("{out:?}");
        assert!(
            !rendered.contains(CLIENT_SECRET),
            "客户端凭据泄漏：{rendered}"
        );
        assert!(
            !rendered.contains("client-key"),
            "客户端凭据泄漏：{rendered}"
        );
    }

    #[test]
    fn build_upstream_headers_injects_the_provider_credential() {
        let out = build_upstream_headers(&client_headers(), &target(injected())).expect("可构建");
        assert_eq!(
            out.get("x-api-key").map(HeaderValue::as_bytes),
            Some(PROVIDER_SECRET.as_bytes())
        );
    }

    /// 宪法 C2：没见过的 header 默认透传，否则 Argus 会变成能力瓶颈。
    #[test]
    fn build_upstream_headers_passes_unknown_headers_through() {
        let out = build_upstream_headers(&client_headers(), &target(injected())).expect("可构建");
        assert!(out.contains_key("anthropic-version"));
        assert!(out.contains_key("anthropic-beta"));
        assert!(out.contains_key("content-type"));
    }

    /// hop-by-hop 与 `Host` 由这一跳自己决定，客户端说了不算。
    #[test]
    fn build_upstream_headers_replaces_host_and_drops_hop_by_hop() {
        let out = build_upstream_headers(&client_headers(), &target(injected())).expect("可构建");
        assert_eq!(
            out.get(HOST).map(HeaderValue::as_bytes),
            Some(&b"api.example.com"[..]),
            "Host 必须是上游的，不是客户端写的"
        );
        assert!(!out.contains_key("connection"));
    }

    /// `x-argus-*` 是保留前缀，客户端发什么都剥掉 —— 防伪造内部标记。
    #[test]
    fn build_upstream_headers_strips_the_reserved_prefix() {
        let out = build_upstream_headers(&client_headers(), &target(injected())).expect("可构建");
        assert!(!out.contains_key("x-argus-internal"));
    }

    /// Passthrough：客户端自带上游凭据，原样带过去。
    ///
    /// 默认剥离 + 显式放回，两步分开是刻意的：忘了处理时凭据不会泄漏，
    /// 而放回来必须是一个写明了 `Passthrough` 才发生的动作。
    #[test]
    fn build_upstream_headers_in_passthrough_mode_restores_the_client_credential() {
        let out = build_upstream_headers(&client_headers(), &target(CredentialMode::Passthrough))
            .expect("可构建");
        assert_eq!(
            out.get("authorization").map(HeaderValue::as_bytes),
            Some(CLIENT_SECRET.as_bytes())
        );
    }

    /// 上游不需要认证（本地模型很常见）：既不注入也不放回。
    #[test]
    fn build_upstream_headers_with_no_credential_leaves_both_out() {
        let out = build_upstream_headers(&client_headers(), &target(CredentialMode::None))
            .expect("可构建");
        assert!(!out.contains_key("authorization"));
        assert!(!out.contains_key("x-api-key"));
    }

    /// 同名多值不能被合并成一条 —— 那会改变语义（宪法 C1）。
    #[test]
    fn build_upstream_headers_keeps_repeated_headers_separate() {
        let mut client = HeaderMap::new();
        client.append("accept", HeaderValue::from_static("text/event-stream"));
        client.append("accept", HeaderValue::from_static("application/json"));
        let out = build_upstream_headers(&client, &target(CredentialMode::None)).expect("可构建");
        assert_eq!(out.get_all("accept").iter().count(), 2);
    }

    #[test]
    fn build_upstream_headers_adds_a_user_agent_when_the_client_omits_it() {
        let out = build_upstream_headers(&HeaderMap::new(), &target(CredentialMode::None))
            .expect("可构建");
        let ua = out.get(USER_AGENT).expect("有 UA");
        assert!(ua.to_str().expect("ascii").starts_with("argus/"));
    }

    #[test]
    fn build_upstream_headers_keeps_the_client_user_agent_when_present() {
        let mut client = HeaderMap::new();
        client.insert(USER_AGENT, HeaderValue::from_static("my-sdk/1.0"));
        let out = build_upstream_headers(&client, &target(CredentialMode::None)).expect("可构建");
        assert_eq!(
            out.get(USER_AGENT).map(HeaderValue::as_bytes),
            Some(&b"my-sdk/1.0"[..])
        );
    }

    /// 下行也要剥 hop-by-hop：漏掉 `transfer-encoding` 会让 hyper 重新分块，
    /// 两个撞在一起就是响应截断。
    #[test]
    fn hop_by_hop_covers_every_connection_scoped_header() {
        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "proxy-authenticate",
            "proxy-authorization",
        ] {
            let header = HeaderName::try_from(name).expect("合法");
            assert!(hop_by_hop(&header), "{name} 应该被认作 hop-by-hop");
        }
        assert!(!hop_by_hop(&HeaderName::from_static("content-type")));
    }
}
