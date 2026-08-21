# F04 · Observation IR 与最小 Accumulator

| | |
|---|---|
| ID | F04 |
| 里程碑 | M0 |
| 依赖 | F02 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/11-observation-model.md` §4,§6,§7；`docs/ADR/ADR-011-observation-ir.md` |
| 宪法条款 | C2, C6, C13, C14, C21 |

## 1. 目标

定义 Observation IR 与 `InteractionSink` 契约，并实现只累积**文本与 usage** 的最小 Accumulator。完整能力留给 F09。

## 2. 范围

**In scope**：`ObservedEvent` 全部类型定义（不是子集）、`BlockCoord`、`ObservedInteraction`、`Outcome`/`Integrity`、`UsageAccumulator`、文本 block 的累积、`InteractionSink` trait。

**Out of scope**：tool call / reasoning 的累积、异常容忍、交叉校验、非流式合成（全部 → F09）。

> **注意这个切分**：类型定义要**一次到位**（改 IR 会波及 F05/F08/F09/F12），但累积逻辑可以增量。

## 3. 需求

- **R1** THE SYSTEM SHALL 定义 `ObservedEvent` 与 `ObservedEventKind`，取值与 `docs/11` §4.2 完全一致，`#[non_exhaustive]`。
- **R2** THE SYSTEM SHALL 定义 `BlockCoord { choice: u8, item: u16, part: u16 }`，`Copy` + `Ord` + `Hash`。
- **R3** THE SYSTEM SHALL 为未知内容提供容纳位：`Unknown{event_name}`、`BlockDelta::Opaque`、`BlockKind::Other`、`FinishReason::Other + raw_reason`、`UsageDelta.extra`（宪法 C2）。
- **R4** THE SYSTEM SHALL 定义 `trait InteractionSink`，方法 `try_submit(&self, Box<ObservedInteraction>) -> Result<(), SinkFull>`，**非阻塞**（宪法 C5）。
- **R5** THE SYSTEM SHALL 把 usage 按 **last-non-null wins、逐字段独立** 合并（`docs/11` §6）。
- **R6** THE SYSTEM SHALL NOT 对 usage 求和。
- **R7** WHEN usage 完全缺失 THE SYSTEM SHALL 提供 `estimated_output_tokens` 并置 `usage_source = Missing`，且**不写入真实 usage 字段**（宪法 C14）。
- **R8** WHEN 累积文本超过 `max_accumulated_bytes` THE SYSTEM SHALL 置 `truncated = true` 并停止累积，但**继续消费后续事件**（宪法 C10）。
- **R9** THE SYSTEM SHALL 在累积文本时使用摊销 `push_str`，稳态每 delta 零堆分配（宪法 C21）。
- **R10** WHEN 收到 `Gap` 事件 THE SYSTEM SHALL 把所有 open block 置 `degraded` 并把 `Integrity` 降级为 `Degraded`（宪法 C13）。
- **R11** THE SYSTEM SHALL 让 `Outcome` 与 `Integrity` 保持正交，二者都出现在 `ObservedInteraction` 中。
- **R12** THE SYSTEM SHALL 在任意时刻可被 `finish(outcome)` 调用并产出语义正确的 partial 结果。

## 4. 接口契约

完整类型定义见 `docs/11` §4.1–4.5、§7.1、§7.5 与 `docs/06` §10（`TraceContext`）。本 spec **不重复**，只固定这几处签名：

```rust
// argus-protocol/src/sink.rs   ← 放这里而不是 core，见 docs/15 §4.1（避免循环依赖）
pub trait InteractionSink: Send + Sync + 'static {
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull>;
}
pub struct SinkFull(pub Box<ObservedInteraction>);   // 归还所有权，调用方决定丢弃策略

// argus-protocol/src/accumulator/mod.rs
pub struct Accumulator { /* 见 docs/11 §7.1 */ }

impl Accumulator {
    pub fn new(meta: RequestMeta, cfg: AccumulatorConfig) -> Self;
    /// 同步、不 panic、不返回 Err —— 异常一律记入 stats.anomalies
    pub fn apply(&mut self, ev: &ObservedEvent);
    pub fn finish(self, outcome: Outcome) -> Box<ObservedInteraction>;   // 见 §4.1
    pub fn accumulated_bytes(&self) -> usize;
}

pub struct AccumulatorConfig {
    pub max_accumulated_bytes: usize,   // 默认 1 MiB
    pub raw_mode: RawMode,              // Full | Selective { head, tail } | None
}
```

**`apply` 的签名是刻意的**：返回 `()` 而不是 `Result`。累积器永不失败——任何异常都是 `anomalies` 里的一条记录（`docs/11` §7.3）。

### 4.1 落地时的偏差

**`docs/11` 里有 11 个类型只出现了名字、没有定义**（`ObservedRequest` /
`ResponseMeta` / `ResolvedUsage` / `UsageSource` / `ObservedBlock` /
`RawEventLog` / `UnknownRecord` / `ProviderErrorRecord` / `ParseFailureKind` /
`TextChunk` / `RequestMeta` / `RawMode`）。本切片按 C31 先把它们补进
`docs/11` §4.6 与 §7.5.1，再实现。

