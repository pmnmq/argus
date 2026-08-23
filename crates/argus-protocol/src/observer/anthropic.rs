//! Anthropic Messages 的 SSE 事件 → Observation IR。
//!
//! 映射规则逐行来自 `docs/11-observation-model.md` §5.3，线上细节（那 10 个坑）
//! 来自 `docs/09-observers.md` §5.3。本切片只做**文本路径**：`tool_use` /
//! `thinking` / `citations` 的 block 语义留给 F09 —— 但它们的**兜底**在这里
//! 就必须是对的（`Other` / `Opaque`），否则 F09 之前的每一条带工具的流都会
//! 丢数据。
//!
//! 两阶段解析：先只取 `type`，再按 type 解析对应的窄结构体。比一个大 enum
//! 省，而且未知 type 天然落进 `Unknown` 分支（宪法 C2）。
//!
//! 稳态零堆分配（宪法 C21）：所有字符串字段都是 `Cow<str>` + `#[serde(borrow)]`，
//! 没有转义序列时借用底层缓冲，文本片段则通过 [`TextChunk::Shared`] 与原始帧
//! 共享同一块 `Bytes`。

use std::{borrow::Cow, marker::PhantomData, ops::Deref};

use argus_core::{ObserveMode, ObserverKind};
use bytes::Bytes;
use compact_str::CompactString;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, Visitor},
};
use smallvec::SmallVec;

use super::{Emitter, EventSink, ParserStats, ProtocolObserver, share};
use crate::{
    framer::FrameRef,
    ir::{
        BlockCoord, BlockDelta, BlockKind, Finish, FinishReason, ObservedEventKind,
        ParseFailureKind, ResponseStart, TextChunk, UsageDelta,
    },
};

/// Anthropic Messages 解析器。
///
/// 状态只有两件：事件序号（在 [`Emitter`] 里）与「哪些 block 还开着」。
/// 刻意不做状态机校验 —— 乱序、重复、孤儿事件的容忍策略归 Accumulator
/// （F04 记 anomaly，F09 补齐），解析器只负责如实翻译。
pub struct AnthropicMessagesParser {
    em: Emitter,
    /// 还没收到 `content_block_stop` 的块。正常流里最多一两个
    open: SmallVec<[u16; 4]>,
}

impl AnthropicMessagesParser {
    #[must_use]
    pub fn new(mode: ObserveMode) -> Self {
        Self {
            em: Emitter::new(mode),
            open: SmallVec::new(),
        }
    }

    fn on_event(
        &mut self,
        event_line: Option<&[u8]>,
        data: &[u8],
        raw: &Bytes,
        at: u32,
        out: &mut dyn EventSink,
    ) {
        let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(data) else {
            // R16：非法 JSON 是一个事件，不是一个 Err。后面的帧照常解析
            return self.em.emit_parse_failure(out, at, ParseFailureKind::MalformedJson, raw);
        };

        let line = event_line.and_then(|b| core::str::from_utf8(b).ok()).map(str::trim);

        // R14：`data` 里的 `type` 是权威，`event:` 行只作 fallback
        let ty = match envelope.ty.as_deref() {
            Some(ty) => {
                if line.is_some_and(|l| l != ty) {
                    self.em.note_anomaly();
                }
                ty
            }
            None => match line {
                Some(l) => l,
                None => {
                    return self.em.emit_parse_failure(
                        out,
                        at,
                        ParseFailureKind::MissingField {
                            field: CompactString::const_new("type"),
                        },
                        raw,
                    );
                }
            },
        };

        match ty {
            "message_start" => self.on_message_start(data, raw, at, out),
            "content_block_start" => self.on_block_start(data, raw, at, out),
            "content_block_delta" => self.on_block_delta(data, raw, at, out),
            "content_block_stop" => self.on_block_stop(data, raw, at, out),
            "message_delta" => self.on_message_delta(data, raw, at, out),
            // R7：流结束标记，不产出事件
            "message_stop" => {}
            // R8（坑 A1）：ping 可随时出现，**不碰任何状态**
            "ping" => self.em.emit(out, at, ObservedEventKind::Heartbeat),
            // R9（坑 A8）：error 可在流中任意位置出现，产出事件但不中止解析
            "error" => self.on_error(data, raw, at, out),
            // R10（坑 A9）：未来会有新 event type，透传 + 计数，绝不报错
            other => {
                self.em.note_unknown_event();
                self.em.emit_raw(
                    out,
                    at,
                    ObservedEventKind::Unknown {
                        event_name: CompactString::new(other),
                    },
                    raw,
                );
            }
        }
    }

    /// R2（坑 A5）：`message_start` 同时给出响应元信息**与** input 侧 usage。
    fn on_message_start(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<MessageStartFrame<'_>>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        let message = frame.message.unwrap_or_default();
        self.em.emit_raw(
            out,
            at,
            ObservedEventKind::response_start(ResponseStart {
                provider_response_id: message.id.map(into_compact),
                model: message.model.map(into_compact),
                role: message.role.map(into_compact),
            }),
            raw,
        );
        // usage 里的 output_tokens 在这里通常是 1，照报不误 —— 合并是
        // last-non-null-wins（`docs/11` §6），message_delta 会盖掉它。
        // **绝不求和**：求和会把这个 1 加进最终计数
        if let Some(usage) = message.usage {
            self.emit_usage(usage, data, raw, at, out);
        }
    }

