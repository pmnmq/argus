# 04 · Raw Transparent Proxy Mode

> 状态：Draft v2 · 最后更新 2026-08-20
> 前置：`02-routing-model.md`
> 强相关：`13-ssrf-protection.md`（Raw 模式把 SSRF 变成核心安全边界）

---

## 1. Problem

Managed 模式要求先配好 Provider Profile。但总有配不上的时候：

```text
尚未纳管的供应商        临时调试一个新 API
新出现的 endpoint        内部服务
自建/本地模型            未来还不存在的供应商
```

Raw 模式让客户端**直接指定完整的目标 URL**，Argus 只做透明转发 + 观测 + 安全。

它是 **escape hatch**，不是主路径。但它必须存在——否则每遇到一个新东西
就要先改配置，Argus 就成了阻碍而不是帮助。

---

## 2. Goals

- G-W1 客户端可以指定**任意完整 target URL**，包括 scheme / host / port / path / query。
- G-W2 target URL 在穿过 Argus 的过程中**可无损恢复**。
- G-W3 SSRF 是显式的、可配置的边界，默认收紧。
- G-W4 与 Managed 模式共用同一个 Proxy Core。

## 3. Non-goals

- 不要求 Raw 模式适配所有官方 SDK 的 `base_url`（见 §10）。
- 不为 Raw 模式做 provider 特定的适配。
- 不默认开放：`allow_raw_proxy` 默认 `false`。

---

## 4. URL 形式

Argus 接受**两种**形式，都映射到同一个 target：

### 4.1 直接形式（primary）

```text
/raw/{scheme}://{authority}{path}[?{query}]
```

```text
/raw/https://api.openai.com/v1/responses
/raw/http://192.168.1.20:11434/api/chat
/raw/https://[2001:db8::1]:8000/v1/chat
```

好处：一眼看懂，粘贴即用。
风险：中间有任何会折叠 `//` 的组件就会坏（见 §6）。

### 4.2 分段形式（normalization-safe）

```text
/raw/{scheme}/{authority}{path}[?{query}]
```

```text
/raw/https/api.openai.com/v1/responses
/raw/http/192.168.1.20:11434/api/chat
/raw/https/[2001:db8::1]:8000/v1/chat
```

整条 URL 里**没有连续的两个斜杠**，因此能穿过任何会做 `merge_slashes` 的组件。

### 4.3 判别规则

取 `/raw/` 之后的剩余串 `rest`：

```text
若 rest 以 "{scheme}://" 开头（scheme ∈ {http, https}）→ 直接形式
否则若 rest 以 "{scheme}/" 开头                        → 分段形式
否则                                                    → 400 invalid_raw_target
```

两种形式解析后产出完全相同的 `ResolvedTarget`。**不做猜测性修补**：
`/raw/api.openai.com/v1` 缺 scheme，直接 400，不默认补 `https`。

> 默认补 scheme 会让「用户以为在用 TLS，实际是明文」成为可能。

---

## 5. 解析必须按原始字节做（**实测结论**）

在 hyper + axum 上实测，结论如下。

### 5.1 hyper/axum 保留原始 path

用裸 TCP 发请求（绕过任何客户端侧规范化），服务端 `req.uri().path()` 收到的是：

| 送出 | `uri().path()` 收到 |
|---|---|
| `/raw/https://api.openai.com/v1/responses` | `/raw/https://api.openai.com/v1/responses` |
| `/raw/https://[2001:db8::1]:8000/v1/chat` | `/raw/https://[2001:db8::1]:8000/v1/chat` |
| `/provider/openai/v1//double` | `/provider/openai/v1//double` |
| `/provider/openai/v1/models/gpt%2F4` | `/provider/openai/v1/models/gpt%2F4` |

`?a=1&b=a%2Bb` 的 query 也逐字节保留。

**结论：Argus 自身的 HTTP 栈不会破坏 raw path。**

### 5.2 ⚠️ 但 axum 的 `Path` 提取器会百分号解码

同一次实测：

```text
送出        /provider/openai/v1/models/gpt%2F4
Path 提取器  rest = "v1/models/gpt/4"      ← %2F 被解码成了 /
uri().path() "/provider/openai/v1/models/gpt%2F4"   ← 原样
```

`%2F` 解码成 `/` 之后**路径段数都变了**，上游收到的就是另一个资源。
这是一个静默的透明性破坏（违反 §46 的 byte-equivalent 要求）。

**因此硬性规定：**

```text
❌ 禁止用 axum 的 Path / 任何会 decode 的提取器取上游 path
✅ 只能读 req.uri().path() 与 req.uri().query()，按字节切
```

这一条要有 CI 层面的检查（禁止在 ingress 模块里出现 `Path<` 提取器），
见 `19-rust-project-structure.md` §9.3。

### 5.3 切分算法

