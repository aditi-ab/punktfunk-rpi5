################################################################################
# punktfunk — low-latency desktop/game streaming host (RPM for Fedora / Bazzite)
#
# Builds `punktfunk-host` from source with cargo and installs the binary, the
# uinput udev rule (virtual gamepads), the systemd *user* unit, and the headless
# session helpers. Designed for COPR (build-from-SCM): COPR clones the repo and
# runs this spec; `cargo build` fetches crates over the network (COPR allows it).
#
# DEPENDENCIES NOT IN BASE FEDORA:
#   * ffmpeg / ffmpeg-libs with NVENC — from RPM Fusion *nonfree*. Enable it in
#     the COPR project (External Repositories) and on the target host.
#   * The NVIDIA driver (libnvidia-encode / libEGL_nvidia) — present on Bazzite's
#     -nvidia images; on plain Fedora install akmod-nvidia + xorg-x11-drv-nvidia-cuda.
#
# Bazzite already ships gamescope, PipeWire and the NVIDIA stack, so on Bazzite the
# only new runtime bits are ffmpeg-libs (RPM Fusion) + opus + libei.
################################################################################

Name:           punktfunk
# Version/Release are overridable so CI can stamp a rolling snapshot: a canary main build passes
#   --define "pf_version 0.3.0" --define "pf_release 0.ci42.gdeadbee"
# (Release starting "0." sorts BEFORE the eventual "1" release; the canary base stays one minor
# ahead of the latest stable), a vX.Y.Z release tag passes the clean version with "pf_release 1".
# A plain `rpmbuild` (or COPR) with no defines builds 0.3.0-1.
Version:        %{?pf_version}%{!?pf_version:0.3.0}
Release:        %{?pf_release}%{!?pf_release:1}%{?dist}
Summary:        Low-latency desktop/game streaming host (Moonlight-compatible + punktfunk/1)

License:        MIT OR Apache-2.0
URL:            https://git.unom.io/unom/punktfunk
# COPR SCM builds provide the checkout; for a tarball build, drop a git archive here:
Source0:        %{name}-%{version}.tar.gz

# punktfunk-host is Linux-only and links system FFmpeg/PipeWire/Opus. x86_64 only for now: encode
# is NVENC (desktop NVIDIA) and no aarch64 build is produced/published by CI — claiming aarch64
# here would advertise an arch we never ship. Re-add aarch64 once there's an arm64 build leg.
ExclusiveArch:  x86_64

# The zerocopy FFI links the NVIDIA driver's libcuda.so.1; rpm's auto-dep generator would turn
# that into a hard Requires on libcuda.so.1 (and we never want to pin the driver — NVENC/EGL come
# from whatever NVIDIA stack the host runs, expressed below as the weak xorg-x11-drv-nvidia-cuda
# Recommends). Drop it from the auto-Requires, mirroring the Debian package's NVIDIA filter.
%global __requires_exclude ^libcuda\\.so.*$

# Management web console subpackage (punktfunk-web). OFF by default: building the Nitro SSR bundle
# (and running it) needs `bun`, which a plain rpmbuild / COPR mock chroot does NOT have. CI's builder
# image (ci/fedora-rpm.Dockerfile) DOES have bun and builds with `--with web`, so the Gitea RPM
# registry carries punktfunk-web. COPR (no bun) builds host+client only — use the Gitea registry for
# the console, or enable bun + `--with web` in the COPR project. Mirrors the Debian punktfunk-web .deb.
%bcond_with web

