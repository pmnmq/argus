# 21 · 性能

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`05-transparent-proxy.md`、`10-streaming.md`、`17-reliability.md`
>
> ⚠️ **本文件里的所有数字都是「目标」，不是实测结果。**
> 区分三件事：**目标**（我们希望达到什么）、**基准方法**（怎么测）、
> **实测结果**（真的测出来是多少）。没有跑过基准就写结果，等于伪造数据。
> 实测数据由 F07（G-M0 门禁）与 F28（G-M4）回填。

---

## 1. Problem

Argus 坐在客户端和模型之间，它加的每一毫秒都是用户等待的一毫秒。而 AI 流量
有两个特点让通用网关的性能直觉失效：

1. **单请求生命周期长**（几秒到几分钟），连接数比 QPS 更重要。
2. **首 token 延迟（TTFT）比吞吐更重要**，用户对"开始出字"的感知远强于总时长。

因此性能目标不是"每秒多少请求"，而是：**加了多少 TTFT、每条流占多少内存、
同时能扛多少条流**。

---

## 2. Goals

- G-Pf1 `proxy_added_ttft` 可测量、可归因，且有硬门禁。
- G-Pf2 每条活跃流的内存占用**可计算**，不是"应该不多"。
- G-Pf3 观测开销与主链路开销可分别测量。
- G-Pf4 所有性能声明都能追溯到一次具体的基准运行。

## 3. Non-goals

- 不追求极限吞吐（这不是一个 CDN）。
- 不在没有基准的情况下声称任何性能优势。
- 不为性能牺牲透明性或正确性（优先级见 `00-overview.md` §7）。

---

## 4. 指标

> 以下清单是**代表性的，不是穷尽的**。各文档的 Failure Modes 表中出现的指标（如 `argus_framer_overflow_total`、`argus_resync_failed_total`、`argus_client_write_timeout_total`）同样属于正式指标集。实现时以 `crates/*/src/**/metrics.rs` 中的常量定义为准，并保证命名遵循 `argus_<子系统>_<对象>_<单位或 total>` 的约定。

### 4.1 主链路

| 指标 | 类型 | 标签 | 说明 |
|------|------|------|------|
| `argus_requests_total` | Counter | `protocol,model,provider,status,outcome` | |
| `argus_request_duration_seconds` | Histogram | `protocol,model,provider,stream` | 端到端 |
| `argus_ttft_seconds` | Histogram | `protocol,model,provider` | 到第一个内容 delta |
| `argus_proxy_overhead_seconds` | Histogram | `protocol,stage` | **见 §5，核心指标** |
| `argus_upstream_duration_seconds` | Histogram | `provider,endpoint,stage` | 分 S0/S1/S2/S3 |
| `argus_active_streams` | Gauge | `protocol` | 当前并发 SSE |
| `argus_bytes_forwarded_total` | Counter | `direction` | |
| `argus_tokens_total` | Counter | `workspace,model,kind` | kind=input/output/cache_read/… |
| `argus_cost_micro_usd_total` | Counter | `workspace,model,provider` | |

**标签基数警告**：`workspace` 标签在多租户下可能有上千个值。因此：
- `argus_requests_total` **不带** `workspace` 标签（用审计库做租户维度分析）。
- 只有 `argus_tokens_total` 与 `argus_cost_micro_usd_total` 带 `workspace`，且提供配置开关 `[metrics] per_workspace = false` 关闭。
- `model` 标签同理，数量可控（几十个）。

### 4.2 路由与上游

| 指标 | 类型 | 标签 |
|------|------|------|
| `argus_route_attempts_total` | Counter | `provider,outcome,stage` |
| `argus_fallback_total` | Counter | `from_provider,to_provider,reason` |
| `argus_retry_after_send_total` | Counter | `provider` ← **计费风险信号（`07` §7.3）** |
| `argus_retry_budget_exhausted_total` | Counter | |
| `argus_circuit_state` | Gauge | `provider` (0=closed,1=half,2=open) |
| `argus_provider_throttled_total` | Counter | `provider` |
| `argus_upstream_pool_connections` | Gauge | `provider,state` |
| `argus_provider_auth_failure_total` | Counter | `provider` ← 应立即告警 |
| `argus_upstream_idle_timeout_total` | Counter | `provider` — 两个 body frame 间隔超时 |
| `argus_client_write_timeout_total` | Counter | `protocol` — slow client 被断开 |

