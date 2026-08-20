# 11 · Observation Model

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`09-observers.md`（线上格式）、`10-streaming.md`（字节层）

---

## 1. Problem

需要一个内部数据模型，把三种（未来更多）互不相同的流式协议，归一化成可以统一审计、统计、安全分析的结构——**同时不丢失任何原始信息**。

难点：

1. 三种协议的**坐标系不同**：Anthropic 是一维 `index`；OpenAI Chat 是 `choices[i].delta` 里的隐式位置 + `tool_calls[j].index`；OpenAI Responses 是二维 `output_index` + `content_index`。
2. **增量语义不同**：Anthropic 的 `input_json_delta` 是字符串拼接；OpenAI 的 tool arguments 也是拼接但 `id`/`name` 出现时机不同；Gemini 干脆发累积快照。
3. **usage 语义不同**：Anthropic 分两次给（input 在开头，output 在结尾）；OpenAI 在最后一个 chunk 一次给全。
4. **信息不可逆丢失的风险**：任何归一化都有丢失字段的倾向，而审计恰恰最需要那些"奇怪的字段"。

---

## 2. Goals

- G-O1 IR 能无歧义表达三种协议的全部结构化语义。
- G-O2 任何 IR 事件都能追溯到产生它的原始字节。
- G-O3 未知字段、未知事件、未知 finish reason 都有明确的容纳位置。
- G-O4 IR **不用于重建上游请求**（这是刻意的非目标，见 §3）。
- G-O5 Accumulator 在流被任意位置中断时，都能产出一个**语义正确的 partial 结果**，且明确标注哪部分不完整。

## 3. Non-goals

- **不做统一请求格式**。IR 只描述"发生了什么"，不描述"如何重放"。想重放请用 raw request。
- 不做 token 级别的对齐（我们看到的是 provider 给的 delta，不是真正的 token）。
- 不做语义理解（那是 `12-security.md` 的事）。

### 3.1 为什么 IR 不能用于重建请求

如果 IR 要能重建任意 provider 的请求，它就必须是三种协议的**并集**，且要处理所有互斥字段。这个模型会：
- 随每个 provider 的每次更新而膨胀；
- 迫使我们对未知字段做决策（丢弃？保留？放哪？）；
- 最终变成一个"统一协议"——正是 `00-overview.md` N2 明确拒绝的东西。

IR 只需要是三种协议的**共同抽象的交集 + 一个 raw 逃生舱**，这个模型小、稳定、可测。

---

## 4. Observation IR 定义

### 4.1 顶层

```rust
/// 观测事件。一条流产出一个有序序列。
pub struct ObservedEvent {
    /// 单调递增，从 0 开始，per-interaction
    pub seq: u32,
    /// 相对请求开始的微秒数
    pub at_micros: u32,
    pub kind: ObservedEventKind,
    /// 产生该事件的原始帧的引用（零拷贝）。ObserveMode::Lite 下为 None。
    pub raw: Option<RawRef>,
}

/// 指向原始 SSE frame 的引用。Bytes 是 refcount 的，不复制。
pub struct RawRef {
    pub frame: Bytes,
    /// frame 在整条流中的序号，用于排序与去重
    pub frame_seq: u32,
}
```

### 4.2 事件类型

```rust
#[non_exhaustive]
pub enum ObservedEventKind {
    // ── 生命周期 ────────────────────────────────
    ResponseStart(ResponseStart),
    Finish(Finish),

    // ── 内容块 ──────────────────────────────────
    BlockStart { at: BlockCoord, kind: BlockKind },
    BlockDelta { at: BlockCoord, delta: BlockDelta },
    BlockStop  { at: BlockCoord },

    // ── 计量 ────────────────────────────────────
    Usage(UsageDelta),

    // ── 元信息 ──────────────────────────────────
    /// provider 特有的、我们没有语义映射的事件（如 Anthropic 未来新增的 event）
    Unknown { event_name: CompactString },
    /// provider 主动发出的错误事件
    ProviderError { kind: CompactString, message: CompactString },
    /// SSE 层的心跳 / 注释行
    Heartbeat,

    // ── 流健康（Argus 自己产生，不来自 provider）───
    Gap { dropped_bytes: u64 },
    ParseFailure { reason: ParseFailureKind },
}
```

