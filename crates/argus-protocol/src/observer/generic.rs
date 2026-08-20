//! 认不出协议时的兜底观测器。
//!
//! **认不出来不等于不观测。** Raw 模式和未知供应商仍然要有基础观测 ——
//! 字节数、事件数、耗时、状态码、raw 留存 —— 而不是一片空白
//! （`docs/09-observers.md` §4.1）。
//!
//! 两个观测器都**不解语义**：它们不知道什么是 token、什么是 tool call，
//! 也不该知道。一旦这里开始猜语义，猜错的结果就会以"看起来合理的错数据"
//! 形式进入审计，那比没有数据更糟。

use compact_str::CompactString;
use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
use smallvec::SmallVec;

use crate::framer::{FrameRef, FrameSink};

/// SSE 兜底：切帧、计数、留 raw，**不解语义**。
///
/// 直接实现 [`FrameSink`]，所以它就挂在 F03 的 `SseFramer` 后面，
/// 不需要另一套分帧逻辑。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenericSseObserver {
    frames: u64,
    /// 只统计 `data:` 字段的字节数，不含分隔符与 `event:` 行
    data_bytes: u64,
    /// `:` 开头的心跳注释。provider 用它保活，计数能看出连接是否健康
    comments: u64,
    /// 见到过 `data: [DONE]`
    saw_done: bool,
    /// 观测旁路丢掉的字节（主链路不受影响，见 `docs/10` §6）
    dropped_bytes: u64,
}

impl GenericSseObserver {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: 0,
            data_bytes: 0,
            comments: 0,
            saw_done: false,
            dropped_bytes: 0,
        }
    }

    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    #[must_use]
    pub const fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    #[must_use]
    pub const fn comments(&self) -> u64 {
        self.comments
    }

    #[must_use]
    pub const fn saw_done(&self) -> bool {
        self.saw_done
    }

    #[must_use]
    pub const fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }

    /// 观测是否完整。丢过字节就不完整 —— 对应 `Integrity::Degraded`
    /// （`docs/11-observation-model.md` §4.5，接线在 F04）。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.dropped_bytes == 0
    }
}

impl FrameSink for GenericSseObserver {
    fn emit(&mut self, frame: FrameRef<'_>) {
        match frame {
            FrameRef::Event { data, .. } => {
                self.frames = self.frames.saturating_add(1);
                self.data_bytes = self.data_bytes.saturating_add(data.len() as u64);
            }
            // `[DONE]` 是 OpenAI 的结束哨兵，算一帧但没有 data
            FrameRef::Done => {
                self.frames = self.frames.saturating_add(1);
                self.saw_done = true;
            }
            // 心跳不算事件 —— 把它算进 frames 会让"这条流产出了多少东西"
            // 这个数字随连接时长膨胀
            FrameRef::Comment => self.comments = self.comments.saturating_add(1),
        }
    }

    fn emit_gap(&mut self, dropped_bytes: u64) {
        self.dropped_bytes = self.dropped_bytes.saturating_add(dropped_bytes);
    }
}

/// 顶层 JSON 值的形状。不是对象时没有 key 可记，但"是什么形状"本身有用。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum JsonShape {
    #[default]
    Object,
    Array,
    /// 字符串 / 数字 / 布尔 / null
    Scalar,
    /// 根本不是合法 JSON
    Malformed,
}

impl JsonShape {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Object => "object",
            Self::Array => "array",
            Self::Scalar => "scalar",
            Self::Malformed => "malformed",
        }
    }
}

/// 非流式 JSON 兜底：记 raw 与**顶层字段名**，`不记 value`（宪法 C27）。
///
/// 「不记 value」不是靠自觉，是靠类型：反序列化时 value 一律吃进
/// [`IgnoredAny`]，它不分配、也不保留任何内容。想改成记 value 得先改这里
/// 的 visitor，那是一次显眼的改动。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenericJsonObserver {
    shape: JsonShape,
    top_level_keys: SmallVec<[CompactString; 8]>,
    /// key 太多时截断，被丢掉的个数
    truncated_keys: u32,
    body_bytes: u64,
}

