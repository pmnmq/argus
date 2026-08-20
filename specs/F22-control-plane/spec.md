# F22 · Control Plane API 与 Credential 加密

| | |
|---|---|
| ID | F22 |
| 里程碑 | M3 |
| 依赖 | F12 |
| 主 crate | `argus-server` + `argus-storage` |
| 设计依据 | `docs/07-control-plane.md` §4–8 |
| 宪法条款 | C19, C20, C27, C31 |

## 1. 目标

让 Argus 可管理。**关键约束：Control Plane 完全不可用时，Data Plane 必须继续用最后的快照服务。**

## 2. 范围

**In scope**：Admin API（独立端口 + 独立池）、Admin 认证与 scope、Credential 加解密与轮换、资源 CRUD、语义校验、`dry_run`/`If-Match`、`config apply/export`、`/admin/diagnostics`、`audit_events`。

**Out of scope**：配置热更新与 `ArcSwap`（→ F23）、审计查询（→ F26）、Dashboard UI（→ F28）。

## 3. 需求 · 隔离

- **R1** Control Plane SHALL 监听独立端口（默认 `127.0.0.1:9900`）。
- **R2** Control Plane SHALL 使用独立的 `PgPool`（更小的 `max_connections` + `statement_timeout`）。
- **R3** THE SYSTEM SHALL 支持 `role = all | data | control`；`role = data` 时不监听 admin 端口。
- **R4** WHEN Control Plane 完全不可用 THE Data Plane SHALL 继续服务（宪法 C19）。

## 4. 需求 · 认证与授权

- **R5** Data Plane 的 API Key SHALL NOT 可用于 Control Plane（前缀与校验路径完全分离）。
- **R6** THE SYSTEM SHALL 支持 Admin Session（登录）与 Admin Token（`at-argus-...`，用于 CI/IaC）。
- **R7** THE SYSTEM SHALL 实现 `docs/07` §6.2 的 scope 集合。
- **R8** `audit:read_content` SHALL 是独立的高敏 scope，**不包含在任何默认角色中**。
- **R9** WHEN 使用 `audit:read_content` THE SYSTEM SHALL 记录一条 `audit_events`（访问内容本身要被审计）。
- **R10** 用户密码 SHALL 用 argon2id 哈希（与 Data Plane 的 API Key 不同——密码是低熵的）。

## 5. 需求 · Credential

- **R11** Provider credential SHALL 用 master key（**不入库**）AES-256-GCM 加密（宪法 C27）。
- **R12** THE SYSTEM SHALL NOT 提供任何返回明文 credential 的 API。
- **R13** THE SYSTEM SHALL 暴露 `fingerprint = base64(sha256(plaintext))[..12]` 用于识别。
- **R14** THE SYSTEM SHALL 支持 `master_key_previous`：解密先试当前后试 previous，加密恒用当前。
- **R15** THE SYSTEM SHALL 提供 `argus credentials rewrap` 用新 key 重新加密全部 credential。
- **R16** WHEN 启动 THE SYSTEM SHALL 验证 master key 能解密至少一个已存在的 credential；失败则拒绝启动（宪法 C20）。
- **R17** `ResolvedCredential` SHALL 预构造 `HeaderValue`，避免每请求 `format!`（宪法 C21）。

## 6. 需求 · CRUD 与校验

- **R18** THE SYSTEM SHALL 提供 `docs/07` §5.1 的全部资源端点。
- **R19** 写操作 SHALL 支持 `?dry_run=true`（完整校验，不落库）。
- **R20** 写操作 SHALL 要求 `If-Match: <etag>`（乐观并发）。
- **R21** 写操作 SHALL 产生 `audit_events`（actor / 动作 / 资源 / diff）。
- **R22** THE SYSTEM SHALL 做**语义校验**，尤其是：`credential.mode=injected` 时 `header`/`format` 必填；`allow_raw_proxy` + `injected` 必须配非空 target allowlist（`docs/13` §9）。**不做**「model 是否被该 provider 支持」这类校验——模型目录不存在。
- **R23** `capability_overrides` SHALL 只允许收窄；放宽的配置必须被拒绝。
- **R24** 校验失败的错误信息 SHALL 可操作（指出问题 + 给出正确做法）。
- **R25** THE SYSTEM SHALL 提供 `argus config apply/export`；`credential_ref: env:XXX` 让配置可进 Git。
- **R26** `config export` SHALL NOT 输出 credential 明文。
- **R27** THE SYSTEM SHALL 提供 `/admin/diagnostics`（`docs/07` §5.4 的字段）。
- **R28** `base_url` SHALL 形如 `scheme://authority[/path-prefix]`：scheme 在
  白名单内，authority 非空，且**不得**含 query、fragment 或 userinfo。这三样
  会在拼接时静默毁掉结果（`docs/03` §5 的表），而 Data Plane 刻意不重复校验
  —— 拦不下来就只能在生产里以「上游 404」的形式暴露。

## 7. 接口契约

