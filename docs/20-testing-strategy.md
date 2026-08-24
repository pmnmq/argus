# 20 · 测试策略

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`09-observers.md`（协议坑）、`11-observation-model.md`（映射表）

---

## 1. Problem

Argus 的正确性风险集中在两个地方：

1. **协议兼容性**：三种协议、几十个事件类型、大量边界情况（分片、乱序、未知事件、中断）。写错一处，用户就会遇到"用官方 SDK 直连正常，走 Argus 就报错"——这是最伤信任的失败模式。
2. **并发与背压**：Tee 的丢弃逻辑、取消传播、优雅停机。这些 bug 只在压力下出现，很难用单元测试覆盖。

同时有一个硬约束：**测试不能依赖真实的 provider API**。原因：花钱、有速率限制、不确定（模型输出随机）、CI 网络受限、且 provider 故障会让 CI 红。

---

## 2. Goals

- G-T1 协议兼容性由录制的真实字节驱动，不依赖网络。
- G-T2 解析器对任意字节切分方式产生完全相同的结果（`10-streaming.md` G-S5 的验证）。
- G-T3 架构不变式（依赖方向、无界 channel、panic 策略）由 CI 自动检查。
- G-T4 官方 SDK 作为端到端 oracle，但不是 CI 的必需依赖。
- G-T5 新增 provider / 新增协议事件的测试成本低（加一个 fixture 目录即可）。

## 3. Non-goals

- 不追求 100% 行覆盖率（追求关键路径的分支覆盖）。
- 不做模型输出质量的测试。
- 不在 CI 中调用真实的付费 API（有单独的、手动触发的 contract test）。

---

## 4. 测试金字塔

```text
                    ┌──────────────────┐
                    │  ⑥ Contract      │  手动 / 每周，调真实 API
                    │  （真实 provider）│  ~10 个用例
                    ├──────────────────┤
                    │  ⑤ E2E           │  真客户端 SDK → Argus → Mock Provider
                    │                  │  ~30 个用例
                    ├──────────────────┤
                    │  ④ Integration   │  Argus + testcontainers PG
                    │                  │  ~80 个用例
                    ├──────────────────┤
                    │  ③ Property/Fuzz │  proptest + cargo-fuzz
                    │                  │  持续运行
                    ├──────────────────┤
                    │  ② Golden        │  录制字节 → snapshot  ★ 核心
                    │                  │  ~300 个用例
                    ├──────────────────┤
                    │  ① Unit          │  纯函数
                    │                  │  ~500 个用例
                    └──────────────────┘
```

**② Golden 是重心**。它同时提供最高的正确性保障与最低的编写成本。

### 4.1 测试命名约定

统一格式：

```
[被测方法名]_[前置条件/测试场景]_[期望结果]
```

全小写 snake_case，三段之间用 `_` 连接，段内可以有多个词。

```rust
// ✅
fn is_retryable_stage_is_s3_committed_returns_false()
fn header_value_any_credential_marks_the_value_sensitive()
fn http_status_upstream_status_is_not_representable_returns_502()
fn supports_protocol_is_bedrock_invoke_returns_false_for_every_kind()

// ❌
fn test_retry()                      // 什么都没说
fn s3_not_retryable()                // 缺被测方法名
fn works()                           // 失败时看名字不知道坏了什么
fn debug_and_display_are_redacted()  // 两个方法混在一起，见 4.1.2
```

#### 4.1.1 为什么必须写出被测方法名

CI 失败时人看到的往往只有一行测试名。名字里带着方法名就能直接定位到是哪个
函数回归了，不用先翻测试体。golden test 有几百个用例时这是刚需。

#### 4.1.2 一个测试只测一个方法

如果「期望结果」那一段写不成一句话，说明这个测试在做两件事，应该拆开。

```rust
// ❌ 断言了两个不同方法，期望结果写不出来
fn debug_and_display_are_redacted()

// ✅
fn debug_credential_holding_a_secret_renders_redacted()
fn display_credential_holding_a_secret_renders_only_redacted()
```

同一个方法在一组输入上验证同一个期望，不算两件事，可以合成一个：

```rust
// ✅ 一个方法、一个期望、一组输入
fn is_retryable_upstream_status_is_transient_returns_true()
```

#### 4.1.3 没有前置条件时，中间段可以省

构造函数、默认值这类没有场景可言的，允许两段，但仍要说清期望：

```rust
fn default_observe_mode_no_pressure_returns_full()
fn upstream_stage_ord_follows_the_state_machine_direction()
```

#### 4.1.4 golden test 不适用本节

golden 用例名由 fixture 目录名生成（见 §5.1），目录名另有约定。

---

