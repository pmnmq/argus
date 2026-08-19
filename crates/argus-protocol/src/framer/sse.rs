//! SSE 分帧器。
//!
//! 设计依据：`docs/05-streaming.md` §5.2–5.4、§6.4、§8.2。
//!
//! 三条不变式，测试逐条钉住：
//!
//! 1. **切分不变性**（宪法 C24）：同一份字节，无论怎么切分喂进来，
//!    产出的帧序列完全相同。这是整个系统正确性的地基。
//! 2. **有界**（宪法 C9）：内部缓冲永远不超过 `max_frame_bytes`。
//! 3. **丢失可见**（宪法 C13）：`gap_before > 0` 必然产生一次 `emit_gap`。

use bytes::{Buf, Bytes, BytesMut};

use argus_core::TeeChunk;

use super::{FrameData, FrameRef, FrameSink, Framer, FramerError};

/// 分帧器的上限配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramerConfig {
    /// 单帧字节上限。超过即放弃该流的观测（主链路照常转发）。
    pub max_frame_bytes: usize,
}

impl Default for FramerConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
        }
    }
}

/// 分帧统计。落审计用，也是「这条流的观测质量如何」的依据。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FramerStats {
    /// 产出的帧数（含 `Done`，不含 `Comment`）
    pub frames: u64,
    /// 注释/心跳行数。R11 要求忽略但计数 —— 心跳消失往往是连接出问题的第一个信号。
    pub comments: u64,
    /// 发生过几次 gap
    pub gaps: u64,
    /// 累计丢了多少字节（旁路丢弃 + resync 期间扫掉的）
    pub dropped_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Normal,
    /// 丢过字节，正在找下一个帧边界。`scanned` 是本次 resync 已扫过的字节数。
    Resync {
        scanned: u64,
    },
    /// 已经放弃这条流。再 push 也只会返回同一个错误 —— 中途"恢复"只会
    /// 产出一堆撕裂的帧，比不产出更糟。
    Failed(FramerError),
}

/// resync 时需要保留的尾巴长度：最长的分隔符是 `\r\n\r\n`，
/// 它的最长真前缀是 `\r\n\r`，3 字节。
const MAX_SEP_PREFIX: usize = 3;

pub struct SseFramer {
    buf: BytesMut,
    state: State,
    cfg: FramerConfig,
    stats: FramerStats,
}

impl SseFramer {
    #[must_use]
    pub fn new(cfg: FramerConfig) -> Self {
        Self {
            buf: BytesMut::new(),
            state: State::Normal,
            cfg,
            stats: FramerStats::default(),
        }
    }

    #[must_use]
    pub fn stats(&self) -> FramerStats {
        self.stats
    }

    /// 旁路丢过字节：当前缓冲的尾部已经不连续了，整块作废。
    fn enter_resync(&mut self, dropped: u64, out: &mut dyn FrameSink) {
        self.stats.gaps += 1;
        self.stats.dropped_bytes = self.stats.dropped_bytes.saturating_add(dropped);
        self.buf.clear();
        self.state = State::Resync { scanned: 0 };
        out.emit_gap(dropped);
    }

    /// 在 resync 状态下消化一段输入。
    ///
    /// 返回 `Ok(true)` 表示已经找到边界、回到 `Normal`，`buf` 里是边界之后的
    /// 残留，调用方应继续走正常路径。
    fn resync(&mut self, input: &[u8], scanned: u64) -> Result<bool, FramerError> {
        self.buf.extend_from_slice(input);
        if let Some(end) = find_frame_end(&self.buf) {
            self.stats.dropped_bytes = self.stats.dropped_bytes.saturating_add(end as u64);
            self.buf.advance(end);
            self.state = State::Normal;
            return Ok(true);
        }

        // 没找到。只留够拼出跨块分隔符的尾巴，其余全部丢弃。
        let keep = self.buf.len().min(MAX_SEP_PREFIX);
        let discard = self.buf.len() - keep;
        self.buf.advance(discard);
        self.stats.dropped_bytes = self.stats.dropped_bytes.saturating_add(discard as u64);

        let scanned = scanned.saturating_add(discard as u64);
        if scanned > self.cfg.max_frame_bytes as u64 {
            let err = FramerError::ResyncFailed(scanned);
            self.state = State::Failed(err);
            return Err(err);
        }
        self.state = State::Resync { scanned };
        Ok(false)
    }

