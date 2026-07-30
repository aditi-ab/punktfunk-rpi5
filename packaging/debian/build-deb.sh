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
# BUNDLE_FFMPEG=1 (Ubuntu 24.04 LTS builds, ci/rust-ci-noble.Dockerfile): instead of hard-depending
# on the distro's libav* — which don't exist on 24.04 (it ships FFmpeg 6.1 / libavcodec60, the host
# needs 8 / libavcodec62) — copy a from-source FFmpeg into /usr/lib/punktfunk-host, repoint the
# binary's rpath there, and drop the libav*/libsw*/libpostproc sonames from the auto Depends. Set
# FFMPEG_PREFIX to that FFmpeg's install prefix (default /opt/ffmpeg, as the noble image sets it).
# See packaging/debian/README.md → "Ubuntu 24.04 LTS".
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [ARCH=amd64] [BUNDLE_FFMPEG=1] bash packaging/debian/build-deb.sh
# Output: dist/punktfunk-host_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
ARCH="${ARCH:-amd64}"
PKG="punktfunk-host"
BUNDLE_FFMPEG="${BUNDLE_FFMPEG:-0}"
FFMPEG_PREFIX="${FFMPEG_PREFIX:-/opt/ffmpeg}"
LIBDIR_REL="usr/lib/$PKG"                     # bundled FFmpeg lands here: /usr/lib/punktfunk-host
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

BIN="target/release/$PKG"
if [ ! -x "$BIN" ]; then
  echo "==> building $PKG (release)"
  PUNKTFUNK_BUILD_VERSION="$VERSION" cargo build --release -p "$PKG" --locked   # stamp --version (build.rs)
fi
TRAY_BIN="target/release/punktfunk-tray"
# ALWAYS built here, in its OWN cargo invocation — load-bearing, not tidiness, and deliberately not
# skipped when the artifact already exists. Cargo unifies features across everything in one build,
# so a caller that co-built the tray with the host (the .deb workflow used to) leaves behind a
# binary whose zbus took the host's ashpd -> zbus/tokio while the tray runs ksni's async-io
# executor with no tokio runtime by design — it then panics at every launch with "there is no
# reactor running, must be called from the context of a Tokio 1.x runtime". Skipping the rebuild is
# exactly how that binary shipped. Building it alone keeps its zbus on async-io; cargo no-ops this
# when the existing artifact was already resolved that way, and rebuilds it when it wasn't.
echo "==> building punktfunk-tray (release, own invocation — see comment above)"
cargo build --release -p punktfunk-tray --locked

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DOCDIR="$STAGE/usr/share/doc/$PKG"
SHAREDIR="$STAGE/usr/share/$PKG"

# --- file layout (matches the RPM %install) ----------------------------------
install -Dm0755 "$BIN"                              "$STAGE/usr/bin/$PKG"
install -Dm0644 scripts/60-punktfunk.rules         "$STAGE/usr/lib/udev/rules.d/60-punktfunk.rules"
# Managed gamescope takeover on DM-autologin boxes: root helper + polkit action so the host can
# stop/restore the display manager for the stream (the helper derives the DM unit itself).
install -Dm0755 scripts/pf-dm-helper               "$STAGE/usr/libexec/punktfunk/pf-dm-helper"
install -Dm0644 scripts/io.unom.punktfunk.dm-helper.policy \
                                                   "$STAGE/usr/share/polkit-1/actions/io.unom.punktfunk.dm-helper.policy"
