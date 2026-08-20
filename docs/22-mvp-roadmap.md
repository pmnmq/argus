# 22 · MVP 路线图

> 状态：Draft v2 · 最后更新 2026-08-20

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

| | 目标 | 门禁 |
|---|---|---|
| **M0** | 骨架 + 核心类型 + SSE 分帧 + **URL 解析** + MockProvider + 性能基线 | **G-M0**：`proxy_added_ttft` p50<2ms/p99<15ms；10k 流 RSS<1.5GB；切分不变性 100%；**URL 解析矩阵全绿** |
| **M1** | Managed 模式端到端 + 三种 Observer + Accumulator + golden test + 存储 | **G-M1**：官方 SDK 兼容全绿（OpenAI/Anthropic × Python/JS）；24h 无内存增长 |
| **M2** | Raw 模式 + Target Policy/SSRF + 认证授权 + 限流配额 + Spool + 混沌 | **G-M2**：SSRF 用例全绿（含 rebinding、redirect）；**nginx/Traefik 后的 URL 形式实测**；28 个混沌场景 |
| **M3** | 安全引擎 + ToolGate + Control Plane + 配置热更新 + 留存分区 | 安全规则集完整；Control Plane 不可用时 Data Plane 继续 |
| **M4** | 打包发布 + 长稳 + **实测数据回填** | **G-M4**：7 天长稳；`21-performance.md` 里不得留"未验证"标记 |

M0 与 M2 的门禁里各多了一条 v2 新增的硬要求：

- **M0 的 URL 解析矩阵**（`20-testing-strategy.md` §5.1）—— 拼接、编码、
  双斜杠、两种 Raw 形式。这是新路由模型的地基，错了后面全错。
- **M2 的前置组件实测**（`20` §5.5）—— 必须真的把 Argus 放到 nginx 后面跑，
  不能只在 `cargo run` 下验证过就宣布 Raw 模式可用。

---

## 4. MVP 范围

**做：**

```text
Managed Provider Mode（/provider/{name}/…）      Raw Proxy Mode（两种 URL 形式）
Provider Profile CRUD + 凭据加密存储             Target Policy / SSRF 防护
透明 body 转发（字节保真）                        Header 策略（hop-by-hop + 凭据）
SSE 分帧 + 三种协议的 Observer                    GenericSse / GenericJson 兜底
Stream Accumulator                              审计落 PostgreSQL + Spool
Argus API Key + Workspace + RBAC                 限流 + token/cost 软配额
规则引擎安全检测 + ToolGate                       Prometheus 指标 + 可选 OTLP
优雅停机 + 隔板 + load shedding
```

**明确不做（第一阶段）：**

```text
❌ model → provider 自动选路          ❌ 模型目录 / 价格表 / 成本计算
❌ 跨 Provider fallback               ❌ 协议转换（OpenAI ↔ Anthropic）
❌ Gemini / Bedrock（预留接口）        ❌ SigV4 / OAuth2 凭据
❌ Dashboard UI                       ❌ LLM 安全检测器（接口预留）
❌ Redis（可选，非默认）               ❌ 多实例强一致配额
```

---

## 5. Top 5 技术风险

| # | 风险 | 为什么危险 | 缓解 |
|---|---|---|---|
| R1 | **Raw 模式的 URL 在前置组件里被规范化** | 失败方式是"在开发机上好好的，一上生产就 404"，且只影响一部分部署 | 设计了分段形式；M2 必须在 nginx/Traefik 后实测（`20` §5.5） |
| R2 | **透明性被静默破坏** | 已实测 axum 的 `Path` 提取器会把 `%2F` 解成 `/`。这类 bug 不报错、不告警，只是上游收到了另一个请求 | 禁用会 decode 的提取器（CI 检查）；字节级透明性测试（`20` §5.2） |
| R3 | **SSRF 绕过** | Argus 通常在内网且手握凭据，一次绕过就是横向移动的起点 | 判定 IP 而非主机名；只解析一次 DNS；连接时确认（`13` §6） |
| R4 | **TTFT 不达标** | 观测和 Target Policy 都在关键路径上；DNS 未命中可能几十 ms | F07 硬门禁；DNS 缓存；缓存命中/未命中分别测（`21` §8） |
| R5 | **Observer 反压主链路** | 违反最核心的隔离原则，且只在高负载下暴露 | `try_send` 永不 await；双预算；CI 检查 tee 里不得有 `.send().await` |

## 6. Top 5 架构风险

| # | 风险 | 缓解 |
|---|---|---|
| A1 | 有人为了"方便"重新引入 model → provider 推断 | 宪法条款 + CI 检查 + `02-routing-model.md` 完整论证 |
| A2 | `provider_type` 悄悄变成业务分支 | CI 检查：`argus-proxy` 里不得对 `ProviderType` 穷举 match |
| A3 | 为了观测方便，主链路开始反序列化 body | 宪法 C1 + 字节级透明性测试 |
| A4 | Raw + injected 凭据被误配成开放代理 | 配置期强制 allowlist（`13` §9） |
| A5 | 模型目录以"只是加个校验"的形式回归 | `02` §12 明确记录了这个代价是刻意接受的 |

## 7. 推荐的第一批开发任务

按依赖顺序，前 10 个任务（约对应 M0）：

| # | 任务 | 产出 | 依赖 |
|---|------|------|------|
| **T1** | workspace 骨架 + CI | 5 个 crate、`deny.toml`、`ci/check-invariants.sh`、fmt/clippy/test 流水线 | — |
| **T2** | `argus-core` 的基础类型 | `Protocol`、`ProviderType`、`TraceContext`、`TeeChunk`、`InteractionSink` trait、错误类型 | T1 |
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

- [ ] `\n\n` / `\r\n\r\n` / `\r\r` 三种分隔符，**一次扫描取最早出现的**（`10-streaming.md` §5.3 标记的易错点）
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

## 8. 不做什么（工作方式上的）

| 反模式 | 为什么避免 |
|--------|-----------|
| 先写完整的 Control Plane 再做代理 | 管理面是最容易做的部分，先做它会推迟核心风险的验证 |
| 先支持 8 种 provider | 前 3 种就能暴露 90% 的架构问题；后面的是重复劳动 |
| 一开始就做 Dashboard | API 稳定前做 UI 是浪费 |
| 为"将来的扩展性"加抽象层 | `19-rust-project-structure.md` §4 的判据；等真的需要时再加 |
| 把 M0 的压测推迟到"功能做完之后" | 那时改架构的成本是 10 倍 |
| 手写 SSE fixture | 与真实字节必然有差异（`20-testing-strategy.md` §5.5） |

---

## 9. Open Questions

| # | 问题 | 何时决定 |
|---|------|---------|
| OQ-1 | M0 的退出标准是否过于严格？（p50 < 2ms 是否可达） | M0 首次测量后调整，但不降低到失去意义 |
| OQ-2 | 时间估算（13–18 周）是否现实？取决于投入人力。 | 每个里程碑结束后重估 |
| OQ-3 | 是否应该在 M1 就加入 Spool（而不是 M2）？PG 故障在开发期就会遇到。 | M1 开始时决定 |
| OQ-4 | Gemini 支持是否应该提前到 MVP？（如果有明确用户需求） | 有需求时 |
| OQ-5 | ToolGate 放在 M3 是否太晚？它是核心差异化能力。 | 若安全是主要卖点，可提前到 M2 |
