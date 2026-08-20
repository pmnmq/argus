# 03 · Managed Provider Mode

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`02-routing-model.md`
> 相关：`08-provider-profiles.md`、`14-auth-and-credentials.md`

---

## 1. Problem

Managed 模式是普通客户端和官方 SDK 的**主要使用方式**。它要解决的问题是：

> 让用户把 SDK 的 `base_url` 指向 Argus，其余什么都不用改。

这句话里藏着几个必须做对、做错了就会以"SDK 报错但直连正常"的形式暴露的细节：

- `base_url` 和剩余 path 怎么拼，才不会多一个或少一个 `/`
- query string 会不会丢
- 已经百分号编码过的 path 段会不会被二次编码
- SDK 自己会在 `base_url` 后面接什么

---

## 2. Goals

- G-M1 官方 SDK 只改 `base_url` 就能工作，不需要打补丁。
- G-M2 上游收到的 path + query 与「客户端直连供应商时」逐字节一致。
- G-M3 拼接规则简单到可以在脑子里执行，不需要查表。
- G-M4 凭据注入是配置，不是代码分支。

## 3. Non-goals

- 不重写 request body（`02-routing-model.md` §4.2）。
- 不代替客户端决定 API 版本或 endpoint。
- 不在 Managed 模式里做 URL 白名单之外的智能纠错。

---

## 4. URL 规范

```text
https://argus.local/provider/{profileName}/{upstreamPath}[?{query}]
```

- `profileName`：Provider Profile 的 `name`，单个 path 段，
  `^[a-z0-9][a-z0-9-]{0,62}$`（OQ-R1）。
- `upstreamPath`：**剩余的全部 path 段**，可以为空以外的任意长度。
- `query`：原样保留。

### 4.1 实例

| 进来的 | Profile `base_url` | 发出去的 |
|---|---|---|
| `/provider/openai/v1/responses` | `https://api.openai.com` | `https://api.openai.com/v1/responses` |
| `/provider/anthropic/v1/messages` | `https://api.anthropic.com` | `https://api.anthropic.com/v1/messages` |
| `/provider/deepseek/v1/chat/completions` | `https://api.deepseek.com` | `https://api.deepseek.com/v1/chat/completions` |
| `/provider/dashscope/compatible-mode/v1/chat/completions` | `https://dashscope.aliyuncs.com` | `https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` |
| `/provider/openrouter/api/v1/chat/completions` | `https://openrouter.ai` | `https://openrouter.ai/api/v1/chat/completions` |
| `/provider/azure-eu/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21` | `https://xx.openai.azure.com` | `https://xx.openai.azure.com/openai/deployments/gpt4o/chat/completions?api-version=2024-10-21` |

最后一行说明：Azure 那套 `deployments/{name}` + `api-version` 的形状**不需要
Argus 做任何特殊处理**，客户端怎么写就怎么转。这正是 `provider_type` 不参与
路由的价值。

---

## 5. 拼接规则（Q6）

只有一条规则：

```text
target = normalize_base(base_url) + "/" + rest_path
```

其中：

- `normalize_base` = 去掉 `base_url` 尾部**全部**的 `/`
- `rest_path` = 去掉前导 `/` 之后的剩余 path

因此 `https://x.com/`、`https://x.com//`、`https://x.com` 三种写法等价，
用户不会因为配置里多打一个斜杠而收到 404。

### 5.1 base_url 带路径前缀

允许 `base_url` 自带前缀，此时它整体作为前缀：

```text
base_url = https://gw.corp.com/ai/v1
请求      /provider/corp/chat/completions
上游      https://gw.corp.com/ai/v1/chat/completions
```

这让 Argus 可以挂在企业内已有的 AI 网关后面。

### 5.2 空 path

```text
/provider/openai        → 400 empty_upstream_path
/provider/openai/       → rest_path = ""，转发到 base_url 根路径
```

区分这两者是刻意的：前者说明客户端 base_url 配错了（少了尾部斜杠又没接 path），
直接报错比转发到一个几乎肯定是 404 的地址更容易诊断。

### 5.3 不做 path 规范化

Argus **不**折叠 `//`、**不**解析 `.`/`..`、**不**改变大小写。

