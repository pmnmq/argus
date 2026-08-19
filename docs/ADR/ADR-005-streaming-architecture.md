# ADR-005 · 流式架构：Tee + 有界旁路

- **状态**：Accepted
- **日期**：2026-08-19
- **相关**：`05-streaming.md`、`02-data-plane.md` §5、`12-reliability.md`

---

## Context

Argus 必须同时做到：

1. **真流式转发**（provider 产出第一个 token 时客户端立刻看到）
2. **完整观测**（拿到全部内容用于审计与安全）
3. **观测不影响转发**（观测慢、观测崩溃、观测的下游挂掉，都不能影响 AI 请求）
4. **内存有界**（10k 并发流下，无论客户端多慢、响应多长）

第 2 与第 3 是直接矛盾的：观测必然消耗资源，而资源是共享的。第 1 与第 4 也有张力：真流式意味着不能缓冲，但不缓冲就无法吸收速度差异。

这是整个项目最核心的技术决策。

---

## Decision

**一份字节，两条路径，非对称保障。**

```text
Provider ─→ hyper client ─→ TeeBody ═══════════════→ hyper server ─→ Client
                                │                        （主链路：保证送达）
                                └ Bytes::clone + try_send ┐
                                                          ↓
                                            bounded mpsc（旁路：保证不阻塞，允许有损）
                                                          ↓
                                     SseFramer → Parser → Accumulator → Sink
```

具体的四条不变式：

| # | 不变式 | 实现 |
|---|--------|------|
| **I1** | 主链路对旁路**只做 `try_send`，永不 `.await`** | `TeeHandle::offer` 无 async |
| **I2** | 转发路径上**零 memcpy** | `Bytes::clone` = refcount++ |
| **I3** | 丢弃必须**可检测** | `gap_before` + `Integrity::Degraded` |
| **I4** | 所有缓冲**有硬上限**，超限时**降级而非报错** | 四个 cap（见下） |

---

## 核心机制

### M1 · TeeBody：Body 层的分流

Tee 实现为 `http_body::Body` 的包装器，而不是 Tower Layer、不是 Stream adapter、不是独立进程。

```rust
fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, E>>> {
    let polled = ready!(this.inner.poll_frame(cx));
    if let Some(Ok(frame)) = &polled {
        if let Some(data) = frame.data_ref() {
            this.sink.offer(data.clone(), this.stats);   // ← 唯一的跨界动作
        }
    }
    Poll::Ready(polled)     // 原样返回给 hyper server
}
```

**为什么是 Body 层**：
- Tower Layer 拿到的是 `Response` 对象，对流式响应而言仍需在 Body 层做分流——Layer 方案最终会退化成同一件事，但多一层抽象。
- `Stream` adapter 会丢失 trailer 支持，且 `Incoming` 转 `Stream` 有额外开销。
- 独立进程需要真实的字节拷贝 + IPC，TTFT 与内存双输。

**开销核算**：一次 `Bytes::clone`（原子加，~5 ns）+ 一次 `try_send`（无竞争时是几次原子操作，~50 ns）≈ **< 100 ns/frame**。相对每 frame 数十微秒的网络与 hyper 开销，是噪声级别。

---

### M2 · 双预算保护

`try_send` 的队列长度限制不足以约束内存——64 个 100 KB 的 chunk 就是 6.4 MB。因此加一层字节预算：

```rust
fn offer(&mut self, data: Bytes, stats: &mut TeeStats) {
    // 预算 1：在途字节
    if self.in_flight.load(Relaxed) + data.len() as u64 > self.byte_budget {
        self.pending_gap += data.len() as u64;
        return;
    }
    // 预算 2：队列长度
    match self.tx.try_send(TeeChunk { data, gap_before: self.pending_gap }) {
        Ok(())  => { self.in_flight.fetch_add(len, Relaxed); self.pending_gap = 0; }
        Err(Full(c))   => { self.pending_gap += c.data.len() as u64; }
        Err(Closed(_)) => { self.closed = true; }    // Observer 没了，摘掉 tee
    }
}
```

Observer 每消费一个 chunk 就 `fetch_sub`，形成信用回收。

---

### M3 · `gap_before`：让有损可检测（I3）

**静默丢弃字节是危险的**。ProtocolParser 会看到一段"看起来连续但实际断裂"的字节流，可能把两个 event 的碎片拼成一个语法合法但语义错误的 event。

