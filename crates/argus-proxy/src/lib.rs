//! Argus L3 数据面：请求从进到出的全部主链路，以及与之隔离的观测旁路。
//!
//! 关键性质（编译期可证）：本 crate 的依赖树中**没有** `sqlx`、也**没有** `argus-storage`。
//! 持久化通过 `argus_protocol::sink::InteractionSink` 注入，由 `argus-server` 在组装期绑定。
//!
//! 设计依据：`docs/06-data-plane.md`、`docs/10-streaming.md`、`docs/08-provider-profiles.md`、
//! `docs/12-security.md`、`docs/14-auth-and-credentials.md`。

#![forbid(unsafe_code)]

pub mod ingress;
pub mod target;

/// 编译期常量，用于 F01 验收：证明 crate 真的被编译进依赖图。
pub const CRATE_NAME: &str = "argus-proxy";