```text
/provider/x/v1//foo   →  {base}/v1//foo
```

理由：这是透明代理。上游想怎么理解 `//` 是上游的事，Argus 替它做决定只会
制造出「直连能用、走 Argus 不能用」的差异。

> 反过来，**Argus 自己前面**可能有会折叠 `//` 的组件。这属于部署约束，
> 见 `18-deployment.md` §5 与 `04-raw-proxy-mode.md` §6。

---

## 6. Query String（Q7）

原样透传，**一个字节都不动**：

```text
?api-version=2024-10-21&foo=a%2Bb&empty=&flag
        ↓
?api-version=2024-10-21&foo=a%2Bb&empty=&flag
```

具体做法是从原始请求的 `Uri` 上取 `query()`（它是**未解码**的原串），
直接拼到 target 上。不解析成 key-value map，不重新序列化。

> 解析再序列化会改变参数顺序、丢掉重复 key、把 `a%2Bb` 变成 `a+b`。
> 这三种改动都真实地破坏过某些供应商的签名校验。

---

## 7. 百分号编码（Q8）

规则：**在整条链路上只保持一种表示——客户端给的那种。**

```text
客户端 path 段：  /v1/models/gpt%2F4
Argus 内部：      按原始字节持有，不 decode
上游 path 段：    /v1/models/gpt%2F4
```

具体约束：

1. 从 `Uri::path()` 取到的是**原始（仍编码）**的串，直接用。
2. 分割 `providerName` 与剩余 path 时，**按 `/` 字节切**，不 decode 后再切。
   —— 这保证 `%2F`（编码后的斜杠）不会被误当成路径分隔符。
3. **一处都不 decode**，`providerName` 也不例外。它的字符集被限制成
   `[a-z0-9-]`（OQ-R1），`%` 不在其中，所以对**原始字节**做一次字符集校验
   就足以拒绝 `%6Fpenai` 这类绕过写法。顺序反过来是错的：先 decode 再校验，
   `%6Fpenai` 会解成 `openai` 并顺利通过。
4. 绝不 re-encode。已经是合法 URI 的东西再编一次就会变成 `%252F`。

---

## 8. 凭据与 Header 注入（Q17）

Profile 上配的是**数据**，不是代码分支：

```yaml
name: anthropic-prod
base_url: https://api.anthropic.com
credential:
  ref: cred_01J...              # 指向加密存储的凭据
  mode: injected
  header: x-api-key             # 放哪个 header
  format: "{secret}"            # 值的形状
extra_headers:
  anthropic-version: "2023-06-01"
```

```yaml
name: openai-prod
base_url: https://api.openai.com
credential:
  ref: cred_01J...
  mode: injected
  header: authorization
  format: "Bearer {secret}"
```

注入发生在 header 变换阶段（`05-transparent-proxy.md` §5）：

```text
1. 剥掉客户端带来的上游凭据 header（见 §9）
2. 剥掉 hop-by-hop header
3. 注入 Profile 的 credential header
4. 注入 Profile 的 extra_headers（不覆盖客户端已显式设置的非凭据 header）
5. 重写 Host 为 target 的 authority
```

`format` 里只允许 `{secret}` 一个占位符。不做模板语言——模板语言意味着
Control Plane 的一个配置字段可以影响 Data Plane 的字符串构造，那是注入面。

### 8.1 为什么 credential 是「预构造好的 header 值」

见 `14-auth-and-credentials.md` §6：凭据在内存里以可清零的缓冲持有，
每请求构造一次 `HeaderValue` 并标记 sensitive（HTTP/2 下不进 HPACK 动态表）。

---

## 9. 客户端凭据的处理

Managed 模式下，客户端发来的是 **Argus 的** API Key。它必须在转发前被移除，
**绝不能**泄漏给上游：

```text
authorization / x-api-key / api-key / x-goog-api-key
    ↓
被 Argus 消费掉，不转发
```

这条是硬规则，不可配置。见 `05-transparent-proxy.md` §5.2。

> 例外只有一个：Profile 显式配置 `credential.mode: passthrough`。
> 那时 Argus 不注入自己的凭据，而是把客户端的凭据放行。
> 见 `14-auth-and-credentials.md` §5.2。

