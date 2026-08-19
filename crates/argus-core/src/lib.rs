//! Argus L1 基础层：被所有层共享的类型。
//!
//! 依赖图的根。**不得**依赖任何 `argus-*` crate（宪法 C6：反向依赖即循环），
//! 也不得依赖 tokio / hyper / sqlx —— 这里只有数据形状，没有行为。
//!
//! 模块划分见 `docs/14-rust-project-structure.md` §5.3。

#![forbid(unsafe_code)]

pub mod error;
pub mod ids;
pub mod protocol;
pub mod provider;
pub mod route;
pub mod tee;

pub use error::{ProxyError, QuotaKind, UpstreamStage};
pub use ids::{
    ApiKeyId, BindingId, ConversationLink, CredentialId, EndpointId, ProviderId, TraceContext,
    WorkspaceId,
};
pub use protocol::{FrameFormat, Protocol, ProviderKind};
pub use provider::{CredentialKind, InvalidCredential, ResolvedCredential};
pub use route::{Backoff, ObserveMode, RetryPolicy, RouteDecision, Timeouts};
pub use tee::{CancelReason, TeeChunk, TeeEnd};
