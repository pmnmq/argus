//! 上游客户端：发请求、拿 header，**不碰 body 的一个字节**。
//!
//! 用 `hyper_util` 的 legacy `Client` 而不是 `reqwest`（`docs/06` §13-A）：
//! reqwest 会自动跟随重定向、自动解压、自动 retry —— 对透明代理这三条**全是
//! 负面的**。自动解压会改变字节（宪法 C1 直接破了），重定向会跳到未经判定的
//! host（R03 的判定链绕过去了），而它把 body 包成自己的类型之后取消语义
//! 也不透明了。
//!
//! 设计依据：`docs/06-data-plane.md` §6.3–6.4、§7、§10。

pub mod connect;
pub mod headers;

use std::sync::Arc;

use argus_core::{ProxyError, ResolvedTarget, Timeouts};
use bytes::Bytes;
use http::{HeaderMap, Method, Response};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::{client::legacy::Client, rt::TokioExecutor};
use tokio::time::{Instant, timeout};

pub use self::{
    connect::ArgusConnector,
    headers::{STRIP_FROM_CLIENT, build_upstream_headers},
};
use crate::target::{CheckedResolver, TargetPolicy};

/// 一次上游往返的产物。
pub struct UpstreamResponse {
    pub response: Response<Incoming>,
    /// 发出请求到收到 response header 的耗时
    pub header_micros: u32,
}

/// 上游客户端。内部是连接池，**clone 是共享而不是复制**。
#[derive(Clone)]
pub struct UpstreamClient {
    inner: Client<ArgusConnector, Full<Bytes>>,
}

impl UpstreamClient {
    /// # Errors
    ///
    /// TLS 配置构建失败时返回 [`ProxyError::Internal`]。
    pub fn new(
        policy: Arc<TargetPolicy>,
        resolver: Arc<CheckedResolver>,
        timeouts: Timeouts,
    ) -> Result<Self, ProxyError> {
        Ok(Self::from_connector(ArgusConnector::new(
            policy,
            resolver,
            timeouts.connect,
        )?))
    }

    /// 额外信任一组根证书。**只给 testkit 用**，理由见
    /// [`ArgusConnector::trusting`]。
    ///
    /// # Errors
    ///
    /// 同 [`Self::new`]。
    #[cfg(feature = "testkit")]
    pub fn trusting(
        policy: Arc<TargetPolicy>,
        resolver: Arc<CheckedResolver>,
        timeouts: Timeouts,
        extra_roots: &[rustls::pki_types::CertificateDer<'static>],
    ) -> Result<Self, ProxyError> {
        Ok(Self::from_connector(ArgusConnector::trusting(
            policy,
            resolver,
            timeouts.connect,
            extra_roots,
        )?))
    }

    fn from_connector(connector: ArgusConnector) -> Self {
        let inner = Client::builder(TokioExecutor::new())
            // h2 优先由 ALPN 决定（宪法 C23），这里不写死 http2_only ——
            // 写死了就连不上只支持 1.1 的自建上游
            .pool_idle_timeout(core::time::Duration::from_secs(90))
            .build(connector);
        Self { inner }
    }

    /// 发一次请求，拿到 response header 就返回。
    ///
    /// **body 不在这里读**：返回的是 `Incoming`，由主链路挂上 tee 之后直接
    /// 交给 hyper server。中间任何一次 `collect()` 都会把真流式变成假流式。
    ///
    /// # Errors
    ///
    /// | 情况 | 错误 |
    /// |---|---|
    /// | 目标被 Target Policy 拒 / DNS 失败 / 连不上 | 由判定链给出 |
    /// | `Timeouts.headers` 内没等到 response header | [`ProxyError::UpstreamTimeout`] |
    /// | 传输层出错 | [`ProxyError::Connect`] |
    pub async fn send(
        &self,
        target: &ResolvedTarget,
        method: &Method,
        client_headers: &HeaderMap,
        body: Bytes,
    ) -> Result<UpstreamResponse, ProxyError> {
        let headers = build_upstream_headers(client_headers, target)?;
        let mut req = http::Request::builder()
            .method(method)
            .uri(target.uri.clone())
            .body(Full::new(body))
            .map_err(|e| ProxyError::InvalidRequest(e.to_string().into()))?;
        *req.headers_mut() = headers;

        let started = Instant::now();
        let response = timeout(target.timeouts.headers, self.inner.request(req))
            .await
            .map_err(|_| ProxyError::UpstreamTimeout {
                stage: argus_core::UpstreamStage::S1Sent,
            })?
            .map_err(|e| ProxyError::Connect(e.to_string().into()))?;

        Ok(UpstreamResponse {
            response,
            header_micros: u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX),
        })
    }
}
