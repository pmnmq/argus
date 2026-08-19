# 13 · 部署

> 状态：Draft v1 · 最后更新 2026-08-19
> 前置：`12-reliability.md`（资源模型与停机）

---

## 1. Problem

Argus 要覆盖从「家用 NAS 上跑给自己用」到「公司内多实例生产集群」的跨度。这两端对配置、依赖、运维复杂度的容忍度完全不同。

常见错误是只为其中一端设计：为大规模设计会让 NAS 用户装不起来（要 K8s、Redis、Kafka）；只为小规模设计则无法水平扩展。

---

## 2. Goals

- G-D1 单个静态二进制 + 一个 PostgreSQL 即可完整运行。
- G-D2 配置有分层来源（文件 / 环境变量 / 数据库），且优先级明确。
- G-D3 提供针对不同规模的**预设配置**，用户不需要理解全部参数。
- G-D4 升级不中断正在进行的流。
- G-D5 敏感配置（master key、DB 密码）不必写进配置文件。

## 3. Non-goals

- 不提供 Helm chart 之外的 K8s 抽象（不做 Operator/CRD）。
- 不做自动化的数据库运维（备份/failover 交给用户或托管服务）。
- 不内置 TLS 证书管理（用前置代理或 `--tls-cert`/`--tls-key`）。

---

## 4. 部署形态

| 形态 | 规模 | 依赖 | 说明 |
|------|------|------|------|
| **单二进制** | 个人 / 小团队 | PostgreSQL | systemd 服务；NAS / 家用服务器 |
| **Docker Compose** | 小团队 | 内含 PG | 一条命令起完整栈 |
| **Kubernetes** | 生产 | 外部 PG | Deployment + Service + HPA |
| **单容器 + 托管 PG** | 中小生产 | RDS / Cloud SQL / Neon | 最省心 |

### 4.1 二进制

```bash
# 构建
cargo build --release --locked
# 产物：target/release/argus，静态链接（musl 时完全静态）

# 交叉编译为完全静态的二进制（无 glibc 依赖）
cargo build --release --target x86_64-unknown-linux-musl
```

用 `rustls` 而非 `openssl` 是刻意的：避免动态链接 OpenSSL，让 musl 静态构建可行，且不受系统 OpenSSL 版本影响。

体积预估：~25–35 MB（含 rustls 根证书）。启用 `otel` feature 后 +~8 MB。

```bash
argus serve --config /etc/argus/argus.toml
argus migrate                      # 单独执行迁移
argus config apply -f config.yaml  # 声明式配置导入
argus config export -o config.yaml
argus keys create --workspace default --name my-app
argus doctor                       # 诊断：检查配置、连通性、权限
```

`argus doctor` 是重要的易用性投资：检查 PG 连通、迁移状态、master key 可用、各 provider 的 credential 是否有效、fd 限制、磁盘空间，并给出可执行的修复建议。

### 4.2 systemd

```ini
[Unit]
Description=Argus AI Gateway
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=notify                          # 用 sd_notify 报告就绪
ExecStart=/usr/local/bin/argus serve --config /etc/argus/argus.toml
Restart=always
RestartSec=5

# 优雅停机（必须 > force_kill_after）
KillSignal=SIGTERM
TimeoutStopSec=400

# 资源
LimitNOFILE=65536
MemoryMax=6G                         # 配合 12-reliability §5.4 的 cgroup 感知

# 安全加固
User=argus
Group=argus
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/argus
CapabilityBoundingSet=
AmbientCapabilities=

# 敏感配置从文件读取，不进环境变量（环境变量会出现在 /proc/pid/environ）
LoadCredential=master_key:/etc/argus/master.key
Environment=ARGUS_MASTER_KEY_FILE=%d/master_key

[Install]
WantedBy=multi-user.target
```

`LoadCredential` 是 systemd 250+ 的特性，把密钥以 0600 权限放进 `$CREDENTIALS_DIRECTORY`，比环境变量安全。

### 4.3 Docker

```dockerfile
# 构建阶段
# 基础镜像的版本必须与 rust-toolchain.toml 一致，否则 rustup 会在构建时
# 再下一次工具链，白白拖慢镜像构建。
FROM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
RUN cargo build --release --locked --target x86_64-unknown-linux-musl

# 运行阶段：distroless / scratch
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/argus /argus
USER nonroot:nonroot
EXPOSE 8080 9900
ENTRYPOINT ["/argus", "serve"]
```

