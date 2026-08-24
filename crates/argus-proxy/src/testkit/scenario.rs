//! 场景 → 发送计划。**纯计算**，不碰 socket，所以可以单独测。
//!
//! 把「发什么」与「怎么发」分开是刻意的：场景描述的是 provider 的行为，
//! 传输层描述的是字节怎么落到线上。两者交叉相乘，合在一起写会得到一个
//! 谁都不敢改的函数。
//!
//! ⚠️ 本文件里的 SSE 字节是**合成**的，不是 fixture：宪法 C25 要求 fixture
//! 只能来自录制，而录制工具是 F10 R6 的交付物。命名上也刻意避开 `fixture`
//! 一词，免得将来分不清哪些是真录的。

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// 回放的节奏（`docs/20` §8）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pacing {
    /// 按录制时的真实间隔回放。TTFT 与 token 间隔只有这样才有意义
    Realtime,
    /// 尽快发完
    AsFastAsPossible,
    /// 固定间隔
    Fixed(Duration),
}

/// provider 的行为（`docs/20` §8）。
#[derive(Clone, Debug)]
pub enum Scenario {
    /// 回放一段录制。`fixture` 旁边可以有一个同名 `.timing` 文件，
    /// 每行一个微秒数，对应每一帧**之前**要等多久（`Pacing::Realtime` 用）
    Replay { fixture: PathBuf, pacing: Pacing },
    /// 慢首 token：首帧之前先干等
    SlowFirstToken {
        delay: Duration,
        then: Box<Scenario>,
    },
    /// 发够 N 帧就把连接掐掉
    TruncateAfter { frames: usize },
    /// 直接回错误状态
    HttpError {
        status: u16,
        body: &'static str,
        retry_after: Option<u32>,
    },
    /// 200，但首个 event 就是 error（`docs/02` §7.4）
    OkThenErrorEvent,
    /// 一帧特别大
    OversizedFrame { bytes: usize },
    /// 正常发完但不给终止事件
    NoDoneSentinel,
    /// 每 `interval` 一个 token。压测主力：不依赖录制，参数可扫
    Synthetic { tokens: usize, interval: Duration },
    /// 永不响应
    Hang,
    /// 逐帧指定字节与分片边界。
    ///
    /// 上面八个场景描述的是「provider 表现出什么行为」，没有一个能表达
    /// 「这些字节，按这个边界，分这么多次发」—— 而透明性测试
    /// （`docs/20` §5.2）与丢弃 / `gap_before` 的用例要的恰恰是后者。
    Scripted {
        frames: Vec<Vec<u8>>,
        status: u16,
        content_type: &'static str,
        pacing: Pacing,
        /// 发完之后不打招呼直接关连接
        truncate: bool,
    },
}

/// 一次写出：先等 `delay`，再写 `bytes`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub delay: Duration,
    pub bytes: Vec<u8>,
}

/// 流走完之后怎么收场。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// 正常结束（h1 下写 0 长度 chunk）
    Complete,
    /// 不打招呼直接关连接。客户端会看到一条被截断的流
    Abrupt,
    /// 挂住不动，直到被对端断开
    Hang,
}

/// 场景展开后的完整发送计划。
#[derive(Clone, Debug)]
pub struct Plan {
    pub status: u16,
    pub content_type: &'static str,
    /// 额外响应头。目前只有 `Retry-After` 用得上
    pub extra_headers: Vec<(&'static str, String)>,
    pub steps: Vec<Step>,
    pub ending: Ending,
}

impl Plan {
    /// 计划里全部字节拼起来的长度。定长响应要用它算 `Content-Length`
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.steps.iter().map(|s| s.bytes.len()).sum()
    }

    fn sse(steps: Vec<Step>, ending: Ending) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            extra_headers: Vec::new(),
            steps,
            ending,
        }
    }
}

/// 展开时可能出的岔子。
#[derive(Debug)]
pub enum PlanError {
    /// 录制文件读不出来。**不兜底成空流** —— 那会让「录制丢了」表现成
    /// 「provider 返回了空响应」，是最难查的一类测试假绿
    Fixture { path: PathBuf, why: String },
    /// `.timing` 的行数与帧数对不上。对不上就说明录制是坏的
    TimingMismatch { frames: usize, timings: usize },
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fixture { path, why } => write!(f, "读不了录制 {}: {why}", path.display()),
            Self::TimingMismatch { frames, timings } => {
                write!(f, "录制有 {frames} 帧，但 .timing 有 {timings} 行")
            }
        }
    }
}

impl std::error::Error for PlanError {}

