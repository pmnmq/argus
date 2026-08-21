//! usage 合并：**last-non-null wins，逐字段独立**。
//!
//! **绝不求和。** Anthropic 的 `message_delta.usage.output_tokens` 是
//! **累积值**不是增量，求和会得到约 2 倍的结果 —— 而且那个结果"看起来
//! 合理"，直到有人对账（`docs/11-observation-model.md` §6、宪法 C14）。
//! 这条有专门的锁死测试。

use bytes::Bytes;

use crate::{
    interaction::{ResolvedUsage, UsageSource},
    ir::UsageDelta,
};

/// 按 token 估算的字节数。粗糙但够用，精度问题记在 `docs/11` OQ-5。
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UsageAccumulator {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    reasoning_tokens: Option<u32>,
    extra: Option<Box<Bytes>>,
    observed_count: u32,
}

impl UsageAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 每个字段独立：新值非 `None` 就覆盖，`None` 保持原值。
    pub fn apply(&mut self, d: &UsageDelta) {
        if let Some(v) = d.input_tokens {
            self.input_tokens = Some(v);
        }
        if let Some(v) = d.output_tokens {
            self.output_tokens = Some(v);
        }
        if let Some(v) = d.cache_creation_input_tokens {
            self.cache_creation_input_tokens = Some(v);
        }
        if let Some(v) = d.cache_read_input_tokens {
            self.cache_read_input_tokens = Some(v);
        }
        if let Some(v) = d.reasoning_tokens {
            self.reasoning_tokens = Some(v);
        }
        if let Some(extra) = &d.extra {
            self.extra = Some(extra.clone());
        }
        self.observed_count = self.observed_count.saturating_add(1);
    }

    /// 收到过几次 usage 上报。0 次与"报了但全是 null"是两回事。
    #[must_use]
    pub const fn observed_count(&self) -> u32 {
        self.observed_count
    }

    #[must_use]
    pub const fn has_any(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_input_tokens.is_some()
            || self.cache_read_input_tokens.is_some()
            || self.reasoning_tokens.is_some()
    }

    /// 收尾。
    ///
    /// provider 一个字段都没报时才估算，而且估算值**只写进
    /// `estimated_output_tokens`**，绝不写进真实字段 —— 计费必须能区分
    /// 实测与估算（宪法 C14）。
    ///
    /// 只有部分字段缺失的情况（报了 input 没报 output）本切片不补，
    /// 留给 F09 的 [`UsageSource::Estimated`]。
    #[must_use]
    pub fn resolve(self, accumulated_text_bytes: usize) -> ResolvedUsage {
        let reported = self.has_any();
        ResolvedUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            reasoning_tokens: self.reasoning_tokens,
            source: if reported {
                UsageSource::Reported
            } else {
                UsageSource::Missing
            },
            estimated_output_tokens: if reported {
                None
            } else {
                Some(estimate_tokens(accumulated_text_bytes))
            },
            extra: self.extra,
        }
    }
}

