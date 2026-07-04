---
title: Bazzite
description: Set up a punktfunk host on Bazzite — it follows the box between Steam Gaming Mode (gamescope) and the KDE Plasma desktop automatically.
---

[Bazzite](https://bazzite.gg/) already ships everything a punktfunk host needs — the NVIDIA driver,
NVENC, PipeWire, **gamescope**, and the **KDE Plasma desktop**. So a Bazzite host is the most
"appliance-like" setup, and it streams **both** of Bazzite's faces:

- **Steam Gaming Mode** (gamescope) — the couch/handheld game UI.
- **The KDE Plasma desktop** — the full desktop you get from "Switch to Desktop".

The host **auto-detects which one is live and follows the box across the switch** — including
mid-stream. You flip between Gaming Mode and Desktop with Bazzite's normal Steam UI /
"Switch to Desktop"; the host just re-targets whatever's running and keeps streaming. Nothing in
`host.env` forces a mode.

> Ideal for a dedicated game-streaming box that you also occasionally want as a remote desktop. For a
> pure desktop machine, [Ubuntu/Fedora KDE](/docs/ubuntu-kde) or [GNOME](/docs/ubuntu-gnome) are
> simpler.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## Install

The host installs as a **systemd system extension (sysext)** — no `rpm-ostree` layering. The
Bazzite docs treat layering as a last resort (layered packages slow every OS update and can block
upgrades until removed); a sysext never enters an rpm-ostree transaction: it overlays `/usr`
read-only from `/var/lib/extensions/`, survives OS updates, installs and updates **without a
reboot**, and is removable in one command. This is the same mechanism the Fedora Atomic
maintainers ship via the [fedora-sysexts](https://fedora-sysexts.github.io/) project.

```sh
# One-time bootstrap (afterwards the updater is on PATH as `punktfunk-sysext`):
curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh
sudo bash punktfunk-sysext.sh install          # add `--channel canary` for rolling builds
```

That downloads the newest image (host + tray + web console, SHA-256-verified over HTTPS from
punktfunk's package registry), merges it, and applies the udev/sysctl setup on the spot — the
host is usable immediately, no reboot. From then on:

```sh
sudo punktfunk-sysext update     # fetch + merge the newest build
sudo punktfunk-sysext status     # channel, installed vs latest version
sudo punktfunk-sysext remove     # unmerge and delete — the box is back to stock
```

Two things to know:

- **After a Bazzite major rebase** (Fedora 43 → 44) the old image **refuses to load** rather than
  run against mismatched system libraries — run `sudo punktfunk-sysext update` once and it fetches
  the image built for the new base.
- **Already layering punktfunk?** Install the sysext (it shadows the layered copy immediately),
  then drop the layer so it stops slowing your updates:
  `sudo rpm-ostree uninstall punktfunk punktfunk-web && systemctl reboot`.

For a fully baked appliance image there's also a **bootc** Containerfile that installs the RPMs
from the registry at image-build time — see `packaging/bootc/` in the repo. Plain `rpm-ostree`
layering from the [RPM registry](https://git.unom.io/unom/-/packages) keeps working too (see
`packaging/bazzite/README.md`), but the sysext is the supported default. Building from source
also works (Bazzite is Fedora Atomic underneath — same steps as [Fedora KDE](/docs/fedora-kde)).

## Allow controller input

Gamepad and DualSense input needs your user in the `input` group. On Bazzite, don't use
`usermod` — the base is immutable and the group is managed by a recipe. Use:

```sh
ujust add-user-to-input-group
```

Then **log out and back in**. (A controller that's "detected but does nothing" is almost always this
permission, not a client problem.)

## Configure

The RPM ships a Bazzite-tuned config you can copy as your starting point:

```sh
mkdir -p ~/.config/punktfunk
cp /usr/share/punktfunk/host.env.bazzite ~/.config/punktfunk/host.env
```

The template is deliberately minimal — it does **not** force a compositor, because the host
auto-detects Gaming Mode (gamescope) vs Desktop (KWin) on every connect and follows the switch
mid-stream. The only settings that matter are the session anchors plus zero-copy:

```sh
XDG_RUNTIME_DIR=/run/user/1000
DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1            # GPU zero-copy (dmabuf → CUDA → NVENC); auto-falls back to CPU
PUNKTFUNK_GAMESCOPE_ATTACH=1    # Gaming Mode = attach to the box's own session (see below)
```

### Gaming Mode: attach vs managed

For Gaming Mode there are two models (pick one; the shipped default is **attach**):

- **Attach** (`PUNKTFUNK_GAMESCOPE_ATTACH=1`, the default) — the **box** owns its gamescope session
  and decides Gaming vs Desktop via the normal Steam UI. The host just attaches to whatever's live
  and never tears it down, so switching Desktop ↔ Game is rock-solid and disconnecting leaves the box
  where it was. The streamed game-mode resolution is the box's gamescope mode
  (`SCREEN_WIDTH/HEIGHT` in `/etc/gamescope-session-plus/sessions.d/steam`), not the client's.
- **Managed** (`PUNKTFUNK_GAMESCOPE_MANAGED=1`, and remove the attach line) — the host tears the
  box's gamescope down on connect and launches its **own** at the *client's* exact resolution and
  refresh, restoring on idle. Client-mode-following, but it can't coexist with a box-owned game-mode
  session, and there must be **no physical gaming session already running**.

Mid-stream Gaming ↔ Desktop following (`PUNKTFUNK_SESSION_WATCH`) is **on by default** on
Bazzite/SteamOS. See [Configuration](/docs/configuration) for the full list of knobs.

### Streaming the KDE Plasma desktop

The **virtual output** (video) for the Desktop session needs no config — the host package ships an
`io.unom.Punktfunk.Host.desktop` file whose `X-KDE-Wayland-Interfaces` grants the host KWin's
restricted screencast protocol on a normal interactive Plasma session (least-privilege, the same
mechanism krfb/krdp use). After a **fresh host install, log out and back into the Desktop session
once** so KWin re-reads that grant.

The one thing a normal KDE login lacks is the RemoteDesktop grant for headless **input** injection.
Seed it once (as the streaming user, no root) so the host auto-approves instead of popping an
un-answerable dialog:

```sh
bash /usr/share/punktfunk/bazzite/kde-desktop-setup.sh
```

Gaming Mode needs none of this — it auto-attaches.

## Run as an always-on host

Bazzite hosts are typically headless. Enable the host service and linger so it starts at boot — see
[Running as a Service](/docs/running-as-a-service). One host service covers both Gaming Mode and the
Desktop; it follows whichever the box is in.

```sh
systemctl --user enable --now punktfunk-host
# Web console (pairing + status) — enable it and read the auto-generated login password,
# then open http://<host-ip>:47992:
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
```

### Console login password

The console is password-protected. On first start `punktfunk-web-init` generates a random login
password and saves it to `~/.config/punktfunk/web-password` (as `PUNKTFUNK_UI_PASSWORD=…`). Read it
back at any time — from the init service's journal, or straight from the file:

```sh
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
```

To set your own password, edit that file (`PUNKTFUNK_UI_PASSWORD=<your-password>`) and restart the
console: `systemctl --user restart punktfunk-web`. Forgot it? This is the recovery path linked from
the console login screen — see [Forgot your Password?](/docs/forgot-password).

## Good to know

These apply to the **Gaming Mode (gamescope)** path; the KDE Desktop path is unaffected:

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  current gamescope is fine; this only bites if you've pinned an old one.
- **The mouse cursor isn't included in the captured image** — a gamescope limitation for now. (The
  KDE Desktop path renders the cursor normally.)
- **HDR isn't supported yet** on the gamescope path — gamescope's capture output is 8-bit. SDR streams
  normally.

Then [connect a client](/docs/clients) — Moonlight works great for couch gaming, and the Apple app for
Apple TV / iPad.
