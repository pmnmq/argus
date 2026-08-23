//! Anthropic 解析器的切分不变性（宪法 C24）。
//!
//! `tests/split.rs` 证明的是**分帧**层切分不变；这一条往上再走一层：
//! 同一份字节，无论怎么切，产出的**事件序列**必须完全相同。
//! 逐字节喂入是最严格的检验 —— `data: {"ty` + `pe":"text_delta"...}`
//! 这种分片在生产里天天发生，只在完整帧下测过的解析器会在那里崩。
//!
//! ⚠️ 下面的字节是**合成**的，不是 fixture。宪法 C25 要求 fixture 来自录制，
//! 录制工具是 F10 的活（R6）—— 在它到位之前 `tests/fixtures/` 保持为空，
//! 比混进手写字节好：混进去之后就再也分不清哪些是真的了。

mod common;

use argus_core::{ObserveMode, ObserverKind, TeeChunk};
use argus_protocol::{
    AnyObserver, ObserverSink, ProtocolObserver,
    framer::{Framer, FramerConfig, SseFramer},
    ir::ObservedEvent,
};
use bytes::Bytes;
use common::SplitStrategy;

fn run(input: &[u8], split: &SplitStrategy) -> Vec<ObservedEvent> {
    let mut observer = AnyObserver::for_kind(ObserverKind::AnthropicMessages, ObserveMode::Full);
    let mut framer = SseFramer::new(FramerConfig::default());
    let mut events: Vec<ObservedEvent> = Vec::new();
    {
        let mut sink = ObserverSink::new(&mut observer, &mut events, 0);
        for piece in split.apply(input) {
            framer
                .push(&TeeChunk::new(Bytes::from(piece)), &mut sink)
                .expect("语料不该触发上限");
        }
        framer.finish(&mut sink).expect("finish 不该失败");
    }
    observer.finish(0, &mut events);
    events
}

fn labels(events: &[ObservedEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.kind.as_str()).collect()
}

/// 每条语料对应 F05 §7 首批 fixture 里的一行。
const CORPUS: &[(&str, &str)] = &[
    (
        "simple_stream",
        concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_syn_1","type":"message","role":"assistant","model":"claude-x","content":[],"usage":{"input_tokens":25,"output_tokens":1}}}"#,
            "\n\nevent: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}"#,
            "\n\nevent: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\nevent: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
            "\n\nevent: message_stop\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        ),
    ),
    (
        "ping_interleaved",
        concat!(
            "event: ping\n",
            r#"data: {"type":"ping"}"#,
            "\n\nevent: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            "\n\nevent: ping\n",
            r#"data: {"type":"ping"}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            "\n\n: keep-alive\n\nevent: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
        ),
    ),
    (
        "usage_split_with_cache_tokens",
        concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_syn_2","usage":{"input_tokens":25,"cache_creation_input_tokens":3,"cache_read_input_tokens":7,"output_tokens":1}}}"#,
            "\n\nevent: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1024}}"#,
            "\n\n",
        ),
    ),
    (
        "unknown_event",
        concat!(
            "event: message_pause\n",
            r#"data: {"type":"message_pause","reason":"tool_wait"}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            "\n\n",
        ),
    ),
    (
        "unknown_stop_reason",
        concat!(
            "event: message_delta\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"},"usage":{"output_tokens":2}}"#,
            "\n\n",
        ),
    ),
    (
        "error_mid_stream",
        concat!(
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            "\n\nevent: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"still here"}}"#,
            "\n\n",
        ),
    ),
    (
        "unknown_delta_type",
        concat!(
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            "\n\nevent: content_block_stop\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
        ),
    ),
    (
        "malformed_then_recovered",
        concat!(
            "event: content_block_delta\n",
            r#"data: {"type":"content_block_de"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"recovered"}}"#,
            "\n\n",
        ),
    ),
    (
        "escaped_text_and_crlf",
        concat!(
            "event: content_block_delta\r\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"line\nbreak é"}}"#,
            "\r\n\r\nevent: content_block_stop\r\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\r\n\r\n",
        ),
    ),
    (
        "truncated_mid_stream",
        concat!(
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
            "\n\nevent: content_block_delta\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cut"}}"#,
            "\n\n",
        ),
    ),
];

