# F28 · 打包、性能报告与发布

| | |
|---|---|
| ID | F28 |
| 里程碑 | M4（**门禁 G-M4**） |
| 依赖 | F21, F26 |
| 主 crate | （全部） |
| 设计依据 | `docs/18-deployment.md` §4,§8;`docs/16-opentelemetry.md` §6;`docs/22-mvp-roadmap.md` §3 M4 |
| 宪法条款 | C16, C30 |

## 1. 目标

把系统交付出去，并**用实测数据替换设计文档中所有"目标，未验证"的数字**。

## 2. 范围

**In scope**：静态二进制与容器构建、K8s/Compose manifest、最小 Dashboard、7 天长稳、性能报告与回填、性能回归门禁、迁移兼容检查、用户文档、发布流程。

**Out of scope**：Phase 2 的功能（Gemini/Bedrock/LLM Detector/Redis/OTel/delay_window）。

## 3. 需求 · 构建与分发

- **R1** THE SYSTEM SHALL 产出 musl 静态链接的二进制（无 glibc / OpenSSL 依赖）。
- **R2** 容器镜像 SHALL 基于 `distroless/static`，非 root 运行。
- **R3** THE SYSTEM SHALL 提供 systemd unit（含 `LoadCredential`、`TimeoutStopSec > force_kill_after`）。
- **R4** THE SYSTEM SHALL 提供 Docker Compose（含 PG + 持久化 spool 卷 + `stop_grace_period`）。
- **R5** THE SYSTEM SHALL 提供 K8s manifest（`maxUnavailable: 0`、`terminationGracePeriodSeconds > force_kill_after`、`preStop sleep`）。
- **R6** 构建 SHALL 可复现（`--locked`）。
- **R7** THE SYSTEM SHALL 发布 SBOM 与 `cargo-deny` 报告。
- **R8** IF 依赖树中仍存在 `aws-lc-sys` THEN 构建 SHALL 失败并提示改用 ring 后端。
  R1 的 musl 静态链接与 `aws-lc-sys` 的 C 工具链需求不相容；该问题在 F01
  就已发现并被显式推迟，见 `docs/19` §7.3.1。断言方式：`cargo tree -i aws-lc-sys`
  必须无输出。

## 4. 需求 · 性能回填（G-M4 核心）

- **R8** THE SYSTEM SHALL 在标准环境执行完整压测：100 / 1k / 10k / 50k 并发 × {Full, Lite, Off}。
- **R9** THE SYSTEM SHALL 执行 7 天长稳测试（固定 5k 并发流），验证无内存泄漏与 p99 漂移。
- **R10** THE SYSTEM SHALL 用实测数据回填 `docs/16-opentelemetry.md` §6，把「目标」单栏改为「目标 / 实测」双栏（宪法 C16）。
- **R11** THE SYSTEM SHALL 回填 `docs/17-reliability.md` §4 的资源模型（每流实际内存、CPU）。
- **R12** IF 实测显著偏离目标 THEN THE SYSTEM SHALL 修订目标值并在文档中说明原因，**不得隐藏偏差**（宪法 C16）。
- **R13** THE SYSTEM SHALL 回填以下 Open Question 的实测答案：
  - `docs/10` OQ-1（framer fast path 命中率）、OQ-2（channel 容量）、OQ-5（中文转义下的借用收益）
  - `docs/14` OQ-2（软配额实际超额量，F19 已采集）
  - `docs/15` OQ-6（分区数量对规划的影响，F27 已采集）
  - `docs/16` OQ-1（preflight 是否为 overhead 主要来源）、OQ-4（quanta 是否值得）
  - `docs/17` OQ-1（每流 54 KB 是否准确）、OQ-2（jemalloc 对比）、OQ-6（runtime.split 收益）
  - `docs/18` OQ-1（nano profile 能否在 1C1G 跑）、OQ-5（musl 分配器影响）
  - `docs/20` OQ-1（golden test 总耗时）

## 5. 需求 · 回归门禁与升级

- **R14** THE SYSTEM SHALL 把 criterion 微基准接入 CI，退化 > 20% 时失败。
- **R15** THE SYSTEM SHALL 把端到端 TTFT 基线接入 CI（每日运行，非每 PR）。
- **R16** `ci/check-migration-compat.sh` SHALL 用上一个 release 的二进制验证新 schema（宪法 C30）。
- **R17** THE SYSTEM SHALL 验证滚动升级：新旧版本同时运行时数据面正常。
- **R18** THE SYSTEM SHALL 补齐 `cargo-machete`（未使用依赖）与编译时间门禁。

## 6. 需求 · 文档与 Dashboard

- **R19** THE SYSTEM SHALL 提供最小 Dashboard：请求列表、详情、用量图表（只读）。
- **R20** THE SYSTEM SHALL 提供用户文档：快速开始、配置参考、部署指南、运维手册、故障排查。
- **R21** 用户文档 SHALL 包含 `docs/ADR/ADR-012` §"对文档的要求"中那段**能力边界声明**（安全检测会被跳过、检出率有限、配额是软的、已输出内容无法撤回）。
- **R22** THE SYSTEM SHALL 明示前置代理的必需配置（关闭 buffering、长超时、XFF 信任）。
- **R23** THE SYSTEM SHALL 明示 spool 目录必须实例独占。
- **R24** THE SYSTEM SHALL 明示 master key 的备份要求（**与数据库备份分离存放**）。

