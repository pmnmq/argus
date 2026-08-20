# ADR-007 · Provider Profile 是路由单位，Provider Type 只做模板

- 状态：Accepted · 2026-08-20
- 相关：`08-provider-profiles.md` §4

## Decision

引入 **Provider Profile**：一个具名的上游接入点（base_url + credential +
policy + observer 默认值）。`provider_type` **仅用于 UI 模板与指标分组**，
Data Plane 禁止据此分支。

## Rationale

同一个供应商需要多套接入配置是常态：`openai-prod` / `openai-dev` 的
base_url 相同，但凭据、配额、安全策略、审计归属都不同。若 Profile 名必须
等于公司名，这些就没地方放。

反过来，把认证形状写进代码（`match provider_type`）意味着每加一个供应商
就要发版。Azure 那种"同一 type 不同 URL 形状"会立刻打脸。因此认证是**数据**：
`credential.header` + `credential.format` + `extra_headers` 三个字段。

## Consequences

**得到**：加供应商 = 加一条配置；同供应商可多套策略。

**失去**：用户要理解"Profile ≠ 供应商"；算出来的凭据（SigV4/OAuth2）
要等 Phase 2 以新的 `CredentialMode` 变体接入。

**强制**：CI 检查 `argus-proxy` 里不得对 `ProviderType` 穷举 match。
