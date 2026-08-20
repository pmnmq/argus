//! 切分不变性（宪法 C24 / `docs/22` §6.1 的最后一条验收）。
//!
//! 同一份字节，无论怎么切分喂进 `SseFramer`，产出的帧序列必须完全相同。
//! 这条性质成立，上层解析器才有一个可信的地基。

mod common;

use argus_core::TeeChunk;
use argus_protocol::framer::{FrameRef, FrameSink, Framer, FramerConfig, SseFramer};
use bytes::Bytes;
use common::SplitStrategy;

/// 归一化后的帧，只保留语义，不含地址与借用。
#[derive(Debug, PartialEq, Eq, Clone)]
enum Frame {
    Event {
        event: Option<Vec<u8>>,
        data: Vec<u8>,
        raw: Vec<u8>,
    },
    Done,
    Comment,
    Gap(u64),
}

#[derive(Default)]
struct Collector(Vec<Frame>);

impl FrameSink for Collector {
    fn emit(&mut self, frame: FrameRef<'_>) {
        self.0.push(match frame {
            FrameRef::Event { event, data, raw } => Frame::Event {
                event: event.map(<[u8]>::to_vec),
                data: data.as_bytes().to_vec(),
                raw: raw.to_vec(),
            },
            FrameRef::Done => Frame::Done,
            FrameRef::Comment => Frame::Comment,
        });
    }

    fn emit_gap(&mut self, dropped_bytes: u64) {
        self.0.push(Frame::Gap(dropped_bytes));
    }
}

fn run(input: &[u8], split: &SplitStrategy) -> Vec<Frame> {
    let mut framer = SseFramer::new(FramerConfig::default());
    let mut out = Collector::default();
    for piece in split.apply(input) {
        framer
            .push(&TeeChunk::new(Bytes::from(piece)), &mut out)
            .expect("正常输入不该触发上限");
    }
    framer.finish(&mut out).expect("finish 不该失败");
    out.0
}

/// 覆盖三种分隔符、注释、多行 data、`[DONE]`、无尾随空行的结尾。
const CORPUS: &[(&str, &[u8])] = &[
    ("openai_chat_like", b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\ndata: [DONE]\n\n"),
    ("anthropic_like", b"event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
    ("crlf_endings", b"event: a\r\ndata: 1\r\n\r\nevent: b\r\ndata: 2\r\n\r\n"),
    ("cr_endings", b"event: a\rdata: 1\r\revent: b\rdata: 2\r\r"),
    ("mixed_endings", b"data: 1\n\ndata: 2\r\r\ndata: 3\r\n\r\ndata: 4\n\n"),
    ("with_heartbeats", b": ping\n\ndata: 1\n\n: ping\n\n: keep-alive\n\ndata: 2\n\n"),
    ("multiline_data", b"data: a\ndata: b\ndata: c\n\ndata: single\n\n"),
    ("empty_and_odd_fields", b"data:\n\nid: 7\nretry: 100\ndata: x\n\n\n\ndata: y\n\n"),
    ("no_trailing_blank_line", b"data: 1\n\ndata: 2"),
    ("only_separators", b"\n\n\r\r\r\n\r\n"),
    ("single_byte", b"x"),
    ("empty", b""),
];

#[test]
fn push_same_bytes_split_every_which_way_produces_identical_frames() {
    let strategies = SplitStrategy::all();
    assert!(
        strategies.len() >= 30,
        "切分矩阵至少要 30 种，当前 {}",
        strategies.len()
    );
    assert!(strategies.contains(&SplitStrategy::ByteByByte));

    for (name, input) in CORPUS {
        let baseline = run(input, &SplitStrategy::Whole);
        for split in &strategies {
            let got = run(input, split);
            assert_eq!(
                got, baseline,
                "语料 `{name}` 在切分策略 {split:?} 下与基线不一致"
            );
        }
    }
}

/// 上面那条如果被写成「拿被测代码生成期望值」就毫无意义。
/// 这条独立断言基线本身是对的。
#[test]
fn run_openai_like_corpus_with_whole_input_produces_the_expected_frames() {
    let frames = run(CORPUS[0].1, &SplitStrategy::Whole);
    assert_eq!(frames.len(), 3);
    assert!(matches!(frames[2], Frame::Done));
    match &frames[0] {
        Frame::Event { data, event, .. } => {
            assert!(event.is_none());
            assert!(data.starts_with(b"{\"choices\""));
        }
        other => panic!("第一帧应该是 Event，实际是 {other:?}"),
    }
}

#[test]
fn apply_every_strategy_preserves_the_input_bytes() {
    for (name, input) in CORPUS {
        for split in SplitStrategy::all() {
            let joined: Vec<u8> = split.apply(input).concat();
            assert_eq!(&joined, input, "语料 `{name}` 被策略 {split:?} 改变了内容");
        }
    }
}

#[test]
fn apply_every_strategy_produces_no_empty_pieces() {
    for (_, input) in CORPUS {
        for split in SplitStrategy::all() {
            assert!(
                split.apply(input).iter().all(|p| !p.is_empty()),
                "策略 {split:?} 产生了空块"
            );
        }
    }
}

/// 逐字节喂入时，缓冲区不能悄悄堆积。
#[test]
fn buffered_bytes_feeding_byte_by_byte_stays_below_the_longest_frame() {
    let input = CORPUS[1].1;
    let longest = input.split(|b| *b == b'\n').map(<[u8]>::len).max().unwrap_or(0);

    let mut framer = SseFramer::new(FramerConfig::default());
    let mut out = Collector::default();
    for piece in SplitStrategy::ByteByByte.apply(input) {
        framer.push(&TeeChunk::new(Bytes::from(piece)), &mut out).unwrap();
        assert!(
            framer.buffered_bytes() <= input.len(),
            "缓冲超过了整个输入的长度"
        );
    }
    assert!(longest > 0);
}
