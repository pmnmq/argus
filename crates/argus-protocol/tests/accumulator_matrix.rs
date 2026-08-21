//! F04 §7 的验收矩阵。
//!
//! 这一层的 bug 全都是"数据看起来对但其实错了"那一类：usage 翻倍、
//! 截断了却没标、丢了字节却报 Complete。所以断言的一律是**具体数值**，
//! 不是"没有 panic"。

use argus_core::{CancelReason, ObserveMode, ObserverKind, TraceContext};
use argus_protocol::{
    accumulator::{Accumulator, AccumulatorConfig, RawMode, RequestMeta},
    interaction::{Integrity, Outcome, UsageSource},
    ir::{
        BlockCoord, BlockDelta, BlockKind, ObservedEvent, ObservedEventKind, RawRef, TextChunk,
        UsageDelta,
    },
};
use bytes::Bytes;

fn meta() -> RequestMeta {
    RequestMeta {
        trace: TraceContext::root(),
        observer: ObserverKind::AnthropicMessages,
        observe_mode: ObserveMode::Full,
        stream: true,
        observed_model: Some("claude-x".into()),
        request_sha256: None,
        raw_request: None,
    }
}

fn acc() -> Accumulator {
    Accumulator::new(meta(), AccumulatorConfig::default())
}

fn acc_with(cfg: AccumulatorConfig) -> Accumulator {
    Accumulator::new(meta(), cfg)
}

/// 事件序号由调用顺序隐含，测试里手动给就够了。
fn ev(seq: u32, kind: ObservedEventKind) -> ObservedEvent {
    ObservedEvent::new(seq, seq * 1000, kind)
}

fn text_delta(seq: u32, coord: BlockCoord, s: &str) -> ObservedEvent {
    ev(
        seq,
        ObservedEventKind::block_delta(coord, BlockDelta::Text(TextChunk::Decoded(s.into()))),
    )
}

const C0: BlockCoord = BlockCoord {
    choice: 0,
    item: 0,
    part: 0,
};

// ─────────────────────────────────────────────────────────────────────────
mod accumulator_usage {
    use super::{C0, ObservedEventKind, Outcome, UsageDelta, UsageSource, acc, ev, text_delta};
    use argus_protocol::ir::{BlockKind, FinishReason};

