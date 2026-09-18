---
title: Access levels
description: What each paired device may do, and for how long — the three presets, the advanced toggles, temporary access that expires on its own, and what access control honestly does not cover.
---

Pairing used to be all-or-nothing: a paired device had full control of the host, forever. Now every
paired device carries an **access level** — what it may send to the host — and optionally an
expiry: a friend's phone as a second controller for the evening, a TV that can play but never type,
a spectator who only watches. A friend who is not on your LAN needs one more thing — a way to
reach the host and nothing else — see [Friends over the internet](/docs/friends-over-the-internet).

Access is **enforced by the host**. A client's UI reflects its access as a courtesy, but the host
drops anything a device isn't granted regardless of what the client sends — nothing a client sends
can widen its own access.

You manage access from the host's [web console](/docs/web-console): when you
[approve a device or arm pairing](/docs/pairing#choosing-access-when-you-admit-a-device), and any
time after on the **Paired devices** table, where each device shows an **Access** chip (with a live
countdown if it expires) and an edit sheet.

## The three presets

| Access level | What the device can do |
|---|---|
| **Full control** | Everything — keyboard, mouse, controllers, clipboard, microphone, launching games. What pairing has always meant, and still the default: every device paired before access levels existed keeps full control, and so does a plain **Approve**. |
| **Controller only** | Gamepad input only — the guest and co-play preset. The device's pads show up as additional controllers (with rumble and pad audio), but it cannot type, move the mouse, read the clipboard, use the mic, or launch anything. |
| **View only** | See and hear the stream, send nothing. The spectator preset. |

The preset label is derived from the underlying toggles, so a hand-tuned combination shows as
**Custom** — there is no separate thing to keep in sync.

## The advanced toggles

Each preset is a bundle of independent grants, under **Advanced** in the edit sheet:

| Toggle | Covers |
|---|---|
| **Gamepad** | Controller buttons, sticks and motion, plus everything that rides with a pad: virtual pad creation on the host, rumble back to the client, pad audio. |
| **Pointer** | Mouse (relative and absolute), scroll, touch, and pen input. |
| **Keyboard** | Key presses. |
| **Clipboard** | The [shared clipboard](/docs/clipboard). Both switches still apply: the host operator's clipboard policy *and* this grant have to allow it — the grant can only narrow, never widen, what the operator permits. An ungranted device gets a clean "not permitted" instead of a toggle that silently does nothing. |
| **Microphone** | Sending the client's microphone to the host. Without it, the session never attaches to the host's mic service at all. |
| **Launch** | Starting a game from the host's [library](/docs/game-library) when connecting. Without it, a connect that asks to launch is refused with a clear error rather than dropped onto the bare desktop. The library stays *visible* — this governs launching, not browsing. |
| **Host power** | Sleeping, restarting or shutting down the host machine from the client (see [Host power](/docs/host-power)). Included in Full control on purpose: a device with Keyboard and Pointer can already reach the desktop's own power menu, so withholding only the polite path would be a lock painted on an open door. The bit's real job is keeping power away from *limited* devices — the controller-only guest and the view-only spectator cannot touch it. |

**Controller only deliberately does not include Launch**: in co-play the owner drives what runs.
Want a guest picking games? Turn on that one Advanced toggle.

## Couch co-op over JOIN

Two people, one screen, one game — the second player on their own phone or PC. Pair the guest's
device **Controller only**, have them connect to the host you are already streaming from, and the
host puts them on the *same* display instead of opening a second one (the session list calls that
**Joined another session**). Their controllers arrive as extra controllers on that desktop, with
rumble and pad audio, and nothing else they send lands. From the game's side it is simply two pads
on one PC, which is what local co-op wants.

**Who is Player 1** is a host decision, because a controller's OS slot is what a local co-op game
reads and each client numbers its own pads from zero. Left alone, the slot goes to whichever pad
appears first, so the order changes every time someone reconnects. The Dashboard's **Sessions**
card carries a player picker on each row: pick **Player 1–4** and that session's controllers take
that slot. The pick is remembered against the device's pairing, so the same device is the same
player next time it connects — it follows the pairing, not an address.

Two things the picker deliberately does not do. A controller that is already plugged in keeps the
slot it was created under until it re-plugs, because renumbering a live pad is an unplug in front
of the game. And a slot another live session asked for first stays theirs — the picker says so
rather than taking it. A session with no pick keeps the old behaviour: first free slot.

Up-to-date clients name the player themselves: the [stats overlay](/docs/stats) leads with
*"player 2"* whenever the session holds a controller, so the guest can check without asking.

This is a *shared* desktop, so everything under
[what access levels do not cover](#what-this-does-not-cover) still applies: the guest sees and
hears your whole screen, and game audio is one stream both of you hear.

A session's quality controls — resolution, bitrate, keyframe requests — are *not* governed. They
only shape that device's own stream; restricting them would cost usability and buy no security.

## Temporary access

Any grant can carry an expiry, picked when you approve the device or set later in its edit sheet:
**1 h / 4 h / 8 h / custom / forever**.

- Expiry is **wall-clock time on the host** — "4 hours" means four hours from now by the host's
  clock.
- A device streaming when its access runs out gets **warnings at 5 minutes and 1 minute** before
  the deadline, then its session ends with an explicit reason: *"Your access to this host has
  expired."* Only that device's sessions end — yours is untouched.
- An expired device is **not unpaired into oblivion**. It stays listed as **Expired** in the
  Paired devices table, and when it next tries to connect it appears under **Waiting for
  approval** like a new device — re-granting is one click, with the same access dialog.
- **Extend** and **Expire now** live in the edit sheet, and both hit live sessions immediately:
  extending re-arms the running session's deadline, and Expire now ends it with the same clean
  "access expired" message — no lingering stream.

Other edits are just as immediate: changing a device's access level while it streams takes effect
within moments, and removing the device ends its sessions. Access is per *device*, not per session
— two sessions from the same device share one grant.

## What this does not cover

Three limits before relying on access levels:

> **A view-only guest still sees your whole desktop.** On the shared-desktop backends every
> session shows the *same* desktop — access levels govern what a device can send *in*, not what it
> sees going *out*. A view-only or controller-only guest watches and hears everything you do,
> notifications included.

- **Moonlight / GameStream devices are not governed yet.** Access levels apply to the native
  Punktfunk protocol. A device paired via [Moonlight](/docs/moonlight) has full control and shows
  an honest **Full (ungoverned)** chip in the console — not a fake editor. When enforcement reaches
  the GameStream plane it will be *silent* from the client's side: the protocol has no way to tell
  a Moonlight client about its access, so an ungranted keyboard will simply be inert, with the
  explanation visible only in the console.
- **Older Punktfunk clients are enforced, but can't explain it.** The host enforces access
  identically for every client version; a client from before this feature just lacks the chrome:
  no "Controller only · ends in 2 h" chip, no expiry warnings, a generic disconnect instead of
  "access expired" — and an ungranted keyboard is silently inert rather than never captured. If a
  guest reports "my keyboard does nothing", check their access level in the console first, then
  whether their client is current.

Up-to-date native clients get the chrome: they stop capturing what can't land (no keyboard grab
without the Keyboard grant), hide the clipboard and mic controls when ungranted, show a small
overlay chip naming the session's access and time remaining, and surface the expiry warnings as
toasts.

The chip rides the [stats overlay](/docs/stats) — it is there at every tier above off, and gone
with the overlay off. A guest who wants to check what they are allowed to do brings it up the same
way they bring up the stats. The expiry warnings are separate: they are toasts, they announce a
change rather than describe a state, and they appear whatever the overlay is set to.

## Changing access while someone is streaming

The Dashboard's **Sessions** card carries an access picker on each row. It changes the session in
front of you — the mask the host checks every event against — with no reconnect: hand a
view-only friend the pad, take it back when your turn comes round. An up-to-date client's access
chip follows within the same second.

This is the *session*, not the pairing. The device's stored access is untouched, so the change
lasts until that session ends, and an edit to the device's own access overrides it. The picker can
only re-point **within** the pairing: asking for more than the device is paired for lands on what
the pairing allows, not on what was asked. Widening the device itself is still the **Paired
devices** sheet, behind the console login.

## Where enforcement happens

The host checks every input event against the session's live grants before injecting it, refuses
ungranted planes at session setup (no Gamepad grant means the virtual pads are never created; no
Microphone grant means the mic plane never attaches), and re-pairing a device **preserves** its
existing access — the only way to widen a grant is the console's own dialogs, behind the console
login. Dropped traffic is logged once per session and category, not per event, so a misbehaving
client can't flood the log. See [Security & Safe Use](/docs/security) for the wider picture.
