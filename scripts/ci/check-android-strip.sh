#!/bin/sh
# Asserts every libpunktfunk_android.so in an APK or AAB carries a GNU build-id and is
# `stripped` (release) or keeps its `symbols` (debug). AGP only warns when it cannot find the
# NDK's llvm-strip; the build passes.
# Usage: check-android-strip.sh <apk-or-aab> stripped|symbols   (needs $ANDROID_HOME)
set -eu

PKG=$1
WANT=$2
READELF=$(find "${ANDROID_HOME:?}/ndk" -path '*/bin/llvm-readelf' | head -n 1)
[ -n "$READELF" ] || { echo "check-android-strip: no llvm-readelf under $ANDROID_HOME/ndk" >&2; exit 1; }

DIR=$(mktemp -d)
trap 'rm -rf "$DIR"' EXIT
unzip -q "$PKG" '*/libpunktfunk_android.so' -d "$DIR" || true # none found fails below

n=0
bad=0
for so in $(find "$DIR" -name libpunktfunk_android.so | sort); do
    n=$((n + 1))
    if "$READELF" -S -W "$so" | grep -q ' \.symtab '; then got=symbols; else got=stripped; fi
    id=$("$READELF" -n "$so" | awk '/Build ID:/ {print $3}')
    echo "${so#"$DIR"/}: $got, build-id ${id:-none} ($(($(wc -c < "$so"))) bytes)"
    [ "$got" = "$WANT" ] && [ -n "$id" ] || bad=1
done
[ "$n" -gt 0 ] || { echo "check-android-strip: no libpunktfunk_android.so in $PKG" >&2; exit 1; }
[ "$bad" -eq 0 ] || { echo "check-android-strip: $PKG: every library must be $WANT with a build-id" >&2; exit 1; }
