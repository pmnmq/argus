//! 错误渲染：**用调用方协议的形状**回错误（`docs/06-data-plane.md` §9）。
//!
//! 为什么不能统一成一种形状：客户端用的是官方 SDK，SDK 按自己协议的错误
//! 结构去解析。回一个形状不对的 JSON，SDK 抛出的是「无法解析响应」这类
//! 与真实原因毫无关系的异常 —— 排查会从「为什么 429」变成「SDK 坏了吗」。
//!
//! 两条铁律：
//!
//! 1. body 里**不含**上游凭据、内部栈信息、IP、策略细节。
//!    尤其是 `TargetDenied`：区分「被策略拒」和「连不上」就等于把 Argus
//!    变成一台内网存活探测器（`docs/13` §8）。
//! 2. 所有错误响应都带 `x-argus-request-id`，这是对账的唯一抓手。

use argus_core::{ObserverKind, ProxyError, TraceContext};
use http::{HeaderValue, Response, StatusCode, header::CONTENT_TYPE};
use serde_json::json;

use super::REQUEST_ID_HEADER;

/// `ProxyError` → 客户端能看懂的响应。
///
/// `observer` 决定形状；认不出协议（两个 Generic）时用 `OpenAI` 的形状 ——
/// 它是事实上的通用格式，绝大多数兼容客户端都认。
#[must_use]
pub fn render(err: &ProxyError, observer: ObserverKind, trace: &TraceContext) -> Response<String> {
    let status = err.http_status();
    let body = match observer {
        ObserverKind::AnthropicMessages => anthropic_shape(err, status),
        _ => openai_shape(err, status),
    };
    let mut resp = Response::builder()
        .status(status)
        .body(body.to_string())
        .unwrap_or_else(|_| fallback(status));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(id) = HeaderValue::from_str(&trace.request_id.to_string()) {
        resp.headers_mut().insert(REQUEST_ID_HEADER, id);
    }
    if let Some(after) = err.retry_after()
        && let Ok(v) = HeaderValue::from_str(&after.as_secs().to_string())
    {
        resp.headers_mut().insert(http::header::RETRY_AFTER, v);
    }
    resp
}

/// `{"type":"error","error":{"type","message"}}`
fn anthropic_shape(err: &ProxyError, status: StatusCode) -> serde_json::Value {
    json!({
        "type": "error",
        "error": { "type": err.error_code(), "message": message(err, status) },
    })
}

/// `{"error":{"message","type","param","code"}}`
fn openai_shape(err: &ProxyError, status: StatusCode) -> serde_json::Value {
    json!({
        "error": {
            "message": message(err, status),
            "type": err.error_code(),
            "param": serde_json::Value::Null,
            "code": err.error_code(),
        },
    })
}

/// 面向客户端的文字。
///
/// 5xx 一律用固定短语：`ProxyError` 的 `Display` 里可能带着上游的错误描述，
/// 那是给运维看的，不是给调用方看的。4xx 是调用方自己能改的东西，说清楚
/// 反而有用。
fn message(err: &ProxyError, status: StatusCode) -> String {
    if status.is_server_error() {
        status.canonical_reason().unwrap_or("upstream error").to_owned()
    } else {
        err.to_string()
    }
}

/// `Response::builder` 只会因为 status 非法而失败，而 status 来自
/// `http_status()`，所以这条分支实际不可达。留着是因为 unwrap 不行。
fn fallback(status: StatusCode) -> Response<String> {
    let mut resp = Response::new(String::from(r#"{"error":{"message":"internal error"}}"#));
    *resp.status_mut() = status;
    resp
}
