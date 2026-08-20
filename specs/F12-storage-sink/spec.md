# F12 · 存储 Schema 与 AuditSink

| | |
|---|---|
| ID | F12 |
| 里程碑 | M1 |
| 依赖 | F11 |
| 主 crate | `argus-storage` |
| 设计依据 | `docs/15-storage.md` §4–7,§10 |
| 宪法条款 | C5, C6, C9, C10, C15, C19, C30 |

## 1. 目标

建立数据库 schema 与批量落库管线。**关键约束：Data Plane 在编译期不可能直连数据库。**

## 2. 范围

**In scope**：全部迁移、`PgSink`（实现 `InteractionSink`）、Batcher、UNNEST 批量写、成本计算、迁移兼容性检查。

**Out of scope**：Spool（→ F18）、分区维护与保留（→ F27）、Control Plane CRUD（→ F22）、安全事件表的写入（→ F26，表结构本切片建好）。

## 3. 需求

- **R1** THE SYSTEM SHALL 建立 `docs/15` §5 的全部表与索引。
- **R2** `argus-proxy` SHALL NOT 依赖 `sqlx` 或 `argus-storage`（宪法 C6，CI 强制）。
- **R3** `PgSink` SHALL 实现 `argus_protocol::InteractionSink`，`try_submit` **非阻塞**（宪法 C5）。
- **R4** WHEN 提交队列已满 THE SYSTEM SHALL 按 `on_full = degrade` 剥离 payload 只保留 `requests` 行（宪法 C10、`docs/15` §7.3）。
- **R5** IF degrade 后队列**仍**满 THEN THE SYSTEM SHALL 按 `drop_oldest` 丢弃并计数。
- **R6** THE SYSTEM SHALL 按 `batch_size = 256` 或 `batch_delay = 200ms` 触发批量写。
- **R7** THE SYSTEM SHALL 用 `UNNEST` 单条 SQL 写入整批。
- **R8** 全部插入 SHALL 使用 `ON CONFLICT DO NOTHING`（为 F18 的回放幂等做准备）。
- **R9** 一次 interaction SHALL 产生 ≤ 3 类行：1 行 `requests` + 1 行 `request_payloads` + N 行 `tool_calls`（`docs/15` G-St2）。
- **R10** THE SYSTEM SHALL 在 AuditSink 的连接池上（**且仅在该池上**）设置 `SET synchronous_commit = off`（`docs/18` §6.1）。
- **R11** THE SYSTEM SHALL 落库 `integrity`、`outcome`、`dropped_bytes`、`anomaly_count`、`unknown_event_count`（宪法 C13）。
- **R12** THE SYSTEM SHALL 落库 `usage_source ∈ {reported, estimated, missing}`，估算值不混入真实字段（宪法 C14）。
- **R13** THE SYSTEM SHALL 用 `micro_usd` 整数计算成本，支持按 `effective_from` 的历史价格。
- **R14** WHEN payload 超过 `storage.payload.max_*` THE SYSTEM SHALL 截断并置 `truncated = true`。
- **R15** WHERE `redaction.mode = "on_write"` THE SYSTEM SHALL 在落库前脱敏（本切片只做接口，规则来自 F24）。
- **R16** 迁移 SHALL 遵循"先加后删"两阶段（宪法 C30）。
- **R17** WHEN 迁移失败 THE SYSTEM SHALL 拒绝启动（宪法 C20：启动严格）。

## 4. 接口契约

```rust
// argus-storage/src/sink/mod.rs
pub struct PgSink { tx: mpsc::Sender<SinkItem>, /* … */ }

impl argus_protocol::InteractionSink for PgSink {
    fn try_submit(&self, it: Box<ObservedInteraction>) -> Result<(), SinkFull> {
        match self.tx.try_send(SinkItem::Full(it)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(SinkItem::Full(it))) => self.degrade(it),   // R4
            Err(TrySendError::Closed(_)) => Err(SinkFull(it)),
        }
    }
}

enum SinkItem { Full(Box<ObservedInteraction>), Degraded(Box<RequestRow>) }

// argus-storage/src/sink/batcher.rs
pub struct Batcher { /* 见 docs/15 §7.1 */ }

// argus-storage/src/sink/writer.rs
pub trait BatchWriter: Send + Sync {
    async fn write(&self, batch: &[SinkItem]) -> Result<(), WriteError>;
}
pub struct PgWriter { pool: PgPool }
// F18 会加 SpoolWriter，并在二者间做状态机切换
```

