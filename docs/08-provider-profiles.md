# 08 · Provider Profile

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`02-routing-model.md`、`03-managed-provider-mode.md`
>
> ⚠️ 本文件取代了 v1 的 `07-provider-routing.md`（该文件已不存在）。v1 里的「逻辑模型 → 绑定 →
> 加权选路 → 跨 provider fallback」整套机制**已删除**，理由见 `02` §4 与 §7。

---

## 1. Problem

Argus 要抹平的差异清单其实很短：

```text
不同 Base URL          不同 Workspace 归属
不同 Credential        不同 Timeout
不同认证 header 形式   不同代理策略
不同必需 header        不同安全策略
```

注意这里**没有**：不同的 API 语义、不同的模型名、不同的参数集合。
那些差异是真实的、有意义的，抹平它们等于删信息。

Provider Profile 就是承载上面那张短清单的东西。

---

## 2. Goals

- G-P1 加一个新供应商 = 加一条配置，**不改代码**。
- G-P2 同一个供应商可以有多个 Profile（不同 key、不同配额、不同策略）。
- G-P3 Data Plane 不出现 `match provider_type`。
- G-P4 供应商上新模型，Profile 不需要动。

## 3. Non-goals

- 不维护 Profile 支持哪些模型。
- 不做跨 Profile 的自动 fallback（`02` §7）。
- 不做请求体改写。

---

## 4. 数据模型

```rust
pub struct ProviderProfile {
    pub id: ProviderProfileId,
    /// URL 里出现的那个名字。^[a-z0-9][a-z0-9-]{0,62}$，workspace 内唯一
    pub name: CompactString,
    pub display_name: CompactString,
    /// 仅用于 UI 模板与默认值。**Data Plane 不得据此分支**
    pub provider_type: ProviderType,
    /// 只能是 `scheme://authority[/path-prefix]`。不得带 query / fragment /
    /// userinfo —— 理由与强制位置见 `03-managed-provider-mode.md` §5
    pub base_url: Uri,
    pub enabled: bool,

    /// 默认 observer；可被 observer_rules 按 path 细化（`09-observers.md` §4）
    pub observer: ObserverKind,

    pub credential: CredentialBinding,
    /// 每请求注入的固定 header（如 anthropic-version）
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,

    pub timeouts: Timeouts,
    pub proxy_policy: Arc<ProxyPolicy>,
    pub target_policy: Option<Arc<TargetPolicy>>,   // None = 用全局策略
    pub security_policy: Option<SecurityPolicyId>,

    pub workspace_id: WorkspaceId,
    pub created_at: jiff::Timestamp,
    pub updated_at: jiff::Timestamp,
}

#[non_exhaustive]
pub enum ProviderType {
    OpenAi, Anthropic, AzureOpenAi, Gemini, Bedrock, Vertex,
    DashScope, DeepSeek, SiliconFlow, Moonshot, OpenRouter,
    Ollama, VLlm,
    Custom,
}

pub struct CredentialBinding {
    pub mode: CredentialModeConfig,     // Injected { .. } | Passthrough | None
    pub credential_id: Option<CredentialId>,
    /// 放哪个 header。Injected 时必填
    pub header: Option<HeaderName>,
    /// 值的形状，唯一允许的占位符是 {secret}
    pub format: Option<CompactString>,
}
```

### 4.1 `ProviderType` 是什么、不是什么

**是**：UI 上选"新建 OpenAI Provider"时用来填默认值的模板键；
指标和审计里的一个分组标签。

**不是**：路由依据、行为分支、能力声明。

```rust
// ❌ 禁止出现在 Data Plane
match profile.provider_type {
    ProviderType::Anthropic => req.headers_mut().insert("anthropic-version", ..),
    _ => {}
}

