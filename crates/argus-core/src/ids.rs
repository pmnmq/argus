//! 强类型 ID 与 `TraceContext`。
//!
//! 所有 ID 都是 `Uuid` 的 newtype。裸 `Uuid` 到处传的代价不是理论上的：
//! `WorkspaceId` 和 `ApiKeyId` 类型相同，参数写反了编译器不会吭声，
//! 而这种 bug 的表现形式是「A 工作区的配额被算到 B 头上」。
//!
//! 设计依据：`docs/02-data-plane.md` §10。

use core::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_newtype {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// 生成一个新 ID。用 UUIDv7：自带时间序，做主键时索引局部性远好于 v4。
            #[must_use]
            pub fn generate() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(id: Uuid) -> Self {
                Self(id)
            }

            /// 取出内层 `Uuid`。
            ///
            /// 这是与存储层交互的唯一出口：`argus-core` 不依赖 sqlx（宪法 C6），
            /// 所以 ID 类型无法实现 `sqlx::Type`，绑定 SQL 参数时走这里。
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl From<Uuid> for $name {
            fn from(id: Uuid) -> Self {
                Self(id)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

id_newtype!(
    /// 租户边界。配额、策略、审计可见性都以它为单位。
    WorkspaceId
);
id_newtype!(
    /// 客户端 API Key 的标识部分（明文可索引，见 `docs/09` §5）。
    ApiKeyId
);
id_newtype!(
    /// 一个已配置的 provider（含 kind、endpoint 列表、凭据）。
    ProviderId
);
id_newtype!(
    /// provider 下的一个具体接入点（region / deployment / base_url）。
    EndpointId
);
id_newtype!(
    /// 逻辑模型 → provider 模型的一条绑定。capabilities 挂在这一层。
    BindingId
);
id_newtype!(
    /// 一份 provider 凭据。轮换时新增而非覆盖，因此需要独立 ID。
    CredentialId
);

/// `conversation_id` 的来源。
///
/// 这个区分不是记账用的：安全策略**只信任 `Explicit`**。推断出来的会话关联
/// 可以被构造（比如刻意复用某个 `previous_response_id`），如果拿它做跨轮次的
/// 风险累积判定，就等于把判定权交给了调用方。见 `docs/06` §9。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationLink {
    /// 客户端显式提供 `x-argus-conversation-id`
    Explicit,
    /// 由 `previous_response_id` 或消息前缀指纹推断 —— 安全策略不信任
    Inferred,
    #[default]
    None,
}

impl ConversationLink {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Inferred => "inferred",
            Self::None => "none",
        }
    }

    /// 安全策略是否可以基于这个关联做跨轮次判定。
    #[must_use]
    pub const fn is_trusted(self) -> bool {
        matches!(self, Self::Explicit)
    }
}

/// 一次请求的全部关联标识。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TraceContext {
    /// 单个 HTTP 请求的唯一标识。多次上游 attempt 共用同一个
    /// （attempt 之间用 `AttemptRecord.seq` 区分）。
    pub request_id: Uuid,
    /// 一次端到端操作。来自客户端的 `traceparent`，或由 Argus 生成。
    pub trace_id: Uuid,
    /// 一次多轮对话。单轮请求为 `None`。
    pub conversation_id: Option<Uuid>,
    /// `conversation_id` 的来源，决定它能否被安全策略信任。
    pub conversation_link: ConversationLink,
}

impl TraceContext {
    /// 新建一条 trace：客户端没带 `traceparent` 时用这个。
    ///
    /// `request_id` 与 `trace_id` 都用 UUIDv7 —— 审计表按时间分区，
    /// 时间序主键让新写入集中在最后一个分区。
    #[must_use]
    pub fn root() -> Self {
        Self {
            request_id: Uuid::now_v7(),
            trace_id: Uuid::now_v7(),
            conversation_id: None,
            conversation_link: ConversationLink::None,
        }
    }

    /// 沿用客户端传来的 `trace_id`，但 `request_id` 始终由 Argus 生成
    /// —— 它必须是全局唯一的，不能让调用方决定。
    #[must_use]
    pub fn with_trace_id(trace_id: Uuid) -> Self {
        Self {
            request_id: Uuid::now_v7(),
            trace_id,
            conversation_id: None,
            conversation_link: ConversationLink::None,
        }
    }

