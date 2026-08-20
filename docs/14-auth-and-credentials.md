# 14 · 认证与凭据

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`06-data-plane.md` §4（P2–P4）、`07-control-plane.md` §6（管理面认证）

---

## 1. Problem

网关的认证要同时满足：

- **快**：P2–P4 在主链路上，每请求都跑。查一次数据库（哪怕 1ms）也会成为 TTFT 的显著组成。
- **安全**：API Key 是长期凭据，泄漏后果严重。不能明文存储，不能被时序攻击。
- **可用**：数据库挂了不能立刻拒绝所有请求（那等于把 PG 变成 SPOF）。
- **细粒度**：不同 Key 应能限制模型、provider、速率、token 量、成本。

这四条互相拉扯：快 → 要缓存；安全 → 缓存不能存明文；可用 → 缓存要能在 DB 挂时继续用；细粒度 → 缓存的对象更复杂。

---

## 2. Goals

- G-Au1 认证的 P50 开销 < 10 µs（缓存命中路径）。
- G-Au2 数据库中不存储可用于调用的完整 Key。
- G-Au3 PostgreSQL 不可用时，已知 Key 在一段时间内继续可用；未知 Key 一律拒绝。
- G-Au4 Key 的吊销在 15 秒内全网生效。
- G-Au5 限流与配额不引入外部强依赖。

## 3. Non-goals

- MVP 不做 OIDC / SAML / SCIM（Control Plane 的 Phase 3）。
- 不做 mTLS 客户端认证（可由前置负载均衡完成）。
- 不做精确到分的实时计费与出账。
- 不做跨实例的强一致配额（明确接受最终一致，见 §8.4）。

---

## 4. 实体模型

```mermaid
erDiagram
    WORKSPACE ||--o{ API_KEY : owns
    WORKSPACE ||--o{ MEMBERSHIP : has
    USER ||--o{ MEMBERSHIP : in
    ROLE ||--o{ MEMBERSHIP : grants
    API_KEY ||--o{ USAGE_RECORD : produces
    WORKSPACE ||--o{ QUOTA : constrains
    API_KEY ||--o{ QUOTA : constrains
```

| 实体 | 说明 |
|------|------|
| **User** | 一个人。有登录凭据（用于 Control Plane），可属于多个 Workspace。 |
| **Workspace** | 租户边界。所有资源（Key、配额、策略、审计数据）都归属于某个 Workspace。 |
| **Membership** | User × Workspace × Role。 |
| **Role** | 角色，携带一组 scope（见 `07-control-plane.md` §6.2）。 |
| **ApiKey** | Data Plane 的调用凭据。属于 Workspace，可选归属某个 User。 |
| **Quota** | 配额定义，可挂在 Workspace 或 ApiKey 上。 |

**Workspace 是隔离的根**。所有查询、所有配置、所有审计数据都必须带 `workspace_id` 过滤。这一点在 `15-storage.md` 的每个表设计里都体现。

---

## 5. API Key

### 5.1 格式

```text
sk-argus-<env>-<key_id>-<secret>
         │      │        └─ 32 字节随机（base62 编码，43 字符）
         │      └────────── 12 字符的 key_id（base62），可公开
         └───────────────── "live" | "test"
```

例：`sk-argus-live-a7Kq2mZx9Lp3-XbT4n...（43 chars）`

**为什么把 `key_id` 放进 Key 本身**：

朴素做法是"用整个 key 的 hash 查表"。这要求缓存以 hash 为 key，且数据库要在 hash 上建唯一索引。但**如果用慢哈希（argon2/bcrypt）**，每次认证要跑一次慢哈希（毫秒级），完全不可接受；**如果用快哈希（sha256）**，则数据库被拖库后可以暴力破解（尽管 32 字节随机的搜索空间实际上无法暴力破解）。

把 `key_id` 嵌入后：
- 查表用 `key_id`（明文，有索引，O(1)）
- 验证用 `secret` 与存储的 hash 比对
- 因为 `secret` 是 32 字节的**高熵随机值**（不是用户密码），用 SHA-256 + 每 Key 独立 salt 就足够——彩虹表和字典攻击对 256 位随机数无意义。
- 无需慢哈希，验证是一次 sha256（~1 µs）

