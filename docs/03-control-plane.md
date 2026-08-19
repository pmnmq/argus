# 03 · Control Plane

> 状态：Draft v1 · 最后更新 2026-08-19
> 前置：`01-architecture.md` §7（ConfigSnapshot）

---

## 1. Problem

管理面要提供 Provider、模型、用户、Key、策略的配置能力和统计查询能力。它天然是：

- **重查询**：Dashboard 会做聚合统计，可能扫描大量 `requests` 行。
- **重写入**：配置变更、批量导入。
- **弱可用性要求**：管理员看不到 Dashboard 是不便，AI 请求失败是事故。

如果 Control Plane 与 Data Plane 共享同一个数据库连接池、同一个 Tokio 线程池、同一份限流预算，那么一个写得不好的统计查询就能拖垮整个网关。

---

## 2. Goals

- G-C1 Control Plane 完全不可用时，Data Plane 继续用最后一次成功加载的 `ConfigSnapshot` 正常服务。
- G-C2 Control Plane 的数据库负载与 Data Plane 的审计写入负载互不阻塞。
- G-C3 配置变更有审计轨迹（谁、何时、改了什么、旧值）。
- G-C4 配置变更可以被验证（dry-run）与回滚。

## 3. Non-goals

- 不做前端 UI 实现（本文只定义 API 契约）。
- 不做多租户的物理数据隔离。
- 不做配置的 Git-ops 双向同步（单向导入见 §8）。

---

## 4. 隔离手段

三层隔离，从强到弱：

| 层 | 手段 | 默认 |
|---|---|---|
| 端口 | Control Plane 监听独立端口（默认 9900），可绑定到内网地址 | ✅ 启用 |
| 连接池 | 独立的 `PgPool`，`max_connections` 单独配置且更小 | ✅ 启用 |
| 进程 | `role = "data"` / `role = "control"` 分开部署 | 可选 |

```toml
[server]
listen        = "0.0.0.0:8080"     # data plane
admin_listen  = "127.0.0.1:9900"   # control plane
role          = "all"

[database]
# Data Plane 用：审计写入 + 配置加载
url             = "postgres://..."
max_connections = 16
# Control Plane 用：独立池，更小，且带更短的 statement_timeout
[database.admin]
max_connections   = 4
statement_timeout = "30s"
```

`statement_timeout = 30s` 是硬保护：任何管理查询超过 30 秒直接被 PG 杀掉，不会长期占用连接。

> 注意：这**不能**防止 Control Plane 的重查询消耗 PostgreSQL 的 CPU/IO 从而间接影响审计写入。真正的隔离需要读写分离（只读副本）。MVP 不做，但 §9 给出了预留。

---

## 5. API 设计

Base path：`/admin/v1`。认证见 §6。全部 JSON。

### 5.1 资源与端点

| 资源 | 端点 | 说明 |
|------|------|------|
| Provider | `GET/POST /providers`、`GET/PATCH/DELETE /providers/{id}` | provider 定义 |
| Endpoint | `GET/POST /providers/{id}/endpoints`、`PATCH/DELETE /endpoints/{eid}` | 一个 provider 可有多个地域/部署 |
| Credential | `POST /providers/{id}/credentials`、`DELETE /credentials/{cid}` | **只写不读**，见 §7 |
| Logical Model | `GET/POST /models`、`GET/PATCH/DELETE /models/{name}` | 逻辑模型 |
| Provider Model | `GET/POST /models/{name}/bindings`、`PATCH/DELETE /bindings/{bid}` | 逻辑模型 → provider 模型的绑定 |
| Workspace | `GET/POST /workspaces`、`GET/PATCH/DELETE /workspaces/{id}` | |
| User | `GET/POST /users`、… | |
| API Key | `POST /workspaces/{id}/keys`（返回明文**仅此一次**）、`GET /keys`、`DELETE /keys/{kid}` | |
| Policy | `GET/POST /policies`、`PATCH/DELETE /policies/{pid}` | 安全策略 |
| Rule | `GET/POST /policies/{pid}/rules` | 规则 |
| Config | `POST /config/reload`、`GET /config/snapshot` | 强制刷新 / 查看当前快照 |
| Audit | `GET /audit/requests`、`GET /audit/requests/{rid}`、`GET /audit/requests/{rid}/raw` | 见 §5.3 |
| Security | `GET /security/events`、`PATCH /security/events/{eid}` | 处置安全事件 |
| Stats | `GET /stats/usage`、`GET /stats/latency`、`GET /stats/errors` | 聚合查询 |
| Health | `GET /admin/healthz`、`GET /admin/diagnostics` | 见 §5.4 |

### 5.2 写操作的通用约定

- 所有写操作要求 `If-Match: <etag>`（乐观并发），避免并发覆盖。
- 所有写操作支持 `?dry_run=true`：执行完整校验并返回「若应用会发生什么」，但不落库。
- 所有写操作产生一条 `audit_events` 记录（actor、动作、资源、diff）。

**校验必须包含语义检查**，不只是 schema：

