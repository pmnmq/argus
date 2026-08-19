# 04 · 协议层

> 状态：Draft v1 · 最后更新 2026-08-19
> 本文定义 Argus 认识的线上协议、endpoint 映射、header 变换规则，以及「Protocol 与 Provider 解耦」的具体含义。
> SSE 字节层处理见 `05-streaming.md`；事件语义映射见 `06-observation-model.md`。

---

## 1. Problem

「一个 Provider = 一套协议」是错误的模型。现实是：

- 同一个协议（OpenAI Chat Completions）被十几家供应商实现：OpenAI、Azure、DeepSeek、Qwen、Together、Groq、vLLM、Ollama……
- 同一个供应商提供多个协议：OpenAI 同时有 Chat Completions 和 Responses；Anthropic 的模型同时可通过 Anthropic 原生 API、Bedrock、Vertex 访问，而这三者的**线上格式并不相同**。
- 同一个模型在不同 provider 上有不同的模型 ID、不同的能力子集、不同的 header 要求。

如果把这三个维度耦合成一个枚举，配置模型会立刻崩塌。

---

## 2. Goals

- G-P1 `Protocol` 与 `ProviderKind` 是两个正交维度。
- G-P2 新增一个 OpenAI-compatible 供应商**不需要写代码**，只需配置。
- G-P3 provider 新增未知字段 / 未知 SSE event 时，Argus 原样透传且不报错。
- G-P4 header 变换规则是数据驱动的表，不是散落在代码里的 if-else。

## 3. Non-goals

- 不做跨协议转换（见 §7）。
- 不校验请求体的业务字段（不做 JSON Schema 校验，见 §8）。
- MVP 不实现 Gemini `generateContent` 与 Bedrock 原生 eventstream（见 §9）。

---

## 4. 两个正交维度

### 4.1 Protocol

`Protocol` 描述**线上字节的形状**：请求 JSON 结构、响应 JSON 结构、流式帧格式。

```rust
#[non_exhaustive]
pub enum Protocol {
    OpenAiChat,              // POST /v1/chat/completions
    OpenAiResponses,         // POST /v1/responses
    AnthropicMessages,       // POST /v1/messages
    GeminiGenerateContent,   // 预留，MVP 不实现
    BedrockInvoke,           // 预留，非 SSE（见 §9.2）
}
```

`Protocol` 决定：
- Argus 用哪个 `ProtocolParser`
- Argus 用哪个 `ErrorRenderer`
- 流式帧是不是 SSE

### 4.2 ProviderKind

`ProviderKind` 描述**如何连接与鉴权**：URL 构造、认证 header、必需的额外 header、错误码方言。

```rust
#[non_exhaustive]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    AzureOpenAi,
    Bedrock,
    Vertex,
    /// 任何声称兼容 OpenAI Chat 的自建/第三方服务
    OpenAiCompatible,
    /// 任何声称兼容 Anthropic Messages 的服务
    AnthropicCompatible,
}
```

### 4.3 组合矩阵

| ProviderKind | 支持的 Protocol | 认证 | 备注 |
|---|---|---|---|
| `OpenAi` | `OpenAiChat`, `OpenAiResponses` | `Authorization: Bearer` | |
| `AzureOpenAi` | `OpenAiChat`, `OpenAiResponses` | `api-key: <k>` | URL 含 deployment + `api-version` |
| `OpenAiCompatible` | `OpenAiChat`（`OpenAiResponses` 需显式声明） | `Authorization: Bearer`（可配置 header 名） | DeepSeek / Qwen / vLLM / Ollama / Groq… |
| `Anthropic` | `AnthropicMessages` | `x-api-key: <k>` + `anthropic-version` | |
| `AnthropicCompatible` | `AnthropicMessages` | 可配置 | |
| `Bedrock` | `AnthropicMessages`（经 `invoke-with-response-stream`） | SigV4 | 流式**不是 SSE**，见 §9.2 |
| `Vertex` | `AnthropicMessages`（`:streamRawPredict`）、`GeminiGenerateContent` | OAuth2 Bearer | |

> 这张表就是 G-P1 的具体体现：`AnthropicMessages` 这一个协议横跨 4 种 provider。

---

