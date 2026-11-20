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
#   --dns-rounds 10 在 full 模式另外采集 DNS 冷/热缓存数据。
#   ARGUS_BENCH_LOADGEN 可指定跨主机 loadgen 包装命令，负责转发传入的回环 URL。
#
# 产物：bench/results/<observe>-<streams>.json
set -euo pipefail

cd "$(dirname "$0")/.."

STREAMS=100
DURATION=30
OBSERVE=full
TOKENS=200
INTERVAL_MS=5
DNS_ROUNDS=0

while [ $# -gt 0 ]; do
  case "$1" in
    --streams)  STREAMS="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --observe)  OBSERVE="$2"; shift 2 ;;
    --tokens)   TOKENS="$2"; shift 2 ;;
    --interval-ms) INTERVAL_MS="$2"; shift 2 ;;
    --dns-rounds) DNS_ROUNDS="$2"; shift 2 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

case "$OBSERVE" in
  full|off) ;;
  *) echo "--observe 只能是 full 或 off" >&2; exit 2 ;;
esac
DURATION=${DURATION%s}
if [[ ! "$DURATION" =~ ^[1-9][0-9]*$ || ! "$DNS_ROUNDS" =~ ^(0|[1-9][0-9]*)$ ]]; then
  echo "--duration 必须是正整数秒，--dns-rounds 必须是非负整数" >&2
  exit 2
fi
if [ "$OBSERVE" = off ] && [ "$DNS_ROUNDS" -ne 0 ]; then
  echo "DNS 冷/热探测使用 --observe full" >&2
  exit 2
fi

OUT_DIR="bench/results"
mkdir -p "$OUT_DIR"
OUT="$OUT_DIR/${OBSERVE}-${STREAMS}.json"

say() { printf '\033[1m==>\033[0m %s\n' "$*" >&2; }

# release 构建：debug 下的数字没有参考价值，且会让对照组与实验组的
# 相对开销失真（Argus 侧代码多，受 -O0 影响更大）
say "构建（release）"
cargo build --locked --release -p argus-proxy --features testkit \
  --example mock-provider --example loadgen >&2
cargo build --locked --release -p argus-server >&2

BIN_MOCK=target/release/examples/mock-provider
BIN_LOAD=${ARGUS_BENCH_LOADGEN:-target/release/examples/loadgen}
BIN_ARGUS=target/release/argus
METRICS=ci/bench-metrics.py

TMP=$(mktemp -d)
MOCK_PID=
ARGUS_PID=
SAMPLER_PID=
cleanup() {
  for pid in "$SAMPLER_PID" "$ARGUS_PID" "$MOCK_PID"; do
    [ -n "$pid" ] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  rm -rf -- "$TMP"
}
trap cleanup EXIT

# ── Mock Provider ───────────────────────────────────────────────────
say "起 mock provider（${TOKENS} tokens，间隔 ${INTERVAL_MS}ms）"
MOCK_URL_FILE=$TMP/mock-url
"$BIN_MOCK" --tokens "$TOKENS" --interval-ms "$INTERVAL_MS" > "$MOCK_URL_FILE" &
MOCK_PID=$!

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
start_argus() {
  ARGUS_ALLOW_LOOPBACK=1 \
  ARGUS_UPSTREAM="$1" \
  ARGUS_OBSERVE="$OBSERVE" \
  ARGUS_BENCH_METRICS=1 \
  ARGUS_PROFILE=mock \
  ARGUS_LISTEN="127.0.0.1:${ARGUS_PORT}" \
  RUST_LOG=warn \
    "$BIN_ARGUS" serve >"$TMP/argus.log" 2>&1 &
  ARGUS_PID=$!

  for _ in $(seq 1 100); do
    if kill -0 "$ARGUS_PID" 2>/dev/null \
      && curl -sf "http://127.0.0.1:${ARGUS_PORT}/metrics" >/dev/null; then
      return
    fi
    sleep 0.1
  done
  cat "$TMP/argus.log" >&2
  echo "Argus 或指标端点启动失败" >&2
  exit 1
}
start_argus "$MOCK_URL"

say "实验组：客户端 → Argus → mock"
SAMPLES=$TMP/resources.jsonl
python3 "$METRICS" sample "$ARGUS_PID" > "$SAMPLES" &
SAMPLER_PID=$!

EXPERIMENT=$("$BIN_LOAD" --url "http://127.0.0.1:${ARGUS_PORT}/provider/mock/v1/messages" \
  --streams "$STREAMS" --duration-s "$DURATION" --label experiment)
kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=

WARMUP=30
[ "$DURATION" -ge 60 ] || WARMUP=$((DURATION / 4))
RESOURCES=$(python3 "$METRICS" summarize "$SAMPLES" "$WARMUP" "$DURATION")

# ── 汇总 ────────────────────────────────────────────────────────────
UNAME=$(uname -srm)
CPUS=$( (nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null) || echo 0)
RUSTC=$(rustc --version)
EXTERNAL_LOADGEN=0
[ -z "${ARGUS_BENCH_LOADGEN:-}" ] || EXTERNAL_LOADGEN=1

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

TEE=null
if [ "$OBSERVE" = full ]; then
  TEE=$(python3 "$METRICS" tee "$ARGUS_PORT" "$EXP_N")
fi

DNS=null
if [ "$DNS_ROUNDS" -gt 0 ]; then
  say "DNS 冷/热缓存探测（${DNS_ROUNDS} 轮）"
  DNS_UPSTREAM=${MOCK_URL/127.0.0.1/localhost}
  for _ in $(seq 1 "$DNS_ROUNDS"); do
    kill "$ARGUS_PID"
    wait "$ARGUS_PID" 2>/dev/null || true
    ARGUS_PID=
    start_argus "$DNS_UPSTREAM"
    python3 "$METRICS" dns-pair "$ARGUS_PORT" >> "$TMP/dns.jsonl"
  done
  DNS=$(python3 "$METRICS" dns-summary "$TMP/dns.jsonl")
fi

cat > "$TMP/result.json" <<JSON
{
  "observe": "${OBSERVE}",
  "streams": ${STREAMS},
  "duration_s": ${DURATION},
  "external_loadgen": ${EXTERNAL_LOADGEN},
  "mock": { "tokens": ${TOKENS}, "interval_ms": ${INTERVAL_MS} },
  "env": {
    "uname": "${UNAME}",
    "cpus": ${CPUS},
    "rustc": "${RUSTC}",
    "date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  },
  "resources": ${RESOURCES},
  "tee": ${TEE},
  "dns_ttft": ${DNS},
  "control": ${CONTROL},
  "experiment": ${EXPERIMENT}
}
JSON
mv "$TMP/result.json" "$OUT"

say "写出 $OUT"
cat "$OUT"
