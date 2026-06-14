# punktfunk on Arch Linux / SteamOS

Packaging for the punktfunk streaming **host** on Arch and Arch-derived immutable distros
(SteamOS 3, etc.). Mirrors the artifact set of `packaging/rpm/punktfunk.spec` and
`packaging/debian/build-deb.sh`.

> ⚠️ **Encode is NVENC-only today.** `crates/punktfunk-host/src/encode/linux.rs` implements
> `hevc_nvenc`/`av1_nvenc`/`h264_nvenc` and a CUDA zero-copy path — there is **no VAAPI backend**.
> So this package is functional on **Arch + NVIDIA** (the realistic target). On an **AMD Steam
> Deck it installs but cannot encode** until a `hevc_vaapi`/`av1_vaapi` encoder is added in
> `src/encode/` — a code change, not a packaging one. (`bazzite-deck-nvidia`, i.e. SteamOS-style
> images running on NVIDIA hardware, work fine — they're NVIDIA.)

## Arch Linux (mutable)

```sh
cd packaging/arch
# Build the working tree (CI / dev) — no git fetch:
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
# …or build the tagged release the AUR way:
makepkg -si
```
Then the standard first-run (printed by the install scriptlet):
```sh
sudo usermod -aG input "$USER"          # virtual gamepads; re-login after
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env   # gamescope backend
systemctl --user enable --now punktfunk-host
```
NVENC/EGL come from the NVIDIA driver: `sudo pacman -S --needed nvidia-utils`. Arch's stock
`ffmpeg` already has NVENC built in — no RPM-Fusion-style swap needed (unlike Fedora).

### Runtime dependency map (Fedora/Debian → Arch)

| Need | Arch package |
|------|--------------|
| FFmpeg + NVENC | `ffmpeg` (NVENC built in) |
| PipeWire + Pulse + session mgr | `pipewire` `pipewire-pulse` `wireplumber` |
| Opus / input injection | `opus` `libei` |
| GL/EGL + gbm + xkb + wayland | `libglvnd` `mesa` `libxkbcommon` `wayland` |
| NVIDIA driver (NVENC/EGL/CUDA) | `nvidia-utils` *(optdepend — never a hard dep)* |
| Compositor backends | `gamescope` (≥3.16.22) / `kwin` / `mutter` / `sway` *(optdepends)* |

## SteamOS 3 (immutable) — use a systemd-sysext

SteamOS has a **read-only `/usr` on A/B partitions**, and every OS update reimages the rootfs —
so `steamos-readonly disable` + `pacman` (and flatpak/distrobox) are fragile or unusable for a
host that needs `/dev/uinput`, `/dev/uhid`, the host PipeWire socket, the GPU render node, and the
right to spawn a compositor. The update-survivable, SteamOS-blessed mechanism is a
**systemd-sysext**: an overlay image merged read-only over `/usr` at boot, living in the writable
`/var/lib/extensions/` (so it persists across A/B updates, no readonly-disable).

Build the package, then wrap its `/usr` payload into a sysext image:
```sh
# 1. build the pacman package (needs an Arch environment / container)
cd packaging/arch && PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
# 2. turn it into a sysext .raw (extracts the package's /usr into an image + extension-release)
bash build-sysext.sh punktfunk-host-*.pkg.tar.zst
# 3. on the SteamOS box:
sudo cp punktfunk-host.raw /var/lib/extensions/
sudo systemctl enable --now systemd-sysext      # merges it; survives OS updates
systemctl --user enable --now punktfunk-host     # the user unit is now under /usr/lib
```
The udev rule, sysctl, and systemd **user** unit all live under `/usr/lib`, so the merged sysext
exposes them. `systemd-sysext refresh` re-merges after a reboot.

## Files
- `PKGBUILD` — the package recipe (builds the working tree via `PF_SRCDIR`, or a git tag for AUR).
- `punktfunk-host.install` — pacman scriptlet (udev reload + sysctl + first-run hint), mirrors RPM `%post`.
- `build-sysext.sh` — wraps a built `.pkg.tar.zst` into a `systemd-sysext` `.raw` for SteamOS.
