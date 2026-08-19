# 16 · MVP 范围与路线图

> 状态：Draft v1 · 最后更新 2026-08-19

---

## 1. Problem

设计文档描述了一个相当完整的系统。如果照单全收地实现，第一个可用版本会在很久之后才出现，而在那之前无法验证任何假设——尤其是**性能假设**（所有的资源模型与 SLO 都还是估算）。

需要一个能尽早验证核心假设、且每一步都产出可用东西的推进顺序。

---

## 2. 核心判断

**最大的技术不确定性不是功能，而是"边转发边观测"这条路是否真的零成本。**

因此第一个里程碑不是"完整的网关"，而是**一个能证明 Tee 架构成立的最小可测系统**。如果这条路走不通（比如 tee 的开销远超预期，或 parser 的 CPU 消耗吃掉主链路），整个设计需要重来——越早知道越好。

---

## 3. 里程碑

### M0 · 骨架与可行性验证（1–2 周）

**目标：证明 Tee 架构成立，拿到第一组真实性能数字。**

| 交付 | 说明 |
|------|------|
| workspace 骨架 | 5 个 crate + CI（fmt/clippy/test/不变式检查） |
| `argus-protocol` 的 `SseFramer` | 含 fast path、gap resync、切分策略测试 |
| `AnthropicMessagesParser` | 只支持 text delta（不做 tool/thinking） |
| 最小 Accumulator | 只累积文本与 usage |
| `TeeBody` | 完整实现（含双预算） |
| 硬编码单 provider 的透传 | 无认证、无数据库、无配置 |
| Mock Provider | `Replay` + `Synthetic` 两种 scenario |
| **第一次压测** | 对照法测 proxy-added TTFT；1k/10k 并发流的内存与 CPU |

**退出标准（关键）**：
- `proxy_added_ttft` p50 < 2 ms，p99 < 15 ms
- 10k 并发流下 RSS < 1.5 GB
- 观测开启 vs 关闭的 CPU 差异 < 30%
- 逐字节切分与整块切分产出相同结果

**若不达标**：停下来重新设计，不要继续往上叠功能。这是整个路线图里唯一的硬门禁。

---

### M1 · 可用的透明代理（3–4 周）

**目标：一个真的能用的网关（但功能最小）。**

| 交付 | 说明 |
|------|------|
| 三个协议的完整 parser | OpenAI Chat / Responses / Anthropic Messages |
| 完整 Accumulator | tool calls、reasoning、多 block、乱序容忍、partial finish |
| Golden test 体系 | ~150 个 fixture，切分矩阵，`covers` 追踪 |
| 配置系统 | TOML + 环境变量 + profile |
| `argus-storage` 基础 | 迁移、`requests` + `request_payloads` + `tool_calls` 表、批量写 |
| `AuditSink` | Batcher + UNNEST 写入（**先不做 Spool**） |
| API Key 认证 | KeyCache、创建/吊销、基础授权 |
| 单 provider 路由 | logical model → 单个 binding，无 fallback |
| 优雅停机 | 完整实现 |
| `/healthz` `/readyz` `/metrics` | 基础指标 |
| Docker 镜像 | |

**退出标准**：能用官方 SDK（Python openai + anthropic）跑通全部 golden 场景；能连续运行 24 小时无内存增长。

---

### M2 · 多 Provider 与可靠性（3–4 周）

**目标：生产可用的可靠性。**

| 交付 | 说明 |
|------|------|
| 完整路由 | 多 binding、priority/weight、能力过滤 |
| Retry 状态机 | S0–S3 完整实现 + 三重预算 |
| 熔断 + throttle | 被动健康检查、429 独立处理 |
| Provider 方言 | Azure / DeepSeek / OpenAI-compatible |
| Spool | 完整的降级与回放 |
| Auth 的 stale cache | PG 故障时的降级 |
| 限流 + 软配额 | GCRA + token/cost 计数 |
| `ObserveMode` 降级 | 压力驱动的请求粒度采样 |
| 资源上限 | 大请求 semaphore、RSS watermark、load shedding |
| 集成测试 | PG 故障、fallback、取消、停机等场景 |
| `argus doctor` | |

**退出标准**：`15-testing-strategy.md` §10 的集成测试场景全部通过；混沌测试（随机杀 PG、断 provider）下无数据丢失、无请求失败率上升。

---

### M3 · Control Plane 与安全（4–5 周）

**目标：可管理、有安全能力。**

