# Argus 项目宪法

> 状态：Ratified v1 · 2026-08-19
> **本文件是唯一必须始终在上下文中的文档。** 每个 spec 的执行都必须遵守。
> 违反任何一条 = 该实现不可接受，无论它是否"能跑"。
> 条款的完整论证在 `docs/`，本文件只给规则与检查方式。

---

## 0. 优先级

冲突时按此顺序取舍，不得自行调整：

```
Correctness > Compatibility > Reliability > Observability > Security > Performance > Feature Count
```

**Security 排在 Observability 之后**的含义：安全能力的失效不得损害网关可用性。

---

## 一、透明性（Transparency）

### C1 · 不重新序列化
请求体与响应体在 Argus 内部**永不**被反序列化后重新序列化。转发的字节必须与收到的字节逐字节相同。
- 例外：`/v1/models`（Argus 自己的资源清单）、错误响应、Bedrock 的帧格式重组（`docs/04-protocols.md` §9.2）。
- 依据：`docs/ADR/ADR-002-transparent-proxy.md`
- 检查：主链路代码中不得出现对 body 的 `serde_json::to_*`。

### C2 · 未知即透传
未知的 header / JSON 字段 / SSE event / finish_reason / delta type，一律原样转发，**绝不报错、绝不丢弃**。
- 观测层把它们收进 `Unknown` / `Opaque` / `Other(raw)` 并计数。
- 依据：`docs/04-protocols.md` §11、`docs/ADR/ADR-004-observation-ir.md` D5

### C3 · 不做协议转换
`Protocol` 不匹配时返回 400，**不做隐式转换**。跨协议调用不在本项目范围内。
- 依据：`docs/04-protocols.md` §7

### C4 · SDK 禁入运行时
provider 官方 SDK 只能出现在 `dev-dependencies` 与测试工具中。任何在 `[dependencies]` 引入 provider SDK 的改动一律拒绝。
- 依据：`docs/ADR/ADR-002-transparent-proxy.md`

---

## 二、隔离（Isolation）

### C5 · 主链路不等待旁路
主链路（P0–P11）上**不得存在**对观测、存储、日志、安全后端的 `.await`。
- 唯一例外：`PreflightGuard`，且必须有硬超时（默认 50ms）与 fail-open。
- Tee 只能用 `try_send`，**永不** `send().await`。
- 依据：`docs/ADR/ADR-005-streaming-architecture.md` I1
- 检查：`ci/check-invariants.sh` grep `tee.rs` 中的 `.send(`

### C6 · 依赖方向单向
```
argus-server → argus-proxy ┐
              argus-storage┴→ argus-protocol → argus-core
```
- `argus-core` **不得**依赖任何内部 crate（它是根，反向依赖即循环）。
- `argus-protocol` **不得**依赖 `tokio` / `hyper` / `sqlx` / `axum` / `std::fs` / `std::net`。
- `argus-proxy` **不得**依赖 `sqlx` / `argus-storage`。
- 依据：`docs/01-architecture.md` §5.1、`docs/14-rust-project-structure.md` §7.2
- 检查：`cargo tree` 断言

### C7 · panic 不得杀进程
- `panic = "unwind"` 固定，**任何**改为 `abort` 的改动一律拒绝。
- `argus-protocol` 内禁止 `unwrap` / `expect` / `panic!` / 切片索引 / 整数除法（clippy `deny`）。
- observer 的 parser step 必须包 `catch_unwind`。
- 依据：`docs/01-architecture.md` §9

### C8 · 全 workspace 禁 unsafe
`#![forbid(unsafe_code)]`。例外需在 PR 中给出论证并单独 `#[allow]`。

---

## 三、有界性（Boundedness）

### C9 · 禁止无界
不得出现 `unbounded_channel` / `unbounded()`。所有队列、缓冲、缓存必须有显式上限。
- 检查：`ci/check-invariants.sh` grep

### C10 · 超限降级，不报错
任何缓冲触顶时的行为是**降级并标记**，不是返回错误、不是中断请求。
- `max_frame_bytes` 超限 → 停止该流观测，继续透传
- `max_accumulated_bytes` 超限 → truncate，继续消费事件
- sink 队列满 → `degrade`（丢 payload 保 `requests` 行）
- 依据：`docs/05-streaming.md` §8.2、`docs/10-storage.md` §7.3

### C11 · 不为慢客户端缓冲
慢客户端由 TCP 背压自然处理。**绝不**为它分配发送缓冲区。
- 依据：`docs/05-streaming.md` §6.2

### C12 · 过载快速失败
所有隔板（semaphore / 队列 / 连接池）满时**立即拒绝**，不排队。
- 依据：`docs/12-reliability.md` §5.3

---

## 四、诚实性（Honesty）

### C13 · 有损必可检测
任何观测数据的丢失、截断、降级，都必须在落库记录中显式可见。
- `gap_before` → `Integrity::Degraded { dropped_bytes, gap_count, truncated_blocks }`
- `Outcome` 与 `Integrity` 是**两个正交维度**，都必须落库。
- **静默的有损观测是本项目最严重的缺陷类别。**
- 依据：`docs/ADR/ADR-005-streaming-architecture.md` I3、M3

