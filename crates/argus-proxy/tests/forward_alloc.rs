//! 宪法 C21 在主链路上的验证：**转发一帧不分配堆内存**（F07 R7 / G6）。
//!
//! 为什么用 `dhat` 而不是自己写计数分配器：`#[global_allocator]` 要
//! `unsafe impl GlobalAlloc`，而全 workspace 禁 unsafe（宪法 C8，
//! `ci/check-invariants.sh` 会拦）。dhat 把 unsafe 关在它自己的 crate 里，
//! 我们这边一行都不用写 —— F07 §5 早就把它列成了备选项。
//!
//! ⚠️ 全局分配器是**整个测试二进制**的，而 `HeapStats` 是全局计数。所以
//! 这个文件里只有一个 `#[test]`：多个测试并行跑会互相污染计数，表现为
//! 随机的、只在多核机器上出现的失败。要加断言就往这一个里加。

use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use argus_proxy::tee::{TeeBody, TeeHandle};
use bytes::Bytes;
use http_body::{Body, Frame};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// 一个只吐预先造好的 `Bytes` 的 body。
///
/// 预先造好是关键：要量的是**转发**的成本，不是造数据的成本。让 body
/// 在 poll 里现场 `to_vec()` 的话，量到的全是构造开销。
struct Canned {
    chunks: Vec<Bytes>,
    next: usize,
}

impl Body for Canned {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let Some(chunk) = self.chunks.get(self.next).cloned() else {
            return Poll::Ready(None);
        };
        self.next += 1;
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.next >= self.chunks.len()
    }
}

/// 同步地把一个 body 抽干，返回转发出去的字节数。
fn drain<B: Body<Data = Bytes> + Unpin>(body: &mut B) -> usize {
    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut total = 0;
    loop {
        match Pin::new(&mut *body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    total += data.len();
                }
            }
            // 三种「没有下一帧了」在这里是一回事：都停止抽取
            Poll::Ready(Some(Err(_)) | None) | Poll::Pending => return total,
        }
    }
}

const FRAMES: usize = 1_000;

#[test]
fn poll_frame_forwarding_a_frame_allocates_nothing_on_the_main_chain() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // 语料先造好，且在计数之前 —— 造它的分配不算进转发成本
    let chunk = Bytes::from(vec![b'x'; 512]);
    let chunks: Vec<Bytes> = (0..FRAMES).map(|_| chunk.clone()).collect();

    // ── ① 摘掉旁路：这是纯转发路径 ──────────────────────────────
    let mut detached = TeeBody::detached(Canned {
        chunks: chunks.clone(),
        next: 0,
    });

    let before = dhat::HeapStats::get();
    let bytes = drain(&mut detached);
    let after = dhat::HeapStats::get();

    assert_eq!(bytes, FRAMES * 512, "语料没被完整转发，量到的不是转发成本");
    let allocs = after.total_blocks - before.total_blocks;
    assert_eq!(
        allocs, 0,
        "纯转发路径分配了 {allocs} 次堆内存（{FRAMES} 帧）—— 宪法 C21 要求 0"
    );

    // ── ② 挂上旁路：多出来的只该是 channel 的摊销成本 ────────────
    let (sink, mut rx) = TeeHandle::new(FRAMES * 2, u64::MAX);
    let mut teed = TeeBody::new(Canned { chunks, next: 0 }, sink);

    let before = dhat::HeapStats::get();
    let bytes = drain(&mut teed);
    let after = dhat::HeapStats::get();
    assert_eq!(bytes, FRAMES * 512);

    let teed_allocs = after.total_blocks - before.total_blocks;
    // tokio 的 mpsc 每 32 个槽位申请一个 block，所以「每帧 0 次」在挂了
    // 旁路之后只能是**摊销**的。这里钉住的是「摊销后远小于 1 次/帧」——
    // 真正要防的是有人在 offer 里加了一次 to_vec()，那会变成 1.0 次/帧
    assert!(
        teed_allocs * 8 < FRAMES as u64,
        "挂上旁路后 {FRAMES} 帧分配了 {teed_allocs} 次，超过了 channel 的摊销量级"
    );

    // 旁路真的收到了东西 —— 否则上面量的是「什么都没做」
    let mut received = 0;
    while let Ok(chunk) = rx.try_recv() {
        received += chunk.data.len();
    }
    assert_eq!(received, FRAMES * 512, "旁路没收到完整的字节");
}
