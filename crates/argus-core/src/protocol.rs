//! 两个正交维度：`Protocol`（线上字节长什么样）与 `ProviderKind`（怎么连、怎么鉴权）。
//!
//! 把它们分开是 Argus 最基础的一条设计：`AnthropicMessages` 这一个协议横跨
//! Anthropic、Bedrock、Vertex、以及各种自建兼容服务；反过来 `OpenAi` 这一个
//! provider 同时说 Chat 与 Responses 两种协议。任何「一个 provider = 一种协议」
//! 的建模都会在第二个 provider 上崩掉。
//!
//! 设计依据：`docs/04-protocols.md` §4。

use serde::{Deserialize, Serialize};

/// 线上字节的形状：请求 JSON 结构、响应 JSON 结构、流式帧格式。
///
/// 决定 Argus 用哪个 `ProtocolParser`、哪个 `ErrorRenderer`，以及流式帧是不是 SSE。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// `POST /v1/chat/completions`
    OpenAiChat,
    /// `POST /v1/responses`
    OpenAiResponses,
    /// `POST /v1/messages`
    AnthropicMessages,
    /// 预留，MVP 不实现
    GeminiGenerateContent,
    /// 预留，非 SSE（见 `docs/04` §9.2）
    BedrockInvoke,
}

impl Protocol {
    /// 指标标签与日志用。返回 `&'static str`，热路径零分配（宪法 C21）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiGenerateContent => "gemini_generate_content",
            Self::BedrockInvoke => "bedrock_invoke",
        }
    }

    /// 该协议流式响应的分帧格式。
    ///
    /// ⚠️ 这只在「协议自己决定分帧」时成立。`ProviderKind::Bedrock` 承载
    /// `AnthropicMessages` 时走的是 AWS event stream 而不是 SSE —— 那种情况下
    /// 分帧由 provider 决定，不能只看协议。见 `docs/04` §9.2：Framer 与 Parser
    /// 必须从第一天起就分开，正是为了容纳这件事。
    #[must_use]
    pub const fn frame_format(self) -> FrameFormat {
        match self {
            Self::OpenAiChat
            | Self::OpenAiResponses
            | Self::AnthropicMessages
            | Self::GeminiGenerateContent => FrameFormat::Sse,
            Self::BedrockInvoke => FrameFormat::AwsEventStream,
        }
    }

    /// 全部取值，供配置校验与测试遍历使用。
    pub const ALL: [Self; 5] = [
        Self::OpenAiChat,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
        Self::GeminiGenerateContent,
        Self::BedrockInvoke,
    ];
}

/// 流式响应的分帧格式。Framer 按它选择实现，与语义解析无关。
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

/// 如何连接与鉴权：URL 构造、认证 header、必需的额外 header、错误码方言。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    AzureOpenAi,
    /// 任何声称兼容 OpenAI Chat 的自建/第三方服务（DeepSeek / Qwen / vLLM / Ollama / Groq…）
    OpenAiCompatible,
    Anthropic,
    /// 任何声称兼容 Anthropic Messages 的服务
    AnthropicCompatible,
    Bedrock,
    Vertex,
}

