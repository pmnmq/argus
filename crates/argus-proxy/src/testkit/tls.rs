//! `MockProvider` 的自签证书。
//!
//! 为什么非要上 TLS：**ALPN 只在 TLS 上存在**。宪法 C23 要的「h2 优先」
//! 是靠 ALPN 协商出来的，明文 h2c 走的是 `Upgrade` 或先验知识，完全是另
//! 一条路径。不上 TLS 就只能验到 `tls_config` 里的那个数组，验不到「真的
//! 协商成了 h2」—— F06 §7.2 欠的正是这一条。
//!
//! 证书每次进程启动现生成，不落盘：落盘的测试证书迟早会被人拷进生产。

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// 一张自签证书与配对的私钥。
pub struct TestCert {
    pub der: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl TestCert {
    /// 给 `localhost` / `127.0.0.1` / `::1` 现签一张。
    ///
    /// # Panics
    ///
    /// 生成失败即测试基础设施本身坏了，没有可继续的路径。
    #[must_use]
    pub fn localhost() -> Self {
        let names = vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
            "::1".to_owned(),
        ];
        let signed = rcgen::generate_simple_self_signed(names).expect("自签证书应当能生成");
        let key = PrivateKeyDer::try_from(signed.key_pair.serialize_der())
            .expect("rcgen 产出的私钥应当是合法 DER");
        Self {
            der: CertificateDer::from(signed.cert.der().to_vec()),
            key,
        }
    }

    /// 服务端 TLS 配置：ALPN 同时提供 h2 与 `http/1.1`，让客户端来选。
    ///
    /// 服务端**不**表达偏好 —— 表达了就测不出客户端的优先级，而 C23 约束
    /// 的正是客户端那一侧。
    ///
    /// # Panics
    ///
    /// 配置构建失败即证书与私钥不配对，属于上面那次生成出了问题。
    #[must_use]
    pub fn server_config(&self) -> Arc<rustls::ServerConfig> {
        let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("默认协议版本应当可用")
        .with_no_client_auth()
        .with_single_cert(vec![self.der.clone()], self.key.clone_key())
        .expect("现签的证书与私钥应当配对");
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(cfg)
    }
}
