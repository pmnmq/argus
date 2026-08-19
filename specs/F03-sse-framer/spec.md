# F03 · SSE 分帧器

| | |
|---|---|
| ID | F03 |
| 里程碑 | M0 |
| 依赖 | F02 |
| 主 crate | `argus-protocol` |
| 设计依据 | `docs/05-streaming.md` §5.2–5.4,§6.4；`docs/04-protocols.md` §9.2；`docs/16-mvp-roadmap.md` §6.1 |
| 宪法条款 | C7, C9, C10, C13, C21, C24 |

## 1. 目标

把任意切分的字节流切成 SSE 逻辑帧，且**对任意切分方式产出完全相同的结果**。这是整个系统正确性的地基。

> `docs/16` §6.1 把这一项标为"最容易被低估的任务"。它看着简单，实际是全项目最容易出静默 bug 的地方。

## 2. 范围

**In scope**：`Framer` trait、`SseFramer`、fast/slow path、三种分隔符、gap resync、缓冲上限、`[DONE]` 哨兵、注释行、多行 `data:`。

**Out of scope**：理解 `data:` 里的 JSON（→ F05/F08）、`EventStreamFramer`（→ Phase 2，但 trait 现在就要留位）。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供 `trait Framer`，使 `SseFramer` 与未来的 `EventStreamFramer` 可互换（`docs/04` §9.2）。
- **R2** WHEN 输入包含完整的帧分隔符 THE SYSTEM SHALL 产出对应的 `FrameRef`。
- **R3** THE SYSTEM SHALL 识别 `\n\n`、`\r\n\r\n`、`\r\r` 三种分隔符，**一次扫描取最早出现者**。
- **R4** WHEN 内部缓冲为空且本次输入自带完整边界 THE SYSTEM SHALL 走 fast path，用 `Bytes::split_to` 产出帧而不发生 memcpy（宪法 C21/C22）。
- **R5** WHEN 一帧跨越多次输入 THE SYSTEM SHALL 只拷贝残留部分到内部缓冲。
- **R6** WHEN `TeeChunk.gap_before > 0` THE SYSTEM SHALL 丢弃当前缓冲、进入 `Resync` 状态、并产出一个 `Gap { dropped_bytes }` 信号（宪法 C13）。
- **R7** WHILE 处于 `Resync` 状态 THE SYSTEM SHALL 丢弃字节直到遇到下一个分隔符，然后恢复正常解析。
- **R8** IF `Resync` 扫描超过 `max_frame_bytes` 仍未找到边界 THEN THE SYSTEM SHALL 放弃该流的观测并返回 `FramerError::ResyncFailed`（不是 panic）。
- **R9** IF 内部缓冲超过 `max_frame_bytes` THEN THE SYSTEM SHALL 返回 `FramerError::Overflow` 并停止该流观测（宪法 C10：降级不报错到主链路）。
- **R10** WHEN 帧内容为 `data: [DONE]` THE SYSTEM SHALL 产出 `FrameRef::Done` 而非尝试解析 JSON。
- **R11** THE SYSTEM SHALL 忽略以 `:` 开头的注释行（SSE 心跳），但计数。
- **R12** THE SYSTEM SHALL 支持一帧内多个 `data:` 行（SSE 规范允许），按规范用 `\n` 连接。
- **R13** THE SYSTEM SHALL NOT 对帧内容做 UTF-8 校验（`docs/05` §5.4）。
- **R14** THE SYSTEM SHALL 对任意字节输入不 panic（宪法 C7）。
- **R15** 对同一输入的任意切分方式（含逐字节），THE SYSTEM SHALL 产出完全相同的帧序列（宪法 C24）。

## 4. 接口契约

