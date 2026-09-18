#!/usr/bin/env bash
# What the link cap did over a run: the summary row, the cap's own log lines,
# and the drain guard — which logs nothing, so it is reconstructed from the
# delay trend the trajectory carries (4 windows after a link-attributed cut,
# each one whose delay fell faster than 5 000 µs).
#
#   scripts/abr-rig/read-linkcap.sh <tag> [link_kbps]
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
    n++; s += v["target_kbps"]
    if (v["target_kbps"]+0 > pk) pk = v["target_kbps"]+0
    # A cut, and how soon after the last one.
    if (v["request_kbps"] != "null" && v["request_kbps"]+0 < v["target_kbps"]+0) {
      cuts++
      if (last_cut_ms && v["t_ms"] - last_cut_ms < 3000) cascade++
      last_cut_ms = v["t_ms"]; drain = 4; next
    }
    # The guard: while it lasts, a delay falling faster than 5 000 µs.
    if (drain > 0) {
      if (v["delay_rise_us"] != "null" && v["delay_rise_us"]+0 < -5000) { guard++; drain-- }
      else drain = 0
    } }
  END { printf "peak=%-7d mean=%-7d", pk, s/n
        if (link > 0) printf " (%.0f%% of link)", (s/n)*100/link
        printf " | cuts=%d cascades=%d drain_guard=%d\n", cuts, cascade, guard }'

echo "  cap + cut lines:"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/$TAG-client.log" 2>/dev/null \
  | grep -E "link cap|link ceiling|cut to what the link|re-target" \
  | sed -E 's/^([0-9-]+T[0-9:]+)\.[0-9]+Z +INFO +[a-z_:]+: /    \1 /' | head -40