## 5. 路由与透明性测试（v2 新增）

新的路由模型把风险从"协议解析"扩展到了"URL 解析"与"字节保真"。这两类的
失败方式都是**静默的**，必须有独立测试。

### 5.1 URL 解析矩阵

| 类别 | 用例 |
|---|---|
| Managed 拼接 | base_url 带/不带尾斜杠 × path 带/不带前导斜杠；base_url 自带前缀 |
| 空 path | `/provider/x` → 400；`/provider/x/` → 转发到根 |
| Query 保真 | `?a=1&b=a%2Bb&empty=&flag`（含空值与无值参数） |
| 百分号编码 | `%2F` / `%20` / 中文 / 已编码的 `%25` |
| 双斜杠 | `/provider/x/v1//foo` 不被折叠 |
| Raw 直接形式 | `https` / `http` / 带端口 / IPv6 方括号 |
| Raw 分段形式 | 同上，且与直接形式产出**同一个** target |
| Raw 非法 | 缺 scheme / 未知 scheme / 空 authority → 400，**不猜测补全** |
| Profile 名 | 合法字符集边界；`%6Fpenai` 这类绕过写法必须 400 |

### 5.2 透明性测试（§46 的硬要求）

> 未启用 Credential/Header Rewrite 时，上游收到的 body 必须与客户端发送的
> body **逐字节相同**。

测法：MockProvider 记录收到的**原始字节**，与客户端发出的原始字节
`assert_eq`。**不是**比较反序列化后的 JSON —— 那会掩盖 key 顺序、
数字表示（`1.0` vs `1`）、Unicode 转义形式的差异。

必测：未知 JSON 字段、供应商特有字段、嵌套很深的未知结构、数字精度、
key 顺序、非 UTF-8 字节（多模态附件）、空 body、超大 body。

### 5.3 Header 策略测试

| 断言 | 说明 |
|---|---|
| hop-by-hop 全删 | 含 `Connection` 里**列出**的自定义名 |
| 客户端凭据被删 | `authorization` / `x-api-key` / `api-key` / `x-goog-api-key` |
| Profile 凭据被注入 | 且标记为 sensitive |
| `Host` 被重写 | 为 target 的 authority |
| 其余 header 原样 | 包括未知的 `x-*` |
| Passthrough 模式 | 客户端凭据**不**删，Argus 凭据**不**注入 |

### 5.4 SSRF 测试

| 场景 | 期望 |
|---|---|
| 解析到 `127.0.0.1` / RFC1918 / `169.254.169.254` | 403 `target_denied` |
| 显式 allow 后的同一目标 | 放行 |
| 多值 A 记录含一个内网 IP | **整体**拒绝 |
| DNS rebinding（两次解析结果不同） | 连接时检出，中止 + 计数 |
| 上游返回 3xx（含指向内网的 `Location`） | 不跟随，原样透传给客户端 |
| 拒绝类错误 | 不同原因返回**同一个** 403，body 不含 IP |

### 5.5 前置组件测试（不能只在 `cargo run` 下验证）

`04-raw-proxy-mode.md` §6 指出直接形式会被 nginx 默认折叠。必须有一组
**容器化**集成测试，把 Argus 放到 nginx / Traefik 后面跑同一组 URL 用例：

```text
client → nginx(merge_slashes on)  → argus   直接形式应失败、分段形式应成功
client → nginx(merge_slashes off) → argus   两种都成功
client → traefik                  → argus   两种都成功（待验证）
client → argus 直连                          两种都成功（已实测）
```

这一组**不进默认 CI**（需要 Docker），但必须在发布前跑，结果回填到
`18-deployment.md` §5.9 的表格。

---

## 6. Golden Test 系统（核心）

### 5.1 目录结构

