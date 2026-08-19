# F04 · Observation IR 与最小 Accumulator

| | |
|---|---|
| ID | F04 |
| 里程碑 | M0 |
| 依赖 | F02 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/06-observation-model.md` §4,§6,§7；`docs/ADR/ADR-004-observation-ir.md` |
| 宪法条款 | C2, C6, C13, C14, C21 |

## 1. 目标

定义 Observation IR 与 `InteractionSink` 契约，并实现只累积**文本与 usage** 的最小 Accumulator。完整能力留给 F09。

## 2. 范围

**In scope**：`ObservedEvent` 全部类型定义（不是子集）、`BlockCoord`、`ObservedInteraction`、`Outcome`/`Integrity`、`UsageAccumulator`、文本 block 的累积、`InteractionSink` trait。

**Out of scope**：tool call / reasoning 的累积、异常容忍、交叉校验、非流式合成（全部 → F09）。

> **注意这个切分**：类型定义要**一次到位**（改 IR 会波及 F05/F08/F09/F12），但累积逻辑可以增量。

## 3. 需求

- **R1** THE SYSTEM SHALL 定义 `ObservedEvent` 与 `ObservedEventKind`，取值与 `docs/06` §4.2 完全一致，`#[non_exhaustive]`。
- **R2** THE SYSTEM SHALL 定义 `BlockCoord { choice: u8, item: u16, part: u16 }`，`Copy` + `Ord` + `Hash`。
- **R3** THE SYSTEM SHALL 为未知内容提供容纳位：`Unknown{event_name}`、`BlockDelta::Opaque`、`BlockKind::Other`、`FinishReason::Other + raw_reason`、`UsageDelta.extra`（宪法 C2）。
- **R4** THE SYSTEM SHALL 定义 `trait InteractionSink`，方法 `try_submit(&self, Box<ObservedInteraction>) -> Result<(), SinkFull>`，**非阻塞**（宪法 C5）。
- **R5** THE SYSTEM SHALL 把 usage 按 **last-non-null wins、逐字段独立** 合并（`docs/06` §6）。
- **R6** THE SYSTEM SHALL NOT 对 usage 求和。
- **R7** WHEN usage 完全缺失 THE SYSTEM SHALL 提供 `estimated_output_tokens` 并置 `usage_source = Missing`，且**不写入真实 usage 字段**（宪法 C14）。
- **R8** WHEN 累积文本超过 `max_accumulated_bytes` THE SYSTEM SHALL 置 `truncated = true` 并停止累积，但**继续消费后续事件**（宪法 C10）。
- **R9** THE SYSTEM SHALL 在累积文本时使用摊销 `push_str`，稳态每 delta 零堆分配（宪法 C21）。
- **R10** WHEN 收到 `Gap` 事件 THE SYSTEM SHALL 把所有 open block 置 `degraded` 并把 `Integrity` 降级为 `Degraded`（宪法 C13）。
- **R11** THE SYSTEM SHALL 让 `Outcome` 与 `Integrity` 保持正交，二者都出现在 `ObservedInteraction` 中。
- **R12** THE SYSTEM SHALL 在任意时刻可被 `finish(outcome)` 调用并产出语义正确的 partial 结果。

## 4. 接口契约

完整类型定义见 `docs/06` §4.1–4.5、§7.1、§7.5 与 `docs/02` §10（`TraceContext`）。本 spec **不重复**，只固定这几处签名：

```rust
// argus-protocol/src/sink.rs   ← 放这里而不是 core，见 docs/10 §4.1（避免循环依赖）
pub trait InteractionSink: Send + Sync + 'static {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull>;
}
pub struct SinkFull(pub Box<ObservedInteraction>);   // 归还所有权，调用方决定丢弃策略

// argus-protocol/src/accumulator/mod.rs
pub struct Accumulator { /* 见 docs/06 §7.1 */ }

impl Accumulator {
    pub fn new(meta: RequestMeta, cfg: AccumulatorConfig) -> Self;
    /// 同步、不 panic、不返回 Err —— 异常一律记入 stats.anomalies
    pub fn apply(&mut self, ev: &ObservedEvent);
    pub fn finish(self, outcome: Outcome) -> ObservedInteraction;
    pub fn accumulated_bytes(&self) -> usize;
}

pub struct AccumulatorConfig {
    pub max_accumulated_bytes: usize,   // 默认 1 MiB
    pub raw_mode: RawMode,              // Full | Selective { head, tail } | None
}
```

