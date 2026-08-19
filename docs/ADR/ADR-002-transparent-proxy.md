# ADR-002 · 透明字节代理，不依赖官方 SDK

- **状态**：Accepted
- **日期**：2026-08-19
- **相关**：`04-protocols.md`、`05-streaming.md`、`15-testing-strategy.md` §8

---

## Context

Argus 需要把客户端请求转发给 provider，并把响应转发回去。有两种根本不同的做法：

- **A. 透明字节代理**：把请求体作为不透明的 `Bytes` 原样转发；响应体逐 frame 原样转发。Argus 只理解 header 与路由所需的最小字段。
- **B. 结构化中转**：用官方 SDK（或自己的类型）反序列化请求 → 处理 → 重新序列化 → 发给 provider；响应同理。

同时要回答一个具体问题：**官方 SDK（openai-python、anthropic-sdk、以及各种 Rust 移植）在这个项目里应该承担什么角色？**

---

## Decision

1. **采用透明字节代理**。请求体与响应体在 Argus 内部**永不被重新序列化**。
2. **官方 SDK 不进入运行时依赖**。它们的角色限于：录制测试 fixture、作为测试 oracle、schema 对照。

---

## Rationale：为什么不能用 SDK 做透明代理

### 理由 1 · SDK 会丢弃未知字段（致命）

官方 SDK 的反序列化目标是**已知的类型定义**。当 provider 新增一个字段：

```jsonc
// provider 返回
{"type":"content_block_delta","index":0,
 "delta":{"type":"text_delta","text":"hi"},
 "new_experimental_field": {...}}      // ← SDK 版本不认识
```

多数 SDK 会静默丢弃 `new_experimental_field`。如果 Argus 用 SDK 反序列化再重新序列化转发给客户端，**客户端就永远看不到这个字段**——即使客户端用的是更新版本的 SDK。

**结果**：Argus 成为 provider 新能力的瓶颈。用户升级 SDK 后功能还是不可用，因为中间的网关把字段吃掉了。这与 Argus 的核心价值主张（保留协议差异与能力）直接矛盾。

透明代理下，未知字段原样透传，Argus 甚至不需要知道它存在。

---

### 理由 2 · 重新序列化会改变字节（高风险）

即使 SDK 保留了所有字段，`serialize(deserialize(x)) != x`：

| 差异 | 后果 |
|------|------|
| 字段顺序变化 | provider 侧若有基于 body 的签名或缓存键，会失效 |
| 数字表示（`1.0` vs `1`、精度） | 可能改变语义 |
| Unicode 转义（`é` vs 直接的 é） | 字节不同，长度不同 |
| 空对象/空数组的省略（`Option::None` → 字段消失 vs `null`） | **语义不同**：省略 = 用默认值，`null` = 显式清空 |
| 浮点的往返精度 | `0.7` 可能变成 `0.7000000000000001` |

**最严重的具体风险**：Anthropic 的 prompt caching 依赖请求前缀的**逐字节稳定**。如果 Argus 重新序列化导致字节变化（哪怕只是字段顺序），缓存全部失效，用户成本可能翻数倍且毫无提示。

这一条本身就足以否决方案 B。

---

### 理由 3 · SDK 版本永远滞后于 API

Provider 的 API 更新与 SDK 发布之间有滞后（数天到数周）。而**网关必须在 API 更新的那一刻就能透传新能力**——用户不会接受"等 Argus 升级 SDK 再说"。

更糟的是：如果 SDK 遇到未知的 enum 值（如新的 `stop_reason`）会**反序列化失败**，导致 Argus 报错而 provider 其实工作正常。

透明代理下，Argus 对新 `stop_reason` 的反应是：转发（客户端正常），观测层记为 `FinishReason::Other + raw_reason`，指标 `argus_unknown_finish_reason_total` 上报。**零影响 + 主动告警**。

---

### 理由 4 · SDK 自带无法接管的 HTTP 层

官方 SDK 内置了自己的 HTTP 客户端、重试策略、超时、连接池。这些与 Argus 的需求冲突：

| Argus 需要 | SDK 提供 |
|-----------|---------|
| S0–S3 状态机的精确 retry 控制（`02` §7.1） | 自己的重试逻辑，无法表达"已向 client 输出后禁止 retry" |
| 与 client 断开联动的取消传播 | 无法接入 |
| 逐 frame 的 tee（`Bytes` 级） | 只暴露解析后的高层事件 |
| 四段式超时（connect/headers/idle_frame/total） | 通常只有单一超时 |
| 连接池的统一管理与指标 | 各 SDK 各自的池 |
| HTTP/2 多路复用的显式控制 | 不可控 |

要绕过这些，等于只用 SDK 的类型定义而不用它的传输层——那还不如自己写类型。