    /// R3 / R12：`type == "text"` 走 [`BlockKind::Text`]，其余原样保留。
    fn on_block_start(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<BlockStartFrame<'_>>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        let Some(coord) = self.coord(frame.index, raw, at, out) else {
            return;
        };
        let kind = match frame.content_block.and_then(|b| b.ty) {
            Some(ty) if &*ty == "text" => BlockKind::Text,
            // tool_use / thinking / redacted_thinking 也走这里（F09 补语义）。
            // 兜底成 `Other(type)` 而不是丢弃：块的坐标空间必须保持完整，
            // 否则后面的 delta 会落到一个不存在的块上
            Some(ty) => BlockKind::Other(into_compact(ty)),
            // provider 没说这是什么块。空的 `Other` 比编一个类型名诚实，
            // 而且块照样开着，delta 有地方落
            None => {
                self.em.note_anomaly();
                BlockKind::Other(CompactString::default())
            }
        };
        if !self.open.contains(&coord.item) {
            self.open.push(coord.item);
        }
        self.em.emit_raw(out, at, ObservedEventKind::block_start(coord, kind), raw);
    }

    /// R4 / R11：`text_delta` 走文本，其余原样装进 [`BlockDelta::Opaque`]。
    fn on_block_delta(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<BlockDeltaFrame<'_>>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        let Some(coord) = self.coord(frame.index, raw, at, out) else {
            return;
        };
        let delta = frame.delta.unwrap_or_default();
        let mapped = match delta.ty.as_deref() {
            Some("text_delta") => {
                let text = delta.text.map_or_else(
                    || {
                        // `text_delta` 没带 text。不丢事件（丢了就少一段文本
                        // 的位置信息），发一个空片段并记 anomaly
                        self.em.note_anomaly();
                        TextChunk::Shared(Bytes::new())
                    },
                    |t| text_chunk(raw, t),
                );
                BlockDelta::Text(text)
            }
            // input_json_delta / thinking_delta / signature_delta /
            // citations_delta 与将来新增的都走这里（F09 补语义）。
            // **原样保留整帧**：前向兼容诊断只能靠它（`docs/11` §8.1）
            Some(other) => BlockDelta::Opaque {
                delta_type: CompactString::new(other),
                raw: share(raw, data),
            },
            None => {
                self.em.note_anomaly();
                BlockDelta::Opaque {
                    delta_type: CompactString::default(),
                    raw: share(raw, data),
                }
            }
        };
        self.em.emit_raw(out, at, ObservedEventKind::block_delta(coord, mapped), raw);
    }

    /// R5：块结束。对 tool arguments 而言这是**可以 parse JSON 的时刻**（F09）。
    fn on_block_stop(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<BlockStopFrame>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        let Some(coord) = self.coord(frame.index, raw, at, out) else {
            return;
        };
        self.open.retain(|i| *i != coord.item);
        self.em.emit_raw(out, at, ObservedEventKind::BlockStop { at: coord }, raw);
    }

    /// R6 / R13：output 侧 usage + 结束原因。
    fn on_message_delta(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<MessageDeltaFrame<'_>>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        if let Some(usage) = frame.usage {
            self.emit_usage(usage, data, raw, at, out);
        }
        let Some(raw_reason) = frame.delta.and_then(|d| d.stop_reason) else {
            // `stop_reason` 是 null —— 造一个 `Finish` 会让这条流看起来
            // 正常结束了。记 anomaly，把「有没有结束」留给 Accumulator 判
            self.em.note_anomaly();
            return;
        };
        let reason = map_stop_reason(&raw_reason);
        if reason == FinishReason::Other {
            self.em.note_unknown_finish_reason();
        }
        self.em.emit_raw(
            out,
            at,
            ObservedEventKind::finish(Finish {
                reason,
                // 「永不丢失」：即使映射成功也留着原字符串 —— `end_turn` 与
                // `stop_sequence` 都映射成 `Stop`，只有原文能把它们分开
                raw_reason: Some(into_compact(raw_reason)),
                // Anthropic 只有一个 choice。填 `(0,0,0)` 会被误读成块坐标
                at: None,
            }),
            raw,
        );
    }

    fn on_error(&mut self, data: &[u8], raw: &Bytes, at: u32, out: &mut dyn EventSink) {
        let Ok(frame) = serde_json::from_slice::<ErrorFrame<'_>>(data) else {
            return self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
        };
        let body = frame.error.unwrap_or_default();
        self.em.emit_raw(
            out,
            at,
            ObservedEventKind::ProviderError {
                kind: body.ty.map(into_compact).unwrap_or_default(),
                message: body.message.map(into_compact).unwrap_or_default(),
            },
            raw,
        );
    }

    fn emit_usage(
        &mut self,
        usage: UsageFields,
        data: &[u8],
        raw: &Bytes,
        at: u32,
        out: &mut dyn EventSink,
    ) {
        let delta = usage.into_delta(raw, data);
        if delta.is_empty() && delta.extra.is_none() {
            return;
        }
        self.em.emit_raw(out, at, ObservedEventKind::usage(delta), raw);
    }

    /// `index` → [`BlockCoord`]。缺失或超范围时产出 `ParseFailure` 并返回
    /// `None` —— 猜一个 index 会让 delta 落到别的块上，那比丢一个事件糟。
    fn coord(
        &mut self,
        index: Option<u32>,
        raw: &Bytes,
        at: u32,
        out: &mut dyn EventSink,
    ) -> Option<BlockCoord> {
        match index.map(u16::try_from) {
            Some(Ok(i)) => return Some(BlockCoord::item(i)),
            // 一条消息不会有六万个块。走到这里是形状不对，不是 index 大
            Some(Err(_)) => {
                self.em.emit_parse_failure(out, at, ParseFailureKind::UnexpectedShape, raw);
            }
            None => self.em.emit_parse_failure(
                out,
                at,
                ParseFailureKind::MissingField {
                    field: CompactString::const_new("index"),
                },
                raw,
            ),
        }
        None
    }
}

