//! 分帧层：字节 → 逻辑帧。**不理解帧里装的是什么**。
//!
//! 分帧与解析必须分开，这是 `docs/04-protocols.md` §9.2 称为「本文最重要的
//! 前瞻性约束」的那一条：Bedrock 的 `invoke-with-response-stream` 返回的是
//! AWS 私有的二进制帧格式，不是 SSE，但**解出来的 payload 是标准的
//! Anthropic 事件 JSON**。也就是说换 provider 只该换 `Framer`，
//! `ProtocolParser` 原样复用。如果这一层从一开始就没分开，
//! 支持 Bedrock 就意味着重构。
//!
//! 目前只有 [`SseFramer`]。`EventStreamFramer`（Bedrock，Phase 2）届时实现
//! [`Framer`] 即可接入，不需要动上层。

pub mod sse;

use bytes::Bytes;

pub use sse::{FramerConfig, FramerStats, SseFramer};

use argus_core::TeeChunk;

/// 字节流 → 逻辑帧。
pub trait Framer {
    /// 喂入一段字节，产出零个或多个帧。
    ///
    /// `chunk.gap_before > 0` 表示旁路丢过字节，实现必须据此重新同步，
    /// 并通过 [`FrameSink::emit_gap`] 让丢失可见（宪法 C13）。
    ///
    /// # Errors
    /// 缓冲超过上限，或重新同步失败。两者都意味着**放弃这条流的观测**，
    /// 但主链路的转发不受影响（宪法 C10）。
    fn push(&mut self, chunk: &TeeChunk, out: &mut dyn FrameSink) -> Result<(), FramerError>;

    /// 上游 EOF。
    ///
    /// 必须处理「最后一帧没有尾随空行」的情况 —— 真实 provider 会这样
    /// （`docs/04` 坑 C8：OpenAI 异常结束时不发 `[DONE]` 也不发尾随空行）。
    ///
    /// # Errors
    /// 同 [`Framer::push`]。
    fn finish(&mut self, out: &mut dyn FrameSink) -> Result<(), FramerError>;

    /// 当前滞留在内部缓冲里的字节数。用于断言有界性（宪法 C9）。
    fn buffered_bytes(&self) -> usize;
}

/// 帧的接收端。
pub trait FrameSink {
    fn emit(&mut self, frame: FrameRef<'_>);

    /// 旁路丢了 `dropped_bytes` 个字节。
    ///
    /// 单独一个方法而不是混进 [`FrameRef`]：gap 不是 provider 产生的帧，
    /// 把它塞进帧枚举会让下游每次 match 都要处理一个不存在的"帧"。
    fn emit_gap(&mut self, dropped_bytes: u64);
}

/// 一个逻辑帧。借用自底层 `Bytes`，不拥有数据。
#[derive(Debug)]
pub enum FrameRef<'a> {
    /// 普通 SSE 事件。
    Event {
        /// `event:` 字段。`OpenAI` Chat 不发这个字段，Anthropic 发。
        event: Option<&'a [u8]>,
        data: FrameData<'a>,
        /// 整帧原始字节（含分隔符）。`ObserveMode::Full` 下要原样留存。
        raw: &'a Bytes,
    },
    /// `data: [DONE]` —— `OpenAI` 的结束哨兵，不是 JSON，别拿去解析。
    Done,
    /// 整帧只有 `:` 开头的注释行（SSE 心跳）。
    Comment,
}

/// `data:` 字段的值。
///
/// 单行时借用底层缓冲（绝大多数情况）；只有一帧里出现多个 `data:` 行时
/// 才按 SSE 规范用 `\n` 拼接，那时才分配。实践中 AI provider 不这么发，
/// 但规范允许，必须容忍。
#[derive(Debug, PartialEq, Eq)]
pub enum FrameData<'a> {
    Borrowed(&'a [u8]),
    Joined(Vec<u8>),
}

impl FrameData<'_> {
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(b) => b,
            Self::Joined(v) => v,
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
}

/// 分帧失败。两个变体都是**终止这条流的观测**，不是终止请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FramerError {
    /// 单帧超过 `max_frame_bytes`。防的是「provider 发了一个永不结束的
    /// `data:` 行」把内存吃光。
    #[error("frame exceeds max_frame_bytes ({0})")]
    Overflow(usize),
    /// 丢字节之后扫了 `max_frame_bytes` 还没找到帧边界，无法重新同步。
    #[error("resync failed after {0} bytes")]
    ResyncFailed(u64),
}
