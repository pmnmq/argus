# F05 · Anthropic 解析器（最小）

| | |
|---|---|
| ID | F05 |
| 里程碑 | M0 |
| 依赖 | F03, F04 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/04-protocols.md` §5.3；`docs/06-observation-model.md` §5.3 |
| 宪法条款 | C2, C7, C21, C24, C26 |

## 1. 目标

把 Anthropic Messages 的 SSE 帧映射成 `ObservedEvent`。本切片只做**文本路径**（text block + usage + finish + unknown），足以驱动 M0 的端到端验证。

## 2. 范围

**In scope**：`ProtocolParser` trait、`AnthropicMessagesParser`、`message_start` / `content_block_start(text)` / `content_block_delta(text_delta)` / `content_block_stop` / `message_delta` / `message_stop` / `ping` / `error` / 未知 event。

**Out of scope**：`tool_use` / `input_json_delta` / `thinking` / `signature_delta` / `citations_delta`（→ F09），OpenAI 协议（→ F08）。

> 未知 event 的**处理**在本切片就要做对（宪法 C2），只是未映射的具体类型留给 F09。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供 `trait ProtocolParser`，同步、无 `async`、无 `Box<dyn>` 热路径分发。
- **R2** WHEN 收到 `message_start` THE SYSTEM SHALL 产出 `ResponseStart` **与** 一个 `Usage`（input_tokens 等在此处给出，`docs/04` 坑 A5）。
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
// argus-protocol/src/parser/mod.rs
pub trait ProtocolParser {
    /// 同步、不返回 Err —— 解析失败以 ParseFailure 事件表达（宪法 C2/C7）
    fn on_frame(&mut self, frame: FrameRef<'_>, out: &mut dyn EventSink);
    /// 流结束：关闭仍打开的 block（若协议需要）
    fn finish(&mut self, out: &mut dyn EventSink);
    fn protocol(&self) -> Protocol;
}

pub trait EventSink { fn emit(&mut self, ev: ObservedEvent); }

/// 枚举分发，避免 Box<dyn> 与虚表（宪法 C21）
pub enum AnyParser {
    AnthropicMessages(AnthropicMessagesParser),
    OpenAiChat(OpenAiChatParser),          // F08 填充
    OpenAiResponses(OpenAiResponsesParser),// F08 填充
}
impl AnyParser {
    pub fn for_protocol(p: Protocol, dialect: Arc<Dialect>) -> Self;
}
```

映射规则表见 `docs/06` §5.3，**逐行实现，不得增删**。

## 5. 实现要点

- 用 `#[derive(Deserialize)]` + `#[serde(borrow)]` 定义**帧特定的窄结构体**，而不是通用 `Value`：
  ```rust
  #[derive(Deserialize)]
  struct AnthropicFrame<'a> {
      #[serde(borrow)] r#type: &'a str,
      // 其余字段按 type 分支用 serde_json::from_slice 二次解析对应结构
  }
  ```
  两阶段解析（先取 `type`，再按类型解析）比一个大 enum 更省，且未知 type 天然走 R10。
- `event:` 行与 `data.type` 不一致时以后者为准（R14），但记一条 anomaly。
- `seq` 由 parser 维护并单调递增；`at_micros` 由调用方（Observer）注入，parser 不碰时钟（保持纯函数、可测）。
- **未知 event 的 `raw` 必须保留**——它是前向兼容诊断的唯一依据（`docs/06` §8.1）。

## 6. 任务

- [ ] T1 `parser/mod.rs`：`ProtocolParser` / `EventSink` / `AnyParser` 枚举分发
- [ ] T2 `parser/anthropic.rs`：帧类型分发（两阶段解析）
- [ ] T3 `message_start` → `ResponseStart` + `Usage`
- [ ] T4 `content_block_start/delta/stop`（仅 text 路径 + 未知 type 兜底）
- [ ] T5 `message_delta` → `Usage` + `Finish`；`stop_reason` 映射表（含 `Other`）
- [ ] T6 `ping` / `error` / 未知 event / 非法 JSON 的处理
- [ ] T7 计数：`unknown_event` / `unknown_finish_reason` / `parse_failure`（供 F06 接指标）
- [ ] T8 单元测试 + 首批 fixture（≥ 6 个，见 §7）

## 7. 验收

```bash
cargo test -p argus-protocol anthropic
```

**首批 fixture**（放 `tests/fixtures/anthropic_messages/`，来源必须是录制，宪法 C25）：

| 目录 | 覆盖 |
|---|---|
| `001_simple_stream` | R2–R7 基础路径 |
| `009_ping_interleaved` | R8（坑 A1） |
| `010_usage_split` | R2+R6+R15（坑 A5/A6）——**必须断言不求和** |
| `012_unknown_event` | R10（坑 A9） |
| `014_unknown_stop_reason` | R13（坑 A10） |
| `015_error_mid_stream` | R9（坑 A8） |

- [ ] 每个 fixture 在 `SplitStrategy` 全矩阵下产出相同事件序列（宪法 C24）
- [ ] 未知 event 不导致任何 `Err`，且 `raw` 被保留
- [ ] 非法 JSON 帧 → `ParseFailure` 事件，后续帧仍被正确解析
- [ ] `event:` 与 `data.type` 冲突时以 `data.type` 为准并记 anomaly
- [ ] bench：单帧解析 < 3 µs，堆分配次数为 0（无转义的 delta）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| `tool_use` / `input_json_delta` | F09 |
| `thinking` / `redacted_thinking` / `signature_delta` | F09 |
| `citations_delta` | F09 |
| OpenAI Chat / Responses | F08 |
| 映射表全覆盖检查（`coverage_matrix`） | F10 |
