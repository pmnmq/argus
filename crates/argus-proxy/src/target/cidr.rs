//! CIDR 解析与最长前缀匹配。
//!
//! 没有用 trie。默认策略只有十来条规则，而最长前缀匹配可以化简成「按前缀
//! 长度倒序排好，第一个命中的就是最长的」—— 一次连续数组扫描，在这个规模上
//! 稳稳快过指针跳转的 trie，代码量还少一个数量级（`docs/21` §8 的口径：
//! CIDR 判定 µs 级）。规则涨到几百条再说。
//!
//! 设计依据：`docs/13-ssrf-protection.md` §5、§5.2。

use core::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

/// 把 IPv4-mapped 的 IPv6 地址（`::ffff:127.0.0.1`）折回 IPv4。
///
/// **这一步是安全必需的，不是整洁**：不折的话，一条 AAAA 记录返回
/// `::ffff:127.0.0.1` 就能整张 v4 deny 表擦肩而过 —— `127.0.0.0/8` 是 v4
/// 规则，跟一个 v6 地址永远不匹配。
#[must_use]
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

/// 一条 CIDR，网络位已规整。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

/// [`Cidr::parse`] 的失败原因。只在配置加载期出现。
#[derive(Copy, Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum CidrParseError {
    #[error("missing '/' prefix length")]
    MissingPrefix,
    #[error("not an ip address")]
    BadAddress,
    #[error("prefix length is not a number")]
    BadPrefix,
    #[error("prefix length exceeds address size")]
    PrefixTooLong,
    #[error("host bits are set outside the prefix")]
    HostBitsSet,
}

impl Cidr {
    /// 解析 `192.168.0.0/16` 这样的写法。
    ///
    /// # Errors
    ///
    /// 见 [`CidrParseError`]。其中 [`CidrParseError::HostBitsSet`] 值得单说：
    /// `192.168.1.20/16` 会被**拒绝**而不是悄悄规整成 `192.168.0.0/16`。
    /// 想写 `/32` 却手滑写成 `/16` 的 allow 规则会因此放行整个 `/16` ——
    /// 那正是 `docs/13` §5.1 反复强调不要干的事，静默纠正等于帮凶。
    pub fn parse(s: &str) -> Result<Self, CidrParseError> {
        let (addr, len) = s.split_once('/').ok_or(CidrParseError::MissingPrefix)?;
        let ip: IpAddr = addr.trim().parse().map_err(|_| CidrParseError::BadAddress)?;
        let prefix: u8 = len.trim().parse().map_err(|_| CidrParseError::BadPrefix)?;

        let ip = canonical(ip);
        if prefix > Self::max_prefix(ip) {
            return Err(CidrParseError::PrefixTooLong);
        }
        let net = mask(ip, prefix);
        if net != ip {
            return Err(CidrParseError::HostBitsSet);
        }
        Ok(Self { net, prefix })
    }

    const fn max_prefix(ip: IpAddr) -> u8 {
        match ip {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        }
    }

    #[must_use]
    pub const fn prefix(&self) -> u8 {
        self.prefix
    }

    /// 地址在不在这个网段里。跨协议族一律 `false`。
    ///
    /// 调用方传进来的 `ip` 必须已经过 [`canonical`]。
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                masked_v4(ip, self.prefix) == u32::from_be_bytes(net.octets())
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                masked_v6(ip, self.prefix) == u128::from_be_bytes(net.octets())
            }
            _ => false,
        }
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.net, self.prefix)
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(masked_v4(v4, prefix).to_be_bytes())),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(masked_v6(v6, prefix).to_be_bytes())),
    }
}

// `checked_shr` 而不是 `>>`：两个边界都会踩坑。`u32::MAX >> 32` 在 debug 下
// panic、release 下悄悄变成 `>> 0`；而 prefix == 0 时又不能让掩码变成全 1。
// `checked_shr` 返回 `None` 表示「移光了」，也就是低位掩码为 0，取反即全 1。
fn masked_v4(ip: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from_be_bytes(ip.octets());
    let keep = u32::MAX.checked_shr(u32::from(prefix)).map_or(u32::MAX, |low| !low);
    bits & keep
}

fn masked_v6(ip: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from_be_bytes(ip.octets());
    let keep = u128::MAX.checked_shr(u32::from(prefix)).map_or(u128::MAX, |low| !low);
    bits & keep
}

/// 一组 CIDR，按前缀长度倒序存放。
#[derive(Clone, Debug, Default)]
pub struct CidrSet {
    /// 已按 `prefix` 从大到小排序 —— 第一个命中的即最长前缀
    entries: Vec<Cidr>,
}

impl CidrSet {
    /// 从字符串列表构建。
    ///
    /// # Errors
    ///
    /// 任一条解析失败即整体失败，并带上出错的那一条（配置期要能定位）。
    pub fn parse_all<'a>(
        items: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, (String, CidrParseError)> {
        let mut entries = items
            .into_iter()
            .map(|s| Cidr::parse(s).map_err(|e| (s.to_owned(), e)))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort_unstable_by_key(|c| core::cmp::Reverse(c.prefix));
        Ok(Self { entries })
    }

