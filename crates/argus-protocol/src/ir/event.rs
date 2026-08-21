//! 观测事件：一条流产出一个有序序列。
//!
//! 设计依据：`docs/11-observation-model.md` §4.1–4.2、§4.6。

use bytes::Bytes;
use compact_str::CompactString;

use super::{BlockCoord, BlockDelta, BlockKind, Finish, ResponseStart, UsageDelta};

/// 指向原始 SSE frame 的引用。`Bytes` 是引用计数的，不复制。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawRef {
    pub frame: Bytes,
    /// frame 在整条流中的序号，用于排序与去重
    pub frame_seq: u32,
}

/// 一个观测事件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObservedEvent {
    /// 单调递增，从 0 开始，per-interaction
    pub seq: u32,
    /// 相对请求开始的微秒数
    pub at_micros: u32,
    pub kind: ObservedEventKind,
    /// 产生该事件的原始帧（零拷贝）。`ObserveMode::Lite` 下为 `None`
    pub raw: Option<RawRef>,
}

impl ObservedEvent {
    #[must_use]
    pub const fn new(seq: u32, at_micros: u32, kind: ObservedEventKind) -> Self {
        Self {
            seq,
            at_micros,
            kind,
            raw: None,
        }
    }

    #[must_use]
    pub fn with_raw(mut self, raw: RawRef) -> Self {
        self.raw = Some(raw);
        self
    }
}

/// 事件类型。
///
/// **`ToolCall*` 不是独立的事件类型**，而是 `BlockKind::ToolUse` +
/// `BlockDelta::Json`。在 Anthropic 里 `tool_use` **就是**一个 content block，
/// 强行拆开会让映射不自然（`docs/11` §4.2）。
///
/// 大小守在 64 字节以内（宪法 C21）：这个枚举会以每 token 一个的频率被创建，
/// 胖一倍就意味着热路径多一倍的 memcpy。守不住时**装箱罕见的那个变体**，
/// 而不是放宽预算 —— 有专门的测试盯着。
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ObservedEventKind {
    // ── 生命周期 ────────────────────────────────
    ResponseStart(Box<ResponseStart>),
    Finish(Box<Finish>),

    // ── 内容块 ──────────────────────────────────
    BlockStart {
        at: BlockCoord,
        kind: Box<BlockKind>,
    },
    BlockDelta {
        at: BlockCoord,
        delta: Box<BlockDelta>,
    },
    BlockStop {
        at: BlockCoord,
    },

    // ── 计量 ────────────────────────────────────
    Usage(Box<UsageDelta>),

    // ── 元信息 ──────────────────────────────────
    /// provider 特有的、我们没有语义映射的事件
    Unknown {
        event_name: CompactString,
    },
    /// provider 主动发出的错误事件
    ProviderError {
        kind: CompactString,
        message: CompactString,
    },
    /// SSE 层的心跳 / 注释行
    Heartbeat,

    // ── 流健康（Argus 自己产生，不来自 provider）───
    Gap {
        dropped_bytes: u64,
    },
    ParseFailure {
        reason: ParseFailureKind,
    },
}

impl ObservedEventKind {
    /// 指标标签与日志用，零分配（宪法 C21）。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ResponseStart(_) => "response_start",
            Self::Finish(_) => "finish",
            Self::BlockStart { .. } => "block_start",
            Self::BlockDelta { .. } => "block_delta",
            Self::BlockStop { .. } => "block_stop",
            Self::Usage(_) => "usage",
            Self::Unknown { .. } => "unknown",
            Self::ProviderError { .. } => "provider_error",
            Self::Heartbeat => "heartbeat",
            Self::Gap { .. } => "gap",
            Self::ParseFailure { .. } => "parse_failure",
        }
    }

    /// 这个事件是不是内容 —— 用于判定 TTFT（第一个内容 delta 的时刻）。
    #[must_use]
    pub const fn is_content(&self) -> bool {
        matches!(self, Self::BlockDelta { .. })
    }

    /// 便利构造，省掉调用点满屏的 `Box::new`。
    #[must_use]
    pub fn block_delta(at: BlockCoord, delta: BlockDelta) -> Self {
        Self::BlockDelta {
            at,
            delta: Box::new(delta),
        }
    }

    #[must_use]
    pub fn block_start(at: BlockCoord, kind: BlockKind) -> Self {
        Self::BlockStart {
            at,
            kind: Box::new(kind),
        }
    }

    #[must_use]
    pub fn usage(delta: UsageDelta) -> Self {
        Self::Usage(Box::new(delta))
    }

    #[must_use]
    pub fn finish(finish: Finish) -> Self {
        Self::Finish(Box::new(finish))
    }

    #[must_use]
    pub fn response_start(start: ResponseStart) -> Self {
        Self::ResponseStart(Box::new(start))
    }
}

