# F06 · Tee 与最小透传服务

| | |
|---|---|
| ID | F06 |
| 里程碑 | M0 |
| 依赖 | F02, F05 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/06-data-plane.md` §5,§8；`docs/10-streaming.md` §4,§6；`docs/ADR/ADR-009-streaming-tee.md` |
| 宪法条款 | C1, C5, C9, C10, C11, C13, C21, C22, C23 |

## 1. 目标

把 F03–F05 串成一个**能真跑的透传代理**：客户端 → Argus → provider，同时旁路观测。这是 M0 架构假设的实体化。

## 2. 范围

**In scope**：`TeeBody`、`TeeHandle`（双预算）、Observer task、最小 Ingress（硬编码单 provider、无认证、无配置）、hyper 上游客户端（HTTP/2 优先）、取消传播。

**Out of scope**：认证（→ F13）、配置（→ F11）、路由（→ F14）、落库（→ F12）、降级策略（→ F20）。本切片的 Sink 是一个把结果打到日志的 stub。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供 `TeeBody<B>: http_body::Body`，`poll_frame` 原样返回上游 frame。
- **R2** THE SYSTEM SHALL 在 `poll_frame` 中对 data frame 执行 `Bytes::clone` + `try_send`，**不得** `await`（宪法 C5）。
- **R3** THE SYSTEM SHALL NOT 在 provider→client 路径上引入除 TLS 外的任何数据拷贝（宪法 C22）。
- **R4** THE SYSTEM SHALL 实施两级预算：在途字节 `tee_byte_budget`（默认 256 KiB）与队列长度 `tee_channel_capacity`（默认 64）。
- **R5** IF 任一预算触顶 THEN THE SYSTEM SHALL 丢弃该 chunk 并累加 `pending_gap`，主链路继续（宪法 C10）。
- **R6** WHEN 下一次 `try_send` 成功 THE SYSTEM SHALL 通过 `TeeChunk.gap_before` 上报累积的丢弃量（宪法 C13）。
- **R7** IF channel 已关闭（Observer 结束/panic）THEN THE SYSTEM SHALL 摘掉 tee 并继续透传。
- **R8** THE SYSTEM SHALL 在 Observer 侧消费 chunk 后回收字节信用（`in_flight.fetch_sub`）。
- **R9** Observer SHALL 只持有 tee 的接收端，**不得**持有任何 hyper 类型（否则取消传播失效，`docs/06` §8.1）。
- **R10** WHEN 客户端断开 THE SYSTEM SHALL 在一个 RTT 内关闭上游连接，且 Observer SHALL 以 `Outcome::ClientAborted` 收尾并提交 partial 结果。
- **R11** THE SYSTEM SHALL 用 `catch_unwind` 包裹 parser step；IF parser panic THEN 摘掉观测、继续透传、`Outcome::ParserFailed`（宪法 C7）。
- **R12** THE SYSTEM SHALL 在收到上游 response header 后、写出 client header 前检查 status（为 F16 的 S2 重试预留钩子）。
- **R13** THE SYSTEM SHALL 对上游连接器 ALPN 优先协商 h2（宪法 C23）。
- **R14** THE SYSTEM SHALL NOT 为慢客户端分配发送缓冲（宪法 C11）。
- **R15** THE SYSTEM SHALL 全量 buffer 请求体（上限 8 MiB），并把同一个 `Bytes` 的 clone 交给 Observer（零拷贝）。
- **R16** Observer task SHALL 由 `tokio::spawn` 启动，主链路 **不 join** 它。

## 4. 接口契约

```rust
// argus-proxy/src/tee.rs
pin_project! { pub struct TeeBody<B> { #[pin] inner: B, sink: Option<TeeHandle>, stats: TeeStats } }
impl<B: Body<Data = Bytes>> Body for TeeBody<B> { /* 见 docs/06 §5.1 */ }

pub struct TeeHandle { /* tx, byte_budget, in_flight, pending_gap, report */ }
impl TeeHandle {
    pub fn new(cap: usize, byte_budget: u64) -> (Self, TeeReceiver);
    pub fn report(&self) -> Arc<TeeReport>;
    fn offer(&mut self, data: Bytes, stats: &mut TeeStats);   // 见 docs/06 §5.1
    fn close(self, end: TeeEnd, stats: TeeStats);             // 写进 TeeReport，不走 channel
}

/// 旁路侧的接收端。**里面一个 hyper 类型都没有**（R9）
pub struct TeeReceiver { /* rx, in_flight, report */ }
impl TeeReceiver {
    pub async fn recv(&mut self) -> Option<TeeChunk>;         // 顺手归还字节信用（R8）
    pub fn report(&self) -> &TeeReport;
}

/// 结局、转发计数、尾部丢弃。三个 OnceLock，先写者胜（见 §4.1 与 docs/06 §5.1.1）
#[derive(Default)]
pub struct TeeReport { end: OnceLock<TeeEnd>, stats: OnceLock<TeeStats>,
                       trailing_gap: OnceLock<u64> }

#[derive(Copy, Clone, Default)]
pub struct TeeStats {
    pub bytes_forwarded: u64, pub bytes_dropped: u64, pub drop_events: u32, pub frames: u32,
}

// argus-proxy/src/observer/task.rs
pub struct Observer { rx: TeeReceiver, framer: SseFramer, parser: AnyObserver,
                      acc: Accumulator, sink: Arc<dyn InteractionSink>, /* … */ }
impl Observer {
    pub fn spawn(cfg: ObserverConfig, rx: TeeReceiver, meta: RequestMeta,
                 facts: ResponseFacts, sink: Arc<dyn InteractionSink>) -> JoinHandle<()>;
}
```

Observer 内部是**同步调用链**，无 await、无 channel、无动态分发（`docs/10` §7.3）：

```
rx.recv().await → framer.push() → parser.on_frame() → acc.apply()
```

### 4.1 与本节初稿的四处偏离

初稿写在实现之前，下面四处按落地结果回填。

| 初稿 | 实际 | 为什么 |
|---|---|---|
| `new` 返回 `mpsc::Receiver<TeeChunk>` | 返回 `TeeReceiver` newtype | R8 的信用回收挂在 `TeeReceiver::recv` 里面。裸 `Receiver` 意味着每个调用方都得记得 `fetch_sub` 一次；漏一次 `in_flight` 就再也降不下来，这条流剩下的字节全被丢弃，而现象看起来像「旁路太慢」——极难查。放进 `recv` 就没有漏掉的可能 |
| `close(self, end: TeeEnd)`，`TeeEnd` 走 channel | `close(self, end, stats)`，结局走 `TeeReport` | 有界 channel 的 `try_send` **恰好在队列满时失败**，而那正是结局最要紧的时刻。结局走 channel 等于「越需要它越送不到」。改由 `Arc<TeeReport>`（三个 `OnceLock`：`end` / `stats` / `trailing_gap`，先写者胜）带出，见 `docs/06` §5.1.1 |
| `spawn(cfg, rx, meta, sink)` | 多一个 `facts: ResponseFacts` | R9 禁止旁路持有任何 hyper 类型，所以 status 与前两段耗时（`queued_micros` / `upstream_header_micros`）只能由主链路在 spawn 时传进来 |
| Framer 直连 parser | 中间隔一个 `ObserverSink` 适配器 | `SseFramer` 产出 `Frame`，`ProtocolObserver::on_frame` 还要 `at_micros` 与 `EventSink`。适配器在 F05 就落在 `argus-protocol::observer` 里了，本切片直接用 |

`TeeReport::end()` 的先写者胜是有语义的：`Cancellable` 在超时/取消时先写
`Cancelled(reason)`，随后 `TeeBody` 的 `PinnedDrop` 才写 `ClientAborted`。
留下前者是对的 —— 「客户端断开」是取消的**后果**，不是原因。

## 5. 实现要点

- `offer` 的顺序很重要：**先查字节预算，再 `try_send`**。反过来会让超大 chunk 已经进队列才发现超预算。
- `TeeStats` 用 `&mut` 传递而非 `Arc<Atomic>`：它只被 `poll_frame` 单线程访问，无需原子。
- `catch_unwind` 包的是 `framer.push + parser.on_frame + acc.apply` 这一整段（它们是纯数据，`UnwindSafe`）。捕获后设 `parser_dead = true`，之后只 drain channel 不解析。
- 上游客户端用 `hyper_util::client::legacy::Client<HttpsConnector, Full<Bytes>>`，**不要用 `reqwest`**（`docs/06` §13-A）。
- R12 的钩子本切片只需要"拿到 status 后再决定是否写 client header"的**结构**，判定逻辑是 `if status.is_success() { commit() } else { error_out() }`，F16 才填 fallback。
- 本切片的 `InteractionSink` 实现是 `LogSink`（`tracing::info!` 打印摘要），F12 换成 `PgSink`。
- **`poll_frame` 必须问一句 `inner.is_end_stream()`**（实现时踩到的）：只等 `None`
  是不够的 —— `Content-Length` 满足之后 hyper 就不再 poll 了，那个 `None` 永远
  不会来，body 直接被 drop。照初稿写会让**每一个定长响应**都记成
  `ClientAborted`。集成测试没抓到它，因为 `BodyExt::collect()` 会一路 poll 到
  `None`；是手工 curl 冒烟才暴露的。
- **流末尾的丢弃没有车可搭**（实现时踩到的）：R6 的 `gap_before` 是挂在「下一个
  成功发送的 chunk」上的，末尾那段丢弃报不出去。每个 chunk 都超字节预算时一次
  都送不出去，旁路看到一条**空流**并记成 `Complete` —— 正是 C13 禁止的静默有损。
  落地做法是把残留的 `pending_gap` 一并写进 `TeeReport`，收尾时由 Observer 补一个
  `data` 为空、`gap_before > 0` 的 `TeeChunk` **走同一个 framer 入口**，让 resync
  与 `Gap` 事件的产生路径与正常路径完全一致（`docs/10` §6.4）。
- 取消做成 **body 包装器**而不是 Observer 侧的逻辑：`Cancellable<TeeBody<Incoming>>`，
  取消在外、tee 在内。`idle_frame` 与 `total` 两个超时统一表达成
  `CancelReason::Deadline`，与显式取消走同一条收尾路径。
- `catch_unwind` 捕获后那三个对象（framer / parser / acc）**再也不碰**（`dead = true`）：
  panic 之后它们的内部状态是否自洽没有任何保证。panic payload 刻意丢弃（宪法 C27）——
  它可能含有请求内容，而 Rust 默认 hook 已经把它打到 stderr 了。
- router 用 `.fallback()` 而不是 `POST /v1/messages`：路径要按**原始字节**切
  （宪法 C35，`axum::extract::Path` 已被 `clippy.toml` 禁用），所以从
  `uri().path()` 自己读。`/healthz` 与 `/readyz` 是仅有的两条具名路由。

## 6. 任务

- [x] T1 `tee.rs`：`TeeBody` 的 `Body` 实现 + `TeeHandle::offer` 双预算 + `TeeStats` + `TeeReport`
- [x] T2 `observer/task.rs`：Observer task 主循环（同步链 + 信用回收 + 尾部 gap 收尾）
- [x] T3 `observer/panic.rs`：`isolate()` 的 `catch_unwind` 隔离 + `dead` 状态
- [x] T4 `upstream/`：`connect.rs`（`ArgusConnector` = `PinnedConnector` + rustls + ALPN h2 优先）、`headers.rs`（header 变换）、`mod.rs`（`UpstreamClient`）
- [x] T5 `ingress/handler.rs`：Axum router（`.fallback()` + 按原始字节切 path）+ body ingest（8 MiB 上限）
- [x] T6 主链路串联：P1–P8（ingest → 发上游 → `commit_or_fail` 钩子 → 挂 TeeBody → 返回）
- [x] T7 取消传播：`body.rs` 的 `Cancellable<TeeBody<Incoming>>` + `CancelHandle`
- [x] T8 `sink.rs`：`LogSink`（只打计数与标签，不打内容）
- [x] T9 `argus-server`：最小 `main.rs`（`serve` 子命令，env 读 base_url + key）
- [x] T10 集成测试：§7（`tests/passthrough.rs` 16 条 + `tests/mock_provider/`）

## 7. 验收

```bash
cargo test -p argus-proxy        # 182 条（其中 passthrough 16 条为本切片新增的端到端）
ARGUS_ALLOW_LOOPBACK=1 ARGUS_UPSTREAM=http://127.0.0.1:9999 \
  cargo run -p argus-server -- serve                        # 手工冒烟
```

### 7.1 验收清单

- [x] **零拷贝断言**：`poll_frame_hands_both_paths_the_very_same_buffer` —— 转发给 client 的 `Bytes` 与旁路收到的 `Bytes` `as_ptr()` 相等
- [x] **不阻塞断言**：`poll_frame_with_a_consumer_that_sleeps_does_not_slow_the_producer`
- [x] channel 灌满 → `proxy_with_an_overflowing_tee_keeps_forwarding_and_marks_degraded`：`bytes_dropped > 0`、客户端收到的字节与上游**逐字节相同**、interaction 为 `Degraded`
- [x] 客户端中途断开 → `proxy_when_the_client_drops_mid_stream_submits_a_partial_interaction`：mock 侧观察到 `peer_gone()`，interaction 为 `ClientAborted` 且含 partial 文本
- [x] Observer task 在主链路结束后仍能独立完成 → `proxy_does_not_wait_for_the_observer_before_answering`
- [x] `ci/check-invariants.sh` 通过：`tee.rs` 无阻塞 `.send(`（C5 那条 guard 从 skip 转为 **ok**）
- [x] 定长响应收尾 → `poll_frame_on_a_sized_body_records_eof_without_ever_seeing_none`（见 §5，初稿漏了这条）
- [x] 尾部丢弃不静默 → `close_after_dropping_everything_still_reports_the_gap` + `close_after_a_successful_send_leaves_no_trailing_gap`

### 7.2 三条没能验到的

不是漏了，是这一切片**验不了**。逐条写清楚谁来接。

| 初稿要求 | 状态 | 为什么 / 谁接 |
|---|---|---|
| 上游协商到 h2（`resp.version() == HTTP_2`） | **未验到** | 端到端断言需要一个会 ALPN 的 TLS mock，而 mock provider 是本切片手写的明文 HTTP/1.1 服务。现在只到「配置层」为止：`tls_config_offers_h2_ahead_of_http11` + `tls_config_still_offers_http11_as_a_fallback` 钉住 ALPN 顺序，`negotiated_h2_on_a_plain_connection_is_always_false` 钉住明文连接不谎报。**端到端断言 → F07** |
| 慢客户端 RSS 不增长 | **未验到** | RSS 是进程级指标，测进程里跑着整个 test harness，量出来的数没有意义。结构上的保证在：`TeeBody` 不持有任何发送缓冲，背压靠 hyper 自己的 flow control（R14）。**压测下的内存曲线 → F21** |
| 注入 parser panic → `Outcome::ParserFailed` | **部分** | 真正让 `AnyObserver` panic 需要往解析器里塞注入点，那等于为测试改生产代码。改成两头夹：`isolate()` 的四条单元测试（含「panic 后状态半写」与「payload 不外泄」）钉住隔离语义；端到端用**分帧失败**走同一条 `dead = true` 路径 —— `proxy_with_a_failing_framer_still_returns_the_full_response`（200、响应完整、进程存活）。**真注入 → F21 的故障注入** |

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 认证 / 授权 | F13 |
| 配置文件 | F11 |
| 路由与 provider 选择 | F14 |
| 真实落库 | F12 |
| `ObserveMode` 降级、`GatedBody` | F20 / F25 |
| S2 重试的实际判定 | F16 |
| 指标接线 | F15 |
| **h2 端到端断言**（需要会 ALPN 的 TLS mock） | F07（见 §7.2） |
| **慢客户端内存曲线**、**真 parser panic 注入** | F21（见 §7.2） |
| `ProfileRegistry` 的真实实现（本切片是 `StaticRegistry`） | F11 / F22 |
| `TeeReport` 的丢弃统计接进 Prometheus | F15 |
