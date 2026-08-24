//! `InteractionSink` 的 stub 实现：把观测结果打进日志。
//!
//! F12 换成 `PgSink`。在那之前它证明的是一件真事：**整条旁路链路是通的**，
//! 而且 `argus-proxy` 拿到落库能力**不需要依赖 argus-storage**
//! （宪法 C6，`ci/check-invariants.sh` 断言过依赖树）。
//!
//! ⚠️ 这里一个字节的内容都不打（宪法 C27）：没有 prompt、没有响应正文、
//! 没有 tool arguments。打出去的全是计数与枚举标签。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use argus_protocol::{
    interaction::{Integrity, ObservedInteraction},
    sink::{InteractionSink, SinkFull},
};

/// 只记摘要的 sink。
#[derive(Debug, Default)]
pub struct LogSink {
    submitted: AtomicU64,
}

impl LogSink {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 收到过多少条。集成测试用它确认旁路真的跑完了。
    #[must_use]
    pub fn submitted(&self) -> u64 {
        self.submitted.load(Ordering::Relaxed)
    }
}

impl InteractionSink for LogSink {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        self.submitted.fetch_add(1, Ordering::Relaxed);
        let (dropped, gaps) = match interaction.integrity {
            Integrity::Degraded {
                dropped_bytes,
                gap_count,
                ..
            } => (dropped_bytes, gap_count),
            // `Integrity` 是 #[non_exhaustive] 的，跨 crate 必须留通配符。
            // 新增的完整性等级默认按「没丢字节」记 —— 丢多少要靠专门的字段说
            _ => (0, 0),
        };
        tracing::info!(
            outcome = interaction.outcome.as_str(),
            integrity = interaction.integrity.as_str(),
            observer = interaction.observer.as_str(),
            status = interaction.response_meta.status_code,
            blocks = interaction.blocks.len(),
            events = interaction.stats.events_emitted,
            heartbeats = interaction.stats.heartbeats,
            parse_failures = interaction.stats.parse_failures,
            unknown_events = interaction.unknown_events.len(),
            dropped_bytes = dropped,
            gap_count = gaps,
            input_tokens = interaction.usage.input_tokens,
            output_tokens = interaction.usage.output_tokens,
            ttft_micros = interaction.timings.ttft_micros,
            total_micros = interaction.timings.total_micros,
            "interaction observed"
        );
        Ok(())
    }
}

/// 永远满的 sink。用来验「满了就丢，绝不阻塞」这条路径（宪法 C5）。
#[derive(Debug, Default)]
pub struct RejectingSink;

impl InteractionSink for RejectingSink {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        Err(SinkFull(interaction))
    }
}
