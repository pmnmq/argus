//! Target Policy：让 Argus 不成为一台开放的请求转发器，同时不妨碍它代理
//! NAS 上的本地模型。
//!
//! 判定链（`docs/13-ssrf-protection.md` §4），顺序是规范的一部分：
//!
//! ```text
//! scheme → hostname 规则 → DNS 解析 → 逐 IP 判定 → 端口 → 连接时确认
//! ```
//!
//! 这条链**只跑一次**：Argus 不跟随 redirect，3xx 原样透传给客户端
//! （`docs/04-raw-proxy-mode.md` §8），所以不存在"第二跳"要重跑。
//!
//! 两个关键点都在链条下半段：
//!
//! 1. **判定的对象是 IP，不是主机名。** 主机名可以指向任何地方。
//! 2. **连接时再确认一次。** 这是防 DNS rebinding 的唯一可靠办法。
//!
//! Managed 模式**同样**走这条链（§10）：配置可能把 `base_url` 写成
//! `http://169.254.169.254`，而 Control Plane 被攻破时这是第二道防线。
//!
//! > 这不是唯一一道防线。生产部署应同时用网络层隔离（K8s NetworkPolicy、
//! > 安全组）限制 Argus 的出站范围（§12）。

pub mod cidr;
pub mod connector;
pub mod policy;
pub mod resolve;

use argus_core::ProxyError;
use http::Uri;

pub use self::{
    cidr::{Cidr, CidrParseError, CidrSet},
    connector::PinnedConnector,
    policy::{HostVerdict, PolicyError, TargetPolicy, TargetPolicyConfig},
    resolve::{Addrs, CheckedResolver, DnsResolver, SystemResolver},
};

/// 跑完整条判定链，产出一个只连已判定地址的 [`PinnedConnector`]。
///
/// # Errors
///
/// | 情况 | 错误 | status |
/// |---|---|---|
/// | 缺 scheme / authority | `InvalidRawTarget` | 400 |
/// | scheme 不在白名单 | `SchemeNotAllowed` | 400 |
/// | 主机名、IP 或端口被策略拒 | `TargetDenied` | 403 |
/// | DNS 失败、超时或返回空 | `TargetDnsFailure` | 502 |
///
/// 三种**拒绝原因**返回的是同一个 403 与同一段 body，且不含任何 IP：
/// 区分开就等于把 Argus 变成一台内网存活探测器（§8）。DNS 失败能用不同的
/// 码，是因为它不泄漏策略信息。
pub async fn check_target(
    uri: &Uri,
    policy: &TargetPolicy,
    resolver: &CheckedResolver,
) -> Result<PinnedConnector, ProxyError> {
    let scheme = uri.scheme().ok_or(ProxyError::InvalidRawTarget)?;
    policy.check_scheme(scheme)?;

    let authority = uri.authority().ok_or(ProxyError::InvalidRawTarget)?;
    // IPv6 字面量在 authority 里带方括号，DNS 与 SNI 都不要那对括号
    let host = authority
        .host()
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or_else(|| authority.host());
    let port = authority.port_u16().unwrap_or_else(|| policy::default_port(scheme));

    let addrs = resolver.resolve_and_check(host, port, policy).await?;
    PinnedConnector::new(addrs, host)
}

/// `docs/13` §9 的硬约束，在**配置校验期**强制（R11，不是运行期）。
///
/// `allow_raw_proxy = true` 且凭据模式为 `injected` 时，调用方可以指定任意
/// target，而 Argus 会往上面注入自己保管的供应商凭据 —— 等于把 key 送到
/// 对方的服务器上。必须同时配一个非空的 allowlist。
///
/// # Errors
///
/// 组合成立且没有 allowlist 时返回
/// [`PolicyError::RawInjectedWithoutAllowlist`]。
pub fn check_raw_injected_combination(
    allow_raw_proxy: bool,
    credential_is_injected: bool,
    policy: &TargetPolicy,
) -> Result<(), PolicyError> {
    if allow_raw_proxy && credential_is_injected && !policy.has_allowlist() {
        return Err(PolicyError::RawInjectedWithoutAllowlist);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认组合是安全的：Raw 默认 passthrough 凭据（`docs/04` §9）。
    #[test]
    fn check_raw_injected_combination_default_setup_is_accepted() {
        let p = TargetPolicy::default();
        assert!(check_raw_injected_combination(false, false, &p).is_ok());
        assert!(check_raw_injected_combination(true, false, &p).is_ok());
        assert!(check_raw_injected_combination(false, true, &p).is_ok());
    }

    /// 危险组合 + 空 allowlist 必须在配置期就炸，而不是运行期。
    #[test]
    fn check_raw_injected_combination_without_an_allowlist_is_rejected() {
        assert_eq!(
            check_raw_injected_combination(true, true, &TargetPolicy::default()).unwrap_err(),
            PolicyError::RawInjectedWithoutAllowlist
        );
    }

    #[test]
    fn check_raw_injected_combination_with_an_allowlist_is_accepted() {
        let cfg = TargetPolicyConfig {
            allow_hosts: vec!["api.openai.com".into()],
            ..Default::default()
        };
        let p = TargetPolicy::compile(&cfg).expect("合法");
        assert!(check_raw_injected_combination(true, true, &p).is_ok());
    }
}
