//! 观测器：把字节变成「这条流发生了什么」。
//!
//! 分帧（[`crate::framer`]）之后、累积（[`crate::accumulator`]）之前的那一层：
//! 输入是一个逻辑帧，输出是零个或多个 [`ObservedEvent`]。
//!
//! 三条贯穿本模块的性质：
//!
//! 1. **同步、不返回 `Err`**。解析不出来是这条流的一个观测结论，不是 Argus
//!    的故障 —— 它以 [`ObservedEventKind::ParseFailure`] 事件表达，主链路
//!    一个字节都不受影响（宪法 C2/C7/C13）。
//! 2. **不碰时钟**。`at_micros` 由调用方逐帧传进来，解析器因此是纯函数：
//!    同样的字节 + 同样的时间戳 = 同样的事件序列，可回放、可比对。
//! 3. **枚举分发**。[`AnyObserver`] 是一个 `enum` 而不是 `Box<dyn>`：热路径上
//!    每帧一次虚表跳转是能省掉的（宪法 C21）。
//!
//! 设计依据：`docs/09-observers.md` §4、`docs/11-observation-model.md` §5。

pub mod anthropic;
pub mod generic;

use argus_core::{ObserveMode, ObserverKind};
use bytes::Bytes;
use compact_str::CompactString;

use crate::{
    framer::{FrameRef, FrameSink},
    ir::{ObservedEvent, ObservedEventKind, RawRef},
};

pub use anthropic::AnthropicMessagesParser;
pub use generic::{GenericJsonObserver, GenericSseObserver, JsonShape};

/// 事件的接收端。
///
/// 调用方在这里做三件事之一：塞进 `Accumulator`、塞进 `Vec` 供测试比对、
/// 或者直接丢弃（`ObserveMode::Off` 根本不会走到这里）。
pub trait EventSink {
    fn emit(&mut self, ev: ObservedEvent);
}

impl EventSink for Vec<ObservedEvent> {
    fn emit(&mut self, ev: ObservedEvent) {
        self.push(ev);
    }
}

/// 把闭包当 sink 用。
///
/// 刻意**不**给 `F: FnMut(ObservedEvent)` 写覆盖式 impl —— 那会与上面
/// `Vec<ObservedEvent>` 的 impl 重叠，因为 Rust 无法证明 `Vec` 不是 `FnMut`。
pub struct FnSink<F>(pub F);

impl<F: FnMut(ObservedEvent)> EventSink for FnSink<F> {
    fn emit(&mut self, ev: ObservedEvent) {
        (self.0)(ev);
    }
}

/// 帧 → 事件。
///
/// 每个方法都收一个 `at_micros`：相对请求开始的微秒数，由调用方注入。
/// 解析器自己**不读时钟** —— 那会让它不可回放，也让「同一份 fixture 在
/// 任意切分下产出相同序列」（宪法 C24）这条性质无法断言。
pub trait ProtocolObserver {
    /// 吃一个逻辑帧。**同步、不 panic、不返回 `Err`。**
    fn on_frame(&mut self, frame: FrameRef<'_>, at_micros: u32, out: &mut dyn EventSink);

    /// 旁路丢了字节（`docs/10-streaming.md` §6）。
    ///
    /// 由解析器而不是调用方翻译成 [`ObservedEventKind::Gap`]，因为 `seq`
    /// 归解析器维护 —— 让调用方自己造事件会把序号弄断。
    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink);

    /// 流结束：关掉还开着的 block（若协议需要）。
    fn finish(&mut self, at_micros: u32, out: &mut dyn EventSink);

    fn kind(&self) -> ObserverKind;

    /// 供指标上报（F06 接线）。
    fn stats(&self) -> ParserStats;
}

