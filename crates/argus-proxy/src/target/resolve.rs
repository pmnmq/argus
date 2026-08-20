//! DNS 解析 + 逐 IP 判定 + 缓存。
//!
//! 整个模块只为一件事存在：**只解析一次 DNS，然后连那个 IP**。
//! 「校验时解析一次、连接时再解析一次」中间那个窗口就是 DNS rebinding
//! 的全部（`docs/13-ssrf-protection.md` §6）。
//!
//! 缓存的是「hostname → **已判定通过**的地址集合」，不是原始 DNS 结果 ——
//! 差别在于：前者过期只影响可用性，后者过期会影响安全。

use core::{future::Future, net::SocketAddr, pin::Pin, time::Duration};
use std::{io, sync::Arc};

use argus_core::ProxyError;
use compact_str::CompactString;
use moka::sync::Cache;
use smallvec::SmallVec;

use super::policy::{HostVerdict, TargetPolicy, parse_host_ip};

/// 一个 target 通常只有几个地址，栈上放得下。
pub type Addrs = SmallVec<[SocketAddr; 4]>;

/// DNS 解析器。抽成 trait 只为一个目的：测试要能构造出
/// 「第二次解析返回不同 IP」的 rebinding 场景（R03 T8）。
pub trait DnsResolver: Send + Sync + 'static {
    /// 解析 `host:port`。返回空集合视为解析失败。
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Addrs>> + Send + 'a>>;
}

/// 系统解析器（`getaddrinfo`），带超时。
///
/// 拿不到记录自身的 TTL —— 这是 `getaddrinfo` 的限制，不是偷懒。要做到
/// `min(记录 TTL, 30s)` 得引入一个自己发 DNS 报文的解析器，第一阶段不引入，
/// 理由见 `docs/13` §6.1 的实现约束。
#[derive(Clone, Debug)]
pub struct SystemResolver {
    timeout: Duration,
}

impl SystemResolver {
    /// `docs/13` §11：DNS 超时默认 2s。
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TIMEOUT)
    }
}

impl DnsResolver for SystemResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = io::Result<Addrs>> + Send + 'a>> {
        let timeout = self.timeout;
        Box::pin(async move {
            let lookup = tokio::net::lookup_host((host, port));
            match tokio::time::timeout(timeout, lookup).await {
                Ok(Ok(addrs)) => Ok(addrs.collect()),
                Ok(Err(e)) => Err(e),
                Err(_elapsed) => Err(io::Error::new(io::ErrorKind::TimedOut, "dns timeout")),
            }
        })
    }
}

/// 解析 + 判定 + 缓存。
///
/// 判定顺序严格按 `docs/13` §4：hostname 规则 → DNS → 逐 IP → 端口。
/// 把端口挪到最前面能省掉一次注定失败请求的 DNS 往返，但那会改掉 R1 钉住的
/// 顺序，也会让「被拒」和「解析不出来」在时间上更容易区分（OQ-S3）。
pub struct CheckedResolver {
    dns: Arc<dyn DnsResolver>,
    /// key 是 `(host, port)`：同一个名字不同端口是不同的判定结果
    cache: Cache<(CompactString, u16), Addrs>,
}

impl CheckedResolver {
    /// `docs/13` §6.1：TTL 上限 30s。
    pub const CACHE_TTL: Duration = Duration::from_secs(30);
    const CACHE_CAPACITY: u64 = 4096;

    #[must_use]
    pub fn new(dns: Arc<dyn DnsResolver>) -> Self {
        Self::with_ttl(dns, Self::CACHE_TTL)
    }

    #[must_use]
    pub fn with_ttl(dns: Arc<dyn DnsResolver>, ttl: Duration) -> Self {
        Self {
            dns,
            cache: Cache::builder().max_capacity(Self::CACHE_CAPACITY).time_to_live(ttl).build(),
        }
    }