```text
POST /models/claude-sonnet/bindings  { provider: "openai", provider_model: "gpt-4o" }
→ 400: protocol_mismatch
  逻辑模型 claude-sonnet 声明 protocol=anthropic_messages，
  但 provider "openai" 的该 endpoint 只提供 openai_chat。
  跨协议调用在当前版本不受支持（见 docs/04-protocols.md §7）。
```

这类校验是 Argus 的核心价值之一：**在配置时就阻止不可能工作的组合**，而不是等到运行时报错。

### 5.3 审计查询

```http
GET /admin/v1/audit/requests
  ?workspace_id=...&api_key_id=...&model=...&provider=...
  &status=error&from=2026-08-01T00:00:00Z&to=2026-08-19T00:00:00Z
  &has_tool_calls=true&security_action=block
  &cursor=<opaque>&limit=50
```

- **游标分页**，不用 OFFSET（`requests` 表很大，OFFSET 会全表扫）。游标编码 `(started_at, request_id)`。
- 默认返回摘要（不含 body）。完整 raw 走 `/raw` 子资源，且需要额外 scope（见 §6.2）——prompt 内容是敏感数据。
- `/raw` 返回时按 workspace 的脱敏策略处理（见 `10-storage.md` §9）。

### 5.4 诊断端点

`GET /admin/diagnostics` 返回运行时自省信息，这是排障的主力：

```json
{
  "config": { "version": 412, "loaded_at": "...", "refresh_lag_ms": 3200 },
  "observation": {
    "active_observers": 1843,
    "tee_drop_rate_1m": 0.0002,
    "sink_queue_depth": 120,
    "sink_queue_capacity": 4096,
    "spool": { "enabled": true, "pending_bytes": 0, "oldest_age_s": null }
  },
  "upstream": {
    "openai":    { "state": "closed", "pool_idle": 12, "p95_ttft_ms": 640 },
    "anthropic": { "state": "half_open", "consecutive_failures": 3 }
  },
  "storage": { "pg": "up", "pool_in_use": 3, "pool_size": 16 }
}
```

---

## 6. Control Plane 认证

### 6.1 与 Data Plane 的 API Key 完全分离

Data Plane 的 API Key（`sk-argus-...`）**不能**用于 Control Plane。理由：泄漏一个应用的调用 key 不应导致配置被篡改。

Control Plane 支持两种主体：

| 主体 | 凭据 | 用途 |
|------|------|------|
| **Admin Session** | 用户登录后的 session cookie / JWT | Dashboard |
| **Admin Token** | `at-argus-...`，与调用 key 不同前缀 | CI/CD、IaC |

MVP 的登录方式：本地用户名 + 密码（argon2id）。OIDC/SAML 排在 Phase 3。

### 6.2 Scope 模型

```text
config:read        读取 provider/model/policy 配置
config:write       修改配置
keys:manage        创建/吊销 API Key
audit:read         读审计摘要
audit:read_content 读 raw prompt / response  ← 单独的高敏 scope
security:manage    处置安全事件、修改规则
stats:read         统计
```

`audit:read_content` 单独拆出来是关键设计：大多数运维人员需要看「谁在什么时候调了什么模型花了多少钱」，但**不需要看到用户的 prompt 内容**。默认角色里不含这个 scope。

每次 `audit:read_content` 的使用本身产生一条 `audit_events` 记录（**访问内容也要被审计**）。

---

## 7. Credential 管理

### 7.1 存储

Provider 的上游凭据（OpenAI key、AWS SigV4 密钥等）是系统里最敏感的数据。

```text
明文 credential
    ↓ AES-256-GCM（key 来自 master key）
密文 + nonce → provider_credentials.ciphertext
```

Master key 来源，按优先级：

1. `ARGUS_MASTER_KEY`（环境变量，32 字节 base64）
2. 文件路径 `credentials.master_key_file`（权限必须 0600，否则拒绝启动）
3. 外部 KMS（`aws-kms` / `vault`）—— 可选，Phase 3

**master key 绝不入库**。这保证仅有数据库备份泄漏时，credential 仍不可解密。

### 7.2 API 语义

- `POST /providers/{id}/credentials` 接受明文，返回 `{ id, fingerprint, created_at }`。
- **不存在读取明文的 API**。任何情况下都不返回。
- `fingerprint = base64(sha256(plaintext))[..12]`，用于「确认我配的是哪一把 key」而不泄漏内容。
- 轮换：新增一个 credential 并 `PATCH /providers/{id} { active_credential: <cid> }`，旧的保留一段时间后删除。这允许零停机轮换。

### 7.3 内存中的处理

```rust
pub struct ResolvedCredential {
    pub id: CredentialId,
    /// 已经预构造好的 header 值，避免每请求重新格式化与分配
    pub header: HeaderValue,          // 例如 "Bearer sk-..."
    pub kind: CredentialKind,
}
impl Drop for ResolvedCredential {
    fn drop(&mut self) { /* zeroize 敏感缓冲 */ }
}
```

`HeaderValue` 预构造是一个真实的性能点：每请求做一次 `format!("Bearer {}", key)` 会产生一次堆分配。预构造后每请求只是 `HeaderValue::clone()`（内部 `Bytes`，refcount）。