```text
tests/fixtures/
├── openai_chat/
│   ├── 001_simple_stream/
│   │   ├── meta.toml              # 用例元数据
│   │   ├── request.json           # 客户端请求体
│   │   ├── upstream.headers       # provider 返回的 header
│   │   ├── upstream.sse           # provider 返回的原始字节 ★
│   │   ├── expected.events.json   # 期望的 ObservedEvent 序列（snapshot）
│   │   └── expected.interaction.json  # 期望的 ObservedInteraction（snapshot）
│   ├── 002_tool_call_single/
│   ├── 003_tool_calls_parallel/
│   ├── 004_tool_name_fragmented/       ← 坑 C3
│   ├── 005_usage_final_chunk/          ← 坑 C4
│   ├── 006_finish_with_content/        ← 坑 C5
│   ├── 007_reasoning_content_deepseek/ ← 坑 C6
│   ├── 008_n_2_multiple_choices/       ← 坑 C7
│   ├── 009_no_done_sentinel/           ← 坑 C8
│   ├── 010_non_stream/
│   └── ...
├── openai_responses/
│   ├── 001_simple_stream/
│   ├── 002_function_call/
│   ├── 003_reasoning_summary/
│   ├── 004_multi_output_item/
│   ├── 005_completed_cross_validate/   ← 06 §7.4
│   ├── 006_response_incomplete/
│   ├── 007_response_failed/
│   ├── 008_unknown_event_type/         ← R6
│   └── ...
├── anthropic_messages/
│   ├── 001_simple_stream/
│   ├── 002_tool_use/
│   ├── 003_input_json_delta_split/     ← A3，跨 frame 的 JSON
│   ├── 004_tool_use_no_args/           ← A4，空 partial_json
│   ├── 005_thinking/                   ← A7
│   ├── 006_redacted_thinking/
│   ├── 007_signature_delta/
│   ├── 008_citations_delta/
│   ├── 009_ping_interleaved/           ← A1
│   ├── 010_usage_split/                ← A5
│   ├── 011_cache_tokens/               ← A6
│   ├── 012_unknown_event/              ← A9
│   ├── 013_unknown_delta_type/
│   ├── 014_unknown_stop_reason/        ← A10
│   ├── 015_error_mid_stream/           ← A8
│   ├── 016_parallel_tool_use/
│   ├── 017_non_stream/
│   └── ...
└── _abnormal/                          # 跨协议的异常情况
    ├── 001_truncated_mid_json/         # 流在 tool args 中间断开
    ├── 002_truncated_mid_utf8/         # 断在 UTF-8 字符中间
    ├── 003_gap_injected/               # 模拟 tee 丢字节
    ├── 004_delta_before_start/         # 乱序
    ├── 005_duplicate_block_start/
    ├── 006_orphan_block_stop/
    ├── 007_event_after_finish/
    ├── 008_oversized_frame/            # > max_frame_bytes
    ├── 009_crlf_line_endings/          # \r\n\r\n 分隔符
    ├── 010_mixed_line_endings/         # ← 05 §5.3 的已知易错点
    └── 011_empty_data_lines/
```

### 5.2 `meta.toml`

```toml
name = "anthropic tool_use with split input_json_delta"
protocol = "anthropic_messages"
provider_kind = "anthropic"
stream = true
# 这份字节是怎么来的。取值只有三个，与 §5.5 的三条途径一一对应：
#   recorded   = Argus 自己录的
#   sdk        = 官方 SDK 调真实 API 录的
#   production = 从生产 raw_events 导出并脱敏的
# 必填。宪法 C25 禁止手写 SSE 字节，而"禁止"要能查才有意义 ——
# ci/check-invariants.sh 会拦掉缺这一行或取值不在集合内的 fixture。
source = "sdk"
# 期望的最终状态
expect_outcome = "completed"
expect_integrity = "complete"
expect_finish_reason = "tool_use"
# 该用例覆盖的已知坑（用于覆盖率追踪）
covers = ["A3", "A4"]
# 是否应该产生 anomaly
expect_anomalies = []
```

### 5.3 测试驱动器

```rust
#[rstest]
fn golden(#[files("tests/fixtures/**/meta.toml")] path: PathBuf) {
    let case = Case::load(&path);

    // 对每种切分方式都跑一遍 —— 这是 G-T2 的核心
    for split in SplitStrategy::all() {
        let mut framer = SseFramer::new();
        let mut parser = AnyObserver::for_kind(case.observer, ObserveMode::Full);
        let mut acc    = Accumulator::new(&case.request_meta);
        let mut events = Vec::new();

        for chunk in split.apply(&case.upstream_sse) {
            framer.push(&chunk, &mut |frame| {
                parser.on_frame(frame, &mut |ev| { events.push(ev.clone()); acc.apply(ev); });
            }).unwrap();
        }
        let interaction = acc.finish(case.expected_outcome());

        // 所有切分方式必须产出完全相同的结果
        if split.is_baseline() {
            insta::assert_json_snapshot!(case.name("events"), &events);
            insta::assert_json_snapshot!(case.name("interaction"), &interaction);
        } else {
            assert_eq!(events, baseline_events, "split strategy {split:?} diverged");
        }
    }
}
```

### 5.4 切分策略（G-T2）★

**这是整个测试体系里最有价值的一个技巧。**

