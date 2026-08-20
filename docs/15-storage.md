# 15 · 存储

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`11-observation-model.md`（要存什么）、`01-architecture.md` §5.1（编译期解耦）

---

## 1. Problem

存储层要同时应对两种完全相反的负载：

- **审计写入**：高频、只追加、批量友好、丢一点可以接受（但要知道丢了）。峰值可能是数千条/秒，每条含 KB 级的 JSON。
- **管理查询**：低频、复杂、可能扫大量行、延迟不敏感。

同时必须满足：`Argus + PostgreSQL` 就能完整运行（不能要求 ClickHouse），且 **PostgreSQL 挂掉时 Data Plane 不受影响**。

核心矛盾：AI 交互数据天然是「半结构化 + 大文本」，用纯关系模型存会产生巨大的写放大（一次回复拆成几十行 content_block），用纯 JSONB 存又会失去查询能力。

---

## 2. Goals

- G-St1 Data Plane **在编译期**不可能直接调用数据库。
- G-St2 单次 interaction 的落库写放大 ≤ 3 行（不含 tool_calls）。
- G-St3 PostgreSQL 不可用时，审计数据进本地 Spool，恢复后自动回放，不丢。
- G-St4 数据清理是 O(1) 的（DROP PARTITION），不是 O(n) 的 DELETE。
- G-St5 常见的审计查询（按 workspace/时间/模型/状态）走索引，不全表扫。

## 3. Non-goals

- 不做时序数据库级别的压缩比。
- 不做全文检索（PostgreSQL 的 GIN 全文索引可选启用，但不是默认）。
- 不做跨库分片。
- 不保证 100% 的审计完整性（`06` 的 `Integrity` 字段负责表达这一点）。

---

## 4. 分层与解耦

### 4.1 编译期解耦（G-St1）

```rust
// argus-protocol/src/sink.rs
// Data Plane 只知道这个 trait，不知道任何存储实现
pub trait InteractionSink: Send + Sync + 'static {
    /// 非阻塞提交。返回 Err 表示队列满，调用方应丢弃并记指标。
    fn try_submit(&self, interaction: Box<ObservedInteraction>) -> Result<(), SinkFull>;
}
```

**为什么这个 trait 在 `argus-protocol` 而不是 `argus-core`**：它的签名引用 `ObservedInteraction`，而后者定义在 `argus-protocol`。若把 trait 放进 `argus-core`，`argus-core` 就要反向依赖 `argus-protocol`，形成循环依赖（`argus-protocol → argus-core → argus-protocol`）。放在 `argus-protocol` 后依赖链是干净的单向图：

```text
argus-storage ──┐
argus-proxy   ──┼──→ argus-protocol ──→ argus-core
argus-server  ──┘
```

`argus-proxy` 的 `Cargo.toml` 中**没有** `sqlx` / `argus-storage` 依赖。`argus-server` 在启动时把 `Arc<PgSink>` 注入进去。CI 用 `cargo tree` 检查这条边不存在（见 `20-testing-strategy.md` §9）。

### 4.2 写入管线

```mermaid
flowchart LR
    O1["Observer 1"] --> Q{{"submit queue<br/>bounded 4096"}}
    O2["Observer 2"] --> Q
    O3["Observer N"] --> Q
    Q --> B["Batcher<br/>攒批: 256 条 或 200ms"]
    B --> W["Writer<br/>COPY / UNNEST"]
    W -->|成功| PG[("PostgreSQL")]
    W -->|失败| SP[["Spool<br/>本地磁盘"]]
    SP --> R["Replayer<br/>周期重试"] --> W
```

**关键**：`Observer → Q` 是 `try_send`；`Q → Batcher → Writer` 是独立 task，可以任意慢。Data Plane 与 Observer 都不等待写入完成。

---

## 5. Schema

完整 DDL 见 `migrations/`。以下是设计要点。

### 5.1 配置类表（低频写，强一致）

