---
title: Pairing & Trust
description: How a client and host establish trust — PIN pairing once, pinned reconnects after.
---

Punktfunk has no accounts and no cloud. Trust is established directly between a client and a host,
on your network, with a one-time pairing — either an **approval click in the host's
[web console](/docs/web-console)** or a **PIN ceremony**. After that, the device reconnects
automatically on a pinned cryptographic identity.

## How it works

- Each host has a stable **identity** (a certificate). Clients remember its fingerprint, so they know
  they're talking to the same host next time.
- The first time a client connects, you **pair** it: with the native protocol the **host** shows a
  short **4-digit PIN** and you type it into the client. (With Moonlight it runs the other way round
  — Moonlight shows the PIN and you type it into the host's console.) Either way a secure exchange
  (SPAKE2) binds the two identities, and an attacker who doesn't know the PIN gets a single online
  guess — no offline cracking.
- After pairing, the host stores the client's identity in its allow-list, and the client stores the
  host's fingerprint. Reconnects are automatic — no PIN. Having seen the host on your network while
  it was awake also teaches the client its MAC address, so a later connect can
  [wake it from sleep](/docs/wake-on-lan).

## Approving a device from the console (no PIN)

The fastest way to admit a new device: just **try to connect** from it. On a pairing-required host,
the attempt shows up in the web console's Pairing page under **Waiting for approval** — with the
device's name and identity fingerprint. Click **Approve** (and optionally give it a label like
"Living Room TV"), and the device is paired on the spot: its next connect goes straight through. No
PIN to read or type.

**Deny** just dismisses the request (the device can knock again later — it's "not now", not a
blocklist). Requests expire on their own after **10 minutes**.

This works because approval happens on the host's authenticated management surface — only someone
with console access can admit a device.

## Pairing with a PIN

PIN pairing is the **default and required** path for any new host: unless the host has explicitly
opted into trust-on-first-use (see below), a client connecting to an unknown host must complete the
PIN ceremony — or be approved from the console, as above — before it can stream. It's the right path
for the *first* device (before the console has admitted anything) or when you're at the client and
the console isn't handy.

Pairing has to be **armed** on the host before a client can pair (so a random device can't pair
itself). On the production host (`serve`), this is done from the **web console**: open the
host's management console, click to arm pairing, and the host displays a 4-digit PIN along with the
list of paired devices. This works on a headless host over the network — there is no command-line flag
to arm pairing on `serve`.

The armed window lasts **2 minutes** — the console counts it down under the PIN and offers a
**Cancel** button. Arm it once you're standing at the device; if it lapses, just arm it again.

Pairing from the console needs the console running. On Linux that's the separate `punktfunk-web`
systemd user unit, which you enable once — see [The Web Console](/docs/web-console).

Then, on the client:

- **[Native clients](/docs/clients) (Apple, Linux, Windows, Android):** select the host (or use
  *Pair with PIN…* from its menu) and enter the PIN the host displays.
- **[Steam Deck](/docs/steam-deck) (the Decky plugin):** open Punktfunk from the Quick Access menu
  and pick the host — an unpaired one opens a sheet offering **Request access** (no PIN: somebody
  approves the Deck at the host) or **Use a PIN instead**, which opens the 4-digit pad.
- **[Moonlight](/docs/moonlight):** choose **Pair**; Moonlight shows a 4-digit PIN, and you type
  that PIN into the console's **Moonlight (GameStream) pairing** card and press **Submit PIN**.
  (This direction is the reverse of the native flow, and arming doesn't apply to it.)

A link can't stand in for any of this. A
[`punktfunk://` link](/docs/profiles-and-links#what-a-link-can-and-cant-do) starts a stream on a
host this device already trusts; `punktfunk://pair/…` is refused outright, and a link naming a host
you've never paired with can at most open the app's own trust prompt.

### Pairing from a terminal

On Linux the client package also installs `punktfunk`, a headless CLI. Arm pairing in the console,
read the PIN, then run:

```sh
punktfunk pair 192.168.1.50 --pin 1234 --name "Living Room"
```

It prints `paired <addr>:<port> fp=<fingerprint>` and saves the host in the same store the desktop
client uses, so later connects are silent. `--name` is the label the host files this device under
(default: this machine's name), and the port defaults to **9777** — write `host:port` to use another
one. Without `--pin` the command asks for one; in a script with no terminal it exits **6** rather
than hanging, and exit **3** means the host refused or the PIN was wrong.

The GTK client can do the same thing without opening a window:

```sh
punktfunk-client --connect 192.168.1.50:9777 --pair 1234 --name "Living Room"
```

## Requiring pairing (the default)

By default, the native host **requires** pairing — only devices that have paired can stream. This is
the right setting on a shared network: a device has to complete the PIN ceremony once before it can
connect.

If you're on a fully trusted single-user network and want to skip pairing, run the host open with
`serve --open` — it then advertises `pair=optional` and accepts unpaired clients. Requiring pairing
is strongly recommended.

## Trust-on-first-use (host opt-in)

Trust-on-first-use (TOFU) is **off by default** and is an explicit *host* opt-in for fully trusted
networks. A host enables it by running open — `serve --open` — which makes it advertise
`pair=optional` over mDNS and accept unpaired clients. Only then does a client offer the
TOFU path: connecting to such a host for the first time shows the host's fingerprint and asks you to
confirm it (compare it with the one the host logged at startup), then pins it. The client presents
this clearly as the reduced-security option, alongside **Pair with PIN**.

> **Warning:** TOFU cannot detect an impostor on the first connection — if someone is impersonating
> the host the very first time you connect, you'll pin the attacker's fingerprint. PIN pairing closes
> that gap (the SPAKE2 ceremony binds both identities), which is why it's the default. Use TOFU only
> on a network you fully trust — see [Security & Safe Use](/docs/security).

For every other case — a host advertising `pair=required` (the default), a host you typed in by hand,
or a discovered host whose pair policy is unknown — TOFU is not offered and the client routes straight
to the PIN ceremony.

Once a host is pinned, a fingerprint change is treated as the impostor signal: the client forces
re-pairing through the PIN ceremony rather than offering to re-trust the new identity.

## Managing paired devices

The [web console](/docs/web-console) lists every paired device and lets you remove one (revoking its
access). Re-pairing is just the PIN ceremony again.

If a client can't pair at all, see [Troubleshooting → Pairing is
rejected](/docs/troubleshooting#pairing-is-rejected--the-client-cant-connect).

(There is also a developer/measurement host, `punktfunk-host punktfunk1-host` — a subcommand of the
same binary, not the host you install. It has its own `--allow-tofu` / `--pairing-pin` flags for test
harnesses; nothing on this page applies to it.)
