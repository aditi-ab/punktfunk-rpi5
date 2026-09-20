#!/usr/bin/env bash
# What the bring-up ramp measured, against what `iperf3` says the link is.
#
#   scripts/abr-rig/ramp-truth.sh [runs] [profile ...]
#
# A short session per run: the ramp is over in the first few seconds, and ten
# runs of a profile is the point. Prints the iperf3 reading the run's own
# pre-check took, then each run's steps and outcome. A `*` on a step is a
# repeat: a rate refused without loss, asked once more before it counts.
set -euo pipefail

RUNS=${1:-10}
shift || true
PROFILES=${*:-wan_wg_12 lte_variable wifi_tv_probe_damage lan_1g}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out

for p in $PROFILES; do
  echo "== $p =="
  for r in $(seq "$RUNS"); do
    PF_RIG_SKIP_BUILD=${PF_RIG_SKIP_BUILD:-1} "$HERE/run.sh" "$p" 8 \
      > "$OUT/truth-$p-$r.run" 2>&1 || { echo "  run $r FAILED"; continue; }
    cp "$OUT/$p-1.jsonl" "$OUT/truth-$p-$r.jsonl"
    cp "$OUT/$p-1.log" "$OUT/truth-$p-$r-client.log"
    # The iperf3 the run itself took, as the link's ground truth.
    iperf=$(grep -o "[0-9]* Kbits/sec.*receiver" "$OUT/truth-$p-$r.run" | head -1 | awk '{print $1}')
    printf "  run %-2s iperf3=%-8s " "$r" "${iperf:-?}"
    grep -h '"ramp' "$OUT/truth-$p-$r.jsonl" | tr -d '{}"' | awk -F'[:,]' '
      { delete v; for (i = 1; i <= NF; i += 2) v[$i] = $(i+1) }
      /ramp_step/ { printf "step%s[%s kbps off=%s del=%s int=%sms %s] ",
                    (v["repeat"] == "true" ? "*" : ""),
                    v["asked_kbps"], v["offered_packets"], v["delivered_packets"],
                    v["client_interval_ms"], v["verdict"] }
      /ramp:done/ { printf "-> wall=%s proven=%s steps=%s ms=%s opened=%s",
                    v["wall"], v["proven_kbps"], v["steps"], v["took_ms"], v["opening_kbps"] }'
    echo
  done
done
