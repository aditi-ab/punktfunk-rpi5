#!/usr/bin/env bash
# Build PunktfunkCore.xcframework for the Apple clients — run ON A MAC with Xcode + rustup.
#
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin   # + aarch64-apple-ios for iOS
#   bash scripts/build-xcframework.sh
#
# Output: clients/apple/PunktfunkCore.xcframework (consumed by clients/apple/Package.swift).
# The library is built WITH the `quic` feature (the punktfunk/1 connection API), so the bundled
# header gets PUNKTFUNK_FEATURE_QUIC pre-defined — Swift sees punktfunk_connect & co. unconditionally.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGETS_MAC=(aarch64-apple-darwin x86_64-apple-darwin)
BUILD_IOS="${BUILD_IOS:-0}" # BUILD_IOS=1 adds iOS device + simulator slices (rustup targets aarch64-apple-ios{,-sim})
BUILD_TVOS="${BUILD_TVOS:-0}" # BUILD_TVOS=1 adds tvOS slices — TIER-3 Rust targets: needs `rustup toolchain install nightly` + `rustup component add rust-src --toolchain nightly`

# Toolchain resolution. Cargo's HOST artifacts (proc-macros, build scripts) are loaded by
# the RUNNING OS, so their linker must not be newer than it: a beta Xcode's ld emits
# LINKEDIT layouts the current dyld rejects ("mis-aligned LINKEDIT string pool"), and
# every proc-macro then dies with a misleading E0463 "can't find crate" — with the bad
# artifacts cached (cargo doesn't fingerprint the linker; rm -rf target after fixing).
# CLT is always dyld-safe but ships no iOS/tvOS SDKs. Resolution: a NON-BETA full Xcode
# for everything; with only a beta installed, macOS slices build against CLT and
# iOS/tvOS slices are refused.
pick_nonbeta_xcode() {
    local app
    for app in /Applications/Xcode.app /Applications/Xcode*.app; do
        case "$app" in *[Bb]eta*) continue ;; esac
        [ -x "$app/Contents/Developer/usr/bin/xcodebuild" ] && { echo "$app/Contents/Developer"; return; }
    done
}
case "${DEVELOPER_DIR:-}" in *[Bb]eta*) unset DEVELOPER_DIR ;; esac # never let a beta in via env
if [[ -z "${DEVELOPER_DIR:-}" ]]; then
    DEFAULT_DIR="$(xcode-select -p 2>/dev/null || true)"
    case "$DEFAULT_DIR" in
    *[Bb]eta*|*CommandLineTools*|'')
        NONBETA="$(pick_nonbeta_xcode || true)"
        if [[ -n "$NONBETA" ]]; then
            export DEVELOPER_DIR="$NONBETA"
        elif [[ "$BUILD_IOS" == "1" || "$BUILD_TVOS" == "1" ]]; then
            echo "ERROR: iOS/tvOS slices need a full NON-BETA Xcode in /Applications" >&2
            echo "       (CLT has no iOS SDK; a beta's ld breaks host proc-macro dylibs)." >&2
            exit 1
        elif [[ "$DEFAULT_DIR" != *CommandLineTools* ]]; then
            echo "ERROR: xcode-select default is a beta (or missing) and no non-beta Xcode/CLT" >&2
            echo "       fallback exists — install CLT or a release Xcode." >&2
            exit 1
        fi
        # else: the default IS CLT — dyld-safe for the mac slices; deliberately leave the
        # env untouched (an EXPLICIT DEVELOPER_DIR=<CLT> export trips xcrun's Xcode
        # license check when a full Xcode is also installed).
        ;;
    esac # a non-beta xcode-select default is fine as-is
fi

# Deployment targets must match Package.swift's platforms, or every consumer link emits
# "object file was built for newer macOS version" warnings.
for t in "${TARGETS_MAC[@]}"; do
    MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p punktfunk-core --features quic --target "$t"
