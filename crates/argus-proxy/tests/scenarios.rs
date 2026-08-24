//! 九个场景**真的跑起来**是什么样（`docs/20` §8）。
//!
//! `scenario.rs` 的单元测试证的是「展开成了正确的计划」，这里证的是
//! 「计划真的按那样落到线上，并且 Argus 的反应符合预期」。两者缺一不可：
//! 计划对但传输层写错了，或者传输层对但 Argus 处理错了，单元测试都看不见。

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
use http::{Request, StatusCode, header::CONTENT_TYPE};
use http_body_util::BodyExt as _;
use tokio::time::{Duration, timeout};
use tower::ServiceExt as _;

#[derive(Default)]
struct CountingSink(Mutex<Vec<ObservedInteraction>>);

impl InteractionSink for CountingSink {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        if let Ok(mut seen) = self.0.lock() {
            seen.push(*interaction);
        }
        Ok(())
    }
}

fn loopback_policy() -> TargetPolicy {
    let cfg = TargetPolicyConfig {
        allow_cidrs: vec!["127.0.0.0/8".into()],
        ..TargetPolicyConfig::default()
    };
    TargetPolicy::compile(&cfg).expect("固定的合法配置")
}

struct Harness {
    router: axum::Router,
    provider: MockProvider,
}

async fn harness(scenario: Scenario) -> Harness {
    harness_with_timeouts(scenario, Timeouts::default()).await
}

async fn harness_with_timeouts(scenario: Scenario, timeouts: Timeouts) -> Harness {
    let provider = MockProvider::raw(scenario).await;
    let policy = Arc::new(loopback_policy());
    let resolver = Arc::new(CheckedResolver::new(Arc::new(SystemResolver::default())));
    let client = UpstreamClient::new(policy, resolver, Timeouts::default()).expect("客户端可构建");

    let state = ProxyState {
        profiles: Arc::new(
            StaticRegistry::new("mock", provider.base_url()).with_timeouts(timeouts),
        ),
        client,
        sink: Arc::new(CountingSink::default()) as Arc<dyn InteractionSink>,
        rules: Arc::new(ObserverRules::default()),
        cfg: ProxyConfig::default(),
        shutdown: CancelHandle::new(),
    };
    Harness {
        router: router(state),
        provider,
    }
}

const REQ: &str = r#"{"model":"claude-x","max_tokens":16,"messages":[]}"#;

