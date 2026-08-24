# F07 · Mock Provider 与性能基线 ⛔ 门禁

| | |
|---|---|
| ID | F07 |
| 里程碑 | M0（**门禁 G-M0**） |
| 依赖 | F06 |
| 主 crate | `tests/` + `benches/` |
| 设计依据 | `docs/21-performance.md` §5–6；`docs/20-testing-strategy.md` §8,§12；`docs/22-mvp-roadmap.md` §5 风险 1 |
| 宪法条款 | C16, C21, C22, C24 |

## 1. 目标

建立可复现的性能测量基础设施，并**回答 M0 的唯一问题：Tee 架构的开销是否真的可以忽略。**

> 这是全项目唯一的硬门禁。不达标就停下重新设计 `ADR-009`，**不得**继续 M1。
> 依据：`docs/22` §5 风险 1 —— 若这条假设不成立，整个"零成本观测"前提崩塌。

## 2. 范围

**In scope**：`MockProvider`（可编程 scenario）、对照法压测脚本、criterion 微基准、内存/CPU 采集、门禁判定脚本、**基线报告**。

**Out of scope**：真实 provider 的 contract test（→ F10）、长稳 7 天测试（→ F28）。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供本地 `MockProvider`，支持 `docs/20` §8 定义的全部 scenario。
- **R2** `MockProvider` SHALL 支持 `Pacing::Realtime`（按录制时的真实间隔回放），使 TTFT 与 token 间隔有意义。
- **R3** THE SYSTEM SHALL 用**对照法**测量 proxy 开销：`客户端→Mock` 与 `客户端→Argus→Mock` 的差值（`docs/21` §5.3）。
- **R4** THE SYSTEM SHALL 分别在 `observe=Full` 与 `observe=Off` 两种模式下测量，以隔离观测的成本。
- **R5** THE SYSTEM SHALL 在 100 / 1,000 / 10,000 并发流下采集：延迟分布、RSS、CPU、`tee_dropped_bytes`。
- **R6** THE SYSTEM SHALL 提供 criterion 微基准：`SseFramer::push`（× 切分策略）、`AnthropicMessagesParser::on_frame`、`Accumulator::apply`。
- **R7** THE SYSTEM SHALL 断言热路径分配次数：转发一帧主链路 0 次；稳态每 text delta 0 次（宪法 C21）。
- **R8** THE SYSTEM SHALL 产出 `bench/BASELINE.md`，内容标注为**实测**并记录环境（CPU/内存/OS/rustc 版本/日期）。
- **R9** THE SYSTEM SHALL 提供 `ci/gate-m0.sh`，按 §7 判定并以退出码表达通过与否。
- **R10** IF 门禁不通过 THEN 团队 SHALL 停止 M1 工作并记录一份 `bench/M0-FAILURE.md` 说明哪条不达标及初步归因。

## 4. 接口契约

