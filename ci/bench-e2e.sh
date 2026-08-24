#!/usr/bin/env bash
# 对照法端到端压测（docs/21 §5.3、F07 R3/R5）。
#
#   A: 客户端 → Mock            → TTFT_a
#   B: 客户端 → Argus → Mock    → TTFT_b
#   proxy_added_ttft = TTFT_b - TTFT_a
#
# 两轮用**同一个 loadgen 二进制、同一份负载**，只改 --url。这是差值可信
# 的前提：换客户端、换负载脚本，量到的差值里就混进了与 Argus 无关的东西。
#
# 用法：
#   ci/bench-e2e.sh --streams 100 --observe full --duration 30
#   ci/bench-e2e.sh --streams 100 --observe off  --duration 30
#
# 产物：bench/results/<observe>-<streams>.json
set -euo pipefail

cd "$(dirname "$0")/.."

STREAMS=100
DURATION=30
OBSERVE=full
TOKENS=200
INTERVAL_MS=5

while [ $# -gt 0 ]; do
  case "$1" in
    --streams)  STREAMS="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --observe)  OBSERVE="$2"; shift 2 ;;
    --tokens)   TOKENS="$2"; shift 2 ;;
    --interval-ms) INTERVAL_MS="$2"; shift 2 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

case "$OBSERVE" in
  full|off) ;;
  *) echo "--observe 只能是 full 或 off" >&2; exit 2 ;;
esac

OUT_DIR="bench/results"
mkdir -p "$OUT_DIR"
OUT="$OUT_DIR/${OBSERVE}-${STREAMS}.json"

say() { printf '\033[1m==>\033[0m %s\n' "$*" >&2; }

# release 构建：debug 下的数字没有参考价值，且会让对照组与实验组的
# 相对开销失真（Argus 侧代码多，受 -O0 影响更大）
say "构建（release）"
cargo build --release -p argus-proxy --features testkit \
  --example mock-provider --example loadgen >&2
cargo build --release -p argus-server >&2

BIN_MOCK=target/release/examples/mock-provider
BIN_LOAD=target/release/examples/loadgen
BIN_ARGUS=target/release/argus

PIDS=()
cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT

# ── Mock Provider ───────────────────────────────────────────────────
say "起 mock provider（${TOKENS} tokens，间隔 ${INTERVAL_MS}ms）"
MOCK_URL_FILE=$(mktemp)
"$BIN_MOCK" --tokens "$TOKENS" --interval-ms "$INTERVAL_MS" > "$MOCK_URL_FILE" &
PIDS+=($!)

for _ in $(seq 1 100); do
  MOCK_URL=$(head -n1 "$MOCK_URL_FILE" 2>/dev/null || true)
  [ -n "$MOCK_URL" ] && break
  sleep 0.1
done
[ -n "${MOCK_URL:-}" ] || { echo "mock provider 没有报出 base_url" >&2; exit 1; }
say "mock provider: $MOCK_URL"

# ── A 组：直连 mock ─────────────────────────────────────────────────
say "对照组：客户端 → mock"
CONTROL=$("$BIN_LOAD" --url "${MOCK_URL}/v1/messages" \
  --streams "$STREAMS" --duration-s "$DURATION" --label control)

# ── B 组：经 Argus ──────────────────────────────────────────────────
say "起 Argus（observe=${OBSERVE}）"
ARGUS_PORT=$(( 18000 + RANDOM % 2000 ))
# ARGUS_PROFILE 必须与 loadgen 请求里的 /provider/<name>/ 对上，
# 否则每一发都是 404 —— 而 404 是「极快的响应」，会让实验组的分位数
# 比对照组还好看
ARGUS_ALLOW_LOOPBACK=1 \
ARGUS_UPSTREAM="$MOCK_URL" \
ARGUS_OBSERVE="$OBSERVE" \
ARGUS_PROFILE=mock \
ARGUS_LISTEN="127.0.0.1:${ARGUS_PORT}" \
  "$BIN_ARGUS" serve >/dev/null 2>&1 &
ARGUS_PID=$!
PIDS+=("$ARGUS_PID")

