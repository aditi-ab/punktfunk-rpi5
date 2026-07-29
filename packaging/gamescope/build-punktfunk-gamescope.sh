#!/usr/bin/env bash
# Build gamescope + punktfunk's `pipewire-hdr` patches and install it as `punktfunk-gamescope`.
#
# The ONE build recipe every packaging path calls (Arch PKGBUILD, the Bazzite sysext image, an
# operator building by hand); the nix derivation expresses the same thing declaratively. It never
# touches the distro's own `gamescope` — the binary lands under a different name, and the host's
# resolution order (PUNKTFUNK_GAMESCOPE_BIN > punktfunk-gamescope > gamescope) picks it up.
#
# Usage:
#   bash build-punktfunk-gamescope.sh [--rev <git-rev>] [--prefix /usr] [--destdir DIR]
#                                     [--srcdir DIR] [--jobs N] [--no-setcap]
#
#   --rev       gamescope commit/tag to build (default: the pin below)
#   --prefix    install prefix (default /usr)
#   --destdir   staging root for packaging (default: install straight into --prefix)
#   --srcdir    reuse an existing gamescope checkout instead of cloning
#   --no-setcap skip CAP_SYS_NICE (always skipped when --destdir is set — a package sets it)
#
# Needs: git, meson >= 0.58, ninja, a C++20 compiler, and gamescope's own build deps. Those vary
# by distro; the packaging files list them (packaging/gamescope/PKGBUILD for Arch). gamescope
# vendors wlroots / libliftoff / vkroots / libdisplay-info / SPIRV-Headers / reshade as git
# SUBMODULES (plus two meson wraps for glm + stb), so both the clone and the build want network
# access unless the checkout already carries them — hence `--recurse-submodules` below.
set -euo pipefail

# The pinned upstream. Bump together with the patches (they are `git am`-able and rebase cheaply —
# two files, mirroring code that already exists in-tree; see README.md).
GAMESCOPE_REV="8c676c399c761e4540587f61004c957993d12fea"
GAMESCOPE_REPO="https://github.com/ValveSoftware/gamescope.git"

REV="$GAMESCOPE_REV" PREFIX=/usr DESTDIR="" SRCDIR="" JOBS="" SETCAP=1
while [ $# -gt 0 ]; do
  case "$1" in
    --rev)       REV="${2:?}"; shift 2 ;;
    --prefix)    PREFIX="${2:?}"; shift 2 ;;
    --destdir)   DESTDIR="${2:?}"; shift 2 ;;
    --srcdir)    SRCDIR="${2:?}"; shift 2 ;;
    --jobs)      JOBS="${2:?}"; shift 2 ;;
    --no-setcap) SETCAP=0; shift ;;
    *) echo "unknown argument: $1" >&2; exit 1 ;;
  esac
done
# A staged install is a package build: the package manager owns file capabilities.
[ -n "$DESTDIR" ] && SETCAP=0

for tool in git meson ninja; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

