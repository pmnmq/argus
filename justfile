# Argus 开发命令入口。
#
# 这个文件是**唯一的事实源**：CI 调用的就是下面这些 recipe，本地 `just ci`
# 与 CI 跑的是同一串命令。两边各写一份迟早会漂移，然后就变成
# 「本地明明是绿的」。
#
# 快速上手：
#   just            列出所有命令
#   just check      日常写代码时用（约 5 秒）
#   just ci         提交前跑一遍 CI 会跑的全部内容（约 1 分钟）
#
# 装 just：brew install just / cargo install just --locked

set shell := ["bash", "-c"]

cargo := env("CARGO", "cargo")
# 全 feature、锁定依赖版本。--locked 让 CI 不会悄悄改 Cargo.lock。
scope := "--all-features --locked"

# 没给参数时显式加 --workspace；给了参数就不加。
#
# 因为 cargo 里 --workspace 会**静默盖掉** -p，`just test -p argus-core` 会
# 变成跑全量而没有任何提示。反过来又不能干脆不写 --workspace ——
# 虚拟根默认等于全部成员只是当前的巧合，将来谁加一行 default-members，
# 无参数那次就会悄悄收窄。所以两种情况分开写。
all := "--workspace"

# 列出所有命令
default:
    @just --list --unsorted

# ── 单项 ────────────────────────────────────────────────────────────

# 格式化（有 nightly rustfmt 就用它，见 ci/fmt.sh）
[group('单项')]
fmt:
    @./ci/fmt.sh

# 只校验格式，不改文件
[group('单项')]
fmt-check:
    @./ci/fmt.sh --check

# 静态检查，警告即失败。例：just clippy -p argus-core
[group('单项')]
clippy *args:
    {{ cargo }} clippy {{ if args == "" { all } else { "" } }} {{ scope }} --all-targets {{ args }} -- -D warnings

# 跑测试。例：just test -p argus-protocol framer
[group('单项')]
test *args:
    {{ cargo }} test {{ if args == "" { all } else { "" } }} {{ scope }} {{ args }}

# 架构不变式（宪法里可机械检查的条款）
[group('单项')]
invariants:
    @./ci/check-invariants.sh

# 护栏的护栏：逐条注入违规，确认检查真的会失败
[group('单项')]
guards:
    @./ci/verify-guards.sh

# 依赖卫生：许可证 / 漏洞 / 来源 / 禁用包
[group('单项')]
deny:
    @command -v cargo-deny >/dev/null 2>&1 || (echo "cargo-deny 未安装，先跑：just tools" && exit 1)
    {{ cargo }} deny --all-features check

# release 构建
[group('单项')]
build:
    {{ cargo }} build {{ all }} {{ scope }} --release

# 跑一下二进制（目前只有自检输出）
[group('单项')]
run *args:
    @{{ cargo }} run -q --bin argus -- {{ args }}

# ── 组合 ────────────────────────────────────────────────────────────

# 日常：格式化 + 静态检查 + 测试 + 不变式
[group('组合')]
check: fmt clippy test invariants
    @echo ""
    @echo "check 通过。提交前建议再跑一次 just ci。"

# 全量：CI 会跑的一切
[group('组合')]
ci: fmt-check clippy invariants guards deny test build
    @echo ""
    @echo "ci 全部通过。"

# ── 杂项 ────────────────────────────────────────────────────────────

# 安装本仓库用到的开发工具
[group('杂项')]
tools:
    {{ cargo }} install cargo-deny --locked
    rustup toolchain install nightly --component rustfmt

# 端到端压测（对照法）。例：just bench-e2e --streams 100 --observe full
[group('杂项')]
bench-e2e *args:
    @./ci/bench-e2e.sh {{ args }}

# G-M0 门禁判定。读 bench/results/ 下的 JSON，未判定项与失败同等对待
[group('杂项')]
gate-m0:
    @./ci/gate-m0.sh

# 清掉构建产物
[group('杂项')]
clean:
    {{ cargo }} clean
