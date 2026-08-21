//! Accumulator：把事件序列折叠成一次 [`ObservedInteraction`]。
//!
//! **`apply` 返回 `()` 而不是 `Result`，这是刻意的。** 累积器永不失败 ——
//! 任何反常都是 `stats.anomalies` 里的一条记录（`docs/11-observation-model.md`
//! §7.3）。一个能失败的累积器意味着旁路可以把主链路拖下水，而宪法 C13 不许。
//!
//! 本切片只累积**文本与 usage**（F04 §2）。tool call / reasoning 的结构化
//! 累积、乱序容忍、交叉校验、非流式合成全部留给 F09 —— 但**类型定义一次
//! 到位**，因为改 IR 会波及 F05/F08/F09/F12。

pub mod text;
pub mod usage;

use std::collections::{BTreeMap, VecDeque};

use argus_core::{ObserveMode, ObserverKind, TraceContext};
use bytes::Bytes;
use compact_str::CompactString;
use smallvec::SmallVec;

use crate::{
    interaction::{
        Anomaly, Integrity, ObservedBlock, ObservedInteraction, ObservedRequest, Outcome,
        ProviderErrorRecord, RawEventLog, ResponseMeta, StreamStats, Timings, UnknownRecord,
    },
    ir::{
        BlockCoord, BlockDelta, BlockKind, Finish, ObservedEvent, ObservedEventKind, RawRef,
        TextChunk,
    },
};

pub use self::{text::TextBuf, usage::UsageAccumulator};

/// 建 Accumulator 时就已知的东西 —— 它们不来自流。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RequestMeta {
    pub trace: TraceContext,
    pub observer: ObserverKind,
    pub observe_mode: ObserveMode,
    pub stream: bool,
    /// 从 body 里**观测到**的模型名，仅用于统计（宪法 C33）
    pub observed_model: Option<CompactString>,
    pub request_sha256: Option<[u8; 32]>,
    pub raw_request: Option<Bytes>,
}

/// raw 事件的保存策略（`docs/11` §8.1）。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RawMode {
    Full,
    /// 默认。保留开头 `head` 帧 + 结尾 `tail` 帧 + 全部结构性事件
    Selective {
        head: u16,
        tail: u16,
    },
    None,
}