    /// 从 `buf` 里把所有完整帧切出来。
    fn drain_buffer(&mut self, out: &mut dyn FrameSink) {
        while let Some(end) = find_frame_end(&self.buf) {
            let frame = self.buf.split_to(end).freeze();
            self.emit_frame(&frame, out);
        }
    }

    fn guard_buffer(&mut self) -> Result<(), FramerError> {
        if self.buf.len() > self.cfg.max_frame_bytes {
            let err = FramerError::Overflow(self.buf.len());
            self.state = State::Failed(err);
            self.buf.clear();
            return Err(err);
        }
        Ok(())
    }

    /// 解析一帧的字段并发出。
    ///
    /// `raw` 含尾部分隔符。分隔符产生的空行在字段循环里被跳过，
    /// 不需要先切掉 —— 少一次边界计算，也少一处能算错的地方。
    fn emit_frame<'f>(&mut self, raw: &'f Bytes, out: &mut dyn FrameSink) {
        let mut event: Option<&'f [u8]> = None;
        let mut data_first: Option<&'f [u8]> = None;
        let mut data_more: Vec<u8> = Vec::new();
        let mut data_lines = 0_usize;
        let mut comments = 0_u64;
        let mut has_field = false;

        for line in Lines::new(raw.as_ref()) {
            if line.is_empty() {
                continue;
            }
            if line.first() == Some(&b':') {
                comments += 1;
                continue;
            }
            let (name, value) = split_field(line);
            has_field = true;
            match name {
                b"event" => event = Some(value),
                b"data" => {
                    if data_lines == 0 {
                        data_first = Some(value);
                    } else {
                        // 第二行才开始分配。SSE 规范要求用 \n 连接。
                        if data_lines == 1 {
                            data_more.extend_from_slice(data_first.unwrap_or_default());
                        }
                        data_more.push(b'\n');
                        data_more.extend_from_slice(value);
                    }
                    data_lines += 1;
                }
                // id / retry / 未知字段：按 SSE 规范忽略。
                // 未知字段必须静默通过而不是报错（宪法 C2）。
                _ => {}
            }
        }

        self.stats.comments += comments;

        if !has_field {
            // 只有注释的帧（心跳），或者纯空帧。
            if comments > 0 {
                out.emit(FrameRef::Comment);
            }
            return;
        }

        let data = if data_lines > 1 {
            FrameData::Joined(data_more)
        } else {
            FrameData::Borrowed(data_first.unwrap_or_default())
        };

        self.stats.frames += 1;
        if data_lines == 1 && data.as_bytes() == b"[DONE]" {
            out.emit(FrameRef::Done);
        } else {
            out.emit(FrameRef::Event { event, data, raw });
        }
    }
}

impl Framer for SseFramer {
    fn push(&mut self, chunk: &TeeChunk, out: &mut dyn FrameSink) -> Result<(), FramerError> {
        if let State::Failed(err) = self.state {
            return Err(err);
        }
        if chunk.gap_before > 0 {
            self.enter_resync(chunk.gap_before, out);
        }

        if let State::Resync { scanned } = self.state {
            if !self.resync(&chunk.data, scanned)? {
                return Ok(());
            }
            // 已回到 Normal，buf 里是边界之后的残留 —— 走 slow path
            self.drain_buffer(out);
            return self.guard_buffer();
        }

        if self.buf.is_empty() {
            // Fast path：缓冲为空，直接在输入的 Bytes 上切。
            // `split_to` 只动引用计数与偏移，不发生 memcpy（宪法 C22）。
            let mut rest = chunk.data.clone();
            while let Some(end) = find_frame_end(&rest) {
                let frame = rest.split_to(end);
                self.emit_frame(&frame, out);
            }
            if !rest.is_empty() {
                // 只有残留才拷贝
                self.buf.extend_from_slice(&rest);
            }
        } else {
            self.buf.extend_from_slice(&chunk.data);
            self.drain_buffer(out);
        }
        self.guard_buffer()
    }

