//! `docs/20-testing-strategy.md` §5.1 的 URL 解析矩阵，逐格实现。
//!
//! 这些用例的失败方式是**静默的**：上游收到一个略有不同的 path，返回 404，
//! 而日志里看起来一切正常。所以这里断言的一律是**完整 target 串**，不是
//! 「解析成功」。

mod common;

use argus_core::{CredentialMode, ObserverKind, ProxyError, TargetOrigin};
use argus_proxy::ingress::resolve_target;

use crate::common::{TestProfile, TestProfiles, parts};

/// 跑一次 Managed 解析，返回完整 target 串。
fn managed(base_url: &str, request_target: &str) -> Result<String, ProxyError> {
    let profiles = TestProfiles::one("openai", base_url);
    resolve_target(&parts(request_target), &profiles).map(|t| t.uri.to_string())
}

/// 跑一次 Raw 解析，返回完整 target 串。Raw 不查 Profile。
fn raw(request_target: &str) -> Result<String, ProxyError> {
    resolve_target(&parts(request_target), &TestProfiles::default()).map(|t| t.uri.to_string())
}

// ─────────────────────────────────────────────────────────────────────────
mod managed_resolver {
    use super::{ProxyError, TestProfile, TestProfiles, managed, parts, resolve_target};

    /// §5.1「Managed 拼接」：`base_url` 带/不带尾斜杠、`base_url` 自带前缀。
    #[test]
    fn resolve_managed_url_matrix_produces_the_documented_targets() {
        // (base_url, 请求 target, 期望上游 URL)
        let matrix = [
            // `docs/03` §4.1 的实例表
            (
                "https://api.openai.com",
                "/provider/openai/v1/responses",
                "https://api.openai.com/v1/responses",
            ),
            (
                "https://api.deepseek.com",
                "/provider/openai/v1/chat/completions",
                "https://api.deepseek.com/v1/chat/completions",
            ),
            (
                "https://dashscope.aliyuncs.com",
                "/provider/openai/compatible-mode/v1/chat/completions",
                "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
            ),
            (
                "https://openrouter.ai",
                "/provider/openai/api/v1/chat/completions",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
            // 尾斜杠：三种写法等价（§5）
            (
                "https://api.openai.com/",
                "/provider/openai/v1/responses",
                "https://api.openai.com/v1/responses",
            ),
            (
                "https://api.openai.com//",
                "/provider/openai/v1/responses",
                "https://api.openai.com/v1/responses",
            ),
            // base_url 自带路径前缀（§5.1）：挂在企业内已有网关后面
            (
                "https://gw.corp.com/ai/v1",
                "/provider/openai/chat/completions",
                "https://gw.corp.com/ai/v1/chat/completions",
            ),
            (
                "https://gw.corp.com/ai/v1/",
                "/provider/openai/chat/completions",
                "https://gw.corp.com/ai/v1/chat/completions",
            ),
            // 端口与 http 也要能拼
            (
                "http://127.0.0.1:11434",
                "/provider/openai/api/chat",
                "http://127.0.0.1:11434/api/chat",
            ),
        ];

        for (base_url, target, expected) in matrix {
            assert_eq!(
                managed(base_url, target).as_deref(),
                Ok(expected),
                "base_url={base_url} target={target}"
            );
        }
    }

    /// §5.1「空 path」：`/provider/x` 与 `/provider/x/` **刻意**不同（`docs/03` §5.2）。
    #[test]
    fn resolve_managed_url_without_upstream_path_returns_empty_upstream_path() {
        assert_eq!(
            managed("https://api.openai.com", "/provider/openai"),
            Err(ProxyError::EmptyUpstreamPath)
        );
        assert_eq!(
            managed("https://api.openai.com", "/provider/openai/").as_deref(),
            Ok("https://api.openai.com/"),
            "尾斜杠是「转发到根」，不是错误"
        );
    }

    /// §5.1「Query 保真」：一个字节都不动，含空值与无值参数（`docs/03` §6）。
    #[test]
    fn resolve_managed_url_with_query_forwards_it_byte_for_byte() {
        assert_eq!(
            managed(
                "https://x.com",
                "/provider/openai/v1/responses?a=1&b=a%2Bb&empty=&flag"
            )
            .as_deref(),
            Ok("https://x.com/v1/responses?a=1&b=a%2Bb&empty=&flag")
        );
        // Azure 的 api-version：不需要任何特殊处理，客户端怎么写就怎么转
        assert_eq!(
            managed(
                "https://xx.openai.azure.com",
                "/provider/openai/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
            )
            .as_deref(),
            Ok(
                "https://xx.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21"
            )
        );
        // 重复 key 与顺序都不能被「解析再序列化」改掉
        assert_eq!(
            managed("https://x.com", "/provider/openai/v1?z=2&a=1&z=1").as_deref(),
            Ok("https://x.com/v1?z=2&a=1&z=1")
        );
    }

    /// §5.1「百分号编码」：`%2F` / `%20` / 中文 / 已编码的 `%25`，一律原样。
    ///
    /// `%2F` 这条是全 spec 最关键的一格：解码它会**改变路径段数**，
    /// 上游收到的就是另一个资源（`docs/04` §5.2）。
    #[test]
    fn resolve_managed_url_with_percent_encoding_never_decodes_or_reencodes() {
        let cases = [
            ("/provider/openai/v1/models/gpt%2F4", "/v1/models/gpt%2F4"),
            ("/provider/openai/v1/a%20b", "/v1/a%20b"),
            (
                "/provider/openai/v1/%E4%B8%AD%E6%96%87",
                "/v1/%E4%B8%AD%E6%96%87",
            ),
            // 已经编码过的 % 再编一次就成了 %2525
            ("/provider/openai/v1/100%25", "/v1/100%25"),
        ];
        for (target, expected_path) in cases {
            assert_eq!(
                managed("https://x.com", target).as_deref(),
                Ok(format!("https://x.com{expected_path}").as_str()),
                "target={target}"
            );
        }
    }

    /// §5.1「双斜杠」：不折叠。上游想怎么理解 `//` 是上游的事（`docs/03` §5.3）。
    #[test]
    fn resolve_managed_url_with_double_slashes_does_not_collapse_them() {
        assert_eq!(
            managed("https://x.com", "/provider/openai/v1//foo").as_deref(),
            Ok("https://x.com/v1//foo")
        );
        assert_eq!(
            managed("https://x.com", "/provider/openai//foo").as_deref(),
            Ok("https://x.com//foo")
        );
        assert_eq!(
            managed("https://x.com", "/provider/openai/v1/foo//").as_deref(),
            Ok("https://x.com/v1/foo//")
        );
    }

    /// §5.1「Profile 名」：字符集边界，以及 `%6Fpenai` 这类绕过写法。
    #[test]
    fn resolve_managed_url_with_an_illegal_profile_name_returns_invalid_request() {
        for target in [
            "/provider/OpenAI/v1",     // 大写不在字符集里
            "/provider/open_ai/v1",    // 下划线不在字符集里
            "/provider/-openai/v1",    // 首字符必须是 [a-z0-9]
            "/provider//v1",           // 空名字
            "/provider/",              // 空名字，且没有 path
            "/provider/%6Fpenai/v1",   // decode 之后是 openai —— 必须在这之前就拦下
            "/provider/openai%2Fx/v1", // 编码斜杠不能用来伪造段
        ] {
            let err = managed("https://x.com", target).unwrap_err();
            assert_eq!(err.error_code(), "invalid_request", "target={target}");
            assert_eq!(err.http_status(), 400, "target={target}");
        }
    }

    /// 合法字符集的边界值。
    #[test]
    fn resolve_managed_url_with_boundary_profile_names_is_accepted() {
        for name in ["a", "9", "x9", "openai-prod", "a-b-c-9", &"a".repeat(63)] {
            let profiles = TestProfiles::one(name, "https://x.com");
            let target = format!("/provider/{name}/v1");
            let uri = resolve_target(&parts(&target), &profiles)
                .unwrap_or_else(|e| panic!("{name} 应该合法：{e}"))
                .uri;
            assert_eq!(uri.to_string(), "https://x.com/v1");
        }
    }

    /// 不存在与无权访问必须不可区分，所以只有 404 一种表现
    /// （`docs/14-auth-and-credentials.md` §7.2）。
    #[test]
    fn resolve_managed_unknown_profile_returns_provider_not_found() {
        let err = managed("https://x.com", "/provider/nope/v1").unwrap_err();
        assert_eq!(err, ProxyError::ProviderNotFound);
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn resolve_managed_disabled_profile_returns_provider_disabled() {
        let profiles =
            TestProfiles::of(vec![TestProfile::new("openai", "https://x.com").disabled()]);
        assert_eq!(
            resolve_target(&parts("/provider/openai/v1"), &profiles).map(|_| ()),
            Err(ProxyError::ProviderDisabled)
        );
    }

    /// 一个写错的 `base_url` 会让 `Uri` 解析出合法的 origin-form，那就变成
    /// 「Argus 往自己身上发请求」。必须在这里就拦下。
    #[test]
    fn resolve_managed_base_url_without_scheme_returns_provider_misconfigured() {
        assert_eq!(
            managed("api.openai.com", "/provider/openai/v1"),
            Err(ProxyError::ProviderMisconfigured)
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod raw_resolver {
    use super::{ProxyError, raw};

    /// §5.1「Raw 直接形式」与「Raw 分段形式」：两种写法必须产出
    /// **byte-identical** 的 target。
    #[test]
    fn resolve_raw_both_url_forms_produce_byte_identical_targets() {
        // (直接形式, 分段形式, 期望上游 URL)
        let matrix = [
            (
                "/raw/https://api.openai.com/v1/responses",
                "/raw/https/api.openai.com/v1/responses",
                "https://api.openai.com/v1/responses",
            ),
            (
                "/raw/http://192.168.1.20:11434/api/chat",
                "/raw/http/192.168.1.20:11434/api/chat",
                "http://192.168.1.20:11434/api/chat",
            ),
            // IPv6 字面量：方括号里的 `:` 不能被当成端口分隔符
            (
                "/raw/https://[2001:db8::1]:8000/v1/chat",
                "/raw/https/[2001:db8::1]:8000/v1/chat",
                "https://[2001:db8::1]:8000/v1/chat",
            ),
            (
                "/raw/https://[::1]/v1/chat",
                "/raw/https/[::1]/v1/chat",
                "https://[::1]/v1/chat",
            ),
            // 没有 path：转发到根（`docs/04` §4.4）
            (
                "/raw/https://api.openai.com",
                "/raw/https/api.openai.com",
                "https://api.openai.com/",
            ),
            // query、百分号编码、双斜杠在 Raw 下同样原样保留
            (
                "/raw/https://x.com/v1//a%2Fb?z=2&a=1&empty=&flag",
                "/raw/https/x.com/v1//a%2Fb?z=2&a=1&empty=&flag",
                "https://x.com/v1//a%2Fb?z=2&a=1&empty=&flag",
            ),
        ];

        for (direct, segmented, expected) in matrix {
            assert_eq!(raw(direct).as_deref(), Ok(expected), "直接形式 {direct}");
            assert_eq!(
                raw(segmented).as_deref(),
                Ok(expected),
                "分段形式 {segmented}"
            );
        }
    }

    /// scheme 大小写不敏感（RFC 3986 §3.1），产出一律小写。
    #[test]
    fn resolve_raw_scheme_in_upper_case_is_normalised_to_lower_case() {
        assert_eq!(
            raw("/raw/HTTPS://api.openai.com/v1").as_deref(),
            Ok("https://api.openai.com/v1")
        );
        assert_eq!(
            raw("/raw/HtTp/api.openai.com/v1").as_deref(),
            Ok("http://api.openai.com/v1")
        );
    }

    /// §5.1「Raw 非法」：缺 scheme / 未知 scheme / 空 authority → 400，
    /// **不猜测补全**。
    #[test]
    fn resolve_raw_malformed_targets_return_invalid_raw_target() {
        for target in [
            "/raw/api.openai.com/v1",        // 缺 scheme —— 绝不默认补 https
            "/raw/",                         // 什么都没有
            "/raw/ftp://example.com/x",      // 未知 scheme
            "/raw/file:///etc/passwd",       // 尤其是这个
            "/raw/ws://example.com/x",       //
            "/raw/https:///v1/responses",    // 空 authority
            "/raw/https//api.openai.com/v1", // 前置组件折叠了 `//` 之后的样子
            "/raw/https:/api.openai.com/v1", // 同上，另一种折叠结果
        ] {
            let err = raw(target).unwrap_err();
            assert_eq!(err, ProxyError::InvalidRawTarget, "target={target}");
            assert_eq!(err.http_status(), 400, "target={target}");
        }
    }

    /// `https://api.openai.com@evil.com/v1` 的 authority 是 `evil.com`。
    /// 人眼与 allowlist 得出不同结论的地方就是 SSRF 的入口（`docs/04` §4.4）。
    #[test]
    fn resolve_raw_authority_carrying_userinfo_returns_invalid_raw_target() {
        for target in [
            "/raw/https://api.openai.com@evil.com/v1",
            "/raw/https/api.openai.com@evil.com/v1",
            "/raw/https://user:pass@example.com/v1",
        ] {
            assert_eq!(
                raw(target),
                Err(ProxyError::InvalidRawTarget),
                "target={target}"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod resolver_wiring {
    use super::{
        CredentialMode, ObserverKind, ProxyError, TargetOrigin, TestProfiles, parts, resolve_target,
    };

    #[test]
    fn resolve_target_managed_path_marks_the_origin_as_managed() {
        let profiles = TestProfiles::one("openai-prod", "https://api.openai.com");
        let t =
            resolve_target(&parts("/provider/openai-prod/v1/responses"), &profiles).expect("合法");
        assert_eq!(t.origin.as_str(), "managed");
        assert_eq!(t.origin.profile_name(), Some("openai-prod"));
        assert_eq!(t.attempt, 0);
        // Profile 上的默认 observer 原样带出，R04 再细化
        assert_eq!(t.observer, ObserverKind::OpenAiChat);
    }

    /// `docs/04` §9：Raw 默认 passthrough —— Argus 不承担「用我的 key 打任意
    /// 地址」的风险。观测则退到 Generic：认不出协议 ≠ 不观测。
    #[test]
    fn resolve_target_raw_path_defaults_to_passthrough_and_a_generic_observer() {
        let t = resolve_target(
            &parts("/raw/https://api.openai.com/v1/responses"),
            &TestProfiles::default(),
        )
        .expect("合法");
        assert_eq!(t.origin, TargetOrigin::Raw);
        assert_eq!(t.origin.profile_name(), None);
        assert_eq!(t.credential.as_str(), CredentialMode::Passthrough.as_str());
        assert_eq!(t.observer, ObserverKind::GenericJson);
    }

    /// 路由表只有三条，其余一律 404（`docs/06` §6.1）。运维端点在这一层
    /// 之前就被分流，走不到这里。
    #[test]
    fn resolve_target_unknown_prefix_returns_provider_not_found() {
        for target in [
            "/",
            "/healthz",
            "/v1/messages",
            "/provider",
            "/raw",
            "/rawx/y",
        ] {
            assert_eq!(
                resolve_target(&parts(target), &TestProfiles::default()).map(|_| ()),
                Err(ProxyError::ProviderNotFound),
                "target={target}"
            );
        }
    }
}
