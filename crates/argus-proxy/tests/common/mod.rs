//! Resolver 测试夹具：一个最小的 [`ProfileRegistry`]，以及从 URL 造 `Parts`。
//!
//! 这里刻意**不**依赖配置系统（F11）。R02 需要的只是「按名字拿到 `base_url`」，
//! 用一个 Vec 就够了 —— 让测试跑得动的前提越少，spec 的边界就越清楚。

#![allow(dead_code, reason = "两个测试 binary 各用一部分")]

use std::sync::Arc;

use argus_core::{CredentialMode, ObserverKind, ProviderProfileId, RetryPolicy, Timeouts};
use argus_proxy::ingress::{ProfileRegistry, ProfileView};
use http::{Request, request::Parts};

/// 一条测试 Profile。`ProviderProfile`（`docs/08` §4）在 R02 用得到的那部分。
pub struct TestProfile {
    pub id: ProviderProfileId,
    pub name: String,
    pub base_url: String,
    pub enabled: bool,
    pub credential: CredentialMode,
    pub observer: ObserverKind,
    pub timeouts: Timeouts,
    pub retry: Arc<RetryPolicy>,
}

impl TestProfile {
    pub fn new(name: &str, base_url: &str) -> Self {
        Self {
            id: ProviderProfileId::generate(),
            name: name.to_owned(),
            base_url: base_url.to_owned(),
            enabled: true,
            credential: CredentialMode::None,
            observer: ObserverKind::OpenAiChat,
            timeouts: Timeouts::default(),
            retry: Arc::new(RetryPolicy::default()),
        }
    }

    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// 最小的 Profile 注册表。线性查找，测试里够用。
#[derive(Default)]
pub struct TestProfiles(Vec<TestProfile>);

impl TestProfiles {
    #[must_use]
    pub fn of(profiles: Vec<TestProfile>) -> Self {
        Self(profiles)
    }

    /// 只有一条 Profile 的注册表，绝大多数用例用它。
    #[must_use]
    pub fn one(name: &str, base_url: &str) -> Self {
        Self(vec![TestProfile::new(name, base_url)])
    }
}

impl ProfileRegistry for TestProfiles {
    fn lookup(&self, name: &str) -> Option<ProfileView<'_>> {
        self.0.iter().find(|p| p.name == name).map(|p| ProfileView {
            id: p.id,
            name: &p.name,
            base_url: &p.base_url,
            enabled: p.enabled,
            credential: &p.credential,
            observer: p.observer,
            timeouts: p.timeouts,
            retry: &p.retry,
        })
    }
}

/// 从一个 request-target 造出 `Parts`。
///
/// 走的是 `http` 自己的 `Uri` 解析 —— 也就是 hyper 交给我们的那条路径，
/// 因此 `uri.path()` 拿到的与线上完全一致（`docs/04` §5.1 的实测结论）。
///
/// # Panics
///
/// `target` 不是合法的 request-target 时 panic。测试里传的都是常量。
#[must_use]
pub fn parts(target: &str) -> Parts {
    Request::builder()
        .uri(target)
        .body(())
        .expect("测试用的都是合法 request-target")
        .into_parts()
        .0
}
