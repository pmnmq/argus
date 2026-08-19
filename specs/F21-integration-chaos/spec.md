# F21 · 集成测试与混沌验证

| | |
|---|---|
| ID | F21 |
| 里程碑 | M2（**门禁 G-M2**） |
| 依赖 | F17, F18 |
| 主 crate | `tests/` |
| 设计依据 | `docs/15-testing-strategy.md` §10 |
| 宪法条款 | C5, C7, C10, C11, C13, C17, C18, C19 |

## 1. 目标

验证宪法中**无法用单元测试证明**的那些条款——它们只在真实的故障与并发下才暴露。

## 2. 范围

**In scope**：testcontainers PG、`docs/15` §10 的全部场景、混沌注入、宪法条款的端到端断言。

**Out of scope**：性能压测（F07 已建立，F28 做回归门禁）、7 天长稳（→ F28）。

## 3. 需求

- **R1** THE SYSTEM SHALL 用 testcontainers 起真实 PostgreSQL，**不使用 mock 数据库**（SQL 方言差异是真实 bug 来源）。
- **R2** 集成测试 SHALL NOT 使用 `sleep` 等待，一律用条件轮询 + 明确超时（防 flaky）。
- **R3** THE SYSTEM SHALL 覆盖 `docs/15` §10 的全部场景表。
- **R4** THE SYSTEM SHALL 提供混沌注入能力：随机杀 PG、断 provider、注入 panic、灌满队列、施加内存压力。
- **R5** 每条被验证的宪法条款 SHALL 在测试名中标注（如 `c19_pg_outage_does_not_affect_dataplane`）。

## 4. 场景矩阵

### 4.1 依赖故障

| # | 场景 | 断言 | 条款 |
|---|------|------|------|
| S1 | PG 停止 → 100 请求 → PG 恢复 | 全部 200；数据经 spool 零丢失零重复 | C19 |
| S2 | PG 停止 + auth 缓存命中 | 200，`argus_auth_stale_hit_total` 递增 | C17/C19 |
| S3 | PG 停止 + auth 缓存未命中 | **503**（fail-close） | C17 |
| S4 | PG 慢（人为加 statement 延迟） | sink degrade，`requests` 行仍写入 | C10 |
| S5 | Redis 停止（若启用） | 降级本地限流，成功率不变 | C17 |
| S6 | spool 磁盘只读 | 服务正常 + 告警 | C19 |
| S7 | `/readyz` 在 PG 停止时 | **仍返回 200** | C19 |

### 4.2 上游故障

| # | 场景 | 断言 | 条款 |
|---|------|------|------|
| S8 | provider 429 + `Retry-After` | fallback 成功，客户端无感知，`x-argus-fallback-from` 存在 | — |
| S9 | provider 401 | **不** fallback，透传 401 + 告警指标 | — |
| S10 | provider 在 S3 中断 | 流内 error 事件；audit 记 partial + `UpstreamError` | C18 |
| S11 | provider hang（无响应） | headers 超时 → S1 → 按配置 retry，`retry_after_send` 已记录 | — |
| S12 | 全部 provider 熔断 | 503 + `Retry-After` | — |
| S13 | provider "200 then error event" + `first_event_gate` | 成功 fallback | — |

### 4.3 客户端行为

| # | 场景 | 断言 | 条款 |
|---|------|------|------|
| S14 | 客户端中途断开 | 上游连接被关闭（mock 侧断言）；audit 记 `ClientAborted` + partial | — |
| S15 | 慢客户端 ×100（连接不读） | RSS 增长 < 100 × 64 KiB；最终被 idle timeout 断开 | C11 |
| S16 | 客户端只发 header 不发 body | 408 | — |
| S17 | 8 MiB body ×33 并发 | 第 33 个 503，RSS 有界 | C12 |

### 4.4 内部故障

| # | 场景 | 断言 | 条款 |
|---|------|------|------|
| S18 | 注入 parser panic | 请求 200；`argus_observer_panic_total` +1；进程存活 | C7 |
| S19 | Tee channel 灌满 | interaction `Degraded` + `dropped_bytes > 0`；**主链路延迟不变** | C5/C10/C13 |
| S20 | Observer 人为 sleep 5s | 主链路转发耗时不变 | C5 |
| S21 | sink 队列灌满 | degrade → `requests` 保留、payload 丢弃 | C10 |
| S22 | RSS 越过 watermark | `ObserveMode` 转 Off、大请求被拒、无 OOM | C10 |
| S23 | 停机时 10 条活跃流 | 全部正常完成；partial 落库 | — |
| S24 | 停机超 `drain_timeout` | 剩余流收到 error 事件；进程按时退出 | — |