/// 解析过程中的计数。全部 `saturating`，一条流再长也不会回绕。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct ParserStats {
    /// 收到的帧数（含注释与哨兵）
    pub frames: u32,
    /// 产出的事件数
    pub events: u32,
    /// 认不出的 event 名（宪法 C2：透传 + 计数，不报错）
    pub unknown_events: u32,
    /// 不在已知集合里的 `stop_reason`
    pub unknown_finish_reasons: u32,
    /// 非法 JSON 或形状不对
    pub parse_failures: u32,
    /// 协议上说不通但不影响解析的地方（如 `event:` 行与 `data.type` 打架）
    pub anomalies: u32,
}

/// 事件工厂：管 `seq`、`frame_seq` 与 raw 留存。
///
/// 三个观测器共用它，是为了让「seq 单调递增」「`ObserveMode::Lite` 不留
/// raw」这两条只有一份实现 —— 复制三遍就会有一遍写错。
#[derive(Clone, Debug)]
pub(crate) struct Emitter {
    seq: u32,
    frame_seq: u32,
    mode: ObserveMode,
    stats: ParserStats,
}

impl Emitter {
    pub(crate) const fn new(mode: ObserveMode) -> Self {
        Self {
            seq: 0,
            frame_seq: 0,
            mode,
            stats: ParserStats {
                frames: 0,
                events: 0,
                unknown_events: 0,
                unknown_finish_reasons: 0,
                parse_failures: 0,
                anomalies: 0,
            },
        }
    }

    pub(crate) const fn stats(&self) -> ParserStats {
        self.stats
    }

    /// 每收到一个帧调一次，无论那个帧最后有没有产出事件。
    pub(crate) const fn begin_frame(&mut self) {
        self.stats.frames = self.stats.frames.saturating_add(1);
        self.frame_seq = self.frame_seq.saturating_add(1);
    }

    /// 当前帧的序号（从 0 开始，`begin_frame` 之后取）。
    pub(crate) const fn frame_seq(&self) -> u32 {
        self.frame_seq.saturating_sub(1)
    }

    pub(crate) const fn note_anomaly(&mut self) {
        self.stats.anomalies = self.stats.anomalies.saturating_add(1);
    }

    pub(crate) const fn note_unknown_event(&mut self) {
        self.stats.unknown_events = self.stats.unknown_events.saturating_add(1);
    }

    pub(crate) const fn note_unknown_finish_reason(&mut self) {
        self.stats.unknown_finish_reasons = self.stats.unknown_finish_reasons.saturating_add(1);
    }

    /// 产出一个不带 raw 的事件（`Gap` / `Heartbeat` 这类不来自某一帧的）。
    pub(crate) fn emit(
        &mut self,
        out: &mut dyn EventSink,
        at_micros: u32,
        kind: ObservedEventKind,
    ) {
        self.push(out, ObservedEvent::new(self.seq, at_micros, kind));
    }

    /// 产出一个挂着原始帧的事件。`ObserveMode::Lite` 下 raw 被丢掉。
    pub(crate) fn emit_raw(
        &mut self,
        out: &mut dyn EventSink,
        at_micros: u32,
        kind: ObservedEventKind,
        raw: &Bytes,
    ) {
        let ev = ObservedEvent::new(self.seq, at_micros, kind);
        let ev = if self.mode.keeps_raw() {
            ev.with_raw(RawRef {
                frame: raw.clone(),
                frame_seq: self.frame_seq(),
            })
        } else {
            ev
        };
        self.push(out, ev);
    }

    pub(crate) fn emit_parse_failure(
        &mut self,
        out: &mut dyn EventSink,
        at_micros: u32,
        reason: crate::ir::ParseFailureKind,
        raw: &Bytes,
    ) {
        self.stats.parse_failures = self.stats.parse_failures.saturating_add(1);
        self.emit_raw(
            out,
            at_micros,
            ObservedEventKind::ParseFailure { reason },
            raw,
        );
    }