```rust
pub enum SplitStrategy {
    /// 整块一次喂入（基线）
    Whole,
    /// 每次 1 字节
    ByteByByte,
    /// 固定大小
    Fixed(usize),          // 7, 13, 64, 512, 4096
    /// 恰好在 SSE 分隔符前后各切一刀（最容易出错的边界）
    AtBoundaries,
    /// 恰好把 `data:` 前缀切开
    SplitFieldPrefix,
    /// 恰好把一个 UTF-8 多字节字符切开
    SplitUtf8,
    /// 恰好把 JSON 字符串的转义序列切开（`\u00` | `e9`）
    SplitEscape,
    /// 用固定种子的伪随机切分（可复现）
    Seeded(u64),           // 20 个不同种子
}
```

**为什么这么重要**：真实的 TCP 分片是任意的。一个只在"完整 frame"下测试过的 parser，在生产中遇到 `data: {"ty` + `pe":"text_delta"...}` 时就会崩。这类 bug 只在特定的网络条件下出现，极难复现和诊断。

`ByteByByte` 是最严格的检验：如果逐字节喂入的结果与整块喂入完全一致，parser 的状态机就是正确的。

### 5.5 Fixture 的获取

**不允许手写 SSE 字节**（手写的必然与真实的不同）。三条获取途径：

```bash
# ① 录制模式：Argus 自己录（开发环境）
ARGUS_RECORD_FIXTURES=tests/fixtures/_recorded argus serve ...
# 每次上游响应的原始字节被完整落盘

# ② 用官方 SDK 录制（见 §8）
python tools/record_fixture.py --provider anthropic --case tool_use

# ③ 从生产的 raw_events 导出（脱敏后）
argus fixtures export --request-id req_01J... --redact --out tests/fixtures/...
```

途径 ③ 特别有价值：生产中遇到的怪异情况可以直接变成回归测试。

**脱敏是必须的**：fixture 会进 Git，必须移除真实的 API key、用户内容、org ID。`--redact` 用与 `12-security.md` §9 相同的规则集。

### 5.6 非流式与流式的交叉验证

对同一个逻辑请求，同时录制 `stream=true` 与 `stream=false` 的响应：

```rust
#[test]
fn stream_and_non_stream_agree() {
    let streaming    = run_golden("anthropic_messages/002_tool_use");
    let non_streaming = run_golden("anthropic_messages/002_tool_use_nonstream");

    // 除时间戳与 raw 外，ObservedInteraction 应完全一致
    assert_eq!(streaming.normalized(), non_streaming.normalized());
}
```

这验证了 `11-observation-model.md` §5.4 的 `SyntheticEventGen` 与流式路径的等价性——两条完全不同的代码路径产出相同结果，是很强的正确性信号。

---

## 7. Property / Fuzz 测试

### 6.1 Framer 的不变式（proptest）

```rust
proptest! {
    /// 任意字节输入下，framer 不 panic 且缓冲有界
    #[test]
    fn framer_never_panics(input in prop::collection::vec(any::<u8>(), 0..100_000)) {
        let mut f = SseFramer::new();
        let _ = f.push(&TeeChunk::from(Bytes::from(input)), &mut |_| {});
        prop_assert!(f.buffered_bytes() <= MAX_FRAME_BYTES);
    }

    /// 切分不变性：任意切分点集合下结果一致
    #[test]
    fn split_invariance(
        data in valid_sse_stream(),
        cuts in prop::collection::vec(0usize..10_000, 0..50),
    ) {
        let whole  = parse_all(&data, &[]);
        let chunked = parse_all(&data, &cuts);
        prop_assert_eq!(whole, chunked);
    }

    /// gap 注入后，parser 能恢复且标记 degraded
    #[test]
    fn gap_recovery(data in valid_sse_stream(), gap_at in 0usize..10_000) {
        let r = parse_with_gap(&data, gap_at);
        prop_assert!(matches!(r.integrity, Integrity::Degraded { .. }));
        // 关键：gap 之后的事件仍能被正确解析
        prop_assert!(r.events_after_gap() > 0 || gap_at > data.len() - 100);
    }
}
```

### 6.2 Accumulator 的不变式

```rust
proptest! {
    /// 任意事件序列（含乱序、重复、孤儿）下不 panic，且必产出 interaction
    #[test]
    fn accumulator_total(events in arbitrary_event_sequence()) {
        let mut acc = Accumulator::new(&meta());
        for e in &events { acc.apply(e); }     // 不 panic
        let it = acc.finish(Outcome::Completed);
        // 累积的字节数不超过 cap
        prop_assert!(it.total_text_bytes() <= MAX_ACCUMULATED_BYTES);
        // 每个 anomaly 都被计数
        prop_assert_eq!(it.stats.anomalies.len(), expected_anomaly_count(&events));
    }

    /// tool arguments 永不被"修复"
    #[test]
    fn never_repairs_json(partial in ".*") {
        let it = run_tool_args_case(&partial);
        match it.tool_calls[0].arguments {
            ToolArguments::Parsed(v) => {
                // 若声称解析成功，则必须能反序列化回等价的输入
                prop_assert!(serde_json::from_str::<Value>(&partial).is_ok());
            }
            ToolArguments::Invalid { raw, .. } => prop_assert_eq!(raw, partial),
            _ => {}
        }
    }
}
```

