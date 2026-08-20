# F15 · 优雅停机与运维端点

| | |
|---|---|
| ID | F15 |
| 里程碑 | M1（**门禁 G-M1**） |
| 依赖 | F14 |
| 主 crate | `argus-server` |
| 设计依据 | `docs/17-reliability.md` §8；`docs/16-opentelemetry.md` §4,§7,§9；`docs/18-deployment.md` §4 |
| 宪法条款 | C15, C16, C19, C20, C27 |

## 1. 目标

让 Argus 成为一个**可运维的进程**：能优雅停机、能被监控、能自检。M1 的收尾切片。

## 2. 范围

**In scope**：四阶段停机、`/healthz` `/readyz` `/metrics`、指标接线、结构化日志与采样、`argus doctor`、启动依赖检查、fd 限制提升。

**Out of scope**：OpenTelemetry（→ Phase 2，feature 位留好）、`/admin/diagnostics`（→ F22）、告警规则（文档已有）。

## 3. 需求 · 停机

- **R1** WHEN 收到 SIGTERM THE SYSTEM SHALL 依次执行：`/readyz` 转 503 → 等待 `lb_drain_delay` → 停止 accept → 等待活跃流（≤ `drain_timeout`）→ flush sink → 退出。
- **R2** WHILE 处于 drain 阶段 THE SYSTEM SHALL 让已建立的流正常完成，不中断。
- **R3** IF `drain_timeout` 到期仍有活跃流 THEN THE SYSTEM SHALL 向其发出协议内 error 事件后关闭。
- **R4** WHEN 停机取消 Observer THE SYSTEM SHALL 用当前累积状态 `finish_partial(Outcome::Cancelled(Shutdown))` 并提交（**部分数据 > 无数据**）。
- **R5** THE SYSTEM SHALL 在退出前 flush AuditSink 的全部批次（≤ `sink_flush_timeout`）。
- **R6** IF 总时长超过 `force_kill_after` THEN THE SYSTEM SHALL 强制退出并记录未完成项。
- **R7** THE SYSTEM SHALL 用 `sd_notify` 报告 READY / STOPPING（systemd `Type=notify`）。

## 4. 需求 · 可观测

- **R8** `/healthz` SHALL 只要进程存活就返回 200。
- **R9** `/readyz` SHALL 检查：配置快照已加载、未在停机中、至少一个健康 provider。
- **R10** `/readyz` SHALL NOT 检查 PostgreSQL（宪法 C19）。
- **R11** THE SYSTEM SHALL 暴露 `docs/16` §4 的全部指标。
- **R12** 指标标签值 SHALL 为 `&'static str`，热路径**不得** `format!`（宪法 C21）。
- **R13** 未知模型名 SHALL 归入 `model="_unknown"` 标签，防止基数爆炸。
- **R14** 每个 fail-open 点 SHALL 有对应计数器（宪法 C15）。
- **R15** THE SYSTEM SHALL 输出结构化 access log；成功请求可采样，**错误 / 安全事件 / degraded 永不采样**。
- **R16** 日志 SHALL NOT 包含 prompt 内容、响应内容、tool arguments、任何凭据（宪法 C27，CI 强制）。
- **R17** 日志写入 SHALL 非阻塞，队列满时丢弃日志而非阻塞请求。
- **R18** THE SYSTEM SHALL 分解并上报 `proxy_overhead = pre_upstream + header_relay`（`docs/16` §5.1）。
- **R19** 计时 SHALL 使用单调时钟。

## 5. 需求 · 启动与自检

- **R20** WHEN 启动 THE SYSTEM SHALL 依次硬性检查：master key 可用（含权限 0600）、PG 可达、迁移完成、配置快照可构建。任一失败则拒绝启动（宪法 C20）。
- **R21** THE SYSTEM SHALL 软性检查 spool 目录可写、Redis 可达，失败仅警告。
- **R22** THE SYSTEM SHALL 尝试提升 `RLIMIT_NOFILE` 到硬限制；低于 `min_nofile` 时打印修复指引。
- **R23** THE SYSTEM SHALL 提供 `argus doctor`：检查配置、PG 连通、迁移状态、master key、provider credential 有效性、fd 限制、磁盘空间，并给出可执行建议。

## 6. 接口契约