```rust
// argus-server/src/admin/mod.rs
pub fn router(state: AdminState) -> axum::Router;   // 挂载到独立端口

// argus-storage/src/crypto.rs
pub struct MasterKey([u8; 32]);
impl MasterKey {
    pub fn load(cfg: &CredentialsConfig) -> Result<(Self, Option<Self>), KeyError>;  // current, previous
    pub fn seal(&self, plaintext: &[u8]) -> (Vec<u8>, [u8; 12]);
    pub fn open(&self, ct: &[u8], nonce: &[u8; 12]) -> Result<Zeroizing<Vec<u8>>, KeyError>;
}

// argus-storage/src/repo/mod.rs
pub trait Repo<T> {
    async fn get(&self, id: &T::Id) -> Result<(T, Etag), RepoError>;
    async fn upsert(&self, v: &T, etag: Option<&Etag>, dry_run: bool) -> Result<Etag, RepoError>;
    async fn delete(&self, id: &T::Id, etag: &Etag) -> Result<(), RepoError>;
}

// argus-server/src/admin/validate.rs
pub fn validate_profile(p: &ProviderProfile, tp: &TargetPolicy)
    -> Result<(), Vec<ValidationProblem>>;    // R22/R23
```

```bash
argus config apply -f argus-config.yaml [--dry-run]
argus config export -o argus-config.yaml
argus credentials rewrap
argus keygen master
```

## 8. 实现要点

- **R22 是 Argus 的核心价值之一**：在配置时就阻止不可能工作的组合，而不是等运行时报错。错误信息照 `docs/07` §5.2 的样式，指出正确的 endpoint。
- `Etag` 用行的 `xmin` 或一个显式 `version` 列。前者更简单但与 PG 耦合；推荐显式 `version BIGINT`。
- **R12 的贯彻要靠类型**：`ResolvedCredential` 不实现 `Serialize`；`Debug`/`Display` 手写为 `<redacted>`。CI 加一条 grep 检查 admin handler 中不出现 credential 的序列化。
- `config apply` 是幂等 upsert，用 `name` 作自然键。**不做双向同步**（`docs/07` §8）。
- `/admin/diagnostics` 的价值在排障：字段要直接对应 `docs/16` 的指标，让"看一眼就知道哪里不对"。
- Admin API 的 handler 里**不得**引用 `argus-proxy` 的类型（保持 role 分离时能编译）。

## 9. 任务

- [ ] T1 `crypto.rs`：`MasterKey` 加载（含权限检查）、seal/open、previous key
- [ ] T2 `credentials rewrap` + `keygen master` CLI
- [ ] T3 启动时的 credential 解密验证（R16），接入 F15 的 boot
- [ ] T4 `admin/auth.rs`：Admin Session + Admin Token + scope 检查
- [ ] T5 `admin/audit.rs`：`audit_events` 记录中间件（含 R9）
- [ ] T6 `repo/`：各资源的 CRUD + `Etag` 乐观并发
- [ ] T7 `admin/validate.rs`：R22/R23 + 可操作错误信息
- [ ] T8 `admin/handlers/`：`docs/07` §5.1 的全部端点 + `dry_run`
- [ ] T9 `admin/diagnostics.rs`
- [ ] T10 `config apply/export`（YAML + `env:` 引用）
- [ ] T11 独立端口 + 独立池 + `role` 分离
- [ ] T12 测试：§10

## 10. 验收

```bash
cargo test -p argus-server admin
cargo test -p argus-storage crypto repo
```

- [ ] **C19 断言**：停掉 admin 端口/池，Data Plane 请求成功率不变
- [ ] `role = data` 启动时不监听 9900
- [ ] Data Plane 的 `sk-argus-...` 调 admin API → 401
- [ ] 默认角色不含 `audit:read_content`；使用它时产生 `audit_events` 行
- [ ] credential 写入后，任何 API 都无法读回明文（遍历所有端点断言）
- [ ] `fingerprint` 相同的两次写入产生相同指纹
- [ ] master key 轮换：previous 可解密旧数据，rewrap 后 previous 可移除
- [ ] master key 错误 → 启动被拒（R16）
- [ ] **R22 关键用例**：给 `claude-sonnet`（anthropic_messages）绑定 OpenAI provider → 400 且错误信息含正确指引
- [ ] `capability_overrides` 尝试放宽 → 400
- [ ] `dry_run=true` 不产生任何写入
- [ ] `If-Match` 不匹配 → 409
- [ ] `config export` 输出中不含任何密钥字符
- [ ] `config apply` 幂等（连跑两次结果相同）
- [ ] admin 池耗尽不影响 data 池（并发压测断言）

## 11. 交给后续

| 不做的事 | 承接 |
|---|---|
| `ConfigSnapshot` 热更新 | F23 |
| 审计查询端点 | F26 |
| 安全策略与规则的 CRUD | F24（表结构本切片建好） |
| Dashboard UI | F28 |
| OIDC / SAML | Phase 3 |