| 偏差 | 为什么 |
|---|---|
| `ObservedEventKind` 的 5 个大变体装 `Box` | 不装的话 `BlockDelta` 变体就把枚举顶到 80 字节，守不住 §7 的 64 字节预算。装箱的都是每流一两次的罕见变体 |
| `UsageDelta.extra` 是 `Option<Box<Bytes>>` 而非 `Option<Bytes>` | 同上：裸 `Bytes` 是 32 字节，会把 `UsageDelta` 顶到 72 |
| `Accumulator::finish` 返回 `Box<ObservedInteraction>` | `InteractionSink::try_submit` 收的就是 `Box`，直接产出省掉一次大结构体搬运 |
| `StreamStats` 新增 `parse_failures: u32` | 一条流里 50 次解析失败正是 `StreamStats` 存在的意义，而 `Anomaly` 里没有它的位置（那些是累积器视角的反常，解析失败是解析器视角的）。已回填 `docs/11` §7.1 |
| `ObservedInteraction` 多了 `observer` / `observe_mode` | `docs/15` 的 `interactions` 表有这两列，不带出来就得让 F12 另找地方取 |
| `BlockState.text` / `json_raw` 用 `TextBuf` 而非裸 `String` | cap 检查必须在 `push_str` **之前**（§5），而预算是全部 block 共享的一份，裸 `String` 表达不了 |
| 文本预算是**全部 block 共享**的一份 | `max_accumulated_bytes` 是「这次交互最多累积多少」，不是「每个块最多多少」。后者在 `n>1` 或多 tool call 时会成倍放大 |

## 5. 实现要点

- `ObservedEventKind` 的大小控制在 64 字节内，用 `CompactString`/`SmallVec`/`Bytes` 而非 `String`/`Vec`（宪法 C21）。
- `blocks` 用 `BTreeMap<BlockCoord, BlockState>`：`BlockCoord` 是 `Copy` 的固定大小 key，无堆分配；`BTreeMap` 保证最终按坐标有序输出。
- `TextBuf::push` 的 cap 检查在 `push_str` **之前**，避免先扩容再发现超限。装不下时回退到最近的 UTF-8 字符边界 —— 写半个字符会让落库文本变乱码。
- **R6 是最危险的一条**：Anthropic 的 `message_delta.usage.output_tokens` 是累积值，求和会让计费翻倍且"看起来合理"。必须有专门的单元测试锁死。
- `estimated_output_tokens` 本切片用最简策略（`text_bytes / 4`），精度问题记在 `docs/11` OQ-5。

## 6. 任务

- [x] T0 **先补 `docs/11`**：11 个只有名字没有定义的类型（见 §4.1）
- [x] T1 `ir/event.rs`：`ObservedEvent` / `ObservedEventKind` / `RawRef` / `ParseFailureKind`
- [x] T2 `ir/block.rs`：`BlockCoord` / `BlockKind` / `BlockDelta` / `TextChunk`
- [x] T3 `ir/meta.rs`：`ResponseStart` / `UsageDelta` / `Finish` / `FinishReason`
- [x] T4 `interaction.rs`：`ObservedInteraction` / `Outcome` / `Integrity` / `StreamStats` / `Anomaly` / `Timings` + §7.5.1 的六个产物类型
- [x] T5 `sink.rs`：`InteractionSink` / `SinkFull`
- [x] T6 `accumulator/usage.rs`：`UsageAccumulator`（last-non-null wins）+ 估算
- [x] T7 `accumulator/text.rs`：`TextBuf`（摊销 push + 共享预算 + 字符边界回退）
- [x] T8 `accumulator/mod.rs`：`Accumulator::apply` / `finish` / raw 的 selective 策略
- [x] T9 单元测试 + `tests/accumulator_matrix.rs` + `tests/accumulator_alloc.rs`

## 7. 验收

```bash
cargo test -p argus-protocol accumulator
cargo test -p argus-protocol ir
```

- [x] `size_of::<ObservedEventKind>() <= 64` —— 实测 48
- [x] **usage last-wins 锁死测试**：`(25, 15)` 而非 `(25, 16)`
- [x] usage 缺失 → `Missing` + `output_tokens.is_none()` + `estimated_output_tokens.is_some()`
- [x] 文本超 cap → `truncated == true`，且**继续 apply 后续事件不 panic、usage 仍被正确记录**
- [x] `Gap` → `Integrity::Degraded { .. }`；**仍打开**的 block `degraded == true`，已关闭的不受影响
- [x] 任意时刻 `finish` 产出合法 partial 结果（含一个事件都没有的情况）
- [x] `Outcome::Completed` + `Integrity::Degraded` 与 `ClientAborted` + `Complete` 两个方向都验
- [x] 分配行为：10k 次 text delta 的扩容次数是**对数级**（≤ 8），且稳态下为 0
- [x] 全 workspace 测试 240 → 279（新增 39 个：19 单测 + 20 集成）

> **「10k 次 delta 堆分配为 0」这条做不到，也不该做到。** 真正的全程零分配
> 只有一开口就预留到 `max_accumulated_bytes` 才成立，而那意味着每个 block
> 一开口吃掉 1 MiB —— 一万条并发流直接把内存打光（宪法 C9 的同一条理由）。
> R9 的措辞本身是「稳态每 delta 零堆分配（**摊销**）」，验的就该是摊销：
> 扩容次数对数级、且稳态下为 0。两条都有专门的测试。
>
> 用的是「容量没变 == 这次没分配」而不是计数分配器：后者需要 `unsafe`，
> 而全 workspace 禁 unsafe（宪法 C8，`check-invariants.sh` 会拦）。

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| tool call / reasoning / signature 的累积 | F09 |
| 乱序、重复、孤儿事件的**容忍策略**（本切片只记 anomaly，不重排也不修复） | F09 |
| `response.completed` 交叉校验 | F09 |
| 非流式 `SyntheticEventGen` | F09 |
| `InteractionSink` 的实现 | F12 |
| `Timings` 除 `ttft_micros` 外的字段填充 | F06（其余几段耗时只有主链路知道） |
| `ObservedRequest.blocks`（客户端回传的 ToolResult） | F09 |