```rust
// argus-proxy/src/testkit/provider.rs（feature = "testkit"）
pub enum Transport {
    /// 手写 HTTP/1.1 字节：分片与时序逐字节可控
    Raw,
    /// TLS + ALPN（h2 与 http/1.1 都提供），用现签的自签证书
    Tls(Box<TestCert>),
}

pub struct MockProvider { /* 临时端口上的监听 */ }

impl MockProvider {
    pub async fn start(scenario: Scenario, transport: Transport) -> Self;
    pub async fn raw(scenario: Scenario) -> Self;   // 明文快捷方式
    pub async fn tls(scenario: Scenario) -> Self;   // TLS 快捷方式
    pub fn base_url(&self) -> String;
    pub fn root_cert(&self) -> Option<CertificateDer<'static>>;
    pub fn expected_body(&self) -> Vec<u8>;         // 逐字节断言的期望值
    pub fn hits(&self) -> u32;
    pub fn peer_gone(&self) -> bool;                // 取消传播的判定
    pub fn last_request(&self) -> RecordedRequest;
    pub fn requests_seen(&self) -> Vec<RecordedRequest>;
}

pub enum Scenario {          // 见 docs/20 §8
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

### 4.1 与本节初稿的六处偏离

| 初稿 | 实际 | 为什么 |
|---|---|---|
| `tests/support/mock_provider.rs` | `argus-proxy/src/testkit/`，`feature = "testkit"`（非默认） | `tests/` 下的模块只有同目录的测试二进制能用，而这套设施有三个调用方：集成测试、`examples/` 里的压测驱动、将来 F21 的混沌用例。放 `tests/` 就得抄三份 |
| 单开一个 testkit crate | 不开 | 宪法 C32 把 crate 集合钉死在五个，新增要满足 `docs/19` §4 四条判据之一。测试工具箱一条都不满足 |
| 一种传输（axum server） | 两种：`Raw`（手写字节）与 `Tls`（真 h2） | 缺一不可，理由见 `docs/20` §8 的表：要验分片就不能让 HTTP 库替我们重新分片；要验 ALPN 就必须上 TLS |
| `set_scenario(key, s)` 运行期改场景 | 场景在 `start()` 时定死 | 没有一个用例需要中途换场景，而可变共享状态会让「这条流用的是哪个场景」变成时序问题 |
| `connection_closed(req_id)` | `peer_gone()` | 请求 id 是 Argus 侧的概念，mock 拿不到。它真正能观察到的是「写响应时对端已经断开」，那也正是取消传播要验的事 |
| `Scenario` 九个变体 | 十个（加 `Scripted`） | 前九个描述「provider 表现出什么行为」，没有一个能表达「这些字节，按这个边界，分这么多次发」—— 而 `docs/20` §5.2 的透明性测试要的恰恰是后者。已同步补进 `docs/20` §8 |

`MockProvider` 吸收了 F06 留在 `crates/argus-proxy/tests/mock_provider/` 的那个
明文 mock，该目录已删除，F06 的 16 条用例全部改跑 testkit。

```bash
# 压测入口
ci/bench-e2e.sh --streams 10000 --observe full  --duration 300s
ci/bench-e2e.sh --streams 10000 --observe off   --duration 300s
ci/gate-m0.sh                                    # 读取上面产出的 JSON 并判定
```

## 5. 实现要点

- `MockProvider` 必须支持 **h2**（否则测不出 `docs/17` §4.2 的连接复用效应，宪法 C23）。
- **它同时是 F06 那条 h2 验收的承接方**：F06 在 `crates/argus-proxy/tests/mock_provider/`
  里留了一个手写的明文 HTTP/1.1 mock，够跑透传与取消，但不会 ALPN，所以
  `resp.version() == HTTP_2` 那条端到端断言在 F06 只验到了 `tls_config` 的
  ALPN 顺序（F06 §7.2）。本切片的 mock 上 TLS 之后，**必须补上那条端到端断言**。
- 对照组（直连 Mock）与实验组必须用**同一个客户端进程、同一份负载脚本**，只改 base_url。否则差值不可信。
- 内存采集读 `/proc/self/status` 的 `VmRSS`，采样间隔 1s，取稳态段的 p50/p99（跳过前 30 秒预热）。
- CPU 用 `/proc/self/stat` 的 utime+stime 差分，除以墙钟与核数。
- 分配次数断言用一个计数分配器（`#[global_allocator]` 包装，仅测试 build 启用）或 `dhat-rs`。
- **`Synthetic` scenario 是压测主力**：不依赖 fixture，可精确控制 token 数与间隔，便于扫参。
- **计数分配器不能自己写**（实现时发现的）：`#[global_allocator]` 要
  `unsafe impl GlobalAlloc`，而宪法 C8 禁 unsafe 且 `ci/check-invariants.sh`
  会拦。用本节已列的备选项 `dhat`：unsafe 关在它自己的 crate 里，我们这边
  一行都不写。代价是 dhat 的全局分配器作用于**整个测试二进制**，而
  `HeapStats` 是全局计数 —— 所以那个文件里只能有一个 `#[test]`，多个并行
  跑会互相污染，症状是只在多核机器上出现的随机失败。
- **`ObserveMode::Off` 原本没被落实**（实现时发现的，算 F06 的漏）：
  `argus_core::ObserveMode` 的文档写的是「Tee 不挂载」，但 F06 的主链路
  无条件挂 tee，`ProfileView` 里也没有 `observe` 字段。照挂不误的话 Off 只是
  「照样 tee、照样解析、最后把结果扔掉」，既省不下 CPU 也省不下内存 ——
  而 R4/G4 要的正是用 Off 把观测成本单独摘出来。已在 P7 落实机制
  （`ObserveMode::Off` → `TeeBody::detached`，不 spawn Observer），
  并把 `observe` 从 Profile 一路接到 `ResolvedTarget`。
  **什么时候**切到 Off 仍是降级策略，归 F20。