impl Scenario {
    /// 展开成发送计划。
    ///
    /// # Errors
    ///
    /// 只有 [`Scenario::Replay`] 会失败：录制读不出来，或 `.timing` 与帧数对不上。
    pub fn plan(&self) -> Result<Plan, PlanError> {
        match self {
            Self::Replay { fixture, pacing } => replay(fixture, *pacing),
            Self::SlowFirstToken { delay, then } => {
                let mut plan = then.plan()?;
                if let Some(first) = plan.steps.first_mut() {
                    first.delay = first.delay.saturating_add(*delay);
                } else {
                    // 一帧都没有的场景（比如 Hang）也要能慢：补一个空 step，
                    // 否则 delay 会被静默吞掉
                    plan.steps.push(Step {
                        delay: *delay,
                        bytes: Vec::new(),
                    });
                }
                Ok(plan)
            }
            Self::TruncateAfter { frames } => {
                let mut steps = synthetic_steps(*frames + 8, Duration::ZERO);
                steps.truncate(*frames);
                Ok(Plan::sse(steps, Ending::Abrupt))
            }
            Self::HttpError {
                status,
                body,
                retry_after,
            } => Ok(Plan {
                status: *status,
                content_type: "application/json",
                extra_headers: retry_after
                    .map(|s| vec![("retry-after", s.to_string())])
                    .unwrap_or_default(),
                steps: vec![Step {
                    delay: Duration::ZERO,
                    bytes: (*body).as_bytes().to_vec(),
                }],
                ending: Ending::Complete,
            }),
            Self::OkThenErrorEvent => Ok(Plan::sse(
                vec![
                    frame_step(ERROR_EVENT, Duration::ZERO),
                    frame_step(MESSAGE_STOP, Duration::ZERO),
                ],
                Ending::Complete,
            )),
            Self::OversizedFrame { bytes } => Ok(oversized(*bytes)),
            // 发完正文就收工，既不给 message_stop 也不给 [DONE]
            Self::NoDoneSentinel => Ok(Plan::sse(
                vec![
                    frame_step(MESSAGE_START, Duration::ZERO),
                    frame_step(BLOCK_START, Duration::ZERO),
                    frame_step(&delta_frame("Hello"), Duration::ZERO),
                ],
                Ending::Complete,
            )),
            Self::Synthetic { tokens, interval } => Ok(Plan::sse(
                synthetic_steps(*tokens, *interval),
                Ending::Complete,
            )),
            Self::Scripted {
                frames,
                status,
                content_type,
                pacing,
                truncate,
            } => Ok(scripted(frames, *status, content_type, *pacing, *truncate)),
            Self::Hang => Ok(Plan {
                status: 200,
                content_type: "text/event-stream",
                extra_headers: Vec::new(),
                steps: Vec::new(),
                ending: Ending::Hang,
            }),
        }
    }
}

impl Scenario {
    /// 一段完整的 Anthropic Messages 流，每帧一次写出。
    ///
    /// 内容是**合成**的（见文件头）。它的价值在于帧边界确定：转发的
    /// 字节数、事件数、usage 都能在测试里写死。
    #[must_use]
    pub fn anthropic_sample() -> Self {
        Self::Scripted {
            frames: ANTHROPIC_SAMPLE.iter().map(|f| f.as_bytes().to_vec()).collect(),
            status: 200,
            content_type: "text/event-stream",
            pacing: Pacing::AsFastAsPossible,
            truncate: false,
        }
    }

    /// 一整块 JSON，一次发完。
    #[must_use]
    pub fn json_body(body: &str) -> Self {
        Self::Scripted {
            frames: vec![body.as_bytes().to_vec()],
            status: 200,
            content_type: "application/json",
            pacing: Pacing::AsFastAsPossible,
            truncate: false,
        }
    }

    /// `frames` 个各带 `filler` 字节文本的 delta —— 用来把旁路的队列灌满。
    #[must_use]
    pub fn flood(frames: usize, filler: usize) -> Self {
        let text = "x".repeat(filler);
        Self::Scripted {
            frames: (0..frames).map(|_| delta_frame(&text).into_bytes()).collect(),
            status: 200,
            content_type: "text/event-stream",
            pacing: Pacing::AsFastAsPossible,
            truncate: false,
        }
    }

    /// 改状态码。只对 [`Scenario::Scripted`] 有意义，其余场景原样返回。
    #[must_use]
    pub fn with_status(mut self, code: u16) -> Self {
        if let Self::Scripted { status, .. } = &mut self {
            *status = code;
        }
        self
    }

