# F24 · 安全规则引擎与 PreflightGuard

| | |
|---|---|
| ID | F24 |
| 里程碑 | M3 |
| 依赖 | F09 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/12-security.md` §5–8,§10；`docs/ADR/ADR-012-fail-open-observation.md` |
| 宪法条款 | C15, C17, C21, C29 |

## 1. 目标

实现确定性的规则引擎与请求时检查。**安全模块的失效不得损害可用性**——这是它能被部署的前提。

## 2. 范围

**In scope**：规则模型、Layer 0–2 匹配器、`SecurityExtractor`（容错）、`PreflightGuard`、判定合并、策略绑定、`suppressions`、内置规则集、`force_full_observation`。

**Out of scope**：ToolGate（→ F25）、安全事件落库与查询（→ F26）、LLM Detector（→ Phase 2）、`delay_window`（→ Phase 2）。

## 3. 需求 · 引擎

- **R1** THE SYSTEM SHALL 实现 `docs/12` §7.2 的 `Rule` / `RuleTarget` / `Matcher` 模型。
- **R2** THE SYSTEM SHALL 实现 Layer 0–2 的匹配器：结构规则、Aho-Corasick、RegexSet、熵检测、指纹、shell 命令分析、工具白名单。
- **R3** Layer 0–2 SHALL 是确定性的（不依赖 LLM）。
- **R4** THE SYSTEM SHALL 实现 `docs/12` §7.3 的内置规则集（11 个规则族）。
- **R5** `SecurityAction` SHALL 实现 `Ord`：`Allow < Flag < Warn < RequireReview < Block`。
- **R6** 多规则命中 SHALL 取最严 action，但保留全部 `hits` 用于解释（G-Sec5）。
- **R7** THE SYSTEM SHALL 支持 `Policy.suppressions`：抑制指定 `rule_id`，15 秒内全网生效，**无需发版**。
- **R8** 规则集编译 SHALL 有模式总数上限（默认 10k）；超限则拒绝该规则集并**保留旧的**。
- **R9** IF 单条规则编译失败 THEN THE SYSTEM SHALL 跳过该规则、继续加载其余（fail-open，宪法 C17）。

## 4. 需求 · 提取器

- **R10** `SecurityExtractor` SHALL 提取 `docs/12` §5.1 的请求内容（system / user / tool 定义 / tool 结果 / 附件元数据）。
- **R11** IF 结构化提取失败 THEN THE SYSTEM SHALL 降级为 `RawFallback`（在原始 body 文本上跑通用规则），**绝不拒绝请求**（宪法 C17）。
- **R12** WHILE 处于 `RawFallback` THE SYSTEM SHALL 跳过基于角色的规则（无法确定内容来源）。
- **R13** `tool_result` SHALL 被识别为独立 target 并应用更严格的规则集（它承载不可信的外部内容）。

## 5. 需求 · PreflightGuard

- **R14** `PreflightGuard` SHALL 在 P6 内联执行，可阻断。
- **R15** THE SYSTEM SHALL 施加硬超时（默认 50 ms）。
- **R16** IF 超时 THEN THE SYSTEM SHALL 按 `on_timeout`（默认 `open`）放行，并计 `argus_preflight_timeout_total`（宪法 C15）。
- **R17** IF 规则求值 panic THEN THE SYSTEM SHALL 放行并计数（宪法 C17，不可配置为 close）。
- **R18** WHEN 判定为 fail-open 降级放行 THE SYSTEM SHALL 在 `SecurityVerdict.degraded` 中标记原因，且该标记必须落库（宪法 C15）。
- **R19** WHEN 判定为 `Block` THE SYSTEM SHALL 返回 403 + 按调用协议渲染的错误体，含 `rule_id` 与 `request_id`。
- **R20** WHEN 判定为 `Warn` THE SYSTEM SHALL 放行并加 `x-argus-warning` 响应头。
- **R21** Preflight 的耗时 SHALL 被记录到 `argus_security_check_duration_seconds{timing="preflight"}`。

## 6. 需求 · 脱敏与策略绑定

- **R22** `credential-leak` / `pii` 规则命中的内容 SHALL 只存前缀 + 长度 + hash，**不存完整匹配值**（宪法 C29）。
- **R23** THE SYSTEM SHALL 支持 `PolicyBinding` 按 workspace / api_key / model / tag 选择策略，第一个匹配生效。
- **R24** `Policy.force_full_observation` SHALL 使 F20 的压力降级失效（安全优先）。
- **R25** 规则匹配 SHALL 在热路径上零分配（预编译的 Aho-Corasick / RegexSet 存于快照）。

## 7. 接口契约

```rust
// argus-proxy/src/security/mod.rs
pub struct SecurityEngine { rules: Arc<CompiledRuleSet>, cfg: Arc<SecurityConfig> }

impl SecurityEngine {
    /// 内联调用，有硬超时。永不返回 Err —— 失败即 fail-open 的 Verdict。
    pub fn preflight(&self, content: &ExtractedContent, policy: &Policy) -> SecurityVerdict;
    pub fn evaluate(&self, target: RuleTarget, text: &str, policy: &Policy) -> SmallVec<[RuleHit; 4]>;
}

