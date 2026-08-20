//! `docs/20-testing-strategy.md` §5.4 的 SSRF 测试矩阵，逐格实现。
//!
//! SSRF 的失败方式和 URL 解析一样是静默的：请求成功了，只是打到了不该打的
//! 地方。所以这里断言的不只是"被拒"，还有**拒得对不对** —— 同一个 403、
//! 同一段 body、不含任何 IP。

mod common;

use core::time::Duration;
use std::sync::Arc;

use argus_core::ProxyError;
use argus_proxy::target::{
    CheckedResolver, PinnedConnector, TargetPolicy, TargetPolicyConfig, check_target,
};
use http::Uri;

use crate::common::{MockResolver, addrs};

/// 用一个可编排的解析器跑完整条判定链。
async fn check(
    uri: &str,
    dns: MockResolver,
    policy: &TargetPolicy,
) -> Result<PinnedConnector, ProxyError> {
    let resolver = CheckedResolver::new(Arc::new(dns));
    check_target(
        &uri.parse::<Uri>().expect("固定的合法 URI"),
        policy,
        &resolver,
    )
    .await
}

fn policy_with(mutate: impl FnOnce(&mut TargetPolicyConfig)) -> TargetPolicy {
    let mut cfg = TargetPolicyConfig::default();
    mutate(&mut cfg);
    TargetPolicy::compile(&cfg).expect("测试策略必须合法")
}

// ─────────────────────────────────────────────────────────────────────────
mod target_policy_matrix {
    use std::io;

    use super::{Arc, CheckedResolver, MockResolver, ProxyError, TargetPolicy, check, policy_with};

    /// §5.4 第一行：解析到内网、回环或云元数据 → 403 `target_denied`。
    #[tokio::test]
    async fn check_target_resolving_to_a_private_address_returns_target_denied() {
        let p = TargetPolicy::default();
        for addr in [
            "127.0.0.1:443",
            "10.1.2.3:443",
            "192.168.1.20:443",
            "169.254.169.254:443", // 云元数据
            "[::1]:443",
            "[fd00:ec2::254]:443",
            // v4-mapped 的写法必须一样拦得住，否则一条 AAAA 记录就绕过了
            "[::ffff:127.0.0.1]:443",
        ] {
            let err =
                check("https://evil.example/v1", MockResolver::always(&[addr]), &p).await.err();
            assert_eq!(
                err,
                Some(ProxyError::TargetDenied),
                "解析到 {addr} 应该被拒"
            );
        }
    }

    #[tokio::test]
    async fn check_target_resolving_to_a_public_address_is_allowed() {
        let p = TargetPolicy::default();
        let conn = check(
            "https://api.openai.com/v1/responses",
            MockResolver::always(&["104.18.0.1:443"]),
            &p,
        )
        .await
        .expect("公网地址应该放行");
        assert_eq!(conn.addrs().len(), 1);
        // SNI 用的是原始 hostname，不是 IP（`docs/13` §6 第 3 点）
        assert_eq!(conn.sni(), "api.openai.com");
    }

    /// §5.4 第三行：多值 A 记录里混进一个内网 IP → **整体**拒绝。
    ///
    /// 「挑一个合规的连」会让攻击者用 `[1.2.3.4, 127.0.0.1]` 碰运气。
    #[tokio::test]
    async fn check_target_multi_valued_answer_with_one_private_ip_denies_all() {
        let p = TargetPolicy::default();
        let err = check(
            "https://evil.example/v1",
            MockResolver::always(&["1.2.3.4:443", "127.0.0.1:443"]),
            &p,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProxyError::TargetDenied);
    }

    /// §5.4 第二行 / §7 最后一条：精确放行那一台 NAS 之后就该能连上。
    #[tokio::test]
    async fn check_target_explicitly_allowed_cidr_makes_the_local_model_reachable() {
        let p = policy_with(|c| c.allow_cidrs = vec!["192.168.1.20/32".into()]);
        let conn = check(
            "http://192.168.1.20:11434/api/chat",
            MockResolver::always(&["192.168.1.20:11434"]),
            &p,
        )
        .await
        .expect("显式放行的地址应该可达");
        assert_eq!(conn.sni(), "192.168.1.20");

        // 同网段的邻居仍然拒 —— 放行范围要精确到能用为止（`docs/13` §5.1）
        let err = check(
            "http://192.168.1.21:11434/api/chat",
            MockResolver::always(&["192.168.1.21:11434"]),
            &p,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProxyError::TargetDenied);
    }

