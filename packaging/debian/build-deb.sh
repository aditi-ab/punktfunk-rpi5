#!/usr/bin/env bash
# Build a punktfunk-host .deb for Ubuntu/Debian hosts.
#
# Mirrors the Fedora RPM (../rpm/punktfunk.spec): the host binary + the uinput udev rule
# + the systemd *user* unit + headless session helpers + example config + the OpenAPI doc.
#
# Runtime Depends are computed by `dpkg-shlibdeps` from the binary's actual DT_NEEDED, NOT
# hand-listed: the binary pulls a large transitive lib closure (most of it via ffmpeg) and
# the exact soname package names (libavcodec62, libpipewire-0.3-0t64, …) drift across distro
# releases — shlibdeps tracks them automatically and pins them to whatever the BUILD distro
# ships. Build this inside the Ubuntu 26.04 rust-ci image so those names match the target
# boxes exactly. `--ignore-missing-info` drops libcuda.so.1 (the NVIDIA driver lib, linked via
# FFI): on a GPU-less builder it resolves to no package, and we must never hard-depend on a
# specific libnvidia-compute-<ver> anyway — NVENC/EGL come from the driver, out of band.
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [ARCH=amd64] bash packaging/debian/build-deb.sh
# Output: dist/punktfunk-host_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
ARCH="${ARCH:-amd64}"
PKG="punktfunk-host"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

BIN="target/release/$PKG"
if [ ! -x "$BIN" ]; then
  echo "==> building $PKG (release)"
  PUNKTFUNK_BUILD_VERSION="$VERSION" cargo build --release -p "$PKG" --locked   # stamp --version (build.rs)
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"
SHAREDIR="$STAGE/usr/share/$PKG"

# --- file layout (matches the RPM %install) ----------------------------------
install -Dm0755 "$BIN"                              "$STAGE/usr/bin/$PKG"
install -Dm0644 scripts/60-punktfunk.rules         "$STAGE/usr/lib/udev/rules.d/60-punktfunk.rules"
# UDP socket-buffer tuning (32 MB) — without it the kernel clamps the host's SO_SNDBUF to ~416 KB
# and high-bitrate frames overflow it (send-side packet loss). systemd-sysctl applies it at boot.
install -Dm0644 scripts/99-punktfunk-net.conf      "$STAGE/usr/lib/sysctl.d/99-punktfunk-net.conf"
install -Dm0644 scripts/punktfunk-host.service     "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# The source unit's ExecStart points at the dev source tree; a packaged install has the binary at
# /usr/bin. Rewrite it so a fresh apt install (no hand-rolled unit) starts the installed binary.
sed -i 's#%h/punktfunk/target/release/punktfunk-host#/usr/bin/punktfunk-host#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# Optional headless KWin session unit (the kwin --virtual appliance), as the RPM/Arch ship.
# Repoint its ExecStart from the dev source tree to the packaged script. NOT enabled by default.
install -Dm0644 scripts/punktfunk-kde-session.service "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"
sed -i 's#%h/punktfunk/scripts/headless/run-headless-kde.sh#/usr/share/punktfunk-host/headless/run-headless-kde.sh#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"
install -Dm0755 scripts/headless/run-headless-kde.sh   "$SHAREDIR/headless/run-headless-kde.sh"
install -Dm0755 scripts/headless/run-headless-sway.sh  "$SHAREDIR/headless/run-headless-sway.sh"
install -Dm0644 scripts/headless/kde-authorized        "$SHAREDIR/headless/kde-authorized"
install -Dm0644 scripts/headless/punktfunk-sink.conf   "$SHAREDIR/headless/punktfunk-sink.conf"
install -Dm0644 scripts/host.env.example           "$SHAREDIR/host.env.example"
install -Dm0644 packaging/bazzite/host.env         "$SHAREDIR/host.env.bazzite"
install -Dm0644 packaging/kde/host.env             "$SHAREDIR/host.env.kde"
install -Dm0644 api/openapi.json              "$SHAREDIR/openapi.json"
install -Dm0644 LICENSE-MIT                         "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                      "$DOCDIR/LICENSE-APACHE"
install -Dm0644 README.md                           "$DOCDIR/README.md"

# Debian copyright + changelog (cheap, keeps the package well-formed).
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

# --- dependencies ------------------------------------------------------------
# Auto: the binary's directly-linked shared libs (libcuda ignored, see header).
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
cat > "$SHLIB_TMP/debian/control" <<EOF
Source: $PKG

