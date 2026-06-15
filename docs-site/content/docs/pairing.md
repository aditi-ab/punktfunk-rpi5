---
title: Pairing & Trust
description: How a client and host establish trust — PIN pairing once, pinned reconnects after.
---

punktfunk has no accounts and no cloud. Trust is established directly between a client and a host,
on your network, with a one-time pairing — either an **approval click in the host's console** or a
**PIN ceremony**. After that, the device reconnects automatically on a pinned cryptographic
identity.

## How it works

- Each host has a stable **identity** (a certificate). Clients remember its fingerprint, so they know
  they're talking to the same host next time.
- The first time a client connects, you **pair** it: the host shows a short **4-digit PIN**, you type
  it into the client, and a secure exchange (SPAKE2) binds the two identities. An attacker who doesn't
  know the PIN gets a single online guess — no offline cracking.
- After pairing, the host stores the client's identity in its allow-list, and the client stores the
  host's fingerprint. Reconnects are automatic — no PIN.

## Approving a device from the console (no PIN)

The fastest way to admit a new device: just **try to connect** from it. On a pairing-required host,
the attempt shows up in the web console's Pairing page under **Waiting for approval** — with the
device's name and identity fingerprint. Click **Approve** (and optionally give it a label like
"Living Room TV"), and the device is paired on the spot: its next connect goes straight through. No
PIN to read or type.

**Deny** just dismisses the request (the device can knock again later — it's "not now", not a
blocklist). Requests expire on their own after a few minutes.

This works because approval happens on the host's authenticated management surface — only someone
with console access can admit a device.

## Pairing with a PIN

The PIN ceremony is the other path — useful for the *first* device (before the console has admitted
anything) or when you're at the client and the console isn't handy.

Pairing has to be **armed** on the host before a client can pair (so a random device can't pair
itself). On the production host (`serve --native`), this is done from the **web console**: open the
host's management console, click to arm pairing, and the host displays a 4-digit PIN along with the
list of paired devices. This works on a headless host over the network — there is no command-line flag
to arm pairing on `serve`.

(The standalone headless test host, `m3-host`, takes `--allow-pairing`/`--require-pairing` on its
command line instead; the production `serve --native` host arms pairing from the console.)

Then, on the client:

- **Apple app:** select the host (or use *Pair with PIN…* from its menu) and enter the PIN.
- **Moonlight:** choose **Pair**; Moonlight shows the PIN to confirm on the host side.

## Requiring pairing (the default)

By default, the native host **requires** pairing — only devices that have paired can stream. This is
the right setting on a shared network: a device has to complete the PIN ceremony once before it can
connect.

If you're on a fully trusted single-user network and want to skip pairing, start the host with
`serve --native --open` — but requiring pairing is strongly recommended.

## Trust-on-first-use

If a host *isn't* requiring pairing, a client connecting for the first time will show the host's
fingerprint and ask you to confirm it (trust-on-first-use), then pin it. Pairing is the stronger path
and the default; trust-on-first-use is a convenience for trusted setups.

## Managing paired devices

The web console lists every paired device and lets you remove one (revoking its access). Re-pairing is
just the PIN ceremony again.