done
if [[ "$BUILD_IOS" == "1" ]]; then
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-core --features quic --target aarch64-apple-ios
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-core --features quic --target aarch64-apple-ios-sim
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-core --features quic --target x86_64-apple-ios
fi
if [[ "$BUILD_TVOS" == "1" ]]; then
    # Tier-3 targets: no prebuilt std — nightly + -Zbuild-std compiles it from rust-src.
    TVOS_DEPLOYMENT_TARGET=17.0 cargo +nightly build --release -p punktfunk-core --features quic \
        -Z build-std=std,panic_abort --target aarch64-apple-tvos
    TVOS_DEPLOYMENT_TARGET=17.0 cargo +nightly build --release -p punktfunk-core --features quic \
        -Z build-std=std,panic_abort --target aarch64-apple-tvos-sim
    TVOS_DEPLOYMENT_TARGET=17.0 cargo +nightly build --release -p punktfunk-core --features quic \
        -Z build-std=std,panic_abort --target x86_64-apple-tvos
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# Universal macOS static lib.
mkdir -p "$STAGE/macos"
lipo -create \
    target/aarch64-apple-darwin/release/libpunktfunk_core.a \
    target/x86_64-apple-darwin/release/libpunktfunk_core.a \
    -output "$STAGE/macos/libpunktfunk_core.a"

# Headers dir: the generated C header (with the quic API force-enabled) + a modulemap so
# Swift can `import PunktfunkCore`.
mkdir -p "$STAGE/include"
{
    echo "#define PUNKTFUNK_FEATURE_QUIC 1"
    cat include/punktfunk_core.h
} > "$STAGE/include/punktfunk_core.h"
cat > "$STAGE/include/module.modulemap" <<'EOF'
module PunktfunkCore {
    header "punktfunk_core.h"
    export *
}
EOF

ARGS=(-library "$STAGE/macos/libpunktfunk_core.a" -headers "$STAGE/include")
if [[ "$BUILD_IOS" == "1" ]]; then
    # Universal simulator lib (arm64 Macs run arm64 sims, but generic builds link x86_64 too).
    mkdir -p "$STAGE/iossim"
    lipo -create \
        target/aarch64-apple-ios-sim/release/libpunktfunk_core.a \
        target/x86_64-apple-ios/release/libpunktfunk_core.a \
        -output "$STAGE/iossim/libpunktfunk_core.a"
    ARGS+=(-library target/aarch64-apple-ios/release/libpunktfunk_core.a -headers "$STAGE/include")
    ARGS+=(-library "$STAGE/iossim/libpunktfunk_core.a" -headers "$STAGE/include")
fi
if [[ "$BUILD_TVOS" == "1" ]]; then
    mkdir -p "$STAGE/tvossim"
    lipo -create \
        target/aarch64-apple-tvos-sim/release/libpunktfunk_core.a \
        target/x86_64-apple-tvos/release/libpunktfunk_core.a \
        -output "$STAGE/tvossim/libpunktfunk_core.a"
    ARGS+=(-library target/aarch64-apple-tvos/release/libpunktfunk_core.a -headers "$STAGE/include")
    ARGS+=(-library "$STAGE/tvossim/libpunktfunk_core.a" -headers "$STAGE/include")
fi

# Cargo does NOT fingerprint MACOSX_DEPLOYMENT_TARGET — units cached from a build without
# it keep their old minos forever. Refuse to ship anything newer than the package floor
# (objects BELOW it, e.g. rustup's precompiled std at 11.0, are fine and unavoidable).
for obj in "$STAGE"/macos/libpunktfunk_core.a; do
    bad=$(otool -l "$obj" 2>/dev/null | awk '/minos/ {print $2}' | sort -uV | awk -F. '$1 > 14' | head -1)
    if [[ -n "$bad" ]]; then
        echo "ERROR: $obj contains objects built for macOS $bad (> 14.0)." >&2
        echo "Stale cache — rm -rf target/{aarch64,x86_64}-apple-darwin and rebuild." >&2
        exit 1
    fi
done

# -create-xcframework needs a full Xcode (CLT has no xcodebuild) but does NO linking —
# it only copies the libs and writes the bundle plist, so a beta Xcode is safe here.
XCODEBUILD=(xcodebuild)
if ! xcodebuild -version >/dev/null 2>&1; then
    for app in /Applications/Xcode.app /Applications/Xcode*.app; do
        if DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version >/dev/null 2>&1; then
            XCODEBUILD=(env DEVELOPER_DIR="$app/Contents/Developer" xcodebuild)
            echo "==> using $app for -create-xcframework"
            break
        fi
    done
fi

rm -rf clients/apple/PunktfunkCore.xcframework
"${XCODEBUILD[@]}" -create-xcframework "${ARGS[@]}" -output clients/apple/PunktfunkCore.xcframework
echo "OK: clients/apple/PunktfunkCore.xcframework"