- **不用 oha / k6，自己写压测客户端**（`examples/loadgen.rs`）：它们量的是
  「HTTP 请求什么时候完成」，而 R3 要的是 **TTFT**。对一条 SSE 流，前者约
  等于流的总长度，跟代理开销几乎无关 —— 用错指标，压测再漂亮也是白测。
  自己写还顺带避开了 `clippy.toml` 里禁用的 `HttpConnector`。
- **资源采样用 `ps` 而不是 `/proc/self/status`**：后者只有 Linux 有，而这套
  脚本要能在开发机上先跑通再上门禁机器。代价是 CPU 只拿得到累计 `TIME`，
  粒度不足以支撑 G4 的 30% 判定（见 §7.2）。

## 6. 任务

- [x] T1 `src/testkit/`：`MockProvider` + 两种传输 + scenario 分发（吸收 F06 的明文 mock，原目录已删）
- [x] T1b 补 F06 欠的端到端 h2 断言：`tests/h2_upstream.rs` 3 条（首次握手 / 复用连接 / 明文不谎报）
- [x] T2 `Pacing::Realtime` 的回放（读同名 `.timing`，每行一个微秒数）
- [x] T3 全部 10 种 scenario 实现 + `tests/scenarios.rs` 逐个跑通
- [x] T4 `benches/framer.rs` / `accumulator.rs`（`parser` 由 F05 的 `anthropic.rs` 覆盖）
- [x] T5 `tests/forward_alloc.rs`：dhat 计数 + 零分配断言（不是自写分配器，见 §5）
- [x] T6 `ci/bench-e2e.sh`：自写 loadgen + `ps` 资源采集 + JSON 输出 + 一致性闸门
- [x] T7 `ci/gate-m0.sh`：逐条判定，**未判定项与失败同等对待**
- [~] T8 跑基线：100 / 1k × {Full, Off} 已跑；**10k 跑不了**（见 §7.2）
- [x] T9 撰写 `bench/BASELINE.md`（环境、方法、原始数据、以及三条不能单独读的限定）
- [x] T10 把实测数字回填 `docs/21-performance.md` §6.1/§6.2/§6.4，标注「目标 / 实测」并写明非门禁环境（宪法 C16）

## 7. 验收 — 门禁 G-M0

| # | 判据 | 阈值 | 依据 |
|---|------|------|------|
| G1 | `proxy_added_ttft` p50（连接复用，observe=Full） | **< 2 ms** | `docs/22` §3 M0 |
| G2 | `proxy_added_ttft` p99 | **< 15 ms** | 同上 |
| G3 | 10k 并发流 RSS（observe=Full，稳态 p99） | **< 1.5 GB** | 同上 |
| G4 | CPU(observe=Full) / CPU(observe=Off) − 1 | **< 30%** | 同上 |
| G5 | `SplitStrategy` 全矩阵结果一致 | **100%** | 宪法 C24 |
| G6 | 主链路转发一帧的堆分配 | **= 0** | 宪法 C21 |
| G7 | 10k 流下 `tee_dropped_bytes / bytes_forwarded` | **< 1%** | `docs/21` §6.3 |

```bash
./ci/gate-m0.sh && echo "G-M0 PASS → 可以进入 M1" || echo "G-M0 FAIL → 停止，写 M0-FAILURE.md"
```

**不通过时的处置**（写进 `bench/M0-FAILURE.md`）：
1. 哪条不达标、差多少
2. 归因：是 tee 本身、parser CPU、还是 hyper/rustls 的固有开销？
3. 备选方案评估：`ObserveMode` 默认降为低比例采样（机制已在 `docs/10` §6.3 就位，非灾难性重构）

### 7.1 实测结果（非门禁环境）

完整报告见 `bench/BASELINE.md`。环境：8C24G arm64 笔记本，
**mock 与压测客户端同机**，最高 1 000 并发流。门禁要求 4C8G Linux、10k 流。

