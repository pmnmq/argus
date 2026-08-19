# F08 · OpenAI Chat / Responses 解析器

| | |
|---|---|
| ID | F08 |
| 里程碑 | M1 |
| 依赖 | F07 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/04-protocols.md` §5.1–5.2；`docs/06-observation-model.md` §5.1–5.2 |
| 宪法条款 | C2, C7, C21, C24, C26 |

## 1. 目标

补齐另外两个协议的解析器。Chat Completions 需要**合成坐标**，Responses 需要**二维坐标**——两者都是 `BlockCoord` 抽象的实际检验。

## 2. 范围

**In scope**：`OpenAiChatParser`、`OpenAiResponsesParser`、坐标合成规则、`docs/04` 中列出的全部 8 个 Chat 坑 + 7 个 Responses 要点。

**Out of scope**：Accumulator 侧的 tool args 拼接（→ F09）、Gemini（Phase 2）。

## 3. 需求 · OpenAI Chat

- **R1** THE SYSTEM SHALL 按 `docs/06` §5.1 的表合成坐标：content→`(i,0,0)`、reasoning→`(i,1,0)`、refusal→`(i,2,0)`、tool_calls[j]→`(i,16+j,0)`。
- **R2** WHEN 帧为 `data: [DONE]` THE SYSTEM SHALL 不产出事件（framer 已消化，坑 C1）。
- **R3** WHEN `delta.tool_calls[j]` 首次出现 THE SYSTEM SHALL 产出 `BlockStart{ToolUse{id,name}}`；后续 `arguments` 产出 `BlockDelta::Json`（坑 C2）。
- **R4** WHEN `function.name` 分片到达 THE SYSTEM SHALL **append 而非覆盖**（坑 C3）。
- **R5** WHEN chunk 的 `choices` 为空且含 `usage` THE SYSTEM SHALL 产出 `Usage`（坑 C4）。
- **R6** WHEN 一个 chunk 同时含 content delta 与 `finish_reason` THE SYSTEM SHALL 产出多个事件，顺序为 delta → BlockStop → Finish（坑 C5）。
- **R7** WHERE `dialect.reasoning_field` 已配置 THE SYSTEM SHALL 把该字段映射为 `BlockKind::Reasoning`（坑 C6，DeepSeek 的 `reasoning_content`）。
- **R8** WHEN `n > 1` 产生多个 choices THE SYSTEM SHALL 为每个 choice 使用独立的 `choice` 坐标（坑 C7）。
- **R9** WHEN 上游 EOF 但未收到 `[DONE]` THE SYSTEM SHALL 视为正常结束（坑 C8）。
- **R10** WHEN `finish_reason` 非空 THE SYSTEM SHALL 隐式关闭该 choice 下所有 open block（Chat 无 `content_block_stop`）。
- **R11** `finish_reason` 映射按 `docs/06` §5.1 表；未知值 → `Other + raw_reason`。

## 4. 需求 · OpenAI Responses

- **R12** THE SYSTEM SHALL 以 `data` 中的 `type` 为权威，`event:` 行仅 fallback（要点 R1）。
- **R13** THE SYSTEM SHALL 把 `output_index`→`item`、`content_index`→`part`、`choice` 恒 0（要点 R2）。
- **R14** THE SYSTEM SHALL 按 `docs/06` §5.2 的表处理 `response.*` 全部已知事件。
- **R15** WHEN 事件名以 `response.` 开头但不在已知集合 THE SYSTEM SHALL 产出 `Unknown{event_name}`（要点 R6，宪法 C2）。
- **R16** WHEN 收到 `response.output_item.done` THE SYSTEM SHALL 关闭该 item 下**所有** part。
- **R17** WHEN 收到 `response.completed` THE SYSTEM SHALL 产出 `Usage` + `Finish{Stop}`，并把完整响应对象附在事件上供 F09 交叉校验（要点 R5）。
- **R18** WHEN 收到 `response.failed` / `response.incomplete` THE SYSTEM SHALL 映射为 `Finish{Error}` / `Finish{Incomplete}` 并保留 `raw_reason`（要点 R7）。

## 5. 接口契约

沿用 F05 定义的 `ProtocolParser` / `EventSink` / `AnyParser`，本切片只填充两个变体：

```rust
pub struct OpenAiChatParser {
    dialect: Arc<Dialect>,
    /// choice → 已见过的 tool_call index → 是否已发过 BlockStart
    open: SmallVec<[ChoiceState; 1]>,
    seq: u32,
}

