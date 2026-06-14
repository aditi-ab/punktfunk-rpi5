#!/usr/bin/env bash
# Build the punktfunk-client .deb (the native GTK4 client) for Ubuntu/Debian desktops.
#
# Counterpart to build-deb.sh (the host package); same conventions: runtime Depends are
# computed by dpkg-shlibdeps from the binary's DT_NEEDED (GTK4/libadwaita, SDL3, the
# FFmpeg/PipeWire/Opus sonames), so build inside the Ubuntu 26.04 rust-ci image to pin
# the package names the target boxes ship. The client links no NVIDIA libs — no filter
# needed.
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [ARCH=amd64] bash packaging/debian/build-client-deb.sh
# Output: dist/punktfunk-client_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
ARCH="${ARCH:-amd64}"
PKG="punktfunk-client"
CRATE="punktfunk-client-linux"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

BIN="target/release/$PKG"
if [ ! -x "$BIN" ]; then
  echo "==> building $CRATE (release)"
  cargo build --release -p "$CRATE" --locked
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"

# --- file layout --------------------------------------------------------------
install -Dm0755 "$BIN"                                   "$STAGE/usr/bin/$PKG"
install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
                "$STAGE/usr/share/applications/io.unom.Punktfunk.desktop"
# DualSense hidraw access (full pad fidelity through SDL's HIDAPI driver).
install -Dm0644 scripts/70-punktfunk-client.rules \
                "$STAGE/usr/lib/udev/rules.d/70-punktfunk-client.rules"
# UDP receive-buffer tuning (32 MB) — without it the kernel clamps the client's SO_RCVBUF and
# high-bitrate streams overflow it at the receiver (measured: 4 MB cap = 31.6% loss at 2 Gbps,
# 32 MB = 0%). systemd-sysctl applies it at boot; the postinst applies it on install.
install -Dm0644 scripts/99-punktfunk-client-net.conf \
                "$STAGE/usr/lib/sysctl.d/99-punktfunk-client-net.conf"
install -Dm0644 LICENSE-MIT                              "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                           "$DOCDIR/LICENSE-APACHE"
install -Dm0644 README.md                                "$DOCDIR/README.md"

cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: punktfunk
Source: https://git.unom.io/unom/punktfunk

Files: *
Copyright: punktfunk contributors
License: MIT or Apache-2.0
 Dual-licensed. Full texts in /usr/share/doc/$PKG/LICENSE-MIT and
 /usr/share/doc/$PKG/LICENSE-APACHE.
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <noreply@anthropic.com>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

# --- dependencies --------------------------------------------------------------
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
cat > "$SHLIB_TMP/debian/control" <<EOF
Source: $PKG

Package: $PKG
Architecture: any
Depends: \${shlibs:Depends}
EOF
SHDEPS="$(cd "$SHLIB_TMP" && dpkg-shlibdeps -O --ignore-missing-info "$ROOTDIR/$BIN" 2>/dev/null \
          | sed -n 's/^shlibs:Depends=//p')"
rm -rf "$SHLIB_TMP"
[ -n "$SHDEPS" ] || { echo "dpkg-shlibdeps produced no deps — is dpkg-dev installed?" >&2; exit 1; }

# Manual additions shlibdeps can't see: the PipeWire daemon + session manager are runtime
# services (audio playback / mic capture degrade gracefully without them — Recommends).
RECOMMENDS="pipewire, wireplumber, pipewire-pulse"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: unom <noreply@anthropic.com>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Homepage: https://git.unom.io/unom/punktfunk
Depends: $SHDEPS
Recommends: $RECOMMENDS
Description: Low-latency desktop/game streaming client (punktfunk/1, GTK4)
 The native Linux client for punktfunk, a Linux-first low-latency desktop and
 game streaming stack. Discovers hosts on the LAN (mDNS), trusts them via
 certificate pinning with a SPAKE2 PIN pairing ceremony, and streams HEVC video
 (GF(2^16) Leopard FEC + AES-GCM over UDP, QUIC control plane) with Opus audio,
 microphone passthrough, and full gamepad support including DualSense touchpad,
 motion, adaptive triggers and lightbar through SDL3.
 .
 The host creates a virtual output at exactly this client's resolution and
 refresh rate — no scaling. See the punktfunk-host package for the host side.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # Pick up the DualSense hidraw rule without a reboot (best-effort, no-op in containers).
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=hidraw 2>/dev/null || true
    update-desktop-database /usr/share/applications 2>/dev/null || true
    # Apply the UDP recv-buffer tuning now (also auto-applied at boot by systemd-sysctl).
    sysctl -p /usr/lib/sysctl.d/99-punktfunk-client-net.conf >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
echo "  Depends: $SHDEPS"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size' || true
