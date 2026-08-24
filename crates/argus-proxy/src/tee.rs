//! Tee：把响应字节分流给观测旁路，**主链路一步都不为它停**。
//!
//! 整个设计压在四条不变式上（ADR-009）：
//!
//! | # | 不变式 | 这里怎么保证 |
//! |---|---|---|
//! | I1 | 主链路对旁路只 `try_send`，永不 `.await` | [`TeeHandle::offer`] 不是 async；`ci/check-invariants.sh` 直接 grep 本文件里的 `.send(` |
//! | I2 | 转发路径零 memcpy | 交给旁路的是 `Bytes::clone`，即 refcount++（宪法 C22） |
//! | I3 | 丢弃必须可检测 | 累积 `pending_gap`，下一次成功发送时挂在 `gap_before` 上（宪法 C13） |
//! | I4 | 缓冲有硬上限，超限降级而非报错 | 队列长度 + 在途字节双预算（宪法 C9/C10） |
//!
//! **为什么是 `Body` 层而不是 Tower Layer**：Layer 拿到的是 `Response` 对象，
//! 对流式响应仍然要在 Body 层分流 —— 最终会退化成同一件事，只是多一层抽象
//! （ADR-009 M1）。

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use argus_core::{CancelReason, TeeChunk, TeeEnd};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use tokio::sync::mpsc::{self, error::TrySendError};

use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll, ready},
};

/// 旁路队列的默认长度（chunk 个数）。
pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

/// 旁路在途字节的默认预算。
///
/// 光有队列长度不够：64 个 100 KB 的 chunk 就是 6.4 MB，而 10k 并发流下
/// 这个数字要乘以一万（ADR-009 M2）。
pub const DEFAULT_BYTE_BUDGET: u64 = 256 * 1024;

/// 一条流在主链路侧的计数。
///
/// 用 `Copy` + `&mut` 传递而不是 `Arc<Atomic>`：它只被 `poll_frame` 访问，
/// 而同一个 body 不会被两个 task 同时 poll。多付一组原子操作换不来任何东西。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct TeeStats {
    /// 转发给客户端的字节数。**主链路的真实产出**，与旁路收到多少无关
    pub bytes_forwarded: u64,
    /// 因为预算触顶没能交给旁路的字节数
    pub bytes_dropped: u64,
    /// 发生丢弃的次数（连续丢弃算多次）
    pub drop_events: u32,
    /// 转发的 data frame 数
    pub frames: u32,
}

impl TeeStats {
    /// 旁路有没有错过东西。对应 `Integrity::Degraded`。
    #[must_use]
    pub const fn is_lossless(&self) -> bool {
        self.bytes_dropped == 0
    }
}

/// 一条流结束时留给旁路的两样东西：**为什么结束**与**主链路转发了多少**。
///
/// 为什么不走 channel：`TeeEnd` 走 channel 就意味着 `close` 时要 `try_send`，
/// 而队列正满的时候那一发会失败 —— 恰恰是最需要知道结局的时候把结局弄丢了。
/// 两个 `OnceLock` 没有这个失败模式。
///
/// 两个格子而不是一个：取消发生在 [`Cancellable`] 那一层，它只知道原因；
/// 字节数只有 [`TeeBody`] 知道。各写各的，谁先写完谁的都不用等对方。
#[derive(Debug, Default)]
pub struct TeeReport {
    end: OnceLock<TeeEnd>,
    stats: OnceLock<TeeStats>,
    trailing_gap: OnceLock<u64>,
}

impl TeeReport {
    /// 结束原因。旁路必须在 channel 关闭**之后**读 —— 那时写入一定已经发生。
    ///
    /// 读不到时按 [`TeeEnd::ClientAborted`] 算：能走到「channel 关了但没人
    /// 写结局」的只有 handle 被非正常路径丢掉，那本质上就是流没跑完。
    #[must_use]
    pub fn end(&self) -> TeeEnd {
        self.end.get().cloned().unwrap_or(TeeEnd::ClientAborted)
    }

