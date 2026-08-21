//! `InteractionSink`：Data Plane 与存储之间的**唯一**接缝。
//!
//! 它放在 `argus-protocol` 而不是 `argus-core`，也不放在 `argus-storage`：
//! `argus-proxy` 只依赖这个 trait，因此「数据面不依赖存储」这条
//! （宪法 C6）是**编译期可证**的 —— `ci/check-invariants.sh` 直接断言
//! `argus-proxy` 的依赖树里没有 `sqlx` 也没有 `argus-storage`。
//!
//! 设计依据：`docs/15-storage.md` §4.1。

use crate::interaction::ObservedInteraction;

/// 提交一次观测结果。
///
/// **必须非阻塞**（宪法 C5）：主链路绝不为旁路等待。实现方满了就返回
/// [`SinkFull`]，由调用方决定丢弃策略 —— 而不是在这里 await 一个有界队列。
pub trait InteractionSink: Send + Sync + 'static {
    /// # Errors
    ///
    /// 队列已满时返回 [`SinkFull`]，**并把所有权还回去**。
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull>;
}

/// 队列满。
///
/// 把 `Box` 原样还给调用方是刻意的：丢弃策略（丢最旧的？降级成只记元数据？
/// 计一个指标就算了？）是调用方的决定，不是 sink 的。顺手也避免了
/// 「返回 Err 之后数据不知去向」这种最难查的一类问题。
#[derive(Debug)]
pub struct SinkFull(pub Box<ObservedInteraction>);

impl core::fmt::Display for SinkFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("interaction sink is full")
    }
}

impl core::error::Error for SinkFull {}
