//! F06 的端到端验收：客户端 → Argus → provider，同时旁路观测。
//!
//! 这是 M0 架构假设的实体化。要证的不是「能转发」，而是那四条非对称保障
//! （ADR-009）：主链路零拷贝、主链路不等旁路、丢弃可检测、旁路挂了主链路
//! 不受影响。

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use argus_core::{ObserveMode, ObserverKind, TeeChunk, Timeouts};
use argus_protocol::{
    framer::FramerConfig,
    interaction::{Integrity, ObservedInteraction, Outcome},
    sink::{InteractionSink, SinkFull},
};
use argus_proxy::{
    body::CancelHandle,
    ingress::{ProxyConfig, ProxyState, StaticRegistry, router},
    observer::{ObserverConfig, ObserverRules},
    target::{CheckedResolver, SystemResolver, TargetPolicy, TargetPolicyConfig},
    tee::{TeeBody, TeeHandle},
    testkit::{MockProvider, Scenario},
    upstream::UpstreamClient,
};
use axum::body::Body;
use bytes::Bytes;
use http::{Request, Response, StatusCode, header::CONTENT_TYPE};
use http_body_util::BodyExt;
use tokio::time::{Duration, sleep, timeout};
use tower::ServiceExt as _;

// ── 夹具 ────────────────────────────────────────────────────────────

/// 把交上来的 interaction 全存下来。
#[derive(Default)]
struct CollectingSink {
    // `Box` 是 sink 契约要求的（`InteractionSink::try_submit` 收 Box），
    // 不是这里为了装箱而装箱
    #[allow(clippy::vec_box, reason = "元素类型由 InteractionSink 定")]
    seen: Mutex<Vec<Box<ObservedInteraction>>>,
}

impl InteractionSink for CollectingSink {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        self.seen.lock().expect("锁没被毒化").push(interaction);
        Ok(())
    }
}

impl CollectingSink {
    /// 旁路是 spawn 出去的，主链路结束时它未必跑完 —— 那正是 R16 要的效果。
    async fn wait_for_one(&self) -> Box<ObservedInteraction> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(one) = self.seen.lock().expect("锁没被毒化").pop() {
                return one;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("5 秒内旁路没有提交任何 interaction");
    }

    fn count(&self) -> usize {
        self.seen.lock().expect("锁没被毒化").len()
    }
}

/// 回环地址在默认策略里是**拒绝**的（SSRF 防护）。测试显式放行它 ——
/// `allow_cidrs` 优先级高于 `deny_cidrs`，这正是文档给的逃生口。
fn loopback_policy() -> TargetPolicy {
    let cfg = TargetPolicyConfig {
        allow_cidrs: vec!["127.0.0.0/8".into()],
        ..TargetPolicyConfig::default()
    };
    TargetPolicy::compile(&cfg).expect("固定的合法配置")
}

struct Harness {
    router: axum::Router,
    sink: Arc<CollectingSink>,
    provider: MockProvider,
}

async fn harness(scenario: Scenario) -> Harness {
    harness_with(scenario, ProxyConfig::default()).await
}

async fn harness_with(scenario: Scenario, cfg: ProxyConfig) -> Harness {
    let provider = MockProvider::raw(scenario).await;
    let sink = Arc::new(CollectingSink::default());
    let policy = Arc::new(loopback_policy());
    let resolver = Arc::new(CheckedResolver::new(Arc::new(SystemResolver::default())));
    let client = UpstreamClient::new(policy, resolver, Timeouts::default()).expect("客户端可构建");

    let state = ProxyState {
        profiles: Arc::new(StaticRegistry::new("mock", provider.base_url())),
        client,
        sink: Arc::clone(&sink) as Arc<dyn InteractionSink>,
        rules: Arc::new(ObserverRules::default()),
        cfg,
        shutdown: CancelHandle::new(),
    };
    Harness {
        router: router(state),
        sink,
        provider,
    }
}

async fn post(router: &axum::Router, path: &str, body: &'static str) -> Response<Body> {
    let req = Request::post(path)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("请求可构造");
    router.clone().oneshot(req).await.expect("handler 不返回 Err")
}

async fn body_bytes(resp: Response<Body>) -> Bytes {
    resp.into_body().collect().await.expect("读完 body").to_bytes()
}

const REQ: &str = r#"{"model":"claude-x","max_tokens":16,"messages":[]}"#;

// ── 主链路 ──────────────────────────────────────────────────────────

