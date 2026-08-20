//! 观测器：把字节变成「这条流发生了什么」。
//!
//! 本模块现在只有兜底的两个（R04）。三种语义 Observer 与它们共用的
//! `ProtocolObserver` trait 在 F05 落地 —— 那需要先有 Observation IR（F04）。
//!
//! 设计依据：`docs/09-observers.md` §4。

pub mod generic;

pub use generic::{GenericJsonObserver, GenericSseObserver, JsonShape};
