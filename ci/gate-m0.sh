#!/usr/bin/env bash
# G-M0 判定（F07 §7）。读 ci/bench-e2e.sh 产出的 JSON，逐条判据给结论。
#
# 这是全项目唯一的硬门禁：不达标就停下重新设计 ADR-009，不得进 M1
# （docs/22 §5 风险 1）。所以这个脚本刻意**不**在任何一条上「宽容一下」——
# 判不了的项目一律记 UNKNOWN 并让整体失败，而不是当作通过。
set -uo pipefail

cd "$(dirname "$0")/.."

RESULTS=bench/results
STREAMS=${STREAMS:-10000}

pass=0
fail=0
unknown=0

verdict() { # verdict <PASS|FAIL|UNKNOWN> <编号> <说明>
  case "$1" in
    PASS)    printf '  \033[32mPASS\033[0m    %-4s %s\n' "$2" "$3"; pass=$((pass+1)) ;;
    FAIL)    printf '  \033[31mFAIL\033[0m    %-4s %s\n' "$2" "$3"; fail=$((fail+1)) ;;
    UNKNOWN) printf '  \033[33mUNKNOWN\033[0m %-4s %s\n' "$2" "$3"; unknown=$((unknown+1)) ;;
  esac
}

# 从 JSON 里挖一个数字。用 python 而不是 jq：jq 不是本仓库已有的依赖，
# 而 python3 在 ci/ 下已经用了两个脚本
jnum() { # jnum <文件> <点路径>
  python3 -c '
import json,math,sys
try:
    d = json.load(open(sys.argv[1]))
    for k in sys.argv[2].split("."):
        d = d[k]
    if type(d) in (int, float) and math.isfinite(d):
        print(d)
except Exception:
    print("")
' "$1" "$2" 2>/dev/null
}

FULL="$RESULTS/full-${STREAMS}.json"
OFF="$RESULTS/off-${STREAMS}.json"

echo "=== G-M0 门禁（${STREAMS} 并发流） ==="
echo

# ── G1 / G2：proxy_added_ttft ───────────────────────────────────────
if [ -f "$FULL" ]; then
  A50=$(jnum "$FULL" control.ttft_us.p50)
  B50=$(jnum "$FULL" experiment.ttft_us.p50)
  A99=$(jnum "$FULL" control.ttft_us.p99)
  B99=$(jnum "$FULL" experiment.ttft_us.p99)
  CONTROL_N=$(jnum "$FULL" control.ttft_us.samples)
  EXP_N=$(jnum "$FULL" experiment.ttft_us.samples)
  CONTROL_ERRS=$(jnum "$FULL" control.errors)
  ERRS=$(jnum "$FULL" experiment.errors)

  if [ "$ERRS" = 0 ] && [ "$CONTROL_ERRS" = 0 ] \
    && [ "${CONTROL_N:-0}" -gt 0 ] && [ "${EXP_N:-0}" -gt 0 ] \
    && [ -n "$A50" ] && [ -n "$B50" ] && [ -n "$A99" ] && [ -n "$B99" ]; then
    ADD50=$(( B50 - A50 ))
    ADD99=$(( B99 - A99 ))
    # 阈值来自 docs/22 §3 M0：p50 < 2ms、p99 < 15ms
    [ "$ADD50" -lt 2000 ] \
      && verdict PASS G1 "proxy_added_ttft p50 = ${ADD50}µs（< 2000）" \
      || verdict FAIL G1 "proxy_added_ttft p50 = ${ADD50}µs（要求 < 2000）"
    [ "$ADD99" -lt 15000 ] \
      && verdict PASS G2 "proxy_added_ttft p99 = ${ADD99}µs（< 15000）" \
      || verdict FAIL G2 "proxy_added_ttft p99 = ${ADD99}µs（要求 < 15000）"
  else
    verdict UNKNOWN G1 "$FULL 缺有效 TTFT 样本或压测请求有错误"
    verdict UNKNOWN G2 "同上"
  fi
else
  verdict UNKNOWN G1 "缺 $FULL —— 先跑 ci/bench-e2e.sh --observe full --streams ${STREAMS}"
  verdict UNKNOWN G2 "同上"
fi

# ── G3：RSS ─────────────────────────────────────────────────────────
RSS=$(jnum "$FULL" resources.rss_kb_p99)
if [ -n "$RSS" ] && [ "$RSS" -gt 0 ] 2>/dev/null; then
  # 1.5 GB = 1572864 KB
  [ "$RSS" -lt 1572864 ] \
    && verdict PASS G3 "稳态 RSS p99 = $((RSS / 1024)) MiB（< 1536）" \
    || verdict FAIL G3 "稳态 RSS p99 = $((RSS / 1024)) MiB（要求 < 1536）"
else
  verdict UNKNOWN G3 "没有 RSS 采样"
fi

# ── G4：CPU(Full)/CPU(Off) ──────────────────────────────────────────
CPU_FULL=$(jnum "$FULL" resources.cpu_cores_mean)
CPU_OFF=$(jnum "$OFF" resources.cpu_cores_mean)
if awk -v full="$CPU_FULL" -v off="$CPU_OFF" 'BEGIN { exit !(full > 0 && off > 0) }'; then
  CPU_GROWTH=$(awk -v full="$CPU_FULL" -v off="$CPU_OFF" 'BEGIN { printf "%.2f", (full / off - 1) * 100 }')
  if awk -v full="$CPU_FULL" -v off="$CPU_OFF" 'BEGIN { exit !(full < off * 1.3) }'; then
    verdict PASS G4 "CPU 增幅 = ${CPU_GROWTH}%（< 30%）"
  else
    verdict FAIL G4 "CPU 增幅 = ${CPU_GROWTH}%（要求 < 30%）"
  fi