## 5. 三种协议的线上要点

以下是 Argus 必须正确处理的协议细节。**这些不是完整 API 文档，只列出对代理与解析有影响的部分。**

### 5.1 OpenAI Chat Completions

**请求**：`POST /v1/chat/completions`，关键字段 `model`、`messages`、`stream`、`stream_options.include_usage`、`tools`、`tool_choice`。

**非流式响应**：`{"id","object":"chat.completion","choices":[{"index","message","finish_reason"}],"usage"}`

**流式响应**（SSE，**无 `event:` 行，只有 `data:`**）：

```text
data: {"id":"chatcmpl-x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}

data: {"id":"chatcmpl-x","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}

data: {"id":"chatcmpl-x","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chatcmpl-x","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}

data: [DONE]
```

对解析器有影响的坑（每一条都有对应的 golden test，见 `15-testing-strategy.md`）：

| # | 坑 | 处理 |
|---|---|---|
| C1 | `data: [DONE]` 不是 JSON | framer 层特判，产出 `StreamDone` |
| C2 | tool_calls 是数组，用 `index` 标识，`id`/`name` 只在首个 chunk 出现 | 以 `index` 为 key，`id`/`name` 用 first-non-empty |
| C3 | 部分 provider 会**分片发送 `function.name`** | `name` 必须 append 而非覆盖 |
| C4 | usage 在 `choices:[]` 的最后一个 chunk（需 `include_usage`） | 独立处理 `choices` 为空的 chunk |
| C5 | `finish_reason` 可能与最后一个 content delta 在同一 chunk | 一个 chunk 可产出多个 `ObservedEvent` |
| C6 | DeepSeek 等用 `delta.reasoning_content` 承载推理 | 映射为 `BlockKind::Reasoning`（provider 方言，见 §6.3） |
| C7 | `n > 1` 时有多个 `choices`，各有独立 `index` | 每个 choice 是独立的 block 命名空间，见 `06` §5.1 |
| C8 | 某些 provider 在流末尾不发 `[DONE]` | 上游 EOF 也视为正常结束，不报错 |

### 5.2 OpenAI Responses

**请求**：`POST /v1/responses`，字段 `model`、`input`、`stream`、`tools`、`reasoning`、`previous_response_id`、`store`。

**流式响应**：SSE，**同时有 `event:` 和 `data:` 行**，事件名是层级化的：

```text
event: response.created
data: {"type":"response.created","response":{...}}

event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x",...}}

event: response.content_part.added
data: {"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"Hello"}

event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{...}}

event: response.completed
data: {"type":"response.completed","response":{...,"usage":{...}}}
```

要点：

| # | 要点 | 处理 |
|---|---|---|
| R1 | `type` 字段与 `event:` 行冗余但可能不一致 | **以 `data` 里的 `type` 为准**，`event:` 行只作 fallback |
| R2 | 坐标是二维：`output_index` + `content_index` | 映射为 `BlockIndex::TwoLevel`，见 `06` §5.2 |
| R3 | 函数调用参数走 `response.function_call_arguments.delta` | 与 text delta 分开处理 |
| R4 | 推理摘要走 `response.reasoning_summary_text.delta` | `BlockKind::Reasoning { summary: true }` |
| R5 | `response.completed` 里含**完整最终响应**（含 usage） | 作为 Accumulator 的权威校验源，见 `06` §7.4 |
| R6 | 事件类型是开放集合，OpenAI 持续新增 | 未知 `response.*` → `ObservedEventKind::Unknown`，原样透传 |
| R7 | 失败态是 `response.failed` / `response.incomplete` | 映射到 `FinishReason::Error` / `Incomplete` |

### 5.3 Anthropic Messages

**请求**：`POST /v1/messages`，字段 `model`、`messages`、`system`、`max_tokens`（**必填**）、`stream`、`tools`、`thinking`。

**必需 header**：`anthropic-version: 2023-06-01`（缺失会被 Anthropic 拒绝）。

**流式响应**：SSE，`event:` + `data:` 双行：

```text
event: message_start
data: {"type":"message_start","message":{"id":"msg_x","model":"...","usage":{"input_tokens":25,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}

event: message_stop
data: {"type":"message_stop"}
```