| 交付 | 说明 |
|------|------|
| Control Plane API | Provider/Model/Key/Policy 的完整 CRUD |
| Credential 加密 | master key、轮换、fingerprint |
| 配置热更新 | ConfigSnapshot + refresher (+ 可选 NOTIFY) |
| `config apply` / `export` | 声明式配置 |
| 安全规则引擎 | Layer 0–2、内置规则集 |
| PreflightGuard | 内联检查 |
| **ToolGate** | 核心安全机制 |
| `security_events` | 记录、脱敏、查询 |
| 审计查询 API | 游标分页、`audit:read_content` scope |
| 数据保留与分区维护 | 分级保留、自动分区 |

**退出标准**：能通过 Control Plane 完成一次从零到可用的配置；ToolGate 能拦住 `curl | sh` 且不增加文本延迟。

---

### M4 · 打磨与发布（2–3 周）

| 交付 | 说明 |
|------|------|
| Dashboard（最小） | 请求列表、详情、用量图表 |
| 完整文档 | 部署、配置、运维、故障排查 |
| 性能报告 | 实测数据回填 `11-observability.md` §6 |
| 长稳测试 | 7 天 |
| K8s manifest / Compose | |
| v0.1.0 发布 | |

---

## 4. MVP Scope（M0–M4 合计约 13–18 周）

### 4.1 包含

```text
协议     OpenAI Chat Completions / OpenAI Responses / Anthropic Messages
         流式 + 非流式，tool calls，reasoning，多模态透传，usage
Provider OpenAI / Anthropic / Azure OpenAI / 任意 OpenAI-compatible
路由     多 binding、priority/weight、能力过滤、fallback、熔断、retry 状态机
观测     完整 IR + Accumulator、raw selective 保存、降级模式
安全     规则引擎 Layer 0–2、PreflightGuard、ToolGate、security_events
认证     API Key、Workspace、RBAC、限流、软配额
存储     PostgreSQL + 分区 + 分级保留 + Spool
运维     指标、日志、健康检查、优雅停机、doctor
部署     二进制 / Docker / K8s / Compose
```

### 4.2 明确不实现（MVP）

| 功能 | 原因 | 计划 |
|------|------|------|
| **跨协议转换** | 语义有损（`04-protocols.md` §7.2） | 有明确需求时作为独立 `/compat/` 端点 |
| **Gemini 协议** | 语义映射不平凡 | Phase 2 |
| **Bedrock / Vertex** | Bedrock 需要 eventstream 重分帧 + SigV4 | Phase 2（架构已预留 `Framer` 抽象） |
| **LLM Detector** | 成本与延迟不确定；规则引擎先跑起来 | Phase 2 |
| **`delay_window` 安全模式** | 与低延迟目标冲突，需求未验证 | Phase 2 |
| **Redis 支持** | 单实例不需要 | Phase 2 |
| **OpenTelemetry** | feature 已预留，实现简单但非核心 | Phase 2（低成本） |
| **`RequireReview` 审批流** | 流程设计未定 | Phase 3 |
| **OIDC / SAML** | 本地账号够用 | Phase 3 |
| **成本感知路由** | 需要先有成本数据 | Phase 3 |
| **`LeastLatency` 路由** | 羊群效应需要仔细设计 | Phase 3 |
| **语义缓存** | 正确性风险高 | 可能永不做 |
| **对象存储 payload** | 大规模才需要 | Phase 3 |
| **ClickHouse 双写** | 违背轻量目标 | Phase 3（可选） |
| **Embeddings / Files / Batch API** | 非核心场景 | 按需求 |
| **多区域 active-active** | 超出范围 | — |
| **Dashboard 完整功能** | MVP 只做只读列表与图表 | 持续迭代 |

### 4.3 Phase 2（v0.2，MVP 后 2–3 个月）

```text
Gemini / Bedrock / Vertex
LLM Detector
Redis（分布式限流 + 熔断共享 + taint 共享）
OpenTelemetry
delay_window
argus-security 拆 crate（若达到触发条件）
增量 JSON parser（实时 tool args 检测）
物化视图 / 统计加速
Dashboard 完善
```

---

## 5. 最大技术风险 Top 5

### 风险 1 · Tee 的实际开销超出预期（严重度：致命）

**假设**：`Bytes::clone` + `try_send` 的开销 < 100 ns，观测的 CPU 消耗 < 30%。

**若不成立**：整个"零成本观测"的前提崩塌。可能表现为 10k 并发时 p99 延迟显著恶化，或 CPU 打满。

**缓解**：
- M0 就验证，不达标就停下来重新设计。
- 后备方案：把 tee 改为**采样**（只对 1–10% 的请求做完整观测），牺牲观测覆盖率换性能。这个后备方案已经在设计里（`ObserveMode`），所以不是灾难性的。

**当前状态**：未验证。这是 M0 存在的唯一理由。

---

