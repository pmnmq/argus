# R03 · Target Policy 与 SSRF 防护

| | |
|---|---|
| ID | R03 |
| 类型 | v2 新增能力 |
| 里程碑 | M2 |
| 依赖 | R02 |
| 主 crate | `argus-proxy::target` |
| 设计依据 | `docs/13-ssrf-protection.md` 全篇；ADR-013 |
| 宪法条款 | C17（安全边界 fail-close）, C20 |

## 1. 目标

让 Argus 不成为一台开放的请求转发器，**同时不妨碍它代理 NAS 上的本地模型**。

## 2. 范围

**In scope**：scheme 白名单、hostname allow/deny、CIDR 判定、端口策略、
自定义 DNS 解析与缓存、连接时 IP 确认。

**Out of scope**：策略的 CRUD 与热更新（→ F22/F23）、出站 HTTP proxy 支持（OQ-S4）。

## 3. 需求

- **R1** THE SYSTEM SHALL 按 `docs/13` §4 的顺序判定：scheme → hostname 规则
  → DNS 解析后逐 IP → 端口 → 连接时确认。
- **R2** THE SYSTEM SHALL 对**每个**解析出的 IP 判定；IF 任一不合规
  THEN 整体拒绝（不挑合规的连）。
- **R3** THE SYSTEM SHALL 只解析一次 DNS，并把**已判定通过的 IP**直接交给
  connector；禁止把 hostname 交给 `TcpStream::connect`。
- **R4** THE SYSTEM SHALL 用原始 hostname 做 TLS SNI 与证书校验。
- **R5** IF 实际连接的 IP 与判定过的不一致 THEN SHALL 中止并计
  `argus_target_rebind_detected_total`。
- **R6** allow/deny CIDR SHALL 按**最长前缀匹配**，allow 赢过 deny。
- **R7** 拒绝类错误 SHALL 统一为 403 `target_denied`，**不回显 IP**，
  且不按拒绝原因区分状态码。
- **R8** DNS 解析失败 SHALL 返回 502 `target_dns_failure`（与 `target_denied` 不同，
  因为它不泄漏策略信息）。
- **R9** THE SYSTEM SHALL 缓存「hostname → 已判定通过的 IP 集合」，
  TTL = min(记录 TTL, 30s)。**实现偏差**：走系统解析器拿不到记录自身的 TTL，
  实际生效的是固定 30s 上限；要真做到 min 得引入自己发 DNS 报文的解析器
  （`docs/13` §6.1 的实现约束、OQ-S1）。
- **R10** THE SYSTEM SHALL NOT 跟随 redirect。3xx 连同 `Location` 原样透传，
  判定链因此**只跑一次**（`docs/04` §8，2026-08-21 由「Profile 级可选」
  改为不做）。
- **R11** IF `allow_raw_proxy` 与 `injected` 凭据同时开启且 allowlist 为空
  THEN 配置校验 SHALL 失败（不是运行期）。
- **R12** Managed 模式 SHALL 同样执行本策略。

## 4. 接口契约

```rust
pub struct TargetPolicy { /* schemes, allow/deny cidrs, hosts, ports … */ }
pub struct TargetPolicyConfig { /* `docs/13` §5 的 TOML 形状，serde */ }
pub enum HostVerdict { Deny, Allow, CheckIps }

impl TargetPolicy {
    pub fn compile(cfg: &TargetPolicyConfig) -> Result<Self, PolicyError>;
    pub fn check_scheme(&self, s: &Scheme) -> Result<(), ProxyError>;
    pub fn check_host(&self, host: &str) -> HostVerdict;
    pub fn check_ip(&self, ip: IpAddr) -> bool;      // 最长前缀匹配
    pub fn check_port(&self, port: u16) -> bool;
    pub fn has_allowlist(&self) -> bool;             // R11 用
}

/// 解析器抽成 trait，测试才构造得出 rebinding 场景（T8）
pub trait DnsResolver: Send + Sync + 'static {
    fn resolve<'a>(&'a self, host: &'a str, port: u16)
        -> Pin<Box<dyn Future<Output = io::Result<Addrs>> + Send + 'a>>;
}
pub struct SystemResolver;    // getaddrinfo + 超时

/// 只解析一次，返回全部通过判定的地址。缓存挂在这里
pub struct CheckedResolver;
impl CheckedResolver {
    pub async fn resolve_and_check(&self, host: &str, port: u16, p: &TargetPolicy)
        -> Result<Addrs, ProxyError>;
}

/// 自定义 connector：只连给定的 SocketAddr，不再解析
pub struct PinnedConnector { addrs: Addrs, sni: CompactString }

/// 整条判定链的入口
pub async fn check_target(uri: &Uri, p: &TargetPolicy, r: &CheckedResolver)
    -> Result<PinnedConnector, ProxyError>;
```

三处与初稿的偏差：