```rust
// 存储
pub struct ApiKeyRecord {
    pub key_id: KeyId,                  // 明文，索引
    pub secret_hash: [u8; 32],          // sha256(salt || secret)
    pub salt: [u8; 16],
    pub prefix_display: CompactString,  // "sk-argus-live-a7Kq...XbT4" 供 UI 展示
    pub workspace_id: WorkspaceId,
    // ...
}

// 验证（常数时间比较）
fn verify(record: &ApiKeyRecord, secret: &[u8]) -> bool {
    let mut h = Sha256::new();
    h.update(&record.salt);
    h.update(secret);
    // subtle::ConstantTimeEq，防时序攻击
    h.finalize().ct_eq(&record.secret_hash).into()
}
```

> **明确的取舍**：这个方案的前提是 `secret` 必须是密码学随机的 32 字节，由服务端生成，**绝不允许用户自定义 Key**。如果允许用户自定义（可能是弱口令），就必须改用 argon2id。Control Plane 的创建 API 不接受用户提供的 secret。

### 5.2 创建与展示

- `POST /admin/v1/workspaces/{id}/keys` 返回完整 Key，**仅此一次**。
- 之后任何 API 只返回 `prefix_display`（前 20 后 4 字符）。
- 数据库中没有可还原完整 Key 的信息。

### 5.3 Key 属性

```rust
pub struct ApiKeyConfig {
    pub key_id: KeyId,
    pub workspace_id: WorkspaceId,
    pub user_id: Option<UserId>,
    pub name: CompactString,

    // 生命周期
    pub created_at: SystemTime,
    pub expires_at: Option<SystemTime>,
    pub revoked_at: Option<SystemTime>,
    pub last_used_at: AtomicU64,        // 见 §5.4

    // 访问控制
    pub allowed_models: Option<Arc<HashSet<CompactString>>>,     // None = 全部
    pub allowed_providers: Option<Arc<HashSet<ProviderId>>>,
    pub allowed_tools: Option<Arc<HashSet<CompactString>>>,       // 供 tool-abuse 规则用
    pub allowed_protocols: EnumSet<Protocol>,
    pub ip_allowlist: Option<Arc<IpRangeSet>>,

    // 限额
    pub rate_limit: Option<RateLimitConfig>,
    pub token_limit: Option<TokenLimitConfig>,
    pub cost_limit: Option<CostLimitConfig>,

    // 策略
    pub policy_override: Option<PolicyId>,
}
```

### 5.4 `last_used_at` 的写入问题

每请求更新 `last_used_at` 会产生与请求量相同的 UPDATE，对 PG 是灾难（每行的 HOT update + WAL）。

解法：内存 `AtomicU64` + 周期批量 flush：

```rust
// 每请求：一次 relaxed store，~1ns
key.last_used_at.store(now_secs, Ordering::Relaxed);

// 后台 task 每 60 秒：
//   收集所有 last_used_at 变化的 key，一次 UPDATE ... FROM (VALUES ...) 批量写
```

精度损失 60 秒，完全可接受。同样的手法用于 `usage` 计数（见 §8）。

---

## 6. 认证流程（P2）

```mermaid
flowchart TB
    A["提取凭据<br/>Authorization: Bearer / x-api-key"] --> B{"格式合法?"}
    B -->|否| E1["401 invalid_api_key"]
    B --> C["解析出 key_id"]
    C --> D{"KeyCache 命中?"}
    D -->|命中且新鲜| F["常数时间比对 secret"]
    D -->|未命中| G["查 PostgreSQL"]
    G -->|找到| H["写入 KeyCache"] --> F
    G -->|未找到| I["写入 NegativeCache"] --> E1
    G -->|PG 不可用| J{"KeyCache 有<br/>stale 条目?"}
    J -->|有且在 stale_ttl 内| F
    J -->|无| E2["503 auth_unavailable"]
    F -->|不匹配| E1
    F --> K{"expires_at / revoked_at?"}
    K -->|已过期/吊销| E3["401 key_expired / key_revoked"]
    K --> L{"IP allowlist?"}
    L -->|不匹配| E4["403 ip_not_allowed"]
    L --> M["PrincipalCtx 构造完成"]
```

### 6.1 凭据提取

按协议惯例接受多种 header（客户端 SDK 的行为不同）：

| Header | 来源 |
|---|---|
| `Authorization: Bearer sk-argus-...` | OpenAI SDK 风格 |
| `x-api-key: sk-argus-...` | Anthropic SDK 风格 |
| `api-key: sk-argus-...` | Azure SDK 风格 |