    /// 按名字放行：`allow_hosts` 命中就跳过逐 IP 判定（§5.2 的优先级）。
    #[tokio::test]
    async fn check_target_host_on_the_allowlist_skips_the_ip_judgement() {
        let p = policy_with(|c| c.allow_hosts = vec!["nas.local".into()]);
        let conn = check(
            "http://nas.local:11434/api/chat",
            MockResolver::always(&["192.168.1.20:11434"]),
            &p,
        )
        .await
        .expect("按名字放行");
        assert_eq!(conn.addrs()[0].ip().to_string(), "192.168.1.20");
    }

    /// 但按名字放行**不能**把端口 denylist 一起绕过去。
    #[tokio::test]
    async fn check_target_host_on_the_allowlist_still_honours_the_port_denylist() {
        let p = policy_with(|c| c.allow_hosts = vec!["nas.local".into()]);
        let err = check(
            "http://nas.local:6379/",
            MockResolver::always(&["192.168.1.20:6379"]),
            &p,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProxyError::TargetDenied);
    }

    #[tokio::test]
    async fn check_target_scheme_outside_the_allowlist_returns_scheme_not_allowed() {
        let p = policy_with(|c| c.schemes = vec!["https".into()]);
        let err = check(
            "http://api.openai.com/v1",
            MockResolver::always(&["104.18.0.1:80"]),
            &p,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProxyError::SchemeNotAllowed);
    }

    /// §7：DNS 失败必须与「被策略拒」用**不同**的码 —— 它不泄漏策略信息，
    /// 而且 502 能让调用方知道该重试。
    #[tokio::test]
    async fn check_target_dns_failure_returns_target_dns_failure_not_denied() {
        let p = TargetPolicy::default();
        for dns in [
            MockResolver::failing(io::ErrorKind::NotFound),
            MockResolver::failing(io::ErrorKind::TimedOut),
            MockResolver::empty(),
        ] {
            let err = check("https://nope.example/v1", dns, &p).await.unwrap_err();
            assert_eq!(err, ProxyError::TargetDnsFailure);
            assert_eq!(err.http_status(), 502);
        }
    }

    /// §5.4 最后一行 / §7：**不同的拒绝原因**必须返回完全相同的 403 与 body。
    /// 区分开就等于把 Argus 变成一台内网存活探测器。
    #[tokio::test]
    async fn check_target_every_denial_reason_renders_the_same_403() {
        let p = policy_with(|c| c.deny_hosts = vec!["blocked.example".into()]);

        let private = check(
            "https://a.example/v1",
            MockResolver::always(&["10.0.0.5:443"]),
            &p,
        )
        .await
        .unwrap_err();
        let bad_port = check(
            "https://b.example:6379/v1",
            MockResolver::always(&["1.2.3.4:6379"]),
            &p,
        )
        .await
        .unwrap_err();
        let denied_host = check(
            "https://blocked.example/v1",
            MockResolver::always(&["1.2.3.4:443"]),
            &p,
        )
        .await
        .unwrap_err();

        for err in [&private, &bad_port, &denied_host] {
            assert_eq!(err.http_status(), 403);
            assert_eq!(err.error_code(), "target_denied");
            assert_eq!(err.to_string(), private.to_string());
        }
    }

