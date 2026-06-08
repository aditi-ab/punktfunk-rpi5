#!/usr/bin/env bash
# Build lumen-core's staticlib, then compile + link + run the C ABI harness against it.
# Proves the core links from C. Works on Linux and macOS (link flags come from rustc).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ws="$(cd "$here/../../../.." && pwd)"   # tests/c -> crates/lumen-core -> crates -> ws
cd "$ws"

profile="${1:-debug}"
build_flag=""
[ "$profile" = "release" ] && build_flag="--release"

echo ">> building lumen-core staticlib ($profile)"
cargo build -p lumen-core $build_flag >/dev/null

staticlib="$ws/target/$profile/liblumen_core.a"
header_dir="$ws/include"
[ -f "$staticlib" ] || { echo "missing $staticlib"; exit 1; }
[ -f "$header_dir/lumen_core.h" ] || { echo "missing generated header"; exit 1; }

# Ask rustc what native libs the staticlib needs to link into a C program.
native_libs="$(cargo rustc -p lumen-core --lib --crate-type staticlib $build_flag -- \
    --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)"
echo ">> native libs: ${native_libs:-<none>}"

out="$(mktemp -d)/lumen_harness"
cc="${CC:-cc}"
echo ">> compiling + linking harness"
$cc -std=c11 -Wall -Wextra -O2 -I "$header_dir" \
    "$here/harness.c" "$staticlib" $native_libs -o "$out"

echo ">> running"
"$out"
