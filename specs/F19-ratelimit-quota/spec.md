# F19 · 限流与软配额

| | |
|---|---|
| ID | F19 |
| 里程碑 | M2 |
| 依赖 | F13 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/09-auth.md` §8；`docs/12-reliability.md` §6.3 |
| 宪法条款 | C14, C16, C17 |

## 1. 目标

实现三个维度的准入控制。**必须诚实地把"软配额"的一致性模型写进用户文档**——含糊的配额承诺是运维灾难的来源。

## 2. 范围

**In scope**：GCRA 本地限流、token/cost 软配额、写后核对同步、`soft_threshold` 收紧、可选 Redis 与降级、429 响应语义。

**Out of scope**：Control Plane 的配额配置 UI（→ F22）、成本告警（Phase 3）。

## 3. 需求 · 限流

- **R1** THE SYSTEM SHALL 用进程内 GCRA 实现请求速率限流，每 key 状态为一个 `u64`。
- **R2** WHEN 触发限流 THE SYSTEM SHALL 返回 429 并给出**精确的** `Retry-After`（GCRA 可算出）。
- **R3** 限流检查 SHALL 在 p50 < 100 ns 内完成。
- **R4** THE SYSTEM SHALL 在文档中明示：多实例部署下实际速率 = 配置值 × 实例数，除非启用 Redis。

## 4. 需求 · 软配额

- **R5** THE SYSTEM SHALL 用 **check-then-record（写后核对）** 而非预扣。
- **R6** WHEN 请求前用量已达上限 THE SYSTEM SHALL 返回 429 + `x-argus-quota-reset`。
- **R7** WHEN 请求完成 THE SYSTEM SHALL 把实际 usage 累加到内存计数器。
- **R8** THE SYSTEM SHALL 每 `sync_interval`（默认 10s）与 PG 双向同步：flush 本实例增量、拉取其他实例用量。
- **R9** WHEN 用量达到 `soft_threshold`（默认 0.9）THE SYSTEM SHALL 开始优先拒绝 `max_tokens` 大的请求，让小请求继续通过。
- **R10** THE SYSTEM SHALL 允许超出上限至 `overshoot_tolerance`（默认 1.05）才硬拒。
- **R11** IF PG 不可用 THEN THE SYSTEM SHALL 基于内存中最后已知用量继续检查（fail-open，宪法 C17）。
- **R12** THE SYSTEM SHALL 暴露 `argus_quota_sync_lag_seconds`；超过 60 秒说明同步 task 卡死。
- **R13** 成本 SHALL 用 `micro_usd` 整数。
- **R14** THE SYSTEM SHALL 区分 `usage_source = estimated` 的用量，且在配额查询 API 中可分别统计（宪法 C14）。

## 5. 需求 · Redis（可选）

- **R15** WHERE `redis.enabled = true` THE SYSTEM SHALL 用 Redis 做跨实例限流与配额同步（间隔可降到 1s）。
- **R16** IF Redis 不可用 THEN THE SYSTEM SHALL **降级为本地限流并继续服务**，计 `argus_redis_degraded_total`，**绝不拒绝请求**（宪法 C17）。
- **R17** Redis SHALL NOT 成为启动的硬依赖。

## 6. 接口契约

```rust
// argus-proxy/src/quota/mod.rs
pub struct Admission { rate: RateLimiters, quota: QuotaTracker }
impl Admission {
    /// P4 阶段。同步、无 IO。
    pub fn check(&self, p: &PrincipalCtx, demand: &RequestDemand) -> Result<(), AdmissionError>;
    /// 请求完成后由 Observer/Sink 调用
    pub fn record(&self, p: &PrincipalCtx, usage: &ResolvedUsage, cost_micro_usd: u64);
}

pub enum AdmissionError {
    RateLimited { retry_after: Duration },
    QuotaExceeded { kind: QuotaKind, reset_at: SystemTime },
    Shed,                                  // load shedding（F20）
}

// argus-proxy/src/quota/backend.rs
pub trait QuotaBackend: Send + Sync {
    async fn flush(&self, deltas: &[UsageDelta]) -> Result<(), BackendError>;
    async fn fetch(&self, scopes: &[Scope]) -> Result<Vec<UsageSnapshot>, BackendError>;
}
pub struct PgQuotaBackend;      // 默认
pub struct RedisQuotaBackend;   // 可选，失败时上层降级
```

## 7. 实现要点

- **R5 的理由必须在代码注释里写清楚**：硬配额需要预扣，而预扣必须按 `max_tokens`（最坏情况）估算，会大量误拒；且需要分布式锁，每请求一次同步往返。软配额是权衡后的刻意选择。
- `QuotaTracker` 的内存结构：`DashMap<Scope, AtomicCounters>`。`check` 只读，`record` 只做 `fetch_add`，均无锁竞争。
- **R9 的实现**：`soft_threshold` 触发后，拒绝 `demand.max_tokens_requested > remaining × 0.5` 的请求。这让配额耗尽的过程平滑而非断崖。
- Redis 降级（R16）用 `match ... Err(_) => local_fallback()`，**不重试、不阻塞**。
- `usage_counters` 表每实例一行（PK 含 `instance_id`），避免跨实例写冲突（F12 已建表）。
- **R4/R10 的文档化是本切片的交付物之一**，不只是代码。

## 8. 任务

- [ ] T1 `quota/rate.rs`：GCRA（governor）+ 精确 `Retry-After`
- [ ] T2 `quota/tracker.rs`：内存计数器 + `check` / `record`
- [ ] T3 `quota/sync.rs`：周期 flush + fetch + lag 指标
- [ ] T4 `quota/backend.rs`：`QuotaBackend` trait + `PgQuotaBackend`
- [ ] T5 `soft_threshold` 收紧策略（R9）+ `overshoot_tolerance`
- [ ] T6 `RedisQuotaBackend`（feature-gated）+ 降级路径
- [ ] T7 接入主链路 P4 阶段
- [ ] T8 `record` 接入 AuditSink（成本计算已在 F12）
- [ ] T9 指标（R12/R16）
- [ ] T10 **撰写用户文档中的一致性声明**（`docs/09` §8.4 的原文）
- [ ] T11 测试：§9

## 9. 验收

```bash
cargo test -p argus-proxy quota
cargo test -p argus-proxy --features redis quota_redis
```

- [ ] GCRA：配置 10 req/s，第 11 个请求 429 且 `Retry-After` 精确
- [ ] bench：`check()` p50 < 100 ns（R3）
- [ ] token 配额耗尽 → 429 + `x-argus-quota-reset`
- [ ] **超额量化**：并发 50 个请求同时通过检查，测量实际超额量并记录到测试输出（验证 `docs/09` OQ-2）
- [ ] `soft_threshold` 触发后，大请求被拒、小请求通过
- [ ] PG 停止 → 配额检查基于陈旧数据继续工作，**不拒绝服务**（R11）
- [ ] 同步 task 被人为阻塞 → `argus_quota_sync_lag_seconds` 上升
- [ ] Redis 停止 → 自动降级本地，`argus_redis_degraded_total` 递增，**请求成功率不变**（R16）
- [ ] Redis 未配置时启动正常（R17）
- [ ] 成本 `SUM()` 无浮点误差
- [ ] `estimated` 与 `reported` 用量可分别查询（R14）

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| Load shedding（`AdmissionError::Shed`） | F20 |
| Control Plane 的配额配置与查询 | F22 |
| 成本告警与预算通知 | Phase 3 |
