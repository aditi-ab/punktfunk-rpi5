---
title: Virtual displays
description: Control how Punktfunk creates, keeps alive, and arranges the virtual displays it streams — presets, keep-alive, exclusive vs. extend, and persistent per-client scaling.
---

When a client connects, Punktfunk creates a **virtual display** sized to exactly that client's
resolution and refresh, renders your desktop or game onto it, and streams it. This page is about the
**policy** for that display: how long it survives a disconnect, whether it takes over your physical
monitors, what happens when a second client connects, and how desktop environments remember
per-client settings like scaling.

You set this policy in the **web console** (the **Virtual displays** page), or by editing
`~/.config/punktfunk/display-settings.json` directly (`%ProgramData%\punktfunk\display-settings.json`
on Windows). A change applies to the **next** connection — a running session keeps the display it
opened on.

> **You rarely need to touch this.** The default behavior matches how Punktfunk has always worked.
> Reach for a preset when you want a specific experience — a dedicated box you only stream from, a
> desktop you also use in person, or a multi-monitor workstation.
>
> Here for one specific problem — monitors that stayed dark, or a streamed screen showing nothing
> but wallpaper? Go straight to [Troubleshooting](#troubleshooting) at the bottom.

If you'd rather stream a monitor the host **already has** than have Punktfunk make one, that's a
different setting and it turns most of this page off — see
[Stream a real monitor instead](#stream-a-real-monitor-instead) right below.

> **What's live today:** **keep-alive** (linger, or **forever**), **topology** (extend / primary /
> exclusive), **conflict handling**, **per-client identity + persistent scaling** (Windows, KDE/KWin
> *and* GNOME/Mutter), and **multi-monitor layout** (several clients as monitors of one desktop) are
> all enforced. A reconnect always resumes the kept display — even a fast one — instead of spawning a
> second. The remaining gaps are noted inline: the Linux `primary` physical-keep *effect*, and
> multi-display for a *single* client (that last is the next stage).

## Stream a real monitor instead

> **Linux only.** A Windows host enumerates its monitors but has no backend that can capture one, so
> the Streamed screen card is read-only there and every Windows session gets a virtual display.

Sometimes you don't want a new screen — you want *that* screen. A shop-floor PC on a wall mount, a
lab bench machine, a media box whose TV output you'd like to watch from the couch: what matters is
seeing the monitor that's already there, exactly as the person in front of it sees it.

Set **Virtual displays → Streamed screen** in the console to one of the listed monitors and
Punktfunk stops creating a virtual display altogether. It streams that physical monitor instead,
and every client sees it at *its* resolution — you're a viewer of a screen, not the owner of your
own.

- The monitor is **never touched**. Punktfunk doesn't resize it, move it, disable it or restore it —
  it only watches. Keep-alive, topology and the multi-monitor layout options above simply don't
  apply, because there's no display of ours to apply them to.
- **The resolution is the monitor's**, not yours. A client asking for a different one is politely
  told no and scales its own picture — reconfiguring the display someone is sitting at would be
  rude, and the mid-stream resize machinery is switched off for this reason.
- **Every client sees the same screen.** Two clients means two viewers of one monitor, not two
  desktops.
- Naming a monitor this host **doesn't have, while it has others**, is a **hard error**, not a
  fallback: the session fails with `no monitor named "DP-9" — this host has: HDMI-1`. Showing you
  the wrong screen would be worse than showing you none. The one exception is a session with **no
  physical heads at all** — a nested or headless compositor: there the pin is set aside with a
  warning in the log and you get an ordinary virtual display, and it applies again the next time a
  session with real heads runs. Streaming the *wrong* screen is refused; streaming a virtual one
  when there's no screen to mirror is not.
- Choosing **Virtual screen (default)** in the same card puts you back on the normal path.

Supported on **KDE/KWin**, **GNOME/Mutter**, **Sway/wlroots**, **Hyprland** and **gamescope Game
Mode** (a Steam Deck / Bazzite couch box, where gamescope drives the screen) — each through the
compositor's own screen-recording API, so there is **no chooser dialog** and nothing to click. That
matters most for a host running unattended as a [service](/docs/running-as-a-service): a background
`systemd --user` daemon has nobody to answer a permission prompt, so the monitor has to be
configuration rather than a question. On gamescope only the one head the session is driving is
listed and mirrorable — mirroring attaches to the session's own composited stream, so that screen
keeps showing exactly what the person in front of it sees and nothing is relaunched. A *nested* or
headless gamescope (the per-client sessions the host spawns itself) has no head of its own, so the
picker is empty there.

### Naming the monitor from the host

Monitors are named by **connector** — `HDMI-A-1`, `DP-2`, `eDP-1`. List what this host has (these
subcommands are Linux-only, like the setting itself):

```sh
punktfunk-host list-monitors
```

```
Kwin:
  HDMI-A-1        1920x1080@60 at +0,+0    scale 1  Dell U2412M  [primary]
  DP-2            2560x1440@144 at +1920,+0  scale 1  ACME 27
```

To pin it **from the host's configuration** instead of the console — the appliance route — set it in
[`host.env`](/docs/configuration):

```sh
PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1
```

The environment variable **wins over the console setting**, deliberately: an operator who declared
the answer in the unit's environment shouldn't have it re-aimed by a click. The console shows the
Streamed screen card as locked while it's set. Leave it unset on a machine you want to steer from the
console.

Check the whole path — mirror, capture, frames — without a client involved:

```sh
punktfunk-host mirror-test --monitor HDMI-A-1 --seconds 20
```

Compositor screen recording is **damage-driven**: an idle desktop legitimately produces almost no
frames, so move the mouse on the host while it runs or a working mirror reads as a stall.

### Absolute input follows the pin

Pinning a monitor also re-aims **absolute** mouse and pen input: the host anchors it to that head's
origin, so a click lands where you point on *that* screen. It's matched by position rather than by
size, because two monitors can be the same size — and getting that wrong puts the pointer silently
on the wrong screen. The host resolves the pin at startup and again whenever the console writes it,
so a change needs no restart; the log line to look for is `capture monitor: …`.

To check it with no client involved:

```sh
punktfunk-host anchor-test --monitor HDMI-A-1
```

It lists this host's heads, says whether the box even has the same-size pair the matching exists
for, walks the pointer through the centre and the corners so you can watch which screen it moves
on, and prints the region it mapped into. `--none` runs the same walk unanchored, as an A/B.

The anchor rides the **libei** injector — the backend a GNOME/Mutter host uses. On KWin, Sway and
Hyprland the host injects through a different protocol, and `anchor-test` stops with a message
saying so rather than reporting a green run that proves nothing.

## Pick a preset

A preset is the easy way in — select one in the console and you're done. Each expands to a bundle of
the individual options documented further down.

| Preset | What it's for |
|---|---|
| **Default** | Good for most setups. Reconnects resume quickly, the streamed output becomes the whole desktop, and extra viewers each get their own screen. |
| **Headless box** | A machine with no monitor that you only ever stream from. The game and its display survive disconnects indefinitely (keep-alive **forever**), and whoever connects next takes the box over. Release it from the console when you're done. |
| **Shared desktop** | A PC you also use in person. Punktfunk never blanks your real monitors and never leaves a leftover display behind; extra viewers each get their own screen. |
| **Hot-desk** | One person at a time — roam between your own devices with an instant reconnect. Anyone else is told the box is busy, and each device+resolution keeps its own scaling. |
| **Workstation** | Your multi-monitor daily driver. Displays come back exactly where you arranged them, each client keeps its own settings, and the desktop is yours alone. |

## Save your own preset

The five above are curated starting points. When you've dialed in a setup you like — whether by
picking a preset and tweaking it or by setting every option under **Custom** — you can **save it as
your own named preset** and switch back to it in one click later.

- **Save as preset** — names the settings currently in force (all of the options below **plus**
  *Dedicated game sessions*) and adds it to the picker alongside the built-ins.
- **Apply** — selecting a saved preset writes exactly those settings, the same as picking a built-in.
- **Edit / delete** — rename a saved preset, update it to your current settings, or remove it. Deleting
  a preset never changes what's running — it only takes the card out of the picker.

Unlike the built-in presets (which deliberately leave *Dedicated game sessions* alone so switching
presets never changes your game-launch routing), a **custom preset captures your full setup**,
including that axis — because it's *your* saved configuration, not a curated behavior bundle. Custom
presets live on the host in `display-presets.json` (next to `display-settings.json`); the catalog and
the active policy are independent, so editing a preset never disturbs a running session.

## Options reference

Choose **Custom** in the console to set these directly.

### Keep alive

How long the virtual display survives after your last session disconnects. On a gamescope game host,
this also keeps the **game itself running** so you can reconnect straight back into it.

- **Off** — tear the display down at session end (nothing lingers).
- **A duration** (seconds) — keep it for that long; a reconnect inside the window drops you straight
  back in, with no re-negotiation and no desktop reshuffle.
- **Forever** — keep it until you stop the host or **release it** from the console (**Virtual
  displays** → *Release*). This is the headless-box model.

Default: **10 seconds**. Windows has always lingered 10 s; the Linux backends previously tore down
immediately — a short linger makes reconnects smoother on both.

**A reconnect always resumes the kept display** — the host recognises your device and hands back the
same display, even if you reconnect a second or two after dropping (before it has noticed you left).
**Deliberately quitting** (closing the client, not a network drop) tears the display down at once,
skipping the linger, so you don't leave a ghost behind. How quickly a *dropped* client is noticed is
the QUIC idle timeout — 8 s by default, tunable with `PUNKTFUNK_IDLE_TIMEOUT_MS` (see
[Legacy environment knobs](#legacy-environment-knobs)) if you want kept displays freed sooner.

> **Keep-alive + Exclusive keeps your physical monitors dark after you disconnect**, until the
> linger expires or you release the display. That's intentional for a dedicated gaming box, but
> don't set a long/forever keep-alive together with Exclusive on a machine whose monitors you also
> use in person — use **Shared desktop** there instead.

### Topology

What Punktfunk does with your monitor layout while it streams.

- **Extend** — add the virtual display alongside your real monitors; touch nothing else.
- **Primary** — make the virtual display your primary output; your physical monitors stay on.
- **Exclusive** — the virtual display becomes your **only** enabled output (physical monitors are
  disabled, then restored when streaming ends). This is what makes the streamed surface *be* the
  desktop, so panels and windows land on it.
- **Automatic** *(default)* — Exclusive on Windows and on an auto-detected KDE/GNOME desktop
  ("stream this desktop" means the streamed output *is* the desktop); Extend when you've pinned a
  specific compositor with `PUNKTFUNK_COMPOSITOR` (a test/CI posture).

Per-backend support:

| | KWin | Mutter/GNOME | Sway/wlroots · Hyprland | Windows |
|---|---|---|---|---|
| Extend | ✅ | ✅ | ✅ | ✅ |
| Primary | ✅ | ✅ | ⚠️ treated as Extend | ✅ |
| Exclusive | ✅ | ✅ | ✅ | ✅ |

**Primary** has no equivalent on **Sway/wlroots and Hyprland**, and that is a Wayland fact rather
than a missing feature: there is no primary-output concept to set. What these compositors do have is
a *focused* output, and the host already points that at the streamed display — once at session start,
and again immediately before it launches anything from your library. Both open a new window on the
focused monitor, so that is what puts the game on the display you're streaming. Choosing Primary
therefore behaves as Extend, and the host says so in the log.

One thing follows from it being focus rather than promotion: a window that opens *later* (a launcher
that spawns a second window, a game that re-parents itself) follows whatever has focus at that
moment, so if you're also sitting at the machine, clicking on a physical monitor mid-launch can still
pull a window over to it.

**Exclusive** does disable your physical monitors on both, and switches them back on when the last
streaming display is torn down. Two details are specific to these compositors:

- Punktfunk only ever disables monitors it did not create, so a second client streaming at the same
  time never goes dark.
- On Hyprland the restore is a `hyprctl reload`, because nothing else re-enables a monitor that a
  rule disabled — a re-applied monitor rule is accepted and ignored. That re-reads your Hyprland
  config, which is what puts your monitors back; the side effect is that any settings you changed at
  runtime with `hyprctl keyword` are dropped too, and a non-Lua config re-runs its `exec =` lines
  (`exec-once` is not re-run). This only happens if a session actually disabled something.

### Conflict handling · identity · layout

- **Conflict handling** — what happens when a *different* client connects while one is already
  streaming and asks for a different resolution: give it its own display (**separate**), take the
  box over (**steal**), share the existing display at its current mode (**join**), or refuse it
  (**reject**). On Linux, `separate` gives each client its own display on the shared desktop. On
  **Windows** a second client is **rejected** (a clean "host busy") even under `separate` — two
  clients can't yet share one virtual display's capture there (that's a later stage), so the live
  session is protected instead. A same-client *reconnect* never conflicts — it resumes.
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
  refresh**, with just the game inside. The game boots straight in — no Steam Big Picture to navigate,
  no game-mode desktop. Steam titles launch with the client hidden (`steam -silent`); non-Steam titles
  start almost instantly (gamescope up in ~1 s, then the game's own boot). Combined with **keep alive**,
  the game keeps running when you disconnect and you re-attach straight back into it.

Dedicated needs `gamescope` installed on the host; if it isn't, a launch falls back to **Auto**
routing. This axis is independent of the preset — pick it on the **Virtual displays** page. On a box
that's already in Steam game mode, a dedicated Steam launch frees game mode's Steam first and restores
it when the session ends. (GameStream / Moonlight launches follow the same routing.)

## When a game ends, and when a session does

A streaming session and the game the host launched for it can share a fate. Two switches, on the
**Virtual displays** page under **When a game or a session ends**. They apply to every store and both
protocols — and only ever to a game **this host launched for the session**: a game you started
yourself is never touched.

### When the game exits

**End the session** (default). Quit the game and your client goes back to its own library instead of
staring at your desktop. This is what a dedicated game session has always done; it now works on
every path — your live KDE/GNOME/Sway desktop, an attached gamescope, and Moonlight.

**Keep streaming** if you stream the desktop and treat the game as incidental.

### When the session ends

Whether stopping — or losing — a session also closes the game.

- **Leave it running** (default). Nothing is ever closed. Disconnect, and the game plays on for when
  you come back.
- **Close it on Stop** — closing the client, or pressing *Stop* in the console, closes the game.
  A network drop does not: you get your game back when you reconnect.
- **Always close it** — a drop closes it too, but only after a **reconnect window** (5 minutes by
  default). Reconnect inside the window and nothing happens; the console shows the countdown while
  it runs, with an **End now** button if you'd rather not wait.

Closing a game costs whatever it hadn't saved, which is why nothing closes by default. The host asks
first — a polite close, the same thing clicking the window's X does, so the game runs its own
shutdown — and only forces the issue after ten seconds of being ignored.

> **Keep alive and this setting are different clocks.** Keep-alive decides how long the *display*
> outlives a disconnect (10 s by default); the reconnect window decides how long the *game* does
> (5 minutes). A display set to **Forever** stays up regardless of what happens to the game — a
> pinned display is a deliberate "this box is a game host" choice, and closing a game doesn't undo it.

### On a gamescope session, the display has the final say

When a launch gets its **own gamescope** — a dedicated game session, the usual setup on a Steam Deck
or a Bazzite couch box — the game runs *inside* the streamed display. So it lives exactly as long as
that display does, and **Keep alive decides that, not the setting above**:

| you disconnect by | what happens to the game |
|---|---|
| pressing **Stop** (or the console's stop) | the display tears down at once — keep-alive is deliberately skipped for a real stop — and the game goes with it, even on *Leave it running* |
| dropping out (network, sleep) | the display lingers for your keep-alive window, then tears down; the game ends with it |
| dropping out, keep-alive **Forever** | the display is pinned, so the game genuinely survives — and *Always close it* still ends it when the reconnect window closes |

So on a gamescope box, "leave the game running after I disconnect" means **keep-alive Forever** (or a
window long enough to come back in), not just this setting. On a desktop session — KWin, GNOME, Sway —
the game is an ordinary process next to your desktop and none of this applies; the setting above is
the whole story.

### Automation

The host publishes `game.running` and `game.exited` events (the latter says whether the player quit
it or the host closed it), so a hook or plugin can react without polling. See
[Automation](/docs/automation).

## Persistent scaling

Set your display **scaling** once and have it stick across reconnects. This works by giving each
client a *stable display identity*, so your desktop environment keys its per-monitor settings to it.

| Host | Supported | How |
|---|---|---|
| **Windows** | ✅ today | Connect, set scaling in Settings while streaming — Windows remembers it per client. |
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

One knob has no console equivalent — it's a transport tuning, not display policy:

- **`PUNKTFUNK_IDLE_TIMEOUT_MS`** (host, default `8000`) — how long the host waits before declaring a
  *dropped* client gone, which is when a kept display starts its linger (or is freed). Lower it (e.g.
  `3000`) to reclaim kept displays sooner after an ungraceful drop; it's clamped to ≥1 s and its
  keep-alive ping scales with it, so a live session never false-disconnects. A deliberate quit is
  instant regardless. Also `--idle-timeout-ms` on `punktfunk1-host`.

## Troubleshooting

**My physical monitors stayed off after I disconnected.** You have keep-alive set together with
Exclusive topology — the display (and your isolated desktop) is being kept for the linger window.
Release it from the console (**Virtual displays**), or switch to the **Shared desktop** preset
so streaming never disables your real monitors.

**The virtual output shows only my wallpaper.** Your topology is Extend, so the streamed display is
an empty extension. Use **Primary** or **Exclusive** so your desktop actually lands on it.

**KWin can't create the virtual output.** On a normal Plasma session KWin runs its **DRM backend**,
which creates virtual outputs at any version. The 6.5.6 floor applies only to the **virtual backend**
(`kwin_wayland --virtual`, used for headless and test sessions) — below that the request fails with
"Could not find output". See [requirements](/docs/requirements).

**Reconnecting into game mode reconnects cleanly now.** On a Steam Deck / Bazzite box, disconnecting
and reconnecting within game mode reuses the still-warm session (or cleanly recreates it) instead of
landing on a dead stream — and switching between game mode and the KDE / GNOME desktop mid-stream
follows the switch. If a launched game **exits**, a dedicated session ends and returns you to your
library; a game mode / desktop session keeps streaming.

**My keep-alive / topology / layout settings do nothing.** Check whether **Streamed screen** is set
to a real monitor. Those options are all about a display Punktfunk created; when it's mirroring one
of yours there is nothing of ours to keep alive or rearrange. Switch the card back to *Virtual screen
(default)* to get them back.

**The console won't let me change Streamed screen.** `PUNKTFUNK_CAPTURE_MONITOR` is set in this
host's [`host.env`](/docs/configuration) and deliberately outranks the console. Unset it (and restart
the host) to choose from the console instead.

**My session fails with "no monitor named …".** The pinned connector isn't among this host's
monitors — it was renamed, unplugged, or the host is now in a different session. Run
`punktfunk-host list-monitors` on the host to see the real names. Punktfunk will not quietly stream a
different screen instead.

**My couch box's TV stayed on the streamed session after I disconnected.** With the **Headless box**
preset (keep alive = *forever*), a managed Steam session is held indefinitely so a reconnect resumes
instantly — return to game mode on the box (or restart the host) to hand the TV back.
