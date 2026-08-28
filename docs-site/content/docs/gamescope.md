---
title: Steam / gamescope
description: Configure a gamescope/Steam host — how the host gets a gamescope, session following, and limits.
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

## How the host gets a gamescope

There are three models; the host picks one per session, and you rarely have to. With **nothing
set** — which is what every shipped template does — a box that has gamescope session infrastructure
(Bazzite, SteamOS, Nobara) gets **managed**.

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
- **Bare spawn** (the default on a plain distro with no gamescope session infrastructure and no
  gamescope already running, and the route a dedicated game launch takes unless you've forced
  managed or attach) — the host starts its own headless gamescope per session at the client's mode
  and runs the session's launch command (or `PUNKTFUNK_GAMESCOPE_APP`) inside it. Nothing on the
  box is taken over, because there is nothing to take over.

### Nobara and other autologin display managers

The managed takeover has to stop the box's Gaming Mode session to free Steam — and when a display
manager autologs into that session, stopping it alone accomplishes nothing: the autologin puts it
straight back. So the host **idles** that session for the length of the stream instead, with a
systemd drop-in that replaces its `ExecStart` with a process that just sleeps. The autologin still
succeeds (nothing relogin-loops), the session it logs into does nothing (Steam is free), and the
**display manager keeps running** — which is what lets you still switch the box to Desktop Mode
from Steam while a stream is up.

That needs no privilege at all: the drop-in is a user-level unit override, written under
`$XDG_RUNTIME_DIR` so it cannot outlive the login session, and a reboot clears it regardless.
Versions before this one stopped the display manager for the stream's duration — which needed a
root helper, the `punktfunk` group, and lingering, and left the box with nothing able to start a
desktop session, so Steam's own "Switch to Desktop" hung until a reboot.

> **Join the `punktfunk` group on any box you stream Game Mode from.** The guided installer
> defaults to yes on Bazzite, Nobara, and other `ujust` / Fedora Atomic boxes. The takeover
> itself no longer needs the group — it now gates the usbip nodes the virtual Steam Deck pad
> attaches through, so without it the pad arrives as an ordinary Xbox 360 controller:
>
> ```sh
> sudo usermod -aG punktfunk "$USER"   # then log out and back in
> ```
>
> It also keeps the box's own power menu working: on a takeover flavour that *stops* the display
> manager, the box has no active local session for the length of the stream, and Shut Down /
> Restart / Sleep are refused unless you are in this group.
>
> Your package created the group at install time and put **nobody** in it, on purpose: it also
> gates the usbip nodes the virtual Steam Deck pad attaches through, and writing those can present
> arbitrary emulated USB hardware — so joining stays a deliberate act, on a machine you trust.
> Skip it and nothing fails loudly. Every takeover degrades to mirroring the box's own session
> (below), which on a box whose panel is off reads as a black screen on every connect. The host
> checks this at startup on any box that will need the takeover and says so in its log; the
> symptom side is [Game Mode: black screen on
> connect](/docs/troubleshooting#game-mode-black-screen-on-connect-or-the-stream-is-stuck-at-the-boxs-resolution).

The display-manager flavor is no longer an input — SDDM, plasmalogin and the rest all get the
idled session above, and none of them is stopped. The root helper described below is therefore no
longer part of a normal takeover; it is kept for the restore path, and for a box where an older
host left a display manager stopped:

- The packages ship it: a root helper
  (`/usr/libexec/punktfunk/pf-dm-helper`, or `/usr/lib/punktfunk/pf-dm-helper` from the Arch
  package) behind its own polkit action (`io.unom.punktfunk.dm-helper`), invoked automatically
  when the plain `systemctl` verbs are denied. The helper only stops/restores the unit the
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
  what happened; check the host log for "managed takeover unavailable"). That log line now quotes
  the privileged path's own reason for refusing, so read it before changing anything: by far the
  most common one is `not in the 'punktfunk' group`, which the group command above fixes and
  neither a reinstall nor a polkit rule does. If the display-manager restart ever loses its
  privilege mid-restore, `PUNKTFUNK_RECOVER_SESSION_CMD` (see
  [Configuration](/docs/configuration)) is fired as the fallback.

  **Lingering is required here**, and the host turns it on for you the first time it takes the box
  over. Stopping the display manager ends your last login session, and without
  `loginctl enable-linger` logind stops your `systemd --user` manager about ten seconds later —
  taking the host with it, mid-stream, with the display manager down and nothing left to bring it
  back. If lingering can't be enabled the host refuses the takeover and degrades to attach instead
  (above) rather than risk that. Run `sudo loginctl enable-linger "$USER"` once, as the setup guides
  ask; `loginctl disable-linger "$USER"` reverts it. (A host with no login session of its own turns
  lingering on through the *same* helper, so a missing group grant surfaces here first — the log
  says "enabling lingering failed" and then quotes the same reason.)

  With the takeover authorized the **in-stream session switch round-trips** in managed mode:
  Steam's "Switch to Desktop" inside the streamed Game Mode returns the box to its desktop session
  and the stream follows it there; the desktop's "Return to Gaming Mode" switches it forward again.

