#!/usr/bin/env bash
# Wrap a built punktfunk pacman package into a systemd-sysext image — the update-survivable way to
# add it to an immutable Arch-derived distro (SteamOS 3): the .raw overlays /usr read-only from the
# writable /var/lib/extensions/, so it persists across A/B OS updates with no `steamos-readonly
# disable`. Works for either split package — on a Steam Deck you'd wrap the CLIENT. Needs
# `bsdtar`/`tar`, `squashfs-tools` (mksquashfs).
#
# Usage:  bash build-sysext.sh <punktfunk-{host,client}-*.pkg.tar.zst>
# Output: <pkgname>.raw   (e.g. punktfunk-client.raw)
set -euo pipefail

PKG="${1:?usage: build-sysext.sh <punktfunk-{host,client}-*.pkg.tar.zst>}"
[ -f "$PKG" ] || { echo "no such package: $PKG" >&2; exit 1; }
# Derive the package name from the file (pkgname is everything before the -<version>).
NAME="$(basename "$PKG" | sed -E 's/-[0-9].*//')"
[ -n "$NAME" ] || { echo "could not derive package name from $PKG" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# A pacman package is a (zstd) tarball; a sysext only carries /usr (the host /etc, /var are the
# system's). Extract just usr/ from the payload.
if command -v bsdtar >/dev/null 2>&1; then
  bsdtar -C "$STAGE" -xf "$PKG" usr
else
  tar -C "$STAGE" -xf "$PKG" usr
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
