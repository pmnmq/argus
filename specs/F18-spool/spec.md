# F18 · Spool 与存储降级

| | |
|---|---|
| ID | F18 |
| 里程碑 | M2 |
| 依赖 | F12 |
| 主 crate | `argus-storage` |
| 设计依据 | `docs/10-storage.md` §7.3,§8 |
| 宪法条款 | C10, C15, C19 |

## 1. 目标

让 PostgreSQL 故障对 Data Plane **完全不可见**。这是宪法 C19 的实体化。

## 2. 范围

**In scope**：Spool segment 格式、写入器、状态机（Direct/Spooling/Draining）、Replayer、幂等回放、容量与保留、实例独占锁。

**Out of scope**：`storage.audit.require_durable = true` 的 fail-close 路径（仅需接口存在，行为在 F26 完善）。

## 3. 需求

- **R1** WHEN 写 PG 连续失败 3 次 THE SYSTEM SHALL 进入 `Spooling` 状态，新数据全部写本地磁盘。
- **R2** WHILE 处于 `Spooling` THE Data Plane SHALL 完全不受影响（吞吐、延迟、成功率均无变化）。
- **R3** WHEN PG 恢复 THE SYSTEM SHALL 进入 `Draining`，由 Replayer 按**产生顺序**回放最老的 segment。
- **R4** WHILE 处于 `Draining` 新数据 SHALL 仍写入 spool（保序，避免新数据先于老数据落库导致分区路由问题）。
- **R5** WHEN spool 清空 THE SYSTEM SHALL 回到 `Direct`。
- **R6** 回放 SHALL 幂等（依赖 F12 已有的 `ON CONFLICT DO NOTHING`）。
- **R7** Segment 格式 SHALL 为长度前缀 + CRC32 + 每帧独立 zstd 压缩，append-only。
- **R8** THE SYSTEM SHALL 在 segment 切换时 fsync，**不**对每条记录 fsync。
- **R9** IF spool 超过 `max_bytes` 或 `max_age` THEN THE SYSTEM SHALL 丢弃最老的 segment 并计 `argus_spool_dropped_bytes_total`（宪法 C15：数据永久丢失必须留痕）。
- **R10** IF spool 目录不可写 THEN THE SYSTEM SHALL 禁用 spool 并持续告警，但**不拒绝服务**（宪法 C19）。
- **R11** THE SYSTEM SHALL 在 spool 目录持有独占锁文件；IF 检测到其他实例的活跃锁 THEN 拒绝启动（多实例共享 spool 会损坏数据）。
- **R12** IF 读取到损坏的 segment 帧（CRC 不匹配）THEN THE SYSTEM SHALL 跳过该帧、计数、继续回放，不中止。
- **R13** THE SYSTEM SHALL 暴露 `argus_sink_state`、`argus_spool_pending_bytes`、`argus_spool_oldest_age_seconds`、`argus_spool_write_failed_total`。
- **R14** 停机时 THE SYSTEM SHALL flush 当前 segment 并更新 checkpoint。

## 4. 接口契约

```rust
// argus-storage/src/spool/mod.rs
pub struct Spool { dir: PathBuf, _lock: FileLock, current: SegmentWriter, cfg: SpoolConfig }
impl Spool {
    pub fn open(cfg: &SpoolConfig) -> Result<Self, SpoolError>;   // 含 R11 的锁检测
    pub fn append(&mut self, batch: &[SinkItem]) -> Result<(), SpoolError>;
    pub fn pending_bytes(&self) -> u64;
    pub fn oldest_age(&self) -> Option<Duration>;
}

// 复用 F12 定义的 BatchWriter trait
pub struct SpoolWriter { spool: Mutex<Spool> }
impl BatchWriter for SpoolWriter { .. }

// argus-storage/src/sink/state.rs
pub enum SinkState { Direct, Spooling { since: Instant }, Draining }
pub struct StatefulWriter { pg: PgWriter, spool: Option<SpoolWriter>, state: SinkState }
impl BatchWriter for StatefulWriter { /* 状态机在此 */ }

// argus-storage/src/spool/replay.rs
pub struct Replayer { /* 周期任务：读 segment → PgWriter → 更新 checkpoint → 删 segment */ }
```

