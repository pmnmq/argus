//! 解析后的上游目标。
//!
//! Managed 与 Raw 两种模式的解析器产出**同一个** [`ResolvedTarget`]，
//! 之后的代码路径完全一致 —— 不写两套代理实现，因为两套实现必然在透明性上
//! 出现细微分歧，而这类分歧只会在生产里以「Raw 模式下某个 header 丢了」
//! 的形式暴露。见 `docs/02-routing-model.md` §5。

use core::fmt;
use std::sync::Arc;

use compact_str::CompactString;
use http::Uri;

use crate::{
    ids::ProviderProfileId,
    observe::{ObserveMode, ObserverKind},
    provider::ResolvedCredential,
    route::{RetryPolicy, Timeouts},
};

/// 上游地址是怎么定下来的。审计必须能区分这两者。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TargetOrigin {
    /// Managed 模式：由 Provider Profile 的 `base_url` 决定
    Managed {
        profile: ProviderProfileId,
        /// 冗余一份名字：Profile 被删掉之后，历史审计仍然要能读懂
        profile_name: CompactString,
    },
    /// Raw 模式：由客户端在 URL 里直接给出
    Raw,
}

impl TargetOrigin {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Managed { .. } => "managed",
            Self::Raw => "raw",
        }
    }

    #[must_use]
    pub const fn is_raw(&self) -> bool {
        matches!(self, Self::Raw)
    }

    /// Managed 模式的 Profile 名，Raw 模式返回 `None`。
    #[must_use]
    pub fn profile_name(&self) -> Option<&str> {
        match self {
            Self::Managed { profile_name, .. } => Some(profile_name),
            Self::Raw => None,
        }
    }
}

/// 上游凭据从哪来。
///
/// 手写 `Debug`（见下）：`Injected` 里装着解密后的凭据，而
/// [`ResolvedTarget`] 是 `derive(Debug)` 的 —— 泄漏会从这里发生
/// （宪法 C27）。
#[derive(Clone)]
pub enum CredentialMode {
    /// Argus 注入自己保管的供应商凭据（Managed 模式的主路径）
    Injected(Arc<ResolvedCredential>),
    /// 客户端自带上游凭据，Argus 原样透传，既不看也不存
    Passthrough,
    /// 上游不需要认证（本地模型常见）
    None,
}

impl CredentialMode {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Injected(_) => "injected",
            Self::Passthrough => "passthrough",
            Self::None => "none",
        }
    }

    /// 是否由 Argus 注入。用于「Raw + injected 必须配 allowlist」这条硬约束
    /// （`docs/13-ssrf-protection.md` §9）。
    #[must_use]
    pub const fn is_injected(&self) -> bool {
        matches!(self, Self::Injected(_))
    }
}

// `ResolvedCredential` 自己的 Debug 已经是 <redacted>，但这里仍然手写：
// derive 出来的 `Injected(ResolvedCredential { .. })` 会把结构暴露出来，
// 而且一旦将来有人给 ResolvedCredential 加了字段，泄漏面就变了。
// 手写之后，谁再加 #[derive(Debug)] 会因为重复实现而编译失败。
impl fmt::Debug for CredentialMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Injected(c) => f
                .debug_tuple("Injected")
                .field(&format_args!("id={} <redacted>", c.id))
                .finish(),
            Self::Passthrough => f.write_str("Passthrough"),
            Self::None => f.write_str("None"),
        }
    }
}

/// 一次上游尝试的全部决定。
///
/// 注意这里**没有** `logical_model` / `provider_model` / `binding_id`：
/// `model` 对 Data Plane 是不透明数据，原样待在 body 里
/// （`docs/02-routing-model.md` §4.2、宪法 C33）。
#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    /// 第几次尝试，从 0 开始。重试的是**同一个** Profile，v2 无跨 Profile fallback
    pub attempt: u8,
    pub origin: TargetOrigin,
    /// 完整上游 URL。hyper 直接消费，clone 是引用计数
    pub uri: Uri,
    pub credential: CredentialMode,
    /// 观测器选择。Raw 模式或认不出协议时可能是 `GenericSse` / `GenericJson`
    pub observer: ObserverKind,
    pub timeouts: Timeouts,
    pub retry: Arc<RetryPolicy>,
    /// 采样结果，整条流固定
    pub observe: ObserveMode,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use http::HeaderName;

    use super::*;
    use crate::{ids::CredentialId, provider::CredentialKind};

    const SECRET: &str = "sk-live-0123456789abcdef";

    fn injected() -> CredentialMode {
        CredentialMode::Injected(Arc::new(
            ResolvedCredential::new(
                CredentialId::generate(),
                CredentialKind::ApiKeyHeader,
                HeaderName::from_static("x-api-key"),
                SECRET,
            )
            .expect("固定的合法值"),
        ))
    }

    fn target(origin: TargetOrigin, credential: CredentialMode) -> ResolvedTarget {
        ResolvedTarget {
            attempt: 0,
            origin,
            uri: Uri::from_static("https://api.anthropic.com/v1/messages"),
            credential,
            observer: ObserverKind::AnthropicMessages,
            timeouts: Timeouts::default(),
            retry: Arc::new(RetryPolicy::default()),
            observe: ObserveMode::Full,
        }
    }

    fn managed() -> TargetOrigin {
        TargetOrigin::Managed {
            profile: ProviderProfileId::generate(),
            profile_name: "anthropic-prod".into(),
        }
    }

    /// 泄漏几乎从不是「有人打印了 credential」，而是「有人打印了包含它的
    /// `ResolvedTarget`」。这条防线完全依赖 `CredentialMode` 的手写 Debug。
    #[test]
    fn debug_resolved_target_carrying_a_credential_renders_redacted() {
        let rendered = format!("{:?}", target(managed(), injected()));
        assert!(!rendered.contains(SECRET), "凭据泄漏了：{rendered}");
        assert!(!rendered.contains("0123456789"), "凭据片段泄漏了");
        assert!(rendered.contains("<redacted>"));
        // 非敏感字段仍然要能看见，否则这个 Debug 就没用了
        assert!(rendered.contains("anthropic-prod"));
        assert!(rendered.contains("api.anthropic.com"));
    }

    #[test]
    fn debug_credential_mode_is_passthrough_renders_the_variant_name() {
        assert_eq!(format!("{:?}", CredentialMode::Passthrough), "Passthrough");
        assert_eq!(format!("{:?}", CredentialMode::None), "None");
    }

    #[test]
    fn is_raw_origin_is_raw_returns_true() {
        assert!(TargetOrigin::Raw.is_raw());
        assert!(!managed().is_raw());
    }

    #[test]
    fn profile_name_origin_is_raw_returns_none() {
        assert_eq!(TargetOrigin::Raw.profile_name(), None);
        assert_eq!(managed().profile_name(), Some("anthropic-prod"));
    }

    /// 「Raw + injected 必须配 allowlist」这条硬约束靠它判断
    /// （`docs/13-ssrf-protection.md` §9）。
    #[test]
    fn is_injected_mode_is_injected_returns_true() {
        assert!(injected().is_injected());
        assert!(!CredentialMode::Passthrough.is_injected());
        assert!(!CredentialMode::None.is_injected());
    }

    #[test]
    fn as_str_across_all_credential_modes_returns_distinct_labels() {
        let labels = [
            injected().as_str(),
            CredentialMode::Passthrough.as_str(),
            CredentialMode::None.as_str(),
        ];
        let mut deduped = labels.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(deduped.len(), 3);
    }
}
