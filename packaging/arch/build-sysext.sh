#!/usr/bin/env bash
# Wrap a built punktfunk pacman package into a systemd-sysext image — the update-survivable way to
# add it to an immutable Arch-derived distro (SteamOS 3): the .raw overlays /usr read-only from the
# writable /var/lib/extensions/, so it persists across A/B OS updates with no `steamos-readonly
# disable`. Works for either split package — on a Steam Deck you'd wrap the CLIENT. Needs
# `bsdtar`/`tar`, `squashfs-tools` (mksquashfs).
#
# Usage:  bash build-sysext.sh [--gamescope <punktfunk-gamescope-*.pkg.tar.zst>] \
#                              <punktfunk-{host,client}-*.pkg.tar.zst>
# Output: <pkgname>.raw   (e.g. punktfunk-client.raw)
#
# --gamescope folds the HDR-capable gamescope companion package (packaging/gamescope) into a HOST
# image as /usr/bin/punktfunk-gamescope — what lets the gamescope backend stream 10-bit BT.2020 PQ
# instead of 8-bit SDR (the host prefers that name on PATH and attempts HDR by default). Mirrors
# the Bazzite image's fold-in, including the honesty check: the binary is verified by executing
# its `+pfhdr` banner, never trusted by filename. Omit it and the image is exactly what it was —
# the host then stays SDR on that backend, by design. (No CAP_SYS_NICE inside the image: file
# capabilities don't survive this squashfs path — gamescope runs without it, pacing slightly
# worse, same as the Bazzite sysext.)
set -euo pipefail

GAMESCOPE=""
if [ "${1:-}" = "--gamescope" ]; then
  GAMESCOPE="${2:?--gamescope needs a punktfunk-gamescope package}"; shift 2
fi
# No braces in the message: a literal `}` inside ${1:?...} terminates the expansion early and
# corrupts $PKG (the tail of the message gets appended to the value — a real field bug).
PKG="${1:?usage: build-sysext.sh [--gamescope <pkg>] <punktfunk-host|client pkg.tar.zst>}"
[ -f "$PKG" ] || { echo "no such package: $PKG" >&2; exit 1; }
# Derive the package name from the file (pkgname is everything before the -<version>).
NAME="$(basename "$PKG" | sed -E 's/-[0-9].*//')"
[ -n "$NAME" ] || { echo "could not derive package name from $PKG" >&2; exit 1; }
if [ -n "$GAMESCOPE" ] && [ "$NAME" != "punktfunk-host" ]; then
  echo "--gamescope only makes sense for a punktfunk-host image (got: $NAME)" >&2; exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# A pacman package is a (zstd) tarball; a sysext only carries /usr (the host /etc, /var are the
# system's). Extract just usr/ from the payload.
if command -v bsdtar >/dev/null 2>&1; then
  bsdtar -C "$STAGE" -xf "$PKG" usr
else
  tar -C "$STAGE" -xf "$PKG" usr
fi

# The HDR gamescope companion (see --gamescope in the header). Verified by its banner marker
# rather than trusted by filename: an unpatched gamescope shipped under this name would make the
# host promise HDR it cannot deliver, and the punktfunk/1 Welcome cannot take that back
# mid-session. Executing the staged binary needs a build box the binary runs on (the Arch CI
# container qualifies; it built it).
if [ -n "$GAMESCOPE" ]; then
  [ -f "$GAMESCOPE" ] || { echo "no such package: $GAMESCOPE" >&2; exit 1; }
  if command -v bsdtar >/dev/null 2>&1; then
    bsdtar -C "$STAGE" -xf "$GAMESCOPE" usr
  else
    tar -C "$STAGE" -xf "$GAMESCOPE" usr
  fi
  GS_BIN="$STAGE/usr/bin/punktfunk-gamescope"
  [ -x "$GS_BIN" ] || { echo "$GAMESCOPE did not provide usr/bin/punktfunk-gamescope" >&2; exit 1; }
  "$GS_BIN" --version 2>&1 | grep -q '+pfhdr' || {
    echo "$GAMESCOPE's binary has no +pfhdr marker — it is not a punktfunk HDR build" >&2; exit 1; }
  echo "folded in $("$GS_BIN" --version 2>&1 | head -1)"
fi

# The marker systemd-sysext requires to merge the image. ID=_any merges onto ANY host os-release
# (SteamOS, Arch, Bazzite); ARCHITECTURE pins it to x86-64 so it's never merged on the wrong arch.
install -d "$STAGE/usr/lib/extension-release.d"
cat > "$STAGE/usr/lib/extension-release.d/extension-release.$NAME" <<EOF
ID=_any
ARCHITECTURE=x86-64
EOF

OUT="$NAME.raw"
rm -f "$OUT"
mksquashfs "$STAGE" "$OUT" -all-root -noappend -quiet
echo "built $OUT"
echo "  install:  sudo cp $OUT /var/lib/extensions/ && sudo systemctl enable --now systemd-sysext"
if [ "$NAME" = "punktfunk-host" ]; then
  echo "  then:     systemctl --user enable --now punktfunk-host"
else
  echo "  then:     run 'punktfunk-client' (or let the Decky plugin launch it)"
fi
