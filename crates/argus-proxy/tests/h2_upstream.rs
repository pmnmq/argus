//! 宪法 C23 的端到端断言：上游真的协商到了 HTTP/2。
//!
//! F06 只验到了 `tls_config` 里 ALPN 数组的顺序（F06 §7.2）—— 那证明的是
//! 「我们提了 h2」，不是「协商成了 h2」。中间隔着 rustls 的握手、hyper 的
//! 版本选择、连接池的复用策略，每一环都可能把它退回 HTTP/1.1，而且**退回
//! 是静默的**：请求照样成功，只是 10k 并发下连接数差两个数量级。
//!
//! 这条断言必须走 TLS：ALPN 只在 TLS 上存在。明文 h2c 走的是另一条协商
//! 路径，验不了这件事。

use std::sync::{Arc, Mutex};

use argus_core::Timeouts;
use argus_protocol::{
    interaction::ObservedInteraction,
    sink::{InteractionSink, SinkFull},
};
use argus_proxy::{
    body::CancelHandle,
    ingress::{ProxyConfig, ProxyState, StaticRegistry, router},
    observer::ObserverRules,
    target::{CheckedResolver, SystemResolver, TargetPolicy, TargetPolicyConfig},
    testkit::{MockProvider, Scenario},
    upstream::UpstreamClient,
};
use axum::body::Body;
use http::{Request, header::CONTENT_TYPE};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

#[derive(Default)]
struct NullSink(Mutex<Vec<ObservedInteraction>>);

impl InteractionSink for NullSink {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        if let Ok(mut seen) = self.0.lock() {
            seen.push(*interaction);
        }
        Ok(())
    }
}

/// 回环地址在默认策略里是拒绝的。`localhost` 可能同时解析到 v4 与 v6，
/// 两个都要显式放行，否则会因为「多值解析里有一个没放行」被整体拒掉。
fn loopback_policy() -> TargetPolicy {
    let cfg = TargetPolicyConfig {
        allow_cidrs: vec!["127.0.0.0/8".into(), "::1/128".into()],
        ..TargetPolicyConfig::default()
    };
    TargetPolicy::compile(&cfg).expect("固定的合法配置")
}

const REQ: &str = r#"{"model":"claude-x","max_tokens":16,"messages":[]}"#;

fn tls_harness(provider: &MockProvider) -> axum::Router {
    let policy = Arc::new(loopback_policy());
    let resolver = Arc::new(CheckedResolver::new(Arc::new(SystemResolver::default())));
    let roots: Vec<_> = provider.root_cert().into_iter().collect();
    let client = UpstreamClient::trusting(policy, resolver, Timeouts::default(), &roots)
        .expect("客户端可构建");

    router(ProxyState {
        profiles: Arc::new(StaticRegistry::new("mock", provider.base_url())),
        client,
        sink: Arc::new(NullSink::default()) as Arc<dyn InteractionSink>,
        rules: Arc::new(ObserverRules::default()),
        cfg: ProxyConfig::default(),
        shutdown: CancelHandle::new(),
    })
}

async fn call(router: &axum::Router) -> http::Response<Body> {
    let req = Request::post("/provider/mock/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(REQ))
        .expect("请求可构造");
    router.clone().oneshot(req).await.expect("handler 不返回 Err")
}

/// ALPN 提了 h2、服务端也提供 h2 → 上游那一跳必须是 HTTP/2。
#[tokio::test]
async fn send_over_tls_when_the_provider_offers_h2_negotiates_http2_upstream() {
    let provider = MockProvider::tls(Scenario::Synthetic {
        tokens: 3,
        interval: core::time::Duration::ZERO,
    })
    .await;
    let router = tls_harness(&provider);

    let resp = call(&router).await;
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.expect("读完 body").to_bytes();
    assert!(
        String::from_utf8_lossy(&body).contains("text_delta"),
        "响应应当是那段合成的 SSE"
    );

    let head = String::from_utf8_lossy(&provider.last_request().head).into_owned();
    assert!(
        head.starts_with("POST /v1/messages HTTP/2.0"),
        "上游收到的应当是 HTTP/2 请求，实际请求行：{:?}",
        head.lines().next().unwrap_or_default()
    );
}

/// 连接复用：第二次请求走同一条 h2 连接，仍然是 HTTP/2。
///
/// 单独验它是因为 hyper 的连接池是按「scheme + authority + 版本」分桶的，
/// 复用时的版本选择与首次握手是两段代码。
#[tokio::test]
async fn send_over_tls_on_a_reused_connection_stays_on_http2() {
    let provider = MockProvider::tls(Scenario::Synthetic {
        tokens: 1,
        interval: core::time::Duration::ZERO,
    })
    .await;
    let router = tls_harness(&provider);

    for _ in 0..2 {
        let resp = call(&router).await;
        assert_eq!(resp.status(), 200);
        let _ = resp.into_body().collect().await;
    }

    assert_eq!(provider.hits(), 2, "两次请求都应当到达上游");
    for req in provider.requests_seen() {
        let head = String::from_utf8_lossy(&req.head).into_owned();
        assert!(
            head.starts_with("POST /v1/messages HTTP/2.0"),
            "复用连接后退回了 HTTP/1.1：{:?}",
            head.lines().next().unwrap_or_default()
        );
    }
}

/// 明文上游没有 ALPN，只能是 HTTP/1.1 —— 而且**不能谎报**成 h2。
#[tokio::test]
async fn send_over_plaintext_reports_http11_instead_of_claiming_h2() {
    let provider = MockProvider::raw(Scenario::Synthetic {
        tokens: 1,
        interval: core::time::Duration::ZERO,
    })
    .await;
    let router = tls_harness(&provider);

    let resp = call(&router).await;
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await;

    let head = String::from_utf8_lossy(&provider.last_request().head).into_owned();
    assert!(
        head.starts_with("POST /v1/messages HTTP/1.1"),
        "明文上游应当是 HTTP/1.1：{:?}",
        head.lines().next().unwrap_or_default()
    );
}