    fn finish(&mut self, out: &mut dyn FrameSink) -> Result<(), FramerError> {
        match self.state {
            State::Failed(err) => return Err(err),
            // resync 没成功就 EOF 了：残留是半截帧，丢掉。
            State::Resync { .. } => {
                self.stats.dropped_bytes = self
                    .stats
                    .dropped_bytes
                    .saturating_add(self.buf.len() as u64);
                self.buf.clear();
                return Ok(());
            }
            State::Normal => {}
        }

        self.drain_buffer(out);
        if !self.buf.is_empty() {
            // 上游 EOF 但最后一帧没有尾随空行。真实 provider 会这样
            // （docs/04 坑 C8），当成完整帧处理。
            let frame = self.buf.split_to(self.buf.len()).freeze();
            self.emit_frame(&frame, out);
        }
        Ok(())
    }

    fn buffered_bytes(&self) -> usize {
        self.buf.len()
    }
}

/// 找到第一个帧分隔符的结束位置（分隔符之后的下标）。
///
/// # 这个函数是 F03 的全部难点
///
/// SSE 规范允许 `\n\n`、`\r\n\r\n`、`\r\r` 三种。`docs/05` §5.3 给的示意实现
/// 是三次独立的 `memmem::find` 再 `or_else`，那会返回**最早出现的 `\n\n`**，
/// 而不是**最早出现的任意分隔符**。对 `a\r\rb\n\n` 它会切出整个 7 字节，
/// 而正确答案是前 3 字节。文档里已经标注了这个 bug。
///
/// 这里改成一次从左到右的扫描：`memchr2` 找到下一个 `\n` 或 `\r`，就地判断
/// 它是不是某个分隔符的开头。因为是单向扫描，**第一个命中必然是最早的**。
///
/// 缓冲末尾出现分隔符的真前缀（如 `\r\n\r`）时返回 `None` —— 还不能确定，
/// 等更多字节。这正是切分不变性成立的原因。
///
/// # 只认这三种，是刻意的
///
/// SSE 规范里「空行」= 两个连续的行终止符，而终止符有 `\r\n` / `\n` / `\r`
/// 三种，组合起来共 7 种。R3 只要求认 `\n\n` / `\r\n\r\n` / `\r\r`；
/// 另外 4 种（`\n\r`、`\n\r\n`、`\r\n\n`、`\r\n\r` + 单 `\r`）都需要
/// 在一帧里混用不同的终止符，没有任何真实 provider 这么发。
///
/// 不扩大范围：每多认一种边界，就多一种能被构造出来的分帧歧义。
/// 若 F10 的 `_abnormal/010_mixed_line_endings` fixture 真的需要，再回来放宽。
fn find_frame_end(buf: &[u8]) -> Option<usize> {
    let mut from = 0_usize;
    loop {
        let rel = memchr::memchr2(b'\n', b'\r', buf.get(from..)?)?;
        let i = from + rel;
        match buf.get(i) {
            Some(b'\n') if buf.get(i + 1) == Some(&b'\n') => return Some(i + 2),
            Some(b'\r') => {
                if buf.get(i + 1) == Some(&b'\r') {
                    return Some(i + 2);
                }
                if buf.get(i + 1) == Some(&b'\n')
                    && buf.get(i + 2) == Some(&b'\r')
                    && buf.get(i + 3) == Some(&b'\n')
                {
                    return Some(i + 4);
                }
            }
            _ => {}
        }
        from = i + 1;
    }
}

/// 按 SSE 的三种行终止符切行：`\r\n`、`\n`、`\r`。
struct Lines<'a> {
    buf: &'a [u8],
}

impl<'a> Lines<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        let Some(i) = memchr::memchr2(b'\n', b'\r', self.buf) else {
            let line = self.buf;
            self.buf = &[];
            return Some(line);
        };
        let line = self.buf.get(..i)?;
        let skip =
            usize::from(self.buf.get(i) == Some(&b'\r') && self.buf.get(i + 1) == Some(&b'\n')) + 1;
        self.buf = self.buf.get(i + skip..).unwrap_or_default();
        Some(line)
    }
}