impl ProtocolObserver for AnthropicMessagesParser {
    fn on_frame(&mut self, frame: FrameRef<'_>, at_micros: u32, out: &mut dyn EventSink) {
        self.em.begin_frame();
        match frame {
            FrameRef::Event { event, data, raw } => {
                self.on_event(event, data.as_bytes(), raw, at_micros, out);
            }
            // Anthropic 不发 `[DONE]`（那是 OpenAI 的哨兵）。兼容网关可能会发，
            // 它不携带语义 —— 记一条 anomaly 就够，不该产出事件
            FrameRef::Done => self.em.note_anomaly(),
            FrameRef::Comment => self.em.emit(out, at_micros, ObservedEventKind::Heartbeat),
        }
    }

    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink) {
        self.em.emit(out, at_micros, ObservedEventKind::Gap { dropped_bytes });
    }

    /// 关掉还开着的块。
    ///
    /// 正常的 Anthropic 流每个块都有 `content_block_stop`，所以这里通常什么
    /// 都不做。会走到的是被截断的流 —— 那时补上 `BlockStop` 让下游拿到一个
    /// 形状完整的 interaction，「是否完整」由 `Integrity` 单独表达。
    fn finish(&mut self, at_micros: u32, out: &mut dyn EventSink) {
        let mut open = core::mem::take(&mut self.open);
        open.sort_unstable();
        for item in open {
            self.em.emit(
                out,
                at_micros,
                ObservedEventKind::BlockStop {
                    at: BlockCoord::item(item),
                },
            );
        }
    }

    fn kind(&self) -> ObserverKind {
        ObserverKind::AnthropicMessages
    }

    fn stats(&self) -> ParserStats {
        self.em.stats()
    }
}

/// `stop_reason` → [`FinishReason`]（`docs/11` §5.3 的映射表，逐行）。
fn map_stop_reason(raw: &str) -> FinishReason {
    match raw {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolUse,
        "refusal" => FinishReason::Refusal,
        // 坑 A10：取值集合会扩展（`model_context_window_exceeded`、`pause_turn`…）。
        // 映射成 `Other` + 保留原文 + 上报指标，**不报错**
        _ => FinishReason::Other,
    }
}

/// 有转义就解码新分配，没转义就与原始帧共享同一块 `Bytes`。
///
/// 「这次有没有多一次分配」因此在类型上就看得见（宪法 C21）。
fn text_chunk(raw: &Bytes, text: Str<'_>) -> TextChunk {
    match text.0 {
        Cow::Borrowed(s) => TextChunk::Shared(share(raw, s.as_bytes())),
        Cow::Owned(s) => TextChunk::Decoded(CompactString::from(s)),
    }
}

/// 借用的原样拷一份；已经解码过的直接接管那块 `String`，不再拷第二次。
fn into_compact(value: Str<'_>) -> CompactString {
    match value.0 {
        Cow::Borrowed(s) => CompactString::new(s),
        Cow::Owned(s) => CompactString::from(s),
    }
}

/// 借得到就借，借不到才分配。
///
/// serde 自带的 `Cow<str>` 反序列化**永远**产出 `Owned` —— 用它意味着每个
/// text delta 都多一次堆分配，而 delta 是每 token 一个的（宪法 C21）。
/// 手写一个 visitor：没有转义序列时 `visit_borrowed_str` 直接借底层缓冲，
/// 有转义时才落到 `visit_str` 去解码。
struct Str<'a>(Cow<'a, str>);

impl Deref for Str<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Str<'a> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_str(StrVisitor(PhantomData))
    }
}

struct StrVisitor<'a>(PhantomData<&'a ()>);

impl<'de: 'a, 'a> Visitor<'de> for StrVisitor<'a> {
    type Value = Str<'a>;

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a string")
    }

    /// 无转义：直接借，零分配
    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Str<'a>, E> {
        Ok(Str(Cow::Borrowed(v)))
    }

    /// 含转义：`serde_json` 已经解码到一块临时缓冲，必须拷一份
    fn visit_str<E>(self, v: &str) -> Result<Str<'a>, E> {
        Ok(Str(Cow::Owned(v.to_owned())))
    }

    fn visit_string<E>(self, v: String) -> Result<Str<'a>, E> {
        Ok(Str(Cow::Owned(v)))
    }
}

// ── 帧结构体 ────────────────────────────────────────────────────────────
//
// 每个都是**窄**的：只声明这一层要用的字段。serde 默认忽略未知字段，
// 所以 provider 加字段不会让解析失败（宪法 C2）。

/// 第一阶段：只把 `type` 取出来。
#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(borrow, rename = "type")]
    ty: Option<Str<'a>>,
}

#[derive(Deserialize)]
struct MessageStartFrame<'a> {
    #[serde(borrow)]
    message: Option<MessageBody<'a>>,
}

#[derive(Deserialize, Default)]
struct MessageBody<'a> {
    #[serde(borrow)]
    id: Option<Str<'a>>,
    #[serde(borrow)]
    model: Option<Str<'a>>,
    #[serde(borrow)]
    role: Option<Str<'a>>,
    usage: Option<UsageFields>,
}

#[derive(Deserialize)]
struct BlockStartFrame<'a> {
    index: Option<u32>,
    #[serde(borrow)]
    content_block: Option<ContentBlock<'a>>,
}

#[derive(Deserialize)]
struct ContentBlock<'a> {
    #[serde(borrow, rename = "type")]
    ty: Option<Str<'a>>,
}