// ✅ 正确：它是 Profile 上的数据
for (k, v) in &profile.extra_headers {
    req.headers_mut().insert(k.clone(), v.clone());
}
```

这条要有 CI 检查：`argus-proxy` 里不得对 `ProviderType` 做穷举 match。
见 `19-rust-project-structure.md` §9。

### 4.2 为什么 Profile 名可以是 `openai-prod` / `openai-dev`

因为**路由的单位是接入点，不是公司**。同一个 `base_url` 完全可能需要：

| | `openai-prod` | `openai-dev` |
|---|---|---|
| credential | 生产 key | 沙盒 key |
| 配额 | 无限 | 每天 $5 |
| 安全策略 | 严格（block） | 宽松（flag） |
| workspace | prod | dev |
| 审计保留 | 90 天 | 7 天 |

如果 Profile 名必须等于公司名，上面这些就没地方放了。

---

## 5. 认证形状是数据

各家的认证差异用同一组字段表达，不需要代码分支：

| Provider | `header` | `format` | `extra_headers` |
|---|---|---|---|
| OpenAI | `authorization` | `Bearer {secret}` | — |
| Anthropic | `x-api-key` | `{secret}` | `anthropic-version: 2023-06-01` |
| Azure OpenAI | `api-key` | `{secret}` | — |
| Gemini | `x-goog-api-key` | `{secret}` | — |
| DeepSeek / Moonshot / SiliconFlow | `authorization` | `Bearer {secret}` | — |
| OpenRouter | `authorization` | `Bearer {secret}` | `http-referer` / `x-title`（可选） |
| Ollama / vLLM（无认证） | — | — | — |

`format` 只支持 `{secret}` 一个占位符。**不引入模板语言**——那会让一个
Control Plane 的配置字段变成 Data Plane 的字符串构造注入面。

### 5.1 SigV4 / OAuth2 这类"算出来的"凭据

Bedrock 的 SigV4 和 Vertex 的 OAuth2 token 无法预构造成固定 header。
它们是 **Phase 2**，届时以 `CredentialModeConfig` 的新变体接入
（`#[non_exhaustive]` 已经给它们留了位置），而不是加 `match provider_type`。

---

## 6. 配置示例

```yaml
providers:
  - name: openai-prod
    display_name: "OpenAI（生产）"
    provider_type: openai
    base_url: https://api.openai.com
    observer: openai-chat          # 会被 observer_rules 按 path 细化
    credential:
      mode: injected
      credential_id: cred_01J...
      header: authorization
      format: "Bearer {secret}"
    timeouts: { connect: 3s, headers: 60s, idle_frame: 120s, total: 900s }

  - name: anthropic-prod
    provider_type: anthropic
    base_url: https://api.anthropic.com
    observer: anthropic-messages
    credential:
      mode: injected
      credential_id: cred_01K...
      header: x-api-key
      format: "{secret}"
    extra_headers:
      anthropic-version: "2023-06-01"

  - name: dashscope-cn
    provider_type: dashscope
    base_url: https://dashscope.aliyuncs.com
    observer: openai-chat
    credential: { mode: injected, credential_id: cred_01M..., header: authorization, format: "Bearer {secret}" }

  - name: ollama-nas
    provider_type: ollama
    base_url: http://192.168.1.20:11434
    observer: openai-chat
    credential: { mode: none }
    target_policy:
      allow_cidrs: ["192.168.1.20/32"]     # 精确放行，见 13-ssrf-protection.md §5.1
```

注意 `dashscope-cn` 这一条：客户端要打
`/provider/dashscope-cn/compatible-mode/v1/chat/completions`，
而 Argus 完全不需要知道 `compatible-mode` 是什么意思。

---

## 7. 重试与熔断（粒度 = Profile）

跨 Provider 的 fallback 已删除。剩下的是**同一个 Profile 内**的重试：

| 阶段 | 触发 | 默认 | 说明 |
|---|---|---|---|
| S0 连接/DNS/TLS 失败 | `ConnectError` | ✅ 重试 | provider 完全没收到，绝对安全 |
| S0 熔断器 Open | 选路时 | ❌ 直接失败 | 没有别的候选可切了 |
| S1 已发出、header 超时 | `Timeouts.headers` | ✅ 重试（可关） | **可能重复计费**，见 §7.2 |
| S2 429 / 5xx | status | ✅ 重试 | 带退避 |
| S2 401/403/4xx | status | ❌ 透传 | 换个姿势重试结果一样 |
| S3 已向客户端写出 | 任意 | ❌ **永不** | 宪法 C18，不可配置 |

