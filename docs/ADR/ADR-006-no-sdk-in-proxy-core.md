# ADR-012 · Proxy Core 不使用任何 provider SDK

- 状态：Accepted · 2026-08-20
- 相关：`05-transparent-proxy.md` §12、`20-testing-strategy.md` §9

## Decision

`argus-proxy` 与 `argus-protocol` 的 `[dependencies]` 中**禁止**出现任何
provider SDK。上游客户端用 `hyper` 直接实现，也不用 `reqwest`。

## Rationale

SDK 是 API Client，不是代理。它们会 deserialize、validate、drop unknown
fields、normalize、rebuild request/response —— 每一条都直接破坏透明性与
forward compatibility。

`reqwest` 同理：默认自动解压、自动跟随 redirect，body 类型也不透明。

## SDK 的正确位置

测试 oracle：

```text
录制的真实 SSE ──> Argus Observer ──> 观测结果
                └─> 官方 SDK      ──> 期望结果   ← 两者比对
```

以及 SDK 兼容性测试（把 SDK 的 base_url 指向 Argus）。但测试**不能完全依赖网络**。

## Consequences

**得到**：供应商加字段不需要 Argus 发版；字节级透明可验证。

**失去**：协议细节要自己读文档和抓包；SDK 的类型定义只能当参考。

**强制**：`ci/check-invariants.sh` 断言运行时依赖树中无 SDK（宪法 C4）。
