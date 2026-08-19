#!/usr/bin/env bash
# 架构不变式检查（F01 T7 / docs/15-testing-strategy.md §9）
#
# 每条检查都对应 specs/constitution.md 中的一条，失败时必须打印条款号，
# 这样看到红色 CI 的人不用猜"为什么不让我这么写"。
#
# 用法： ./ci/check-invariants.sh
# 退出码：0 = 全部通过；1 = 有违反（fail-fast，第一条失败即退出）
set -euo pipefail

cd "$(dirname "$0")/.."

CONST="specs/constitution.md"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 注意全部写到 stderr：fail 可能在一个 stdout 被重定向到文件的函数里被调用
# （deps_of 就是），写 stdout 会让失败信息神秘消失。
fail() {  # fail <条款> <说明> [可操作提示]
  {
    echo ""
    echo "FAIL [$1] $2"
    [ $# -ge 3 ] && echo "      → $3"
    echo "      → 条款原文：$CONST  (搜索 '### $1 ')"
  } >&2
  exit 1
}
ok() { echo "  ok   $1"; }

# 取某个 crate 的全部传递依赖包名（仅 normal 边，含所有 feature）
deps_of() {
  local out err
  err="$TMP/tree-err.$1"
  # ⚠️ 绝不能用 2>&1：cargo 会把 "Locking 1 package" / "Adding foo v0.1.0"
  #    这类进度信息写到 stderr，混进来之后 awk 会把 "Adding"、以及被顶到
  #    第二行的根包名当成依赖 —— 结果就是"argus-core 依赖了 argus-core"这种鬼话。
  if ! out="$(cargo tree -q -p "$1" -e normal --all-features --prefix none 2>"$err")"; then
    # cargo tree 解析失败最常见的原因就是循环依赖 —— 那本身就是 C6 的违反，
    # 不能让它变成一次"静默通过"。
    fail C6 "cargo tree 无法解析 $1 的依赖树（通常意味着出现了循环依赖）" \
      "cargo 原文：$(grep -iE 'cyclic|error' "$err" | head -2 | tr '\n' ' ')"
  fi
  printf '%s\n' "$out" | awk 'NR>1 {print $1}' | sed 's/[()]//g' | grep -v '^$' | sort -u || true
}

# 在依赖列表中查找禁止的包前缀
has_dep() {  # has_dep <deps文件> <包名前缀>
  grep -qE "^$2(\$|-)" "$1"
}


# ---------------------------------------------------------------------------
echo "=== [C32] crate 集合 ==="
# 拆 crate 需要理由。新增 crate 必须先在 docs/14 §4 给出判据，再改这一行。
EXPECTED="argus-core argus-protocol argus-proxy argus-server argus-storage"
ACTUAL="$(ls -1 crates | sort | tr '\n' ' ' | sed 's/ $//')"
[ "$ACTUAL" = "$EXPECTED" ] || fail C32 \
  "crates/ 目录与已登记的 crate 集合不一致" \
  "期望：${EXPECTED} / 实际：${ACTUAL}。新增 crate 需满足 docs/14 §4 四条判据之一，并同步更新本脚本。"
ok "5 个 crate，与 docs/14 §5 一致"

# ---------------------------------------------------------------------------
echo "=== [C6] 依赖方向 ==="
for c in argus-core argus-protocol argus-proxy argus-storage argus-server; do
  deps_of "$c" > "$TMP/$c.deps"
done

# argus-core 是依赖图的根：不得依赖任何内部 crate
grep -E '^argus-' "$TMP/argus-core.deps" > "$TMP/core.bad" 2>/dev/null || true
[ -s "$TMP/core.bad" ] && fail C6 \
  "argus-core 依赖了内部 crate：$(tr '\n' ' ' < "$TMP/core.bad")" \
  "argus-core 是依赖图的根，反向依赖即循环。把共享类型下沉到 argus-core，而不是让它向上依赖。"
ok "argus-core 不依赖任何内部 crate"

# argus-protocol：纯计算，编译期不可能做 IO
for banned in tokio hyper sqlx axum reqwest tower; do
  has_dep "$TMP/argus-protocol.deps" "$banned" && fail C6 \
    "argus-protocol 依赖了 $banned" \
    "解析层必须是纯计算的（给定字节 → 产出事件）。需要 IO 的部分放到 argus-proxy，用 trait 反向注入。"
done
ok "argus-protocol 无 tokio/hyper/sqlx/axum/reqwest/tower"

# argus-proxy：数据面不得直接依赖存储
for banned in sqlx argus-storage; do
  has_dep "$TMP/argus-proxy.deps" "$banned" && fail C6 \
    "argus-proxy 依赖了 $banned" \
    "持久化必须通过 argus_protocol::sink::InteractionSink 注入，由 argus-server 在组装期绑定（docs/10 §4.1）。"
done
ok "argus-proxy 无 sqlx / argus-storage"

# argus-storage 不得反向依赖数据面
has_dep "$TMP/argus-storage.deps" "argus-proxy" && fail C6 \
  "argus-storage 依赖了 argus-proxy" \
  "存储层只认 argus-core / argus-protocol 的类型。"
ok "argus-storage 不依赖 argus-proxy"

# ---------------------------------------------------------------------------
echo "=== [C4] provider SDK 禁入运行时 ==="
# SDK 只能出现在 dev-dependencies / 测试工具里（它是 oracle，不是运行时依赖）。
SDKS='async-openai|openai-api-rs|openai|anthropic|anthropic-sdk|claude-sdk|aws-sdk-bedrockruntime|google-generative-ai|gemini-rs|langchain'
for c in argus-core argus-protocol argus-proxy argus-storage argus-server; do
  if grep -qE "^($SDKS)(\$|-)" "$TMP/$c.deps"; then
    fail C4 "$c 的运行时依赖树中出现 provider SDK：$(grep -E "^($SDKS)(\$|-)" "$TMP/$c.deps" | tr '\n' ' ')" \
      "SDK 只能进 [dev-dependencies]。运行时引入 SDK 会把它的重试/解压/字段裁剪行为带进主链路，破坏透明代理。"
  fi
done
ok "运行时依赖树中无 provider SDK"

# ---------------------------------------------------------------------------
echo "=== [C7] panic 策略 ==="
# 注意：必须去掉注释再匹配。文件里那句"绝不可改为 abort"的注释本身就含
# `panic = "unwind"`，直接 grep 会让"把真正的声明删掉"这种改动安静通过。
sed 's/#.*//' Cargo.toml | grep -qE '^[[:space:]]*panic[[:space:]]*=[[:space:]]*"unwind"[[:space:]]*$' \
  || fail C7 \
  'Cargo.toml 的 [profile.release] 缺少 panic = "unwind"（注释里写不算）' \
  "缺省值虽然也是 unwind，但必须显式写出来，否则没人知道它不能改。"
find . -name Cargo.toml -not -path './target/*' -print0 \
  | xargs -0 -I{} sh -c 'sed "s/#.*//" "{}" | grep -qE "^[[:space:]]*panic[[:space:]]*=[[:space:]]*\"abort\"" && echo "{}"' \
  > "$TMP/abort" 2>/dev/null || true
if [ -s "$TMP/abort" ]; then
  fail C7 "出现了 panic = \"abort\"：$(tr '\n' ' ' < "$TMP/abort")" \
    "abort 会让单个请求的 panic 杀掉整个进程，连带切断数千条 SSE 长连接。见 docs/01 §9。"
fi
ok 'panic = "unwind"，且全仓无 abort'

# ---------------------------------------------------------------------------
echo "=== [C9] 无界队列/缓冲 ==="
# `unbounded_channel::<u8>()` 的 turbofish 会插在名字和括号之间，
# 所以不能要求 `(` 紧跟其后；同时先去注释，避免文档里提一句就误报。
find crates -name '*.rs' -print0 \
  | xargs -0 grep -nE '(^|[^A-Za-z0-9_])(unbounded_channel|unbounded)([^A-Za-z0-9_]|$)' 2>/dev/null \
  | sed 's://.*::' \
  | grep -E '(^|[^A-Za-z0-9_])(unbounded_channel|unbounded)([^A-Za-z0-9_]|$)' \
  > "$TMP/unbounded" 2>/dev/null || true
if [ -s "$TMP/unbounded" ]; then
  fail C9 "发现无界 channel：$(head -3 "$TMP/unbounded" | tr '\n' ' ')" \
    "所有队列必须有显式上限。满了要降级（丢观测、置 gap_before），不是无限增长直到 OOM。见 docs/02 §5。"
fi
ok "无 unbounded_channel / unbounded()"

# ---------------------------------------------------------------------------
echo "=== [C8] unsafe ==="
# 去掉行注释后再扫，避免文档里提到 unsafe 就误报；忽略 #![forbid(unsafe_code)] 本身。
if find crates -name '*.rs' -print0 \
   | xargs -0 grep -nE '(^|[^A-Za-z_])unsafe([^A-Za-z_]|$)' 2>/dev/null \
   | sed 's://.*::' \
   | grep -E '(^|[^A-Za-z_])unsafe([^A-Za-z_]|$)' \
   | grep -v 'unsafe_code' > "$TMP/unsafe" 2>/dev/null; then
  if [ -s "$TMP/unsafe" ]; then
    fail C8 "发现 unsafe：$(head -3 "$TMP/unsafe" | tr '\n' ' ')" \
      "全 workspace 禁 unsafe。确有必要时需在 PR 中给出论证并单独 #[allow]，不要动 #![forbid]。"
  fi
fi
ok "无 unsafe 块"

# ---------------------------------------------------------------------------
echo "=== [C5] 主链路不等待旁路 ==="
# tee/gate 是主链路把数据交给旁路的唯一出口：这里只能 try_send，绝不能 .await。
# F01 时这些文件还不存在，检查处于"待激活"状态（F04/F25 落地后自动生效）。
GUARDED=0
for f in crates/argus-proxy/src/tee.rs crates/argus-proxy/src/gate.rs; do
  [ -f "$f" ] || continue
  GUARDED=1
  if sed 's://.*::' "$f" | grep -nE '\.(send|send_timeout|reserve)[[:space:]]*\(' > "$TMP/c5" 2>/dev/null; then
    fail C5 "$f 中出现了会 await 的 send：$(head -2 "$TMP/c5" | tr '\n' ' ')" \
      "主链路只能 try_send。旁路慢/满时丢观测并累计 gap_before，绝不反压主链路。见 docs/02 §5.1。"
  fi
done
if [ "$GUARDED" = 1 ]; then ok "tee/gate 中无阻塞 send"; else echo "  skip tee.rs/gate.rs 尚未存在（F04/F25 落地后自动生效）"; fi

# ---------------------------------------------------------------------------
echo "=== [C27] 日志敏感字段 ==="
python3 ci/check_log_fields.py crates || exit 1

# ---------------------------------------------------------------------------
# 这一条不是宪法条款，是 docs/15 §4.1 的约定。放在这里是因为它和上面几条
# 一样：能机械检查的东西就不该靠 review 记着。
echo "=== [docs/15 §4.1] 测试命名 ==="
python3 ci/check_test_names.py crates || exit 1

echo ""
echo "全部架构不变式通过。"
