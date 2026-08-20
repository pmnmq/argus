# F16 · 多 Provider 路由与 Retry 状态机

| | |
|---|---|
| ID | F16 |
| 里程碑 | M2 |
| 依赖 | F14 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/08-provider-profiles.md` §5,§7,§9；`docs/06-data-plane.md` §7 |
| 宪法条款 | C18 |


> ⚠️ **v2 变更**：跨 Provider fallback **整套删除**（`docs/02-routing-model.md` §7）。
> 客户端已在 URL 里指名 provider，悄悄发到别处会让计费与行为都不可预测。
> 剩下的是**同一个 Profile 内**的重试。`max_attempts` 从 3 降到 2——
> 三次都打同一个地方，收益递减而重复计费风险线性增长。
## 1. 目标

实现 S0–S3 状态机驱动的重试。**重试的是同一个 Provider Profile**——v2 没有跨 Profile fallback（`docs/02` §7）。**S3 后永不重试**是不可配置的硬约束。

## 2. 范围

**In scope**：S0–S3 retry 决策表、退避与抖动、三重预算（次数/时间/全局令牌桶）、`AttemptRecord` 链、重试可观测性响应头。

**已删除（v2）**：有序候选列表、`WeightedRandom`/`RoundRobin`、会话粘性、跨 Profile fallback。理由见 `docs/02-routing-model.md` §7 与 ADR-003。

**Out of scope**：熔断与 throttle（→ F17，本切片预留接口）、`LeastLatency`（Phase 2）、成本感知路由（Phase 3）。

## 3. 需求 · 候选列表

- **R1** `Router::resolve` SHALL 返回**有序候选列表**而非单个决定。
- **R2** THE SYSTEM SHALL 按 priority 分组，组内按策略选择。
- **R3** THE SYSTEM SHALL 支持 `WeightedRandom`（默认）与 `RoundRobin`。
- **R4** `WeightedRandom` SHALL 用预计算的累积权重数组 + 二分查找，**每请求零分配**。
- **R5** ~~会话粘性~~ **已删除**（没有候选集，粘性无意义）。原文：WHERE `routing.sticky = "conversation"` THE SYSTEM SHALL 用 rendezvous hashing 按 `conversation_id` 选择 binding，以保护 provider 侧 prompt cache。
- **R6** ~~粘性不凌驾可用性~~ **已删除**，随 R5。
- **R7** 候选列表 SHALL 已应用 Key 白名单、协议匹配、能力过滤（F14 已有）。

## 4. 需求 · Retry 决策（`docs/08` §7.1 表）

- **R8** WHEN S0（connect/DNS/TLS 失败）THE SYSTEM SHALL 立即尝试下一候选。
- **R9** WHEN S1（已发出请求，headers 超时或连接断开）THE SYSTEM SHALL 按 `retry.on_no_response`（默认 true）决定，且**必须记录 `retry_after_send = true`** 供计费核查。
- **R10** WHEN S2 且 status ∈ {408, 429, 500, 502, 503, 504} THE SYSTEM SHALL **重试同一个 Profile**（带退避），次数受 `max_attempts` 约束（默认 2，含首次）。
- **R11** WHEN S2 且 status ∈ {400, 401, 403, 404, 413, 422} THE SYSTEM SHALL **不重试**，原样透传 provider 错误。
- **R12** WHEN status = 401/403 THE SYSTEM SHALL 额外记 `argus_provider_auth_failure_total` 并触发告警级日志。
- **R13** WHEN 已进入 S3 THE SYSTEM SHALL **永不** retry。此行为不可配置（宪法 C18）。
- **R14** THE SYSTEM SHALL 遵守三重预算：`max_attempts`、`max_attempts_per_provider`、时间 `budget`。
- **R15** THE SYSTEM SHALL 维护全局重试令牌桶（默认 ≤ 总请求量的 10%），耗尽时不再重试。
- **R16** 退避 SHALL 带抖动（`jitter = 0.3`）防惊群。

## 5. 需求 · 可观测

- **R17** THE SYSTEM SHALL 在响应头暴露 `x-argus-provider`、`x-argus-attempts`、`x-argus-fallback-from`。
- **R18** THE SYSTEM SHALL 把完整 `AttemptRecord` 链落库到 `requests.attempt_chain`。
- **R19** THE SYSTEM SHALL 提供 `argus_fallback_total{from,to,reason}` 与 `argus_retry_after_send_total{provider}`。

## 6. 接口契约

```rust
impl Router {
    pub fn resolve(&self, ctx: &RequestCtx, demand: &RequestDemand)
        -> Result<Candidates, RouteError>;
}
pub struct Candidates { ordered: SmallVec<[RouteDecision; 3]>, strategy: RoutingStrategy }
impl Candidates { pub fn next(&mut self) -> Option<&RouteDecision>; }

