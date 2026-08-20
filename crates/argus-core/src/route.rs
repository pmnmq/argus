//! 上游请求携带的策略：`Timeouts` / `RetryPolicy` / `Backoff`。
//!
//! v2 里**没有 `RouteDecision`** —— 解析结果是 `crate::target::ResolvedTarget`，
//! 因为 provider 由 URL 决定而不是选出来的（`docs/02-routing-model.md`）。
//! `ObserveMode` 迁到了 `crate::observe`。
//!
//! 设计依据：`docs/06-data-plane.md` §10、`docs/08-provider-profiles.md` §7。

use core::time::Duration;

use serde::{Deserialize, Serialize};

/// 四段式超时。
///
/// 分成四个而不是一个，是因为 AI 流式请求的形状很特别：**首 token 慢、
/// token 之间快、总时长可能很长**。用单一超时要么误杀推理模型（设短了），
/// 要么卡死的连接挂满几分钟才被回收（设长了）。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Timeouts {
    /// TCP + TLS 建连
    #[serde(with = "humantime_serde")]
    pub connect: Duration,
    /// 发出请求到收到 response header。推理模型首 token 可能超过 30 秒，
    /// 设短了会大量误触发 S1 重试（而 S1 重试有重复计费风险）。
    #[serde(with = "humantime_serde")]
    pub headers: Duration,
    /// 两个 body frame 之间的最大间隔。这一条才是「流卡住了」的判据。
    #[serde(with = "humantime_serde")]
    pub idle_frame: Duration,
    /// 整个请求的硬上限
    #[serde(with = "humantime_serde")]
    pub total: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(3),
            headers: Duration::from_secs(60),
            idle_frame: Duration::from_secs(120),
            total: Duration::from_secs(900),
        }
    }
}

/// 指数退避参数。
#[derive(Copy, Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Backoff {
    #[serde(with = "humantime_serde")]
    pub initial: Duration,
    #[serde(with = "humantime_serde")]
    pub max: Duration,
    pub multiplier: f32,
    /// 抖动比例（0.0–1.0）。没有抖动，同时失败的请求会同步重试，
    /// 把一次故障放大成周期性的尖峰。
    pub jitter: f32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(1),
            multiplier: 2.0,
            jitter: 0.3,
        }
    }
}

/// 重试与 fallback 预算。
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// 含首次尝试。3 表示最多 2 次重试。
    pub max_attempts: u8,
    /// 同一个 provider 最多试几次。默认 1 —— 在刚失败的 provider 上重试
    /// 通常只是浪费预算，应该换一个。
    pub max_attempts_per_provider: u8,
    /// 总的 fallback 时间预算，硬性。超过就直接失败，让客户端自己决定。
    #[serde(with = "humantime_serde")]
    pub budget: Duration,
    /// S1（请求已发出但没收到 header）是否重试。
    ///
    /// 默认 `true`，但这是个**有代价的**默认：provider 可能已经开始生成并计费。
    /// 每次这样的重试都必须在审计里记 `retry_after_send = true`，
    /// 让重复计费事后可核查。见 `docs/08` §7.3。
    pub on_no_response: bool,
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_attempts_per_provider: 1,
            budget: Duration::from_secs(10),
            on_no_response: true,
            backoff: Backoff::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 默认值直接对应 `docs/06` §10。改这些数字之前先改文档 ——
    /// 这个测试是那条纪律的执行者（宪法 C31）。
    #[test]
    fn timeouts_default_matches_docs_02_values() {
        let t = Timeouts::default();
        assert_eq!(t.connect, Duration::from_secs(3));
        assert_eq!(t.headers, Duration::from_secs(60));
        assert_eq!(t.idle_frame, Duration::from_secs(120));
        assert_eq!(t.total, Duration::from_secs(900));
    }

    /// 四段超时必须构成递增的层次，否则外层永远轮不到触发。
    #[test]
    fn timeouts_default_forms_a_consistent_hierarchy() {
        let t = Timeouts::default();
        // headers 必须给推理模型留足首 token 时间，否则会大量误触发 S1 重试
        assert!(t.headers >= Duration::from_secs(30));
        assert!(t.connect < t.headers);
        assert!(t.headers <= t.total);
        assert!(t.idle_frame < t.total);
    }

    /// 默认值直接对应 `docs/08` §7.2。
    #[test]
    fn retry_policy_default_matches_docs_07_values() {
        let r = RetryPolicy::default();
        assert_eq!(r.max_attempts, 3);
        assert_eq!(r.max_attempts_per_provider, 1);
        assert_eq!(r.budget, Duration::from_secs(10));
        assert!(r.on_no_response);
        assert_eq!(r.backoff.initial, Duration::from_millis(50));
        assert_eq!(r.backoff.max, Duration::from_secs(1));
        // 没有抖动，同时失败的请求会同步重试，把故障放大成周期性尖峰
        assert!(r.backoff.jitter > 0.0);
    }

    /// 配置文件里写的是 "3s"，不是 `{ secs: 3, nanos: 0 }`。
    #[test]
    fn deserialize_timeouts_given_humantime_strings_parses_durations() {
        let t: Timeouts = serde_json::from_str(
            r#"{"connect":"3s","headers":"1m","idle_frame":"2m","total":"15m"}"#,
        )
        .expect("humantime 格式");
        assert_eq!(t, Timeouts::default());
    }
}
