# ADR-012 · 观测 fail-open，认证 fail-close

- **状态**：Accepted
- **日期**：2026-08-19
- **相关**：`12-security.md`、`14-auth-and-credentials.md`、`15-storage.md`、`17-reliability.md`

---

## Context

Argus 处于所有 AI 流量的关键路径上。它有多个可能失败的辅助组件：

- 安全规则引擎（可能超时、崩溃、规则编译失败）
- 观测链路（parser 可能 panic、channel 可能满）
- 审计存储（PostgreSQL 可能不可用）
- 限流后端（Redis 可能不可用）
- 认证后端（PostgreSQL 可能不可用）

每一个都要回答同一个问题：**它失败时，请求应该被放行还是被拒绝？**

这不是一个可以含糊处理的问题。默认值选错的后果：

- 全部 fail-close → 一个安全规则的超时就能让整个 AI 平台停摆。**安全模块变成最大的可用性风险。**
- 全部 fail-open → 认证组件失败时任何人都能调用。**这是安全漏洞，不是降级。**

---

## Decision

**认证与授权 fail-close；其余一切 fail-open。**

理由用一句话表达：

> **认证 fail-open = 安全漏洞；可观测性 fail-close = 自造 DoS。**

---

## 完整矩阵

| 组件 | 失败场景 | 默认 | 可配置 | 理由 |
|------|---------|------|--------|------|
| **AuthN** | PG 不可用 + KeyCache 命中 | **放行**（stale，≤15min） | ❌ | 已验证过的凭据，短期信任是合理的；否则 PG 抖动 = 全站中断 |
| **AuthN** | PG 不可用 + KeyCache 未命中 | **拒绝 503** | ❌ | **fail-close**。未知凭据绝不放行 |
| **AuthN** | 凭据格式错误 / secret 不匹配 | 拒绝 401 | ❌ | |
| **AuthZ** | 权限检查（纯内存，不会"失败"） | 拒绝 403 | ❌ | **fail-close** |
| **PreflightGuard** | 超时（> 50ms） | **放行** | ✅ | 超时通常意味着请求异常大，不是攻击信号 |
| **PreflightGuard** | panic | **放行** | ❌ | 我们的 bug 不应变成客户的故障 |
| **PreflightGuard** | 规则编译失败 | **跳过该规则** | ✅ | 单条坏规则不应禁用整个策略 |
| **SecurityExtractor** | 请求 JSON 解析失败 | **降级为 RawFallback** | ❌ | 仍能跑通用规则 |
| **ToolGate** | 超时（> 20ms） | **放行** | ✅ | |
| **ToolGate** | 缓冲溢出 | **flush 放行** | ❌ | 不能无界缓冲 |
| **ToolGate** | Observer 已挂 | **PassThrough** | ❌ | 退化为无 gate |
| **LLM Detector** | 超时 / 失败 / 并发满 | **跳过** | ✅（timeout 可调） | 排队会拖垮自己 |
| **PostGuard** | 任何失败 | **无影响** | ❌ | 它本就不能阻断 |
| **Observer / Parser** | panic | **继续透传** | ❌ | 强制 fail-open |
| **Observer** | channel 满 | **丢字节 + 标记** | ❌ | |
| **AuditSink** | 队列满 | **degrade**（丢 payload 保 requests） | ✅ | |
| **AuditSink** | PG 不可用 | **写 Spool** | ✅（`require_durable=true` 可改 fail-close） | |
| **Spool** | 磁盘满 | **丢弃 + 告警** | ❌ | |
| **RateLimit** | Redis 不可用 | **降级本地限流** | ✅ | 绝不因此拒绝服务 |
| **Quota** | PG 不可用（用量数据陈旧） | **按陈旧数据检查**（可能超额） | ✅ | |
| **Router** | 无健康 provider | 拒绝 503 | ❌ | 不是"失败开放"，是真的无法服务 |
| **Provider 健康检查** | 探测不可用 | **视为健康** | ❌ | 探测失败不代表 provider 失败 |
| **ConfigRefresher** | 刷新失败 | **保留旧快照** | ❌ | |
| **Master Key** | 缺失 / 无法解密 | **拒绝启动** | ❌ | 启动时严格（见下） |

