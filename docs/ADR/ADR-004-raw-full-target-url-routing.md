# ADR-011 · Raw 模式支持两种 target URL 形式

- 状态：Accepted · 2026-08-20
- 相关：`04-raw-proxy-mode.md` §4–6

## Context

Raw 模式要让客户端直接指定完整 target URL。最自然的写法
`/raw/https://api.openai.com/v1/responses` 里有个 `//`，而设计要求明确说了
「不要假设它一定可以无损工作」。

## Decision

接受**两种**形式，解析后产出同一个 target：

- 直接形式 `/raw/{scheme}://{authority}{path}`
- 分段形式 `/raw/{scheme}/{authority}{path}`（全程无连续斜杠）

## Rationale

**实测**（裸 TCP 发请求，绕过客户端规范化）：hyper + axum 完整保留 `//`、
`%2F`、`%2B`、IPv6 方括号、端口、query。**Argus 自己没问题。**

问题在前置组件：nginx 的 `merge_slashes` **默认是 on**。云厂商 LB / CDN
多数会规范化且不可关。只支持直接形式等于把「能不能用」交给部署环境。

分段形式因此不是备选，而是**在不确定前面有什么时的推荐写法**。

## Consequences

**得到**：直接形式好读好粘贴；分段形式能穿过任何规范化组件。

**失去**：解析分支多一条，URL 测试矩阵翻倍；文档要解释清楚何时用哪种。

**放弃**：只支持直接形式（部署脆弱）；base64 编码（不可读，看日志看不出打到哪）；
只用 query（对人不友好，且无法追加 path）。header 形式作为 OQ-W2 保留。