### 风险 2 · Provider 协议的隐藏行为（严重度：高）

**问题**：文档里的协议描述来自公开文档，但真实 API 有大量文档未提及的行为——字段顺序、可选字段的实际出现条件、错误格式的变体、非标准的 SSE 分隔符、provider 特有的 event。

已知的例子（`04-protocols.md` 的坑列表）已经有 25+ 条，实际肯定更多。

**表现**：用户报"用 SDK 直连正常，走 Argus 观测数据不对"。

**缓解**：
- Golden test 从**真实录制**而非文档构造（`15-testing-strategy.md` §5.5）。
- `Unknown` 事件是一等公民 + `argus_unknown_event_total` 告警 → 尽早发现。
- Contract test 定期跑真实 API。
- **关键**：即使观测错了，**转发仍然正确**（因为我们不重新序列化）。这把"协议理解错误"的后果从"服务故障"降级为"观测数据不准"。这是透明代理架构的最大价值。

**当前状态**：已有缓解设计，但需要大量真实 fixture 才能收敛。

---

### 风险 3 · PostgreSQL 成为写入瓶颈（严重度：中高）

**假设**：批量写（256 条/200ms）能支撑目标 QPS。

**风险点**：
- payload 是 JSONB 且可能很大（TOAST 写入 + WAL 放大）
- 分区表的插入需要路由（PG 11+ 已优化，但仍有开销）
- 索引维护（虽然大量用了部分索引）
- 1000 req/s × 平均 5KB payload = 5 MB/s 的持续写入 + WAL 放大 ≈ 15 MB/s

**表现**：sink queue 持续积压 → 降级到 `degrade` → 丢失内容数据。

**缓解**：
- `synchronous_commit = off`（仅 sink 池）
- `on_full = degrade` 保住计量数据
- Spool 作为缓冲
- 若仍不够：`store_raw_events = false` / `payloads_days` 缩短 / Phase 3 的对象存储

**当前状态**：未测。M1 结束时应做一次写入压测。

---

### 风险 4 · Accumulator 的正确性（严重度：中高）

**问题**：Accumulator 要处理三种协议 × 多种 block 类型 × 乱序/中断/gap 的组合。状态空间大，且错误是**静默的**（不会崩溃，只是数据不对）。

具体易错点：
- Anthropic 的 usage 是 last-wins 不是求和（求和会得到 2× 的错误结果，且看起来"合理"）
- OpenAI tool_calls 的 `index` 语义与 Anthropic 的 `index` 不同
- tool arguments 的 JSON 拼接时机
- `n > 1` 时的 choice 隔离

**表现**：token 计费错误、tool call 追踪缺失、审计数据与实际不符。**最糟的是没人发现**。

**缓解**：
- 映射表的每一行都有 golden case（`coverage_matrix` 测试）
- OpenAI Responses 的 `response.completed` 交叉校验（免费的自检）
- 流式 vs 非流式的交叉验证
- SDK 作为 oracle 对照
- `anomaly` 计数 + 告警

**当前状态**：设计已考虑，但正确性最终取决于 fixture 的覆盖度。

---

### 风险 5 · 内存模型在真实负载下失准（严重度：中）

**假设**：每流 ~54 KB，10k 流 ~700 MB。

**风险点**：
- rustls 的实际缓冲占用未实测（可能远超 8 KB/连接）
- Rust 分配器的碎片（尤其是 musl 的 mallocng，已知碎片较多）
- accumulator 在长对话下的实际分布（我们假设中位数 8 KB，但重度用户可能是 100 KB）
- 突发的大请求

**表现**：OOM Kill，或不得不把并发上限调到远低于预期。

**缓解**：
- M0 就实测 1k/10k 的 RSS
- RSS watermark 主动降级
- 所有缓冲都有硬 cap
- 可切换 jemalloc（`13-deployment.md` OQ-5）

**当前状态**：全部是估算。M0 的压测是第一次校准。

---

### 风险清单之外的已知弱点

| 弱点 | 影响 | 接受理由 |
|------|------|---------|
| 熔断器进程内，多实例不共享 | 故障发现慢 | 样本代价可接受（20 请求/实例） |
| 软配额必然超额 | 成本可能略超预算 | 已量化并文档化；硬配额代价太大 |
| 会话 taint 不持久 | 重启后失效 | 攻击窗口有限 |
| Master key 是 SPOF | 丢失即灾难 | 无法避免；靠备份流程 |
| `prompt-injection` 检测能力有限 | 定向攻击可绕过 | 已明确声明不承诺检出率 |
| Spool 单机，Pod 重建时丢 | 极端情况丢审计 | 可选 PVC 缓解 |

---

