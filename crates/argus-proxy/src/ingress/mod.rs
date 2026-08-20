//! Ingress：把进来的 URL 变成 [`ResolvedTarget`]，**一个字节都不改**。
//!
//! 路由表是静态的，只有三条（`docs/06-data-plane.md` §6.1）：
//!
//! | path 前缀 | 解析器 | 产出 |
//! |---|---|---|
//! | `/provider/{name}/…` | [`ManagedResolver`] | `origin: Managed` |
//! | `/raw/…` | [`RawResolver`] | `origin: Raw` |
//! | `/healthz` `/readyz` `/metrics` | —— | 运维端点，在这一层之前就被分流 |
//!
//! **三条硬约束**（宪法 C33/C35，`docs/06` §6.1）：
//!
//! 1. 只读 `uri().path()` / `uri().query()` 的原始字节。禁止 `axum::extract::Path`
//!    这类会百分号解码的提取器 —— 实测它把 `%2F` 解成 `/`，路径段数都变了，
//!    上游收到的是另一个资源，**而且不报错**（`docs/04` §5.2）。
//!    `clippy.toml` 的 `disallowed-types` 挡着它，`tests/resolver_matrix.rs`
//!    里还有一个真的会失败的用例证明这不是空谈。
//! 2. 不做 body 内容嗅探，path 是唯一依据。
//! 3. 不看 `model` 字段，它对 Data Plane 是不透明数据。
//!
//! 设计依据：`docs/03-managed-provider-mode.md` §4–7、
//! `docs/04-raw-proxy-mode.md` §4–5、`docs/06-data-plane.md` §6.1。

pub mod managed;
pub mod path;
pub mod raw;

use std::sync::Arc;

use argus_core::{
    CredentialMode, ObserverKind, ProviderProfileId, ProxyError, ResolvedTarget, RetryPolicy,
    Timeouts,
};
use http::request::Parts;

pub use self::{
    managed::ManagedResolver,
    path::{is_valid_profile_name, split_first_segment},
    raw::{RawForm, RawResolver, detect_raw_form},
};

/// Managed 模式的 path 前缀，长 10。
pub const MANAGED_PREFIX: &str = "/provider/";
/// Raw 模式的 path 前缀，长 5。
pub const RAW_PREFIX: &str = "/raw/";

/// URL → [`ResolvedTarget`]。
///
/// 两个实现（[`ManagedResolver`] / [`RawResolver`]）产出的是**同一个**类型，
/// 之后的代码路径完全一致 —— 不写两套代理实现，因为两套实现必然在透明性上
/// 出现细微分歧（`docs/02-routing-model.md` §5）。
pub trait TargetResolver {
    /// 只读 `parts.uri` 的原始字节。返回值里的 `uri` 已经是完整上游地址。
    ///
    /// # Errors
    ///
    /// path 不属于本解析器、profile 不存在或被禁用、URL 形式非法时返回对应的
    /// [`ProxyError`]；具体码见各实现。
    fn resolve(
        &self,
        parts: &Parts,
        profiles: &dyn ProfileRegistry,
    ) -> Result<ResolvedTarget, ProxyError>;
}

/// Resolver 需要的 Provider Profile 字段，**只有这些**。
///
/// 配置的加载与缓存属于 F11（`ConfigSnapshot`），那是 M1；R02 在 M0。把
/// Resolver 真正依赖的东西收成一个 trait，R02 就能自洽收口而不必把 F11 提前
/// 拽进来。F11 落地后给 `ConfigSnapshot` impl 一下即可，Resolver 一行不用改。
pub trait ProfileRegistry {
    /// 按 URL 里出现的那个名字查 Profile。名字已经过字符集校验。
    fn lookup(&self, name: &str) -> Option<ProfileView<'_>>;
}

/// [`ProfileRegistry::lookup`] 的返回值：完整 `ProviderProfile`
/// （`docs/08-provider-profiles.md` §4）在 Resolver 这一层用得到的那个子集。
///
/// 注意这里**没有** `provider_type` —— Data Plane 不得据它分支（宪法 C34）。
#[derive(Clone, Debug)]
pub struct ProfileView<'a> {
    pub id: ProviderProfileId,
    /// URL 里出现的那个名字，会冗余进审计
    pub name: &'a str,
    /// 尾部斜杠随便写，拼接时统一 `trim_end_matches('/')`（`docs/03` §5）
    pub base_url: &'a str,
    pub enabled: bool,
    pub credential: &'a CredentialMode,
    /// 默认 observer。R04 的 `observer_rules` 会在此基础上细化
    pub observer: ObserverKind,
    pub timeouts: Timeouts,
    pub retry: &'a Arc<RetryPolicy>,
}

/// 按 path 前缀分发到对应的解析器。
///
/// 运维端点（`/healthz` 等）不到这里 —— 它们在 Gatekeeper 之前就被分流。
///
/// # Errors
///
/// 前缀两条都不匹配时返回 [`ProxyError::ProviderNotFound`]（404）。其余错误
/// 由具体解析器给出。
pub fn resolve_target(
    parts: &Parts,
    profiles: &dyn ProfileRegistry,
) -> Result<ResolvedTarget, ProxyError> {
    let path = parts.uri.path();
    if path.starts_with(MANAGED_PREFIX) {
        ManagedResolver.resolve(parts, profiles)
    } else if path.starts_with(RAW_PREFIX) {
        RawResolver.resolve(parts, profiles)
    } else {
        Err(ProxyError::ProviderNotFound)
    }
}

/// 拼好的 target 必须是绝对 URL。
///
/// `Uri` 会愉快地把 `/v1/messages` 解析成合法的 origin-form，所以配置里一个
/// 写错的 `base_url` 能一路静默地变成「往自己身上发请求」。这一步是那种
/// 静默失败的最后一道门。
///
/// 注意 `scheme().is_some()` **不够**：实测 `Uri` 会把 `://x.com` 解析成
/// scheme 为**空串**、authority 为 `x.com` 的 URI —— 看着绝对，实际连不上
/// 任何东西。所以要看的是 scheme 串非空。
///
/// # Errors
///
/// 缺 scheme（含空 scheme）或缺 authority 时返回 `on_relative`。
pub(crate) fn ensure_absolute(
    uri: http::Uri,
    on_relative: ProxyError,
) -> Result<http::Uri, ProxyError> {
    if is_absolute(&uri) {
        Ok(uri)
    } else {
        Err(on_relative)
    }
}

/// 有**非空** scheme 且有 authority。
pub(crate) fn is_absolute(uri: &http::Uri) -> bool {
    uri.scheme().is_some_and(|s| !s.as_str().is_empty()) && uri.authority().is_some()
}

/// 把未解码的 query 原样接到 `out` 后面。
///
/// 不解析成 map、不重新序列化：那会改变参数顺序、丢掉重复 key、把 `a%2Bb`
/// 变成 `a+b`，这三种改动都真实地破坏过供应商的签名校验（`docs/03` §6）。
/// `Some("")` 对应 URL 末尾一个光秃秃的 `?`，也要原样保留。
pub(crate) fn push_query(out: &mut String, query: Option<&str>) {
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
}
