//! Target Policy：判定链里不需要网络 I/O 的那几段。
//!
//! scheme → hostname 规则 → 逐 IP 的 CIDR 判定 → 端口。DNS 解析与连接时
//! 确认在 [`super::resolve`] 与 [`super::connector`]。
//!
//! 优先级（`docs/13-ssrf-protection.md` §5.2）：
//!
//! ```text
//! deny_hosts  >  allow_hosts  >  最长前缀匹配（同长时 allow 赢）  >  放行
//! ```
//!
//! 末项是**放行**：`deny_cidrs` 是黑名单，未命中任何规则的公网地址放行，
//! 否则默认配置下连 `api.openai.com` 都到不了。真正的白名单模式由
//! `allow_hosts` 提供，并在 Raw + injected 组合下被强制要求（§9、R11）。

use core::net::IpAddr;

use argus_core::ProxyError;
use compact_str::CompactString;
use http::uri::Scheme;
use serde::Deserialize;

use super::cidr::{CidrParseError, CidrSet, canonical};

/// hostname 规则的判定结果。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HostVerdict {
    /// 命中 `deny_hosts`，或是不被允许的字面 IP
    Deny,
    /// 命中 `allow_hosts` —— 按 §5.2 的优先级，**跳过** CIDR 判定。
    /// 这是「我知道我在干什么，放行我那台 NAS」的唯一正当写法
    Allow,
    /// 没命中主机名规则，交给解析出的 IP 逐个判
    CheckIps,
}

/// `docs/13` §5 的策略，编译成可直接判定的形状。
#[derive(Clone, Debug)]
pub struct TargetPolicy {
    schemes: Vec<CompactString>,
    allow_cidrs: CidrSet,
    deny_cidrs: CidrSet,
    allow_hosts: Vec<HostPattern>,
    deny_hosts: Vec<HostPattern>,
    /// 已排序，用二分查
    deny_ports: Vec<u16>,
    allow_literal_ip: bool,
}

/// 编译 [`TargetPolicyConfig`] 时的失败原因。只在配置加载期出现，
/// 按「启动严格」的原则一律让启动失败（`docs/13` §11）。
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("{field} 里的 `{entry}` 不是合法 CIDR：{source}")]
    Cidr {
        field: &'static str,
        entry: String,
        source: CidrParseError,
    },
    #[error("scheme 白名单为空，那样什么都到不了")]
    NoSchemes,
    #[error("scheme `{0}` 不受支持，只能是 http / https")]
    UnsupportedScheme(String),
    #[error("`{0}` 不是合法的 hostname 规则")]
    BadHostPattern(String),
    /// `docs/13` §9 的硬约束，见 [`super::check_raw_injected_combination`]
    #[error("allow_raw_proxy + injected 凭据必须同时配置非空的 allow_hosts 或 allow_cidrs")]
    RawInjectedWithoutAllowlist,
}

impl TargetPolicy {
    /// 把配置编译成策略。
    ///
    /// # Errors
    ///
    /// 见 [`PolicyError`]。任一条规则写错都整体失败并指出是哪一条 ——
    /// 一条被静默丢掉的 deny 规则就是一个洞。
    pub fn compile(cfg: &TargetPolicyConfig) -> Result<Self, PolicyError> {
        if cfg.schemes.is_empty() {
            return Err(PolicyError::NoSchemes);
        }
        for s in &cfg.schemes {
            if s != "http" && s != "https" {
                return Err(PolicyError::UnsupportedScheme(s.to_string()));
            }
        }

        let allow_cidrs = compile_cidrs("allow_cidrs", &cfg.allow_cidrs)?;
        let deny_cidrs = compile_cidrs("deny_cidrs", &cfg.deny_cidrs)?;

        let mut deny_ports = cfg.deny_ports.clone();
        deny_ports.sort_unstable();
        deny_ports.dedup();

        Ok(Self {
            schemes: cfg.schemes.clone(),
            allow_cidrs,
            deny_cidrs,
            allow_hosts: compile_hosts(&cfg.allow_hosts)?,
            deny_hosts: compile_hosts(&cfg.deny_hosts)?,
            deny_ports,
            allow_literal_ip: cfg.allow_literal_ip,
        })
    }

