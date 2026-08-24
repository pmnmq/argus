//! 分帧的耗时预算（`docs/21` §6.4 ①：确认单次操作在 µs 级）。
//!
//! 为什么按**切分方式**分组：分帧器的成本几乎全在「跨块的那一帧怎么拼」。
//! 整块喂入时它退化成一次 memchr 扫描，而逐字节喂入时每个字节都要过一遍
//! 状态机。两者差多少，决定了 TCP 分片变碎（小 MTU、TLS 记录边界、
//! provider 换了写法）时会不会突然多出一大块 CPU —— 而那种退化在只测
//! `Whole` 的基准里完全看不见。
//!
//! 「零分配」不在这里断言：那由 `TextChunk::is_shared()` 在单元测试里从
//! 类型上钉住，比数 malloc 更直接（宪法 C8 禁 unsafe，写不了计数分配器）。

use argus_core::{ObserveMode, ObserverKind, TeeChunk};
use argus_protocol::{
    AnyObserver, ObserverSink,
    framer::{Framer, FramerConfig, SseFramer},
    ir::ObservedEvent,
};
use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::{fmt::Write as _, hint::black_box};

/// 一段合成的 Anthropic 流。**不是 fixture**（宪法 C25：fixture 只能来自
/// 录制，录制工具是 F10 R6 的交付物）。
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
    s.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Bytes::from(s)
}

/// 把整段语料按 `chunk` 字节切开喂进去，返回产出的事件数（防优化掉）。
fn feed(raw: &Bytes, chunk: usize, events: &mut Vec<ObservedEvent>) -> usize {
    let mut observer = AnyObserver::for_kind(ObserverKind::AnthropicMessages, ObserveMode::Full);
    let mut framer = SseFramer::new(FramerConfig::default());
    let mut offset = 0;
    while offset < raw.len() {
        let end = (offset + chunk).min(raw.len());
        let piece = raw.slice(offset..end);
        {
            let mut sink = ObserverSink::new(&mut observer, events, 0);
            let _ = framer.push(&TeeChunk::new(piece), &mut sink);
        }
        offset = end;
    }
    events.len()
}

fn bench_framer(c: &mut Criterion) {
    let raw = corpus(64);
    let mut group = c.benchmark_group("framer_push");
    group.throughput(Throughput::Bytes(raw.len() as u64));

    // 1 字节是最坏情况，usize::MAX 等价于整块一次喂入。中间几档覆盖
    // 真实分片的常见量级（TLS 记录 ~16KB、以太网 MSS ~1.4KB）
    for chunk in [1_usize, 64, 1_400, 16_384, usize::MAX] {
        let label = if chunk == usize::MAX {
            "whole".to_owned()
        } else {
            chunk.to_string()
        };
        group.bench_with_input(BenchmarkId::from_parameter(label), &chunk, |b, &chunk| {
            let mut events = Vec::with_capacity(256);
            b.iter(|| {
                events.clear();
                black_box(feed(black_box(&raw), chunk, &mut events))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_framer);
criterion_main!(benches);
