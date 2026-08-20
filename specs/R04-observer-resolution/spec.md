# R04 · Observer 选择

| | |
|---|---|
| ID | R04 |
| 类型 | v2 新增能力 |
| 里程碑 | M1 |
| 依赖 | R01, R02 |
| 主 crate | `argus-proxy::observer`、`argus-protocol::observer` |
| 设计依据 | `docs/09-observers.md` §4；ADR-010 |
| 宪法条款 | C2, C10, **C34** |

## 1. 目标

决定"这条流该用哪套语义理解"，并保证**认不出来时仍然有观测**。

## 2. 范围

**In scope**：三级判定、`observer_rules` 匹配、`GenericSse` / `GenericJson` 兜底。

**Out of scope**：三种协议的具体解析（→ F05/F08）、规则的 CRUD（→ F22）。

## 3. 需求

- **R1** THE SYSTEM SHALL 按顺序判定：`observer_rules` → Profile 默认值 →
  Raw 模式按 host/path 猜 → 按 Content-Type 兜底。
- **R2** `observer_rules` SHALL 是**有序**表，**首个命中生效**，无权重无打分。
- **R3** 匹配维度 SHALL 为 `profile` / `host_suffix` / `path_prefix` / `method` /
  `content_type`，多个条件之间是 AND。
- **R4** WHEN `Content-Type` 为 `text/event-stream` 且无更具体匹配
  THE SYSTEM SHALL 选 `GenericSse`。
- **R5** WHEN `Content-Type` 为 `application/json` 且无更具体匹配
  THE SYSTEM SHALL 选 `GenericJson`。
- **R6** `GenericSse` SHALL 产出帧计数、字节数与 raw 留存，**不解析语义**。
- **R7** `GenericJson` SHALL 记录 raw 与顶层字段名，**不解析语义**。
- **R8** IF Observer 选错或解析失败 THEN 主链路 SHALL 完全不受影响，
  interaction 标 `Integrity::Degraded`。
- **R9** THE SYSTEM SHALL 暴露 `argus_observer_selected_total{kind}` 与
  `argus_observer_fallback_total{reason}`。

## 4. 接口契约

```rust
pub struct ObserverRules { rules: Vec<ObserverRule> }   // 有序
pub struct ObserverRule {
    pub profile: Option<CompactString>,
    pub host_suffix: Option<CompactString>,
    pub path_prefix: Option<CompactString>,
    pub method: Option<Method>,
    pub content_type: Option<CompactString>,
    pub observer: ObserverKind,
}
impl ObserverRules {
    /// 首个命中生效
    pub fn select(&self, ctx: &SelectCtx) -> Option<ObserverKind>;
}

pub fn resolve_observer(
    target: &ResolvedTarget,
    rules: &ObserverRules,
    resp_content_type: Option<&HeaderValue>,
) -> ObserverKind;

// argus-protocol
pub struct GenericSseObserver { frames: u64, bytes: u64 }
pub struct GenericJsonObserver { top_level_keys: SmallVec<[CompactString; 8]> }
```

## 5. 实现要点

- **Content-Type 兜底发生在收到响应 header 之后**，而前三级在请求时就能定。
  实现上先给一个"暂定值"，收到 header 后可以**收窄**到 Generic，但不能改成
  另一个语义 Observer——那会让已经解析的事件与后面的不一致。
- `GenericJson` 只记顶层 key 名，**不记 value**（宪法 C27）。
- Raw 模式的 host/path 猜测用一张内置的小表（`api.openai.com` → OpenAiChat 等），
  它是**便利**不是契约，猜错只影响观测。

## 6. 任务

- [ ] T1 `ObserverRules` + `select`（纯函数）
- [ ] T2 `resolve_observer` 三级判定
- [ ] T3 `GenericSseObserver`（复用 F03 的 SseFramer）
- [ ] T4 `GenericJsonObserver`
- [ ] T5 内置的 Raw host/path 猜测表
- [ ] T6 指标
- [ ] T7 测试：规则顺序、AND 语义、四级兜底各一例、选错只降级不影响主链路

## 7. 验收

```bash
just test -p argus-proxy observer
just test -p argus-protocol generic
```

- [ ] 首个命中生效（后面还有更"精确"的规则也不生效）
- [ ] 无任何规则 + Raw 模式 + `text/event-stream` → `GenericSse` 且有帧计数
- [ ] `GenericJson` 的输出中不含任何 value
- [ ] 故意选错 Observer：主链路字节完全不变，interaction 标 Degraded

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 三种协议的解析 | F05 / F08 |
| 规则 CRUD | F22 |