    /// 判定链第一段。
    ///
    /// # Errors
    ///
    /// scheme 不在白名单时返回 [`ProxyError::SchemeNotAllowed`]。
    /// 这一条**不**并进 `target_denied`：它不泄漏任何策略信息，
    /// 而且能让写错 URL 的人立刻明白问题在哪。
    pub fn check_scheme(&self, scheme: &Scheme) -> Result<(), ProxyError> {
        if self.schemes.iter().any(|s| s == scheme.as_str()) {
            Ok(())
        } else {
            Err(ProxyError::SchemeNotAllowed)
        }
    }

    /// 判定链第二段：DNS 解析**之前**先看主机名。
    #[must_use]
    pub fn check_host(&self, host: &str) -> HostVerdict {
        if self.deny_hosts.iter().any(|p| p.matches(host)) {
            return HostVerdict::Deny;
        }
        if self.allow_hosts.iter().any(|p| p.matches(host)) {
            return HostVerdict::Allow;
        }
        // 字面 IP 不经过 DNS，但仍然要过 CIDR 判定
        if !self.allow_literal_ip && parse_host_ip(host).is_some() {
            return HostVerdict::Deny;
        }
        HostVerdict::CheckIps
    }

    /// 判定链第三段：单个 IP 过不过。最长前缀匹配，同长时 allow 赢（R6）。
    #[must_use]
    pub fn check_ip(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        match (
            self.allow_cidrs.longest_match(ip),
            self.deny_cidrs.longest_match(ip),
        ) {
            // 同长时 allow 赢（R6）
            (Some(allow), Some(deny)) => allow >= deny,
            (None, Some(_)) => false,
            // 没命中任何 deny —— 绝大多数公网地址走这条，也就是 §5.2
            // 优先级链的末项：未命中即放行
            (_, None) => true,
        }
    }

    /// 判定链第四段。
    ///
    /// 端口 denylist **不是**安全边界，只是降低误伤：真正的边界是 IP 判定
    /// （`docs/13` §12）。
    #[must_use]
    pub fn check_port(&self, port: u16) -> bool {
        self.deny_ports.binary_search(&port).is_err()
    }

    /// 有没有配置过任何形式的 allowlist。
    ///
    /// `allow_raw_proxy` + `injected` 凭据这个组合必须配非空 allowlist，
    /// 否则调用方可以把 Argus 保管的 key 送到自己的服务器上
    /// （`docs/13` §9、R11）。这一条在**配置校验期**强制，不是运行期。
    #[must_use]
    pub fn has_allowlist(&self) -> bool {
        !self.allow_hosts.is_empty() || !self.allow_cidrs.is_empty()
    }
}

impl Default for TargetPolicy {
    /// `docs/13` §5 的默认策略。
    ///
    /// # Panics
    ///
    /// 不会。默认配置里的每条 CIDR 都有对应的单元测试。
    fn default() -> Self {
        #[allow(
            clippy::expect_used,
            reason = "默认策略是编译期常量，编译不过等于代码写错，有专门的测试盯着"
        )]
        Self::compile(&TargetPolicyConfig::default()).expect("默认策略必须合法")
    }
}

/// `docs/13` §5 的 TOML 形状。F22 会用它做 Profile 级的策略覆盖。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TargetPolicyConfig {
    pub schemes: Vec<CompactString>,
    /// 默认拒绝的网段。管理员可以在 `allow_cidrs` 里逐条放行
    pub deny_cidrs: Vec<String>,
    /// 显式放行，优先级**高于** `deny_cidrs`
    pub allow_cidrs: Vec<String>,
    /// `api.openai.com` 精确匹配；`.corp.com` 匹配它自己与全部子域
    pub allow_hosts: Vec<String>,
    pub deny_hosts: Vec<String>,
    pub deny_ports: Vec<u16>,
    /// target 能不能直接写字面 IP（而非主机名）
    pub allow_literal_ip: bool,
}