Package: $PKG
Architecture: any
Depends: \${shlibs:Depends}
EOF
SHDEPS_RAW="$(cd "$SHLIB_TMP" && dpkg-shlibdeps -O --ignore-missing-info "$ROOTDIR/$BIN" 2>/dev/null \
          | sed -n 's/^shlibs:Depends=//p')"
rm -rf "$SHLIB_TMP"
[ -n "$SHDEPS_RAW" ] || { echo "dpkg-shlibdeps produced no deps — is dpkg-dev installed?" >&2; exit 1; }

# Drop the NVIDIA driver lib unconditionally. --ignore-missing-info already skips libcuda on a
# GPU-less builder (stub, no owning package), but on a box WITH the driver shlibdeps resolves
# libcuda.so.1 -> libnvidia-compute-<ver> and would pin that exact driver build. NVENC/EGL are
# provided by whatever driver the host runs, so this must never be a package dependency.
SHDEPS="$(printf '%s' "$SHDEPS_RAW" | tr ',' '\n' | sed 's/^ *//; s/ *$//' \
          | grep -ivE '^(libnvidia-compute|libcuda)' | awk 'NF' | paste -sd ',' - | sed 's/,/, /g')"
[ -n "$SHDEPS" ] || { echo "no deps left after filtering — unexpected" >&2; exit 1; }

# Manual additions shlibdeps can't see:
#  - libei1: input injection (libei) is loaded at runtime, not in DT_NEEDED.
#  - pipewire/wireplumber: runtime services (the daemon + session manager), not linked libs.
DEPENDS="$SHDEPS, libei1, pipewire, wireplumber"
# ffmpeg: Ubuntu's ffmpeg ships the NVENC-enabled libav* the binary links AND is the encoder
# runtime; the libav* sonames are already hard Depends via shlibdeps, so the ffmpeg metapackage
# is a Recommends. gamescope = a ready compositor backend; pipewire-pulse = desktop audio.
# mesa-va-drivers / intel-media-va-driver = the VAAPI encode drivers for AMD (radeonsi) and Intel
# (iHD) — pulled by default so the auto-selected VAAPI backend works out of the box; NVIDIA boxes
# don't need them (NVENC comes from the driver) and can --no-install-recommends.
# punktfunk-web = the management web console (pairing + status) every user needs — a separate
# Architecture:all .deb; Recommends so `apt install punktfunk-host` pulls it by default, while a
# headless/encoding-only box can opt out with --no-install-recommends.
RECOMMENDS="ffmpeg, gamescope, pipewire-pulse, mesa-va-drivers, intel-media-va-driver, punktfunk-web"
SUGGESTS="kwin-wayland, mutter"

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
Depends: $DEPENDS
Recommends: $RECOMMENDS
Suggests: $SUGGESTS
Description: Low-latency desktop/game streaming host (Moonlight + punktfunk/1)
 punktfunk is a Linux-first, low-latency desktop and game streaming host. It speaks
 the Moonlight/GameStream protocol (pair a stock Moonlight client) and its own native
 punktfunk/1 protocol (GF(2^16) Leopard FEC + AES-GCM, mid-stream mode renegotiation,
 client microphone passthrough). Each session gets a virtual output at the client's
 exact resolution and refresh via a per-compositor backend (KWin, gamescope, Mutter,
 Sway/wlroots), captured zero-copy (dmabuf -> CUDA -> NVENC). Input (mouse, keyboard,
 gamepads) is injected back into the session.
 .
 NVENC + GPU EGL come from the NVIDIA driver (libnvidia-encode / libEGL_nvidia),
 installed out of band. After install: add yourself to the 'input' group for virtual
 gamepads, then enable the systemd user service punktfunk-host.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # Pick up the /dev/uinput rule without a reboot (best-effort, no-op in containers).
    udevadm control --reload-rules 2>/dev/null || true
    udevadm trigger --subsystem-match=misc 2>/dev/null || true
    # Apply the UDP socket-buffer tuning now (also auto-applied at boot by systemd-sysctl).
    sysctl -p /usr/lib/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || true
    echo "punktfunk-host installed. Add yourself to the 'input' group for virtual gamepads:"
    echo "    sudo usermod -aG input \"\$USER\"   # then re-login"
    echo "Config:  mkdir -p ~/.config/punktfunk && cp /usr/share/punktfunk-host/host.env.example ~/.config/punktfunk/host.env"
    echo "Enable:  systemctl --user enable --now punktfunk-host"
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
echo "  Depends: $DEPENDS"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size' || true