impl GenericJsonObserver {
    /// 顶层 key 的记录上限。
    ///
    /// 上界不是洁癖：body 来自上游，一个有一百万个顶层 key 的响应会让
    /// 观测旁路吃掉一大块内存（宪法 C9 的同一条理由）。超出的只记个数。
    pub const MAX_KEYS: usize = 64;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 看一个完整的 JSON body。
    ///
    /// 永不返回 `Err`：解析不了就是 [`JsonShape::Malformed`]，那是一条
    /// 观测结论，不是错误（宪法 C2/C7）。
    #[must_use]
    pub fn observe(body: &[u8]) -> Self {
        let body_bytes = body.len() as u64;
        match serde_json::from_slice::<TopLevel>(body) {
            Ok(top) => Self {
                shape: top.shape,
                top_level_keys: top.keys,
                truncated_keys: top.truncated,
                body_bytes,
            },
            Err(_) => Self {
                shape: JsonShape::Malformed,
                body_bytes,
                ..Self::default()
            },
        }
    }

    #[must_use]
    pub const fn shape(&self) -> JsonShape {
        self.shape
    }

    #[must_use]
    pub fn top_level_keys(&self) -> &[CompactString] {
        &self.top_level_keys
    }

    #[must_use]
    pub const fn truncated_keys(&self) -> u32 {
        self.truncated_keys
    }

    #[must_use]
    pub const fn body_bytes(&self) -> u64 {
        self.body_bytes
    }

    /// 解析出了形状即算完整。`Malformed` 对应 `Integrity::Degraded`。
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        !matches!(self.shape, JsonShape::Malformed)
    }
}

/// 只用来把顶层形状读出来的中间类型。
#[derive(Default)]
struct TopLevel {
    shape: JsonShape,
    keys: SmallVec<[CompactString; 8]>,
    truncated: u32,
}

impl<'de> Deserialize<'de> for TopLevel {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(TopLevelVisitor)
    }
}

struct TopLevelVisitor;

fn scalar() -> TopLevel {
    TopLevel {
        shape: JsonShape::Scalar,
        ..TopLevel::default()
    }
}

/// 每个 `visit_*` 都只回答"是什么形状"，value 一个都不留。
impl<'de> Visitor<'de> for TopLevelVisitor {
    type Value = TopLevel;

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("any json value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TopLevel, A::Error> {
        let mut keys = SmallVec::<[CompactString; 8]>::new();
        let mut truncated = 0_u32;
        // `IgnoredAny` 是这里的关键：它会把 value 整个跳过，既不分配也不保留
        // key 用 `CompactString` 而不是 `&str`：含转义序列的 key
        // （`"a\u0062"`）借不出来，用 `&str` 会让整个 body 变成 Malformed
        while let Some((key, IgnoredAny)) = map.next_entry::<CompactString, IgnoredAny>()? {
            if keys.len() < GenericJsonObserver::MAX_KEYS {
                keys.push(key);
            } else {
                truncated = truncated.saturating_add(1);
            }
        }
        Ok(TopLevel {
            shape: JsonShape::Object,
            keys,
            truncated,
        })
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<TopLevel, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(TopLevel {
            shape: JsonShape::Array,
            ..TopLevel::default()
        })
    }

    fn visit_str<E>(self, _v: &str) -> Result<TopLevel, E> {
        Ok(scalar())
    }
    fn visit_i64<E>(self, _v: i64) -> Result<TopLevel, E> {
        Ok(scalar())
    }
    fn visit_u64<E>(self, _v: u64) -> Result<TopLevel, E> {
        Ok(scalar())
    }
    fn visit_f64<E>(self, _v: f64) -> Result<TopLevel, E> {
        Ok(scalar())
    }
    fn visit_bool<E>(self, _v: bool) -> Result<TopLevel, E> {
        Ok(scalar())
    }
    fn visit_unit<E>(self) -> Result<TopLevel, E> {
        Ok(scalar())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "测试里用固定的合法值构造，失败即测试本身写错了"
)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::framer::{Framer, FramerConfig, SseFramer};
    use argus_core::TeeChunk;