**`apply` 的签名是刻意的**：返回 `()` 而不是 `Result`。累积器永不失败——任何异常都是 `anomalies` 里的一条记录（`docs/06` §7.3）。

## 5. 实现要点

- `ObservedEventKind` 的大小控制在 64 字节内，用 `CompactString`/`SmallVec`/`Bytes` 而非 `String`/`Vec`（宪法 C21）。
- `blocks` 用 `BTreeMap<BlockCoord, BlockState>`：`BlockCoord` 是 `Copy` 的固定大小 key，无堆分配；`BTreeMap` 保证最终按坐标有序输出。
- `TextBlockState::push` 的 cap 检查在 `push_str` **之前**，避免先扩容再发现超限。
- **R6 是最危险的一条**：Anthropic 的 `message_delta.usage.output_tokens` 是累积值，求和会让计费翻倍且"看起来合理"。必须有专门的单元测试锁死。
- `estimated_output_tokens` 本切片用最简策略（`text_bytes / 4`），精度问题记在 `docs/06` OQ-5。

## 6. 任务

- [ ] T1 `ir/event.rs`：`ObservedEvent` / `ObservedEventKind` / `RawRef`
- [ ] T2 `ir/block.rs`：`BlockCoord` / `BlockKind` / `BlockDelta` / `TextChunk`
- [ ] T3 `ir/meta.rs`：`ResponseStart` / `UsageDelta` / `Finish` / `FinishReason`
- [ ] T4 `interaction.rs`：`ObservedInteraction` / `Outcome` / `Integrity` / `StreamStats` / `Anomaly` / `Timings`
- [ ] T5 `sink.rs`：`InteractionSink` / `SinkFull`
- [ ] T6 `accumulator/usage.rs`：`UsageAccumulator`（last-non-null wins）+ 估算
- [ ] T7 `accumulator/text.rs`：`TextBlockState`（摊销 push + cap）
- [ ] T8 `accumulator/mod.rs`：`Accumulator::apply`（本切片只处理 Text 类 block + Usage + Finish + Gap + Unknown）+ `finish`
- [ ] T9 单元测试：§7

## 7. 验收

```bash
cargo test -p argus-protocol accumulator
cargo test -p argus-protocol ir
```

- [ ] `size_of::<ObservedEventKind>() <= 64`
- [ ] **usage last-wins 锁死测试**：喂入 `input=25` 然后 `output=1` 然后 `output=15`，断言结果是 `(25, 15)` 而非 `(25, 16)`
- [ ] usage 缺失 → `usage_source == Missing` 且 `usage.output_tokens.is_none()` 且 `estimated_output_tokens.is_some()`
- [ ] 文本超 cap → `truncated == true`，且**继续 apply 后续事件不 panic、usage 仍被正确记录**
- [ ] `Gap` → `Integrity::Degraded { .. }` 且 open block 的 `degraded == true`
- [ ] 任意时刻 `finish(ClientAborted)` 产出合法 `ObservedInteraction`
- [ ] `Outcome::Completed` + `Integrity::Degraded` 的组合可表达（正交性）
- [ ] bench：10k 次 text delta 的堆分配次数为 0（用 `dhat` 或计数分配器）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| tool call / reasoning / signature 的累积 | F09 |
| 乱序、重复、孤儿事件的容忍 | F09 |
| `response.completed` 交叉校验 | F09 |
| 非流式 `SyntheticEventGen` | F09 |
| `InteractionSink` 的实现 | F12 |
| raw 的 `selective` 策略实际裁剪 | F09 |
