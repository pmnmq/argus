# 14 · Rust 项目结构

> 状态：Draft v1 · 最后更新 2026-08-19
> 前置：`01-architecture.md` §5（依赖方向）

---

## 1. Problem

需要决定 workspace 怎么拆。这个决定影响：编译时间、测试速度、模块纪律、以及**能否在编译期禁止某些依赖关系**。

用户提出的候选是 9 个 crate（`server`/`proxy`/`protocol`/`provider`/`observer`/`security`/`storage`/`auth`/`common`）。核心问题是：

> **第一版是否真的需要拆这么多？**

答案是**不需要**。但也不能只用一个 crate——因为有两条边界必须在编译期强制。

---

## 2. Goals

- G-P1 增量编译在改动业务代码时 < 15 秒。
- G-P2 `argus-protocol` 的单元测试（含数百个 golden case）能在 < 5 秒内跑完。
- G-P3 「Data Plane 不能直连数据库」和「协议解析层不能做 IO」在**编译期**被保证。
- G-P4 crate 数量最小化（每个 crate 都要有明确的、非组织性的理由）。

## 3. Non-goals

- 不追求"每个模块一个 crate"的对称美感。
- 不为将来可能的复用而提前拆分。
- 不做 dynamic plugin / WASM 扩展的 ABI 设计。

---

## 4. 拆 crate 的判据

一个模块**只有**满足以下之一才值得独立成 crate：

| 判据 | 说明 |
|------|------|
| **A. 编译期依赖隔离** | 需要在编译期证明"X 不能依赖 Y"。`mod` 做不到这件事。 |
| **B. 依赖集差异显著** | 该模块的依赖与其他部分几乎不重叠，合并会拖慢所有人的编译。 |
| **C. 测试密集且快** | 有大量快速单元测试，独立后可以单独 `cargo test -p` 而不重编整个项目。 |
| **D. 独立发布** | 真的要作为独立的 crates.io 包发布。 |

**不成立的理由**（这些都不该导致拆 crate）：
- ❌ "这是一个独立的概念" → 用 `mod`
- ❌ "为了模块化" → 用 `mod` + `pub(crate)`
- ❌ "将来可能复用" → 等真的要复用时再拆
- ❌ "文件太多了" → 用子目录

---

## 5. MVP 结构：5 个 crate

```text
argus/
├── Cargo.toml                    # workspace root
├── rust-toolchain.toml
├── deny.toml                     # cargo-deny（许可证 + 漏洞 + 重复依赖）
├── crates/
│   ├── argus-core/               # L1 基础
│   ├── argus-protocol/           # L2 领域（纯计算）
│   ├── argus-proxy/              # L3 数据面
│   ├── argus-storage/            # L3 存储
│   └── argus-server/             # L4 组装 + bin
├── migrations/
├── config/
│   ├── argus.example.toml
│   └── profiles/{nano,small,medium,large}.toml
├── tests/
│   ├── fixtures/                 # golden test 数据（见 15-testing-strategy.md）
│   └── e2e/
├── benches/
└── docs/
```

### 5.1 每个 crate 的理由

| crate | 判据 | 说明 |
|-------|------|------|
| `argus-core` | **A** | 承载被所有层共享的类型（`Protocol`、`TeeChunk`、配置、错误）。无它则 protocol 与 proxy 之间会出现类型重复或反向依赖。 |
| `argus-protocol` | **A + B + C** | A：编译期禁止 tokio/sqlx/hyper，保证解析层不可能阻塞；同时定义 `InteractionSink` trait 使 proxy 与 storage 解耦。B：依赖只有 serde/memchr/bytes，极轻。C：数百个 golden test，秒级迭代。 |
| `argus-proxy` | — | 数据面主体。本身不需要独立（合进 server 也行），但它必须与 storage 分离（判据 A 的另一半）。 |
| `argus-storage` | **A + B** | A：sqlx 只在这里。B：sqlx + 迁移 + zstd，依赖重且编译慢，隔离后改 proxy 代码不用重编 sqlx。 |
| `argus-server` | — | bin crate，组装一切。 |

**关键洞察**：`argus-proxy` 与 `argus-storage` 分开，不是因为它们"是不同的东西"，而是因为**必须在编译期证明前者不依赖后者**（`10-storage.md` G-St1）。这是判据 A 的典型应用。

### 5.2 为什么 security / auth / provider / observer 不独立成 crate