    /// F04 §7 点名的锁死用例：`input=25`、`output=1`、`output=15`
    /// 必须得到 `(25, 15)` 而不是 `(25, 16)`。
    ///
    /// Anthropic 的 `message_delta.usage.output_tokens` 是**累积值**。
    /// 求和的结果会翻倍，而且"看起来合理"，直到有人对账。
    #[test]
    fn accumulator_repeated_usage_reports_take_the_last_value_not_the_sum() {
        let mut a = acc();
        a.apply(&ev(
            0,
            ObservedEventKind::usage(UsageDelta {
                input_tokens: Some(25),
                ..UsageDelta::default()
            }),
        ));
        a.apply(&ev(
            1,
            ObservedEventKind::usage(UsageDelta {
                output_tokens: Some(1),
                ..UsageDelta::default()
            }),
        ));
        a.apply(&ev(
            2,
            ObservedEventKind::usage(UsageDelta {
                output_tokens: Some(15),
                ..UsageDelta::default()
            }),
        ));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.usage.input_tokens, Some(25));
        assert_eq!(out.usage.output_tokens, Some(15), "必须是 15，不是 16");
        assert_eq!(out.usage.source, UsageSource::Reported);
    }

    /// R7：usage 完全缺失 → `Missing` + 估算值，且真实字段留空。
    #[test]
    fn accumulator_without_any_usage_event_reports_missing_and_an_estimate() {
        let mut a = acc();
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        // 400 字节文本 → 估 100 token
        a.apply(&text_delta(1, C0, &"x".repeat(400)));
        a.apply(&ev(
            2,
            ObservedEventKind::finish(argus_protocol::ir::Finish::new(FinishReason::Stop)),
        ));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.usage.source, UsageSource::Missing);
        assert_eq!(out.usage.output_tokens, None, "估算值绝不写进真实字段");
        assert_eq!(out.usage.estimated_output_tokens, Some(100));
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod accumulator_text {
    use super::{
        AccumulatorConfig, BlockCoord, BlockKind, C0, ObservedEventKind, Outcome, UsageDelta,
        UsageSource, acc_with, ev, text_delta,
    };

    fn small_cap(bytes: usize) -> AccumulatorConfig {
        AccumulatorConfig {
            max_accumulated_bytes: bytes,
            ..AccumulatorConfig::default()
        }
    }

    /// R8 / 宪法 C10：撞上限就停止累积，但**继续消费后续事件** ——
    /// usage 这类结构性信息一个都不能丢。
    #[test]
    fn accumulator_text_exceeding_the_cap_truncates_but_keeps_consuming_events() {
        let mut a = acc_with(small_cap(10));
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        a.apply(&text_delta(1, C0, "0123456789abcdef"));
        // 上限之后又来了一堆
        a.apply(&text_delta(2, C0, "more text"));
        a.apply(&ev(
            3,
            ObservedEventKind::usage(UsageDelta {
                input_tokens: Some(7),
                output_tokens: Some(3),
                ..UsageDelta::default()
            }),
        ));

        let out = a.finish(Outcome::Completed);
        let block = out.blocks.first().expect("有一个 block");
        assert!(block.truncated, "必须标记截断");
        assert_eq!(block.text, "0123456789");
        // 关键：截断之后的 usage 仍然被正确记录
        assert_eq!(out.usage.input_tokens, Some(7));
        assert_eq!(out.usage.output_tokens, Some(3));
        assert_eq!(out.usage.source, UsageSource::Reported);
    }

    /// 预算是**全部 block 共享**的，不是每 block 一份。
    #[test]
    fn accumulator_cap_is_shared_across_blocks() {
        let mut a = acc_with(small_cap(6));
        let c1 = BlockCoord::item(1);
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        a.apply(&ev(1, ObservedEventKind::block_start(c1, BlockKind::Text)));
        a.apply(&text_delta(2, C0, "abcd"));
        a.apply(&text_delta(3, c1, "efgh"));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.blocks[0].text, "abcd");
        assert_eq!(out.blocks[1].text, "ef");
        assert!(out.blocks[1].truncated);
        assert!(!out.blocks[0].truncated);
    }

    /// 没见过 `BlockStart` 的 delta 不能被丢掉 —— 补一个块并记一条异常。
    #[test]
    fn accumulator_delta_without_a_start_creates_the_block_and_records_an_anomaly() {
        let mut a = acc_with(AccumulatorConfig::default());
        a.apply(&text_delta(0, C0, "orphan"));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.blocks.len(), 1);
        assert_eq!(out.blocks[0].text, "orphan");
        assert_eq!(out.stats.anomalies.len(), 1);
        assert_eq!(out.stats.anomalies[0].as_str(), "implicit_start");
    }

    /// `BTreeMap` 保证按坐标有序输出，与事件到达顺序无关。
    #[test]
    fn accumulator_blocks_are_emitted_in_coordinate_order() {
        let mut a = acc_with(AccumulatorConfig::default());
        for (seq, item) in [(0_u32, 2_u16), (1, 0), (2, 1)] {
            let coord = BlockCoord::item(item);
            a.apply(&ev(
                seq,
                ObservedEventKind::block_start(coord, BlockKind::Text),
            ));
            a.apply(&text_delta(seq + 10, coord, &item.to_string()));
        }
        let out = a.finish(Outcome::Completed);
        let items: Vec<u16> = out.blocks.iter().map(|b| b.coord.item).collect();
        assert_eq!(items, [0, 1, 2]);
        assert_eq!(out.blocks[0].text, "0");
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod accumulator_integrity {
    use super::{
        AccumulatorConfig, C0, CancelReason, Integrity, ObserveMode, ObservedEventKind, Outcome,
        RequestMeta, TraceContext, acc, acc_with, ev, text_delta,
    };
    use argus_core::ObserverKind;
    use argus_protocol::{accumulator::Accumulator, ir::BlockKind};

    /// R10：`Gap` → 整体降级，且**仍打开**的 block 标 degraded。
    #[test]
    fn accumulator_gap_degrades_the_interaction_and_its_open_blocks() {
        let mut a = acc();
        let closed = argus_protocol::ir::BlockCoord::item(0);
        let open = argus_protocol::ir::BlockCoord::item(1);
        a.apply(&ev(
            0,
            ObservedEventKind::block_start(closed, BlockKind::Text),
        ));
        a.apply(&ev(1, ObservedEventKind::BlockStop { at: closed }));
        a.apply(&ev(
            2,
            ObservedEventKind::block_start(open, BlockKind::Text),
        ));
        a.apply(&ev(3, ObservedEventKind::Gap { dropped_bytes: 512 }));

        let out = a.finish(Outcome::Completed);
        assert_eq!(
            out.integrity,
            Integrity::Degraded {
                dropped_bytes: 512,
                gap_count: 1,
                truncated_blocks: 0,
            }
        );
        assert!(!out.blocks[0].degraded, "已经关掉的 block 不受影响");
        assert!(out.blocks[1].degraded, "仍打开的 block 标 degraded");
        assert_eq!(out.stats.gap_count, 1);
        assert_eq!(out.stats.dropped_bytes, 512);
    }

    /// R11：`Outcome` 与 `Integrity` **正交**。
    ///
    /// 一条流可以正常结束却丢过字节，也可以被客户端掐断但观测完整。
    /// 混为一谈会让下游分析出错。
    #[test]
    fn accumulator_outcome_and_integrity_are_independent_dimensions() {
        // Completed + Degraded
        let mut a = acc();
        a.apply(&ev(0, ObservedEventKind::Gap { dropped_bytes: 8 }));
        let out = a.finish(Outcome::Completed);
        assert_eq!(out.outcome, Outcome::Completed);
        assert!(out.integrity.is_degraded());

        // ClientAborted + Complete
        let mut a = acc();
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        a.apply(&text_delta(1, C0, "hi"));
        let out = a.finish(Outcome::ClientAborted {
            at_seq: 1,
            bytes_forwarded: 2,
        });
        assert_eq!(out.integrity, Integrity::Complete);
        assert_eq!(out.outcome.as_str(), "client_aborted");
    }

    /// 截断也算降级 —— 少了内容和丢了字节对下游是同一类问题。
    #[test]
    fn accumulator_truncation_alone_is_enough_to_degrade() {
        let mut a = acc_with(AccumulatorConfig {
            max_accumulated_bytes: 2,
            ..AccumulatorConfig::default()
        });
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        a.apply(&text_delta(1, C0, "far too long"));

        let out = a.finish(Outcome::Completed);
        assert_eq!(
            out.integrity,
            Integrity::Degraded {
                dropped_bytes: 0,
                gap_count: 0,
                truncated_blocks: 1,
            }
        );
    }

    /// `ObserveMode::Lite` / `Off` 下只有元数据 —— 不是「降级」，是另一回事。
    #[test]
    fn accumulator_non_full_observe_mode_yields_metadata_only() {
        for mode in [ObserveMode::Lite, ObserveMode::Off] {
            let a = Accumulator::new(
                RequestMeta {
                    trace: TraceContext::root(),
                    observer: ObserverKind::GenericJson,
                    observe_mode: mode,
                    stream: false,
                    observed_model: None,
                    request_sha256: None,
                    raw_request: None,
                },
                AccumulatorConfig::default(),
            );
            assert_eq!(
                a.finish(Outcome::Completed).integrity,
                Integrity::MetadataOnly,
                "mode={}",
                mode.as_str()
            );
        }
    }

    /// R12：任意时刻 `finish` 都要产出语义正确的 partial 结果。
    #[test]
    fn accumulator_finish_midstream_produces_a_valid_partial_interaction() {
        let mut a = acc();
        a.apply(&ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)));
        a.apply(&text_delta(1, C0, "half a sen"));

        let out = a.finish(Outcome::Cancelled {
            reason: CancelReason::Shutdown,
        });
        assert_eq!(out.blocks[0].text, "half a sen");
        assert_eq!(out.blocks[0].closed_at, None, "block 还没关");
        assert_eq!(out.finish, None, "没收到 finish 事件");
        assert_eq!(out.integrity, Integrity::Complete, "没丢字节就是完整的");
        assert_eq!(out.usage.source, super::UsageSource::Missing);
    }

    /// 一个事件都没有也要能收尾。
    #[test]
    fn accumulator_finish_without_any_event_still_produces_an_interaction() {
        let out = acc().finish(Outcome::UpstreamError {
            kind: "connect".into(),
        });
        assert!(out.blocks.is_empty());
        assert_eq!(out.stats.events_emitted, 0);
        assert_eq!(out.integrity, Integrity::Complete);
    }
}