/// `text_bytes / 4`。
///
/// `checked_div` 而不是 `/`：本 crate deny 了 `integer_division`，那条 lint
/// 的用意是让每一次整除都显式说明"除数不会是 0"。这里除数是常量，但绕过
/// lint 的正确方式是用 checked 版本而不是 `#[allow]`。
fn estimate_tokens(text_bytes: usize) -> u32 {
    let tokens = text_bytes.checked_div(BYTES_PER_TOKEN_ESTIMATE).unwrap_or(0);
    u32::try_from(tokens).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(input: Option<u32>, output: Option<u32>) -> UsageDelta {
        UsageDelta {
            input_tokens: input,
            output_tokens: output,
            ..UsageDelta::default()
        }
    }

    /// **本 spec 最危险的一条**（F04 §5）：Anthropic 的
    /// `message_delta.usage.output_tokens` 是累积值。求和会让计费翻倍，
    /// 而且结果"看起来合理"。这条测试把它锁死。
    #[test]
    fn apply_repeated_output_token_reports_keeps_the_last_not_the_sum() {
        let mut acc = UsageAccumulator::new();
        acc.apply(&delta(Some(25), None));
        acc.apply(&delta(None, Some(1)));
        acc.apply(&delta(None, Some(15)));

        let usage = acc.resolve(0);
        assert_eq!(usage.input_tokens, Some(25));
        assert_eq!(usage.output_tokens, Some(15), "必须是 15，不是 1+15=16");
    }

    /// 逐字段独立：后来的 `None` 不会把先前报过的值抹掉。
    #[test]
    fn apply_a_delta_with_none_fields_does_not_clear_earlier_values() {
        let mut acc = UsageAccumulator::new();
        acc.apply(&delta(Some(25), Some(7)));
        acc.apply(&delta(None, None));

        let usage = acc.resolve(0);
        assert_eq!(usage.input_tokens, Some(25));
        assert_eq!(usage.output_tokens, Some(7));
    }

    /// 每个字段各自 last-wins，互不影响。
    #[test]
    fn apply_deltas_touching_different_fields_merges_them_independently() {
        let mut acc = UsageAccumulator::new();
        acc.apply(&UsageDelta {
            input_tokens: Some(100),
            cache_read_input_tokens: Some(80),
            ..UsageDelta::default()
        });
        acc.apply(&UsageDelta {
            output_tokens: Some(9),
            reasoning_tokens: Some(4),
            ..UsageDelta::default()
        });
        acc.apply(&UsageDelta {
            cache_read_input_tokens: Some(81),
            ..UsageDelta::default()
        });

        let usage = acc.resolve(0);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(9));
        assert_eq!(usage.cache_read_input_tokens, Some(81));
        assert_eq!(usage.reasoning_tokens, Some(4));
        assert_eq!(usage.source, UsageSource::Reported);
        assert_eq!(usage.estimated_output_tokens, None);
    }

    /// R7：完全缺失 → 估算值只进 `estimated_output_tokens`，真实字段留空。
    #[test]
    fn resolve_without_any_usage_report_marks_missing_and_only_estimates() {
        let usage = UsageAccumulator::new().resolve(400);
        assert_eq!(usage.source, UsageSource::Missing);
        assert_eq!(usage.output_tokens, None, "估算值绝不写进真实字段");
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.estimated_output_tokens, Some(100));
    }

    /// 报过哪怕一个字段，就不再估算 —— 免得实测与估算混在一起。
    #[test]
    fn resolve_with_a_partial_report_does_not_estimate() {
        let mut acc = UsageAccumulator::new();
        acc.apply(&delta(Some(25), None));

        let usage = acc.resolve(4000);
        assert_eq!(usage.source, UsageSource::Reported);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.estimated_output_tokens, None, "部分缺失留给 F09");
    }

    /// 报了一次但全是 null：算「报过」吗？不算 —— 一个数值都没有，
    /// 落库时写 `reported` 会让下游以为这是实测的 0。
    #[test]
    fn resolve_after_an_all_null_report_still_counts_as_missing() {
        let mut acc = UsageAccumulator::new();
        acc.apply(&UsageDelta::default());
        assert_eq!(acc.observed_count(), 1, "确实收到过一次上报");
        assert!(!acc.has_any(), "但一个数值都没有");

        let usage = acc.resolve(40);
        assert_eq!(usage.source, UsageSource::Missing);
        assert_eq!(usage.estimated_output_tokens, Some(10));
    }

    #[test]
    fn observed_count_counts_every_report_including_all_null_ones() {
        let mut acc = UsageAccumulator::new();
        assert_eq!(acc.observed_count(), 0);
        acc.apply(&UsageDelta::default());
        acc.apply(&delta(Some(1), None));
        assert_eq!(acc.observed_count(), 2);
    }

    #[test]
    fn estimate_tokens_divides_text_bytes_by_four() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(3), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(4001), 1000);
    }
}