| # | 判据 | 结果 |
|---|---|---|
| G1 | `+ttft` p50 < 2 ms | 100 并发下 **664 µs**，通过（10k 未测） |
| G2 | `+ttft` p99 < 15 ms | 100 并发下 **2.66 ms**，通过（10k 未测） |
| G3 | 10k RSS < 1.5 GB | 100 并发 46 MB、1 000 并发 232 MB，通过（10k 未测；线性外推 ~1.5 GB **正好压线**） |
| G4 | CPU(Full)/CPU(Off) − 1 < 30% | **未判定** |
| G5 | 切分不变性 100% | **通过** |
| G6 | 转发一帧堆分配 = 0 | **通过**（1 000 帧，0 次；挂 tee 是 0.031 次/帧的 channel 摊销） |
| G7 | 10k 下丢弃率 < 1% | **未判定** |
| 7b | URL 解析矩阵全绿 | **通过** |
| 7b | 字节级透明性 | **通过** |
| 7b | DNS 命中/未命中分别报 | **未判定** |

三条读数据时必须一起看的限定，写在这里而不只是 BASELINE 里，
因为它们决定了上面那些「通过」能被引用到什么程度：

1. **100 并发档的噪声下界是 ±700 µs**：`observe=Off` 量出来的 added TTFT
   （1 340 µs）比 `Full`（664 µs）还大。关掉观测不可能让代理变慢，所以
   这个差值就是噪声。664 µs 只能说明「在 1 ms 量级」。
2. **1 000 并发档整档作废**：对照组自身也慢了 6.3 倍（1 145 → 7 187 µs），
   而对照组里没有 Argus —— 压测夹具先饱和了。
3. **preflight 还不存在**（F24）。`docs/21` §6.1 的推算里 preflight 是
   overhead 的主要构成，所以 `docs/16` OQ-1 **仍未回答**。

### 7.2 为什么没有渲染门禁判定，也没有写 M0-FAILURE.md

`ci/gate-m0.sh` 的结论是**未通过**：`STREAMS=100` 下 7 通过 / 3 未判定，
`STREAMS=10000` 下 4 通过 / 6 未判定。脚本刻意把未判定与失败同等对待 ——
门禁的意义是「证明假设成立」，不是「没有证据表明它不成立」。

10k 并发在这台机器上**测不了**，不是调参能解决的：

| 障碍 | 为什么不能绕 |
|---|---|
| 三个进程同机 | §7.1 限定 2 已经证明 1 000 并发时夹具先饱和，10k 只会更严重。压测客户端必须在另一台机器上 |
| `kern.ipc.somaxconn` = 128 | 10k 的连接建立速率会持续超过 backlog，测出来的是 accept 队列的行为 |
| 门禁条件是 4C8G Linux | 在 8C24G arm64 上「测过了」不能替代 —— 核数、内存、内核 TCP 栈全不一样 |

G4 与 G7 缺的不是机器而是仪表：

- **G4** 需要按秒采样 utime/stime。当前只有 `ps` 的累计 `TIME`，粒度不足以
  支撑 30% 的判定。给一个不可信的比值比不给更糟（宪法 C16）。
- **G7** 需要 `tee_dropped_bytes` 导出成指标（`docs/21` §4.3），那是 F15。
  测试里能读到 `TeeStats`，压测时拿不到。

**`bench/M0-FAILURE.md` 刻意没写。** F07 §7 的那份文件是给「测了、不达标」
用的，要写清楚差多少与归因。现在的状态是「没测」，写成 FAILURE 会让人
以为 tee 架构已被证伪 —— 那是另一种形式的失实。处置见 §8 的移交表。

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
| **在 4C8G Linux 上跑 10k 并发、渲染 G1–G3 的判定** | F07 收尾（需要门禁机器）或 F28 R8 |
| **压测客户端与被测进程分机部署** | 同上 |
| **按秒采样 utime/stime，让 G4 可判** | 同上，采样点加在 `ci/bench-e2e.sh` |
| `tee_dropped_bytes` 等指标导出（G7 的前提） | F15 |
| DNS 命中/未命中分别测（需要 mock 挂主机名） | F07 收尾 |
| `Pacing::Realtime` 的真实录制回放（现在只有机制，没有语料） | F10 R6 |
| `ObserveMode` 何时自动降级（本切片只做了机制） | F20 |
| 微基准的回归门禁（退化 > 20% 失败） | F28 |