要点：

| # | 要点 | 处理 |
|---|---|---|
| A1 | `ping` 事件可随时出现 | 忽略（但计数），不影响状态机 |
| A2 | delta 有多种 type：`text_delta` / `input_json_delta` / `thinking_delta` / `signature_delta` / `citations_delta` | 分别映射，见 `06` §4.3 |
| A3 | `input_json_delta.partial_json` 是**字符串片段**，拼接后才是 JSON | 只 buffer，`content_block_stop` 才 parse。见 `06` §7.2 |
| A4 | 无参数工具的 `partial_json` 拼接结果可能是**空字符串** | 空字符串 → `{}`，不是 parse 错误 |
| A5 | usage 分两处：`message_start` 给 input，`message_delta` 给 output | 合并策略：last-non-null wins（见 `06` §6） |
| A6 | `cache_creation_input_tokens` / `cache_read_input_tokens` 也在 usage | 必须保留，影响成本计算 |
| A7 | `thinking` block 有 `signature`，不可篡改 | 原样保存，脱敏时特殊处理（见 `08` §9） |
| A8 | `error` 事件可在流中任意位置出现 | 映射为 `ObservedEventKind::Error`，仍然透传 |
| A9 | 未来会有新 event type | 未知 event → `Unknown`，透传 + 记录 |
| A10 | `stop_reason` 取值集合会扩展（如 `refusal`、`model_context_window_exceeded`） | `FinishReason` 保留 `Other(CompactString)` 变体 |

---

## 6. Endpoint 映射与 URL 构造

### 6.1 入站路由表

| Method | Path | Protocol | 是否流式可能 |
|---|---|---|---|
| POST | `/v1/chat/completions` | `OpenAiChat` | ✅ |
| POST | `/v1/responses` | `OpenAiResponses` | ✅ |
| POST | `/v1/messages` | `AnthropicMessages` | ✅ |
| POST | `/v1/messages/count_tokens` | `AnthropicMessages` | ❌ |
| GET | `/v1/models` | — | ❌ |
| GET | `/v1/models/{id}` | — | ❌ |

### 6.2 出站 URL 构造

由 `ProviderKind` + `Protocol` + endpoint 配置共同决定：

| ProviderKind | Protocol | URL 模板 |
|---|---|---|
| `OpenAi` / `OpenAiCompatible` | `OpenAiChat` | `{base}/v1/chat/completions` |
| `OpenAi` | `OpenAiResponses` | `{base}/v1/responses` |
| `AzureOpenAi` | `OpenAiChat` | `{base}/openai/deployments/{deployment}/chat/completions?api-version={ver}` |
| `AzureOpenAi` | `OpenAiResponses` | `{base}/openai/v1/responses?api-version={ver}` |
| `Anthropic` / `AnthropicCompatible` | `AnthropicMessages` | `{base}/v1/messages` |
| `Vertex` | `AnthropicMessages` | `{base}/v1/projects/{p}/locations/{l}/publishers/anthropic/models/{m}:streamRawPredict` |
| `Bedrock` | `AnthropicMessages` | `{base}/model/{modelId}/invoke-with-response-stream` |

Azure 的 `{deployment}` 来自 `provider_models.deployment_name`——这正是「同一逻辑模型在不同 provider 上有不同标识」的例子。

### 6.3 `/v1/models` 的特殊性

`/v1/models` 是**唯一由 Argus 合成响应**的 endpoint。它不转发到任何 provider，而是把 Control Plane 的逻辑模型列表投影成调用方期望的格式。

这不违反透明代理原则，因为：**它返回的本来就是 Argus 自己的资源清单**，不是任何单一 provider 的清单。转发给某个 provider 反而是错的（客户端会看到自己无权访问的模型）。

响应格式按 `Accept` 与调用路径推断，默认 OpenAI 形状：

```jsonc
// GET /v1/models
{ "object": "list", "data": [
  { "id": "claude-sonnet", "object": "model", "created": 0, "owned_by": "argus",
    // Argus 扩展，放在 x- 前缀下，不污染标准字段
    "x-argus": { "protocol": "anthropic_messages",
                 "capabilities": { "streaming": true, "tools": true, "vision": true,
                                   "reasoning": true, "context_length": 200000 },
                 "providers": ["anthropic-prod", "bedrock-us-east-1"] } }
]}
```