**关键**：`ToolCall*` 不是独立的事件类型，而是 `BlockKind::ToolUse` + `BlockDelta::Json`。这样处理是因为在 Anthropic 里 tool_use **就是**一个 content block，强行拆开会让映射不自然。OpenAI Chat 的 tool_calls 虽然不在 content 数组里，但用同一个模型表达同样自然（见 §5.1）。

### 4.3 内容块

```rust
#[non_exhaustive]
pub enum BlockKind {
    Text,
    /// 推理内容。summary=true 表示这是摘要而非原文（OpenAI Responses 的行为）
    Reasoning { summary: bool, redacted: bool },
    ToolUse { id: Option<CompactString>, name: Option<CompactString> },
    /// 客户端回传的工具结果（只在请求侧观测到）
    ToolResult { tool_use_id: CompactString, is_error: bool },
    Refusal,
    Image { media_type: Option<CompactString> },
    Audio { media_type: Option<CompactString> },
    /// provider 有但我们没映射的 block 类型
    Other(CompactString),
}

#[non_exhaustive]
pub enum BlockDelta {
    /// 文本片段。借用自 RawRef 的字节，或在含转义时是新分配的。
    Text(TextChunk),
    Reasoning(TextChunk),
    /// tool arguments 的 JSON 片段。**绝不在此处 parse**。
    Json(TextChunk),
    /// Anthropic thinking 的签名，必须原样保留
    Signature(TextChunk),
    /// 引用信息（Anthropic citations_delta）
    Citation(Bytes),
    /// 我们没映射的 delta 类型，原样保留
    Opaque { delta_type: CompactString, raw: Bytes },
}
```

### 4.4 坐标系

这是 IR 的核心抽象，用来统一三种协议的定位方式：

```rust
/// 统一的内容块坐标。
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct BlockCoord {
    /// 候选索引。OpenAI 的 choices[i] / n>1；Anthropic 恒为 0。
    pub choice: u8,
    /// 输出项索引。Responses 的 output_index；Anthropic 的 content index；
    /// OpenAI Chat 见 §5.1 的合成规则。
    pub item: u16,
    /// 项内部的内容索引。Responses 的 content_index；其他协议恒为 0。
    pub part: u16,
}
```

三级坐标 `(choice, item, part)` 是三种协议坐标系的**最小公共超集**。用固定大小的 `Copy` 类型（5 字节，pad 到 8）意味着它可以作为 `HashMap` key 而无需分配。

### 4.5 生命周期与计量

```rust
pub struct ResponseStart {
    pub provider_response_id: Option<CompactString>,
    pub model: Option<CompactString>,      // provider 实际使用的模型（可能与请求不同！）
    pub role: Option<CompactString>,
}

pub struct UsageDelta {
    pub input_tokens:  Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
    pub cache_read_input_tokens:     Option<u32>,
    pub reasoning_tokens: Option<u32>,
    /// provider 给出但我们没有映射的 usage 字段，原样保留
    pub extra: Option<Bytes>,
}

pub struct Finish {
    pub reason: FinishReason,
    /// provider 的原始字符串，永不丢失
    pub raw_reason: Option<CompactString>,
    pub at: Option<BlockCoord>,            // 哪个 choice 结束了
}

#[non_exhaustive]
pub enum FinishReason {
    Stop, Length, ToolUse, ContentFilter, Refusal,
    Incomplete, Error,
    /// provider 新增的、我们不认识的 stop_reason
    Other,
}
```

`raw_reason` 与 `FinishReason::Other` 的组合是 G-O3 的体现：即使 Anthropic 明天新增 `stop_reason: "model_context_window_exceeded"`，我们也会：映射为 `Other`、保留原字符串、不报错、指标 `argus_unknown_finish_reason_total{reason="..."}` 上报，运维看到指标后可以在下个版本加映射。

---

## 5. 跨协议映射表

这张表是 `argus-protocol` 的实现规格，也是 golden test 的依据。

### 5.1 OpenAI Chat Completions → IR

Chat Completions 没有显式的 block 概念，需要**合成坐标**：

