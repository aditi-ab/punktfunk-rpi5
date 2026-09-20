#!/usr/bin/env bash
# What the link cap did over a run: the summary row, the cap's own log lines,
# and what the lift probe and the drain guard decided — both of which say so in
# the log, so nothing here is reconstructed from the trajectory's slopes.
#
#   scripts/abr-rig/read-linkcap.sh <tag> [link_kbps]
#
# The guard's per-window line is `debug`, so a run wanting the engagement count
# needs RUST_LOG=info,punktfunk_core::abr=debug.
set -euo pipefail

TAG=${1:?usage: read-linkcap.sh <tag> [link_kbps]}
LINK=${2:-0}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out
J=$OUT/$TAG.jsonl

printf '%-16s ' "$TAG"
# A run with the ramp not advertised carries no such line, and that is a row too.
{ grep '"ramp":"done"' "$J" 2>/dev/null || echo '{"wall":"none","proven_kbps":0,"opening_kbps":0}'; } \
  | tr -d '{}"' | awk -F'[:,]' '
  { for (i=1;i<=NF;i+=2) v[$i]=$(i+1)
    printf "ramp[wall=%-5s proven=%-7s opened=%-7s] ", v["wall"], v["proven_kbps"], v["opening_kbps"] }'

grep -v '"ramp' "$J" | grep -v summary | tr -d '{}"' | awk -F'[:,]' -v link="$LINK" '
  { delete v; for (i=1;i<=NF;i+=2) v[$i]=$(i+1)
    if (v["discarded"] == "true") next
    n++; s += v["target_kbps"]; lost += v["lost_frames"]
    if (v["target_kbps"]+0 > pk) pk = v["target_kbps"]+0
    if (v["request_kbps"] != "null" && v["request_kbps"]+0 < v["target_kbps"]+0) {
      cuts++
      if (last_cut_ms && v["t_ms"] - last_cut_ms < 3000) cascade++
      last_cut_ms = v["t_ms"]
    } }
  END { printf "peak=%-7d mean=%-7d", pk, s/n
        if (link > 0) printf " (%.0f%% of link)", (s/n)*100/link
        printf " | cuts=%d cascades=%d lost=%d\n", cuts, cascade, lost }'

# under5_pct / queue_p95_ms are the recorder's own, over the same windows.
tail -1 "$J" | tr -d '{}"' | awk -F'[:,]' '
  { for (i=1;i<=NF;i+=2) v[$i]=$(i+1)
    printf "  windows=%s under5=%s%% queue_p95=%s ms cuts/10min=%s lost/10min=%s\n",
      v["windows"], v["under5_pct"], v["queue_p95_ms"], v["cuts_per_10min"], v["lost_per_10min"] }'

LOG=$OUT/$TAG-client.log
CLEAN=$(mktemp)
sed 's/\x1b\[[0-9;]*m//g' "$LOG" 2>/dev/null > "$CLEAN" || true
trap 'rm -f "$CLEAN"' EXIT

# The lift probe and the guard, from their own lines. A guard that ends
# `drained=true` reached its reference; one that ends after the full budget of
# per-window lines (LINK_DRAIN_WINDOWS - 1 = 5) ran out of budget; anything
# shorter took the escape hatch — loss while the delay was not falling.
awk '
  /refused the lift/ {
    lifts++
    settled = ($0 ~ /settled=true/) ? "settled" : "probing"
    printf "    retreat %d: %s\n", lifts, substr($0, index($0, "from_kbps"))
  }
  /still emptying/ { pending++ }
  /the last cut.s queue is done/ {
    guards++
    match($0, /suppressed_windows=[0-9]+/); sup = substr($0, RSTART+19, RLENGTH-19)
    suppressed += sup
    if ($0 ~ /drained=true/) reached++
    else if (pending >= 5) budget++
    else hatch++
    pending = 0
  }
  END {
    if (pending > 0) guards++          # a guard the run ended inside
    printf "  lifts_retreated=%d | guards=%d (reference=%d budget=%d escape=%d) suppressed_windows=%d\n",
      lifts, guards, reached, budget, hatch, suppressed
  }' "$CLEAN"

echo "  cap + cut lines:"
grep -E "link cap|link ceiling|cut to what the link|carried less than it was asked|re-target|refused the lift" "$CLEAN" \
  | sed -E 's/^([0-9-]+T[0-9:]+)\.[0-9]+Z +INFO +[a-z_:]+: /    \1 /' | head -40
