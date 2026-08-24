//! 上游连接器：把 R03 的判定链接进 hyper。
//!
//! hyper 自带的 `HttpConnector` 会拿 hostname 自己再解析一次 —— 用上它，
//! 前面那套逐 IP 判定就完全是摆设：判定用的是第一次解析的结果，连接用的是
//! 第二次的，中间那个窗口正是 DNS rebinding（`docs/13` §6）。`clippy.toml`
//! 把它和 `GaiResolver` 一起列进了 `disallowed-types`，所以这里必须自己实现
//! 一个 `tower::Service<Uri>`：
//!
//! ```text
//! Uri → check_target()（scheme → host → DNS → 逐 IP → 端口）
//!     → PinnedConnector（只连判定过的 SocketAddr，连上后再确认 peer）
//!     → TLS（SNI 用原始 hostname，ALPN 优先 h2）
//! ```
//!
//! ALPN 里 `h2` 排在 `http/1.1` 前面是宪法 C23：10k 并发下 h2 的多路复用让
//! 连接数差两个数量级。

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use argus_core::ProxyError;
use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::{
    client::legacy::connect::{Connected, Connection},
    rt::TokioIo,
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{io::AsyncWrite, net::TcpStream};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tower::Service;

use crate::target::{CheckedResolver, TargetPolicy, check_target};

/// 明文或 TLS。两个变体都是 `Unpin`，所以下面的委托不需要 pin 投影。
pub enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl MaybeTls {
    fn negotiated_h2(&self) -> bool {
        match self {
            Self::Plain(_) => false,
            Self::Tls(s) => s.get_ref().1.alpn_protocol() == Some(b"h2"),
        }
    }
}

macro_rules! delegate {
    ($self:ident, $inner:ident => $body:expr) => {
        match &mut *$self {
            MaybeTls::Plain($inner) => $body,
            MaybeTls::Tls($inner) => $body,
        }
    };
}

impl tokio::io::AsyncRead for MaybeTls {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, s => Pin::new(s).poll_write(cx, buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_shutdown(cx))
    }
}

/// 交给 hyper 的连接。除了 IO，还带着一条信息：**这条连接协商到 h2 了吗**。
///
/// 少了它 hyper 会把每条 TLS 连接都当 HTTP/1.1 用，ALPN 就白协商了。
pub struct UpstreamIo {
    io: TokioIo<MaybeTls>,
    h2: bool,
}

impl UpstreamIo {
    fn new(inner: MaybeTls) -> Self {
        let h2 = inner.negotiated_h2();
        Self {
            io: TokioIo::new(inner),
            h2,
        }
    }
}

impl Connection for UpstreamIo {
    fn connected(&self) -> Connected {
        let c = Connected::new();
        if self.h2 { c.negotiated_h2() } else { c }
    }
}

impl Read for UpstreamIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl Write for UpstreamIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// 走 Target Policy 判定链的 connector。
#[derive(Clone)]
pub struct ArgusConnector {
    policy: Arc<TargetPolicy>,
    resolver: Arc<CheckedResolver>,
    tls: TlsConnector,
    connect_timeout: core::time::Duration,
}

impl ArgusConnector {
    /// # Errors
    ///
    /// 系统根证书装不进 rustls 时返回 [`ProxyError::Internal`]。
    pub fn new(
        policy: Arc<TargetPolicy>,
        resolver: Arc<CheckedResolver>,
        connect_timeout: core::time::Duration,
    ) -> Result<Self, ProxyError> {
        Self::build(policy, resolver, connect_timeout, &[])
    }