    #[must_use]
    pub fn with_conversation(mut self, id: Uuid, link: ConversationLink) -> Self {
        self.conversation_id = Some(id);
        self.conversation_link = link;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_called_twice_returns_distinct_v7_uuids() {
        let a = WorkspaceId::generate();
        let b = WorkspaceId::generate();
        assert_ne!(a, b);
        assert_eq!(a.as_uuid().get_version_num(), 7);
        assert_eq!(b.as_uuid().get_version_num(), 7);
    }

    /// v7 的时间序是它相对 v4 的唯一理由：做主键时新写入集中在最后一个分区，
    /// 索引不会随机散写。
    ///
    /// 用显式时间戳而不是 sleep 来验证 —— 测试里 sleep 既慢又是 clippy
    /// `disallowed-methods` 拦的东西（见 clippy.toml）。
    #[test]
    fn ord_ids_built_from_increasing_timestamps_sorts_by_time() {
        use uuid::{NoContext, Timestamp};
        let early = WorkspaceId::from_uuid(Uuid::new_v7(Timestamp::from_unix(
            NoContext,
            1_700_000_000,
            0,
        )));
        let later = WorkspaceId::from_uuid(Uuid::new_v7(Timestamp::from_unix(
            NoContext,
            1_700_086_400,
            0,
        )));
        assert!(early < later, "UUIDv7 应该按时间排序：{early} !< {later}");
    }

    /// 数据库与 JSON 里就是一个裸 uuid 字符串，newtype 不能改变线上表示。
    #[test]
    fn serde_id_roundtrip_preserves_value_as_bare_uuid_string() {
        let id = ApiKeyId::generate();
        let json = serde_json::to_string(&id).expect("序列化");
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(
            serde_json::from_str::<ApiKeyId>(&json).expect("反序列化"),
            id
        );
    }

    #[test]
    fn as_uuid_after_from_uuid_returns_the_original_uuid() {
        let raw = Uuid::now_v7();
        assert_eq!(ProviderId::from_uuid(raw).as_uuid(), raw);
        assert_eq!(Uuid::from(EndpointId::from(raw)), raw);
    }

    /// 安全策略只信任显式声明的会话关联。
    ///
    /// 推断出来的关联可以被构造（比如刻意复用某个 `previous_response_id`），
    /// 拿它做跨轮次的风险累积判定，等于把判定权交给了调用方。
    #[test]
    fn is_trusted_link_is_explicit_returns_true() {
        assert!(ConversationLink::Explicit.is_trusted());
    }

    #[test]
    fn is_trusted_link_is_inferred_or_none_returns_false() {
        assert!(!ConversationLink::Inferred.is_trusted());
        assert!(!ConversationLink::None.is_trusted());
    }

    #[test]
    fn root_no_client_context_generates_two_distinct_v7_ids_and_no_conversation() {
        let ctx = TraceContext::root();
        assert_ne!(ctx.request_id, ctx.trace_id);
        assert_eq!(ctx.request_id.get_version_num(), 7);
        assert_eq!(ctx.conversation_link, ConversationLink::None);
        assert!(ctx.conversation_id.is_none());
    }

    /// 客户端可以决定 `trace_id`（用于跨服务串联），但绝不能决定 `request_id`
    /// —— 它必须是全局唯一的，是审计与对账的锚点。
    #[test]
    fn with_trace_id_client_supplies_trace_id_keeps_it_but_still_generates_request_id() {
        let client_trace = Uuid::now_v7();
        let ctx = TraceContext::with_trace_id(client_trace);
        assert_eq!(ctx.trace_id, client_trace);
        assert_ne!(ctx.request_id, client_trace);
        assert_eq!(ctx.request_id.get_version_num(), 7);
    }

    #[test]
    fn with_conversation_given_explicit_link_sets_id_and_marks_it_trusted() {
        let cid = Uuid::now_v7();
        let ctx = TraceContext::root().with_conversation(cid, ConversationLink::Explicit);
        assert_eq!(ctx.conversation_id, Some(cid));
        assert!(ctx.conversation_link.is_trusted());
    }
}