### 4.3 观测链路健康（G-Ob3）

**这组指标是本文最重要的部分。**

| 指标 | 类型 | 标签 | 含义 |
|------|------|------|------|
| `argus_observe_mode_total` | Counter | `mode` | full/lite/off 各多少 |
| `argus_tee_dropped_bytes_total` | Counter | `protocol` | 因背压丢的字节 |
| `argus_tee_drop_events_total` | Counter | `protocol` | 丢弃发生次数 |
| `argus_interaction_integrity_total` | Counter | `integrity` | complete/degraded/metadata_only |
| `argus_observer_panic_total` | Counter | `protocol` | **应恒为 0** |
| `argus_parse_failure_total` | Counter | `protocol,reason` | |
| `argus_unknown_event_total` | Counter | `protocol,event_name` | **前向兼容的关键信号** |
| `argus_unknown_finish_reason_total` | Counter | `protocol,reason` | |
| `argus_accumulator_anomaly_total` | Counter | `protocol,anomaly` | 乱序/重复/mismatch |
| `argus_framer_overflow_total` | Counter | `protocol` | |
| `argus_active_observers` | Gauge | | |
| `argus_observer_pressure` | Gauge | | 0.0–1.0，驱动降级 |
| `argus_accumulator_mismatch_total` | Counter | `protocol` | 与 `response.completed` 交叉校验不一致（`06` §7.4）——parser 回归的最佳信号 |
| `argus_invalid_utf8_total` | Counter | `protocol` | 上游发送非法 UTF-8 |
| `argus_resync_failed_total` | Counter | `protocol` | gap 后找不到帧边界，放弃该流观测 |

**`argus_unknown_event_total{event_name="..."}` 的运维价值**：provider 上线新功能时，这个指标会立刻出现新的 `event_name` 值。这是"provider 改了 API"的最早信号，比等用户报障早得多。建议告警：`increase(argus_unknown_event_total[1h]) > 0` 且 event_name 是新值。

### 4.4 存储

| 指标 | 类型 | 标签 |
|------|------|------|
| `argus_sink_queue_depth` | Gauge | |
| `argus_sink_submitted_total` | Counter | `result` (ok/dropped/degraded) |
| `argus_sink_batch_size` | Histogram | |
| `argus_sink_write_duration_seconds` | Histogram | `table` |
| `argus_sink_state` | Gauge | 0=direct,1=spooling,2=draining |
| `argus_spool_pending_bytes` | Gauge | |
| `argus_spool_oldest_age_seconds` | Gauge | |
| `argus_spool_dropped_bytes_total` | Counter | ← 数据永久丢失 |
| `argus_spool_write_failed_total` | Counter | ← 磁盘满/不可写，需紧急处理 |
| `argus_db_pool_connections` | Gauge | `pool,state` |
| `argus_config_reload_failed_total` | Counter | 配置快照重建失败（保留旧快照） |

### 4.5 认证与配额

| 指标 | 类型 | 标签 |
|------|------|------|
| `argus_auth_result_total` | Counter | `result` |
| `argus_auth_cache_total` | Counter | `result` (hit/miss/stale_hit/negative_hit) |
| `argus_auth_stale_hit_total` | Counter | ← PG 故障期间才应非零 |
| `argus_ratelimit_rejected_total` | Counter | `scope` |
| `argus_quota_rejected_total` | Counter | `scope,kind` |
| `argus_quota_sync_lag_seconds` | Gauge | ← 超过 60s 说明同步 task 卡了 |
| `argus_redis_degraded_total` | Counter | `op` (ratelimit/quota/taint) ← Redis 不可用，已降级本地 |

### 4.6 安全