### 6.3 cargo-fuzz

```bash
cargo fuzz run framer          # 目标：panic / 无界内存 / 死循环
cargo fuzz run parser_anthropic
cargo fuzz run parser_openai_chat
cargo fuzz run parser_responses
```

在 CI 上每次 PR 跑 60 秒，nightly 跑 4 小时。语料库从 fixtures 初始化。

---

## 8. Mock Provider

一个可编程的本地 SSE 服务器，是 E2E 与压测的公共设施。

```rust
pub struct MockProvider {
    pub scenarios: HashMap<String, Scenario>,
}

pub enum Scenario {
    /// 回放一个 fixture
    Replay { fixture: PathBuf, pacing: Pacing },
    /// 慢首 token
    SlowFirstToken { delay: Duration, then: Box<Scenario> },
    /// 中途断开
    TruncateAfter { frames: usize },
    /// 返回错误状态
    HttpError { status: u16, body: &'static str, retry_after: Option<u32> },
    /// 200 但首个 event 是 error（02 §7.4）
    OkThenErrorEvent,
    /// 超大 frame
    OversizedFrame { bytes: usize },
    /// 不发 [DONE]
    NoDoneSentinel,
    /// 每 N 毫秒一个 token（压测用）
    Synthetic { tokens: usize, interval: Duration },
    /// 永不响应（测超时）
    Hang,
    /// 逐帧指定字节与分片边界
    Scripted { frames: Vec<Vec<u8>>, status: u16, pacing: Pacing, truncate: bool },
}

pub enum Pacing {
    /// 按录制时的真实时间间隔回放
    Realtime,
    /// 尽快
    AsFastAsPossible,
    /// 固定间隔
    Fixed(Duration),
}
```

**`Pacing::Realtime` 很重要**：它能重现真实的 TTFT 与 token 间隔，让 E2E 测试的时序有意义。

**`Scripted` 是给字节级断言留的口子**。上面八个场景描述的都是「provider
表现出什么行为」，没有一个能表达「这些字节，按这个边界，分这么多次发」——
而 §5.2 的透明性测试与 `10-streaming.md` §6 的丢弃/`gap_before` 用例要的
恰恰是后者。缺了它，这类测试只能各自手写一个 HTTP 服务器，公共设施就白设了。

配套地，`MockProvider` 有两种传输，**都不能少**：

| 传输 | 用途 | 为什么不能只留一个 |
|---|---|---|
| 明文，手写 HTTP/1.1 字节 | 分片与时序逐字节可控 | 任何 HTTP 库都会替我们重新分片，把要验的东西抹掉 |
| TLS + ALPN | 验 h2 真的协商上了（宪法 C23） | 手写字节做不出 h2 的帧层；而 ALPN 只在 TLS 上存在 |

它落在 `argus-proxy` 的 `testkit` feature 下（非默认，不进 release）。
不单开 crate 是因为宪法 C32 把 crate 集合钉死在五个，而一个测试工具箱
不满足 `19-rust-project-structure.md` §4 的任何一条拆分判据。

---

## 9. 官方 SDK 作为 Oracle（G-T4）

### 8.1 SDK 的三个用途

回答 `十九节 Q2`（官方 SDK 在项目中的角色）：

| 用途 | 说明 | 是否在 CI |
|------|------|----------|
| **① 录制 fixture** | 用官方 SDK 调真实 API，录下原始 HTTP 字节 | ❌ 手动 |
| **② 消费验证** | 官方 SDK 作为客户端调 Argus（后接 Mock Provider），验证 SDK 能正常解析 | ✅ 是 |
| **③ Schema 对照** | 从 SDK 的类型定义（Python 的 pydantic / TS 的 d.ts）核对我们的 IR 映射是否遗漏字段 | ⚠️ 半自动 |

**SDK 绝不进入 Argus 的运行时依赖**（`ADR/ADR-008` 详述）。

### 8.2 用途 ②：SDK 消费验证（最有价值）