```sql
CREATE TABLE workspaces (
    id              UUID PRIMARY KEY,
    name            TEXT NOT NULL,
    slug            TEXT NOT NULL UNIQUE,
    settings        JSONB NOT NULL DEFAULT '{}',
    retention_days  INT  NOT NULL DEFAULT 90,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted_at      TIMESTAMPTZ
);

CREATE TABLE users (
    id            UUID PRIMARY KEY,
    email         CITEXT NOT NULL UNIQUE,
    password_hash TEXT,              -- argon2id；OIDC 用户为 NULL
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    disabled_at   TIMESTAMPTZ
);

CREATE TABLE memberships (
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    role         TEXT NOT NULL,
    PRIMARY KEY (user_id, workspace_id)
);

CREATE TABLE api_keys (
    key_id          TEXT PRIMARY KEY,          -- 12 字符 base62，见 09 §5.1
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id         UUID REFERENCES users(id) ON DELETE SET NULL,
    name            TEXT NOT NULL,
    secret_hash     BYTEA NOT NULL,            -- sha256(salt||secret)
    salt            BYTEA NOT NULL,
    prefix_display  TEXT NOT NULL,
    allowed_models     TEXT[],                 -- NULL = 不限制
    allowed_providers  UUID[],
    allowed_tools      TEXT[],
    -- 这把 Key 允许访问哪些 Profile。NULL = 全部（02 §11 的防枚举依赖它）
    allowed_profiles   TEXT[],
    -- Raw 模式是显式权限，默认关（04 §11）
    allow_raw_proxy    BOOLEAN NOT NULL DEFAULT false,
    ip_allowlist    CIDR[],
    limits          JSONB NOT NULL DEFAULT '{}',   -- rate / token / cost
    policy_id       UUID REFERENCES security_policies(id),
    expires_at      TIMESTAMPTZ,
    revoked_at      TIMESTAMPTZ,
    last_used_at    TIMESTAMPTZ,               -- 批量更新，精度 60s
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ON api_keys (workspace_id) WHERE revoked_at IS NULL;

CREATE TABLE provider_profiles (
    id            UUID PRIMARY KEY,
    workspace_id  UUID REFERENCES workspaces(id) ON DELETE CASCADE,  -- NULL = 全局
    -- URL 里出现的那个名字。字符集受限，见 02-routing-model.md OQ-R1
    name          TEXT NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    display_name  TEXT NOT NULL DEFAULT '',
    -- 仅用于 UI 模板与指标分组。Data Plane 不得据此分支（08 §4.1）
    provider_type TEXT NOT NULL,
    base_url      TEXT NOT NULL,
    -- 默认 observer，可被 observer_rules 细化（09 §4.2）
    observer      TEXT NOT NULL,
    -- injected / passthrough / none
    credential_mode TEXT NOT NULL,
    credential_id UUID REFERENCES provider_credentials(id) ON DELETE RESTRICT,
    credential_header TEXT,
    credential_format TEXT,
    extra_headers JSONB NOT NULL DEFAULT '{}',
    dialect       JSONB NOT NULL DEFAULT '{}',
    timeouts      JSONB NOT NULL DEFAULT '{}',
    retry         JSONB NOT NULL DEFAULT '{}',
    target_policy JSONB,                        -- NULL = 用全局策略
    security_policy_id UUID,
    enabled       BOOLEAN NOT NULL DEFAULT true,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name),
    -- injected 时 header/format 必填（08 §8）
    CHECK (credential_mode <> 'injected'
           OR (credential_id IS NOT NULL AND credential_header IS NOT NULL
               AND credential_format IS NOT NULL))
);

CREATE TABLE provider_credentials (
    id          UUID PRIMARY KEY,
    workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE,
    ciphertext  BYTEA NOT NULL,          -- AES-256-GCM，master key 不入库
    nonce       BYTEA NOT NULL,
    fingerprint TEXT NOT NULL,           -- base64(sha256(明文))[..12]，用于"我配的是哪一把"
    is_active   BOOLEAN NOT NULL DEFAULT true,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Observer 选择规则。**有序**，首个命中生效（09 §4.2）
CREATE TABLE observer_rules (
    id            UUID PRIMARY KEY,
    workspace_id  UUID REFERENCES workspaces(id) ON DELETE CASCADE,
    ord           INT NOT NULL,
    match_profile TEXT,
    match_host_suffix TEXT,
    match_path_prefix TEXT,
    match_method  TEXT,
    match_content_type TEXT,
    observer      TEXT NOT NULL,
    UNIQUE (workspace_id, ord)
);

-- Target Policy（SSRF 边界，13-ssrf-protection.md §5）
CREATE TABLE target_policies (
    id           UUID PRIMARY KEY,
    workspace_id UUID REFERENCES workspaces(id) ON DELETE CASCADE,
    schemes      TEXT[] NOT NULL DEFAULT ARRAY['https','http'],
    allow_cidrs  TEXT[] NOT NULL DEFAULT '{}',
    deny_cidrs   TEXT[] NOT NULL,
    allow_hosts  TEXT[] NOT NULL DEFAULT '{}',
    deny_hosts   TEXT[] NOT NULL DEFAULT '{}',
    deny_ports   INT[]  NOT NULL DEFAULT '{}',
    allow_literal_ip BOOLEAN NOT NULL DEFAULT true,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

> **v1 的 `models` 与 `provider_models` 两张表已删除。** 模型目录不再存在
> （`02-routing-model.md` §3），供应商上新模型不需要写库。
> `provider_endpoints` 也删了：一个 Profile 一个 `base_url`，多地域 = 多个 Profile。

### 5.2 审计类表（高频写，分区）

**核心决定：一次 interaction = 1 行 `requests` + 1 行 `request_payloads` + N 行 `tool_calls`。**

不把 messages / content_blocks 拆成行。理由见 §6。

```sql
-- 主表：只放可索引的标量维度
CREATE TABLE requests (
    request_id      UUID        NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL,
    workspace_id    UUID        NOT NULL,
    api_key_id      TEXT,
    user_id         UUID,
    trace_id        UUID        NOT NULL,
    conversation_id UUID,
    conversation_link TEXT,                  -- explicit | inferred | none

    -- 上游是怎么定下来的
    origin          TEXT NOT NULL,           -- managed | raw
    profile_id      UUID,                    -- origin=managed 时非空
    profile_name    TEXT,                    -- 冗余一份，profile 删了也能查历史
    target_host     TEXT NOT NULL,           -- raw 模式排查的主要维度
    target_path     TEXT NOT NULL,
    -- Observer 选出来的种类；解析失败时是 generic_sse / generic_json
    observer        TEXT NOT NULL,
    -- 从 body 里**观测到**的模型名。仅用于统计，不参与路由（02 §4.2）
    observed_model  TEXT,
    credential_mode TEXT NOT NULL,           -- injected | passthrough | none

    stream          BOOLEAN NOT NULL,
    status_code     SMALLINT,
    outcome         TEXT NOT NULL,           -- Outcome 枚举
    integrity       TEXT NOT NULL,           -- Complete | Degraded | MetadataOnly
    finish_reason   TEXT,
    error_kind      TEXT,

    -- 计量
    input_tokens          INT,
    output_tokens         INT,
    cache_read_tokens     INT,
    cache_creation_tokens INT,
    reasoning_tokens      INT,
    usage_source    TEXT NOT NULL,           -- reported | estimated | missing
    cost_micro_usd  BIGINT,

    -- 延迟（微秒）
    queued_us            INT,
    upstream_header_us   INT,
    ttft_us              INT,
    total_us             INT NOT NULL,
    proxy_overhead_us    INT,

    -- 路由
    attempts        SMALLINT NOT NULL DEFAULT 1,
    attempt_chain   JSONB,                   -- Vec<AttemptRecord>，通常很小

    -- 观测健康
    observe_mode    TEXT NOT NULL,           -- full | lite | off
    dropped_bytes   BIGINT NOT NULL DEFAULT 0,
    anomaly_count   SMALLINT NOT NULL DEFAULT 0,
    unknown_event_count SMALLINT NOT NULL DEFAULT 0,

    -- 安全
    security_action TEXT,                    -- allow | flag | warn | block | require_review
    security_hits   SMALLINT NOT NULL DEFAULT 0,
    -- fail-open 降级标记：非 NULL 表示这次「放行」是因为检查失败而非检查通过。
    -- ADR-012 原则 3 要求 fail-open 必须可审计，否则无法统计安全能力的实际覆盖率。
    security_degraded TEXT,                  -- timeout | extract_failed | rule_panic | detector_unavailable

    -- 布尔快捷字段（用于部分索引，避免查 JSONB）
    has_tool_calls  BOOLEAN NOT NULL DEFAULT false,
    has_images      BOOLEAN NOT NULL DEFAULT false,
    has_reasoning   BOOLEAN NOT NULL DEFAULT false,

    PRIMARY KEY (started_at, request_id)
) PARTITION BY RANGE (started_at);