| 内容 | `BlockCoord` 合成规则 |
|------|---------------------|
| `choices[i].delta.content` | `(choice=i, item=0, part=0)` |
| `choices[i].delta.reasoning_content`（方言） | `(choice=i, item=1, part=0)` |
| `choices[i].delta.refusal` | `(choice=i, item=2, part=0)` |
| `choices[i].delta.tool_calls[j]` | `(choice=i, item=16+j, part=0)` |

`item` 的 0/1/2 是保留槽位，工具从 16 开始，留出扩展空间。

| 线上事件 | IR 事件 |
|---|---|
| 首个 chunk（含 `id`/`model`/`delta.role`） | `ResponseStart { provider_response_id: id, model, role }` |
| `delta.content` 首次非空 | `BlockStart { (i,0,0), Text }` + `BlockDelta{ Text }` |
| `delta.content` 后续 | `BlockDelta { (i,0,0), Text }` |
| `delta.reasoning_content` | `BlockStart/Delta { (i,1,0), Reasoning{summary:false} }` |
| `delta.refusal` | `BlockStart/Delta { (i,2,0), Refusal }` |
| `delta.tool_calls[j]` 首次出现 | `BlockStart { (i,16+j,0), ToolUse{ id, name } }` |
| `delta.tool_calls[j].function.arguments` | `BlockDelta { (i,16+j,0), Json }` |
| `delta.tool_calls[j].function.name`（分片，坑 C3） | 更新 block 的 `name`（append），不产生新事件 |
| `finish_reason != null` | 隐式 `BlockStop` 所有该 choice 的 open block + `Finish` |
| `choices: []` + `usage` | `Usage` |
| `data: [DONE]` | 无 IR 事件（framer 层消化） |

**注意**：Chat Completions 没有 `content_block_stop`。Accumulator 在收到 `finish_reason` 时才隐式关闭所有 block——这意味着 tool arguments 的 JSON 只能在此刻 parse。

`finish_reason` 映射：

| OpenAI | `FinishReason` |
|---|---|
| `stop` | `Stop` |
| `length` | `Length` |
| `tool_calls` / `function_call` | `ToolUse` |
| `content_filter` | `ContentFilter` |
| 其他 | `Other` + `raw_reason` |

### 5.2 OpenAI Responses → IR

Responses 的坐标天然是二维，直接映射：

```text
output_index  → BlockCoord.item
content_index → BlockCoord.part
choice        → 恒为 0（Responses 无 n>1）
```

| 线上事件 | IR 事件 |
|---|---|
| `response.created` | `ResponseStart { id: response.id, model }` |
| `response.in_progress` | 忽略（不产生事件） |
| `response.output_item.added`，`item.type = "message"` | 记录 item 类型，暂不产生 BlockStart |
| `response.output_item.added`，`item.type = "function_call"` | `BlockStart { (0,oi,0), ToolUse{ id: item.call_id, name: item.name } }` |
| `response.output_item.added`，`item.type = "reasoning"` | `BlockStart { (0,oi,0), Reasoning{summary:true} }` |
| `response.content_part.added` | `BlockStart { (0,oi,ci), Text/Refusal }` 依 `part.type` |
| `response.output_text.delta` | `BlockDelta { (0,oi,ci), Text }` |
| `response.refusal.delta` | `BlockDelta { (0,oi,ci), Text }`（block kind 为 Refusal） |
| `response.function_call_arguments.delta` | `BlockDelta { (0,oi,0), Json }` |
| `response.reasoning_summary_text.delta` | `BlockDelta { (0,oi,pi), Reasoning }` |
| `response.content_part.done` | `BlockStop { (0,oi,ci) }` |
| `response.output_item.done` | `BlockStop { (0,oi,*) }`（关闭该 item 下所有 part） |
| `response.completed` | `Usage`（从 `response.usage`）+ `Finish { Stop }` |
| `response.incomplete` | `Finish { Incomplete, raw_reason }` |
| `response.failed` | `ProviderError` + `Finish { Error }` |
| `error` | `ProviderError` |
| 其他 `response.*` | `Unknown { event_name }` |

**`response.completed` 的特殊价值**：它携带完整的最终响应对象。Accumulator 用它做**交叉校验**（见 §7.4）——如果我们累积出的文本与 `response.output[].content[].text` 不一致，说明解析有 bug，记录 `argus_accumulator_mismatch_total`。这是一个免费的自检机制，其他两个协议没有。

