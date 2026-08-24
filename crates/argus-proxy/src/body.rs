//! 主链路上包在上游 body 外面的两件事：**取消**与**超时**。
//!
//! 转发循环由 hyper 驱动，主链路没有一个能 `select!` 的地方（`docs/06` §8.2）。
//! 所以取消只能实现成又一个 `Body` 包装器，跟 [`TeeBody`](crate::tee::TeeBody)
//! 一样挂在链上：
//!
//! ```text
//! Cancellable< TeeBody< Incoming > >
//!     ↑            ↑
//!     停止的理由    分流给旁路
//! ```
//!
//! 顺序不能反。`Cancellable` 在**外**层，因为它要在字节流到达 tee 之前就把
//! 流掐断；而 tee 在里层，才能看到真正被转发出去的每一个字节。
//!
//! `idle_frame` 与 `total` 两段超时也在这里，理由是它们和取消是同一件事的
//! 三种说法 —— `CancelReason::Deadline` 的文档原话就是「触到 `Timeouts.total`
//! 或 `Timeouts.idle_frame`」。分成三个包装器只会让 poll 链多两层。
//!
//! 设计依据：`docs/06-data-plane.md` §8、`docs/10-streaming.md` §6.2。

use std::sync::{Arc, OnceLock};

use argus_core::CancelReason;
use bytes::Bytes;
use compact_str::CompactString;
use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use tokio::time::{Duration, Instant, Sleep};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use crate::tee::TeeReport;

/// 转发过程中让流提前结束的原因。
///
/// **不携带响应内容**（宪法 C27）：`Upstream` 里装的是传输层错误的描述，
/// 不是 provider 发回来的 body。
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum StreamError {
    #[error("upstream body error: {0}")]
    Upstream(CompactString),
    #[error("stream cancelled: {}", .0.as_str())]
    Cancelled(CancelReason),
}

impl StreamError {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Upstream(_) => "upstream",
            Self::Cancelled(_) => "cancelled",
        }
    }
}

/// 取消令牌 + 原因。
///
/// `CancellationToken` 只回答「取消了吗」，不回答「为什么」。而审计必须能
/// 回答后者：停机、超时和安全终止对下游是三件完全不同的事
/// （`docs/06` §8.2 的三个取消源）。
///
/// 原因用 `OnceLock`：**第一个**取消的理由才是真的，后面的都是它的后果。
#[derive(Clone, Debug)]
pub struct CancelHandle {
    token: CancellationToken,
    reason: Arc<OnceLock<CancelReason>>,
}

impl Default for CancelHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelHandle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            reason: Arc::new(OnceLock::new()),
        }
    }

    /// 派生一个子句柄：父被取消时子跟着被取消，但子有自己的原因格子。
    ///
    /// 进程停机就靠它 —— 一个 shutdown token 挂上成千上万条在飞的流。
    #[must_use]
    pub fn child(&self, inherited: CancelReason) -> Self {
        let child = Self {
            token: self.token.child_token(),
            reason: Arc::new(OnceLock::new()),
        };
        if self.token.is_cancelled() {
            let _ = child.reason.set(inherited);
        }
        child
    }

    pub fn cancel(&self, reason: CancelReason) {
        let _ = self.reason.set(reason);
        self.token.cancel();
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// 已经取消但没记下原因的，按 `Shutdown` 算 —— 那是唯一会从父 token
    /// 传下来、而子句柄看不到理由的情况。
    #[must_use]
    pub fn reason(&self) -> Option<CancelReason> {
        if self.token.is_cancelled() {
            Some(self.reason.get().copied().unwrap_or(CancelReason::Shutdown))
        } else {
            None
        }
    }

    fn wait(&self) -> WaitForCancellationFutureOwned {
        self.token.clone().cancelled_owned()
    }
}

pin_project! {
    /// 给上游 body 加上取消与两段超时。
    pub struct Cancellable<B> {
        #[pin]
        inner: B,
        cancel: CancelHandle,
        report: Arc<TeeReport>,
        // 两个 frame 之间的最大间隔。每收到一个 frame 就重置
        idle: Duration,
        #[pin]
        idle_sleep: Sleep,
        // 整条流的硬上限，不重置
        #[pin]
        total_sleep: Sleep,
        #[pin]
        cancelled: WaitForCancellationFutureOwned,
        // 已经产出过终止错误，不再重复
        done: bool,
    }
}

impl<B> Cancellable<B> {
    /// `idle` 对应 `Timeouts.idle_frame`，`total` 对应 `Timeouts.total`。
    #[must_use]
    pub fn new(
        inner: B,
        cancel: CancelHandle,
        report: Arc<TeeReport>,
        idle: Duration,
        total: Duration,
    ) -> Self {
        let cancelled = cancel.wait();
        Self {
            inner,
            cancel,
            report,
            idle,
            idle_sleep: tokio::time::sleep(idle),
            total_sleep: tokio::time::sleep(total),
            cancelled,
            done: false,
        }
    }
}

