#!/usr/bin/env bash
# Loopback integration: real punktfunk/1 hosts (synthetic source — pure protocol, runs fine on
# macOS) on 127.0.0.1, then the Swift integration tests against them through the xcframework.
# Two hosts: an open one (stream round trip) and one armed with --require-pairing (the PIN
# ceremony + pairing gate — its random PIN is parsed out of its log).
set -euo pipefail
cd "$(dirname "$0")/../.."

PORT="${PUNKTFUNK_LOOPBACK_PORT:-19778}"
PAIR_PORT="${PUNKTFUNK_PAIRING_PORT:-19779}"

cargo build --release -p punktfunk-host

# Each host gets a throwaway config home: the pairing host persists a trust store
# (punktfunk1-paired.json, resolved from $HOME) and both mint an identity cert on first
# run — none of that belongs in the user's real ~/.config/punktfunk, and separate homes
# also keep the two first runs from racing on the same cert.pem.
CFG="$(mktemp -d "${TMPDIR:-/tmp}/punktfunk-loopback.XXXXXX")"
PAIR_LOG="$CFG/pairing-host.log"
mkdir -p "$CFG/open" "$CFG/paired"
trap 'kill "${HOST_PID:-}" "${PAIR_PID:-}" 2>/dev/null || true' EXIT
HOME="$CFG/open" XDG_CONFIG_HOME="$CFG/open/.config" \
    target/release/punktfunk-host m3-host --port "$PORT" --source synthetic --frames 300 &
HOST_PID=$!
HOME="$CFG/paired" XDG_CONFIG_HOME="$CFG/paired/.config" \
    target/release/punktfunk-host m3-host --port "$PAIR_PORT" --source synthetic --frames 300 \
    --require-pairing >"$PAIR_LOG" 2>&1 &
PAIR_PID=$!
sleep 1

PIN=""
for _ in $(seq 50); do
    PIN="$(grep -oE 'pair: [0-9]+' "$PAIR_LOG" | head -1 | cut -d' ' -f2 || true)"
    [ -n "$PIN" ] && break
    sleep 0.2
done
if [ -z "$PIN" ]; then
    echo "no arming PIN in the pairing host's log ($PAIR_LOG)" >&2
    exit 1
fi

cd clients/apple
PUNKTFUNK_LOOPBACK_PORT="$PORT" PUNKTFUNK_PAIRING_PORT="$PAIR_PORT" PUNKTFUNK_PAIRING_PIN="$PIN" \
    swift test --filter LoopbackIntegrationTests