| 候选 | 归属 | 为什么不独立 |
|------|------|-------------|
| `argus-security` | `argus-proxy::security` | 依赖集与 proxy 高度重叠（都要 bytes/serde/regex）；PreflightGuard 与 ToolGate 与主链路耦合紧密（ToolGate 需要触及 Body）；无编译期隔离需求 |
| `argus-auth` | `argus-proxy::auth` | 只有 sha256 + cache，代码量小（< 800 行）；与 Gatekeeper 是同一件事 |
| `argus-provider` | `argus-proxy::upstream` + `argus-core::config` | provider "配置模型"属于 core，"发起请求"属于 proxy。独立成 crate 会造成循环依赖压力 |
| `argus-observer` | `argus-proxy::observer` | Observer 是 proxy 内部的一个 task，它调用 `argus-protocol`（已独立）；自己没有独立的依赖集 |
| `argus-common` | 就是 `argus-core` | 换个名字。避免 "common" 这种垃圾桶命名 |

> `argus-security` 是最接近应该独立的一个（安全规则集将来可能很大）。**留作 Phase 2 的候选**：当 `security/` 目录超过 ~5000 行或引入重依赖（如 tokenizer、ML 推理）时拆出来。

### 5.3 模块布局

```text
crates/argus-core/src/
├── lib.rs
├── config/          # 进程配置（Config / Limits / Timeouts）
├── ids.rs           # 强类型 ID（WorkspaceId / ApiKeyId / TraceContext）
├── protocol.rs      # Protocol / ProviderKind 枚举
├── model.rs         # LogicalModel / ModelBinding / Capabilities
├── provider.rs      # ProviderConfig / Endpoint / Dialect / ResolvedCredential
├── route.rs         # RouteDecision / RetryPolicy
├── snapshot.rs      # ConfigSnapshot / ConfigHandle (ArcSwap)
├── tee.rs           # TeeChunk / TeeEnd
└── error.rs         # ProxyError / 错误分类

crates/argus-protocol/src/
├── lib.rs           # #![forbid(unsafe_code)] + deny(unwrap_used, indexing_slicing, panic)
├── ir/              # ObservedEvent / BlockCoord / BlockKind / Finish / UsageDelta
├── framer/
│   ├── mod.rs       # trait Framer
│   ├── sse.rs       # SseFramer
│   └── eventstream.rs  # Bedrock（Phase 3，先留空实现）
├── parser/
│   ├── mod.rs       # trait ProtocolParser + enum dispatch
│   ├── openai_chat.rs
│   ├── openai_responses.rs
│   └── anthropic.rs
├── accumulator/     # Accumulator / BlockState / UsageAccumulator
├── synthetic.rs     # 非流式 JSON → 等价事件序列
├── interaction.rs   # ObservedInteraction / Outcome / Integrity
└── sink.rs          # trait InteractionSink   ← proxy/storage 解耦的关键

crates/argus-proxy/src/
├── lib.rs
├── ingress/         # Axum router / 协议识别 / body ingest / ErrorRenderer
├── auth/            # KeyCache / 认证 / 授权 / EffectiveLimits
├── quota/           # GCRA 限流 / token & cost 软配额
├── router/          # 选路 / 权重 / 熔断 / throttle
├── upstream/        # hyper client / header 变换 / retry 状态机 / 连接池
├── tee.rs           # TeeBody / TeeHandle
├── gate.rs          # GatedBody（ToolGate 的主链路侧）
├── observer/        # Observer task / ObserveMode 决策 / ObserverLoad
└── security/        # 规则引擎 / PreflightGuard / StreamGuard / ToolGate / PostGuard
    ├── rules/       # 内置规则集
    ├── extract.rs   # SecurityExtractor
    └── detector.rs  # LLM detector（可选）

crates/argus-storage/src/
├── lib.rs
├── pool.rs          # PgPool 构建（data / admin 两个）
├── sink/            # Batcher / Writer / PgSink（impl InteractionSink）
├── spool/           # segment 写入 / Replayer / checkpoint
├── repo/            # Control Plane 的 CRUD
├── partition.rs     # 分区维护 task
└── crypto.rs        # credential 加解密

crates/argus-server/src/
├── main.rs          # CLI（serve / migrate / config / keys / doctor / keygen）
├── boot.rs          # 依赖检查 / 组装 / 注入
├── admin/           # Control Plane HTTP handlers
├── shutdown.rs      # 优雅停机编排
└── doctor.rs
```

---

## 6. 长期结构（Phase 3+）

只有当出现明确信号时才拆。**预定义触发条件**：

