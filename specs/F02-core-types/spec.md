# F02 · 核心类型与错误

| | |
|---|---|
| ID | F02 |
| 里程碑 | M0 |
| 依赖 | F01 |
| 主 crate | `argus-core` |
| 设计依据 | `docs/06-data-plane.md` §10；`docs/09-observers.md` §4,§10；`docs/00-overview.md` §6.4 |
| 宪法条款 | C6, C8, C21 |


> ⚠️ **本 spec 已被 R01 修正。** v2 的路由模型下，`ProviderKind::supports`
> 矩阵、`RouteDecision` 的三个模型字段、`Protocol` 这个名字都不再正确。
> **实现时以 `specs/R01-routing-realign/spec.md` 为准**，本文件保留是为了
> 让 R01 的 diff 有对照。
## 1. 目标

定义被所有层共享的类型。这些类型一旦定下，后续 26 个 spec 都在它们之上工作，因此**命名必须与 `docs/00` §6 术语表逐字一致**。

## 2. 范围

**In scope**：`Protocol`、`ProviderKind`、`TraceContext`、`ConversationLink`、强类型 ID、`TeeChunk`/`TeeEnd`、`Timeouts`、`RouteDecision`、`ProxyError` 与错误分类。

**Out of scope**：`ConfigSnapshot`（→ F11/F23）、`ObservedEvent`（→ F04）、`InteractionSink`（→ F04，在 `argus-protocol`）、`PrincipalCtx`（→ F13）。

## 3. 需求

- **R1** THE SYSTEM SHALL 提供 `Protocol` 枚举，取值与 `docs/09` §4.1 完全一致，且标注 `#[non_exhaustive]`。
- **R2** THE SYSTEM SHALL 提供 `ProviderKind` 枚举，取值与 `docs/09` §4.2 一致，`#[non_exhaustive]`。
- **R3** THE SYSTEM SHALL 用 newtype 包裹所有 ID（`WorkspaceId`、`ApiKeyId`、`ProviderId`、`EndpointId`、`BindingId`、`CredentialId`），禁止裸 `Uuid`/`String` 互串。
- **R4** THE SYSTEM SHALL 提供 `TraceContext { request_id, trace_id, conversation_id, conversation_link }`，`request_id` 用 UUIDv7。
- **R5** THE SYSTEM SHALL 提供 `TeeChunk { data: Bytes, gap_before: u64 }` 与 `TeeEnd` 枚举。
- **R6** THE SYSTEM SHALL 提供四段式 `Timeouts { connect, headers, idle_frame, total }`（`docs/06` §10）。
- **R7** THE SYSTEM SHALL 提供 `ProxyError`，且每个变体能映射到 (a) HTTP status (b) 是否可 retry (c) 所处的 `UpstreamStage`。
- **R8** WHEN 需要在指标标签中使用 `Protocol` / `ProviderKind` THE SYSTEM SHALL 提供返回 `&'static str` 的方法（宪法 C21：热路径零分配）。
- **R9** THE SYSTEM SHALL 让 `ResolvedCredential` 的 `Debug`/`Display` 输出 `<redacted>`，且实现 `Drop` 时 zeroize。
- **R10** `argus-core` SHALL NOT 依赖任何内部 crate（宪法 C6）。

## 4. 接口契约