    /// 命中的最长前缀长度，没命中返回 `None`。
    #[must_use]
    pub fn longest_match(&self, ip: IpAddr) -> Option<u8> {
        self.entries.iter().find(|c| c.contains(ip)).map(Cidr::prefix)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
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

    #[test]
    fn parse_well_formed_cidrs_returns_the_masked_network() {
        for s in [
            "0.0.0.0/0",
            "10.0.0.0/8",
            "192.168.1.20/32",
            "::/0",
            "fc00::/7",
        ] {
            assert!(Cidr::parse(s).is_ok(), "{s} 应该合法");
        }
    }

    /// 想写 `/32` 手滑写成 `/16` 的 allow 规则会放行整个 `/16`。
    /// 静默规整等于帮凶，所以直接拒绝。
    #[test]
    fn parse_cidr_with_host_bits_set_returns_host_bits_set() {
        assert_eq!(
            Cidr::parse("192.168.1.20/16").unwrap_err(),
            CidrParseError::HostBitsSet
        );
        assert_eq!(
            Cidr::parse("fc00::1/7").unwrap_err(),
            CidrParseError::HostBitsSet
        );
    }

    #[test]
    fn parse_malformed_input_returns_the_matching_error() {
        assert_eq!(
            Cidr::parse("10.0.0.0").unwrap_err(),
            CidrParseError::MissingPrefix
        );
        assert_eq!(
            Cidr::parse("not-an-ip/8").unwrap_err(),
            CidrParseError::BadAddress
        );
        assert_eq!(
            Cidr::parse("10.0.0.0/x").unwrap_err(),
            CidrParseError::BadPrefix
        );
        assert_eq!(
            Cidr::parse("10.0.0.0/33").unwrap_err(),
            CidrParseError::PrefixTooLong
        );
        assert_eq!(
            Cidr::parse("::/129").unwrap_err(),
            CidrParseError::PrefixTooLong
        );
    }

    /// `prefix == 0` 会让朴素的 `!(u32::MAX >> 32)` 在 debug 下 panic。
    #[test]
    fn contains_zero_length_prefix_matches_every_address_of_that_family() {
        let all_v4 = Cidr::parse("0.0.0.0/0").expect("合法");
        assert!(all_v4.contains(ip("1.2.3.4")));
        assert!(all_v4.contains(ip("127.0.0.1")));
        assert!(!all_v4.contains(ip("2001:db8::1")));

        let all_v6 = Cidr::parse("::/0").expect("合法");
        assert!(all_v6.contains(ip("2001:db8::1")));
        assert!(!all_v6.contains(ip("1.2.3.4")));
    }

    #[test]
    fn contains_addresses_inside_and_outside_the_network_discriminates() {
        let rfc1918 = Cidr::parse("192.168.0.0/16").expect("合法");
        assert!(rfc1918.contains(ip("192.168.1.20")));
        assert!(rfc1918.contains(ip("192.168.255.255")));
        assert!(!rfc1918.contains(ip("192.169.0.1")));
        assert!(!rfc1918.contains(ip("10.0.0.1")));
    }

    /// 一条 AAAA 记录返回 `::ffff:127.0.0.1` 就能绕过整张 v4 deny 表。
    #[test]
    fn canonical_ipv4_mapped_v6_address_folds_back_to_ipv4() {
        assert_eq!(ip("::ffff:127.0.0.1"), ip("127.0.0.1"));
        assert_eq!(ip("::ffff:169.254.169.254"), ip("169.254.169.254"));
        // 折回之后就能被 v4 规则拦住
        let loopback = Cidr::parse("127.0.0.0/8").expect("合法");
        assert!(loopback.contains(ip("::ffff:127.0.0.1")));
    }

    /// 最长前缀匹配：`/32` 必须赢过 `/16`，与写在列表里的顺序无关。
    #[test]
    fn longest_match_overlapping_networks_returns_the_longest_prefix() {
        let set =
            CidrSet::parse_all(["192.168.0.0/16", "192.168.1.20/32", "10.0.0.0/8"]).expect("合法");
        assert_eq!(set.longest_match(ip("192.168.1.20")), Some(32));
        assert_eq!(set.longest_match(ip("192.168.1.21")), Some(16));
        assert_eq!(set.longest_match(ip("10.1.2.3")), Some(8));
        assert_eq!(set.longest_match(ip("1.2.3.4")), None);
    }

    #[test]
    fn parse_all_one_bad_entry_reports_which_one() {
        let (bad, err) = CidrSet::parse_all(["10.0.0.0/8", "192.168.1.20/16"]).unwrap_err();
        assert_eq!(bad, "192.168.1.20/16");
        assert_eq!(err, CidrParseError::HostBitsSet);
    }
}