# vhci-hcd autoload — usbip transport for the virtual Steam Deck pad (Steam only adopts USB pads).
install -Dm0644 scripts/punktfunk-modules.conf     "$STAGE/usr/lib/modules-load.d/punktfunk.conf"
# UDP socket-buffer tuning (32 MB) — without it the kernel clamps the host's SO_SNDBUF to ~416 KB
# and high-bitrate frames overflow it (send-side packet loss). systemd-sysctl applies it at boot.
install -Dm0644 scripts/99-punktfunk-net.conf      "$STAGE/usr/lib/sysctl.d/99-punktfunk-net.conf"
install -Dm0644 scripts/punktfunk-host.service     "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# The source unit's ExecStart points at the dev source tree; a packaged install has the binary at
# /usr/bin. Rewrite it so a fresh apt install (no hand-rolled unit) starts the installed binary.
sed -i 's#%h/punktfunk/target/release/punktfunk-host#/usr/bin/punktfunk-host#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-host.service"
# Optional drop-in for a DESKTOP-LOGIN host: binds the host to graphical-session.target so a
# Plasma/GNOME restart restarts it instead of leaving it on a dead compositor connection. Shipped
# under /usr/share (NOT as an active drop-in) because it is wrong for the appliance route — the
# operator copies it into ~/.config/systemd/user/punktfunk-host.service.d/ when they want it.
install -Dm0644 scripts/punktfunk-host-desktop-session.conf \
    "$STAGE/usr/share/punktfunk-host/punktfunk-host-desktop-session.conf"
# Install-kind + channel marker, read by the host's update-check surface (planning:
# host-update-from-web-console.md §4.1). ONE canonical path across all package formats —
# /usr/share/punktfunk/, not this package's punktfunk-host/ data dir. A canary version
# carries `~ciN`; anything else is stable.
case "$VERSION" in
  *~ci*) _pf_update_channel=canary ;;
  *)     _pf_update_channel=stable ;;
esac
printf 'apt %s\n' "$_pf_update_channel" | \
    install -Dm0644 /dev/stdin "$STAGE/usr/share/punktfunk/install-kind"
# Optional headless KWin session unit (the kwin --virtual appliance), as the RPM/Arch ship.
# Repoint its ExecStart from the dev source tree to the packaged script. NOT enabled by default.
install -Dm0644 scripts/punktfunk-kde-session.service "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"
sed -i 's#%h/punktfunk/scripts/headless/run-headless-kde.sh#/usr/share/punktfunk-host/headless/run-headless-kde.sh#' \
    "$STAGE/usr/lib/systemd/user/punktfunk-kde-session.service"

# KWin Desktop-mode authorization: non-launcher .desktop whose X-KDE-Wayland-Interfaces lets the
# host bind KWin's restricted zkde_screencast (virtual output) + fake_input globals on an
# interactive Plasma session. Must ship with the host — KWin caches the per-exe grant on first
# connect, so it has to be present before the host ever connects. See the file's header comment.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
    "$STAGE/usr/share/applications/io.unom.Punktfunk.Host.desktop"
# Status tray: the per-user SNI icon + its XDG autostart entry (self-gating: --autostart exits
# silently for users who don't run a host) + the hicolor status icons it names.
install -Dm0755 "$TRAY_BIN"                        "$STAGE/usr/bin/punktfunk-tray"
install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
    "$STAGE/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop"
for sz in 22x22 48x48; do
  for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
    install -Dm0644 "$png" "$STAGE/usr/share/icons/hicolor/$sz/apps/$(basename "$png")"
  done
done
install -Dm0755 scripts/headless/run-headless-kde.sh   "$SHAREDIR/headless/run-headless-kde.sh"
install -Dm0755 scripts/headless/run-headless-sway.sh  "$SHAREDIR/headless/run-headless-sway.sh"
install -Dm0644 scripts/headless/kde-authorized        "$SHAREDIR/headless/kde-authorized"
install -Dm0644 scripts/headless/punktfunk-sink.conf   "$SHAREDIR/headless/punktfunk-sink.conf"
install -Dm0644 scripts/host.env.example           "$SHAREDIR/host.env.example"
install -Dm0644 packaging/bazzite/host.env         "$SHAREDIR/host.env.bazzite"
install -Dm0644 packaging/kde/host.env             "$SHAREDIR/host.env.kde"
install -Dm0644 api/openapi.json              "$SHAREDIR/openapi.json"
# Firewall openers (shared across all Linux packaging), NOT auto-enabled — the postinst prints the
# enable command for whichever firewall is present. Debian ships none and Ubuntu's ufw is
# installed-but-inactive, so these are a no-op until the admin turns a firewall on.
install -Dm0644 packaging/linux/punktfunk.ufw \
                "$STAGE/etc/ufw/applications.d/punktfunk"