Credential 的 `Debug`/`Display` 实现必须打印 `<redacted>`。由 `#[derive]` 禁用 + 手写实现保证。

---

## 8. 配置的声明式导入

为了让配置可以进版本库，提供单向导入：

```bash
argus config apply --file argus-config.yaml          # 幂等 upsert
argus config apply --file argus-config.yaml --dry-run
argus config export --out argus-config.yaml          # 不含 credential 明文
```

```yaml
providers:
  - name: anthropic-prod
    kind: anthropic
    endpoints:
      - url: https://api.anthropic.com
        weight: 100
    credential_ref: env:ANTHROPIC_API_KEY      # 从环境读，不写进文件

models:
  - name: claude-sonnet
    protocol: anthropic_messages
    capabilities: { streaming: true, tools: true, vision: true, reasoning: true,
                    context_length: 200000, max_output: 64000 }
    bindings:
      - provider: anthropic-prod
        provider_model: claude-sonnet-4-6
        priority: 100
      - provider: bedrock-us-east-1
        provider_model: anthropic.claude-sonnet-4-6-v1:0
        priority: 50                            # fallback
```

`credential_ref: env:XXX` 让配置文件可以安全地提交到 Git。

**不做双向同步**：Git → DB 是单向的。DB 里通过 API 做的改动不会回写文件。双向同步的冲突处理复杂度远超收益。

---

## 9. Failure Modes

| 场景 | Data Plane 影响 | Control Plane 表现 |
|------|---------------|------------------|
| PG 完全不可用 | **无**（用内存快照 + Spool + stale key cache） | 全部 API 返回 503 |
| PG 慢（admin 池耗尽） | 无（独立池） | admin 请求排队 / 503 |
| ConfigRefresher 连续失败 | 无（保留旧快照），但配置变更不生效 | `/diagnostics` 显示 `refresh_lag_ms` 增长，触发告警 |
| 配置被改成非法值 | 取决于是否通过校验；`config apply` 的校验是第一道防线 | 400 + 具体原因 |
| 新快照构建时 OOM 风险 | 构建失败则保留旧快照 | 记 `config_reload_failed` |
| master key 丢失 | credential 无法解密 → **所有上游请求失败** | 启动时即检测并拒绝启动，避免半可用状态 |

**master key 丢失是唯一会同时打垮两个平面的故障**，因此 `13-deployment.md` §7 会强调它的备份要求。

---

## 10. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| 同进程双平面 | 部署简单，单二进制 | 需要靠端口/连接池/角色做软隔离；重查询仍可能间接影响 PG |
| Credential 只写不读 | 大幅降低泄漏面 | 管理员无法「看一眼确认」，只能靠 fingerprint |
| `audit:read_content` 单独 scope | prompt 内容最小权限 | 多一个权限维度，角色配置更复杂 |
| 配置周期 refresh | PG 故障隔离 | 变更最长 15s 生效（可用 LISTEN/NOTIFY 缩短） |
| 单向配置导入 | 无冲突处理 | API 改动不回写 Git，可能与文件漂移 |

---

## 11. Alternatives Considered

**A. Control Plane 独立成一个服务（不同二进制）。**
更强的隔离，但违背「单二进制」目标（G5）。折中方案是同一个二进制 + `role` 开关，既保留单文件分发，又允许生产环境分开部署。**采纳折中。**

**B. 配置存在 etcd / Consul 而不是 PostgreSQL。**
引入额外的强依赖组件，违背 G5。且我们已经需要 PostgreSQL 存审计。**否决。**

**C. 用 Kubernetes CRD + Operator 管理配置。**
只对 K8s 用户可用，NAS/单机场景完全无法工作。**否决**（可作为 Phase 4 的可选适配层，通过 `config apply` 实现）。

**D. Credential 明文存 DB，靠数据库权限保护。**
数据库备份、快照、只读副本都会带走明文。**否决。**

---

## 12. Decision

1. Control Plane 与 Data Plane 同进程，但独立端口、独立 PgPool、可通过 `role` 分离部署。
2. Control Plane 认证与 Data Plane API Key 完全隔离，`audit:read_content` 为独立高敏 scope 且其使用本身被审计。
3. Credential 用 master key（不入库）AES-256-GCM 加密，**无读取明文的 API**，只暴露 fingerprint。
4. 写操作支持 `dry_run` + `If-Match`，并做语义校验（尤其是 protocol 匹配）。
5. 提供单向的声明式配置导入/导出，credential 通过 `env:` 引用。
6. 审计查询用游标分页，raw 内容单独授权。

---

## 13. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-1 | 是否需要支持只读副本以彻底隔离统计查询？ | `requests` 表超过 1 亿行时 |
| OQ-2 | 统计聚合是否需要预计算物化视图（按小时/按天）？ | Dashboard 出现慢查询时，见 `10-storage.md` OQ-2 |
| OQ-3 | OIDC 接入的优先级 | 有企业用户需求时 |
| OQ-4 | 是否提供 credential 的「代理验证」端点（帮管理员测试 key 是否有效）？ | 实现时评估，注意不要成为凭据探测工具 |