用 `distroless/static` 而非 `alpine`：无 shell、无包管理器，攻击面最小。因为是静态二进制，不需要任何运行时库。

**镜像体积**：~30 MB（几乎全是二进制本身）。

```yaml
# docker-compose.yml
services:
  argus:
    image: ghcr.io/xxx/argus:0.1
    ports: ["8080:8080"]
    environment:
      ARGUS_DATABASE__URL: postgres://argus:${PG_PASSWORD}@postgres/argus
      ARGUS_MASTER_KEY_FILE: /run/secrets/master_key
    secrets: [master_key]
    volumes:
      - argus-spool:/var/lib/argus/spool
    depends_on:
      postgres: { condition: service_healthy }
    stop_grace_period: 400s          # 对应 force_kill_after
    ulimits:
      nofile: { soft: 65536, hard: 65536 }

  postgres:
    image: postgres:17-alpine
    environment:
      POSTGRES_DB: argus
      POSTGRES_USER: argus
      POSTGRES_PASSWORD: ${PG_PASSWORD}
    command: >
      postgres -c default_toast_compression=lz4
               -c max_connections=100
               -c shared_buffers=512MB
    volumes: [pgdata:/var/lib/postgresql/data]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U argus"]
      interval: 5s

secrets:
  master_key: { file: ./secrets/master.key }
volumes: { pgdata: {}, argus-spool: {} }
```

**注意 `argus-spool` 必须是持久卷**：容器重启时 spool 里可能还有未回放的审计数据。

### 4.4 Kubernetes

```yaml
apiVersion: apps/v1
kind: Deployment
metadata: { name: argus }
spec:
  replicas: 3
  strategy:
    type: RollingUpdate
    rollingUpdate: { maxSurge: 1, maxUnavailable: 0 }   # 先起后停
  template:
    spec:
      terminationGracePeriodSeconds: 400
      containers:
      - name: argus
        image: ghcr.io/xxx/argus:0.1
        ports: [{ containerPort: 8080, name: http }]
        env:
        - name: ARGUS_DATABASE__URL
          valueFrom: { secretKeyRef: { name: argus-db, key: url } }
        - name: ARGUS_MASTER_KEY_FILE
          value: /secrets/master.key
        - name: ARGUS_SERVER__INSTANCE_ID
          valueFrom: { fieldRef: { fieldPath: metadata.uid } }
        volumeMounts:
        - { name: master-key, mountPath: /secrets, readOnly: true }
        - { name: spool, mountPath: /var/lib/argus/spool }
        resources:
          requests: { cpu: "2", memory: "2Gi" }
          limits:   { cpu: "4", memory: "8Gi" }
        readinessProbe:
          httpGet: { path: /readyz, port: 8080 }
          periodSeconds: 2
          failureThreshold: 1
        livenessProbe:
          httpGet: { path: /healthz, port: 8080 }
          periodSeconds: 10
          failureThreshold: 3
        lifecycle:
          preStop: { exec: { command: ["sleep", "5"] } }
      volumes:
      - name: master-key
        secret: { secretName: argus-master-key }
      - name: spool
        emptyDir: { sizeLimit: 2Gi }
```

**关于 spool 用 `emptyDir`**：Pod 被删除时 spool 数据丢失。这是可接受的取舍——emptyDir 在 Pod 重启（容器重启）时保留，只在 Pod 重建时丢失。用 StatefulSet + PVC 可以完全避免，但会让部署复杂很多（Argus 本身是无状态的）。

> **明确的限制**：spool 目录**必须是实例独占的本地存储**。多个实例共享同一个 spool 目录（如 NFS）会导致数据损坏（append-only 文件的并发写）。文档必须强调这一点。

**`maxUnavailable: 0`** 很重要：结合 400 秒的 grace period，滚动更新会先起新 pod，等它 ready，再开始 drain 旧 pod。整个过程中容量不下降。

---

## 5. 配置

### 5.1 分层与优先级

```text
优先级从低到高：
① 内置默认值
② 配置文件（TOML）
③ 环境变量（ARGUS_ 前缀，双下划线表示层级）
④ 命令行参数
⑤ 数据库中的配置（Provider/Model/Policy/Key）
```

⑤ 与 ①–④ 是**不同的配置域**，不重叠：
- ①–④ 是**进程配置**：监听地址、数据库连接、资源上限、超时、观测策略。改动需要重启（少数支持 SIGHUP 重载）。
- ⑤ 是**业务配置**：provider、模型、策略、Key。运行时通过 ConfigSnapshot 热更新。

