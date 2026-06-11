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
Version:        0.0.1
Release:        1%{?dist}
Summary:        Low-latency desktop/game streaming host (Moonlight-compatible + punktfunk/1)

License:        MIT OR Apache-2.0
URL:            https://git.unom.io/unom/punktfunk
# COPR SCM builds provide the checkout; for a tarball build, drop a git archive here:
Source0:        %{name}-%{version}.tar.gz

# punktfunk-host is Linux-only and links system FFmpeg/PipeWire/Opus.
ExclusiveArch:  x86_64 aarch64

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

%prep
%autosetup -n %{name}-%{version}

%build
# Release build of the host binary only (the workspace also has the core lib + clients).
# cargo fetches crates over the network; COPR build hosts allow this.
export RUSTFLAGS="%{?build_rustflags}"
cargo build --release -p punktfunk-host

%install
# Binary
install -Dm0755 target/release/punktfunk-host %{buildroot}%{_bindir}/punktfunk-host

# udev rule — /dev/uinput access for virtual gamepads (input group).
install -Dm0644 scripts/60-punktfunk.rules %{buildroot}%{_udevrulesdir}/60-punktfunk.rules

# systemd *user* unit (the host runs in the graphical session, not as root).
install -Dm0644 scripts/punktfunk-host.service %{buildroot}%{_userunitdir}/punktfunk-host.service

# Headless session helpers + example config + OpenAPI doc (reference material).
install -d %{buildroot}%{_datadir}/%{name}/headless
install -Dm0755 scripts/headless/run-headless-kde.sh   %{buildroot}%{_datadir}/%{name}/headless/run-headless-kde.sh
install -Dm0755 scripts/headless/run-headless-sway.sh  %{buildroot}%{_datadir}/%{name}/headless/run-headless-sway.sh
install -Dm0644 scripts/host.env.example               %{buildroot}%{_datadir}/%{name}/host.env.example
install -Dm0644 packaging/bazzite/host.env             %{buildroot}%{_datadir}/%{name}/host.env.bazzite
install -Dm0644 docs/api/openapi.json                  %{buildroot}%{_datadir}/%{name}/openapi.json

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md docs/implementation-plan.md packaging/README.md
%{_bindir}/punktfunk-host
%{_udevrulesdir}/60-punktfunk.rules
%{_userunitdir}/punktfunk-host.service
%dir %{_datadir}/%{name}
%{_datadir}/%{name}/*

%post
# Reload udev so /dev/uinput picks up the new rule without a reboot (best-effort).
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=misc 2>/dev/null || :
echo "punktfunk installed. Add yourself to the 'input' group (sudo usermod -aG input \$USER)"
echo "then enable the host: systemctl --user enable --now punktfunk-host"
echo "Config: cp %{_datadir}/%{name}/host.env.bazzite ~/.config/punktfunk/host.env"

%changelog
* Tue Jun 10 2026 punktfunk <noreply@anthropic.com> - 0.0.1-1
- Initial RPM: punktfunk-host + udev rule + systemd user unit + headless helpers.
