---
title: Steam / gamescope
description: Configure a gamescope/Steam host — attach vs managed, session following, and limits.
---

gamescope is the compositor behind Steam **Gaming Mode** — the couch/handheld game UI on Bazzite,
SteamOS, or any distro running a gamescope session. The host **auto-detects** gamescope from your
live session, so you rarely need to set anything here. It also **follows a Gaming ↔ Desktop switch
mid-stream** — flip between Gaming Mode and the desktop with Steam's normal UI and the host
re-targets whatever's running without a reconnect.

This page covers the gamescope-specific choices. To get a host running on an appliance box, start
from the install guide for your OS: [Bazzite](/docs/bazzite) or [SteamOS (Host)](/docs/steamos-host).

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## Attach vs managed

There are two mutually-exclusive models for a gamescope box; pick one. The shipped default is
**attach**.

- **Attach** (`PUNKTFUNK_GAMESCOPE_ATTACH=1`, the default) — the **box** owns its gamescope session
  and decides Gaming vs Desktop via the normal Steam UI. The host just attaches to whatever's live
  and never tears it down, so switching Desktop ↔ Game is rock-solid and disconnecting leaves the box
  where it was. The streamed game-mode resolution is the box's gamescope mode
  (`SCREEN_WIDTH/HEIGHT` in `/etc/gamescope-session-plus/sessions.d/steam`), not the client's.
- **Managed** (`PUNKTFUNK_GAMESCOPE_MANAGED=1`, and remove the attach line) — the host tears the
  box's gamescope down on connect and launches its **own** at the *client's* exact resolution and
  refresh, restoring on idle. Client-mode-following, but it can't coexist with a box-owned game-mode
  session, and there must be **no physical gaming session already running**.

## Session following

`PUNKTFUNK_SESSION_WATCH` follows a Gaming ↔ Desktop switch **mid-stream** — the host rebuilds the
backend in place, with no reconnect. It is **on by default** on Bazzite/SteamOS; set `0` to disable.
One host service covers both faces of the box: it streams Gaming Mode over gamescope and the desktop
over its own compositor, and re-targets whichever is live on each switch.

## Start the host

On an appliance box (Bazzite, SteamOS) the install guide already enables the host service for you. On
any other distro running a gamescope session, start it from your session — the default attach model
just latches onto whatever gamescope session is live:

```sh
systemctl --user enable --now punktfunk-host
```

Then bring up [The Web Console](/docs/web-console) to arm pairing.

## gamescope knobs

The gamescope-specific settings in `host.env`. Leave them unset to auto-detect; set one only to force
a model. See the full [Configuration reference](/docs/configuration) for every other knob.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` | **Attach** model: the box owns its gamescope session; the host captures whatever's live and never tears it down. Streamed resolution is the box's gamescope mode. The default. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model: the host tears the box's gamescope down on connect and launches its own at the client's exact mode, restoring on idle. Doesn't coexist with a box-owned game-mode session. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode — a headless appliance with no physical session running. |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover and capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch mid-stream (rebuild in place, no reconnect). On by default on Bazzite/SteamOS; set `0` to disable. |

## Known limits

These apply to the **Gaming Mode (gamescope)** path only; the desktop path is unaffected.

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  and SteamOS's current gamescope is fine; this only bites if you've pinned an old one.
- **The mouse cursor isn't included in the captured image** — a gamescope limitation for now.
- **HDR isn't supported on the gamescope path** — gamescope's capture output is 8-bit. SDR streams
  normally.

To stream the KDE Plasma desktop of a Steam box instead, see [KDE Plasma](/docs/kde). To bring up the
web console and pair a client, see [The Web Console](/docs/web-console).
