---
title: Virtual displays
description: Control how punktfunk creates, keeps alive, and arranges the virtual displays it streams — presets, keep-alive, exclusive vs. extend, and persistent per-client scaling.
---

When a client connects, punktfunk creates a **virtual display** sized to exactly that client's
resolution and refresh, renders your desktop or game onto it, and streams it. This page is about the
**policy** for that display: how long it survives a disconnect, whether it takes over your physical
monitors, what happens when a second client connects, and how desktop environments remember
per-client settings like scaling.

You set this policy in the **web console** (Host → *Virtual displays*), or by editing
`~/.config/punktfunk/display-settings.json` directly (`%ProgramData%\punktfunk\display-settings.json`
on Windows). A change applies to the **next** connection — a running session keeps the display it
opened on.

> **You rarely need to touch this.** The default behavior matches how punktfunk has always worked.
> Reach for a preset when you want a specific experience — a dedicated box you only stream from, a
> desktop you also use in person, or a multi-monitor workstation.

> **What's live today:** **keep-alive** (linger, or **forever**), **topology** (extend / primary /
> exclusive), **conflict handling**, **per-client identity + persistent scaling** (Windows, KDE/KWin
> *and* GNOME/Mutter), and **multi-monitor layout** (several clients as monitors of one desktop) are
> all enforced. A reconnect always resumes the kept display — even a fast one — instead of spawning a
> second. The remaining gaps are noted inline: the Linux `primary` physical-keep *effect*, Sway
> `exclusive`, and multi-display for a *single* client (that last is the next stage).

## Pick a preset

A preset is the easy way in — select one in the console and you're done. Each expands to a bundle of
the individual options documented further down.

| Preset | What it's for |
|---|---|
| **Default** | Good for most setups. Reconnects resume quickly, the streamed output becomes the whole desktop, and extra viewers each get their own screen. |
| **Headless box** | A machine with no monitor that you only ever stream from. The game and its display survive disconnects indefinitely (keep-alive **forever**), and whoever connects next takes the box over. Release it from the console when you're done. |
| **Shared desktop** | A PC you also use in person. punktfunk never blanks your real monitors and never leaves a leftover display behind; extra viewers each get their own screen. |
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
- **Forever** — keep it until you stop the host or **release it** from the console (Host → *Virtual
  displays* → *Release*). This is the headless-box model.

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

What punktfunk does with your monitor layout while it streams.

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
| Exclusive | ✅ | ✅ | ⏳ following release | ✅ |

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
  each client), up to **max displays**. Arrange them under Host → *Virtual displays* once two or more
  are streaming.

### Dedicated game sessions

**Dedicated game sessions** control how a session that *launches a game from your library* is served
(Linux hosts):

- **Auto** (default) — the launch rides whatever session the box is in: the managed Steam session on a
  Steam Deck / Bazzite couch box, a bare gamescope on a plain distro, or spawned into your live KDE /
  GNOME / Sway desktop.
- **Dedicated** — every library launch gets its **own headless gamescope at your exact resolution and
  refresh**, with just the game inside. The game boots straight in — no Steam Big Picture to navigate,
  no game-mode desktop. Steam titles launch with the client hidden (`steam -silent`); non-Steam titles
  start almost instantly (gamescope up in ~1 s, then the game's own boot). Combined with **keep alive**,
  the game keeps running when you disconnect and you re-attach straight back into it; when you quit the
  game, the session ends cleanly and your client returns to its library.

Dedicated needs `gamescope` installed on the host; if it isn't, a launch falls back to **Auto**
routing. This axis is independent of the preset — pick it under Host → *Virtual displays*. On a box
that's already in Steam game mode, a dedicated Steam launch frees game mode's Steam first and restores
it when the session ends. (GameStream / Moonlight launches follow the same routing.)

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
Release it from the console (Host → *Virtual displays*), or switch to the **Shared desktop** preset
so streaming never disables your real monitors.

**The virtual output shows only my wallpaper.** Your topology is Extend, so the streamed display is
an empty extension. Use **Primary** or **Exclusive** so your desktop actually lands on it.

**KWin virtual outputs need KWin ≥ 6.5.6.** Older KWin can't create the virtual output at all —
see [requirements](/docs/requirements).

**Reconnecting into game mode reconnects cleanly now.** On a Steam Deck / Bazzite box, disconnecting
and reconnecting within game mode reuses the still-warm session (or cleanly recreates it) instead of
landing on a dead stream — and switching between game mode and the KDE / GNOME desktop mid-stream
follows the switch. If a launched game **exits**, a dedicated session ends and returns you to your
library; a game mode / desktop session keeps streaming.

**My couch box's TV stayed on the streamed session after I disconnected.** With the **Headless box**
preset (keep alive = *forever*), a managed Steam session is held indefinitely so a reconnect resumes
instantly — return to game mode on the box (or restart the host) to hand the TV back.
