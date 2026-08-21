//! 宪法 C21 在文本累积上的验证：**稳态每 delta 零堆分配（摊销）**。
//!
//! 不用自定义全局分配器 —— 那需要 `unsafe`，而全 workspace 禁 unsafe
//! （宪法 C8，`ci/check-invariants.sh` 会拦）。改用一个同样直接的判据：
//! `String` 只在扩容时分配，所以**容量没变 == 这次没分配**。
//!
//! 这个判据比"数一数 malloc 调用"更贴题：它测的正是我们关心的那件事
//! （追加文本时会不会反复申请内存），而且不受测试框架自身分配的干扰。

use argus_protocol::accumulator::TextBuf;

/// 每个 delta 的典型大小。真实的 token delta 大多在这个量级。
const DELTA: &str = "the quick brown fox ";
const DELTAS: usize = 10_000;

/// 10k 次 push，分配次数必须是**对数级**而不是线性。
///
/// 线性意味着每个 delta 都申请一次内存 —— 那是 C21 要拦的形状。
#[test]
fn text_buf_ten_thousand_deltas_reallocate_a_logarithmic_number_of_times() {
    let mut buf = TextBuf::new();
    let mut budget = usize::MAX;
    let mut reallocations = 0_u32;
    let mut last_capacity = buf.capacity();

    for _ in 0..DELTAS {
        buf.push(DELTA, &mut budget);
        if buf.capacity() != last_capacity {
            reallocations += 1;
            last_capacity = buf.capacity();
        }
    }

    assert_eq!(buf.len(), DELTA.len() * DELTAS);
    // 从 4 KiB 倍增到 200 KB 是 6 次；留一点余量，但绝不能接近 10_000
    assert!(
        reallocations <= 8,
        "10k 次 delta 扩容了 {reallocations} 次，不是摊销行为"
    );
}

/// 稳态：容量足够之后，再多的 delta 都不会再分配。
///
/// 这才是 R9 那句「稳态每 delta 零堆分配」的准确含义。**真正的
/// 「全程零分配」只有一开口就预留到 `max_accumulated_bytes` 才做得到**，
/// 而那意味着每个 block 一开口吃掉 1 MiB —— 一万条并发流直接把内存打光。
#[test]
fn text_buf_in_steady_state_does_not_allocate_at_all() {
    let mut buf = TextBuf::new();
    let mut budget = usize::MAX;

    // 预热到稳态
    for _ in 0..DELTAS {
        buf.push(DELTA, &mut budget);
    }
    let warm_capacity = buf.capacity();
    let headroom = warm_capacity - buf.len();
    let fits = headroom / DELTA.len();
    assert!(fits > 0, "预热之后应该还有余量");

    for _ in 0..fits {
        buf.push(DELTA, &mut budget);
        assert_eq!(buf.capacity(), warm_capacity, "稳态下不该再分配");
    }
}

/// 撞上限之后继续 push 一次分配都不该有 —— 什么都不写，自然不用扩容。
#[test]
fn text_buf_after_truncation_never_allocates_again() {
    let mut buf = TextBuf::new();
    let mut budget = 64;
    for _ in 0..DELTAS {
        buf.push(DELTA, &mut budget);
    }
    let capacity_after_truncation = buf.capacity();
    assert!(buf.truncated());

    for _ in 0..DELTAS {
        buf.push(DELTA, &mut budget);
        assert_eq!(buf.capacity(), capacity_after_truncation);
    }
    // 3 个完整 delta（60 字节）之后只剩 4 字节，第 4 个 delta 写进
    // "the " 就停 —— 全 ASCII，字符边界正好在 64
    assert_eq!(buf.len(), 64);
}
