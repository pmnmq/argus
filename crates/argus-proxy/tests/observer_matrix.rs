//! R04 的四级判定矩阵。
//!
//! 每一级各验一例，外加两条最容易被写坏的性质：**首个命中生效**，
//! 以及**选错只降级、主链路不受影响**。

mod common;

use std::sync::Arc;

use argus_core::{
    CredentialMode, ObserveMode, ObserverKind, ProviderProfileId, ResolvedTarget, RetryPolicy,
    TargetOrigin, Timeouts,
};
use argus_proxy::observer::{
    ObserverRule, ObserverRules, RequestFacts, SelectCtx, guess_raw_shape, provisional,
    resolve_observer,
};
use http::{HeaderValue, Method, Uri};

/// 造一个 Managed target，`observer` 是 Profile 上的默认值。
fn managed(uri: &str, profile: &str, default_observer: ObserverKind) -> ResolvedTarget {
    target(
        uri,
        TargetOrigin::Managed {
            profile: ProviderProfileId::generate(),
            profile_name: profile.into(),
        },
        default_observer,
    )
}

/// 造一个 Raw target。R02 给它填的 `observer` 是 `GenericJson` 占位值。
fn raw(uri: &str) -> ResolvedTarget {
    target(uri, TargetOrigin::Raw, ObserverKind::GenericJson)
}

fn target(uri: &str, origin: TargetOrigin, observer: ObserverKind) -> ResolvedTarget {
    ResolvedTarget {
        attempt: 0,
        origin,
        uri: uri.parse::<Uri>().expect("固定的合法 URI"),
        credential: CredentialMode::None,
        observer,
        timeouts: Timeouts::default(),
        retry: Arc::new(RetryPolicy::default()),
        observe: ObserveMode::default(),
    }
}

const GET: RequestFacts<'static> = RequestFacts {
    method: &Method::GET,
    content_type: None,
};

fn posting(content_type: Option<&str>) -> RequestFacts<'_> {
    RequestFacts {
        method: &Method::POST,
        content_type,
    }
}

fn ct(v: &str) -> HeaderValue {
    HeaderValue::from_str(v).expect("固定的合法 header")
}

// ─────────────────────────────────────────────────────────────────────────
mod observer_rule_matching {
    use super::{
        GET, Method, ObserverKind, ObserverRule, ObserverRules, SelectCtx, managed, posting,
        provisional,
    };

    /// R2：**首个命中生效**。后面还有更"精确"的规则也不生效 ——
    /// 顺序就是优先级，没有权重也没有打分。
    #[test]
    fn observer_rules_select_returns_the_first_match_not_the_most_specific() {
        let rules = ObserverRules::new(vec![
            // 宽松的在前
            ObserverRule::new(ObserverKind::GenericJson).with_path_prefix("/v1"),
            // 更"精确"的在后 —— 永远轮不到
            ObserverRule::new(ObserverKind::OpenAiResponses)
                .with_profile("openai-prod")
                .with_path_prefix("/v1/responses"),
        ]);
        let t = managed(
            "https://api.openai.com/v1/responses",
            "openai-prod",
            ObserverKind::OpenAiChat,
        );
        assert_eq!(
            provisional(&t, &rules, &posting(None)),
            Some(ObserverKind::GenericJson)
        );
    }

    /// `docs/09` §4.2 的那个例子：Profile 默认是 chat，但 /v1/responses
    /// 要用另一个 observer。
    #[test]
    fn observer_rules_select_narrows_one_endpoint_of_a_profile() {
        let rules = ObserverRules::new(vec![
            ObserverRule::new(ObserverKind::OpenAiResponses)
                .with_profile("openai-prod")
                .with_path_prefix("/v1/responses"),
        ]);
        let chat = managed(
            "https://api.openai.com/v1/chat/completions",
            "openai-prod",
            ObserverKind::OpenAiChat,
        );
        let responses = managed(
            "https://api.openai.com/v1/responses",
            "openai-prod",
            ObserverKind::OpenAiChat,
        );

        assert_eq!(
            provisional(&chat, &rules, &posting(None)),
            Some(ObserverKind::OpenAiChat)
        );
        assert_eq!(
            provisional(&responses, &rules, &posting(None)),
            Some(ObserverKind::OpenAiResponses)
        );
    }

