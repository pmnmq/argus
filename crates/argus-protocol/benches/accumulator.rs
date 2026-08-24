//! 事件累积的耗时预算（`docs/21` §6.4 ①）。
//!
//! `apply` 跑在**每一个事件**上，而一条 2000 token 的流有 2000+ 个事件。
//! 它比解析更容易悄悄变慢：解析是纯函数，而累积在往里塞状态 —— 一次
//! 不小心的 `clone()`、一个线性查找的 block 表，都会让成本随流长增长
//! 而不是常数。所以这里刻意**按流长分档**跑：曲线是平的才对。
//!
//! 「稳态零分配」不在这里断言：那由 `tests/accumulator_alloc.rs` 用
//! 「容量没变 == 这次没分配」钉住，比数 malloc 更贴题，也不需要自定义
//! 全局分配器（宪法 C8 禁 unsafe）。

use argus_core::{ObserveMode, ObserverKind, TeeChunk, TraceContext};
use argus_protocol::{
    AnyObserver, ObserverSink,
    accumulator::{Accumulator, AccumulatorConfig, RequestMeta},
    framer::{Framer, FramerConfig, SseFramer},
    interaction::Outcome,
    ir::ObservedEvent,
};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::{fmt::Write as _, hint::black_box};

/// 合成语料（不是 fixture，理由同 `framer.rs`）。
fn corpus(deltas: usize) -> Bytes {
    let mut s = String::with_capacity(deltas * 128 + 256);
    s.push_str("event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_bench\",\"model\":\"claude-x\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n");
    s.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
    for i in 0..deltas {
        let _ = write!(
            s,
            "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"the quick brown fox {i} \"}}}}\n\n"
        );
    }
    s.push_str(
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    );
    s.push_str("event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":128}}\n\n");
    s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Bytes::from(s)
}

/// 先把语料解析成事件序列 —— bench 只想量 `apply`，不想量解析。
fn events_of(raw: &Bytes) -> Vec<ObservedEvent> {
    let mut observer = AnyObserver::for_kind(ObserverKind::AnthropicMessages, ObserveMode::Full);
    let mut framer = SseFramer::new(FramerConfig::default());
    let mut events = Vec::new();
    {
        let mut sink = ObserverSink::new(&mut observer, &mut events, 0);
        let _ = framer.push(&TeeChunk::new(raw.clone()), &mut sink);
    }
    events
}

fn meta() -> RequestMeta {
    RequestMeta {
        trace: TraceContext::root(),
        observer: ObserverKind::AnthropicMessages,
        observe_mode: ObserveMode::Full,
        stream: true,
        observed_model: None,
        request_sha256: None,
        raw_request: None,
    }
}

fn bench_accumulator(c: &mut Criterion) {
    let mut group = c.benchmark_group("accumulator_apply");

    for deltas in [16_usize, 128, 1_024] {
        let raw = corpus(deltas);
        let events = events_of(&raw);
        // 按事件数归一化：曲线平了才说明 apply 是常数成本
        group.throughput(criterion::Throughput::Elements(events.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(deltas), &events, |b, events| {
            b.iter(|| {
                let mut acc = Accumulator::new(meta(), AccumulatorConfig::default());
                for ev in events {
                    acc.apply(black_box(ev));
                }
                black_box(acc.finish(Outcome::Completed))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_accumulator);
criterion_main!(benches);
