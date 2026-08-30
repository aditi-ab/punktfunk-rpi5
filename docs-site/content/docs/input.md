---
title: Mouse, touch and pen
description: The in-stream keyboard shortcuts that give your mouse back, the two mouse modes, the three touch modes, and full-fidelity stylus input.
---

A stream takes your mouse and keyboard the moment you click into it. This page starts with how to
get them back, then covers driving the host with a mouse, a touchscreen and a pen. The rows that
pick these modes sit in your client's **Input** settings; the toggles that share that page are in
[Client settings](/docs/client-settings#input).

## Getting your input back

On the Linux and Windows clients the stream runs in its own session window. Input is **captured**
when the stream starts and whenever you click the video: your local cursor disappears and keys go to
the host. In the default mouse mode the pointer is also locked to the window — see
[Mouse modes](#mouse-modes).

| Shortcut | What it does |
|---|---|
| **Ctrl+Alt+Shift+Q** | Release captured input (press again, or click the stream, to take it back) |
| **Ctrl+Alt+Shift+M** | Switch the mouse mode (capture ⇄ desktop) |
| **Ctrl+Alt+Shift+D** | Disconnect |
| **Ctrl+Alt+Shift+S** | Cycle the [stats overlay](/docs/stats) — off · compact · normal · detailed |
| **Ctrl+Alt+Shift+V** | Mute or unmute your microphone |
| **F11** or **Alt+Enter** | Toggle fullscreen |

While input is released the session window prints the shortest list over the stream:

```text
Click the stream to capture input · Ctrl+Alt+Shift+Q releases · Ctrl+Alt+Shift+M mouse mode ·
Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats
```

With a controller in use the hint names the controller chord instead of the mouse-mode and stats
entries. The full list is always available without a stream running — see below.

### Muting your microphone

**Ctrl+Alt+Shift+V** stops sending your microphone to the host; pressing it again resumes. The
uplink keeps running underneath, so unmuting is instant.

While muted, a **Microphone muted** badge sits in the top-right corner of the stream — separate
from the [stats overlay](/docs/stats), so it shows even with stats off.

The mute lasts for that stream only — the next session starts unmuted; nothing is written to your
settings. With **Stream microphone** off in [client settings](/docs/client-settings#audio) the
shortcut does nothing and no badge appears.

The **keyboard** chord is **Linux and Windows** only (a Steam Deck stream is the Linux client, so an
attached keyboard gets it). On **Android** a controller can reach the same toggle: **Select + Y**,
and on a DualSense the pad's own **Mute** button does it too — one toggle per press, and the badge
is the same. On **Apple** clients there is no shortcut; turn **Stream microphone** off in settings
instead.

Alt-Tabbing away releases input on its own and takes it back when you return. A release you asked
for with the chord stays released until you opt back in. Either way, keys and buttons you were
holding are released on the host, so nothing sticks down.

Without a stream running, the Linux client lists the shortcuts under **Keyboard Shortcuts** in its
main menu, and the Windows client on a **Shortcuts** screen reached from its host list. Both list the
microphone mute; the in-stream hint over the video doesn't, to stay one readable line.

### On the other clients

- **macOS** honours the release, mouse-mode, disconnect and stats combos, written
  **⌃⌥⇧Q / M / D / S** — but not the microphone mute. **⌘⎋** also toggles capture, **⌃⌘F** toggles
  fullscreen, and **⌃⌥⇧C** starts or stops [clipboard sharing](/docs/clipboard). The **Stream** menu
  lists them all except the mouse-mode combo, which works but has no menu item. Every *other* ⌘
  chord goes to the host while input is captured — ⌘Q reaches the host's compositor rather than
  quitting the app — unless you turn **Capture system shortcuts** off in
  [client settings](/docs/client-settings#input). ⌘⎋ and ⌃⌘F are held back either way, so there is
  always a way out.
- **iPhone and iPad** with a hardware keyboard: **⌃⌥⇧Q** releases input while it is captured, and
  **⌘⎋** toggles capture in either direction. **⌃⌥⇧D** (disconnect) and **⌃⌥⇧S** (stats) come from
  the app's Stream shortcuts rather than from the stream itself; if they don't respond while you're
  captured, release first or use the on-screen controls.
- **Android and Android TV** honour **Ctrl+Alt+Shift+Q** only; it toggles pointer capture. The
  system Back button leaves the stream.
- **Apple TV** has no keyboard path, and a short press of the Siri Remote's Back button deliberately
  does nothing — so a controller's B button can't end your session by accident. To leave, **hold
  Back for about a second and let go**. During a session the remote's touch surface drives the host
  cursor, a press is a left click, and Play/Pause is a right click — **hold Play/Pause** instead and
  it cycles the [stats overlay](/docs/stats). With a controller in hand, **Select + X** does the
  same on every Apple client.

### Leaving with a controller

Every client reserves one controller chord: **L1 + R1 + Start + Select** (LB + RB + Start + Back on
an Xbox pad), held on any connected pad.

- **Linux, Windows** — a press releases captured input, and leaves fullscreen if you didn't start
  fullscreen. Hold about 1.5 seconds and it disconnects.
- **Steam Deck** — a press releases captured input only. The Decky plugin always launches the client
  fullscreen, and a stream that started fullscreen stays that way. Holding disconnects, as above.
- **macOS, iPhone/iPad, Apple TV** — holding about 1.5 seconds disconnects. There is no quick-press
  step.
- **Android** — holding about a second disconnects. A quick press does nothing; the moment the chord
  completes a **Hold to quit…** cue appears so you know it registered.

The chord is read off the pads a client forwards, so turning
[**Forward controllers**](/docs/client-settings#input) off takes it away on **Linux and Windows** —
there the client stops opening the controller at all. Use
**Ctrl+Alt+Shift+D** or the client's own UI to leave instead. The Apple and Android apps keep
watching for the chord either way.

### Statistics with a controller

The **Apple** apps reserve a second chord: **Select + X**, which cycles the
[stats overlay](/docs/stats) one level each time you complete it — for a pad with no keyboard and
no free screen for the three-finger tap; on **Apple TV** it is the only way there with a pad. Both
buttons still reach the game; only the overlay changes locally.

On the **Siri Remote**, **hold Play/Pause** for about half a second instead. A quick tap is still a
right click, sent when you let go.

### The guide button (Xbox / PS / Steam) and Quick Access

A controller's **guide button** — the Xbox logo, the PS button, the Deck's **Steam** button — is
meant to open menus **on the host**. Some devices want that button for themselves, so every client
also carries a gesture that works everywhere: **hold Select (Back / View) on its own for about a
third of a second**. The host sees its guide button held for as long as you hold — a long press,
which is how SteamOS opens the **Quick Access Menu** for a regular pad. A quick tap of Select still
reaches the game, delivered when you let go (a beat late); Select in a combo — including the leave
chord above — passes through untouched.

What the raw button does, per client:

- **Linux & Windows desktop, macOS, Android** — the guide press is forwarded to the host. If Steam
  Big Picture or the Xbox Game Bar is also watching for it *on the device in your hands*, both may
  react — that's a local setting on that device, not something the stream can suppress.
- **Steam Deck / Gaming Mode** — the **Steam** and **`…`** buttons stay with the Deck by default
  (SteamOS always opens its own menus for them; forwarding the raw press too opens both menus at
  once). Reach the host's menus with **hold-Select**, or the Punktfunk panel's **Host menus**
  buttons ([Steam Deck page](/docs/steam-deck)); **Steam / guide button → Send to host** restores
  the old behavior.
- **iPhone / iPad** — iOS reserves the Home press for its own Game Overlay, so hold-Select is the
  reliable route to the host's overlay. On iOS 27 or later you can also hand the button to the app
  yourself, in the system's per-controller Home-button setting.
- **Apple TV** — tvOS never delivers the Home press to apps; hold-Select is the only route.

Both halves are [settings](/docs/client-settings#input), per profile like everything else:
**Steam / guide button** (Automatic / Send to host / This device) and **Hold Select for guide**
(Automatic / On / Off). Automatic picks the behavior above for each platform — the gesture stays off
where the raw button already works, so games that use a *held* Select keep it.

## Mouse modes

There are two, and they are a per-client setting called **Mouse input**:

- **Capture (games)** — the pointer locks to the stream and only relative movement is sent. The only
  cursor you see is the host's. This is what mouse-look in a game needs. The session window also
  grabs the keyboard, so Alt+Tab and the Windows key (Super on Linux) reach the host rather than
  your own desktop — on macOS that is the ⌘ chords, ⌘Q included, with ⌘⎋ kept back as the way out.
  Turn **Capture system shortcuts** off in [client settings](/docs/client-settings#input) to keep
  them local.
- **Desktop (absolute)** — the pointer is not locked. It moves in and out of the stream freely and
  its position is sent as an absolute point — what you want for remote desktop work. Your local
  cursor is hidden over the stream; the one you see there is the host's. On Linux and Windows,
  Alt+Tab and the Windows/Super key go to the host here too while **Capture system shortcuts** is
  on — the host's Start menu is part of the desktop you're driving — and clicking any other local
  window takes them back. (On a Mac the ⌘ chords stay local in this mode.)

**Capture is the default** on the Linux, Windows and macOS clients. **Android defaults to Desktop**.

Switch live with **Ctrl+Alt+Shift+M** (**⌃⌥⇧M** on macOS), whether input is captured or not. On
Android, Ctrl+Alt+Shift+Q flips the capture instead. The picker is macOS-only among the Apple apps;
on iPad the equivalent is the **Capture pointer for games** toggle (on by default), which needs the
stream fullscreen and frontmost.

Two things can override your choice. **gamescope hosts can't take absolute pointer input**: ask for
desktop mode against one and the session quietly stays captured, and the chord has nothing to offer
(see [gamescope](/docs/gamescope)). And against a host that forwards its cursor separately instead of
drawing it into the video, the Linux and Windows clients flip to relative motion by themselves when
an app on the host grabs or hides the pointer, then back when it lets go. Using the chord yourself
overrides that until the host's intent next changes. The macOS client ignores the signal on purpose.

## Touch modes

On a touchscreen client the **Touch input** setting picks one of three models. All three exist on
Android, iPhone/iPad, Linux and Windows.

- **Trackpad** (the default) — your finger drives the host cursor like a laptop touchpad. The cursor
  stays put when you touch down and moves by your finger's travel, so you can lift and re-swipe to
  walk it across a screen far larger than your own.
- **Direct pointer** — the cursor jumps to your finger and follows it.
- **Touch passthrough** — every finger is forwarded as a real touch contact, with no gesture
  interpretation at all. Only useful for apps and games that genuinely understand touch.

Trackpad and Direct pointer share one gesture vocabulary: tap = left click, two-finger tap = right
click, two-finger drag = scroll, tap-then-press-and-drag = a held left drag, **three-finger tap =
cycle the stats overlay**. On Android and iPhone/iPad a **three-finger swipe up or down** summons or
dismisses the local on-screen keyboard for typing on the host; the Linux and Windows clients have no
such keyboard, and there any two-or-more-finger drag scrolls.

Touch passthrough depends on the host being able to inject touch, and that varies:

| Host | Touch passthrough |
|---|---|
| KDE Plasma (KWin), GNOME | Full multi-touch |
| Windows 10 1809 and newer | Full multi-touch |
| Sway, Hyprland and other wlroots compositors | Not injected — contacts are dropped |
| gamescope Gaming Mode | Degraded to a single absolute pointer — see [gamescope](/docs/gamescope) |

Wherever the compositor offers no touchscreen device to drive, only the first finger is used, as
an absolute pointer — tapping still clicks; pinches and multi-finger gestures don't survive. The
trackpad and pointer models are unaffected: they send ordinary mouse events.

A host says whether it injects touch at all. Against one that does not (the wlroots row above,
or Windows before 1809), a client set to Touch passthrough runs the trackpad model for that
session and says so in a short notice when the stream starts, instead of forwarding contacts the
host would drop.

## The quick-action ring

On Android, iPhone and iPad a **two-finger twist** on the stream opens a ring of six buttons under
your fingers: about 10° starts it opening, 30° commits it, and lifting short of that winds it back
in and sends nothing. The centre button opens a sheet with the whole catalogue and the resolution
presets. On Android the **Back** gesture opens the same ring at the screen centre instead of ending
the session; on iPhone and iPad the corner disc does; on Apple TV a short press of the remote's
Back; with a controller, **Select+A** (Select first) on every client, and the host never sees the
two presses. What the six buttons hold is the **Quick actions** setting — on the phones the editor
is the ring itself.

### Virtual controller

Android and iPhone/iPad can draw a controller over the stream, for a game that needs one when no
controller is attached. Show or hide it from the ring's **Virtual controller** button. The host sees
one controller arrive when it appears and one leave when it goes, exactly as for a real pad, on the
next free pad index beside any real controller you have connected. A finger on one of its controls
drives the game; a finger anywhere else still drives the touch mode, so tap-to-click keeps working
beside it. A stick follows your thumb from wherever it lands, the D-pad reads eight directions, and a
trigger reads how far down its pill your finger sits, so a slow press is a slow press. **Layout**,
**Opacity** and **Scale** live under Quick actions in the [client settings](/docs/client-settings#input):
Full (two sticks, D-pad, face buttons, bumpers and triggers), Sticks and shoulders, or D-pad and
face buttons. Not on Apple TV, a Steam Deck or the desktop clients.

## Pen and stylus

A stylus is not treated as a finger. Punktfunk carries **position, tip pressure, tilt angle and tilt
direction, barrel roll, hover distance, the eraser end, and two barrel buttons** on their own input
plane.

**Clients that send pen input:**

- **iPhone and iPad** with an Apple Pencil, including hover. The Pencil has no hardware eraser or
  barrel buttons, so a **double-tap** is sent as barrel button 2, and on iOS 17.5 or newer a
  **squeeze** is sent as barrel button 1. Pencil Pro's barrel roll also needs iOS 17.5.
- **Android** phones and tablets with an active stylus — pressure, tilt, hover, the eraser tool and
  both barrel buttons. Android exposes no barrel-roll axis, so roll is not sent from there.
- **[Moonlight](/docs/moonlight) clients** that send pen events reach the same host-side pen.

The Linux, Windows, macOS and Apple TV clients do not send stylus input.

**What the host presents it as:**

- On **Linux**, a virtual tablet named **Punktfunk Pen** appears the first time you use the stylus
  and is removed when the session ends. Applications see a real pen through the usual tablet path,
  so Krita, GIMP and Xournal++ treat it as a graphics tablet. It is a screen tablet, mapped by your
  compositor's own default tablet mapping — correct on a single output; multi-monitor pinning is up
  to the compositor.
- On **Windows**, a per-session synthetic pen pointer feeds Windows' normal pen system: pressure,
  tilt, rotation, the barrel button and the eraser. This needs **Windows 10 1809 or newer**.

**Before it can work on Linux**, the host needs access to `/dev/uinput` — the same `input` group step
the virtual gamepads need, step 3 of your [install guide](/docs/install). Without it the host never
offers pen at all.

**If the host is too old, or pen is switched off**, the client folds the stylus into its ordinary
touch or pointer path — you can still draw, without pressure and tilt. Whether pen splits out is
decided by the host, not your touch mode.

**Operators** can turn the whole feature off by setting `PUNKTFUNK_PEN=0` in the host's `host.env`
(see [Configuration](/docs/configuration)). The host then stops advertising pen to Punktfunk and
Moonlight clients alike, and every client falls back to touch.
