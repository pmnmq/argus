# ADR-008 · 第一版采用 Modular Monolith（5 个 crate）

- 状态：Accepted · 2026-08-20
- 相关：`19-rust-project-structure.md` §4–5

## Context

设计要求「MVP 优先 Modular Monolith，不要第一版为了架构漂亮拆十几个 crate」，
并给出了单 crate `src/{api,proxy,provider,...}` 的示意布局。而当前实现已经是
5 个 crate 的 workspace。

## Decision

**保留 5-crate workspace**：`argus-core` ← `argus-protocol` ←
{`argus-proxy`, `argus-storage`} ← `argus-server`。

## Rationale

1. 要避免的反模式是「十几个 crate」。5 个不是那个量级，且它们的边界都对应
   一条真实的约束，不是概念分类。
2. **这个拆分是承重的**：`cargo tree -p argus-proxy | grep sqlx` 为空
   是「存储/观测故障不影响 Proxy」的**机械证明**。模块边界做不到这件事——
   同一个 crate 内谁都能 `use crate::storage`。
3. `argus-protocol` 编译期禁止 tokio/hyper/sqlx，保证解析层不可能阻塞。
   这同样只能靠 crate 边界表达。
4. 设计要求里给出的长期形态（server/proxy/observer/security/storage/common）
   本身就是当前 5 个的超集，方向一致。

## Consequences

**得到**：依赖方向可被 CI 断言；改 proxy 不用重编 sqlx；解析层可以微秒级迭代。

**失去**：跨 crate 重构比跨模块重构麻烦；新人要先理解 5 个 crate 的职责。

**放弃的选项**：单 crate（会作废 F01 的整套护栏，换不来正确性）；
拆到 8+ crate（`argus-security` 等留作后续，触发条件见 `19` §6）。