    /// 改 content-type。同上。
    #[must_use]
    pub fn with_content_type(mut self, ct: &'static str) -> Self {
        if let Self::Scripted { content_type, .. } = &mut self {
            *content_type = ct;
        }
        self
    }

    /// 改帧间隔。同上。
    #[must_use]
    pub fn with_pacing(mut self, gap: Duration) -> Self {
        if let Self::Scripted { pacing, .. } = &mut self {
            *pacing = Pacing::Fixed(gap);
        }
        self
    }
}

/// 合成的 Anthropic Messages 流。**不是 fixture**（见文件头）。
pub const ANTHROPIC_SAMPLE: [&str; 7] = [
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"model\":\"claude-x\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
];

fn oversized(bytes: usize) -> Plan {
    let filler = "x".repeat(bytes);
    Plan::sse(
        vec![
            frame_step(MESSAGE_START, Duration::ZERO),
            Step {
                delay: Duration::ZERO,
                bytes: delta_frame(&filler).into_bytes(),
            },
            frame_step(MESSAGE_STOP, Duration::ZERO),
        ],
        Ending::Complete,
    )
}

fn scripted(
    frames: &[Vec<u8>],
    status: u16,
    content_type: &'static str,
    pacing: Pacing,
    truncate: bool,
) -> Plan {
    // Realtime 在 Scripted 下没有意义：没有录制就没有真实间隔可依。
    // 退化成「尽快」而不是报错 —— 这里的字节是调用方现造的，本来就没
    // 有「录制时的时序」这回事
    let gap = match pacing {
        Pacing::AsFastAsPossible | Pacing::Realtime => Duration::ZERO,
        Pacing::Fixed(d) => d,
    };
    Plan {
        status,
        content_type,
        extra_headers: Vec::new(),
        steps: frames
            .iter()
            .map(|bytes| Step {
                delay: gap,
                bytes: bytes.clone(),
            })
            .collect(),
        ending: if truncate {
            Ending::Abrupt
        } else {
            Ending::Complete
        },
    }
}

// 同步文件 IO：`plan()` 只在 mock 启动期跑一次，且由 `spawn_blocking`
// 兜住（见 `provider::MockProvider::start`），不在任何请求路径上。
#[expect(
    clippy::disallowed_methods,
    reason = "启动期一次性读取，调用方已放进 spawn_blocking"
)]
fn replay(fixture: &Path, pacing: Pacing) -> Result<Plan, PlanError> {
    let raw = std::fs::read(fixture).map_err(|e| PlanError::Fixture {
        path: fixture.to_path_buf(),
        why: e.to_string(),
    })?;
    let frames = split_frames(&raw);

    let delays = match pacing {
        Pacing::AsFastAsPossible => vec![Duration::ZERO; frames.len()],
        Pacing::Fixed(d) => vec![d; frames.len()],
        Pacing::Realtime => read_timings(fixture, frames.len())?,
    };

    let steps = frames
        .into_iter()
        .zip(delays)
        .map(|(bytes, delay)| Step { delay, bytes })
        .collect();
    Ok(Plan::sse(steps, Ending::Complete))
}

/// `Pacing::Realtime` 的时间来源：与录制同名的 `.timing`，每行一个微秒数。
///
/// 缺文件不静默退化成「尽快发」：那会让一次录制丢失变成一组**看起来正常
/// 但时序全错**的测试，而时序正是 `Realtime` 唯一的存在理由。
#[expect(
    clippy::disallowed_methods,
    reason = "启动期一次性读取，调用方已放进 spawn_blocking"
)]
fn read_timings(fixture: &Path, frames: usize) -> Result<Vec<Duration>, PlanError> {
    let path = fixture.with_extension("timing");
    let text = std::fs::read_to_string(&path).map_err(|e| PlanError::Fixture {
        path: path.clone(),
        why: e.to_string(),
    })?;
    let micros: Vec<u64> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.parse::<u64>().map_err(|e| PlanError::Fixture {
                path: path.clone(),
                why: format!("{l:?} 不是微秒数: {e}"),
            })
        })
        .collect::<Result<_, _>>()?;

    if micros.len() != frames {
        return Err(PlanError::TimingMismatch {
            frames,
            timings: micros.len(),
        });
    }
    Ok(micros.into_iter().map(Duration::from_micros).collect())
}

/// 按 SSE 的 `\n\n` 边界切帧。**保留分隔符**，拼起来必须与原文逐字节相同。
fn split_frames(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < raw.len() {
        if raw[i] == b'\n' && raw[i + 1] == b'\n' {
            out.push(raw[start..=i + 1].to_vec());
            start = i + 2;
            i += 2;
        } else {
            i += 1;
        }
    }
    if start < raw.len() {
        out.push(raw[start..].to_vec());
    }
    out
}

