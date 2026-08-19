# F23 · ConfigSnapshot 与热更新

| | |
|---|---|
| ID | F23 |
| 里程碑 | M3 |
| 依赖 | F22 |
| 主 crate | `argus-core` + `argus-server` |
| 设计依据 | `docs/01-architecture.md` §7；`docs/03-control-plane.md` §5 |
| 宪法条款 | C19, C21 |

## 1. 目标

让业务配置在运行时生效，且**配置读取不产生任何数据库查询**——这是 PG 故障隔离的前提。

## 2. 范围

**In scope**：`ArcSwap<ConfigSnapshot>` 分发、`ConfigRefresher`、快照构建、请求内视图一致性、可选 `LISTEN/NOTIFY`、预计算（标签、累积权重、`EffectiveLimits`、header）。

**Out of scope**：进程配置的热更新（进程配置需重启，`docs/13` §5.1）。

## 3. 需求

- **R1** THE SYSTEM SHALL 用 `ArcSwap<ConfigSnapshot>` 分发业务配置；读取为无锁 load（宪法 C21）。
- **R2** 单个请求 SHALL 在入口处 `load()` 一次，整个生命周期使用同一快照（视图一致性）。
- **R3** `ConfigRefresher` SHALL 周期（默认 15s）从 PG 重建快照并原子替换。
- **R4** IF 刷新失败 THEN THE SYSTEM SHALL 保留上一个快照继续服务，并计 `argus_config_reload_failed_total`（宪法 C19）。
- **R5** IF 连续刷新失败超过阈值 THEN THE SYSTEM SHALL 提升日志级别并暴露 `refresh_lag_ms`。
- **R6** WHERE `config.notify = true` THE SYSTEM SHALL 监听 PG `LISTEN/NOTIFY` 通道以加速生效。
- **R7** IF NOTIFY 通道断开 THEN THE SYSTEM SHALL 自动退回周期轮询（可选增强不得成为依赖）。
- **R8** 快照构建 SHALL 预计算：指标标签（`&'static str`）、路由的累积权重数组、provider 的 `extra_headers`、`ResolvedCredential` 的 `HeaderValue`。
- **R9** 快照 SHALL NOT 包含 API Key（数量大且变化频繁，走 `KeyCache`）。
- **R10** THE SYSTEM SHALL 提供 `POST /admin/v1/config/reload` 强制刷新与 `GET /admin/v1/config/snapshot` 查看当前快照。
- **R11** 快照替换 SHALL NOT 影响正在进行的请求。
- **R12** THE SYSTEM SHALL 暴露 `argus_config_version` 与 `argus_config_loaded_timestamp`。

## 4. 接口契约

```rust
// argus-core/src/snapshot.rs
pub struct ConfigSnapshot {
    pub version: u64,
    pub loaded_at: SystemTime,
    pub providers: HashMap<ProviderId, Arc<ProviderConfig>>,
    pub logical_models: HashMap<CompactString, Arc<LogicalModel>>,
    pub routes: HashMap<(Protocol, CompactString), Arc<RouteTable>>,
    pub policies: Arc<PolicySet>,
    pub workspaces: HashMap<WorkspaceId, Arc<WorkspaceConfig>>,
    /// 预计算的 &'static str 标签池（R8）
    pub labels: Arc<LabelInterner>,
}

#[derive(Clone)]
pub struct ConfigHandle(Arc<ArcSwap<ConfigSnapshot>>);
impl ConfigHandle {
    #[inline] pub fn load(&self) -> arc_swap::Guard<Arc<ConfigSnapshot>> { self.0.load() }
    pub fn store(&self, s: ConfigSnapshot);
}

/// RouteTable 已预计算累积权重，选路时二分查找（零分配）
pub struct RouteTable { candidates: Vec<Arc<ModelBinding>>, cumulative_weights: Vec<u32> }

// argus-server/src/config_refresh.rs
pub struct ConfigRefresher { handle: ConfigHandle, repo: Arc<dyn ConfigRepo>, cfg: RefreshConfig }
impl ConfigRefresher { pub async fn run(self, shutdown: CancellationToken); }
```

## 5. 实现要点

- **R2 的实现方式**：在 Ingress 层 `load()` 一次并放进 `RequestCtx.snapshot: Arc<ConfigSnapshot>`。后续所有阶段用它，**不再 `load()`**。
- **R8 的标签 interning 要注意 `docs/11` OQ-3 指出的问题**：`Box::leak` 在频繁 reload 下会累积泄漏。用有界 interner（保留已见过的标签，跨快照复用），不是每次重建。
- 快照构建是全量重建（`docs/01` OQ-3 假设 < 1000 个 provider/model）。若将来超过 5k，改增量。构建耗时要记指标。
- NOTIFY 用独立的长连接；断开时记录并回退。**不要**让 NOTIFY 的失败影响周期刷新。
- `GET /config/snapshot` 的输出必须脱敏（credential 只显示 fingerprint）。
- 替换用 `ArcSwap::store`，旧快照被最后一个持有它的请求 drop 时释放——这天然满足 R11。

## 6. 任务

- [ ] T1 `snapshot.rs`：`ConfigSnapshot` / `ConfigHandle` / `RouteTable`
- [ ] T2 `LabelInterner`：有界、跨快照复用
- [ ] T3 快照构建器：从 `ConfigRepo` 拉取并预计算（R8）
- [ ] T4 `ConfigRefresher`：周期刷新 + 失败保留 + lag 指标
- [ ] T5 可选 `LISTEN/NOTIFY` + 断开回退
- [ ] T6 `RequestCtx.snapshot` 接线（R2），改造 F14/F16 的读取路径
- [ ] T7 `POST /config/reload` + `GET /config/snapshot`（脱敏）
- [ ] T8 指标（R12）+ `/admin/diagnostics` 的 config 段
- [ ] T9 测试：§7

## 7. 验收

```bash
cargo test -p argus-core snapshot
cargo test -p argus-server config_refresh
```

- [ ] bench：`ConfigHandle::load()` < 20 ns，零分配
- [ ] 请求处理期间发生快照替换 → 该请求全程使用旧快照（R2/R11）
- [ ] 新增 provider → 15 秒内生效
- [ ] **C19 断言**：PG 停止 → 快照保持不变，Data Plane 正常；`refresh_lag_ms` 上升
- [ ] 刷新失败 → 旧快照保留，`argus_config_reload_failed_total` 递增
- [ ] NOTIFY 启用时变更 < 1 秒生效；断开通道后自动退回 15 秒轮询
- [ ] `RouteTable` 的累积权重在快照构建时算好；`resolve()` 零分配
- [ ] 连续 1000 次快照重建后，`LabelInterner` 的内存不增长（无泄漏）
- [ ] `GET /config/snapshot` 输出无 credential 明文
- [ ] 1000 个 provider 的快照构建耗时 < 500 ms

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 增量快照更新 | Phase 3（`docs/01` OQ-3） |
| 安全策略的快照加载 | F24（`PolicySet` 字段本切片留好） |
