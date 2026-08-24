//! 主链路：一次请求从进到出。
//!
//! ```text
//! P1 解析 URL      resolve_target()        —— 只读原始字节（宪法 C35）
//! P2 读请求体      ingest()                —— 全量 buffer，8 MiB 上限
//! P3 选观测器      provisional()           —— 前三级，响应头还不存在
//! P4 发上游        UpstreamClient::send()  —— header 变换 + 判定链 + 连接池
//! P5 看 status     commit_or_fail()        —— F16 的重试边界就在这一刀
//! P6 定观测器      resolve_observer()      —— 第四级：按 Content-Type 收窄
//! P7 挂 tee        Cancellable<TeeBody<_>> —— 旁路从这里开始
//! P8 返回          原样交给 hyper server   —— 主链路上没有第二次拷贝
//! ```
//!
//! **P7 之后主链路不再等任何人**。Observer 是 spawn 出去的，客户端断开时
//! 主链路的 future 立刻被 drop，而旁路照样把已经收到的部分收尾提交
//! （`docs/06` §8.3）。
//!
//! 设计依据：`docs/06-data-plane.md` §5–§9、`docs/10-streaming.md` §4。

use std::sync::Arc;

use argus_core::{ObserverKind, ProxyError, ResolvedTarget, TraceContext};
use argus_protocol::{accumulator::RequestMeta, sink::InteractionSink};
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use compact_str::CompactString;
use http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE};
use sha2::{Digest, Sha256};
use tokio::time::{Duration, Instant, timeout};

use super::{ProfileRegistry, REQUEST_ID_HEADER, error::render, resolve_target};
use crate::{
    body::{CancelHandle, Cancellable},
    observer::{
        Observer, ObserverConfig, ObserverRules, RequestFacts, ResponseFacts, provisional,
        resolve_observer,
    },
    tee::{TeeBody, TeeHandle},
    upstream::{UpstreamClient, headers::hop_by_hop},
};

/// 主链路的旋钮。配置文件是 F11 的活，这里先给一组硬编码的默认值。
#[derive(Copy, Clone, Debug)]
pub struct ProxyConfig {
    /// 请求体上限。超过直接 413（`docs/06` §6.2）
    pub max_request_bytes: usize,
    /// 读完请求体的整体超时
    pub ingest_timeout: Duration,
    pub tee_channel_capacity: usize,
    pub tee_byte_budget: u64,
    pub observer: ObserverConfig,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: 8 * 1024 * 1024,
            ingest_timeout: Duration::from_secs(30),
            tee_channel_capacity: crate::tee::DEFAULT_CHANNEL_CAPACITY,
            tee_byte_budget: crate::tee::DEFAULT_BYTE_BUDGET,
            observer: ObserverConfig::default(),
        }
    }
}

/// 组装好的数据面。
#[derive(Clone)]
pub struct ProxyState {
    pub profiles: Arc<dyn ProfileRegistry + Send + Sync>,
    pub client: UpstreamClient,
    pub sink: Arc<dyn InteractionSink>,
    pub rules: Arc<ObserverRules>,
    pub cfg: ProxyConfig,
    /// 进程级停机令牌。每条流从它派生一个子句柄
    pub shutdown: CancelHandle,
}

/// 路由表只有三条，其余全部落到代理 handler。
///
/// 用 `fallback` 而不是给 `/provider/{name}` 注册一条 axum 路由：axum 的
/// 路径参数会做百分号解码，`%2F` 会变成 `/`，段数都变了（宪法 C35）。
/// 拿到整条原始 path 自己切是唯一安全的做法。
pub fn router(state: ProxyState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .fallback(proxy)
        .with_state(state)
}

async fn proxy(State(state): State<ProxyState>, req: Request) -> Response {
    let started = Instant::now();
    let trace = TraceContext::root();
    let (parts, body) = req.into_parts();

    // 观测器要到 P6 才定得下来，但错误可能在那之前就发生 —— 先拿一个
    // 尽力而为的形状，免得 400 用 Anthropic 的壳、500 用 OpenAI 的壳
    let mut shape = ObserverKind::GenericJson;

    match run(&state, &parts, body, &trace, started, &mut shape).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(
                error_code = e.error_code(),
                status = e.http_status().as_u16(),
                "请求失败"
            );
            render(&e, shape, &trace).map(Body::new).into_response()
        }
    }
}

