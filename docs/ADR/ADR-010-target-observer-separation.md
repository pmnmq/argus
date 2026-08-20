# ADR-010 · Target 解析与 Observer 选择彻底分离

- 状态：Accepted · 2026-08-20（取代 v1 的 ADR-010「Protocol ⊥ ProviderKind」）
- 相关：`02-routing-model.md` §5、`09-observers.md` §4

## Context

v1 把 `Protocol` 当成路由键，并维护一张 `ProviderKind × Protocol` 的合法性矩阵，
配置期用它拒绝无意义的组合。新路由模型下 provider 由 URL 决定，这张矩阵
既不再被需要，也不再正确。

## Decision

两件事彻底分开：

```text
Target 解析：URL → 发到哪里      （P1，决定主链路行为）
Observer 选择：用哪套语义理解字节  （旁路，不影响主链路）
```

`ProviderKind` 降级为 `ProviderType`（仅 UI 模板）；`Protocol` 降级为
`ObserverKind`（仅观测），**`ProviderKind::supports(Protocol)` 矩阵删除**。

## Rationale

1. 一个 Profile 可以同时提供多种形状的 endpoint
   （`/v1/chat/completions` 与 `/v1/responses`），把 Observer 挂死在 Profile 上
   会立刻崩。
2. 同一种形状横跨多个 Profile（`AnthropicMessages` 出现在 Anthropic、Bedrock、
   Vertex、以及任何 anthropic-compatible 的自建服务上）。
3. Observer **选错的后果只是观测降级**，主链路一个字节都不受影响。
   既然如此，就不该让它有能力拒绝一个请求。

## Observer 的三级判定

```text
observer_rules 命中  →  Profile 的默认值  →  按 target host/path 猜
                                          →  按 Content-Type 兜底
                                             （GenericSse / GenericJson）
```

新增的 `GenericSse` / `GenericJson` 是关键：**认不出协议不等于不观测**。
Raw 模式和未知供应商仍然有字节数、事件数、耗时、raw 留存。

## Consequences

**得到**：加 endpoint 不用改矩阵；Raw 模式天然有兜底观测；
Observer 出错不可能影响转发。

**失去**：配置期不再能拦下"这个 provider 不支持这个协议"——但那本来也是
一个会过期的判断。

**要删的代码**：`ProviderKind::supports` 及其 35 格逐格断言的测试（见 R01）。
