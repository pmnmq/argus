//! `ProxyError` 与它到 HTTP / 重试 / 阶段的映射。
//!
//! 设计依据：`docs/06-data-plane.md` §7,§9；`docs/08-provider-profiles.md` §7.1；
//! `docs/14-auth-and-credentials.md` §6；`docs/17-reliability.md` §5。

use core::{fmt, time::Duration};

use compact_str::CompactString;
use http::StatusCode;

use crate::tee::CancelReason;

/// 请求在上游状态机里走到了哪一步。
///
/// 这个顺序是有意义的：它决定了「provider 有没有可能已经产生副作用」
/// 以及「有没有字节已经写给客户端」。`Ord` 按状态推进方向排列。
/// 见 `docs/06` §7.1。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum UpstreamStage {
    /// 还没走到上游（认证、限流、选路阶段的失败）
    #[default]
    Local,
    /// 请求构造好了但一个字节都没发出。provider 完全没收到，重试绝对安全。
    S0NotSent,
    /// 请求已写入 socket，但还没收到 response header。
    /// provider **可能**已经开始生成并计费。
    S1Sent,
    /// 收到了 response header，但还没有任何字节写给客户端。
    /// 这是最有价值的 fallback 点：切换 provider 客户端完全无感知。
    S2Headers,
    /// 已经向客户端写出。**从这里开始永不重试**（宪法 C18）。
    S3Committed,
}

impl UpstreamStage {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S0NotSent => "s0_not_sent",
            Self::S1Sent => "s1_sent",
            Self::S2Headers => "s2_headers",
            Self::S3Committed => "s3_committed",
        }
    }

    /// 是否已经向客户端提交了响应。
    #[must_use]
    pub const fn is_committed(self) -> bool {
        matches!(self, Self::S3Committed)
    }
}

/// 配额的三个维度。任一超限即 429。
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum QuotaKind {
    /// 请求数 / 时间窗
    Rate,
    /// input + output tokens
    Token,
    /// 货币（micro-USD）
    Cost,
}

impl QuotaKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rate => "rate",
            Self::Token => "token",
            Self::Cost => "cost",
        }
    }
}

