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
// argus-proxy/src/observer/rules.rs
pub struct ObserverRules { rules: Vec<ObserverRule> }   // 有序
pub struct ObserverRule {
    pub profile: Option<CompactString>,
    pub host_suffix: Option<CompactString>,
    pub path_prefix: Option<CompactString>,
    pub method: Option<Method>,
    pub content_type: Option<CompactString>,   // **请求**的
    pub observer: ObserverKind,
}
impl ObserverRules {
    /// 首个命中生效
    pub fn select(&self, ctx: &SelectCtx<'_>) -> Option<ObserverKind>;
}
/// ResolvedTarget 里没有的那部分请求事实
pub struct RequestFacts<'a> { pub method: &'a Method, pub content_type: Option<&'a str> }
pub struct SelectCtx<'a> { /* profile / host / path / method / content_type */ }

// argus-proxy/src/observer/mod.rs
/// 请求时能定下来的前三级。None = 等响应 header 再说
pub fn provisional(t: &ResolvedTarget, r: &ObserverRules, req: &RequestFacts<'_>)
    -> Option<ObserverKind>;
/// 收到响应 header 之后。定不下来按 Content-Type 兜底；定下来了只准收窄
pub fn resolve_observer(provisional: Option<ObserverKind>, ct: Option<&HeaderValue>)
    -> ObserverKind;
pub fn guess_raw_shape(path: &str) -> Option<ObserverKind>;
pub enum FallbackReason { UnknownShape, ContentTypeMismatch, NoContentType }

// argus-protocol/src/observer/generic.rs
pub struct GenericSseObserver { /* frames / data_bytes / comments / saw_done / dropped */ }
impl FrameSink for GenericSseObserver { .. }          // 直接挂在 F03 的 SseFramer 后面
pub struct GenericJsonObserver { /* shape / top_level_keys / truncated / body_bytes */ }
pub enum JsonShape { Object, Array, Scalar, Malformed }
```

四处与初稿的偏差：

- **`resolve_observer` 拆成了两个函数。** 初稿的签名一个函数吃下四级，
  但 §5 自己就说了前三级在请求时定、第四级要等响应 header。硬塞进一个
  函数意味着调用方得把 `ResolvedTarget` 一路带到收到响应 header 的那一刻，
  而那时它真正需要的只是「上一步定的是什么」。
- **多了 `RequestFacts`。** 规则要匹配 `method` 与请求 `content_type`，
  这两样 `ResolvedTarget` 里没有（它是 URL 的解析结果，不是请求的全貌）。
- **`GenericSseObserver` 直接 impl `FrameSink`**，不另造一套接口 ——
  它就挂在 F03 的 `SseFramer` 后面。字段也从 `{frames, bytes}` 扩成五个：
  心跳注释要跟事件分开计（算进去会让「这条流产出了多少」随连接时长膨胀），
  gap 要单独记（那是 `Integrity::Degraded` 的依据）。
- **`GenericJsonObserver` 多了 `shape` 与 `truncated_keys`。** 顶层不是
  对象时没有 key 可记，但「是什么形状」本身有用；而 key 的条数必须有上界，
  否则一个有一百万个顶层 key 的响应会让观测旁路吃掉一大块内存（宪法 C9
  的同一条理由）。

## 5. 实现要点

- **Content-Type 兜底发生在收到响应 header 之后**，而前三级在请求时就能定。
  实现上先给一个"暂定值"，收到 header 后可以**收窄**到 Generic，但不能改成
  另一个语义 Observer——那会让已经解析的事件与后面的不一致。
- `GenericJson` 只记顶层 key 名，**不记 value**（宪法 C27）。
- Raw 模式的 host/path 猜测用一张内置的小表（`api.openai.com` → OpenAiChat 等），
  它是**便利**不是契约，猜错只影响观测。

## 6. 任务

- [x] T1 `observer/rules.rs`：`ObserverRules` + `select`（纯函数）
- [x] T2 `observer/mod.rs`：`provisional`（前三级）+ `resolve_observer`（第四级
      与收窄）。拆成两步的理由见 §4
- [x] T3 `GenericSseObserver`，直接 impl F03 的 `FrameSink`
- [x] T4 `GenericJsonObserver`。value 一律吃进 `serde::de::IgnoredAny` ——
      「不记 value」靠类型保证，不靠自觉
- [x] T5 `RAW_SHAPE_HINTS`：**只按 path 前缀猜，不按 host**。形状是比 host
      强得多的信号（`docs/09` §4.3）
- [x] T6 `argus_observer_selected_total{kind}` 与
      `argus_observer_fallback_total{reason}`
- [x] T7 规则顺序、AND 语义、四级各一例、喂错东西只降级不报错

## 7. 验收

```bash
just test -p argus-proxy observer
just test -p argus-protocol generic
```

- [x] 首个命中生效（后面还有更"精确"的规则也不生效）
- [x] 无任何规则 + Raw 模式 + 认不出形状 + `text/event-stream` → `GenericSse`；
      挂在 `SseFramer` 后面能数出帧、心跳与 `[DONE]`
- [x] `GenericJson` 的输出中不含任何 value —— 断言 `{:?}` 里既没有 prompt
      正文也没有被塞进 body 的凭据，但 key 名要在
- [x] 故意喂错 body：观测标记为不完整、字节数照记、不 panic 不报错
- [ ] **主链路字节完全不变** —— 主链路还不存在（F06 才有透传），本切片验不了
- [ ] **interaction 标 `Integrity::Degraded`** —— `ObservedInteraction` 属于
      F04，本切片只提供 `is_complete()` 这个判据
- [x] 全 workspace 测试 208 → 240（新增 32 个：14 单测 + 18 集成）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 三种协议的解析 | F05 / F08 |
| 把两个 Generic 接进 `AnyObserver` 枚举分发 | F05（§4 已按 R04 更新） |
| `is_complete()` → `Integrity::Degraded` 的接线 | F04 |
| 「主链路不受影响」的端到端验证 | F06 |
| 规则 CRUD | F22 |
