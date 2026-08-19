//! 选路结果与它携带的策略：`RouteDecision` / `Timeouts` / `RetryPolicy` / `ObserveMode`。
//!
//! 本模块只定义**形状**。填充逻辑在 `argus-proxy::router`（F14）。
//!
//! 设计依据：`docs/02-data-plane.md` §10、`docs/07-provider-routing.md` §7.2、
//! `docs/05-streaming.md` §6。

use core::time::Duration;
use std::sync::Arc;

use compact_str::CompactString;
use http::Uri;
use serde::{Deserialize, Serialize};

use crate::ids::{BindingId, EndpointId, ProviderId};
use crate::protocol::{Protocol, ProviderKind};
use crate::provider::ResolvedCredential;

/// 四段式超时。
///
/// 分成四个而不是一个，是因为 AI 流式请求的形状很特别：**首 token 慢、
/// token 之间快、总时长可能很长**。用单一超时要么误杀推理模型（设短了），
/// 要么卡死的连接挂满几分钟才被回收（设长了）。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Timeouts {
    /// TCP + TLS 建连
    #[serde(with = "humantime_serde")]
    pub connect: Duration,
    /// 发出请求到收到 response header。推理模型首 token 可能超过 30 秒，
    /// 设短了会大量误触发 S1 重试（而 S1 重试有重复计费风险）。
    #[serde(with = "humantime_serde")]
    pub headers: Duration,
    /// 两个 body frame 之间的最大间隔。这一条才是「流卡住了」的判据。
    #[serde(with = "humantime_serde")]
    pub idle_frame: Duration,
    /// 整个请求的硬上限
    #[serde(with = "humantime_serde")]
    pub total: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            headers: Duration::from_secs(60),
            idle_frame: Duration::from_secs(120),
            total: Duration::from_secs(900),
        }
    }
}

/// 指数退避参数。
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Backoff {
    #[serde(with = "humantime_serde")]
    pub initial: Duration,
    #[serde(with = "humantime_serde")]
    pub max: Duration,
    pub multiplier: f32,
    /// 抖动比例（0.0–1.0）。没有抖动，同时失败的请求会同步重试，
    /// 把一次故障放大成周期性的尖峰。
    pub jitter: f32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(1),
            multiplier: 2.0,
            jitter: 0.3,
        }
    }
}

/// 重试与 fallback 预算。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// 含首次尝试。3 表示最多 2 次重试。
    pub max_attempts: u8,
    /// 同一个 provider 最多试几次。默认 1 —— 在刚失败的 provider 上重试
    /// 通常只是浪费预算，应该换一个。
    pub max_attempts_per_provider: u8,
    /// 总的 fallback 时间预算，硬性。超过就直接失败，让客户端自己决定。
    #[serde(with = "humantime_serde")]
    pub budget: Duration,
    /// S1（请求已发出但没收到 header）是否重试。
    ///
    /// 默认 `true`，但这是个**有代价的**默认：provider 可能已经开始生成并计费。
    /// 每次这样的重试都必须在审计里记 `retry_after_send = true`，
    /// 让重复计费事后可核查。见 `docs/07` §7.3。
    pub on_no_response: bool,
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_attempts_per_provider: 1,
            budget: Duration::from_secs(10),
            on_no_response: true,
            backoff: Backoff::default(),
        }
    }
}

/// 这次请求的观测强度。在 P7（选路）阶段决定一次，之后整条流不再改变。
///
/// 为什么是**请求粒度**而不是逐字节丢弃：丢掉一条流中间 30% 的字节，
/// 得到的是一个残缺且不可信的 interaction —— 文本有洞、tool arguments 是坏
/// JSON、usage 可能丢失。它占着存储，却几乎没有分析价值。
/// 宁可完整地观测一部分请求。见 `docs/05` §6。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    /// 完整观测：raw request + 全部 event + 完整 accumulate
    #[default]
    Full,
    /// 轻量观测：只累积结构性信息（usage / tool call 元数据 / finish），
    /// 不保留正文与 raw event
    Lite,
    /// 完全不观测：Tee 不挂载，只记录请求级元数据
    Off,
}