### 5.3 Anthropic Messages → IR

最直接的映射，因为 IR 的 block 模型本就借鉴自 Anthropic：

```text
choice → 恒为 0
item   → content block index
part   → 恒为 0
```

| 线上事件 | IR 事件 |
|---|---|
| `message_start` | `ResponseStart { id, model, role }` + `Usage`（input_tokens 等） |
| `content_block_start`，`type=text` | `BlockStart { (0,i,0), Text }` |
| `content_block_start`，`type=tool_use` | `BlockStart { (0,i,0), ToolUse{ id, name } }` |
| `content_block_start`，`type=thinking` | `BlockStart { (0,i,0), Reasoning{summary:false, redacted:false} }` |
| `content_block_start`，`type=redacted_thinking` | `BlockStart { (0,i,0), Reasoning{redacted:true} }` |
| `content_block_start`，未知 type | `BlockStart { (0,i,0), Other(type) }` |
| `content_block_delta.text_delta` | `BlockDelta { Text }` |
| `content_block_delta.input_json_delta` | `BlockDelta { Json }`（`partial_json`） |
| `content_block_delta.thinking_delta` | `BlockDelta { Reasoning }` |
| `content_block_delta.signature_delta` | `BlockDelta { Signature }` |
| `content_block_delta.citations_delta` | `BlockDelta { Citation }` |
| `content_block_delta`，未知 delta type | `BlockDelta { Opaque{ delta_type, raw } }` |
| `content_block_stop` | `BlockStop { (0,i,0) }` ← **可以 parse JSON 的时刻** |
| `message_delta` | `Usage`（output_tokens）+ `Finish{ stop_reason }` |
| `message_stop` | 无事件（流结束标记） |
| `ping` | `Heartbeat` |
| `error` | `ProviderError` |
| 未知 event | `Unknown { event_name }` |

`stop_reason` 映射：

| Anthropic | `FinishReason` |
|---|---|
| `end_turn` | `Stop` |
| `max_tokens` | `Length` |
| `tool_use` | `ToolUse` |
| `stop_sequence` | `Stop` |
| `refusal` | `Refusal` |
| 其他 | `Other` + `raw_reason` |

### 5.4 非流式响应

非流式响应用**同一套 IR**，由一个"合成器"把完整 JSON 展开成等价的事件序列：

```text
非流式 JSON → SyntheticEventGen → 与流式完全相同的 ObservedEvent 序列 → 同一个 Accumulator
```

好处：Accumulator 只有一套代码路径，非流式与流式的审计结果结构完全一致，golden test 可以交叉验证（同一个请求的流式与非流式结果应该产出相同的 `ObservedInteraction`，除时间戳外）。

---

## 6. Usage 合并语义

三种协议的 usage 时序完全不同，必须明确合并规则。

| 协议 | input_tokens | output_tokens | cache tokens | 时机 |
|---|---|---|---|---|
| Anthropic | `message_start` | `message_delta`（最终值） | `message_start` | 两次 |
| OpenAI Chat | 最后一个 chunk（需 `include_usage`） | 同左 | 同左（`prompt_tokens_details`） | 一次 |
| OpenAI Responses | `response.completed` | 同左 | 同左 | 一次 |

**统一规则：last-non-null wins，逐字段独立。**

```rust
impl UsageAccumulator {
    fn apply(&mut self, d: &UsageDelta) {
        // 每个字段独立：新值非 None 就覆盖，None 保持原值
        if let Some(v) = d.input_tokens  { self.input_tokens  = Some(v); }
        if let Some(v) = d.output_tokens { self.output_tokens = Some(v); }
        if let Some(v) = d.cache_creation_input_tokens { self.cache_creation = Some(v); }
        if let Some(v) = d.cache_read_input_tokens     { self.cache_read     = Some(v); }
        if let Some(v) = d.reasoning_tokens { self.reasoning = Some(v); }
        self.observed_count += 1;
    }
}
```

**为什么不是求和？** Anthropic 的 `message_delta.usage.output_tokens` 是**累积值**不是增量。求和会得到约 2 倍的错误结果。这是一个容易出错的地方，golden test 必须覆盖。