三者都接受。**提取后立刻从转发的 header 中剥离**（`06-data-plane.md` §6.4）。

### 6.2 KeyCache

```rust
pub struct KeyCache {
    /// key_id → 记录。用 moka 或手写分片 LRU。
    entries: Cache<KeyId, Arc<CachedKey>>,
    /// 不存在的 key_id，防止用随机 key 打爆数据库
    negative: Cache<KeyId, ()>,
}

pub struct CachedKey {
    pub config: Arc<ApiKeyConfig>,
    pub secret_hash: [u8; 32],
    pub salt: [u8; 16],
    pub loaded_at: Instant,
    pub generation: u64,       // 用于吊销传播，见 §6.4
}
```

```toml
[auth.cache]
capacity        = 50_000
ttl             = "5m"       # 正常刷新周期
stale_ttl       = "15m"      # PG 不可用时允许继续使用的时长
negative_ttl    = "30s"      # 不存在的 key 缓存时长
negative_capacity = 10_000
```

**负缓存是必须的**：没有它，攻击者用随机 key 就能对每个请求触发一次数据库查询。30 秒的负缓存把这个放大器关掉。

### 6.3 PG 不可用时的行为（G-Au3）

```rust
match cache.get(&key_id) {
    // 缓存新鲜，正常路径
    Some(k) if k.age() < ttl => verify(k),
    // 缓存过期，尝试刷新
    Some(k) => match db.load(&key_id).await {
        Ok(fresh) => { cache.insert(fresh); verify(fresh) }
        // 关键：DB 不可用时，用 stale 数据继续服务
        Err(DbError::Unavailable) if k.age() < stale_ttl => {
            metrics::counter!("argus_auth_stale_hit_total").increment(1);
            verify(k)
        }
        Err(_) => Err(AuthError::Unavailable),   // 503
    },
    // 缓存未命中且 DB 不可用 → 只能拒绝
    None => match db.load(&key_id).await {
        Ok(k)  => { cache.insert(k); verify(k) }
        Err(DbError::NotFound) => Err(AuthError::Invalid),
        Err(_) => Err(AuthError::Unavailable),   // 503，fail-close
    },
}
```

**这是 fail-close 与可用性的平衡点**：
- 已经在用的 Key（在缓存里）继续工作最多 15 分钟 → 现有流量不中断。
- 新的/未见过的 Key 被拒绝 → 不会因为 DB 挂了就放行任意凭据。

15 分钟的 `stale_ttl` 是取舍：太短则 PG 短暂抖动就影响服务；太长则吊销的 Key 会长时间有效。15 分钟对应"人工发现并修复 PG 故障"的量级。

### 6.4 吊销传播（G-Au4）

正常路径靠 `ttl = 5m` 自然过期。但吊销需要更快。

```text
Control Plane 吊销 Key
  → UPDATE api_keys SET revoked_at = now()
  → 同时 bump 一个全局 revocation_generation（存在 config 表）
  → ConfigRefresher（15s 周期）发现 generation 变化
  → 拉取「自上次以来被吊销的 key_id 列表」（增量，很小）
  → 从 KeyCache 中显式驱逐这些条目
```

增量拉取而不是全量刷新缓存，因为缓存可能有 5 万条。

若启用 `LISTEN/NOTIFY`（`01-architecture.md` §7.3），吊销可以做到亚秒级。

---

## 7. 授权（P3）

授权是纯内存的集合运算：

```rust
fn authorize(p: &PrincipalCtx, req: &RequestFacts) -> Result<(), AuthzError> {
    let k = &p.key;
    if !k.allowed_protocols.contains(req.protocol) {
        return Err(AuthzError::ProtocolNotAllowed(req.protocol));
    }
    if let Some(models) = &k.allowed_models {
        if !models.contains(req.model) { return Err(AuthzError::ModelNotAllowed); }
    }
    // allowed_providers 在选路阶段生效（见 07 §5.1），不在此处
    Ok(())
}
```

**注意 `allowed_providers` 的位置**：它不是在 P3 拒绝请求，而是在 P7 作为候选过滤条件。这样"这个 Key 只能用 Anthropic 官方不能用 Bedrock"就自然表达为路由约束，而不是二值的允许/拒绝。

### 7.1 Workspace 与 Key 的层级合并

