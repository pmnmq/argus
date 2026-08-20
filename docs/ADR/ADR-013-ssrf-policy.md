# ADR-013 · Target Policy：判定 IP、只解析一次 DNS、连接时确认

- 状态：Accepted · 2026-08-20
- 相关：`13-ssrf-protection.md`

## Context

Raw 模式让 Argus 成为一台可按调用方指令访问任意地址的服务器，而它通常跑在
内网且手握供应商凭据。同时它**必须**能代理 `http://192.168.1.20:11434`
这样的本地模型——所以"私有地址一律拒绝"是错的。

## Decision

1. 判定链：scheme 白名单 → hostname 规则 → **DNS 解析后逐 IP 判定** →
   端口 → **连接时确认实际 IP**。
2. 默认拒绝 loopback / RFC1918 / link-local / ULA / CGNAT / multicast；
   管理员用 `allow_cidrs` 精确放行（最长前缀匹配优先）。
3. **只解析一次 DNS，连接时直接给 IP**；TLS 的 SNI 与证书校验仍用 hostname。
4. 多个 IP 中有任一不合规 → **整体拒绝**。
5. 默认不跟随 redirect；开启时每跳完整重校验、跨 origin 剥凭据、最多 3 跳。
6. 拒绝类错误统一 `403 target_denied`，不回显 IP、不按原因区分状态码。
7. `allow_raw_proxy` + `injected` 凭据必须配 target allowlist（配置期强制）。
8. Managed 模式同样执行 Target Policy。

## Rationale

- 判定 hostname 等于没做——它可以指向任何 IP。
- 校验和连接各解析一次 DNS，中间就是 rebinding 的窗口。
- "挑一个合规的 IP 连"会让攻击者用多值 A 记录碰运气。
- 拒绝原因不同返回不同错误 = Argus 变成内网存活探测器。
- 第 7 条：不加这个约束，调用方可以用你保管的 OpenAI key 打自己的服务器。

## Consequences

**得到**：rebinding 不可绕过；本地模型仍可用；配置错误也能被兜住。

**失去**：每请求一次 DNS 判定（用带 TTL 上限的缓存摊薄，见 `21` §8）；
某些 CDN 的混合 A 记录会被误伤。

**注意**：这不是唯一一道防线。生产应同时用 NetworkPolicy / 安全组限制出站。