    #[must_use]
    pub fn stats(&self) -> TeeStats {
        self.stats.get().copied().unwrap_or_default()
    }

    /// 流结束时还没来得及上报的丢弃量。
    ///
    /// `gap_before` 是**搭下一个成功发送的顺风车**上报的，所以最后一段丢弃
    /// 没有车可搭。极端情况下（每一个 chunk 都超预算）一次都送不出去，
    /// 旁路会看到一条空流并以为它是完整的 —— 那正是宪法 C13 说的
    /// 「悄无声息的有损」。旁路必须在收尾时把这个数补上。
    #[must_use]
    pub fn trailing_gap(&self) -> u64 {
        self.trailing_gap.get().copied().unwrap_or_default()
    }

    /// 抢先记下取消原因。晚到的 `ClientAborted` 会被 `OnceLock` 挡掉 ——
    /// 「客户端断了」是取消的**后果**，不是原因，覆盖掉会丢掉真相。
    pub fn note_cancelled(&self, reason: CancelReason) {
        let _ = self.end.set(TeeEnd::Cancelled(reason));
    }

    fn finish(&self, end: TeeEnd, stats: TeeStats, trailing_gap: u64) {
        let _ = self.end.set(end);
        let _ = self.stats.set(stats);
        let _ = self.trailing_gap.set(trailing_gap);
    }
}

/// 主链路侧的发送端。
pub struct TeeHandle {
    tx: mpsc::Sender<TeeChunk>,
    byte_budget: u64,
    in_flight: Arc<AtomicU64>,
    /// 从上一次成功发送以来丢掉的字节数
    pending_gap: u64,
    report: Arc<TeeReport>,
    /// 旁路没了。之后连 `try_send` 都省掉
    closed: bool,
}

impl fmt::Debug for TeeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeeHandle")
            .field("byte_budget", &self.byte_budget)
            .field("in_flight", &self.in_flight.load(Ordering::Relaxed))
            .field("pending_gap", &self.pending_gap)
            .field("closed", &self.closed)
            // tx 与 report 不进 Debug：一个没有有用的表示，另一个会把
            // 结束原因也印出来，而这个 Debug 的用途是排查预算
            .finish_non_exhaustive()
    }
}

impl TeeHandle {
    /// 建一对收发端。`capacity` 是队列长度，`byte_budget` 是在途字节上限。
    #[must_use]
    pub fn new(capacity: usize, byte_budget: u64) -> (Self, TeeReceiver) {
        // 容量 0 的 mpsc 会 panic，而"不观测"应该走 ObserveMode::Off 而不是
        // 建一个零容量的通道
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let in_flight = Arc::new(AtomicU64::new(0));
        let report = Arc::new(TeeReport::default());
        let handle = Self {
            tx,
            byte_budget,
            in_flight: Arc::clone(&in_flight),
            pending_gap: 0,
            report: Arc::clone(&report),
            closed: false,
        };
        (
            handle,
            TeeReceiver {
                rx,
                in_flight,
                report,
            },
        )
    }

    #[must_use]
    pub fn report(&self) -> Arc<TeeReport> {
        Arc::clone(&self.report)
    }

    /// 把一段字节的引用计数副本交给旁路。**不阻塞、不 await、可失败**。
    ///
    /// 顺序是刻意的：**先查字节预算，再 `try_send`**。反过来的话，一个
    /// 超大 chunk 会先挤进队列，等发现超预算时它已经在旁路手里了 ——
    /// 预算就白设了（F06 §5）。
    fn offer(&mut self, data: Bytes, stats: &mut TeeStats) {
        if self.closed {
            return;
        }
        let len = data.len() as u64;

        // 预算 1：在途字节。防的是"少量超大 chunk 撑爆内存"
        if self.in_flight.load(Ordering::Relaxed).saturating_add(len) > self.byte_budget {
            self.drop_chunk(len, stats);
            return;
        }

        let chunk = TeeChunk {
            data,
            gap_before: self.pending_gap,
        };

        // 预算 2：队列长度。防的是"旁路卡死"
        match self.tx.try_send(chunk) {
            Ok(()) => {
                self.in_flight.fetch_add(len, Ordering::Relaxed);
                self.pending_gap = 0;
            }
            Err(TrySendError::Full(c)) => self.drop_chunk(c.data.len() as u64, stats),
            // 旁路已经结束（panic / 提前收尾）。彻底摘掉 tee，主链路照常转发
            Err(TrySendError::Closed(_)) => self.closed = true,
        }
    }