# --- Build toolchain ---------------------------------------------------------
BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  clang
BuildRequires:  clang-devel
BuildRequires:  cmake
BuildRequires:  nasm
BuildRequires:  pkgconfig
BuildRequires:  systemd-rpm-macros
# Link-time system libraries (the -sys crates probe these via pkg-config):
BuildRequires:  pkgconfig(libpipewire-0.3)
BuildRequires:  pkgconfig(libspa-0.2)
BuildRequires:  pkgconfig(wayland-client)
BuildRequires:  pkgconfig(xkbcommon)
BuildRequires:  pkgconfig(opus)
# FFmpeg dev headers with NVENC — from RPM Fusion (ffmpeg-devel), NOT ffmpeg-free.
# Version-agnostic: ffmpeg-sys-next auto-detects the installed FFmpeg, so this builds
# against FFmpeg 7.x (libavcodec 61, e.g. Fedora 43 / Bazzite) or 8.x (libavcodec 62).
BuildRequires:  pkgconfig(libavcodec)
BuildRequires:  pkgconfig(libavformat)
BuildRequires:  pkgconfig(libavutil)
# Zero-copy GPU path: src/zerocopy/ links libGL + libgbm (mesa) via hand-rolled FFI.
BuildRequires:  pkgconfig(gl)
BuildRequires:  pkgconfig(gbm)
# The client subpackage (GTK4 shell + SDL3 gamepads + the Vulkan session streamer).
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(sdl3)
# The client's pf-ffvk crate runs bindgen over FFmpeg's libavutil/hwcontext_vulkan.h, which
# #include <vulkan/vulkan.h> — provided by vulkan-headers (Fedora).
BuildRequires:  vulkan-headers
# It ALSO links the NVIDIA CUDA driver lib (-lcuda) via FFI, so libcuda.so must be present
# at LINK time. A normal NVIDIA host (or Bazzite -nvidia) has it; a headless COPR/koji builder
# without a GPU does NOT — point %build at the CUDA toolkit stub (…/stubs/libcuda.so) there,
# e.g. `ln -s $(rpm -ql cuda-cudart-devel | grep stubs/libcuda.so | head -1) /usr/lib64/`.
# (Proper fix tracked separately: make the cuda/gbm/GL FFI dlopen-based like khronos-egl.)

# --- Runtime -----------------------------------------------------------------
Requires:       pipewire
Requires:       pipewire-pulseaudio
Requires:       wireplumber
Requires:       opus
Requires:       libei
# FFmpeg runtime with NVENC (RPM Fusion). Weak-dep so the package installs even if
# the user hasn't enabled RPM Fusion yet, but it WILL fail to encode without it.
Recommends:     ffmpeg-libs
# A compositor to drive. Bazzite ships gamescope; the others are user choice.
Recommends:     gamescope
Suggests:       kwin
Suggests:       mutter
# NVENC + GPU EGL come from the NVIDIA driver; on Bazzite the -nvidia image has it.
Recommends:     (xorg-x11-drv-nvidia-cuda if xorg-x11-drv-nvidia)
# VAAPI encode drivers for AMD (radeonsi) / Intel (iHD) — the auto-selected VAAPI backend on a
# non-NVIDIA GPU. NOTE: Fedora's stock mesa-va-drivers has HEVC/AV1 *disabled* (patents); full
# encode needs mesa-va-drivers-freeworld from RPM Fusion (same nonfree repo as ffmpeg-libs).
Recommends:     mesa-va-drivers
Recommends:     intel-media-driver
# The management web console (pairing + status) every user needs — a separate noarch subpackage.
# Weak-dep so `dnf install punktfunk` pulls it where it exists (the Gitea registry); harmless where
# it doesn't (a COPR build without `--with web` simply has no punktfunk-web to satisfy).
Recommends:     punktfunk-web

%description
punktfunk is a Linux-first, low-latency desktop and game streaming host. It speaks
the Moonlight/GameStream protocol (pair a stock Moonlight client) and its own native
punktfunk/1 protocol (GF(2^16) Leopard FEC + AES-GCM, mid-stream mode renegotiation,
client microphone passthrough). Each session gets a virtual output at the client's
exact resolution and refresh via a per-compositor backend (KWin, gamescope, Mutter,
Sway/wlroots), captured zero-copy (dmabuf -> CUDA -> NVENC) and split-encoded above
~1 Gpix/s. Input (mouse/keyboard/gamepads) is injected back into the session.

%package client
Summary:        Low-latency desktop/game streaming client (punktfunk/1, GTK4)
# Audio playback / mic capture want the PipeWire daemon; degrade gracefully without it.
Recommends:     pipewire
Recommends:     wireplumber
# The session streamer loads libvulkan at runtime (ash) for its ash/Skia presenter + Vulkan
# Video decode. vulkan-loader provides libvulkan.so.1; the ICD is the GPU's mesa/NVIDIA driver.
Requires:       vulkan-loader