impl ObserveMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lite => "lite",
            Self::Off => "off",
        }
    }

    /// 是否需要挂 Tee。`Off` 下主链路完全不产生副本。
    #[must_use]
    pub const fn needs_tee(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 是否保留原始字节（raw request / raw events）。
    #[must_use]
    pub const fn keeps_raw(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// 一次上游尝试的全部决定。每个 attempt 一个。
#[derive(Clone, Debug)]
pub struct RouteDecision {
    /// 第几次尝试，从 0 开始。
    pub attempt: u8,
    pub provider_id: ProviderId,
    pub provider_kind: ProviderKind,
    /// 具体接入点。`docs/07` §7.4 的 `AttemptRecord` 要按它归因。
    pub endpoint_id: EndpointId,
    /// 逻辑模型 → provider 模型的这条绑定，capabilities 挂在这一层。
    pub binding_id: BindingId,
    /// 完整的上游 URL。
    ///
    /// 用 `http::Uri` 而不是 `url::Url`：它是 hyper 直接消费的类型，省掉每请求
    /// 一次 parse；`Uri` 的 clone 是引用计数。URL **拼装**（Azure 的 deployment
    /// 路径、`api-version` 等）在 `argus-proxy::router` 里用 `url::Url` 做，
    /// 结果在配置解析期转换一次。
    pub endpoint_url: Uri,
    pub credential: Arc<ResolvedCredential>,
    /// 上游真实模型名
    pub provider_model: CompactString,
    /// 调用方请求的逻辑模型名
    pub logical_model: CompactString,
    /// 必须与请求的 `Protocol` 相同 —— Argus 不做跨协议转换（宪法 C3）。
    pub protocol: Protocol,
    pub timeouts: Timeouts,
    pub retry: Arc<RetryPolicy>,
    /// 采样结果，整条流固定
    pub observe: ObserveMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值直接对应 `docs/02` §10。改这些数字之前先改文档 ——
    /// 这个测试是那条纪律的执行者（宪法 C31）。
    #[test]
    fn timeouts_default_matches_docs_02_values() {
        let t = Timeouts::default();
        assert_eq!(t.connect, Duration::from_secs(3));
        assert_eq!(t.headers, Duration::from_secs(60));
        assert_eq!(t.idle_frame, Duration::from_secs(120));
        assert_eq!(t.total, Duration::from_secs(900));
    }

    /// 四段超时必须构成递增的层次，否则外层永远轮不到触发。
    #[test]
    fn timeouts_default_forms_a_consistent_hierarchy() {
        let t = Timeouts::default();
        // headers 必须给推理模型留足首 token 时间，否则会大量误触发 S1 重试
        assert!(t.headers >= Duration::from_secs(30));
        assert!(t.connect < t.headers);
        assert!(t.headers <= t.total);
        assert!(t.idle_frame < t.total);
    }

    /// 默认值直接对应 `docs/07` §7.2。
    #[test]
    fn retry_policy_default_matches_docs_07_values() {
        let r = RetryPolicy::default();
        assert_eq!(r.max_attempts, 3);
        assert_eq!(r.max_attempts_per_provider, 1);
        assert_eq!(r.budget, Duration::from_secs(10));
        assert!(r.on_no_response);
        assert_eq!(r.backoff.initial, Duration::from_millis(50));
        assert_eq!(r.backoff.max, Duration::from_secs(1));
        // 没有抖动，同时失败的请求会同步重试，把故障放大成周期性尖峰
        assert!(r.backoff.jitter > 0.0);
    }

    /// 配置文件里写的是 "3s"，不是 `{ secs: 3, nanos: 0 }`。
    #[test]
    fn deserialize_timeouts_given_humantime_strings_parses_durations() {
        let t: Timeouts = serde_json::from_str(
            r#"{"connect":"3s","headers":"1m","idle_frame":"2m","total":"15m"}"#,
        )
        .expect("humantime 格式");
        assert_eq!(t, Timeouts::default());
    }

    /// 泄漏几乎从不是「有人打印了 credential」，而是「有人打印了包含它的
    /// `RouteDecision`」。`RouteDecision` 是 derive(Debug) 的，所以这条防线
    /// 完全依赖 `ResolvedCredential` 自己的手写 Debug。这里把它钉住。
    #[test]
    fn debug_route_decision_carrying_a_credential_renders_redacted() {
        use crate::ids::{BindingId, CredentialId, EndpointId, ProviderId};
        use crate::provider::CredentialKind;
        use http::HeaderName;

        const SECRET: &str = "sk-live-0123456789abcdef";
        let decision = RouteDecision {
            attempt: 0,
            provider_id: ProviderId::generate(),
            provider_kind: ProviderKind::Anthropic,
            endpoint_id: EndpointId::generate(),
            binding_id: BindingId::generate(),
            endpoint_url: Uri::from_static("https://api.anthropic.com/v1/messages"),
            credential: Arc::new(
                ResolvedCredential::new(
                    CredentialId::generate(),
                    CredentialKind::ApiKeyHeader,
                    HeaderName::from_static("x-api-key"),
                    SECRET,
                )
                .expect("固定的合法值"),
            ),
            provider_model: "claude-sonnet-4".into(),
            logical_model: "fast".into(),
            protocol: Protocol::AnthropicMessages,
            timeouts: Timeouts::default(),
            retry: Arc::new(RetryPolicy::default()),
            observe: ObserveMode::Full,
        };

        let rendered = format!("{decision:?}");
        assert!(!rendered.contains(SECRET), "凭据从 RouteDecision 泄漏了");
        assert!(rendered.contains("<redacted>"));
        // 非敏感字段仍然要能看见，否则这个 Debug 就没用了
        assert!(rendered.contains("claude-sonnet-4"));
        assert!(rendered.contains("anthropic"));
    }

    #[test]
    fn needs_tee_mode_is_off_returns_false() {
        assert!(!ObserveMode::Off.needs_tee());
    }

    #[test]
    fn needs_tee_mode_is_full_or_lite_returns_true() {
        assert!(ObserveMode::Full.needs_tee());
        assert!(ObserveMode::Lite.needs_tee());
    }

    #[test]
    fn keeps_raw_mode_is_full_returns_true() {
        assert!(ObserveMode::Full.keeps_raw());
    }

    #[test]
    fn keeps_raw_mode_is_lite_or_off_returns_false() {
        assert!(!ObserveMode::Lite.keeps_raw());
        assert!(!ObserveMode::Off.keeps_raw());
    }

    /// 默认是完整观测，降级必须是显式决定。
    #[test]
    fn default_observe_mode_no_pressure_returns_full() {
        assert_eq!(ObserveMode::default(), ObserveMode::Full);
    }
}
