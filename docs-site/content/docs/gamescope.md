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

There are two mutually-exclusive models for a gamescope box; pick one. With **nothing set**, a box
that has gamescope session infrastructure (Bazzite, SteamOS, Nobara) gets **managed**; the
[Bazzite template](/docs/bazzite) ships with **attach** chosen instead.

- **Attach** (`PUNKTFUNK_GAMESCOPE_ATTACH=1`) — the **box** owns its gamescope session and decides
  Gaming vs Desktop via the normal Steam UI. Game Mode stays on the box's own (physical) display;
  the host attaches to whatever's live and never tears it down, so switching Desktop ↔ Game is
  rock-solid and disconnecting leaves the box where it was. When the session is the box's own
  autologin unit, the host restarts it at the **client's** resolution on a mismatch; a foreign or
  bare gamescope is streamed at its own mode.
- **Managed** (the infra-detected default; force with `PUNKTFUNK_GAMESCOPE_MANAGED=1`) — the host
  takes the box's gamescope session over and relaunches it **headless** at the *client's* exact
  resolution and refresh — Game Mode runs on the virtual screen, physical displays drop out of it —
  restoring the box on idle after disconnect.

## Session following

`PUNKTFUNK_SESSION_WATCH` follows a Gaming ↔ Desktop switch **mid-stream** — the host rebuilds the
backend in place, with no reconnect. It is **on by default** on Bazzite/SteamOS; set `0` to disable.
One host service covers both faces of the box: it streams Gaming Mode over gamescope and the desktop
over its own compositor, and re-targets whichever is live on each switch.

## Start the host

On an appliance box (Bazzite, SteamOS) the install guide already enables the host service for you. On
any other distro running a gamescope session, just start it — the host auto-detects the live
gamescope session and picks the model for it:

```sh
systemctl --user enable --now punktfunk-host
```

Then bring up [The Web Console](/docs/web-console) to arm pairing.

## gamescope knobs

The gamescope-specific settings in `host.env`. Leave them unset to auto-detect; set one only to force
a model. See the full [Configuration reference](/docs/configuration) for every other knob.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` | **Attach** model: the box owns its gamescope session (on its own display); the host captures whatever's live and never tears it down. A box-owned autologin session is restarted at the client's resolution on a mismatch; a foreign/bare gamescope streams at its own mode. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model (the default where session infra is detected): the host takes the box's gamescope over and relaunches it headless at the client's exact mode, restoring on idle. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode — a headless appliance with no physical session running. |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover and capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch mid-stream (rebuild in place, no reconnect). On by default on Bazzite/SteamOS; set `0` to disable. |

## Known limits

These apply to the **Gaming Mode (gamescope)** path only; the desktop path is unaffected.

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  and SteamOS's current gamescope is fine; this only bites if you've pinned an old one.
- **The mouse cursor isn't included in the captured image** — a gamescope limitation for now.
- **Touch arrives as a single-finger pointer.** gamescope's virtual input device has no
  touchscreen, so the host maps a client's touchscreen to an absolute pointer: taps click exactly
  where you touch and drags work, but multi-touch gestures (pinch) aren't available in Gaming
  Mode. The desktop path has full multi-touch.
- **HDR isn't supported on the gamescope path** — gamescope's capture output is 8-bit. SDR streams
  normally.

To stream the KDE Plasma desktop of a Steam box instead, see [KDE Plasma](/docs/kde). To bring up the
web console and pair a client, see [The Web Console](/docs/web-console).