install -Dm0644 packaging/linux/punktfunk-gamestream.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-gamestream.xml"
install -Dm0644 packaging/linux/punktfunk-native.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-native.xml"
# Web console opener (TCP 47992) — only meaningful with the optional punktfunk-web package; opened
# deliberately (see README.md → Firewall). ufw's equivalent is the punktfunk-web profile above.
install -Dm0644 packaging/linux/punktfunk-web.xml \
                "$STAGE/usr/lib/firewalld/services/punktfunk-web.xml"
install -Dm0644 LICENSE-MIT                         "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                      "$DOCDIR/LICENSE-APACHE"
install -Dm0644 README.md                           "$DOCDIR/README.md"
# Third-party crate attributions (regenerate with scripts/gen-third-party-notices.sh).
if [ -f THIRD-PARTY-NOTICES.txt ]; then
    install -Dm0644 THIRD-PARTY-NOTICES.txt "$DOCDIR/THIRD-PARTY-NOTICES.txt"
fi

# Debian copyright + changelog (cheap, keeps the package well-formed).
cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: punktfunk
Source: https://git.unom.io/unom/punktfunk

Files: *
Copyright: unom and the punktfunk contributors
License: MIT or Apache-2.0
 Dual-licensed. Full texts in /usr/share/doc/$PKG/LICENSE-MIT and
 /usr/share/doc/$PKG/LICENSE-APACHE.
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <packages@unom.io>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

