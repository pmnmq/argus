# F27 · 分区维护与分级保留

| | |
|---|---|
| ID | F27 |
| 里程碑 | M3 |
| 依赖 | F12 |
| 主 crate | `argus-storage` |
| 设计依据 | `docs/15-storage.md` §9,§11 |
| 宪法条款 | C10, C19 |

## 1. 目标

让数据量长期可控，且清理是 O(1) 的。**零外部扩展依赖**——不用 `pg_partman`、不用 TimescaleDB。

## 2. 范围

**In scope**：按天 RANGE 分区的自动创建与删除、分级保留、workspace 级覆盖、缺失分区的自愈、容量监控。

**Out of scope**：对象存储归档（→ Phase 3）、物化视图（→ Phase 3）、ClickHouse 双写（→ Phase 3）。

## 3. 需求

- **R1** THE SYSTEM SHALL 对 `requests` / `request_payloads` / `tool_calls` / `security_events` 按天 RANGE 分区。
- **R2** THE SYSTEM SHALL 由自身的后台 task 管理分区，**不依赖任何 PG 扩展**。
- **R3** THE SYSTEM SHALL 预创建未来 7 天的分区。
- **R4** THE SYSTEM SHALL 按 `docs/15` §9.2 的分级保留删除过期分区：`requests` 365 天、`payloads` 30 天、`tool_calls` 90 天、`security_events` 365 天、`audit_events` 730 天。
- **R5** 清理 SHALL 用 `DROP PARTITION`（O(1)），**不用 `DELETE`**。
- **R6** Workspace 可覆盖保留期，但**只能更短不能更长**（防止单租户撑爆磁盘）。
- **R7** IF 插入时目标分区不存在（时钟跳变 / 回放老数据）THEN THE SYSTEM SHALL 自动创建缺失分区并重试一次。
- **R8** `DROP PARTITION` 前 SHALL 校验 `upper_bound < now - retention` 且 `retention >= 1 day`（防误删）。
- **R9** IF 分区维护失败 THEN THE SYSTEM SHALL 记录并告警，**不影响写入路径**（宪法 C19）。
- **R10** THE SYSTEM SHALL 暴露分区数量、最老分区时间、各表磁盘占用的指标。
- **R11** THE SYSTEM SHALL 在 `argus doctor` 中检查：分区是否齐全、磁盘剩余空间、是否有超期未清理的分区。
- **R12** THE SYSTEM SHALL 提供 `storage.payload.minimal` 模式（只存元数据），供 `nano` profile 使用。

## 4. 接口契约

```rust
// argus-storage/src/partition.rs
pub struct PartitionManager { pool: PgPool, cfg: RetentionConfig }

impl PartitionManager {
    /// 每小时运行
    pub async fn maintain(&self) -> Result<MaintenanceReport, PartitionError>;
    pub async fn ensure_exists(&self, table: &str, day: jiff::civil::Date) -> Result<(), PartitionError>;  // R7
    pub async fn list(&self, table: &str) -> Result<Vec<PartitionInfo>, PartitionError>;
}

pub struct PartitionInfo {
    pub name: String, pub lower: jiff::civil::Date, pub upper: jiff::civil::Date,
    pub row_estimate: i64, pub size_bytes: i64,
}

pub struct MaintenanceReport {
    pub created: Vec<String>, pub dropped: Vec<String>,
    pub errors: Vec<PartitionError>, pub total_size_bytes: i64,
}
```

## 5. 实现要点

- **R5 是分表设计（F12）的兑现点**：`requests` 与 `request_payloads` 分开，才能让"内容存 30 天、计量存 365 天"成立，空间降低 90%+。
- **R8 的防误删要有多重保险**：(a) 校验上界 (b) `retention >= 1 day` (c) 只删名字符合 `{table}_{YYYY}_{MM}_{DD}` 模式的表 (d) 干跑模式先输出将删除的列表。
- R7 的自愈很重要：Spool 回放（F18）可能带来跨越多天的老数据。没有它，回放会在分区边界失败。
- 分区数量：90 天 × 4 表 = 360 个。`docs/15` OQ-6 指出 PG 对几百个分区的规划开销未测——本切片要**实测并回填**。
- 按天而非按月的理由（`docs/15` §9.1）：让 `retention_days = 7` 也能精确工作。若保留期 > 180 天，文档建议改按周。
- 维护 task 与写入路径共用 data 池但用低优先级（`statement_timeout` 设短），避免 DDL 阻塞写入。

## 6. 任务

- [ ] T1 `partition.rs`：`PartitionInfo` 查询（`pg_class` + `pg_inherits`）
- [ ] T2 分区创建（幂等）+ 预创建 7 天
- [ ] T3 分区删除 + R8 的四重防误删 + 干跑模式
- [ ] T4 分级保留配置 + workspace 覆盖（只能更短）
- [ ] T5 `ensure_exists` 自愈 + 插入失败重试一次（接入 F12 的 writer）
- [ ] T6 维护 task 调度（每小时）+ 失败隔离
- [ ] T7 指标（R10）
- [ ] T8 `argus doctor` 的分区与磁盘检查（R11）
- [ ] T9 `storage.payload.minimal` 模式
- [ ] T10 **实测分区数量对查询规划的影响**（R13，回填 `docs/15` OQ-6）
- [ ] T11 测试：§7

## 7. 验收

```bash
cargo test -p argus-storage partition     # testcontainers
```

- [ ] 空库启动 → 自动创建今天 + 未来 7 天的分区（4 张表 × 8 = 32 个）
- [ ] 重复运行 `maintain()` 幂等，不重复创建
- [ ] 模拟时间前进 40 天 → `payloads` 的旧分区被删除，`requests` 的保留
- [ ] `DROP PARTITION` 耗时 < 100 ms（对比 `DELETE` 的量级差异）
- [ ] **防误删**：把 `retention_days` 配成 0 → 拒绝执行并报错
- [ ] **防误删**：手动建一个名字不符模式的表 → 不被删除
- [ ] 干跑模式输出将删除的分区列表且不实际删除
- [ ] 插入 30 天前的数据（模拟 spool 回放）→ 分区自动创建、插入成功（R7）
- [ ] workspace 配 `retention_days = 1000`（超过全局 365）→ 被拒绝或截断到全局值
- [ ] 维护 task 抛异常 → 写入路径不受影响（R9）
- [ ] `minimal` 模式下 `request_payloads` 不产生行
- [ ] **实测**：360 个分区下的典型审计查询规划时间，记录到 `docs/15` OQ-6

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 对象存储归档 | Phase 3 |
| 统计物化视图 | Phase 3（`docs/15` OQ-2） |
| GIN 内容检索索引 | Phase 3 |
| 按周分区（长保留期场景） | 按需 |