%description client
The native Linux client for punktfunk. Discovers hosts on the LAN (mDNS), trusts
them via certificate pinning with a SPAKE2 PIN pairing ceremony, and streams HEVC
video (GF(2^16) Leopard FEC + AES-GCM over UDP, QUIC control plane) with Opus
audio, microphone passthrough, and full gamepad support including DualSense
touchpad, motion, adaptive triggers and lightbar through SDL3. The host creates a
virtual output at exactly this client's resolution and refresh rate — no scaling.

%if %{with web}
%package web
Summary:        punktfunk management web console (Nitro SSR on bun + React)
# Runtime is BUN (the console uses Nitro's `bun` preset + a Bun.serve TLS entry — node can't
# run it). Bun isn't in Fedora repos, so we VENDOR a bun binary into the package, which makes this
# subpackage arch-specific (it can no longer be noarch). No system nodejs/bun dependency.

%description web
The browser console for a punktfunk streaming host: status, paired devices, and the SPAKE2
PIN pairing flow every client needs. Runs as a systemd --user service on port 3000 over HTTPS
(HTTP/1.1 over TLS, with the host's own identity cert), login-gated (a password generated on first
start), proxying the host's loopback HTTPS management API with a bearer token injected server-side
(never sent to the browser). Auto-wired to the host on a packaged install — it sources the host's
mgmt token, identity cert, and a generated login password, no env editing. Bundles its own bun
runtime. Enable with `systemctl --user enable --now punktfunk-web`.
%endif

%prep
%autosetup -n %{name}-%{version}

%build
# Release build of the host + client binaries (the workspace also has the core lib).
# cargo fetches crates over the network; COPR build hosts allow this.
export RUSTFLAGS="%{?build_rustflags}"
# Use the toolchain baked into the builder image as-is, ignoring rust-toolchain.toml. The toml
# floats `channel = "stable"` and requests rustfmt/clippy (lint-only — not needed for a build); when
# a newer stable lands upstream, that combination makes rustup try to UPDATE the baked, minimal-
# profile `stable` toolchain in place, and the in-image OverlayFS rejects the staging rename with
# EXDEV ("Invalid cross-device link"), failing %build. RUSTUP_TOOLCHAIN bypasses the toml so rustup
# neither re-resolves the channel nor adds components — it just builds with what's installed.
export RUSTUP_TOOLCHAIN=stable
# Stamp the exact NVR into the binary for --version / mgmt /health provenance (build.rs reads it).
export PUNKTFUNK_BUILD_VERSION="%{version}-%{release}"
# --locked: reproducible from (commit + Cargo.lock), matching the .deb build path.
# punktfunk-client-session is the Vulkan/Skia streamer the shell execs for a connect — both
# client binaries must ship or streaming from the desktop client breaks.
# --features punktfunk-host/nvenc: the direct-SDK NVENC path (real RFI + recovery anchor on Linux
# NVIDIA; design/linux-direct-nvenc.md). AMD/Intel-safe — the NVENC/CUDA entry points are dlopen'd
# at runtime (no link-time dep; __requires_exclude already drops libcuda), so the binary starts
# driver-less; the encoder is only built for a CUDA frame + PUNKTFUNK_NVENC_DIRECT, never on VAAPI.
cargo build --release --locked --features punktfunk-host/nvenc \
  -p punktfunk-host -p punktfunk-client-linux -p punktfunk-client-session -p punktfunk-tray

%if %{with web}
# Management web console: build the Nitro SSR bundle with bun (the `bun` preset + our Bun.serve
# TLS entry). bun is both the build tool AND the runtime (vendored in %%install below).
(cd web && bun install --frozen-lockfile && bun run build)
if ! grep -q 'Bun\.serve' web/.output/server/index.mjs; then
  echo "ERROR: web build is not a bun bundle — need the 'bun' preset + custom entry" >&2
  exit 1
fi
%endif

%install
# Binary
install -Dm0755 target/release/punktfunk-host %{buildroot}%{_bindir}/punktfunk-host

# udev rule — /dev/uinput access for virtual gamepads (input group).
install -Dm0644 scripts/60-punktfunk.rules %{buildroot}%{_udevrulesdir}/60-punktfunk.rules

# vhci-hcd autoload — the usbip transport that makes the virtual Steam Deck controller a
# real USB device (Steam Input only adopts those; the UHID fallback is invisible to Steam).
install -Dm0644 scripts/punktfunk-modules.conf %{buildroot}%{_prefix}/lib/modules-load.d/punktfunk.conf

