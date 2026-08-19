---
title: Build from source
description: Compile the Linux host yourself — on Ubuntu/Debian, Fedora, or with the Arch PKGBUILD — when no package fits your release or you want to track main.
---

The package repos are the supported path ([Install the Host](/docs/install)). Build from source when
your release is older than a package supports (Ubuntu before 26.04, Debian 12, a Fedora without a
repo group), or to hack on it. A source build gets **no packaged units and no clean updates** — you
wire the service up by hand ([Running as a service](/docs/running-as-a-service) shows the unit).

Two build features matter on every distro: `punktfunk-host/nvenc` (direct NVENC on NVIDIA) and
`punktfunk-host/vulkan-encode` (Vulkan Video on AMD/Intel). They're what the packaged builds use;
without them the host falls back to the slower libav backends. Rust comes from
[rustup](https://rustup.rs) if you don't have it:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Ubuntu / Debian

The packaged host is built against **FFmpeg 8**. Ubuntu 26.04's and Debian 13's `libavcodec-dev`
are new enough; Ubuntu 24.04's is FFmpeg 6.1 — build FFmpeg 8 yourself first there (what
`ci/rust-ci-noble.Dockerfile` does), or stick with the packaged host.

```sh
sudo apt install build-essential pkg-config cmake clang libclang-dev nasm git curl \
  pipewire pipewire-pulse wireplumber libpipewire-0.3-dev libspa-0.2-dev \
  libwayland-dev wayland-protocols libxkbcommon-dev libopus-dev \
  libdrm-dev libgbm-dev libgl-dev libegl-dev libgles-dev mesa-common-dev libva-dev \
  ffmpeg libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libnvidia-egl-wayland1 libnvidia-egl-gbm1 libei-dev
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host
```

## Fedora

```sh
sudo dnf install gcc gcc-c++ make cmake clang clang-devel nasm git pkgconf-pkg-config \
  pipewire-devel wayland-devel wayland-protocols-devel libxkbcommon-devel opus-devel \
  libdrm-devel mesa-libgbm-devel mesa-libGL-devel mesa-libEGL-devel mesa-libGLES-devel libva-devel \
  ffmpeg-devel libei-devel
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode \
  -p punktfunk-host
```

`ffmpeg-devel` must be RPM Fusion's (with NVENC), not `ffmpeg-free-devel`. `mesa-libGL-devel` isn't
optional — the zero-copy GPU path links `libGL`, and without it the build fails at link time with
`cannot find -lGL`. To build an RPM instead, use the same toolchain CI does:
`docker build --build-arg FEDORA_VERSION=NN -f ci/fedora-rpm.Dockerfile -t pf-rpm ci`, then run
`packaging/rpm/build-rpm.sh` inside it.

## Arch (PKGBUILD)

The split `PKGBUILD` in `packaging/arch/` produces `punktfunk-host` and `punktfunk-client`; set
`PF_WITH_WEB=1` to also build `punktfunk-web` and `PF_WITH_SCRIPTING=1` for `punktfunk-scripting`
(both need `bun`):

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk/packaging/arch
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver   # builds the working tree, no git fetch
sudo pacman -U punktfunk-host-*.pkg.tar.zst
```

NVENC/EGL come from `nvidia-utils`; on a GPU-less builder, symlink the CUDA stub into the link path
first (the `PKGBUILD` header documents this). Packager notes, the Fedora→Arch dependency map and the
sysext mechanism: [packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md).
For a **SteamOS** host don't use the PKGBUILD — the [on-device installer](/docs/steamos-host) builds
ABI-matched to the running OS.

## Running what you built

The binary lands at `target/release/punktfunk-host`. Run it from inside your desktop session — it
auto-detects the compositor:

```sh
target/release/punktfunk-host serve              # secure native-only host
target/release/punktfunk-host serve --gamestream # + Moonlight compat (trusted LAN only)
```

To run it as a user service, copy `scripts/punktfunk-host.service` to
`~/.config/systemd/user/` (it already points at `%h/punktfunk/target/release/punktfunk-host`), then
`systemctl --user daemon-reload && systemctl --user enable --now punktfunk-host`. The other
workspace members (`punktfunk-web`, `punktfunk-scripting`, the client) build the same way — the
root [README](https://git.unom.io/unom/punktfunk#build--test-from-source) covers the dev loop.