impl Default for RawMode {
    fn default() -> Self {
        Self::Selective { head: 8, tail: 8 }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AccumulatorConfig {
    /// 全部 block 共享的文本预算，默认 1 MiB
    pub max_accumulated_bytes: usize,
    pub raw_mode: RawMode,
}

impl Default for AccumulatorConfig {
    fn default() -> Self {
        Self {
            max_accumulated_bytes: 1024 * 1024,
            raw_mode: RawMode::default(),
        }
    }
}

/// 一个内容块在累积过程中的状态。
#[derive(Clone, PartialEq, Eq, Debug)]
struct BlockState {
    kind: BlockKind,
    text: TextBuf,
    /// tool arguments 的原始 JSON。**从不在流中 parse**（`docs/11` §7.2）
    json_raw: TextBuf,
    signature: Option<String>,
    opened_at: u32,
    closed_at: Option<u32>,
    /// 这个 block 经历过 `Gap`
    degraded: bool,
}

impl BlockState {
    fn new(kind: BlockKind, opened_at: u32) -> Self {
        Self {
            kind,
            text: TextBuf::new(),
            json_raw: TextBuf::new(),
            signature: None,
            opened_at,
            closed_at: None,
            degraded: false,
        }
    }

    fn truncated(&self) -> bool {
        self.text.truncated() || self.json_raw.truncated()
    }

    fn into_observed(self, coord: BlockCoord) -> ObservedBlock {
        ObservedBlock {
            coord,
            kind: self.kind,
            truncated: self.text.truncated() || self.json_raw.truncated(),
            text: self.text.into_string(),
            json_raw: self.json_raw.into_string(),
            signature: self.signature,
            opened_at: self.opened_at,
            closed_at: self.closed_at,
            degraded: self.degraded,
        }
    }
}

/// 事件序列 → 一次交互。
pub struct Accumulator {
    meta: RequestMeta,
    cfg: AccumulatorConfig,
    response: ResponseMeta,
    /// `BTreeMap` 保证最终按坐标有序输出，且 `BlockCoord` 是 `Copy` 的定长
    /// key，做 key 不产生任何分配（宪法 C21）
    blocks: BTreeMap<BlockCoord, BlockState>,
    usage: UsageAccumulator,
    finish: Option<Finish>,
    unknown_events: SmallVec<[UnknownRecord; 4]>,
    provider_errors: SmallVec<[ProviderErrorRecord; 2]>,
    stats: StreamStats,
    /// 全部 block 共享的剩余文本预算
    budget: usize,
    accumulated_bytes: usize,
    first_content_micros: Option<u32>,
    last_frame_seq: Option<u32>,
    raw_kept: Vec<RawRef>,
    raw_tail: VecDeque<RawRef>,
    raw_skipped: u32,
}

impl Accumulator {
    #[must_use]
    pub fn new(meta: RequestMeta, cfg: AccumulatorConfig) -> Self {
        Self {
            budget: cfg.max_accumulated_bytes,
            meta,
            cfg,
            response: ResponseMeta::default(),
            blocks: BTreeMap::new(),
            usage: UsageAccumulator::new(),
            finish: None,
            unknown_events: SmallVec::new(),
            provider_errors: SmallVec::new(),
            stats: StreamStats::default(),
            accumulated_bytes: 0,
            first_content_micros: None,
            last_frame_seq: None,
            raw_kept: Vec::new(),
            raw_tail: VecDeque::new(),
            raw_skipped: 0,
        }
    }

    /// 已经累积了多少字节。
    #[must_use]
    pub const fn accumulated_bytes(&self) -> usize {
        self.accumulated_bytes
    }

    /// 吃一个事件。**同步、不 panic、不返回 `Err`。**
    pub fn apply(&mut self, ev: &ObservedEvent) {
        self.stats.events_emitted = self.stats.events_emitted.saturating_add(1);
        self.note_frame(ev);
        self.retain_raw(ev);

        if self.finish.is_some() && !matches!(ev.kind, ObservedEventKind::Gap { .. }) {
            self.stats.anomalies.push(Anomaly::EventAfterFinish { at: ev.seq });
        }

        match &ev.kind {
            ObservedEventKind::ResponseStart(start) => {
                self.response.provider_response_id.clone_from(&start.provider_response_id);
                self.response.model.clone_from(&start.model);
                self.response.role.clone_from(&start.role);
            }
            ObservedEventKind::Finish(finish) => {
                if self.finish.is_some() {
                    self.stats.anomalies.push(Anomaly::DuplicateFinish { at: ev.seq });
                } else {
                    self.finish = Some((**finish).clone());
                }
            }
            ObservedEventKind::BlockStart { at, kind } => self.on_block_start(*at, kind, ev.seq),
            ObservedEventKind::BlockDelta { at, delta } => {
                if ev.kind.is_content() && self.first_content_micros.is_none() {
                    self.first_content_micros = Some(ev.at_micros);
                }
                self.on_block_delta(*at, delta, ev.seq);
            }
            ObservedEventKind::BlockStop { at } => self.on_block_stop(*at, ev.seq),
            ObservedEventKind::Usage(delta) => self.usage.apply(delta),
            ObservedEventKind::Unknown { event_name } => {
                self.unknown_events.push(UnknownRecord {
                    event_name: event_name.clone(),
                    seq: ev.seq,
                    raw: ev.raw.as_ref().map(|r| r.frame.clone()),
                });
            }
            ObservedEventKind::ProviderError { kind, message } => {
                self.provider_errors.push(ProviderErrorRecord {
                    kind: kind.clone(),
                    message: message.clone(),
                    seq: ev.seq,
                });
            }
            ObservedEventKind::Heartbeat => {
                self.stats.heartbeats = self.stats.heartbeats.saturating_add(1);
            }
            ObservedEventKind::Gap { dropped_bytes } => self.on_gap(*dropped_bytes),
            ObservedEventKind::ParseFailure { .. } => {
                self.stats.parse_failures = self.stats.parse_failures.saturating_add(1);
            } // 这里**刻意不写 `_ => {}`**。`ObservedEventKind` 是
              // `#[non_exhaustive]`，但那只对下游 crate 生效 —— 在本 crate 内
              // 新增变体会让这个 match 编译失败，正好逼人想一想新事件该怎么累积。
              // 加个通配符就把这个提醒关掉了
        }
    }

    fn on_block_start(&mut self, at: BlockCoord, kind: &BlockKind, seq: u32) {
        if self.blocks.contains_key(&at) {
            self.stats.anomalies.push(Anomaly::DuplicateStart { coord: at, at: seq });
            return;
        }
        self.blocks.insert(at, BlockState::new(kind.clone(), seq));
    }

    fn on_block_delta(&mut self, at: BlockCoord, delta: &BlockDelta, seq: u32) {
        // 没见过 start 就先补一个：丢掉 delta 比记一条异常糟得多
        if !self.blocks.contains_key(&at) {
            self.stats.anomalies.push(Anomaly::ImplicitStart { coord: at, at: seq });
            self.blocks.insert(at, BlockState::new(BlockKind::Text, seq));
        }
        if self.blocks.get(&at).is_some_and(|b| b.closed_at.is_some()) {
            self.stats.anomalies.push(Anomaly::DeltaAfterStop { coord: at, at: seq });
        }

        let budget = &mut self.budget;
        let mut written = 0;
        if let Some(block) = self.blocks.get_mut(&at) {
            written = match delta {
                BlockDelta::Text(c) | BlockDelta::Reasoning(c) => {
                    push_chunk(&mut block.text, c, budget)
                }
                BlockDelta::Json(c) => push_chunk(&mut block.json_raw, c, budget),
                BlockDelta::Signature(c) => {
                    // 签名必须原样保留，不进共享预算 —— 它很短，而且被截断
                    // 之后就完全没用了
                    if let Some(s) = c.as_str() {
                        block.signature.get_or_insert_with(String::new).push_str(s);
                    }
                    0
                }
                // Citation / Opaque 在本切片不累积（→ F09），但它们的 raw
                // 已经通过 retain_raw 留下了
                BlockDelta::Citation(_) | BlockDelta::Opaque { .. } => 0,
            };
        }
        self.accumulated_bytes = self.accumulated_bytes.saturating_add(written);
    }

    fn on_block_stop(&mut self, at: BlockCoord, seq: u32) {
        if let Some(block) = self.blocks.get_mut(&at) {
            block.closed_at = Some(seq);
        } else {
            self.stats.anomalies.push(Anomaly::OrphanStop { coord: at, at: seq });
        }
    }

    /// R10：丢字节 → 所有仍打开的 block 标 degraded，整体 `Integrity` 降级。
    fn on_gap(&mut self, dropped_bytes: u64) {
        self.stats.gap_count = self.stats.gap_count.saturating_add(1);
        self.stats.dropped_bytes = self.stats.dropped_bytes.saturating_add(dropped_bytes);
        for block in self.blocks.values_mut() {
            if block.closed_at.is_none() {
                block.degraded = true;
            }
        }
    }

    fn note_frame(&mut self, ev: &ObservedEvent) {
        let Some(raw) = &ev.raw else { return };
        // 同一帧可能产出多个事件，只在帧号变化时计一次
        if self.last_frame_seq != Some(raw.frame_seq) {
            self.last_frame_seq = Some(raw.frame_seq);
            self.stats.frames_seen = self.stats.frames_seen.saturating_add(1);
            self.stats.bytes_seen = self.stats.bytes_seen.saturating_add(raw.frame.len() as u64);
        }
    }

    fn retain_raw(&mut self, ev: &ObservedEvent) {
        let Some(raw) = &ev.raw else { return };
        match self.cfg.raw_mode {
            RawMode::None => self.raw_skipped = self.raw_skipped.saturating_add(1),
            RawMode::Full => self.raw_kept.push(raw.clone()),
            RawMode::Selective { head, tail } => {
                if self.raw_kept.len() < usize::from(head) || is_structural(&ev.kind) {
                    self.raw_kept.push(raw.clone());
                } else {
                    if self.raw_tail.len() >= usize::from(tail) {
                        self.raw_tail.pop_front();
                        self.raw_skipped = self.raw_skipped.saturating_add(1);
                    }
                    self.raw_tail.push_back(raw.clone());
                }
            }
        }
    }

    /// 收尾。**任意时刻都能调**，产出的 partial 结果语义正确（R12）。
    #[must_use]
    pub fn finish(mut self, outcome: Outcome) -> Box<ObservedInteraction> {
        let truncated_blocks =
            u32::try_from(self.blocks.values().filter(|b| b.truncated()).count())
                .unwrap_or(u32::MAX);

        let integrity = resolve_integrity(self.meta.observe_mode, &self.stats, truncated_blocks);

        let blocks: Vec<ObservedBlock> = self
            .blocks
            .into_iter()
            .map(|(coord, state)| state.into_observed(coord))
            .collect();

        let raw_events = self.cfg.raw_mode.build_log(
            core::mem::take(&mut self.raw_kept),
            core::mem::take(&mut self.raw_tail),
            self.raw_skipped,
        );

        Box::new(ObservedInteraction {
            trace: self.meta.trace,
            outcome,
            integrity,
            observer: self.meta.observer,
            observe_mode: self.meta.observe_mode,
            request: ObservedRequest {
                observed_model: self.meta.observed_model,
                stream: self.meta.stream,
                request_sha256: self.meta.request_sha256,
                blocks: Vec::new(),
            },
            response_meta: self.response,
            blocks,
            usage: self.usage.resolve(self.accumulated_bytes),
            finish: self.finish,
            raw_request: self.meta.raw_request,
            raw_events,
            provider_errors: self.provider_errors.into_vec(),
            unknown_events: self.unknown_events.into_vec(),
            stats: self.stats,
            timings: Timings {
                ttft_micros: self.first_content_micros,
                ..Timings::default()
            },
        })
    }
}

impl RawMode {
    fn build_log(
        self,
        mut kept: Vec<RawRef>,
        tail: VecDeque<RawRef>,
        skipped: u32,
    ) -> Option<RawEventLog> {
        if self == Self::None {
            return None;
        }
        kept.extend(tail);
        // head 与 tail 分开收集，合起来必须重新按帧序排 —— 否则落库的
        // raw_events 顺序与流实际发生的顺序对不上
        kept.sort_unstable_by_key(|r| r.frame_seq);
        kept.dedup_by_key(|r| r.frame_seq);
        Some(RawEventLog {
            frames: kept,
            skipped,
        })
    }
}

/// `selective` 模式下**永远保留**的事件：它们结构性、量小，而且正是
/// 「解析出问题了」时唯一有用的东西（`docs/11` §8.1）。
fn is_structural(kind: &ObservedEventKind) -> bool {
    matches!(
        kind,
        ObservedEventKind::ResponseStart(_)
            | ObservedEventKind::Finish(_)
            | ObservedEventKind::Usage(_)
            | ObservedEventKind::Unknown { .. }
            | ObservedEventKind::ProviderError { .. }
    )
}

fn push_chunk(buf: &mut TextBuf, chunk: &TextChunk, budget: &mut usize) -> usize {
    // 不是合法 UTF-8 的片段不进文本缓冲 —— 它的字节已经在 raw 里了，
    // 往 String 里塞乱码只会污染审计
    chunk.as_str().map_or(0, |s| buf.push(s, budget))
}

/// `Integrity` 与 `Outcome` 正交：这里只看观测本身完不完整，
/// 完全不看流是怎么结束的。
fn resolve_integrity(mode: ObserveMode, stats: &StreamStats, truncated_blocks: u32) -> Integrity {
    if mode != ObserveMode::Full {
        return Integrity::MetadataOnly;
    }
    if stats.dropped_bytes > 0 || stats.gap_count > 0 || truncated_blocks > 0 {
        return Integrity::Degraded {
            dropped_bytes: stats.dropped_bytes,
            gap_count: stats.gap_count,
            truncated_blocks,
        };
    }
    Integrity::Complete
}