```rust
// ── protocol.rs ──
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Protocol { OpenAiChat, OpenAiResponses, AnthropicMessages, GeminiGenerateContent, BedrockInvoke }

impl Protocol {
    /// 指标标签用，零分配（宪法 C21）
    pub const fn as_str(self) -> &'static str { .. }
    /// 该协议的流式帧格式
    pub const fn frame_format(self) -> FrameFormat { .. }   // Sse | AwsEventStream | None
}

#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ProviderKind { OpenAi, AzureOpenAi, OpenAiCompatible, Anthropic, AnthropicCompatible, Bedrock, Vertex }

impl ProviderKind {
    pub const fn as_str(self) -> &'static str { .. }
    /// docs/09 §4.3 的合法组合矩阵
    pub fn supports(self, p: Protocol) -> bool { .. }
}

// ── ids.rs ──
macro_rules! id_newtype { ($name:ident) => { .. } }
id_newtype!(WorkspaceId); id_newtype!(ProviderId); /* … */

#[derive(Clone, Debug)]
pub struct TraceContext {
    pub request_id: Uuid,                 // UUIDv7
    pub trace_id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub conversation_link: ConversationLink,
}
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ConversationLink { Explicit, Inferred, None }

// ── tee.rs ──
#[derive(Clone, Debug)]
pub struct TeeChunk { pub data: Bytes, pub gap_before: u64 }

#[derive(Clone, Debug)]
pub enum TeeEnd { Eof, ClientAborted, UpstreamError(CompactString), Cancelled(CancelReason) }

#[derive(Copy, Clone, Debug)]
pub enum CancelReason { Shutdown, Deadline, SecurityBlock, ClientGone }

// ── error.rs ──
#[derive(Debug, thiserror::Error)]
pub enum ProxyError { /* … */ }

impl ProxyError {
    pub fn http_status(&self) -> StatusCode;
    pub fn error_code(&self) -> &'static str;      // "invalid_api_key" / "model_not_allowed" / …
    pub fn stage(&self) -> UpstreamStage;          // S0 | S1 | S2 | S3 | Local
    pub fn is_retryable(&self) -> bool;            // 必须遵守宪法 C18：S3 恒为 false
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum UpstreamStage { Local, S0NotSent, S1Sent, S2Headers, S3Committed }
```

## 5. 实现要点

- `Protocol::supports` 的矩阵直接编码 `docs/09` §4.3 的表，**用 `const fn` + match**，不查表不分配。
- `ProxyError::is_retryable()` 对 `stage() == S3Committed` 必须**无条件返回 false**，且写一个测试断言这一点（宪法 C18 不可配置）。
- ID newtype 用宏生成，统一派生 `Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize`。
  **不派生 `sqlx::Type`**：`argus-core` 不得依赖 sqlx（宪法 C6），而孤儿规则又让
  `argus-storage` 无法给外部类型实现外部 trait。改为提供 `as_uuid()` / `From<Uuid>`，
  存储层在绑定 SQL 参数时显式转换 —— 边界因此是看得见的。
- `TeeChunk` 故意不实现 `Copy`（`Bytes` 是引用计数，clone 要显式）。

## 6. 任务

- [x] T1 `protocol.rs`：`Protocol` / `ProviderKind` / `FrameFormat` + `as_str` + `supports` 矩阵
- [x] T2 `ids.rs`：`id_newtype!` 宏 + 6 个 ID + `TraceContext` / `ConversationLink`
- [x] T3 `tee.rs`：`TeeChunk` / `TeeEnd` / `CancelReason`
- [x] T4 `route.rs`：`Timeouts` / `RouteDecision` / `RetryPolicy` 骨架（字段照 `docs/06` §10）
- [x] T5 `provider.rs`：`ResolvedCredential`（含 redacted Debug + Drop zeroize）
- [x] T6 `error.rs`：`ProxyError` + `UpstreamStage` + 四个映射方法
- [x] T7 单元测试：`supports` 矩阵逐格断言、S3 不可 retry、credential 不泄漏

## 7. 验收

```bash
cargo test -p argus-core
cargo tree -p argus-core -e normal | grep -Eq 'argus-(protocol|proxy|storage|server)' && exit 1  # C6
```

- [x] `Protocol::supports` 的测试覆盖 `docs/09` §4.3 表格的**每一格**（7 kind × 5 protocol = 35 格）
- [x] `assert!(!err.is_retryable())` 对所有 `stage() == S3Committed` 的构造成立
- [x] `format!("{:?}", credential)` 不含密钥字符，`format!("{}", credential)` 同理
- [x] `argus-core` 依赖树中无内部 crate

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| `ConfigSnapshot` / `ArcSwap` | F11（结构）、F23（热更新） |
| `PrincipalCtx` / `EffectiveLimits` | F13 |
| `RouteDecision` 的实际填充逻辑 | F14 |
| `ErrorRenderer`（按协议渲染错误体） | F14 |
