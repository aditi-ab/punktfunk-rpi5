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

for t in "${TARGETS_MAC[@]}"; do
    cargo build --release -p lumen-core --features quic --target "$t"
done
if [[ "$BUILD_IOS" == "1" ]]; then
    cargo build --release -p lumen-core --features quic --target aarch64-apple-ios
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

rm -rf clients/apple/LumenCore.xcframework
xcodebuild -create-xcframework "${ARGS[@]}" -output clients/apple/LumenCore.xcframework
echo "OK: clients/apple/LumenCore.xcframework"