/// 把一行拆成 `(字段名, 值)`。
///
/// SSE 规范：冒号前是字段名，冒号后是值；值以单个空格开头时去掉那个空格
/// （只去一个 —— `data:  x` 的值是 ` x`）。整行没有冒号时，整行是字段名、值为空。
fn split_field(line: &[u8]) -> (&[u8], &[u8]) {
    let Some(i) = memchr::memchr(b':', line) else {
        return (line, &[]);
    };
    let name = line.get(..i).unwrap_or_default();
    let mut value = line.get(i + 1..).unwrap_or_default();
    if value.first() == Some(&b' ') {
        value = value.get(1..).unwrap_or_default();
    }
    (name, value)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "测试里这些是可读性收益大于风险的；crate 级的 deny 保持不变"
)]
mod tests {
    use super::*;

    /// 记录下来的帧。测试要跨 chunk 比较，所以必须拿走所有权。
    #[derive(Debug, PartialEq, Eq)]
    enum Rec {
        Event {
            event: Option<Vec<u8>>,
            data: Vec<u8>,
        },
        Done,
        Comment,
        Gap(u64),
    }

    #[derive(Default)]
    struct Recorder {
        recs: Vec<Rec>,
        /// 每个 Event 帧 `raw` 的地址，用来断言 fast path 没有发生拷贝
        raw_ptrs: Vec<usize>,
    }

    impl FrameSink for Recorder {
        fn emit(&mut self, frame: FrameRef<'_>) {
            match frame {
                FrameRef::Event { event, data, raw } => {
                    self.raw_ptrs.push(raw.as_ptr() as usize);
                    self.recs.push(Rec::Event {
                        event: event.map(<[u8]>::to_vec),
                        data: data.as_bytes().to_vec(),
                    });
                }
                FrameRef::Done => self.recs.push(Rec::Done),
                FrameRef::Comment => self.recs.push(Rec::Comment),
            }
        }

        fn emit_gap(&mut self, dropped_bytes: u64) {
            self.recs.push(Rec::Gap(dropped_bytes));
        }
    }

    fn framer() -> SseFramer {
        SseFramer::new(FramerConfig::default())
    }

    fn chunk(bytes: &'static [u8]) -> TeeChunk {
        TeeChunk::new(Bytes::from_static(bytes))
    }

    fn event(data: &str) -> Rec {
        Rec::Event {
            event: None,
            data: data.as_bytes().to_vec(),
        }
    }

    // ── find_frame_end：F03 的全部难点 ─────────────────────────────────

    #[test]
    fn find_frame_end_input_ends_with_lf_lf_returns_position_after_separator() {
        assert_eq!(find_frame_end(b"data: a\n\n"), Some(9));
    }

    #[test]
    fn find_frame_end_input_ends_with_crlf_crlf_returns_position_after_separator() {
        assert_eq!(find_frame_end(b"data: a\r\n\r\n"), Some(11));
    }

    #[test]
    fn find_frame_end_input_ends_with_cr_cr_returns_position_after_separator() {
        assert_eq!(find_frame_end(b"data: a\r\r"), Some(9));
    }

    /// **这是 `docs/05` §5.3 标记的 bug 点。**
    ///
    /// 三次独立 `memmem::find` 再 `or_else` 会先找到 `\n\n`（位置 4），
    /// 返回 7；但最早的分隔符是位置 1 的 `\r\r`，正确答案是 3。
    /// 一次从左到右扫描才能保证「第一个命中即最早」。
    #[test]
    fn find_frame_end_input_mixes_separators_returns_the_earliest_one() {
        assert_eq!(find_frame_end(b"a\r\rb\n\n"), Some(3));
        assert_eq!(find_frame_end(b"a\n\nb\r\r"), Some(3));
        // "a" + "\r\n\r\n" = 5 字节，帧结束位置是 5
        assert_eq!(find_frame_end(b"a\r\n\r\nb\r\r"), Some(5));
        assert_eq!(find_frame_end(b"a\r\rb\r\n\r\n"), Some(3));
    }

