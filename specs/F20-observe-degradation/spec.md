# F20 · 观测降级与资源上限

| | |
|---|---|
| ID | F20 |
| 里程碑 | M2 |
| 依赖 | F06 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/05-streaming.md` §6.3；`docs/12-reliability.md` §4–5,§7 |
| 宪法条款 | C9, C10, C11, C12, C13 |

## 1. 目标

实现三层降级与全部资源隔板，让"最坏情况内存可计算"从设计变成事实。

**核心原则**：降级优先在**请求粒度**，逐字节丢弃只是最后防线。半个 interaction 没有分析价值。

## 2. 范围

**In scope**：`ObserveMode` 准入决策、`ObserverLoad` 压力计算、确定性抽样、大请求 Semaphore、load shedding、RSS watermark（cgroup 感知）、fd 提升、可选 `runtime.split`。

**Out of scope**：安全策略强制 Full 观测的联动（→ F24 提供 `force_full_observation` 标志）。

## 3. 需求 · 观测降级

- **R1** THE SYSTEM SHALL 在 P7（Route）阶段决定 `ObserveMode ∈ {Full, Lite, Off}`。
- **R2** `ObserverLoad::pressure()` SHALL 由三个信号合成：活跃 observer 数/上限、AuditSink 队列深度/容量、近 1 分钟 tee drop rate。
- **R3** THE SYSTEM SHALL 按压力分级：`Normal(<0.60) → Full`、`Elevated(0.60–0.85) → 抽样 Full 其余 Lite`、`High(0.85–0.95) → Lite`、`Critical(>0.95) → Off`。
- **R4** 抽样 SHALL 是**确定性的**（`hash(trace_id) % 10000 < rate`），保证同一 conversation 的多轮请求观测状态一致且可复现。
- **R5** WHERE 策略要求 `force_full_observation` THE SYSTEM SHALL 忽略压力降级（安全优先）。
- **R6** `Lite` 模式 SHALL 只累积结构性信息（usage / tool 元数据 / finish），不保留正文与 raw。
- **R7** `Off` 模式 SHALL 不挂载 Tee，只记录请求级元数据。
- **R8** THE SYSTEM SHALL 落库 `observe_mode` 并暴露 `argus_observe_mode_total{mode}`（宪法 C13）。

## 4. 需求 · 资源隔板

- **R9** THE SYSTEM SHALL 实施 `docs/12` §7.1 的全部隔板：大请求 Semaphore(32)、observer 数上限、sink 队列、tee 双预算、PG 双池、LLM detector 并发。
- **R10** 每个隔板满时 THE SYSTEM SHALL **立即拒绝**，不排队（宪法 C12）。
- **R11** WHEN body 大小超过 `large_threshold`（256 KiB）THE SYSTEM SHALL 需先获取大请求许可；获取失败返回 503。
- **R12** WHEN 并发请求数超过 `max_concurrent_requests` THE SYSTEM SHALL load shed（503 + `Retry-After: 1`）并计 `argus_load_shed_total`。
- **R13** THE SYSTEM SHALL NOT 为慢客户端分配发送缓冲（宪法 C11）；用 `client_write_idle_timeout` 兜底。

## 5. 需求 · 内存压力

- **R14** THE SYSTEM SHALL 周期（5s）采集 RSS，并**从 cgroup 读取限额**（v2 `memory.max` / v1 `memory.limit_in_bytes`），而非系统总内存。
- **R15** WHEN RSS 越过 `high_watermark`（70%）THE SYSTEM SHALL：强制 `Pressure::Critical`、禁止新的大请求、把 `max_active_streams` 收紧 10%。
- **R16** WHEN RSS 回落到 `low_watermark`（55%）THE SYSTEM SHALL 恢复。
- **R17** THE SYSTEM SHALL 暴露 `argus_memory_pressure` 与 `argus_rss_bytes`。
- **R18** THE SYSTEM SHALL 在启动时尝试提升 `RLIMIT_NOFILE`；低于 `min_nofile` 时打印带命令的修复指引。

## 6. 需求 · 可选 Runtime 隔离

- **R19** WHERE `runtime.split = true` THE SYSTEM SHALL 用独立 Tokio Runtime 跑 Observer 与 AuditSink。
- **R20** `runtime.split` SHALL 默认关闭。
- **R21** WHEN 启用 split THE sqlx pool SHALL 在 observer runtime 上创建（pool 绑定 runtime）。

## 7. 接口契约

```rust
// argus-proxy/src/observer/load.rs
pub struct ObserverLoad {
    active: Arc<AtomicU64>, max_active: u64,
    sink_depth: Arc<AtomicU64>, sink_cap: u64,
    drop_rate: Arc<AtomicU32>,          // ppm，滑动窗口
    forced: Arc<AtomicU8>,              // RSS watermark 可强制置 Critical
}
impl ObserverLoad {
    pub fn pressure(&self) -> Pressure;
    pub fn decide(&self, ctx: &RequestCtx, policy: &Policy) -> ObserveMode;
    pub fn force_pressure(&self, p: Pressure);
}
pub enum Pressure { Normal, Elevated, High, Critical }