```rust
// argus-server/src/shutdown.rs
pub struct ShutdownCoordinator {
    token: CancellationToken,       // 根 token，各处 child_token()
    active_requests: Arc<AtomicU64>,
    active_observers: Arc<AtomicU64>,
}
impl ShutdownCoordinator {
    pub async fn run(self, cfg: &ShutdownConfig) -> ExitReason;
}

// argus-server/src/boot.rs
pub async fn startup(cfg: &Config) -> Result<Runtime, StartupError>;   // R20/R21

// argus-server/src/doctor.rs
pub struct DoctorReport { pub checks: Vec<Check> }
pub struct Check { pub name: &'static str, pub status: Status, pub detail: String, pub fix: Option<String> }
```

```bash
argus doctor [--config path]      # 退出码 0=全绿 1=有警告 2=有错误
```

## 7. 实现要点

- **R10 是最容易被"好心"违反的一条**。在 `/readyz` 的实现旁写明注释与 `docs/16` §9 的链接。可提供 `?strict=true` 给确实需要的场景，但默认不查。
- `lb_drain_delay` 的作用是等 K8s endpoint 传播（`docs/17` §8.2）。没有它，SIGTERM 后立刻停 accept 会 RST 掉 LB 还在发的连接。
- R4 的 partial 提交：Observer 的 `select!` 分支收到取消后**不能直接 return**，必须走 `finish_partial` + `try_submit`。
- 指标标签的 `&'static str` 用 interning（注意 `docs/16` OQ-3 指出的 `Box::leak` 在频繁 reload 下会累积泄漏——直接用 `string_cache` 或有界 interner）。
- `argus doctor` 的价值在 `fix` 字段：每个失败都要给出可复制粘贴的修复命令。

## 8. 任务

- [ ] T1 `shutdown.rs`：四阶段协调器 + `sd_notify`
- [ ] T2 Observer 的取消分支：`finish_partial` + 提交（R4）
- [ ] T3 AuditSink 的 shutdown flush（R5）
- [ ] T4 `ops/health.rs`：`/healthz` `/readyz`（含 R10 的注释与 `?strict`）
- [ ] T5 `ops/metrics.rs`：exporter + 标签 interning + `_unknown` 归并
- [ ] T6 指标接线：把 `docs/16` §4 的指标埋到 F03–F14 的各处
- [ ] T7 `ops/logging.rs`：非阻塞 appender + 采样 + 永不采样的三类
- [ ] T8 `Timings` 采集与 `proxy_overhead` 分解（R18）
- [ ] T9 `boot.rs`：R20/R21 的依赖检查 + `rlimit` 提升
- [ ] T10 `doctor.rs`：R23 的检查项 + fix 建议
- [ ] T11 测试：§9

## 9. 验收 — 门禁 G-M1

```bash
cargo test -p argus-server
cd tests/e2e && pytest sdk_compat/          # 全绿
./ci/check-invariants.sh                    # 含日志字段检查
```

- [ ] SIGTERM 时有 10 条活跃 SSE 流 → **全部正常完成**，无一被切断
- [ ] 停机时的 partial interaction 已落库，`outcome = Cancelled(Shutdown)`
- [ ] `drain_timeout` 到期 → 剩余流收到协议内 error 事件
- [ ] `/readyz` 在 PG 停止后**仍返回 200**（R10 关键断言）
- [ ] `/readyz` 在停机开始后立刻 503
- [ ] `/metrics` 可被 Prometheus 抓取；标签基数在 1000 请求后不增长
- [ ] 日志中 grep 不到 prompt 内容 / key（宪法 C27）
- [ ] 日志队列灌满 → 请求延迟不变（R17）
- [ ] master key 权限 0644 → 拒绝启动 + 提示 `chmod 600`
- [ ] 迁移未执行 → 拒绝启动
- [ ] `argus doctor` 在健康环境全绿；人为破坏一项后精确定位并给出 fix
- [ ] **G-M1**：`pytest sdk_compat/` 全绿（latest + n-1 SDK）
- [ ] **G-M1**：24 小时长稳，RSS 曲线无单调增长

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| `/admin/diagnostics` | F22 |
| OpenTelemetry 实现 | Phase 2 |
| 混沌测试 | F21 |
| 7 天长稳 + 性能回填 | F28 |
