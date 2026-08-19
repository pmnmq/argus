# F10 · Golden Test 体系

| | |
|---|---|
| ID | F10 |
| 里程碑 | M1 |
| 依赖 | F09 |
| 主 crate | `tests/` |
| 设计依据 | `docs/15-testing-strategy.md` §5–6,§8,§12 |
| 宪法条款 | C24, C25, C26, C27 |

## 1. 目标

把 F03–F09 零散的 fixture 收编成统一体系：驱动器、切分矩阵、映射表覆盖检查、SDK oracle、fuzz、contract test。

## 2. 范围

**In scope**：fixture 目录规范与 `meta.toml`、golden 驱动器、`SplitStrategy` 全矩阵、`coverage_matrix` 测试、录制工具、SDK 消费验证、proptest/cargo-fuzz、contract test 骨架。

**Out of scope**：集成测试（→ F21）、性能回归门禁（→ F28）。

## 3. 需求

- **R1** THE SYSTEM SHALL 采用 `docs/15` §5.1 的 fixture 目录结构与 §5.2 的 `meta.toml` 格式。
- **R2** THE SYSTEM SHALL 提供 `#[rstest]` 驱动器，自动发现 `tests/fixtures/**/meta.toml` 并执行。
- **R3** THE SYSTEM SHALL 对每个 fixture 用 `SplitStrategy` 全矩阵（≥ 30 种，含 `ByteByByte`）执行，全部结果必须一致（宪法 C24）。
- **R4** THE SYSTEM SHALL 用 `insta` 对 baseline 切分的 `events` 与 `interaction` 做 snapshot。
- **R5** THE SYSTEM SHALL 提供 `coverage_matrix` 测试：`docs/06` §5 映射表的每一行必须被至少一个 fixture 的 `covers` 字段覆盖，否则失败（宪法 C26）。
- **R6** THE SYSTEM SHALL 提供录制工具，产出符合 R1 规范的 fixture（宪法 C25）。
- **R7** THE SYSTEM SHALL 在录制与导出时执行脱敏，移除 API key、org id、用户内容（宪法 C27）。
- **R8** IF fixture 目录中检出敏感模式 THEN pre-commit hook SHALL 阻止提交。
- **R9** THE SYSTEM SHALL 提供 SDK 消费验证：官方 SDK → Argus → MockProvider，断言 (a) SDK 正常工作 (b) SDK 的解析结果与 Argus 的 `ObservedInteraction` 一致（`docs/15` §8.2）。
- **R10** SDK 验证 SHALL NOT 需要网络访问真实 provider。
- **R11** THE SYSTEM SHALL 覆盖每个 SDK 的 latest 与 n-1 版本。
- **R12** THE SYSTEM SHALL 提供 `cargo-fuzz` 目标：framer、三个 parser。
- **R13** THE SYSTEM SHALL 提供 contract test（`--ignored`，需真实 key），断言无 `Unknown` 事件、usage 完整、finish_reason 在已知集合内。
- **R14** contract test SHALL NOT 在常规 CI 中运行。
- **R15** WHEN contract test 发现新的 `Unknown` event THE 流程 SHALL 产出一个新 fixture 并开 issue。

## 4. 接口契约

```rust
// tests/golden.rs
#[rstest]
fn golden(#[files("tests/fixtures/**/meta.toml")] path: PathBuf) { /* docs/15 §5.3 */ }

// tests/support/split.rs   （F03 已产出雏形，此处收编）
pub enum SplitStrategy {
    Whole, ByteByByte, Fixed(usize), AtBoundaries,
    SplitFieldPrefix, SplitUtf8, SplitEscape, Seeded(u64),
}
impl SplitStrategy { pub fn all() -> Vec<Self>; pub fn apply(&self, data: &[u8]) -> Vec<TeeChunk>; }

// tests/coverage_matrix.rs
/// 解析 docs/06 §5 的 markdown 表，与所有 meta.toml 的 covers 求差集
#[test] fn all_mapping_rows_covered() { .. }
```

```bash
tools/record_fixture.py --provider anthropic --case tool_use --out tests/fixtures/...
argus fixtures export --request-id req_01J... --redact --out tests/fixtures/...
```

```
tests/e2e/sdk_compat/
├── conftest.py
├── test_openai_chat.py
├── test_openai_responses.py
├── test_anthropic.py
└── test_raw_curl.py        # 不依赖 SDK 行为的原始正确性
```

## 5. 实现要点

- **R5 的实现方式**：直接解析 `docs/06-observation-model.md` 的 markdown 表格（`| 线上事件 | IR 事件 |` 那几张），提取行标识，与 `meta.toml` 的 `covers` 数组求差。这样"文档即测试规格"，改了映射表就会立刻暴露缺失的用例。
- `SplitStrategy::all()` 返回 ~30 个实例：`Whole`、`ByteByByte`、`Fixed` × 5、四个语义切分点、`Seeded` × 20。
- SDK 验证的关键断言不是"SDK 没报错"，而是**"SDK 的累积结果 == Argus 的观测结果"**。前者只验证透明性，后者同时验证我们解析器的正确性（`docs/15` §8.2）。
- fuzz 语料从 `tests/fixtures/**/upstream.sse` 初始化。CI 每 PR 跑 60 秒，nightly 4 小时。
- fuzz 发现的崩溃语料**自动进 `_abnormal/`** 成为永久回归用例。

## 6. 任务

- [ ] T1 fixture 规范文档 + `meta.toml` schema
- [ ] T2 `tests/support/split.rs`：`SplitStrategy` 全矩阵（收编 F03 的雏形）
- [ ] T3 `tests/golden.rs`：驱动器 + insta snapshot
- [ ] T4 `tests/coverage_matrix.rs`：markdown 表解析 + 差集断言
- [ ] T5 `tools/record_fixture.py`：SDK + 录制 transport + 脱敏
- [ ] T6 `argus fixtures export` 子命令（脱敏导出）
- [ ] T7 pre-commit hook：fixture 敏感模式扫描
- [ ] T8 补齐 fixture 到 `docs/15` §5.1 的完整清单（~40 个目录）
- [ ] T9 `tests/e2e/sdk_compat/`：4 个 SDK × 主要场景
- [ ] T10 `fuzz/`：4 个 target + 语料初始化 + CI 接线
- [ ] T11 `tests/contract.rs`：`--ignored`，10 个最小用例
- [ ] T12 流式/非流式等价测试收编（F09 产出，此处纳入驱动器）

## 7. 验收

```bash
cargo test --test golden              # 全部 fixture × 30 切分
cargo test --test coverage_matrix
cargo test --test golden -- --nocapture 2>&1 | grep "mapping rows covered"
cd tests/e2e && pytest sdk_compat/
cargo fuzz run framer -- -max_total_time=60
```

- [ ] `coverage_matrix` 输出三个协议均为 `N/N covered`
- [ ] 全部 fixture 在 30 种切分下一致（宪法 C24）
- [ ] SDK 验证：Anthropic SDK 的 `get_final_message()` 的 tool input == Argus 的 `tool_calls[0].arguments`
- [ ] SDK 验证覆盖 latest 与 n-1
- [ ] 60 秒 fuzz 无崩溃
- [ ] fixture 目录敏感扫描为空
- [ ] `cargo test --test golden` 总耗时 < 60 秒（`docs/15` OQ-1）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| PG / 网络的集成测试 | F21 |
| 混沌测试 | F21 |
| 性能回归门禁 | F28 |
| contract test 的定时调度与 secret 管理 | F28 |
