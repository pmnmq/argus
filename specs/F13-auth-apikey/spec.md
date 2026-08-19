# F13 · 认证、API Key 与授权

| | |
|---|---|
| ID | F13 |
| 里程碑 | M1 |
| 依赖 | F12 |
| 主 crate | `argus-proxy` + `argus-storage` |
| 设计依据 | `docs/09-auth.md` §5–7 |
| 宪法条款 | C17, C19, C27, C28 |

## 1. 目标

实现 Data Plane 的认证与授权。这是**唯一 fail-close 的子系统**（宪法 C17）。

## 2. 范围

**In scope**：Key 格式与生成、`KeyCache`（正/负/stale）、认证流程、授权检查、`EffectiveLimits` 合并、`last_used_at` 批量更新、吊销传播、`argus keys` CLI。

**Out of scope**：限流与配额（→ F19）、Control Plane 的 Key 管理 API（→ F22）、Redis（→ F19）。

## 3. 需求

- **R1** API Key SHALL 为 `sk-argus-<env>-<key_id>-<secret>`，`secret` 为 32 字节 CSPRNG 随机值。
- **R2** THE SYSTEM SHALL NOT 接受用户自定义的 secret（这是用快哈希的前提，`docs/09` §5.1）。
- **R3** THE SYSTEM SHALL 存储 `sha256(salt || secret)` 与 salt，**不存可还原的完整 Key**（宪法 C27）。
- **R4** THE SYSTEM SHALL 用常数时间比较验证 secret（`subtle::ConstantTimeEq`）。
- **R5** THE SYSTEM SHALL 接受 `Authorization: Bearer` / `x-api-key` / `api-key` 三种 header。
- **R6** THE SYSTEM SHALL 在转发前剥离客户端凭据 header 与所有 `x-argus-*` header（宪法 C28）。
- **R7** WHEN KeyCache 命中且新鲜 THE SYSTEM SHALL 不查数据库。
- **R8** IF PG 不可用且 KeyCache 有 stale 条目（< `stale_ttl`）THEN THE SYSTEM SHALL 放行并计 `argus_auth_stale_hit_total`（宪法 C19）。
- **R9** IF PG 不可用且 KeyCache 未命中 THEN THE SYSTEM SHALL 返回 **503**（宪法 C17：fail-close）。
- **R10** THE SYSTEM SHALL 维护负缓存（`negative_ttl = 30s`）防止随机 key 打爆数据库。
- **R11** THE SYSTEM SHALL 在 401 响应中**不区分**"key 不存在"与"格式错误"（防枚举）；但 `key_revoked` / `key_expired` 可明确（已通过 secret 验证）。
- **R12** THE SYSTEM SHALL 检查 `expires_at` / `revoked_at` / `ip_allowlist`。
- **R13** THE SYSTEM SHALL 通过 `revocation_generation` 增量传播吊销，15 秒内生效。
- **R14** `EffectiveLimits` SHALL 由 workspace 与 key 合并，**key 只能收紧不能放宽**，且在 KeyCache 载入时预计算。
- **R15** THE SYSTEM SHALL 把 `allowed_providers` 作为**选路过滤条件**而非授权拒绝条件（`docs/09` §7）。
- **R16** `last_used_at` SHALL 用内存 `AtomicU64` + 60 秒批量 flush，主链路**不得**产生 UPDATE。
- **R17** 认证的缓存命中路径 SHALL 在 p50 < 10 µs 内完成。
- **R18** WHERE `trusted_proxies` 已配置 THE SYSTEM SHALL 从 `X-Forwarded-For` 取真实 IP；否则忽略 XFF（防 allowlist 绕过）。

## 4. 接口契约