**过滤**：只返回当前 API Key 的 `allowed_models` 交集。这让客户端的模型选择器天然只显示有权访问的模型。

### 6.4 Provider 方言（dialect）

某些 OpenAI-compatible provider 有小的偏差。用**声明式方言配置**而非代码分支处理：

```toml
[[providers]]
name = "deepseek"
kind = "openai_compatible"
base_url = "https://api.deepseek.com"

[providers.dialect]
# 推理内容字段名（OpenAI 官方无此字段）
reasoning_field = "reasoning_content"
# 是否支持 stream_options.include_usage
supports_stream_usage = true
# 是否会发送 data: [DONE]
emits_done_sentinel = true
# 认证 header 名（少数自建服务用别的）
auth_header = "Authorization"
auth_scheme = "Bearer"
```

方言**只影响 ProtocolParser 的观测行为，绝不改写转发的字节**。这一点必须严格遵守——方言配置错了顶多是观测数据不准，不会破坏代理。

---

## 7. 跨协议调用：明确不做

### 7.1 决定

若客户端用 `Protocol::AnthropicMessages` 调用一个只提供 `Protocol::OpenAiChat` 的 provider，Argus 返回 **400**，不做隐式转换：

```json
{"type":"error","error":{
  "type":"invalid_request_error",
  "message":"model 'gpt-4o' is bound to provider protocol 'openai_chat' and cannot be served over the Anthropic Messages API. Use /v1/chat/completions instead, or bind an anthropic_messages-capable provider."
}}
```

Control Plane 在配置时就会拒绝这种绑定（见 `03-control-plane.md` §5.2），所以运行时遇到它意味着配置漂移。

### 7.2 为什么不做

跨协议转换看起来只是字段搬运，实际上是有损的：

| 语义 | Anthropic | OpenAI Chat | 转换损失 |
|------|-----------|-------------|---------|
| system prompt | 顶层 `system` 字段，可以是 block 数组、可带 cache_control | `messages[0].role="system"` | cache_control 丢失 |
| thinking | 独立 content block + `signature` | 无对应物 | 完全丢失，且 signature 丢失后无法回传给 Anthropic 继续对话 |
| tool_result | `messages[].content[]` 中的 block | 独立的 `role: "tool"` 消息 | 顺序与并行关系需要重建，易错 |
| prompt caching | `cache_control` 断点 | `prompt_cache_key`（语义完全不同） | 无法映射，且会显著改变成本 |
| 图片 | `source.type: base64/url` | `image_url.url`（可为 data URI） | 可映射但细节多 |
| `max_tokens` | 必填 | 可选 | 需要臆造默认值 |
| stop_reason | `end_turn`/`tool_use`/`max_tokens`/`refusal`/… | `stop`/`tool_calls`/`length`/`content_filter` | 非双射 |

一次转换丢失 cache_control，用户的成本可能翻数倍且毫无提示。**这类静默损失正是 Argus 要避免的东西。**

### 7.3 将来如果要做

作为**独立的、显式开启的** compatibility layer：

```text
POST /compat/anthropic/v1/messages   →  显式的兼容端点，与 /v1/messages 不同 path
```

- 走独立 path，让用户明确知道自己在用转换层。
- 转换层产出 `ObservedInteraction` 时标注 `translated_from: Protocol`，审计里可见。
- 有独立的 golden test 套件覆盖损失矩阵。
- 不复用 `/v1/*` 的透明路径，两者代码不交叉。

---

## 8. 请求体校验：只做最小必要

Argus **不做** JSON Schema 校验。理由：provider 的 schema 每周都在变，Argus 的 schema 必然滞后，滞后的校验会**拒绝合法请求**——这是最糟糕的失败模式。

只做三项最小解析（用 `serde_json` 的部分反序列化，不构造完整 `Value`）：

```rust
#[derive(Deserialize)]
struct RequestProbe<'a> {
    #[serde(borrow)] model: Option<&'a str>,   // 必需：选路
    stream: Option<bool>,                       // 必需：决定转发模式
    // 其他字段一律忽略
}
```

