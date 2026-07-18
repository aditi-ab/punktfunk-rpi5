#!/usr/bin/env bash
# Re-vendors PyroWave (+ the minimal Granite subset it builds against) into
# crates/pyrowave-sys/vendor/pyrowave. Network access required; run manually,
# never from CI or build.rs (the flatpak/CI builders are offline — that is the
# whole reason the tree is committed).
#
# ⚠️ Bumping PYROWAVE_COMMIT is a protocol-affecting change: the PyroWave
# bitstream has no version field, so the CODEC_PYROWAVE wire bit means
# "PyroWave bitstream as of this pin" (design/pyrowave-codec-plan.md §4.2).
# A bump that changes the bitstream must bump the punktfunk protocol version,
# and the Apple Metal hand-port (§4.7) must re-diff the two decode shaders +
# bitstream header structs.
set -euo pipefail

PYROWAVE_COMMIT=509e4f887b585a3f97471fcc804e9de649f2c16f
# The Granite pin + submodule set come from upstream's checkout_granite.sh at
# that commit; recorded here for the vendor manifest only.

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$REPO_ROOT/crates/pyrowave-sys/vendor/pyrowave"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

git clone https://github.com/Themaister/pyrowave "$WORK/pyrowave"
git -C "$WORK/pyrowave" checkout "$PYROWAVE_COMMIT"
(cd "$WORK/pyrowave" && bash checkout_granite.sh)

GRANITE_COMMIT="$(git -C "$WORK/pyrowave/Granite" rev-parse HEAD)"
VOLK_COMMIT="$(git -C "$WORK/pyrowave/Granite/third_party/volk" rev-parse HEAD)"
VKHDR_COMMIT="$(git -C "$WORK/pyrowave/Granite/third_party/khronos/vulkan-headers" rev-parse HEAD)"

cd "$WORK/pyrowave"
rm -rf .git Granite/.git
rm -f Granite/third_party/volk/.git Granite/third_party/khronos/vulkan-headers/.git
# Upstream's own .gitignore files ignore the Granite checkout (`/Granite`) —
# fatal for a committed vendor tree; strip them all.
find . -name .gitignore -delete

# Everything below is never entered by the standalone configure that
# crates/pyrowave-sys/CMakeLists.txt performs (GRANITE_SHIPPING=ON,
# GRANITE_RENDERER=OFF, GRANITE_PLATFORM=null, no PYROWAVE_DEVEL) — verified
# empirically: configure fails loudly if a needed dir goes missing.
# third_party/renderdoc stays: Granite adds it unconditionally.
rm -rf Granite/renderer Granite/ui Granite/scene-export Granite/video \
       Granite/audio Granite/physics Granite/tests Granite/tools \
       Granite/viewer Granite/assets Granite/slangmosh Granite/network \
       Granite/.github Granite/third_party/mikktspace
# vulkan-headers: the build needs the C headers + CMake package only.
rm -rf Granite/third_party/khronos/vulkan-headers/registry \
       Granite/third_party/khronos/vulkan-headers/tests
rm -f Granite/third_party/khronos/vulkan-headers/include/vulkan/*.hpp \
      Granite/third_party/khronos/vulkan-headers/include/vulkan/*.cppm

mkdir -p "$(dirname "$DEST")"
rm -rf "$DEST"
cp -a "$WORK/pyrowave" "$DEST"

# Local patches on top of the pin (crates/pyrowave-sys/patches/*.patch, applied
# in order). Each patch documents its upstream status; drop it when a vendor
# bump includes the fix.
for p in "$REPO_ROOT"/crates/pyrowave-sys/patches/*.patch; do
  [ -e "$p" ] || continue
  git -C "$REPO_ROOT" apply "$p"
  echo "applied $(basename "$p")"
done

cat > "$DEST/PUNKTFUNK-VENDOR.txt" <<EOF
Vendored by scripts/vendor-pyrowave.sh — do not edit by hand.

pyrowave:        $PYROWAVE_COMMIT
Granite:         $GRANITE_COMMIT
volk:            $VOLK_COMMIT
vulkan-headers:  $VKHDR_COMMIT

Tree is pruned to what the pyrowave-sys standalone build needs
(see the rm -rf list in the script). All parts are MIT-licensed
(pyrowave, Granite) or Apache-2.0/MIT (volk, Vulkan-Headers).

Local patches (crates/pyrowave-sys/patches/, re-applied on re-vendor):
  0001-payload-data-444-sizing.patch — encoder payload_data worst-case buffer
    was sized for 4:2:0's 1.5 samples/px; busy 4:4:4 (3 samples/px) overran it
    on the GPU → nondeterministic corrupt bitstreams/crashes at any bitrate.
    Found + validated 2026-07-18 (RTX 5070 Ti, 1080p/4K, 8/16-bit); to be
    reported upstream.
EOF

echo "Vendored pyrowave@${PYROWAVE_COMMIT:0:12} (Granite ${GRANITE_COMMIT:0:12}) into $DEST"
du -sh "$DEST"
