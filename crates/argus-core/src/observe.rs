//! 观测器的种类与强度。
//!
//! **这些类型不参与路由。** provider 由 URL 决定（`docs/02-routing-model.md`），
//! `ObserverKind` 只回答另一个问题：这条流里的字节该用哪套语义去理解。
//!
//! 因此这里**没有** `supports()` 之类的合法性矩阵 —— Observer 选错的后果只是
//! 观测降级，它不该有能力拒绝一个请求。见 ADR-010。

use serde::{Deserialize, Serialize};

/// 用哪套语义理解这条流。
///
/// 由三级判定选出：`observer_rules` → Profile 默认值 → 按 target host/path 猜
/// → 按 `Content-Type` 兜底。见 `docs/09-observers.md` §4.2。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverKind {
    /// `/v1/chat/completions` 的形状
    OpenAiChat,
    /// `/v1/responses` 的形状
    OpenAiResponses,
    /// `/v1/messages` 的形状
    AnthropicMessages,
    /// 预留，MVP 不实现
    GeminiGenerateContent,
    /// 认不出具体协议，但是 SSE：切帧、记 raw、统计事件数，**不解语义**
    GenericSse,
    /// 认不出协议的非流式 JSON：记 raw + 顶层字段名，**不解语义**
    GenericJson,
}

impl ObserverKind {
    /// 指标标签与日志用，零分配（宪法 C21）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiGenerateContent => "gemini_generate_content",
            Self::GenericSse => "generic_sse",
            Self::GenericJson => "generic_json",
        }
    }

    /// 这条流的分帧格式。
    ///
    /// ⚠️ 只在「协议自己决定分帧」时成立。Bedrock 承载 `AnthropicMessages` 时
    /// 走的是 AWS event stream 而不是 SSE —— 那种情况下分帧由 provider 决定。
    /// Framer 与 Observer 从第一天就分开，正是为了容纳这件事
    /// （`docs/09-observers.md` §9.2）。
    #[must_use]
    pub const fn frame_format(self) -> FrameFormat {
        match self {
            Self::OpenAiChat
            | Self::OpenAiResponses
            | Self::AnthropicMessages
            | Self::GeminiGenerateContent
            | Self::GenericSse => FrameFormat::Sse,
            Self::GenericJson => FrameFormat::None,
        }
    }

    /// 是否会尝试理解语义。两个 `Generic` 兜底返回 `false`。
    ///
    /// 认不出协议**不等于**不观测：Generic 仍然产出字节数、事件数、耗时与
    /// raw 留存，只是不解析内容。
    #[must_use]
    pub const fn is_semantic(self) -> bool {
        !matches!(self, Self::GenericSse | Self::GenericJson)
    }

    /// 全部取值，供配置校验与测试遍历使用。
    pub const ALL: [Self; 6] = [
        Self::OpenAiChat,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
        Self::GeminiGenerateContent,
        Self::GenericSse,
        Self::GenericJson,
    ];
}

/// 流式响应的分帧格式。Framer 按它选实现，与语义解析无关。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameFormat {
    /// `text/event-stream`，以空行分隔
    Sse,
    /// AWS `application/vnd.amazon.eventstream`，带长度前缀与 CRC
    AwsEventStream,
    /// 非流式：整个 body 就是一个 JSON 文档
    None,
}

impl FrameFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sse => "sse",
            Self::AwsEventStream => "aws_event_stream",
            Self::None => "none",
        }
    }
}

/// 这次请求的观测强度。在解析 target 时决定一次，之后整条流不再改变。
///
/// 为什么是**请求粒度**而不是逐字节丢弃：丢掉一条流中间 30% 的字节，得到的是
/// 一个残缺且不可信的 interaction —— 文本有洞、tool arguments 是坏 JSON、
/// usage 可能丢失。它占着存储却几乎没有分析价值。宁可完整地观测一部分请求。
/// 见 `docs/10-streaming.md` §6。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_across_all_observer_kinds_returns_distinct_labels() {
        let labels: Vec<&str> = ObserverKind::ALL.iter().map(|k| k.as_str()).collect();
        let mut deduped = labels.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), labels.len());
    }

    /// 认不出协议不等于不观测 —— 两个 Generic 仍然要能被选中并产出基础数据。
    #[test]
    fn is_semantic_kind_is_generic_returns_false() {
        assert!(!ObserverKind::GenericSse.is_semantic());
        assert!(!ObserverKind::GenericJson.is_semantic());
    }

    #[test]
    fn is_semantic_kind_is_a_known_protocol_returns_true() {
        for k in [
            ObserverKind::OpenAiChat,
            ObserverKind::OpenAiResponses,
            ObserverKind::AnthropicMessages,
            ObserverKind::GeminiGenerateContent,
        ] {
            assert!(k.is_semantic(), "{} 应该是语义 observer", k.as_str());
        }
    }

    #[test]
    fn frame_format_kind_is_generic_json_returns_none() {
        assert_eq!(ObserverKind::GenericJson.frame_format(), FrameFormat::None);
    }

    #[test]
    fn frame_format_kind_is_sse_based_returns_sse() {
        for k in ObserverKind::ALL {
            if matches!(k, ObserverKind::GenericJson) {
                continue;
            }
            assert_eq!(k.frame_format(), FrameFormat::Sse, "{}", k.as_str());
        }
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
