# ADR-014 · OTLP 是导出层，不是 Observation 的替代

- 状态：Accepted · 2026-08-20
- 相关：`16-opentelemetry.md` §4

## Context

Argus 既要回答"AI 内容和行为到底发生了什么"，也要回答"这次调用链发生了什么"。
把两者混成一个系统会导致：要么 prompt 进了 trace attribute（合规灾难），
要么用 trace 去查模型说了什么（数据不全）。

## Decision

两套数据，一个方向的投影：

```text
Observation IR ──> PostgreSQL（内容，权限受控，保留期短）
               └─> OTel Mapper ──> OTLP（元数据，无内容）
```

OTel Mapper 是**单向、有损**的：它挑出元数据，丢掉全部内容。这个有损是设计意图。

## 绝对不进 trace attribute

prompt / system prompt / message 内容 / 响应正文 / reasoning /
tool arguments / tool result / 任何凭据 / 用户可控的长字符串。

允许的是它们的元数据：长度、哈希、条数、token 数、tool 名称、
finish_reason、命中的规则名。

## Rationale

trace 后端的访问控制通常远宽松于审计库，很多团队还把 trace 转发给第三方
SaaS。把 prompt 放进去等于把用户数据交出去。

## Consequences

**得到**：合规边界清晰；OTLP 挂掉不影响审计（反之亦然）；用户可以自选后端。

**失去**：想在 trace 里直接看到内容的人会失望——他们应该去查审计。

**强制**：`ci/check_log_fields.py` 的字段黑名单、`12-security.md` §9 的脱敏规则、
以及本条，是同一套要求在三处的体现。
