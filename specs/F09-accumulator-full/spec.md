# F09 · 完整 Accumulator 与非流式合成

| | |
|---|---|
| ID | F09 |
| 里程碑 | M1 |
| 依赖 | F08 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/11-observation-model.md` §7；`docs/09-observers.md` §8；`docs/ADR/ADR-011-observation-ir.md` D3,D4 |
| 宪法条款 | C10, C13, C14, C21 |

## 1. 目标

把 F04 的最小 Accumulator 补成生产级：tool calls、reasoning、异常容忍、交叉校验、raw 分级保存、非流式合成。

**本切片承载全项目最危险的一类 bug**：静默的、看起来合理的、影响钱的（`docs/22` §5 风险 4）。

## 2. 范围

**In scope**：tool call block 累积与 JSON 解析时机、reasoning/signature、§7.3 的 8 类异常容忍、Responses 交叉校验、`selective` raw 策略、`SyntheticEventGen`。

**Out of scope**：安全检查（→ F24）、落库（→ F12）。

## 3. 需求 · Tool Arguments（最关键）

- **R1** WHILE 流进行中 THE SYSTEM SHALL 仅把 `BlockDelta::Json` 追加到 `json_raw: String`，**不得**调用 `serde_json`（宪法 C14、`docs/11` §7.2）。
- **R2** WHEN 收到该 block 的 `BlockStop` THE SYSTEM SHALL 尝试解析一次。
- **R3** WHEN 协议无显式 BlockStop（OpenAI Chat）THE SYSTEM SHALL 在 `Finish` 时关闭并解析。
- **R4** IF `json_raw` trim 后为空字符串 THEN THE SYSTEM SHALL 产出 `ToolArguments::Empty`（**不是** parse 错误，`docs/09` 坑 A4）。
- **R5** IF 解析成功 THEN THE SYSTEM SHALL 存 `ToolArguments::Parsed(Box<RawValue>)`（不构造 DOM）。
- **R6** IF 解析失败 THEN THE SYSTEM SHALL 存 `ToolArguments::Invalid { raw, error }`。
- **R7** IF 流中断且未收到 BlockStop/Finish THEN THE SYSTEM SHALL 存 `ToolArguments::Incomplete { raw }` 且**不尝试解析**。
- **R8** THE SYSTEM SHALL NOT 补齐括号、引号或以任何方式"修复"截断的 JSON（宪法 C14）。

## 4. 需求 · 异常容忍

按 `docs/11` §7.3 逐条实现，每条产出一个 `Anomaly` 记录且**不中止累积**：

- **R9** `BlockDelta` 先于 `BlockStart` → 隐式创建，`Anomaly::ImplicitStart`
- **R10** 同坐标重复 `BlockStart` → 旧的强制关闭，`Anomaly::DuplicateStart`
- **R11** 孤儿 `BlockStop` → 忽略，`Anomaly::OrphanStop`
- **R12** `BlockStop` 后又来 `BlockDelta` → 重新打开并 append，`Anomaly::DeltaAfterStop`
- **R13** `Finish` 后仍有事件 → 继续处理，`Anomaly::EventAfterFinish`
- **R14** 多个 `Finish` → 首个为准，`Anomaly::DuplicateFinish`
- **R15** THE SYSTEM SHALL 在任何异常组合下不 panic（宪法 C7）

## 5. 需求 · 其余

- **R16** THE SYSTEM SHALL 累积 `BlockKind::Reasoning` 的文本与 `BlockDelta::Signature`，signature 原样保留（`docs/09` 坑 A7）。
- **R17** WHEN 协议为 `OpenAiResponses` 且收到 `response.completed` THE SYSTEM SHALL 交叉校验累积文本，不一致时记 `Anomaly::TextMismatch` 并**不修正**（宪法 C14、`docs/11` §7.4）。
- **R18** THE SYSTEM SHALL 按 `RawMode::Selective { head, tail }` 裁剪 raw：保留全部 Unknown / ProviderError / 结构性事件 / 首尾各 N 条（`docs/11` §8.1）。
- **R19** THE SYSTEM SHALL 提供 `SyntheticEventGen`，把非流式 JSON 响应展开为与流式**完全等价**的事件序列（`docs/11` §5.4）。
- **R20** THE SYSTEM SHALL 提供 `ObservedInteraction::normalized()`，抹去时间戳与 raw，用于流式/非流式的等价性断言。

## 6. 接口契约

```rust
pub enum ToolArguments {
    Empty,
    Parsed(Box<serde_json::value::RawValue>),
    Invalid { raw: String, error: CompactString },
    Incomplete { raw: String },
}