#[derive(Deserialize)]
struct BlockDeltaFrame<'a> {
    index: Option<u32>,
    #[serde(borrow)]
    delta: Option<DeltaBody<'a>>,
}

#[derive(Deserialize, Default)]
struct DeltaBody<'a> {
    #[serde(borrow, rename = "type")]
    ty: Option<Str<'a>>,
    #[serde(borrow)]
    text: Option<Str<'a>>,
}

#[derive(Deserialize)]
struct BlockStopFrame {
    index: Option<u32>,
}

#[derive(Deserialize)]
struct MessageDeltaFrame<'a> {
    #[serde(borrow)]
    delta: Option<StopInfo<'a>>,
    usage: Option<UsageFields>,
}

#[derive(Deserialize)]
struct StopInfo<'a> {
    #[serde(borrow)]
    stop_reason: Option<Str<'a>>,
}

#[derive(Deserialize)]
struct ErrorFrame<'a> {
    #[serde(borrow)]
    error: Option<ErrorBody<'a>>,
}

#[derive(Deserialize, Default)]
struct ErrorBody<'a> {
    #[serde(borrow, rename = "type")]
    ty: Option<Str<'a>>,
    #[serde(borrow)]
    message: Option<Str<'a>>,
}

/// usage 对象。R15：cache 两个字段必须保留，它们直接影响成本计算。
///
/// 手写 `Deserialize` 而不是 `#[derive]`，只为多得到一个 `saw_unknown` ——
/// provider 新加了计量字段（`cache_creation` 从数字变成对象就发生过）时，
/// 我们要能把整帧留下来，而不是悄悄丢掉一笔钱（宪法 C2）。
#[derive(Default)]
struct UsageFields {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
    cache_read_input_tokens: Option<u32>,
    saw_unknown: bool,
}

impl UsageFields {
    fn into_delta(self, raw: &Bytes, data: &[u8]) -> UsageDelta {
        UsageDelta {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            // Anthropic 不单独报 reasoning token
            reasoning_tokens: None,
            // 留的是**整帧**而不是 usage 子对象：切出子对象要再扫一遍，
            // 而整帧是现成的、零拷贝的，诊断时信息还更全
            extra: self.saw_unknown.then(|| Box::new(share(raw, data))),
        }
    }
}

impl<'de> Deserialize<'de> for UsageFields {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_map(UsageVisitor)
    }
}

struct UsageVisitor;

impl<'de> Visitor<'de> for UsageVisitor {
    type Value = UsageFields;

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("an anthropic usage object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UsageFields, A::Error> {
        let mut out = UsageFields::default();
        // key 用 `Cow` 而不是 `&str`：含转义的 key 借不出来，用 `&str`
        // 会让整个 usage 变成解析失败
        while let Some(key) = map.next_key::<Str<'de>>()? {
            match &*key {
                "input_tokens" => out.input_tokens = map.next_value()?,
                "output_tokens" => out.output_tokens = map.next_value()?,
                "cache_creation_input_tokens" => {
                    out.cache_creation_input_tokens = map.next_value()?;
                }
                "cache_read_input_tokens" => out.cache_read_input_tokens = map.next_value()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                    out.saw_unknown = true;
                }
            }
        }
        Ok(out)
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
    use crate::{
        accumulator::UsageAccumulator,
        framer::{Framer, FramerConfig, SseFramer},
        ir::ObservedEvent,
        observer::ObserverSink,
    };
    use argus_core::TeeChunk;

    // ── 合成语料 ──────────────────────────────────────────────────────
    //
    // ⚠️ 这些是**合成**的字节，不是 fixture。宪法 C25 要求 fixture 来自录制，
    // 而录制工具是 F10 的活（R6）—— 在它到位之前，`tests/fixtures/` 保持为空
    // 比塞进手写字节好：混进去之后就再也分不清哪些是真的了。
    // 这里的字节只用来钉住映射表的每一行，形状照抄 `docs/09` §5.3 的示例。

    const MESSAGE_START: &[u8] = br#"event: message_start
data: {"type":"message_start","message":{"id":"msg_syn_1","type":"message","role":"assistant","model":"claude-x","content":[],"stop_reason":null,"usage":{"input_tokens":25,"cache_creation_input_tokens":3,"cache_read_input_tokens":7,"output_tokens":1}}}

"#;

    const BLOCK_START: &[u8] = br#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#;

    const TEXT_DELTA: &[u8] = br#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

"#;

    const BLOCK_STOP: &[u8] = br#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#;

    const MESSAGE_DELTA: &[u8] = br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

"#;

    const MESSAGE_STOP: &[u8] = br#"event: message_stop
data: {"type":"message_stop"}