```rust
pub struct EffectiveLimits {
    pub rate: Option<RateLimitConfig>,
    pub tokens: Option<TokenLimitConfig>,
    pub cost: Option<CostLimitConfig>,
    pub allowed_models: Option<Arc<HashSet<CompactString>>>,
}

// 合并规则：取交集 / 取更严格者
fn merge(ws: &WorkspaceLimits, key: &ApiKeyConfig) -> EffectiveLimits {
    EffectiveLimits {
        // 速率：取较小者
        rate: min_by_rate(ws.rate, key.rate_limit),
        // 模型：取交集（None 表示不限制）
        allowed_models: intersect(&ws.allowed_models, &key.allowed_models),
        // ...
    }
}
```

**Key 不能放宽 Workspace 的限制**，只能收紧。这个不变式在 Control Plane 保存时校验，运行时合并时也再保证一次（防御性）。

`EffectiveLimits` 在 KeyCache 载入时**预计算并缓存**，不在每请求做集合运算。

---

### 7.2 防枚举（硬要求）

以下三种情况必须对调用方**不可区分**：

```text
profile 根本不存在
profile 存在但这把 Key 无权访问
profile 存在、有权，但被禁用
```

前两种统一返回 `404 provider_not_found`。第三种可以返回 503 —— 因为能走到
这一步说明调用方本来就有权知道它存在。

不这么做的后果：攻击者拿一把低权限 Key，用 `/provider/{猜测}/…` 逐个探测，
就能把租户配了哪些供应商、甚至 Profile 命名习惯（`openai-prod` / `openai-dev`）
摸清楚。

同理，Raw 模式的 `target_denied` 不区分"私有地址"与"DNS 失败"
（`13-ssrf-protection.md` §8）。

---

### 7.3 两种上游凭据模式

| | Managed Credential（Injected） | Passthrough |
|---|---|---|
| 谁持有供应商凭据 | Argus（加密存储） | 客户端 |
| 客户端发什么 | Argus 的 API Key | 上游自己的凭据 |
| 主要用于 | Managed 模式（**主路径**） | Raw 模式、调试、特殊供应商 |
| Argus 能审计到凭据吗 | 只有 fingerprint | 只有 fingerprint（同样脱敏） |
| 风险 | 凭据集中，master key 是 SPOF | 客户端各自管理，Argus 无法统一轮换 |

**权限边界必须分清**：

1. Managed 模式下，客户端带的 Argus Key 在转发前**必须删除**，
   绝不能泄漏给上游（`05-transparent-proxy.md` §5.1）。
2. Passthrough 模式下，Argus **不看**也**不存**客户端的上游凭据，
   只在审计里记 `credential_mode = passthrough` 与 fingerprint。
3. `allow_raw_proxy` + `injected` 的组合**必须**配 target allowlist，
   否则调用方能用你保管的凭据打任意地址。配置期强制
   （`13-ssrf-protection.md` §9）。

---

## 8. 限流与配额（P4）

### 8.1 三个维度

| 维度 | 单位 | 窗口 | 典型用途 |
|------|------|------|---------|
| **Rate** | 请求数 | 秒/分 | 防滥用、保护上游 |
| **Token** | input + output tokens | 分/小时/天/月 | 成本控制的主要手段 |
| **Cost** | 货币（micro-USD） | 天/月 | 财务预算 |

三者独立，任一超限即拒绝（429）。

### 8.2 Rate limit：本地 GCRA

MVP 用进程内的 GCRA（Generic Cell Rate Algorithm，即 leaky bucket 的一种高效实现）：

```rust
// governor crate，无锁，per-key 状态只有一个 u64
pub struct RateLimiter {
    inner: governor::RateLimiter<KeyId, DashMapStateStore<KeyId>, QuantaClock>,
}
```

每次检查是一次 CAS，~50ns。状态是每 key 一个 `u64`（16 字节含 key），5 万个 key 只需 ~1MB。

**多实例的问题**：N 个实例各自限流，实际全局速率是配置值的 N 倍。

处理方式，按优先级：

1. **配置时除以实例数**（`rate_limit_divisor`，由部署方设置）。粗糙但零依赖，MVP 默认。
2. **可选 Redis 共享**（见 §8.5）。
3. 明确文档化：Argus 的限流是"保护性"的而非"计费性"的。真正的成本控制靠 token/cost 配额（那个是准确的，见 §8.3）。

### 8.3 Token / Cost 配额：写后核对