这个划分很重要：进程配置进文件（随部署走，可以进 Git），业务配置进数据库（运行时可改，有审计）。

### 5.2 环境变量映射

```bash
ARGUS_SERVER__LISTEN=0.0.0.0:8080
ARGUS_DATABASE__URL=postgres://...
ARGUS_DATABASE__MAX_CONNECTIONS=16
ARGUS_LIMITS__MAX_ACTIVE_STREAMS=10000
ARGUS_OBSERVATION__RAW__MODE=selective
```

`__`（双下划线）映射到 TOML 的层级。用 `figment` 或 `config` crate 实现。

### 5.3 完整配置示例

```toml
# /etc/argus/argus.toml

[server]
listen       = "0.0.0.0:8080"
admin_listen = "127.0.0.1:9900"
role         = "all"                  # all | data | control
instance_id  = ""                     # 留空则启动时生成 UUID 并持久化

[database]
url              = "postgres://argus@localhost/argus"
max_connections  = 16
connect_timeout  = "10s"
[database.admin]
max_connections   = 4
statement_timeout = "30s"

[credentials]
master_key_file = "/etc/argus/master.key"   # 或 ARGUS_MASTER_KEY

[limits]
max_connections               = 20000
max_concurrent_requests       = 12000
max_active_streams            = 10000
max_request_bytes             = "8MiB"
large_threshold               = "256KiB"
max_large_request_concurrency = 32
ingest_timeout                = "30s"      # 读完 client body 的整体超时（02 §6.2）
auto_raise_nofile             = true
min_nofile                    = 65536
[limits.memory]
high_watermark = "70%"
low_watermark  = "55%"
[limits.stream]
client_write_idle_timeout = "60s"
max_stream_duration       = "900s"
max_frame_bytes           = "1MiB"
max_accumulated_bytes     = "1MiB"
tee_channel_capacity      = 64
tee_byte_budget           = "256KiB"

[upstream]
prefer_http2      = true
pool_max_idle_per_host = 32
pool_idle_timeout = "90s"
[upstream.timeouts]
connect    = "3s"
headers    = "60s"
idle_frame = "120s"
total      = "900s"

[routing]
sticky     = "none"        # none | conversation | api_key（07 §5.3，保护 provider 侧 prompt cache）
sticky_ttl = "30m"

[routing.retry]
max_attempts              = 3
max_attempts_per_provider = 1
budget                    = "10s"
on_no_response            = true
[routing.retry.backoff]
initial    = "50ms"
max        = "1s"
multiplier = 2.0
jitter     = 0.3
[routing.circuit_breaker]
window       = "30s"
min_requests = 20
failure_rate = 0.5
open_duration    = "15s"
half_open_probes = 3

[observation]
default_mode = "full"          # full | lite | off
[observation.raw]
mode = "selective"
selective_head = 8
selective_tail = 8
[observation.pressure]
elevated = 0.60
high     = 0.85
critical = 0.95

[security]
[security.preflight]
timeout    = "50ms"
on_timeout = "open"
on_error   = "open"
[security.tool_gate]
enabled = false                # 默认关闭，见 08 §6.2
timeout    = "20ms"
on_timeout = "open"
max_buffered_bytes = "256KiB"
on_overflow        = "flush"
[security.stream_guard]
mode = "observe"               # observe | delay_window
delay_window = "0ms"
[security.llm_detector]
enabled         = false
model           = "claude-haiku"
timing          = ["post_response"]   # 绝不放 preflight（08 §7.4）
trigger         = "on_flag"           # always | on_flag | sampled
sample_rate     = 0.01
timeout         = "5s"
max_input_chars = 20000
concurrency     = 4                   # bulkhead，满则跳过（fail-open）

[storage.sink]
queue_capacity = 4096
batch_size = 256
batch_delay = "200ms"
on_full = "degrade"
[storage.payload]
max_raw_request_bytes = "1MiB"
max_blocks_bytes      = "1MiB"
max_raw_events_bytes  = "2MiB"
on_overflow           = "truncate"    # truncate | drop
store_raw_request     = true
store_blocks          = true
store_raw_events      = false
minimal               = false         # true = 只存元数据（nano profile 建议开）

[storage.audit]
require_durable = false    # ⚠ true = 审计写失败则拒绝请求，PG 变成 Data Plane 硬依赖（ADR-006）

[storage.encryption]
encrypt_payloads = false   # 开启后无法用 PG 检索内容（08 §9.3）

[storage.spool]
enabled = true
dir = "/var/lib/argus/spool"
max_bytes     = "1GiB"
max_age       = "24h"
segment_bytes = "64MiB"
[storage.retention]
requests_days = 365
payloads_days = 30
tool_calls_days = 90
security_events_days = 365
audit_events_days = 730
[storage.redaction]
mode = "on_write"

[auth.cache]
capacity  = 50000
ttl       = "5m"
stale_ttl = "15m"
negative_ttl      = "30s"
negative_capacity = 10000

[quota]
sync_interval       = "10s"
soft_threshold      = 0.9
overshoot_tolerance = 1.05

[redis]
enabled    = false
# url      = "redis://..."
on_failure = "degrade"     # degrade（降级本地）| fail_open | fail_close

[logging]
level      = "info"
format     = "json"
access_log  = "sampled"    # all | sampled | errors_only | off
sample_rate = 0.05
# 以下三类永不采样
always_log_errors   = true
always_log_security = true
always_log_degraded = true

[telemetry.otlp]
enabled     = false        # 同时需要 `otel` cargo feature（11 §8.1）
# endpoint  = "http://localhost:4317"
sample_rate = 0.01

[shutdown]
lb_drain_delay = "5s"
drain_timeout  = "300s"
sink_flush_timeout = "30s"
force_kill_after   = "360s"

[runtime]
split            = false   # true = observer/sink 跑在独立 Runtime（12 §7.2，默认关）
worker_threads   = 0       # 0 = CPU 核数
observer_threads = 2       # split=true 时生效
```