| 指标 | 类型 | 标签 |
|------|------|------|
| `argus_security_verdict_total` | Counter | `timing,action,rule_id` |
| `argus_security_check_duration_seconds` | Histogram | `timing` |
| `argus_preflight_timeout_total` | Counter | ← fail-open 发生次数 |
| `argus_tool_gate_total` | Counter | `outcome` (allow/block/timeout/overflow) |
| `argus_tool_gate_hold_seconds` | Histogram | ← 验证零感知延迟的依据 |
| `argus_llm_detector_total` | Counter | `outcome` |
| `argus_llm_detector_skipped_total` | Counter | `reason` (concurrency/timeout/disabled) |
| `argus_security_no_rules_total` | Counter | ← 规则集为空，通常是配置错误 |

### 4.7 过载与资源

| 指标 | 类型 | 标签 |
|------|------|------|
| `argus_load_shed_total` | Counter | `reason` (concurrent/streams/large_request) |
| `argus_memory_pressure` | Gauge | 0.0 / 1.0，RSS 越过 high watermark 时置 1 |
| `argus_rss_bytes` | Gauge | |
| `argus_large_request_permits` | Gauge | `state` (available/held) |

### 4.8 实现：低开销

```rust
// metrics crate：宏在编译期展开，运行时是一次原子加
metrics::counter!("argus_requests_total", "protocol" => proto_str).increment(1);
```

标签值必须是 `&'static str` 或预先 `intern` 的，**绝不在热路径 `format!()`**：

```rust
// ❌ 每请求一次堆分配
counter!("x", "model" => model.to_string());
// ✅ 预先 intern，标签是 &'static str
counter!("x", "model" => ctx.model_label);   // 从 ConfigSnapshot 拿的 'static 引用
```

`ConfigSnapshot` 在构建时为每个 model/provider 预生成 `&'static str` 标签（用 `Box::leak` 或 `string_cache`）。因为配置项数量有限且长期存活，泄漏是可接受的（有上限，随 snapshot 版本增长——需要注意，见 OQ-3）。

Histogram 用 `metrics-exporter-prometheus` 的固定桶，避免动态分配。

---

## 5. Proxy-added latency 的测量（G-Pf1）

这是 SLO 的核心，也是最容易测错的东西。

### 5.1 定义

```text
proxy_overhead = (客户端感知的 TTFT) - (上游返回 header 所需时间)
```

具体分解：

```rust
pub struct Timings {
    /// t0: 请求进入 Ingress
    /// t1: Gatekeeper 完成
    /// t2: Preflight 完成
    /// t3: Route 完成
    /// t4: 上游请求写出（socket write 完成）
    /// t5: 上游 response header 到达
    /// t6: client response header 写出
    /// t7: 第一个 body frame 转发完成
    pub pre_upstream_us: u32,      // t4 - t0   ← Argus 的入站处理开销
    pub upstream_wait_us: u32,     // t5 - t4   ← provider 的时间，不算我们的
    pub header_relay_us: u32,      // t6 - t5   ← Argus 的出站开销
    pub first_frame_us: u32,       // t7 - t6
}

// proxy_overhead = pre_upstream_us + header_relay_us
```

**关键**：`upstream_wait_us` **不算**我们的开销，但它包含了 TCP 连接建立与 TLS 握手（如果连接池未命中）。因此单独测量：

```rust
pub struct UpstreamTimings {
    pub connect_us: Option<u32>,   // None = 复用连接
    pub tls_us: Option<u32>,
    pub pool_wait_us: u32,         // 等待连接池的时间 ← 这算我们的问题
}
```

`pool_wait_us` 显著非零意味着连接池太小或 provider 慢，需要调优。

### 5.2 时钟

全部用 `Instant`（单调时钟），不用 `SystemTime`。NTP 调整不会影响测量。

`quanta::Clock` 比 `Instant::now()` 快约 5–10 倍（用 TSC），在每请求 8 次采样的场景下值得用。

### 5.3 基准对照法

单靠自测的 `proxy_overhead` 不够可信（可能有没测到的地方）。因此压测时用**对照法**：