Token 与 Cost 有一个根本特性：**请求前无法准确知道会消耗多少**（output tokens 未知）。因此不能做"预扣"。

采用 **check-then-record（软配额）**：

```text
P4（请求前）：
  读取内存中的当前用量（近似值）
  if used >= limit: 429
  else: 放行

请求后（Observer → AuditSink）：
  实际 usage → 累加到内存计数器
  周期性（10s）flush 到 PG 的 usage_counters 表
  周期性（10s）从 PG 拉取其他实例的用量，合并
```

**必然存在超额**：
- 单实例内：并发请求同时通过检查，之后各自消耗 → 最多超出「并发数 × 单请求最大消耗」。
- 多实例：同步延迟 10 秒内的用量不可见 → 最多超出「10 秒的全网消耗量」。

**这是刻意接受的**。硬配额需要预扣 + 分布式锁，会给每个请求加上一次同步的分布式往返（数毫秒），且预扣量必须按最坏情况（max_tokens）估算，会大量误拒。

缓解：

```toml
[quota]
sync_interval = "10s"
# 软阈值：达到 90% 时开始收紧（拒绝 max_tokens 大的请求）
soft_threshold = 0.9
# 硬性余量：超过 limit × overshoot_tolerance 才真正硬拒
overshoot_tolerance = 1.05
```

`soft_threshold` 是一个实用技巧：接近配额时，优先拒绝那些"可能消耗很大"的请求（`max_tokens` 高的），让小请求继续通过。这让配额耗尽的过程更平滑。

### 8.4 配额的一致性模型（明确声明）

> **Argus 的 token/cost 配额是最终一致的软配额。** 在多实例部署下，实际消耗可能超出配置上限，超出量约等于「同步间隔内全网的消耗量」（默认 10 秒）。需要硬性上限的场景，请在 provider 侧同时配置组织级配额。

把这句话直接写进用户文档。**含糊其辞的配额承诺是运维灾难的来源。**

### 8.5 可选的 Redis

```toml
[redis]
enabled = false
url = "redis://..."
# Redis 不可用时的行为
on_failure = "degrade"    # degrade（降级本地）| fail_open | fail_close
```

启用后：
- Rate limit 用 Redis 的 GCRA 脚本（一次 EVALSHA，~0.3ms）
- Token/cost 计数器用 Redis 的 `INCRBY`，同步间隔可降到 1 秒

**Redis 不可用时必须降级而非拒绝服务**：

```rust
match redis_limiter.check(key).await {
    Ok(decision) => decision,
    Err(_) => {
        metrics::counter!("argus_redis_degraded_total").increment(1);
        // 降级到本地限流器（预算 = 全局 / 预估实例数）
        local_limiter.check(key)
    }
}
```

这直接回答"Redis 挂掉之后是否继续工作"：**继续，降级为本地限流**。

---

## 9. Data Model

见 `15-storage.md` §5 的完整 DDL。此处只列关键字段：

```sql
-- 核心索引
CREATE UNIQUE INDEX ON api_keys (key_id);                    -- 认证路径
CREATE INDEX ON api_keys (workspace_id) WHERE revoked_at IS NULL;
CREATE INDEX ON usage_counters (scope_type, scope_id, window_start);
```

---

## 10. Failure Modes

| 场景 | 行为 |
|------|------|
| 缺失/格式错误的 Key | 401，**不透露是哪种错误**（防枚举） |
| Key 不存在 | 401 + 负缓存 30s |
| Key 已吊销 | 401 `key_revoked`（这个可以明确，因为已通过 secret 验证） |
| Key 已过期 | 401 `key_expired` |
| PG 不可用 + 缓存命中 | 正常服务（最多 15 分钟），记 `argus_auth_stale_hit_total` |
| PG 不可用 + 缓存未命中 | 503 `auth_unavailable`（**fail-close**） |
| 配额超限 | 429 + `Retry-After` + `x-argus-quota-reset` |
| 限流触发 | 429 + `Retry-After`（GCRA 能算出精确的等待时间） |
| Redis 不可用 | 降级本地限流，记指标 |
| 配额同步 task 卡死 | 用量停止更新 → 配额检查基于陈旧数据 → 可能大幅超额。加 watchdog 指标 `argus_quota_sync_lag_seconds` |
| KeyCache 被填满（大量不同 key_id） | LRU 淘汰 + 负缓存保护；正常 Key 可能被挤出（会重新查 DB） |

