# ADR-011 · Observation IR

- **状态**：Accepted
- **日期**：2026-08-19
- **相关**：`11-observation-model.md`

---

## Context

Argus 需要把三种（未来更多）互不相同的流式协议归一化成可以统一审计、统计、安全分析的结构。

这个内部数据模型的**定位**是整个设计中最容易走偏的地方。存在一个强烈的诱惑：

> "既然我们已经能把三种协议解析成统一格式了，为什么不用这个格式重建上游请求？这样就能支持跨协议调用了。"

这条路走下去，IR 会变成"统一 AI 协议"——正是 `00-overview.md` N1/N2 明确拒绝的东西。

---

## Decision

**Observation IR 只描述"发生了什么"，不描述"如何重放"。它是只读的观测产物，不是可执行的中间表示。**

三条硬性边界：

1. **IR 绝不用于重建上游请求。** 需要重放请求时，用 `raw_request`（原始字节）。
2. **IR 不追求成为三种协议的并集。** 它是**共同抽象的交集 + 一个 raw 逃生舱**。
3. **凡是 IR 表达不了的，进 raw / extensions / Unknown，绝不丢弃，也绝不强行归一化。**

---

## 为什么 IR 不能承担重放职责

如果 IR 要能重建任意 provider 的请求，它必须：

| 要求 | 后果 |
|------|------|
| 包含所有协议的所有字段 | 模型随每个 provider 的每次更新而膨胀 |
| 处理互斥字段（如 Anthropic 的 `system` vs OpenAI 的 `messages[0]`） | 需要标记"这个 IR 来自哪个协议"，那归一化就失去意义 |
| 对未知字段做决策（丢弃？保留在哪？） | 每个新字段都是一次设计决策 |
| 保证 `serialize(IR) == original_bytes` | 不可能（见 `ADR-008` 理由 2） |

最终它会变成一个又大又不精确的"统一协议"，且**没有任何一个 provider 真正使用它**。

相反，一个**只读的观测 IR** 可以：
- 保持小而稳定（几十个变体，不随 provider 更新膨胀）
- 对未知内容有统一的处理（`Unknown` / `Opaque` / `Other`）
- 容易测试（纯函数：字节进，事件出）

---

## 核心设计决定

### D1 · 统一坐标系 `BlockCoord`

三种协议的定位方式不同：

| 协议 | 坐标 |
|------|------|
| Anthropic Messages | 一维 `index` |
| OpenAI Chat | `choices[i]` + `tool_calls[j].index`（隐式） |
| OpenAI Responses | 二维 `output_index` + `content_index` |

```rust
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockCoord {
    pub choice: u8,    // OpenAI n>1；其他协议恒 0
    pub item: u16,     // Responses 的 output_index / Anthropic 的 index / Chat 的合成槽位
    pub part: u16,     // Responses 的 content_index；其他恒 0
}
```

**为什么是三级**：这是三种协议坐标系的**最小公共超集**。二级（照抄 Anthropic）会丢失 Responses 的层级关系；四级是没必要的。

**为什么是 `Copy` 的固定大小类型**：它是 `BTreeMap` 的 key，每个 delta 事件都要用。堆分配的 key 会在热路径上产生大量分配。

**代价**：OpenAI Chat 需要**合成坐标**（text→item 0，reasoning→1，refusal→2，tool_calls[j]→16+j）。这有点人为，但把人为的部分限制在一个明确的映射表里（`06` §5.1），好过让三种协议各用各的坐标。

---

### D2 · ToolUse 建模为 Block，不是独立事件类型

用户最初提出的模型有 `ToolCallStart` / `ToolCallArgumentsDelta` / `ToolCallEnd` 三个独立事件。

**改为**：`BlockKind::ToolUse` + `BlockDelta::Json`，与 text/reasoning 共用 `BlockStart`/`BlockDelta`/`BlockStop`。

**理由**：
- 在 Anthropic 协议里，`tool_use` **就是**一个 content block（`content_block_start` with `type: "tool_use"`）。拆成独立事件反而是对协议的扭曲。
- Responses 的 `function_call` 也是一个 `output_item`，与 `message` 同级。
- 统一后，Accumulator 的状态机只有一套 block 逻辑，而不是两套。
- 顺序关系天然保留（tool_use 与 text 的相对位置就是 `BlockCoord` 的顺序）。

**代价**：OpenAI 用户看审计数据时，tool call 的坐标是 `item=16+j` 而非 `j`，不够直观。用展示层的换算解决。

---

### D3 · tool arguments 只在 BlockStop 解析

**规则**：流进行中，`json_raw` 只是一个 `String`；只在 `BlockStop` 或 `Finish` 时调用一次 `serde_json`。

**理由**：
- `input_json_delta.partial_json` 可以在任意字节处切断——包括 UTF-8 字符中间、字符串字面量中间、转义序列中间（`\u00` | `e9`）。
- 中途 parse 必然失败。"失败了就下次再试"意味着每个 delta 一次失败的 parse，纯浪费 CPU。
- 没有任何场景需要中途的结构化参数（`12-security.md` 的 ToolGate 也是在 BlockStop 检查）。

**推论：绝不"修复"截断的 JSON。** 补齐括号/引号得到的参数可能与模型真实意图不同。把猜测的结果写进审计库，等于伪造数据。

三种结果明确区分：
```rust
enum ToolArguments {
    Empty,                                   // partial_json 拼接后为空（无参数工具，坑 A4）
    Parsed(Box<RawValue>),                   // 完整且合法
    Invalid { raw: String, error: String },  // 完整但非法 JSON
    Incomplete { raw: String },              // 流中断，未收到 BlockStop
}
```