// ── 合成语料（不是 fixture，见文件头） ──────────────────────────────

const MESSAGE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"model\":\"claude-mock\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n";
const BLOCK_START: &str = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const BLOCK_STOP: &str =
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n";
const MESSAGE_STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
const ERROR_EVENT: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"mock overloaded\"}}\n\n";

fn delta_frame(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
    )
}

fn frame_step(frame: &str, delay: Duration) -> Step {
    Step {
        delay,
        bytes: frame.as_bytes().to_vec(),
    }
}

/// `tokens` 个 text delta，外面裹上完整的 message 骨架。
///
/// 间隔只加在 delta 上：骨架帧是 provider 一口气发的，给它们也加间隔会让
/// TTFT 里混进本不存在的等待。
fn synthetic_steps(tokens: usize, interval: Duration) -> Vec<Step> {
    let mut steps = Vec::with_capacity(tokens + 5);
    steps.push(frame_step(MESSAGE_START, Duration::ZERO));
    steps.push(frame_step(BLOCK_START, Duration::ZERO));
    for i in 0..tokens {
        steps.push(Step {
            delay: interval,
            bytes: delta_frame(&format!("tok{i} ")).into_bytes(),
        });
    }
    steps.push(frame_step(BLOCK_STOP, Duration::ZERO));
    steps.push(Step {
        delay: Duration::ZERO,
        bytes: format!(
            "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":{tokens}}}}}\n\n"
        )
        .into_bytes(),
    });
    steps.push(frame_step(MESSAGE_STOP, Duration::ZERO));
    steps
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "测试里用固定的合法值构造；文件 IO 是造语料，不在任何请求路径上"
)]
mod tests {
    use super::*;

    fn joined(plan: &Plan) -> String {
        plan.steps.iter().flat_map(|s| s.bytes.clone()).map(|b| b as char).collect()
    }