impl Default for TargetPolicyConfig {
    fn default() -> Self {
        Self {
            schemes: vec!["https".into(), "http".into()],
            deny_cidrs: [
                "127.0.0.0/8",
                // `::/96` 一条盖三样：未指定地址 `::`、回环 `::1`，以及已废弃的
                // IPv4-compatible 写法 `::a.b.c.d`。最后那个尤其要盖住 ——
                // 它长得像 v6，却指向一个 v4 地址
                "::/96",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "fc00::/7",
                // link-local。169.254.169.254（AWS/GCP/Azure 元数据）在这条里，
                // fd00:ec2::254 在上面的 fc00::/7 里。
                // OQ-S2：倾向把元数据地址做成不可被 allow_cidrs 覆盖的硬黑名单，
                // 第一阶段先只放进默认 deny
                "169.254.0.0/16",
                "fe80::/10",
                "100.64.0.0/10",
                "0.0.0.0/8",
                "224.0.0.0/4",
                "ff00::/8",
            ]
            .map(String::from)
            .to_vec(),
            allow_cidrs: Vec::new(),
            allow_hosts: Vec::new(),
            deny_hosts: Vec::new(),
            deny_ports: vec![
                22, 23, 25, 111, 135, 139, 445, 3306, 5432, 6379, 11211, 27017,
            ],
            allow_literal_ip: true,
        }
    }
}

/// hostname 规则。DNS 名大小写不敏感，比较也是。
#[derive(Clone, Debug)]
enum HostPattern {
    Exact(CompactString),
    /// 存的是带前导点的形式（`.corp.com`），匹配它自己与全部子域
    Suffix(CompactString),
}

impl HostPattern {
    fn parse(s: &str) -> Result<Self, PolicyError> {
        let s = s.trim();
        if s.is_empty() || s == "." {
            return Err(PolicyError::BadHostPattern(s.to_owned()));
        }
        Ok(if s.starts_with('.') {
            Self::Suffix(s.into())
        } else {
            Self::Exact(s.into())
        })
    }

    fn matches(&self, host: &str) -> bool {
        match self {
            Self::Exact(want) => want.eq_ignore_ascii_case(host),
            Self::Suffix(suffix) => {
                // `.corp.com` 既匹配 `corp.com` 本身，也匹配 `a.corp.com`
                let bare = &suffix[1..];
                if bare.eq_ignore_ascii_case(host) {
                    return true;
                }
                host.len() > suffix.len()
                    && host
                        .get(host.len() - suffix.len()..)
                        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
            }
        }
    }
}

fn compile_cidrs(field: &'static str, items: &[String]) -> Result<CidrSet, PolicyError> {
    CidrSet::parse_all(items.iter().map(String::as_str)).map_err(|(entry, source)| {
        PolicyError::Cidr {
            field,
            entry,
            source,
        }
    })
}

fn compile_hosts(items: &[String]) -> Result<Vec<HostPattern>, PolicyError> {
    items.iter().map(|s| HostPattern::parse(s)).collect()
}

/// 把 authority 里的 host 解析成 IP。IPv6 字面量带方括号，先剥掉。
#[must_use]
pub fn parse_host_ip(host: &str) -> Option<IpAddr> {
    let bare = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(host);
    bare.parse().ok().map(canonical)
}