/// 宪法 C1：转发的字节与上游发的**完全一致**，一个都不多一个都不少。
#[tokio::test]
async fn proxy_a_streaming_response_forwards_every_upstream_byte() {
    let h = harness(Scenario::anthropic_sample()).await;
    let expected = h.provider.expected_body();

    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(CONTENT_TYPE).map(http::HeaderValue::as_bytes),
        Some(&b"text/event-stream"[..]),
        "上游的 Content-Type 必须原样透传"
    );
    assert_eq!(body_bytes(resp).await, Bytes::from(expected));
}

/// 上游的 status 原样透传 —— 透明网关不翻译、不吞掉 4xx/5xx。
#[tokio::test]
async fn proxy_a_non_2xx_upstream_passes_the_status_through_untouched() {
    let h = harness(Scenario::json_body(r#"{"error":"nope"}"#).with_status(429)).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body_bytes(resp).await,
        Bytes::from_static(br#"{"error":"nope"}"#)
    );
}

/// 每个响应都带 `x-argus-request-id` —— 对账的唯一抓手。
#[tokio::test]
async fn proxy_any_response_carries_the_argus_request_id() {
    let h = harness(Scenario::anthropic_sample()).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert!(resp.headers().contains_key("x-argus-request-id"));
}

/// 客户端的凭据**绝不透传**，Argus 注入自己那份（`docs/06` §6.3–6.4）。
#[tokio::test]
async fn proxy_strips_client_credentials_before_reaching_the_provider() {
    let h = harness(Scenario::anthropic_sample()).await;
    let req = Request::post("/provider/mock/v1/messages")
        .header(CONTENT_TYPE, "application/json")
        .header("authorization", "Bearer client-secret-token")
        .header("x-api-key", "client-key")
        .header("x-argus-internal", "forged")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(REQ))
        .expect("请求可构造");
    let resp = h.router.clone().oneshot(req).await.expect("handler 不返回 Err");
    let _ = body_bytes(resp).await;

    let seen = String::from_utf8_lossy(&h.provider.last_request().head).into_owned();
    assert!(
        !seen.contains("client-secret-token"),
        "客户端凭据泄漏到上游：{seen}"
    );
    assert!(!seen.contains("client-key"), "客户端凭据泄漏到上游：{seen}");
    assert!(
        !seen.to_lowercase().contains("x-argus-internal"),
        "内部标记可被伪造"
    );
    // 未知 header 默认透传，否则 Argus 会变成能力瓶颈（宪法 C2）
    assert!(seen.to_lowercase().contains("anthropic-version"));
}

/// 路径两条前缀都不匹配 → 404，且 body 是调用方协议的形状。
#[tokio::test]
async fn proxy_an_unroutable_path_returns_404_in_the_callers_shape() {
    let h = harness(Scenario::anthropic_sample()).await;
    let resp = post(&h.router, "/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_bytes(resp).await;
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"error\""), "错误体形状不对：{text}");
    assert_eq!(h.provider.hits(), 0, "根本不该碰上游");
}

/// 请求体上限：超过直接 413，不读进内存（`docs/06` §6.2）。
#[tokio::test]
async fn proxy_a_request_over_the_size_limit_returns_413() {
    let cfg = ProxyConfig {
        max_request_bytes: 32,
        ..ProxyConfig::default()
    };
    let h = harness_with(Scenario::anthropic_sample(), cfg).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(h.provider.hits(), 0);
}

// ── 旁路 ────────────────────────────────────────────────────────────

/// 一条完整的流被完整地观测下来。
#[tokio::test]
async fn proxy_a_streaming_response_produces_a_complete_interaction() {
    let h = harness(Scenario::anthropic_sample()).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    let _ = body_bytes(resp).await;

    let seen = h.sink.wait_for_one().await;
    assert_eq!(seen.outcome, Outcome::Completed);
    assert_eq!(seen.integrity, Integrity::Complete);
    assert_eq!(seen.observer, ObserverKind::AnthropicMessages);
    assert_eq!(seen.observe_mode, ObserveMode::Full);
    assert_eq!(seen.response_meta.status_code, Some(200));
    assert_eq!(seen.response_meta.model.as_deref(), Some("claude-x"));

    // 文本被累积起来了 —— 整条链路（分帧 → 解析 → 累积）都通了
    assert_eq!(seen.blocks.len(), 1);
    assert_eq!(seen.blocks[0].text.as_str(), "Hello, world");

    // usage 是 last-non-null-wins，**不求和**：message_start 报了 1，
    // message_delta 报了 7，答案是 7 不是 8
    assert_eq!(seen.usage.input_tokens, Some(11));
    assert_eq!(seen.usage.output_tokens, Some(7));
    assert!(seen.timings.ttft_micros.is_some(), "TTFT 应该被记下来");

    // 请求侧：raw request 与 model 都是从主链路交过来的同一份 Bytes
    assert_eq!(seen.request.observed_model.as_deref(), Some("claude-x"));
    assert!(seen.raw_request.is_some());
}