    /// 额外信任一组根证书。**只给 testkit 用**。
    ///
    /// 生产路径上没有这个口子是刻意的：一旦能从外面塞根证书进来，
    /// 「信任谁」就变成了运行期配置，而这类配置最后总会被人用来
    /// 关掉证书校验。要支持私有 CA 时应当先在 `docs/06` 里定清楚。
    ///
    /// # Errors
    ///
    /// 同 [`Self::new`]。
    #[cfg(feature = "testkit")]
    pub fn trusting(
        policy: Arc<TargetPolicy>,
        resolver: Arc<CheckedResolver>,
        connect_timeout: core::time::Duration,
        extra_roots: &[rustls::pki_types::CertificateDer<'static>],
    ) -> Result<Self, ProxyError> {
        Self::build(policy, resolver, connect_timeout, extra_roots)
    }

    fn build(
        policy: Arc<TargetPolicy>,
        resolver: Arc<CheckedResolver>,
        connect_timeout: core::time::Duration,
        extra_roots: &[rustls::pki_types::CertificateDer<'static>],
    ) -> Result<Self, ProxyError> {
        Ok(Self {
            policy,
            resolver,
            tls: TlsConnector::from(Arc::new(tls_config(extra_roots)?)),
            connect_timeout,
        })
    }
}

impl Service<Uri> for ArgusConnector {
    type Response = UpstreamIo;
    type Error = ProxyError;
    type Future = Pin<Box<dyn Future<Output = Result<UpstreamIo, ProxyError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), ProxyError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let policy = Arc::clone(&self.policy);
        let resolver = Arc::clone(&self.resolver);
        let tls = self.tls.clone();
        let connect_timeout = self.connect_timeout;

        Box::pin(async move {
            // 判定链：scheme → hostname → DNS → 逐 IP → 端口
            let pinned = check_target(&uri, &policy, &resolver)
                .await?
                .with_connect_timeout(connect_timeout);
            let stream = pinned.connect().await?;

            if uri.scheme_str() == Some("http") {
                return Ok(UpstreamIo::new(MaybeTls::Plain(stream)));
            }
            // SNI 与证书校验用**原始 hostname**，不是刚才连上的那个 IP。
            // 漏掉这条的人下一步通常会去关证书校验，那比 SSRF 本身更糟
            let name = ServerName::try_from(pinned.sni().to_owned())
                .map_err(|_| ProxyError::InvalidRawTarget)?;
            let tls_stream = tls
                .connect(name, stream)
                .await
                .map_err(|e| ProxyError::Connect(e.kind().to_string().into()))?;
            Ok(UpstreamIo::new(MaybeTls::Tls(Box::new(tls_stream))))
        })
    }
}

/// 只信 webpki 的根证书集合（外加 testkit 塞进来的那几张），
/// ALPN 优先 h2（宪法 C23）。
fn tls_config(
    extra_roots: &[rustls::pki_types::CertificateDer<'static>],
) -> Result<ClientConfig, ProxyError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in extra_roots {
        roots
            .add(cert.clone())
            .map_err(|e| ProxyError::Internal(e.to_string().into()))?;
    }

    // 显式给 provider 而不是依赖进程级默认值：库不该悄悄改全局状态，
    // 而且没装默认值时 `builder()` 会 panic，那是最难查的一类启动失败
    let mut cfg =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| ProxyError::Internal(e.to_string().into()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(cfg)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use super::*;

    /// 宪法 C23：ALPN 里 h2 必须排在 `http/1.1` **前面**。
    ///
    /// 顺序就是优先级。反过来写的话每条上游连接都会退化成 HTTP/1.1，
    /// 10k 并发下连接数差两个数量级 —— 而这个错误不会有任何报错，
    /// 只会表现为"连接数怎么这么多"。
    #[test]
    fn tls_config_offers_h2_ahead_of_http11() {
        let cfg = tls_config(&[]).expect("根证书可加载");
        assert_eq!(
            cfg.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    /// 不能只提 h2：只支持 1.1 的自建上游（vLLM、Ollama）会连不上。
    #[test]
    fn tls_config_still_offers_http11_as_a_fallback() {
        let cfg = tls_config(&[]).expect("根证书可加载");
        assert!(cfg.alpn_protocols.contains(&b"http/1.1".to_vec()));
    }

    /// 明文连接没有 ALPN，只能是 HTTP/1.1 —— 报 h2 会让 hyper 用错协议。
    #[tokio::test]
    async fn negotiated_h2_on_a_plain_connection_is_always_false() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("绑定回环端口");
        let addr = listener.local_addr().expect("拿到端口");
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let stream = TcpStream::connect(addr).await.expect("连上");
        let io = UpstreamIo::new(MaybeTls::Plain(stream));
        assert!(!io.h2);
        assert!(!io.connected().is_negotiated_h2());
    }
}