| 拆出 | 触发条件 |
|------|---------|
| `argus-security` | security/ > 5000 行，或引入 tokenizer / ML 推理依赖 |
| `argus-control` | Control Plane 的 handler > 3000 行，或需要独立发布 admin-only 二进制 |
| `argus-provider` | 支持 > 8 种 ProviderKind，且 SigV4/OAuth2 等认证逻辑变重 |
| `argus-sdk`（客户端库） | 有外部用户要用 Rust 调 Argus 的 admin API |

```text
argus/crates/
├── argus-core/
├── argus-protocol/
├── argus-security/     ← 从 proxy 拆出
├── argus-provider/     ← 从 proxy + core 拆出
├── argus-proxy/
├── argus-storage/
├── argus-control/      ← 从 server 拆出
└── argus-server/
```

**注意长期结构里仍然没有** `argus-observer` 与 `argus-auth`。它们永远不该独立——前者是 proxy 的内部 task，后者代码量太小。

---

## 7. 依赖清单

### 7.1 workspace 根

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
rust-version = "1.90"
license = "Apache-2.0"

[workspace.dependencies]
# 运行时
tokio        = { version = "1", features = ["rt-multi-thread","net","time","sync","signal","macros"] }
tokio-util   = { version = "0.7", features = ["rt"] }
# HTTP
hyper        = { version = "1", features = ["client","server","http1","http2"] }
hyper-util   = { version = "0.1", features = ["client-legacy","server-auto","tokio"] }
http         = "1"
http-body    = "1"
http-body-util = "0.1"
axum         = { version = "0.8", default-features = false, features = ["http1","http2","json","tokio"] }
tower        = { version = "0.5", features = ["util","limit"] }
tower-http   = { version = "0.6", features = ["catch-panic","timeout","request-id"] }
# TLS（rustls，不用 openssl）
rustls              = { version = "0.23", default-features = false, features = ["ring","tls12"] }
hyper-rustls        = { version = "0.27", default-features = false, features = ["http1","http2","ring","webpki-tokio"] }
tokio-rustls        = "0.26"
webpki-roots        = "0.26"
# 序列化
serde        = { version = "1", features = ["derive","rc"] }
serde_json   = { version = "1", features = ["raw_value","preserve_order"] }
# 字节与字符串
bytes        = "1"
memchr       = "2"
compact_str  = { version = "0.8", features = ["serde"] }
smallvec     = { version = "1", features = ["union","const_generics"] }
# 并发
arc-swap     = "1"
dashmap      = "6"
parking_lot  = "0.12"
# 存储
sqlx         = { version = "0.8", default-features = false, features = ["runtime-tokio","tls-rustls","postgres","uuid","chrono","json","macros","migrate"] }
zstd         = "0.13"
# 加密
sha2         = "0.10"
aes-gcm      = "0.10"
argon2       = "0.5"
subtle       = "2"
zeroize      = { version = "1", features = ["derive"] }
rand         = "0.8"
# 安全规则
regex        = "1"
aho-corasick = "1"
# 限流
governor     = "0.7"
# 可观测
tracing              = "0.1"
tracing-subscriber   = { version = "0.3", features = ["json","env-filter"] }
tracing-appender     = "0.2"
metrics              = "0.24"
metrics-exporter-prometheus = "0.16"
# 配置与 CLI
figment      = { version = "0.10", features = ["toml","env"] }
clap         = { version = "4", features = ["derive","env"] }
humantime-serde = "1"
# 杂项
uuid         = { version = "1", features = ["v4","v7","serde"] }
chrono       = { version = "0.4", features = ["serde"] }
thiserror    = "2"
anyhow       = "1"
url          = "2"
moka         = { version = "0.12", features = ["sync"] }
rlimit       = "0.10"
memory-stats = "1"

# 可选（feature-gated）
# ⚠️ `optional` 不能写在 [workspace.dependencies]：cargo 会直接拒绝解析
#    （"workspace dependencies cannot be optional"）。可选性由引用方声明，
#    member 侧写 `{ workspace = true, optional = true }` 并配 [features]。
opentelemetry       = { version = "0.27" }
opentelemetry-otlp  = { version = "0.27" }
tracing-opentelemetry = { version = "0.28" }
redis               = { version = "0.27", features = ["tokio-comp","script"] }