    // ── GenericSse ──────────────────────────────────────────────────────

    fn run_sse(input: &[u8]) -> GenericSseObserver {
        let mut framer = SseFramer::new(FramerConfig::default());
        let mut obs = GenericSseObserver::new();
        framer
            .push(&TeeChunk::new(Bytes::copy_from_slice(input)), &mut obs)
            .expect("固定输入不会溢出");
        framer.finish(&mut obs).expect("固定输入不会溢出");
        obs
    }

    /// 挂在 F03 的 `SseFramer` 后面就能用 —— 不需要第二套分帧逻辑。
    #[test]
    fn emit_a_stream_of_sse_events_counts_frames_and_data_bytes() {
        let obs = run_sse(b"data: {\"a\":1}\n\ndata: {\"b\":22}\n\n");
        assert_eq!(obs.frames(), 2);
        // 只数 data: 字段本身：`{"a":1}` 7 字节 + `{"b":22}` 8 字节
        assert_eq!(obs.data_bytes(), 7 + 8);
        assert!(obs.is_complete());
    }

    /// 心跳不算事件：算进去会让「这条流产出了多少东西」随连接时长膨胀。
    #[test]
    fn emit_comment_frames_counts_them_separately_from_events() {
        let obs = run_sse(b": ping\n\ndata: x\n\n: ping\n\n");
        assert_eq!(obs.frames(), 1);
        assert_eq!(obs.comments(), 2);
    }

    /// `[DONE]` 算一帧但没有 data —— 它是哨兵不是 JSON。
    #[test]
    fn emit_done_sentinel_counts_a_frame_without_data_bytes() {
        let obs = run_sse(b"data: x\n\ndata: [DONE]\n\n");
        assert_eq!(obs.frames(), 2);
        assert_eq!(obs.data_bytes(), 1);
        assert!(obs.saw_done());
    }

    /// 旁路丢字节 → 观测不完整，但主链路不受影响（`docs/10` §6）。
    #[test]
    fn emit_gap_marks_the_observation_as_incomplete() {
        let mut obs = GenericSseObserver::new();
        assert!(obs.is_complete());
        obs.emit_gap(4096);
        assert_eq!(obs.dropped_bytes(), 4096);
        assert!(!obs.is_complete());
    }

    /// 不解语义：`data:` 里是什么完全不影响计数。
    #[test]
    fn emit_events_with_unparseable_data_still_counts_them() {
        let obs = run_sse(b"data: not json at all\n\ndata: <xml/>\n\n");
        assert_eq!(obs.frames(), 2);
        assert!(obs.is_complete());
    }

    #[test]
    fn emit_nothing_leaves_every_counter_at_zero() {
        let obs = GenericSseObserver::new();
        assert_eq!(obs.frames(), 0);
        assert_eq!(obs.data_bytes(), 0);
        assert_eq!(obs.comments(), 0);
        assert!(!obs.saw_done());
    }

    // ── GenericJson ─────────────────────────────────────────────────────

    /// R7：记顶层字段名。
    #[test]
    fn observe_a_json_object_records_its_top_level_key_names() {
        let obs = GenericJsonObserver::observe(
            br#"{"id":"resp_1","model":"gpt-4o","usage":{"input_tokens":5}}"#,
        );
        assert_eq!(obs.shape(), JsonShape::Object);
        assert_eq!(obs.top_level_keys(), ["id", "model", "usage"]);
        assert!(obs.is_complete());
    }