```rust
// argus-protocol/src/framer/mod.rs
pub trait Framer {
    fn push(&mut self, chunk: &TeeChunk, out: &mut dyn FrameSink) -> Result<(), FramerError>;
    /// 上游 EOF：处理残留缓冲（可能有最后一个未以分隔符结尾的帧）
    fn finish(&mut self, out: &mut dyn FrameSink) -> Result<(), FramerError>;
    fn buffered_bytes(&self) -> usize;
}

pub trait FrameSink {
    fn emit(&mut self, frame: FrameRef<'_>);
    fn emit_gap(&mut self, dropped_bytes: u64);
}

pub enum FrameRef<'a> {
    /// 普通 SSE 帧
    Event { event: Option<&'a [u8]>, data: FrameData<'a>, raw: &'a Bytes },
    /// data: [DONE]
    Done,
    /// 注释/心跳
    Comment,
}

/// 单行 data 时借用；多行 data 时才拼接（罕见）
pub enum FrameData<'a> { Borrowed(&'a [u8]), Joined(Vec<u8>) }

#[derive(Debug, thiserror::Error)]
pub enum FramerError {
    #[error("frame exceeds max_frame_bytes ({0})")]        Overflow(usize),
    #[error("resync failed after {0} bytes")]              ResyncFailed(u64),
}

pub struct SseFramer { /* buf: BytesMut, state: State, cfg: FramerConfig */ }

pub struct FramerConfig { pub max_frame_bytes: usize }   // 默认 1 MiB
```

## 5. 实现要点

- **R3 是已知易错点**：`docs/05` §5.3 里给的示意实现有 bug（三次独立 `find` 会返回"最早的 `\n\n`"而不是"最早的任意分隔符"）。必须一次扫描同时匹配三种，取 `min`。用 `memchr::memmem::Finder` 预构建三个 finder，或直接手写一次 `memchr2(b'\n', b'\r')` 后判断后续字节。
- fast path 的判据是 `self.buf.is_empty()`，不是"chunk 里有分隔符"——后者仍需处理残留。
- `Gap` 通过 `FrameSink::emit_gap` 单独发出，**不混进 `FrameRef`**——它不是 provider 产生的帧。
- `finish()` 必须处理"上游 EOF 但最后一帧没有尾随空行"的情况（真实 provider 会这样，`docs/04` 坑 C8）。
- `FrameData::Joined` 只在多行 `data:` 时出现，实践中 AI provider 不用，但必须容忍。

## 6. 任务

- [ ] T1 `framer/mod.rs`：`Framer` / `FrameSink` / `FrameRef` / `FramerError` 定义
- [ ] T2 `framer/sse.rs`：一次扫描的三分隔符查找（注意 §5 的 bug）→ `find_frame_end`
- [ ] T3 fast path（`buf` 空 + `split_to`）
- [ ] T4 slow path（跨 chunk 拼接 + 上限检查）
- [ ] T5 帧内字段解析：`event:` / `data:` / `:` 注释 / 多行 data / `[DONE]`
- [ ] T6 gap 处理与 `Resync` 状态机
- [ ] T7 `finish()`：EOF 时的残留帧
- [ ] T8 `SplitStrategy` 测试工具（本 spec 产出，F10 复用）→ `argus-protocol/tests/split.rs`
- [ ] T9 property test：任意字节不 panic、缓冲有界
- [ ] T10 单元测试：§7 的验收清单逐条

## 7. 验收

```bash
cargo test -p argus-protocol framer
cargo test -p argus-protocol --test split
```

**必测清单**（`docs/16` §6.1 的验收细节）：

- [ ] `\n\n` / `\r\n\r\n` / `\r\r`，以及**混合出现时取最早**（这是 §5 指出的 bug 点，必须有专门用例）
- [ ] fast path 不发生 memcpy —— 断言产出的 `Bytes` 与输入共享底层（`Bytes::as_ptr` 落在输入范围内）
- [ ] slow path 只拷贝残留
- [ ] `gap_before > 0` → 发出 `emit_gap` + 进入 Resync + 下一个完整帧被正确产出
- [ ] Resync 超 `max_frame_bytes` → `ResyncFailed`，不 panic
- [ ] 缓冲超限 → `Overflow`，不 panic
- [ ] `data: [DONE]` → `FrameRef::Done`
- [ ] `: ping` 注释行被忽略且计数
- [ ] 多行 `data:` 用 `\n` 连接
- [ ] 空 `data:` 行
- [ ] EOF 时无尾随空行的最后一帧
- [ ] **`SplitStrategy` 全矩阵（≥ 30 种，含 `ByteByByte`）结果一致**（宪法 C24）
- [ ] proptest：10 万随机字节不 panic、`buffered_bytes() <= max_frame_bytes`

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 解析 `data:` 里的 JSON | F05（Anthropic）、F08（OpenAI） |
| `EventStreamFramer`（Bedrock 二进制帧） | Phase 2（trait 已留位） |
| golden fixture 目录与 `insta` snapshot | F10 |
| 把 `SplitStrategy` 接到全部 fixture 上 | F10 |
