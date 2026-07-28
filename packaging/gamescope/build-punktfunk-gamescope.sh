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
meson setup "$BUILD" "$SRCDIR" \
  --prefix="$PREFIX" \
  --buildtype=release \
  -Dpipewire=enabled

echo "==> building"
ninja -C "$BUILD" ${JOBS:+-j "$JOBS"}

# Install ONLY the compositor, under our own name. gamescope's `ninja install` would also drop
# gamescopectl/gamescopereaper/gamescopestream + the WSI layer into the prefix, colliding with the
# distro's gamescope package — and we need none of them: the host only ever execs the compositor.
BIN="$BUILD/src/gamescope"
[ -x "$BIN" ] || { echo "build produced no $BIN" >&2; exit 1; }
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
