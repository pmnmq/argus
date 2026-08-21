//! Observation IR：三种协议的**最小公共超集**。
//!
//! IR 的作用是让 Accumulator、存储与安全规则只面对一套形状，而不是三套。
//! 它**不用于重建请求**（`docs/11-observation-model.md` §3.1）—— 那会诱使
//! 有人去做协议转换，而宪法 C3 明确禁止。
//!
//! 三条贯穿全模块的性质：
//!
//! 1. **未知即容纳**（宪法 C2）：`Unknown` / `Opaque` / `Other` / `raw_reason`
//!    /`extra`，每一层都有一个装不认识的东西的位置，绝不报错、绝不丢弃。
//! 2. **不解析**：tool arguments 的 JSON 片段在流中**从不** parse（§7.2）。
//! 3. **热路径零分配**（宪法 C21）：`CompactString` / `SmallVec` / `Bytes`，
//!    并且 `ObservedEventKind` 的大小守在 64 字节以内。

pub mod block;
pub mod event;
pub mod meta;

pub use self::{
    block::{BlockCoord, BlockDelta, BlockKind, TextChunk},
    event::{ObservedEvent, ObservedEventKind, ParseFailureKind, RawRef},
    meta::{Finish, FinishReason, ResponseStart, UsageDelta},
};
