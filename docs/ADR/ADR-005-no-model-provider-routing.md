# ADR-009 · 不维护模型目录，model 对 Data Plane 不透明

- 状态：Accepted · 2026-08-20
- 相关：`02-routing-model.md` §4.2–4.3

## Decision

删除 `models` / `provider_models` / `model_capabilities` 等全部模型目录表。
`model` 字段原样转发，**不校验、不改写、不查表**。

## Rationale

供应商每周都在上新模型。任何"Argus 侧的模型清单"都会过期，而过期的清单
表现为「用户用不了刚发布的模型，要等 Argus 发版」——这正是本产品要消灭的问题。

Observation 侧仍然可以从 body 里**观测**到 model 并用于审计与指标。
那是旁路，与转发无关。

## Consequences

**得到**：provider 上新模型 Argus 零改动；没有会过期的数据要维护。

**失去**（刻意接受）：
1. Argus **无法**在请求前告诉用户"这个模型名写错了"，只能把上游的 404 转回去。
2. Argus **不再自己算成本**。`cost_micro_usd` 只在上游返回计费信息时填，
   否则留 NULL，由下游用自己的价格表算。

**猜错的校验比没有校验更糟** —— 这是接受上面两条代价的理由。
