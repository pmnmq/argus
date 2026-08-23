//! 单帧解析的耗时预算（F05 §7：< 3 µs/帧）。
//!
//! 为什么值得有一条基准：解析发生在**每一个 token 上**。3 µs 与 30 µs 的
//! 差别，在一条 2000 token 的流上是 6 ms 与 60 ms —— 后者会开始跟旁路的
//! channel 抢 CPU，然后表现为 TTFT 抖动，而那时已经很难查回这里了。
//!
//! 「零分配」不在这里断言：那由 `TextChunk::is_shared()` 在单元测试里从
//! 类型上钉住（`Shared` 只可能来自 `Bytes::slice`），比数 malloc 更直接，
//! 也不需要自定义全局分配器（宪法 C8 禁 unsafe）。

use argus_core::{ObserveMode, ObserverKind, TeeChunk};
use argus_protocol::{
    AnyObserver, ObserverSink,
    framer::{Framer, FramerConfig, SseFramer},
    ir::ObservedEvent,
};
use bytes::Bytes;
use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

/// 最热的那一帧：一个没有转义序列的 text delta。
const TEXT_DELTA: &[u8] = br#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"the quick brown fox"}}

"#;

/// 含转义的 delta —— 必须解码，是慢的那一侧。
const ESCAPED_DELTA: &[u8] = br#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line\nbreak \u00e9"}}

"#;

/// 未知 event：两阶段解析里只走第一阶段。
const UNKNOWN: &[u8] = br#"event: message_pause
data: {"type":"message_pause","reason":"tool_wait"}

"#;

fn parse_one(input: &'static [u8], events: &mut Vec<ObservedEvent>) {
    let mut observer = AnyObserver::for_kind(ObserverKind::AnthropicMessages, ObserveMode::Full);
    let mut framer = SseFramer::new(FramerConfig::default());
    let mut sink = ObserverSink::new(&mut observer, events, 0);
    let _ = framer.push(&TeeChunk::new(Bytes::from_static(input)), &mut sink);
}

fn bench_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("anthropic_frame");
    for (name, input) in [
        ("text_delta", TEXT_DELTA),
        ("escaped_delta", ESCAPED_DELTA),
        ("unknown_event", UNKNOWN),
    ] {
        group.bench_function(name, |b| {
            let mut events = Vec::with_capacity(4);
            b.iter(|| {
                events.clear();
                parse_one(black_box(input), &mut events);
                black_box(events.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_frames);
criterion_main!(benches);
