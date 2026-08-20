//! 只连**已判定通过**的地址的 connector。
//!
//! 这是 `docs/13-ssrf-protection.md` §6 里最容易写错的一环：hyper 默认的
//! `HttpConnector` 会自己拿 hostname 去解析，一旦用了它，前面那套逐 IP
//! 判定就完全是摆设 —— 判定用的是第一次解析的结果，连接用的是第二次的。
//!
//! 三条不变式：
//!
//! 1. **只连给定的 `SocketAddr`**，从不把 hostname 交给 `TcpStream::connect`。
//! 2. **TLS 的 SNI 与证书校验仍然用原始 hostname**。漏掉这条的人接下来通常
//!    会去开 `danger_accept_invalid_certs`，那比 SSRF 本身更糟。
//! 3. **连上之后再确认一次 peer 地址**。这一条在正常路径上永远成立，
//!    它存在的意义是：哪天有人把 connector 换回默认实现，测试会立刻红。

use core::{net::SocketAddr, time::Duration};

use argus_core::ProxyError;
use compact_str::CompactString;
use tokio::net::TcpStream;

use super::resolve::Addrs;

/// 绑定到一组已判定地址的 connector。
#[derive(Clone, Debug)]
pub struct PinnedConnector {
    addrs: Addrs,
    /// 原始 hostname。TLS 用它做 SNI 与证书校验，**不**用来解析
    sni: CompactString,
    connect_timeout: Duration,
}

impl PinnedConnector {
    /// `docs/06` §10 的四段超时里的第一段。
    pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

    /// # Errors
    ///
    /// 地址集合为空时返回 [`ProxyError::TargetDenied`] —— 那说明判定链
    /// 把所有候选都拒掉了，或者调用方跳过了判定链。
    pub fn new(addrs: Addrs, sni: &str) -> Result<Self, ProxyError> {
        if addrs.is_empty() {
            return Err(ProxyError::TargetDenied);
        }
        Ok(Self {
            addrs,
            sni: sni.into(),
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
        })
    }

    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// TLS 该用的名字。**不是**用来做 DNS 的。
    #[must_use]
    pub fn sni(&self) -> &str {
        &self.sni
    }

    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// 连上之后确认 peer 就是判定过的那个。
    ///
    /// # Errors
    ///
    /// 不在集合里时返回 [`ProxyError::TargetRebindDetected`] 并计数。
    pub fn verify_peer(&self, peer: SocketAddr) -> Result<(), ProxyError> {
        if self.addrs.contains(&peer) {
            return Ok(());
        }
        metrics::counter!("argus_target_rebind_detected_total").increment(1);
        Err(ProxyError::TargetRebindDetected)
    }

    /// 依次尝试每个已判定地址，第一个连上的即返回。
    ///
    /// # Errors
    ///
    /// | 情况 | 错误 |
    /// |---|---|
    /// | 全部地址都连不上或超时 | [`ProxyError::Connect`] |
    /// | 连上了但 peer 不在判定集合里 | [`ProxyError::TargetRebindDetected`] |
    pub async fn connect(&self) -> Result<TcpStream, ProxyError> {
        let mut last: Option<std::io::Error> = None;

        for &addr in &self.addrs {
            // 这里传的是 `SocketAddr` 而不是 `&str` —— 类型系统替我们保证了
            // 不会再发生一次 DNS 解析。别为了"方便"改成字符串
            match tokio::time::timeout(self.connect_timeout, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    let peer = stream
                        .peer_addr()
                        .map_err(|e| ProxyError::Connect(e.to_string().into()))?;
                    self.verify_peer(peer)?;
                    return Ok(stream);
                }
                Ok(Err(e)) => last = Some(e),
                Err(_elapsed) => {
                    last = Some(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "connect timeout",
                    ));
                }
            }
        }

        // 错误信息里不带地址：拒绝路径与失败路径都不该回显 IP（`docs/13` §8）
        Err(ProxyError::Connect(last.map_or_else(
            || CompactString::const_new("no address"),
            |e| e.kind().to_string().into(),
        )))
    }
}