```sql
-- migrations/0001_init.sql … 见 docs/15 §5 的完整 DDL
-- migrations/README.md 记录两阶段规则（C30）
```

## 5. 实现要点

- **R2 的实现是本切片的核心价值**：`argus-proxy/Cargo.toml` 里没有 `sqlx`。`argus-server` 在 boot 时构造 `Arc<PgSink>` 并作为 `Arc<dyn InteractionSink>` 注入 proxy。
- `BatchWriter` trait 是为 F18 预留的：F12 只有 `PgWriter`，F18 加 `SpoolWriter` 与状态机。**现在就抽出来，否则 F18 要改 Batcher。**
- degrade 的实现：从 `ObservedInteraction` 提取 `RequestRow`（几百字节）丢弃 payload，再 `try_send`。若还满则丢弃并计 `argus_sink_submitted_total{result="dropped"}`。
- `UNNEST` 的绑定用 sqlx 的数组绑定；每个字段一个 `Vec<T>`。字段多（`requests` 表 40+ 列），用宏或代码生成减少手写错误。
- **不做成本计算**（v2）：模型目录与价格表都已删除。`cost_micro_usd` 只在上游自己返回计费信息时填，否则 NULL，由下游用自己的价格表算（`docs/15` §10）。
- `ci/check-migration-compat.sh`：用上一个 release 的二进制对新 schema 跑冒烟（宪法 C30）。首个版本无上一版，脚本需优雅跳过。

## 6. 任务

- [ ] T1 `migrations/0001_init.sql`：配置类表（workspaces/users/memberships/api_keys/providers/…）
- [ ] T2 `migrations/0002_audit.sql`：审计类表（requests/request_payloads/tool_calls/security_events/usage_counters/conversations/audit_events）+ 索引
- [ ] T3 `migrations/README.md`：两阶段规则（C30）
- [ ] T4 `pool.rs`：data / admin 双池 + `synchronous_commit = off`（仅 sink 池）
- [ ] T5 `sink/mod.rs`：`PgSink` + `try_submit` + degrade 逻辑
- [ ] T6 `sink/batcher.rs`：攒批循环（含 shutdown flush）
- [ ] T7 `sink/writer.rs`：`BatchWriter` trait + `PgWriter`（UNNEST）
- [ ] T8 `sink/row.rs`：`ObservedInteraction` → 三类行的映射
- [ ] T9 `cost.rs`：micro_usd 计算 + 历史价格
- [ ] T10 `redact.rs`：脱敏接口（实现留 F24）
- [ ] T11 `ci/check-migration-compat.sh`
- [ ] T12 `argus-server` boot 接线：构造 PgSink 并注入 proxy（替换 F06 的 LogSink）
- [ ] T13 集成测试（testcontainers）

## 7. 验收

```bash
cargo test -p argus-storage                      # 含 testcontainers
cargo tree -p argus-proxy -e normal | grep -q sqlx && exit 1     # C6
./ci/check-invariants.sh
```

- [ ] `argus-proxy` 依赖树无 `sqlx` / `argus-storage`
- [ ] 一次 interaction 精确产生 1+1+N 行（R9）
- [ ] 队列灌满 → `requests` 行仍全部写入，`request_payloads` 缺失，指标 `result="degraded"`
- [ ] 重复提交同一 interaction → `ON CONFLICT DO NOTHING`，行数不变
- [ ] `integrity` / `usage_source` / `dropped_bytes` 正确落库
- [ ] 成本用整数，`SUM()` 结果与逐条相加一致（无浮点误差）
- [ ] payload 超限 → `truncated = true` 且行仍写入
- [ ] 迁移在空库上从零执行成功；重复执行幂等
- [ ] 迁移失败 → 进程拒绝启动（R17）
- [ ] 批量写 256 条的耗时 < 单条 × 20（验证 UNNEST 的收益）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| Spool 与 PG 故障降级 | F18 |
| 分区创建与保留清理 | F27 |
| Control Plane 的 CRUD | F22 |
| Credential 加解密 | F22 |
| 脱敏规则的实现 | F24 |
| `security_events` 的写入 | F26 |
