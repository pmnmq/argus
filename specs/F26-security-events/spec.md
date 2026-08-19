# F26 · 安全事件存储与审计查询

| | |
|---|---|
| ID | F26 |
| 里程碑 | M3 |
| 依赖 | F24 |
| 主 crate | `argus-storage` + `argus-server` |
| 设计依据 | `docs/08-security.md` §8.4,§9；`docs/03-control-plane.md` §5.3,§6.2 |
| 宪法条款 | C15, C27, C29 |

## 1. 目标

让安全判定与审计数据可查、可处置，同时**审计库本身不成为泄漏源**。

## 2. 范围

**In scope**：`security_events` 写入、`PostGuard`、会话 taint、审计查询 API（游标分页）、`audit:read_content` 的二次授权与留痕、`on_write` 脱敏、`require_durable` 的 fail-close 路径。

**Out of scope**：Dashboard UI（→ F28）、LLM Detector（→ Phase 2）、`RequireReview` 审批流（→ Phase 3）。

## 3. 需求 · 事件写入

- **R1** THE SYSTEM SHALL 把 `SecurityVerdict` 的每个 `RuleHit` 写入 `security_events`。
- **R2** 写入 SHALL 走 F12 的批量管线（非阻塞，宪法 C5）。
- **R3** `redacted_sample` SHALL 只含前缀 + 长度，**绝不含完整匹配值**（宪法 C29）。
- **R4** `context` 字段 SHALL 受 workspace 的 `store_security_context` 开关控制。
- **R5** THE SYSTEM SHALL 落库 `requests.security_degraded`（fail-open 留痕，宪法 C15）。
- **R6** THE SYSTEM SHALL 支持四个 `timing` 取值：`preflight | stream | tool_gate | post`。

## 4. 需求 · PostGuard

- **R7** `PostGuard` SHALL 在流结束后于 Observer task 中运行，**永远不可阻断**。
- **R8** `PostGuard` SHALL 对完整的 `ObservedInteraction` 执行响应侧规则（assistant 文本、reasoning、tool 调用）。
- **R9** IF `PostGuard` 任何环节失败 THEN THE SYSTEM SHALL 静默跳过并计数（宪法 C17）。
- **R10** `PostGuard` 的耗时 SHALL 计入 `argus_security_check_duration_seconds{timing="post"}`。

## 5. 需求 · 会话 taint

- **R11** WHEN 判定为 `Block` 且策略启用 taint THE SYSTEM SHALL 标记该 `conversation_id`。
- **R12** taint SHALL 只对 `conversation_link = Explicit` 的会话生效（推断的 ID 会误伤无关请求）。
- **R13** taint SHALL 存于内存（TTL 默认 1 小时 + 容量上限 10k + LRU），**不落 PG**（重启失效可接受）。
- **R14** WHEN 后续请求命中 taint THE SYSTEM SHALL 返回 403 并说明原因与 `taint_rule_id`。
- **R15** THE SYSTEM SHALL 在文档中明示：taint 在多实例下不共享。

## 6. 需求 · 审计查询

- **R16** THE SYSTEM SHALL 提供 `docs/03` §5.3 的查询端点与过滤条件。
- **R17** 分页 SHALL 用游标（编码 `(started_at, request_id)`），**不用 OFFSET**。
- **R18** 默认响应 SHALL 只含摘要，不含 body。
- **R19** `/raw` 子资源 SHALL 要求 `audit:read_content` scope。
- **R20** 每次 `audit:read_content` 的使用 SHALL 产生一条 `audit_events`（宪法 C27）。
- **R21** `/raw` 响应 SHALL 按 workspace 的脱敏策略处理。
- **R22** Anthropic `thinking` 的 `signature` SHALL 在 `/raw` 响应中默认脱敏（它是密文，无分析价值）。
- **R23** THE SYSTEM SHALL 提供 `GET /security/events` 与 `PATCH /security/events/{id}`（处置）。
- **R24** THE SYSTEM SHALL 提供 `GET /stats/potential_double_billing`（列出 S1 重试，供财务核对，`docs/07` §7.3）。

## 7. 需求 · 脱敏与 fail-close

- **R25** WHERE `redaction.mode = "on_write"`（默认）THE SYSTEM SHALL 在落库前用 `RuleHit.span` 做字节级替换，保留长度信息。
- **R26** 脱敏 SHALL 不可逆；文档必须明示误报会永久损坏数据（`docs/10` OQ-5）。
- **R27** WHERE `storage.audit.require_durable = true` THE SYSTEM SHALL 在审计写入失败时拒绝请求。
- **R28** `require_durable` SHALL 默认 `false`，且启用时配置校验 SHALL 发出警告说明它使 PG 成为 Data Plane 硬依赖（违背宪法 C19 的设计意图）。