    fn push(&mut self, out: &mut dyn EventSink, ev: ObservedEvent) {
        self.seq = self.seq.saturating_add(1);
        self.stats.events = self.stats.events.saturating_add(1);
        out.emit(ev);
    }
}

/// 三个观测器的枚举分发。
///
/// 用 `enum` 而不是 `Box<dyn ProtocolObserver>`：分发发生在每一帧上，
/// 省掉虚表跳转是白捡的（宪法 C21）。代价是加一个协议要动这个文件 ——
/// 一年动两次的东西，不值得为它付出运行时代价。
pub enum AnyObserver {
    AnthropicMessages(AnthropicMessagesParser),
    GenericSse(GenericSseFallback),
    GenericJson(GenericJsonFallback),
}

impl AnyObserver {
    /// 按判定出来的 [`ObserverKind`] 造一个观测器。
    ///
    /// **还没实现的语义协议一律落到 Generic 兜底**，而不是 panic 也不是
    /// 返回 `None`：认不出协议不等于不观测（`docs/09` §4.1）。落下去之后
    /// [`Self::kind`] 返回的是**实际在观测的那个**，不是请求时判定的那个 ——
    /// 报告一个没在跑的观测器，比报告降级更误导人。
    #[must_use]
    pub fn for_kind(kind: ObserverKind, mode: ObserveMode) -> Self {
        match kind {
            ObserverKind::AnthropicMessages => {
                Self::AnthropicMessages(AnthropicMessagesParser::new(mode))
            }
            ObserverKind::GenericJson => Self::GenericJson(GenericJsonFallback::new(mode)),
            // OpenAiChat / OpenAiResponses 由 F08 填，GeminiGenerateContent 是
            // Phase 2 的预留位。在那之前它们都是 SSE，兜底能给出字节数、
            // 事件数与 raw 留存
            _ => Self::GenericSse(GenericSseFallback::new(mode)),
        }
    }
}

impl ProtocolObserver for AnyObserver {
    fn on_frame(&mut self, frame: FrameRef<'_>, at_micros: u32, out: &mut dyn EventSink) {
        match self {
            Self::AnthropicMessages(p) => p.on_frame(frame, at_micros, out),
            Self::GenericSse(p) => p.on_frame(frame, at_micros, out),
            Self::GenericJson(p) => p.on_frame(frame, at_micros, out),
        }
    }

    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink) {
        match self {
            Self::AnthropicMessages(p) => p.on_gap(dropped_bytes, at_micros, out),
            Self::GenericSse(p) => p.on_gap(dropped_bytes, at_micros, out),
            Self::GenericJson(p) => p.on_gap(dropped_bytes, at_micros, out),
        }
    }

    fn finish(&mut self, at_micros: u32, out: &mut dyn EventSink) {
        match self {
            Self::AnthropicMessages(p) => p.finish(at_micros, out),
            Self::GenericSse(p) => p.finish(at_micros, out),
            Self::GenericJson(p) => p.finish(at_micros, out),
        }
    }

    fn kind(&self) -> ObserverKind {
        match self {
            Self::AnthropicMessages(p) => p.kind(),
            Self::GenericSse(p) => p.kind(),
            Self::GenericJson(p) => p.kind(),
        }
    }

    fn stats(&self) -> ParserStats {
        match self {
            Self::AnthropicMessages(p) => p.stats(),
            Self::GenericSse(p) => p.stats(),
            Self::GenericJson(p) => p.stats(),
        }
    }
}

/// 把 [`GenericSseObserver`] 接进事件流。
///
/// 计数器本身（R04 已经落地）不产出事件，但**认不出协议不等于不观测**：
/// 每个 `data:` 帧变成一个带 raw 的 [`ObservedEventKind::Unknown`]，于是
/// `docs/09` §4.1 承诺的「字节数、事件数、raw 留存」在 IR 层就都有了，
/// 下游的 Accumulator 与存储不需要为兜底路径写第二套代码。
///
/// `event_name` 直接取 `event:` 行；没有那一行（`OpenAI` 系）就是空串 ——
/// 这里不猜，猜错的语义比没有语义糟。
pub struct GenericSseFallback {
    em: Emitter,
    inner: GenericSseObserver,
}