HERE="$(cd "$(dirname "$0")" && pwd)"
PATCHES=("$HERE"/patches/*.patch)
[ -e "${PATCHES[0]}" ] || { echo "no patches found in $HERE/patches" >&2; exit 1; }

WORK=""
if [ -z "$SRCDIR" ]; then
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  SRCDIR="$WORK/gamescope"
  echo "==> cloning gamescope @ ${REV:0:12}"
  git clone --recurse-submodules "$GAMESCOPE_REPO" "$SRCDIR"
  git -C "$SRCDIR" checkout --recurse-submodules "$REV"
fi

# `git am` needs an identity and a clean tree; both are ours to provide in a throwaway checkout.
# Idempotent: a checkout that already carries the marker is left alone, so a re-run of this script
# against --srcdir does not fail on an already-applied patch.
if grep -q '+pfhdr' "$SRCDIR/src/meson.build"; then
  echo "==> patches already applied in $SRCDIR"
else
  echo "==> applying punktfunk patches"
  git -C "$SRCDIR" \
    -c user.name=punktfunk -c user.email=packages@unom.io \
    am "${PATCHES[@]}"
fi

BUILD="$SRCDIR/build-punktfunk"
echo "==> configuring"
# We ship ONE binary out of this tree, so build only what leads to it:
#   -Denable_tests=false        gamescope's own unit tests want Catch2 **v3**
#                               (`catch2-with-main`); Fedora ships v2, and building someone else's
#                               test suite is not our job either way.
#   -Denable_openvr_support     the VR integration pulls the openvr submodule + its build for a
#                               code path a headless capture session never enters.
#   -Denable_gamescope_wsi_layer the WSI layer is a SEPARATE artifact the distro's gamescope
#                               package already installs; ours must not collide with it.
#                               (The layer the nested games load is that one — it is version-
#                               independent of the compositor binary.)
#
# `force_fallback_for` includes **wlroots** on purpose, and it is load-bearing for a binary we
# SHIP: gamescope vendors a wlroots submodule, but meson prefers a system one when the build host
# happens to have `wlroots-devel` — and then links it SHARED. The result starts fine on the build
# host and dies with `libwlroots-0.19.so: cannot open shared object file` anywhere else, which is
# every machine that installs our package. Fedora 43 has no wlroots-devel so it fell back and
# linked static by luck; Fedora 44's `dnf builddep gamescope` pulls one in, and the binary built
# there would not start on the host. Pinning the fallback makes the outcome the same everywhere.
# (gamescope's own meson.build hard-errors if libliftoff/vkroots are missing from this list, so
# all three go together.)
#
# The C++ runtime goes STATIC for the same reason wlroots does: this binary is built on a ROLLING
# distro and has to start on a FROZEN one. Arch's gcc (16.1.1 when this was written) makes the
# compositor require `GLIBCXX_3.4.35`, and SteamOS 3.8.16 ships libstdc++ 3.4.34 — so the published
# Arch package died on the very platform the gamescope backend matters most on ("version
# GLIBCXX_3.4.35 not found"), while every other soname resolved and glibc was never close to the
# limit (the binary asks for 2.38 at most; SteamOS has 2.41). Safe because
# gamescope links NO shared C++ library (its `NEEDED` list is all C — glslang/SPIRV are build-time
# only), so no C++ ABI ever crosses a shared boundary; it costs ~1 MB.
# Appended to LDFLAGS rather than passed as `-Dcpp_link_args`, because that option would REPLACE
# the value meson derives from the environment and silently drop makepkg's hardening flags
# (`-z relro`, `-z now`, `--as-needed`).
export LDFLAGS="${LDFLAGS:-} -static-libstdc++ -static-libgcc"
meson setup "$BUILD" "$SRCDIR" \
  --prefix="$PREFIX" \
  --buildtype=release \
  -Dforce_fallback_for=libliftoff,vkroots,wlroots \
  -Dpipewire=enabled \
  -Denable_tests=false \
  -Denable_openvr_support=false \
  -Denable_gamescope_wsi_layer=false

echo "==> building"
ninja -C "$BUILD" ${JOBS:+-j "$JOBS"}

# Install ONLY the compositor, under our own name. gamescope's `ninja install` would also drop
# gamescopectl/gamescopereaper/gamescopestream + the WSI layer into the prefix, colliding with the
# distro's gamescope package — and we need none of them: the host only ever execs the compositor.
BIN="$BUILD/src/gamescope"
[ -x "$BIN" ] || { echo "build produced no $BIN" >&2; exit 1; }
# The static C++ runtime above is invisible in a successful build and only shows up as a binary
# that will not start on an older distro — so assert it here, where a mistake is a build failure
# instead of a package that dies at `--version` on SteamOS. No `libstdc++.so.6` in NEEDED is the
# whole invariant (and it needs no version threshold to check).
if command -v objdump >/dev/null; then
  objdump -p "$BIN" 2>/dev/null | grep -q 'NEEDED.*libstdc++' && {
    echo "built binary links libstdc++ dynamically — the static C++ runtime did not take, and this" >&2
    echo "package would not start on a distro older than the build host" >&2
    exit 1
  }
fi
DEST="${DESTDIR}${PREFIX}/bin/punktfunk-gamescope"
echo "==> installing $DEST"
install -Dm755 "$BIN" "$DEST"

if [ "$SETCAP" = 1 ] && command -v setcap >/dev/null; then
  # gamescope raises its own scheduling priority; without CAP_SYS_NICE it still runs, just noisier
  # and with worse frame pacing. Best-effort — needs root, and a package sets it declaratively.
  setcap 'CAP_SYS_NICE=eip' "$DEST" 2>/dev/null \
    || echo "note: could not setcap CAP_SYS_NICE on $DEST (run as root, or let the package do it)"
fi

echo "==> done: $("$DEST" --version 2>&1 | head -1)"
echo "    the banner above must contain +pfhdr — that marker is how the host detects HDR support"