    fn drop_chunk(&mut self, len: u64, stats: &mut TeeStats) {
        self.pending_gap = self.pending_gap.saturating_add(len);
        stats.bytes_dropped = stats.bytes_dropped.saturating_add(len);
        stats.drop_events = stats.drop_events.saturating_add(1);
    }

    /// 收尾：先写结局，再放掉发送端。
    ///
    /// 顺序不能反 —— 旁路是靠 channel 关闭知道流结束的，先放 tx 的话它可能
    /// 在结局写进去之前就去读了。
    fn close(self, end: TeeEnd, stats: TeeStats) {
        self.report.finish(end, stats, self.pending_gap);
    }
}

/// 旁路侧的接收端。
///
/// **这里面一个 hyper 类型都没有**，这是硬性规则（`docs/06` §8.1）：
/// Observer 一旦持有上游 body 的所有权，客户端断开时上游连接就不会被关闭，
/// provider 会继续生成、继续计费。
pub struct TeeReceiver {
    rx: mpsc::Receiver<TeeChunk>,
    in_flight: Arc<AtomicU64>,
    report: Arc<TeeReport>,
}

impl TeeReceiver {
    /// 取下一段字节，**顺手归还字节信用**。
    ///
    /// 信用回收放在这里而不是让调用方自己 `fetch_sub`：漏掉一次，`in_flight`
    /// 就再也降不下来，这条流剩下的字节会被全部丢弃 —— 而且看起来像是
    /// "旁路太慢"，极难查。放进 `recv` 就没有漏掉的可能。
    pub async fn recv(&mut self) -> Option<TeeChunk> {
        let chunk = self.rx.recv().await?;
        self.in_flight.fetch_sub(chunk.data.len() as u64, Ordering::Relaxed);
        Some(chunk)
    }

    /// 结束原因与主链路的转发计数。**只在 [`Self::recv`] 返回 `None` 之后读**。
    #[must_use]
    pub fn report(&self) -> &TeeReport {
        &self.report
    }
}

pin_project! {
    /// `Body` 的包装器：原样返回上游 frame，顺手把 data 的引用计数副本
    /// 丢给旁路。
    ///
    /// 类型上带 `B: Body` 的约束是为了让 `PinnedDrop` 能问一句
    /// 「body 自己读完了没有」—— 见下面 drop 里的说明。
    pub struct TeeBody<B>
    where
        B: Body<Data = Bytes>,
    {
        #[pin]
        inner: B,
        // `None` = tee 已摘掉（正常收尾或旁路没了）
        sink: Option<TeeHandle>,
        stats: TeeStats,
    }

    impl<B> PinnedDrop for TeeBody<B>
    where
        B: Body<Data = Bytes>,
    {
        /// 还没收尾就被丢掉了。**先问 body 自己读没读完**再下结论。
        ///
        /// hyper 在 `Content-Length` 已经满足时会直接停止 poll，不会再来
        /// 取那个 `None` —— 光看「有没有见过 None」会把每一个定长响应都
        /// 记成 `ClientAborted`，而那是审计里最误导人的一种错。
        ///
        /// 取消的情况下 [`Cancellable`] 已经把原因写进 report 了，这里的
        /// 结论会被 `OnceLock` 挡掉。但 `stats` 只有这里知道，所以无论
        /// 如何都要走一趟。
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(sink) = this.sink.take() {
                let end = if this.inner.is_end_stream() {
                    TeeEnd::Eof
                } else {
                    TeeEnd::ClientAborted
                };
                sink.close(end, *this.stats);
            }
        }
    }
}

