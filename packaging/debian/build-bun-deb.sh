#!/usr/bin/env bash
# Build the punktfunk-bun .deb — the one bun runtime punktfunk-web and punktfunk-scripting run on.
#
# Bun isn't in apt, so we vendor a pinned build. It lives at /usr/lib/punktfunk-bun/bun, never on
# PATH, so it never collides with a system-wide bun. Both consumers depend on this exact version.
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [DEB_ARCH=amd64] [BUN_BIN=/path/to/bun] bash packaging/debian/build-bun-deb.sh
# Output: dist/punktfunk-bun_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
PKG="punktfunk-bun"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Map deb arch → bun's release arch tag.
DEB_ARCH="${DEB_ARCH:-$(dpkg --print-architecture)}"
BUN_VERSION="${BUN_VERSION:-1.3.14}"
case "$DEB_ARCH" in
  amd64) BUN_ARCH=x64 ;;
  arm64) BUN_ARCH=aarch64 ;;
  *) echo "ERROR: unsupported DEB_ARCH=$DEB_ARCH (want amd64 or arm64)" >&2; exit 1 ;;
esac

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"
LIBDIR="$STAGE/usr/lib/$PKG"

# Honor a pre-fetched bun (CI passes its pinned one) via BUN_BIN; else download the pinned release.
mkdir -p "$LIBDIR"
if [ -n "${BUN_BIN:-}" ]; then
  echo "==> vendoring bun from BUN_BIN=$BUN_BIN"
  install -m0755 "$BUN_BIN" "$LIBDIR/bun"
else
  url="https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/bun-linux-${BUN_ARCH}.zip"
  echo "==> downloading bun $BUN_VERSION ($BUN_ARCH) from $url"
  tmp="$(mktemp -d)"
  curl -fsSL "$url" -o "$tmp/bun.zip"
  unzip -q "$tmp/bun.zip" -d "$tmp"
  install -m0755 "$tmp/bun-linux-${BUN_ARCH}/bun" "$LIBDIR/bun"
  rm -rf "$tmp"
fi
"$LIBDIR/bun" --version

mkdir -p "$DOCDIR"
cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: bun
Source: https://github.com/oven-sh/bun

Files: *
Copyright: Oven and the bun contributors
License: MIT
 Bun is MIT-licensed. The components it links are listed in
 https://github.com/oven-sh/bun/blob/main/LICENSE.md
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <packages@unom.io>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: unom <packages@unom.io>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Homepage: https://git.unom.io/unom/punktfunk
Description: bun runtime for the punktfunk web console and plugin runner
 The pinned bun build punktfunk-web and punktfunk-scripting run on, installed once at
 /usr/lib/punktfunk-bun/bun. It is not on PATH and does not replace a system-wide bun.
EOF

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size|Depends' || true