    /// 宪法 C27：**一个 value 都不能出现在观测里**。
    ///
    /// 这条断言把 prompt、回复正文、以及任何被塞进 body 的凭据一起挡在外面。
    #[test]
    fn observe_a_json_object_records_no_values_at_all() {
        let obs = GenericJsonObserver::observe(
            br#"{"prompt":"my secret question","api_key":"sk-live-0123456789","n":42}"#,
        );
        let rendered = format!("{obs:?}");
        for leak in ["secret", "question", "sk-live", "0123456789", "42"] {
            assert!(!rendered.contains(leak), "泄漏了 `{leak}`：{rendered}");
        }
        // 但 key 名要在 —— 否则这个观测器就没用了
        assert_eq!(obs.top_level_keys(), ["prompt", "api_key", "n"]);
    }

    /// 嵌套结构里的 key 也不记：只有**顶层**。
    #[test]
    fn observe_nested_objects_records_only_the_outermost_keys() {
        let obs =
            GenericJsonObserver::observe(br#"{"outer":{"inner":{"deep":"v"}},"list":[{"x":1}]}"#);
        assert_eq!(obs.top_level_keys(), ["outer", "list"]);
        let rendered = format!("{obs:?}");
        assert!(!rendered.contains("inner"));
        assert!(!rendered.contains("deep"));
    }

    /// 含转义序列的 key 借不出 `&str`，早期实现会让整个 body 变 Malformed。
    #[test]
    fn observe_keys_containing_escapes_are_decoded_not_rejected() {
        let obs = GenericJsonObserver::observe(br#"{"abc":1,"d\"e":2}"#);
        assert_eq!(obs.shape(), JsonShape::Object);
        assert_eq!(obs.top_level_keys(), ["abc", "d\"e"]);
    }

    #[test]
    fn observe_non_object_json_records_the_shape_and_no_keys() {
        for (body, shape) in [
            (&br"[1,2,3]"[..], JsonShape::Array),
            (&br#""just a string""#[..], JsonShape::Scalar),
            (&br"42"[..], JsonShape::Scalar),
            (&br"true"[..], JsonShape::Scalar),
            (&br"null"[..], JsonShape::Scalar),
        ] {
            let obs = GenericJsonObserver::observe(body);
            assert_eq!(obs.shape(), shape, "body={:?}", core::str::from_utf8(body));
            assert!(obs.top_level_keys().is_empty());
            assert!(obs.is_complete());
        }
    }

    /// 解析不了是一条**观测结论**，不是错误（宪法 C2/C7）。
    #[test]
    fn observe_malformed_json_returns_a_degraded_observation_not_an_error() {
        for body in [&b""[..], &b"{"[..], &b"not json"[..], &b"\xff\xfe"[..]] {
            let obs = GenericJsonObserver::observe(body);
            assert_eq!(obs.shape(), JsonShape::Malformed);
            assert!(!obs.is_complete());
            assert_eq!(obs.body_bytes(), body.len() as u64);
        }
    }

    /// 一个有一百万个顶层 key 的响应不该让观测旁路吃掉一大块内存。
    #[test]
    fn observe_an_object_with_too_many_keys_truncates_and_counts_the_rest() {
        let pairs: Vec<String> = (0..100).map(|i| format!("\"k{i}\":{i}")).collect();
        let body = format!("{{{}}}", pairs.join(","));
        let obs = GenericJsonObserver::observe(body.as_bytes());
        assert_eq!(obs.top_level_keys().len(), GenericJsonObserver::MAX_KEYS);
        let want = u32::try_from(100 - GenericJsonObserver::MAX_KEYS).expect("常量");
        assert_eq!(obs.truncated_keys(), want);
        assert!(obs.is_complete(), "截断不等于观测失败");
    }

    #[test]
    fn observe_records_the_body_size_regardless_of_shape() {
        let body = br#"{"a":1}"#;
        assert_eq!(GenericJsonObserver::observe(body).body_bytes(), 7);
    }
}
