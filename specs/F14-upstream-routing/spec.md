# F14 · Upstream 与单 Provider 路由

| | |
|---|---|
| ID | F14 |
| 里程碑 | M1 |
| 依赖 | F11 |
| 主 crate | `argus-proxy` |
| 设计依据 | `docs/06-data-plane.md` §4,§6,§7,§9；`docs/08-provider-profiles.md` §4–6；`docs/09-observers.md` §6 |
| 宪法条款 | C1, C3, C18, C23, C28 |


> ⚠️ **v2 变更（R01–R04 之后）**：本 spec 的「选路」部分**已不存在**。
> provider 由 URL 决定（`docs/02-routing-model.md`），解析在 R02，
> 准入在 R03。本 spec 现在只负责 **header 变换 + 发起上游请求 + 超时**。
> 下面被 ~~删除线~~ 标记的需求是 v1 遗留，不要实现。
## 1. 目标

把 F06 的硬编码上游换成真正的路由：模型三层结构、协议识别、header 变换、URL 构造、`ErrorRenderer`。本切片只做**单候选**，多候选与 retry 留 F16。

## 2. 范围

**In scope**：header 变换（hop-by-hop 删除、客户端凭据剥离、Profile 凭据注入、
`extra_headers`、Host 重写、XFF/Via）、hyper 上游客户端与连接池、四段式超时、
`ResolvedTarget` 到 `Request` 的构造、`ErrorRenderer`。

**Out of scope**：URL 解析（→ R02）、Target Policy（→ R03）、Observer 选择（→ R04）、同 Profile 重试（→ F16）、熔断与方言（→ F17）。

## 3. 需求 · 路由

- **R1** THE SYSTEM SHALL 按 `docs/09` §6.1 的表由 URL path 确定 `Protocol`，**不做 body 嗅探**。
- **R2** THE SYSTEM SHALL 只探测请求体的 `model` 与 `stream` 两个字段，其余不校验（`docs/09` §8）。
- **R3** IF `model` 缺失 THEN THE SYSTEM SHALL 返回 400（按调用协议的错误形状）。
- **R4** THE SYSTEM SHALL 支持 `LogicalModel` → `ModelBinding` → `Provider`/`Endpoint` 三层解析，含别名。
- **R5** ~~协议匹配校验~~ **已删除**：v2 里没有 binding，也没有 `supports` 矩阵（ADR-010）。
- **R6** THE SYSTEM SHALL 用 `binding.capability_overrides` 覆盖 `LogicalModel.capabilities` 得到有效能力，**overrides 只能收窄**。
- **R7** THE SYSTEM SHALL 提取 `RequestDemand`（`docs/08` §6.1），提取失败时全部取保守值且**不拒绝请求**。
- **R8** IF 有效能力不满足 `RequestDemand` THEN THE SYSTEM SHALL 返回 400 并列出缺失的能力清单。
- **R9** `uses_prompt_caching` SHALL 作为软信号（不作硬过滤），不满足时加 `x-argus-warning` 响应头。
- **R10** THE SYSTEM SHALL 应用 API Key 的 `allowed_profiles` / `allow_raw_proxy`，且**与「profile 不存在」返回同一个 404**（`docs/14` §7.2 防枚举）。

## 4. 需求 · Upstream

- **R11** THE SYSTEM SHALL 按 `docs/09` §6.2 的表构造上游 URL（OpenAi / AzureOpenAi / Anthropic / OpenAiCompatible 四种）。
- **R12** THE SYSTEM SHALL 按 `docs/06` §6.3–6.4 变换 header：剥离 hop-by-hop 与客户端凭据、注入 provider 凭据与必需 header、**未知 header 默认透传**（宪法 C2）。
- **R13** THE SYSTEM SHALL 注入 `anthropic-version` 等 `Dialect.required_headers`，不依赖客户端提供。
- **R14** THE SYSTEM SHALL 实施四段式超时（connect / headers / idle_frame / total）。
- **R15** THE SYSTEM SHALL 在收到上游 response header 后、写出 client header 前判定 status（S2 检查点）。
- **R16** WHEN 已写出首个响应字节 THE SYSTEM SHALL 进入 S3 且**永不** retry（宪法 C18）。
- **R17** THE SYSTEM SHALL 优先 ALPN 协商 h2（宪法 C23）。
- **R18** THE SYSTEM SHALL NOT 重新序列化请求体或响应体（宪法 C1）。

## 5. 需求 · 错误与 `/v1/models`

- **R19** THE SYSTEM SHALL 按调用方协议渲染错误体：OpenAI 形状 `{"error":{...}}`，Anthropic 形状 `{"type":"error","error":{...}}`。
- **R20** WHEN 已进入 S3 THE SYSTEM SHALL 在流内发出协议原生的 error 事件；OpenAI 路径**不发** `[DONE]`（`docs/06` §9）。
- **R21** 错误体 SHALL NOT 包含 provider 凭据或内部栈信息。
- **R22** 全部响应 SHALL 携带 `x-argus-request-id`。
- **R23** ~~`GET /v1/models` 由 Argus 合成~~ **已删除**：模型目录不存在了，它现在就是一次普通透传（`docs/09` §6.1）。

