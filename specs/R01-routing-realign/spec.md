# R01 · 路由模型对齐（修正 F01–F03）

| | |
|---|---|
| ID | R01 |
| 类型 | **修正 spec**（不新增能力，只把已落地的代码对齐到 v2 设计） |
| 里程碑 | M0 |
| 依赖 | F01, F02, F03（均已实现） |
| 主 crate | `argus-core`、`argus-protocol`、`argus-proxy` |
| 设计依据 | `docs/02-routing-model.md`；`docs/04-raw-proxy-mode.md` §5；`docs/08-provider-profiles.md` §4；`docs/09-observers.md` §4；ADR-003/004/005/007/010 |
| 宪法条款 | C1, C2, C6, C21, C27, **C33, C34, C35** |

## 1. 目标

F01–F03 是按 v1 设计实现的。v2 把路由模型从「model → binding → provider」
换成「provider 由 URL 决定、model 完全不透明」，因此 F02 里有一部分类型
**现在是错的**，不是不完整。

本 spec 把它们改对，并补上 v2 新增的核心类型。**不实现任何新功能**——
Resolver、Target Policy、Observer 选择分别是 F04/F05/F06 的事，
这里只提供它们要用的类型。

> **F03（SseFramer）完全不受影响。** 分帧器是协议无关的，v2 反而更依赖它
> （`GenericSse` 兜底走的是同一个 Framer）。R01 不动 `argus-protocol/src/framer/`。

## 2. 范围

**In scope**：删除 `ProviderKind::supports` 矩阵及其测试；`Protocol` →
`ObserverKind`（+2 个兜底变体）；`ProviderKind` → `ProviderType`；
`RouteDecision` → `ResolvedTarget`（删 3 个模型字段，加 origin/credential）；
新增 `ProviderProfileId` / `TargetOrigin` / `CredentialMode`；删除 `BindingId` /
`EndpointId`；`ProxyError` 变体增删；新增两条 CI 检查。

**Out of scope**：URL 解析实现（→ F04）、Target Policy 实现（→ F05）、
Observer 选择实现（→ F06）、`TargetPolicy` 配置类型（→ F11 配置系统）。

## 3. 需求

- **R1** THE SYSTEM SHALL 删除 `ProviderKind::supports(Protocol)` 及其
  35 格逐格断言测试与 `bedrock_invoke_has_no_provider_yet`（ADR-010）。
- **R2** THE SYSTEM SHALL 把 `Protocol` 改名为 `ObserverKind`，并新增
  `GenericSse` 与 `GenericJson` 两个变体（`docs/09` §4.1）。
- **R3** THE SYSTEM SHALL 把 `ProviderKind` 改名为 `ProviderType`，取值扩展到
  `docs/08` §4 的清单，且**不提供任何据其分支的方法**。
- **R4** THE SYSTEM SHALL 用 `ResolvedTarget` 取代 `RouteDecision`，移除
  `logical_model` / `provider_model` / `binding_id`，新增 `origin` / `credential`。
- **R5** THE SYSTEM SHALL 提供 `TargetOrigin { Managed{profile, profile_name}, Raw }`
  与 `CredentialMode { Injected, Passthrough, None }`。
- **R6** THE SYSTEM SHALL 新增 `ProviderProfileId`，删除 `BindingId` 与 `EndpointId`
  （模型绑定与多 endpoint 都已不存在）。
- **R7** `ProxyError` SHALL 删除 `ModelNotAllowed` / `UnknownModel` /
  `ProtocolNotSupported`，新增 `ProviderNotFound` / `ProviderDisabled` /
  `RawProxyNotAllowed` / `InvalidRawTarget` / `SchemeNotAllowed` /
  `EmptyUpstreamPath` / `TargetDenied` / `TargetDnsFailure` /
  `TargetRebindDetected` / `CredentialUnavailable` / `ProviderMisconfigured`。
- **R8** WHEN 授权失败或 profile 不存在 THE SYSTEM SHALL 使两者的
  `http_status()` 与 `error_code()` **完全相同**（`docs/14` §7.2 防枚举）。
- **R9** IF `argus-proxy` 中出现对 `ProviderType` 的穷举 `match` THEN CI SHALL 失败。
- **R10** IF 任何 crate 中出现 `axum::extract::Path` THEN clippy SHALL 失败
  （实测它会把 `%2F` 解成 `/`，`docs/04` §5.2）。
- **R11** THE SYSTEM SHALL 保持 `argus-protocol/src/framer/` 一字不改，
  且 F03 的 97 个测试全部继续通过。

## 4. 接口契约

