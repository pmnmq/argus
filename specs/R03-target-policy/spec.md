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
自定义 DNS 解析与缓存、连接时 IP 确认、redirect 重校验。

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
  TTL = min(记录 TTL, 30s)。
- **R10** WHEN `follow_redirects` 开启 THE SYSTEM SHALL 对每一跳完整重跑判定链，
  最多 3 跳，且跨 origin 时剥除全部凭据 header。
- **R11** IF `allow_raw_proxy` 与 `injected` 凭据同时开启且 allowlist 为空
  THEN 配置校验 SHALL 失败（不是运行期）。
- **R12** Managed 模式 SHALL 同样执行本策略。

## 4. 接口契约

```rust
pub struct TargetPolicy { /* schemes, allow/deny cidrs, hosts, ports … */ }

pub enum Verdict { Allow(SmallVec<[SocketAddr; 4]>), Deny }

impl TargetPolicy {
    pub fn check_scheme(&self, s: &Scheme) -> Result<(), ProxyError>;
    pub fn check_host(&self, host: &str) -> HostVerdict;
    pub fn check_ip(&self, ip: IpAddr) -> bool;      // 最长前缀匹配
    pub fn check_port(&self, port: u16) -> bool;
}

/// 只解析一次，返回全部通过判定的地址
pub async fn resolve_and_check(host: &str, port: u16, p: &TargetPolicy)
    -> Result<SmallVec<[SocketAddr; 4]>, ProxyError>;

/// 自定义 connector：只连给定的 SocketAddr，不再解析
pub struct PinnedConnector { addrs: SmallVec<[SocketAddr; 4]>, sni: ServerName }
```

## 5. 实现要点

- CIDR 判定预编译成 trie（`ip_network_table` 或手写），不要每请求遍历列表。
- **`PinnedConnector` 是本 spec 最容易写错的地方。** hyper 的默认
  `HttpConnector` 会自己解析 hostname——必须换掉，否则 R3/R5 形同虚设。
  要有一个测试：mock 一个"第二次解析返回不同 IP"的 resolver，断言被检出。
- 云元数据地址（`169.254.169.254`、`fd00:ec2::254`）考虑做成**不可被 allow 覆盖**
  的硬黑名单（OQ-S2）。第一阶段先放进默认 deny，但在代码里留注释。
- R7 的"不回显"要连**日志**一起：审计里可以记完整信息，返回给客户端的 body 不行。

## 6. 任务

- [ ] T1 `target/policy.rs`：`TargetPolicy` + CIDR trie + 最长前缀匹配
- [ ] T2 `target/resolver.rs`：DNS 解析 + 逐 IP 判定 + 缓存
- [ ] T3 `target/connector.rs`：`PinnedConnector`（只连给定 IP，SNI 用 hostname）
- [ ] T4 rebinding 检出与指标
- [ ] T5 redirect 逐跳重校验 + 跨 origin 剥凭据
- [ ] T6 R11 的配置期强制
- [ ] T7 `docs/20` §5.4 的 SSRF 测试矩阵
- [ ] T8 一个 mock resolver，用于构造 rebinding 场景

## 7. 验收

```bash
just test -p argus-proxy target
```

- [ ] `docs/20` §5.4 矩阵全绿
- [ ] 多值 A 记录含一个内网 IP → 整体拒绝
- [ ] rebinding 场景被连接时检出
- [ ] 私有地址与 DNS 失败返回**不同**错误码（前者 403 后者 502），但两种
      **拒绝原因**（私有 / 端口禁用）返回**相同**的 403 与 body
- [ ] 拒绝 body 中不含任何 IP 字符串
- [ ] 显式 allow `192.168.1.20/32` 后该地址可达

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 策略 CRUD 与热更新 | F22 / F23 |
| 出站 HTTP proxy | OQ-S4，暂不做 |
| 时间侧信道 | OQ-W3，第一阶段不做 |