    /// 缓冲末尾是分隔符的真前缀时必须返回 None —— 还不能确定。
    /// 这正是切分不变性成立的原因。
    #[test]
    fn find_frame_end_input_ends_with_partial_separator_returns_none() {
        for partial in [
            b"data: a".as_slice(),
            b"data: a\n",
            b"data: a\r",
            b"data: a\r\n",
            b"data: a\r\n\r",
        ] {
            assert_eq!(
                find_frame_end(partial),
                None,
                "{partial:?} 不该被判为完整帧"
            );
        }
    }

    /// LF 之后跟 CR 系终止符：当前刻意不认。钉成显式行为，免得被当成漏洞。
    #[test]
    fn find_frame_end_input_is_lf_then_cr_terminator_returns_none() {
        for mixed in [b"a\n\r".as_slice(), b"a\n\r\n"] {
            assert_eq!(
                find_frame_end(mixed),
                None,
                "{mixed:?} 需要混用终止符风格，当前刻意不认"
            );
        }
    }

    /// 反过来，CR 系在前的混用是**认**的 —— 它们把点名的分隔符当子串包含了。
    /// 这个用例是写上一条时才发现的：原以为「只认三种」，实际覆盖比三种宽。
    #[test]
    fn find_frame_end_input_is_crlf_then_lf_terminator_returns_the_embedded_match() {
        assert_eq!(find_frame_end(b"a\r\n\n"), Some(4));
        assert_eq!(find_frame_end(b"a\r\n\r\r"), Some(5));
    }

    #[test]
    fn find_frame_end_input_has_no_separator_returns_none() {
        assert_eq!(find_frame_end(b""), None);
        assert_eq!(find_frame_end(b"no separators here"), None);
    }

    // ── fast path / slow path ─────────────────────────────────────────

    /// 宪法 C22：缓冲为空时转发不产生 memcpy。
    ///
    /// 判据是产出帧的 `raw` 指针落在输入缓冲的地址区间内 —— 说明它只是
    /// 同一块内存上的一个偏移，而不是一份拷贝。
    #[test]
    fn push_buffer_empty_and_chunk_self_contained_shares_the_input_buffer() {
        let data = Bytes::from_static(b"data: a\n\ndata: b\n\n");
        let lo = data.as_ptr() as usize;
        let hi = lo + data.len();

        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&TeeChunk::new(data.clone()), &mut rec).unwrap();