## 8. 接口契约

```rust
// argus-proxy/src/security/post.rs
pub struct PostGuard { engine: Arc<SecurityEngine> }
impl PostGuard {
    /// 在 Observer task 中调用。永不返回 Err。
    pub fn evaluate(&self, it: &ObservedInteraction, policy: &Policy) -> SecurityVerdict;
}

// argus-proxy/src/security/taint.rs
pub struct TaintStore { inner: moka::sync::Cache<Uuid, TaintRecord> }
impl TaintStore {
    pub fn mark(&self, conv: Uuid, rule: RuleId, ttl: Duration);
    /// 只对 Explicit 生效（R12）
    pub fn check(&self, trace: &TraceContext) -> Option<TaintRecord>;
}

// argus-server/src/admin/audit.rs
pub struct AuditQuery { /* filters + cursor + limit */ }
pub struct Cursor(String);   // base64((started_at, request_id))

// argus-storage/src/redact.rs
pub fn redact_on_write(payload: &mut PayloadRow, hits: &[RuleHit]);   // R25
```

## 9. 实现要点

- **R12 是防误伤的关键**。`TaintStore::check` 的签名接受 `&TraceContext` 而非 `Uuid`，从类型上强制调用方提供 `conversation_link`，避免忘记检查。
- taint 的 key 加 `workspace_id` 盐（与 `conversation_id` 一起哈希），跨租户绝不碰撞。
- **R17 的游标实现**：`WHERE (started_at, request_id) < ($1, $2) ORDER BY started_at DESC, request_id DESC`，配合 F12 的 `(workspace_id, started_at DESC)` 索引。
- **R20 容易漏**：把它做成 handler 的中间件而非手写调用，避免新增端点时忘记。
- R25 的脱敏用 `RuleHit.span`（F24 已提供位置），从后往前替换避免 offset 失效。
- `PostGuard` 在 Observer task 内同步调用（规则是 CPU 工作），但要有耗时上限——超时则跳过（R9）。

## 10. 任务

- [ ] T1 `security/post.rs`：`PostGuard` + 响应侧规则求值
- [ ] T2 `security/taint.rs`：`TaintStore`（moka + TTL + 容量）+ Explicit 限制
- [ ] T3 taint 检查接入主链路 P3
- [ ] T4 `security_events` 的行映射与批量写入（接 F12 管线）
- [ ] T5 `requests.security_degraded` 落库
- [ ] T6 `storage/redact.rs`：`on_write` 脱敏（用 F24 的 span）
- [ ] T7 `require_durable` 的 fail-close 路径 + 配置警告
- [ ] T8 `admin/audit.rs`：查询端点 + 游标分页
- [ ] T9 `audit:read_content` 的二次授权 + 留痕中间件（R19/R20）
- [ ] T10 `/raw` 的脱敏与 signature 处理（R21/R22）
- [ ] T11 `GET/PATCH /security/events` + `GET /stats/potential_double_billing`
- [ ] T12 测试：§11

## 11. 验收

```bash
cargo test -p argus-storage security_events redact
cargo test -p argus-server admin::audit
```

- [ ] `RuleHit` 的完整匹配值在**任何**落库字段中都不存在（grep 全表断言，宪法 C29）
- [ ] `security_degraded` 在 preflight 超时的请求上非 NULL（宪法 C15）
- [ ] `PostGuard` 抛异常 → interaction 仍正常落库，计数递增（R9）
- [ ] taint：`Explicit` 会话被阻断；**`Inferred` 会话不受影响**（R12 关键断言）
- [ ] taint 跨 workspace 不碰撞（相同 conversation_id、不同 workspace）
- [ ] 游标分页在 100 万行表上翻 10 页，每页 < 50 ms 且无重复/遗漏
- [ ] 无 `audit:read_content` scope 调 `/raw` → 403
- [ ] 有 scope 调 `/raw` → 200 且产生一条 `audit_events`（R20）
- [ ] `/raw` 响应中 `signature` 已脱敏（R22）
- [ ] `on_write` 脱敏：注入 AKIA 密钥 → 库中为 `AKIA[REDACTED:16]`，长度信息保留
- [ ] `require_durable = true` + PG 停止 → 请求返回 5xx（验证 fail-close 路径存在）
- [ ] `require_durable = true` 时配置校验输出警告（R28）

## 12. 交给后续

| 不做的事 | 承接 |
|---|---|
| Dashboard | F28 |
| `RequireReview` 审批流 | Phase 3 |
| taint 的 Redis 共享 | Phase 2 |
| LLM Detector | Phase 2 |
| GIN 内容检索 | Phase 3（`docs/10` OQ-3） |