    /// R3：多个条件之间是 **AND**，差一个都不算命中。
    #[test]
    fn observer_rule_matches_requires_every_specified_dimension() {
        let rule = ObserverRule::new(ObserverKind::AnthropicMessages)
            .with_profile("anthropic-prod")
            .with_host_suffix("api.anthropic.com")
            .with_path_prefix("/v1/messages")
            .with_method(Method::POST)
            .with_content_type("application/json");

        let t = managed(
            "https://api.anthropic.com/v1/messages",
            "anthropic-prod",
            ObserverKind::OpenAiChat,
        );
        let full = posting(Some("application/json"));
        assert!(rule.matches(&SelectCtx::new(&t, &full)));

        // 换掉任意一个维度都不再命中
        assert!(!rule.matches(&SelectCtx::new(&t, &GET)), "method 不对");
        assert!(
            !rule.matches(&SelectCtx::new(&t, &posting(Some("text/plain")))),
            "content_type 不对"
        );

        let wrong_profile = managed(
            "https://api.anthropic.com/v1/messages",
            "anthropic-dev",
            ObserverKind::OpenAiChat,
        );
        assert!(
            !rule.matches(&SelectCtx::new(&wrong_profile, &full)),
            "profile 不对"
        );

        let wrong_path = managed(
            "https://api.anthropic.com/v1/complete",
            "anthropic-prod",
            ObserverKind::OpenAiChat,
        );
        assert!(
            !rule.matches(&SelectCtx::new(&wrong_path, &full)),
            "path 不对"
        );

        let wrong_host = managed(
            "https://api.openai.com/v1/messages",
            "anthropic-prod",
            ObserverKind::OpenAiChat,
        );
        assert!(
            !rule.matches(&SelectCtx::new(&wrong_host, &full)),
            "host 不对"
        );
    }

    /// host 后缀匹配自身与子域，但 `notapi.anthropic.com` 不算子域。
    #[test]
    fn observer_rule_host_suffix_matches_the_domain_and_its_subdomains_only() {
        let rule =
            ObserverRule::new(ObserverKind::AnthropicMessages).with_host_suffix("anthropic.com");
        for (host, want) in [
            ("anthropic.com", true),
            ("api.anthropic.com", true),
            ("a.b.anthropic.com", true),
            ("notanthropic.com", false),
            ("anthropic.com.evil.net", false),
        ] {
            let t = managed(
                &format!("https://{host}/v1/messages"),
                "p",
                ObserverKind::OpenAiChat,
            );
            assert_eq!(
                rule.matches(&SelectCtx::new(&t, &posting(None))),
                want,
                "host={host}"
            );
        }
    }

