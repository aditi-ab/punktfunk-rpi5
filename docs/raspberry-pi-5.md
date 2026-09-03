# Raspberry Pi 5 client

The RPi5 fork is a standalone Punktfunk client for 64-bit Raspberry Pi 5 Linux
systems. StreamOS uses it, but the binaries do not require StreamOS services or
libraries.

## Supported baseline

Release bundles are built natively on ARM64 Ubuntu 22.04 to keep the glibc floor
compatible with 64-bit Raspberry Pi OS Bookworm and newer Ubuntu/Debian systems.
They include `punktfunk`, `punktfunk-session`, and the matching Raspberry Pi
FFmpeg shared libraries. SDL3 and Skia are built into the client; Vulkan,
Wayland, PipeWire, DRM, input and the graphics driver come from the target OS.

The bundle expects:

- a Raspberry Pi 5 running a 64-bit kernel and userspace;
- the `rpi-hevc-dec` V4L2 Request device for hardware HEVC decoding;
- a working V3DV Vulkan driver and Wayland compositor;
- PipeWire for audio; and
- normal Linux input permissions for attached controllers.

## Install a release bundle

Download the archive and its `.sha256` file from the GitHub release. Verify it
before extracting:

```sh
sha256sum --check punktfunk-0.34.0-rpi5.1-linux-arm64.tar.gz.sha256
tar -xzf punktfunk-0.34.0-rpi5.1-linux-arm64.tar.gz
cd punktfunk-0.34.0-rpi5.1-linux-arm64
sudo ./install.sh
```

The installer places the self-contained runtime in `/opt/punktfunk-rpi5` and
creates links in `/usr/local/bin`. It does not install or modify compositor,
PipeWire, kernel, controller, or StreamOS configuration.

Use `punktfunk --help` for discovery, pairing, library, and streaming commands.
Force the Pi decoder while validating the hardware path with:

```sh
PUNKTFUNK_DECODER=v4l2-request punktfunk stream HOST
```

If the decoder is unavailable, inspect the kernel devices and logs:

```sh
grep -H . /sys/class/video4linux/video*/name
PUNKTFUNK_DECODER=v4l2-request RUST_LOG=info punktfunk stream HOST
```

## Build locally

Install the packages listed in `.github/workflows/rpi5-release.yml`, then run on
an ARM64 Linux host:

```sh
packaging/rpi5/build-release.sh v0.34.0-rpi5.1 dist
```

The script checks out the tag into a temporary tree, builds the pinned Raspberry
Pi FFmpeg fork, builds the CLI and Vulkan session renderer, applies relative
runtime paths, checks dynamic dependencies, and emits a reproducible archive plus
SHA-256 file. Building requires no StreamOS checkout.

## StreamOS consumption

StreamOS may download and verify this same archive during its image build, then
copy the extracted directory to `/opt/streamos-shell/punktfunk`. Alternatively,
it may build the tagged source with the same pinned FFmpeg revision. StreamOS
continues to own its real-time PipeWire settings, compositor policy, readiness
detection, controller forwarding, and global quit monitor.