## Stream the screen the box is already driving

There is a fourth thing you can ask for, and it isn't a gamescope model at all: **mirror the head
Gaming Mode is lighting**. A Gaming Mode gamescope is the DRM master of a real connector, so that
head is listed by `punktfunk-host list-monitors` and appears in the web console under **Virtual
displays → Streamed screen**. Pick it there, or pin it from
[`host.env`](/docs/configuration):

```sh
PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1
```

The host then attaches to the session's own composited output: nothing is stopped, nothing is
relaunched, no mode is imposed, and what you see is exactly what is on the TV. That is the
difference from **managed**, which deliberately takes the session over and relaunches it headless,
so the box's own session stops driving that panel.

Whether the panel then goes *dark* is the **Topology** setting's job, not the model's — see
[Virtual displays](/docs/virtual-displays#topology). What `exclusive` can do differs by model:

- **Managed** and **bare spawn** — the box's own gaming session is moved out of the way so it
  stops driving the panel, and then the panel is actually turned **off** for the stream and
  restored at teardown. The host asks whichever desktop is running: KDE goes through KWin's DPMS,
  sway and Hyprland through their own; a box already in Game Mode has no desktop to ask, so the
  host turns the CRTCs off over DRM itself. None of these needs root — the DRM path rides the same
  seat access every local compositor gets.
- **Attach** — nothing is darkened, and cannot be: this model streams the panel the box is
  driving, so turning it off would turn off the picture.

Under `extend` or `primary` none of this happens and your screens are left alone. If `exclusive`
asked for a dark screen and the host could not deliver one — **GNOME** is the case that cannot be
served, because Mutter offers apps no way to turn a screen off and holds the graphics card itself
so the DRM route is refused — it says so in the log rather than leaving you guessing at a lit
screen.

Only the one head the session drives is listed — a nested or headless gamescope (including the
per-session ones the host spawns itself) has none of its own, so the picker is empty there. Full
details, including what the setting turns off, are in
[Stream a real monitor instead](/docs/virtual-displays#stream-a-real-monitor-instead).

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

Which of the three it picked is logged per session:

```sh
journalctl --user -u punktfunk-host | grep 'gamescope sub-mode'
```

Then bring up [The Web Console](/docs/web-console) to arm pairing.

## gamescope knobs

Every gamescope setting — the three models above, `PUNKTFUNK_GAMESCOPE_NODE`, the bare-spawn flags
(`APP`, `SPLASH`, `STEAM`, `GRAB_CURSOR`), the binary override, the two HDR knobs, and
`PUNKTFUNK_SESSION_WATCH` — lives in the `host.env` reference, under *gamescope / session
following*: [Configuration](/docs/configuration). Leave them unset to auto-detect; set one only to
force a model.

Two are worth naming here, because the sections below turn on them:

- `PUNKTFUNK_GAMESCOPE_HDR` — on by default; see [HDR on gamescope](#hdr-on-gamescope).
- `PUNKTFUNK_GAMESCOPE_BIN` — forces one gamescope binary. Unset, the host prefers
  `punktfunk-gamescope` on `PATH` and falls back to `gamescope`.

## HDR on gamescope

Games can render HDR on a headless gamescope today, but a stock gamescope's **capture** output is
8-bit SDR: its PipeWire node offers only 8-bit formats, and it tone-maps the composite down before
handing it over. So a stock setup streams SDR — correctly, including a correct SDR rendition of an
HDR game — and there is nothing to configure. This section is the gamescope half; the rest of the
chain, and what to check when a stream comes out SDR, is on [HDR](/docs/hdr#linux--gamescope).

To stream real HDR you need `punktfunk-gamescope`: gamescope plus a small patch that adds the
10-bit BT.2020 PQ formats to that node (offered upstream as
[gamescope#2126](https://github.com/ValveSoftware/gamescope/issues/2126)). It installs under its
own name and does **not** replace your system gamescope — your Gaming Mode keeps using that one.

- **Bazzite / Fedora Atomic** — included in the Punktfunk sysext; `punktfunk-sysext update` gets it.
- **Fedora, Nobara and other RPM boxes** — `sudo dnf install punktfunk-gamescope` from the same
  Punktfunk repo the host comes from.
- **Debian 13 and Ubuntu 26.04** — `sudo apt install punktfunk-gamescope` from the Punktfunk apt
  repo. It is the only gamescope those two have: Debian packages none at all, and Ubuntu 26.04's
  is 3.16.20 — below the [3.16.22 floor](/docs/requirements), so headless capture deadlocks
  against the PipeWire 1.6 that same release ships. **Ubuntu 24.04 cannot run it**: the build needs
  wayland ≥ 1.23.1 and noble has 1.22.0, so build from source or move to a newer release.
- **Arch** — the `punktfunk-gamescope` package.
- **SteamOS (Steam Deck installer)** — built and wired automatically by
  `scripts/steamdeck/install.sh` / `update.sh`.
- **NixOS** — `services.punktfunk.host.gamescopeHdr` (default `true`).
- **Anything else** — `bash packaging/gamescope/build-punktfunk-gamescope.sh` from the source tree.

HDR is attempted by default once the build is present (`PUNKTFUNK_GAMESCOPE_HDR=0` forces SDR).

**The build only reaches sessions the host starts itself** — managed, `PUNKTFUNK_GAMESCOPE_SESSION`,
or a bare spawn. In **attach** mode the running session is the box's own, started by the display
manager with the distro's `gamescope`, so it offers neither the 10-bit formats nor the in-node
cursor — but the host answers both questions by asking the *installed* binary, so an installed
`punktfunk-gamescope` makes it believe the attached session has them. Attach *plus* that build is
the combination to avoid: [HDR → Linux + gamescope](/docs/hdr#linux--gamescope) has what it costs
you and the two ways out.

The cursor is the half this page owns. The host leaves the pointer to the compositor whenever the
installed build can paint it (below) — so on an attached session, which can't, nothing draws it and
the stream has no cursor at all. Removing
`PUNKTFUNK_GAMESCOPE_ATTACH=1` from your `host.env` and letting the managed
default take over fixes that along with HDR. To stay on attach, point `PUNKTFUNK_GAMESCOPE_BIN` at
your distro's own `gamescope` (`/usr/bin/gamescope`) instead: the host goes back to compositing the
cursor itself, and — since the HDR answer comes from the same binary — stops attempting HDR too.

One thing to know beyond HDR itself: **the pointer rides on the same build.** When the compositor
paints the cursor into the capture node the host stops blending one in, which is also what frees the
session to take the encoder's fastest source — a front end with no blend stage. That is why
`punktfunk-gamescope` is worth installing even on a box where you never turn HDR on.

## Known limits

These apply to the **Gaming Mode (gamescope)** path only; the desktop path is unaffected.

- **gamescope 3.16.22 or newer is required; 3.16.23 or newer for the Steam overlay.** Below
  3.16.22, headless capture can deadlock against PipeWire 1.6. Between 3.16.22 and 3.16.23 capture
  works, but gamescope doesn't paint the Steam overlay (Shift+Tab / the Quick Access Menu) into its
  capture node, so the overlay is missing from an otherwise perfect picture. Either case is logged
  at startup with the version found. Bazzite's and SteamOS's current gamescope is past both; this
  only bites if you've pinned an old one.
- **On a stock gamescope, Gaming Mode reports the wrong refresh rate — and offers no resolutions.**
  A headless gamescope has no EDID, and upstream's headless connector advertises no display modes
  and no refresh rates at all. Steam's in-session display settings then show a single refresh entry
  and an empty resolution list, and that one entry is whatever the session was launched with — or
  **60 Hz** if the launch flag went missing. Games that pace themselves to the display will hold
  themselves there, even though the stream is running at your client's full rate (the client's own
  fps counter keeps reading correctly, because the encoder repeats held frames — so the counter is
  not the thing to trust here; an in-game fps readout is). `punktfunk-gamescope` publishes the real
  mode and rate, and `PUNKTFUNK_GAMESCOPE_REFRESH_RATES=60,90,120` puts more than one entry in that
  menu. If the host log says *"the session did not start at the mode we asked for"*, a file in
  `/etc/gamescope-session-plus/sessions.d/` is overriding `GAMESCOPE_BIN` or setting `GAMESCOPECMD`.
- **The performance overlay (fps / frametime / stats) needs the patched build.** It is mangoapp,
  which gamescope draws as an *external overlay* — a layer upstream's capture composite has never
  included on any version, so on a stock gamescope you can turn the overlay on and it simply will
  not appear in the stream. There is no host-side substitute: the host cannot reconstruct another
  process's overlay window. `punktfunk-gamescope` paints it into the capture stream.
- **The cursor comes from the compositor when it can, and from the host otherwise.** A stock
  gamescope leaves the pointer out of its captured image, so the host reads it separately and draws
  it into every frame — a full pass over the picture, and the fastest encode source cannot blend at
  all. `punktfunk-gamescope` paints the pointer into the capture instead, so the host stops
  redrawing it and the frame reaches the encoder untouched — but only in a session the host starts
  itself, not in **attach** mode (see [HDR on gamescope](#hdr-on-gamescope) above).
- **Touch arrives as a single-finger pointer.** gamescope's virtual input device has no
  touchscreen, so the host maps a client's touchscreen to an absolute pointer: taps click exactly
  where you touch and drags work, but multi-touch gestures (pinch) aren't available in Gaming
  Mode. The desktop path has full multi-touch, and the client's other two
  [touch modes](/docs/input#touch-modes) — trackpad and direct pointer — are unaffected either way,
  because they send ordinary mouse events.
- **Desktop (absolute) mouse mode is unavailable.** A client asking for it quietly stays captured
  against a Gaming Mode session, and the [mouse-mode](/docs/input#mouse-modes) shortcut has nothing
  to switch to.
- **There is no clipboard.** A gamescope session offers neither mechanism the host can read and
  write a clipboard through, so [clipboard sharing](/docs/clipboard) does nothing in Gaming Mode,
  even with both of its switches on.
- **HDR needs the Punktfunk gamescope build** — see [HDR on gamescope](#hdr-on-gamescope) above.

To stream the KDE Plasma desktop of a Steam box instead, see [KDE Plasma](/docs/kde). To bring up the
web console and pair a client, see [The Web Console](/docs/web-console).
