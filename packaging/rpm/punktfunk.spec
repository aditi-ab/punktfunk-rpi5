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
# Version/Release are overridable so CI can stamp a rolling snapshot: a main build passes
#   --define "pf_version 0.0.1" --define "pf_release 0.ci42.gdeadbee"
# (Release starting "0." sorts BEFORE the eventual "1" release), a v* tag passes the clean
# version with "pf_release 1". A plain `rpmbuild` (or COPR) with no defines builds 0.0.1-1.
Version:        %{?pf_version}%{!?pf_version:0.0.1}
Release:        %{?pf_release}%{!?pf_release:1}%{?dist}
Summary:        Low-latency desktop/game streaming host (Moonlight-compatible + punktfunk/1)

License:        MIT OR Apache-2.0
URL:            https://git.unom.io/unom/punktfunk
# COPR SCM builds provide the checkout; for a tarball build, drop a git archive here:
Source0:        %{name}-%{version}.tar.gz

# punktfunk-host is Linux-only and links system FFmpeg/PipeWire/Opus.
ExclusiveArch:  x86_64 aarch64

# The zerocopy FFI links the NVIDIA driver's libcuda.so.1; rpm's auto-dep generator would turn
# that into a hard Requires on libcuda.so.1 (and we never want to pin the driver — NVENC/EGL come
# from whatever NVIDIA stack the host runs, expressed below as the weak xorg-x11-drv-nvidia-cuda
# Recommends). Drop it from the auto-Requires, mirroring the Debian package's NVIDIA filter.
%global __requires_exclude ^libcuda\\.so.*$

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
# The client subpackage (GTK4 shell + SDL3 gamepads).
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  pkgconfig(sdl3)
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

%description client
The native Linux client for punktfunk. Discovers hosts on the LAN (mDNS), trusts
them via certificate pinning with a SPAKE2 PIN pairing ceremony, and streams HEVC
video (GF(2^16) Leopard FEC + AES-GCM over UDP, QUIC control plane) with Opus
audio, microphone passthrough, and full gamepad support including DualSense
touchpad, motion, adaptive triggers and lightbar through SDL3. The host creates a
virtual output at exactly this client's resolution and refresh rate — no scaling.

%prep
%autosetup -n %{name}-%{version}

%build
# Release build of the host + client binaries (the workspace also has the core lib).
# cargo fetches crates over the network; COPR build hosts allow this.
export RUSTFLAGS="%{?build_rustflags}"
cargo build --release -p punktfunk-host -p punktfunk-client-linux

%install
# Binary
install -Dm0755 target/release/punktfunk-host %{buildroot}%{_bindir}/punktfunk-host

# udev rule — /dev/uinput access for virtual gamepads (input group).
install -Dm0644 scripts/60-punktfunk.rules %{buildroot}%{_udevrulesdir}/60-punktfunk.rules

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

# --- client subpackage ---
install -Dm0755 target/release/punktfunk-client %{buildroot}%{_bindir}/punktfunk-client
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
install -Dm0644 docs/api/openapi.json                  %{buildroot}%{_datadir}/%{name}/openapi.json

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md docs/implementation-plan.md packaging/README.md
%{_bindir}/punktfunk-host
%{_udevrulesdir}/60-punktfunk.rules
%{_prefix}/lib/sysctl.d/99-punktfunk-net.conf
%{_userunitdir}/punktfunk-host.service
%{_userunitdir}/punktfunk-kde-session.service
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/*

%files client
%license LICENSE-MIT LICENSE-APACHE
%{_bindir}/punktfunk-client
%{_datadir}/applications/io.unom.Punktfunk.desktop
%{_udevrulesdir}/70-punktfunk-client.rules
%{_prefix}/lib/sysctl.d/99-punktfunk-client-net.conf

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

%changelog
* Wed Jun 10 2026 punktfunk <noreply@anthropic.com> - 0.0.1-1
- Initial RPM: punktfunk-host + udev rule + systemd user unit + headless helpers.
