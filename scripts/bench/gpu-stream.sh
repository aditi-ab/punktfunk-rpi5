#!/usr/bin/env bash
# Tier-3 GPU stream benchmark — the REAL pipeline: virtual output → zero-copy dmabuf→CUDA → NVENC →
# punktfunk/1 over loopback UDP → FEC/decrypt/reassemble, with the client measuring end-to-end
# latency. This is the "real-world" regression test the GPU-less CI can't run; it runs on a
# self-hosted GPU runner (a dev box with an NVIDIA GPU + a KWin session). Report-only by default.
#
#   scripts/bench/gpu-stream.sh [WxHxHz] [seconds]      # measure + compare to the baseline
#   scripts/bench/gpu-stream.sh 1920x1080x120 12 --update   # (re)write scripts/bench/gpu-baseline.json
#
# Metrics (host PUNKTFUNK_PERF + client report): encode_us_p50/p99, tx_mbps, send_dropped, and the
# client's capture→received lat_p50/p95/p99_us. Lower is better for latency/encode/drops, higher
# for throughput. Regressions are flagged ⚠ but the script exits 0 (gate decisions stay human).
set -uo pipefail

MODE="${1:-1920x1080x120}"
SECS="${2:-12}"
UPDATE=""
[[ "${3:-}" == "--update" || "${2:-}" == "--update" ]] && UPDATE=1
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
BASELINE="scripts/bench/gpu-baseline.json"

# Compositor session: reuse one if present, else bring up a headless KWin (dev-box KDE pattern).
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-kde}"
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-KDE}"
export PUNKTFUNK_COMPOSITOR="${PUNKTFUNK_COMPOSITOR:-kwin}"
export PUNKTFUNK_VIDEO_SOURCE=virtual PUNKTFUNK_ZEROCOPY=1 PUNKTFUNK_PERF=1
OWN_KWIN=""
if [[ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then
  echo "==> no $WAYLAND_DISPLAY — bringing up a headless KWin session"
  setsid bash scripts/headless/run-headless-kde.sh "${MODE%x*}" </dev/null >/tmp/bench-kwin.log 2>&1 &
  OWN_KWIN=$!
  for _ in $(seq 1 30); do [[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] && break; sleep 1; done
fi

echo "==> building host + client (release)"
cargo build -rq -p punktfunk-host -p punktfunk-probe

HOST_LOG="$(mktemp)"; CLI_LOG="$(mktemp)"
trap 'kill "$HOST_PID" 2>/dev/null; [[ -n "$OWN_KWIN" ]] && pkill -f "kwin_wayland --virtual" 2>/dev/null; rm -f "$HOST_LOG" "$CLI_LOG"' EXIT

echo "==> host: punktfunk1-host --source virtual ($MODE, ${SECS}s)"
target/release/punktfunk-host punktfunk1-host --source virtual --seconds "$SECS" --max-sessions 1 \
  >"$HOST_LOG" 2>&1 &
HOST_PID=$!
sleep 3
echo "==> client: streaming + measuring latency"
target/release/punktfunk-probe --connect 127.0.0.1:9777 --mode "$MODE" --out /dev/null \
  >"$CLI_LOG" 2>&1 || true
wait "$HOST_PID" 2>/dev/null || true

# --- extract metrics ---------------------------------------------------------
field() { grep -oE "$1=\"?[0-9]+" "$2" | tail -1 | grep -oE "[0-9]+$"; }
ENC_P50=$(field "encode_us_p50" "$HOST_LOG"); ENC_P99=$(field "encode_us_p99" "$HOST_LOG")
TX_MBPS=$(field "tx_mbps" "$HOST_LOG");       DROPPED=$(field "send_dropped_total" "$HOST_LOG")
LAT_P50=$(field "lat_p50_us" "$CLI_LOG");     LAT_P95=$(field "lat_p95_us" "$CLI_LOG")
LAT_P99=$(field "lat_p99_us" "$CLI_LOG")
if [[ -z "$LAT_P50" || -z "$ENC_P50" ]]; then
  echo "!! incomplete metrics (host/client did not stream). host log tail:"; tail -8 "$HOST_LOG"
  exit 0
fi

python3 - "$BASELINE" "${UPDATE:-}" <<PY
import json, os, sys
baseline_path, update = sys.argv[1], sys.argv[2]
# (metric, value, lower_is_better)
cur = {
  "encode_us_p50": ($ENC_P50, True), "encode_us_p99": ($ENC_P99, True),
  "lat_us_p50": ($LAT_P50, True), "lat_us_p95": ($LAT_P95, True), "lat_us_p99": ($LAT_P99, True),
  "tx_mbps": (${TX_MBPS:-0}, False), "send_dropped_total": (${DROPPED:-0}, True),
}
vals = {k: v for k, (v, _) in cur.items()}
if update:
    json.dump(vals, open(baseline_path, "w"), indent=2); open(baseline_path,"a").write("\n")
    print("wrote GPU baseline ->", baseline_path); sys.exit(0)
base = json.load(open(baseline_path)) if os.path.exists(baseline_path) else {}
THRESH = 0.20  # 20% on a dedicated runner
rows = ["## Tier-3 GPU stream benchmark ($MODE)", "", "| metric | baseline | current | Δ |", "|---|---:|---:|---:|"]
regr = []
for k, (v, lower) in cur.items():
    b = base.get(k)
    if b is None: rows.append(f"| {k} | — | {v} | _new_ |"); continue
    d = (v - b) / b if b else 0.0
    worse = (d > THRESH) if lower else (d < -THRESH)
    flag = " ⚠" if worse else ""
    rows.append(f"| {k} | {b} | {v} | {d:+.1%}{flag} |")
    if worse: regr.append(k)
out = "\n".join(rows)
print(out)
s = os.environ.get("GITHUB_STEP_SUMMARY")
if s: open(s, "a").write(out + "\n")
if regr: print("\n⚠ regressed:", ", ".join(regr))
PY
