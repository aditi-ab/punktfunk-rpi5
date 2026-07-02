# punktfunk on Arch Linux / SteamOS

Packaging for punktfunk on Arch and Arch-derived immutable distros. The `PKGBUILD` is a **split
package** producing **`punktfunk-host`** (the gaming-rig host) and **`punktfunk-client`** (the GTK4
couch/Deck client) — mirrors the rpm subpackages (`packaging/rpm/punktfunk.spec`) and the deb build
scripts. On a **Steam Deck used as a client you want `punktfunk-client`** (it's what the
[Decky plugin](../../clients/decky/) launches); on a gaming rig, `punktfunk-host`.

> **Steam Deck as a HOST:** don't use this PKGBUILD — SteamOS's read-only root makes `makepkg`/sysext
> awkward, and a prebuilt binary breaks on OS library bumps. Use the on-device build script instead:
> **[`scripts/steamdeck/install.sh`](../../scripts/steamdeck/)** (it builds in a Debian-trixie distrobox
> ABI-matched to SteamOS and uses **VAAPI** on the Deck's AMD GPU). The Deck host path is the one
> exception to "host encode is NVENC-only" below.

A third member, **`punktfunk-web`** (the browser management console — pairing + status), is
**opt-in**: build it by setting `PF_WITH_WEB=1`, which requires **`bun`** at build time (`bun-bin`
from the AUR if it isn't in your repos). bun is also the **runtime** — the console serves HTTPS
(HTTP/1.1 over TLS) via `Bun.serve`, so the package vendors the bun binary (no `nodejs` dependency). A
default `makepkg` builds only host+client with no JS tooling — mirroring the RPM spec's `%bcond_with web`.

> **Host encode: NVENC on NVIDIA, VAAPI on AMD/Intel** (`PUNKTFUNK_ENCODER=auto` picks one). The host
> now has a VAAPI encoder + zero-copy dmabuf path alongside NVENC/CUDA, so `punktfunk-host` works on
> Arch + NVIDIA **and** AMD/Intel (incl. the Steam Deck — see the on-device path above). The client
> decodes via VAAPI on AMD/Intel with a software fallback.

## Arch Linux (mutable)

```sh
cd packaging/arch
# Build the working tree (CI / dev) — no git fetch:
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
# …or build the tagged release the AUR way:
makepkg -si
# …add the web console too (needs bun / bun-bin):
PF_WITH_WEB=1 PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
```
Then the standard first-run (printed by the install scriptlet):
```sh
sudo usermod -aG input "$USER"          # virtual gamepads; re-login after
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env   # gamescope backend
systemctl --user enable --now punktfunk-host
# Web console (if you installed the punktfunk-web package): enable it + read the login password.
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'   # open https://<host-ip>:47992
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

## Steam Deck — the client (what the Decky plugin launches)

To stream *to* a Deck, you install **`punktfunk-client`** there — same sysext mechanism, but
wrapping the client package instead. The split `makepkg` produces both `.pkg.tar.zst` files; on the
Deck use the client one:
```sh
cd packaging/arch && PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
bash build-sysext.sh punktfunk-client-*.pkg.tar.zst        # → punktfunk-client.raw
# on the Deck:
sudo cp punktfunk-client.raw /var/lib/extensions/
sudo systemctl enable --now systemd-sysext
sudo pacman -S --needed libva-mesa-driver                  # VAAPI hw decode on the Deck's AMD APU
```
Now `punktfunk-client` is on `PATH`, so the **[Decky plugin](../../clients/decky/)** finds and
launches it (`punktfunk-client --connect host:port`) — gamescope composites its video like a game.
The client needs no `/dev/uinput` or compositor-spawning rights (it captures input and decodes),
so it's a much lighter sysext than the host.

## Firewall

If the host box runs a firewall, open the ports it listens on. The **native `punktfunk/1`** plane:

- **QUIC control plane: UDP 9777** (`serve --native-port N` to change).
- **Data plane: an *ephemeral* UDP port** — negotiated per session, so there is no fixed port to
  open. For a restrictive firewall you'd need to allow a UDP range (the repo does not pin one).

And the **GameStream / Moonlight** ports (fixed) — only needed if you run the host with
`serve --gamestream` (opt-in, trusted LAN only); bare `serve` is native-only and doesn't open these:

| Port | Proto | Purpose |
|---|---|---|
| 47984 | TCP | HTTPS nvhttp (paired, mutual-TLS) |
| 47989 | TCP | HTTP nvhttp (`/serverinfo`, `/pair` PIN flow) |
| 48010 | TCP | RTSP handshake |
| 47998–48010 | UDP | Video RTP (+ FEC), ENet control (47999), audio (48000) |
| 5353 | UDP | mDNS auto-discovery |

The mgmt API (TCP 47990) binds to loopback by default — leave it closed unless you move it off
loopback with `--mgmt-bind IP:PORT` (which then requires `--mgmt-token`).

With `ufw`:

```sh
sudo ufw allow 9777/udp                                 # punktfunk/1 control plane
sudo ufw allow 47984/tcp && sudo ufw allow 47989/tcp && sudo ufw allow 48010/tcp
sudo ufw allow 47998:48010/udp
sudo ufw allow 5353/udp
# plus the ephemeral punktfunk/1 data port — open a UDP range you reserve for it.
```

With raw `nftables` (add to your `inet filter input` chain):

```
udp dport 9777 accept                  # punktfunk/1 control plane
tcp dport { 47984, 47989, 48010 } accept
udp dport { 47998-48010, 5353 } accept
# plus the ephemeral punktfunk/1 data port (a reserved UDP range).
```

## Files
- `PKGBUILD` — split package: `punktfunk-host` + `punktfunk-client` (builds the working tree via
  `PF_SRCDIR`, or a git tag for AUR).
- `punktfunk-host.install` / `punktfunk-client.install` — pacman scriptlets (udev reload + sysctl +
  first-run hint), mirror the RPM `%post` / deb postinst.
- `build-sysext.sh` — wraps either built `.pkg.tar.zst` into a `systemd-sysext` `.raw` for SteamOS
  (derives the name from the package, so it works for host or client).