用 `Box<RawValue>` 而非 `Value`：我们只需验证它是合法 JSON 并原样存储，不需要构建 DOM。

---

### D4 · Usage 用 last-non-null wins，逐字段独立

三种协议的 usage 时序完全不同：Anthropic 分两次给（`message_start` 给 input，`message_delta` 给 output），OpenAI 在最后一个 chunk 一次给全。

**统一规则：每个字段独立，新值非 `None` 就覆盖。**

**最重要的一条**：Anthropic 的 `message_delta.usage.output_tokens` 是**累积值不是增量**。求和会得到约 2 倍的错误结果，且这个错误"看起来合理"——不会崩溃，只会让所有 Anthropic 请求的计费翻倍。

这是本项目最危险的一类 bug：**静默的、看起来合理的、影响钱的**。因此它有专门的 golden case（`anthropic_messages/010_usage_split`）。

**usage 缺失时**：提供 `estimated_output_tokens`（按累积文本粗估），但**绝不写进真实字段**。`usage_source ∈ {reported, estimated, missing}` 让下游能区分。

---

### D5 · 未知内容的三个容纳位

| 层级 | 未知时的去处 |
|------|------------|
| 事件类型 | `ObservedEventKind::Unknown { event_name }` + raw |
| delta 类型 | `BlockDelta::Opaque { delta_type, raw }` |
| block 类型 | `BlockKind::Other(CompactString)` |
| finish reason | `FinishReason::Other` + `raw_reason: Some("...")` |
| usage 字段 | `UsageDelta.extra: Option<Bytes>` |

**每一处都配套一个指标**（`argus_unknown_event_total{event_name}` 等）。当 provider 上线新功能时，这些指标会立刻出现新的标签值——这是"provider 改了 API"的最早信号，比等用户报障早得多。

---

### D6 · `Outcome` 与 `Integrity` 是两个正交维度

```rust
enum Outcome {   // 流是怎么结束的
    Completed, ClientAborted { .. }, UpstreamError { .. },
    Cancelled { .. }, ObserverTimeout, ParserFailed { .. },
}
enum Integrity { // 观测到的内容有多完整
    Complete,
    Degraded { dropped_bytes, gap_count, truncated_blocks },
    MetadataOnly,
}
```

一个流可以是 `Completed` 但 `Degraded`（正常结束但中间丢了字节），也可以是 `ClientAborted` 但 `Complete`（中断前观测到的部分是完整的）。

**混为一谈会让下游分析出错**：例如成本分析要过滤掉 `Degraded` 的记录，但不应该过滤掉 `ClientAborted`（那些请求确实产生了费用）。

---

### D7 · Raw 保真是分级的

保存全部原始 SSE 字节的代价是最终文本的 3–5 倍（每个 delta 都有 JSON 包装）。

默认 `selective` 策略：只保存 `Unknown` / `ProviderError` / 结构性事件（`ResponseStart`/`Finish`/`Usage`）/ 触发安全规则的事件 / 首尾各 8 条。

**理由**：内容本身已经在 `blocks` 里以更紧凑的形式保存了。真正需要 raw 的场景是"解析出问题了"，而这些场景的信号恰恰是 Unknown / anomaly / error。存储量减少约 80%。

---

## Options Considered

**A. 用 `serde_json::Value` 作为通用 IR。** 最灵活，零设计成本。但：无类型安全；每事件 O(字段) 次分配（`05` §7.1 已论证这是不可接受的）；下游要写大量 `.get("x").and_then(...)`。**否决。**

**B. 每个协议一套独立 IR，不归一化。** 安全模块、统计模块、Accumulator 要各写三遍，且新增协议时全部要改。**否决**——归一化的价值正在于此。

**C. IR 采用一维 index（照抄 Anthropic）。** Responses 的 `output_index`/`content_index` 关系会被压平，无法表达"一个 output_item 下有多个 content_part"。**否决。**

**D. 流式增量 JSON parser（边收边解析 tool arguments）。** 对实时安全检测有价值。但需要支持"部分输入"的 JSON parser，复杂度高，且部分解析的结果可能误导。**推迟到 Phase 2**——当前 ToolGate 在 BlockStop 检查已经足够（`08` §6.2）。

**E. 把 usage 估算值写进真实字段。** 会让计费数据不可区分。**强烈否决。**

---

## Consequences

### 正面

- IR 小而稳定（约 12 个事件变体），不随 provider 更新膨胀。
- 一套 Accumulator 服务三种协议，且非流式响应通过 `SyntheticEventGen` 走同一条路径——两条完全不同的输入路径产出相同结果，是很强的正确性信号。
- 未知内容全部有容纳位 + 指标，前向兼容有主动信号。
- `Box<RawValue>` + `CompactString` + `SmallVec` 让 IR 的分配开销接近零。

### 负面

| 负面 | 缓解 |
|------|------|
| OpenAI Chat 需要合成坐标（人为） | 映射规则集中在一张表；展示层做换算 |
| 中断的流拿不到结构化 tool arguments | 保留 `raw`，明确标记 `Incomplete` |
| 不能用 IR 做跨协议调用 | 这是刻意的（见 Context） |
| 新增协议需要写一套映射 | 但映射表是声明式的，且 golden test 框架现成 |

### 对实现的约束

1. `argus-protocol` 必须是纯计算 crate（无 tokio/hyper/sqlx），否则 IR 的定义会被 IO 关注点污染。
2. `11-observation-model.md` §5 映射表的**每一行**必须有对应的 golden case（`coverage_matrix` 测试强制）。
3. IR 的所有枚举必须是 `#[non_exhaustive]`，保证新增变体不是 breaking change。