```rust
// argus-proxy/src/auth/mod.rs
pub struct Gatekeeper { cache: KeyCache, repo: Arc<dyn KeyRepo>, cfg: AuthConfig }

impl Gatekeeper {
    pub async fn authenticate(&self, headers: &HeaderMap, peer: IpAddr)
        -> Result<PrincipalCtx, AuthError>;
    pub fn authorize(&self, p: &PrincipalCtx, facts: &RequestFacts) -> Result<(), AuthzError>;
}

// argus-core（trait 在 core，实现在 storage —— 保持 C6）
pub trait KeyRepo: Send + Sync {
    async fn load(&self, key_id: &KeyId) -> Result<ApiKeyRecord, RepoError>;
    async fn revoked_since(&self, gen: u64) -> Result<(u64, Vec<KeyId>), RepoError>;
    async fn flush_last_used(&self, rows: &[(KeyId, SystemTime)]) -> Result<(), RepoError>;
}

pub struct PrincipalCtx {   // docs/02 §10
    pub workspace_id: WorkspaceId,
    pub api_key_id: ApiKeyId,
    pub user_id: Option<UserId>,
    pub scopes: ScopeSet,
    pub limits: Arc<EffectiveLimits>,     // 预计算（R14）
}

pub enum AuthError { Invalid, Expired, Revoked, IpNotAllowed, Unavailable }
```

```bash
argus keys create --workspace default --name my-app [--expires 90d] [--models a,b]
argus keys list --workspace default
argus keys revoke <key_id>
```

## 5. 实现要点

- **R2 是 R3 用 SHA-256 而非 argon2 的前提**。在 `keys create` 的代码里加注释锁死这个关联，避免将来有人"顺手"加了自定义 secret 功能而不知道会削弱哈希强度。
- `KeyCache` 用 `moka::sync::Cache`（分片并发 + TTL 内建）。正负两个独立实例。
- stale 判定：`moka` 的 TTL 到期会驱逐，因此**不能直接用 TTL 表达 stale**。做法：`ttl = stale_ttl`（15min），条目内自带 `loaded_at`，逻辑上 `age > 5min` 视为需刷新但仍可用。
- R11 的"不区分"要落实到**响应体与时序**：无论哪种失败都走同一条返回路径，避免时序侧信道。
- `EffectiveLimits` 的合并（R14）要在 KeyCache 插入时做一次，不在每请求做集合运算（`docs/09` §7.1）。
- `authenticate` 是 `async` 但缓存命中路径**不 await**（`moka::sync` 是同步的）；只有 miss 才 await 数据库。

## 6. 任务

- [ ] T1 `auth/key.rs`：Key 格式、生成、解析、`verify`（常数时间）
- [ ] T2 `auth/cache.rs`：`KeyCache` 正/负/stale 三态
- [ ] T3 `auth/extract.rs`：三种 header 提取 + 剥离（R5/R6）+ XFF 处理（R18）
- [ ] T4 `auth/authn.rs`：`docs/09` §6 的完整流程图
- [ ] T5 `auth/authz.rs`：`authorize` + `EffectiveLimits` 合并（只收紧）
- [ ] T6 `auth/revocation.rs`：`revocation_generation` 增量拉取 + 缓存驱逐
- [ ] T7 `auth/last_used.rs`：AtomicU64 + 60s 批量 flush
- [ ] T8 `argus-storage/src/repo/keys.rs`：`KeyRepo` 实现
- [ ] T9 `argus keys` CLI 子命令
- [ ] T10 接入 F06 的主链路（P2–P3 阶段）
- [ ] T11 测试：§7

## 7. 验收

```bash
cargo test -p argus-proxy auth
cargo test -p argus-storage keys
```

- [ ] Key 生成 → 验证成功；篡改 1 bit → 失败
- [ ] **PG 挂 + 缓存命中 → 200**；`argus_auth_stale_hit_total` 递增（R8）
- [ ] **PG 挂 + 缓存未命中 → 503**（R9，fail-close）
- [ ] 随机 key 连打 1000 次 → 数据库查询次数 ≤ 负缓存容量（R10）
- [ ] 吊销后 15 秒内所有实例拒绝（R13）
- [ ] key 尝试放宽 workspace 的 `allowed_models` → 合并结果仍是交集（R14）
- [ ] 转发到上游的 header 中**不含**客户端的 `Authorization` / `x-api-key` / 任何 `x-argus-*`（宪法 C28）
- [ ] 未配 `trusted_proxies` 时伪造 XFF 无法绕过 `ip_allowlist`（R18）
- [ ] 1000 次认证请求，`last_used_at` 的 UPDATE 次数 ≤ 1（R16）
- [ ] bench：缓存命中路径 p50 < 10 µs（R17）
- [ ] 401 的四种成因在响应体与耗时上不可区分（R11）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 限流、token/cost 配额 | F19 |
| Control Plane 的 Key 管理 API 与 scope | F22 |
| Admin Session / Admin Token | F22 |
| OIDC | Phase 3 |