**当 usage 完全缺失时**（provider 不支持、或 `include_usage` 未开、或流被中断）：
- `usage.source = Missing`
- Accumulator 提供一个 `estimated_output_tokens`（按累积文本长度粗估），**明确标记为估算**，供成本兜底用。
- 绝不把估算值写进 `usage` 的真实字段——计费必须能区分实测与估算。

---

## 7. Accumulator

### 7.1 状态机

```rust
pub struct Accumulator {
    outcome: Option<Outcome>,
    integrity: Integrity,
    response: ResponseMeta,
    /// 用 BTreeMap 保证按坐标有序，便于最终按顺序输出 block
    blocks: BTreeMap<BlockCoord, BlockState>,
    usage: UsageAccumulator,
    finish: Option<Finish>,
    unknown_events: SmallVec<[UnknownRecord; 4]>,
    provider_errors: SmallVec<[ProviderErrorRecord; 2]>,
    stats: StreamStats,
}

/// 流健康度的量化记录。落库到 request_payloads.stats，
/// 是「观测数据可信度」与「provider 规范程度」的唯一依据。
pub struct StreamStats {
    pub frames_seen: u32,
    pub events_emitted: u32,
    pub bytes_seen: u64,
    pub dropped_bytes: u64,
    pub gap_count: u32,
    pub heartbeats: u32,
    /// §7.3 的全部异常，每项含 BlockCoord 与 seq
    pub anomalies: SmallVec<[Anomaly; 4]>,
}

pub enum Anomaly {
    ImplicitStart { coord: BlockCoord, at: u32 },
    DuplicateStart { coord: BlockCoord, at: u32 },
    OrphanStop { coord: BlockCoord, at: u32 },
    DeltaAfterStop { coord: BlockCoord, at: u32 },
    EventAfterFinish { at: u32 },
    DuplicateFinish { at: u32 },
    /// 仅 OpenAI Responses：与 response.completed 交叉校验不一致（§7.4）
    TextMismatch { coord: BlockCoord },
    MissingBlock { coord: BlockCoord },
}

pub struct BlockState {
    kind: BlockKind,
    /// 文本 / reasoning 的累积缓冲
    text: String,
    /// tool arguments 的原始 JSON 缓冲（**从不在流中 parse**）
    json_raw: String,
    signature: Option<String>,
    opened_at: u32,        // seq
    closed_at: Option<u32>,
    truncated: bool,
    /// 该 block 是否经历过 Gap
    degraded: bool,
}
```

### 7.2 JSON 何时可以解析 —— 明确规则

这是本文档最需要明确的一条规则。

> **只在 `BlockStop` 或 `Finish` 时尝试解析 tool arguments。流进行中，`json_raw` 只是一个 `String`，绝不调用 `serde_json`。**

原因：`input_json_delta` 的片段可以在任意字节处切断，包括：
- 一个 UTF-8 字符的中间
- 一个字符串字面量的中间（`{"loc` + `ation":"SF"}`）
- 一个转义序列的中间（`\u00` + `e9`）

中途 parse 必然失败，而"失败了就重试"会浪费大量 CPU（每个 delta 一次失败的 parse）。

解析时机与结果：

| 触发 | 动作 |
|------|------|
| `BlockStop`（Anthropic / Responses） | `parse(json_raw)` → 成功则填 `arguments_json`，失败则 `arguments_parse_error` |
| `Finish`（OpenAI Chat，无显式 stop） | 关闭所有 open block，同上 |
| 流中断（无 Finish） | **不解析**，保留 `json_raw`，标记 `incomplete = true` |

```rust
fn close_tool_block(&mut self, st: &mut BlockState) {
    let raw = st.json_raw.trim();
    st.arguments = match raw {
        // 坑 A4：无参数工具的 partial_json 拼接结果是空字符串
        "" => ToolArguments::Empty,
        s  => match serde_json::from_str::<Box<RawValue>>(s) {
            Ok(v)  => ToolArguments::Parsed(v),
            Err(e) => ToolArguments::Invalid { raw: st.json_raw.clone(), error: e.to_string() },
        },
    };
}
```

用 `Box<RawValue>` 而不是 `Value`：我们只需要验证它是合法 JSON 并原样存储，不需要遍历结构。这避免了一次完整的 DOM 构建。（安全检测需要遍历时，由 `12-security.md` 在自己的路径里做。）

