#!/usr/bin/env bash
# 格式化（或校验格式）。
#
# 为什么需要这个包装：rustfmt.toml 里的 `imports_granularity` 是 nightly-only。
# 在 stable 上 rustfmt 会对**每个文件**打一条 "unstable features are only
# available in nightly channel" 的 warning，然后忽略它 —— 几十行噪音，
# 而且训练人无视 rustfmt 的输出。
#
# 这里的处理：有 nightly rustfmt 就用它（选项真的生效），没有就退回 stable
# 并把那条已知 warning 收掉，只留一行说明。
#
# 已验证：nightly 合并过的 import，stable 的 `cargo fmt --check` 照样通过
# —— 两边不会来回打架。因此 CI 的校验步骤保持在 stable，不引入 nightly 依赖。
#
# 用法：
#   ./ci/fmt.sh          格式化
#   ./ci/fmt.sh --check  只校验，不改文件
set -o pipefail

cd "$(dirname "$0")/.."

if rustup run nightly rustfmt --version >/dev/null 2>&1; then
  echo "rustfmt: nightly（imports_granularity 生效）"
  exec cargo +nightly fmt --all "$@"
fi

echo "rustfmt: stable（imports_granularity 被忽略）"
echo "         启用它：rustup toolchain install nightly --component rustfmt"
out="$(cargo fmt --all "$@" 2>&1)"
rc=$?
# 只滤掉那一条已知的 nightly 提示，其余原样透出
printf '%s' "$out" | grep -v 'imports_granularity' | grep -v '^[[:space:]]*$'
exit "$rc"