- **删掉了 `Verdict`**。`Result<Addrs, ProxyError>` 已经表达了 Allow/Deny，
  而且能区分 `target_denied` 与 `target_dns_failure`（R8 要求这两者不同）。
  再套一层只会让调用方多一次 match。
- **`resolve_and_check` 成了 `CheckedResolver` 的方法**。它要持有缓存
  （R9）和一个可替换的 `DnsResolver`（T8），自由函数装不下这两样。
- **`PinnedConnector.sni` 存的是 `CompactString` 而不是 `ServerName`**。
  构造 `ServerName` 属于 TLS 装配，那是 F14 的事；这一层只保证「TLS 该用
  哪个名字」这个信息不丢。

## 5. 实现要点

- CIDR 判定要预编译，不要每请求现解析字符串。**没用 trie**：最长前缀匹配可以
  化简成「按前缀长度倒序排好，第一个命中的就是最长的」，默认策略只有 12 条
  规则，一次连续数组扫描比指针跳转的 trie 快，代码量少一个数量级。规则涨到
  几百条再换。
- **`PinnedConnector` 是本 spec 最容易写错的地方。** hyper 的默认
  `HttpConnector` 会自己解析 hostname——必须换掉，否则 R3/R5 形同虚设。
  要有一个测试：mock 一个"第二次解析返回不同 IP"的 resolver，断言被检出。
- 云元数据地址（`169.254.169.254`、`fd00:ec2::254`）考虑做成**不可被 allow 覆盖**
  的硬黑名单（OQ-S2）。第一阶段先放进默认 deny，但在代码里留注释。
- R7 的"不回显"要连**日志**一起：审计里可以记完整信息，返回给客户端的 body 不行。

## 6. 任务

- [x] T1 `target/cidr.rs` + `target/policy.rs`：最长前缀匹配。**没用 trie** ——
      默认策略只有 12 条规则，「按前缀长度倒序排好、第一个命中即最长」是一次
      连续数组扫描，在这个规模上稳稳快过指针跳转的 trie。规则涨到几百条再说
- [x] T2 `target/resolve.rs`：`DnsResolver` trait + `SystemResolver` +
      `CheckedResolver`（逐 IP 判定 + 已判定地址的缓存）
- [x] T3 `target/connector.rs`：`PinnedConnector`（只连给定 `SocketAddr`，
      SNI 留原 hostname）
- [x] T4 rebinding 检出与 `argus_target_rebind_detected_total`
- [x] T5 ~~redirect 逐跳重校验~~ **不做了**。透明网关不该替客户端跟随
      redirect，3xx 原样透传即可（`docs/04` §8）。这一格从"待接线"变成
      "不存在" —— 少一整套需要逐跳防守的攻击面
- [x] T6 `check_raw_injected_combination`（R11 的判定逻辑）。**接进配置校验
      仍待 F22**：Control Plane 的 `validate_profile` 还没有
- [x] T7 `docs/20` §5.4 的 SSRF 矩阵 → `tests/target_matrix.rs`
- [x] T8 `tests/common` 的 `MockResolver`：按调用次序换答案，构造出
      「第一次公网、第二次回环」的 rebinding 场景

## 7. 验收

```bash
just test -p argus-proxy target
```

- [x] `docs/20` §5.4 矩阵全绿。redirect 那行已简化为「3xx 原样透传」，
      要等 F06 有真的转发链路才验得了 —— 本切片没有任何 redirect 逻辑可验
- [x] 多值 A 记录含一个内网 IP → 整体拒绝
- [x] rebinding 场景：第二次解析确实答了回环，而 `PinnedConnector` 手里
      仍然只有第一次判定过的地址；`verify_peer` 兜底那一层单独验
- [x] 私有地址与 DNS 失败返回**不同**错误码（403 / 502），三种**拒绝原因**
      （私有 IP / 端口禁用 / deny_hosts）返回**相同**的 403 与同一段 body
- [x] 拒绝 body 中不含任何 IP 字符串
- [x] 显式 allow `192.168.1.20/32` 后该地址可达，同网段邻居仍然拒
- [x] 全 workspace 测试 161 → 208（新增 47 个：29 单测 + 18 集成）
- [x] `verify-guards.sh` 通过数 26 → 28（8f 两条注入用例）

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 策略 CRUD 与热更新 | F22 / F23 |
| 把 R11 接进 `validate_profile` | F22。判定逻辑已就位，`ci/check-invariants.sh` 的 `[R03 R11]` 盯着：谁引入 `allow_raw_proxy` 谁必须接上 |
| 把 `PinnedConnector` 装进 hyper client、`ServerName` 与 TLS 装配 | F14（`clippy.toml` 禁 `HttpConnector` 盯着） |
| ~~redirect 逐跳重校验~~ | 已取消，见 R10 与 `docs/04` §16 OQ-W5 |
| 出站 HTTP proxy | OQ-S4，暂不做 |
| 时间侧信道 | OQ-W3，第一阶段不做 |