"#;

    fn simple_stream() -> Vec<u8> {
        [
            MESSAGE_START,
            BLOCK_START,
            TEXT_DELTA,
            BLOCK_STOP,
            MESSAGE_DELTA,
            MESSAGE_STOP,
        ]
        .concat()
    }

    fn run(input: &[u8]) -> (Vec<ObservedEvent>, ParserStats) {
        run_with(input, ObserveMode::Full)
    }

    fn run_with(input: &[u8], mode: ObserveMode) -> (Vec<ObservedEvent>, ParserStats) {
        let mut framer = SseFramer::new(FramerConfig::default());
        let mut parser = AnthropicMessagesParser::new(mode);
        let mut events: Vec<ObservedEvent> = Vec::new();
        {
            let mut sink = ObserverSink::new(&mut parser, &mut events, 0);
            framer
                .push(&TeeChunk::new(Bytes::copy_from_slice(input)), &mut sink)
                .expect("合成语料不会触发上限");
            framer.finish(&mut sink).expect("合成语料不会触发上限");
        }
        parser.finish(0, &mut events);
        let stats = parser.stats();
        (events, stats)
    }

    fn labels(events: &[ObservedEvent]) -> Vec<&'static str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    fn usage_at(events: &[ObservedEvent], idx: usize) -> UsageDelta {
        match &events.get(idx).expect("下标越界").kind {
            ObservedEventKind::Usage(u) => (**u).clone(),
            other => panic!("第 {idx} 个事件不是 usage，是 {}", other.as_str()),
        }
    }

    // ── R2–R7：基础路径 ───────────────────────────────────────────────

    /// `docs/11` §5.3 的映射表，一条简单文本流走完的样子。
    #[test]
    fn on_frame_a_simple_text_stream_produces_the_documented_event_sequence() {
        let (events, stats) = run(&simple_stream());
        assert_eq!(
            labels(&events),
            [
                "response_start",
                "usage",
                "block_start",
                "block_delta",
                "block_stop",
                "usage",
                "finish",
            ],
            "映射表被改动了"
        );
        // seq 单调递增、从 0 开始，且中间不跳号
        let seqs: Vec<u32> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (0..7).collect::<Vec<_>>());
        assert_eq!(stats.frames, 6);
        assert_eq!(stats.events, 7);
        assert_eq!(stats.parse_failures, 0);
        assert_eq!(stats.unknown_events, 0);
        assert_eq!(stats.anomalies, 0);
    }

    /// R2：`message_start` 同时给出元信息与 input 侧 usage。
    #[test]
    fn on_frame_message_start_emits_response_start_and_input_usage() {
        let (events, _) = run(MESSAGE_START);
        let ObservedEventKind::ResponseStart(start) = &events.first().expect("有事件").kind
        else {
            panic!("第一个事件应该是 response_start");
        };
        assert_eq!(start.provider_response_id.as_deref(), Some("msg_syn_1"));
        assert_eq!(start.model.as_deref(), Some("claude-x"));
        assert_eq!(start.role.as_deref(), Some("assistant"));

        let usage = usage_at(&events, 1);
        assert_eq!(usage.input_tokens, Some(25));
        assert_eq!(usage.output_tokens, Some(1));
    }

    /// R3 + R4：text block 的开始与增量。
    #[test]
    fn on_frame_text_block_start_and_delta_map_to_text_kind() {
        let (events, _) = run(&[BLOCK_START, TEXT_DELTA].concat());
        let ObservedEventKind::BlockStart { at, kind } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 block_start");
        };
        assert_eq!(*at, BlockCoord::new(0, 0, 0));
        assert_eq!(**kind, BlockKind::Text);

        let ObservedEventKind::BlockDelta { at, delta } = &events.get(1).expect("有事件").kind
        else {
            panic!("应该是 block_delta");
        };
        assert_eq!(*at, BlockCoord::new(0, 0, 0));
        let BlockDelta::Text(chunk) = &**delta else {
            panic!("应该是 text delta");
        };
        assert_eq!(chunk.as_str(), Some("Hello"));
    }

    /// R7：`message_stop` 只是流结束标记，不产出事件。
    #[test]
    fn on_frame_message_stop_produces_no_event() {
        let (events, stats) = run(MESSAGE_STOP);
        assert!(events.is_empty(), "message_stop 不该产出事件：{events:?}");
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.anomalies, 0);
    }

    // ── R8（坑 A1）：ping ─────────────────────────────────────────────

    /// ping 可以出现在任何位置，产出 `Heartbeat` 且**不碰任何状态**。
    ///
    /// 判据不是「ping 被正确处理了」，而是**把 ping 全部删掉之后，剩下的
    /// 事件序列一模一样** —— 这才叫「不影响状态机」。
    #[test]
    fn on_frame_ping_interleaved_everywhere_changes_nothing_but_heartbeats() {
        const PING: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let clean = simple_stream();
        let peppered = [
            PING,
            MESSAGE_START,
            PING,
            BLOCK_START,
            PING,
            TEXT_DELTA,
            PING,
            BLOCK_STOP,
            PING,
            MESSAGE_DELTA,
            PING,
            MESSAGE_STOP,
            PING,
        ]
        .concat();

        let (base, _) = run(&clean);
        let (got, stats) = run(&peppered);

        let without_heartbeats: Vec<&'static str> =
            labels(&got).into_iter().filter(|l| *l != "heartbeat").collect();
        assert_eq!(without_heartbeats, labels(&base));
        assert_eq!(
            labels(&got).iter().filter(|l| **l == "heartbeat").count(),
            7
        );
        assert_eq!(stats.anomalies, 0, "ping 不是异常");
        assert_eq!(stats.unknown_events, 0, "ping 是已知事件");
    }

    // ── R2 + R6 + R15（坑 A5/A6）：usage 分两处 ────────────────────────

    /// usage 分两处上报，合并是 **last-non-null-wins，绝不求和**。
    ///
    /// 这条断言存在的理由：`message_start` 里的 `output_tokens` 通常是 1。
    /// 一旦有人把合并写成求和，最终的 output 就会多 1 —— 一个小到不会有人
    /// 察觉、却会一路错进账单的偏差。
    #[test]
    fn on_frame_usage_split_across_two_frames_is_merged_not_summed() {
        let (events, _) = run(&simple_stream());
        let first = usage_at(&events, 1);
        let second = usage_at(&events, 5);

        // 两次上报各自独立：`None` 表示"这次没报"，不是"报了 0"
        assert_eq!(first.output_tokens, Some(1));
        assert_eq!(second.output_tokens, Some(15));
        assert_eq!(second.input_tokens, None);

        let mut acc = UsageAccumulator::new();
        acc.apply(&first);
        acc.apply(&second);
        let resolved = acc.resolve(0);
        assert_eq!(resolved.input_tokens, Some(25));
        assert_eq!(
            resolved.output_tokens,
            Some(15),
            "output 被求和了：1 + 15 = 16，而正确答案是最后一次上报的 15"
        );
    }

    /// R15（坑 A6）：cache 两个字段直接影响成本，必须保留。
    #[test]
    fn on_frame_cache_tokens_are_carried_into_the_usage_delta() {
        let (events, _) = run(MESSAGE_START);
        let usage = usage_at(&events, 1);
        assert_eq!(usage.cache_creation_input_tokens, Some(3));
        assert_eq!(usage.cache_read_input_tokens, Some(7));
    }

    /// 宪法 C2：provider 新加的计量字段不能被悄悄丢掉。
    #[test]
    fn on_frame_usage_with_unmapped_fields_keeps_the_whole_frame_in_extra() {
        let frame = br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9,"server_tool_use":{"web_search_requests":2}}}