- `model` 缺失 → 400（无法选路）。
- `stream` 缺失 → 按协议默认（三者都默认 false）。
- 其他一切交给 provider 判断。provider 返回的 400 原样透传给客户端，**包括 provider 的原始错误信息**——这比 Argus 自己编一个更有用。

> 例外：PreflightGuard 需要更深的解析（提取 system/user message、tool 定义）。它使用**独立的、容错的**提取器（见 `08-security.md` §5），提取失败时降级为「只检查 raw 文本」，绝不因此拒绝请求。

---

## 9. 预留协议

### 9.1 Gemini `generateContent`

- URL：`{base}/v1beta/models/{model}:streamGenerateContent?alt=sse`
- 请求形状与 OpenAI/Anthropic 差异大（`contents[].parts[]`、`systemInstruction`、`functionDeclarations`）。
- SSE 只有 `data:` 行，每个 chunk 是完整的 `GenerateContentResponse`（**不是增量 delta，是累积快照的分片**）。
- 结论：需要独立的 `GeminiParser`，语义映射不平凡。排在 MVP 之后。

### 9.2 Bedrock：流式不是 SSE

这是一个必须提前记录的架构风险。

Bedrock 的 `invoke-with-response-stream` 返回 `application/vnd.amazon.eventstream`——**AWS 私有的二进制帧格式**（prelude + headers + payload + CRC32），不是 SSE。

影响：
- `SseFramer` 不适用，需要一个 `EventStreamFramer`。
- 但**上层的 `ProtocolParser` 可以复用**：解出的 payload 是标准的 Anthropic 事件 JSON。

因此 `argus-protocol` 的接口必须把「分帧」与「解析」分开：

```rust
pub trait Framer {
    /// 喂入字节，产出零个或多个逻辑帧
    fn push(&mut self, input: &TeeChunk, out: &mut FrameSink) -> Result<(), FramerError>;
}
pub struct SseFramer { .. }
pub struct EventStreamFramer { .. }   // Bedrock，Phase 3

pub trait ProtocolParser {
    fn on_frame(&mut self, frame: Frame<'_>, out: &mut EventSink) -> Result<(), ParseError>;
}
```

这个分层从 MVP 第一天就要有，否则 Bedrock 支持会需要重构。**这是本文最重要的前瞻性约束。**

另外，Bedrock 的透明代理还有一个更麻烦的问题：客户端用 Anthropic SDK 发来的是 SSE 期望，而 Bedrock 返回二进制 eventstream。这意味着 `AnthropicMessages` over `Bedrock` **不可能是纯字节透传**，必须重新分帧。这属于「帧格式转换」而非「协议语义转换」，是可接受的（语义无损），但要在文档和实现中标记为**特殊路径**：

```text
Anthropic client → Argus → Bedrock
  字节层：SSE ← 重新分帧 ← eventstream      （有损：帧边界改变）
  语义层：完全无损（event JSON 逐字保留）
```

---

## 10. Data Model

```rust
pub struct ProviderConfig {
    pub id: ProviderId,
    pub name: CompactString,
    pub kind: ProviderKind,
    pub endpoints: Vec<Arc<ProviderEndpoint>>,
    pub credential: Arc<ResolvedCredential>,
    pub dialect: Arc<Dialect>,
    pub supported_protocols: EnumSet<Protocol>,
    pub health: Arc<HealthState>,
}

pub struct ProviderEndpoint {
    pub id: EndpointId,
    pub base_url: Url,
    pub region: Option<CompactString>,
    pub weight: u16,
    pub priority: u8,
    pub timeouts: Timeouts,
    pub extra_headers: Arc<HeaderMap>,   // 预构造，避免每请求分配
}

pub struct Dialect {
    pub reasoning_field: Option<CompactString>,
    pub supports_stream_usage: bool,
    pub emits_done_sentinel: bool,
    pub auth_header: HeaderName,
    pub auth_scheme: Option<CompactString>,
    pub required_headers: Arc<HeaderMap>,     // 如 anthropic-version
}
```

---

## 11. Failure Modes

