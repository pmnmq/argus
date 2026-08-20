//! Argus L2 领域层：`Framer` / `ProtocolParser` / `Accumulator` / 观测 IR / `InteractionSink`。
//!
//! 本 crate 是**纯计算**的：给定字节，产出事件。它不做 IO、不 spawn 任务、不 panic。
//! 这些性质由下面的属性 + `ci/check-invariants.sh` 的依赖树断言共同保证。
//!
//! 设计依据：`docs/09-observers.md`、`docs/10-streaming.md`、`docs/11-observation-model.md`。

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing, // 强制用 .get()
    clippy::integer_division,
    clippy::unreachable,
    clippy::todo
)]
// clippy::pedantic 现在由 [workspace.lints] 统一开启，见根 Cargo.toml

// 说明：上面的 deny 列表对 `#[cfg(test)]` 模块同样生效。测试里要用 unwrap 时，
// 在该 mod 上写 `#![allow(clippy::unwrap_used, clippy::expect_used)]` 并说明理由，
// 不要放宽 crate 级别的设置。

pub mod framer;
pub mod observer;

pub use framer::{FrameData, FrameRef, FrameSink, Framer, FramerConfig, FramerError, SseFramer};
pub use observer::{GenericJsonObserver, GenericSseObserver, JsonShape};

/// 编译期常量，用于 F01 验收：证明 crate 真的被编译进依赖图。
pub const CRATE_NAME: &str = "argus-protocol";