    #[test]
    fn plan_synthetic_wraps_the_deltas_in_a_complete_message_skeleton() {
        let plan = Scenario::Synthetic {
            tokens: 3,
            interval: Duration::ZERO,
        }
        .plan()
        .unwrap();
        let text = joined(&plan);
        assert!(text.starts_with("event: message_start"));
        assert!(text.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
        assert_eq!(text.matches("text_delta").count(), 3);
        assert_eq!(plan.ending, Ending::Complete);
    }

    /// 间隔只加在 delta 上：骨架帧也加的话，TTFT 里会混进本不存在的等待。
    #[test]
    fn plan_synthetic_puts_the_interval_only_on_the_deltas() {
        let gap = Duration::from_millis(7);
        let plan = Scenario::Synthetic {
            tokens: 4,
            interval: gap,
        }
        .plan()
        .unwrap();
        let waiting = plan.steps.iter().filter(|s| s.delay == gap).count();
        assert_eq!(waiting, 4, "只有 4 个 delta 该带间隔");
    }

    #[test]
    fn plan_slow_first_token_adds_the_delay_to_the_first_step_only() {
        let inner = Scenario::Synthetic {
            tokens: 2,
            interval: Duration::ZERO,
        };
        let plan = Scenario::SlowFirstToken {
            delay: Duration::from_millis(500),
            then: Box::new(inner),
        }
        .plan()
        .unwrap();
        assert_eq!(plan.steps[0].delay, Duration::from_millis(500));
        assert!(plan.steps[1..].iter().all(|s| s.delay == Duration::ZERO));
    }

    /// 一帧都不发的场景也要能「慢」，否则 delay 会被静默吞掉。
    #[test]
    fn plan_slow_first_token_over_an_empty_plan_still_keeps_the_delay() {
        let plan = Scenario::SlowFirstToken {
            delay: Duration::from_millis(30),
            then: Box::new(Scenario::Hang),
        }
        .plan()
        .unwrap();
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].delay, Duration::from_millis(30));
        assert!(plan.steps[0].bytes.is_empty());
    }

    #[test]
    fn plan_truncate_after_stops_at_the_frame_count_and_closes_abruptly() {
        let plan = Scenario::TruncateAfter { frames: 3 }.plan().unwrap();
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.ending, Ending::Abrupt);
    }

    #[test]
    fn plan_http_error_carries_the_status_and_retry_after() {
        let plan = Scenario::HttpError {
            status: 429,
            body: r#"{"error":"slow down"}"#,
            retry_after: Some(11),
        }
        .plan()
        .unwrap();
        assert_eq!(plan.status, 429);
        assert_eq!(plan.extra_headers, vec![("retry-after", "11".to_owned())]);
    }

    #[test]
    fn plan_ok_then_error_event_returns_200_with_an_error_as_the_first_frame() {
        let plan = Scenario::OkThenErrorEvent.plan().unwrap();
        assert_eq!(plan.status, 200);
        assert!(joined(&plan).starts_with("event: error"));
    }

    #[test]
    fn plan_oversized_frame_produces_a_frame_of_at_least_the_requested_size() {
        let plan = Scenario::OversizedFrame { bytes: 4096 }.plan().unwrap();
        let biggest = plan.steps.iter().map(|s| s.bytes.len()).max().unwrap();
        assert!(biggest >= 4096, "最大帧只有 {biggest} 字节");
    }

    #[test]
    fn plan_no_done_sentinel_ends_without_a_terminal_event() {
        let plan = Scenario::NoDoneSentinel.plan().unwrap();
        let text = joined(&plan);
        assert!(!text.contains("message_stop"));
        assert!(!text.contains("[DONE]"));
        assert_eq!(plan.ending, Ending::Complete);
    }

    #[test]
    fn plan_hang_produces_no_steps_and_never_ends() {
        let plan = Scenario::Hang.plan().unwrap();
        assert!(plan.steps.is_empty());
        assert_eq!(plan.ending, Ending::Hang);
    }

    /// 切帧必须可逆：拼回去要与原文逐字节相同（宪法 C1 的测试侧对应物）。
    #[test]
    fn split_frames_rejoined_is_byte_identical_to_the_input() {
        for raw in [
            b"a\n\nb\n\n".as_slice(),
            b"no separator at all",
            b"trailing\n\npartial",
            b"\n\n",
            b"",
        ] {
            let rejoined: Vec<u8> = split_frames(raw).concat();
            assert_eq!(rejoined, raw, "{raw:?} 切完拼不回去");
        }
    }

    #[test]
    fn plan_replay_on_a_missing_fixture_reports_the_path_instead_of_an_empty_stream() {
        let err = Scenario::Replay {
            fixture: PathBuf::from("/nonexistent/recording.sse"),
            pacing: Pacing::AsFastAsPossible,
        }
        .plan()
        .expect_err("录制不存在必须报错");
        assert!(matches!(err, PlanError::Fixture { .. }));
        assert!(err.to_string().contains("recording.sse"));
    }

    #[test]
    fn plan_replay_realtime_without_a_timing_file_fails_instead_of_pacing_freely() {
        let dir = std::env::temp_dir().join("argus-testkit-replay");
        std::fs::create_dir_all(&dir).unwrap();
        let sse = dir.join("only_bytes.sse");
        std::fs::write(&sse, b"event: a\ndata: {}\n\n").unwrap();
        let _ = std::fs::remove_file(sse.with_extension("timing"));

        let err = Scenario::Replay {
            fixture: sse,
            pacing: Pacing::Realtime,
        }
        .plan()
        .expect_err("Realtime 缺 .timing 必须报错");
        assert!(err.to_string().contains("timing"));
    }

    #[test]
    fn plan_replay_realtime_uses_the_recorded_interframe_delays() {
        let dir = std::env::temp_dir().join("argus-testkit-replay");
        std::fs::create_dir_all(&dir).unwrap();
        let sse = dir.join("paced.sse");
        std::fs::write(&sse, b"event: a\ndata: {}\n\nevent: b\ndata: {}\n\n").unwrap();
        std::fs::write(sse.with_extension("timing"), "0\n2500\n").unwrap();

        let plan = Scenario::Replay {
            fixture: sse,
            pacing: Pacing::Realtime,
        }
        .plan()
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].delay, Duration::ZERO);
        assert_eq!(plan.steps[1].delay, Duration::from_micros(2500));
    }

    /// 行数对不上说明录制是坏的，不能挑着用。
    #[test]
    fn plan_replay_realtime_with_a_mismatched_timing_file_is_rejected() {
        let dir = std::env::temp_dir().join("argus-testkit-replay");
        std::fs::create_dir_all(&dir).unwrap();
        let sse = dir.join("mismatched.sse");
        std::fs::write(&sse, b"event: a\ndata: {}\n\nevent: b\ndata: {}\n\n").unwrap();
        std::fs::write(sse.with_extension("timing"), "0\n").unwrap();

        let err = Scenario::Replay {
            fixture: sse,
            pacing: Pacing::Realtime,
        }
        .plan()
        .expect_err("帧数与 timing 行数对不上必须报错");
        assert!(matches!(
            err,
            PlanError::TimingMismatch {
                frames: 2,
                timings: 1
            }
        ));
    }
}
