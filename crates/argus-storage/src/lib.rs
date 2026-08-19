//! Argus L3 存储层：`InteractionSink` 的 PostgreSQL 实现、本地 Spool、Control Plane 仓储。
//!
//! PostgreSQL **不是** Data Plane 的依赖（宪法 C19）：PG 挂掉时网关继续转发，
//! 审计数据落到本地 Spool，恢复后幂等重放。见 `docs/10-storage.md` §8。

#![forbid(unsafe_code)]

/// 编译期常量，用于 F01 验收：证明 crate 真的被编译进依赖图。
pub const CRATE_NAME: &str = "argus-storage";