**错误的观测数据比没有观测数据更危险**——它会污染审计、误导安全分析、算错成本，而且没人知道。

因此丢弃时累积 `pending_gap`，在下一次成功发送时通过 `TeeChunk.gap_before` 上报：

```text
gap_before > 0
  → SseFramer 丢弃当前缓冲（尾部已不连续）
  → 进入 Resync：扫描到下一个 \n\n 才恢复
  → 产出 ObservedEventKind::Gap { dropped_bytes }
  → Accumulator：所有 open block 置 degraded
  → interaction.integrity = Degraded { .. }   ← 必须落库
```

`Integrity` 字段是**强制落库**的。任何下游分析都必须能区分完整与残缺的记录。

---

### M4 · 降级优先在请求粒度，不在字节粒度 ★

这是本 ADR 最重要的设计决定。

**朴素方案的问题**：逐字节丢弃 30% 的字节，得到的是一个残缺且不可信的 interaction——文本有洞、tool arguments 是坏 JSON、usage 可能丢失。它**占用了存储，却几乎没有分析价值**。

**更好的方案**：在请求开始时就决定观测与否，让被观测的流保持完整。

```rust
pub enum ObserveMode {
    Full,   // raw request + 全部 event + 完整 accumulate
    Lite,   // 只累积结构性信息（usage / tool 元数据 / finish），不留正文与 raw
    Off,    // 不挂 Tee，只记请求级元数据
}

fn decide(ctx: &RequestCtx, load: &ObserverLoad) -> ObserveMode {
    if ctx.policy.force_full_observation { return Full; }     // 安全策略优先
    match load.pressure() {
        Normal   => Full,                          // < 60%
        Elevated => sample(ctx, load.rate),        // 60–85%：确定性抽样
        High     => Lite,                          // 85–95%
        Critical => Off,                           // > 95%
    }
}
```

**抽样是确定性的**（`hash(trace_id) % 10000 < rate`），不是随机的：同一 conversation 的多轮请求要么都被观测要么都不被，便于会话级分析；且结果可复现。

三层保护的关系：

```text
第 1 层  ObserveMode 准入降级      ← 正常运行时唯一生效的
第 2 层  byte_budget               ← 防单流的超大 chunk
第 3 层  try_send 失败 → gap       ← 最后防线，正常不应触发
```

`argus_tee_dropped_bytes_total` 持续非零意味着容量规划有问题，应该告警。

---

### M5 · Slow client：不缓冲，让背压传播

```text
client 不读 → TCP 窗口关闭 → hyper server 写 pending
  → TeeBody::poll_frame 不被调用 → hyper client 不从 provider 读
  → provider 侧窗口关闭（HTTP/2 下是 stream 级流控）→ provider 停止发送
```

**没有任何一处出现无界缓冲。** 慢客户端只是让自己的流变慢，不消耗 Argus 的额外内存。

**明确拒绝**：为慢客户端加发送缓冲区。这会把客户端的问题转化成 Argus 的内存问题——一个恶意客户端只要连接后不读，就能让 Argus 缓冲整个响应；10k 个这样的连接就是 OOM。

配套保护：`client_write_idle_timeout = 60s`、`max_stream_duration = 900s`。

---

### M6 · 四个硬上限

| 上限 | 默认 | 超限行为 |
|------|------|---------|
| `tee_channel_capacity` | 64 条 | 丢弃 + gap |
| `tee_byte_budget` | 256 KiB | 丢弃 + gap |
| `max_frame_bytes` | 1 MiB | **停止该流的观测**，继续透传 |
| `max_accumulated_bytes` | 1 MiB | **truncate**，继续消费事件 |

注意后两者的行为：**降级而非报错**。一个 10 MB 的响应不应该让观测崩溃，更不应该让代理失败。

---

### M7 · 零分配的解析

两个手段：

1. **借用式部分反序列化**（`serde(borrow)`）：字符串不含转义时直接借用原 buffer，零分配。
2. **Accumulator 的 amortized append**：`String::push_str`，整条流只发生 3–5 次 realloc。

```rust
#[inline]
fn push(&mut self, delta: &str) {
    if self.text.len() + delta.len() > self.byte_cap { self.truncated = true; return; }
    self.text.push_str(delta);      // 摊销 O(1)，稳态零分配
}
```

**每个 token 的分配次数：0（摊销）。**