// argus-proxy/src/upstream/retry.rs
pub struct RetryLoop<'a> {
    candidates: Candidates,
    budget: RetryBudget,          // attempts + 时间 + 全局令牌桶
    attempts: SmallVec<[AttemptRecord; 3]>,
    deadline: Instant,
}
impl RetryLoop<'_> {
    /// 唯一的重试入口。内部严格遵守 §4 的决策表。
    pub async fn execute(&mut self, up: &Upstream, req: &Request<Full<Bytes>>)
        -> Result<(Response<Incoming>, RouteDecision), ProxyError>;
}

/// 决策表的纯函数形式 —— 便于逐格单测
pub fn retry_decision(stage: UpstreamStage, status: Option<StatusCode>, cfg: &RetryPolicy)
    -> RetryDecision;
pub enum RetryDecision { NextCandidate, SameCandidate, Stop }
```

## 7. 实现要点

- **把决策表做成纯函数 `retry_decision`**（不是散在循环里的 if），这样 `docs/08` §7.1 的表可以逐格写测试。
- **R13 的实现要在类型层面兜底**：`retry_decision` 对 `stage == S3Committed` 无条件返回 `Stop`，并加 `debug_assert!`。同时 `ProxyError::is_retryable()`（F02）已有同样保证，两处都要有。
- S2 fallback 的关键在 F14 已做的 R15：`send()` 返回时 body 未被 poll，`drop(resp)` 即可放弃。本切片只是在这个检查点上加决策。
- 全局重试令牌桶用 `governor`，容量按"总请求速率 × 10%"，与限流器分开实例。
- 粘性用 rendezvous hashing（`max(hash(conv_id, binding_id))`）而非一致性哈希——候选数很少（< 10），rendezvous 更简单且分布更好。
- `AttemptRecord` 用 `SmallVec<[_; 3]>`：绝大多数请求只有 1 次 attempt，不分配。

## 8. 任务

- [ ] T1 `router/candidates.rs`：分组、排序、`Candidates` 迭代
- [ ] T2 `router/strategy.rs`：`WeightedRandom`（预计算累积权重）+ `RoundRobin`
- [ ] T3 `router/sticky.rs`：rendezvous hashing + 健康度让位
- [ ] T4 `upstream/retry.rs`：`retry_decision` 纯函数（决策表）
- [ ] T5 `upstream/retry.rs`：`RetryLoop::execute` + 三重预算
- [ ] T6 `upstream/budget.rs`：全局重试令牌桶
- [ ] T7 退避 + 抖动
- [ ] T8 `AttemptRecord` 收集 + 响应头 + 落库
- [ ] T9 指标接线（R19）
- [ ] T10 测试：§9

## 9. 验收

```bash
cargo test -p argus-proxy retry router
```

- [ ] **决策表逐格测试**：`docs/08` §7.1 的每一行 × 每个 status 类别
- [ ] **S3 铁律**：任意配置下，`stage = S3Committed` 的 `retry_decision` 恒为 `Stop`（proptest 遍历配置空间）
- [ ] 429 → fallback，客户端收到 200 且 `x-argus-fallback-from` 存在
- [ ] 401 → **不** fallback，原样透传 provider 的 401 + `argus_provider_auth_failure_total` 递增
- [ ] `max_attempts_per_provider = 1` → 同一 provider 不被连试两次
- [ ] 时间 `budget` 耗尽 → 立即返回上一个错误
- [ ] 全局令牌桶耗尽 → 不重试，`argus_retry_budget_exhausted_total` 递增
- [ ] S1 重试 → audit 中 `retry_after_send = true`
- [ ] 粘性：同一 `conversation_id` 的 10 次请求命中同一 binding；该 binding 标记不健康后自动转移
- [ ] `WeightedRandom` 在 10000 次抽样下的分布符合权重（卡方检验）
- [ ] bench：`resolve()` 零分配

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| 熔断状态查询（本切片预留 `is_available()` 钩子） | F17 |
| 429 的 `throttled_until` | F17 |
| `LeastLatency` / `LeastPending` | Phase 2 |
| 成本感知路由 | Phase 3 |
