---
title: Arch Linux
description: Install a punktfunk host on Arch (and Arch-derived distros) from the signed pacman binary repo.
---

Set up a punktfunk host on **Arch Linux** (or an Arch-derived distro like CachyOS/EndeavourOS). The
host installs from a **signed pacman binary repo**, so it updates with `pacman -Syu` like the rest
of your system — no building required. Host encode is **NVENC on NVIDIA** and **VAAPI on
AMD/Intel** (`PUNKTFUNK_ENCODER=auto` picks per GPU).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

> Prefer to build it yourself? A split `PKGBUILD` (host + client + optional web console) is in the
> repo at `packaging/arch/` — see the [appendix](#appendix--build-from-source-pkgbuild). The binary
> repo below is the supported path.

## 1. GPU prerequisites

- **NVIDIA:** `sudo pacman -S --needed nvidia-utils` (provides NVENC + the EGL/CUDA zero-copy path).
  Arch's stock `ffmpeg` already has NVENC built in — no RPM-Fusion-style swap like Fedora needs.
- **AMD / Intel:** the Mesa stack (`mesa`, `libva-mesa-driver` for AMD, `intel-media-driver` for
  Intel) provides the VAAPI encoder — usually already installed on a desktop.

## 2. Add the signed repo

The registry **signs its database and every package**, so first trust its key once (after this,
packages install signature-verified):

```sh
# Trust the registry signing key.
curl -fsS https://git.unom.io/api/packages/unom/arch/repository.key \
  | sudo pacman-key --add -
sudo pacman-key --lsign-key E0CA04465C99C936E0B0C6510A317015A34DDD69

# Add the repo (append to /etc/pacman.conf). No SigLevel line needed — pacman's default
# verifies signed packages against the key you just trusted. (printf, not a heredoc, so this
# works in fish too — CachyOS's default shell has no `<<EOF` support.)
printf '\n[punktfunk]\nServer = https://git.unom.io/api/packages/unom/arch/$repo/$arch\n' \
  | sudo tee -a /etc/pacman.conf >/dev/null
```

> **Stable vs canary.** `[punktfunk]` is the **stable** channel — it moves only when a `vX.Y.Z`
> release is cut. For the latest `main` build, use `[punktfunk-canary]` instead (same `Server` line,
> just the repo name). Enable exactly one. See [Release Channels](/docs/channels).

## 3. Install the host

```sh
sudo pacman -Sy punktfunk-host      # the streaming host
sudo pacman -S  punktfunk-web       # optional: the browser management console (pairing + status)
sudo usermod -aG input "$USER"      # /dev/uinput access for virtual gamepads (re-login to apply)
```

`punktfunk-client` (the native GTK4 Linux client) is in the same repo if this box is also a client.
The host package ships the systemd **user** units, the udev rule, the UDP socket-buffer sysctl
tuning, and example configs. Updates later are just `sudo pacman -Syu`.

## 4. Configure and run

The host runs as a systemd **`--user`** service — it needs your session's PipeWire and D-Bus. Copy a
starting config:

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.example ~/.config/punktfunk/host.env
```

How the host creates its virtual display and injects input depends on your desktop, not your distro —
edit `host.env` for the desktop you run, following its page for the exact settings and any quirks:

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Sway / wlroots](/docs/sway)

Then enable the service and turn on linger so it starts at boot without a login:

```sh
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-host
sudo loginctl enable-linger "$USER"
```

Check it came up:

```sh
systemctl --user status punktfunk-host          # active
journalctl --user -u punktfunk-host -f          # watch a client connect
```

Enable the browser console, find your login password, and arm PIN pairing from
[The Web Console](/docs/web-console). For a headless KWin appliance that streams at boot with no
graphical login, see [KDE → Headless session](/docs/kde#headless-session). Full reference:
[Configuration](/docs/configuration) · [Running as a Service](/docs/running-as-a-service).

## 5. Open the firewall (if you have one)

**Stock Arch ships no firewall** — every port is already open, so you can skip this. But **CachyOS
enables `ufw` by default** (firewalld is not installed), and some other spins (e.g. EndeavourOS)
enable **`firewalld`** — an Arch package never opens ports for you, so on those the host is
unreachable until you allow it.

The `punktfunk-host` package installs openers for **both**, so it's a one-liner whichever you run:

```sh
# ufw — CachyOS (and Ubuntu, once you enable ufw):
sudo ufw allow punktfunk-native        # the secure native host (the default)
sudo ufw allow punktfunk-gamestream    # …also this if you run `serve --gamestream` (Moonlight)

# firewalld — Fedora-like spins (EndeavourOS, …):
sudo firewall-cmd --reload                                    # load the installed definition
sudo firewall-cmd --permanent --add-service=punktfunk-native
sudo firewall-cmd --reload
```

`punktfunk-native` opens the QUIC control port (UDP 9777) + mDNS discovery; add
`punktfunk-gamestream` as well if you run `serve --gamestream` (the fixed Moonlight ports + mDNS).
The media **data plane** uses an *ephemeral* UDP port that the client opens with a hole-punch — the
host streams back out through the path the client opened, so there's **nothing fixed to open** as
long as the firewall allows outbound UDP (the default for both ufw and firewalld).

Enabled the **web console** (`punktfunk-web`, above) and want to reach it from your phone or another
machine? It's not opened by the streaming rules — open its port too, the same one-liner way:

```sh
sudo ufw allow punktfunk-web                                                            # ufw
sudo firewall-cmd --permanent --add-service=punktfunk-web && sudo firewall-cmd --reload  # firewalld
```

That opens **TCP 47992** (HTTPS, login-gated). The mgmt API (47990) stays loopback-only and is never
opened. Full port lists (`nftables`, explicit ports) are in
[`packaging/arch/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md#firewall).

## 6. Connect a client

From any [client](/docs/clients), `--discover` finds the host on the LAN. On first connect, complete
the **PIN pairing**: arm it from [The Web Console](/docs/web-console#arm-pairing), which displays a
4-digit PIN to type into the client. (Pairing is required by default; pass `serve --open` only if
you deliberately want to disable it.) See [Clients](/docs/clients) for per-platform setup.

## Appendix — build from source (PKGBUILD)

To build instead of using the binary repo, use the split `PKGBUILD` in `packaging/arch/` (produces
`punktfunk-host` + `punktfunk-client`; set `PF_WITH_WEB=1` to also build `punktfunk-web`, which needs
`bun`):

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk/packaging/arch
# Build the working tree (no git fetch):
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
sudo pacman -U punktfunk-host-*.pkg.tar.zst
```

NVENC/EGL come from the NVIDIA driver (`nvidia-utils`); on a GPU-less builder, symlink the CUDA
stub into the link path first (the `PKGBUILD` header documents this). Full details, the
Fedora→Arch dependency map, and the SteamOS systemd-sysext path are in
[`packaging/arch/README.md`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md).
