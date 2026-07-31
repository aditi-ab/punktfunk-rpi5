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
  rock-solid and disconnecting leaves the box where it was. When the box is **headless** (no
  display connected) and the session is its own autologin unit, the host restarts it at the
  **client's** resolution on a mismatch; a box driving a physical display — and any foreign or
  bare gamescope — is streamed at its own mode.
- **Managed** (the infra-detected default; force with `PUNKTFUNK_GAMESCOPE_MANAGED=1`) — the host
  takes the box's gamescope session over and relaunches it **headless** at the *client's* exact
  resolution and refresh — Game Mode runs on the virtual screen, physical displays drop out of it —
  restoring the box on idle after disconnect.

### Nobara and other autologin display managers

The managed takeover has to stop the box's Gaming Mode session to free Steam. How it does that
depends on the display manager driving the autologin:

- **SDDM** (Bazzite, SteamOS): handled automatically — no setup.
- **plasmalogin** (Nobara) and other display managers: the host must stop the display manager
  itself for the length of the stream and restart it afterwards, which needs privilege. The
  packages ship that privilege: a root helper (`/usr/libexec/punktfunk/pf-dm-helper`) behind its
  own polkit action (`io.unom.punktfunk.dm-helper`), invoked automatically when the plain
  `systemctl` verbs are denied — no setup. The helper only stops/restores the unit the
  `display-manager.service` symlink points at, the same class of local-seat operation these
  distros already authorize for their own session switcher (Nobara's `os-session-select`).

  Installed from a tarball, or prefer not to ship the `allow_any` action? Remove the `.policy`
  file and use a polkit rule scoped to your user instead (adjust the unit and user names to your
  box) — the host tries the plain verbs first, so the rule takes precedence:

  ```js
  // /etc/polkit-1/rules.d/49-punktfunk-dm.rules
  polkit.addRule(function(action, subject) {
      if (action.id == "org.freedesktop.systemd1.manage-units" &&
          action.lookup("unit") == "plasmalogin.service" &&
          subject.user == "YOUR_USER") {
          return polkit.Result.YES;
      }
  });
  ```

  With no privilege path at all the host degrades safely: it **attaches** to the live Gaming Mode
  session instead (Game Mode stays on the box's display at the box's own resolution, mirrored to
  the client — if your monitor stays on and the stream runs at the desktop's resolution, this is
  what happened; check the host log for "managed takeover unavailable"). If the display-manager
  restart ever loses its privilege mid-restore, `PUNKTFUNK_RECOVER_SESSION_CMD` (see
  [Configuration](/docs/configuration)) is fired as the fallback.

  **Lingering is required here**, and the host turns it on for you the first time it takes the box
  over. Stopping the display manager ends your last login session, and without
  `loginctl enable-linger` logind stops your `systemd --user` manager about ten seconds later —
  taking the host with it, mid-stream, with the display manager down and nothing left to bring it
  back. If lingering can't be enabled the host refuses the takeover and degrades to attach instead
  (above) rather than risk that. Run `sudo loginctl enable-linger "$USER"` once, as the setup guides
  ask; `loginctl disable-linger "$USER"` reverts it.

  With the takeover authorized the **in-stream session switch round-trips** in managed mode:
  Steam's "Switch to Desktop" inside the streamed Game Mode returns the box to its desktop session
  and the stream follows it there; the desktop's "Return to Gaming Mode" switches it forward again.

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
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` | **Attach** model: the box owns its gamescope session (on its own display); the host captures whatever's live and never tears it down. On a **headless** box the box-owned autologin session is restarted at the client's resolution on a mismatch; a box driving a physical display, and any foreign/bare gamescope, streams at its own mode. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model (the default where session infra is detected): the host takes the box's gamescope over and relaunches it headless at the client's exact mode, restoring on idle. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode — a headless appliance with no physical session running. |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover and capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch mid-stream (rebuild in place, no reconnect). On by default on Bazzite/SteamOS; set `0` to disable. |
| `PUNKTFUNK_GAMESCOPE_HDR` | `1` · `0` *(default on)* | Allow HDR (10-bit BT.2020 PQ) sessions. Needs `punktfunk-gamescope` (SDR otherwise) — see below. Set `0` to force SDR. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | e.g. `400` | On an HDR session, how bright SDR content (the desktop, the Steam overlay, an SDR game) is inside the PQ container. gamescope's default is 400. |
| `PUNKTFUNK_GAMESCOPE_BIN` | path | Force a specific gamescope binary. Otherwise the host prefers `punktfunk-gamescope` on `PATH`, then `gamescope`. |

## HDR on gamescope

Games can render HDR on a headless gamescope today, but a stock gamescope's **capture** output is
8-bit SDR: its PipeWire node offers only 8-bit formats, and it tone-maps the composite down before
handing it over. So a stock setup streams SDR — correctly, including a correct SDR rendition of an
HDR game — and there is nothing to configure.

To stream real HDR you need `punktfunk-gamescope`: gamescope plus a small patch that adds the
10-bit BT.2020 PQ formats to that node (offered upstream as
[gamescope#2126](https://github.com/ValveSoftware/gamescope/issues/2126)). It installs under its
own name and does **not** replace your system gamescope — your Gaming Mode keeps using that one.

- **Bazzite / Fedora Atomic** — included in the punktfunk sysext; `punktfunk-sysext update` gets it.
- **Arch** — the `punktfunk-gamescope` package.
- **SteamOS (Steam Deck installer)** — built and wired automatically by
  `scripts/steamdeck/install.sh` / `update.sh`.
- **NixOS** — `services.punktfunk.host.gamescopeHdr` (default `true`).
- **Anything else** — `bash packaging/gamescope/build-punktfunk-gamescope.sh` from the source tree.

HDR is attempted by default once the build is present (`PUNKTFUNK_GAMESCOPE_HDR=0` forces SDR).
Check what the host thinks it can do:

```sh
punktfunk-host hdr-probe
```

A session goes HDR when **all** of: the host allows it (`PUNKTFUNK_10BIT`, on by default, plus the
knob above), the resolved gamescope offers the 10-bit formats, the client advertises 10-bit
decoding with HDR enabled in its settings, the codec is HEVC or AV1 (never H.264), and the GPU
encodes Main10. Anything missing and the session streams 8-bit SDR — decided **before** the stream
starts, so you never get a mislabelled picture. The client overlay shows `10-bit · HDR (BT.2020
PQ)` when it worked.

Two things to know:

- **SDR content rides the same PQ stream.** The desktop, the Steam overlay and SDR games are mapped
  into the HDR container at `PUNKTFUNK_GAMESCOPE_SDR_NITS` (400 by default). If white looks too
  bright or too dim on your TV, that is the knob.
- **On AMD and Intel, HDR follows what the GPU can encode.** The host asks the driver whether it
  can encode your codec at 10 bits and picks the fast path when it can — which current AMD and
  Intel GPUs do for HEVC, and newer ones for AV1 too. On a GPU that declines, the session still
  streams HDR through a slower path. That slower path is also the only one that can draw a pointer
  back into the picture — so on a declining GPU, a *stock* gamescope would cost you the cursor.
  `punktfunk-gamescope` puts the pointer in the capture itself, which is why it is worth installing
  even on a box where you never turn HDR on. `punktfunk-host hdr-probe` reports what your box
  answered, including which side is drawing the cursor.

## Known limits

These apply to the **Gaming Mode (gamescope)** path only; the desktop path is unaffected.

- **gamescope 3.16.22 or newer is required.** Older versions can deadlock during capture. Bazzite's
  and SteamOS's current gamescope is fine; this only bites if you've pinned an old one.
- **gamescope leaves the mouse cursor out of its captured image.** You still see a pointer: the
  host reads it separately and draws it into every frame. That costs a full pass over the picture,
  and on the fastest encode paths it cannot be done at all (see the AMD/Intel note above).
  `punktfunk-gamescope` fixes it at the source — that build paints the pointer straight into the
  capture, so the host stops redrawing it and the frame reaches the encoder untouched. Nothing to
  configure; installing the build is enough, HDR or not.
- **Touch arrives as a single-finger pointer.** gamescope's virtual input device has no
  touchscreen, so the host maps a client's touchscreen to an absolute pointer: taps click exactly
  where you touch and drags work, but multi-touch gestures (pinch) aren't available in Gaming
  Mode. The desktop path has full multi-touch.
- **HDR needs the punktfunk gamescope build.** A stock gamescope's capture output is 8-bit SDR, so
  sessions stream SDR — correctly, including SDR versions of HDR games. Install
  `punktfunk-gamescope` (gamescope plus a small patch that teaches its capture node the 10-bit
  BT.2020 PQ formats), and a 10-bit-capable client streams true HDR10 — no knob needed.
  See [HDR on gamescope](#hdr-on-gamescope) below.

To stream the KDE Plasma desktop of a Steam box instead, see [KDE Plasma](/docs/kde). To bring up the
web console and pair a client, see [The Web Console](/docs/web-console).