/// 未显式给端口时按 scheme 取默认值。
#[must_use]
pub fn default_port(scheme: &Scheme) -> u16 {
    if scheme == &Scheme::HTTPS { 443 } else { 80 }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        canonical(s.parse().expect("固定的合法地址"))
    }

    fn policy(mutate: impl FnOnce(&mut TargetPolicyConfig)) -> TargetPolicy {
        let mut cfg = TargetPolicyConfig::default();
        mutate(&mut cfg);
        TargetPolicy::compile(&cfg).expect("测试策略必须合法")
    }

    /// 默认策略是一段写在代码里的常量，编译不过就等于 SSRF 防护整个没上。
    #[test]
    fn compile_the_documented_default_config_succeeds() {
        let p = TargetPolicy::default();
        assert!(!p.has_allowlist(), "默认不带 allowlist");
        assert_eq!(p.deny_cidrs.len(), 12);
    }

    /// `docs/20` §5.4 第一行：内网、回环、云元数据一律拒。
    #[test]
    fn check_ip_private_and_metadata_addresses_returns_false() {
        let p = TargetPolicy::default();
        for addr in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.20",
            "169.254.169.254", // AWS/GCP/Azure 元数据
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::",
            "fd00::1",
            "fd00:ec2::254", // 元数据的 v6 形式，在 fc00::/7 里
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!p.check_ip(ip(addr)), "{addr} 应该被拒");
        }
    }

    #[test]
    fn check_ip_public_addresses_returns_true() {
        let p = TargetPolicy::default();
        for addr in ["1.2.3.4", "104.18.0.1", "172.32.0.1", "2606:4700::1"] {
            assert!(p.check_ip(ip(addr)), "{addr} 应该放行");
        }
    }

    /// v4 deny 表必须挡住 IPv4-mapped 的 v6 写法，否则一条 AAAA 记录就绕过了。
    #[test]
    fn check_ip_ipv4_mapped_loopback_returns_false() {
        let p = TargetPolicy::default();
        assert!(!p.check_ip(ip("::ffff:127.0.0.1")));
        assert!(!p.check_ip(ip("::ffff:169.254.169.254")));
        assert!(!p.check_ip(ip("::ffff:192.168.1.20")));
    }

    /// `docs/13` §5.1 的正确写法：精确到单机放行，而不是整个 /16。
    #[test]
    fn check_ip_explicitly_allowed_host_wins_over_the_denied_network() {
        let p = policy(|c| c.allow_cidrs = vec!["192.168.1.20/32".into()]);
        assert!(p.check_ip(ip("192.168.1.20")), "显式放行的那一台");
        assert!(!p.check_ip(ip("192.168.1.21")), "同网段的其他机器仍然拒");
        assert!(p.has_allowlist());
    }

    /// R6：同长前缀时 allow 赢。
    #[test]
    fn check_ip_allow_and_deny_of_equal_prefix_lets_allow_win() {
        let p = policy(|c| {
            c.allow_cidrs = vec!["203.0.113.0/24".into()];
            c.deny_cidrs = vec!["203.0.113.0/24".into()];
        });
        assert!(p.check_ip(ip("203.0.113.7")));
    }

    /// 更长的 deny 也要能赢过更短的 allow —— 最长前缀匹配是双向的。
    #[test]
    fn check_ip_longer_deny_wins_over_shorter_allow() {
        let p = policy(|c| {
            c.allow_cidrs = vec!["10.0.0.0/8".into()];
            c.deny_cidrs = vec!["10.1.2.0/24".into()];
        });
        assert!(p.check_ip(ip("10.9.9.9")));
        assert!(!p.check_ip(ip("10.1.2.3")));
    }

    #[test]
    fn check_scheme_outside_the_allowlist_returns_scheme_not_allowed() {
        let p = policy(|c| c.schemes = vec!["https".into()]);
        assert_eq!(p.check_scheme(&Scheme::HTTPS), Ok(()));
        assert_eq!(
            p.check_scheme(&Scheme::HTTP).unwrap_err(),
            ProxyError::SchemeNotAllowed
        );
    }

    #[test]
    fn check_host_denied_host_beats_allowed_host() {
        let p = policy(|c| {
            c.allow_hosts = vec!["api.openai.com".into()];
            c.deny_hosts = vec!["api.openai.com".into()];
        });
        assert_eq!(p.check_host("api.openai.com"), HostVerdict::Deny);
    }

    /// `allow_hosts` 命中就跳过 CIDR 判定 —— 这是按名字放行 NAS 的唯一写法。
    #[test]
    fn check_host_allowed_host_returns_allow_and_skips_ip_checks() {
        let p = policy(|c| c.allow_hosts = vec!["nas.local".into()]);
        assert_eq!(p.check_host("nas.local"), HostVerdict::Allow);
        assert_eq!(p.check_host("other.local"), HostVerdict::CheckIps);
    }

    /// DNS 名大小写不敏感，规则匹配也是。
    #[test]
    fn check_host_pattern_matching_ignores_ascii_case() {
        let p = policy(|c| c.deny_hosts = vec!["Evil.COM".into()]);
        assert_eq!(p.check_host("evil.com"), HostVerdict::Deny);
        assert_eq!(p.check_host("EVIL.com"), HostVerdict::Deny);
    }

    /// `.corp.com` 匹配它自己与全部子域，但不匹配 `notcorp.com`。
    #[test]
    fn check_host_dotted_suffix_matches_the_domain_and_its_subdomains() {
        let p = policy(|c| c.deny_hosts = vec![".corp.com".into()]);
        assert_eq!(p.check_host("corp.com"), HostVerdict::Deny);
        assert_eq!(p.check_host("a.corp.com"), HostVerdict::Deny);
        assert_eq!(p.check_host("a.b.corp.com"), HostVerdict::Deny);
        assert_eq!(p.check_host("notcorp.com"), HostVerdict::CheckIps);
        assert_eq!(p.check_host("corp.com.evil.net"), HostVerdict::CheckIps);
    }

    #[test]
    fn check_host_literal_ip_when_disallowed_returns_deny() {
        let p = policy(|c| c.allow_literal_ip = false);
        assert_eq!(p.check_host("1.2.3.4"), HostVerdict::Deny);
        assert_eq!(p.check_host("[2001:db8::1]"), HostVerdict::Deny);
        assert_eq!(p.check_host("api.openai.com"), HostVerdict::CheckIps);
    }

    /// 字面 IP 允许时仍然要过 CIDR 判定，不是直接放行。
    #[test]
    fn check_host_literal_ip_when_allowed_still_defers_to_ip_checks() {
        let p = TargetPolicy::default();
        assert_eq!(p.check_host("127.0.0.1"), HostVerdict::CheckIps);
        assert!(!p.check_ip(ip("127.0.0.1")));
    }

    #[test]
    fn check_port_ports_on_the_denylist_returns_false() {
        let p = TargetPolicy::default();
        for port in [22, 3306, 6379, 27017] {
            assert!(!p.check_port(port), "{port} 应该被拒");
        }
        // 本地模型的常用端口不在默认 denylist 里（`docs/13` §5.1）
        for port in [80, 443, 8000, 11434] {
            assert!(p.check_port(port), "{port} 应该放行");
        }
    }

    #[test]
    fn compile_config_with_a_bad_cidr_reports_the_field_and_the_entry() {
        let cfg = TargetPolicyConfig {
            allow_cidrs: vec!["192.168.1.20/16".into()],
            ..Default::default()
        };
        let err = TargetPolicy::compile(&cfg).unwrap_err();
        assert!(matches!(
            err,
            PolicyError::Cidr {
                field: "allow_cidrs",
                ref entry,
                source: CidrParseError::HostBitsSet,
            } if entry == "192.168.1.20/16"
        ));
    }

    #[test]
    fn compile_config_with_an_unsupported_scheme_is_rejected() {
        let mut cfg = TargetPolicyConfig {
            schemes: vec!["ftp".into()],
            ..Default::default()
        };
        assert!(matches!(
            TargetPolicy::compile(&cfg).unwrap_err(),
            PolicyError::UnsupportedScheme(_)
        ));
        cfg.schemes.clear();
        assert_eq!(
            TargetPolicy::compile(&cfg).unwrap_err(),
            PolicyError::NoSchemes
        );
    }

    #[test]
    fn default_port_https_is_443_and_http_is_80() {
        assert_eq!(default_port(&Scheme::HTTPS), 443);
        assert_eq!(default_port(&Scheme::HTTP), 80);
    }
}