**绝不尝试"修复"截断的 JSON**（补齐括号、补引号）。修复出来的参数可能与模型真实意图不同，写进审计就是伪造数据。宁可留一个明确的 `Invalid`。

### 7.3 乱序与异常事件的处理

真实世界的流不会总是规范的。Accumulator 必须容忍：

| 异常 | 处理 |
|------|------|
| `BlockDelta` 先于 `BlockStart` 到达 | 隐式创建 block，`kind` 从 delta 类型推断，标记 `implicit_start = true` |
| 同一个 `BlockCoord` 被 `BlockStart` 两次 | 第二次视为新 block，旧的强制关闭，记录 `duplicate_start` |
| `BlockStop` 对应的 block 不存在 | 忽略，记录 `orphan_stop` |
| `BlockStop` 之后又来 `BlockDelta` | 重新打开该 block（append），记录 `delta_after_stop` |
| `Finish` 之后又来事件 | 继续处理（不丢弃），记录 `event_after_finish` |
| 多个 `Finish` | 第一个为准，后续记录 `duplicate_finish` |
| `Gap` | 所有 open block 置 `degraded = true`；integrity 降级 |

所有这些异常都累加到 `StreamStats.anomalies`，落库后可以统计「哪个 provider 的流最不规范」——这本身就是有价值的运维信息。

**原则：永不因为异常事件而中止累积。** 一个畸形的流仍然应该产出尽可能多的信息。

### 7.4 交叉校验（仅 OpenAI Responses）

`response.completed` 携带完整最终对象，用它自检：

```rust
fn cross_validate(&mut self, final_response: &RawValue) {
    for (coord, expected_text) in extract_texts(final_response) {
        match self.blocks.get(&coord) {
            Some(st) if st.text == expected_text => {}   // ✅
            Some(st) if st.truncated => {}               // 已知截断，跳过
            Some(_) => self.stats.anomalies.push(Anomaly::TextMismatch { coord }),
            None    => self.stats.anomalies.push(Anomaly::MissingBlock { coord }),
        }
    }
}
```

发现不一致时**不修正**累积结果（那会掩盖 bug），而是记录 anomaly 并上报指标。这是发现 parser 回归的最佳信号。

### 7.5 最终产物

```rust
pub struct ObservedInteraction {
    pub trace: TraceContext,
    pub outcome: Outcome,
    pub integrity: Integrity,

    // 请求侧
    pub request: ObservedRequest,

    // 响应侧
    pub response_meta: ResponseMeta,
    pub blocks: Vec<ObservedBlock>,        // 按 BlockCoord 有序
    pub usage: ResolvedUsage,
    pub finish: Option<Finish>,

    // 保真
    pub raw_request: Option<Bytes>,
    pub raw_events: Option<RawEventLog>,   // 见 §8
    pub provider_errors: Vec<ProviderErrorRecord>,
    pub unknown_events: Vec<UnknownRecord>,

    // 诊断
    pub stats: StreamStats,
    pub timings: Timings,
}

pub enum Outcome {
    Completed,
    ClientAborted { at_seq: u32, bytes_forwarded: u64 },
    UpstreamError { kind: CompactString },
    Cancelled { reason: CancelReason },
    ObserverTimeout,
    ParserFailed { at_seq: u32 },
}

pub enum Integrity {
    /// 观测完整
    Complete,
    /// 有字节丢失或截断，附带定量信息
    Degraded { dropped_bytes: u64, gap_count: u32, truncated_blocks: u32 },
    /// 只有元数据，没有内容（ObserveMode::Lite / Off）
    MetadataOnly,
}

pub struct Timings {
    pub queued_micros: u32,        // 入口到发出上游请求
    pub upstream_header_micros: u32, // 发出请求到收到 header（含模型排队）
    pub ttft_micros: Option<u32>,  // 到第一个内容 delta
    pub total_micros: u32,
    pub proxy_overhead_micros: u32, // Argus 自身开销（见 11-observability §5）
}
```

`Integrity` 与 `Outcome` 是**两个正交维度**：一个流可以是 `Completed` 但 `Degraded`（正常结束但中间丢了字节），也可以是 `ClientAborted` 但 `Complete`（中断前观测到的部分是完整的）。混为一谈会让下游分析出错。