### C14 · 不修复、不猜测
- 截断的 tool arguments **绝不**补齐括号引号，标记 `Incomplete { raw }`。
- usage 缺失时的估算值**绝不**写进真实字段，用 `usage_source = estimated` 区分。
- 交叉校验发现不一致时**记录 anomaly，不修正累积结果**。
- 依据：`docs/ADR/ADR-004-observation-ir.md` D3、D4

### C15 · fail-open 必须留痕
每一个 fail-open 点都必须有计数器指标，且落库记录中带降级标记（`security_degraded`）。
- **没有指标的 fail-open 等于没有安全策略。**
- 依据：`docs/ADR/ADR-006-fail-open-security.md` 原则 3

### C16 · 不伪造性能数据
未经 benchmark 的数字一律标注「目标，未验证」。禁止把估算写成实测。

---

## 五、失败模式（Failure Semantics）

### C17 · fail-open 为默认，认证除外
| 组件 | 默认 |
|---|---|
| AuthN（缓存未命中 + PG 挂） | **fail-close 503** |
| AuthZ | **fail-close 403** |
| 其余全部（安全 / 观测 / 审计 / 限流 / 配额 / 健康检查） | **fail-open** |
- 依据：`docs/ADR/ADR-006-fail-open-security.md`

### C18 · S3 后禁止 retry
一旦向客户端写出第一个响应字节，**永不**重试或 fallback。此规则不可配置。
- 依据：`docs/02-data-plane.md` §7.1、`docs/07-provider-routing.md` §7.1

### C19 · PostgreSQL 不是 Data Plane 的依赖
PG 不可用时代理必须继续工作。`/readyz` **不得**检查 PostgreSQL。
- 依据：`docs/10-storage.md` §8.5、`docs/11-observability.md` §9

### C20 · 启动严格、运行宽容
启动时依赖缺失可以拒绝启动；运行时任何依赖故障都必须降级而非拒绝服务。
- 依据：`docs/12-reliability.md` §6.2

---

## 六、性能预算（Budgets）

### C21 · 热路径零分配
每转发一个 SSE frame，主链路的堆分配次数必须为 **0**；观测链路稳态每 token 分配次数为 **0**（摊销）。
- 手段：`Bytes::clone`、`serde(borrow)`、`String::push_str`、`CompactString`、`SmallVec`
- 禁止：热路径上的 `format!` / `to_string()` / `serde_json::Value`
- 依据：`docs/05-streaming.md` §7

### C22 · 转发零 memcpy
provider → client 路径上不得引入除 TLS 加密外的任何数据拷贝。

### C23 · 上游优先 HTTP/2
上游连接器必须 ALPN 优先协商 h2。这是 10k+ 并发的前提（连接数差 100×）。

---

## 七、可测性（Testability）

### C24 · 解析器必须切分不变
任何 `Framer` / `Parser` 的实现，对同一输入的**任意字节切分方式**（含逐字节）必须产出完全相同的事件序列。
- 依据：`docs/15-testing-strategy.md` §5.4

### C25 · fixture 必须来自录制
禁止手写 SSE 字节作为测试数据。来源只能是：Argus 录制、SDK 录制、生产导出（脱敏后）。

### C26 · 映射表逐行覆盖
`docs/06-observation-model.md` §5 映射表的每一行必须有对应 golden case，由 `coverage_matrix` 测试强制。

---

## 八、安全与隐私（Security & Privacy）

### C27 · 凭据不落明文
- 客户端 API Key：只存 `sha256(salt||secret)`，`key_id` 明文可索引。
- Provider credential：master key（不入库）AES-256-GCM 加密，**无读取明文的 API**。
- 日志中**绝不**出现 prompt 内容、响应内容、tool arguments、任何 key。
- 检查：`ci/check_log_fields.py`

### C28 · 客户端凭据不透传
客户端的 `Authorization` / `x-api-key` / `api-key` 必须剥离，不得转发给 provider。所有 `x-argus-*` 前缀的客户端 header 一律剥离。

### C29 · 安全命中内容脱敏
`credential-leak` / `pii` 规则命中的内容只存前缀 + 长度 + hash，不存完整匹配值。

---

## 九、流程（Process）

### C30 · 迁移先加后删
数据库迁移必须两阶段：先加可空列 → 代码切换 → 下个版本删旧列。保证滚动更新期间新旧版本共存。

### C31 · spec 引用而非复制
spec 中**不得**复制 `docs/` 的设计内容，只能以 `docs/XX-name.md §N` 形式引用。发现设计需要修改时，**先改 docs 再改 spec**。

### C32 · 拆 crate 需要理由
新增 crate 必须满足四条判据之一（编译期隔离 / 依赖集差异 / 测试密集 / 独立发布）。"这是不同概念"不是理由。
- 依据：`docs/14-rust-project-structure.md` §4

---

## 附：CI 强制清单

| 条款 | 检查方式 |
|---|---|
| C4, C6 | `cargo tree` 断言依赖树 |
| C5 | grep `tee.rs` 中的 `.send(` |
| C7 | grep `Cargo.toml` 的 `panic =` + clippy deny |
| C8 | `#![forbid(unsafe_code)]` + grep |
| C9 | grep `unbounded` |
| C24 | `SplitStrategy` 矩阵测试 |
| C26 | `coverage_matrix` 测试 |
| C27 | `ci/check_log_fields.py` |
| C32 | PR 评审（人工，需引用判据） |
