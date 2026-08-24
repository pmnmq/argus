//! Observer 选择：决定「这条流该用哪套语义去理解」。
//!
//! **Observer 与路由无关。** 路由已经在 P1 由 URL 决定完了
//! （`docs/02-routing-model.md`）；这里回答的是另一个问题。把两件事绑在
//! 一起，第一个提供多种 endpoint 形状的供应商就会让它崩掉 ——
//! `openai-prod` 同时有 `/v1/chat/completions`、`/v1/responses`、
//! `/v1/embeddings`，三种形状（`docs/09-observers.md` §4.3）。
//!
//! 判定分两步，因为**第四级要等响应 header 才能定**（§5）：
//!
//! ```text
//! 请求时   provisional()      规则 → Profile 默认值 → Raw 按 host/path 猜
//! 响应头后 resolve_observer() 定不下来就按 Content-Type 兜底；定下来了只准收窄
//! ```
//!
//! 选错了不会怎样：后果是**观测数据降级**，主链路一个字节都不受影响 ——
//! 它根本不看 Observer 的输出（§4.4）。

pub mod panic;
pub mod rules;

use argus_core::{ObserverKind, ResolvedTarget};
use http::HeaderValue;

pub use self::rules::{ObserverRule, ObserverRules, RequestFacts, SelectCtx};

/// Raw 模式的形状猜测表。
///
/// **这是便利，不是契约。** 猜错只影响观测，而且第四级还会按 Content-Type
/// 兜底。所以它可以很短，也不必跟着供应商更新 —— 想要精确控制就写
/// `observer_rules`。
///
/// path 前缀是比 host 强得多的信号：`/v1/messages` 这个形状会出现在
/// `api.anthropic.com`、Bedrock、Vertex，以及任何 anthropic-compatible 的
/// 自建服务上（§4.3）。所以这里只按 path 猜，不按 host。
const RAW_SHAPE_HINTS: &[(&str, ObserverKind)] = &[
    ("/v1/chat/completions", ObserverKind::OpenAiChat),
    ("/v1/responses", ObserverKind::OpenAiResponses),
    ("/v1/messages", ObserverKind::AnthropicMessages),
    // OpenAI 兼容网关常见的前缀写法
    ("/openai/v1/chat/completions", ObserverKind::OpenAiChat),
    (
        "/compatible-mode/v1/chat/completions",
        ObserverKind::OpenAiChat,
    ),
];

/// 落到 Generic 的原因。进 `argus_observer_fallback_total{reason}`。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FallbackReason {
    /// 前三级都没定下来，只能看 Content-Type
    UnknownShape,
    /// 定下来了，但响应的 Content-Type 说这不是它能解的东西
    ContentTypeMismatch,
    /// 连 Content-Type 都没有
    NoContentType,
}

impl FallbackReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownShape => "unknown_shape",
            Self::ContentTypeMismatch => "content_type_mismatch",
            Self::NoContentType => "no_content_type",
        }
    }
}

/// 请求时能定下来的前三级。返回 `None` 表示「等响应 header 再说」。
///
/// 顺序即 `docs/09` §4.2 的流程图：规则 → Managed 用 Profile 默认值 →
/// Raw 按 path 猜。
#[must_use]
pub fn provisional(
    target: &ResolvedTarget,
    rules: &ObserverRules,
    req: &RequestFacts<'_>,
) -> Option<ObserverKind> {
    let ctx = SelectCtx::new(target, req);
    if let Some(kind) = rules.select(&ctx) {
        return Some(kind);
    }
    if target.origin.is_raw() {
        // Raw 模式没有 Profile 可依，只能看形状。注意**不能**用
        // `target.observer` —— R02 给 Raw 填的是 `GenericJson` 占位值，
        // 拿它当"Profile 默认值"会让第四级的 Content-Type 兜底永远轮不到
        return guess_raw_shape(ctx.path);
    }
    // Managed：Profile 上的 `observer` 只是默认值，`observer_rules` 才是
    // 精确控制（§4.3）。走到这里说明没有规则命中
    Some(target.observer)
}

/// Raw 模式按 path 前缀猜形状。猜不出返回 `None`。
#[must_use]
pub fn guess_raw_shape(path: &str) -> Option<ObserverKind> {
    RAW_SHAPE_HINTS
        .iter()
        .find(|(prefix, _)| path.starts_with(prefix))
        .map(|&(_, kind)| kind)
}

/// 收到响应 header 之后的最终判定。
///
/// 两种情况：
///
/// - `provisional` 是 `None` —— 前三级没定下来，按 Content-Type 兜底
///   （`text/event-stream` → `GenericSse`，其余 → `GenericJson`）。
/// - `provisional` 是 `Some(k)` —— **只准收窄**，不准换成另一个语义
///   Observer。换掉会让已经解析出来的事件与后面的不一致（§5）。
///   只有在 Content-Type 说明 `k` 根本解不了时才收窄到 Generic。
///
/// 流程图里的「其他 Content-Type → 只记元数据」由 `GenericJson` 实现：
/// 它在非 JSON 上退化成 `JsonShape::Malformed`，留下的正好只有字节数与
/// 形状，没有 key、更没有 value。
#[must_use]
pub fn resolve_observer(
    provisional: Option<ObserverKind>,
    resp_content_type: Option<&HeaderValue>,
) -> ObserverKind {
    let ct = resp_content_type.and_then(|v| v.to_str().ok());
    let kind = match provisional {
        None => {
            let reason = if ct.is_some() {
                FallbackReason::UnknownShape
            } else {
                FallbackReason::NoContentType
            };
            record_fallback(reason);
            generic_for(ct)
        }
        Some(kind) if kind.is_semantic() && !can_carry(ct) => {
            record_fallback(FallbackReason::ContentTypeMismatch);
            generic_for(ct)
        }
        Some(kind) => kind,
    };
    metrics::counter!("argus_observer_selected_total", "kind" => kind.as_str()).increment(1);
    kind
}

/// 这个 Content-Type 能不能承载一个语义 Observer 的输入。
///
/// 缺失时按**能**处理：不少上游在流式响应上不发 `Content-Type`，而按
/// 「没有就降级」处理会把正常的流一起降掉。
fn can_carry(content_type: Option<&str>) -> bool {
    content_type.is_none_or(|ct| is_sse(ct) || is_json(ct))
}

fn generic_for(content_type: Option<&str>) -> ObserverKind {
    if content_type.is_some_and(is_sse) {
        ObserverKind::GenericSse
    } else {
        ObserverKind::GenericJson
    }
}

/// `text/event-stream; charset=utf-8` 也算。
fn is_sse(content_type: &str) -> bool {
    mime_type(content_type).eq_ignore_ascii_case("text/event-stream")
}

/// `application/json` 与 `application/*+json`（如 `application/vnd.x+json`）。
fn is_json(content_type: &str) -> bool {
    let mime = mime_type(content_type);
    mime.eq_ignore_ascii_case("application/json") || mime.to_ascii_lowercase().ends_with("+json")
}

/// 砍掉 `; charset=…` 之类的参数并去空白。
fn mime_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or(content_type).trim()
}

fn record_fallback(reason: FallbackReason) {
    metrics::counter!("argus_observer_fallback_total", "reason" => reason.as_str()).increment(1);
}