---

## 三条原则

### 原则 1 · 认证/授权 fail-close，其余 fail-open

**为什么认证必须 fail-close**：认证的作用就是区分"能访问"与"不能访问"。失败时放行等于取消了这个区分——不是降级，是功能完全失效。而且这类失效通常是静默的（没人会注意到自己不该有权限却成功了）。

**为什么其余 fail-open**：观测、审计、安全检测都是**增强**。它们失败时，系统仍能完成主要职能（转发 AI 请求）。让增强功能的失败阻断主要职能，是把可用性押在最不成熟的组件上。

### 原则 2 · 启动时严格，运行时宽容

```text
启动时：master key 缺失 → 拒绝启动
        PG 不可达 → 重试 60s 后退出
        迁移失败 → 拒绝启动
        配置无法加载 → 拒绝启动
        spool 目录不可写 → 警告，禁用 spool（这一条是宽容的）

运行时：PG 挂 → 继续服务（Spool + stale cache）
        Redis 挂 → 降级本地
        安全模块崩 → 放行
        观测崩 → 继续转发
```

**理由**：启动时还没有流量，拒绝启动是安全的（运维会立刻发现）。运行时拒绝会造成真实事故。

这个不对称是刻意的，也是很多系统做错的地方——它们在启动时宽容（"先跑起来再说"），运行时严格（"检测到异常就拒绝"），恰好反了。

### 原则 3 · 每一个 fail-open 都必须有指标与告警

fail-open 的危险在于它是**静默的**：安全模块挂了，请求照常通过，没人知道保护已经失效。

因此每个 fail-open 点都配套一个计数器：

| 指标 | 含义 |
|------|------|
| `argus_preflight_timeout_total` | 安全检查被跳过的次数 |
| `argus_security_no_rules_total` | 规则集为空（配置错误） |
| `argus_observer_panic_total` | **应恒为 0** |
| `argus_tee_dropped_bytes_total` | 观测有损 |
| `argus_auth_stale_hit_total` | 认证走了 stale cache（PG 有问题） |
| `argus_redis_degraded_total` | 限流降级 |
| `argus_sink_state{state="spooling"}` | 审计走 Spool |
| `argus_spool_dropped_bytes_total` | **审计数据永久丢失** |
| `argus_llm_detector_skipped_total` | 检测被跳过 |

`21-performance.md` §7 给出了对应的告警规则。

> **没有指标的 fail-open 等于没有安全策略。** 这条必须在实现时严格执行。

---

## 允许改为 fail-close 的场景

高合规环境可以把部分点改为 fail-close，但必须理解代价：

```toml
[security.preflight]
on_timeout = "close"       # 超时则拒绝请求
timeout = "200ms"          # 同时必须调大超时，否则误拒率高

[storage.audit]
require_durable = true     # 审计写入失败则拒绝请求
```

**`require_durable = true` 的代价必须说清楚**：它把 PostgreSQL 变成 Data Plane 的硬依赖。PG 挂掉 = 网关完全不可用。这与 `15-storage.md` 的整个 Spool 设计相悖，只应在"审计缺失比服务中断更严重"的场景（如受监管的金融/医疗）启用。

文档中必须用明确的警告框标注这个选项。

---

## 已经输出后发现风险的处置

这是 fail-open 立场下最需要诚实回答的问题（`12-security.md` §6.4）。

**已经写入客户端 socket 的字节无法收回。没有任何技术手段能改变这一点。**

能做的：

| 措施 | 效果 | 局限 |
|------|------|------|
| 立即截断流 | 阻止后续内容 | 已发出的留在客户端 |
| 发协议内 error event | SDK 抛异常，agent 通常中止 | 依赖客户端行为 |
| 会话 taint | 后续请求 403 | 只对多轮有效 |
| **ToolGate 拦执行** | **阻止危险内容被执行** | 需要开启 |
| 告警 + 记录 | 人工介入 | 事后 |