async fn call(h: &Harness) -> http::Response<Body> {
    let req = Request::post("/provider/mock/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(REQ))
        .expect("请求可构造");
    h.router.clone().oneshot(req).await.expect("handler 不返回 Err")
}

/// 读 body，**出错时保留已经到达的部分**。
///
/// 不能用 `collect()`：它遇到传输错误会把整个结果丢掉，而被截断的流正是
/// 「前半段到了、后半段没到」——用 `collect()` 会把它看成「什么都没到」，
/// 恰好抹掉要断言的东西。
async fn text_of(resp: http::Response<Body>) -> String {
    let mut body = resp.into_body();
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(f) => {
                if let Some(data) = f.data_ref() {
                    out.extend_from_slice(data);
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ① Synthetic：压测主力。token 数可控，且真的产出那么多 delta。
#[tokio::test]
async fn scenario_synthetic_delivers_exactly_the_requested_token_count() {
    let h = harness(Scenario::Synthetic {
        tokens: 12,
        interval: Duration::ZERO,
    })
    .await;
    let body = text_of(call(&h).await).await;
    assert_eq!(body.matches("text_delta").count(), 12);
}

/// ② `SlowFirstToken`：首帧确实被推迟了。
///
/// 断言的是「至少等了这么久」而不是精确值 —— 精确值会让用例在负载高的
/// CI 上随机失败，而那种 flaky 比没有断言更糟（`docs/20` R2）。
#[tokio::test]
async fn scenario_slow_first_token_delays_the_first_byte_to_the_client() {
    let delay = Duration::from_millis(150);
    let h = harness(Scenario::SlowFirstToken {
        delay,
        then: Box::new(Scenario::Synthetic {
            tokens: 1,
            interval: Duration::ZERO,
        }),
    })
    .await;

    let started = std::time::Instant::now();
    let body = text_of(call(&h).await).await;
    assert!(
        started.elapsed() >= delay,
        "首帧没有被推迟：只用了 {:?}",
        started.elapsed()
    );
    assert!(body.contains("message_start"));
}

/// ③ `TruncateAfter`：流被掐断，客户端拿到的是残缺的字节。
#[tokio::test]
async fn scenario_truncate_after_cuts_the_stream_short() {
    let h = harness(Scenario::TruncateAfter { frames: 2 }).await;
    let body = text_of(call(&h).await).await;
    assert!(body.contains("message_start"), "前两帧应当到达");
    assert!(!body.contains("message_stop"), "结尾帧不该到达");
}

/// ④ `HttpError`：状态码与 `Retry-After` 原样透传（透明网关不翻译）。
#[tokio::test]
async fn scenario_http_error_passes_the_status_and_retry_after_through() {
    let h = harness(Scenario::HttpError {
        status: 429,
        body: r#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
        retry_after: Some(7),
    })
    .await;
    let resp = call(&h).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()),
        Some("7"),
        "Retry-After 是客户端退避的唯一依据，不能吞掉"
    );
}

/// ⑤ `OkThenErrorEvent`：HTTP 200，但流里第一个事件就是 error。
///
/// 这是 F16 的 S2 重试要认的形状 —— 只看状态码会以为成功了。
#[tokio::test]
async fn scenario_ok_then_error_event_returns_200_with_the_error_inside_the_stream() {
    let h = harness(Scenario::OkThenErrorEvent).await;
    let resp = call(&h).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = text_of(resp).await;
    assert!(body.starts_with("event: error"));
    assert!(body.contains("overloaded_error"));
}

/// ⑥ `OversizedFrame`：超大帧照常逐字节转发，主链路不设内容上限。
#[tokio::test]
async fn scenario_oversized_frame_is_still_forwarded_byte_for_byte() {
    let h = harness(Scenario::OversizedFrame { bytes: 256 * 1024 }).await;
    let expected = h.provider.expected_body();
    let body = text_of(call(&h).await).await;
    assert_eq!(
        body.len(),
        expected.len(),
        "超大帧被截断了 —— 宪法 C1 要求逐字节转发"
    );
}

/// ⑦ `NoDoneSentinel`：没有终止事件，但连接正常关闭。
#[tokio::test]
async fn scenario_no_done_sentinel_completes_the_body_without_a_terminal_event() {
    let h = harness(Scenario::NoDoneSentinel).await;
    let body = text_of(call(&h).await).await;
    assert!(body.contains("text_delta"));
    assert!(!body.contains("message_stop"));
    assert!(!body.contains("[DONE]"));
}

/// ⑧ `Hang`：上游不响应 → headers 超时，**不是**无限等待。
#[tokio::test]
async fn scenario_hang_trips_the_upstream_header_timeout() {
    // headers 超时调到 300ms：默认的 60 秒会让这条用例独占一分钟，
    // 而要证的只是「会超时返回」，不是「超时值是多少」
    let h = harness_with_timeouts(
        Scenario::Hang,
        Timeouts {
            headers: Duration::from_millis(300),
            ..Timeouts::default()
        },
    )
    .await;
    let resp = timeout(Duration::from_secs(10), call(&h))
        .await
        .expect("必须在超时内返回，而不是永远挂着");
    assert!(
        resp.status().is_server_error(),
        "上游不响应应当变成 5xx，实际 {}",
        resp.status()
    );
    assert_eq!(h.provider.hits(), 1, "请求应当真的发到了上游");
}

/// ⑨ `Scripted`：逐帧指定的字节原样到达客户端。
#[tokio::test]
async fn scenario_scripted_delivers_the_exact_bytes_it_was_given() {
    let frames = vec![
        b"event: a\ndata: {\"x\":1}\n\n".to_vec(),
        b"event: b\ndata: {\"y\":2}\n\n".to_vec(),
    ];
    let h = harness(Scenario::Scripted {
        frames: frames.clone(),
        status: 200,
        content_type: "text/event-stream",
        pacing: argus_proxy::testkit::Pacing::AsFastAsPossible,
        truncate: false,
    })
    .await;
    let body = text_of(call(&h).await).await;
    assert_eq!(body.as_bytes(), frames.concat().as_slice());
}

/// `Replay` 单独说明：录制不存在，所以这里只能验「读不出来会报错」。
///
/// 真正的回放用例要等 F10 的录制工具落地（宪法 C25：fixture 只能来自录制）。
/// 现在就手写一份塞进 `tests/fixtures/` 会永久污染语料来源。
#[test]
fn scenario_replay_without_a_recording_fails_loudly_instead_of_serving_nothing() {
    let err = Scenario::Replay {
        fixture: std::path::PathBuf::from("tests/fixtures/does-not-exist.sse"),
        pacing: argus_proxy::testkit::Pacing::AsFastAsPossible,
    }
    .plan()
    .expect_err("录制不存在必须报错");
    assert!(err.to_string().contains("does-not-exist.sse"));
}

// ── ObserveMode::Off ────────────────────────────────────────────────

/// `ObserveMode::Off` 的语义是「Tee 不挂载」。
///
/// 照挂不误的话 Off 只是「照样 tee、照样解析、最后把结果扔掉」，既省不下
/// CPU 也省不下内存 —— 而它存在的全部理由就是过载时省下这两样。F07 的 G4
/// （Full 与 Off 的 CPU 比值）没有这条就根本量不出差别。
#[tokio::test]
async fn proxy_with_observe_off_forwards_everything_and_submits_nothing() {
    let provider = MockProvider::raw(Scenario::Synthetic {
        tokens: 5,
        interval: Duration::ZERO,
    })
    .await;
    let expected = provider.expected_body();

    let policy = Arc::new(loopback_policy());
    let resolver = Arc::new(CheckedResolver::new(Arc::new(SystemResolver::default())));
    let client = UpstreamClient::new(policy, resolver, Timeouts::default()).expect("客户端可构建");
    let sink = Arc::new(CountingSink::default());

    let router = router(ProxyState {
        profiles: Arc::new(
            StaticRegistry::new("mock", provider.base_url())
                .with_observe_mode(argus_core::ObserveMode::Off),
        ),
        client,
        sink: Arc::clone(&sink) as Arc<dyn InteractionSink>,
        rules: Arc::new(ObserverRules::default()),
        cfg: ProxyConfig::default(),
        shutdown: CancelHandle::new(),
    });

    let req = Request::post("/provider/mock/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(REQ))
        .expect("请求可构造");
    let resp = router.oneshot(req).await.expect("handler 不返回 Err");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = text_of(resp).await;
    assert_eq!(
        body.as_bytes(),
        expected.as_slice(),
        "关掉观测不能影响转发的字节（宪法 C1）"
    );

    // 旁路根本没起来，所以等多久都不会有 interaction
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sink.0.lock().expect("锁没被毒化").len(),
        0,
        "Off 之下不该有任何 interaction 被提交"
    );
}