impl GenericSseFallback {
    #[must_use]
    pub fn new(mode: ObserveMode) -> Self {
        Self {
            em: Emitter::new(mode),
            inner: GenericSseObserver::new(),
        }
    }

    #[must_use]
    pub const fn inner(&self) -> &GenericSseObserver {
        &self.inner
    }
}

impl ProtocolObserver for GenericSseFallback {
    fn on_frame(&mut self, frame: FrameRef<'_>, at_micros: u32, out: &mut dyn EventSink) {
        self.em.begin_frame();
        // 先把要用的东西借出来，再把 frame 交给计数器 —— `FrameRef` 不是
        // `Clone`，而 `data` 里可能挂着一个 `Vec`（多行 data: 的情况）
        let carried = match &frame {
            FrameRef::Event { event, raw, .. } => Some((event_name(*event), (*raw).clone())),
            FrameRef::Done | FrameRef::Comment => None,
        };
        let was_comment = matches!(frame, FrameRef::Comment);
        self.inner.emit(frame);

        if let Some((event_name, raw)) = carried {
            self.em.note_unknown_event();
            self.em.emit_raw(
                out,
                at_micros,
                ObservedEventKind::Unknown { event_name },
                &raw,
            );
        } else if was_comment {
            self.em.emit(out, at_micros, ObservedEventKind::Heartbeat);
        }
    }

    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink) {
        self.inner.emit_gap(dropped_bytes);
        self.em.emit(out, at_micros, ObservedEventKind::Gap { dropped_bytes });
    }

    fn finish(&mut self, _at_micros: u32, _out: &mut dyn EventSink) {
        // 没有块的概念，没有要关的东西
    }

    fn kind(&self) -> ObserverKind {
        ObserverKind::GenericSse
    }

    fn stats(&self) -> ParserStats {
        self.em.stats()
    }
}

/// 把 [`GenericJsonObserver`] 接进事件流。
///
/// 它看的是**整个 body**，不是帧 —— `ObserverKind::GenericJson.frame_format()`
/// 是 `None`，所以按 R04 判定出它的那条流根本不会挂 Framer。
pub struct GenericJsonFallback {
    em: Emitter,
    inner: GenericJsonObserver,
}

impl GenericJsonFallback {
    #[must_use]
    pub fn new(mode: ObserveMode) -> Self {
        Self {
            em: Emitter::new(mode),
            inner: GenericJsonObserver::new(),
        }
    }

    #[must_use]
    pub const fn inner(&self) -> &GenericJsonObserver {
        &self.inner
    }

    /// 看一个完整的非流式 body。
    ///
    /// 不产出事件：这个观测器不解语义，硬造 `ResponseStart` 之类的东西
    /// 等于往审计里写猜测。结论从 [`Self::inner`] 上读（形状 + 顶层 key）。
    /// 非流式 JSON 展开成等价事件序列是 `SyntheticEventGen` 的活（F09）。
    pub fn observe_body(&mut self, body: &[u8]) {
        self.inner = GenericJsonObserver::observe(body);
    }
}

impl ProtocolObserver for GenericJsonFallback {
    fn on_frame(&mut self, _frame: FrameRef<'_>, _at_micros: u32, _out: &mut dyn EventSink) {
        // 走到这里说明调用方无视了 `frame_format() == None` 把 Framer 挂上了。
        // 不产出事件（造一个就是编数据），但记一条 anomaly —— 这样接线错误
        // 会以数据的形式浮出来，而不是静悄悄地什么都不发生
        self.em.begin_frame();
        self.em.note_anomaly();
    }

    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink) {
        self.em.emit(out, at_micros, ObservedEventKind::Gap { dropped_bytes });
    }

    fn finish(&mut self, _at_micros: u32, _out: &mut dyn EventSink) {}

    fn kind(&self) -> ObserverKind {
        ObserverKind::GenericJson
    }

    fn stats(&self) -> ParserStats {
        self.em.stats()
    }
}