```python
# tests/e2e/sdk_compat/test_anthropic.py
import anthropic

def test_streaming_tool_use(argus_url, mock_provider):
    mock_provider.set_scenario("anthropic_messages/002_tool_use", pacing="realtime")
    client = anthropic.Anthropic(base_url=argus_url, api_key=TEST_KEY)

    tool_blocks = []
    with client.messages.stream(model="claude-sonnet", max_tokens=1024,
                                messages=[{"role":"user","content":"weather in SF"}],
                                tools=[WEATHER_TOOL]) as stream:
        for event in stream:
            pass
        final = stream.get_final_message()

    # SDK 自己的累积结果，与 Argus 的 ObservedInteraction 对照
    assert final.stop_reason == "tool_use"
    tool_use = next(b for b in final.content if b.type == "tool_use")
    assert tool_use.input == {"location": "San Francisco"}

    # 关键断言：Argus 观测到的与 SDK 解析出的一致
    observed = fetch_argus_interaction(argus_url, request_id)
    assert observed["tool_calls"][0]["arguments"] == tool_use.input
    assert observed["finish_reason"] == "tool_use"
```

**这个测试同时验证了两件事**：
1. Argus 的转发对 SDK 是透明的（SDK 能正常工作）
2. Argus 的观测结果与 SDK 的解析结果一致（我们的 Accumulator 是对的）

第 2 点是"SDK 作为 oracle"的精髓：官方 SDK 是各家协议的权威实现，用它来交叉验证我们的解析器，比我们自己写断言可信得多。

**CI 中不需要网络**：Mock Provider 在本地，SDK 只连 Argus。

### 8.3 覆盖的 SDK

| SDK | 版本 | 覆盖的协议 |
|-----|------|-----------|
| `openai` (Python) | latest + n-1 | Chat Completions, Responses |
| `anthropic` (Python) | latest + n-1 | Messages |
| `openai` (TypeScript) | latest | Chat Completions |
| `@anthropic-ai/sdk` | latest | Messages |
| `curl` + 手写解析 | — | 验证不依赖 SDK 行为的原始正确性 |

**测 n-1 版本很重要**：用户不会都升到最新。如果 Argus 只在最新 SDK 上工作，会有大量兼容问题。

---

## 10. 架构不变式检查（G-T3）

在 CI 中自动化，防止架构逐渐腐化。

```bash
#!/usr/bin/env bash
# ci/check-invariants.sh
set -euo pipefail

echo "=== 依赖方向 ==="
# argus-proxy 不得依赖 sqlx 或 argus-storage
! cargo tree -p argus-proxy -e normal --prefix none | grep -Eq '^(sqlx|argus-storage) ' \
  || { echo "FAIL: argus-proxy depends on storage"; exit 1; }
# argus-protocol 不得依赖 tokio / hyper / sqlx
! cargo tree -p argus-protocol -e normal --prefix none | grep -Eq '^(tokio|hyper|sqlx|axum) ' \
  || { echo "FAIL: argus-protocol has IO deps"; exit 1; }

echo "=== panic 策略 ==="
grep -q 'panic = "unwind"' Cargo.toml || { echo "FAIL: panic must be unwind"; exit 1; }
! grep -q 'panic = "abort"' Cargo.toml || { echo "FAIL: panic=abort forbidden"; exit 1; }

echo "=== 无界 channel ==="
# 主链路与观测链路禁止无界 channel
! grep -rn 'unbounded_channel\|unbounded()' crates/argus-proxy/src crates/argus-storage/src \
  || { echo "FAIL: unbounded channel found"; exit 1; }

echo "=== 主链路禁止 await 观测/存储 ==="
# 启发式：tee.rs / gate.rs 中不得出现 send().await
! grep -rn '\.send(' crates/argus-proxy/src/tee.rs \
  || { echo "FAIL: blocking send in tee"; exit 1; }

echo "=== 日志不含敏感字段 ==="
python3 ci/check_log_fields.py crates/   # 检查 tracing 宏中的字段名黑名单

echo "=== unsafe ==="
! grep -rn 'unsafe ' crates/*/src --include='*.rs' | grep -v '^.*//' \
  || { echo "FAIL: unsafe code found"; exit 1; }

echo "=== 迁移向后兼容 ==="
ci/check-migration-compat.sh    # 用上一个 release 的二进制跑冒烟测试
```

再加一个更强的检查：**用 `cargo-machete` 找未使用的依赖**，用 `cargo-deny` 检查许可证与已知漏洞。

---

## 11. 集成测试

用 `testcontainers` 起真实的 PostgreSQL（不用 mock DB——SQL 的行为差异是真实的 bug 来源）。

