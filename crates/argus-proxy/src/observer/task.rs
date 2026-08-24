//! Observer task：把旁路收到的字节变成一条 `ObservedInteraction`。
//!
//! 内部是一条**同步调用链**，没有 await、没有 channel、没有 `Box<dyn>`
//! （`docs/10-streaming.md` §7.3）：
//!
//! ```text
//! rx.recv().await ─┬→ framer.push() → parser.on_frame() → acc.apply()
//!                  └ 唯一的 await 在这里
//! ```
//!
//! 每 frame 大约 1–3 µs，10k 并发流下约 0.4 核（`docs/10` §7.4）。把这条链
//! 拆成多个 task 或多个 channel，付出的调度开销会超过解析本身。
//!
//! **这个 task 不被 join**（R16）。主链路把它 spawn 出去就不管了 ——
//! 客户端断开时主链路的 future 立刻被 drop，而这条流已经产出的部分对审计和
//! 计费仍然有价值（provider 通常已经为已生成的 token 计了费）。

use std::sync::Arc;

use argus_core::{TeeChunk, TeeEnd};
use argus_protocol::{
    AnyObserver, ObserverSink, ProtocolObserver,
    accumulator::{Accumulator, AccumulatorConfig, RequestMeta},
    framer::{Framer, FramerConfig, SseFramer},
    interaction::{Outcome, StreamStats},
    ir::ObservedEvent,
    observer::EventSink,
    sink::InteractionSink,
};
use tokio::{
    task::JoinHandle,
    time::{Duration, Instant, timeout},
};

use crate::{
    observer::panic::{ParserPanic, isolate},
    tee::TeeReceiver,
};

/// Observer 侧的旋钮。
#[derive(Copy, Clone, Debug)]
pub struct ObserverConfig {
    pub framer: FramerConfig,
    pub accumulator: AccumulatorConfig,
    /// 多久收不到 chunk 就认为主链路已经不在了。
    ///
    /// 这是**兜底**而不是主要收尾路径：正常情况下主链路结束时 tee handle
    /// 被 drop，channel 关闭，旁路立刻收尾。会走到超时的只有主链路把
    /// handle 泄漏了 —— 那是 bug，但不该让一个 task 永远挂着。
    pub idle_timeout: Duration,
}

impl Default for ObserverConfig {
    fn default() -> Self {
        Self {
            framer: FramerConfig::default(),
            accumulator: AccumulatorConfig::default(),
            // `docs/06` §8.3 的 observer.idle_timeout
            idle_timeout: Duration::from_secs(300),
        }
    }
}

/// 主链路才知道的那几样东西。
///
/// Observer 拿不到 HTTP 响应对象（R9：**旁路不得持有任何 hyper 类型**），
/// 所以 status 与前两段耗时必须在 spawn 时传进来。
#[derive(Copy, Clone, Debug)]
pub struct ResponseFacts {
    pub status: u16,
    /// 请求进入 Argus 的时刻。全部耗时都相对它计算
    pub request_started: Instant,
    /// 入口到发出上游请求
    pub queued_micros: u32,
    /// 发出请求到收到上游 response header
    pub upstream_header_micros: u32,
}

/// 旁路的主体。
pub struct Observer {
    rx: TeeReceiver,
    framer: SseFramer,
    parser: AnyObserver,
    acc: Accumulator,
    sink: Arc<dyn InteractionSink>,
    facts: ResponseFacts,
    idle_timeout: Duration,
    /// 最近一个事件的序号，`Outcome` 需要它定位「断在哪」
    last_seq: u32,
    /// 解析链已经不可信（panic 或分帧失败）。之后只 drain，不解析
    dead: bool,
}

impl Observer {
    /// 起一个 Observer task 并**立刻撒手**。
    ///
    /// 返回的 `JoinHandle` 只用于测试与优雅停机的统计，主链路不 join 它
    /// （R16）—— join 就等于主链路在等旁路，那正是宪法 C5 禁止的事。
    pub fn spawn(
        cfg: ObserverConfig,
        rx: TeeReceiver,
        meta: RequestMeta,
        facts: ResponseFacts,
        sink: Arc<dyn InteractionSink>,
    ) -> JoinHandle<()> {
        let observer = Self {
            parser: AnyObserver::for_kind(meta.observer, meta.observe_mode),
            framer: SseFramer::new(cfg.framer),
            acc: Accumulator::new(meta, cfg.accumulator),
            rx,
            sink,
            facts,
            idle_timeout: cfg.idle_timeout,
            last_seq: 0,
            dead: false,
        };
        tokio::spawn(observer.run())
    }

    async fn run(mut self) {
        let outcome = self.consume().await;
        self.submit(outcome);
    }

    /// 主循环。返回这条流的结局。
    async fn consume(&mut self) -> Outcome {
        loop {
            match timeout(self.idle_timeout, self.rx.recv()).await {
                // 主链路把 handle 泄漏了。已经收到的部分照常提交
                Err(_) => return Outcome::ObserverTimeout,
                // channel 关了 —— 主链路结束了，结局写在 report 里
                Ok(None) => return self.wrap_up(),
                Ok(Some(chunk)) => {
                    let at = self.at_micros();
                    self.step(&chunk, at);
                }
            }
        }
    }

