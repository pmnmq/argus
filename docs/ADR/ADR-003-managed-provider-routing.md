# ADR-010 · Provider 由 URL 路径显式选择

- 状态：Accepted · 2026-08-20
- 相关：`02-routing-model.md` §4、`03-managed-provider-mode.md`

## Context

请求进来后要决定发往哪个上游。两条路：从 body 的 `model` 字段推断，
或从 URL 里显式读出。

## Decision

`POST /provider/{profileName}/{upstreamPath}`。**provider 永远由 URL 决定。**

## Rationale

同名模型在不同 provider 上价格、量化、参数支持、上下文长度、版本都可能不同；
模型名本身还可能含 `/`（DashScope 的 `kimi/kimi-k3`）。要从 model 推断就必须
维护一张归一化映射表，而这张表：每次上新模型都要更新、更新不及时用户就用不了、
归一化规则会和某家的命名习惯冲突——**而且它解决的问题是自己创造的。**

## Consequences

**得到**：同一 URL 永远打到同一上游；零模型目录；provider 上新模型 Argus 零改动。

**失去**：客户端要为每个 provider 配一次 base_url（或换 path）。

**放弃**：header 指定（`X-Argus-Provider`，保留为兼容手段）；
子域名（需要泛域名证书，抬高自建门槛）。