## 6. 接口契约

```rust
// argus-proxy/src/router/mod.rs
pub struct Router { snapshot: ConfigHandle }
impl Router {
    /// 本切片返回单个候选；F16 改为返回有序候选列表
    pub fn resolve(&self, ctx: &RequestCtx, demand: &RequestDemand)
        -> Result<ResolvedTarget, ProxyError>;
}

pub enum RouteError {
    ModelNotFound(CompactString),
    ModelNotAllowed,
    ProtocolMismatch { requested: Protocol, binding: Protocol, hint: CompactString },
    CapabilityUnsupported { missing: SmallVec<[&'static str; 4]>, detail: String },
    NoHealthyProvider,
}

// argus-proxy/src/upstream/mod.rs
pub struct Upstream { client: HttpsClient, }
impl Upstream {
    pub fn build_request(&self, route: &RouteDecision, ctx: &RequestCtx, body: Bytes)
        -> Result<Request<Full<Bytes>>, ProxyError>;
    /// 返回时仍处于 S2：body 未被 poll，可安全丢弃换 provider
    pub async fn send(&self, req: Request<Full<Bytes>>, t: &Timeouts)
        -> Result<Response<Incoming>, UpstreamError>;
}

// argus-proxy/src/ingress/error.rs
pub trait ErrorRenderer { fn render(&self, err: &ProxyError, p: Protocol) -> Response; }
```

## 7. 实现要点

- **R15 的实现是 F16 能工作的前提**：`Upstream::send` 返回 `Response<Incoming>` 时 body 一次都没被 poll，此时 `drop(resp)` 就能干净地放弃这条上游连接。这个性质要写测试锁死。
- header 变换用**静态表 + 黑名单**，不要写成一串 if。`STRIP_FROM_CLIENT` 常量照 `docs/06` §6.4。
- `RequestDemand` 提取用容错解析：任何字段解析失败都退化为 `false`/`None`（R7），绝不因此拒绝请求。
- `capability_overrides` 的"只能收窄"要在两处保证：Control Plane 保存时校验（F22）+ 运行时合并时再保证一次（防御性）。
- `/v1/models` 的 `x-argus` 扩展字段放在标准字段之外，保证 OpenAI SDK 的严格解析不会失败。
- `ErrorRenderer` 每协议一个实现，用 `match protocol` 分发，不用 trait object。

## 8. 任务

- [ ] T1 `ingress/route_table.rs`：path → Protocol（`docs/09` §6.1 表）
- [ ] T2 `ingress/probe.rs`：`model` / `stream` 最小探测
- [ ] T3 `router/model.rs`：三层解析 + 别名
- [ ] T4 `router/capability.rs`：`RequestDemand` 提取 + 有效能力计算 + gap 报告
- [ ] T5 `router/resolve.rs`：白名单 → 协议 → 能力四层过滤（单候选）
- [ ] T6 `upstream/url.rs`：4 种 kind 的 URL 构造
- [ ] T7 `upstream/headers.rs`：变换表（剥离/注入/透传）
- [ ] T8 `upstream/send.rs`：四段超时 + S2 检查点 + 阶段机
- [ ] T9 `ingress/error.rs`：`ErrorRenderer` × 2 协议 + 流内 error 事件
- [ ] T10 `ingress/models.rs`：`/v1/models` 合成 + 过滤
- [ ] T11 替换 F06 的硬编码上游
- [ ] T12 测试：§9

## 9. 验收

```bash
cargo test -p argus-proxy router upstream
cd tests/e2e && pytest sdk_compat/     # F10 的 SDK 验证此时应仍全绿
```

- [ ] path 路由表逐行测试；未知 path → 404
- [ ] `model` 缺失 → 400，**错误形状随调用协议变化**（两种都测）
- [ ] `protocol_mismatch` → 400 且错误信息含正确的 endpoint 建议
- [ ] 带图片的请求 + binding 无 vision → 400 且 `missing` 含 `"vision"`
- [ ] `capability_overrides` 尝试放宽 → 合并结果仍是收窄值
- [ ] 4 种 kind 的 URL 构造逐一断言（含 Azure 的 `deployment` + `api-version`）
- [ ] 转发的 header：hop-by-hop 已剥离、客户端凭据已剥离、`anthropic-version` 已注入、未知 header 已透传
- [ ] **S2 性质**：`send()` 返回后 `drop(resp)`，mock provider 侧观察到连接被释放且无 body 传输
- [ ] S3 后触发错误 → 流内 error 事件；OpenAI 路径确认无 `[DONE]`
- [ ] 错误体不含凭据字符串（grep 断言）
- [ ] `/v1/models` 只返回 Key 有权访问的模型
- [ ] **透明性回归**：同一请求直连 mock 与经 Argus，mock 收到的 body 逐字节相同（宪法 C1）

## 10. 交给后续

| 不做的事 | 承接 |
|---|---|
| 多候选、weighted/round-robin、fallback | F16 |
| Retry 状态机的实际重试逻辑 | F16 |
| 熔断、throttle、方言、`first_event_gate` | F17 |
| 会话粘性 | F16 |