pub struct SyntheticEventGen;
impl SyntheticEventGen {
    /// 非流式 body → 等价事件序列。与流式走同一个 Accumulator。
    pub fn expand(protocol: Protocol, body: &[u8], out: &mut dyn EventSink);
}

impl ObservedInteraction {
    /// 去掉 at_micros / raw / timings，用于跨路径等价比较
    pub fn normalized(&self) -> NormalizedInteraction;
}
```

## 7. 实现要点

- **R1 的诱惑很大**：实现者容易想"试着 parse 一下，失败就等下一片"。这在 `input_json_delta` 下每片都会失败，纯浪费 CPU（`docs/11` §7.2）。用 `debug_assert!` 或代码注释锁死。
- `Box<RawValue>` 而非 `Value`：只验证合法性 + 原样存储，避免一次完整 DOM 构建（`ADR-011` D3）。
- 异常计数用 `SmallVec<[Anomaly; 4]>`，绝大多数流是 0 条，不分配。
- `SyntheticEventGen` 与流式路径产出相同结果，是**极强的正确性交叉验证**（两条完全不同的代码路径）。这个等价性测试比任何单元测试都有价值。
- `selective` 裁剪在 `finish()` 时做，流进行中先全存（有 cap 保护），否则无法知道"最后 8 条"是哪些。

## 8. 任务

- [ ] T1 `accumulator/tool.rs`：`json_raw` 累积 + 四态解析（R1–R8）
- [ ] T2 `accumulator/reasoning.rs`：reasoning + signature
- [ ] T3 `accumulator/anomaly.rs`：R9–R15 的 8 类容忍
- [ ] T4 `accumulator/validate.rs`：Responses 交叉校验（R17）
- [ ] T5 `accumulator/raw.rs`：`selective` 裁剪策略（R18）
- [ ] T6 `synthetic.rs`：三协议的非流式展开（R19）
- [ ] T7 `normalized()`（R20）
- [ ] T8 补齐 F05 未做的 Anthropic 映射：`tool_use` / `input_json_delta` / `thinking` / `redacted_thinking` / `signature_delta` / `citations_delta`
- [ ] T9 单元测试 + fixture

## 9. 验收

```bash
cargo test -p argus-protocol accumulator
cargo test -p argus-protocol synthetic
```

**必测**：

- [ ] 流在 tool args 中间断开 → `Incomplete { raw }`，raw 与实际收到的片段逐字节相同
- [ ] 完整但非法的 JSON → `Invalid`，**raw 未被改动**
- [ ] 空 partial_json → `Empty`（fixture `anthropic_messages/004_tool_use_no_args`）
- [ ] **反修复断言**：proptest 生成截断 JSON，断言从不产出 `Parsed`（宪法 C14）
- [ ] 8 类异常各有一个 `_abnormal/` fixture，断言 anomaly 类型与数量精确匹配
- [ ] 任意异常组合下 `apply` 不 panic（proptest，任意事件序列）
- [ ] **流式/非流式等价**：同一请求的两份 fixture，`normalized()` 结果相等
- [ ] Responses 交叉校验：人为篡改 fixture 的一个 delta，断言产出 `TextMismatch` 且累积结果**未被修正**
- [ ] `selective` raw：断言 Unknown/Error 事件 100% 保留，中间的 text delta 被裁掉
- [ ] 累积超 cap 后仍能正确记录 usage 与 finish

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| fixture 全矩阵与 `coverage_matrix` | F10 |
| 落库 | F12 |
| 安全规则消费 IR | F24 |
| 真正的 tokenizer 估算 | Phase 2（`docs/11` OQ-5） |
