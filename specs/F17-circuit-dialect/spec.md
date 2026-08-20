# F17 · 熔断、限流退避与 Provider 方言

| | |
|---|---|
| ID | F17 |
| 里程碑 | M2 |
| 依赖 | F16 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/08-provider-profiles.md` §8；`docs/09-observers.md` §6.4；`docs/06-data-plane.md` §7.4 |
| 宪法条款 | C1, C17 |

## 1. 目标

让路由具备自愈能力：被动熔断、429 独立退避、方言配置。**方言只影响观测，绝不改写转发字节。**

## 2. 范围

**In scope**：`CircuitBreaker`（被动）、429 的 `throttled_until`、可选主动探测、`Dialect` 加载与应用、`first_event_gate`。

**Out of scope**：Redis 共享熔断状态（Phase 2）、`LeastLatency`（Phase 2）。

## 3. 需求 · 熔断

- **R1** THE SYSTEM SHALL 用真实流量结果驱动熔断（被动），不以主动探测为主要机制。
- **R2** THE SYSTEM SHALL 在滑动窗口内同时满足 `min_requests` 与 `failure_rate` 才打开熔断。
- **R3** 计入失败的 SHALL 为：连接/TLS/DNS 错误、5xx、408、headers 超时。
- **R4** **不计入**失败的 SHALL 为：4xx（除 408/429）、S3 阶段的流中断、被 PreflightGuard 拒绝的请求。
- **R5** WHEN 熔断打开 THE SYSTEM SHALL 在选路时跳过该候选，且**不产生网络请求**。
- **R6** WHEN `open_duration` 到期 THE SYSTEM SHALL 进入半开并放行 `half_open_probes` 个请求试探。
- **R7** THE SYSTEM SHALL 暴露 `argus_circuit_state{provider}`。
- **R8** IF 全部候选均熔断 THEN THE SYSTEM SHALL 返回 503 并给出 `Retry-After` = 最近熔断器的剩余时间。

## 4. 需求 · 429 退避

- **R9** 429 SHALL NOT 计入主熔断器（`docs/08` §8.2）。
- **R10** WHEN 收到 429 THE SYSTEM SHALL 设置该 provider 的 `throttled_until`，优先采用响应的 `Retry-After` 头。
- **R11** IF 无 `Retry-After` THEN THE SYSTEM SHALL 使用指数退避的估算值。
- **R12** WHILE provider 处于 throttled THE SYSTEM SHALL 在选路时跳过它，但状态显示为"限流"而非"故障"。
- **R13** IF 全部候选均 throttled THEN THE SYSTEM SHALL 返回 503 + `Retry-After` = 最小的剩余时间。

## 5. 需求 · 方言

- **R14** THE SYSTEM SHALL 支持 `docs/09` §6.4 的 `Dialect` 配置项。
- **R15** `Dialect` SHALL 只影响 `ProtocolObserver` 的观测行为与 header 注入，**绝不改写转发的 body 字节**（宪法 C1）。
- **R16** WHERE `dialect.reasoning_field` 已配置 THE SYSTEM SHALL 把该字段映射为 `BlockKind::Reasoning`。
- **R17** WHERE `dialect.emits_done_sentinel = false` THE SYSTEM SHALL 不因缺少 `[DONE]` 而报异常。
- **R18** `dialect.required_headers` SHALL 被无条件注入（如 `anthropic-version`）。
- **R19** IF 方言配置错误 THEN 影响 SHALL 限于观测数据不准，代理必须不受影响。
- **R20** WHERE `first_event_gate = true`（per-provider，默认 false）THE SYSTEM SHALL 在写出 client header 前先 poll 出首个 body frame 以判定 "200 + error event"（`docs/06` §7.4）。
- **R21** THE SYSTEM SHALL 在文档与配置校验中明示 `first_event_gate` 会增加 TTFT。

## 6. 接口契约

```rust
// argus-proxy/src/router/health.rs
pub struct ProviderHealth {
    circuit: CircuitBreaker,
    throttled_until: AtomicU64,      // epoch millis，0 = 未限流
    probe_healthy: AtomicBool,
}
impl ProviderHealth {
    pub fn is_available(&self, now: Instant) -> Availability;   // F16 预留的钩子
    pub fn record(&self, outcome: &AttemptOutcome);
}
pub enum Availability { Ok, Throttled { until: Instant }, CircuitOpen { until: Instant } }

pub struct CircuitBreaker { state: AtomicU8, window: SlidingWindow, opened_at: AtomicU64 }
```

## 7. 实现要点

- **R4 的边界要小心**：S3 阶段的流中断很可能是客户端断开而非 provider 故障。把它计入失败会导致误熔断。
- 滑动窗口用固定桶数组（如 30 个 1 秒桶），避免每次记录都分配。
- `throttled_until` 与熔断是**两个独立状态**（R9）。选路时二者都要查，但语义与展示不同。
- 主动探测（可选）只用免费端点（`GET /v1/models`），**绝不做 chat 探测**（那会真的花钱，`docs/08` §8.3）。探测结果只作降权软信号，不直接熔断。
- **R19 的验证方式**：故意配一个错误的 `reasoning_field`，断言代理仍 200 且 body 逐字节正确，只是 `blocks` 里少了 reasoning。
- `first_event_gate` 的实现要复用 F14 的 S2 检查点，在 commit 之前多 poll 一次。

## 8. 任务

- [ ] T1 `router/circuit.rs`：`CircuitBreaker` + 滑动窗口
- [ ] T2 失败分类（R3/R4）
- [ ] T3 半开与探测放行
- [ ] T4 `router/throttle.rs`：429 的 `throttled_until` + `Retry-After` 解析
- [ ] T5 `ProviderHealth::is_available` 并接入 F16 的候选过滤
- [ ] T6 全候选不可用时的 503 + `Retry-After` 计算（R8/R13）
- [ ] T7 `Dialect` 加载与传递到 parser
- [ ] T8 `reasoning_field` / `emits_done_sentinel` / `required_headers` 的应用
- [ ] T9 `first_event_gate`（per-provider）
- [ ] T10 指标（R7）
- [ ] T11 测试：§9

## 9. 验收

```bash
cargo test -p argus-proxy circuit throttle dialect
```

- [ ] 19 次失败不熔断，第 20 次（达 `min_requests` 且超 `failure_rate`）熔断
- [ ] 熔断期间**零网络请求**发往该 provider（mock 侧断言）
- [ ] 半开放行恰好 `half_open_probes` 个，成功则关闭、失败则重开
- [ ] 4xx（非 408/429）不触发熔断
- [ ] 客户端断开导致的 S3 中断不触发熔断
- [ ] 429 + `Retry-After: 20` → `throttled_until` = now+20s，且熔断器状态**仍为 Closed**
- [ ] 全部 throttled → 503 且 `Retry-After` 等于最小剩余
- [ ] DeepSeek 方言：`reasoning_content` 被识别为 Reasoning block
- [ ] `emits_done_sentinel = false` 的 provider 在 EOF 时正常收尾
- [ ] **方言隔离**：配错 `reasoning_field` → 代理 200 且 body 逐字节正确（宪法 C1/R19）
- [ ] `first_event_gate = true` + "200 then error event" scenario → 成功 fallback
- [ ] `first_event_gate = false`（默认）时 TTFT 不受影响（对比测量）

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| Redis 共享熔断状态 | Phase 2 |
| `LeastLatency` 与 EWMA | Phase 2 |
| 自动方言探测 | Phase 3 |
| 混沌验证 | F21 |