"#;
        let (events, _) = run(frame);
        let usage = usage_at(&events, 0);
        assert_eq!(usage.output_tokens, Some(9));
        let extra = usage.extra.expect("未映射字段必须留下线索");
        assert!(
            extra.windows(15).any(|w| w == b"server_tool_use"),
            "留下来的应该是原始帧"
        );
    }

    /// 已知字段全在时不该留 extra —— 每帧一次 `Box` 分配是要付代价的。
    #[test]
    fn on_frame_usage_with_only_known_fields_leaves_extra_empty() {
        let (events, _) = run(MESSAGE_START);
        assert!(usage_at(&events, 1).extra.is_none());
    }

    // ── R13（坑 A10）：stop_reason ────────────────────────────────────

    /// `docs/11` §5.3 的 `stop_reason` 映射表，逐行。
    #[test]
    fn map_stop_reason_over_the_documented_table_matches_docs_11() {
        for (raw, want) in [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("max_tokens", FinishReason::Length),
            ("tool_use", FinishReason::ToolUse),
            ("refusal", FinishReason::Refusal),
            ("model_context_window_exceeded", FinishReason::Other),
            ("pause_turn", FinishReason::Other),
            ("", FinishReason::Other),
        ] {
            assert_eq!(map_stop_reason(raw), want, "stop_reason={raw}");
        }
    }

    /// 不认识的 `stop_reason` → `Other` + 保留原文 + 计数，**不报错**。
    #[test]
    fn on_frame_unknown_stop_reason_maps_to_other_and_keeps_the_raw_string() {
        let frame = br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded"},"usage":{"output_tokens":4}}

"#;
        let (events, stats) = run(frame);
        let ObservedEventKind::Finish(finish) = &events.get(1).expect("有 finish").kind else {
            panic!("第二个事件应该是 finish");
        };
        assert_eq!(finish.reason, FinishReason::Other);
        assert_eq!(
            finish.raw_reason.as_deref(),
            Some("model_context_window_exceeded")
        );
        assert_eq!(stats.unknown_finish_reasons, 1);
        assert_eq!(stats.parse_failures, 0, "不认识不等于解析失败");
    }

    /// 映射成功也要留原文：`end_turn` 与 `stop_sequence` 都是 `Stop`，
    /// 只有原文能把它们分开。
    #[test]
    fn on_frame_known_stop_reason_still_keeps_the_raw_string() {
        let (events, _) = run(&simple_stream());
        let ObservedEventKind::Finish(finish) = &events.get(6).expect("有 finish").kind else {
            panic!("最后一个事件应该是 finish");
        };
        assert_eq!(finish.reason, FinishReason::Stop);
        assert_eq!(finish.raw_reason.as_deref(), Some("end_turn"));
        assert_eq!(finish.at, None, "Anthropic 只有一个 choice");
    }

    /// `stop_reason` 是 null 时不该凭空造一个 `Finish`。
    #[test]
    fn on_frame_message_delta_without_stop_reason_emits_usage_only() {
        let frame = br#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":4}}

"#;
        let (events, stats) = run(frame);
        assert_eq!(labels(&events), ["usage"]);
        assert_eq!(stats.anomalies, 1);
    }

    // ── R9（坑 A8）：error ────────────────────────────────────────────

    /// `error` 可以出现在流中任意位置，产出事件但**不中止解析**。
    #[test]
    fn on_frame_error_in_the_middle_emits_provider_error_and_keeps_parsing() {
        const ERROR: &[u8] = br#"event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}

"#;
        let stream = [MESSAGE_START, BLOCK_START, ERROR, TEXT_DELTA, BLOCK_STOP].concat();
        let (events, stats) = run(&stream);
        assert_eq!(
            labels(&events),
            [
                "response_start",
                "usage",
                "block_start",
                "provider_error",
                "block_delta",
                "block_stop",
            ],
            "error 之后的帧必须照常解析"
        );
        let ObservedEventKind::ProviderError { kind, message } =
            &events.get(3).expect("有 error").kind
        else {
            panic!("应该是 provider_error");
        };
        assert_eq!(kind, "overloaded_error");
        assert_eq!(message, "Overloaded");
        assert_eq!(stats.parse_failures, 0);
    }

    // ── R10–R12（坑 A9）：未知的东西 ──────────────────────────────────

    /// R10：未知 event 名 → `Unknown` + 计数 + **raw 必须留着**。
    #[test]
    fn on_frame_unknown_event_name_emits_unknown_and_keeps_the_raw_frame() {
        let frame =
            b"event: message_pause\ndata: {\"type\":\"message_pause\",\"reason\":\"x\"}\n\n";
        let (events, stats) = run(frame);
        let ev = events.first().expect("有事件");
        let ObservedEventKind::Unknown { event_name } = &ev.kind else {
            panic!("应该是 unknown");
        };
        assert_eq!(event_name, "message_pause");
        assert_eq!(stats.unknown_events, 1);
        assert_eq!(stats.parse_failures, 0, "不认识不等于解析失败");
        // 前向兼容诊断的唯一依据（`docs/11` §8.1）
        let raw = ev.raw.as_ref().expect("Full 模式必须留 raw");
        assert!(raw.frame.windows(13).any(|w| w == b"message_pause"));
    }

    /// R11：未知 `delta.type` → `Opaque`，原样保留。
    #[test]
    fn on_frame_unknown_delta_type_emits_opaque_with_the_raw_frame() {
        let frame = br#"event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}

"#;
        let (events, stats) = run(frame);
        let ObservedEventKind::BlockDelta { at, delta } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 block_delta");
        };
        assert_eq!(*at, BlockCoord::new(0, 2, 0));
        let BlockDelta::Opaque { delta_type, raw } = &**delta else {
            panic!("应该是 opaque");
        };
        assert_eq!(delta_type, "input_json_delta");
        assert!(raw.windows(16).any(|w| w == b"input_json_delta"));
        assert_eq!(stats.parse_failures, 0);
    }

    /// R12：未知 `content_block.type` → `Other(type)`，块照样开着。
    #[test]
    fn on_frame_unknown_content_block_type_emits_other_kind() {
        let frame = br#"event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}

"#;
        let (events, stats) = run(frame);
        let ObservedEventKind::BlockStart { at, kind } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 block_start");
        };
        assert_eq!(*at, BlockCoord::new(0, 1, 0));
        assert_eq!(
            **kind,
            BlockKind::Other(CompactString::const_new("tool_use"))
        );
        assert_eq!(stats.parse_failures, 0);
    }

    // ── R14：`event:` 行只是 fallback ─────────────────────────────────

    /// `data.type` 是权威，`event:` 行只作 fallback；打架时记 anomaly。
    #[test]
    fn on_frame_conflicting_event_line_defers_to_data_type_and_notes_an_anomaly() {
        let frame = b"event: content_block_stop\ndata: {\"type\":\"ping\"}\n\n";
        let (events, stats) = run(frame);
        assert_eq!(labels(&events), ["heartbeat"], "应该听 data.type 的");
        assert_eq!(stats.anomalies, 1);
    }

    /// `data` 里没有 `type` 时才回头看 `event:` 行。
    #[test]
    fn on_frame_missing_data_type_falls_back_to_the_event_line() {
        let frame = b"event: content_block_stop\ndata: {\"index\":0}\n\n";
        let (events, stats) = run(frame);
        assert_eq!(labels(&events), ["block_stop"]);
        assert_eq!(stats.anomalies, 0);
        assert_eq!(stats.parse_failures, 0);
    }

    /// 两边都没有 → `ParseFailure`，而不是猜。
    #[test]
    fn on_frame_without_any_type_emits_a_missing_field_parse_failure() {
        let (events, stats) = run(b"data: {\"index\":0}\n\n");
        let ObservedEventKind::ParseFailure { reason } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 parse_failure");
        };
        assert_eq!(
            *reason,
            ParseFailureKind::MissingField {
                field: CompactString::const_new("type")
            }
        );
        assert_eq!(stats.parse_failures, 1);
    }

    // ── R16：非法 JSON ───────────────────────────────────────────────

    /// 非法 JSON 是**一个事件**，不是一个 `Err`；后续帧照常解析（宪法 C7）。
    #[test]
    fn on_frame_malformed_json_emits_parse_failure_and_keeps_going() {
        let stream = [
            MESSAGE_START,
            b"event: content_block_delta\ndata: {\"type\":\"content_bl\n\n",
            TEXT_DELTA,
        ]
        .concat();
        let (events, stats) = run(&stream);
        assert_eq!(
            labels(&events),
            ["response_start", "usage", "parse_failure", "block_delta"],
            "坏帧之后的帧必须还能解析"
        );
        let ObservedEventKind::ParseFailure { reason } = &events.get(2).expect("有事件").kind
        else {
            panic!("应该是 parse_failure");
        };
        assert_eq!(*reason, ParseFailureKind::MalformedJson);
        assert_eq!(stats.parse_failures, 1);
    }

    /// JSON 合法但形状不对（`index` 缺失）→ 也不 panic。
    #[test]
    fn on_frame_block_delta_without_index_emits_a_missing_field_parse_failure() {
        let frame = br#"event: content_block_delta
data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}

