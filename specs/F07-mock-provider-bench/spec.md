# F07 · Mock Provider 与性能基线 ⛔ 门禁

| | |
|---|---|
| ID | F07 |
| 里程碑 | M0（**门禁 G-M0**） |
| 依赖 | F06 |
| 主 crate | `tests/` + `benches/` |
| 设计依据 | `docs/16-opentelemetry.md` §5–6；`docs/20-testing-strategy.md` §7；`docs/22-mvp-roadmap.md` §5 风险 1 |
| 宪法条款 | C16, C21, C22, C24 |

## 1. 目标

建立可复现的性能测量基础设施，并**回答 M0 的唯一问题：Tee 架构的开销是否真的可以忽略。**

> 这是全项目唯一的硬门禁。不达标就停下重新设计 `ADR-009`，**不得**继续 M1。
> 依据：`docs/22` §5 风险 1 —— 若这条假设不成立，整个"零成本观测"前提崩塌。

## 2. 范围

**In scope**：`MockProvider`（可编程 scenario）、对照法压测脚本、criterion 微基准、内存/CPU 采集、门禁判定脚本、**基线报告**。

**Out of scope**：真实 provider 的 contract test（→ F10）、长稳 7 天测试（→ F28）。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供本地 `MockProvider`，支持 `docs/20` §7 定义的全部 scenario。
- **R2** `MockProvider` SHALL 支持 `Pacing::Realtime`（按录制时的真实间隔回放），使 TTFT 与 token 间隔有意义。
- **R3** THE SYSTEM SHALL 用**对照法**测量 proxy 开销：`客户端→Mock` 与 `客户端→Argus→Mock` 的差值（`docs/16` §5.3）。
- **R4** THE SYSTEM SHALL 分别在 `observe=Full` 与 `observe=Off` 两种模式下测量，以隔离观测的成本。
- **R5** THE SYSTEM SHALL 在 100 / 1,000 / 10,000 并发流下采集：延迟分布、RSS、CPU、`tee_dropped_bytes`。
- **R6** THE SYSTEM SHALL 提供 criterion 微基准：`SseFramer::push`（× 切分策略）、`AnthropicMessagesParser::on_frame`、`Accumulator::apply`。
- **R7** THE SYSTEM SHALL 断言热路径分配次数：转发一帧主链路 0 次；稳态每 text delta 0 次（宪法 C21）。
- **R8** THE SYSTEM SHALL 产出 `bench/BASELINE.md`，内容标注为**实测**并记录环境（CPU/内存/OS/rustc 版本/日期）。
- **R9** THE SYSTEM SHALL 提供 `ci/gate-m0.sh`，按 §7 判定并以退出码表达通过与否。
- **R10** IF 门禁不通过 THEN 团队 SHALL 停止 M1 工作并记录一份 `bench/M0-FAILURE.md` 说明哪条不达标及初步归因。

## 4. 接口契约

```rust
// tests/support/mock_provider.rs
pub struct MockProvider { /* axum server on ephemeral port */ }

impl MockProvider {
    pub async fn start() -> Self;
    pub fn url(&self) -> Url;
    pub fn set_scenario(&self, key: &str, s: Scenario);
    pub fn requests_seen(&self) -> Vec<RecordedRequest>;
    /// 断言上游连接是否已被关闭（验证取消传播）
    pub fn connection_closed(&self, req_id: &str) -> bool;
}

pub enum Scenario {          // 见 docs/20 §7
    Replay { fixture: PathBuf, pacing: Pacing },
    SlowFirstToken { delay: Duration, then: Box<Scenario> },
    TruncateAfter { frames: usize },
    HttpError { status: u16, body: &'static str, retry_after: Option<u32> },
    OkThenErrorEvent,
    OversizedFrame { bytes: usize },
    NoDoneSentinel,
    Synthetic { tokens: usize, interval: Duration },
    Hang,
}
pub enum Pacing { Realtime, AsFastAsPossible, Fixed(Duration) }
```

```bash
# 压测入口
ci/bench-e2e.sh --streams 10000 --observe full  --duration 300s
ci/bench-e2e.sh --streams 10000 --observe off   --duration 300s
ci/gate-m0.sh                                    # 读取上面产出的 JSON 并判定
```

