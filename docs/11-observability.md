# 11 · 可观测性与性能目标

> 状态：Draft v1 · 最后更新 2026-08-19
> 注意区分：本文的「可观测性」指 **Argus 自身的运维可观测性**（metrics/log/trace），与 `06-observation-model.md` 的「AI 流量观测」是两回事。

---

## 1. Problem

需要回答三类问题：

1. **Argus 自己健康吗？** —— 延迟、错误率、资源、队列深度。
2. **上游 provider 健康吗？** —— 每个 provider 的成功率、TTFT、限流情况。
3. **观测链路本身健康吗？** —— 丢了多少字节、多少 interaction 被降级、解析出了多少 anomaly。

第 3 类是特有的：因为 Argus 的观测是**允许有损**的，就必须有指标精确表达"损了多少"。没有这个，有损观测会退化成不可信的数据。

同时有一个硬约束：**遥测本身不能拖慢主链路**。日志写入、指标记录都在每请求路径上。

---

## 2. Goals

- G-Ob1 主链路上的遥测开销 < 5 µs/请求。
- G-Ob2 OpenTelemetry 完全可选，未配置时零开销、零依赖。
- G-Ob3 能精确量化观测的完整性损失。
- G-Ob4 能把 proxy-added latency 与上游 latency 分离测量。
- G-Ob5 日志不包含 prompt 内容或凭据。

## 3. Non-goals

- 不内置 Grafana/Prometheus（提供 `/metrics` 端点即可）。
- 不做日志聚合与检索（交给外部）。
- 不做告警规则的内置引擎（提供推荐的 PromQL）。

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

## 5. Proxy-added latency 的测量（G-Ob4）

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

推导见 `05-streaming.md` §7.4 与 §8.3。

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
   · ProtocolParser::on_frame —— 每种协议的典型 frame
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

Mock Provider 需要能模拟：正常流、慢首 token、中途断开、超大 frame、未知 event、429/503。它是 `15-testing-strategy.md` 的公共设施。

---

## 7. 日志

### 7.1 分级与内容

```rust
// 每请求一条结构化 access log（INFO）
tracing::info!(
    request_id = %ctx.trace.request_id,
    trace_id   = %ctx.trace.trace_id,
    workspace  = %ctx.principal.workspace_id,
    protocol   = %ctx.protocol,
    model      = %route.logical_model,
    provider   = %route.provider_id,
    status     = resp.status,
    outcome    = ?outcome,
    stream     = ctx.stream,
    tokens_in  = usage.input,
    tokens_out = usage.output,
    ttft_us    = timings.ttft_us,
    total_us   = timings.total_us,
    overhead_us= timings.proxy_overhead_us,
    attempts   = attempts,
    integrity  = ?integrity,
    "request completed"
);
```

**绝不记录**（G-Ob5）：
- prompt 内容、响应内容、tool arguments
- API Key（完整或部分）
- provider credential
- 任何 `security_events` 的 `redacted_sample`

这些只进审计数据库（有访问控制），不进日志（日志通常聚合到低权限系统）。

CI 检查：grep 日志宏调用中是否出现 `body` / `content` / `secret` / `key` 等字段名（白名单例外）。

### 7.2 非阻塞写入

```rust
let (writer, _guard) = tracing_appender::non_blocking(std::io::stdout());
tracing_subscriber::fmt().json().with_writer(writer).init();
```

`non_blocking` 把日志写入放到独立线程 + 有界队列。**队列满时丢弃日志**（而不是阻塞请求）。这是 G-Ob1 的保证之一。

丢弃量由 `tracing_appender` 报告，应该导出为指标。

### 7.3 采样

高 QPS 下每请求一条 INFO 日志可能是主要的 IO 开销。

```toml
[logging]
access_log = "sampled"     # all | sampled | errors_only | off
sample_rate = 0.05         # 5%
# 但这些永远记录：
always_log_errors = true
always_log_security = true
always_log_degraded = true
```

**错误与安全事件永不采样**——它们的信息价值远高于成功请求。

---

## 8. 分布式追踪（OpenTelemetry，可选）

### 8.1 可选性的实现（G-Ob2）

```toml
[telemetry.otlp]
enabled = false            # 默认关闭
endpoint = "http://localhost:4317"
sample_rate = 0.01
```

**未启用时零开销**：不是"启用了但采样率 0"，而是 subscriber 层根本不注册 OTel layer。`tracing` 的 layer 在编译期是泛型组合，未注册的 layer 不产生任何运行时分支。

**依赖也是可选的**：

```toml
# argus-server/Cargo.toml
[features]
default = []
otel = ["dep:opentelemetry", "dep:opentelemetry-otlp", "dep:tracing-opentelemetry"]
```

不启用 feature 时，`opentelemetry` 相关的 crate 完全不编译进二进制（减小体积，减少依赖面）。

### 8.2 Span 结构

```text
argus.request                          (root, 或继承 client 的 traceparent)
├── argus.auth
├── argus.preflight
│   └── argus.security.rule_eval
├── argus.route
├── argus.upstream                     (每次 attempt 一个)
│   ├── argus.upstream.connect         (仅冷连接)
│   └── argus.upstream.headers
└── argus.relay
    └── (observer 的 span 是独立 trace，通过 link 关联)
```

**Observer 的 span 不是主链路 span 的 child**，因为它的生命周期可能超出主链路。用 span link 关联：

```rust
let observer_span = tracing::info_span!("argus.observe");
observer_span.follows_from(request_span.id());
```

### 8.3 traceparent 传播

- 入站：接受客户端的 `traceparent` header，作为 `trace_id`。
- 出站：**默认不向 provider 传播** `traceparent`（provider 不需要，且可能泄漏内部拓扑）。可配置开启。

