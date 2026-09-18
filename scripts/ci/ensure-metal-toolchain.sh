#!/bin/sh
# Ensure Xcode's Metal toolchain is installed on the macOS runner.
#
# Xcode 26 and later ship the Metal compiler as a separate component, and a new Xcode arrives
# without it. `swift build` and `xcodebuild` then fail on any .metal file (Glur's shaders) with
# only "CompileMetalFile … failed" and no diagnostic.
#
# Usage: sh scripts/ci/ensure-metal-toolchain.sh — uses XCODE_DEV_DIR when the job selected one.
set -eu
if [ -n "${XCODE_DEV_DIR:-}" ]; then
    DEVELOPER_DIR=$XCODE_DEV_DIR
    export DEVELOPER_DIR
fi
if xcodebuild -showComponent MetalToolchain 2>/dev/null | grep -q '^Status: installed'; then
    exit 0
fi
echo "installing the Metal toolchain for $(xcodebuild -version | head -1)"
xcodebuild -downloadComponent MetalToolchain