```text
raw_path = uri.path()                    // 原始，未解码
若以 "/provider/" 开头：
    name_end = raw_path[11..] 中第一个 '/' 的位置
    profile_name = decode(raw_path[11..name_end])   // 只有这里 decode
    rest        = raw_path[name_end..]              // 原样，含前导 '/'
若以 "/raw/" 开头：
    rest = raw_path[5..]                            // 原样
```

`profile_name` 是唯一需要 decode 的地方，因为它要拿去查表；且它的字符集被限制成
`[a-z0-9-]`，decode 之后必须仍然合法，否则 400 —— 这拒绝掉 `%6Fpenai` 这种
绕过命名校验的写法。

---

## 6. 前置组件的风险（Q9）

Argus 自己没问题，问题在**它前面站着谁**。

| 组件 | 默认行为 | 对直接形式的影响 |
|---|---|---|
| nginx | `merge_slashes on`（**默认开**） | `//` → `/`，**直接形式会坏** |
| Traefik | 不折叠，但部分中间件会规范化 | 需逐个验证 |
| Envoy / Istio | `normalize_path` 可配置 | 取决于配置 |
| 云厂商 LB / API Gateway | 多数会规范化，且**不可关闭** | 直接形式不可用 |
| CDN | 通常规范化 | 直接形式不可用 |

**这就是分段形式存在的理由。** 部署文档必须给出：

```nginx
# 若要支持 Raw 模式的直接形式，必须关掉
merge_slashes off;
```

以及一句话结论：**不确定前面有什么，就用分段形式。**

> `22-mvp-roadmap.md` 里第一个 Prototype 必须在 nginx / Traefik 后面实测这两种形式，
> 不能只在 `cargo run` 下验证过就宣布可用。

---

## 7. Target Policy 是 Raw 模式的核心（Q11–Q14）

Managed 模式下 target 由管理员配置决定，风险有限。Raw 模式下 target
**由调用方给出**，于是 Argus 成了一台开放的请求转发器——这正是 SSRF 的定义。

完整设计见 `13-ssrf-protection.md`。这里只列出 Raw 模式必须遵守的边界：

1. **scheme 白名单**：只允许 `http` / `https`。拒绝 `file` / `gopher` / `ftp` 等。
2. **DNS 解析后校验，而不是校验主机名**：主机名可以指向任何 IP。
3. **连接时校验（防 DNS rebinding）**：解析得到的 IP 与实际连接的 IP 必须是同一个。
4. **默认拒绝私有网段**，但管理员可以显式 allow（本产品要能跑在 NAS 上，
   `http://192.168.1.20:11434` 是**正常用法**而不是攻击）。
5. **不自动跟随 redirect**（见 §8）。
6. **端口策略**：默认允许全部端口，但可配置 denylist（如 22 / 25 / 3306 / 6379）。

---

## 8. Redirect（Q13）

**默认：不跟随。** 3xx 原样返回给客户端，由客户端自己决定。

理由：

```text
https://allowed.example
        ↓ 302
http://127.0.0.1:6379/...
```

如果 Argus 自动跟随，第一跳通过了 Target Policy，第二跳就绕过了它——
除非每一跳都重新走一遍完整校验（含 DNS 重解析、IP 校验、连接时校验）。

比较：

| 方案 | 优点 | 缺点 | 结论 |
|---|---|---|---|
| 不跟随（默认） | 无绕过面；行为可预测；实现最简单 | 少数上游依赖 redirect 时客户端要自己处理 | **推荐** |
| 跟随并逐跳重校验 | 兼容性好 | 每跳都要完整 Target Policy；跳数上限；循环检测；凭据是否随跳转携带又是一个坑 | Profile 级可选，默认关 |
| 跟随不重校验 | —— | 直接就是漏洞 | **禁止** |

若某个 Profile 显式开启 `follow_redirects`，则：

- 每一次 `Location` 都**完整重跑** Target Policy
- 最大跳数 3
- **跨 origin 时必须剥除凭据 header**（否则等于把凭据送给重定向目标）
- 每一跳都记入审计

---

## 9. 凭据

Raw 模式默认 **Passthrough**：客户端自己带上游凭据，Argus 原样转发。

```text
Client ──(上游的 Authorization)──> Argus ──(原样)──> Target
```

也允许 Raw 模式绑定一个 Managed 凭据（`?__argus_cred=cred_01J...` 之外的形式，
见 `14-auth-and-credentials.md` §5.3），但这需要 API Key 上有额外权限，
因为它等于「让调用方用 Argus 保管的凭据去打任意地址」。

> **默认禁止这个组合**：`allow_raw_proxy` + `injected credential` 一起打开时，
> 必须同时指定 target allowlist。否则调用方可以把你的 OpenAI key 送到自己的服务器上。
> 这条在 `13-ssrf-protection.md` §9 里作为硬约束。

---

## 10. 与 SDK 的关系（Q48）

Raw 模式**不以适配官方 SDK 为目标**。

