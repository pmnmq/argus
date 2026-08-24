//! 只有一条 Profile 的静态注册表。
//!
//! 配置系统是 F11 的活（`ConfigSnapshot` + 热更新）。在它到位之前，
//! [`ProfileRegistry`] 这个 trait 需要一个能跑的实现 —— 否则 R02 的
//! Resolver 有接口没有调用方，F06 就串不起来。
//!
//! F11 落地后给 `ConfigSnapshot` impl 一下 [`ProfileRegistry`] 即可，
//! Resolver 与主链路一行都不用改 —— 这正是当初把它收成 trait 的理由。

use std::sync::Arc;

use argus_core::{
    CredentialMode, ObserveMode, ObserverKind, ProviderProfileId, RetryPolicy, Timeouts,
};

use super::{ProfileRegistry, ProfileView};

/// 一条写死的 Provider Profile。
#[derive(Clone, Debug)]
pub struct StaticRegistry {
    id: ProviderProfileId,
    name: String,
    base_url: String,
    enabled: bool,
    credential: CredentialMode,
    observer: ObserverKind,
    observe: ObserveMode,
    timeouts: Timeouts,
    retry: Arc<RetryPolicy>,
}

impl StaticRegistry {
    /// `base_url` 后面**绝不能带 query 或 fragment**（`docs/03` §5）：
    /// 那两样东西会把后面拼上去的 path 整个吞掉，而这是配置错误，
    /// 不是运行期该兜的事（`specs/F22` R28 在配置校验期拦）。
    #[must_use]
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: ProviderProfileId::generate(),
            name: name.into(),
            base_url: base_url.into(),
            enabled: true,
            credential: CredentialMode::None,
            observer: ObserverKind::AnthropicMessages,
            observe: ObserveMode::Full,
            timeouts: Timeouts::default(),
            retry: Arc::new(RetryPolicy::default()),
        }
    }

    #[must_use]
    pub fn with_credential(mut self, credential: CredentialMode) -> Self {
        self.credential = credential;
        self
    }

    #[must_use]
    pub const fn with_observe_mode(mut self, observe: ObserveMode) -> Self {
        self.observe = observe;
        self
    }

    #[must_use]
    pub const fn with_observer(mut self, observer: ObserverKind) -> Self {
        self.observer = observer;
        self
    }

    #[must_use]
    pub const fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl ProfileRegistry for StaticRegistry {
    fn lookup(&self, name: &str) -> Option<ProfileView<'_>> {
        (name == self.name).then(|| ProfileView {
            id: self.id,
            name: &self.name,
            base_url: &self.base_url,
            enabled: self.enabled,
            credential: &self.credential,
            observer: self.observer,
            observe: self.observe,
            timeouts: self.timeouts,
            retry: &self.retry,
        })
    }
}