### 5.4 规模预设（G-D3）

上面的配置有 60+ 个参数，多数用户不该需要理解它们。提供预设：

```toml
[server]
profile = "small"    # nano | small | medium | large
```

| profile | 目标 | 关键差异 |
|---------|------|---------|
| `nano` | NAS / 树莓派 / 1C1G | `max_active_streams=200`，`observation.default_mode="lite"`，`payloads_days=7`，`spool.max_bytes=128MiB`，`raw.mode="none"` |
| `small` | 4C8G，个人/小团队 | 上面的默认值，`max_active_streams=10000` |
| `medium` | 8C16G，团队生产 | `max_active_streams=30000`，`db.max_connections=32`，`sink.queue_capacity=16384` |
| `large` | 16C32G+，多实例 | `max_active_streams=50000`，`observation.default_mode="lite"`，建议启用 Redis |

profile 只设置默认值，任何显式配置都会覆盖它。`argus doctor` 会检查配置与实际资源是否匹配并给出建议。

---

## 6. 数据库准备

```sql
CREATE ROLE argus LOGIN PASSWORD '...';
CREATE DATABASE argus OWNER argus;
\c argus
CREATE EXTENSION IF NOT EXISTS citext;    -- users.email
-- 推荐配置
ALTER DATABASE argus SET default_toast_compression = 'lz4';
ALTER DATABASE argus SET jit = off;       -- 短查询下 JIT 常常是负收益
```

**不需要**任何第三方扩展（不需要 `pg_partman`、`timescaledb`、`pgvector`）。

### 6.1 推荐的 PG 参数

针对「大量小批量写入 + 少量分析查询」：

| 参数 | 建议 | 理由 |
|------|------|------|
| `shared_buffers` | 25% 内存 | 标准建议 |
| `max_connections` | 100 | Argus 只用 20；留给其他 |
| `wal_compression` | `lz4` | 审计写入产生大量 WAL |
| `checkpoint_timeout` | `15min` | 减少 checkpoint 频率 |
| `max_wal_size` | `4GB` | 同上 |
| `synchronous_commit` | `off` | ⚠️ 见下 |
| `autovacuum_vacuum_scale_factor` | `0.05`（分区表） | 更积极的 vacuum |

**`synchronous_commit = off` 的取舍**：关闭后事务不等 WAL fsync，写入吞吐提升 2–5 倍，代价是崩溃时可能丢失最近 ~600ms 的已提交事务。对审计数据这是可接受的（我们本就允许有损，且有 `Integrity` 字段表达）。**但配置类表的写入不应关闭**——因此建议在 session 级别设置，只对 sink 的连接关闭：

```rust
// AuditSink 的连接池初始化
PgPoolOptions::new().after_connect(|conn, _| Box::pin(async move {
    sqlx::query("SET synchronous_commit = off").execute(conn).await?;
    Ok(())
}))
```

这比数据库级设置精确得多。Control Plane 的池保持默认。

---

## 7. 密钥管理（G-D5）

