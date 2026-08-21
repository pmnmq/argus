//! 一次交互的最终产物：`ObservedInteraction` 及其配套类型。
//!
//! `Outcome` 与 `Integrity` 是**两个正交维度**，这是本模块最要紧的一条：
//! 一条流可以是 `Completed` 但 `Degraded`（正常结束但中间丢了字节），
//! 也可以是 `ClientAborted` 但 `Complete`（中断前观测到的部分是完整的）。
//! 混为一谈会让下游分析出错（`docs/11-observation-model.md` §7.5）。
//!
//! 设计依据：`docs/11` §7.5、§8；落库形状见 `docs/15-storage.md`。

use argus_core::{CancelReason, ObserveMode, ObserverKind, TraceContext};
use bytes::Bytes;
use compact_str::CompactString;
use smallvec::SmallVec;

use crate::ir::{BlockCoord, BlockKind, Finish, RawRef};

/// 流是怎么结束的。
#[derive(Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Outcome {
    Completed,
    ClientAborted { at_seq: u32, bytes_forwarded: u64 },
    UpstreamError { kind: CompactString },
    Cancelled { reason: CancelReason },
    ObserverTimeout,
    ParserFailed { at_seq: u32 },
}

impl Outcome {
    /// 落库用的稳定标签（`docs/15` 的 `interactions.outcome`）。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::ClientAborted { .. } => "client_aborted",
            Self::UpstreamError { .. } => "upstream_error",
            Self::Cancelled { .. } => "cancelled",
            Self::ObserverTimeout => "observer_timeout",
            Self::ParserFailed { .. } => "parser_failed",
        }
    }
}

/// 观测到的东西有多完整。**与 [`Outcome`] 正交。**
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub enum Integrity {
    /// 观测完整
    #[default]
    Complete,
    /// 有字节丢失或截断，附带定量信息 —— 「降级了多少」比「降级了」有用得多
    Degraded {
        dropped_bytes: u64,
        gap_count: u32,
        truncated_blocks: u32,
    },
    /// 只有元数据，没有内容（`ObserveMode::Lite` / `Off`）
    MetadataOnly,
}

impl Integrity {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Degraded { .. } => "degraded",
            Self::MetadataOnly => "metadata_only",
        }
    }

    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }
}

/// 请求侧观测到的东西。请求体本身在 `raw_request`，这里只有元信息。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ObservedRequest {
    /// 从 body 里**观测到**的模型名。仅用于统计，不参与路由（宪法 C33）
    pub observed_model: Option<CompactString>,
    pub stream: bool,
    /// 去重与关联用
    pub request_sha256: Option<[u8; 32]>,
    /// 请求侧的内容块，如客户端回传的 `ToolResult`。本切片恒为空（→ F09）
    pub blocks: Vec<ObservedBlock>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ResponseMeta {
    pub provider_response_id: Option<CompactString>,
    /// provider **实际**使用的模型，可能与请求里写的不同
    pub model: Option<CompactString>,
    pub role: Option<CompactString>,
    pub status_code: Option<u16>,
}

/// 收尾之后的内容块。落库到 `request_payloads.blocks`。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObservedBlock {
    pub coord: BlockCoord,
    pub kind: BlockKind,
    /// Text / Reasoning / Refusal 的累积文本
    pub text: String,
    /// tool arguments 的原始 JSON。**从未在流中 parse**（`docs/11` §7.2）
    pub json_raw: String,
    pub signature: Option<String>,
    pub opened_at: u32,
    pub closed_at: Option<u32>,
    /// 撞到 `max_accumulated_bytes` 上限
    pub truncated: bool,
    /// 这个 block 经历过 `Gap`
    pub degraded: bool,
}

/// usage 的来源。对应 `docs/15` 的 `interactions.usage_source`。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum UsageSource {
    /// provider 报了至少一个 token 字段
    Reported,
    /// provider 一个都没报，`estimated_output_tokens` 是 Argus 估的
    #[default]
    Missing,
    /// provider 报了一部分，缺的由 Argus 估算补齐（→ F09）
    Estimated,
}

impl UsageSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Missing => "missing",
            Self::Estimated => "estimated",
        }
    }
}

/// 合并完的 usage。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ResolvedUsage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub source: UsageSource,
    /// **只在 provider 没给 `output_tokens` 时有值**，且绝不写进上面的
    /// 真实字段 —— 计费必须能区分实测与估算（宪法 C14、`docs/11` §6）
    pub estimated_output_tokens: Option<u32>,
    pub extra: Option<Box<Bytes>>,
}

