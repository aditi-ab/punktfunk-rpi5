#!/bin/sh
# Every `external fun` on NativeBridge has a `Java_io_unom_punktfunk_kit_NativeBridge_*` export in
# clients/android/native, and every export has an `external fun`. The JNI name is the whole link:
# a Kotlin method with no export loads fine and throws UnsatisfiedLinkError on the first call, and
# an export with no method is dead code that reads as live.
#
# Text match, no NDK: Rust names come from `fn Java_...` lines and from `json_pusher!` invocations,
# never from `//` comments. Wired into clients/android/kit/build.gradle.kts (`checkJniNames`).
#
# Usage: check-android-jni-names.sh
set -eu

cd "$(dirname "$0")/../.."

KOTLIN=clients/android/kit/src/main/kotlin/io/unom/punktfunk/kit/NativeBridge.kt
NATIVE=clients/android/native/src
PREFIX=Java_io_unom_punktfunk_kit_NativeBridge_

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

sed -n -E 's/^[[:space:]]*((private|internal|public) )?external fun ([A-Za-z0-9_]+).*/\3/p' "$KOTLIN" \
    | sort -u > "$TMP/kotlin"
grep -rh "^[^/]*$PREFIX" "$NATIVE" | grep -o "${PREFIX}[A-Za-z0-9_]*" | sed "s/^$PREFIX//" \
    | sort -u > "$TMP/rust"

# An empty side means a moved path or a changed shape, which would otherwise compare equal.
for side in kotlin rust; do
    [ -s "$TMP/$side" ] || { echo "check-android-jni-names: found no $side names" >&2; exit 1; }
done

no_export=$(comm -23 "$TMP/kotlin" "$TMP/rust")
no_method=$(comm -13 "$TMP/kotlin" "$TMP/rust")
if [ -n "$no_export" ] || [ -n "$no_method" ]; then
    if [ -n "$no_export" ]; then
        echo "check-android-jni-names: NativeBridge methods with no Rust export:" >&2
        echo "$no_export" | sed 's/^/    /' >&2
    fi
    if [ -n "$no_method" ]; then
        echo "check-android-jni-names: Rust exports with no NativeBridge method:" >&2
        echo "$no_method" | sed 's/^/    /' >&2
    fi
    exit 1
fi
echo "check-android-jni-names: $(wc -l < "$TMP/kotlin" | tr -d ' ') names match on both sides"
