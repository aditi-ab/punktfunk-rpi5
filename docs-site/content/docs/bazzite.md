---
title: "Bazzite / SteamOS-like Host"
description: "Run punktfunk on Bazzite as a headless Steam host — gamescope session at the client's mode, input perms, and the gotchas."
---

Running punktfunk on **Bazzite** (Fedora Atomic / SteamOS-like) as a headless game-streaming host.
The host launches a **gamescope** session at the *client's* exact resolution + refresh, so games see
the client mode, not the box's TV. Full packaging (COPR / RPM / bootc) is in
[`packaging/bazzite/README.md`](https://github.com); this page is the operational quick-reference.

## Input permissions — use the ujust command

Gamepad + DualSense injection needs the user in the `input` group. On Bazzite you **can't** just
`usermod -aG input` (the immutable base + how the group is managed) — use the provided recipe:

```sh
ujust add-user-to-input-group
```

The udev rule (`60-punktfunk.rules`) grants access to `/dev/uinput` and `/dev/uhid`. A DualSense that
shows "detected but no input" is almost always this **host-side** `/dev/uhid`/`/dev/uinput`
permission, not a client bug — confirm the input group + the udev rule, then re-login.

## Headless Steam session at the client's mode

The host owns a `gamescope-session-plus` session and relaunches it when the client mode changes, so
games run at the client's resolution + refresh (`--nested-refresh` + a generated CVT mode). Requires
the headless-appliance prerequisites (linger + `multi-user.target`) and **no** physical gaming
session running. Configure via `host.env`:

```sh
PUNKTFUNK_COMPOSITOR=gamescope
PUNKTFUNK_GAMESCOPE_SESSION=steam   # host owns the session at the client mode
PUNKTFUNK_INPUT_BACKEND=gamescope
```

## Gotchas

- **gamescope ≥ 3.16.22 required.** Older versions deadlock capturing on PipeWire ≥ 1.6 (a loop-lock
  bug), and a wedged capture link head-blocks the whole PipeWire daemon. Never `pkill -x gamescope-wl`
  on a box where it's the live session compositor — it kills the user's session.
- **The hardware cursor isn't in the capture** (gamescope limitation; won't-fix for now).
- **HDR is blocked upstream**: gamescope's capture node is 8-bit only (PipeWire HDR export
  unimplemented), so HDR/10-bit is deferred even though the downstream encode path is ready.

## FFmpeg

Bazzite ships FFmpeg 7.x / libavcodec 61 — `ffmpeg-sys-next` auto-detects it and the host builds
against it (validated live). NVENC (`hevc_nvenc` / `av1_nvenc`) works through the system FFmpeg.