对比：如果用 `serde_json::Value`，每个 chunk 产生 10–20 次分配；10k 流 × 20 frame/s × 15 = 300 万次分配/秒，会主导 CPU 开销。

---

## Options Considered

**A. 主链路内联解析（先解析再转发）。**
最简单。但 parser 变慢直接拖慢转发，parser panic 直接杀请求。违背核心目标。**强烈否决。**

**B. 缓冲整个响应再转发。**
TTFT 变成完整响应时间（可能 60 秒），内存无界。这是很多简易网关的做法。**强烈否决。**

**C. Observer 持有上游 body，主链路从 Observer 取数据。**
方向完全反了——Observer 的故障会直接阻塞代理。**强烈否决。**

**D. 观测放到独立进程（Unix socket）。**
观测崩溃完全不影响代理。但字节必须真实拷贝并跨进程序列化，TTFT 与内存双输，部署复杂度翻倍。**否决**——用 task 隔离 + `panic=unwind` 已能获得大部分收益（`01-architecture.md` §8–9）。

**E. 无界 channel（`unbounded_channel`）。**
`try_send` 永不失败，观测永不丢数据。但 observer 慢时内存无界增长 → OOM。**强烈否决**，CI 有 grep 检查禁止 `unbounded`。

**F. `broadcast` channel 支持多消费者。**
语义上接近（滞后订阅者会被丢弃）。但每条消息要 `Clone`（对 `Bytes` 廉价），且滞后检测按消息数而非字节数。MVP 只有一个消费者。**推迟**，需要多消费者时再改。

**G. `spawn_blocking` 跑解析。**
解析是 µs 级纯 CPU，扔进 blocking 池的调度开销大于收益。**否决**（但 LLM detector 与压缩确实应该走 blocking 池）。

**H. 用 `Chain<Bytes, Bytes>` 避免跨 chunk 拼接的拷贝。**
理论上可行，但 `serde_json` 需要连续切片，除非用流式 JSON 解析器（复杂度高得多）。残留部分通常 < 1 KB，拷贝量很小。**否决。**

---

## Consequences

### 正面

- 主链路的额外开销 < 100 ns/frame，可以认为是零。
- 观测的任何故障（慢、崩溃、下游挂）都不影响转发——这是可以证明的（主链路上没有对观测的 `.await`）。
- 内存在任何输入下都有上界，可以逐项核算（`12-reliability.md` §4）。
- 有损是**可检测且可量化**的，不产生污染数据。
- 降级策略保证了"被观测的数据是完整可信的"。

### 负面

| 负面 | 缓解 / 接受理由 |
|------|---------------|
| 高压下部分请求完全没有内容观测 | 优于"所有请求都有残缺观测"；且有指标与告警 |
| Observer 与主链路共享 CPU（task 级隔离不隔离 CPU） | 让解析足够便宜（§M7）；昂贵操作下沉到 sink/blocking 池；可选 `runtime.split` |
| 慢客户端拖慢自己的流 | 这是正确行为，不是缺陷 |
| `gap_before` 增加 Tee 的状态复杂度 | 值得——静默丢弃是不可接受的 |
| 借用式反序列化的生命周期处理复杂 | 边界明确：借用不跨 await，进 Accumulator 立刻拷贝 |
| Accumulator 超 cap 后正文被截断 | 有 `truncated` 标记；1 MB 覆盖绝大多数场景 |

### 关键的验证要求

本 ADR 的全部性能论断**都是估算**。`16-mvp-roadmap.md` 的 M0 里程碑存在的唯一理由就是验证它们：

- `proxy_added_ttft` p50 < 2 ms，p99 < 15 ms
- 10k 并发流 RSS < 1.5 GB
- 观测开启 vs 关闭的 CPU 差异 < 30%
- 逐字节切分与整块切分产出相同结果

**若 M0 不达标，本 ADR 需要重新审视**，后备方案是把观测默认改为低比例采样（`ObserveMode` 的机制已经就位，所以不是灾难性的重构）。

---

## 相关的强制约束

1. **Observer 只持有 `mpsc::Receiver<TeeChunk>`，绝不持有任何 hyper 类型。** 否则上游连接不会因客户端断开而关闭，取消传播失效。
2. **CI 禁止 `unbounded_channel`**（`15-testing-strategy.md` §9）。
3. **CI 禁止 `tee.rs` 中出现 `.send(`**（只允许 `try_send`）。
4. **`panic = "unwind"`**，否则 parser 的一个 bug 会杀死上千条连接。
