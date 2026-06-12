---
title: Pairing & Trust
description: How a client and host establish trust — PIN pairing once, pinned reconnects after.
---

punktfunk has no accounts and no cloud. Trust is established directly between a client and a host, on
your network, with a one-time **PIN pairing**. After that, the device reconnects automatically on a
pinned cryptographic identity.

## How it works

- Each host has a stable **identity** (a certificate). Clients remember its fingerprint, so they know
  they're talking to the same host next time.
- The first time a client connects, you **pair** it: the host shows a short **4-digit PIN**, you type
  it into the client, and a secure exchange (SPAKE2) binds the two identities. An attacker who doesn't
  know the PIN gets a single online guess — no offline cracking.
- After pairing, the host stores the client's identity in its allow-list, and the client stores the
  host's fingerprint. Reconnects are automatic — no PIN.

## Arming pairing on the host

Pairing has to be **armed** on the host before a client can pair (so a random device can't pair
itself). Two ways:

- **Web console** *(recommended)* — open the host's management console, click to arm pairing, and it
  shows the PIN and the list of paired devices. This is the easiest way and works on a headless host
  over the network.
- **Command line** — start the host with `--allow-pairing` (or `--require-pairing`); it prints a PIN
  in its log when a client begins pairing.

Then, on the client:

- **Apple app:** select the host (or use *Pair with PIN…* from its menu) and enter the PIN.
- **Moonlight:** choose **Pair**; Moonlight shows the PIN to confirm on the host side.

## Requiring pairing (the default)

By default, the native host **requires** pairing — only devices that have paired can stream. This is
the right setting on a shared network: a device has to complete the PIN ceremony once before it can
connect.

If you're on a fully trusted single-user network and want to skip pairing, the host can be run open —
but requiring pairing is strongly recommended.

## Trust-on-first-use

If a host *isn't* requiring pairing, a client connecting for the first time will show the host's
fingerprint and ask you to confirm it (trust-on-first-use), then pin it. Pairing is the stronger path
and the default; trust-on-first-use is a convenience for trusted setups.

## Managing paired devices

The web console lists every paired device and lets you remove one (revoking its access). Re-pairing is
just the PIN ceremony again.