    /// 只解析一次，返回**全部通过判定**的地址。
    ///
    /// # Errors
    ///
    /// | 情况 | 错误 | status |
    /// |---|---|---|
    /// | 主机名命中 `deny_hosts` / 不允许的字面 IP | `TargetDenied` | 403 |
    /// | 解析出的 IP 里**任意一个**不合规 | `TargetDenied` | 403 |
    /// | 端口在 denylist 上 | `TargetDenied` | 403 |
    /// | DNS 解析失败、超时或返回空 | `TargetDnsFailure` | 502 |
    ///
    /// 前三种返回**同一个** 403 与同一段 body，不带任何 IP —— 区分开就等于
    /// 把 Argus 变成一台内网存活探测器（`docs/13` §8）。DNS 失败之所以能用
    /// 不同的码，是因为它不泄漏策略信息。
    pub async fn resolve_and_check(
        &self,
        host: &str,
        port: u16,
        policy: &TargetPolicy,
    ) -> Result<Addrs, ProxyError> {
        let started = std::time::Instant::now();
        let verdict = policy.check_host(host);
        let result = self.resolve_and_check_inner(host, port, policy, verdict).await;
        metrics::histogram!("argus_target_policy_duration_us").record(micros(started.elapsed()));
        result
    }

    async fn resolve_and_check_inner(
        &self,
        host: &str,
        port: u16,
        policy: &TargetPolicy,
        verdict: HostVerdict,
    ) -> Result<Addrs, ProxyError> {
        if verdict == HostVerdict::Deny {
            return Err(ProxyError::TargetDenied);
        }
        // `allow_hosts` 命中就跳过逐 IP 判定（§5.2 的优先级），但**不**跳过端口
        let judge_ips = verdict == HostVerdict::CheckIps;

        // 字面 IP 根本不经过 DNS —— 也就没有 rebinding 可言
        if let Some(ip) = parse_host_ip(host) {
            if judge_ips && !policy.check_ip(ip) {
                return Err(ProxyError::TargetDenied);
            }
            return finish(
                port,
                policy,
                SmallVec::from_slice(&[SocketAddr::new(ip, port)]),
            );
        }

        let key = (CompactString::from(host), port);
        if let Some(cached) = self.cache.get(&key) {
            metrics::counter!("argus_dns_cache_hits_total").increment(1);
            return finish(port, policy, cached);
        }
        metrics::counter!("argus_dns_cache_misses_total").increment(1);

        let started = std::time::Instant::now();
        let addrs = self.dns.resolve(host, port).await.map_err(|_| ProxyError::TargetDnsFailure)?;
        metrics::histogram!("argus_dns_resolve_duration_us").record(micros(started.elapsed()));

        if addrs.is_empty() {
            return Err(ProxyError::TargetDnsFailure);
        }

        // **有任一不合规就整体拒绝**，而不是挑合规的连。后者会让攻击者用
        // `[1.2.3.4, 127.0.0.1]` 这样的多值 A 记录来碰运气（`docs/13` §6）
        if judge_ips && !addrs.iter().all(|a| policy.check_ip(a.ip())) {
            return Err(ProxyError::TargetDenied);
        }

        self.cache.insert(key, addrs.clone());
        finish(port, policy, addrs)
    }

    /// 清掉一个名字的缓存。热更新策略之后必须调用，否则旧判定会继续生效。
    pub fn invalidate(&self, host: &str, port: u16) {
        self.cache.invalidate(&(CompactString::from(host), port));
    }
}

impl core::fmt::Debug for CheckedResolver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CheckedResolver")
            .field("cached", &self.cache.entry_count())
            .finish_non_exhaustive()
    }
}

/// 判定链最后一段：端口。
///
/// 端口 denylist **不是**安全边界，只是降低误伤 —— 真正的边界是前面的
/// 逐 IP 判定（`docs/13` §12）。
fn finish(port: u16, policy: &TargetPolicy, addrs: Addrs) -> Result<Addrs, ProxyError> {
    if policy.check_port(port) {
        Ok(addrs)
    } else {
        Err(ProxyError::TargetDenied)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "微秒数进直方图，f64 的 53 位尾数够到 285 年"
)]
fn micros(d: Duration) -> f64 {
    d.as_micros() as f64
}