else
  verdict UNKNOWN G4 "需要 full 和 off 两组有效的 Linux CPU 样本"
fi

# ── G5：切分不变性 ──────────────────────────────────────────────────
if cargo test -q -p argus-protocol --test anthropic_split --test split >/dev/null 2>&1; then
  verdict PASS G5 "SplitStrategy 全矩阵一致"
else
  verdict FAIL G5 "切分不变性矩阵有失败用例"
fi

# ── G6：主链路零分配 ────────────────────────────────────────────────
if cargo test -q -p argus-proxy --test forward_alloc >/dev/null 2>&1; then
  verdict PASS G6 "转发一帧的堆分配 = 0"
else
  verdict FAIL G6 "转发路径有堆分配"
fi

# ── G7：丢弃率 ──────────────────────────────────────────────────────
TEE_STREAMS=$(jnum "$FULL" tee.streams)
COMPLETED=$(jnum "$FULL" experiment.completed)
FORWARDED=$(jnum "$FULL" tee.bytes_forwarded)
DROPPED=$(jnum "$FULL" tee.bytes_dropped)
if [ "${COMPLETED:-0}" -gt 0 ] && [ "$TEE_STREAMS" = "$COMPLETED" ] \
  && [ "${FORWARDED:-0}" -gt 0 ] && [ -n "$DROPPED" ]; then
  DROP_RATE=$(awk -v f="$FORWARDED" -v d="$DROPPED" 'BEGIN { printf "%.4f", 100 * d / f }')
  if [ "$DROPPED" -ge 0 ] && awk -v f="$FORWARDED" -v d="$DROPPED" 'BEGIN { exit !(100 * d < f) }'; then
    verdict PASS G7 "观测丢弃率 = ${DROP_RATE}%（< 1%）"
  else
    verdict FAIL G7 "观测丢弃率 = ${DROP_RATE}%（要求 < 1%）"
  fi
else
  verdict UNKNOWN G7 "缺 tee 字节采样或已收尾的观测流数量与压测不符"
fi

# ── v2 追加项 ───────────────────────────────────────────────────────
echo
echo "=== 7b：v2 追加的门禁项 ==="
if cargo test -q -p argus-proxy --test resolver_matrix --test resolver_path_extractor \
     --test resolver_props >/dev/null 2>&1; then
  verdict PASS URL "URL 解析矩阵全绿"
else
  verdict FAIL URL "URL 解析矩阵有失败用例"
fi
if cargo test -q -p argus-proxy --test passthrough --test scenarios >/dev/null 2>&1; then
  verdict PASS BYTE "字节级透明性测试通过"
else
  verdict FAIL BYTE "字节级透明性测试有失败用例"
fi
DNS_HITS=$(jnum "$FULL" dns_ttft.hit_us.samples)
DNS_MISSES=$(jnum "$FULL" dns_ttft.miss_us.samples)
DNS_HIT_P50=$(jnum "$FULL" dns_ttft.hit_us.p50)
DNS_MISS_P50=$(jnum "$FULL" dns_ttft.miss_us.p50)
DNS_HIT_P99=$(jnum "$FULL" dns_ttft.hit_us.p99)
DNS_MISS_P99=$(jnum "$FULL" dns_ttft.miss_us.p99)
if [ "${DNS_HITS:-0}" -gt 0 ] && [ "$DNS_HITS" = "$DNS_MISSES" ] \
  && [ -n "$DNS_HIT_P50" ] && [ -n "$DNS_MISS_P50" ] \
  && [ -n "$DNS_HIT_P99" ] && [ -n "$DNS_MISS_P99" ]; then
  verdict PASS DNS "TTFT p50/p99：命中 ${DNS_HIT_P50}/${DNS_HIT_P99}µs，未命中 ${DNS_MISS_P50}/${DNS_MISS_P99}µs（各 ${DNS_HITS} 个样本）"
else
  verdict UNKNOWN DNS "缺 DNS 命中/未命中的成对样本（full 压测加 --dns-rounds）"
fi

echo
echo "─────────────────────────────────────────"
printf '通过 %d，失败 %d，未判定 %d\n' "$pass" "$fail" "$unknown"
if [ "$fail" -gt 0 ] || [ "$unknown" -gt 0 ]; then
  echo
  echo "G-M0 未通过。未判定的项目与失败同等对待：门禁的意义在于"
  echo "「证明假设成立」，而不是「没有证据表明它不成立」。"
  echo "处置见 F07 §7：写 bench/M0-FAILURE.md，说明哪条不达标、归因、备选方案。"
  exit 1
fi
if [ "$STREAMS" -eq 10000 ]; then
  if [ "$(jnum "$FULL" external_loadgen)" != 1 ] || [ "$(jnum "$OFF" external_loadgen)" != 1 ]; then
    echo "试跑指标全部通过，正式门禁要求压测客户端分机部署。"
    exit 1
  fi
  if [ "$(jnum "$FULL" env.cpus)" != 4 ] || [ "$(jnum "$OFF" env.cpus)" != 4 ]; then
    echo "试跑指标全部通过，正式门禁要求在 4 核环境采集两组数据。"
    exit 1
  fi
  echo "G-M0 指标判定全部通过，请按 F07 确认测试环境。"
else
  echo "试跑指标全部通过，正式门禁要求 10,000 并发"
fi