### 7.1 Master Key

```bash
# 生成
argus keygen master > /etc/argus/master.key
chmod 600 /etc/argus/master.key
```

来源优先级：
1. `ARGUS_MASTER_KEY`（base64，32 字节）
2. `ARGUS_MASTER_KEY_FILE` / `credentials.master_key_file`
3. 外部 KMS（Phase 3）

**Argus 在启动时**：
- 检查文件权限（非 0600 则拒绝启动并说明原因）
- 尝试解密至少一个已存在的 credential，失败则拒绝启动

### 7.2 备份要求（⚠️ 最重要的运维事项）

> **Master Key 丢失 = 所有 Provider credential 永久不可恢复。**
>
> 必须：
> 1. 离线备份 master key
> 2. **备份位置不能与数据库备份相同** —— 否则加密失去意义（一次泄漏同时拿到密文与密钥）
> 3. 建议：密码管理器 / 硬件保险箱 / 独立的 KMS

### 7.3 轮换

```toml
[credentials]
master_key_file          = "/etc/argus/master.key"
master_key_previous_file = "/etc/argus/master.key.old"   # 轮换期临时配置
```

```bash
argus keygen master > /etc/argus/master.key.new
mv /etc/argus/master.key /etc/argus/master.key.old
mv /etc/argus/master.key.new /etc/argus/master.key
# 重启（携带 previous），然后：
argus credentials rewrap     # 用新 key 重新加密所有 credential
# 确认后移除 previous 配置并重启
```

解密时先试当前 key，失败再试 previous。加密永远用当前 key。

---

## 8. 升级（G-D4）

### 8.1 流程

```text
① 阅读 CHANGELOG 的 breaking changes
② 备份数据库
③ argus migrate --dry-run   （查看将执行的迁移）
④ 单实例：argus migrate 然后重启
   多实例：先在一个实例上执行 migrate，再滚动更新
⑤ 观察 /admin/diagnostics 与告警
```

### 8.2 迁移的兼容性契约

**规则：迁移必须是"先加后删"的两阶段。**

```text
版本 N   → 加新列/新表（可空、有默认值），代码同时写新旧
版本 N+1 → 代码只读新的
版本 N+2 → 删旧列/旧表
```

这保证任意时刻新旧版本的实例可以共存（滚动更新期间必然发生）。**禁止在一个版本内同时改 schema 和让旧代码不兼容。**

CI 检查：`migrations/` 中的每个迁移，用上一个 release 的二进制跑一遍冒烟测试。

### 8.3 回滚

- **代码回滚**：随时可以（因为迁移是向后兼容的）。
- **迁移回滚**：提供 `down` 脚本，但不保证数据不丢（删掉的列回不来）。**首选是代码回滚而不是迁移回滚。**

---

## 9. 前置代理

Argus 不做 TLS 证书自动管理。生产建议在前面放一层：

```text
Client → [Caddy / nginx / ALB]  → Argus
             TLS 终止              明文 HTTP/2 (h2c) 或 HTTPS
             证书自动续期
             IP 层限流
             真实 IP 透传（X-Forwarded-For）
```

**注意事项**：

1. 前置代理必须**支持 SSE 流式转发**且**禁用响应缓冲**：
   - nginx：`proxy_buffering off; proxy_read_timeout 900s;`
   - Caddy：默认正确
   - ALB：`idle_timeout` 必须 > `max_stream_duration`
2. 必须允许长连接（AI 请求可能 10 分钟）。
3. `X-Forwarded-For` 的信任配置：
   ```toml
   [server]
   trusted_proxies = ["10.0.0.0/8"]   # 只信任这些来源的 XFF
   ```
   不配置时忽略 XFF（用直连 IP），防止 IP allowlist 被伪造绕过。

Argus 自己也支持 TLS（`--tls-cert` / `--tls-key`），适合无前置代理的简单部署。

---

## 10. Failure Modes

| 场景 | 表现 | 处理 |
|------|------|------|
| PG 未就绪时启动 | 重试 60s 后退出 | systemd `Restart=always` 会重试 |
| 迁移失败 | 拒绝启动 | 人工检查；不允许 schema 不一致运行 |
| master key 权限过宽 | 拒绝启动 + 明确提示 | `chmod 600` |
| master key 无法解密现有 credential | 拒绝启动 | 检查是否用错 key |
| 端口被占用 | 启动失败 | |
| spool 目录不可写 | 警告，spool 禁用（PG 挂时会丢数据） | 修权限 |
| 多实例共享 spool 目录 | **数据损坏** | 文档明确禁止；启动时检测 lock 文件 |
| fd 限制过低 | 警告；高并发时 accept 失败 | `LimitNOFILE` |
| 前置代理开了 buffering | SSE 不流式（客户端看到卡顿后一次性输出） | 文档明确列出各代理的配置 |
| K8s grace period < force_kill_after | 停机时被 SIGKILL，流被切断 | 文档强调二者关系 |