pub struct OpenAiResponsesParser {
    /// output_index → item 类型（决定后续 delta 归属哪种 BlockKind）
    items: SmallVec<[ItemKind; 4]>,
    seq: u32,
}
```

## 6. 实现要点

- **R1 的合成坐标是本切片最容易出错的地方**。写一个 `const fn coord_for(choice, slot)` 集中处理，禁止散落魔数 16。
- Chat 的 `tool_calls[j].index` 是 provider 给的，**不等于**数组下标（可能乱序）。以 `index` 字段为准。
- Chat 没有显式 block stop（R10），因此 `finish` 时机是唯一的关闭点——这会影响 F09 的 JSON 解析时机。
- Responses 的 `items` 表是必须的：`response.output_text.delta` 只带 `output_index`，要靠之前的 `output_item.added` 才知道它属于 message 还是 reasoning。
- R17 的"完整响应对象"用 `Bytes` 原样携带（不解析），F09 才用它做校验——保持 parser 是纯映射。

## 7. 任务

- [ ] T1 `parser/openai_chat.rs`：坐标合成 + `ChoiceState`
- [ ] T2 Chat 的 content / reasoning / refusal 路径
- [ ] T3 Chat 的 tool_calls 路径（含 R3/R4 的 id/name 处理）
- [ ] T4 Chat 的 usage / finish / 隐式关闭
- [ ] T5 `parser/openai_responses.rs`：`items` 表 + 二维坐标
- [ ] T6 Responses 的 message / function_call / reasoning 三类 item
- [ ] T7 Responses 的 completed / failed / incomplete / error / unknown
- [ ] T8 `AnyParser::for_protocol` 补全三个分支
- [ ] T9 fixture（见 §8）+ 单元测试

## 8. 验收

```bash
cargo test -p argus-protocol openai
```

**fixture**（`tests/fixtures/openai_chat/` 与 `openai_responses/`，来源必须录制，宪法 C25）：

| 目录 | 覆盖的坑 |
|---|---|
| `openai_chat/001_simple_stream` | 基础 |
| `openai_chat/002_tool_call_single` | C2 |
| `openai_chat/003_tool_calls_parallel` | C2 + 多 index |
| `openai_chat/004_tool_name_fragmented` | **C3** |
| `openai_chat/005_usage_final_chunk` | C4 |
| `openai_chat/006_finish_with_content` | C5 |
| `openai_chat/007_reasoning_content_deepseek` | C6 |
| `openai_chat/008_n_2_multiple_choices` | C7 |
| `openai_chat/009_no_done_sentinel` | C8 |
| `openai_responses/001_simple_stream` | 基础 + R2 |
| `openai_responses/002_function_call` | R3 |
| `openai_responses/003_reasoning_summary` | R4 |
| `openai_responses/004_multi_output_item` | R2 二维坐标 |
| `openai_responses/006_response_incomplete` | R7 |
| `openai_responses/007_response_failed` | R7 |
| `openai_responses/008_unknown_event_type` | **R6** |

- [ ] 全部 fixture 在 `SplitStrategy` 全矩阵下一致（宪法 C24）
- [ ] `004_tool_name_fragmented` 断言 name 被拼接为完整值而非最后一片
- [ ] `008_n_2_multiple_choices` 断言两个 choice 的 block 互不串扰
- [ ] `008_unknown_event_type` 断言产出 `Unknown` 且不返回 Err
- [ ] bench：Chat / Responses 单帧解析 < 3 µs，无转义时零分配

## 9. 交给后续

| 不做的事 | 承接 |
|---|---|
| tool args 的拼接与 JSON 解析时机 | F09 |
| `response.completed` 的实际交叉校验 | F09 |
| 方言的实际配置来源 | F17 |
| `coverage_matrix` 检查 | F10 |