/// 宪法 C24：同一份字节，任意切分方式，事件序列完全相同。
#[test]
fn on_frame_the_same_bytes_split_every_which_way_produces_identical_events() {
    let strategies = SplitStrategy::all();
    assert!(strategies.len() >= 30, "切分矩阵至少 30 种");
    assert!(strategies.contains(&SplitStrategy::ByteByByte));

    for (name, input) in CORPUS {
        let baseline = run(input.as_bytes(), &SplitStrategy::Whole);
        assert!(!baseline.is_empty(), "语料 `{name}` 一个事件都没产出");
        for split in &strategies {
            assert_eq!(
                run(input.as_bytes(), split),
                baseline,
                "语料 `{name}` 在切分策略 {split:?} 下与基线不一致"
            );
        }
    }
}

/// 上面那条如果拿被测代码生成期望值就毫无意义 —— 这条独立钉住基线本身。
#[test]
fn on_frame_the_baseline_of_each_corpus_matches_the_documented_mapping() {
    let expected: &[(&str, &[&str])] = &[
        (
            "simple_stream",
            &[
                "response_start",
                "usage",
                "block_start",
                "block_delta",
                "block_delta",
                "block_stop",
                "usage",
                "finish",
            ],
        ),
        (
            "ping_interleaved",
            &[
                "heartbeat",
                "block_start",
                "heartbeat",
                "block_delta",
                "heartbeat",
                "block_stop",
            ],
        ),
        (
            "usage_split_with_cache_tokens",
            &["response_start", "usage", "usage", "finish"],
        ),
        // delta 没有对应的 block_start，`finish()` 也就没有块要关
        ("unknown_event", &["unknown", "block_delta"]),
        ("unknown_stop_reason", &["usage", "finish"]),
        (
            "error_mid_stream",
            &["block_start", "provider_error", "block_delta", "block_stop"],
        ),
        (
            "unknown_delta_type",
            &["block_start", "block_delta", "block_stop"],
        ),
        (
            "malformed_then_recovered",
            &["parse_failure", "block_delta"],
        ),
        ("escaped_text_and_crlf", &["block_delta", "block_stop"]),
        // 流被截断：`finish()` 补上缺的 block_stop
        (
            "truncated_mid_stream",
            &["block_start", "block_delta", "block_stop"],
        ),
    ];
    assert_eq!(expected.len(), CORPUS.len(), "语料与期望没有一一对应");

    for ((name, input), (want_name, want)) in CORPUS.iter().zip(expected) {
        assert_eq!(name, want_name, "语料顺序与期望表对不上");
        let events = run(input.as_bytes(), &SplitStrategy::Whole);
        assert_eq!(labels(&events), *want, "语料 `{name}` 的事件序列变了");
    }
}

/// 逐字节喂入下 `seq` 仍然从 0 开始、连续、不跳号。
#[test]
fn on_frame_byte_by_byte_input_keeps_the_sequence_numbers_contiguous() {
    for (name, input) in CORPUS {
        let events = run(input.as_bytes(), &SplitStrategy::ByteByByte);
        let seqs: Vec<u32> = events.iter().map(|e| e.seq).collect();
        let want: Vec<u32> =
            (0..u32::try_from(events.len()).expect("事件不会有 40 亿个")).collect();
        assert_eq!(seqs, want, "语料 `{name}` 的 seq 断了");
    }
}

/// 无论怎么切，`Unknown` 事件的 raw 都必须完整保留（`docs/11` §8.1）。
#[test]
fn on_frame_unknown_events_keep_their_raw_frame_under_every_split() {
    let input = CORPUS.iter().find(|(n, _)| *n == "unknown_event").expect("语料在").1;
    for split in SplitStrategy::all() {
        let events = run(input.as_bytes(), &split);
        let raw = events.first().expect("有事件").raw.as_ref().expect("Full 模式必须留 raw");
        assert!(
            raw.frame.windows(13).any(|w| w == b"message_pause"),
            "切分策略 {split:?} 下 raw 丢了"
        );
    }
}
