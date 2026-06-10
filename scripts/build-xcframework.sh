#!/usr/bin/env bash
# Build LumenCore.xcframework for the Apple clients — run ON A MAC with Xcode + rustup.
#
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin   # + aarch64-apple-ios for iOS
#   bash scripts/build-xcframework.sh
#
# Output: clients/apple/LumenCore.xcframework (consumed by clients/apple/Package.swift).
# The library is built WITH the `quic` feature (the lumen/1 connection API), so the bundled
# header gets LUMEN_FEATURE_QUIC pre-defined — Swift sees lumen_connect & co. unconditionally.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGETS_MAC=(aarch64-apple-darwin x86_64-apple-darwin)
BUILD_IOS="${BUILD_IOS:-0}" # BUILD_IOS=1 adds an iOS slice (requires the ios target installed)

# Deployment targets must match Package.swift's platforms, or every consumer link emits
# "object file was built for newer macOS version" warnings.
for t in "${TARGETS_MAC[@]}"; do
    MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p lumen-core --features quic --target "$t"
done
if [[ "$BUILD_IOS" == "1" ]]; then
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p lumen-core --features quic --target aarch64-apple-ios
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Universal macOS static lib.
mkdir -p "$STAGE/macos"
lipo -create \
    target/aarch64-apple-darwin/release/liblumen_core.a \
    target/x86_64-apple-darwin/release/liblumen_core.a \
    -output "$STAGE/macos/liblumen_core.a"

# Headers dir: the generated C header (with the quic API force-enabled) + a modulemap so
# Swift can `import LumenCore`.
mkdir -p "$STAGE/include"
{
    echo "#define LUMEN_FEATURE_QUIC 1"
    cat include/lumen_core.h
} > "$STAGE/include/lumen_core.h"
cat > "$STAGE/include/module.modulemap" <<'EOF'
module LumenCore {
    header "lumen_core.h"
    export *
}
EOF

ARGS=(-library "$STAGE/macos/liblumen_core.a" -headers "$STAGE/include")
if [[ "$BUILD_IOS" == "1" ]]; then
    ARGS+=(-library target/aarch64-apple-ios/release/liblumen_core.a -headers "$STAGE/include")
fi

# Cargo does NOT fingerprint MACOSX_DEPLOYMENT_TARGET — units cached from a build without
# it keep their old minos forever. Refuse to ship anything newer than the package floor
# (objects BELOW it, e.g. rustup's precompiled std at 11.0, are fine and unavoidable).
for obj in "$STAGE"/macos/liblumen_core.a; do
    bad=$(otool -l "$obj" 2>/dev/null | awk '/minos/ {print $2}' | sort -uV | awk -F. '$1 > 14' | head -1)
    if [[ -n "$bad" ]]; then
        echo "ERROR: $obj contains objects built for macOS $bad (> 14.0)." >&2
        echo "Stale cache — rm -rf target/{aarch64,x86_64}-apple-darwin and rebuild." >&2
        exit 1
    fi
done

rm -rf clients/apple/LumenCore.xcframework
xcodebuild -create-xcframework "${ARGS[@]}" -output clients/apple/LumenCore.xcframework
echo "OK: clients/apple/LumenCore.xcframework"