Segment 帧格式：

```
┌────────┬────────┬──────────────────┬────────┐
│ len:u32│ crc:u32│ zstd(bincode(item))│ ...    │
└────────┴────────┴──────────────────┴────────┘
```

## 5. 实现要点

- **R4 是最容易做错的**：直觉是"恢复后新数据直接写 PG，老数据慢慢补"，但那会让新数据先落库。若老数据属于已被清理的分区（F27），插入会失败。保序是必须的。
- checkpoint 文件记录 `(segment_id, offset)`。先写 PG 再更新 checkpoint —— 崩溃时会重放，由 R6 的幂等兜住。
- `FileLock` 用 `fs2::FileExt::try_lock_exclusive`，并在锁文件里写 PID + 启动时间，便于诊断。
- zstd 用 level 3（快，压缩比够）。压缩在 `spawn_blocking` 中做（`docs/01` §6.4：昂贵操作不放 observer task）。
- 状态机的转换要有滞后：`Direct → Spooling` 需连续 3 次失败；`Spooling → Draining` 需 PG 健康检查连续 2 次成功。避免抖动。
- **R2 的验证是本切片的核心**，见 §7。

## 6. 任务

- [ ] T1 `spool/segment.rs`：帧格式读写 + CRC + zstd
- [ ] T2 `spool/mod.rs`：`Spool::open`（含 `FileLock`）+ append + 切换 segment + fsync
- [ ] T3 `spool/retention.rs`：`max_bytes` / `max_age` 的丢弃 + 计数
- [ ] T4 `sink/state.rs`：`StatefulWriter` 三态状态机（含滞后）
- [ ] T5 `spool/replay.rs`：Replayer + checkpoint + 损坏帧跳过
- [ ] T6 停机 flush（R14），接入 F15 的停机流程
- [ ] T7 指标（R13）
- [ ] T8 `argus doctor` 增加 spool 检查项（磁盘空间、锁、pending）
- [ ] T9 集成测试：§7

## 7. 验收

```bash
cargo test -p argus-storage spool         # testcontainers，含 PG 启停
```

- [ ] **核心断言**：PG 停止期间连续 100 请求，**全部 200**，p99 延迟与 PG 正常时相比无统计显著差异（R2）
- [ ] PG 停止 → `sink_state = spooling`，`spool_pending_bytes > 0`
- [ ] PG 恢复 → 自动 `draining` → `direct`，**数据零丢失、零重复**（行数精确匹配）
- [ ] 回放期间新产生的数据在老数据**之后**落库（按 `started_at` 排序验证顺序）
- [ ] 人为在 checkpoint 更新前杀进程 → 重启后重放，`ON CONFLICT` 去重，行数正确
- [ ] 人为损坏一个 segment 帧的 CRC → 该帧被跳过并计数，其余正常回放
- [ ] spool 超 `max_bytes` → 丢最老 segment + `argus_spool_dropped_bytes_total` 递增
- [ ] spool 目录只读 → 启动时警告、spool 禁用、**服务正常**（R10）
- [ ] 两个实例指向同一 spool 目录 → 第二个拒绝启动，错误信息清晰（R11）
- [ ] 停机时 spool 被 flush，重启后可继续回放
- [ ] 2.8 小时容量估算验证：以 100 req/s 灌入，确认 1 GiB 能容纳约 2.5–3 小时（`docs/10` §8.4）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| `require_durable = true` 的 fail-close | F26 |
| PVC / StatefulSet 部署形态 | F28（文档） |
| 对象存储归档 | Phase 3 |