# 测试工具
# ⚠️ 不存在 `[workspace.dev-dependencies]` 这张表。cargo 只有一张
#    [workspace.dependencies]，member 的 [dependencies] 与 [dev-dependencies]
#    都通过 `workspace = true` 引用它。所以测试工具也列在上面这张表里。
insta      = { version = "1", features = ["json","redactions"] }
proptest   = "1"
criterion  = { version = "0.5", features = ["html_reports"] }
wiremock   = "0.6"
rstest     = "0.23"

[profile.release]
opt-level     = 3
lto           = "thin"
codegen-units = 4          # 不用 1：编译时间翻倍，收益 < 3%
panic         = "unwind"   # ⚠️ 绝不可改为 abort，见 01-architecture.md §9
strip         = "none"     # 不能 strip debuginfo，否则下一行的 debug = 1 白设
debug         = 1          # 只生成行号表：panic backtrace 有行号，体积代价可接受

[profile.release-dist]     # 发行版：极致优化
inherits      = "release"
lto           = "fat"
codegen-units = 1

[profile.dev]
opt-level     = 0
[profile.dev.package."*"]  # 依赖用 O2 编译，大幅加快 debug 运行速度
opt-level     = 2

[profile.bench]            # 继承 release，避免两个 profile 的 panic 策略不一致
inherits = "release"
```

> 上面三处带 ⚠️ 的注释是 F01 落地时踩到的：前两条会让 `cargo` 直接拒绝解析
> manifest，第三条不会报错但会让 `debug = 1` 完全失效。

### 7.2 各 crate 的依赖约束（CI 强制）

| crate | 允许 | **禁止** |
|-------|------|---------|
| `argus-core` | serde, bytes, http, arc-swap, thiserror, uuid, chrono, compact_str, smallvec, zeroize, humantime-serde | **任何内部 crate**（它是依赖图的根，反向依赖会造成循环）；tokio（除 `sync` 的类型定义）；sqlx；hyper |
| `argus-protocol` | argus-core, serde, serde_json, bytes, memchr, compact_str, smallvec | **tokio, sqlx, hyper, axum, reqwest, std::fs, std::net** |
| `argus-proxy` | argus-core, argus-protocol, tokio, hyper, axum, tower, regex, governor, moka | **sqlx, argus-storage** |
| `argus-storage` | argus-core, argus-protocol, sqlx, tokio, zstd | argus-proxy |
| `argus-server` | 全部 | — |

CI 检查（见 `15-testing-strategy.md` §9）：

```bash
# 断言 argus-proxy 的依赖树中不含 sqlx
! cargo tree -p argus-proxy -e normal | grep -q "^.*sqlx"
# 断言 argus-protocol 不含 tokio
! cargo tree -p argus-protocol -e normal | grep -q "^.*tokio"
```

### 7.3 关键依赖的选型理由

| 选择 | 替代品 | 理由 |
|------|--------|------|
| `hyper` 直接用 | `reqwest` | reqwest 会自动解压/重定向（破坏透明性），且 body 类型不透明。见 `02-data-plane.md` §13-A |
| `rustls` | `openssl` | 静态构建、无系统依赖、无 CVE 追赶（**但见下方 7.3.1**） |
| `sqlx` | `diesel` / `tokio-postgres` | 异步原生；编译期 SQL 校验（可选）；内置迁移 |
| `compact_str` | `String` | ≤24 字节 inline，覆盖绝大多数 tool name / event name，零堆分配 |
| `arc-swap` | `RwLock<Arc<T>>` | 读路径是纯 load，无原子写、无写者饥饿 |
| `governor` | 手写 token bucket | GCRA 实现成熟，能算出精确的 `Retry-After` |
| `moka` | `lru` + Mutex | 分片并发缓存，TTL/TTI 内建 |
| `memchr` | 手写扫描 | SIMD 加速的子串搜索，SseFramer 的热路径 |
| `metrics` | `opentelemetry-metrics` | 更轻；OTel 保持可选 |
| `figment` | `config` | 分层来源与类型化提取更好用 |
| `insta` | 手写断言 | golden snapshot 的标准工具，`cargo insta review` 体验好 |
| `serde_json` 的 `raw_value` | — | `Box<RawValue>` 存 tool arguments 而不构造 DOM |

### 7.3.1 rustls 的加密后端：声明的是 ring，实际跑的是 aws-lc-rs

F01 落地后实测的依赖树：

```
aws-lc-sys v0.44.0 → aws-lc-rs v1.18.0 → rustls v0.23
```

上面 §7.1 明确写了 `rustls = { default-features = false, features = ["ring"] }`，
但 `tokio-rustls` 与 `sqlx` 的默认 feature 打开了 `rustls/aws-lc-rs`，
**cargo 的 feature 统一是并集，选择更多的那一方赢**——`default-features = false`
只约束本 crate 自己的声明，管不住别人替 rustls 开的 feature。

后果：构建期需要 cmake + C 工具链。这与选 rustls 的第一条理由
（静态构建、无系统依赖）直接冲突，也与「单二进制 / NAS 可跑」的目标冲突。

**当前状态：已知，暂不处理**（2026-08-19 决定）。本机与 GitHub runner 都有
cmake，CI 不会因此变红——它会一直潜伏到 F28 做 musl 静态链接时才暴露。

改法（届时执行）：`tokio-rustls` 加 `default-features = false`，
`sqlx` 的 `tls-rustls` 换成 `tls-rustls-ring`，然后用
`cargo tree -i aws-lc-sys` 断言依赖树中不再出现它。

> 换后端会改变 TLS 握手与批量加解密的性能特征。若在 G-M0（F07）之后才换，
> 涉及 TLS 的基线数字需要重测。

### 7.4 `codegen-units = 4` 而非 1

常见的"性能优化"是 `codegen-units = 1`。实测在多数项目上收益 < 3%，但编译时间可能翻倍。开发迭代阶段这个代价不值得。

因此：日常 `release` 用 4，发行版用单独的 `release-dist` profile（`lto = "fat"` + `codegen-units = 1`）。

---

## 8. 编译时间预算（G-P1）

| 场景 | 目标 |
|------|------|
| 冷编译（全量 release） | < 4 分钟（4 核） |
| 改 `argus-protocol` 后 `cargo test -p argus-protocol` | < 8 秒 |
| 改 `argus-proxy` 后 `cargo check` | < 15 秒 |
| 改 `argus-server` 后 `cargo build` | < 20 秒 |

加速手段：
- `sqlx` 只在 `argus-storage`（最重的依赖被隔离）
- `[profile.dev.package."*"] opt-level = 2`（依赖只编一次，之后运行快）
- 用 `cargo-nextest` 跑测试（并行度更好，且能隔离 panic）
- 可选：`mold` / `lld` 链接器（链接时间可降 5–10×）

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

---

## 9. 代码规范（编译期强制）

```rust
// crates/argus-protocol/src/lib.rs
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,      // 强制用 .get()
    clippy::integer_division,
    clippy::unreachable,
    clippy::todo,
)]
#![warn(clippy::pedantic)]
```

其他 crate 用较宽松的规则，但都 `#![forbid(unsafe_code)]`（除非有明确论证的例外，需在代码中写明理由并单独 `#[allow]`）。