---

## 9. 健康检查端点

| 端点 | 语义 | 用途 |
|------|------|------|
| `/healthz` | 进程存活。**永远返回 200**（除非进程死了） | K8s liveness |
| `/readyz` | 能否处理流量 | K8s readiness |
| `/metrics` | Prometheus | |

`/readyz` 的判定：

```rust
fn ready(&self) -> Ready {
    // 必须：有可用的配置快照
    if self.config.load().version == 0 { return NotReady("config not loaded"); }
    // 必须：不在停机中
    if self.shutdown.is_cancelled() { return NotReady("shutting down"); }
    // 必须：至少有一个健康的 provider
    if self.healthy_provider_count() == 0 { return NotReady("no healthy provider"); }
    // 不要求：PostgreSQL 可用！
    Ready
}
```

**`/readyz` 刻意不检查 PostgreSQL**。如果检查了，PG 故障会导致所有实例被摘除，Data Plane 完全不可用——而 Data Plane 明明还能正常工作（`10-storage.md` §8.5）。这是一个常见且严重的错误。

PG 状态通过 `/admin/diagnostics` 与指标暴露，用于告警，不用于流量摘除。

---

## 10. 推荐告警

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

## 11. Failure Modes

| 场景 | 行为 |
|------|------|
| 日志队列满 | 丢弃日志，记录丢弃计数，**不阻塞请求** |
| `/metrics` 抓取超时（指标过多） | 限制标签基数；提供 `[metrics] per_workspace=false` |
| OTLP endpoint 不可达 | OTel 的 batch exporter 内部丢弃，不阻塞 |
| 标签基数爆炸（如 model 名被用户随意指定） | 只对**已知 logical model** 打标签，未知的归到 `model="_unknown"` |
| `quanta::Clock` 在某些虚拟化环境不可用 | 自动回退到 `Instant::now()` |
| `/readyz` 被误配置为检查 PG | 文档明确警告；提供 `/readyz?strict=true` 给确实需要的场景 |

---

## 12. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| `/readyz` 不查 PG | PG 故障不导致全站摘除 | 需要单独的 PG 告警 |
| 日志采样 | 高 QPS 下 IO 可控 | 成功请求的日志不完整（但审计库有全量） |
| OTel 作为 feature | 零开销、二进制小 | 想用 OTel 要重新编译或用带 feature 的发行版 |
| 不带 workspace 标签 | 指标基数可控 | 租户维度分析要查审计库（慢） |
| 预 intern 的 `&'static str` 标签 | 零分配 | 配置重载会累积泄漏（见 OQ-3） |
| 对照法测 overhead | 数字可信 | 需要维护 Mock Provider 与压测环境 |

---

## 13. Alternatives Considered

**A. 用 OpenTelemetry metrics 替代 `metrics` crate。**
OTel metrics 的 Rust 实现较重（每次记录有更多间接层），且会让 OTel 成为必需依赖。**否决**——用 `metrics` + Prometheus exporter，OTel 只做 trace。

**B. 每请求都记 INFO 日志（不采样）。**
1000 QPS × 500 字节 = 500 KB/s 的日志，一天 43 GB。**否决为默认**，提供采样。

**C. 把 access log 写进 PostgreSQL 而不是 stdout。**
审计库已经有更完整的记录了。重复写是浪费。**否决**。

**D. `/readyz` 检查 PostgreSQL（"没有数据库就不算就绪"）。**
这是最危险的反模式：PG 抖动 → 所有 pod NotReady → LB 摘除全部 → 完全不可用，而实际上 Argus 还能转发。**强烈否决**。

**E. 直接用 `Instant::now()`，不引入 quanta。**
`Instant::now()` 在 Linux 上是 vDSO 调用，约 20–25 ns。8 次采样 = 200 ns/请求。相对 1ms 的 overhead 预算是 0.02%，其实可以接受。**quanta 是可选优化**，不是必须。降级为 OQ。

---

## 14. Decision

1. 指标用 `metrics` crate + Prometheus exporter；标签值必须是 `&'static str`，热路径零分配。
2. **观测链路健康指标（§4.3）是一等公民**，尤其是 `tee_dropped_bytes`、`interaction_integrity`、`unknown_event`。
3. `proxy_overhead` 分解为 `pre_upstream` + `header_relay`，上游等待单独计量；SLO 验收用**对照法**。
4. 性能 SLO 全部标注为「目标，未验证」，实测后回填并区分标注。
5. 日志绝不含内容/凭据（CI 检查）；非阻塞写入；成功请求可采样，错误与安全事件永不采样。
6. OpenTelemetry 是 **Cargo feature + 配置双重可选**，未启用时不编译进二进制。
7. **`/readyz` 不检查 PostgreSQL**，只检查配置快照、停机状态、健康 provider 数。
8. Observer 的 span 通过 `follows_from` link 关联，不作为主链路 span 的 child。

---

## 15. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | Preflight 是 overhead 主要来源的推断是否正确？ | 未验证 | 首次压测 |
| OQ-2 | SLO 中的 p99 ≤ 10ms 是否过于乐观？（GC 无，但有调度抖动） | 目标值 | 首次压测后修订 |
| OQ-3 | `Box::leak` 的标签在频繁配置重载下会累积泄漏，需要改用 `string_cache` 或 arena？ | 已知问题 | 实现时直接用 interning crate |
| OQ-4 | 是否值得引入 `quanta`？（vs `Instant::now()` 的 25ns） | 倾向不引入 | profile 后决定 |
| OQ-5 | 观测链路是否需要自己的 `/metrics` 子集以便独立监控？ | 未做 | 指标数量过多时 |