## 5. 实现要点

- `MockProvider` 必须支持 **h2**（否则测不出 `docs/17` §4.2 的连接复用效应，宪法 C23）。
- 对照组（直连 Mock）与实验组必须用**同一个客户端进程、同一份负载脚本**，只改 base_url。否则差值不可信。
- 内存采集读 `/proc/self/status` 的 `VmRSS`，采样间隔 1s，取稳态段的 p50/p99（跳过前 30 秒预热）。
- CPU 用 `/proc/self/stat` 的 utime+stime 差分，除以墙钟与核数。
- 分配次数断言用一个计数分配器（`#[global_allocator]` 包装，仅测试 build 启用）或 `dhat-rs`。
- **`Synthetic` scenario 是压测主力**：不依赖 fixture，可精确控制 token 数与间隔，便于扫参。

## 6. 任务

- [ ] T1 `tests/support/mock_provider.rs`：axum server + scenario 分发 + h2 支持
- [ ] T2 `Pacing::Realtime` 的时间戳录制与回放
- [ ] T3 全部 9 种 scenario 实现
- [ ] T4 `benches/framer.rs` / `parser.rs` / `accumulator.rs`（criterion）
- [ ] T5 计数分配器 + 分配次数断言测试（R7）
- [ ] T6 `ci/bench-e2e.sh`：oha/k6 驱动 + 资源采集 + JSON 输出
- [ ] T7 `ci/gate-m0.sh`：判定逻辑 + 明确的失败输出
- [ ] T8 跑基线：100 / 1k / 10k × {Full, Off}
- [ ] T9 撰写 `bench/BASELINE.md`（含环境、方法、原始数据、结论）
- [ ] T10 **把实测数字回填 `docs/16-opentelemetry.md` §6，并把"目标，未验证"改为"目标 / 实测"双栏**（宪法 C16）

## 7. 验收 — 门禁 G-M0

| # | 判据 | 阈值 | 依据 |
|---|------|------|------|
| G1 | `proxy_added_ttft` p50（连接复用，observe=Full） | **< 2 ms** | `docs/22` §3 M0 |
| G2 | `proxy_added_ttft` p99 | **< 15 ms** | 同上 |
| G3 | 10k 并发流 RSS（observe=Full，稳态 p99） | **< 1.5 GB** | 同上 |
| G4 | CPU(observe=Full) / CPU(observe=Off) − 1 | **< 30%** | 同上 |
| G5 | `SplitStrategy` 全矩阵结果一致 | **100%** | 宪法 C24 |
| G6 | 主链路转发一帧的堆分配 | **= 0** | 宪法 C21 |
| G7 | 10k 流下 `tee_dropped_bytes / bytes_forwarded` | **< 1%** | `docs/16` §6.3 |

```bash
./ci/gate-m0.sh && echo "G-M0 PASS → 可以进入 M1" || echo "G-M0 FAIL → 停止，写 M0-FAILURE.md"
```

**不通过时的处置**（写进 `bench/M0-FAILURE.md`）：
1. 哪条不达标、差多少
2. 归因：是 tee 本身、parser CPU、还是 hyper/rustls 的固有开销？
3. 备选方案评估：`ObserveMode` 默认降为低比例采样（机制已在 `docs/10` §6.3 就位，非灾难性重构）

## 7b. v2 新增的门禁项

G-M0 除原有条件外，**必须**增加：

- [ ] `docs/20-testing-strategy.md` §5.1 的 **URL 解析矩阵全绿**（R02 的产出）
- [ ] 字节级透明性测试通过（§5.2）：上游收到的 body 与客户端发出的**逐字节相同**
- [ ] 基准分别报告 **DNS 缓存命中 / 未命中** 两条路径的 TTFT（`docs/21` §8）

第三条是 v2 引入的新风险：Target Policy 的 DNS 解析在关键路径上，
缓存未命中时可能几十毫秒，直接计入 TTFT。**只报平均值会把它藏起来。**

---

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| golden fixture 全套 | F10 |
| contract test（真实 API） | F10 |
| 混沌测试 | F21 |
| 7 天长稳 | F28 |
| 性能回归门禁接入 CI | F28 |