```text
A: 客户端 → Mock Provider          → 测得 TTFT_a
B: 客户端 → Argus → Mock Provider  → 测得 TTFT_b
proxy_added_ttft = TTFT_b - TTFT_a
```

Mock Provider 是本地的、延迟确定的 SSE 服务器。两次测试用完全相同的客户端与网络路径。这个差值才是真实的额外开销，也是 SLO 的验收依据。

---

## 6. 性能 SLO（目标，未验证）

> ⚠️ **以下全部是目标值，基于组件已知开销的推算，尚无 benchmark 数据。**
> 实测数据产生后必须回填本节，并明确标注「目标 / 实测」。伪造实测数据是不可接受的。

### 6.1 延迟

| 指标 | p50 | p95 | p99 | 测量条件 |
|------|-----|-----|-----|---------|
| `proxy_added_ttft`（连接复用） | ≤ 1 ms | ≤ 3 ms | ≤ 10 ms | 100KB 请求体，observe=Full，preflight 启用 |
| `proxy_added_ttft`（冷连接） | ≤ 30 ms | ≤ 80 ms | ≤ 200 ms | 含 TCP + TLS 握手 |
| `per_frame_added_latency` | ≤ 100 µs | ≤ 300 µs | ≤ 1 ms | 转发一个 SSE frame 的额外耗时 |
| `preflight_duration` | ≤ 2 ms | ≤ 8 ms | ≤ 40 ms | Layer 0–2，100KB 请求 |
| `tool_gate_hold` | ≤ 2 ms | ≤ 8 ms | ≤ 20 ms | 硬超时 20ms |
| `auth_duration`（缓存命中） | ≤ 5 µs | ≤ 15 µs | ≤ 50 µs | |

**推算依据**（p50 的 1ms 从哪来）：

| 步骤 | 估算 |
|------|------|
| HTTP 解析 + 路由（Axum） | ~10 µs |
| 认证（缓存命中 + sha256） | ~5 µs |
| 授权 + 限流（内存） | ~2 µs |
| 读 body（100KB，已在内核缓冲） | ~50 µs |
| Preflight（Layer 0–2） | ~500 µs–2 ms |
| 选路 | ~2 µs |
| Header 变换 + 写出 | ~20 µs |
| 收到 header → 写 client header | ~20 µs |
| **合计（不含 preflight）** | **~110 µs** |
| **合计（含 preflight）** | **~0.6–2 ms** |

可见 **Preflight 是 proxy overhead 的主要构成**。若关闭 preflight，p50 应在 200 µs 以下。这个结论应该在压测中验证。

### 6.2 吞吐与资源

| 指标 | 目标 | 条件 |
|------|------|------|
| 最大并发 SSE 流 | 10,000 | 4C8G，observe=Full |
| 最大并发 SSE 流 | 50,000 | 8C16G，observe=Lite，上游 HTTP/2 |
| 非流式 QPS | ≥ 3,000/s | 4C8G，小请求 |
| 每流稳态内存 | ≤ 64 KB | 不含 accumulator 正文 |
| 每流峰值内存 | ≤ 150 KB | 含 accumulator（1MB cap 下的 p99） |
| CPU（10k 流 @ 20 frame/s） | ≤ 1.5 核 | 含观测 |

推导见 `10-streaming.md` §7.4 与 §8.3。

### 6.3 可靠性

| 指标 | 目标 |
|------|------|
| Argus 自身导致的请求失败率 | < 0.01%（不含 provider 故障与客户端错误） |
| 观测丢弃率（`tee_dropped_bytes / bytes_forwarded`） | < 0.1% 稳态；< 1% 峰值 |
| `integrity = complete` 的 interaction 占比 | > 99%（observe=Full 的请求中） |
| `argus_observer_panic_total` | **0** |
| 审计数据丢失率（PG 正常时） | 0 |
| 审计数据丢失率（PG 故障 < 2h） | 0（靠 Spool） |

### 6.4 Benchmark 方法