原因：SDK 会在 `base_url` 后面拼自己的路径。若把
`https://argus.local/raw/https://api.openai.com/v1` 当作 base_url，
SDK 拼出来的是
`https://argus.local/raw/https://api.openai.com/v1/chat/completions`
——这实际上**能工作**，但依赖 SDK 不做 URL 规范化，且完全没有余量。

明确分工：

```text
Managed Mode  = 普通客户端和 SDK 的主要使用方式，SDK 兼容性测试覆盖它
Raw Mode      = Escape hatch / 高级模式，用 curl、脚本、自研客户端
```

不要为了让 Raw 模式适配所有 SDK 而破坏它的透明代理设计。

---

## 11. Failure Modes

| 情况 | 状态码 | 说明 |
|---|---|---|
| API Key 未开 `allow_raw_proxy` | 403 `raw_proxy_not_allowed` | |
| `rest` 不匹配任何一种形式 | 400 `invalid_raw_target` | 不猜测、不补 scheme |
| scheme 不在白名单 | 400 `scheme_not_allowed` | |
| authority 解析失败 | 400 `invalid_target_authority` | |
| Target Policy 拒绝 | 403 `target_denied` | **不回显解析到的 IP**，避免变成内网扫描器 |
| DNS 解析失败 | 502 `target_dns_failure` | |
| 连接时 IP 与校验时不一致 | 403 `target_rebind_detected` + 告警 | `13` §6 |
| 上游返回 3xx 且未开跟随 | 原样透传 | 不是错误 |

---

## 12. Security Considerations

- Raw 模式的错误信息是**信息泄漏面**。`target_denied` 不能带上解析到的 IP、
  也不能因为「私有地址」和「解析失败」返回不同的错误码——否则 Argus 就成了
  一台内网存活探测器。两者统一返回 403 `target_denied`。
- 响应时间也是侧信道。已知的缓解是给拒绝路径加一个固定的小延迟，
  但这会消耗连接。**第一阶段不做**，作为 Open Question 记录。
- Raw 模式的审计必须记录**完整 target URL**（脱敏后），否则事后无法追查
  「谁用这个 Key 打了哪里」。

---

## 13. Trade-offs

| 选择 | 得到 | 失去 |
|---|---|---|
| 支持两种 URL 形式 | 直接形式好用，分段形式能穿过任何代理 | 解析分支多一条，测试矩阵翻倍 |
| 默认不跟随 redirect | 无绕过面 | 依赖 redirect 的上游需要客户端配合 |
| 默认 passthrough 凭据 | Argus 不承担"用我的 key 打任意地址"的风险 | 客户端要自己管凭据 |
| Raw 不追求 SDK 兼容 | 透明性不被 SDK 的怪癖绑架 | 用户不能把 Raw 当 base_url 用 |

---

## 14. Alternatives Considered

**A. 只支持直接形式**
否决。nginx 默认 `merge_slashes on`，而 nginx 是最常见的前置。
只支持直接形式等于把「能不能用」交给部署环境。

**B. 只支持 query 形式（`/raw?target=https%3A%2F%2F...`）**
最安全（query 几乎不会被规范化），但 §7 要求"客户端可以直接指定完整 target URL"，
而 query 形式对人手写不友好，也无法让客户端在后面追加 path。
**保留为第三种可选形式**，优先级低于前两种。

**C. Base64url 编码整个 target**
可无损、无歧义，但完全不可读，调试时看日志看不出打到哪去了。否决。

**D. 用 header 传 target（`X-Argus-Target: https://...`）**
可行，且完全免疫 path 规范化。但 URL 才是 HTTP 里天然可路由、可在访问日志里
一眼看懂的位置。**作为 OQ-W2 保留**。

---

## 15. Decision

1. Raw 模式接受**两种** URL 形式：直接形式 `/raw/{scheme}://…` 与
   分段形式 `/raw/{scheme}/…`，解析后产出同一个 `ResolvedTarget`。
2. **解析只能基于 `uri().path()` 的原始字节**，禁止使用会 decode 的提取器
   （实测 axum `Path` 会把 `%2F` 解成 `/`）。
3. 不做任何猜测性补全：缺 scheme 直接 400。
4. `allow_raw_proxy` 默认 `false`，是 API Key 上的显式权限。
5. **默认不跟随 redirect**；开启时每跳完整重校验，跨 origin 剥凭据，最多 3 跳。
6. Raw 模式默认 passthrough 凭据；与 injected 凭据组合时必须配 target allowlist。
7. 拒绝类错误统一为 403 `target_denied`，不回显 IP、不区分拒绝原因。

---

## 16. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-W1 | 是否提供第三种 query 形式 `/raw?target=…`？ | 有用户被前置组件卡住且分段形式也不够时 |
| OQ-W2 | 是否提供 `X-Argus-Target` header 形式？ | 同上 |
| OQ-W3 | 拒绝路径的时间侧信道要不要处理？ | 第一阶段不做，M3 复评 |
| OQ-W4 | 分段形式里 authority 含 userinfo（`user:pass@host`）如何编码？ | 倾向直接拒绝 userinfo |
