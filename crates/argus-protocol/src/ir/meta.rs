//! 生命周期与计量事件的载荷。
//!
//! 设计依据：`docs/11-observation-model.md` §4.5、§6。

use bytes::Bytes;
use compact_str::CompactString;

use super::BlockCoord;

/// 响应开始。三种协议都有一个"我要开始回答了"的事件。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ResponseStart {
    pub provider_response_id: Option<CompactString>,
    /// provider **实际**使用的模型，可能与请求里写的不同
    pub model: Option<CompactString>,
    pub role: Option<CompactString>,
}

/// 一次 usage 上报。
///
/// **每个字段独立**：`None` 表示"这次没报"，不是"报了 0"。这个区分是
/// last-non-null-wins 合并的前提（`docs/11` §6）。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UsageDelta {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    /// provider 给出但我们没有映射的 usage 字段，原样保留（宪法 C2）。
    ///
    /// 装在 `Box` 里是为了守住 `size_of::<ObservedEventKind>() <= 64`：
    /// 裸 `Bytes` 是 32 字节，会把整个事件枚举顶爆，而未映射字段在实践中
    /// 很罕见 —— 让罕见情况付这一次分配，比让每个事件都胖 24 字节划算。
    pub extra: Option<Box<Bytes>>,
}

impl UsageDelta {
    /// 一个字段都没报（`extra` 不算 —— 它是没映射的字段，不是计量值）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cache_creation_input_tokens.is_none()
            && self.cache_read_input_tokens.is_none()
            && self.reasoning_tokens.is_none()
    }
}

/// 流的结束。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Finish {
    pub reason: FinishReason,
    /// provider 的原始字符串，**永不丢失**
    pub raw_reason: Option<CompactString>,
    /// 哪个 choice 结束了
    pub at: Option<BlockCoord>,
}

impl Finish {
    #[must_use]
    pub fn new(reason: FinishReason) -> Self {
        Self {
            reason,
            raw_reason: None,
            at: None,
        }
    }
}

/// 结束原因。
///
/// [`FinishReason::Other`] 与 `raw_reason` 的组合是前向兼容的关键：
/// Anthropic 明天新增一个 `stop_reason`，我们会映射为 `Other`、保留原字符串、
/// **不报错**，并让指标 `argus_unknown_finish_reason_total` 上报 ——
/// 运维看到指标之后可以在下个版本加映射（`docs/11` §4.5、宪法 C2）。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum FinishReason {
    Stop,
    Length,
    ToolUse,
    ContentFilter,
    Refusal,
    Incomplete,
    Error,
    /// provider 新增的、我们不认识的 `stop_reason`
    Other,
}

impl FinishReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolUse => "tool_use",
            Self::ContentFilter => "content_filter",
            Self::Refusal => "refusal",
            Self::Incomplete => "incomplete",
            Self::Error => "error",
            Self::Other => "other",
        }
    }

    /// 全部取值，供配置校验与测试遍历。
    pub const ALL: [Self; 8] = [
        Self::Stop,
        Self::Length,
        Self::ToolUse,
        Self::ContentFilter,
        Self::Refusal,
        Self::Incomplete,
        Self::Error,
        Self::Other,
    ];
}