    /// 一条全 `None` 的规则匹配一切 —— 合法，但放在前面会挡住后面所有规则。
    #[test]
    fn observer_rules_select_with_an_unconstrained_rule_first_shadows_the_rest() {
        let rules = ObserverRules::new(vec![
            ObserverRule::new(ObserverKind::GenericSse),
            ObserverRule::new(ObserverKind::AnthropicMessages).with_path_prefix("/v1/messages"),
        ]);
        let t = managed(
            "https://api.anthropic.com/v1/messages",
            "p",
            ObserverKind::OpenAiChat,
        );
        assert_eq!(
            provisional(&t, &rules, &posting(None)),
            Some(ObserverKind::GenericSse)
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod observer_four_level_resolution {
    use super::{
        ObserverKind, ObserverRule, ObserverRules, ct, guess_raw_shape, managed, posting,
        provisional, raw, resolve_observer,
    };

    /// 第一级：规则命中。
    #[test]
    fn provisional_level_one_rule_hit_wins_over_the_profile_default() {
        let rules = ObserverRules::new(vec![
            ObserverRule::new(ObserverKind::AnthropicMessages).with_path_prefix("/v1/messages"),
        ]);
        let t = managed(
            "https://gw.corp.com/v1/messages",
            "corp",
            ObserverKind::OpenAiChat,
        );
        assert_eq!(
            provisional(&t, &rules, &posting(None)),
            Some(ObserverKind::AnthropicMessages)
        );
    }

    /// 第二级：Managed 模式没有规则命中 → 用 Profile 的默认值。
    #[test]
    fn provisional_level_two_managed_without_rules_uses_the_profile_default() {
        let t = managed(
            "https://api.openai.com/v1/embeddings",
            "openai-prod",
            ObserverKind::OpenAiChat,
        );
        assert_eq!(
            provisional(&t, &ObserverRules::default(), &posting(None)),
            Some(ObserverKind::OpenAiChat)
        );
    }

    /// 第三级：Raw 模式按 path 形状猜。
    #[test]
    fn provisional_level_three_raw_guesses_the_shape_from_the_path() {
        for (uri, want) in [
            (
                "https://api.openai.com/v1/chat/completions",
                ObserverKind::OpenAiChat,
            ),
            (
                "https://api.openai.com/v1/responses",
                ObserverKind::OpenAiResponses,
            ),
            (
                "https://api.anthropic.com/v1/messages",
                ObserverKind::AnthropicMessages,
            ),
            // 形状比 host 强：自建的 anthropic-compatible 服务也猜得对
            (
                "http://nas.local:8000/v1/messages",
                ObserverKind::AnthropicMessages,
            ),
        ] {
            assert_eq!(
                provisional(&raw(uri), &ObserverRules::default(), &posting(None)),
                Some(want),
                "uri={uri}"
            );
        }
    }

    /// Raw 模式猜不出来就交给第四级，**不能**拿 R02 填的 `GenericJson`
    /// 占位值当"Profile 默认值"用 —— 那样 Content-Type 兜底永远轮不到。
    #[test]
    fn provisional_level_three_raw_with_an_unknown_shape_defers_to_content_type() {
        let t = raw("https://weird.example/some/unknown/endpoint");
        assert_eq!(
            provisional(&t, &ObserverRules::default(), &posting(None)),
            None
        );
        assert_eq!(guess_raw_shape("/some/unknown/endpoint"), None);
    }

    /// 第四级 R4：`text/event-stream` → `GenericSse`。
    #[test]
    fn resolve_observer_level_four_event_stream_falls_back_to_generic_sse() {
        assert_eq!(
            resolve_observer(None, Some(&ct("text/event-stream"))),
            ObserverKind::GenericSse
        );
        // 带参数也算
        assert_eq!(
            resolve_observer(None, Some(&ct("text/event-stream; charset=utf-8"))),
            ObserverKind::GenericSse
        );
    }

    /// 第四级 R5：`application/json` → `GenericJson`，`+json` 后缀也算。
    #[test]
    fn resolve_observer_level_four_json_falls_back_to_generic_json() {
        for v in [
            "application/json",
            "application/json; charset=utf-8",
            "application/vnd.x+json",
        ] {
            assert_eq!(
                resolve_observer(None, Some(&ct(v))),
                ObserverKind::GenericJson,
                "content_type={v}"
            );
        }
    }

    /// 流程图的「其他 Content-Type → 只记元数据」：交给 `GenericJson`，
    /// 它在非 JSON 上退化成 Malformed，留下的正好只有字节数与形状。
    #[test]
    fn resolve_observer_level_four_other_content_types_record_metadata_only() {
        assert_eq!(
            resolve_observer(None, Some(&ct("application/octet-stream"))),
            ObserverKind::GenericJson
        );
        assert_eq!(resolve_observer(None, None), ObserverKind::GenericJson);
    }

    /// §5：定下来了就**只准收窄**，不准换成另一个语义 Observer ——
    /// 换掉会让已经解析出来的事件与后面的不一致。
    #[test]
    fn resolve_observer_keeps_a_semantic_choice_when_the_content_type_can_carry_it() {
        for v in ["text/event-stream", "application/json"] {
            assert_eq!(
                resolve_observer(Some(ObserverKind::AnthropicMessages), Some(&ct(v))),
                ObserverKind::AnthropicMessages,
                "content_type={v}"
            );
        }
        // 缺 Content-Type 时按"能处理"办：不少上游流式响应不发这个头，
        // 一律降级会把正常的流一起降掉
        assert_eq!(
            resolve_observer(Some(ObserverKind::OpenAiChat), None),
            ObserverKind::OpenAiChat
        );
    }

    /// 只有在 Content-Type 说明它根本解不了时才收窄。
    #[test]
    fn resolve_observer_narrows_a_semantic_choice_when_the_content_type_contradicts_it() {
        assert_eq!(
            resolve_observer(Some(ObserverKind::OpenAiChat), Some(&ct("text/html"))),
            ObserverKind::GenericJson
        );
    }

    /// 已经是 Generic 的不会被再"收窄"一次成另一个 Generic。
    #[test]
    fn resolve_observer_leaves_an_already_generic_choice_untouched() {
        assert_eq!(
            resolve_observer(Some(ObserverKind::GenericSse), Some(&ct("text/html"))),
            ObserverKind::GenericSse
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod observer_degradation {
    use super::{ObserverKind, ObserverRules, ct, managed, posting, provisional, resolve_observer};
    use argus_protocol::{GenericJsonObserver, GenericSseObserver, JsonShape};

    /// R8 / §4.4：选错 Observer 的后果是**观测降级**。
    ///
    /// 主链路一个字节都不受影响 —— 它根本不看 Observer 的输出，这是
    /// `docs/05` §4.2 那条分工的直接好处。本切片还没有主链路可验，能验的是
    /// 另一半：观测器自己在"喂错东西"时不 panic、不报错，只是标记不完整。
    #[test]
    fn observe_a_wrong_kind_of_body_degrades_the_observation_without_failing() {
        // 明明是 SSE，却按 JSON 观测
        let sse_body = b"data: {\"a\":1}\n\ndata: [DONE]\n\n";
        let obs = GenericJsonObserver::observe(sse_body);
        assert_eq!(obs.shape(), JsonShape::Malformed);
        assert!(!obs.is_complete(), "标记为不完整，对应 Integrity::Degraded");
        // 但字节数这类元数据仍然记下来了 —— 降级不等于一片空白
        assert_eq!(obs.body_bytes(), sse_body.len() as u64);
        assert!(obs.top_level_keys().is_empty());
    }

    /// 反过来：把 JSON 喂给 SSE 观测器，同样只是数不出帧。
    #[test]
    fn observe_json_with_the_sse_observer_yields_no_frames_and_no_panic() {
        let obs = GenericSseObserver::new();
        assert_eq!(obs.frames(), 0);
        assert!(obs.is_complete());
    }

    /// 「认不出协议」≠「不观测」：完全未知的上游仍然拿得到基础观测。
    #[test]
    fn resolve_observer_an_entirely_unknown_upstream_still_gets_an_observer() {
        let t = managed(
            "https://unknown.example/some/api",
            "custom",
            ObserverKind::GenericJson,
        );
        let kind = resolve_observer(
            provisional(&t, &ObserverRules::default(), &posting(None)),
            Some(&ct("text/event-stream")),
        );
        // Profile 默认值是 GenericJson，Content-Type 不与之矛盾，因此保留
        assert_eq!(kind, ObserverKind::GenericJson);
        assert!(!kind.is_semantic(), "兜底的两个都不解语义");
    }
}
