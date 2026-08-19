//! Argus L1 基础层：`Protocol`、`TeeChunk`、配置快照、错误分类等被所有层共享的类型。
//!
//! 依赖图的根。**不得**依赖任何 `argus-*` crate（宪法 C6：反向依赖即循环）。
//!
//! 模块布局见 `docs/14-rust-project-structure.md` §5.3；类型定义见 F02。

#![forbid(unsafe_code)]

/// 编译期常量，用于 F01 验收：证明 crate 真的被编译进依赖图。
pub const CRATE_NAME: &str = "argus-core";