| 场景 | 行为 |
|------|------|
| 未知 endpoint path | 404，OpenAI 形状错误（默认） |
| `model` 字段缺失 | 400，按调用协议形状 |
| 逻辑模型的绑定 protocol ≠ 调用 protocol | 400 + 明确指引（§7.1） |
| provider 返回未知 SSE event | 透传 + `ObservedEventKind::Unknown` + `argus_unknown_event_total{protocol,event}` 指标 |
| provider 返回未知 JSON 字段 | 透传（我们从不重新序列化），Accumulator 存入 `extensions` |
| 方言配置错误（如 `reasoning_field` 写错） | 观测数据缺失该字段，**代理不受影响** |
| Anthropic 缺 `anthropic-version` header | 由 `Dialect.required_headers` 强制注入，不依赖客户端 |
| Azure 缺 `api-version` | 配置校验期拒绝保存该 endpoint |

---

## 12. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| Protocol / ProviderKind 正交 | 新 OpenAI-compatible provider 零代码 | 配置维度增加，需要 §5.2 的语义校验兜住错误组合 |
| 不做请求体校验 | 永不误拒合法请求；provider 新字段立刻可用 | 错误请求要多走一次上游 RTT 才报错 |
| 不做跨协议转换 | 零静默语义损失 | 用户不能用 Anthropic SDK 调 GPT |
| 方言用配置而非代码 | 新方言无需发版 | 方言表达能力有限，怪异 provider 仍需代码 |
| Framer / Parser 分层 | Bedrock 可复用 parser | 多一层间接，MVP 用不上 |
| `/v1/models` 合成 | 客户端只看到有权模型 | 与 provider 的真实模型列表可能不同步 |

---

## 13. Alternatives Considered

**A. 用一个大枚举 `Provider { OpenAI, Anthropic, ... }` 同时表达协议与供应商。**
无法表达「Anthropic 协议跑在 Bedrock 上」。**否决**——这正是本文要解决的问题。

**B. 对请求体做 JSON Schema 校验以提前拒绝错误请求。**
Schema 必然滞后于 provider，会拒绝合法请求。**否决**（见 §8）。

**C. `/v1/models` 转发到默认 provider。**
客户端会看到自己无权访问的模型，且多 provider 时选哪个是任意的。**否决。**

**D. 用 `event:` 行作为 Responses API 的事件类型权威来源。**
`data` 里的 `type` 更可靠（非流式响应也有它，便于统一处理），且 SSE 的 `event:` 行在某些代理下可能被处理。**采用 `data.type` 优先。**

**E. 把方言做成 trait + 动态分发。**
过度设计。95% 的方言差异是「字段名不同」「是否发某个 sentinel」，配置足够。**否决**，保留将来加 trait 的余地。

---

## 14. Decision

1. `Protocol` 与 `ProviderKind` 正交，组合合法性由 §4.3 矩阵与 Control Plane 校验保证。
2. MVP 实现 `OpenAiChat`、`OpenAiResponses`、`AnthropicMessages` 三个 Protocol。
3. 请求体只探测 `model` 与 `stream`，其余不校验、不重新序列化。
4. 未知 header / 未知字段 / 未知 SSE event 一律透传，未知 event 额外计数。
5. 不做跨协议转换；配置期拒绝，运行期 400。
6. `/v1/models` 由 Argus 合成，按 Key 权限过滤，扩展信息放 `x-argus` 命名空间。
7. Provider 方言用声明式配置，且**只影响观测，不影响转发字节**。
8. `Framer` 与 `ProtocolParser` 从第一天起就是分离的 trait，为 Bedrock eventstream 预留。

---

## 15. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-1 | `AnthropicMessages` over `Bedrock` 的重新分帧是否应该在 MVP 就支持？ | 有真实 Bedrock 用户时 |
| OQ-2 | 是否要支持 `/v1/completions`（legacy）与 `/v1/embeddings`？ | 用户需求驱动；embeddings 很容易加（非流式） |
| OQ-3 | Responses API 的 `store=true` + `previous_response_id` 会让会话状态存在 OpenAI 侧，Argus 的 conversation trace 如何关联？ | 见 `06-observation-model.md` OQ-3 |
| OQ-4 | 是否为 `OpenAiCompatible` 提供自动方言探测（首次请求时嗅探能力）？ | Phase 3 评估 |
