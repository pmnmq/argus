# F01 · 骨架与架构护栏

| | |
|---|---|
| ID | F01 |
| 里程碑 | M0 |
| 依赖 | — |
| 主 crate | （全部） |
| 设计依据 | `docs/14-rust-project-structure.md` §5,§7,§9；`docs/15-testing-strategy.md` §9 |
| 宪法条款 | C4, C6, C7, C8, C9, C27, C32 |

## 1. 目标

建立 5-crate workspace，并把宪法中的**编译期/CI 可强制条款**变成真正会失败的检查——在写任何业务代码之前。

## 2. 范围

**In scope**：workspace 与 5 个空 crate、`Cargo.toml` profile、clippy/lint 配置、`ci/check-invariants.sh`、`deny.toml`、`rust-toolchain.toml`、`.cargo/config.toml`、CI 工作流。

**Out of scope**：任何业务类型（→ F02）、任何 IO（→ F06）。

## 3. 需求

- **R1** THE SYSTEM SHALL 由 `argus-core`、`argus-protocol`、`argus-proxy`、`argus-storage`、`argus-server` 五个 crate 构成，依赖方向符合宪法 C6。
- **R2** IF 有人给 `argus-proxy` 添加 `sqlx` 或 `argus-storage` 依赖 THEN CI SHALL 失败并打印违反的条款编号。
- **R3** IF 有人给 `argus-protocol` 添加 `tokio` / `hyper` / `sqlx` / `axum` 依赖 THEN CI SHALL 失败。
- **R4** IF `Cargo.toml` 中出现 `panic = "abort"` THEN CI SHALL 失败。
- **R5** IF 代码中出现 `unbounded_channel` 或 `unbounded()` THEN CI SHALL 失败。
- **R6** IF `argus-protocol` 中出现 `unwrap()` / `expect()` / `panic!` / 切片索引 THEN 编译 SHALL 失败。
- **R7** IF 任何 crate 中出现 `unsafe` 块 THEN 编译 SHALL 失败。
- **R8** IF `tracing` 宏的字段名命中敏感词黑名单 THEN CI SHALL 失败。
- **R9** THE SYSTEM SHALL 通过 `cargo-deny` 检查许可证与已知漏洞。
- **R10** WHEN 执行 `cargo build --release` THE SYSTEM SHALL 在 4 核机器上于 4 分钟内完成。

## 4. 接口契约

```
argus/
├── Cargo.toml            # workspace + [workspace.dependencies] + profile
├── rust-toolchain.toml   # channel = "1.95", components = [clippy, rustfmt]
├── deny.toml
├── clippy.toml           # disallowed-methods
├── .cargo/config.toml    # mold 链接器（可选）
├── ci/
│   ├── check-invariants.sh
│   └── check_log_fields.py
├── crates/
│   ├── argus-core/src/lib.rs
│   ├── argus-protocol/src/lib.rs
│   ├── argus-proxy/src/lib.rs
│   ├── argus-storage/src/lib.rs
│   └── argus-server/src/main.rs
└── .github/workflows/ci.yml
```

`crates/argus-protocol/src/lib.rs` 头部（宪法 C7、C8 的编译期实现）：

```rust
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used, clippy::expect_used, clippy::panic,
    clippy::indexing_slicing, clippy::integer_division,
    clippy::unreachable, clippy::todo,
)]
#![warn(clippy::pedantic)]
```

`ci/check-invariants.sh` 的每条检查必须打印它强制的宪法条款号，例如：

```
FAIL [C6] argus-proxy depends on sqlx — see specs/constitution.md#c6
```

## 5. 实现要点

- workspace 依赖清单直接采用 `docs/14-rust-project-structure.md` §7.1，**不要自行调整版本**。
- `[profile.release]` 用 `codegen-units = 4`（不是 1），发行版走单独的 `release-dist`。依据同上 §7.4。
- 五个 crate 此时都是空的（只有 `lib.rs` / `main.rs` 骨架），但**依赖关系要先声明好**，这样 R2/R3 从第一天就在被检查。
- `check-invariants.sh` 用 `set -euo pipefail`，每条检查独立、失败即退出并给出可操作提示。

## 6. 任务

- [x] T1 `cargo new` 改造为 workspace，创建 5 个 crate → `Cargo.toml`, `crates/*/`
- [x] T2 填入 `[workspace.dependencies]`（照抄 `docs/14` §7.1）→ `Cargo.toml`
- [x] T3 配置 `[profile.release]`（`panic = "unwind"` + 注释说明不可改）→ `Cargo.toml`
- [x] T4 各 crate 的 lint 头部与 `Cargo.toml` 依赖声明 → `crates/*/src/lib.rs`
- [x] T5 `rust-toolchain.toml` + `clippy.toml`（disallowed-methods）+ `.cargo/config.toml`
- [x] T6 `deny.toml`（licenses / advisories / bans）
- [x] T7 `ci/check-invariants.sh`：实现 R2–R5、R7 的检查，每条打印条款号
- [x] T8 `ci/check_log_fields.py`：R8 的敏感字段黑名单扫描
- [x] T9 `.github/workflows/ci.yml`：fmt → clippy → check-invariants → deny → test → build
- [x] T10 为每条 CI 检查写一个**故意违反的样例**并确认它真的失败（见 §7 验收）

## 7. 验收

```bash
# 基础
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo build --release --locked

# 护栏必须真的会失败 —— 逐条注入违规再回滚
./ci/verify-guards.sh   # T10 产出：对每条检查做 inject → expect-fail → revert
```

`ci/verify-guards.sh` 必须覆盖：

| 注入 | 期望 |
|---|---|
| 给 `argus-proxy` 加 `sqlx` | check-invariants 退出码 ≠ 0，输出含 `[C6]` |
| 给 `argus-protocol` 加 `tokio` | 同上 |
| 改 `panic = "abort"` | 输出含 `[C7]` |
| 写一个 `mpsc::unbounded_channel()` | 输出含 `[C9]` |
| `argus-protocol` 里写 `x.unwrap()` | `cargo clippy` 失败 |
| 任意 crate 写 `unsafe {}` | `cargo build` 失败 |
| `tracing::info!(prompt = %body)` | `check_log_fields.py` 失败 |

**这一步不能跳过。** 没验证过的护栏等于没有护栏——后面 27 个 spec 都依赖它们真的会拦。

## 8. 交给后续

| 不做的事 | 承接 |
|---|---|
| 任何领域类型 | F02 |
| `cargo-machete` / 编译时间门禁 | F28 |
| 迁移兼容性检查（`check-migration-compat.sh`） | F12 |
| golden test 的 `coverage_matrix` 检查 | F10 |
| 把 rustls 的加密后端从 aws-lc-rs 换回 ring | F28（R8），背景见 `docs/14` §7.3.1 |