        assert_eq!(rec.recs, vec![event("a"), event("b")]);
        assert_eq!(rec.raw_ptrs.len(), 2);
        for p in &rec.raw_ptrs {
            assert!(
                (lo..hi).contains(p),
                "帧的 raw 不在输入缓冲区内，说明发生了拷贝"
            );
        }
        assert_eq!(f.buffered_bytes(), 0);
    }

    #[test]
    fn push_frame_spans_two_chunks_copies_only_the_residue() {
        let mut f = framer();
        let mut rec = Recorder::default();

        f.push(&chunk(b"data: hel"), &mut rec).unwrap();
        assert!(rec.recs.is_empty());
        assert_eq!(f.buffered_bytes(), 9, "只应缓冲残留的 9 字节");

        f.push(&chunk(b"lo\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("hello")]);
        assert_eq!(f.buffered_bytes(), 0);
    }

    #[test]
    fn buffered_bytes_after_partial_frame_returns_residue_length() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data: a\n\ndata: partial"), &mut rec)
            .unwrap();
        assert_eq!(rec.recs, vec![event("a")]);
        assert_eq!(f.buffered_bytes(), b"data: partial".len());
    }

    // ── 帧内字段 ──────────────────────────────────────────────────────

    #[test]
    fn push_data_is_done_sentinel_emits_done() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data: [DONE]\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![Rec::Done]);
    }

    #[test]
    fn push_frame_has_event_field_exposes_it() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"event: message_start\ndata: {}\n\n"), &mut rec)
            .unwrap();
        assert_eq!(
            rec.recs,
            vec![Rec::Event {
                event: Some(b"message_start".to_vec()),
                data: b"{}".to_vec(),
            }]
        );
    }

    /// R11：注释行忽略但计数。心跳消失往往是连接出问题的第一个信号。
    #[test]
    fn push_frame_is_only_comments_emits_comment_and_counts_it() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b": ping\n\n: keep-alive\n\n"), &mut rec)
            .unwrap();
        assert_eq!(rec.recs, vec![Rec::Comment, Rec::Comment]);
        assert_eq!(f.stats().comments, 2);
        assert_eq!(f.stats().frames, 0, "注释不算帧");
    }

    #[test]
    fn push_frame_mixes_comment_and_data_emits_event_and_still_counts_the_comment() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b": ping\ndata: a\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("a")]);
        assert_eq!(f.stats().comments, 1);
    }

    /// R12：SSE 规范允许一帧多个 `data:`，按规范用 `\n` 连接。
    /// 实践中 AI provider 不这么发，但必须容忍。
    #[test]
    fn push_frame_has_multiple_data_lines_joins_them_with_lf() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(
            &chunk(b"data: line1\ndata: line2\ndata: line3\n\n"),
            &mut rec,
        )
        .unwrap();
        assert_eq!(rec.recs, vec![event("line1\nline2\nline3")]);
    }

    #[test]
    fn push_data_line_is_empty_emits_event_with_empty_data() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data:\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("")]);
    }

    /// SSE 规范：值以单个空格开头时去掉那个空格，只去一个。
    #[test]
    fn push_data_value_has_leading_spaces_strips_exactly_one() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data:  x\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event(" x")]);
    }

    /// 宪法 C2：未知字段静默通过，不报错、不丢帧。
    #[test]
    fn push_frame_has_unknown_fields_ignores_them_and_keeps_the_frame() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(
            &chunk(b"id: 42\nretry: 3000\nx-vendor: whatever\ndata: a\n\n"),
            &mut rec,
        )
        .unwrap();
        assert_eq!(rec.recs, vec![event("a")]);
    }

    /// 只有分隔符、没有任何字段的空帧不产生事件 —— SSE 语义里它不派发任何东西。
    #[test]
    fn push_frame_has_no_fields_emits_nothing() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"\n\n\n\ndata: a\n\n"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("a")]);
    }

    // ── gap 与 resync ─────────────────────────────────────────────────

    /// 宪法 C13：有损是允许的，悄无声息的有损不允许。
    #[test]
    fn push_chunk_has_gap_before_emits_gap_and_resyncs_to_the_next_frame() {
        let mut f = framer();
        let mut rec = Recorder::default();

        f.push(&chunk(b"data: first\n\ndata: half"), &mut rec)
            .unwrap();
        assert_eq!(rec.recs, vec![event("first")]);

        // 丢了 500 字节，缓冲里那半截 "data: half" 已经和后续不连续了
        let gapped = TeeChunk {
            data: Bytes::from_static(b"broken-tail\n\ndata: second\n\n"),
            gap_before: 500,
        };
        f.push(&gapped, &mut rec).unwrap();

        assert_eq!(
            rec.recs,
            vec![event("first"), Rec::Gap(500), event("second")],
            "gap 之后残缺的那一帧必须被丢弃，下一个完整帧要正常产出"
        );
        assert_eq!(f.stats().gaps, 1);
        assert!(f.stats().dropped_bytes >= 500);
    }

    #[test]
    fn push_gap_and_separator_span_two_chunks_still_resyncs() {
        let mut f = framer();
        let mut rec = Recorder::default();

        let gapped = TeeChunk {
            data: Bytes::from_static(b"garbage\r\n"),
            gap_before: 10,
        };
        f.push(&gapped, &mut rec).unwrap();
        // 分隔符 \r\n\r\n 被切成了两半
        f.push(&chunk(b"\r\ndata: ok\n\n"), &mut rec).unwrap();

        assert_eq!(rec.recs, vec![Rec::Gap(10), event("ok")]);
    }

    /// R8：扫不到边界不能无限扫下去，也不能 panic。
    #[test]
    fn push_resync_exceeds_max_frame_bytes_returns_resync_failed() {
        let mut f = SseFramer::new(FramerConfig {
            max_frame_bytes: 64,
        });
        let mut rec = Recorder::default();

        let junk = Bytes::from(vec![b'x'; 200]);
        let err = f
            .push(
                &TeeChunk {
                    data: junk,
                    gap_before: 1,
                },
                &mut rec,
            )
            .unwrap_err();

        assert!(matches!(err, FramerError::ResyncFailed(n) if n > 64));
        assert_eq!(rec.recs, vec![Rec::Gap(1)]);
    }

    /// R9：单帧超限时放弃观测，返回错误而不是 panic、也不是无界增长。
    #[test]
    fn push_frame_exceeds_max_frame_bytes_returns_overflow() {
        let mut f = SseFramer::new(FramerConfig {
            max_frame_bytes: 32,
        });
        let mut rec = Recorder::default();

        let err = f
            .push(&TeeChunk::new(Bytes::from(vec![b'a'; 100])), &mut rec)
            .unwrap_err();

        assert!(matches!(err, FramerError::Overflow(n) if n == 100));
        assert!(rec.recs.is_empty());
        assert_eq!(f.buffered_bytes(), 0, "失败后必须把缓冲释放掉");
    }

    /// 中途"恢复"只会产出一堆撕裂的帧，比不产出更糟。
    #[test]
    fn push_after_a_fatal_error_returns_the_same_error() {
        let mut f = SseFramer::new(FramerConfig { max_frame_bytes: 8 });
        let mut rec = Recorder::default();

        let first = f
            .push(&TeeChunk::new(Bytes::from(vec![b'a'; 40])), &mut rec)
            .unwrap_err();
        let second = f.push(&chunk(b"data: a\n\n"), &mut rec).unwrap_err();
        assert_eq!(first, second);
        assert!(rec.recs.is_empty());
    }

    #[test]
    fn finish_after_a_fatal_error_returns_the_same_error() {
        let mut f = SseFramer::new(FramerConfig { max_frame_bytes: 8 });
        let mut rec = Recorder::default();
        let err = f
            .push(&TeeChunk::new(Bytes::from(vec![b'a'; 40])), &mut rec)
            .unwrap_err();
        assert_eq!(f.finish(&mut rec).unwrap_err(), err);
    }

    // ── finish ────────────────────────────────────────────────────────

    /// `docs/04` 坑 C8：真实 provider 异常结束时最后一帧没有尾随空行。
    #[test]
    fn finish_last_frame_lacks_trailing_blank_line_still_emits_it() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data: a\n\ndata: b"), &mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("a")]);

        f.finish(&mut rec).unwrap();
        assert_eq!(rec.recs, vec![event("a"), event("b")]);
        assert_eq!(f.buffered_bytes(), 0);
    }

    #[test]
    fn finish_buffer_is_empty_emits_nothing() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data: a\n\n"), &mut rec).unwrap();
        let before = rec.recs.len();
        f.finish(&mut rec).unwrap();
        assert_eq!(rec.recs.len(), before);
    }

    /// resync 还没成功就 EOF 了：残留是半截帧，不能当成完整帧交出去。
    #[test]
    fn finish_still_resyncing_discards_the_partial_tail() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(
            &TeeChunk {
                data: Bytes::from_static(b"tail-of-a-broken-frame"),
                gap_before: 7,
            },
            &mut rec,
        )
        .unwrap();
        f.finish(&mut rec).unwrap();
        assert_eq!(rec.recs, vec![Rec::Gap(7)]);
    }

    #[test]
    fn stats_after_two_frames_and_one_comment_counts_each_kind() {
        let mut f = framer();
        let mut rec = Recorder::default();
        f.push(&chunk(b"data: a\n\n: ping\n\ndata: [DONE]\n\n"), &mut rec)
            .unwrap();
        let s = f.stats();
        assert_eq!(s.frames, 2, "Done 算帧，Comment 不算");
        assert_eq!(s.comments, 1);
        assert_eq!(s.gaps, 0);
    }
}
