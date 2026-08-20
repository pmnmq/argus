# R02 · Target Resolver（URL → ResolvedTarget）

| | |
|---|---|
| ID | R02 |
| 类型 | v2 新增能力，**执行顺序在 F03 之后、F04 之前** |
| 里程碑 | M0 |
| 依赖 | R01 |
| 主 crate | `argus-proxy::ingress` |
| 设计依据 | `docs/03-managed-provider-mode.md` §4–7；`docs/04-raw-proxy-mode.md` §4–5；`docs/06-data-plane.md` §6.1 |
| 宪法条款 | C1, C2, C21, **C33, C35** |

## 1. 目标

把进来的 URL 变成 `ResolvedTarget`，**且不改变一个字节**。

这是全项目第二个"看着简单实际全是坑"的任务（第一个是 SseFramer）。
它的失败方式是**静默的**：上游收到一个略有不同的 path，返回 404，
而日志里看起来一切正常。

## 2. 范围

**In scope**：`/provider/{name}/…` 与 `/raw/…` 两个解析器；按原始字节切分；
两种 Raw URL 形式；query 透传；profile 名校验。

**Out of scope**：Target Policy 判定（→ R03）、Observer 选择（→ R04）、
Profile 的加载与缓存（→ F11）。

## 3. 需求

- **R1** THE SYSTEM SHALL 只从 `req.uri().path()` 与 `.query()` 读取路径信息，
  **禁止**使用 `axum::extract::Path` 或任何会百分号解码的提取器。
- **R2** WHEN 路径以 `/provider/` 开头 THE SYSTEM SHALL 按**字节**切出第一个
  path 段作为 profile 名，其余原样作为 upstream path。
- **R3** THE SYSTEM SHALL 用 `rstrip(base_url,'/') + '/' + lstrip(rest,'/')` 拼接，
  且这是**唯一**的拼接规则。
- **R4** THE SYSTEM SHALL 原样透传 query，不解析成 map、不重新序列化。
- **R5** THE SYSTEM SHALL NOT 做 path 规范化：不折叠 `//`、不解 `.`/`..`、不改大小写。
- **R6** WHEN 路径为 `/provider/{name}`（无尾斜杠、无 path）THE SYSTEM SHALL
  返回 400 `empty_upstream_path`，**不默认补 `/`**。
- **R7** THE SYSTEM SHALL 对 profile 名的**原始字节**做字符集校验
  （`^[a-z0-9][a-z0-9-]{0,62}$`），**全程不 decode**。`%` 不在字符集内，
  `%6Fpenai` 因此直接 400（`docs/03` §7、`docs/04` §5.3）。
- **R8** THE SYSTEM SHALL 接受两种 Raw 形式：`/raw/{scheme}://{authority}{path}`
  与 `/raw/{scheme}/{authority}{path}`，产出**同一个** target。
- **R9** IF Raw target 缺少 scheme 或 scheme 不在白名单 THEN SHALL 返回 400，
  **不默认补 `https`**。
- **R10** THE SYSTEM SHALL 正确处理 IPv6 字面量（`[2001:db8::1]:8000`）。
- **R11** 对任意输入 THE SYSTEM SHALL NOT panic。
- **R12** IF Raw 的 authority 为空或含 userinfo THEN SHALL 返回 400
  `invalid_raw_target`（`docs/04` §4.4）。
- **R13** WHEN Raw target 没有 path（`/raw/https://host`）THE SYSTEM SHALL
  转发到根路径，**不**报 `empty_upstream_path` —— 那条只对 Managed 成立，
  理由见 `docs/04` §4.4 末段。

## 4. 接口契约

```rust
pub trait TargetResolver {
    /// 只读原始字节。返回值里的 uri 已经是完整上游地址
    fn resolve(&self, parts: &Parts, profiles: &dyn ProfileRegistry)
        -> Result<ResolvedTarget, ProxyError>;
}
pub struct ManagedResolver;
pub struct RawResolver;

/// Resolver 需要的 Profile 字段，**只有这些**。
/// F11 落地 `ConfigSnapshot` 之后由它实现本 trait。
pub trait ProfileRegistry {
    fn lookup(&self, name: &str) -> Option<ProfileView<'_>>;
}
pub struct ProfileView<'a> { /* id / name / base_url / enabled / credential / … */ }

/// 按字节切第一个 path 段。不 decode
pub fn split_first_segment(tail: &str) -> Option<(&str, &str)>;

/// profile 名字符集校验，直接作用在**原始字节**上（`docs/03` §7）
pub fn is_valid_profile_name(raw: &str) -> bool;

/// 判别 Raw 的两种形式
pub enum RawForm { Direct, Segmented }
pub fn detect_raw_form(rest: &str) -> Result<(RawForm, Scheme, &str), ProxyError>;
```

> **为什么不是 `ConfigSnapshot`**：配置系统属于 F11（M1），而 R02 在 M0。
> Resolver 真正需要的只有「按名字拿到 base_url 与几项策略」，把它收成一个
> trait，R02 就能自洽收口而不必把 F11 提前拽进 M0。F11 到时候给
> `ConfigSnapshot` impl 一下即可，Resolver 一行不用改。

## 5. 实现要点

- **R1 是本 spec 的核心。** 实测 `axum::extract::Path` 会把 `%2F` 解成 `/`，
  路径段数都变了（`docs/04` §5.2）。clippy 的 `disallowed-types` 已在 R01 加上，
  但**还要有一个真的会失败的测试**，而不是只靠 lint。
- 拼接不要用 `Url::join()`。它遵循 RFC 3986 的相对引用规则，
  `join("v1/x")` 会**替换掉** base 的最后一段。这不是我们要的语义。
- query 从 `uri.query()` 取，那是未解码的原串，直接拼。
- 分段形式的 authority 里可能有 `:port`，切分时要在第一个 `/` 处停，
  但 IPv6 的 `[::1]:8000` 里没有 `/`，注意别被方括号里的 `:` 误导。

## 6. 任务

- [ ] T1 `ingress/path.rs`：`split_first_segment` + profile 名校验（纯函数，好测）
- [ ] T2 `ingress/managed.rs`：`ManagedResolver` + 拼接规则
- [ ] T3 `ingress/raw.rs`：`detect_raw_form` + 两种形式解析
- [ ] T4 query 透传
- [ ] T5 IPv6 / 端口 / userinfo 拒绝
- [ ] T6 `docs/20` §5.1 的 URL 解析矩阵，逐格实现
- [ ] T7 property test：任意字节不 panic；两种 Raw 形式产出相同 target
- [ ] T8 一个**会失败的**测试证明不能用 `Path` 提取器（而不是只靠 lint）

## 7. 验收

```bash
just test -p argus-proxy resolver
```

- [ ] `docs/20-testing-strategy.md` §5.1 的矩阵全绿
- [ ] `/provider/x/v1/models/gpt%2F4` 解析出的 upstream path 仍含 `%2F`
- [ ] `/provider/x/v1//foo` 的 `//` 保留
- [ ] 两种 Raw 形式对同一 target 产出 byte-identical 的 `uri`
- [ ] `/provider/x` → 400；`/raw/api.openai.com/v1` → 400
- [ ] proptest 10 万随机 path 不 panic

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 判定 target 是否允许连接 | R03 |
| Observer 选择 | R04 |
| nginx/Traefik 后的实测 | F21（混沌与集成） |