for _ in $(seq 1 100); do
  if curl -sf "http://127.0.0.1:${ARGUS_PORT}/healthz" >/dev/null 2>&1; then break; fi
  sleep 0.1
done
curl -sf "http://127.0.0.1:${ARGUS_PORT}/healthz" >/dev/null 2>&1 \
  || { echo "Argus 没起来" >&2; exit 1; }

# 资源采样跟着实验组一起跑。用 ps 而不是 /proc/self/status：后者只有
# Linux 有，而这套脚本要能在开发机上先跑通再上门禁机器
say "实验组：客户端 → Argus → mock"
SAMPLES=$(mktemp)
(
  while kill -0 "$ARGUS_PID" 2>/dev/null; do
    ps -o rss=,time= -p "$ARGUS_PID" 2>/dev/null >> "$SAMPLES" || true
    sleep 1
  done
) &
PIDS+=($!)

EXPERIMENT=$("$BIN_LOAD" --url "http://127.0.0.1:${ARGUS_PORT}/provider/mock/v1/messages" \
  --streams "$STREAMS" --duration-s "$DURATION" --label experiment)

# RSS 取稳态：跳过前 25% 的样本（预热），再取最大值
RSS_KB=$(awk '{print $1}' "$SAMPLES" | sort -n | awk '
  { v[NR] = $1 }
  END { if (NR == 0) { print 0 } else { print v[NR] } }')

# ── 汇总 ────────────────────────────────────────────────────────────
UNAME=$(uname -srm)
CPUS=$( (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null) || echo 0)
RUSTC=$(rustc --version)

# 一致性闸门：两组跑的必须是同一件事。完成数差一个数量级，说明有一侧
# 根本没在做该做的事（最常见的是实验组全是 404），这时产出的 JSON 是
# 一份看起来很正常的废数据 —— 比没有数据更糟
CONTROL_N=$(printf '%s' "$CONTROL" | sed -n 's/.*"completed": \([0-9]*\).*/\1/p')
EXP_N=$(printf '%s' "$EXPERIMENT" | sed -n 's/.*"completed": \([0-9]*\).*/\1/p')
EXP_ERR=$(printf '%s' "$EXPERIMENT" | sed -n 's/.*"errors": \([0-9]*\).*/\1/p')
CONTROL_ERR=$(printf '%s' "$CONTROL" | sed -n 's/.*"errors": \([0-9]*\).*/\1/p')

# 两侧都要查。只查实验组的话，一次「对照组全军覆没」会产出一份
# 差值等于实验组绝对值的 JSON —— 数字看着还挺正常
for side in control:"$CONTROL_N":"$CONTROL_ERR" experiment:"$EXP_N":"$EXP_ERR"; do
  name=${side%%:*}; rest=${side#*:}; n=${rest%%:*}; err=${rest#*:}
  if [ "${n:-0}" -eq 0 ]; then
    echo "${name} 一条流都没跑完 —— 数字不可用" >&2
    exit 1
  fi
  if [ "${err:-0}" -gt 0 ]; then
    echo "${name} 有 ${err} 次失败 —— 先修好再谈数字" >&2
    exit 1
  fi
done

if [ $(( EXP_N * 10 )) -lt "$CONTROL_N" ] || [ $(( CONTROL_N * 10 )) -lt "$EXP_N" ]; then
  echo "两组完成数差了一个数量级（对照 ${CONTROL_N} / 实验 ${EXP_N}）" >&2
  echo "它们没有在测同一件事，数字不可用" >&2
  exit 1
fi

cat > "$OUT" <<JSON
{
  "observe": "${OBSERVE}",
  "streams": ${STREAMS},
  "duration_s": ${DURATION},
  "mock": { "tokens": ${TOKENS}, "interval_ms": ${INTERVAL_MS} },
  "env": {
    "uname": "${UNAME}",
    "cpus": ${CPUS},
    "rustc": "${RUSTC}",
    "date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  },
  "argus_rss_kb_peak": ${RSS_KB},
  "control": ${CONTROL},
  "experiment": ${EXPERIMENT}
}
JSON

say "写出 $OUT"
cat "$OUT"