"#;
        let (events, stats) = run(frame);
        let ObservedEventKind::ParseFailure { reason } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 parse_failure");
        };
        assert_eq!(
            *reason,
            ParseFailureKind::MissingField {
                field: CompactString::const_new("index")
            }
        );
        assert_eq!(stats.parse_failures, 1);
    }

    // ── R17：零分配 ──────────────────────────────────────────────────

    /// 没有转义序列的 delta 与原始帧共享同一块 `Bytes`，不额外分配。
    ///
    /// 判据是类型本身：`TextChunk::Shared` 只可能来自 `Bytes::slice`
    /// （引用计数），`Decoded` 才是新分配。再加一条地址断言，防止将来有人
    /// 把 `share` 改成拷贝之后测试还是绿的。
    #[test]
    fn on_frame_unescaped_text_delta_shares_the_frame_buffer() {
        let (events, _) = run(TEXT_DELTA);
        let ObservedEventKind::BlockDelta { delta, .. } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 block_delta");
        };
        let BlockDelta::Text(chunk) = &**delta else {
            panic!("应该是 text");
        };
        assert!(chunk.is_shared(), "无转义的 delta 不该新分配");

        let raw = &events.first().expect("有事件").raw.as_ref().expect("有 raw").frame;
        let lo = raw.as_ptr() as usize;
        let ptr = chunk.as_bytes().as_ptr() as usize;
        assert!(
            (lo..lo + raw.len()).contains(&ptr),
            "文本片段不在原始帧的地址区间内，说明发生了拷贝"
        );
    }

    /// 含转义的 delta 必须解码 —— 这时新分配是**必要**的，不是浪费。
    #[test]
    fn on_frame_escaped_text_delta_is_decoded_instead_of_shared() {
        let frame = br#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a\nb\u00e9"}}

"#;
        let (events, _) = run(frame);
        let ObservedEventKind::BlockDelta { delta, .. } = &events.first().expect("有事件").kind
        else {
            panic!("应该是 block_delta");
        };
        let BlockDelta::Text(chunk) = &**delta else {
            panic!("应该是 text");
        };
        assert!(!chunk.is_shared(), "含转义就必须解码");
        assert_eq!(chunk.as_str(), Some("a\nb\u{00e9}"));
    }

    // ── finish / gap / ObserveMode ───────────────────────────────────

    /// 流被截断时补上缺的 `BlockStop`，让下游拿到形状完整的 interaction。
    #[test]
    fn finish_with_blocks_still_open_emits_the_missing_block_stops() {
        let stream = [
            BLOCK_START,
            br#"event: content_block_start
data: {"type":"content_block_start","index":3,"content_block":{"type":"text"}}

"#,
            TEXT_DELTA,
        ]
        .concat();
        let (events, _) = run(&stream);
        assert_eq!(
            labels(&events),
            [
                "block_start",
                "block_start",
                "block_delta",
                "block_stop",
                "block_stop",
            ]
        );
        // 补出来的 stop 按 item 升序
        let items: Vec<u16> = events
            .iter()
            .filter_map(|e| match e.kind {
                ObservedEventKind::BlockStop { at } => Some(at.item),
                _ => None,
            })
            .collect();
        assert_eq!(items, [0, 3]);
    }

    /// 正常收尾的流不该被补出多余的 `BlockStop`。
    #[test]
    fn finish_after_a_complete_stream_emits_nothing() {
        let (events, _) = run(&simple_stream());
        assert_eq!(labels(&events).len(), 7);
    }

    /// 旁路丢字节 → `Gap` 事件，且 `seq` 不断档。
    #[test]
    fn on_gap_emits_a_gap_event_without_breaking_the_sequence() {
        let mut parser = AnthropicMessagesParser::new(ObserveMode::Full);
        let mut events: Vec<ObservedEvent> = Vec::new();
        parser.on_gap(4096, 7, &mut events);
        parser.on_gap(8, 9, &mut events);
        assert_eq!(labels(&events), ["gap", "gap"]);
        assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1]);
        assert!(matches!(
            events.first().expect("有事件").kind,
            ObservedEventKind::Gap {
                dropped_bytes: 4096
            }
        ));
    }

    /// `ObserveMode::Lite` 不留 raw（`docs/11` §4.1）—— 事件序列不变。
    #[test]
    fn on_frame_in_lite_mode_produces_the_same_events_without_raw() {
        let stream = simple_stream();
        let (full, _) = run_with(&stream, ObserveMode::Full);
        let (lite, _) = run_with(&stream, ObserveMode::Lite);
        assert_eq!(labels(&lite), labels(&full));
        assert!(full.iter().any(|e| e.raw.is_some()));
        assert!(lite.iter().all(|e| e.raw.is_none()), "Lite 不该留 raw");
    }

    /// 注释行是 SSE 层的心跳，与协议层的 `ping` 走同一个事件。
    #[test]
    fn on_frame_sse_comment_line_emits_heartbeat() {
        let (events, stats) = run(b": keep-alive\n\n");
        assert_eq!(labels(&events), ["heartbeat"]);
        assert_eq!(stats.frames, 1);
    }

    /// Anthropic 不发 `[DONE]`。兼容网关发了也不该变成事件。
    #[test]
    fn on_frame_done_sentinel_emits_no_event_but_notes_an_anomaly() {
        let (events, stats) = run(b"data: [DONE]\n\n");
        assert!(events.is_empty());
        assert_eq!(stats.anomalies, 1);
    }

    /// T7：四类计数各归各位，供 F06 接指标。
    #[test]
    fn stats_over_a_stream_with_every_oddity_counts_each_category_separately() {
        let stream = [
            b"event: nope\ndata: {\"type\":\"nope\"}\n\n".as_slice(),
            b"data: {oops\n\n",
            br#"data: {"type":"message_delta","delta":{"stop_reason":"brand_new"}}

"#,
            b"event: ping\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        ]
        .concat();
        let (_, stats) = run(&stream);
        assert_eq!(stats.frames, 4);
        assert_eq!(stats.unknown_events, 1);
        assert_eq!(stats.parse_failures, 1);
        assert_eq!(stats.unknown_finish_reasons, 1);
        assert_eq!(stats.anomalies, 1);
    }

    /// 观测器报的是自己是谁，不是请求时判定的那个。
    #[test]
    fn kind_of_the_anthropic_parser_reports_anthropic_messages() {
        let parser = AnthropicMessagesParser::new(ObserveMode::Full);
        assert_eq!(parser.kind(), ObserverKind::AnthropicMessages);
    }
}