/// 把 [`Framer`](crate::framer::Framer) 的输出直接接到一个观测器上。
///
/// `at_micros` 一整块 chunk 共用一个：时钟应该在字节从旁路取出来的那一刻读
/// 一次，而不是每帧读一次 —— 同一个 TCP 段里的帧本来就是同时到的，逐帧读
/// 反而是在给噪声编故事，还白付了每帧一次的 `clock_gettime`。
pub struct ObserverSink<'a, P: ProtocolObserver + ?Sized, S: EventSink> {
    pub observer: &'a mut P,
    pub out: &'a mut S,
    pub at_micros: u32,
}

impl<'a, P: ProtocolObserver + ?Sized, S: EventSink> ObserverSink<'a, P, S> {
    pub fn new(observer: &'a mut P, out: &'a mut S, at_micros: u32) -> Self {
        Self {
            observer,
            out,
            at_micros,
        }
    }
}

impl<P: ProtocolObserver + ?Sized, S: EventSink> FrameSink for ObserverSink<'_, P, S> {
    fn emit(&mut self, frame: FrameRef<'_>) {
        self.observer.on_frame(frame, self.at_micros, self.out);
    }

    fn emit_gap(&mut self, dropped_bytes: u64) {
        self.observer.on_gap(dropped_bytes, self.at_micros, self.out);
    }
}

fn event_name(event: Option<&[u8]>) -> CompactString {
    event
        .and_then(|b| core::str::from_utf8(b).ok())
        .map_or_else(CompactString::default, |s| CompactString::new(s.trim()))
}