// argus-proxy/src/limits/mod.rs
pub struct Bulkheads {
    large_requests: Semaphore,
    concurrent_requests: Arc<AtomicU64>,
    effective_max_streams: AtomicU64,
}
impl Bulkheads {
    pub fn try_admit(&self, size_hint: u64) -> Result<AdmitGuard, AdmissionError>;
}

// argus-proxy/src/limits/memory.rs
pub struct MemoryWatchdog { cgroup_limit: u64, high: u64, low: u64 }
impl MemoryWatchdog { pub fn detect_limit() -> u64; /* cgroup v2 → v1 → 系统总内存 */ }
```

## 8. 实现要点

- **R1 的位置很关键**：必须在 P7 决定，因为 (a) 此时已知策略 (b) 尚未挂 Tee。挂了 Tee 再降级就晚了。
- **R4 用 `trace_id` 而非 `request_id`** 做哈希：同一 trace 的多次 attempt 观测状态一致。
- `Lite` 模式的实现不是"少存"，而是**在 Accumulator 层就不累积正文**（`AccumulatorConfig` 加 `mode` 字段），否则内存并没省。
- **R14 的 cgroup 检测容易做错**：容器里读 `/proc/meminfo` 会拿到宿主机的内存。必须按 v2 → v1 → fallback 的顺序探测。
- R15 的"收紧 10%"是渐进的：每次触发收紧当前值的 10%，不是一次砍到 90%。避免抖动导致连续收紧。
- `runtime.split` 默认关闭且 §7 只做基本验证——`docs/12` OQ-6 明确它的真实收益未验证。**不要在没有 profile 数据时打开它**。

## 9. 任务

- [ ] T1 `observer/load.rs`：三信号压力计算 + 分级
- [ ] T2 `observer/sample.rs`：确定性抽样
- [ ] T3 `ObserveMode` 在 P7 的决策 + 接入 Tee 挂载
- [ ] T4 `AccumulatorConfig.mode`：Lite 模式不累积正文（改 `argus-protocol`）
- [ ] T5 `limits/bulkhead.rs`：大请求 Semaphore + `AdmitGuard`
- [ ] T6 `limits/shed.rs`：load shedding layer（最外层 Tower）
- [ ] T7 `limits/memory.rs`：cgroup 检测 + watchdog + 强制降级
- [ ] T8 `rlimit` 提升（R18，与 F15 的 boot 合并）
- [ ] T9 `runtime.split`（feature/config gated）
- [ ] T10 指标（R8/R12/R17）
- [ ] T11 测试：§10

## 10. 验收

```bash
cargo test -p argus-proxy limits observer::load
```

- [ ] 压力从 0 升到 1.0，`ObserveMode` 依次经过 Full → 抽样 → Lite → Off
- [ ] **抽样确定性**：同一 `trace_id` 重复决策 1000 次，结果恒定
- [ ] 同一 conversation 的 5 轮请求，观测状态一致
- [ ] `force_full_observation` 在 Critical 压力下仍返回 Full（R5）
- [ ] Lite 模式下 `request_payloads.blocks` 为空但 `usage` / `tool_calls` 完整
- [ ] Off 模式下 Tee 未挂载（断言 `TeeStats.frames == 0`）
- [ ] 33 个并发大请求 → 第 33 个 503，**进程 RSS 不超过 32 × 8 MiB + 基线**
- [ ] 并发超限 → 503 + `Retry-After: 1`，**不排队**（响应时间 < 10 ms）
- [ ] cgroup 限额检测：在 `--memory=1g` 的容器中断言 `detect_limit() == 1 GiB`
- [ ] RSS 越过 high watermark → `argus_memory_pressure = 1`、大请求被拒、`ObserveMode` 转 Off
- [ ] RSS 回落 → 自动恢复
- [ ] 慢客户端（连接后不读）100 个 → RSS 增长 < 100 × 64 KiB（宪法 C11）
- [ ] fd 限制过低时打印含 `ulimit -n` / `LimitNOFILE=` 的指引

## 11. 交给后续

| 不做的事 | 承接 |
|---|---|
| `force_full_observation` 的策略来源 | F24 |
| 混沌验证（内存压力下的行为） | F21 |
| `runtime.split` 的收益实测 | F28 |
| per-workspace 观测配额 | Phase 3（`docs/05` OQ-4） |