impl<B> TeeBody<B>
where
    B: Body<Data = Bytes>,
{
    /// 挂上 tee。
    #[must_use]
    pub fn new(inner: B, sink: TeeHandle) -> Self {
        Self {
            inner,
            sink: Some(sink),
            stats: TeeStats::default(),
        }
    }

    /// 不观测：`ObserveMode::Off` 走这条，连 `Bytes::clone` 都不做。
    #[must_use]
    pub fn detached(inner: B) -> Self {
        Self {
            inner,
            sink: None,
            stats: TeeStats::default(),
        }
    }

    #[must_use]
    pub const fn stats(&self) -> TeeStats {
        self.stats
    }
}

impl<B> Body for TeeBody<B>
where
    B: Body<Data = Bytes>,
    B::Error: fmt::Display,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let mut this = self.project();
        let polled = ready!(this.inner.as_mut().poll_frame(cx));

        match &polled {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    this.stats.bytes_forwarded =
                        this.stats.bytes_forwarded.saturating_add(data.len() as u64);
                    this.stats.frames = this.stats.frames.saturating_add(1);
                    if let Some(sink) = this.sink.as_mut() {
                        // 唯一的跨界动作：非阻塞、可失败、失败即丢
                        sink.offer(data.clone(), this.stats);
                    }
                }
                // 定长响应在最后一个 frame 之后就 `is_end_stream()` 了，
                // hyper 不会再来取 `None`。等那个 `None` 的话，旁路要拖到
                // body 被 drop 才知道流结束，而且会把它记成客户端断开
                if this.inner.is_end_stream()
                    && let Some(sink) = this.sink.take()
                {
                    sink.close(TeeEnd::Eof, *this.stats);
                }
            }
            Some(Err(e)) => {
                // 错误**描述**进审计，响应内容不进（宪法 C27）
                if let Some(sink) = this.sink.take() {
                    sink.close(TeeEnd::UpstreamError(e.to_string().into()), *this.stats);
                }
            }
            None => {
                if let Some(sink) = this.sink.take() {
                    sink.close(TeeEnd::Eof, *this.stats);
                }
            }
        }
        // 原样交给 hyper server —— 主链路上没有第二次拷贝（宪法 C22）
        Poll::Ready(polled)
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use std::collections::VecDeque;

    use http_body_util::BodyExt;

    use super::*;

    /// 一个按预设顺序吐 frame 的假 body。
    struct Canned {
        frames: VecDeque<Result<Bytes, CannedError>>,
        /// 模拟 `Content-Length` 已满足：最后一个 frame 之后就 end-of-stream，
        /// hyper 不会再来取那个 `None`
        knows_its_length: bool,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("canned upstream failure")]
    struct CannedError;

    impl Canned {
        fn ok(chunks: &[&'static [u8]]) -> Self {
            Self {
                frames: chunks.iter().map(|c| Ok(Bytes::from_static(c))).collect(),
                knows_its_length: false,
            }
        }

        /// 定长响应：最后一个 frame 之后 `is_end_stream()` 就为真。
        fn sized(chunks: &[&'static [u8]]) -> Self {
            Self {
                knows_its_length: true,
                ..Self::ok(chunks)
            }
        }

        fn then_error(chunks: &[&'static [u8]]) -> Self {
            let mut me = Self::ok(chunks);
            me.frames.push_back(Err(CannedError));
            me
        }
    }

    impl Body for Canned {
        type Data = Bytes;
        type Error = CannedError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, CannedError>>> {
            Poll::Ready(self.frames.pop_front().map(|r| r.map(Frame::data)))
        }

        fn is_end_stream(&self) -> bool {
            self.knows_its_length && self.frames.is_empty()
        }
    }

    fn handle(capacity: usize, budget: u64) -> (TeeHandle, TeeReceiver) {
        TeeHandle::new(capacity, budget)
    }

    // ── offer：双预算 ────────────────────────────────────────────────

    #[tokio::test]
    async fn offer_a_chunk_within_both_budgets_reaches_the_receiver() {
        let (mut tee, mut rx) = handle(4, 1024);
        let mut stats = TeeStats::default();
        tee.offer(Bytes::from_static(b"hello"), &mut stats);

        let got = rx.recv().await.expect("应该收到");
        assert_eq!(got.data, Bytes::from_static(b"hello"));
        assert_eq!(got.gap_before, 0);
        assert_eq!(stats.bytes_dropped, 0);
    }

    /// 预算 1：少量超大 chunk 不能把内存撑爆（ADR-009 M2）。
    #[tokio::test]
    async fn offer_a_chunk_over_the_byte_budget_is_dropped_and_counted() {
        let (mut tee, mut rx) = handle(64, 8);
        let mut stats = TeeStats::default();
        tee.offer(Bytes::from_static(b"0123456789"), &mut stats);

        assert_eq!(stats.bytes_dropped, 10);
        assert_eq!(stats.drop_events, 1);
        drop(tee);
        assert!(rx.recv().await.is_none(), "超预算的 chunk 不该进队列");
    }

    /// 顺序是刻意的：**先查字节预算，再 `try_send`**。
    ///
    /// 反过来的话，超大 chunk 会先挤进队列、等发现超预算时它已经在旁路
    /// 手里了 —— 判据就是队列此刻必须是空的。
    #[tokio::test]
    async fn offer_checks_the_byte_budget_before_touching_the_queue() {
        let (mut tee, rx) = handle(64, 4);
        let mut stats = TeeStats::default();
        tee.offer(Bytes::from_static(b"way too long"), &mut stats);
        assert_eq!(rx.rx.len(), 0, "超预算的 chunk 已经进了队列");
    }

    /// 预算 2：队列长度。
    #[tokio::test]
    async fn offer_when_the_queue_is_full_drops_and_keeps_going() {
        let (mut tee, mut rx) = handle(2, u64::MAX);
        let mut stats = TeeStats::default();
        for _ in 0..5 {
            tee.offer(Bytes::from_static(b"abc"), &mut stats);
        }
        assert_eq!(stats.bytes_dropped, 9, "5 个 3 字节，只有 2 个进得去");
        assert_eq!(stats.drop_events, 3);
        assert!(rx.recv().await.is_some());
    }

    /// 宪法 C13：有损是允许的，**悄无声息的有损不允许**。
    #[tokio::test]
    async fn offer_after_a_drop_reports_the_gap_on_the_next_success() {
        let (mut tee, mut rx) = handle(1, u64::MAX);
        let mut stats = TeeStats::default();

        tee.offer(Bytes::from_static(b"first"), &mut stats); // 进队列
        tee.offer(Bytes::from_static(b"lost1"), &mut stats); // 队列满，丢
        tee.offer(Bytes::from_static(b"lost2"), &mut stats); // 继续丢

        let first = rx.recv().await.expect("有第一条");
        assert_eq!(first.gap_before, 0);

        // 腾出位置之后，累积的丢弃量挂在下一条上
        tee.offer(Bytes::from_static(b"third"), &mut stats);
        let third = rx.recv().await.expect("有第三条");
        assert_eq!(third.data, Bytes::from_static(b"third"));
        assert_eq!(third.gap_before, 10, "两条 5 字节的丢弃必须被上报");
        assert!(third.has_gap());
    }

    /// R7：旁路没了就摘掉 tee，主链路继续。
    #[tokio::test]
    async fn offer_after_the_receiver_is_gone_detaches_the_tee() {
        let (mut tee, rx) = handle(4, u64::MAX);
        drop(rx);
        let mut stats = TeeStats::default();
        tee.offer(Bytes::from_static(b"x"), &mut stats);
        assert!(tee.closed, "旁路关了之后应该彻底摘掉");
        // 摘掉之后不再累积 gap —— 没人会来读它了
        tee.offer(Bytes::from_static(b"y"), &mut stats);
        assert_eq!(tee.pending_gap, 0);
    }

    /// R8：信用回收放在 `recv` 里，漏不掉。
    #[tokio::test]
    async fn recv_a_chunk_returns_the_byte_credit_to_the_budget() {
        let (mut tee, mut rx) = handle(4, 8);
        let mut stats = TeeStats::default();

        tee.offer(Bytes::from_static(b"12345678"), &mut stats);
        assert_eq!(tee.in_flight.load(Ordering::Relaxed), 8, "预算用满");

        // 没回收之前，下一个 chunk 一定被丢
        tee.offer(Bytes::from_static(b"9"), &mut stats);
        assert_eq!(stats.bytes_dropped, 1);

        rx.recv().await.expect("有一条");
        assert_eq!(tee.in_flight.load(Ordering::Relaxed), 0, "信用没还回来");

        tee.offer(Bytes::from_static(b"9"), &mut stats);
        assert_eq!(stats.bytes_dropped, 1, "还回信用之后就不该再丢了");
    }

    // ── TeeBody ─────────────────────────────────────────────────────

    /// 宪法 C22：主链路与旁路拿到的是**同一块内存**，不是两份拷贝。
    #[tokio::test]
    async fn poll_frame_hands_both_paths_the_very_same_buffer() {
        let (tee, mut rx) = handle(8, u64::MAX);
        let mut body = TeeBody::new(Canned::ok(&[b"event: ping\n\n"]), tee);

        let frame = body.frame().await.expect("有一帧").expect("不是错误");
        let forwarded = frame.into_data().expect("是 data frame");
        let observed = rx.recv().await.expect("旁路也收到了");

        assert_eq!(
            forwarded.as_ptr(),
            observed.data.as_ptr(),
            "两条路径拿到了不同的内存，说明发生了拷贝"
        );
        assert_eq!(forwarded, observed.data);
    }

    #[tokio::test]
    async fn poll_frame_to_eof_records_the_end_and_the_forwarded_bytes() {
        let (tee, rx) = handle(8, u64::MAX);
        let body = TeeBody::new(Canned::ok(&[b"aaa", b"bb"]), tee);
        let collected = body.collect().await.expect("读完").to_bytes();
        assert_eq!(collected, Bytes::from_static(b"aaabb"));

        let report = rx.report();
        assert_eq!(report.end(), TeeEnd::Eof);
        assert_eq!(report.stats().bytes_forwarded, 5);
        assert_eq!(report.stats().frames, 2);
        assert!(report.stats().is_lossless());
    }

    #[tokio::test]
    async fn poll_frame_hitting_an_upstream_error_records_it_without_the_payload() {
        let (tee, rx) = handle(8, u64::MAX);
        let mut body = TeeBody::new(Canned::then_error(&[b"partial"]), tee);
        let _ = body.frame().await;
        let err = body.frame().await.expect("有第二帧");
        assert!(err.is_err());

        match rx.report().end() {
            TeeEnd::UpstreamError(kind) => assert_eq!(kind, "canned upstream failure"),
            other => panic!("应该是 upstream_error，实际 {}", other.as_str()),
        }
        assert_eq!(rx.report().stats().bytes_forwarded, 7);
    }

    /// 客户端断开：body 没读完就被丢掉。已转发的部分仍然有观测价值。
    #[tokio::test]
    async fn drop_a_half_read_body_records_client_aborted_with_partial_stats() {
        let (tee, rx) = handle(8, u64::MAX);
        let mut body = TeeBody::new(Canned::ok(&[b"first", b"second"]), tee);
        let _ = body.frame().await;
        drop(body);

        assert_eq!(rx.report().end(), TeeEnd::ClientAborted);
        assert_eq!(rx.report().stats().bytes_forwarded, 5, "已转发的部分要留下");
    }

    /// 取消的原因先写先赢：「客户端断了」是取消的**后果**，不是原因。
    #[test]
    fn note_cancelled_before_the_body_drops_wins_over_client_aborted() {
        let report = TeeReport::default();
        report.note_cancelled(CancelReason::Deadline);
        report.finish(TeeEnd::ClientAborted, TeeStats::default(), 0);
        assert_eq!(report.end(), TeeEnd::Cancelled(CancelReason::Deadline));
    }

    /// 定长响应：hyper 拿满 `Content-Length` 就不再 poll，那个 `None`
    /// 永远不会来。
    ///
    /// 这是被冒烟测试抓到的一个真 bug：光等 `None` 的话，每一个定长响应
    /// 都会在 body 被 drop 时记成 `ClientAborted` —— 审计里最误导人的
    /// 一种错，因为它看起来完全合理。
    #[tokio::test]
    async fn poll_frame_on_a_sized_body_records_eof_without_ever_seeing_none() {
        let (tee, rx) = handle(8, u64::MAX);
        let mut body = TeeBody::new(Canned::sized(&[b"aa", b"bbb"]), tee);

        // 只取该取的两帧，**不**多 poll 一次去拿 None —— hyper 就是这么干的
        let _ = body.frame().await.expect("第一帧");
        let _ = body.frame().await.expect("第二帧");
        drop(body);

        assert_eq!(rx.report().end(), TeeEnd::Eof, "定长响应被误记成客户端断开");
        assert_eq!(rx.report().stats().bytes_forwarded, 5);
    }

    /// 最后一段丢弃没有"下一个 chunk"可以搭车，只能写进 report。
    ///
    /// 极端情况是每一个 chunk 都超预算：一次都没送出去，旁路会看到一条
    /// 空流。不把这个数补上，它就会被记成 Complete —— 正是宪法 C13 说的
    /// 「悄无声息的有损」。
    #[tokio::test]
    async fn close_after_dropping_everything_still_reports_the_gap() {
        let (tee, rx) = handle(8, 4);
        let body = TeeBody::new(Canned::ok(&[b"way too long to fit", b"also too long"]), tee);
        let _ = body.collect().await.expect("读完");

        assert_eq!(rx.report().end(), TeeEnd::Eof);
        assert_eq!(rx.report().stats().bytes_dropped, 19 + 13);
        assert_eq!(
            rx.report().trailing_gap(),
            19 + 13,
            "一条都没送出去，全部丢弃必须挂在 trailing_gap 上"
        );
    }

    /// 成功发送会清掉 `pending_gap`，收尾时就没有尾巴了。
    #[tokio::test]
    async fn close_after_a_successful_send_leaves_no_trailing_gap() {
        let (tee, rx) = handle(8, u64::MAX);
        let body = TeeBody::new(Canned::ok(&[b"abc"]), tee);
        let _ = body.collect().await.expect("读完");
        assert_eq!(rx.report().trailing_gap(), 0);
    }

    /// `ObserveMode::Off`：连 `Bytes::clone` 都不做。
    #[tokio::test]
    async fn detached_body_forwards_everything_without_a_sink() {
        let body = TeeBody::detached(Canned::ok(&[b"abc"]));
        let collected = body.collect().await.expect("读完").to_bytes();
        assert_eq!(collected, Bytes::from_static(b"abc"));
    }

    /// 容量 0 的 mpsc 会 panic。不观测应该走 `ObserveMode::Off`，
    /// 而不是建一个零容量通道。
    #[tokio::test]
    async fn new_with_zero_capacity_still_builds_a_usable_channel() {
        let (mut tee, mut rx) = handle(0, u64::MAX);
        let mut stats = TeeStats::default();
        tee.offer(Bytes::from_static(b"x"), &mut stats);
        assert!(rx.recv().await.is_some());
    }
}
