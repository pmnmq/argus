//! 测试用的切分策略。F03 产出，F10 的 golden test 复用。
//!
//! 设计依据：`docs/20-testing-strategy.md` §5.4。
//!
//! **为什么这是整个测试体系里最有价值的一个技巧**：真实 TCP 分片是任意的。
//! 一个只在「完整帧」下测过的解析器，在生产里遇到
//! `data: {"ty` + `pe":"text_delta"...}` 就会崩 —— 而这类 bug 只在特定网络
//! 条件下出现，极难复现。`ByteByByte` 是最严格的检验：逐字节喂入的结果
//! 与整块喂入完全一致，状态机才算对。

#![allow(dead_code, reason = "F10 会用到这里的全部策略")]

/// 一种把字节流切成若干块的方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitStrategy {
    /// 整块一次喂入（基线）
    Whole,
    /// 每次 1 字节 —— 最严格的检验
    ByteByByte,
    /// 固定大小
    Fixed(usize),
    /// 恰好在每个帧分隔符之后切一刀
    AtBoundaries,
    /// 恰好在每个帧分隔符**中间**切一刀（最容易出错的地方）
    InsideBoundaries,
    /// 恰好把 `data:` 这个字段前缀切开
    SplitFieldPrefix,
    /// 用固定种子的伪随机切分（可复现）
    Seeded(u64),
}

impl SplitStrategy {
    /// 全矩阵。数量必须 ≥ 30（`docs/22` §6.1 的验收要求）。
    #[must_use]
    pub fn all() -> Vec<Self> {
        let mut v = vec![
            Self::Whole,
            Self::ByteByByte,
            Self::AtBoundaries,
            Self::InsideBoundaries,
            Self::SplitFieldPrefix,
        ];
        v.extend(
            [
                1_usize, 2, 3, 4, 5, 7, 11, 13, 16, 17, 32, 64, 127, 128, 512, 4096,
            ]
            .into_iter()
            .map(Self::Fixed),
        );
        v.extend((0..20).map(Self::Seeded));
        v
    }

    /// 基线策略：snapshot 以它的结果为准，其余策略与之比较。
    #[must_use]
    pub fn is_baseline(&self) -> bool {
        matches!(self, Self::Whole)
    }

    /// 把 `input` 切成若干块。
    ///
    /// 保证：拼接结果 == `input`，且不产生空块。
    #[must_use]
    pub fn apply(&self, input: &[u8]) -> Vec<Vec<u8>> {
        let cuts = self.cut_points(input);
        let mut out = Vec::new();
        let mut prev = 0_usize;
        for c in cuts {
            if c > prev && c <= input.len() {
                out.push(input[prev..c].to_vec());
                prev = c;
            }
        }
        if prev < input.len() {
            out.push(input[prev..].to_vec());
        }
        out
    }

    /// 切点集合（升序、去重、不含 0 与 len）。
    fn cut_points(&self, input: &[u8]) -> Vec<usize> {
        let n = input.len();
        let mut cuts: Vec<usize> = match self {
            Self::Whole => Vec::new(),
            Self::ByteByByte => (1..n).collect(),
            Self::Fixed(step) => {
                let step = (*step).max(1);
                (step..n).step_by(step).collect()
            }
            Self::AtBoundaries => separator_spans(input).map(|(_, end)| end).collect(),
            Self::InsideBoundaries => separator_spans(input)
                .filter(|(s, e)| e - s > 1)
                .map(|(s, _)| s + 1)
                .collect(),
            Self::SplitFieldPrefix => find_all(input, b"data:")
                .into_iter()
                .flat_map(|i| [i + 1, i + 4])
                .collect(),
            // xorshift64*：固定种子，跨平台可复现。不用 rand，测试不该依赖它的版本。
            Self::Seeded(seed) => {
                let mut state = seed.wrapping_mul(2_685_821_657_736_338_717).max(1);
                let mut cuts = Vec::new();
                let mut pos = 0_usize;
                while pos < n {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    // 1..=64 字节的步长
                    let step = (state % 64) as usize + 1;
                    pos += step;
                    if pos < n {
                        cuts.push(pos);
                    }
                }
                cuts
            }
        };
        cuts.retain(|c| *c > 0 && *c < n);
        cuts.sort_unstable();
        cuts.dedup();
        cuts
    }
}

/// 找出所有帧分隔符的 `(起点, 终点)`。
///
/// 这里刻意用朴素的三模式比较而不是复用 `find_frame_end` ——
/// 测试工具复用被测代码，就测不出被测代码的错。
fn separator_spans(input: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut i = 0_usize;
    std::iter::from_fn(move || {
        while i < input.len() {
            let at = i;
            i += 1;
            if input[at..].starts_with(b"\r\n\r\n") {
                i = at + 4;
                return Some((at, at + 4));
            }
            if input[at..].starts_with(b"\n\n") || input[at..].starts_with(b"\r\r") {
                i = at + 2;
                return Some((at, at + 2));
            }
        }
        None
    })
}

fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0_usize;
    while i + needle.len() <= haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}
