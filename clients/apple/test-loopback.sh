#!/usr/bin/env bash
# Loopback integration: a real lumen/1 host (synthetic source — pure protocol, runs fine on
# macOS) on 127.0.0.1, then the Swift integration tests against it through the xcframework.
# The m3 host serves exactly one session and exits; the trap is just for failure paths.
set -euo pipefail
cd "$(dirname "$0")/../.."

PORT="${LUMEN_LOOPBACK_PORT:-19778}"

cargo build --release -p lumen-host
target/release/lumen-host m3-host --port "$PORT" --source synthetic --frames 300 &
HOST_PID=$!
trap 'kill "$HOST_PID" 2>/dev/null || true' EXIT
sleep 1

cd clients/apple
LUMEN_LOOPBACK_PORT="$PORT" swift test --filter LoopbackIntegrationTests