/// Data Plane 的错误。
///
/// 每个变体都能回答四个问题：给客户端什么 status、错误码叫什么、
/// 走到哪个阶段失败的、能不能重试。
///
/// 变体里的字符串一律是**错误描述**，绝不含 prompt、响应内容或凭据
/// （宪法 C27）—— 它们会被 `ErrorRenderer` 写进响应体。
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyError {
    // ── 认证：全部 fail-close（宪法 C17 的唯一例外面）──────────────────
    #[error("invalid api key")]
    InvalidApiKey,
    #[error("api key expired")]
    KeyExpired,
    #[error("api key revoked")]
    KeyRevoked,
    #[error("source ip not allowed")]
    IpNotAllowed,
    /// 认证后端不可用且缓存未命中。**不放行** —— 认证是唯一不 fail-open 的环节。
    #[error("authentication backend unavailable")]
    AuthUnavailable,

    // ── 授权与准入 ────────────────────────────────────────────────────
    #[error("insufficient scope: {0}")]
    ScopeDenied(&'static str),
    // ⚠️ v2 删除了 ModelNotAllowed / UnknownModel / ProtocolNotSupported：
    // 模型目录不存在，也没有 supports 矩阵（宪法 C33、ADR-005/010）。
    /// Profile 不存在 **或** 这把 Key 无权访问。
    ///
    /// 两种情况必须**不可区分**，否则可以用低权限 Key 逐个探测出租户里
    /// 配了哪些 provider（`docs/14-auth-and-credentials.md` §7.2）。
    #[error("provider not found")]
    ProviderNotFound,
    #[error("provider is disabled")]
    ProviderDisabled,
    #[error("provider is misconfigured")]
    ProviderMisconfigured,
    /// Raw 模式是显式权限，默认关闭
    #[error("raw proxy is not allowed for this key")]
    RawProxyNotAllowed,
    #[error("invalid raw target")]
    InvalidRawTarget,
    #[error("scheme not allowed")]
    SchemeNotAllowed,
    #[error("upstream path is empty")]
    EmptyUpstreamPath,
    /// 被 Target Policy 拒绝。**不携带任何 IP 或拒绝原因** ——
    /// 区分开就等于把 Argus 变成内网存活探测器（`docs/13-ssrf-protection.md` §8）。
    #[error("target is not allowed by policy")]
    TargetDenied,
    #[error("target dns resolution failed")]
    TargetDnsFailure,
    /// 连接时的实际 IP 与判定过的不一致
    #[error("target rebinding detected")]
    TargetRebindDetected,
    #[error("provider credential unavailable")]
    CredentialUnavailable,
    #[error("invalid request: {0}")]
    InvalidRequest(CompactString),
    #[error("payload too large: limit is {limit_bytes} bytes")]
    PayloadTooLarge { limit_bytes: u64 },

    // ── 限流与配额 ────────────────────────────────────────────────────
    #[error("{} quota exceeded", .kind.as_str())]
    QuotaExceeded {
        kind: QuotaKind,
        retry_after: Option<Duration>,
    },

    // ── 安全 ──────────────────────────────────────────────────────────
    #[error("blocked by security policy (rule: {0})")]
    SecurityBlocked(CompactString),

    // ── 选路 ──────────────────────────────────────────────────────────
    #[error("no healthy provider for this model")]
    NoHealthyProvider,
    /// 熔断器打开，这个候选被跳过。不产生网络请求，因此可以立刻换下一个。
    #[error("circuit open for provider {0}")]
    CircuitOpen(CompactString),
    /// 全局重试预算耗尽。这是防重试风暴的闸门，见 `docs/08` §7.2。
    #[error("retry budget exhausted")]
    RetryBudgetExhausted,

    // ── 上游 ──────────────────────────────────────────────────────────
    #[error("upstream connect failed: {0}")]
    Connect(CompactString),
    #[error("upstream timed out at {}", .stage.as_str())]
    UpstreamTimeout { stage: UpstreamStage },
    #[error("upstream connection closed at {}", .stage.as_str())]
    UpstreamIncomplete { stage: UpstreamStage },
    /// 上游返回了非 2xx。status 原样带出，`is_retryable` 按它判断。
    #[error("upstream returned status {status}")]
    UpstreamStatus { status: u16 },
    /// 已经在向客户端转发时上游断了。永不重试。
    #[error("upstream stream aborted: {0}")]
    StreamAborted(CompactString),

    // ── 生命周期与过载 ────────────────────────────────────────────────
    #[error("cancelled: {}", .reason.as_str())]
    Cancelled {
        reason: CancelReason,
        stage: UpstreamStage,
    },
    /// 过载时快速失败，绝不排队。见 `docs/17` §5。
    #[error("overloaded: {0}")]
    Overloaded(&'static str),
    #[error("server is shutting down")]
    ShuttingDown,

    // ── 兜底 ──────────────────────────────────────────────────────────
    #[error("internal error: {0}")]
    Internal(CompactString),
}

impl ProxyError {
    /// 给客户端的 HTTP status。
    #[must_use]
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::InvalidApiKey | Self::KeyExpired | Self::KeyRevoked => StatusCode::UNAUTHORIZED,
            Self::IpNotAllowed
            | Self::ScopeDenied(_)
            | Self::SecurityBlocked(_)
            | Self::RawProxyNotAllowed
            | Self::TargetDenied
            | Self::TargetRebindDetected => StatusCode::FORBIDDEN,
            // ⚠️ 与「无权访问」必须完全一致，见变体上的说明
            Self::ProviderNotFound => StatusCode::NOT_FOUND,
            Self::InvalidRequest(_)
            | Self::InvalidRawTarget
            | Self::SchemeNotAllowed
            | Self::EmptyUpstreamPath => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::QuotaExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            // Cancelled 也归到这里：客户端主动断开对应的 499 不是标准状态码，
            // 而"服务端暂时无法完成"是这几种情况共同的语义。
            Self::AuthUnavailable
            | Self::NoHealthyProvider
            | Self::CircuitOpen(_)
            | Self::Overloaded(_)
            | Self::ShuttingDown
            | Self::ProviderDisabled
            | Self::ProviderMisconfigured
            | Self::CredentialUnavailable
            | Self::Cancelled { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Connect(_)
            | Self::UpstreamIncomplete { .. }
            | Self::StreamAborted(_)
            | Self::TargetDnsFailure
            | Self::RetryBudgetExhausted => StatusCode::BAD_GATEWAY,
            Self::UpstreamTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            // 上游 status 原样透传 —— 包括 3xx（Argus 不跟随重定向，
            // 让客户端自己看见）。只有 HTTP 表示不了的数值才兜底成 502。
            Self::UpstreamStatus { status } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// 稳定的机器可读错误码。写进响应体与审计，客户端可以据此分支。
    /// 返回 `&'static str`，热路径零分配（宪法 C21）。
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        match self {
            Self::InvalidApiKey => "invalid_api_key",
            Self::KeyExpired => "key_expired",
            Self::KeyRevoked => "key_revoked",
            Self::IpNotAllowed => "ip_not_allowed",
            Self::AuthUnavailable => "auth_unavailable",
            Self::ScopeDenied(_) => "insufficient_scope",
            Self::ProviderNotFound => "provider_not_found",
            Self::ProviderDisabled => "provider_disabled",
            Self::ProviderMisconfigured => "provider_misconfigured",
            Self::RawProxyNotAllowed => "raw_proxy_not_allowed",
            Self::InvalidRawTarget => "invalid_raw_target",
            Self::SchemeNotAllowed => "scheme_not_allowed",
            Self::EmptyUpstreamPath => "empty_upstream_path",
            Self::TargetDenied => "target_denied",
            Self::TargetDnsFailure => "target_dns_failure",
            Self::TargetRebindDetected => "target_rebind_detected",
            Self::CredentialUnavailable => "credential_unavailable",
            Self::InvalidRequest(_) => "invalid_request",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::QuotaExceeded { .. } => "quota_exceeded",
            Self::SecurityBlocked(_) => "argus_security_block",
            Self::NoHealthyProvider => "no_healthy_provider",
            Self::CircuitOpen(_) => "circuit_open",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
            Self::Connect(_) => "upstream_connect_error",
            Self::UpstreamTimeout { .. } => "upstream_timeout",
            Self::UpstreamIncomplete { .. } => "upstream_incomplete",
            Self::UpstreamStatus { .. } => "upstream_error",
            Self::StreamAborted(_) => "upstream_stream_aborted",
            Self::Cancelled { .. } => "cancelled",
            Self::Overloaded(_) => "overloaded",
            Self::ShuttingDown => "shutting_down",
            Self::Internal(_) => "internal_error",
        }
    }

    /// 失败发生在上游状态机的哪一步。
    #[must_use]
    pub const fn stage(&self) -> UpstreamStage {
        match self {
            // 还没走到上游
            Self::InvalidApiKey
            | Self::KeyExpired
            | Self::KeyRevoked
            | Self::IpNotAllowed
            | Self::AuthUnavailable
            | Self::ScopeDenied(_)
            | Self::ProviderNotFound
            | Self::ProviderDisabled
            | Self::ProviderMisconfigured
            | Self::RawProxyNotAllowed
            | Self::InvalidRawTarget
            | Self::SchemeNotAllowed
            | Self::EmptyUpstreamPath
            | Self::TargetDenied
            | Self::TargetDnsFailure
            | Self::TargetRebindDetected
            | Self::CredentialUnavailable
            | Self::InvalidRequest(_)
            | Self::PayloadTooLarge { .. }
            | Self::QuotaExceeded { .. }
            | Self::SecurityBlocked(_)
            | Self::NoHealthyProvider
            | Self::RetryBudgetExhausted
            | Self::Overloaded(_)
            | Self::ShuttingDown
            | Self::Internal(_) => UpstreamStage::Local,

            // 决定不发这个请求 —— 等价于「没发出去」
            Self::CircuitOpen(_) | Self::Connect(_) => UpstreamStage::S0NotSent,

            // 上游 status 已知，说明 header 收到了
            Self::UpstreamStatus { .. } => UpstreamStage::S2Headers,

            // 已经在转发 body
            Self::StreamAborted(_) => UpstreamStage::S3Committed,

            // 这几个自己带阶段
            Self::UpstreamTimeout { stage }
            | Self::UpstreamIncomplete { stage }
            | Self::Cancelled { stage, .. } => *stage,
        }
    }

    /// 能不能换个 provider 再试一次。
    ///
    /// # 宪法 C18
    /// 只要走到了 `S3Committed`（已经有字节写给客户端），**无条件** `false`。
    /// 这一条不可配置，也不允许被下面任何 match 分支推翻 —— 所以它写在最前面
    /// 的早退里，而不是散在各个分支中。
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        if self.stage().is_committed() {
            return false;
        }
        match self {
            // S0：provider 完全没收到，绝对安全
            Self::Connect(_) | Self::CircuitOpen(_) => true,

            // S1：可能已计费。是否重试由 RetryPolicy.on_no_response 决定，
            // 这里给出「类型上允许」，最终由 argus-proxy::upstream 结合策略裁决。
            Self::UpstreamTimeout { stage } | Self::UpstreamIncomplete { stage } => {
                !stage.is_committed()
            }

            // S2：看 status。401/403 刻意不重试 —— 静默换 provider 会掩盖
            // 凭据过期，直到备用也坏掉才爆发。见 docs/08 §7.1。
            Self::UpstreamStatus { status } => {
                matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
            }

            // 其余一律不重试：换个 provider 结果一样，或者根本不该重试。
            _ => false,
        }
    }

    /// 客户端应该等多久再试。用于 `Retry-After`。
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::QuotaExceeded { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// 是否应该记为 5xx 级别的服务端问题（用于告警分级）。
    #[must_use]
    pub fn is_server_fault(&self) -> bool {
        self.http_status().is_server_error()
    }
}

impl fmt::Display for UpstreamStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个变体一个样本。
    ///
    /// 加了新变体？`error_code()` 的 match 会先编译失败逼你补上，然后
    /// `error_code_across_all_variants_returns_25_distinct_codes`
    /// 会因为数量对不上而失败，逼你补到这里。
    fn samples() -> Vec<ProxyError> {
        vec![
            ProxyError::InvalidApiKey,
            ProxyError::KeyExpired,
            ProxyError::KeyRevoked,
            ProxyError::IpNotAllowed,
            ProxyError::AuthUnavailable,
            ProxyError::ScopeDenied("audit:read"),
            ProxyError::ProviderNotFound,
            ProxyError::ProviderDisabled,
            ProxyError::ProviderMisconfigured,
            ProxyError::RawProxyNotAllowed,
            ProxyError::InvalidRawTarget,
            ProxyError::SchemeNotAllowed,
            ProxyError::EmptyUpstreamPath,
            ProxyError::TargetDenied,
            ProxyError::TargetDnsFailure,
            ProxyError::TargetRebindDetected,
            ProxyError::CredentialUnavailable,
            ProxyError::InvalidRequest("missing field: model".into()),
            ProxyError::PayloadTooLarge {
                limit_bytes: 1 << 20,
            },
            ProxyError::QuotaExceeded {
                kind: QuotaKind::Token,
                retry_after: Some(Duration::from_secs(30)),
            },
            ProxyError::SecurityBlocked("shell-destructive-command".into()),
            ProxyError::NoHealthyProvider,
            ProxyError::CircuitOpen("anthropic-prod".into()),
            ProxyError::RetryBudgetExhausted,
            ProxyError::Connect("dns failure".into()),
            ProxyError::UpstreamTimeout {
                stage: UpstreamStage::S1Sent,
            },
            ProxyError::UpstreamIncomplete {
                stage: UpstreamStage::S1Sent,
            },
            ProxyError::UpstreamStatus { status: 429 },
            ProxyError::StreamAborted("connection reset".into()),
            ProxyError::Cancelled {
                reason: CancelReason::Shutdown,
                stage: UpstreamStage::S1Sent,
            },
            ProxyError::Overloaded("too many large requests"),
            ProxyError::ShuttingDown,
            ProxyError::Internal("bug".into()),
        ]
    }

    #[test]
    fn error_code_across_all_variants_returns_25_distinct_codes() {
        let mut codes: Vec<&str> = samples().iter().map(ProxyError::error_code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            33,
            "样本没覆盖全部变体，或有两个变体共用了同一个 error_code"
        );
    }

    /// 宪法 C18：一旦向客户端写出，永不重试。不可配置。
    ///
    /// 显式构造四个处于 S3 的错误，逐个断言 —— C18 的价值恰恰在于它没有例外。
    #[test]
    fn is_retryable_stage_is_s3_committed_returns_false() {
        for err in [
            ProxyError::StreamAborted("reset".into()),
            ProxyError::UpstreamTimeout {
                stage: UpstreamStage::S3Committed,
            },
            ProxyError::UpstreamIncomplete {
                stage: UpstreamStage::S3Committed,
            },
            ProxyError::Cancelled {
                reason: CancelReason::Deadline,
                stage: UpstreamStage::S3Committed,
            },
        ] {
            assert_eq!(err.stage(), UpstreamStage::S3Committed, "{err} 应该处于 S3");
            assert!(
                !err.is_retryable(),
                "{err} 已经向客户端写出，却被判为可重试（违反宪法 C18）"
            );
        }
    }

    /// 反向兜底：样本里任何走到 S3 的变体都不可重试。
    /// 加新变体时这条会自动覆盖到它。
    #[test]
    fn is_retryable_any_sample_reaching_s3_returns_false() {
        for err in samples() {
            if err.stage().is_committed() {
                assert!(!err.is_retryable(), "{err} 违反宪法 C18");
            }
        }
    }

    /// `docs/08` §7.1：这些状态码 fallback 到下一个候选。
    #[test]
    fn is_retryable_upstream_status_is_transient_returns_true() {
        for status in [408_u16, 429, 500, 502, 503, 504] {
            assert!(
                ProxyError::UpstreamStatus { status }.is_retryable(),
                "{status} 应该 fallback"
            );
        }
    }

    /// 401/403 刻意不 fallback：静默换 provider 会掩盖凭据过期，
    /// 直到备用也坏掉才爆发。400/404/413/422 换 provider 结果一样。
    #[test]
    fn is_retryable_upstream_status_is_client_error_returns_false() {
        for status in [400_u16, 401, 403, 404, 413, 422] {
            assert!(
                !ProxyError::UpstreamStatus { status }.is_retryable(),
                "{status} 不应该 fallback"
            );
        }
    }

    #[test]
    fn is_retryable_stage_is_s0_not_sent_returns_true() {
        for err in samples() {
            if err.stage() == UpstreamStage::S0NotSent {
                assert!(err.is_retryable(), "{err} 在 S0 却不可重试");
            }
        }
    }

    #[test]
    fn is_retryable_error_is_auth_failure_returns_false() {
        for err in auth_failures() {
            assert!(!err.is_retryable(), "{err} 不该重试");
        }
    }

    #[test]
    fn stage_error_is_auth_failure_returns_local() {
        for err in auth_failures() {
            assert_eq!(err.stage(), UpstreamStage::Local, "{err} 应该是 Local");
        }
    }

    /// 认证后端挂了要 fail-close 成 503，不是放行、也不是 401
    /// —— 401 会让客户端以为自己的 key 坏了。
    #[test]
    fn http_status_auth_backend_unavailable_returns_503() {
        assert_eq!(
            ProxyError::AuthUnavailable.http_status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    fn auth_failures() -> [ProxyError; 5] {
        [
            ProxyError::InvalidApiKey,
            ProxyError::KeyExpired,
            ProxyError::KeyRevoked,
            ProxyError::IpNotAllowed,
            ProxyError::AuthUnavailable,
        ]
    }

    /// R8 / 宪法 C33 的防枚举：「profile 不存在」与「无权访问」必须
    /// 对调用方**完全不可区分**。
    ///
    /// 这两者在 Data Plane 是两条不同的代码路径，很容易在某次重构里
    /// 被拆成两个错误码 —— 那时攻击者就能用低权限 Key 逐个探测出
    /// 租户里配了哪些 provider。这个测试把等价性钉死。
    #[test]
    fn provider_not_found_is_indistinguishable_from_unauthorized() {
        // 授权失败时 Data Plane 必须复用同一个变体，而不是新造一个
        let not_found = ProxyError::ProviderNotFound;
        let unauthorized = ProxyError::ProviderNotFound;
        assert_eq!(not_found.http_status(), unauthorized.http_status());
        assert_eq!(not_found.error_code(), unauthorized.error_code());
        assert_eq!(not_found.to_string(), unauthorized.to_string());
        // 且不能泄漏"存在但无权"这层信息
        assert!(!not_found.to_string().contains("allow"));
        assert!(!not_found.to_string().contains("permission"));
    }

    /// `target_denied` 不得携带任何 IP 或拒绝原因
    /// （`docs/13-ssrf-protection.md` §8）—— 区分开就等于内网存活探测器。
    #[test]
    fn display_target_denied_contains_no_address_or_reason() {
        let msg = ProxyError::TargetDenied.to_string();
        for leak in ["127.0.0.1", "10.", "192.168", "cidr", "private", "port"] {
            assert!(!msg.contains(leak), "拒绝信息泄漏了 `{leak}`：{msg}");
        }
    }

    #[test]
    fn upstream_stage_ord_follows_the_state_machine_direction() {
        assert!(UpstreamStage::Local < UpstreamStage::S0NotSent);
        assert!(UpstreamStage::S0NotSent < UpstreamStage::S1Sent);
        assert!(UpstreamStage::S1Sent < UpstreamStage::S2Headers);
        assert!(UpstreamStage::S2Headers < UpstreamStage::S3Committed);
    }

    #[test]
    fn is_committed_stage_is_s3_returns_true_and_s2_returns_false() {
        assert!(UpstreamStage::S3Committed.is_committed());
        assert!(!UpstreamStage::S2Headers.is_committed());
    }

    #[test]
    fn http_status_upstream_status_is_representable_returns_it_unchanged() {
        assert_eq!(
            ProxyError::UpstreamStatus { status: 429 }.http_status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // 3xx 也原样透传：Argus 刻意不跟随重定向（那会破坏透明性），
        // 让客户端自己看见并决定。
        assert_eq!(
            ProxyError::UpstreamStatus { status: 307 }.http_status(),
            StatusCode::TEMPORARY_REDIRECT
        );
    }

    /// 注意 `from_u16` 接受 100..=999，所以 999 是「合法但奇怪」，仍然透传。
    /// 只有 HTTP 根本表示不了的数值才兜底成 502，且不能 panic。
    #[test]
    fn http_status_upstream_status_is_not_representable_returns_502() {
        assert_eq!(
            ProxyError::UpstreamStatus { status: 1000 }.http_status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ProxyError::UpstreamStatus { status: 0 }.http_status(),
            StatusCode::BAD_GATEWAY
        );
    }

    /// 错误信息会被 `ErrorRenderer` 写进响应体，只能含描述，不能含内容。
    #[test]
    fn display_error_is_security_blocked_contains_rule_name_but_no_content() {
        let msg = ProxyError::SecurityBlocked("shell-destructive-command".into()).to_string();
        assert!(msg.contains("shell-destructive-command"));
        assert!(!msg.contains("rm -rf"));
    }
}
