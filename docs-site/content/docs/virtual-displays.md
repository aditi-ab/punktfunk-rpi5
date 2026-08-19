---
title: Virtual displays
description: Control how Punktfunk creates, keeps alive, and arranges the virtual displays it streams — presets, keep-alive, exclusive vs. extend, and persistent per-client scaling.
---

When a client connects, Punktfunk creates a **virtual display** at exactly that client's resolution
and refresh, renders your desktop or game onto it, and streams it. This page covers the **policy**
for that display: how long it survives a disconnect, whether it takes over your physical monitors,
what happens when a second client connects, and how desktop environments remember per-client
settings like scaling.

Set it in the **web console** (the **Virtual displays** page), or edit
`~/.config/punktfunk/display-settings.json` (`%ProgramData%\punktfunk\display-settings.json` on
Windows). A change applies to the **next** connection — a running session keeps the display it
opened on.

> **You rarely need to touch this.** The default matches how Punktfunk has always worked; reach for
> a preset when you want a specific experience.
>
> Monitors that stayed dark, or a streamed screen showing only wallpaper? Go to
> [Troubleshooting](#troubleshooting).

To stream a monitor the host **already has** instead, see
[Stream a real monitor instead](#stream-a-real-monitor-instead) — it turns most of this page off.

> **What's live today:** **keep-alive** (linger, or **forever**), **topology** (extend / primary /
> exclusive), **conflict handling**, **per-client identity + persistent scaling** (Windows, KDE/KWin
> *and* GNOME/Mutter) and **multi-monitor layout** (several clients as monitors of one desktop) are
> all enforced. A reconnect — even a fast one — always resumes the kept display instead of spawning
> a second. Gaps, noted inline: the Linux `primary` physical-keep *effect*, and multi-display for a
> *single* client (the next stage).

## Stream a real monitor instead

> **Linux only.** A Windows host enumerates its monitors but has no backend that can capture one, so
> the Streamed screen card is read-only there and every Windows session gets a virtual display.

For a wall-mounted shop-floor PC, a lab bench machine or a media box's TV output, you want *that*
screen, not a new one. Set **Virtual displays → Streamed screen** in the console to a listed
monitor and Punktfunk streams that physical monitor instead of creating a virtual display; every
client sees it at *its* resolution.

- The monitor is **never touched** — not resized, moved, disabled or restored. Keep-alive, topology
  and multi-monitor layout don't apply: there's no display of ours to apply them to.
- **The resolution is the monitor's**, not yours. A client asking for a different one is told no and
  scales its own picture; the mid-stream resize machinery is switched off.
- **Every client sees the same screen** — two clients are two viewers of one monitor.
- Naming a monitor this host **doesn't have, while it has others**, is a **hard error**, not a
  fallback: the session fails with `no monitor named "DP-9" — this host has: HDMI-1`. The one
  exception is a session with **no physical heads at all** — a nested or headless compositor: the
  pin is set aside with a warning in the log, you get an ordinary virtual display, and the pin
  applies again the next time a session with real heads runs.
- **Virtual screen (default)** in the same card puts you back on the normal path.

Supported on **KDE/KWin**, **GNOME/Mutter**, **Sway/wlroots**, **Hyprland** and **gamescope Game
Mode** (a Steam Deck / Bazzite couch box, where gamescope drives the screen) — each through the
compositor's own screen-recording API, so there is **no chooser dialog**. That matters for a host
running unattended as a [service](/docs/running-as-a-service): a background `systemd --user` daemon
has nobody to answer a permission prompt. On gamescope only the head the session is driving is
listed — mirroring attaches to the session's own composited stream, so that screen keeps showing
what the person in front of it sees and nothing is relaunched. A *nested* or headless gamescope (the
per-client sessions the host spawns itself) has no head, so the picker is empty there.

### Naming the monitor from the host

Monitors are named by **connector** — `HDMI-A-1`, `DP-2`, `eDP-1`. List this host's (Linux-only
subcommands, like the setting):

```sh
punktfunk-host list-monitors
```