# --- bundled FFmpeg (Ubuntu 24.04 LTS builds) --------------------------------
# Copy the from-source libav*/libsw*/libpostproc .so's into /usr/lib/punktfunk-host and repoint the
# binary at them, so the package carries FFmpeg 8 instead of depending on a distro libavcodec62 that
# 24.04 doesn't have. BUNDLED_LIBS is fed to dpkg-shlibdeps below so the libs' OWN external deps
# (libva2, libdrm2, …, all present on 24.04) still become Depends.
BUNDLED_LIBS=""
if [ "$BUNDLE_FFMPEG" = "1" ]; then
  command -v patchelf >/dev/null || { echo "BUNDLE_FFMPEG=1 needs patchelf" >&2; exit 1; }
  [ -d "$FFMPEG_PREFIX/lib" ] || { echo "FFMPEG_PREFIX=$FFMPEG_PREFIX has no lib/ — build FFmpeg first" >&2; exit 1; }
  DEST="$STAGE/$LIBDIR_REL"
  install -d "$DEST"
  # cp -a preserves the SONAME symlink chain (libavcodec.so -> .so.62 -> .so.62.x.x); the loader
  # resolves the binary's DT_NEEDED (libavcodec.so.62) to the middle link.
  shopt -s nullglob
  for so in "$FFMPEG_PREFIX"/lib/lib{avcodec,avformat,avutil,avfilter,avdevice,swscale,swresample,postproc}.so*; do
    cp -a "$so" "$DEST/"
  done
  shopt -u nullglob
  ls "$DEST"/libavcodec.so.* >/dev/null 2>&1 || { echo "no libav* found under $FFMPEG_PREFIX/lib" >&2; exit 1; }
  # Each bundled lib finds its siblings (libavcodec needs libavutil) via its own $ORIGIN RUNPATH;
  # patch only the real versioned files, not the symlinks. The executable then finds the top-level
  # libs via ../lib/$PKG, written as DT_RPATH (--force-rpath) so it's also searched transitively —
  # belt-and-suspenders against DT_RUNPATH's non-transitivity.
  for so in "$DEST"/*.so.*; do
    [ -L "$so" ] && continue
    patchelf --set-rpath '$ORIGIN' "$so"
  done
  patchelf --force-rpath --set-rpath "\$ORIGIN/../lib/$PKG" "$STAGE/usr/bin/$PKG"
  BUNDLED_LIBS="$(printf '%s ' "$DEST"/*.so.*)"
  echo "==> bundled FFmpeg from $FFMPEG_PREFIX into /$LIBDIR_REL"
fi

# --- dependencies ------------------------------------------------------------
# Auto: the binary's directly-linked shared libs (libcuda ignored, see header). In bundle mode the
# bundled .so's are appended so their external deps (libva2/libdrm2/…) are captured too.
SHLIB_TMP="$(mktemp -d)"
mkdir -p "$SHLIB_TMP/debian"
cat > "$SHLIB_TMP/debian/control" <<EOF
Source: $PKG

Package: $PKG
Architecture: any
Depends: \${shlibs:Depends}
EOF
# In bundle mode the libav* live in FFMPEG_PREFIX/lib — not a standard loader path, and the
# target/release binary carries no rpath (only the staged copy does) — so dpkg-shlibdeps can't
# locate libavcodec.so.62 and exits 2. Point it there via LD_LIBRARY_PATH. Stderr is captured so a
# future resolution failure is visible instead of swallowed.
SHDEPS_RAW="$(
  cd "$SHLIB_TMP"
  if [ "$BUNDLE_FFMPEG" = "1" ]; then export LD_LIBRARY_PATH="$FFMPEG_PREFIX/lib"; fi
  dpkg-shlibdeps -O --ignore-missing-info "$ROOTDIR/$BIN" $BUNDLED_LIBS 2>"$SHLIB_TMP/err" \
    | sed -n 's/^shlibs:Depends=//p'
)" || { echo "dpkg-shlibdeps failed (exit $?):" >&2; sed 's/^/  /' "$SHLIB_TMP/err" >&2; rm -rf "$SHLIB_TMP"; exit 1; }
rm -rf "$SHLIB_TMP"
[ -n "$SHDEPS_RAW" ] || { echo "dpkg-shlibdeps produced no deps — is dpkg-dev installed?" >&2; exit 1; }

# Drop the NVIDIA driver lib unconditionally. --ignore-missing-info already skips libcuda on a
# GPU-less builder (stub, no owning package), but on a box WITH the driver shlibdeps resolves
# libcuda.so.1 -> libnvidia-compute-<ver> and would pin that exact driver build. NVENC/EGL are
# provided by whatever driver the host runs, so this must never be a package dependency.
# In bundle mode also drop the FFmpeg sonames: they're shipped inside the package (/usr/lib/$PKG),
# not pulled from apt, so a `Depends: libavcodec62` would wrongly re-block install on 24.04.
FILTER='^(libnvidia-compute|libcuda)'
[ "$BUNDLE_FFMPEG" = "1" ] && FILTER='^(libnvidia-compute|libcuda|libav|libsw|libpostproc)'
SHDEPS="$(printf '%s' "$SHDEPS_RAW" | tr ',' '\n' | sed 's/^ *//; s/ *$//' \
          | grep -ivE "$FILTER" | awk 'NF' | paste -sd ',' - | sed 's/,/, /g')"
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
# punktfunk-scripting = the plugin/script runner (host automation on bun). Recommends so it's pulled
# by default; its systemd --user unit ships disabled (inert until you add scripts/plugins).
RECOMMENDS="ffmpeg, gamescope, pipewire-pulse, mesa-va-drivers, intel-media-va-driver, punktfunk-web, punktfunk-scripting"
SUGGESTS="kwin-wayland, mutter"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: unom <packages@unom.io>
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
    # Debian ships no active firewall and Ubuntu's ufw is inactive by default; hint whichever is present.
    if command -v ufw >/dev/null 2>&1; then
        echo "Firewall (ufw detected): sudo ufw allow punktfunk-native   (or punktfunk-gamestream for Moonlight)"
    fi
    if command -v firewall-cmd >/dev/null 2>&1; then
        echo "Firewall (firewalld detected): sudo firewall-cmd --reload &&"
        echo "    sudo firewall-cmd --permanent --add-service=punktfunk-native && sudo firewall-cmd --reload"
        echo "    (use punktfunk-gamestream for the Moonlight-compat host)"
    fi
    # Conflicting Moonlight-compatible host (Sunshine/Apollo/...): reuse the host's own detector so
    # the warning lives in one place. Exit 1 = found; never fail the install on it.
    if command -v punktfunk-host >/dev/null 2>&1; then
        if ! conflict="$(punktfunk-host detect-conflicts 2>/dev/null)"; then
            echo ""
            echo "$conflict"
        fi
    fi
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