```toml
# clippy.toml
disallowed-methods = [
  # 主链路禁止的阻塞调用
  { path = "std::thread::sleep", reason = "use tokio::time::sleep" },
  { path = "std::fs::read", reason = "use tokio::fs or spawn_blocking" },
]
```

---

## 10. Failure Modes

结构层面的"故障"是**架构约束被悄悄破坏**。每一条都有对应的 CI 检查（`15-testing-strategy.md` §9）。

| 场景 | 表现 | 检测 | 处理 |
|------|------|------|------|
| 有人给 `argus-proxy` 加了 `sqlx` 依赖 | Data Plane 可以直连 DB，"不被数据库阻塞"的保证失效 | `cargo tree -p argus-proxy` 断言 | CI 失败 |
| 有人给 `argus-protocol` 加了 `tokio` | 解析层可以做 IO，可能引入阻塞 | 同上 | CI 失败 |
| 有人把 `panic` 改成 `abort` | parser 的一个 bug 会杀死上千条连接 | grep `Cargo.toml` | CI 失败 |
| 有人在 `argus-core` 里 `use argus_protocol::*` | 循环依赖 | `cargo build` 直接失败 | 编译期 |
| `argus-protocol` 中出现 `unwrap()` | 可能 panic，触发 observer 隔离 | clippy `deny(unwrap_used)` | 编译失败 |
| 出现 `unsafe` 块 | 内存安全保证失效 | `#![forbid(unsafe_code)]` + grep | 编译失败 |
| 依赖引入了不兼容许可证 / 已知 CVE | 合规与安全风险 | `cargo-deny` | CI 失败 |
| 未使用的依赖累积 | 编译变慢，攻击面变大 | `cargo-machete` | CI 警告 |
| 编译时间超预算 | 迭代变慢 | CI 记录 `cargo build --timings` | 超过 §8 目标时告警 |
| crate 数量因"组织美感"增长 | 过度设计（§4 的反模式） | 人工评审，需引用 §4 的四条判据 | PR 评审拒绝 |