// ─────────────────────────────────────────────────────────────────────────
mod accumulator_fidelity {
    use super::{
        AccumulatorConfig, Bytes, C0, ObservedEvent, ObservedEventKind, Outcome, RawMode, RawRef,
        UsageDelta, acc_with, ev, text_delta,
    };
    use argus_protocol::ir::{BlockKind, Finish, FinishReason};

    fn with_raw(mut e: ObservedEvent, seq: u32) -> ObservedEvent {
        e.raw = Some(RawRef {
            frame: Bytes::from_static(b"data: {}\n\n"),
            frame_seq: seq,
        });
        e
    }

    /// 宪法 C2：不认识的事件不丢，进 `unknown_events` 并带上 raw。
    #[test]
    fn accumulator_unknown_events_are_recorded_with_their_raw_frame() {
        let mut a = acc_with(AccumulatorConfig::default());
        a.apply(&with_raw(
            ev(
                0,
                ObservedEventKind::Unknown {
                    event_name: "message_reflow".into(),
                },
            ),
            0,
        ));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.unknown_events.len(), 1);
        assert_eq!(out.unknown_events[0].event_name, "message_reflow");
        assert!(out.unknown_events[0].raw.is_some());
    }

    #[test]
    fn accumulator_provider_errors_are_recorded_separately_from_anomalies() {
        let mut a = acc_with(AccumulatorConfig::default());
        a.apply(&ev(
            0,
            ObservedEventKind::ProviderError {
                kind: "overloaded_error".into(),
                message: "upstream is busy".into(),
            },
        ));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.provider_errors.len(), 1);
        assert_eq!(out.provider_errors[0].kind, "overloaded_error");
        assert!(
            out.stats.anomalies.is_empty(),
            "provider 报错不是我们的异常"
        );
    }

    /// `selective` 的取舍：头 N 帧 + 尾 N 帧 + **全部结构性事件**。
    ///
    /// 真正需要 raw 的场景是"解析出问题了"，而那些场景的信号正是
    /// unknown / error / 结构性事件（`docs/11` §8.1）。
    #[test]
    fn accumulator_selective_raw_mode_keeps_head_tail_and_structural_frames() {
        let mut a = acc_with(AccumulatorConfig {
            raw_mode: RawMode::Selective { head: 2, tail: 2 },
            ..AccumulatorConfig::default()
        });
        a.apply(&with_raw(
            ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)),
            0,
        ));
        for seq in 1..10_u32 {
            a.apply(&with_raw(text_delta(seq, C0, "x"), seq));
        }
        // 结构性事件夹在中间，必须被留下
        a.apply(&with_raw(
            ev(10, ObservedEventKind::usage(UsageDelta::default())),
            10,
        ));
        for seq in 11..14_u32 {
            a.apply(&with_raw(text_delta(seq, C0, "y"), seq));
        }
        a.apply(&with_raw(
            ev(
                14,
                ObservedEventKind::finish(Finish::new(FinishReason::Stop)),
            ),
            14,
        ));

        let log = a.finish(Outcome::Completed).raw_events.expect("有 raw log");
        let kept: Vec<u32> = log.frames.iter().map(|f| f.frame_seq).collect();
        assert!(kept.contains(&0) && kept.contains(&1), "头两帧");
        assert!(kept.contains(&10), "中间的 usage 事件");
        assert!(kept.contains(&14), "结尾的 finish 事件");
        assert!(kept.contains(&12) && kept.contains(&13), "尾两帧");
        assert!(!kept.contains(&5), "中间的普通 delta 被跳过");
        assert!(log.skipped > 0);
        // 合并 head 与 tail 之后必须重新按帧序排好
        let mut sorted = kept.clone();
        sorted.sort_unstable();
        assert_eq!(kept, sorted, "raw_events 必须按帧序");
    }

    #[test]
    fn accumulator_raw_mode_none_keeps_nothing_but_counts_what_it_dropped() {
        let mut a = acc_with(AccumulatorConfig {
            raw_mode: RawMode::None,
            ..AccumulatorConfig::default()
        });
        a.apply(&with_raw(text_delta(0, C0, "x"), 0));
        let out = a.finish(Outcome::Completed);
        assert!(out.raw_events.is_none());
    }

    /// 同一帧产出多个事件时，`frames_seen` 只能算一次。
    #[test]
    fn accumulator_frames_seen_counts_frames_not_events() {
        let mut a = acc_with(AccumulatorConfig::default());
        a.apply(&with_raw(
            ev(0, ObservedEventKind::block_start(C0, BlockKind::Text)),
            0,
        ));
        a.apply(&with_raw(text_delta(1, C0, "a"), 0));
        a.apply(&with_raw(text_delta(2, C0, "b"), 1));

        let out = a.finish(Outcome::Completed);
        assert_eq!(out.stats.frames_seen, 2);
        assert_eq!(out.stats.events_emitted, 3);
    }
}
