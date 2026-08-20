//! `observer_rules`：Control Plane 里的一张**有序**规则表。
//!
//! 首个命中生效，没有权重也没有打分 —— **可预测比聪明重要**
//! （`docs/09-observers.md` §4.2）。一张能打分的规则表在出问题时没人说得清
//! 为什么是这条赢了。
//!
//! 规则里的全部维度都必须在**请求时**就能读到，因为前三级判定发生在
//! 发出请求之前（§5）。所以 `content_type` 指的是**请求**的，不是响应的；
//! 响应的 Content-Type 是第四级兜底，不参与规则匹配。

use argus_core::{ObserverKind, ResolvedTarget};
use compact_str::CompactString;
use http::Method;

/// 请求侧的事实，`ResolvedTarget` 里没有的那部分。
#[derive(Copy, Clone, Debug)]
pub struct RequestFacts<'a> {
    pub method: &'a Method,
    /// **请求**的 Content-Type，已去掉 `; charset=…` 之类的参数
    pub content_type: Option<&'a str>,
}

/// 一条规则要匹配的全部信息。
#[derive(Copy, Clone, Debug)]
pub struct SelectCtx<'a> {
    /// Managed 模式下是 Profile 名；Raw 模式为 `None`
    pub profile: Option<&'a str>,
    /// 上游 host，不含端口与方括号
    pub host: &'a str,
    /// 上游 path，**原始字节**（宪法 C35）
    pub path: &'a str,
    pub method: &'a Method,
    pub content_type: Option<&'a str>,
}

impl<'a> SelectCtx<'a> {
    /// 从解析结果与请求事实拼出匹配上下文。
    #[must_use]
    pub fn new(target: &'a ResolvedTarget, req: &RequestFacts<'a>) -> Self {
        let authority = target.uri.authority();
        Self {
            profile: target.origin.profile_name(),
            host: authority.map_or("", |a| strip_brackets(a.host())),
            path: target.uri.path(),
            method: req.method,
            content_type: req.content_type,
        }
    }
}

/// IPv6 字面量在 authority 里带方括号，规则写的是不带的。
fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// 一条规则。所有 `Some` 的维度之间是 **AND**；`None` 表示不约束。
///
/// 全 `None` 的规则匹配一切 —— 那是合法的（相当于兜底规则），但它必须放在
/// 表的最后，否则后面的规则永远轮不到。
#[derive(Clone, Debug)]
pub struct ObserverRule {
    pub profile: Option<CompactString>,
    /// 匹配 host 的后缀。`api.anthropic.com` 既匹配它自己也匹配子域
    pub host_suffix: Option<CompactString>,
    pub path_prefix: Option<CompactString>,
    pub method: Option<Method>,
    /// **请求**的 Content-Type
    pub content_type: Option<CompactString>,
    pub observer: ObserverKind,
}

impl ObserverRule {
    /// 只指定 observer 的空规则，用 `with_*` 逐个加约束。
    #[must_use]
    pub fn new(observer: ObserverKind) -> Self {
        Self {
            profile: None,
            host_suffix: None,
            path_prefix: None,
            method: None,
            content_type: None,
            observer,
        }
    }

    #[must_use]
    pub fn with_profile(mut self, profile: &str) -> Self {
        self.profile = Some(profile.into());
        self
    }

    #[must_use]
    pub fn with_host_suffix(mut self, suffix: &str) -> Self {
        self.host_suffix = Some(suffix.into());
        self
    }

    #[must_use]
    pub fn with_path_prefix(mut self, prefix: &str) -> Self {
        self.path_prefix = Some(prefix.into());
        self
    }

    #[must_use]
    pub fn with_method(mut self, method: Method) -> Self {
        self.method = Some(method);
        self
    }

    #[must_use]
    pub fn with_content_type(mut self, content_type: &str) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// 所有已指定的维度都要满足。
    #[must_use]
    pub fn matches(&self, ctx: &SelectCtx<'_>) -> bool {
        if let Some(want) = &self.profile
            && ctx.profile.is_none_or(|p| !want.eq_ignore_ascii_case(p))
        {
            return false;
        }
        if let Some(want) = &self.host_suffix
            && !host_matches(ctx.host, want)
        {
            return false;
        }
        if let Some(want) = &self.path_prefix
            && !ctx.path.starts_with(want.as_str())
        {
            return false;
        }
        if let Some(want) = &self.method
            && want != ctx.method
        {
            return false;
        }
        if let Some(want) = &self.content_type
            && ctx.content_type.is_none_or(|ct| !want.eq_ignore_ascii_case(ct))
        {
            return false;
        }
        true
    }
}

/// `api.anthropic.com` 匹配它自己与它的子域，但不匹配 `notapi.anthropic.com`。
fn host_matches(host: &str, suffix: &str) -> bool {
    if host.eq_ignore_ascii_case(suffix) {
        return true;
    }
    host.len() > suffix.len()
        && host.get(host.len() - suffix.len() - 1..).is_some_and(|tail| {
            tail.starts_with('.') && tail.get(1..).is_some_and(|s| s.eq_ignore_ascii_case(suffix))
        })
}

/// 有序规则表。
#[derive(Clone, Debug, Default)]
pub struct ObserverRules {
    rules: Vec<ObserverRule>,
}

impl ObserverRules {
    #[must_use]
    pub fn new(rules: Vec<ObserverRule>) -> Self {
        Self { rules }
    }

    /// 首个命中生效。后面还有更"精确"的规则也不生效 —— 顺序就是优先级。
    #[must_use]
    pub fn select(&self, ctx: &SelectCtx<'_>) -> Option<ObserverKind> {
        self.rules.iter().find(|r| r.matches(ctx)).map(|r| r.observer)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }
}