---

### 理由 5 · Rust 生态的现实

- OpenAI 没有官方 Rust SDK。
- Anthropic 的 Rust SDK 相对年轻。
- 第三方移植的维护性与及时性不可靠。

把主链路的正确性押在一个可能停止维护的第三方 crate 上，是不可接受的风险。

---

### 理由 6 · 性能

方案 B 的每个 SSE frame 需要：`Bytes → str → serde 反序列化（O(字段) 次分配）→ 类型转换 → serde 序列化（新分配）→ Bytes`。

粗估：每 frame 20–40 次堆分配 + 3 次完整数据拷贝。10k 流 × 20 frame/s = 200k frame/s → **每秒 400–800 万次分配**。这会让分配器成为瓶颈，且严重影响 p99。

透明代理下每 frame 的主链路成本是：一次 `Bytes::clone`（原子加）+ 一次 `try_send`（指针移动）≈ **100 ns，0 次分配**。

---

## 官方 SDK 的正确角色

SDK 不是没用——它们是各协议的**权威实现**，这个价值应该在测试中兑现。

### 角色 1 · 录制 fixture（手动，不在 CI）

用官方 SDK 调真实 API，在 HTTP 层录下原始字节。这保证 fixture 是真实的而非"我们以为的样子"。

```python
# tools/record_fixture.py
import anthropic, httpx
class RecordingTransport(httpx.HTTPTransport):
    def handle_request(self, request):
        resp = super().handle_request(request)
        record_raw_bytes(request, resp)      # 落盘为 fixture
        return resp
```

### 角色 2 · 消费验证 oracle（在 CI）★

官方 SDK 作为**客户端**调用 Argus（后面接 Mock Provider），验证两件事：

1. **Argus 的转发对 SDK 透明** —— SDK 能正常解析、正常累积、正常抛出预期的异常。
2. **Argus 的观测与 SDK 的解析一致** —— 我们的 Accumulator 累出的 tool arguments、finish_reason、usage 应与 SDK 的 `get_final_message()` 完全一致。

第 2 点是核心价值：**用权威实现交叉验证我们的解析器**。这比我们自己写断言可信得多。详见 `15-testing-strategy.md` §8.2。

CI 中不需要网络（Mock Provider 在本地）。

### 角色 3 · Schema 对照（半自动）

从 SDK 的类型定义（Python pydantic 模型 / TS 的 `.d.ts`）提取字段清单，与 `06-observation-model.md` §5 的映射表对照，找出我们遗漏的字段。

### 明确的边界

> **任何 `Cargo.toml` 中出现 provider SDK 依赖的 PR 都应被拒绝**（`argus-server` 的 dev-dependencies 除外）。

---

## Consequences

### 正面

- Provider 新增字段/事件/能力时，Argus **零改动**即可透传。
- 不破坏 provider 侧的 prompt caching（字节完全一致）。
- 主链路零分配、零拷贝。
- 协议理解错误的后果被降级：**观测数据不准 ≠ 服务故障**。这是本架构最重要的容错特性。
- 不依赖任何第三方 SDK 的维护状态。

### 负面

| 负面 | 缓解 |
|------|------|
| 必须自己实现协议解析 | 只在**观测层**需要，且解析失败不影响转发；用 golden test 保证正确性 |
| 无法做任何请求改写（如注入 system prompt） | 这是刻意的非目标；若将来需要，作为独立的、显式开启的功能，且明确标注会破坏 prompt cache |
| 无法自动适配 provider 的破坏性变更 | 但 SDK 方案同样无法（SDK 也要发新版）；且我们的 `Unknown` 指标能更早发现 |
| 错误响应需要自己按协议渲染 | `02-data-plane.md` §9 的 `ErrorRenderer`，每协议一个，代码量小 |

### 边界情况：Bedrock

`AnthropicMessages` over Bedrock **无法**做到字节透明——Bedrock 返回 AWS 私有的二进制 eventstream，而客户端期望 SSE。必须重新分帧。

这属于**帧格式转换**而非**协议语义转换**：event 的 JSON payload 逐字保留，只是外层容器变了。语义无损，但字节不再等价。

处理：在 `04-protocols.md` §9.2 明确标记为特殊路径；`Framer` 与 `ProtocolParser` 从第一天起就是分离的 trait，让 `EventStreamFramer` 可以复用 `AnthropicMessagesParser`。

---

## 相关决策

- 请求体全量 buffer（`02` §6.2）是本决策的推论：既然不解析，就需要完整的 `Bytes` 用于 retry 与 tee。
- 不做请求体 schema 校验（`04` §8）是同一原则的延伸：滞后的校验会拒绝合法请求。
- 不做跨协议转换（`04` §7）是同一原则在协议层面的应用。