async fn run(
    state: &ProxyState,
    parts: &http::request::Parts,
    body: Body,
    trace: &TraceContext,
    started: Instant,
    shape: &mut ObserverKind,
) -> Result<Response, ProxyError> {
    // ── P1：URL → target ────────────────────────────────────────────
    // 放在读 body 之前：路径不对的话没必要先吃掉 8 MiB
    let target = resolve_target(parts, state.profiles.as_ref())?;
    *shape = target.observer;

    // ── P2：全量 buffer 请求体 ──────────────────────────────────────
    let raw_request = ingest(body, &state.cfg).await?;

    // ── P3：观测器前三级 ────────────────────────────────────────────
    let facts = RequestFacts {
        method: &parts.method,
        content_type: mime_of(&parts.headers),
    };
    let provisional = provisional(&target, &state.rules, &facts);
    if let Some(kind) = provisional {
        *shape = kind;
    }

    // ── P4：发上游 ──────────────────────────────────────────────────
    let sent = state
        .client
        .send(&target, &parts.method, &parts.headers, raw_request.clone())
        .await?;
    let queued_micros = micros_since(started);
    let (resp_parts, incoming) = sent.response.into_parts();

    // ── P5：status 检查钩子 ─────────────────────────────────────────
    // F16 的 S2 重试边界就在这一刀：header 已经到手、client header 还没写出去，
    // 是**最后一个**还能换上游重来的时刻。本切片只需要这个结构
    commit_or_fail(resp_parts.status)?;

    // ── P6：观测器第四级 ────────────────────────────────────────────
    let observer = resolve_observer(provisional, resp_parts.headers.get(CONTENT_TYPE));
    *shape = observer;
    let streaming = mime_of(&resp_parts.headers) == Some("text/event-stream");

    // ── P7：挂 tee，旁路开工 ────────────────────────────────────────
    let (tee, rx) = TeeHandle::new(state.cfg.tee_channel_capacity, state.cfg.tee_byte_budget);
    let report = tee.report();
    Observer::spawn(
        state.cfg.observer,
        rx,
        request_meta(trace, &target, observer, streaming, &raw_request),
        ResponseFacts {
            status: resp_parts.status.as_u16(),
            request_started: started,
            queued_micros,
            upstream_header_micros: sent.header_micros,
        },
        Arc::clone(&state.sink),
    );

    // ── P8：原样返回 ────────────────────────────────────────────────
    let cancel = state.shutdown.child(argus_core::CancelReason::Shutdown);
    let piped = Cancellable::new(
        TeeBody::new(incoming, tee),
        cancel,
        report,
        target.timeouts.idle_frame,
        target.timeouts.total,
    );

    let mut out = Response::new(Body::new(piped));
    *out.status_mut() = resp_parts.status;
    *out.headers_mut() = downstream_headers(&resp_parts.headers, trace);
    Ok(out)
}

/// 全量 buffer 请求体。
///
/// 为什么不流式转发（`docs/06` §6.2）：PreflightGuard 要看完整 prompt、
/// 重试要能重放 body、Observer 要记完整 raw request、去重要算 body hash ——
/// 四条需求没一条流式能满足。代价是 AI 请求体通常 1 KB–200 KB，很小。
async fn ingest(body: Body, cfg: &ProxyConfig) -> Result<Bytes, ProxyError> {
    timeout(
        cfg.ingest_timeout,
        axum::body::to_bytes(body, cfg.max_request_bytes),
    )
    .await
    .map_err(|_| ProxyError::UpstreamTimeout {
        stage: argus_core::UpstreamStage::Local,
    })?
    .map_err(|_| ProxyError::PayloadTooLarge {
        limit_bytes: cfg.max_request_bytes as u64,
    })
}

/// R12 的钩子：拿到 status 之后、写出 client header 之前的那一刀。
///
/// 本切片的判定就是「2xx 就提交」。F16 在这里插入 S2 重试判定 ——
/// 之所以必须是这个位置：header 已经到手（能看 429 / `Retry-After`），
/// 而 client header 还没写出去，是最后一个能换上游重来的时刻。
/// 一旦写出去，HTTP 就不允许改 status 了（`docs/06` §7.1）。
#[expect(
    clippy::unnecessary_wraps,
    reason = "返回 Result 就是这个钩子的全部内容：F16 在这里插入 S2 重试判定"
)]
fn commit_or_fail(status: StatusCode) -> Result<(), ProxyError> {
    // 非 2xx 也原样透传给客户端 —— Argus 是透明网关，上游说 429 就是 429，
    // 不翻译、不吞掉。这里只是给 F16 留位置
    let _ = status;
    Ok(())
}

fn request_meta(
    trace: &TraceContext,
    target: &ResolvedTarget,
    observer: ObserverKind,
    streaming: bool,
    raw_request: &Bytes,
) -> RequestMeta {
    let keeps_raw = target.observe.keeps_raw();
    RequestMeta {
        trace: trace.clone(),
        observer,
        observe_mode: target.observe,
        stream: streaming,
        observed_model: observed_model(raw_request),
        request_sha256: Some(Sha256::digest(raw_request).into()),
        raw_request: keeps_raw.then(|| raw_request.clone()),
    }
}

/// 从请求体里**观测**模型名。
///
/// ⚠️ 这个值**只进审计与统计**，绝不参与选路（宪法 C33、ADR-005）。
/// 模型目录不存在，`model` 对 Data Plane 是不透明数据，原样待在 body 里
/// 发给上游 —— 这里读它，就像读一个日志字段。
fn observed_model(body: &[u8]) -> Option<CompactString> {
    #[derive(serde::Deserialize)]
    struct ModelOnly<'a> {
        #[serde(borrow)]
        model: Option<&'a str>,
    }
    serde_json::from_slice::<ModelOnly<'_>>(body)
        .ok()?
        .model
        .map(CompactString::new)
}

/// 上游 header → 客户端 header。剥掉 hop-by-hop，补上 request id。
fn downstream_headers(upstream: &HeaderMap, trace: &TraceContext) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(upstream.len() + 1);
    for (name, value) in upstream {
        if hop_by_hop(name) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Ok(id) = HeaderValue::from_str(&trace.request_id.to_string()) {
        out.insert(REQUEST_ID_HEADER, id);
    }
    out
}

/// `application/json; charset=utf-8` → `application/json`
fn mime_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(CONTENT_TYPE)?.to_str().ok()?.split(';').next().map(str::trim)
}

fn micros_since(start: Instant) -> u32 {
    u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX)
}
