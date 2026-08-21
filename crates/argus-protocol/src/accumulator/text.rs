//! 文本累积缓冲：摊销增长 + 上限。
//!
//! 两条硬要求：
//!
//! - **cap 检查在 `push_str` 之前**（F04 §5）。反过来写会先扩容再发现超限，
//!   而"发现超限"这件事本身就是为了不扩容。
//! - **撞上限之后继续消费后续事件**（宪法 C10）。停止累积 ≠ 停止观测：
//!   usage、finish、gap 这些结构性信息一个都不能丢。

/// 首次写入时预留的容量。
///
/// 4 KiB 覆盖绝大多数回答；再往上按 `String` 的倍增摊销。**不**按
/// `max_accumulated_bytes` 预留 —— 那意味着每个 block 一开口就吃掉 1 MiB，
/// 一万条并发流直接把内存打光（宪法 C9 的同一条理由）。
const INITIAL_CAPACITY: usize = 4096;

/// 带上限的文本缓冲。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TextBuf {
    buf: String,
    truncated: bool,
}

impl TextBuf {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: String::new(),
            truncated: false,
        }
    }

    /// 往缓冲里追加，从 `budget` 里扣掉相应字节数。
    ///
    /// 装不下时写入能装下的**最长 UTF-8 前缀**，标记 `truncated` 并把
    /// `budget` 清零。写半个字符会让落库的文本变成乱码，比少写几个字更糟。
    ///
    /// 返回实际写入的字节数。
    pub fn push(&mut self, text: &str, budget: &mut usize) -> usize {
        if text.is_empty() {
            return 0;
        }
        // cap 检查在扩容之前
        if text.len() <= *budget {
            self.reserve_first_time();
            self.buf.push_str(text);
            *budget -= text.len();
            return text.len();
        }

        let fits = floor_char_boundary(text, *budget);
        if fits > 0 {
            self.reserve_first_time();
            if let Some(prefix) = text.get(..fits) {
                self.buf.push_str(prefix);
            }
        }
        self.truncated = true;
        *budget = 0;
        fits
    }

    fn reserve_first_time(&mut self) {
        if self.buf.capacity() == 0 {
            self.buf.reserve(INITIAL_CAPACITY);
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// 当前缓冲容量。用来验证 C21 的摊销性质 —— 容量不变就意味着这一次
    /// `push` 没有发生堆分配。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// 交出内部缓冲，收尾时用。
    #[must_use]
    pub fn into_string(self) -> String {
        self.buf
    }
}

/// 不超过 `max` 的最大 UTF-8 字符边界。
///
/// 标准库的 `floor_char_boundary` 还没稳定，自己写一个。往回最多退 3 个
/// 字节 —— UTF-8 编码单元最长 4 字节。
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_text_within_budget_appends_it_and_debits_the_budget() {
        let mut buf = TextBuf::new();
        let mut budget = 100;
        assert_eq!(buf.push("hello", &mut budget), 5);
        assert_eq!(buf.push(" world", &mut budget), 6);
        assert_eq!(buf.as_str(), "hello world");
        assert_eq!(budget, 89);
        assert!(!buf.truncated());
    }

    /// R8：撞上限就停，并标记 —— 但缓冲里已有的内容不动。
    #[test]
    fn push_text_exceeding_the_budget_truncates_and_zeroes_the_budget() {
        let mut buf = TextBuf::new();
        let mut budget = 8;
        assert_eq!(buf.push("hello", &mut budget), 5);
        assert_eq!(buf.push(" world", &mut budget), 3);
        assert_eq!(buf.as_str(), "hello wo");
        assert_eq!(budget, 0);
        assert!(buf.truncated());
    }

    /// 写半个字符会让落库的文本变成乱码 —— 宁可少写几个字。
    #[test]
    fn push_text_truncating_mid_character_backs_off_to_a_char_boundary() {
        let mut buf = TextBuf::new();
        // 「中」是 3 字节，预算只剩 2
        let mut budget = 2;
        assert_eq!(buf.push("中文", &mut budget), 0);
        assert_eq!(buf.as_str(), "");
        assert!(buf.truncated());

        let mut buf = TextBuf::new();
        let mut budget = 4;
        assert_eq!(buf.push("中文", &mut budget), 3, "只写得下第一个字");
        assert_eq!(buf.as_str(), "中");
        assert!(buf.truncated());
    }

    /// 预算清零之后继续 push 不 panic，也不再写入。
    #[test]
    fn push_after_the_budget_is_exhausted_writes_nothing_and_does_not_panic() {
        let mut buf = TextBuf::new();
        let mut budget = 2;
        buf.push("abcd", &mut budget);
        assert_eq!(buf.push("efgh", &mut budget), 0);
        assert_eq!(buf.as_str(), "ab");
        assert_eq!(budget, 0);
    }

    #[test]
    fn push_an_empty_string_is_a_no_op() {
        let mut buf = TextBuf::new();
        let mut budget = 10;
        assert_eq!(buf.push("", &mut budget), 0);
        assert_eq!(budget, 10);
        assert!(!buf.truncated());
        assert!(buf.is_empty());
    }

    /// 不按 `max_accumulated_bytes` 预留：一个 block 一开口就吃掉 1 MiB，
    /// 一万条并发流会直接把内存打光。
    #[test]
    fn push_first_write_reserves_a_modest_capacity_not_the_whole_budget() {
        let mut buf = TextBuf::new();
        let mut budget = 1024 * 1024;
        buf.push("hi", &mut budget);
        assert!(buf.buf.capacity() >= INITIAL_CAPACITY);
        assert!(buf.buf.capacity() < 1024 * 1024);
    }

    #[test]
    fn floor_char_boundary_never_splits_a_multibyte_character() {
        let s = "a中b";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 1);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 99), s.len());
    }
}