---

## 8. Raw 保真策略

G-O2 要求可追溯，但保存全部原始字节代价高（一个长响应的 SSE 原文可能是最终文本的 3–5 倍，因为每个 delta 都有 JSON 包装）。

分级策略：

| `ObserveMode` | raw_request | raw_events |
|---|---|---|
| `Full` | ✅ 完整保存 | ✅ 按 §8.1 策略 |
| `Lite` | ✅ 保存（请求通常不大） | ❌ 只保存 unknown / error 事件的 raw |
| `Off` | ❌ 只存 hash | ❌ |

### 8.1 raw_events 的保存策略

```toml
[observation.raw]
# 默认：只保存"值得保存"的原始事件
mode = "selective"   # full | selective | none

# selective 模式保存：
#  - 所有 Unknown 事件（前向兼容诊断的唯一依据）
#  - 所有 ProviderError 事件
#  - ResponseStart / Finish / Usage 事件（结构性，量小）
#  - 触发安全规则的事件
#  - 前 N 个和后 N 个事件（诊断流开头/结尾问题）
selective_head = 8
selective_tail = 8
```

`selective` 是默认值，因为：
- 内容本身已经在 `blocks` 里以更紧凑的形式保存了（去掉了 JSON 包装）。
- 真正需要 raw 的场景是「解析出问题了」，而这些场景的信号是 Unknown / anomaly / error。
- 存储量减少约 80%。

`full` 模式供调试与合规场景使用，按 workspace 可配置。

---

## 9. Conversation 关联

三个 ID 的语义（`00-overview.md` 术语表）：

| ID | 生成者 | 生命周期 | 用途 |
|---|---|---|---|
| `request_id` | Argus | 单个 HTTP 请求 | 定位一次调用 |
| `trace_id` | 客户端（`traceparent`）或 Argus 生成 | 一次端到端操作 | 关联重试、fallback、多次上游尝试 |
| `conversation_id` | 见下 | 一次多轮对话 | 会话级审计与安全 |

**`conversation_id` 的确定顺序**：

1. 客户端显式提供 `x-argus-conversation-id` header → 直接用。（推荐方式）
2. OpenAI Responses 的 `previous_response_id` → 查 `requests` 表找到上一次的 conversation_id。
3. **消息前缀指纹**：对请求的 `messages[0..n-1]`（去掉最后一条）做规范化哈希，与最近 N 小时内的记录匹配。

方案 3 是启发式的，会有误判（不同用户发了完全相同的开头）。因此：
- 指纹加上 `workspace_id` 与 `api_key_id` 作为盐，跨租户绝不会碰撞。
- 匹配结果标记 `conversation_link = Inferred`，与 `Explicit` 区分。
- 安全策略若基于会话历史做判断，**只信任 `Explicit`**（见 `12-security.md` §8）。

---

## 10. Failure Modes

| 场景 | Accumulator 行为 | 落库结果 |
|------|-----------------|---------|
| 流在 `BlockStart` 后中断 | block 保留已有 text，`closed_at = None` | `Outcome::ClientAborted`，block `incomplete=true` |
| tool arguments 不完整 | 不 parse，保留 `json_raw` | `arguments = Incomplete { raw }` |
| tool arguments 是坏 JSON（完整但非法） | parse 失败 | `arguments = Invalid { raw, error }` |
| 全程无 usage | `usage.source = Missing` + 估算值 | 计费用估算并标记 |
| Gap 发生在 tool arguments 中间 | block `degraded=true`，close 时 parse 大概率失败 → `Invalid` | 明确可识别 |
| 未知 event 占比很高 | 正常累积其他部分 | `unknown_events` 非空，触发告警 |
| parser panic | Observer 捕获，Accumulator 用当前状态 finish | `Outcome::ParserFailed { at_seq }` |
| `response.completed` 与累积不一致 | 记 anomaly，**不修正** | `stats.anomalies` 非空 |

---