```rust
// ── observe.rs（新文件）──
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum ObserverKind {
    OpenAiChat, OpenAiResponses, AnthropicMessages, GeminiGenerateContent,
    /// 认不出协议但是 SSE：切帧、记 raw、计数，不解语义
    GenericSse,
    /// 认不出协议的非流式 JSON：记 raw + 顶层字段名
    GenericJson,
}
impl ObserverKind {
    pub const fn as_str(self) -> &'static str;
    pub const fn frame_format(self) -> FrameFormat;
    /// 是否会尝试理解语义。两个 Generic 返回 false
    pub const fn is_semantic(self) -> bool;
    // ⚠️ 没有 supports()。Observer 不参与路由，也就不需要合法性矩阵
}

// ── provider.rs ──
#[non_exhaustive]
pub enum ProviderType {
    OpenAi, Anthropic, AzureOpenAi, Gemini, Bedrock, Vertex,
    DashScope, DeepSeek, SiliconFlow, Moonshot, OpenRouter, Ollama, VLlm,
    Custom,
}
impl ProviderType {
    pub const fn as_str(self) -> &'static str;   // 只用于指标标签与 UI
}

// ── target.rs（新文件）──
pub struct ResolvedTarget {
    pub attempt: u8,
    pub origin: TargetOrigin,
    pub uri: Uri,
    pub credential: CredentialMode,
    pub observer: ObserverKind,
    pub timeouts: Timeouts,
    pub retry: Arc<RetryPolicy>,
    pub observe: ObserveMode,
}

#[derive(Clone, Debug)]
pub enum TargetOrigin {
    Managed { profile: ProviderProfileId, profile_name: CompactString },
    Raw,
}
impl TargetOrigin {
    pub const fn as_str(&self) -> &'static str;   // "managed" | "raw"
    pub const fn is_raw(&self) -> bool;
}

#[derive(Clone)]
pub enum CredentialMode {
    Injected(Arc<ResolvedCredential>),
    Passthrough,
    None,
}
// ⚠️ 手写 Debug：Injected 里有凭据（宪法 C27）
```

## 5. 实现要点

- **`ObserverKind` 不要保留 `supports()` 的任何变体**。有人会想"留一个
  `is_compatible_with(ProviderType)` 吧"——不要。ADR-010 的论证是：
  Observer 选错只导致观测降级，它不该有能力拒绝请求。
- **`CredentialMode` 必须手写 `Debug`**，理由同 `ResolvedCredential`
  （F02 已有先例）：`ResolvedTarget` 是 `derive(Debug)` 的，泄漏会从这里发生。
  加一个 `debug_resolved_target_carrying_a_credential_renders_redacted` 测试。
- **`ProviderType` 只加 `as_str()`**，不加 `default_observer()` /
  `default_auth_header()` 之类。那些是 Profile 上的数据（`docs/08` §5）；
  一旦有了这种方法，`match provider_type` 就会以"只是取默认值"的名义回来。
- R9 的 CI 检查用 grep 即可：在 `crates/argus-proxy/src` 里搜
  `match .*ProviderType|ProviderType::.*=>`。
- R10 用 `clippy.toml` 的 `disallowed-types`，已在 `docs/19` §9.3 记录。

## 6. 任务

- [x] T1 `argus-core/src/observe.rs`：`ObserverKind`（含 2 个 Generic 变体）+ `ObserveMode` 迁入
- [x] T2 删除 `protocol.rs`：`Protocol` / `ProviderKind` / `supports` 矩阵及其 4 个测试
- [x] T3 `provider.rs`：`ProviderType` 取值扩展，只保留 `as_str()`
- [x] T4 `argus-core/src/target.rs`：`ResolvedTarget` / `TargetOrigin` / `CredentialMode`（手写 Debug）
- [x] T5 `ids.rs`：新增 `ProviderProfileId`，删除 `BindingId` / `EndpointId`
- [x] T6 `route.rs`：删除 `RouteDecision`，保留 `Timeouts` / `RetryPolicy` / `Backoff`
- [x] T7 `error.rs`：按 R7 增删变体；R8 的防枚举等价性
- [x] T8 `argus-server/src/main.rs`：自检输出改用新类型
- [x] T9 CI：R9 的 `ProviderType` 穷举 match 检查 + R10 的 `disallowed-types`
- [x] T10 `ci/verify-guards.sh`：为 T9/T10 各加一个注入用例
- [x] T11 更新测试：删除矩阵测试，补 `CredentialMode` 脱敏、防枚举等价性、`is_semantic`

## 7. 验收

```bash
cargo test -p argus-core --all-features --locked
cargo test -p argus-protocol --all-features --locked  # F03 的 97 个测试必须一个不少
./ci/check-invariants.sh
./ci/verify-guards.sh
```

判据要**排除注释**——「这里没有 X，因为 Y」这类说明是有价值的，
不该因为提到了被删的名字就算不合格：

```bash
# 去掉行注释与文档注释后再搜
find crates -name '*.rs' | xargs sed 's|//.*||' \
  | grep -E 'supports\(|logical_model|provider_model|binding_id|EndpointId|BindingId'
```

- [x] 上面这条命令无输出（当前 4 处匹配全在解释性注释里）
- [x] `ProviderKind` / `Protocol` / `RouteDecision` 三个类型名在 `crates/` 中不存在
- [x] `format!("{:?}", resolved_target)` 不含凭据字符，含 `<redacted>`
- [x] `ProxyError::ProviderNotFound` 与授权失败的 `http_status()` / `error_code()` / `to_string()` 完全相同
- [x] `argus-protocol` 自身测试数不变（40 个），F03 一个没丢
- [x] 全 workspace 测试 97 → 103（删 4 个矩阵测试，新增 10 个）
- [x] `verify-guards.sh` 通过数从 24 增至 26

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| URL → `ResolvedTarget` 的解析实现 | F04（重定位为 Target Resolver） |
| Target Policy / SSRF 判定 | F05（重定位） |
| Observer 三级选择与 `observer_rules` | F06（重定位） |
| `TargetPolicy` 的配置结构与热更新 | F11 / F23 |
| Provider Profile 的 CRUD | F22 |