-- 大内容单独存，可独立清理、独立加密、独立降级
CREATE TABLE request_payloads (
    request_id      UUID        NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL,
    workspace_id    UUID        NOT NULL,

    raw_request     JSONB,                   -- 原始请求体
    request_sha256  BYTEA,                   -- 去重与关联
    blocks          JSONB,                   -- Vec<ObservedBlock>，结构化的响应内容
    raw_events      JSONB,                   -- selective 策略下的原始事件
    provider_errors JSONB,
    unknown_events  JSONB,
    stats           JSONB,                   -- StreamStats

    redacted        BOOLEAN NOT NULL DEFAULT false,
    truncated       BOOLEAN NOT NULL DEFAULT false,

    PRIMARY KEY (started_at, request_id)
) PARTITION BY RANGE (started_at);

-- 工具调用：单独成表，因为需要按 tool name 索引与聚合
CREATE TABLE tool_calls (
    id              UUID        NOT NULL,
    request_id      UUID        NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL,
    workspace_id    UUID        NOT NULL,
    conversation_id UUID,

    block_choice    SMALLINT NOT NULL,
    block_item      INT      NOT NULL,
    block_part      INT      NOT NULL,

    tool_use_id     TEXT,
    tool_name       TEXT,
    arguments       JSONB,                   -- 解析成功时
    arguments_raw   TEXT,                    -- 解析失败/不完整时
    arguments_state TEXT NOT NULL,           -- parsed | invalid | incomplete | empty
    arguments_bytes INT NOT NULL DEFAULT 0,

    security_action TEXT,
    gated           BOOLEAN NOT NULL DEFAULT false,   -- 是否经过 ToolGate
    blocked         BOOLEAN NOT NULL DEFAULT false,

    PRIMARY KEY (started_at, id)
) PARTITION BY RANGE (started_at);

