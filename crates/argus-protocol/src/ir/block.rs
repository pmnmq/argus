//! 内容块：坐标系、类型、增量。
//!
//! [`BlockCoord`] 是整个 IR 的核心抽象 —— 三种协议的定位方式在这里被统一成
//! 一个 `Copy` 的三元组（`docs/11-observation-model.md` §4.4）。

use bytes::Bytes;
use compact_str::CompactString;

/// 统一的内容块坐标。
///
/// 三级 `(choice, item, part)` 是三种协议坐标系的**最小公共超集**。用固定
/// 大小的 `Copy` 类型意味着它可以直接当 map 的 key 而不产生任何堆分配
/// （宪法 C21）。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct BlockCoord {
    /// 候选索引。`OpenAI` 的 `choices[i]` / `n>1`；Anthropic 恒为 0
    pub choice: u8,
    /// 输出项索引。Responses 的 `output_index`；Anthropic 的 content index
    pub item: u16,
    /// 项内部的内容索引。Responses 的 `content_index`；其他协议恒为 0
    pub part: u16,
}

impl BlockCoord {
    #[must_use]
    pub const fn new(choice: u8, item: u16, part: u16) -> Self {
        Self { choice, item, part }
    }

    /// 单候选、单项的协议（Anthropic）用得最多的那个构造。
    #[must_use]
    pub const fn item(item: u16) -> Self {
        Self::new(0, item, 0)
    }
}

/// 块的类型。
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum BlockKind {
    Text,
    /// 推理内容。`summary = true` 表示这是摘要而非原文（`OpenAI` Responses 的行为）
    Reasoning {
        summary: bool,
        redacted: bool,
    },
    ToolUse {
        id: Option<CompactString>,
        name: Option<CompactString>,
    },
    /// 客户端回传的工具结果（只在请求侧观测到）
    ToolResult {
        tool_use_id: CompactString,
        is_error: bool,
    },
    Refusal,
    Image {
        media_type: Option<CompactString>,
    },
    Audio {
        media_type: Option<CompactString>,
    },
    /// provider 有、我们没映射的 block 类型。原样保留（宪法 C2）
    Other(CompactString),
}

impl BlockKind {
    /// 指标标签用，零分配（宪法 C21）。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolUse { .. } => "tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Refusal => "refusal",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
            Self::Other(_) => "other",
        }
    }

    /// 这个块的 delta 会不会累积成文本。
    #[must_use]
    pub const fn is_textual(&self) -> bool {
        matches!(self, Self::Text | Self::Reasoning { .. } | Self::Refusal)
    }
}

/// 文本片段。
///
/// 绝大多数 delta 的文本在 JSON 里没有转义，可以直接与 [`super::RawRef`]
/// 共享底层缓冲；只有含 `\n` / `\uXXXX` 时才必须解码并新分配。两种情况分开
/// 表达，是为了让「这次有没有多一次分配」在类型上就看得见（宪法 C21）。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TextChunk {
    /// 与 `RawRef` 共享底层 `Bytes`，clone 是引用计数
    Shared(Bytes),
    /// 原文含 JSON 转义，解码后新分配
    Decoded(CompactString),
}

impl TextChunk {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Shared(b) => b,
            Self::Decoded(s) => s.as_bytes(),
        }
    }

    /// 不是合法 UTF-8 时返回 `None`。
    ///
    /// 只有 `Shared` 可能不合法：它是从上游字节里切出来的，而上游未必守规矩。
    /// 返回 `Option` 而不是 lossy 转换 —— 悄悄替换成 `U+FFFD` 会让审计里的
    /// 文本与上游实际发的对不上。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Shared(b) => core::str::from_utf8(b).ok(),
            Self::Decoded(s) => Some(s),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    /// 有没有为这一片文本额外分配过内存。基准测试用它验证 C21。
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        matches!(self, Self::Shared(_))
    }
}

/// 块的增量。
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum BlockDelta {
    Text(TextChunk),
    Reasoning(TextChunk),
    /// tool arguments 的 JSON 片段。**绝不在此处 parse**（`docs/11` §7.2）
    Json(TextChunk),
    /// Anthropic thinking 的签名，必须原样保留
    Signature(TextChunk),
    /// 引用信息（Anthropic `citations_delta`）
    Citation(Bytes),
    /// 我们没映射的 delta 类型，原样保留（宪法 C2）
    Opaque {
        delta_type: CompactString,
        raw: Bytes,
    },
}

impl BlockDelta {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Reasoning(_) => "reasoning",
            Self::Json(_) => "json",
            Self::Signature(_) => "signature",
            Self::Citation(_) => "citation",
            Self::Opaque { .. } => "opaque",
        }
    }

    /// 这个增量携带的字节数，用于 cap 判断。
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Text(c) | Self::Reasoning(c) | Self::Json(c) | Self::Signature(c) => c.len(),
            Self::Citation(b) => b.len(),
            Self::Opaque { raw, .. } => raw.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