/// 解析失败的原因。
///
/// `ParseFailure` 是一个**事件**而不是一个 `Err`：解析不出来是这条流的一个
/// 观测结论，不是 Argus 的故障，主链路不受影响（宪法 C2/C13）。
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ParseFailureKind {
    /// `data:` 后面不是合法 JSON
    MalformedJson,
    /// JSON 合法，但结构不是这个协议该有的形状
    UnexpectedShape,
    /// 缺了协议要求的字段（如 Anthropic 的 `event:` 行）
    MissingField { field: CompactString },
    /// 分帧层就失败了（`FramerError`）
    Framing,
}

impl ParseFailureKind {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MalformedJson => "malformed_json",
            Self::UnexpectedShape => "unexpected_shape",
            Self::MissingField { .. } => "missing_field",
            Self::Framing => "framing",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BlockCoord, BlockDelta, BlockKind, FinishReason, TextChunk};

    /// 宪法 C21 的一条硬预算。
    ///
    /// 这个枚举会以每 token 一个的频率被创建并在旁路里搬来搬去，胖一倍就
    ///意味着热路径多一倍 memcpy。守不住时**装箱罕见的那个变体**（现在
    /// `ResponseStart` / `Finish` / `BlockKind` / `BlockDelta` / `UsageDelta`
    /// 都是 `Box` 的），而不是放宽预算。
    #[test]
    fn size_of_observed_event_kind_stays_within_the_64_byte_budget() {
        assert!(
            size_of::<ObservedEventKind>() <= 64,
            "ObservedEventKind = {} 字节，超了 64 的预算",
            size_of::<ObservedEventKind>()
        );
    }

    /// `BlockCoord` 做 map key 不能有任何堆分配。
    #[test]
    fn size_of_block_coord_fits_in_a_machine_word_pair() {
        assert!(size_of::<BlockCoord>() <= 8);
    }

    #[test]
    fn as_str_across_all_event_kinds_returns_distinct_labels() {
        let kinds = [
            ObservedEventKind::response_start(ResponseStart::default()),
            ObservedEventKind::finish(super::super::Finish::new(FinishReason::Stop)),
            ObservedEventKind::block_start(BlockCoord::default(), BlockKind::Text),
            ObservedEventKind::block_delta(
                BlockCoord::default(),
                BlockDelta::Text(TextChunk::Decoded("x".into())),
            ),
            ObservedEventKind::BlockStop {
                at: BlockCoord::default(),
            },
            ObservedEventKind::usage(UsageDelta::default()),
            ObservedEventKind::Unknown {
                event_name: "x".into(),
            },
            ObservedEventKind::ProviderError {
                kind: "k".into(),
                message: "m".into(),
            },
            ObservedEventKind::Heartbeat,
            ObservedEventKind::Gap { dropped_bytes: 1 },
            ObservedEventKind::ParseFailure {
                reason: ParseFailureKind::MalformedJson,
            },
        ];
        let mut labels: Vec<&str> = kinds.iter().map(ObservedEventKind::as_str).collect();
        let total = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), total, "有两个变体共用了同一个标签");
    }

    /// TTFT 的判据：只有内容 delta 算「第一个 token 到了」。
    #[test]
    fn is_content_only_block_delta_counts_as_content() {
        assert!(
            ObservedEventKind::block_delta(
                BlockCoord::default(),
                BlockDelta::Text(TextChunk::Decoded("x".into()))
            )
            .is_content()
        );
        for kind in [
            ObservedEventKind::Heartbeat,
            ObservedEventKind::block_start(BlockCoord::default(), BlockKind::Text),
            ObservedEventKind::usage(UsageDelta::default()),
        ] {
            assert!(!kind.is_content(), "{} 不该算内容", kind.as_str());
        }
    }
}
