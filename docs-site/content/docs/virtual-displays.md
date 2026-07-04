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

> **What's live today:** this release wires **keep-alive** (linger duration) and **topology**
> (extend / primary / exclusive). The other options below — conflict handling, identity/scaling
> persistence on Linux, and multi-monitor layout — are **stored but not yet enforced**; they arrive
> in following releases. The console marks them accordingly. Windows already persists per-client
> scaling (see [Persistent scaling](#persistent-scaling)).

## Pick a preset

A preset is the easy way in — select one in the console and you're done. Each expands to a bundle of
the individual options documented further down.

| Preset | What it's for |
|---|---|
| **Default** | Today's behavior. A short linger absorbs reconnects, the streamed output becomes the sole desktop, and extra clients each get their own view. |
| **Gaming rig** | A dedicated couch/headless box. The game and its display survive disconnects indefinitely, and whoever connects takes the box over. *(Arrives with the keep-alive stage.)* |
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
- **Forever** — keep it until you stop the host or release it from the console. *(Arrives with the
  keep-alive lifecycle stage; the console won't let you save it before then.)*

Default: **10 seconds**. Windows has always lingered 10 s; the Linux backends previously tore down
immediately — a short linger makes reconnects smoother on both.

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
| Primary | ✅ | ✅ | ⚠️ treated as Extend | ✅ *(following release)* |
| Exclusive | ✅ | ✅ | ✅ *(following release)* | ✅ |

### Conflict handling · identity · layout

These are **stored but not yet enforced** — they're documented here so you know what's coming and
can set them ahead of the release that turns them on:

- **Conflict handling** — what happens when a *different* client connects while one is already
  streaming and asks for a different resolution: give it its own display (**separate**), take the
  box over (**steal**), share the existing display at its current mode (**join**), or refuse it
  (**reject**).
- **Identity** — whether each client gets a **stable display identity** so your desktop environment
  remembers its settings (see below): one shared identity, one **per client**, or one **per client +
  resolution**.
- **Layout / max displays** — how multiple virtual displays are arranged (for multi-monitor), and an
  upper bound on how many can be live at once.

## Persistent scaling

Set your display **scaling** once and have it stick across reconnects. This works by giving each
client a *stable display identity*, so your desktop environment keys its per-monitor settings to it.

| Host | Supported | How |
|---|---|---|
| **Windows** | ✅ today | Connect, set scaling in Settings while streaming — Windows remembers it per client. |
| **KDE / KWin** | ⏳ following release | A stable per-client output name lets KWin persist scale/mode per client. |
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

## Troubleshooting

**My physical monitors stayed off after I disconnected.** You have keep-alive set together with
Exclusive topology — the display (and your isolated desktop) is being kept for the linger window.
Release it from the console (Host → *Virtual displays*), or switch to the **Shared desktop** preset
so streaming never disables your real monitors.

**The virtual output shows only my wallpaper.** Your topology is Extend, so the streamed display is
an empty extension. Use **Primary** or **Exclusive** so your desktop actually lands on it.

**KWin virtual outputs need KWin ≥ 6.5.6.** Older KWin can't create the virtual output at all —
see [requirements](/docs/requirements).
