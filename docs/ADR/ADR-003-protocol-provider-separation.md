# ADR-003 · Protocol 与 Provider 解耦

- **状态**：Accepted
- **日期**：2026-08-19
- **相关**：`04-protocols.md`、`07-provider-routing.md`

---

## Context

多数 AI 网关把「供应商」建模为一个枚举，隐含地假设「一个供应商 = 一套 API 协议」：

```rust
enum Provider { OpenAI, Anthropic, Google, DeepSeek, ... }
// 然后：match provider { OpenAI => parse_openai(), Anthropic => parse_anthropic(), ... }
```

这个模型在现实面前会立刻崩塌：

1. **一个协议，多个供应商**：OpenAI Chat Completions 协议被 OpenAI、Azure、DeepSeek、Qwen、Together、Groq、vLLM、Ollama、以及无数自建服务实现。
2. **一个供应商，多个协议**：OpenAI 同时提供 Chat Completions 与 Responses，二者的线上格式完全不同。
3. **一个模型，多个供应商 + 不同协议形态**：Claude 可以通过 Anthropic 原生 API、AWS Bedrock、Google Vertex 访问。三者的**认证方式、URL 结构、甚至流式帧格式**都不同，但**事件 JSON 的语义是同一套**。
4. **协议相同但方言不同**：DeepSeek 用 `reasoning_content` 承载推理，OpenAI 官方没有这个字段。

用单一枚举表达这些，会导致组合爆炸或错误的抽象。

---

## Decision

**把「协议」与「供应商」建模为两个正交维度。**

```rust
/// 线上字节的形状：请求/响应 JSON 结构、流式帧格式
#[non_exhaustive]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,   // 预留
    BedrockInvoke,           // 预留
}

/// 如何连接与鉴权：URL 构造、认证 header、必需 header、错误方言
#[non_exhaustive]
pub enum ProviderKind {
    OpenAi, AzureOpenAi, OpenAiCompatible,
    Anthropic, AnthropicCompatible,
    Bedrock, Vertex,
}
```

二者的合法组合由 `04-protocols.md` §4.3 的矩阵定义，并在 Control Plane 保存配置时校验。

---

## 各维度的职责

| 维度 | 决定什么 | 不决定什么 |
|------|---------|-----------|
| `Protocol` | 用哪个 `ProtocolParser`；用哪个 `ErrorRenderer`；帧是否为 SSE；`ObservedEvent` 的映射规则 | URL、认证、header |
| `ProviderKind` | URL 模板；认证 header 的形式；必需的额外 header（如 `anthropic-version`）；错误码方言；是否需要 SigV4/OAuth2 | 请求/响应体的形状 |
| `Dialect`（配置，非枚举） | 字段名的小差异（`reasoning_content`）；是否发 `[DONE]`；auth header 名 | 任何影响转发字节的东西 |

**关键约束**：`Dialect` **只影响观测行为，绝不改写转发的字节**。方言配错了顶多观测数据缺一个字段，不会破坏代理。

---

## Options Considered

### A. 正交的两个枚举 ✅（选定）

**优点**
- 新增一个 OpenAI-compatible 供应商**零代码**：只需配置 `kind = "openai_compatible"` + `base_url` + credential。这是最常见的需求（每周都有新的推理服务上线）。
- `AnthropicMessages` 这一个 parser 服务 4 种 provider（Anthropic / Bedrock / Vertex / 兼容实现），代码复用最大化。
- 能力差异用第三个维度（`ModelBinding.capability_overrides`）表达，不污染前两个。
- 组合的合法性可以用一张矩阵表清晰表达，且能在配置期校验。

**缺点**
- 配置维度增加，用户需要理解「protocol」与「provider kind」的区别。
- 存在非法组合（如 `OpenAiChat` over `ProviderKind::Anthropic`），需要显式校验。

**缓解**：Control Plane 在保存绑定时做语义校验并给出可操作的错误信息（`03-control-plane.md` §5.2）。

---

### B. 单一 `Provider` 枚举 ❌

```rust
enum Provider { OpenAI, Anthropic, Azure, Bedrock, DeepSeek, Qwen, ... }
```

**优点**：简单直观，`match` 一把梭。