```rust
#[tokio::test]
async fn pg_outage_falls_back_to_spool() {
    let pg = Postgres::start().await;
    let argus = TestArgus::start(&pg).await;
    let mock = MockProvider::start().await;

    // 正常写入
    argus.request(chat_request()).await.assert_ok();
    assert_eq!(argus.count_requests_in_db().await, 1);

    // 杀掉 PG
    pg.stop().await;

    // 关键断言：请求仍然成功
    for _ in 0..10 { argus.request(chat_request()).await.assert_ok(); }
    assert_eq!(argus.diagnostics().await.sink_state, "spooling");
    assert!(argus.diagnostics().await.spool_pending_bytes > 0);

    // 恢复 PG
    pg.start().await;
    argus.wait_for(|d| d.sink_state == "direct", Duration::from_secs(30)).await;

    // 数据全部回放，无丢失、无重复
    assert_eq!(argus.count_requests_in_db().await, 11);
}
```

其他必须覆盖的集成场景：

| 场景 | 断言 |
|------|------|
| PG 挂 + auth stale cache | 已缓存 key 继续工作，新 key 503 |
| provider 429 → fallback | 客户端无感知，`x-argus-fallback-from` 存在 |
| provider 在 S3 断开 | 客户端收到 error event；audit 记录 partial + `UpstreamError` |
| 客户端中途断开 | 上游连接被关闭（mock 侧断言）；audit 记录 `ClientAborted` |
| Tee channel 打满 | interaction 标记 `Degraded`；主链路延迟不变 |
| Observer panic 注入 | 请求正常完成；`argus_observer_panic_total` +1 |
| 优雅停机中有活跃流 | 流正常完成；partial interaction 落库 |
| 配置热更新 | 15s 内新 provider 生效 |
| Key 吊销 | 15s 内生效 |
| 软配额超限 | 429；超额量在容忍范围内 |
| ToolGate 阻断 | 客户端从未收到 tool_use block |
| 大请求 semaphore 满 | 503 而非 OOM |

---

## 12. 性能测试

见 `21-performance.md` §6.4 的方法论。bench 跟着被测的 crate 走 ——
`crates/<crate>/benches/`，这是 cargo 的硬要求，不是风格选择；
端到端的性能用例在 `tests/perf/`。

```rust
// crates/argus-protocol/benches/framer.rs
fn bench_framer(c: &mut Criterion) {
    let data = load_fixture("anthropic_messages/001_simple_stream/upstream.sse");
    let mut g = c.benchmark_group("framer");
    for split in [SplitStrategy::Whole, SplitStrategy::Fixed(64), SplitStrategy::ByteByByte] {
        g.bench_with_input(BenchmarkId::from_parameter(&split), &split, |b, s| {
            b.iter(|| { let mut f = SseFramer::new(); feed(&mut f, &data, s); });
        });
    }
}
```

**回归门禁**：criterion 的结果与 baseline 对比，退化 > 20% 时 CI 失败（用 `critcmp` 或 `bencher`）。

---

## 13. Contract Test（真实 API，非 CI）

```bash
# 手动或每周定时，需要真实的 API key
ARGUS_CONTRACT_TEST=1 \
  OPENAI_API_KEY=... ANTHROPIC_API_KEY=... \
  cargo test --test contract -- --ignored
```

目的：**检测 provider 是否改了 API**。用例数量少（~10 个），每个用最小的 token 消耗（`max_tokens=16`）。

断言不是"输出内容正确"（模型输出随机），而是：
- 没有 `Unknown` 事件（若有，说明 provider 新增了 event type → 需要更新映射）
- `usage` 字段完整
- `finish_reason` 在已知集合内
- 整体结构符合预期

失败时的产出是**一个新的 fixture**：把这次真实响应录下来，加进 golden test，然后更新映射代码。

这形成了一个良性循环：contract test 发现变化 → 变成 fixture → golden test 永久覆盖。

---

## 14. 覆盖率目标

| crate | 行覆盖 | 分支覆盖 | 说明 |
|-------|--------|---------|------|
| `argus-protocol` | > 90% | > 85% | 核心正确性，要求最高 |
| `argus-proxy` | > 75% | > 65% | 并发路径难覆盖 |
| `argus-storage` | > 70% | > 60% | |
| `argus-core` | > 80% | — | 多为类型定义 |
| `argus-server` | > 50% | — | 组装代码 |

用 `cargo-llvm-cov`。**覆盖率是信号不是目标**——`argus-protocol` 的 90% 有意义（它是纯函数，容易测），`argus-server` 强求高覆盖率只会产出无意义的测试。

**特别要求**：`11-observation-model.md` §5 映射表中的**每一行**都必须有对应的 golden case。用 `meta.toml` 的 `covers` 字段追踪：

```bash
cargo test --test coverage_matrix
# 输出：
# anthropic_messages: 14/14 mapping rows covered ✅
# openai_chat:        11/12 mapping rows covered ❌ (missing: delta.refusal)
```

---

## 15. Failure Modes

