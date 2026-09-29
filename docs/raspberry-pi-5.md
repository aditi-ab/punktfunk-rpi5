# Raspberry Pi 5 client

The RPi5 fork is a standalone Punktfunk client for 64-bit Raspberry Pi 5 Linux
systems. Choose a complete kiosk image or a portable client bundle from
[GitHub Releases](https://github.com/aditi-ab/punktfunk-rpi5/releases/latest).

## Install the kiosk image

Download the release's `punktfunk-<release>-rpi5-kiosk.img.xz`, matching
`.img.sha256`, `.img.packages.txt`, and `.img.build.txt` files into one directory.
The checksum file covers the compressed image and both reports. Verify all files:

```sh
sha256sum --check punktfunk-*-rpi5-kiosk.img.sha256
```

Use Raspberry Pi Imager's **Use custom** option to write the `.img.xz` to your
chosen microSD card or USB drive. Writing replaces the contents of that drive.
Connect HDMI, Ethernet, and a USB controller or keyboard, then boot the Pi 5.
The image starts Punktfunk directly; pair a host from the home screen. Ethernet
uses DHCP. Bluetooth controllers can be paired from Settings after first boot.
Remote login is disabled by default and no shared login credentials are supplied.

The image contains the portable bundle built from the same release tag and commit,
a Raspberry Pi OS Lite system, Weston, the Pi graphics drivers, PipeWire, and BlueZ.
The build report records the base image, source commit, and bundle checksum.

## Supported baseline

Release bundles are built natively on ARM64 in a Debian Bookworm container to
match 64-bit Raspberry Pi OS Bookworm and remain compatible with newer
Ubuntu/Debian systems.
They include `punktfunk`, `punktfunk-session`, SDL3, and the matching Raspberry
Pi FFmpeg shared libraries. Skia is built into the client; Vulkan,
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
read -r -p 'Archive filename: ' archive
sha256sum --check "${archive}.sha256"
tar -xzf "$archive"
cd "${archive%.tar.gz}"
sudo ./install.sh
```

The installer places the self-contained runtime in `/opt/punktfunk-rpi5` and
creates links in `/usr/local/bin`. It does not install or modify compositor,
PipeWire, kernel, or controller configuration.

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

See [fork maintenance](rpi5-fork.md) for the ARM64 release bundle, complete image
builder, verification steps, and tag-based GitHub release workflow. The bundle and
kiosk image are built entirely from this repository.