```
Kwin:
  HDMI-A-1        1920x1080@60 at +0,+0    scale 1  Dell U2412M  [primary]
  DP-2            2560x1440@144 at +1920,+0  scale 1  ACME 27
```

To pin it **from the host's configuration** instead — the appliance route — set it in
[`host.env`](/docs/configuration):

```sh
PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1
```

The environment variable **wins over the console setting**, so an operator's declaration can't be
re-aimed by a click; the console shows the Streamed screen card as locked while it's set.

Check the whole path — mirror, capture, frames — without a client:

```sh
punktfunk-host mirror-test --monitor HDMI-A-1 --seconds 20
```

Compositor screen recording is **damage-driven**: an idle desktop produces almost no frames, so move
the mouse on the host while it runs or a working mirror reads as a stall.

### Absolute input follows the pin

Pinning a monitor also re-aims **absolute** mouse and pen input to that head's origin, so a click
lands where you point on *that* screen. Heads are matched by position, not size — two monitors can
be the same size, and getting that wrong puts the pointer silently on the wrong screen. The host
resolves the pin at startup and whenever the console writes it, so no restart is needed; the log
line is `capture monitor: …`.

To check it with no client involved:

```sh
punktfunk-host anchor-test --monitor HDMI-A-1
```

It lists this host's heads, says whether the box has the same-size pair the matching exists for,
walks the pointer through the centre and corners so you can watch which screen it moves on, and
prints the region it mapped into. `--none` runs the same walk unanchored, as an A/B.

The anchor rides the **libei** injector — the backend a GNOME/Mutter host uses. On KWin, Sway and
Hyprland the host injects through a different protocol, and `anchor-test` stops and says so rather
than reporting a green run that proves nothing.

## Pick a preset

Select one in the console and you're done. Each expands to a bundle of the options documented
further down.

| Preset | What it's for |
|---|---|
| **Default** | Most setups. Reconnects resume quickly, the streamed output becomes the whole desktop, extra viewers each get their own screen. |
| **Headless box** | A monitorless machine you only stream from. Game and display survive disconnects indefinitely (keep-alive **forever**); whoever connects next takes the box over. Release it from the console when you're done. |
| **Shared desktop** | A PC you also use in person. Never blanks your real monitors, never leaves a leftover display behind; extra viewers each get their own screen. |
| **Hot-desk** | One person at a time — roam between your own devices with an instant reconnect. Anyone else is told the box is busy; each device+resolution keeps its own scaling. |
| **Workstation** | Your multi-monitor daily driver. Displays come back exactly where you arranged them, each client keeps its own settings, the desktop is yours alone. |

## Save your own preset

Once you've dialed in a setup — by tweaking a preset or setting every option under **Custom** —
**save it as your own named preset** and switch back to it in one click.

- **Save as preset** — names the settings currently in force (all the options below **plus**
  *Dedicated game sessions*) and adds it to the picker alongside the built-ins.
- **Apply** — writes exactly those settings, like picking a built-in.
- **Edit / delete** — rename it, update it to your current settings, or remove it. Deleting never
  changes what's running — it only takes the card out of the picker.

The built-in presets deliberately leave *Dedicated game sessions* alone, so switching presets never
changes your game-launch routing; a **custom preset captures your full setup**, including that axis
— it's *your* saved configuration, not a curated behavior bundle. Custom presets live on the host in
`display-presets.json` (next to `display-settings.json`); the catalog and the active policy are
independent, so editing a preset never disturbs a running session.

## Options reference

Choose **Custom** in the console to set these directly.

### Keep alive

How long the virtual display survives after your last session disconnects. On a gamescope game host
this also keeps the **game itself running**, so you can reconnect straight back into it.

- **Off** — tear the display down at session end.
- **A duration** (seconds) — keep it that long; a reconnect inside the window drops you straight
  back in, with no re-negotiation and no desktop reshuffle.
- **Forever** — keep it until you stop the host or **release it** from the console (**Virtual
  displays** → *Release*). The headless-box model.

Default: **10 seconds**. Windows has always lingered 10 s; the Linux backends previously tore down
immediately — a short linger makes reconnects smoother on both.

