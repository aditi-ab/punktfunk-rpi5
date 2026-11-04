# Punktfunk for Raspberry Pi 5

This standalone ARM64 bundle targets 64-bit Raspberry Pi OS Trixie on
Raspberry Pi 5. A complete bootable
kiosk image is also available from the matching GitHub release.

On a kiosk with the updater installed, use **Settings > Raspberry Pi > Punktfunk
updates**. It installs this app bundle without changing the OS or your settings.
The manual installer below is for installations without the kiosk updater.

Install the system runtime first:

```sh
sudo apt update
sudo apt install libasound2 libdbus-1-3 libdrm2 libgbm1 libpipewire-0.3-0 \
  libudev1 libvulkan1 libwayland-client0 libxkbcommon0 mesa-vulkan-drivers \
  pipewire wireplumber
```

Run in place or install under `/opt/punktfunk-rpi5`:

```sh
./punktfunk --help
sudo ./install.sh
punktfunk --help
```

The executables find the bundled Raspberry Pi FFmpeg libraries through their
relative runtime path. Do not move either executable away from the adjacent
`lib` directory unless you preserve that layout.

For an explicit HEVC hardware-decoder check:

```sh
PUNKTFUNK_DECODER=v4l2-request punktfunk stream HOST
```

The operating system must expose the Raspberry Pi HEVC V4L2 Request decoder.
Check `/sys/class/video4linux/video*/name` for `rpi-hevc-dec`. Vulkan, Wayland,
PipeWire and controller configuration remain responsibilities of the host Linux
distribution. See the repository's `docs/raspberry-pi-5.md` for setup and
troubleshooting guidance.
