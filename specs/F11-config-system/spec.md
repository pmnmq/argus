# F11 · 配置系统与 profile

| | |
|---|---|
| ID | F11 |
| 里程碑 | M1 |
| 依赖 | F02 |
| 主 crate | `argus-core` + `argus-server` |
| 设计依据 | `docs/18-deployment.md` §5；`docs/01-architecture.md` §7 |
| 宪法条款 | C20, C27 |

## 1. 目标

实现**进程配置**的加载、校验与规模预设。业务配置（provider/model/policy/key）不在此切片——它们在数据库里。

## 2. 范围

**In scope**：`Config` 类型树、分层加载（默认 < 文件 < 环境变量 < CLI）、四个 profile、启动时校验、`ConfigSnapshot` 的**结构**（不含热更新）。

**Out of scope**：`ArcSwap` 热更新（→ F23）、Control Plane 的配置 CRUD（→ F22）、`argus config apply`（→ F22）。

## 3. 需求

- **R1** THE SYSTEM SHALL 支持 `docs/18` §5.3 的完整配置树，键名逐字一致。
- **R2** THE SYSTEM SHALL 按 `默认值 < TOML 文件 < 环境变量 < CLI 参数` 的优先级合并。
- **R3** 环境变量 SHALL 用 `ARGUS_` 前缀，`__` 映射层级（如 `ARGUS_LIMITS__MAX_ACTIVE_STREAMS`）。
- **R4** THE SYSTEM SHALL 提供 `nano` / `small` / `medium` / `large` 四个 profile 作为默认值集合（`docs/18` §5.4）。
- **R5** WHEN 指定 profile 且同时显式配置某项 THE SYSTEM SHALL 以显式配置为准。
- **R6** THE SYSTEM SHALL 区分进程配置与业务配置，二者不重叠（`docs/18` §5.1）。
- **R7** WHEN 配置含未知键 THE SYSTEM SHALL 拒绝启动并指出键名与最相近的合法键（typo 保护）。
- **R8** WHEN 配置项之间存在矛盾 THE SYSTEM SHALL 拒绝启动并说明矛盾（见 §5 校验清单）。
- **R9** THE SYSTEM SHALL 支持从文件读取敏感值（`*_file` 后缀），并检查文件权限为 0600（宪法 C27）。
- **R10** THE SYSTEM SHALL 提供 `ConfigSnapshot` 结构体（`docs/01` §7.2），本切片只在启动时构建一次。
- **R11** THE SYSTEM SHALL 在启动时打印生效配置的摘要，**敏感值显示为 `<redacted>`**（宪法 C27）。
- **R12** 时长与字节大小 SHALL 支持人类可读格式（`"30s"` / `"8MiB"`）。

## 4. 接口契约

```rust
// argus-core/src/config/mod.rs
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]          // R7
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub credentials: CredentialsConfig,
    pub limits: LimitsConfig,
    pub upstream: UpstreamConfig,
    pub routing: RoutingConfig,
    pub observation: ObservationConfig,
    pub security: SecurityConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub quota: QuotaConfig,
    pub redis: RedisConfig,
    pub logging: LoggingConfig,
    pub telemetry: TelemetryConfig,
    pub shutdown: ShutdownConfig,
    pub runtime: RuntimeConfig,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self, ConfigError>;
    pub fn validate(&self) -> Result<(), Vec<ConfigProblem>>;   // R8
    /// 摘要，敏感值已脱敏（R11）
    pub fn redacted_summary(&self) -> String;
}

pub enum Profile { Nano, Small, Medium, Large }
impl Profile { pub fn defaults(self) -> figment::Figment; }
```

## 5. 实现要点

**校验清单（R8）——每条都要有明确的错误信息**：

| 矛盾 | 错误信息要点 |
|---|---|
| `shutdown.force_kill_after <= drain_timeout` | 强制停机会先于 drain 完成 |
| `limits.memory.low_watermark >= high_watermark` | 迟滞区间非法 |
| `upstream.timeouts.headers > timeouts.total` | headers 超时永不触发 |
| `tee_byte_budget < max_frame_bytes` | 单帧就会撑爆预算，观测必然全丢 |
| `max_large_request_concurrency × max_request_bytes` > 内存上限的 50% | 大请求可能 OOM |
| `security.preflight.on_timeout = "close"` 且 `timeout < 100ms` | fail-close 配短超时会大量误拒 |
| `storage.audit.require_durable = true` | **警告**（非错误）：PG 变成 Data Plane 硬依赖，违背 C19 的设计意图 |
| `role = "data"` 但配了 `admin_listen` | 无效配置 |
| `redis.enabled = false` 但多实例部署迹象 | **提示**：限流是每实例独立的 |

- 用 `figment` 做分层合并；`deny_unknown_fields` 提供 R7，但需要额外做"最相近合法键"的建议（用 `strsim::jaro_winkler`）。
- profile 实现为一个返回 `Figment` 的函数，插在"默认值"与"文件"之间。
- `humantime_serde` 处理时长，自定义 `ByteSize` 处理 `"8MiB"`。
- **R11 的脱敏必须是白名单式**：只打印已知安全的字段，而不是黑名单过滤——后者一定会漏。

## 6. 任务

- [ ] T1 `config/types.rs`：完整配置树（照 `docs/18` §5.3，逐键对齐）
- [ ] T2 `config/defaults.rs`：内置默认值
- [ ] T3 `config/profile.rs`：四个 profile
- [ ] T4 `config/load.rs`：figment 分层 + 环境变量映射 + CLI
- [ ] T5 `config/validate.rs`：§5 校验清单（错误 + 警告两级）
- [ ] T6 `config/secret.rs`：`*_file` 读取 + 0600 权限检查
- [ ] T7 `redacted_summary()`（白名单式）
- [ ] T8 `ConfigSnapshot` 结构体定义（不含 ArcSwap）
- [ ] T9 `config/argus.example.toml` + `config/profiles/*.toml`
- [ ] T10 测试：§7

## 7. 验收

```bash
cargo test -p argus-core config
argus serve --config config/argus.example.toml --check-only    # 只校验不启动
```

- [ ] `docs/18` §5.3 的示例配置能被完整解析（**用它作为测试 fixture，防止文档与代码漂移**）
- [ ] 四个 profile 各自能解析且通过 validate
- [ ] 未知键 → 拒绝 + 建议最相近的键
- [ ] §5 校验清单每条各有一个测试用例
- [ ] `ARGUS_LIMITS__MAX_ACTIVE_STREAMS=999` 覆盖文件值
- [ ] CLI 参数覆盖环境变量
- [ ] master key 文件权限 0644 → 拒绝启动，错误信息含 `chmod 600`
- [ ] `redacted_summary()` 输出中不含任何配置的密钥/密码/URL 中的凭据部分

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| `ArcSwap` 热更新与 refresher | F23 |
| 业务配置（provider/model/policy）的加载 | F22 / F23 |
| `argus config apply` / `export` | F22 |
| `argus doctor` | F15 |