CREATE TABLE security_events (
    id              UUID        NOT NULL,
    occurred_at     TIMESTAMPTZ NOT NULL,
    workspace_id    UUID        NOT NULL,
    request_id      UUID,
    conversation_id UUID,
    api_key_id      TEXT,

    timing          TEXT NOT NULL,           -- preflight | stream | tool_gate | post
    rule_id         TEXT NOT NULL,
    rule_version    INT  NOT NULL,
    severity        TEXT NOT NULL,
    action          TEXT NOT NULL,
    target          TEXT NOT NULL,           -- RuleTarget

    match_span      INT4RANGE,
    redacted_sample TEXT,                    -- 已脱敏，见 08 §9.1
    match_hash      BIGINT,
    context         JSONB,                   -- 受 store_security_context 控制

    reviewed_at     TIMESTAMPTZ,
    reviewed_by     UUID,
    review_outcome  TEXT,

    PRIMARY KEY (occurred_at, id)
) PARTITION BY RANGE (occurred_at);

-- 用量聚合：小表，高频 UPSERT
CREATE TABLE usage_counters (
    scope_type   TEXT        NOT NULL,       -- workspace | api_key
    scope_id     TEXT        NOT NULL,
    window_kind  TEXT        NOT NULL,       -- minute | hour | day | month
    window_start TIMESTAMPTZ NOT NULL,
    instance_id  UUID        NOT NULL,       -- 每实例一行，避免写冲突

    requests        BIGINT NOT NULL DEFAULT 0,
    input_tokens    BIGINT NOT NULL DEFAULT 0,
    output_tokens   BIGINT NOT NULL DEFAULT 0,
    cost_micro_usd  BIGINT NOT NULL DEFAULT 0,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    PRIMARY KEY (scope_type, scope_id, window_kind, window_start, instance_id)
);

CREATE TABLE conversations (
    id              UUID PRIMARY KEY,
    workspace_id    UUID NOT NULL,
    api_key_id      TEXT,
    first_seen_at   TIMESTAMPTZ NOT NULL,
    last_seen_at    TIMESTAMPTZ NOT NULL,
    turn_count      INT NOT NULL DEFAULT 1,
    prefix_fingerprint BIGINT,               -- 用于 inferred 匹配
    tainted_at      TIMESTAMPTZ,
    taint_rule_id   TEXT
);
CREATE INDEX ON conversations (workspace_id, prefix_fingerprint, last_seen_at DESC);

-- 管理面操作审计
CREATE TABLE audit_events (
    id           UUID PRIMARY KEY,
    occurred_at  TIMESTAMPTZ NOT NULL,
    actor_type   TEXT NOT NULL,              -- user | admin_token | system
    actor_id     TEXT,
    workspace_id UUID,
    action       TEXT NOT NULL,              -- create_provider / read_raw_content / ...
    resource     TEXT NOT NULL,
    resource_id  TEXT,
    diff         JSONB,
    ip           INET,
    user_agent   TEXT
);
CREATE INDEX ON audit_events (occurred_at DESC);
CREATE INDEX ON audit_events (workspace_id, occurred_at DESC);
```

### 5.3 关键索引

```sql
-- 主查询路径：workspace + 时间倒序
CREATE INDEX ON requests (workspace_id, started_at DESC);
-- 按 key 查
CREATE INDEX ON requests (api_key_id, started_at DESC);
-- 会话链路
CREATE INDEX ON requests (conversation_id, started_at) WHERE conversation_id IS NOT NULL;
-- trace 关联（含 retry 的多次尝试）
CREATE INDEX ON requests (trace_id);
-- 错误排查（部分索引，只索引少数行）
CREATE INDEX ON requests (workspace_id, started_at DESC) WHERE outcome <> 'completed';
CREATE INDEX ON requests (workspace_id, started_at DESC) WHERE security_action IN ('block','require_review');
-- 安全检查被降级放行的请求（应当稀少；持续非零说明保护实际失效）
CREATE INDEX ON requests (workspace_id, started_at DESC) WHERE security_degraded IS NOT NULL;
CREATE INDEX ON requests (workspace_id, started_at DESC) WHERE has_tool_calls;
CREATE INDEX ON requests (workspace_id, started_at DESC) WHERE integrity <> 'complete';
-- 成本分析
CREATE INDEX ON requests (workspace_id, profile_name, started_at DESC);
-- raw 模式排查靠 host；observed_model 只做统计，不建索引（基数高、查询少）
CREATE INDEX ON requests (workspace_id, target_host, started_at DESC) WHERE origin = 'raw';