impl ProviderKind {
    /// 指标标签用，零分配（宪法 C21）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::AzureOpenAi => "azure_openai",
            Self::OpenAiCompatible => "openai_compatible",
            Self::Anthropic => "anthropic",
            Self::AnthropicCompatible => "anthropic_compatible",
            Self::Bedrock => "bedrock",
            Self::Vertex => "vertex",
        }
    }

    /// `docs/04` §4.3 的组合矩阵：这个 provider 类型**是否可能**承载该协议。
    ///
    /// 这是「结构上是否合法」，用于配置校验时拒绝无意义的组合。它不是
    /// 「默认开启」——具体 binding 的 capabilities 只能在此基础上**收窄**
    /// （`docs/07` §4.3）。例如 `OpenAiCompatible` × `OpenAiResponses` 在这里
    /// 是合法的，但必须在配置里显式声明才会真的启用。
    #[must_use]
    pub const fn supports(self, protocol: Protocol) -> bool {
        matches!(
            (self, protocol),
            (
                Self::OpenAi | Self::AzureOpenAi | Self::OpenAiCompatible,
                Protocol::OpenAiChat | Protocol::OpenAiResponses
            ) | (
                Self::Anthropic | Self::AnthropicCompatible | Self::Bedrock | Self::Vertex,
                Protocol::AnthropicMessages
            ) | (Self::Vertex, Protocol::GeminiGenerateContent)
        )
    }

    /// 全部取值，供配置校验与测试遍历使用。
    pub const ALL: [Self; 7] = [
        Self::OpenAi,
        Self::AzureOpenAi,
        Self::OpenAiCompatible,
        Self::Anthropic,
        Self::AnthropicCompatible,
        Self::Bedrock,
        Self::Vertex,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docs/04-protocols.md` §4.3 的组合矩阵，逐格抄下来。
    ///
    /// 抄一遍而不是调用 `supports()` 生成期望值 —— 用被测函数自己生成期望值
    /// 的测试只能证明它自洽，证明不了它和文档一致。
    const EXPECTED: &[(ProviderKind, &[Protocol])] = &[
        (
            ProviderKind::OpenAi,
            &[Protocol::OpenAiChat, Protocol::OpenAiResponses],
        ),
        (
            ProviderKind::AzureOpenAi,
            &[Protocol::OpenAiChat, Protocol::OpenAiResponses],
        ),
        (
            // Responses 合法但需在配置里显式声明，见 supports() 的文档
            ProviderKind::OpenAiCompatible,
            &[Protocol::OpenAiChat, Protocol::OpenAiResponses],
        ),
        (ProviderKind::Anthropic, &[Protocol::AnthropicMessages]),
        (
            ProviderKind::AnthropicCompatible,
            &[Protocol::AnthropicMessages],
        ),
        (ProviderKind::Bedrock, &[Protocol::AnthropicMessages]),
        (
            ProviderKind::Vertex,
            &[Protocol::AnthropicMessages, Protocol::GeminiGenerateContent],
        ),
    ];

    #[test]
    fn supports_all_35_kind_protocol_pairs_matches_docs_04_matrix() {
        let mut checked = 0;
        for kind in ProviderKind::ALL {
            let allowed = EXPECTED
                .iter()
                .find(|(k, _)| *k == kind)
                .map(|(_, ps)| *ps)
                .expect("每个 ProviderKind 都必须在期望表里");
            for protocol in Protocol::ALL {
                let want = allowed.contains(&protocol);
                assert_eq!(
                    kind.supports(protocol),
                    want,
                    "组合 ({}, {}) 与 docs/04 §4.3 不一致",
                    kind.as_str(),
                    protocol.as_str()
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 35, "7 个 ProviderKind × 5 个 Protocol = 35 格");
    }

    /// `Protocol::BedrockInvoke` 目前没有任何 ProviderKind 承载。
    ///
    /// 这不是遗漏：`docs/04` §4.3 里 Bedrock 那一行写的是 `AnthropicMessages`
    /// （body 是 Anthropic 形状，只是套在 AWS event stream 的分帧里）。
    /// `BedrockInvoke` 是为将来真正的 Bedrock 原生协议预留的。
    /// 这个测试把「当前是空的」这件事钉住，等 Bedrock 落地时它会提醒你回来改。
    #[test]
    fn supports_protocol_is_bedrock_invoke_returns_false_for_every_kind() {
        for kind in ProviderKind::ALL {
            assert!(
                !kind.supports(Protocol::BedrockInvoke),
                "{} 声称支持 BedrockInvoke，但 docs/04 §4.3 没有这一格",
                kind.as_str()
            );
        }
    }

    #[test]
    fn protocol_as_str_across_all_variants_returns_distinct_labels() {
        let labels: Vec<&str> = Protocol::ALL.iter().map(|p| p.as_str()).collect();
        let mut deduped = labels.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), labels.len(), "Protocol::as_str 出现重复");
    }

    #[test]
    fn provider_kind_as_str_across_all_variants_returns_distinct_labels() {
        let labels: Vec<&str> = ProviderKind::ALL.iter().map(|k| k.as_str()).collect();
        let mut deduped = labels.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), labels.len(), "ProviderKind::as_str 出现重复");
    }

    #[test]
    fn frame_format_protocol_is_bedrock_invoke_returns_aws_event_stream() {
        assert_eq!(
            Protocol::BedrockInvoke.frame_format(),
            FrameFormat::AwsEventStream
        );
    }

    #[test]
    fn frame_format_protocol_is_anything_else_returns_sse() {
        for p in Protocol::ALL {
            if matches!(p, Protocol::BedrockInvoke) {
                continue;
            }
            assert_eq!(
                p.frame_format(),
                FrameFormat::Sse,
                "{} 应该是 SSE",
                p.as_str()
            );
        }
    }
}