# UDP socket-buffer tuning (32 MB) — without it the kernel clamps the host's SO_SNDBUF to ~416 KB
# and high-bitrate frames overflow it (send-side loss). systemd-sysctl applies it at boot.
install -Dm0644 scripts/99-punktfunk-net.conf %{buildroot}%{_prefix}/lib/sysctl.d/99-punktfunk-net.conf

# systemd *user* unit (the host runs in the graphical session, not as root).
install -Dm0644 scripts/punktfunk-host.service %{buildroot}%{_userunitdir}/punktfunk-host.service
# The source unit's ExecStart points at the dev source tree; a packaged install has the binary at
# %{_bindir}. Rewrite it so a fresh install (no hand-rolled unit) starts the installed binary.
sed -i 's#%h/punktfunk/target/release/punktfunk-host#%{_bindir}/punktfunk-host#' %{buildroot}%{_userunitdir}/punktfunk-host.service

# Optional headless KDE session unit (the kwin streaming appliance): brings up `kwin --virtual` on
# wayland-kde via the packaged run-headless-kde.sh, so the host's kwin backend has a session whose
# privileged screencast protocol it can bind. Repoint its ExecStart from the dev source tree to the
# installed script. NOT enabled by default — only kwin-backend hosts (e.g. Fedora/Ubuntu KDE) need it.
install -Dm0644 scripts/punktfunk-kde-session.service %{buildroot}%{_userunitdir}/punktfunk-kde-session.service
sed -i 's#%h/punktfunk/scripts/headless/run-headless-kde.sh#%{_datadir}/%{name}/headless/run-headless-kde.sh#' %{buildroot}%{_userunitdir}/punktfunk-kde-session.service

# KWin authorization for Desktop-mode (KWin) streaming: a non-launcher .desktop whose
# X-KDE-Wayland-Interfaces grants the host the restricted zkde_screencast (virtual output) +
# fake_input globals on an interactive Plasma session. Must ship with the host so it is present
# before the host first connects (KWin caches the per-exe grant). Replaces the old manual
# KWIN_WAYLAND_NO_PERMISSION_CHECKS hack for the screencast permission.
install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
                %{buildroot}%{_datadir}/applications/io.unom.Punktfunk.Host.desktop

# Status tray: the per-user SNI icon + its XDG autostart entry (self-gating: --autostart exits
# silently for users who don't run a host) + the hicolor status icons it names.
install -Dm0755 target/release/punktfunk-tray %{buildroot}%{_bindir}/punktfunk-tray
install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
                %{buildroot}%{_sysconfdir}/xdg/autostart/io.unom.Punktfunk.Tray.desktop
for sz in 22x22 48x48; do
  for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
    install -Dm0644 "$png" %{buildroot}%{_datadir}/icons/hicolor/$sz/apps/"$(basename "$png")"
  done
done

# --- client subpackage ---
install -Dm0755 target/release/punktfunk-client %{buildroot}%{_bindir}/punktfunk-client
# The session streamer the shell execs for a connect (resolved as its sibling in %{_bindir}).
install -Dm0755 target/release/punktfunk-session %{buildroot}%{_bindir}/punktfunk-session
install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
                %{buildroot}%{_datadir}/applications/io.unom.Punktfunk.desktop
# DualSense hidraw access (full pad fidelity through SDL's HIDAPI driver).
install -Dm0644 scripts/70-punktfunk-client.rules \
                %{buildroot}%{_udevrulesdir}/70-punktfunk-client.rules