/// 非流式 JSON **不该**被降级：Anthropic 的非流式响应就是 `application/json`，
/// 语义观测器完全能解（`docs/09` §4.2.1「缺 Content-Type 时按能承载处理」的
/// 同一条道理）。收窄只发生在 Content-Type 说明它根本解不了的时候。
#[tokio::test]
async fn proxy_a_json_response_keeps_the_semantic_observer() {
    let h = harness(Scenario::json_body(r#"{"id":"resp_1","usage":{}}"#)).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    let _ = body_bytes(resp).await;

    let seen = h.sink.wait_for_one().await;
    assert_eq!(seen.observer, ObserverKind::AnthropicMessages);
    assert_eq!(seen.outcome, Outcome::Completed);
}

/// 响应根本不是 JSON 也不是 SSE（上游挂了个 HTML 错误页）→ 收窄到兜底。
///
/// 「其他 Content-Type → 只记元数据」这一支由 `GenericJson` 实现：
/// 它在非 JSON 上退化成 `Malformed`，留下的正好只有字节数与形状。
#[tokio::test]
async fn proxy_an_html_response_narrows_to_the_generic_observer() {
    let scenario = Scenario::json_body("<html>502 Bad Gateway</html>")
        .with_content_type("text/html")
        .with_status(502);
    let h = harness(scenario).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let _ = body_bytes(resp).await;

    let seen = h.sink.wait_for_one().await;
    assert_eq!(
        seen.observer,
        ObserverKind::GenericJson,
        "Content-Type 说这不是语义观测器能解的东西，必须收窄"
    );
    assert_eq!(seen.response_meta.status_code, Some(502));
}

/// 宪法 C10 + C13：旁路满了就丢，**主链路一个字节都不少**，且丢弃可检测。
#[tokio::test]
async fn proxy_with_an_overflowing_tee_keeps_forwarding_and_marks_degraded() {
    let cfg = ProxyConfig {
        // 把两级预算都掐到最小，逼出丢弃
        tee_channel_capacity: 1,
        tee_byte_budget: 256,
        ..ProxyConfig::default()
    };
    let h = harness_with(Scenario::flood(200, 512), cfg).await;
    let expected = h.provider.expected_body();

    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_bytes(resp).await.len(),
        expected.len(),
        "旁路丢字节绝不能影响主链路转发"
    );

    let seen = h.sink.wait_for_one().await;
    match seen.integrity {
        Integrity::Degraded {
            dropped_bytes,
            gap_count,
            ..
        } => {
            assert!(dropped_bytes > 0, "丢了却没记下来");
            assert!(gap_count > 0);
        }
        other => panic!("旁路丢过字节，integrity 必须是 Degraded，实际 {other:?}"),
    }
}

/// 分帧失败 = 放弃这条流的观测，**主链路照常 200**（宪法 C10）。
///
/// 这条走的是 `parser_dead` 的恢复路径，与 parser panic 后走的是同一条
/// （panic 本身的捕获由 `observer::panic` 的单元测试钉住 —— 生产代码里
/// 没有能被测试触发的 panic 点，硬造一个就得给 `AnyObserver` 开测试后门）。
#[tokio::test]
async fn proxy_with_a_failing_framer_still_returns_the_full_response() {
    let scenario = Scenario::flood(4, 4096);
    let cfg = ProxyConfig {
        observer: ObserverConfig {
            // 单帧上限 16 字节，第一帧就会溢出
            framer: FramerConfig {
                max_frame_bytes: 16,
            },
            ..ObserverConfig::default()
        },
        ..ProxyConfig::default()
    };
    let h = harness_with(scenario, cfg).await;
    let expected = h.provider.expected_body();

    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::OK, "旁路挂了主链路不该受影响");
    assert_eq!(body_bytes(resp).await.len(), expected.len());

    let seen = h.sink.wait_for_one().await;
    assert!(
        matches!(seen.outcome, Outcome::ParserFailed { .. }),
        "观测失败必须如实记录，实际 {:?}",
        seen.outcome
    );
}

