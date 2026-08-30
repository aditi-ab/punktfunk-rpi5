---
title: Host power
description: Sleep, restart or shut down the host from the web console or a paired client — who may do it, what happens to running streams, and why an action can be refused.
---

[Wake-on-LAN](/docs/wake-on-lan) lets every client wake a sleeping host. Host power closes that
loop: **Sleep host**, **Restart host** and **Shut down host**, from the web console's Host page
(password-confirmed) or — with the right access — from a paired client. Finish playing on the TV,
sleep the host from the couch, wake it again tomorrow.

On a client the rows sit in the host's own menu, right where **Wake host** appears when it is
asleep: the gamepad console's host options, the Linux and Windows host card menus, and the Apple
and Android host cards. They appear only when the host offered them, so a device without the
grant simply has no power rows. Restart and shut down confirm before they run.

## Who may do it

- The **web console** always can — it is the operator's own surface, behind the console login
  plus a per-action password confirmation.
- A **paired device** needs the **Host power** grant ([access levels](/docs/access-levels)).
  Full-control devices have it — a device with keyboard access could already reach the desktop's
  power menu, so Full control saying otherwise would be a fake distinction. Controller-only and
  view-only devices do not, and cannot get it without the operator editing their access.

## What happens

On accept the host first ends every streaming session cleanly — clients show *"the host is going
to sleep or shutting down"* rather than a connection error — waits a moment so the reply reaches
the invoker, then asks the operating system to act. The host tile flips to asleep/offline, and for
sleep the **Wake host** action brings it back.

It also tears the virtual displays down before it goes, so a wake starts clean instead of resuming
onto a display built for whoever put the machine to sleep. A display on keep-alive **Forever** is
deliberately kept — that pin is what keeps a gamescope game running across disconnects — so if you
use one and a wake lands on a black screen, **release it** from the console (**Virtual displays**
→ *Release*).

## Why an action can be refused

The host says no, with the reason, instead of pretending:

- **Another device is streaming** — a granted guest cannot pull the host out from under someone
  else's live session. Your own session never blocks you. The console is never blocked; it warns
  instead.
- **The platform said no** — a Linux host honors other programs' suspend inhibitors and refuses
  while a second local user is logged in; a machine that cannot suspend lists Sleep as
  unavailable with the reason. Punktfunk deliberately does not force past these.
- On Linux the host user must be in group `punktfunk` — the same polkit rule the packages
  install for unattended power operations. The console lists the action as unavailable with a
  hint when the group is missing.

Moonlight/GameStream clients have no vocabulary for this — host power is a native-protocol (and
console) feature.
