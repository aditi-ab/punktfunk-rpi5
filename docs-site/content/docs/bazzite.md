---
title: Bazzite — gamescope
description: Set up a punktfunk host on Bazzite, streaming a Steam/gamescope session at your client's mode.
---

[Bazzite](https://bazzite.gg/) already ships everything a punktfunk host needs — the NVIDIA driver,
NVENC, PipeWire, and **gamescope**. So a Bazzite host is the most "appliance-like" setup: the host
launches its own gamescope session at the **client's** resolution and refresh, so your games run at
the mode of the device you're streaming to, not the TV the box is plugged into.

> This is ideal for a dedicated game-streaming box. For a general desktop, prefer
> [Ubuntu/Fedora KDE](/docs/ubuntu-kde) or [GNOME](/docs/ubuntu-gnome).

## Install

The host installs from the punktfunk COPR repository (see `packaging/bazzite/` in the repo for the
exact COPR/RPM/bootc options). You can also build from source as on
[Fedora KDE](/docs/fedora-kde) — Bazzite is Fedora Atomic underneath, and its FFmpeg builds the host
fine.

## Allow controller input

Gamepad and DualSense input needs your user in the `input` group. On Bazzite, don't use
`usermod` — the base is immutable and the group is managed by a recipe. Use:

```sh
ujust add-user-to-input-group
```

Then **log out and back in**. (A controller that's "detected but does nothing" is almost always this
permission, not a client problem.)

## Configure

Point the host at the gamescope backend in `~/.config/punktfunk/host.env`:

```sh
PUNKTFUNK_COMPOSITOR=gamescope
PUNKTFUNK_GAMESCOPE_SESSION=steam   # the host owns a Steam session at the client's mode
PUNKTFUNK_INPUT_BACKEND=gamescope
PUNKTFUNK_ZEROCOPY=1
```

With this, when a client connects the host starts a `gamescope-session-plus` (Steam) session at the
client's exact resolution and refresh, and relaunches it if the client changes mode. There should be
**no physical gaming session already running** on the box.

## Run as an always-on host

Bazzite hosts are typically headless. Enable the host service and linger so it starts at boot — see
[Running as a Service](/docs/running-as-a-service). Because the host launches its own gamescope
session per client, you don't need a separate desktop-session unit.

## Good to know

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  current gamescope is fine; this only bites if you've pinned an old one.
- **The mouse cursor isn't included in the captured image** — a gamescope limitation for now.
- **HDR isn't supported yet** on the gamescope path — gamescope's capture output is 8-bit. SDR streams
  normally.

Then [connect a client](/docs/clients) — Moonlight works great for couch gaming, and the Apple app for
Apple TV / iPad.
