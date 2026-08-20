//! C35 的注入用例：证明「不能用 `axum::extract::Path`」不是空谈（R02 T8）。
//!
//! `clippy.toml` 的 `disallowed-types` 已经挡着这个提取器，但 lint 只是
//! **约定**：它能被 `#[allow]` 掉，也能在某次升级里被悄悄改掉。所以还要有一个
//! 真的会跑的用例，把危害本身钉死 —— 这样即使有人有意绕过 lint，也会先看见
//! 这段解释。
//!
//! 实测结论见 `docs/04-raw-proxy-mode.md` §5.2。

mod common;

use argus_proxy::ingress::resolve_target;
use axum::{Router, body::Body, routing::get};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::common::{TestProfiles, parts};

/// 客户端要访问的是一个名字里**真的带斜杠**的模型。
const REQUEST_TARGET: &str = "/provider/openai/v1/models/gpt%2F4";

/// 这是本仓库里唯一允许出现 `axum::extract::Path` 的地方。
#[allow(
    clippy::disallowed_types,
    reason = "C35 的注入用例：这里就是要用它，好证明它为什么被禁"
)]
async fn extract_with_axum_path(
    axum::extract::Path((name, rest)): axum::extract::Path<(String, String)>,
) -> String {
    format!("{name}|{rest}")
}

mod resolver_transparency {
    use super::{
        Body, BodyExt, REQUEST_TARGET, Request, Router, ServiceExt, TestProfiles,
        extract_with_axum_path, get, parts, resolve_target,
    };

    /// `Path` 提取器把 `%2F` 解成 `/`，**路径段数都变了**：
    /// 一个模型名 `gpt/4` 变成了两段 `gpt` + `4`。
    ///
    /// 这个断言写的是 axum 的**错误**行为。它哪天变了，这里会红 ——
    /// 那时该做的是重新评估 C35，而不是把断言改一改。
    #[tokio::test]
    async fn resolver_axum_path_extractor_decodes_percent_encoded_slash() {
        let app = Router::new().route("/provider/{name}/{*rest}", get(extract_with_axum_path));
        let response = app
            .oneshot(
                Request::builder()
                    .uri(REQUEST_TARGET)
                    .body(Body::empty())
                    .expect("合法 request"),
            )
            .await
            .expect("Router 不会失败");

        let body = response.into_body().collect().await.expect("body 可读").to_bytes();
        let extracted = String::from_utf8(body.to_vec()).expect("UTF-8");

        assert_eq!(
            extracted, "openai|v1/models/gpt/4",
            "axum 的 Path 提取器把 %2F 解成了 /，这正是 C35 要禁掉它的原因"
        );
        assert!(
            !extracted.contains("%2F"),
            "如果这条断言开始失败，说明 axum 改了行为，需要重新评估 C35"
        );
    }

    /// 同一个 URL 走 Resolver：`%2F` 原样活着，段数没变。
    #[test]
    fn resolver_resolve_target_on_the_same_url_keeps_the_encoded_slash() {
        let profiles = TestProfiles::one("openai", "https://api.openai.com");
        let target = resolve_target(&parts(REQUEST_TARGET), &profiles).expect("合法");

        assert_eq!(
            target.uri.to_string(),
            "https://api.openai.com/v1/models/gpt%2F4"
        );
        // 段数：`` / `v1` / `models` / `gpt%2F4` —— decode 之后会变成 5 段
        assert_eq!(target.uri.path().split('/').count(), 4);
    }
}