---

## 10. SDK 兼容性

各 SDK 会在 `base_url` 后面接不同的东西，因此推荐的配置值不同：

| SDK | 推荐 `base_url` | SDK 会接上 |
|---|---|---|
| OpenAI Python / JS | `https://argus.local/provider/openai/v1` | `/chat/completions`、`/responses`、`/models` |
| Anthropic Python / TS | `https://argus.local/provider/anthropic` | `/v1/messages` |
| OpenAI-compatible（DeepSeek 等） | `https://argus.local/provider/deepseek/v1` | `/chat/completions` |
| DashScope 兼容模式 | `https://argus.local/provider/dashscope/compatible-mode/v1` | `/chat/completions` |

注意 OpenAI 与 Anthropic 的 SDK 对 `/v1` 的处理不同——一个要你写进 base_url，
一个自己加。Argus 不需要知道这个差别（§5 的拼接规则对两者都成立），
但**文档必须写清楚**，否则用户会配错然后得到 404。

这些组合是 `20-testing-strategy.md` §9 的 SDK 兼容性测试矩阵。

---

## 11. Failure Modes

| 情况 | 状态码 | 说明 |
|---|---|---|
| profile 不存在 / 无权访问 | 404 `provider_not_found` | 两者返回同一个错误，防枚举（`02` §11） |
| profile 被禁用 | 503 `provider_disabled` | |
| 剩余 path 为空 | 400 `empty_upstream_path` | §5.2 |
| `providerName` 含非法字符 | 400 `invalid_provider_name` | 含 decode 后才合法的写法 |
| Profile 的 credential 解密失败 | 503 `credential_unavailable` | **不 fail-open**，见 `14` §6.4 |
| base_url 解析失败（配置错误） | 503 `provider_misconfigured` + 告警 | 应在配置写入时就拦下 |

错误体按调用方协议的形状渲染，见 `06-data-plane.md` §9。

---

## 12. Trade-offs

| 选择 | 得到 | 失去 |
|---|---|---|
| 只有一条拼接规则 | 可预测、易解释 | 无法自动适配"base_url 已含 /v1 但客户端又写了 /v1" |
| query 原样透传 | 签名类参数不被破坏 | 无法在网关侧做参数级策略 |
| 不折叠 `//` | 与直连行为一致 | 前置组件若折叠，行为会不一致（需部署约束） |
| 凭据是配置不是代码 | 加 provider 不用改代码 | 配置模型要能表达各种认证形状 |

---

## 13. Alternatives Considered

**A. `base_url` 与 path 之间做智能去重**（发现两边都有 `/v1` 就去掉一个）
否决。"智能"意味着不可预测，而且会在某个 provider 真的需要 `/v1/v1` 时出错。

**B. 每个 provider_type 一套硬编码的 URL 构造器**
否决。这是 `02-routing-model.md` §6 明确禁止的 `match provider_type`。
Azure 的例子说明：把形状交给客户端，Argus 反而不需要知道。

**C. 在 Argus 侧维护每个 provider 的 endpoint 白名单**
否决。等价于维护一份会过期的 API 目录，与 §G-R3 冲突。

---

## 14. Decision

1. URL 形状 `/provider/{profileName}/{upstreamPath}`，profileName 是单段。
2. 拼接规则唯一：`rstrip(base_url,'/') + '/' + lstrip(rest,'/')`。
3. query 原样透传，不解析不重排。
4. 百分号编码全程保持客户端给的形式，只在查 profile 名时 decode 一次。
5. 不做 path 规范化（不折叠 `//`、不解 `..`）。
6. 凭据与额外 header 是 Profile 上的数据，注入发生在 header 变换阶段。
7. 客户端携带的上游凭据 header 默认必须剥除。

---

## 15. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-M1 | `extra_headers` 与客户端同名 header 冲突时谁赢？倾向：凭据类 Argus 赢，其余客户端赢 | 实现 `05` 时定 |
| OQ-M2 | 是否需要 `/provider/{name}` 的 `GET` 健康探测端点？ | M1 |
