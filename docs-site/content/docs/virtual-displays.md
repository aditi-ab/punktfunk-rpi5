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
> Reach for a preset when you want a specific experience — a dedicated couch/gaming box, a desktop
> you also use in person, or a multi-monitor workstation.

> **What's live today:** **keep-alive** (linger, or **forever**), **topology** (extend / primary /
> exclusive), **conflict handling**, **per-client identity + persistent scaling** (Windows *and*
> KDE/KWin), and **multi-monitor layout** (several clients as monitors of one desktop) are all
> enforced. A reconnect always resumes the kept display — even a fast one — instead of spawning a
> second. The remaining gaps are noted inline: the Linux `primary` physical-keep *effect*, Sway
> `exclusive`, and multi-display for a *single* client (that last is the next stage).

## Pick a preset

A preset is the easy way in — select one in the console and you're done. Each expands to a bundle of
the individual options documented further down.

| Preset | What it's for |
|---|---|
| **Default** | Today's behavior. A short linger absorbs reconnects, the streamed output becomes the sole desktop, and extra clients each get their own view. |
| **Gaming rig** | A dedicated couch/headless box. The game and its display survive disconnects indefinitely (keep-alive **forever**), and whoever connects takes the box over. Release it from the console when you're done. |
| **Shared desktop** | A desktop you also use in person. punktfunk never blanks your real monitors and never leaves a ghost display behind; concurrent viewers each get a view. |
| **Hot-desk** | One user at a time with fast reattach — roaming between your own devices. A second user is told the box is busy, and each device+resolution keeps its own scaling. |
| **Workstation** | The multi-monitor daily driver. Your displays come back exactly where you arranged them, with per-client identity and an exclusive desktop. |

## Options reference

Choose **Custom** in the console to set these directly.

### Keep alive

How long the virtual display survives after your last session disconnects. On a gamescope game host,
this also keeps the **game itself running** so you can reconnect straight back into it.

- **Off** — tear the display down at session end (nothing lingers).
- **A duration** (seconds) — keep it for that long; a reconnect inside the window drops you straight
  back in, with no re-negotiation and no desktop reshuffle.
- **Forever** — keep it until you stop the host or **release it** from the console (Host → *Virtual
  displays* → *Release*). This is the gaming-rig model.

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

| | KWin | Mutter/GNOME | Sway/wlroots | Windows |
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

## Persistent scaling

Set your display **scaling** once and have it stick across reconnects. This works by giving each
client a *stable display identity*, so your desktop environment keys its per-monitor settings to it.

| Host | Supported | How |
|---|---|---|
| **Windows** | ✅ today | Connect, set scaling in Settings while streaming — Windows remembers it per client. |
| **KDE / KWin** | ✅ today | Set scaling in System Settings while streaming; KWin keys it to a stable per-client output name and reapplies it on reconnect. Validated live (150 %/125 % survive a full disconnect + reconnect). |
| **GNOME / Mutter** | ❌ | GNOME's virtual-monitor API exposes no stable identity to key config on. |
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