```text
① 微基准（criterion）
   · SseFramer::push        —— 不同 chunk 切分方式
   · ProtocolObserver::on_frame —— 每种协议的典型 frame
   · Accumulator::apply
   · 认证路径
   目标：确认单次操作在 µs 级

② 组件基准
   · 完整 Observer 链路吞吐（frames/s，单核）
   · AuditSink 写入吞吐（interactions/s）

③ 端到端（oha / k6 + 本地 Mock Provider）
   · 对照法测 proxy_added_ttft（§5.3）
   · 阶梯加压：100 → 1k → 10k → 50k 并发流
   · 记录：延迟分布、RSS、CPU、drop rate

④ 长稳（24h）
   · 固定 5k 并发流
   · 观察 RSS 曲线（检测内存泄漏）
   · 观察 p99 漂移
```

Mock Provider 需要能模拟：正常流、慢首 token、中途断开、超大 frame、未知 event、429/503。它是 `20-testing-strategy.md` 的公共设施。

---


## 7. 推荐告警

| 告警 | 表达式（PromQL 概念形式） | 严重度 |
|------|--------------------------|--------|
| Argus 错误率高 | `rate(argus_requests_total{status=~"5.."}[5m]) / rate(argus_requests_total[5m]) > 0.01` | Critical |
| Observer panic | `increase(argus_observer_panic_total[1h]) > 0` | Critical（代码 bug） |
| Provider 认证失败 | `increase(argus_provider_auth_failure_total[5m]) > 0` | Critical（凭据问题） |
| 新的未知 event | `increase(argus_unknown_event_total[1h]) > 0`（新标签值） | Warning（provider 改 API） |
| 观测丢弃率高 | `rate(argus_tee_dropped_bytes_total[5m]) / rate(argus_bytes_forwarded_total[5m]) > 0.01` | Warning |
| Spool 积压 | `argus_spool_pending_bytes > 500e6` 或 `argus_spool_oldest_age_seconds > 3600` | Warning |
| Spool 丢弃 | `increase(argus_spool_dropped_bytes_total[5m]) > 0` | Critical（数据永久丢失） |
| 配额同步卡住 | `argus_quota_sync_lag_seconds > 60` | Warning |
| 熔断打开 | `argus_circuit_state > 0` | Warning |
| 配置刷新滞后 | `time() - argus_config_loaded_timestamp > 300` | Warning |
| 认证走 stale cache | `increase(argus_auth_stale_hit_total[5m]) > 0` | Warning（PG 有问题） |
| TTFT 退化 | `histogram_quantile(0.95, argus_proxy_overhead_seconds) > 0.01` | Warning |

---


---

## 8. 新增维度：Raw 模式与 Target Policy 的开销

新路由模型引入了两项 v1 没有的**每请求**成本，必须单独测量：

| 项 | 预期成本 | 怎么摊薄 | 指标 |
|---|---|---|---|
| Target Policy 判定 | CIDR 匹配 ~µs 级 | 预编译成 trie | `argus_target_policy_duration_us` |
| DNS 解析 | **首次可能数十 ms** | 带 TTL 上限的缓存（`13` §6.1） | `argus_dns_resolve_duration_us`、`argus_dns_cache_hits_total` / `_misses_total`（命中率在 Prometheus 侧算，进程内不维护比值） |

DNS 是这里唯一有量级风险的一项。**缓存未命中时它直接计入 TTFT。**
基准必须分别测「缓存命中」与「缓存未命中」两条路径，不能只报平均值。

> 这也是 `13-ssrf-protection.md` §10「Managed 模式也走 Target Policy」的代价。
> 若实测显示它对 TTFT 的影响超过预算，再考虑给 Managed 模式的固定 base_url
> 做启动期预解析。

---

## 9. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-Pf1 | `proxy_added_ttft` p50 < 2ms 是否可达？ | F07 首次测量后调整，但不降到失去意义 |
| OQ-Pf2 | DNS 缓存未命中对 TTFT 的实际影响？ | F07 |
| OQ-Pf3 | 是否需要给 Managed 的 base_url 做启动期 DNS 预解析？ | 取决于 OQ-Pf2 |