/// 按 `docs/11` §8.1 的策略保存下来的原始帧。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RawEventLog {
    /// 按 `frame_seq` 有序
    pub frames: Vec<RawRef>,
    /// 因策略被跳过的帧数。有它才知道「只有 16 帧」是策略还是真的只有 16 帧
    pub skipped: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownRecord {
    pub event_name: CompactString,
    /// 产生它的事件 seq，便于和 `raw_events` 对齐
    pub seq: u32,
    pub raw: Option<Bytes>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProviderErrorRecord {
    pub kind: CompactString,
    pub message: CompactString,
    pub seq: u32,
}

/// 流健康度的量化记录。
///
/// 落库到 `request_payloads.stats`，是「观测数据可信度」与「provider 规范
/// 程度」的唯一依据 —— 没有它，一条空荡荡的 interaction 分不清是模型没说话
/// 还是我们没看住。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct StreamStats {
    pub frames_seen: u32,
    pub events_emitted: u32,
    pub bytes_seen: u64,
    pub dropped_bytes: u64,
    pub gap_count: u32,
    pub heartbeats: u32,
    /// 解析失败的事件数。一条流里 50 次解析失败正是 `StreamStats` 存在的
    /// 意义 —— 那是「provider 规范程度」最直接的信号
    pub parse_failures: u32,
    /// `docs/11` §7.3 的全部异常，每项含坐标与 seq
    pub anomalies: SmallVec<[Anomaly; 4]>,
}

/// 一条异常记录。
///
/// 它们**不是错误**：累积器永不失败，任何反常都是这里的一条记录
/// （`docs/11` §7.3）。本切片只产生其中几种，其余留给 F09。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Anomaly {
    ImplicitStart {
        coord: BlockCoord,
        at: u32,
    },
    DuplicateStart {
        coord: BlockCoord,
        at: u32,
    },
    OrphanStop {
        coord: BlockCoord,
        at: u32,
    },
    DeltaAfterStop {
        coord: BlockCoord,
        at: u32,
    },
    EventAfterFinish {
        at: u32,
    },
    DuplicateFinish {
        at: u32,
    },
    /// 仅 `OpenAI` Responses：与 `response.completed` 交叉校验不一致（→ F09）
    TextMismatch {
        coord: BlockCoord,
    },
    MissingBlock {
        coord: BlockCoord,
    },
}

impl Anomaly {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::ImplicitStart { .. } => "implicit_start",
            Self::DuplicateStart { .. } => "duplicate_start",
            Self::OrphanStop { .. } => "orphan_stop",
            Self::DeltaAfterStop { .. } => "delta_after_stop",
            Self::EventAfterFinish { .. } => "event_after_finish",
            Self::DuplicateFinish { .. } => "duplicate_finish",
            Self::TextMismatch { .. } => "text_mismatch",
            Self::MissingBlock { .. } => "missing_block",
        }
    }
}

/// 各阶段耗时，微秒。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Timings {
    /// 入口到发出上游请求
    pub queued_micros: u32,
    /// 发出请求到收到 header（含模型排队）
    pub upstream_header_micros: u32,
    /// 到第一个内容 delta
    pub ttft_micros: Option<u32>,
    pub total_micros: u32,
    /// Argus 自身的开销
    pub proxy_overhead_micros: u32,
}

/// 一次交互的完整观测结果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ObservedInteraction {
    pub trace: TraceContext,
    pub outcome: Outcome,
    pub integrity: Integrity,
    /// 这条流最终是用哪个 Observer 解的（R04）
    pub observer: ObserverKind,
    pub observe_mode: ObserveMode,

    // ── 请求侧 ──────────────────────────────────
    pub request: ObservedRequest,

    // ── 响应侧 ──────────────────────────────────
    pub response_meta: ResponseMeta,
    /// 按 `BlockCoord` 有序
    pub blocks: Vec<ObservedBlock>,
    pub usage: ResolvedUsage,
    pub finish: Option<Finish>,

    // ── 保真 ────────────────────────────────────
    pub raw_request: Option<Bytes>,
    pub raw_events: Option<RawEventLog>,
    pub provider_errors: Vec<ProviderErrorRecord>,
    pub unknown_events: Vec<UnknownRecord>,

    // ── 诊断 ────────────────────────────────────
    pub stats: StreamStats,
    pub timings: Timings,
}