    /// §7：拒绝信息里不能出现任何 IP 字符串。
    #[tokio::test]
    async fn check_target_denial_message_contains_no_address_or_reason() {
        let p = TargetPolicy::default();
        let err = check(
            "https://a.example/v1",
            MockResolver::always(&["169.254.169.254:443"]),
            &p,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        for leak in [
            "169.254", "127.0", "10.", "192.168", "cidr", "private", "port",
        ] {
            assert!(!msg.contains(leak), "拒绝信息泄漏了 `{leak}`：{msg}");
        }
    }

    /// `docs/13` §10：Managed 模式也走同一条判定链。配置可能把 `base_url`
    /// 写成 `http://169.254.169.254`，Control Plane 也可能被攻破。
    #[tokio::test]
    async fn check_target_managed_base_url_pointing_at_metadata_is_denied() {
        let p = TargetPolicy::default();
        let err = check(
            "http://169.254.169.254/latest/meta-data/",
            MockResolver::always(&["169.254.169.254:80"]),
            &p,
        )
        .await
        .unwrap_err();
        assert_eq!(err, ProxyError::TargetDenied);
    }

    /// `docs/13` §6.1：缓存的是「已判定通过的地址集合」。命中之后不再解析。
    #[tokio::test]
    async fn resolve_and_check_second_call_within_the_ttl_hits_the_cache() {
        let dns = Arc::new(MockResolver::always(&["1.2.3.4:443"]));
        let resolver = CheckedResolver::new(Arc::clone(&dns) as Arc<_>);
        let p = TargetPolicy::default();

        for _ in 0..5 {
            resolver.resolve_and_check("api.openai.com", 443, &p).await.expect("公网地址");
        }
        assert_eq!(dns.calls(), 1, "五次判定只该有一次 DNS 往返");

        resolver.invalidate("api.openai.com", 443);
        resolver.resolve_and_check("api.openai.com", 443, &p).await.expect("公网地址");
        assert_eq!(dns.calls(), 2, "失效之后要重新解析");
    }

    /// 字面 IP 根本不经过 DNS —— 也就没有 rebinding 可言。
    #[tokio::test]
    async fn resolve_and_check_literal_ip_never_touches_dns() {
        let dns = Arc::new(MockResolver::failing(io::ErrorKind::NotFound));
        let resolver = CheckedResolver::new(Arc::clone(&dns) as Arc<_>);
        let p = TargetPolicy::default();

        resolver.resolve_and_check("1.2.3.4", 443, &p).await.expect("字面 IP 不查 DNS");
        assert_eq!(dns.calls(), 0);
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod target_rebinding {
    use super::{Arc, CheckedResolver, Duration, MockResolver, ProxyError, TargetPolicy, addrs};
    use argus_proxy::target::PinnedConnector;

    /// §5.4 第四行，也是整个 spec 最要紧的一格。
    ///
    /// 攻击形状：t=0 解析出公网地址、通过判定；t=1 Argus 发起连接时该名字
    /// 已经指向 `127.0.0.1`。只要「校验」和「连接」各解析一次，中间那个窗口
    /// 就是漏洞。
    #[tokio::test]
    async fn target_pinned_connector_never_reresolves_after_the_verdict() {
        // 第一次答公网，第二次答回环 —— 典型的 rebinding
        let dns = Arc::new(MockResolver::scripted(&[
            &["1.2.3.4:443"],
            &["127.0.0.1:443"],
        ]));
        let resolver = CheckedResolver::with_ttl(Arc::clone(&dns) as Arc<_>, Duration::ZERO);
        let p = TargetPolicy::default();

        let vetted = resolver
            .resolve_and_check("attacker.example", 443, &p)
            .await
            .expect("第一次解析是公网地址");
        let conn = PinnedConnector::new(vetted, "attacker.example").expect("非空");

        // 关键：connector 手里只有判定过的那个地址，不含攻击者的第二个答案
        assert_eq!(conn.addrs(), addrs(&["1.2.3.4:443"]).as_slice());

        // 第二次判定确实拿到了回环地址 —— 也就是说攻击真的发生了，
        // 只是 connector 不受它影响
        let second = resolver.resolve_and_check("attacker.example", 443, &p).await;
        assert_eq!(second.unwrap_err(), ProxyError::TargetDenied);
        assert_eq!(dns.calls(), 2);
    }

    /// R5：万一 connector 被换回会自己解析的实现，连上之后这一步要拦住。
    #[tokio::test]
    async fn target_verify_peer_outside_the_vetted_set_returns_rebind_detected() {
        let conn = PinnedConnector::new(addrs(&["1.2.3.4:443"]), "attacker.example").expect("非空");

        assert_eq!(
            conn.verify_peer("1.2.3.4:443".parse().expect("合法")),
            Ok(())
        );
        assert_eq!(
            conn.verify_peer("127.0.0.1:443".parse().expect("合法")).unwrap_err(),
            ProxyError::TargetRebindDetected
        );
    }

    #[tokio::test]
    async fn target_pinned_connector_with_no_vetted_address_is_rejected() {
        assert_eq!(
            PinnedConnector::new(addrs(&[]), "x.example").unwrap_err(),
            ProxyError::TargetDenied
        );
    }

    /// 真的连一次：connector 拿到的是 `SocketAddr`，类型上就没有再解析的可能。
    #[tokio::test]
    async fn target_pinned_connector_connects_to_the_vetted_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("能起监听");
        let bound = listener.local_addr().expect("有地址");
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let conn = PinnedConnector::new(addrs(&[&bound.to_string()]), "localhost").expect("非空");
        let stream = conn.connect().await.expect("应该连得上");
        assert_eq!(stream.peer_addr().expect("有 peer"), bound);
    }

    #[tokio::test]
    async fn target_pinned_connector_when_every_address_refuses_returns_connect_error() {
        // 端口 1 上几乎不可能有人监听；连不上时不能泄漏地址
        let conn = PinnedConnector::new(addrs(&["127.0.0.1:1"]), "localhost")
            .expect("非空")
            .with_connect_timeout(Duration::from_millis(200));
        let err = conn.connect().await.unwrap_err();
        assert_eq!(err.error_code(), "upstream_connect_error");
        assert!(!err.to_string().contains("127.0.0.1"), "泄漏了地址：{err}");
    }
}