CREATE INDEX ON tool_calls (workspace_id, tool_name, started_at DESC);
CREATE INDEX ON tool_calls (request_id);
CREATE INDEX ON security_events (workspace_id, severity, occurred_at DESC);
CREATE INDEX ON security_events (rule_id, occurred_at DESC);
```

**部分索引（`WHERE` 子句）是这里的关键技巧**：`outcome <> 'completed'` 的行可能只占 0.1%，部分索引比全索引小 1000 倍，且写入开销几乎为零（绝大多数行不进索引）。

---

## 6. JSONB vs 关系模型：取舍

### 6.1 决定

| 数据 | 存法 | 理由 |
|------|------|------|
| 请求/响应的**标量维度** | 关系列 | 需要索引、聚合、排序 |
| messages / content blocks | **JSONB**（`request_payloads.blocks`） | 见下 |
| tool_calls | **关系表** | 需要按 tool_name 聚合与安全分析 |
| usage | 关系列 | 需要 SUM |
| attempt_chain | JSONB | 小、只在详情页展示 |
| security hit 详情 | 关系表（`security_events`） | 需要按 rule_id 聚合 |

### 6.2 为什么 content blocks 不拆表

假设拆成 `messages` + `content_blocks` 表：

| 指标 | 拆表 | JSONB |
|------|------|-------|
| 一次典型交互的行数 | 请求 8 条 message × 平均 2 block = 16 行 + 响应 3 行 ≈ **20 行** | **1 行** |
| 1000 req/s 的插入行数 | 20,000 行/s | 1,000 行/s |
| 索引维护开销 | 每行至少 2 个索引 | 1 个 |
| 查询"这次请求的全部内容" | JOIN + 排序 | 一次主键查 |
| 查询"包含某关键词的请求" | 可以用 B-tree/GIN | 需要 GIN on jsonb 或全表扫 |
| 清理 90 天前数据 | 两个表都要分区 | 一个表 |

**实际的查询模式是"按 request_id 取全部内容"**（在 Dashboard 详情页），而不是"跨 request 查询某个 message"。JSONB 完美匹配这个模式。

需要跨 request 的内容检索时（"谁提到过 AKIA"），有两条路：
1. 安全模块已经把命中的内容提取到 `security_events` 表了（有索引）。
2. 可选启用 GIN 索引：`CREATE INDEX ON request_payloads USING gin (blocks jsonb_path_ops);`——但这个索引很大（可能是数据的 30%），默认不建。

### 6.3 大 payload 的处理

一个长对话的 `raw_request` 可能是数百 KB。PostgreSQL 的 TOAST 会自动把 > 2KB 的值移到外部表并压缩。

```sql
-- 建议配置（PG 14+）
ALTER DATABASE argus SET default_toast_compression = 'lz4';
```

`lz4` 比默认的 `pglz` 快 3–5 倍（压缩与解压），压缩比略低。对我们的场景（写多读少、CPU 敏感）是明显更好的选择。

**截断策略**：

```toml
[storage.payload]
max_raw_request_bytes = "1MiB"
max_blocks_bytes      = "1MiB"
max_raw_events_bytes  = "2MiB"
on_overflow = "truncate"     # truncate | drop
```

超过上限时截断并置 `truncated = true`。不做拒绝——审计数据宁可残缺也不要丢失整条记录。

---

## 7. 批量写入

### 7.1 Batcher

```rust
pub struct Batcher {
    rx: mpsc::Receiver<Box<ObservedInteraction>>,
    batch: Vec<Box<ObservedInteraction>>,
    max_batch: usize,        // 256
    max_delay: Duration,     // 200ms
}