    /// 吃一个 chunk。**整段包在 `catch_unwind` 里**（R11）。
    fn step(&mut self, chunk: &TeeChunk, at_micros: u32) {
        if self.dead {
            // 解析链已经不可信。continue 消费只是为了让主链路的 try_send
            // 保持成功 —— 否则它会以为旁路没了并摘掉 tee，那样连 gap 都记不到
            return;
        }
        let Self {
            framer,
            parser,
            acc,
            last_seq,
            ..
        } = self;
        let stepped = isolate(|| {
            let mut events = AccSink { acc, last_seq };
            let mut frames = ObserverSink::new(parser, &mut events, at_micros);
            framer.push(chunk, &mut frames)
        });
        match stepped {
            Ok(Ok(())) => {}
            // 分帧失败：缓冲超限或重新同步失败。这条流的观测到此为止，
            // 但主链路一个字节都不受影响（宪法 C10）
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "分帧失败，放弃这条流的观测");
                self.dead = true;
            }
            // 解析链 panic 了。绝不能再碰那三个对象（见 panic.rs）
            Err(ParserPanic) => {
                tracing::error!(seq = self.last_seq, "解析链 panic，摘掉观测");
                metrics::counter!("argus_observer_panic_total").increment(1);
                self.dead = true;
            }
        }
    }

    /// 上游读完了：补上最后一段丢弃，再冲掉 framer 与 parser 里滞留的帧。
    fn wrap_up(&mut self) -> Outcome {
        if !self.dead {
            let at = self.at_micros();
            // `gap_before` 搭的是"下一个成功发送"的顺风车，最后一段丢弃没车可搭。
            // 每个 chunk 都超预算时更是一次都送不出去 —— 不补这一下，旁路会
            // 看到一条空流并把它记成 Complete（宪法 C13）
            let trailing_gap = self.rx.report().trailing_gap();
            let Self {
                framer,
                parser,
                acc,
                last_seq,
                ..
            } = self;
            let flushed = isolate(|| {
                let mut events = AccSink { acc, last_seq };
                {
                    let mut frames = ObserverSink::new(parser, &mut events, at);
                    if trailing_gap > 0 {
                        // 走和正常路径**完全一样**的入口：framer 据此 resync
                        // 并产出 Gap 事件，Accumulator 据此置 Degraded
                        framer.push(
                            &TeeChunk {
                                data: bytes::Bytes::new(),
                                gap_before: trailing_gap,
                            },
                            &mut frames,
                        )?;
                    }
                    // 真实 provider 的最后一帧未必有尾随空行（`docs/09` 坑 C8）
                    framer.finish(&mut frames)?;
                }
                // 流断了但块还开着的，在这里补上 BlockStop
                parser.finish(at, &mut events);
                Ok::<(), argus_protocol::framer::FramerError>(())
            });
            if matches!(flushed, Err(ParserPanic) | Ok(Err(_))) {
                self.dead = true;
            }
        }

        if self.dead {
            return Outcome::ParserFailed {
                at_seq: self.last_seq,
            };
        }
        match self.rx.report().end() {
            TeeEnd::Eof => Outcome::Completed,
            TeeEnd::ClientAborted => Outcome::ClientAborted {
                at_seq: self.last_seq,
                bytes_forwarded: self.rx.report().stats().bytes_forwarded,
            },
            TeeEnd::UpstreamError(kind) => Outcome::UpstreamError { kind },
            TeeEnd::Cancelled(reason) => Outcome::Cancelled { reason },
        }
    }

    /// 收尾：补上只有主链路知道的那几个字段，然后交给 sink。
    fn submit(self, outcome: Outcome) {
        let Self {
            acc, sink, facts, ..
        } = self;
        let total_micros = micros_since(facts.request_started);
        let mut interaction = acc.finish(outcome);
        interaction.response_meta.status_code = Some(facts.status);
        interaction.timings.queued_micros = facts.queued_micros;
        interaction.timings.upstream_header_micros = facts.upstream_header_micros;
        interaction.timings.total_micros = total_micros;
        // 能确切归到 Argus 头上的只有入口到发出上游请求这一段。剩下的要拆开
        // 「上游在想」和「我们在算」需要主链路侧的埋点，那是 F15 的活
        interaction.timings.proxy_overhead_micros = facts.queued_micros;

        record_stats(&interaction.stats);
        if let Err(full) = sink.try_submit(interaction) {
            // 满了就丢，绝不阻塞（宪法 C5）。降级策略是 F20 的活
            metrics::counter!("argus_sink_dropped_total").increment(1);
            tracing::warn!(outcome = full.0.outcome.as_str(), "sink 已满，丢弃观测结果");
        }
    }

    fn at_micros(&self) -> u32 {
        micros_since(self.facts.request_started)
    }
}

/// 事件的终点：直接调 `Accumulator::apply`，同一个 task 内的普通函数调用。
struct AccSink<'a> {
    acc: &'a mut Accumulator,
    last_seq: &'a mut u32,
}

impl EventSink for AccSink<'_> {
    fn emit(&mut self, ev: ObservedEvent) {
        *self.last_seq = ev.seq;
        self.acc.apply(&ev);
    }
}

fn micros_since(start: Instant) -> u32 {
    u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX)
}

/// 指标只发**计数**，不发内容（宪法 C27）。接线的其余部分是 F15 的活。
fn record_stats(stats: &StreamStats) {
    if stats.dropped_bytes > 0 {
        metrics::counter!("argus_tee_dropped_bytes_total").increment(stats.dropped_bytes);
    }
    if stats.parse_failures > 0 {
        metrics::counter!("argus_parse_failures_total").increment(u64::from(stats.parse_failures));
    }
}