// argus-proxy/src/security/extract.rs
pub enum ExtractionResult<'a> {
    Structured(ExtractedContent<'a>),
    RawFallback { text: Cow<'a, str>, reason: ExtractError },
}
pub fn extract(protocol: Protocol, body: &[u8]) -> ExtractionResult<'_>;

// argus-proxy/src/security/rules/mod.rs
pub struct CompiledRuleSet {
    keywords: AhoCorasick,
    regexes: RegexSet,
    by_target: HashMap<RuleTarget, SmallVec<[RuleId; 8]>>,
    meta: HashMap<RuleId, Arc<Rule>>,
}
impl CompiledRuleSet {
    pub fn compile(rules: &[Rule], limits: &CompileLimits) -> (Self, Vec<CompileError>);  // R8/R9
}
```

`SecurityVerdict` 定义见 `docs/12` §8.1（含 `degraded: Option<DegradeReason>`）。

## 8. 实现要点

- **R15 的 50 ms 预算是紧的**。`docs/12` §6.1 给了各匹配器的耗时估算，实现后必须 bench 验证。超预算的规则要么优化要么下沉到 PostGuard。
- 超时的实现：**不要**用 `tokio::time::timeout` 包裹（规则求值是同步 CPU 工作，超时无法中断它）。做法是在规则循环中周期检查 `Instant::elapsed()`，超时则提前返回并标记 `degraded`。
- **R11 的降级必须真的能跑**：`RawFallback` 下仍要能匹配 `credential-leak`（正则在原始 JSON 文本上一样有效）。写测试验证降级路径的检出率。
- `RegexSet` 用 `regex` crate —— 它保证线性时间，无灾难性回溯。这是选它而非 PCRE 的关键理由，要写进注释。
- **R22 的实现**：`RuleHit` 只存 `span` + `redacted_sample`（前 4 字符 + 长度）+ `match_hash`。完整内容**从不进入 `RuleHit`**，从类型上杜绝。
- `suppressions` 存在 `Policy` 里，随 `ConfigSnapshot` 15 秒生效（F23）。

## 9. 任务

- [ ] T1 `security/rule.rs`：`Rule` / `RuleTarget` / `Matcher` / `SecurityAction`（含 `Ord`）
- [ ] T2 `security/compile.rs`：`CompiledRuleSet::compile` + 上限 + 单规则失败跳过
- [ ] T3 `security/matcher/`：keywords / regex / entropy / fingerprint / shell / allowlist
- [ ] T4 `security/rules/builtin.rs`：11 个内置规则族（`docs/12` §7.3）
- [ ] T5 `security/extract.rs`：三协议的内容提取 + `RawFallback`
- [ ] T6 `security/verdict.rs`：合并（取最严 + 保留 hits）+ `degraded` 标记
- [ ] T7 `security/preflight.rs`：内联执行 + 预算内超时检查 + fail-open
- [ ] T8 `security/redact.rs`：`RuleHit` 脱敏（R22）+ 提供给 F12 的 `on_write` 脱敏实现
- [ ] T9 `security/policy.rs`：`PolicySet` / `PolicyBinding` / `suppressions` / `force_full_observation`
- [ ] T10 策略与规则的 CRUD（接入 F22 的 admin API）
- [ ] T11 接入主链路 P6 + F20 的 `force_full_observation`
- [ ] T12 测试：§10

## 10. 验收

```bash
cargo test -p argus-proxy security
cargo bench -p argus-proxy --bench preflight
```

- [ ] 11 个内置规则族各有正例与反例测试
- [ ] `SecurityAction` 的 `Ord` 与合并逻辑：多规则命中取最严，`hits` 全保留
- [ ] **bench：100 KB 请求的 preflight p95 < 8 ms、p99 < 40 ms**（`docs/21` §6.1）
- [ ] 人为让规则求值超 50 ms → 放行 + `argus_preflight_timeout_total` +1 + `verdict.degraded = Timeout`
- [ ] 注入规则 panic → 放行 + 计数 + 进程存活（宪法 C17）
- [ ] 畸形 JSON body → `RawFallback` 且 `credential-leak` 仍能检出（R11/R12）
- [ ] `tool_result` 中的注入模式被检出（R13）
- [ ] `Block` → 403，错误体形状随调用协议变化，含 `rule_id`
- [ ] `Warn` → 200 + `x-argus-warning`
- [ ] **脱敏断言**：注入 `AKIAIOSFODNN7EXAMPLE`，`RuleHit.redacted_sample` 不含完整值，且 grep 全部输出无该字符串（宪法 C29）
- [ ] 一条坏规则（非法正则）→ 该规则被跳过，其余规则集正常工作（R9）
- [ ] 模式数超 10k → 拒绝新规则集，旧的仍生效（R8）
- [ ] `suppressions` 加入 rule_id → 15 秒内该规则不再触发
- [ ] `force_full_observation = true` + Critical 压力 → `ObserveMode == Full`
- [ ] 规则匹配零分配（计数分配器断言）

## 11. 交给后续

| 不做的事 | 承接 |
|---|---|
| ToolGate（流中的工具阻断） | F25 |
| `security_events` 落库与查询 | F26 |
| 会话 taint | F26 |
| LLM Detector | Phase 2 |
| `delay_window` | Phase 2 |
| `RequireReview` 的审批流 | Phase 3 |