// 主循环
loop {
    tokio::select! {
        Some(item) = rx.recv() => {
            batch.push(item);
            if batch.len() >= max_batch { flush().await; }
        }
        _ = sleep_until(deadline) => { if !batch.is_empty() { flush().await; } }
        _ = shutdown.cancelled() => { flush().await; break; }
    }
}
```

`max_delay = 200ms` 意味着审计数据最多延迟 200ms 可见。对审计来说完全可接受，换来的是批量写的巨大收益。

### 7.2 写入方式

```rust
// 用 UNNEST 一次插入整批，一条 SQL、一次往返
sqlx::query(r#"
    INSERT INTO requests (request_id, started_at, workspace_id, ...)
    SELECT * FROM UNNEST(
        $1::uuid[], $2::timestamptz[], $3::uuid[], ...
    )
"#)
.bind(&request_ids).bind(&started_ats).bind(&workspace_ids)
.execute(&pool).await?;
```

对 256 条记录，`UNNEST` 比 256 次单独 INSERT 快约 50–100 倍（一次往返 vs 256 次，一次解析 vs 256 次）。

`COPY` 会更快（约再快 2–3 倍），但错误处理粒度粗（整批失败无法定位是哪一行）。**MVP 用 `UNNEST`**，等确认写入成为瓶颈再换 `COPY BINARY`。

### 7.3 队列满时的策略

```toml
[storage.sink]
queue_capacity = 4096
on_full = "degrade"         # degrade（默认）| drop_oldest | drop_new
```

| 策略 | 语义 | 适用 |
|------|------|------|
| `drop_oldest` | 丢最老的 | 保留最近数据，故障排查更有用 |
| `drop_new` | 丢新的 | 保证时间连续性 |
| `degrade` | 剥离 payload 只保留 `requests` 行 | **默认**：保住计费与统计，丢内容 |

`degrade` 是默认策略：`requests` 行只有几百字节，即使全部保留也很轻；而 `request_payloads` 才是大头。丢内容不丢计量，是正确的取舍。若 degrade 后队列**仍然**满，则退到二级策略 `drop_oldest`。

---

## 8. PostgreSQL 不可用（G-St3）

### 8.1 Spool

```text
argus_data/spool/
  ├── 000000000001.spool      # 已满，等待回放
  ├── 000000000002.spool      # 当前写入
  └── .checkpoint             # 回放进度
```

格式：长度前缀的帧 + CRC32 + 每帧独立的 zstd 压缩。**append-only**，无需 fsync 每条（fsync 每个文件切换时做一次）。

```toml
[storage.spool]
enabled       = true
dir           = "/var/lib/argus/spool"
max_bytes     = "1GiB"
max_age       = "24h"
segment_bytes = "64MiB"
on_full       = "drop_oldest"
```

### 8.2 状态机

```mermaid
stateDiagram-v2
    [*] --> Direct: 启动
    Direct --> Spooling: 写 PG 连续失败 3 次
    Spooling --> Draining: PG 健康检查恢复
    Draining --> Direct: spool 清空
    Draining --> Spooling: PG 再次失败

    note right of Spooling
        新数据全部进 spool
        Data Plane 完全无感
    end note
    note right of Draining
        新数据仍进 spool（保序）
        Replayer 从最老的 segment 读取回放
    end note
```

**Draining 时新数据仍进 spool** 是刻意的：这保证了写入 PG 的顺序与产生顺序一致，避免"新数据先进去、老数据后进去"造成的分区路由问题（老数据可能属于已经被清理的分区）。

### 8.3 回放的幂等性

回放可能重复（写 PG 成功但 checkpoint 更新失败）。因此所有插入用 `ON CONFLICT DO NOTHING`：

```sql
INSERT INTO requests (...) SELECT * FROM UNNEST(...)
ON CONFLICT (started_at, request_id) DO NOTHING;
```

主键 `(started_at, request_id)` 天然去重。

### 8.4 Spool 满

`max_bytes = 1GiB` 大约能存 100 万条 interaction（压缩后平均 1KB）。以 100 req/s 计算，可覆盖约 **2.8 小时**的 PG 故障。

满了之后按 `drop_oldest` 丢弃最老的 segment，并记录 `argus_spool_dropped_bytes_total`。**明确的取舍**：我们选择丢审计数据而不是阻塞服务。

### 8.5 直接回答「PostgreSQL 挂掉之后 Gateway 是否继续工作」

> **是。** 具体表现：
> - AI 请求转发：**完全正常**（配置在内存快照，见 `01` §7）
> - 认证：已缓存的 Key 正常最多 15 分钟；新 Key 返回 503（见 `09` §6.3）
> - 审计：写入 Spool，PG 恢复后自动回放
> - 配额：基于内存中的最后已知用量继续检查，可能超额
> - Control Plane：全部 503
> - 熔断、限流、路由：完全正常（都是内存状态）

---

## 9. 分区、保留与清理

### 9.1 分区策略

```sql
-- 按天分区（requests / request_payloads / tool_calls / security_events）
CREATE TABLE requests_2026_08_19 PARTITION OF requests
    FOR VALUES FROM ('2026-08-19') TO ('2026-08-20');
```

分区管理由 Argus 自己的后台 task 完成（不依赖 `pg_partman`，保持零外部扩展）：

```rust
// 每小时运行
async fn maintain_partitions(pool: &PgPool, cfg: &RetentionConfig) {
    // 预创建未来 7 天的分区
    for day in 0..7 { create_partition_if_missing(pool, today + day).await?; }
    // 删除超过保留期的分区
    for p in list_partitions(pool).await? {
        if p.upper_bound < now - cfg.retention { drop_partition(pool, &p).await?; }
    }
}
```

**为什么按天而不是按月**：按天让清理粒度更细（`retention_days = 7` 也能精确工作），且单个分区不会太大（100 req/s × 86400 = 864 万行/天，是一个健康的分区大小）。

代价：分区数量多（90 天保留 = 90 × 4 表 = 360 个分区）。PostgreSQL 对几百个分区的规划开销可接受（PG 12+ 有分区裁剪优化）。若保留期很长（> 180 天），改为按周分区。

### 9.2 分级保留

不同数据的价值衰减速度不同：

```toml
[storage.retention]
# requests 行（计量与统计）：保留久
requests_days = 365
# payload（prompt 内容）：保留短，且这是最占空间的
payloads_days = 30
# tool_calls：中等
tool_calls_days = 90
# security_events：保留久（合规）
security_events_days = 365
# audit_events（管理操作）：最久
audit_events_days = 730
```

因为 `requests` 与 `request_payloads` 是**分开的表**，可以有不同的保留期。这是 §5.2 拆表的主要收益之一：删掉 30 天前的 prompt 内容后，统计与计费数据完整保留一年，而空间占用降低 90%+。

Workspace 可以覆盖（`workspaces.retention_days`），但**只能更短不能更长**（防止单个 workspace 撑爆磁盘）。

### 9.3 脱敏

```toml
[storage.redaction]
mode = "on_write"          # off | on_write | on_read
```

| 模式 | 语义 | 取舍 |
|------|------|------|
| `off` | 原样存储 | 最高保真，审计库含敏感数据 |
| `on_write` | 落库前把检测到的凭据/PII 替换为占位符 | **不可逆**；审计库更安全 |
| `on_read` | 存原文，在 `/audit/.../raw` 返回时脱敏 | 可逆；但库里仍有明文 |

默认 `on_write`：审计数据库通常会被备份、复制到分析环境，明文凭据在里面是持续的风险。

脱敏的实现复用安全模块的 `RuleHit.span`（已经知道位置），做字节级替换：

```text
"my key is AKIAIOSFODNN7EXAMPLE"  →  "my key is AKIA[REDACTED:16]"
```

保留长度信息便于分析，不保留内容。

---

## 10. 成本计算

```rust
pub struct Pricing {
    pub input_per_mtok: u64,           // micro-USD per million tokens
    pub output_per_mtok: u64,
    pub cache_write_per_mtok: Option<u64>,
    pub cache_read_per_mtok: Option<u64>,
    pub reasoning_per_mtok: Option<u64>,
    pub effective_from: SystemTime,
}
```

成本在 `AuditSink` 中计算（不在主链路），存入 `requests.cost_micro_usd`。

**用 `micro_usd`（整数）而非浮点**：浮点累加会产生误差，且 `SUM(float)` 的结果依赖行顺序。整数微美元的精度（1e-6 USD）远超需要。

价格随时间变化。**但模型目录已删除**，价格无处可挂——第一阶段的处理是：
`requests.cost_micro_usd` 只在**上游自己返回了计费信息**时填，否则留 NULL。

Argus 不再自己算钱。理由与不维护模型目录一致：价格表和模型表一样会过期，
而算错的账比不算账更糟。需要成本核算时，用 `observed_model` + `usage` 列
在**下游**（BI / 财务系统）用它们自己的价格表算。（OQ-St5）

`usage_source = 'estimated'` 的行，其 `cost_micro_usd` 也是估算的。查询时应该能区分：

```sql
SELECT
  SUM(cost_micro_usd) FILTER (WHERE usage_source = 'reported')  AS confirmed_cost,
  SUM(cost_micro_usd) FILTER (WHERE usage_source = 'estimated') AS estimated_cost
FROM requests WHERE workspace_id = $1 AND started_at >= $2;
```

---

## 11. 容量估算

| 项 | 单条大小（压缩后） |
|---|---|
| `requests` 行 | ~400 B |
| `request_payloads` 行（典型对话） | ~4 KB |
| `request_payloads` 行（长对话 + raw_events） | ~40 KB |
| `tool_calls` 行 | ~500 B |
| `security_events` 行 | ~300 B |

| 负载 | 每天数据量 | 30 天 payload + 365 天 requests |
|------|-----------|-------------------------------|
| 10 req/s | ~4 GB | ~120 GB + ~1.3 GB |
| 100 req/s | ~40 GB | ~1.2 TB + ~13 GB |
| 1000 req/s | ~400 GB | ~12 TB + ~130 GB |

**结论**：payload 是绝对的大头。对小型部署（NAS、10 req/s），30 天保留需要 120 GB——这可能超出预期。因此提供：

```toml
[storage.payload]
# 只存请求，不存响应内容（响应可以从 blocks 的摘要还原大意）
store_raw_request = true
store_blocks      = true
store_raw_events  = false     # 默认 selective 已经很省，可完全关闭
# 极简模式：只存元数据
minimal = false
```

`18-deployment.md` 会给出不同部署规模的推荐配置。

---

## 12. Failure Modes

| 场景 | 行为 |
|------|------|
| PG 不可用 | Spool，见 §8 |
| PG 慢（连接池耗尽） | Batcher 阻塞 → submit queue 满 → `degrade` → 丢 payload 保 requests |
| 磁盘满（spool 无法写） | 丢弃并记 `argus_spool_write_failed_total`；持续告警 |
| 单条 payload 超大 | 截断 + `truncated=true` |
| 分区不存在（时钟跳变/回放老数据） | 插入失败 → 自动创建缺失分区后重试一次 |
| 回放重复 | `ON CONFLICT DO NOTHING` 去重 |
| 迁移失败 | 启动时执行迁移，失败则拒绝启动（不允许 schema 不一致运行） |
| 分区清理误删 | `drop_partition` 前检查 `upper_bound < now - retention` 且要求 `retention >= 1 day` |
| `usage_counters` 写冲突 | 每实例一行（PK 含 `instance_id`），无冲突 |

---

## 13. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| content blocks 存 JSONB | 写放大 20× → 1× | 跨 request 的内容检索需要 GIN 索引（默认不建） |
| `requests` / `request_payloads` 分表 | 可分级保留，空间省 90% | 详情查询要 JOIN（但是主键 JOIN，很快） |
| 按天分区 | 清理 O(1)，粒度细 | 分区数量多（90+/表） |
| `UNNEST` 而非 `COPY` | 错误可定位 | 比 COPY 慢 2–3 倍 |
| 200ms 批延迟 | 写入吞吐提升 50× | 审计数据延迟 200ms 可见 |
| Spool 本地磁盘 | PG 故障不丢数据，零外部依赖 | 磁盘满时仍会丢；单机故障时 spool 丢失 |
| `on_write` 脱敏默认 | 审计库不含明文凭据 | 不可逆，误报会永久损坏数据 |
| `micro_usd` 整数 | 无浮点误差 | 需要注意单位转换 |
| 自建分区管理 | 零扩展依赖 | 要自己写并测试（`pg_partman` 更成熟） |

---

## 14. Alternatives Considered

**A. 用 ClickHouse 存审计数据。**
压缩比与查询性能远超 PG。但违背 G5（`Argus + PostgreSQL` 即可运行），且给 NAS 用户增加一个重组件。**否决为默认**；作为可选的 Phase 3 双写目标（大规模部署）。

**B. 用 TimescaleDB 扩展。**
自动分区 + 压缩，很适合。但需要安装扩展，托管 PG（RDS/Cloud SQL）不一定支持。**否决为必需**，但如果检测到扩展存在可以自动启用超表（Phase 3）。

**C. 把 payload 存对象存储（S3/MinIO），PG 只存指针。**
大幅降低 PG 体积。但增加组件依赖，且详情查询变成两次 IO。**推迟**——对超大规模（> 1000 req/s）有价值，MVP 不做。

**D. 不做 Spool，PG 挂了就丢审计数据。**
简单得多。但审计数据丢失在合规场景不可接受，且 PG 的短暂重启（升级、failover）会造成完全可避免的丢失。**否决**，Spool 的实现成本不高。

**E. 每 token 写一条记录（完整 event log）。**
存储与写入量完全不可接受（1000× 放大）。**强烈否决**——这正是「大量 token 不应逐 token 写数据库」的含义。

**F. 用 `pg_partman` 管理分区。**
成熟可靠，但是一个扩展。托管 PG 不一定有。**否决**为默认，但如果检测到已安装则可以使用。

---

## 15. Decision

1. `argus-proxy` 通过 `InteractionSink` trait 与存储解耦，**编译期**不可能直连 DB。
2. 一次 interaction = 1 行 `requests` + 1 行 `request_payloads` + N 行 `tool_calls`（+ M 行 `security_events`）。
3. content blocks 存 JSONB，tool_calls 拆表（因为需要按 tool_name 索引与聚合）。
4. `requests` 与 `request_payloads` **分表**，支持分级保留（计量 365 天 / 内容 30 天）。
5. 按天 RANGE 分区，自建分区管理（零扩展依赖），清理靠 `DROP PARTITION`。
6. 批量写：256 条 / 200ms，用 `UNNEST`，`ON CONFLICT DO NOTHING` 保证幂等。
7. 队列满时的默认策略是 **`degrade`**（丢 payload 保 `requests` 行），而非无差别丢弃。
8. PG 不可用时写本地 Spool（1GiB / 24h），恢复后按序回放。**Data Plane 完全不受影响。**
9. 大量部分索引（`WHERE` 子句）覆盖排障与安全查询，写入开销近乎为零。
10. 默认 `on_write` 脱敏；`default_toast_compression = lz4`。
11. 成本用 `micro_usd` 整数，支持历史价格，估算与实测在查询中可区分。

---

## 16. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | `payloads_days = 30` 的默认值对小型部署仍可能过大（120 GB @ 10 req/s），是否应默认更短或默认 `minimal`？ | 30 天 | 部署文档编写时按规模给不同预设 |
| OQ-2 | 是否需要预计算的小时级/天级物化视图以加速 Dashboard？ | 未做 | `requests` 超过 1 亿行或 Dashboard 出现慢查询时 |
| OQ-3 | GIN 索引（内容检索）是否应该按 workspace 可选开启？ | 未做 | 有检索需求时 |
| OQ-4 | Spool 在多实例共享存储（NFS）下是否安全？ | 未验证 | 明确文档要求 spool 目录必须是实例独占的本地盘 |
| OQ-5 | `on_write` 脱敏的误报会永久损坏数据，是否应默认改为 `on_read`？ | 当前 `on_write` | 有误报数据后重新评估 |
| OQ-6 | 分区数量超过 500 时 PG 的规划开销是否可接受？ | 未测 | 保留期 > 180 天时测试 |