---

## 11. Trade-offs

| 选择 | 收益 | 代价 |
|------|------|------|
| 5 crate 而非 9 | 编译快、无过度抽象、改动不跨 crate | 未来拆分需要一次重构（但有明确触发条件） |
| `argus-protocol` 独立 | 编译期禁 IO；golden test 秒级 | 事件类型跨 crate，改 IR 要动两处 |
| security 留在 proxy | 少一个 crate；ToolGate 与 Body 耦合自然 | security 长大后要拆 |
| `codegen-units = 4` | 编译时间减半 | release 性能损失 < 3% |
| 依赖用 `[workspace.dependencies]` 集中管理 | 版本一致 | 根 Cargo.toml 很长 |
| `panic = "unwind"` | parser bug 不杀进程 | 二进制略大，性能损失 ~1% |

---

## 12. Alternatives Considered

**A. 单个 crate。**
最快的编译，最简单的结构。但无法在编译期保证 "proxy 不依赖 sqlx" 与 "protocol 不依赖 tokio"——这两条是架构的核心不变式，靠代码评审维持太脆弱。**否决**。

**B. 用户提出的 9 crate 结构。**
每个概念一个 crate。问题：`argus-common` 是垃圾桶命名；`argus-observer` 与 `argus-proxy` 之间会有大量双向类型引用（observer 需要 proxy 的 tee 类型，proxy 需要 observer 的句柄），容易导致循环依赖；`argus-auth` 只有几百行；编译单元多导致链接时间增加。**否决为 MVP**，部分保留为长期结构。

**C. 按层拆（`argus-domain` / `argus-application` / `argus-infrastructure`）。**
DDD 风格。对这个规模的项目是纯粹的仪式性开销。**否决**。

**D. 把 IR 放在 `argus-core` 而不是 `argus-protocol`。**
好处是 storage 不用依赖 protocol。但 IR 与 parser/accumulator 是共同演化的，分开会导致每次改 IR 都要跨 crate 同步。**否决**——让 storage 依赖 protocol 是可接受的（它本来就要序列化 `ObservedInteraction`）。

**E. `codegen-units = 1` + `lto = "fat"` 作为默认 release。**
编译时间从 4 分钟涨到 8–10 分钟，收益不到 3%。**否决为默认**，作为 `release-dist` profile。

---

## 13. Decision

1. **MVP 用 5 个 crate**：`argus-core`、`argus-protocol`、`argus-proxy`、`argus-storage`、`argus-server`。
2. 拆 crate 的唯一合法理由是四条判据（编译期隔离 / 依赖集差异 / 测试密集 / 独立发布），"这是不同概念"不是理由。
3. `argus-protocol` 编译期禁止 tokio/sqlx/hyper/fs/net，并启用最严格的 clippy lint（禁 unwrap/panic/索引）。
4. `argus-proxy` 编译期禁止 sqlx 与 `argus-storage`，通过 `argus-protocol::InteractionSink` 解耦。
5. security / auth / provider / observer **不独立成 crate**；security 有明确的 Phase 2 拆分触发条件。
6. `panic = "unwind"` 在 workspace profile 中固定，注释说明不可更改。
7. `codegen-units = 4` 为默认，`release-dist` 用 fat LTO。
8. CI 用 `cargo tree` 断言依赖约束。
9. 全 workspace `#![forbid(unsafe_code)]`（例外需论证）。

---

## 14. Open Questions

| # | 问题 | 现状 | 何时决定 |
|---|------|------|---------|
| OQ-1 | `security` 的实际代码量会不会很快超过 5000 行？ | 未知 | 实现 MVP 规则集后 |
| OQ-2 | `argus-protocol` 禁止 `hyper` 是否会在实现 Bedrock eventstream 时造成麻烦？ | 应该不会（framer 只吃 `&[u8]`） | 实现 Bedrock 时 |
| OQ-3 | musl 静态构建下是否需要显式引入 jemalloc？ | 未测 | 压测阶段（见 `13-deployment.md` OQ-5） |
| OQ-4 | 是否用 sqlx 的编译期 SQL 校验（`query!` 宏）？它要求编译时有数据库或 offline 数据。 | 倾向用 `query_as` + offline | 实现 storage 时 |