---

## 11. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| key_id 嵌入 Key | O(1) 查找 + 快哈希验证 | Key 长度增加；不能允许用户自定义 Key |
| SHA-256 而非 argon2 | 验证 ~1µs 而非 ~1ms | 前提是 secret 必须高熵随机 |
| stale cache 15 分钟 | PG 抖动不中断服务 | 吊销的 Key 最多多活 15 分钟（正常路径是 15 秒） |
| 未命中 + PG 挂 = 503 | 不放行未知凭据 | PG 挂时新 Key 无法使用 |
| 软配额（写后核对） | 无预扣、无分布式锁、TTFT 不受影响 | 必然超额（已量化并文档化） |
| 本地限流为默认 | 零依赖 | 多实例时实际速率 = N × 配置 |
| `last_used_at` 批量写 | 消除每请求 UPDATE | 精度 60 秒 |
| `allowed_providers` 作为路由过滤而非授权拒绝 | 表达力更强 | 用户可能困惑于"为什么不是 403" |

---

## 12. Alternatives Considered

**A. 用 JWT 作为 API Key（自包含，无需查库）。**
优点：零查库。缺点：**无法吊销**（除非维护黑名单，那又要查库）；权限变更要等 token 过期；token 很长。AI 网关的 Key 是长期凭据，吊销能力是刚需。**否决**。

**B. 整个 Key 做 argon2id 哈希。**
安全性更高，但每次认证 ~1ms 且无法用 key_id 索引（必须遍历或存快哈希做索引）。对 32 字节随机 secret，argon2 的额外安全性是零（它防的是弱口令）。**否决**。

**C. 硬配额（请求前预扣 max_tokens）。**
准确，但：需要分布式锁/原子操作（每请求一次 Redis 往返）；按 max_tokens 预扣会大量误拒（实际消耗通常远小于 max_tokens）；退款逻辑复杂。**否决**，接受软配额并明确文档化。

**D. 把限流做在前置的 nginx/Envoy。**
可行，但限流需要 per-key 的配置，而 key 的配置在 Argus 的数据库里。同步这些配置到前置代理是额外的复杂度。**否决**，但不排斥用户在前面加一层粗粒度的 IP 限流。

**E. 不做负缓存，直接查库。**
一个简单的 `for i in {1..100000}; do curl -H "Authorization: Bearer sk-argus-live-random$i"` 就能打爆数据库。**否决**。

---

## 13. Decision

1. API Key 格式 `sk-argus-<env>-<key_id>-<secret>`，`key_id` 明文可索引，`secret` 为 32 字节 CSPRNG 随机值。
2. 存储 `sha256(salt || secret)`，验证用常数时间比较。**不允许用户自定义 Key**（这是快哈希方案的前提）。
3. KeyCache（正 + 负）分层，`ttl=5m` / `stale_ttl=15m` / `negative_ttl=30s`。
4. PG 不可用时：缓存命中 → 继续服务；缓存未命中 → **503 fail-close**。
5. 吊销通过 `revocation_generation` 增量传播，15 秒内生效（启用 NOTIFY 可到亚秒）。
6. Key 的限制只能收紧 Workspace 的限制，`EffectiveLimits` 预计算缓存。
7. `allowed_providers` 作为选路过滤条件，不作为授权拒绝条件。
8. Rate limit 用进程内 GCRA；Token/Cost 用**软配额（写后核对）**，一致性模型明确文档化。
9. Redis 可选；不可用时降级本地限流，**绝不因此拒绝服务**。
10. `last_used_at` 与 usage 计数走内存 + 周期批量 flush，主链路无 UPDATE。

---

## 14. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | `stale_ttl = 15m` 是否合适？ | 拍的值 | 有真实 PG 故障案例后 |
| OQ-2 | 软配额的超额量在真实负载下有多大？ | 未测 | 压测阶段量化并回填文档 |
| OQ-3 | 是否需要支持 Key 的自动轮换（双 Key 并存期）？ | 未做 | 有企业需求时 |
| OQ-4 | `soft_threshold` 的收紧策略（按 max_tokens 拒绝）会不会造成不公平？ | 未验证 | 实现后观察 |
| OQ-5 | 多实例下是否需要 gossip 而非通过 PG 同步用量？ | 用 PG | 实例数 > 20 且同步成为瓶颈时 |