/// 零拷贝地从 `raw` 里切出 `sub`。
///
/// `sub` 必须是 `raw` 内部的一段（`serde` 借用出来的字段、`FrameData` 的
/// `data` 都满足）。用地址区间判断而不是 `Bytes::slice_ref` —— 后者在不满足
/// 时会 panic，而本 crate 禁 panic（宪法 C7）。不满足就退化成一次拷贝：
/// 那只发生在多行 `data:` 被拼接过的帧上，实践中 AI provider 不这么发。
pub(crate) fn share(raw: &Bytes, sub: &[u8]) -> Bytes {
    let base = raw.as_ptr() as usize;
    let start = sub.as_ptr() as usize;
    match (base.checked_add(raw.len()), start.checked_add(sub.len())) {
        (Some(base_end), Some(end)) if start >= base && end <= base_end => {
            raw.slice(start - base..end - base)
        }
        _ => Bytes::copy_from_slice(sub),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use super::*;
    use crate::framer::{Framer, FramerConfig, SseFramer};
    use argus_core::TeeChunk;

    fn run(
        kind: ObserverKind,
        input: &[u8],
        mode: ObserveMode,
    ) -> (AnyObserver, Vec<ObservedEvent>) {
        let mut observer = AnyObserver::for_kind(kind, mode);
        let mut framer = SseFramer::new(FramerConfig::default());
        let mut events: Vec<ObservedEvent> = Vec::new();
        {
            let mut sink = ObserverSink::new(&mut observer, &mut events, 0);
            framer
                .push(&TeeChunk::new(Bytes::copy_from_slice(input)), &mut sink)
                .expect("合成语料不会触发上限");
            framer.finish(&mut sink).expect("合成语料不会触发上限");
        }
        observer.finish(0, &mut events);
        (observer, events)
    }

    fn labels(events: &[ObservedEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    /// 判定出 Anthropic 就真的走 Anthropic 解析器。
    #[test]
    fn for_kind_anthropic_messages_dispatches_to_the_anthropic_parser() {
        let observer = AnyObserver::for_kind(ObserverKind::AnthropicMessages, ObserveMode::Full);
        assert!(matches!(observer, AnyObserver::AnthropicMessages(_)));
        assert_eq!(observer.kind(), ObserverKind::AnthropicMessages);
    }

    /// 还没实现的语义协议落到 SSE 兜底，而不是 panic 也不是什么都不做。
    ///
    /// `kind()` 报的是**实际在观测的那个** —— 报一个没在跑的观测器，
    /// 比如实报告降级更误导人。
    #[test]
    fn for_kind_unimplemented_semantic_kinds_fall_back_to_generic_sse() {
        for kind in [
            ObserverKind::OpenAiChat,
            ObserverKind::OpenAiResponses,
            ObserverKind::GeminiGenerateContent,
            ObserverKind::GenericSse,
        ] {
            let observer = AnyObserver::for_kind(kind, ObserveMode::Full);
            assert_eq!(
                observer.kind(),
                ObserverKind::GenericSse,
                "{} 应该落到 SSE 兜底",
                kind.as_str()
            );
        }
    }

    #[test]
    fn for_kind_generic_json_dispatches_to_the_json_fallback() {
        let observer = AnyObserver::for_kind(ObserverKind::GenericJson, ObserveMode::Full);
        assert_eq!(observer.kind(), ObserverKind::GenericJson);
    }

    /// 认不出协议**不等于**不观测：每帧一个带 raw 的 `Unknown`。
    #[test]
    fn on_frame_generic_sse_emits_one_unknown_per_frame_and_keeps_raw() {
        let input =
            b"event: whatever\ndata: {\"a\":1}\n\n: ping\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n";
        let (observer, events) = run(ObserverKind::GenericSse, input, ObserveMode::Full);
        assert_eq!(labels(&events), ["unknown", "heartbeat", "unknown"]);

        let AnyObserver::GenericSse(fallback) = &observer else {
            panic!("应该是 SSE 兜底");
        };
        // 计数器仍然照常工作：字节数、事件数、`[DONE]` 哨兵
        assert_eq!(fallback.inner().frames(), 3);
        // `[DONE]` 算一帧但没有 data —— 它是哨兵不是 JSON
        assert_eq!(fallback.inner().data_bytes(), 7 + 7);
        assert!(fallback.inner().saw_done());
        // raw 留存：`docs/09` §4.1 承诺的四样东西里最难补的一样
        assert!(events.iter().filter(|e| e.raw.is_some()).count() == 2);

        let ObservedEventKind::Unknown { event_name } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 unknown");
        };
        assert_eq!(event_name, "whatever");
    }

    /// 没有 `event:` 行（`OpenAI` 系）时不猜名字 —— 猜错的语义比没有语义糟。
    #[test]
    fn on_frame_generic_sse_without_an_event_line_leaves_the_name_empty() {
        let (_, events) = run(
            ObserverKind::GenericSse,
            b"data: {\"a\":1}\n\n",
            ObserveMode::Full,
        );
        let ObservedEventKind::Unknown { event_name } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 unknown");
        };
        assert!(event_name.is_empty());
    }

    /// `GenericJson` 看的是整个 body，不是帧。接错线要能看出来。
    #[test]
    fn on_frame_generic_json_emits_no_events_but_records_an_anomaly() {
        let (observer, events) = run(
            ObserverKind::GenericJson,
            b"data: {\"a\":1}\n\n",
            ObserveMode::Full,
        );
        assert!(events.is_empty(), "不该凭空造事件：{events:?}");
        assert_eq!(observer.stats().anomalies, 1, "接线错误必须留下痕迹");
    }

    #[test]
    fn observe_body_on_the_json_fallback_records_shape_and_top_level_keys() {
        let mut fallback = GenericJsonFallback::new(ObserveMode::Full);
        fallback.observe_body(br#"{"id":"x","usage":{"input_tokens":1}}"#);
        assert_eq!(fallback.inner().shape(), JsonShape::Object);
        assert_eq!(fallback.inner().top_level_keys(), ["id", "usage"]);
    }

    /// 旁路丢字节在两条兜底路径上都要变成 `Gap`，否则观测会显示"完整"。
    #[test]
    fn on_gap_across_every_observer_kind_emits_a_gap_event() {
        for kind in ObserverKind::ALL {
            let mut observer = AnyObserver::for_kind(kind, ObserveMode::Full);
            let mut events: Vec<ObservedEvent> = Vec::new();
            observer.on_gap(64, 0, &mut events);
            assert_eq!(labels(&events), ["gap"], "{} 丢了 gap", kind.as_str());
        }
    }

    /// `ObserveMode::Lite` 不留 raw —— 由 `Emitter` 一处实现，三个观测器共享。
    #[test]
    fn emit_raw_in_lite_mode_drops_the_frame_reference() {
        let (_, events) = run(
            ObserverKind::GenericSse,
            b"data: {\"a\":1}\n\n",
            ObserveMode::Lite,
        );
        assert_eq!(labels(&events), ["unknown"]);
        assert!(events.iter().all(|e| e.raw.is_none()));
    }

    /// `frame_seq` 跟着帧走，`seq` 跟着事件走 —— 两者不是一回事。
    #[test]
    fn emit_raw_numbers_frames_and_events_independently() {
        // 注释帧占一个 frame_seq 但产出的 heartbeat 不带 raw
        let input = b": ping\n\nevent: a\ndata: {\"x\":1}\n\nevent: b\ndata: {\"y\":2}\n\n";
        let (_, events) = run(ObserverKind::GenericSse, input, ObserveMode::Full);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1, 2]);
        let frame_seqs: Vec<u32> =
            events.iter().filter_map(|e| e.raw.as_ref()).map(|r| r.frame_seq).collect();
        assert_eq!(frame_seqs, [1, 2], "注释也占一个帧号");
    }

    /// `share` 命中时是引用计数，不是拷贝。
    #[test]
    fn share_a_subslice_of_the_frame_returns_the_same_memory() {
        let raw = Bytes::from_static(b"data: {\"text\":\"hello\"}\n\n");
        let sub = raw.get(15..20).expect("固定下标");
        assert_eq!(sub, b"hello");
        let shared = share(&raw, sub);
        assert_eq!(shared.as_ptr() as usize, sub.as_ptr() as usize);
    }

    /// 不在区间内就退化成拷贝，**不 panic**（宪法 C7）。
    ///
    /// `Bytes::slice_ref` 在这种情况下会 panic，所以这里没用它。
    #[test]
    fn share_a_slice_from_elsewhere_copies_instead_of_panicking() {
        let raw = Bytes::from_static(b"abcdefgh");
        let foreign = b"xyz";
        let copied = share(&raw, foreign);
        assert_eq!(copied, Bytes::from_static(b"xyz"));
        assert_ne!(copied.as_ptr() as usize, foreign.as_ptr() as usize);
    }

    #[test]
    fn share_an_empty_slice_returns_empty_bytes() {
        let raw = Bytes::from_static(b"abc");
        assert!(share(&raw, b"").is_empty());
    }

    /// 闭包 sink：`Vec` 的 impl 与 `FnMut` 的覆盖式 impl 只能有一个。
    #[test]
    fn fn_sink_forwards_every_event_to_the_closure() {
        let mut seen = 0_u32;
        {
            let mut sink = FnSink(|_ev: ObservedEvent| seen += 1);
            let mut observer = AnyObserver::for_kind(ObserverKind::GenericSse, ObserveMode::Full);
            observer.on_gap(1, 0, &mut sink);
            observer.on_gap(2, 0, &mut sink);
        }
        assert_eq!(seen, 2);
    }
}
