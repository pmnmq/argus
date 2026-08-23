# F05 · Anthropic 解析器（最小）

| | |
|---|---|
| ID | F05 |
| 里程碑 | M0 |
| 依赖 | F03, F04 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/09-observers.md` §5.3；`docs/11-observation-model.md` §5.3 |
| 宪法条款 | C2, C7, C21, C24, C26 |

## 1. 目标

把 Anthropic Messages 的 SSE 帧映射成 `ObservedEvent`。本切片只做**文本路径**（text block + usage + finish + unknown），足以驱动 M0 的端到端验证。

## 2. 范围

**In scope**：`ProtocolObserver` trait、`AnthropicMessagesParser`、`message_start` / `content_block_start(text)` / `content_block_delta(text_delta)` / `content_block_stop` / `message_delta` / `message_stop` / `ping` / `error` / 未知 event。

**Out of scope**：`tool_use` / `input_json_delta` / `thinking` / `signature_delta` / `citations_delta`（→ F09），OpenAI 协议（→ F08）。

> 未知 event 的**处理**在本切片就要做对（宪法 C2），只是未映射的具体类型留给 F09。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供 `trait ProtocolObserver`，同步、无 `async`、无 `Box<dyn>` 热路径分发。
- **R2** WHEN 收到 `message_start` THE SYSTEM SHALL 产出 `ResponseStart` **与** 一个 `Usage`（input_tokens 等在此处给出，`docs/09` 坑 A5）。
- **R3** WHEN 收到 `content_block_start` 且 `type == "text"` THE SYSTEM SHALL 产出 `BlockStart { (0,i,0), Text }`。
- **R4** WHEN 收到 `content_block_delta` 且 `delta.type == "text_delta"` THE SYSTEM SHALL 产出 `BlockDelta { Text }`。
- **R5** WHEN 收到 `content_block_stop` THE SYSTEM SHALL 产出 `BlockStop`。
- **R6** WHEN 收到 `message_delta` THE SYSTEM SHALL 产出 `Usage`（output_tokens）**与** `Finish`。
- **R7** WHEN 收到 `message_stop` THE SYSTEM SHALL 不产出事件。
- **R8** WHEN 收到 `ping` THE SYSTEM SHALL 产出 `Heartbeat`，且**不影响任何状态**（坑 A1）。
- **R9** WHEN 收到 `error` 事件 THE SYSTEM SHALL 产出 `ProviderError` 且**不中止解析**（坑 A8）。
- **R10** WHEN 收到未知 event 名 THE SYSTEM SHALL 产出 `Unknown { event_name }` 并计数，**不返回错误**（宪法 C2，坑 A9）。
- **R11** WHEN 收到未知 `delta.type` THE SYSTEM SHALL 产出 `BlockDelta::Opaque { delta_type, raw }`。
- **R12** WHEN 收到未知 `content_block.type` THE SYSTEM SHALL 产出 `BlockKind::Other(type)`。
- **R13** WHEN `stop_reason` 不在已知集合 THE SYSTEM SHALL 映射为 `FinishReason::Other` 并保留 `raw_reason`（坑 A10）。
- **R14** THE SYSTEM SHALL 以 `data` 中的 `type` 字段为权威，`event:` 行仅作 fallback。
- **R15** THE SYSTEM SHALL 保留 `cache_creation_input_tokens` / `cache_read_input_tokens`（坑 A6）。
- **R16** IF 帧内容不是合法 JSON THEN THE SYSTEM SHALL 产出 `ParseFailure` 并**继续处理后续帧**，不 panic（宪法 C7）。
- **R17** THE SYSTEM SHALL 使用借用式反序列化，稳态每帧零堆分配（宪法 C21）。

## 4. 接口契约

```rust
// argus-protocol/src/observer/mod.rs
pub trait ProtocolObserver {
    /// 同步、不返回 Err —— 解析失败以 ParseFailure 事件表达（宪法 C2/C7）
    fn on_frame(&mut self, frame: FrameRef<'_>, at_micros: u32, out: &mut dyn EventSink);
    /// 旁路丢字节 → Gap 事件（seq 归解析器维护，调用方不能自己造）
    fn on_gap(&mut self, dropped_bytes: u64, at_micros: u32, out: &mut dyn EventSink);
    /// 流结束：关闭仍打开的 block（若协议需要）
    fn finish(&mut self, at_micros: u32, out: &mut dyn EventSink);
    fn kind(&self) -> ObserverKind;
    /// unknown_events / unknown_finish_reasons / parse_failures / anomalies
    fn stats(&self) -> ParserStats;
}

pub trait EventSink { fn emit(&mut self, ev: ObservedEvent); }

/// 把 Framer 的输出直接接到观测器上（一整块 chunk 共用一个 at_micros）
pub struct ObserverSink<'a, P: ProtocolObserver + ?Sized, S: EventSink> { .. }
impl<..> FrameSink for ObserverSink<'_, P, S> { .. }

/// 枚举分发，避免 Box<dyn> 与虚表（宪法 C21）
pub enum AnyObserver {
    AnthropicMessages(AnthropicMessagesParser),
    /// R04 的两个兜底，接进同一套事件流
    GenericSse(GenericSseFallback),
    GenericJson(GenericJsonFallback),
}
impl AnyObserver {
    pub fn for_kind(kind: ObserverKind, mode: ObserveMode) -> Self;
}
```

### 4.1 与本节初稿的三处偏离

初稿写在实现之前，下面三处按落地结果回填。

| 初稿 | 实际 | 为什么 |
|---|---|---|
| `parser/mod.rs` | `observer/mod.rs` | `docs/19` §5.3 早就把这层定在 `observer/`，而 `generic.rs`（R04）已经在那儿了。以 docs 为准（宪法 C31） |
| `on_frame(frame, out)` | 多一个 `at_micros` 参数 | §5 要求「parser 不碰时钟」。做成参数而不是「解析器填 0、sink 事后盖章」，是因为后者忘了盖也编译得过 |
| `for_kind(k, dialect: Arc<Dialect>)` | `for_kind(k, mode: ObserveMode)` | `Dialect` 是 F17 的类型，现在还不存在。`ObserveMode` 才是这一层真正要的东西 —— 它决定 `RawRef` 留不留 |

`AnyObserver` 里没有 `OpenAiChat` / `OpenAiResponses` 变体：造两个什么都不做的
空解析器，等于让「观测正常」和「观测什么都没解出来」看起来一样。在 F08 填上
之前，这两个 `ObserverKind` 一律落到 `GenericSse` 兜底，`kind()` 如实返回
`GenericSse` —— 报告一个没在跑的观测器比报告降级更误导人。

映射规则表见 `docs/11` §5.3，**逐行实现，不得增删**。

## 5. 实现要点

- 用 `#[derive(Deserialize)]` 定义**帧特定的窄结构体**，而不是通用 `Value`：
  ```rust
  #[derive(Deserialize)]
  struct Envelope<'a> {
      #[serde(borrow, rename = "type")] ty: Option<Str<'a>>,
      // 其余字段按 type 分支用 serde_json::from_slice 二次解析对应结构
  }
  ```
  两阶段解析（先取 `type`，再按类型解析）比一个大 enum 更省，且未知 type 天然走 R10。
- **`#[serde(borrow)]` 对 `Cow<'a, str>` 不起作用**（实现时踩到的）：serde 自带的
  `Cow` 反序列化**永远**产出 `Owned`，`borrow` 属性只加生命周期约束、不改行为。
  照初稿写会让每个 text delta 都多一次堆分配，而 delta 是每 token 一个的 —— R17
  当场破功。落地的做法是手写一个 `Str<'a>(Cow<'a, str>)`：visitor 的
  `visit_borrowed_str` 直接借底层缓冲，只有含转义序列时才落到 `visit_str` 去解码。
  用 `&'a str` 也不行 —— 含转义的字段借不出来，整帧会变成解析失败。
- 文本片段用 `TextChunk::Shared(Bytes)` 与原始帧共享同一块内存，切片靠地址区间
  判断而不是 `Bytes::slice_ref`（后者不匹配时 panic，本 crate 禁 panic）。
- `event:` 行与 `data.type` 不一致时以后者为准（R14），但记一条 anomaly。
- `seq` 由 parser 维护并单调递增；`at_micros` 由调用方（Observer）注入，parser 不碰时钟（保持纯函数、可测）。
- **未知 event 的 `raw` 必须保留**——它是前向兼容诊断的唯一依据（`docs/11` §8.1）。
- usage 出现未映射字段时把**整帧**塞进 `UsageDelta.extra`：切子对象要再扫一遍，
  而整帧是现成且零拷贝的，诊断时信息还更全（宪法 C2）。

## 6. 任务

- [x] T1 `observer/mod.rs`：`ProtocolObserver` / `EventSink` / `AnyObserver` 枚举分发
- [x] T2 `observer/anthropic.rs`：帧类型分发（两阶段解析）
- [x] T3 `message_start` → `ResponseStart` + `Usage`
- [x] T4 `content_block_start/delta/stop`（仅 text 路径 + 未知 type 兜底）
- [x] T5 `message_delta` → `Usage` + `Finish`；`stop_reason` 映射表（含 `Other`）
- [x] T6 `ping` / `error` / 未知 event / 非法 JSON 的处理
- [x] T7 计数：`ParserStats`（`unknown_events` / `unknown_finish_reasons` / `parse_failures` / `anomalies` / `frames` / `events`），供 F06 接指标
- [x] T8 单元测试（32 条）+ 切分矩阵（`tests/anthropic_split.rs`，10 条语料 × 41 种切分）+ bench

## 7. 验收

```bash
cargo test -p argus-protocol anthropic
cargo bench -p argus-protocol --bench anthropic
```

### 7.1 首批 fixture 为什么没有落地

本节初稿要求把六个录制的 fixture 放进 `tests/fixtures/anthropic_messages/`。
**做不到，而且不该硬做**：宪法 C25 规定 fixture 只能来自录制（Argus 自录、
SDK 录制、生产导出脱敏），而三条途径的工具都是 F10 R6 的交付物，现在一条都
不存在。

手写字节塞进 `tests/fixtures/` 是最坏的选项：混进去之后就**再也分不清**哪些
是真实录制、哪些是想象出来的，而 C25 存在的全部意义就是这个区分。所以
`tests/fixtures/` 保持为空，六个 fixture 目录随录制工具一起交给 F10（见 §8）。

本切片的验收改用**合成语料**，位置与命名都刻意与 fixture 分开：
`src/observer/anthropic.rs` 的单元测试与 `tests/anthropic_split.rs` 的语料表，
两处都在文件头写明「这是合成的，不是 fixture」。这与 F03 的做法一致
（`tests/split.rs` 的 `CORPUS` 用 `*_like` 命名）。

覆盖对应关系：

| 初稿要的 fixture | 覆盖它的用例 |
|---|---|
| `001_simple_stream`（R2–R7） | `simple_stream` 语料 + `on_frame_a_simple_text_stream_produces_the_documented_event_sequence` |
| `009_ping_interleaved`（坑 A1） | `ping_interleaved` 语料 + `on_frame_ping_interleaved_everywhere_changes_nothing_but_heartbeats` |
| `010_usage_split`（坑 A5/A6） | `usage_split_with_cache_tokens` 语料 + `on_frame_usage_split_across_two_frames_is_merged_not_summed` |
| `012_unknown_event`（坑 A9） | `unknown_event` 语料 + `on_frame_unknown_event_name_emits_unknown_and_keeps_the_raw_frame` |
| `014_unknown_stop_reason`（坑 A10） | `unknown_stop_reason` 语料 + `map_stop_reason_over_the_documented_table_matches_docs_11` |
| `015_error_mid_stream`（坑 A8） | `error_mid_stream` 语料 + `on_frame_error_in_the_middle_emits_provider_error_and_keeps_parsing` |

### 7.2 验收清单

- [x] 每条语料在 `SplitStrategy` 全矩阵（41 种，含 `ByteByByte`）下产出**完全相同**的事件序列（宪法 C24）
- [x] 基线序列由一张独立的期望表钉住，而不是拿被测代码生成
- [x] 未知 event 不导致任何 `Err`，且 `raw` 在全部切分策略下都被保留
- [x] 非法 JSON 帧 → `ParseFailure` 事件，后续帧仍被正确解析
- [x] `event:` 与 `data.type` 冲突时以 `data.type` 为准并记 anomaly
- [x] bench：单帧解析 **350 ns**（预算 3 µs），无转义的 delta 与原始帧共享 `Bytes`
- [x] 「零分配」由类型钉住：`TextChunk::is_shared()` + 地址落在原始帧区间内

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| `tool_use` / `input_json_delta` | F09（本切片兜底成 `BlockKind::Other` / `BlockDelta::Opaque`，不丢数据） |
| `thinking` / `redacted_thinking` / `signature_delta` | F09（同上） |
| `citations_delta` | F09（同上） |
| OpenAI Chat / Responses | F08（在此之前落 `GenericSse` 兜底） |
| 映射表全覆盖检查（`coverage_matrix`） | F10 |
| **六个录制 fixture + 录制工具** | F10 R6（见 §7.1：录制工具不存在，手写 fixture 会永久污染语料来源） |
| `Dialect` 传进解析器 | F17（`for_kind` 现在只收 `ObserveMode`） |
| `ParserStats` → Prometheus 指标 | F06 |
| `catch_unwind` 隔离与 `parser_dead` | F06 |
| 非流式 body → 等价事件序列（`SyntheticEventGen`） | F09（`GenericJsonFallback::observe_body` 现在只记形状与顶层 key，不产出事件） |
| bench 的回归门禁（退化 > 20% 失败） | F28 |
