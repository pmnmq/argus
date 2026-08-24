//! Argus L3 数据面：请求从进到出的全部主链路，以及与之隔离的观测旁路。
//!
//! 关键性质（编译期可证）：本 crate 的依赖树中**没有** `sqlx`、也**没有** `argus-storage`。
//! 持久化通过 `argus_protocol::sink::InteractionSink` 注入，由 `argus-server` 在组装期绑定。
//!
//! 设计依据：`docs/06-data-plane.md`、`docs/10-streaming.md`、`docs/08-provider-profiles.md`、
//! `docs/12-security.md`、`docs/14-auth-and-credentials.md`。

#![forbid(unsafe_code)]

pub mod body;
pub mod ingress;
pub mod observer;
pub mod sink;
pub mod target;
pub mod tee;
// 测试与压测的公共设施。非默认 feature，release 构建里根本不存在
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod upstream;

/// 编译期常量，用于 F01 验收：证明 crate 真的被编译进依赖图。
pub const CRATE_NAME: &str = "argus-proxy";