| 场景 | 处理 |
|------|------|
| snapshot 变化（预期的改动） | `cargo insta review` 人工确认后接受 |
| snapshot 变化（未预期） | CI 失败，diff 清晰可读 |
| fixture 含敏感数据 | pre-commit hook 用安全规则集扫描 fixtures/ |
| contract test 失败（provider 改了 API） | 不阻塞 CI（它不在 CI 中）；产出新 fixture 并开 issue |
| fuzz 发现 panic | 语料自动加入 fixtures/_abnormal/；修复后作为回归测试 |
| 集成测试 flaky | 禁止用 sleep 等待，一律用条件轮询（`wait_for`）+ 明确超时 |
| 性能回归 | criterion 门禁失败，需人工确认是否接受 |

---

## 16. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| Golden test 为核心 | 覆盖真实字节的所有细节；新增用例成本极低 | fixture 需要维护；snapshot 变化要人工 review |
| 切分策略矩阵 | 抓住最难复现的一类 bug | 每个用例跑 N 次（N≈30），测试时间变长 |
| SDK 作为 oracle 但不在运行时 | 权威交叉验证 + 零运行时依赖 | E2E 测试需要 Python/Node 环境 |
| 不在 CI 调真实 API | CI 快、稳定、免费 | 需要单独的 contract test 发现 provider 变更 |
| testcontainers 真 PG | SQL 行为真实 | CI 需要 Docker，测试慢一些 |
| 架构不变式用 grep/cargo tree | 简单可靠 | 是启发式的，可能有漏网 |

---

## 17. Alternatives Considered

**A. 用 VCR/cassette 风格的 HTTP 录制回放库。**
Rust 生态里没有成熟的（`wiremock` 是 mock 不是录制）。且我们需要的是**字节级**回放（含精确的 SSE 分隔符与分片），通用库通常在更高层次工作。**自建**，成本不高。

**B. 只测完整 frame，不做切分矩阵。**
测试快得多。但这正好漏掉了最容易出错也最难诊断的一类 bug。**否决**——这是本策略里性价比最高的部分。

**C. 用真实 API 做 CI。**
慢、贵、flaky、且模型输出随机导致断言困难。**否决**。

**D. 完全不用官方 SDK 测试（只用 curl + 手写断言）。**
会漏掉 SDK 特有的解析行为（比如某个 SDK 严格要求某字段存在）。**否决**——SDK 兼容性正是用户最关心的。

**E. 用 mock 的数据库（如 sqlx 的 mock）。**
SQL 方言、约束、事务语义的差异是真实 bug 来源。**否决**，用 testcontainers。

---

## 18. Decision

1. **Golden test 是测试体系的核心**：录制真实字节 → 解析 → snapshot（`insta`）。
2. **每个 golden case 用 ~30 种切分策略跑一遍**（含逐字节），结果必须完全一致。这是 parser 正确性的主要保障。
3. Fixture 一律来自录制（Argus 自录 / SDK 录 / 生产导出 + 脱敏），**禁止手写**。
4. 官方 SDK 的角色：录制 fixture（手动）、消费验证（CI，接 Mock Provider）、schema 对照。**绝不进入运行时依赖。**
5. `06` 的跨协议映射表中每一行必须有对应 golden case，用 `covers` 字段与 `coverage_matrix` 测试追踪。
6. 架构不变式（依赖方向、无界 channel、panic 策略、日志敏感字段、unsafe）由 CI 脚本强制。
7. 集成测试用 testcontainers 起真实 PG，必须覆盖 PG 故障、fallback、取消、停机、降级等场景。
8. Contract test 独立于 CI，手动/定时运行，目的是发现 provider API 变更，产出物是新 fixture。
9. 性能测试用 criterion + 对照法，退化 > 20% 时 CI 失败。

---

## 19. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | 30 种切分策略 × 300 个 fixture = 9000 次解析，测试时间是否可接受？ | 未测（估计 < 30s，因为解析是 µs 级） | 实现后测量；必要时对慢用例减少策略数 |
| OQ-2 | fixture 进 Git 会让仓库变大（每个几 KB–几十 KB），是否需要 Git LFS？ | 暂不用 | 总量超过 50 MB 时 |
| OQ-3 | 如何自动化「SDK schema 对照」（用途 ③）？ | 半自动 | Phase 2 |
| OQ-4 | 并发/背压的正确性能否用确定性模拟测试（如 `turmoil` / `madsim`）？ | 未评估 | Phase 2，这类工具能大幅提升并发 bug 的可测性 |
| OQ-5 | contract test 的定时运行需要一个存 API key 的 CI secret，安全边界如何设？ | 未定 | CI 搭建时 |