**缺点（决定性）**
- 无法表达「Anthropic 协议跑在 Bedrock 上」——这是真实且重要的场景。
- 每加一个 OpenAI-compatible 供应商都要改代码、发版本。而这类供应商有几十个且持续增加。
- 无法表达「OpenAI 的两个协议」——要么拆成 `OpenAIChat` / `OpenAIResponses` 两个 provider（那 credential 与连接池就重复了），要么在 provider 内部再分支（那就是隐式的正交维度）。
- 解析器代码会按供应商重复（DeepSeek 的 parser 与 OpenAI 的 parser 99% 相同）。

---

### C. 只有 `Protocol`，Provider 用配置表达 ❌

即：不要 `ProviderKind` 枚举，把 URL 模板、认证方式全部做成配置字段。

**优点**：最灵活，理论上零代码支持任何供应商。

**缺点**
- SigV4（Bedrock）与 OAuth2 token 刷新（Vertex）不是"配置"能表达的——它们需要代码。
- URL 模板的表达力需求很快会演化成一门小语言（Azure 的 `{deployment}` + `?api-version=`）。
- 失去了编译期的穷尽性检查。

**评价**：正确的做法是**两者结合**——`ProviderKind` 处理需要代码的差异（认证算法、URL 结构），`Dialect` 配置处理不需要代码的差异（字段名、sentinel）。这正是方案 A 所做的。

---

### D. Trait 对象（`Box<dyn ProviderAdapter>`）❌

```rust
trait ProviderAdapter {
    fn build_url(&self, ...) -> Url;
    fn apply_auth(&self, headers: &mut HeaderMap);
    fn parser(&self) -> Box<dyn ProtocolParser>;
}
```

**优点**：最灵活，将来可以做动态插件。

**缺点**
- 把两个正交维度重新耦合进一个 trait（`parser()` 属于 protocol，`build_url()` 属于 provider）。
- 动态分发在热路径上有开销（虽然小）。
- MVP 阶段没有插件需求，是过度设计。

**评价**：`14-rust-project-structure.md` §4 的判据同样适用于抽象——**没有第二个实现者时不要引入 trait**。

---

## Consequences

### 正面

- **新供应商的边际成本接近零**：绝大多数新服务声称 OpenAI 兼容，配置即可接入。
- **解析器代码量最小**：3 个 parser 覆盖 7 种 provider kind。
- **能力差异有明确的表达位置**：`ModelBinding.capability_overrides`（`07` §4.3），而不是塞进 provider 枚举。
- **Bedrock 的帧格式差异有清晰的处理位置**：`Framer` 与 `ProtocolParser` 分离（`04` §9.2），`EventStreamFramer` + `AnthropicMessagesParser` 组合。
- **跨协议调用的拒绝有明确依据**：`request.protocol != binding.provider_protocol` → 400（`04` §7）。

### 负面

| 负面 | 缓解 |
|------|------|
| 非法组合需要显式校验 | 配置期校验 + 运行期 400，错误信息带可操作指引 |
| 用户需要理解两个概念 | 文档 + `config apply` 的校验错误信息教育用户；预设配置模板覆盖常见组合 |
| `Dialect` 是第三个维度，增加认知负担 | 严格限制它只能影响观测；绝大多数用户不需要碰它 |

### 对其他设计的影响

1. **模型管理必须是三层**（`07` §4.1）：`LogicalModel`（声明 protocol）→ `ModelBinding`（指向 provider + provider_model）→ `Provider`/`Endpoint`。二层结构无法表达。
2. **能力必须在 binding 级**：因为同一个模型在不同 provider 上能力不同。
3. **`/v1/models` 必须由 Argus 合成**：因为逻辑模型是 Argus 的概念，不属于任何单一 provider。
4. **路由的能力过滤是选路的一部分**，不是事后检查（`07` §6.2）。

---

## 验证方式

这个决策的正确性可以用一个具体问题检验：

> 假设明天出现一个新服务 "FooAI"，提供 OpenAI 兼容的 Chat Completions API，但认证 header 是 `X-Foo-Token` 而不是 `Authorization`，且不发送 `data: [DONE]`。

在本设计下的接入过程：

```toml
[[providers]]
name = "fooai"
kind = "openai_compatible"
base_url = "https://api.foo.ai"
credential_ref = "env:FOO_TOKEN"
[providers.fooai.dialect]
auth_header = "X-Foo-Token"
auth_scheme = ""                 # 无 "Bearer " 前缀
emits_done_sentinel = false
```

**零代码改动，配置生效时间 15 秒。** 这就是解耦的价值。