## 11. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| 三级 `BlockCoord` | 一套模型覆盖三种协议 | Chat Completions 需要合成坐标（有点人为） |
| ToolUse 建模为 block | 与 Anthropic 天然一致 | OpenAI 用户看审计时坐标不直观（用 `item-16` 换算） |
| 只在 BlockStop 解析 JSON | 零无效 parse | 中断的流拿不到结构化参数（但保留 raw） |
| 不修复截断 JSON | 审计数据可信 | 部分场景下分析要多做一步 |
| `RawValue` 而非 `Value` | 少一次 DOM 构建 | 安全模块需要自己遍历 |
| `selective` raw 默认 | 存储省 80% | 极少数场景需要事后翻 raw 时可能没有 |
| Integrity 与 Outcome 分离 | 语义清晰 | 多一个字段，下游要理解两个维度 |
| `last-non-null wins` usage | 三协议统一 | 若将来出现真·增量 usage 的协议要加分支 |

---

## 12. Alternatives Considered

**A. 用 `serde_json::Value` 作为通用 IR。**
最灵活，但：无类型安全、每事件 O(字段) 次分配、下游要写大量 `.get("x").and_then(...)`。**否决**。

**B. 每个协议一套独立的 IR，不做归一化。**
安全模块、统计模块要各写三遍。**否决**——归一化的价值正在于此。

**C. IR 采用一维 block index（照抄 Anthropic）。**
Responses 的二维坐标会被压平，`output_index` 与 `content_index` 的关系丢失。**否决**。

**D. 流式解析 tool arguments（增量 JSON parser）。**
可以在流进行中就知道参数结构，对实时安全检测有价值。但：需要一个支持"部分输入"的 JSON parser（如 `struson` 风格），复杂度高，且部分解析的结果本身可能误导。**推迟到 Phase 3**，作为 `gate_tools` 安全模式的可选增强。当前 `gate_tools` 在 BlockStop 时检查已经足够（见 `12-security.md` §6.2）。

**E. 把 usage 估算写进真实字段。**
会让计费数据不可区分。**强烈否决**。

---

## 13. Decision

1. IR 由 `ObservedEvent { seq, at_micros, kind, raw }` 构成，`kind` 是 `#[non_exhaustive]` 枚举。
2. 统一坐标 `BlockCoord { choice: u8, item: u16, part: u16 }`，是三协议坐标系的最小公共超集。
3. ToolUse 建模为 `BlockKind::ToolUse` + `BlockDelta::Json`，不设独立事件类型。
4. 未知 event / 未知 delta type / 未知 finish reason 都有专门容纳位（`Unknown` / `Opaque` / `Other + raw_reason`）。
5. **tool arguments 只在 `BlockStop` / `Finish` 时解析，绝不中途 parse，绝不修复截断的 JSON。**
6. usage 合并统一用 **last-non-null wins，逐字段独立**；缺失时提供明确标记的估算值。
7. `Outcome` 与 `Integrity` 是两个正交维度，都必须落库。
8. Accumulator 对乱序/重复/孤儿事件全部容忍并计数，永不中止累积。
9. 非流式响应经 `SyntheticEventGen` 走同一套 IR 与 Accumulator。
10. raw 保存默认 `selective`（unknown / error / 结构性 / 首尾各 8 条）。
11. `conversation_id` 优先显式 header，其次 `previous_response_id`，最后带租户盐的前缀指纹（标记为 `Inferred`）。

---

## 14. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | `BlockCoord.item` 用 16 作为 OpenAI tool 起始槽位是否够？（一次 100 个并行 tool call 会溢出到 116，没问题；但若 OpenAI 增加更多 delta 通道呢） | u16 空间充足 | 出现冲突时 |
| OQ-2 | 是否需要在 IR 层表达"引用/citation"的完整结构而不是 `Opaque`？ | 当前 `Citation(Bytes)` | Anthropic citations 被广泛使用时 |
| OQ-3 | Responses API 的 `store=true` 让会话状态存在 OpenAI 侧，Argus 只能看到增量 input。会话级安全检测如何覆盖？ | 未解决 | Phase 2，可能需要要求客户端关闭 store |
| OQ-4 | `selective` raw 的 head/tail = 8 是否合适？ | 拍的值 | 有真实诊断案例后 |
| OQ-5 | 是否要为 `estimated_output_tokens` 引入真正的 tokenizer（tiktoken/tokenizers）？体积与 CPU 代价如何？ | 当前按字节粗估 | 计费精度成为问题时 |