## 7. 接口契约

```
dist/
├── argus-x86_64-unknown-linux-musl
├── argus-aarch64-unknown-linux-musl
├── argus.sbom.json
├── deploy/
│   ├── systemd/argus.service
│   ├── compose/docker-compose.yml
│   └── k8s/{deployment,service,secret,configmap}.yaml
└── bench/
    ├── BASELINE.md          # F07 的 M0 基线
    ├── RELEASE-v0.1.md      # 本切片的完整报告
    └── SOAK-7d.md
```

## 8. 实现要点

- **R10/R12 是本切片的核心交付**，不是附属品。设计文档里现在有几十个"目标，未验证"的数字，它们必须在 v0.1 时变成有依据的值。**隐藏偏差比偏差本身更严重。**
- 压测环境要固定并记录（CPU 型号、核数、内存、内核版本、rustc 版本、PG 版本、网络拓扑）。没有环境记录的性能数字没有意义。
- 50k 并发的测试需要 8C16G + `Lite` 模式 + 调低各 cap（`docs/17` §4.2 的必要条件）。如果达不到，**如实记录**并修订 `docs/16` §6.2 的目标。
- R17 的滚动升级验证：起 v0.1 的两个实例 → 迁移到 v0.2 schema → 一个实例升级 → 验证两个版本同时工作 → 升级第二个。
- Dashboard 用最简技术栈（服务端渲染或单页静态文件），**不引入前端构建链**——它是 M4 的收尾项，不是重点。
- R21 的能力边界声明要放在文档显眼位置（README + 安全章节开头），不能藏在附录。

## 9. 任务

- [ ] T1 musl 交叉编译 + 多架构（x86_64 / aarch64）
- [ ] T2 distroless Dockerfile + 多架构镜像
- [ ] T3 systemd unit / Compose / K8s manifest
- [ ] T4 SBOM + `cargo-deny` 报告生成
- [ ] T5 标准压测环境搭建与记录
- [ ] T6 完整压测：4 个规模 × 3 个模式
- [ ] T7 7 天长稳（5k 并发）+ RSS/p99 曲线
- [ ] T8 **回填 `docs/16` §6**（目标 / 实测双栏）
- [ ] T9 **回填 `docs/17` §4** 资源模型
- [ ] T10 **回填 R13 列出的 12 个 Open Question**
- [ ] T11 criterion 回归门禁接入 CI
- [ ] T12 端到端 TTFT 基线（每日 job）
- [ ] T13 `ci/check-migration-compat.sh` 完善 + 滚动升级验证
- [ ] T14 `cargo-machete` + 编译时间门禁
- [ ] T15 最小 Dashboard
- [ ] T16 用户文档（含 R21–R24 的四项声明）
- [ ] T17 发布流程（tag / changelog / artifact）

## 10. 验收 — 门禁 G-M4

```bash
./ci/release-check.sh
```

- [ ] 二进制在纯净容器中可运行（无任何动态库依赖，`ldd` 确认 static）
- [ ] 镜像 < 40 MB，非 root，无 shell
- [ ] `argus doctor` 在三种部署形态下全绿
- [ ] **7 天长稳：RSS 曲线无单调增长**（线性回归斜率 ≈ 0）
- [ ] **7 天长稳：p99 无漂移**（首尾各 24 小时的 p99 差异 < 10%）
- [ ] **`docs/16` §6 中不再有任何"未验证"标记**（宪法 C16）
- [ ] R13 的 12 个 OQ 全部有实测答案或明确的"仍未验证 + 原因"
- [ ] 滚动升级验证通过（新旧版本共存期间数据面零错误）
- [ ] criterion 门禁能拦住人为注入的 30% 退化
- [ ] 用户文档包含 R21 的能力边界声明、R22 的代理配置、R23 的 spool 独占、R24 的 master key 备份
- [ ] 从零开始按快速开始文档部署，30 分钟内跑通第一个请求

## 11. 交给后续（Phase 2，发布后）

| 项 | 依据 |
|---|---|
| Gemini `generateContent` | `docs/09` §9.1 |
| Bedrock（`EventStreamFramer` + SigV4） | `docs/09` §9.2 |
| Vertex（OAuth2） | `docs/09` §4.3 |
| LLM Detector | `docs/12` §7.4 |
| `delay_window` | `docs/12` §6.3 |
| Redis（限流 / 熔断 / taint 共享） | `docs/14` §8.5、`docs/08` §10 |
| OpenTelemetry | `docs/16` §8 |
| `LeastLatency` 路由 | `docs/08` §12-B |
| 流式增量 JSON 检测 | `docs/11` §12-D |
| `argus-security` 拆 crate（若达触发条件） | `docs/19` §6 |