### 4.5 配置与生命周期

| # | 场景 | 断言 |
|---|------|------|
| S25 | Key 吊销 | 15 秒内全部实例拒绝 |
| S26 | 软配额超限 | 429；超额量在 `overshoot_tolerance` 内（并记录实际值） |
| S27 | 迁移未执行 | 拒绝启动 |
| S28 | 两实例共享 spool 目录 | 第二个拒绝启动 |

## 5. 接口契约

```rust
// tests/support/harness.rs
pub struct TestArgus { /* 进程或 in-process runtime */ }
impl TestArgus {
    pub async fn start(pg: &PgHandle, cfg: TestConfig) -> Self;
    pub async fn request(&self, req: TestRequest) -> TestResponse;
    pub async fn diagnostics(&self) -> Diagnostics;
    /// 条件轮询，禁止 sleep（R2）
    pub async fn wait_for<F>(&self, pred: F, timeout: Duration) -> Result<(), Timeout>;
    pub fn count_rows(&self, table: &str) -> u64;
}

// tests/support/chaos.rs
pub enum Chaos {
    StopPostgres, StartPostgres, SlowPostgres { delay: Duration },
    InjectParserPanic { after_frames: usize },
    FloodTeeChannel, FloodSinkQueue,
    MemoryPressure { target_ratio: f64 },
    KillProvider { name: &'static str },
}
impl TestArgus { pub async fn inject(&self, c: Chaos); }
```

## 6. 实现要点

- **R2 是 flaky 的主要来源**。`wait_for` 用 50 ms 轮询 + 显式超时，超时消息要包含当前状态（便于诊断）。
- 混沌注入用 feature flag（`#[cfg(feature = "chaos")]`）暴露的测试端点，**不进 release 二进制**。
- S19/S20 的"延迟不变"断言要用**统计方法**：跑 200 次取 p99，与基线比较，允许 20% 波动。单次比较会 flaky。
- S15 的 RSS 断言要减去基线并留余量（分配器碎片）。
- testcontainers 的 PG 启停较慢（~2s），把同一组场景复用一个容器，只在需要时重启。

## 7. 任务

- [ ] T1 `tests/support/harness.rs`：`TestArgus` + `wait_for`
- [ ] T2 `tests/support/pg.rs`：testcontainers 封装 + 启停控制
- [ ] T3 `tests/support/chaos.rs`：混沌注入 + 测试端点（feature-gated）
- [ ] T4 §4.1 依赖故障场景（S1–S7）
- [ ] T5 §4.2 上游故障场景（S8–S13）
- [ ] T6 §4.3 客户端行为场景（S14–S17）
- [ ] T7 §4.4 内部故障场景（S18–S24）
- [ ] T8 §4.5 配置与生命周期场景（S25–S28）
- [ ] T9 CI 接线（集成测试单独 job，允许更长超时）
- [ ] T10 flaky 治理：连续跑 20 次全绿才算通过

## 8. 验收 — 门禁 G-M2

```bash
cargo test --test integration -- --test-threads=1
for i in $(seq 20); do cargo test --test integration || exit 1; done   # 反 flaky
```

- [ ] §4 的 28 个场景全部通过
- [ ] 连续 20 次运行无 flaky
- [ ] 每个宪法条款至少被一个测试名标注覆盖：C5, C7, C10, C11, C12, C13, C17, C18, C19
- [ ] S1 的数据一致性断言精确到行数（零丢失、零重复）
- [ ] S19/S20 的延迟断言用 p99 统计而非单次
- [ ] S26 输出实际超额量（回填 `docs/09` OQ-2）

## 9. 交给后续

| 不做的事 | 承接 |
|---|---|
| 性能回归门禁 | F28 |
| 7 天长稳 | F28 |
| 安全相关场景 | F24–F26 各自的验收 |
| Control Plane 场景 | F22 |