**A reconnect always resumes the kept display** — the host recognises your device and hands back the
same display, even a second or two after dropping (before it has noticed you left). **Deliberately
quitting** (closing the client, not a network drop) tears the display down at once, skipping the
linger. How quickly a *dropped* client is noticed is the QUIC idle
timeout — 8 s by default, tunable with `PUNKTFUNK_IDLE_TIMEOUT_MS` (see
[Legacy environment knobs](#legacy-environment-knobs)) to free kept displays sooner.

> **Keep-alive + Exclusive keeps your physical monitors dark after you disconnect**, until the
> linger expires or you release the display. Intentional for a dedicated gaming box — but don't set
> a long/forever keep-alive with Exclusive on a machine whose monitors you also use in person; use
> **Shared desktop** there.

### Topology

What Punktfunk does with your monitor layout while it streams.

- **Extend** — add the virtual display alongside your real monitors; touch nothing else.
- **Primary** — make the virtual display your primary output; physical monitors stay on.
- **Exclusive** — the virtual display becomes your **only** enabled output (physical monitors are
  disabled, then restored when streaming ends). This makes the streamed surface *be* the desktop,
  so panels and windows land on it.
- **Automatic** *(default)* — Exclusive on Windows and on an auto-detected KDE/GNOME desktop;
  Extend when you've pinned a
  specific compositor with `PUNKTFUNK_COMPOSITOR` (a test/CI posture).

Per-backend support:

| | KWin | Mutter/GNOME | Sway/wlroots · Hyprland | Windows |
|---|---|---|---|---|
| Extend | ✅ | ✅ | ✅ | ✅ |
| Primary | ✅ | ✅ | ⚠️ treated as Extend | ✅ |
| Exclusive | ✅ | ✅ | ✅ | ✅ |

**Primary** has no equivalent on **Sway/wlroots and Hyprland** — a Wayland fact, not a missing
feature: there is no primary-output concept. They have a *focused* output, and the host points that
at the streamed display — at session start, and again immediately before it launches anything from
your library. Both open new windows on the focused monitor, which is what puts the game on the
display you're streaming. Primary therefore behaves as Extend, and the host says so in the log.

Because it's focus rather than promotion, a window that opens *later* (a launcher spawning a second
window, a game re-parenting itself) follows whatever has focus then — so if you're also sitting at
the machine, clicking a physical monitor mid-launch can still pull a window over to it.

**Exclusive** does disable your physical monitors on both, and switches them back on when the last
streaming display is torn down. Two compositor-specific details:

- Punktfunk only disables monitors it did not create, so a second client streaming at the same time
  never goes dark.
- On Hyprland the restore is a `hyprctl reload`, because nothing else re-enables a monitor that a
  rule disabled — a re-applied monitor rule is accepted and ignored. The reload re-reads your
  Hyprland config, which puts your monitors back; the side effect is that settings changed at
  runtime with `hyprctl keyword` are dropped too, and a non-Lua config re-runs its `exec =` lines
  (`exec-once` is not re-run). This only happens if a session actually disabled something.

### Conflict handling · identity · layout

- **Conflict handling** — what happens when a *different* client connects while one is already
  streaming and asks for a different resolution: give it its own display (**separate**), take the
  box over (**steal**), share the existing display at its current mode (**join**), or refuse it
  (**reject**). On Linux, `separate` gives each client its own display on the shared desktop. On
  **Windows** a second client is **rejected** (a clean "host busy") even under `separate` — two
  clients can't yet share one virtual display's capture there (a later stage), so the live session
  is protected instead. A same-client *reconnect* never conflicts — it resumes.
- **Identity** — whether each client gets a **stable display identity** so your desktop environment
  remembers its settings (see [Persistent scaling](#persistent-scaling)): one shared identity, one
  **per client**, or one **per client + resolution**.
- **Layout / max displays** — when several clients each become a monitor of one desktop, this places
  them side by side (**auto**) or exactly where you arrange them in the console (**manual**, keyed to
  each client), up to **max displays**. Arrange them on the **Virtual displays** page once two or more
  are streaming.

### Dedicated game sessions

**Dedicated game sessions** control how a session that *launches a game from
[your library](/docs/game-library)* is served (Linux hosts):

- **Auto** (default) — the launch rides whatever session the box is in: the managed Steam session on a
  Steam Deck / Bazzite couch box, a bare gamescope on a plain distro, or spawned into your live KDE /
  GNOME / Sway desktop.
- **Dedicated** — every library launch gets its **own headless gamescope at your exact resolution and
  refresh**, with just the game inside — no Steam Big Picture to navigate, no game-mode desktop.
  Steam titles launch with the client hidden (`steam -silent`); non-Steam titles start almost
  instantly (gamescope up in ~1 s, then the game's own boot). Combined with **keep alive**, the game
  keeps running when you disconnect and you re-attach straight back into it.

Dedicated needs `gamescope` installed on the host; without it a launch falls back to **Auto**
routing. This axis is independent of the preset — pick it on the **Virtual displays** page. On a box
already in Steam game mode, a dedicated Steam launch frees game mode's Steam first and restores it
when the session ends. (GameStream / Moonlight launches follow the same routing.)

## When a game ends, and when a session does

Two switches, on the **Virtual displays** page under **When a game or a session ends**, tie a
session to the game the host launched for it. They apply to every store and both protocols — and
only to a game **this host launched for the session**: a game you started yourself is never touched.

### When the game exits

**End the session** (default). Quit the game and your client goes back to its own library. A
dedicated game session has always done this; it now works on every path — your live KDE/GNOME/Sway
desktop, an attached gamescope, and Moonlight.

**Keep streaming** if you stream the desktop and treat the game as incidental.

### When the session ends

Whether stopping — or losing — a session also closes the game.

- **Leave it running** (default). Nothing is ever closed. Disconnect, and the game plays on for when
  you come back.
- **Close it on Stop** — closing the client, or pressing *Stop* in the console, closes the game.
  A network drop does not: you get your game back when you reconnect.
- **Always close it** — a drop closes it too, but only after a **reconnect window** (5 minutes by
  default). Reconnect inside the window and nothing happens; the console shows the countdown, with
  an **End now** button if you'd rather not wait.

Closing a game costs whatever it hadn't saved, which is why nothing closes by default. The host asks
first — a polite close, the same as clicking the window's X, so the game runs its own shutdown — and
only forces the issue after ten seconds of being ignored.

> **Keep alive and this setting are different clocks.** Keep-alive decides how long the *display*
> outlives a disconnect (10 s by default); the reconnect window decides how long the *game* does
> (5 minutes). A display set to **Forever** stays up regardless of what happens to the game — a
> pinned display is a deliberate "this box is a game host" choice, and closing a game doesn't undo it.

### On a gamescope session, the display has the final say

When a launch gets its **own gamescope** — a dedicated game session, the usual setup on a Steam Deck
or a Bazzite couch box — the game runs *inside* the streamed display, so it lives exactly as long as
that display does, and **Keep alive decides that, not the setting above**:

| you disconnect by | what happens to the game |
|---|---|
| pressing **Stop** (or the console's stop) | the display tears down at once — keep-alive is deliberately skipped for a real stop — and the game goes with it, even on *Leave it running* |
| dropping out (network, sleep) | the display lingers for your keep-alive window, then tears down; the game ends with it |
| dropping out, keep-alive **Forever** | the display is pinned, so the game genuinely survives — and *Always close it* still ends it when the reconnect window closes |

So on a gamescope box, "leave the game running after I disconnect" means **keep-alive Forever** (or a
window long enough to come back in), not just this setting. On a desktop session — KWin, GNOME, Sway —
the game is an ordinary process next to your desktop and the setting above is the whole story.

### Automation

The host publishes `game.running` and `game.exited` events (the latter says whether the player quit
it or the host closed it), so a hook or plugin can react without polling. See
[Automation](/docs/automation).

## Persistent scaling

Set your display **scaling** once and have it stick across reconnects. Each client gets a *stable
display identity*, so your desktop environment keys its per-monitor settings to it.

| Host | Supported | How |
|---|---|---|
| **Windows** | ✅ today | Set scaling in Settings while streaming — Windows remembers it per client. |
| **KDE / KWin** | ✅ today | Set scaling in System Settings while streaming; KWin keys it to a stable per-client output name and reapplies it on reconnect. Validated live (150 %/125 % survive a full disconnect + reconnect). |
| **GNOME / Mutter** | ✅ today | GNOME's virtual-monitor API exposes no stable identity to key config on, so the **host persists the scale itself**: set scaling in Settings while streaming — the host captures the change, remembers it per client, and reapplies it on reconnect. |
| **Sway / wlroots** | ❌ | Headless outputs can't carry a stable identity; pin scale in your sway config instead. |

## Legacy environment knobs

These `PUNKTFUNK_*` variables still work, but the console (and `display-settings.json`) supersede
them — when a settings file exists, it wins.

| Legacy knob | Now expressed as |
|---|---|
| `PUNKTFUNK_MONITOR_LINGER_MS` | **Keep alive** → duration *(Windows)* |
| `PUNKTFUNK_NO_ISOLATE` | **Topology** → Extend *(Windows)* |
| `PUNKTFUNK_KWIN_VIRTUAL_PRIMARY` / `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY` | **Topology** → Exclusive (when set) / Extend (when `0`) |

One knob has no console equivalent — it's transport tuning, not display policy:

- **`PUNKTFUNK_IDLE_TIMEOUT_MS`** (host, default `8000`) — how long the host waits before declaring a
  *dropped* client gone, which is when a kept display starts its linger (or is freed). Lower it (e.g.
  `3000`) to reclaim kept displays sooner after an ungraceful drop; it's clamped to ≥1 s and its
  keep-alive ping scales with it, so a live session never false-disconnects. A deliberate quit is
  instant regardless. Also `--idle-timeout-ms` on `punktfunk1-host`.

## Troubleshooting

**My physical monitors stayed off after I disconnected.** Keep-alive is set together with Exclusive
topology — the display (and your isolated desktop) is kept for the linger window. Release it from
the console (**Virtual displays**), or switch to the **Shared desktop** preset so streaming never
disables your real monitors.

**The virtual output shows only my wallpaper.** Your topology is Extend, so the streamed display is
an empty extension. Use **Primary** or **Exclusive** so your desktop lands on it.

**KWin can't create the virtual output.** On a normal Plasma session KWin runs its **DRM backend**,
which creates virtual outputs at any version. The 6.5.6 floor applies only to the **virtual backend**
(`kwin_wayland --virtual`, used for headless and test sessions) — below that the request fails with
"Could not find output". On **KWin 6.6+** that same message also covers an output KWin *did* create
and then left disabled; [KDE Plasma](/docs/kde#troubleshooting) walks that one. See
[requirements](/docs/requirements).

**Reconnecting into game mode reconnects cleanly now.** On a Steam Deck / Bazzite box, disconnecting
and reconnecting within game mode reuses the still-warm session (or cleanly recreates it) instead of
landing on a dead stream — and switching between game mode and the KDE / GNOME desktop mid-stream
follows the switch. If a launched game **exits**, a dedicated session ends and returns you to your
library; a game mode / desktop session keeps streaming.

**My keep-alive / topology / layout settings do nothing.** Check whether **Streamed screen** is set
to a real monitor — those options are about a display Punktfunk created, and when it's mirroring one
of yours there is nothing to keep alive or rearrange. Switch the card back to *Virtual screen
(default)*.

**The console won't let me change Streamed screen.** `PUNKTFUNK_CAPTURE_MONITOR` is set in this
host's [`host.env`](/docs/configuration) and outranks the console. Unset it (and restart the host)
to choose from the console instead.

**My session fails with "no monitor named …".** The pinned connector isn't among this host's
monitors — renamed, unplugged, or the host is now in a different session. Run
`punktfunk-host list-monitors` on the host to see the real names. Punktfunk will not quietly stream
a different screen.

**My couch box's TV stayed on the streamed session after I disconnected.** With the **Headless box**
preset (keep alive = *forever*), a managed Steam session is held indefinitely so a reconnect resumes
instantly — return to game mode on the box (or restart the host) to hand the TV back.