---

## 11. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| rustls 而非 openssl | 静态构建可行，无系统依赖 | 少数极老的 TLS 配置不兼容 |
| distroless 镜像 | 攻击面小 | 无法 `docker exec` 进去排障（用 `argus doctor` 与 ephemeral container 替代） |
| spool 用 emptyDir | 部署简单（无状态） | Pod 重建时丢未回放的审计 |
| profile 预设 | 用户不需要理解 60 个参数 | 预设可能不适合特殊场景（可覆盖） |
| 进程配置 vs 业务配置分离 | 前者进 Git，后者可热改 | 用户需要理解这个划分 |
| `synchronous_commit=off` 仅 sink 池 | 写入吞吐 2–5× | 崩溃时可能丢 600ms 审计 |
| 不自动管理 TLS 证书 | 少一大块复杂度 | 需要前置代理或手动证书 |
| 迁移"先加后删" | 滚动更新安全 | 每个 schema 变更要跨 3 个版本完成 |

---

## 12. Alternatives Considered

**A. 提供 Kubernetes Operator + CRD 管理 provider/model。**
K8s 用户体验好，但对非 K8s 用户零价值，且要维护一套额外的控制器。**否决**，`argus config apply` 已经覆盖 GitOps 场景。

**B. 内嵌 SQLite 作为默认存储，PostgreSQL 可选。**
对 NAS 场景很有吸引力（零外部依赖）。但：SQLite 不支持并发写（审计写入会锁）、不支持分区、多实例完全不可行。**推迟**——如果 `nano` profile 的用户反馈强烈，可以作为「只存元数据」的降级存储（Phase 3）。

**C. 内置 Caddy 式的自动 TLS（ACME）。**
方便，但引入证书状态管理与 ACME 客户端的复杂度，且与前置代理场景冲突。**否决**，推荐用现成的前置代理。

**D. 用 `alpine` 而非 `distroless`（保留 shell 便于排障）。**
可排障但攻击面更大。**否决**——用 `kubectl debug` 的 ephemeral container 排障，且 `argus doctor` 覆盖大部分场景。

**E. 把 master key 存进数据库（用启动密码派生）。**
把密钥和密文放一起，加密失去意义。**强烈否决**。

---

## 13. Decision

1. 主要形态：静态二进制（musl + rustls）、distroless Docker 镜像、K8s Deployment。
2. 配置分两个域：**进程配置**（文件/环境变量，随部署走）与**业务配置**（数据库，热更新），互不重叠。
3. 提供 4 个规模 profile（`nano`/`small`/`medium`/`large`）作为默认值集合。
4. Master key 从文件或环境变量读，权限强制 0600，启动时验证可解密，支持 previous key 轮换。**备份必须与数据库备份分离。**
5. `synchronous_commit = off` 只在 AuditSink 的连接池上设置（session 级）。
6. Spool 目录必须实例独占（启动时用 lock 文件检测）。
7. 数据库迁移遵循"先加后删"两阶段，保证滚动更新期间新旧版本共存。
8. K8s：`maxUnavailable: 0` + `terminationGracePeriodSeconds: 400` + `preStop sleep 5`。
9. 不内置 TLS 证书管理；文档明确列出各前置代理的 SSE 必需配置。
10. 提供 `argus doctor` 做部署自检。

---

## 14. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | `nano` profile 是否真的能在 1C1G 上跑？ | 未验证 | 有树莓派/NAS 测试环境后 |
| OQ-2 | SQLite 作为 `nano` 的可选存储，值得做吗？ | 未做 | 看小规模用户反馈 |
| OQ-3 | 是否提供官方 Helm chart？ | 未做 | 有 K8s 用户需求时 |
| OQ-4 | spool 用 PVC（StatefulSet）是否值得？ | 用 emptyDir | 有审计不可丢的合规需求时 |
| OQ-5 | musl 的分配器性能较差，是否应该改用 glibc + 动态链接，或在 musl 下强制 jemalloc？ | 未测 | 压测时对比（这是 musl 的已知问题） |