**本项目的立场**：

> 不追求"阻止模型说出危险内容"（做不到，且代价是给所有请求加延迟）；
> 聚焦"阻止危险内容被执行"（做得到，且零感知延迟）。

这个立场直接导出了 ToolGate 的设计——利用"客户端必须等完整 JSON 才能执行工具"这个天然检查点，在 `content_block_stop` 处扣留 20ms 做检查。对文本输出零影响。

ToolGate 是本设计中**唯一**违反"观测不影响主链路"原则的机制，因此它：
- 默认关闭（`enabled = false`）
- 有硬超时（20ms）与缓冲上限（256 KiB）
- Observer 挂掉时自动退化为 PassThrough
- 三重保护都是 fail-open

---

## Options Considered

**A. 全部 fail-close（"安全优先"）。**
表面上更安全，实际上：一个安全规则的超时就能让 AI 平台停摆；PG 抖动会让所有请求失败。**一个因为安全模块而挂掉的网关，其净安全价值是负的**——用户会绕过它直连 provider，那时连观测都没有了。**否决。**

**B. 全部 fail-open（含认证）。**
认证失败时放行 = 任何人都能用。这不是降级，是漏洞。**强烈否决。**

**C. 按 severity 决定（Critical 规则 fail-close，其余 fail-open）。**
听起来合理，但"规则超时"与"规则命中"是两回事——超时不代表内容危险，只代表检查没跑完。用规则的严重度决定检查失败时的行为，是把两个正交的东西混在一起。**否决。**

**D. 让每个 workspace 自己选。**
已采纳（`Policy.fail_mode` 可配置），但**默认值必须是 fail-open**。让用户在知情的情况下选择更严格的模式，而不是默认给所有人加上可用性风险。

**E. 认证也用 stale cache 无限期 fail-open。**
`stale_ttl = 15m` 已经是一个 fail-open 的妥协。无限期意味着吊销的 Key 可能永远有效。15 分钟对应"人工发现并修复 PG 故障"的量级。**保持有限期。**

---

## Consequences

### 正面

- Argus 的可用性不受安全/观测/存储组件的可靠性拖累。
- PostgreSQL 故障时 Data Plane 完全正常（`15-storage.md` §8.5）。
- Redis 故障时限流降级而非拒绝服务。
- 安全模块可以快速迭代（新规则出 bug 不会造成事故），配合 `suppressions` 可无需发版止损。

### 负面

| 负面 | 缓解 |
|------|------|
| 安全模块失效是静默的 | **每个 fail-open 点必须有指标 + 告警**（原则 3） |
| PG 故障期间配额可能超额 | 已量化并文档化（`14-auth-and-credentials.md` §8.4） |
| 吊销的 Key 在 PG 故障时最多多活 15 分钟 | 正常路径是 15 秒；15 分钟是故障期的上界 |
| 高压时部分请求无观测 | 请求粒度降级保证被观测的数据完整（`ADR-009` M4） |
| 高合规用户需要额外配置 | 提供 `fail_mode = "close"` 与明确的代价说明 |

### 对文档的要求

用户文档中必须有一节明确说明：

> **Argus 的安全能力是纵深防御的一层，不是保证。**
> - 安全检测在系统压力或组件故障时会被跳过（有指标可查）。
> - `prompt-injection` 类检测的检出率有限，不能防住定向攻击。
> - 配额是最终一致的软配额，多实例下可能超额。
> - 已经流式输出的内容无法撤回。
>
> 如果你的场景需要硬性保证，请：(a) 启用 `fail_mode = "close"` 并接受可用性代价；(b) 在 provider 侧同时配置组织级配额；(c) 在客户端侧对工具执行做二次确认。

**含糊其辞的安全承诺比没有安全功能更危险**——它会让用户产生虚假的安全感。