## 6. 推荐的第一批开发任务

按依赖顺序，前 10 个任务（约对应 M0）：

| # | 任务 | 产出 | 依赖 |
|---|------|------|------|
| **T1** | workspace 骨架 + CI | 5 个 crate、`deny.toml`、`ci/check-invariants.sh`、fmt/clippy/test 流水线 | — |
| **T2** | `argus-core` 的基础类型 | `Protocol`、`ProviderKind`、`TraceContext`、`TeeChunk`、`InteractionSink` trait、错误类型 | T1 |
| **T3** | `SseFramer` + 切分测试 | fast path、三种分隔符、gap resync、`max_frame_bytes`、~20 个切分策略的 property test | T2 |
| **T4** | Observation IR 定义 | `ObservedEvent`、`BlockCoord`、`BlockKind`、`BlockDelta`、`Finish`、`UsageDelta` | T2 |
| **T5** | `AnthropicMessagesParser`（最小） | 只做 `message_start` / `content_block_*`(text) / `message_delta` / `message_stop` / `ping` / unknown | T3, T4 |
| **T6** | 最小 Accumulator | 文本累积（amortized push_str + cap）、usage last-wins、partial finish | T4 |
| **T7** | Golden test 驱动器 | fixture 加载、切分矩阵执行、`insta` snapshot、`coverage_matrix` 框架 | T5, T6 |
| **T8** | `TeeBody` + `TeeHandle` | 双预算、`gap_before` 累积、`TeeEnd` 语义 | T2 |
| **T9** | 最小透传服务 | Axum ingress + hyper client（HTTP/2 优先）+ 硬编码 provider + Tee 挂载 + observer task | T8, T5 |
| **T10** | Mock Provider + 压测 | `Replay`/`Synthetic`/`Realtime pacing`；对照法压测脚本；1k/10k 并发的 RSS/CPU/延迟报告 | T9 |

**T10 的产出是 M0 的退出决策依据。** 在它完成之前不要开始 M1 的任何工作。

### 6.1 T3 的验收细节（最容易被低估的任务）

`SseFramer` 看起来简单，但它是整个系统正确性的地基。验收清单：

- [ ] `\n\n` / `\r\n\r\n` / `\r\r` 三种分隔符，**一次扫描取最早出现的**（`05-streaming.md` §5.3 标记的易错点）
- [ ] fast path：缓冲为空时用 `Bytes::split_to`，零拷贝
- [ ] slow path：跨 chunk 拼接，只拷贝残留部分
- [ ] `gap_before > 0` → 丢弃缓冲 + 进入 resync + 产出 `Gap` 事件
- [ ] resync：扫描到下一个分隔符才恢复；扫描超过 `max_frame_bytes` 则放弃观测
- [ ] `data: [DONE]` 特判
- [ ] `:` 开头的注释行（SSE 心跳）
- [ ] 多行 `data:`（SSE 规范允许，实际 provider 不用，但要容忍）
- [ ] 空的 `data:` 行
- [ ] `event:` / `id:` / `retry:` 字段
- [ ] 缓冲上限保护
- [ ] property test：任意字节不 panic、缓冲有界
- [ ] 切分不变性：30 种策略结果一致

---

## 7. 不做什么（工作方式上的）

| 反模式 | 为什么避免 |
|--------|-----------|
| 先写完整的 Control Plane 再做代理 | 管理面是最容易做的部分，先做它会推迟核心风险的验证 |
| 先支持 8 种 provider | 前 3 种就能暴露 90% 的架构问题；后面的是重复劳动 |
| 一开始就做 Dashboard | API 稳定前做 UI 是浪费 |
| 为"将来的扩展性"加抽象层 | `14-rust-project-structure.md` §4 的判据；等真的需要时再加 |
| 把 M0 的压测推迟到"功能做完之后" | 那时改架构的成本是 10 倍 |
| 手写 SSE fixture | 与真实字节必然有差异（`15-testing-strategy.md` §5.5） |

---

## 8. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-1 | M0 的退出标准是否过于严格？（p50 < 2ms 是否可达） | M0 首次测量后调整，但不降低到失去意义 |
| OQ-2 | 时间估算（13–18 周）是否现实？取决于投入人力。 | 每个里程碑结束后重估 |
| OQ-3 | 是否应该在 M1 就加入 Spool（而不是 M2）？PG 故障在开发期就会遇到。 | M1 开始时决定 |
| OQ-4 | Gemini 支持是否应该提前到 MVP？（如果有明确用户需求） | 有需求时 |
| OQ-5 | ToolGate 放在 M3 是否太晚？它是核心差异化能力。 | 若安全是主要卖点，可提前到 M2 |