```toml
[providers.retry]
max_attempts = 2          # 含首次。没有别的 provider 可切，重试次数应更保守
budget = "10s"
on_no_response = true
backoff = { initial = "100ms", max = "2s", multiplier = 2.0, jitter = 0.3 }
```

> `max_attempts` 从 v1 的 3 降到 2：v1 的 3 次是为了"试三个不同的 provider"，
> 现在三次都打同一个地方，收益递减而重复计费风险线性增长。

### 7.1 熔断

粒度是 Profile。熔断打开时**直接返回 503**，不静默切别处（`02` §7）。

429 单独计一个 throttle 窗口，与 5xx 的熔断分开——429 是"你太快了"，
5xx 是"我坏了"，混在一起会让限流把健康的 provider 熔断掉。

### 7.2 S1 重试的计费风险

S1 = 请求已写入 socket 但没收到 header。provider 可能已经开始生成并计费。
默认仍然重试（跨境网络下"根本没收到"是最常见的情况），但：

1. 审计里记 `retry_after_send = true` + 被放弃 attempt 的 provider request id
2. 提供 `GET /admin/v1/stats/potential_double_billing` 供财务核对
3. `headers` 超时默认 60s，减少误判

---

## 8. Failure Modes

| 情况 | 行为 |
|---|---|
| Profile 不存在 / 无权 | 404 `provider_not_found`（统一，防枚举） |
| Profile 被禁用 | 503 `provider_disabled` |
| 凭据解密失败 | 503 `credential_unavailable`，**不 fail-open** |
| `base_url` 非法 | 配置写入期就拒绝；运行期发现则 503 + 告警 |
| `format` 不含 `{secret}` | 配置写入期拒绝 |
| Injected 但没配 `header` | 配置写入期拒绝 |
| 熔断打开 | 503 `circuit_open` + `Retry-After` |

---

## 9. Trade-offs

| 选择 | 得到 | 失去 |
|---|---|---|
| 认证形状是数据 | 加供应商不改代码 | 算出来的凭据（SigV4）要等 Phase 2 |
| Profile 是路由单位 | 同供应商可多套策略 | 用户要理解"Profile ≠ 供应商" |
| 无跨 Profile fallback | 计费与行为可预测 | 单点故障时客户端自己换 |
| `provider_type` 只做模板 | Data Plane 无分支 | UI 要多做一点默认值工作 |

---

## 10. Alternatives Considered

**A. 保留 v1 的逻辑模型绑定**
否决。它把模型目录的维护成本引了回来，与 `02` §4 冲突。

**B. `provider_type` 决定认证方式（代码内置）**
否决。新供应商就要发版。而且 Azure 那种"同一 type 不同形状"的情况会立刻打脸。

**C. Profile 里声明支持的模型列表，用于请求前校验**
否决。见 `02` §12：猜错的校验比没有校验更糟，而且违反 G-P4。

---

## 11. Decision

1. Provider Profile 是路由与策略的单位，`name` 出现在 URL 里。
2. `provider_type` 仅用于 UI 模板与指标分组，**Data Plane 禁止据此分支**（CI 检查）。
3. 认证形状用 `header` + `format` + `extra_headers` 三个数据字段表达。
4. 不声明、不校验模型列表。
5. 重试与熔断的粒度是 Profile，无跨 Profile fallback；S3 之后永不重试。
6. 凭据不可用时 fail-close（503），不降级为不带凭据转发。

---

## 12. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-P1 | Profile 是否需要"继承"（一个基础 Profile + 覆盖）？ | 用户 Profile 数量超过 ~20 时再考虑 |
| OQ-P2 | `extra_headers` 与客户端同名 header 的冲突规则 | 同 OQ-M1 |
| OQ-P3 | 显式 Profile 组（可选 fallback）何时引入 | 有明确需求时，Phase 2 |
