//! Tee 通道上流动的东西：`TeeChunk` 与 `TeeEnd`。
//!
//! 主链路把字节的**引用计数副本**（`Bytes::clone`，不拷贝内容）交给旁路，
//! 然后立刻继续转发。旁路满了就丢，丢掉的字节数记在下一个 chunk 的
//! `gap_before` 上 —— 有损是允许的，**悄无声息的有损不允许**（宪法 C13）。
//!
//! 设计依据：`docs/06-data-plane.md` §5。

use core::fmt;

use bytes::Bytes;
use compact_str::CompactString;

/// 旁路收到的一段字节。
///
/// 故意不实现 `Copy`：`Bytes` 是引用计数的，克隆虽然便宜但不是免费的，
/// 应该在代码里看得见。
#[derive(Clone)]
pub struct TeeChunk {
    /// 与主链路共享同一块内存的引用计数副本。
    pub data: Bytes,
    /// 在这个 chunk **之前**被丢弃的字节数。
    ///
    /// 非 0 意味着旁路错过了内容：Parser 必须据此重新同步（丢弃当前半截帧），
    /// 并把整个 interaction 标成 `Integrity::Degraded`。绝不能当作无事发生。
    pub gap_before: u64,
}

impl TeeChunk {
    #[must_use]
    pub const fn new(data: Bytes) -> Self {
        Self {
            data,
            gap_before: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 这个 chunk 之前是否发生过丢弃。
    #[must_use]
    pub const fn has_gap(&self) -> bool {
        self.gap_before > 0
    }
}

// 手写 Debug：`data` 里装的是用户的 prompt 与模型响应。
// derive 出来的 Debug 会把它们原样打进日志，那正是宪法 C27 禁止的事
// —— 而且是最容易在某次「加个 debug! 看看」时发生的那种泄漏。
impl fmt::Debug for TeeChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeeChunk")
            .field("len", &self.data.len())
            .field("gap_before", &self.gap_before)
            .finish()
    }
}

/// 旁路通道的终止原因。
///
/// 它**不走 tee channel**：有界 channel 的 `try_send` 恰好在队列满时失败，
/// 而那正是结束原因最要紧的时刻。终止原因改由旁路侧的 `TeeReport`
/// （`OnceLock`，先写者胜）带出，与丢弃统计、尾部 `gap_before` 一起。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TeeEnd {
    /// 上游 body 正常读完。
    Eof,
    /// 客户端先断开了。已转发的部分仍然有观测价值，照常提交。
    ClientAborted,
    /// 上游出错。字符串是**错误描述**，不含响应内容。
    UpstreamError(CompactString),
    /// 被主动取消。
    Cancelled(CancelReason),
}

impl TeeEnd {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::ClientAborted => "client_aborted",
            Self::UpstreamError(_) => "upstream_error",
            Self::Cancelled(_) => "cancelled",
        }
    }

    /// 这次交互是否算「完整跑完」。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Eof)
    }
}

/// 取消的来源。区分它们是为了让审计能回答「这条流为什么断了」。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum CancelReason {
    /// 进程正在优雅停机
    Shutdown,
    /// 触到 `Timeouts.total` 或 `Timeouts.idle_frame`
    Deadline,
    /// 安全策略终止了这条流
    SecurityBlock,
    /// 客户端连接没了，继续拉上游没有意义
    ClientGone,
}

impl CancelReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Deadline => "deadline",
            Self::SecurityBlock => "security_block",
            Self::ClientGone => "client_gone",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宪法 C27：`data` 里装的是用户 prompt 与模型响应。
    ///
    /// derive 出来的 Debug 会把它们原样打进日志 —— 而且是在某次
    /// 「加个 debug! 看看这个 chunk 是什么」时不知不觉发生的。
    #[test]
    fn debug_chunk_carrying_payload_renders_length_not_content() {
        const PAYLOAD: &[u8] = b"data: {\"content\":\"my private prompt\"}\n\n";
        let chunk = TeeChunk {
            data: Bytes::from_static(PAYLOAD),
            gap_before: 42,
        };
        let rendered = format!("{chunk:?}");
        assert!(
            !rendered.contains("my private prompt"),
            "内容泄漏：{rendered}"
        );
        assert!(!rendered.contains("data:"), "内容泄漏：{rendered}");
        assert!(rendered.contains(&format!("len: {}", PAYLOAD.len())));
        assert!(rendered.contains("gap_before: 42"));
    }

    /// 宪法 C13：有损是允许的，悄无声息的有损不允许。
    #[test]
    fn has_gap_gap_before_is_nonzero_returns_true() {
        assert!(
            TeeChunk {
                data: Bytes::from_static(b"x"),
                gap_before: 1,
            }
            .has_gap()
        );
    }

    #[test]
    fn has_gap_chunk_built_by_new_returns_false() {
        assert!(!TeeChunk::new(Bytes::from_static(b"x")).has_gap());
    }

    /// 克隆只增加引用计数，不拷贝内容 —— Tee 的整个前提。
    #[test]
    fn clone_chunk_with_large_payload_shares_the_same_buffer() {
        let original = Bytes::from(vec![7_u8; 4096]);
        let chunk = TeeChunk::new(original.clone());
        let copy = chunk.clone();
        assert_eq!(copy.data.as_ptr(), original.as_ptr());
        assert_eq!(copy.len(), 4096);
    }

    #[test]
    fn is_complete_end_is_eof_returns_true() {
        assert!(TeeEnd::Eof.is_complete());
    }

    #[test]
    fn is_complete_end_is_abort_error_or_cancel_returns_false() {
        for end in [
            TeeEnd::ClientAborted,
            TeeEnd::UpstreamError("connection reset".into()),
            TeeEnd::Cancelled(CancelReason::Deadline),
        ] {
            assert!(!end.is_complete(), "{} 不该算完整结束", end.as_str());
        }
    }

    #[test]
    fn as_str_across_all_cancel_reasons_returns_distinct_labels() {
        let labels = [
            CancelReason::Shutdown,
            CancelReason::Deadline,
            CancelReason::SecurityBlock,
            CancelReason::ClientGone,
        ]
        .map(CancelReason::as_str);
        let mut deduped = labels.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), labels.len());
    }
}