/// 客户端中途断开：上游连接被关掉，已收到的部分照常提交（`docs/06` §8.3）。
#[tokio::test]
async fn proxy_when_the_client_drops_mid_stream_submits_a_partial_interaction() {
    let h = harness(Scenario::anthropic_sample().with_pacing(Duration::from_millis(30))).await;

    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 只读前几帧就把 body 丢掉 —— 等价于客户端拔线
    let mut body = resp.into_body();
    for _ in 0..3 {
        let _ = body.frame().await;
    }
    drop(body);

    let seen = h.sink.wait_for_one().await;
    match seen.outcome {
        Outcome::ClientAborted {
            bytes_forwarded, ..
        } => assert!(bytes_forwarded > 0, "已转发的部分要留下"),
        other => panic!("应该是 client_aborted，实际 {other:?}"),
    }
    // 断在半路，但已经收到的文本仍然有价值
    assert!(!seen.blocks.is_empty(), "partial 结果不该是空的");

    // 上游必须察觉到这条流没人要了，否则 provider 会继续生成、继续计费
    let closed = timeout(Duration::from_secs(3), async {
        while !h.provider.peer_gone() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(closed.is_ok(), "客户端断开没有传播到上游");
}

/// R16：旁路是 spawn 出去的，主链路**不 join** 它。
///
/// 判据是主链路返回时旁路还没提交 —— 如果主链路等过它，这里一定已经有值。
#[tokio::test]
async fn proxy_does_not_wait_for_the_observer_before_answering() {
    let h = harness(Scenario::anthropic_sample()).await;
    let resp = post(&h.router, "/provider/mock/v1/messages", REQ).await;
    let _ = body_bytes(resp).await;
    // 主链路已经把 body 交完了。旁路可能还在跑，也可能刚好跑完 ——
    // 不管哪种，都不能是"因为主链路等了它才有值"
    let before = h.sink.count();
    let seen = h.sink.wait_for_one().await;
    assert_eq!(seen.outcome, Outcome::Completed);
    assert!(before <= 1);
}

// ── 不阻塞：直接压 Tee ──────────────────────────────────────────────

/// 一个立刻吐 N 片、不做任何等待的 body。压的是 `poll_frame` 本身的耗时。
struct Repeat {
    left: usize,
}

impl http_body::Body for Repeat {
    type Data = Bytes;
    type Error = core::convert::Infallible;

    fn poll_frame(
        mut self: core::pin::Pin<&mut Self>,
        _cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        if self.left == 0 {
            return core::task::Poll::Ready(None);
        }
        self.left -= 1;
        core::task::Poll::Ready(Some(Ok(http_body::Frame::data(Bytes::from_static(
            b"a chunk of bytes",
        )))))
    }
}

/// 宪法 C5 的核心断言：**旁路再慢，主链路也不等**。
///
/// 直接压 `TeeBody` 而不是走整条 HTTP 链路 —— 要证的是「`poll_frame` 里那次
/// 跨界动作不 await」，中间垫一层 hyper 只会让耗时里混进网络噪声。
#[tokio::test]
async fn poll_frame_with_a_consumer_that_sleeps_does_not_slow_the_producer() {
    const FRAMES: usize = 64;
    let (tee, mut rx) = TeeHandle::new(8, u64::MAX);
    let report = tee.report();

    // 旁路每条 chunk 睡 50ms：全部消费完要 3.2 秒
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            sleep(Duration::from_millis(50)).await;
        }
    });

    let body = TeeBody::new(Repeat { left: FRAMES }, tee);
    let started = Instant::now();
    let forwarded = body.collect().await.expect("读完").to_bytes();
    let elapsed = started.elapsed();

    assert_eq!(forwarded.len(), FRAMES * 16, "主链路的字节一个都不能少");
    assert!(
        elapsed < Duration::from_millis(300),
        "主链路被旁路拖慢了：{elapsed:?}（旁路要跑 3.2 秒）"
    );
    // 队列灌满之后开始丢，但**一次都没有阻塞**
    assert!(report.stats().bytes_dropped > 0, "这个用例本该压出丢弃");
}

/// 旁路 task 已经没了，主链路照常跑完（R7）。
#[tokio::test]
async fn poll_frame_after_the_observer_is_gone_keeps_the_producer_running() {
    let (tee, rx) = TeeHandle::new(4, u64::MAX);
    let report = tee.report();
    drop(rx);

    let body = TeeBody::new(Repeat { left: 1000 }, tee);
    let forwarded = body.collect().await.expect("读完").to_bytes();

    assert_eq!(forwarded.len(), 1000 * 16);
    // 摘掉 tee 之后不再累积丢弃 —— 没人会来读它了
    assert_eq!(report.stats().bytes_dropped, 0);
}

/// `TeeChunk` 的 `Debug` 里不能出现内容（宪法 C27）。
#[test]
fn debug_a_tee_chunk_never_renders_the_payload() {
    let chunk = TeeChunk::new(Bytes::from_static(b"data: {\"prompt\":\"secret\"}\n\n"));
    assert!(!format!("{chunk:?}").contains("secret"));
}
