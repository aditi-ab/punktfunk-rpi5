---
title: "gamescope Multi-User Isolation (deferred)"
description: "Research + design for concurrent INDEPENDENT gamescope desktops (multi-user), and why it's deferred. The shared-desktop multi-view case already landed."
---

**Status: deferred (2026-06-12).** Concurrent sessions landed for the **shared-desktop multi-view**
case — multiple devices viewing/controlling the *same* KWin/Mutter/wlroots desktop ([Status](/docs/status)).
This page captures the research for the *other* model — **independent desktops** (each client its own
gamescope instance: the multi-user / cloud-gaming-on-one-box case) — and why it's parked. Pick this
up from here if the use case becomes a priority.

## What landed vs what this is

| Model | Backends | Input | Audio | Status |
|---|---|---|---|---|
| **Shared-desktop multi-view** | kwin / mutter / wlroots | shared (all drive one desktop) | shared (all hear one desktop) | ✅ **landed** — correct semantics: stream *your* desktop to laptop + TV at once |
| **Independent desktops (multi-user)** | gamescope | **per-session** (each drives its own game) | **per-session** | ⏸ **deferred** — this page |

For independent desktops, shared input/audio is *wrong* — each user must drive and hear only their own
session. gamescope is the natural fit: each `create()` spawns a fresh nested compositor (own
rendering, own EIS input socket). The blocker is that the host's input/audio/mic are host-lifetime
**shared** services, and the gamescope EIS socket is relayed through a single global file.

## Current architecture (the research)

Each gamescope **process is per-session** (`vdisplay/gamescope.rs::create()` spawns one; the
`VirtualOutput.keepalive` owns it). But:

- **EIS input socket — single global file.** gamescope exports `LIBEI_SOCKET` for its children; a
  shell wrapper relays it to the fixed path `/tmp/punktfunk-gamescope-ei` (`EI_SOCKET_FILE`).
  **Two concurrent instances overwrite each other's socket name** in that one file.
- **Injector — one host-lifetime `!Send` service.** `m3.rs::InjectorService` opens **one**
  `inject::open(backend)` for the whole run and forwards events over an mpsc channel. It was made
  shared deliberately (the portal `CreateSession` churn wedged KWin's EIS — "EIS setup timed out").
  For gamescope it reads the one global socket file, so all sessions' input lands in whichever
  instance wrote last.
- **Audio — global default-sink monitor.** `audio::open_audio_capture()` sets
  `STREAM_CAPTURE_SINK` and autoconnects to the host's **default sink monitor** (PW_ID_ANY) — the
  whole system's output, not a per-gamescope node. gamescope exposes **no per-instance audio node**.
- **Mic — one global `Audio/Source`.** `MicService` feeds one PipeWire source named `punktfunk-mic`;
  all clients' mic uplinks mix into it.
- Per-session already (no work): the gamescope process, the PipeWire video node, and the uinput
  gamepads.

## What it would take

1. **Per-instance EIS socket** — give each gamescope a unique relay file
   (`/tmp/punktfunk-gamescope-{id}-ei`) and carry the path on `VirtualOutput` (new field) so the
   session can find its own socket.
2. **Per-session injector** — for gamescope sessions, create a **per-session** injector bound to that
   socket (its own thread, since `InputInjector` is `!Send`), instead of the shared `InjectorService`.
   Keep the shared service for the portal backends (kwin/mutter) where shared input is correct.
   Ordering nuance: the input thread is wired before the gamescope socket exists, so the per-session
   injector must open **lazily** (on first event, by which time gamescope is up) or be created after
   `build_pipeline`.
3. **Per-session audio (the bigger piece).** gamescope has no per-instance audio node, but audio
   *is* isolatable: create a **per-session PipeWire null-sink**, route that gamescope's apps to it
   (`PULSE_SINK` / a target node on the spawn env), and capture **that sink's monitor** per session.
   This is the largest addition — null-sink create/teardown + routing + per-session capture.
4. **Per-session mic** — a virtual `Audio/Source` per session (`punktfunk-mic-{id}`), routed into
   that gamescope, instead of the one global source.

## Why deferred

- It's a **large multi-file refactor** — the whole input path (per-instance sockets + per-session
  injector + the lazy-open ordering), **plus** per-session null-sink audio routing, **plus** per-session
  mic — for a **niche** use case (multiple independent users gaming on one box).
- The **common** concurrency case — stream one desktop to several of *your own* devices — is the
  shared-desktop multi-view model, which **already landed and is the correct semantics** for it.
- No correctness gap in what shipped: concurrent sessions work today; this is purely the *additional*
  independent-desktops model.

Revisit when there's a real multi-user requirement. The plumbing list above is the whole job.
