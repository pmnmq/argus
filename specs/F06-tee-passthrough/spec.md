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
- **R9** Observer SHALL 只持有 `mpsc::Receiver<TeeChunk>`，**不得**持有任何 hyper 类型（否则取消传播失效，`docs/06` §8.1）。
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

pub struct TeeHandle { /* tx, byte_budget, in_flight: Arc<AtomicU64>, pending_gap */ }
impl TeeHandle {
    pub fn new(cap: usize, byte_budget: u64) -> (Self, mpsc::Receiver<TeeChunk>);
    fn offer(&mut self, data: Bytes, stats: &mut TeeStats);   // 见 docs/06 §5.1
    fn close(self, end: TeeEnd);
}

#[derive(Default)]
pub struct TeeStats {
    pub bytes_forwarded: u64, pub bytes_dropped: u64, pub drop_events: u32, pub frames: u32,
}

// argus-proxy/src/observer/mod.rs
pub struct Observer { rx: mpsc::Receiver<TeeChunk>, framer: SseFramer, parser: AnyObserver,
                      acc: Accumulator, sink: Arc<dyn InteractionSink>, /* … */ }
impl Observer {
    pub fn spawn(cfg: ObserverConfig, rx: mpsc::Receiver<TeeChunk>,
                 meta: RequestMeta, sink: Arc<dyn InteractionSink>) -> JoinHandle<()>;
}
```

Observer 内部是**同步调用链**，无 await、无 channel、无动态分发（`docs/10` §7.3）：

```
rx.recv().await → framer.push() → parser.on_frame() → acc.apply()
```

## 5. 实现要点

- `offer` 的顺序很重要：**先查字节预算，再 `try_send`**。反过来会让超大 chunk 已经进队列才发现超预算。
- `TeeStats` 用 `&mut` 传递而非 `Arc<Atomic>`：它只被 `poll_frame` 单线程访问，无需原子。
- `catch_unwind` 包的是 `framer.push + parser.on_frame + acc.apply` 这一整段（它们是纯数据，`UnwindSafe`）。捕获后设 `parser_dead = true`，之后只 drain channel 不解析。
- 上游客户端用 `hyper_util::client::legacy::Client<HttpsConnector, Full<Bytes>>`，**不要用 `reqwest`**（`docs/06` §13-A）。
- R12 的钩子本切片只需要"拿到 status 后再决定是否写 client header"的**结构**，判定逻辑是 `if status.is_success() { commit() } else { error_out() }`，F16 才填 fallback。
- 本切片的 `InteractionSink` 实现是 `LogSink`（`tracing::info!` 打印摘要），F12 换成 `PgSink`。

## 6. 任务

- [ ] T1 `tee.rs`：`TeeBody` 的 `Body` 实现 + `TeeHandle::offer` 双预算 + `TeeStats`
- [ ] T2 `observer/mod.rs`：Observer task 主循环（同步链 + 信用回收）
- [ ] T3 `observer/panic.rs`：`catch_unwind` 隔离 + `parser_dead` 状态
- [ ] T4 `upstream/client.rs`：hyper client + rustls + ALPN h2 优先 + 四段式超时
- [ ] T5 `ingress/mod.rs`：Axum router（仅 `POST /v1/messages`）+ body ingest（8 MiB 上限）
- [ ] T6 主链路串联：ingest → 发上游 → status 检查钩子 → 挂 TeeBody → 返回
- [ ] T7 取消传播：`Cancellable<TeeBody<Incoming>>` + `CancellationToken`
- [ ] T8 `LogSink`（stub 实现 `InteractionSink`）
- [ ] T9 `argus-server`：最小 `main.rs`，硬编码 provider（env 读 base_url + key）
- [ ] T10 集成测试：§7

## 7. 验收

```bash
cargo test -p argus-proxy
ARGUS_UPSTREAM=http://127.0.0.1:9999 cargo run -p argus-server &   # 配合 F07 的 mock
```

- [ ] **零拷贝断言**：转发给 client 的 `Bytes` 与 Observer 收到的 `Bytes` 指向同一底层（`Bytes::as_ptr` 相等）
- [ ] **不阻塞断言**：Observer 侧人为 sleep 5 秒，主链路转发耗时不变（对比无 Observer 的基线）
- [ ] channel 灌满 → `bytes_dropped > 0`，主链路 frame 数不减少，interaction 为 `Degraded`
- [ ] 注入 parser panic → 请求正常完成 200，`Outcome::ParserFailed`，进程存活
- [ ] 客户端中途断开 → mock provider 侧观察到连接关闭；interaction 为 `ClientAborted` 且含 partial 文本
- [ ] Observer task 在主链路结束后仍能独立完成（不被 join）
- [ ] 上游协商到 h2（断言 `resp.version() == HTTP_2`）
- [ ] 慢客户端（读一半就停）→ Argus RSS 不增长，最终被 idle timeout 断开
- [ ] `ci/check-invariants.sh` 通过（`tee.rs` 无 `.send(`）

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
