//! `SseFramer` 的性质测试。
//!
//! 单元测试证明「我想到的情况是对的」，性质测试负责「我没想到的情况不会炸」。
//! 宪法 C7：解析层对任意字节输入都不得 panic —— 它跑在 observer task 里，
//! panic 会被 `catch_unwind` 接住，但那已经意味着这条流的观测数据没了。

mod common;

use argus_core::TeeChunk;
use argus_protocol::framer::{FrameRef, FrameSink, Framer, FramerConfig, FramerError, SseFramer};
use bytes::Bytes;
use common::SplitStrategy;
use proptest::prelude::*;

#[derive(Default)]
struct Counter {
    frames: usize,
    gaps: usize,
}

impl FrameSink for Counter {
    fn emit(&mut self, _frame: FrameRef<'_>) {
        self.frames += 1;
    }
    fn emit_gap(&mut self, _dropped_bytes: u64) {
        self.gaps += 1;
    }
}

const MAX: usize = 4096;

fn framer() -> SseFramer {
    SseFramer::new(FramerConfig {
        max_frame_bytes: MAX,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// 宪法 C7：任意字节都不能 panic。
    #[test]
    fn push_arbitrary_bytes_never_panics(input in prop::collection::vec(any::<u8>(), 0..8192)) {
        let mut f = framer();
        let mut out = Counter::default();
        let _ = f.push(&TeeChunk::new(Bytes::from(input)), &mut out);
        let _ = f.finish(&mut out);
    }

    /// 宪法 C9：缓冲必须有界。
    ///
    /// 断言的是「每次 push 返回之后」的状态：成功时残留不超过上限，
    /// 失败时已经把缓冲释放掉了。push 执行**过程中**的峰值由上游的
    /// tee 字节预算（256 KiB）约束，那是 F06 的事。
    #[test]
    fn buffered_bytes_after_any_push_never_exceeds_max_frame_bytes(
        pieces in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..512), 0..40)
    ) {
        let mut f = framer();
        let mut out = Counter::default();
        for p in pieces {
            match f.push(&TeeChunk::new(Bytes::from(p)), &mut out) {
                Ok(()) => prop_assert!(f.buffered_bytes() <= MAX),
                Err(_) => {
                    prop_assert_eq!(f.buffered_bytes(), 0, "失败后必须释放缓冲");
                    break;
                }
            }
        }
    }

    /// 宪法 C13：`gap_before > 0` 必然产生一次 `emit_gap`，一次都不能少。
    #[test]
    fn push_chunks_with_gaps_emits_exactly_one_gap_per_gapped_chunk(
        gaps in prop::collection::vec(0_u64..1000, 1..12)
    ) {
        let mut f = framer();
        let mut out = Counter::default();
        let expected = gaps.iter().filter(|g| **g > 0).count();
        for g in gaps {
            let chunk = TeeChunk { data: Bytes::from_static(b"data: x\n\n"), gap_before: g };
            if f.push(&chunk, &mut out).is_err() {
                return Ok(());
            }
        }
        prop_assert_eq!(out.gaps, expected);
    }

    /// 宪法 C24：任意随机切分与整块喂入产出相同的帧数。
    ///
    /// 这里只比帧数而不比内容 —— 内容比较由 tests/split.rs 在真实语料上做，
    /// 那里的语料有意义，断言也更强。这条的价值在于输入是随机字节。
    #[test]
    fn push_random_bytes_split_randomly_produces_the_same_frame_count(
        input in prop::collection::vec(
            prop_oneof![Just(b'\n'), Just(b'\r'), Just(b':'), Just(b' '), any::<u8>()],
            0..2048,
        ),
        seed in any::<u64>(),
    ) {
        let baseline = count(&input, &SplitStrategy::Whole);
        prop_assert_eq!(count(&input, &SplitStrategy::Seeded(seed)), baseline);
        prop_assert_eq!(count(&input, &SplitStrategy::ByteByByte), baseline);
    }
}

fn count(input: &[u8], split: &SplitStrategy) -> Result<usize, FramerError> {
    let mut f = framer();
    let mut out = Counter::default();
    for piece in split.apply(input) {
        f.push(&TeeChunk::new(Bytes::from(piece)), &mut out)?;
    }
    f.finish(&mut out)?;
    Ok(out.frames)
}

/// `docs/22` §6.1 点名的那条：10 万随机字节不 panic。
///
/// 用固定种子而不是随机的 —— 失败时能原样复现。
#[test]
fn push_one_hundred_thousand_random_bytes_never_panics() {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let input: Vec<u8> = (0..100_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // 让分隔符字节高频出现，否则随机字节几乎不会构成帧
            match state % 8 {
                0 | 1 => b'\n',
                2 => b'\r',
                3 => b':',
                _ => (state >> 32) as u8,
            }
        })
        .collect();

    let mut f = framer();
    let mut out = Counter::default();
    for piece in SplitStrategy::Seeded(7).apply(&input) {
        if f.push(&TeeChunk::new(Bytes::from(piece)), &mut out)
            .is_err()
        {
            // 超限是合法结局，只要不 panic
            return;
        }
        assert!(f.buffered_bytes() <= MAX);
    }
    let _ = f.finish(&mut out);
}