impl<B> Body for Cancellable<B>
where
    B: Body<Data = Bytes>,
    B::Error: fmt::Display,
{
    type Data = Bytes;
    type Error = StreamError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let mut this = self.project();
        if *this.done {
            return Poll::Ready(None);
        }

        // 三个终止源都要**注册 waker**，否则取消要等到下一个 frame 到达才
        // 生效 —— 而「流卡住了」正是最需要它立刻生效的场景
        if this.cancelled.as_mut().poll(cx).is_ready() {
            let reason = this.cancel.reason().unwrap_or(CancelReason::Shutdown);
            return terminate(this.done, this.report, reason);
        }
        if this.total_sleep.as_mut().poll(cx).is_ready() {
            this.cancel.cancel(CancelReason::Deadline);
            return terminate(this.done, this.report, CancelReason::Deadline);
        }

        match this.inner.poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                // 收到 frame 就重置空闲计时。这一条才是「流卡住了」的判据，
                // 而不是总时长 —— 一个正常的长回答本来就会跑很久
                let idle = *this.idle;
                this.idle_sleep.as_mut().reset(Instant::now() + idle);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                *this.done = true;
                Poll::Ready(Some(Err(StreamError::Upstream(e.to_string().into()))))
            }
            Poll::Ready(None) => {
                *this.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if this.idle_sleep.as_mut().poll(cx).is_ready() {
                    this.cancel.cancel(CancelReason::Deadline);
                    return terminate(this.done, this.report, CancelReason::Deadline);
                }
                Poll::Pending
            }
        }
    }

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.done || self.inner.is_end_stream()
    }

    #[inline]
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// 先把原因写进 report，再产出错误。
///
/// 顺序要紧：错误一路上抛之后 hyper 会把整条 body 链丢掉，`TeeBody` 的
/// `PinnedDrop` 会写一个 `ClientAborted`。先写的是真原因，后写的被
/// `OnceLock` 挡掉 —— 「客户端断了」是取消的后果，不是原因。
fn terminate(
    done: &mut bool,
    report: &TeeReport,
    reason: CancelReason,
) -> Poll<Option<Result<Frame<Bytes>, StreamError>>> {
    *done = true;
    report.note_cancelled(reason);
    Poll::Ready(Some(Err(StreamError::Cancelled(reason))))
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

    use argus_core::TeeEnd;
    use http_body_util::BodyExt;

    use super::*;

    /// 吐完预设 frame 之后**永远 pending** —— 模拟「上游卡住了」。
    struct Stalling {
        frames: VecDeque<Bytes>,
    }

    impl Stalling {
        fn with(chunks: &[&'static [u8]]) -> Self {
            Self {
                frames: chunks.iter().map(|c| Bytes::from_static(c)).collect(),
            }
        }
    }

    impl Body for Stalling {
        type Data = Bytes;
        type Error = core::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            match self.frames.pop_front() {
                Some(b) => Poll::Ready(Some(Ok(Frame::data(b)))),
                // 刻意不注册 waker：上游真卡住时就是这个样子，
                // 能把流救回来的只有超时
                None => Poll::Pending,
            }
        }
    }

    /// `Cancellable` 内部有 `Sleep` 与取消 future，两者都不是 `Unpin`，
    /// 而 `BodyExt::frame()` 要求 `Unpin`。装箱是测试侧的事 —— 生产里
    /// hyper 拿到的本来就是 `Pin<&mut _>`，不需要这一步。
    fn wrap(
        body: Stalling,
        idle: Duration,
        total: Duration,
    ) -> (
        Pin<Box<Cancellable<Stalling>>>,
        CancelHandle,
        Arc<TeeReport>,
    ) {
        let cancel = CancelHandle::new();
        let report = Arc::new(TeeReport::default());
        let wrapped = Cancellable::new(body, cancel.clone(), Arc::clone(&report), idle, total);
        (Box::pin(wrapped), cancel, report)
    }

    const LONG: Duration = Duration::from_secs(3600);

    // ── CancelHandle ────────────────────────────────────────────────

    #[test]
    fn reason_before_any_cancel_is_none() {
        assert_eq!(CancelHandle::new().reason(), None);
    }

    /// 第一个取消的理由才是真的，后面的都是它的后果。
    #[test]
    fn cancel_twice_keeps_the_first_reason() {
        let h = CancelHandle::new();
        h.cancel(CancelReason::SecurityBlock);
        h.cancel(CancelReason::ClientGone);
        assert_eq!(h.reason(), Some(CancelReason::SecurityBlock));
    }

    /// 进程停机：一个 token 挂上成千上万条在飞的流。
    #[test]
    fn child_handle_is_cancelled_together_with_its_parent() {
        let parent = CancelHandle::new();
        let child = parent.child(CancelReason::Shutdown);
        assert!(!child.is_cancelled());
        parent.cancel(CancelReason::Shutdown);
        assert!(child.is_cancelled());
        assert_eq!(child.reason(), Some(CancelReason::Shutdown));
    }

    /// 子句柄自己的原因不会污染父句柄 —— 一条流超时不等于整个进程要停。
    #[test]
    fn cancelling_a_child_leaves_the_parent_running() {
        let parent = CancelHandle::new();
        let child = parent.child(CancelReason::Shutdown);
        child.cancel(CancelReason::Deadline);
        assert!(!parent.is_cancelled());
        assert_eq!(child.reason(), Some(CancelReason::Deadline));
    }

    // ── Cancellable ─────────────────────────────────────────────────

    #[tokio::test]
    async fn poll_frame_before_any_cancel_passes_frames_through_untouched() {
        let (mut body, _cancel, report) = wrap(Stalling::with(&[b"a", b"bc"]), LONG, LONG);
        let first = body.frame().await.expect("有一帧").expect("不是错误");
        assert_eq!(first.into_data().expect("data"), Bytes::from_static(b"a"));
        let second = body.frame().await.expect("有一帧").expect("不是错误");
        assert_eq!(second.into_data().expect("data"), Bytes::from_static(b"bc"));
        assert_eq!(
            report.end(),
            TeeEnd::ClientAborted,
            "还没结束，report 是空的"
        );
    }

    /// 取消要**立刻**生效，而不是等下一个 frame 到达 —— 流卡住时正是最
    /// 需要它生效的场景，而那时下一个 frame 永远不会来。
    #[tokio::test(start_paused = true)]
    async fn poll_frame_on_a_stalled_stream_wakes_up_on_cancel() {
        let (mut body, cancel, report) = wrap(Stalling::with(&[]), LONG, LONG);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            cancel.cancel(CancelReason::SecurityBlock);
        });

        let err = body.frame().await.expect("有终止帧").expect_err("应该是错误");
        assert_eq!(err, StreamError::Cancelled(CancelReason::SecurityBlock));
        assert_eq!(
            report.end(),
            TeeEnd::Cancelled(CancelReason::SecurityBlock),
            "原因必须先写进 report，旁路才能如实记录"
        );
    }

    /// `idle_frame`：两个 frame 之间的最大间隔。这才是「流卡住了」的判据。
    #[tokio::test(start_paused = true)]
    async fn poll_frame_with_no_frame_for_longer_than_idle_cancels_with_deadline() {
        let (mut body, _cancel, report) =
            wrap(Stalling::with(&[b"first"]), Duration::from_secs(30), LONG);
        let _ = body.frame().await;
        let err = body.frame().await.expect("有终止帧").expect_err("应该是错误");
        assert_eq!(err, StreamError::Cancelled(CancelReason::Deadline));
        assert_eq!(report.end(), TeeEnd::Cancelled(CancelReason::Deadline));
    }

    /// `total`：整条流的硬上限，**不会**被 frame 重置。
    ///
    /// 一个正常的长回答会不断吐 frame，`idle_frame` 永远不触发 ——
    /// 拦住「一条流跑一整天」的只有这一条。
    #[tokio::test(start_paused = true)]
    async fn poll_frame_past_the_total_deadline_cancels_even_while_frames_arrive() {
        let idle = Duration::from_secs(30);
        let total = Duration::from_secs(60);
        let (mut body, _cancel, _report) = wrap(Stalling::with(&[b"x".as_slice(); 8]), idle, total);

        // 每 20 秒一个 frame：idle 永远不到，total 到了
        let mut seen = 0_u32;
        loop {
            match body.frame().await {
                Some(Ok(_)) => {
                    seen += 1;
                    tokio::time::sleep(Duration::from_secs(20)).await;
                }
                Some(Err(e)) => {
                    assert_eq!(e, StreamError::Cancelled(CancelReason::Deadline));
                    break;
                }
                None => panic!("不该正常结束"),
            }
        }
        assert!(seen >= 3, "total 该在几个 frame 之后才触发，实际 {seen}");
    }

    /// 终止之后不再产出第二个错误 —— hyper 会继续 poll 直到拿到 `None`。
    #[tokio::test(start_paused = true)]
    async fn poll_frame_after_termination_reports_end_of_stream() {
        let (mut body, cancel, _report) = wrap(Stalling::with(&[]), LONG, LONG);
        cancel.cancel(CancelReason::ClientGone);
        assert!(body.frame().await.expect("有终止帧").is_err());
        assert!(body.frame().await.is_none(), "第二次 poll 应该是流结束");
        assert!(body.is_end_stream());
    }
}