# UDP receive-buffer tuning (32 MB) — the client asks for a 32 MB SO_RCVBUF; without raising
# net.core.rmem_max the kernel clamps it and high-bitrate streams overflow at the receiver
# (measured: 4 MB cap = 31.6% loss at 2 Gbps, 32 MB = 0%). Distinct filename from the host's so
# both can be installed on one box.
install -Dm0644 scripts/99-punktfunk-client-net.conf \
                %{buildroot}%{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf

# Headless session helpers + example config + OpenAPI doc (reference material).
install -d %{buildroot}%{_datadir}/%{name}/headless
install -Dm0755 scripts/headless/run-headless-kde.sh   %{buildroot}%{_datadir}/%{name}/headless/run-headless-kde.sh
install -Dm0755 scripts/headless/run-headless-sway.sh  %{buildroot}%{_datadir}/%{name}/headless/run-headless-sway.sh
# RemoteDesktop grant pre-seed for headless libei input (run-headless-kde.sh copies it in).
install -Dm0644 scripts/headless/kde-authorized        %{buildroot}%{_datadir}/%{name}/headless/kde-authorized
# Virtual "Punktfunk" speaker (null sink the host captures/streams; run-headless-kde.sh installs it).
install -Dm0644 scripts/headless/punktfunk-sink.conf   %{buildroot}%{_datadir}/%{name}/headless/punktfunk-sink.conf
install -Dm0644 scripts/host.env.example               %{buildroot}%{_datadir}/%{name}/host.env.example
install -Dm0644 packaging/bazzite/host.env             %{buildroot}%{_datadir}/%{name}/host.env.bazzite
install -Dm0644 packaging/kde/host.env                 %{buildroot}%{_datadir}/%{name}/host.env.kde
# Bazzite KDE Desktop-mode one-shot setup (seeds the RemoteDesktop grant for libei input; the
# screencast/virtual-output grant ships as io.unom.Punktfunk.Host.desktop, installed above).
install -d %{buildroot}%{_datadir}/%{name}/bazzite
install -Dm0755 packaging/bazzite/kde-desktop-setup.sh %{buildroot}%{_datadir}/%{name}/bazzite/kde-desktop-setup.sh
# Headless GAME-mode fix: a gamescope-session-plus sessions.d drop-in that falls back to gamescope's
# headless backend when no display is connected (so "Switch to Game Mode" works on a display-less
# streaming host instead of crashing + 5-striking back to desktop). No-op on display-attached boxes.
# Sourced by gamescope-session-plus as /etc/gamescope-session-plus/sessions.d/steam (after its
# /usr/share defaults). Harmless on non-gamescope systems (the file is simply never read).
install -Dm0644 packaging/bazzite/gamescope-headless-session \
                %{buildroot}/etc/gamescope-session-plus/sessions.d/steam
install -Dm0644 api/openapi.json                  %{buildroot}%{_datadir}/%{name}/openapi.json
# firewalld service definitions (shared across all Linux packaging). Fedora/RHEL enable firewalld by
# default, so these matter here; NOT auto-enabled — %post prints the enable command. Owned by the
# firewalld package's dir; we drop only the files (same pattern as the sysctl.d file above).
install -Dm0644 packaging/linux/punktfunk-gamestream.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-gamestream.xml
install -Dm0644 packaging/linux/punktfunk-native.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-native.xml
# Web console opener (TCP 47992) — only meaningful with the web subpackage, opened deliberately.
install -Dm0644 packaging/linux/punktfunk-web.xml \
                %{buildroot}%{_prefix}/lib/firewalld/services/punktfunk-web.xml

%if %{with web}
# --- web console subpackage (punktfunk-web) ---
install -d %{buildroot}%{_datadir}/punktfunk-web/.output
cp -r web/.output/server %{buildroot}%{_datadir}/punktfunk-web/.output/server
cp -r web/.output/public %{buildroot}%{_datadir}/punktfunk-web/.output/public
# Vendor the bun runtime (the build env's bun — the CI rpm image) into
# a private libexec dir so it never collides with a system-wide bun on PATH. This is why the web
# subpackage is arch-specific (above): bun is a native binary.
install -Dm0755 "$(command -v bun)" %{buildroot}%{_libexecdir}/punktfunk-web/bun
# PATH-stable launcher (matches the .deb's /usr/bin/punktfunk-web-server) — runs on the vendored bun.
cat > %{buildroot}%{_bindir}/punktfunk-web-server <<'WRAP'
#!/bin/sh
exec /usr/libexec/punktfunk-web/bun /usr/share/punktfunk-web/.output/server/index.mjs "$@"
WRAP
chmod 0755 %{buildroot}%{_bindir}/punktfunk-web-server
# systemd --user units: the console runs per-user; web-init generates the login password.
install -Dm0644 scripts/punktfunk-web.service      %{buildroot}%{_userunitdir}/punktfunk-web.service
install -Dm0644 scripts/punktfunk-web-init.service %{buildroot}%{_userunitdir}/punktfunk-web-init.service
install -Dm0755 scripts/web-init.sh                %{buildroot}%{_datadir}/punktfunk-web/web-init.sh
install -Dm0644 web/web.env.example                %{buildroot}%{_datadir}/punktfunk-web/web.env.example
%endif

%files
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%doc README.md packaging/README.md
%{_bindir}/punktfunk-host
%{_bindir}/punktfunk-tray
%{_udevrulesdir}/60-punktfunk.rules
%{_prefix}/lib/modules-load.d/punktfunk.conf
%{_prefix}/lib/sysctl.d/99-punktfunk-net.conf
%{_prefix}/lib/firewalld/services/punktfunk-gamestream.xml
%{_prefix}/lib/firewalld/services/punktfunk-native.xml
%{_prefix}/lib/firewalld/services/punktfunk-web.xml
%{_userunitdir}/punktfunk-host.service
%{_userunitdir}/punktfunk-kde-session.service
%{_datadir}/applications/io.unom.Punktfunk.Host.desktop
%{_sysconfdir}/xdg/autostart/io.unom.Punktfunk.Tray.desktop
%{_datadir}/icons/hicolor/*/apps/punktfunk-tray*.png
%dir /etc/gamescope-session-plus
%dir /etc/gamescope-session-plus/sessions.d
%config(noreplace) /etc/gamescope-session-plus/sessions.d/steam
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/*

%files client
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%{_bindir}/punktfunk-client
%{_bindir}/punktfunk-session
%{_datadir}/applications/io.unom.Punktfunk.desktop
%{_udevrulesdir}/70-punktfunk-client.rules
%{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf

%if %{with web}
%files web
%license LICENSE-MIT LICENSE-APACHE THIRD-PARTY-NOTICES.txt
%{_bindir}/punktfunk-web-server
%dir %{_libexecdir}/punktfunk-web
%{_libexecdir}/punktfunk-web/bun
%dir %{_datadir}/punktfunk-web
%{_datadir}/punktfunk-web/.output
%{_datadir}/punktfunk-web/web-init.sh
%{_datadir}/punktfunk-web/web.env.example
%{_userunitdir}/punktfunk-web.service
%{_userunitdir}/punktfunk-web-init.service
%endif

%post client
# Pick up the DualSense hidraw rule without a reboot (best-effort; on rpm-ostree it
# applies on the next boot into the layered deployment).
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=hidraw 2>/dev/null || :
# Apply the UDP recv-buffer tuning now (also auto-applied at boot by systemd-sysctl; on
# rpm-ostree it takes effect on the next boot into the layered deployment).
sysctl -p %{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf >/dev/null 2>&1 || :

%post
# Reload udev so /dev/uinput picks up the new rule without a reboot (best-effort).
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=misc 2>/dev/null || :
# Apply the UDP socket-buffer tuning (also auto-applied at boot by systemd-sysctl; on rpm-ostree
# it takes effect on the next boot into the layered deployment).
sysctl -p %{_prefix}/lib/sysctl.d/99-punktfunk-net.conf >/dev/null 2>&1 || :
echo "punktfunk installed. Add yourself to the 'input' group (sudo usermod -aG input \$USER)"
echo "then enable the host: systemctl --user enable --now punktfunk-host"
echo "Config: cp %{_datadir}/%{name}/host.env.bazzite ~/.config/punktfunk/host.env"
# Fedora/RHEL run firewalld by default — point the way to the installed service definitions.
if command -v firewall-cmd >/dev/null 2>&1; then
    echo "Firewall (firewalld): sudo firewall-cmd --reload &&"
    echo "    sudo firewall-cmd --permanent --add-service=punktfunk-gamestream && sudo firewall-cmd --reload"
    echo "    (use punktfunk-native for the native-only host)"
fi

%if %{with web}
%post web
echo "punktfunk-web installed. Enable the console for your user:"
echo "    systemctl --user enable --now punktfunk-web"
echo "A login password is generated on first start — read it with:"
echo "    journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'"
echo "Then open https://<host-ip>:47992"
%endif

%changelog
* Mon Jun 15 2026 punktfunk <noreply@anthropic.com> - 0.0.1-2
- Add punktfunk-web subpackage (management console, --with web; auto-wired to the host token).
* Wed Jun 10 2026 punktfunk <noreply@anthropic.com> - 0.0.1-1
- Initial RPM: punktfunk-host + udev rule + systemd user unit + headless helpers.
