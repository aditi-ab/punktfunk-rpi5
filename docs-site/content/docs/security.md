---
title: Security & Safe Use
description: What a streaming host actually exposes, why to keep it on a trusted network, and how punktfunk protects you.
---

Read this before you put a host on a network you don't fully control. punktfunk is built to be secure
**on a trusted local network**, and that's the setting we support today. This page is upfront about what
a streaming host is, what protects it, and where the honest limits are.

> **The short version**
> - **Keep the host on a network you trust** — your home LAN, or a private VPN that puts host and client
>   on the same subnet. **Do not port-forward it to the public internet.**
> - **A streaming host is remote control of the machine.** Anyone who can stream to it sees the screen
>   and can move the mouse, type, and act as a controller — the same as sitting at the keyboard.
> - **Pairing is the security boundary.** Require pairing (the default), pick a strong console
>   password, and review your paired devices from time to time.
> - **Be thoughtful about *which* machine you run it on** — especially on Windows, where the host runs
>   with high system privileges so it can do its job. Prefer a dedicated or gaming PC over one holding
>   your most sensitive data.

## What a streaming host really is

Low-latency desktop and game streaming means two things travel over the network: **the screen goes
out, and input comes back in.** A paired client doesn't just watch — it drives. Its mouse, keyboard,
and controller are injected into the host's desktop, so **for anything it can reach, a streaming client
is equivalent to a person sitting at that machine.**

That's the feature. It's also the risk to understand:

- The host can capture the **secure desktop** — UAC elevation prompts and the lock screen — so a
  connected client can see and interact with those too. (This is what lets you unlock and administer a
  headless box remotely; it's the same capability Sunshine and Apollo provide.)
- Injected input isn't sandboxed to a game. Whoever is streaming can alt-tab, open a terminal, read
  files, or change settings — whatever the logged-in session can do.

This is true of **every** remote-access and game-streaming tool, not just punktfunk. The takeaway isn't
"don't use it" — it's "treat access to your host the way you'd treat handing someone your unlocked
keyboard." The rest of this page is about making sure only people you intend can get that access.

## Keep it on a trusted network

**punktfunk assumes a trusted local network. It is not designed, tested, or hardened to be exposed to
the public internet — do not port-forward it.** There is no WAN-hardening story yet: no rate-limited
public authentication gateway, no DDoS protection, no assumption that hostile traffic is constantly
probing the ports. Exposing the streaming ports directly to the internet puts an interactive
control surface for your machine in front of the entire world.

If you want to stream from outside your home, tunnel in instead of opening up:

- **Use a VPN** — WireGuard, Tailscale, or your router's built-in VPN. This puts your remote client on
  the *same private subnet* as the host, so from punktfunk's point of view it's still a local
  connection, and the tunnel (not punktfunk) handles internet-facing authentication and encryption.
  Discovery, pairing, and streaming then work exactly as they do at home.
- **Don't** map a router port to the host. A port-forward turns "trusted LAN service" into
  "internet-facing service" with none of the protections that implies.

A note for **portable machines**: the installer opens the streaming ports on the firewall for *all*
network profiles, including Public. That's convenient at home but means that if you take a laptop host
onto an untrusted network — a café, a hotel, a conference — other devices on that network can reach the
ports and attempt to pair. Pairing still protects you (an attacker who doesn't know the PIN can't get
in), but the safest habit is to stop the host service, or firewall it off, when you're on a network you
don't control.

## What actually protects you

punktfunk has **no accounts and no cloud**. Trust is established directly, device-to-device, and then
pinned. The layers, from the outside in:

- **Pairing is required by default.** A new device can't stream until it completes a one-time
  **PIN pairing ceremony** (SPAKE2): the host shows a 4-digit PIN, you enter it on the client, and the
  exchange cryptographically binds both identities. An attacker who doesn't know the PIN gets a
  *single online guess* — no offline cracking, no dictionary attack. See
  [Pairing & Trust](/docs/pairing).
- **Identities are pinned.** After pairing, the client remembers the host's certificate fingerprint and
  the host stores the client's. Reconnects are automatic and mutually authenticated; if a host's
  fingerprint ever changes, the client refuses to auto-trust it and forces re-pairing.
- **The admin surface is loopback-only.** The management API's read-only status is reachable by paired
  clients over the LAN (authenticated by their certificate), but every state-changing action — arming
  pairing, removing devices, session control — is honored **only from the local machine** (the web
  console connects over loopback). It is never exposed to the network.
- **The web console has its own password.** On Windows it's set during install (a strong random default)
  and stored readable only by Administrators and SYSTEM.

**GameStream / Moonlight compatibility is the weak-crypto path — trusted LAN only.** To interoperate
with stock Moonlight clients, punktfunk can speak the legacy GameStream protocol, which pairs over
plain HTTP and uses older encryption. It is **opt-in** (`serve --gamestream`) and appropriate only on a
network you fully trust. The default native `punktfunk/1` protocol is the secure path (modern AEAD
crypto, pinned identities); leave GameStream off unless you specifically need Moonlight.

## Choosing which machine to host on

We've put real work into hardening the host — sealed capture and gamepad channels, no kernel drivers,
loopback-gated admin, pinned trust — and we'll keep at it. But security is also about *blast radius*:
if a host is ever compromised, or you misconfigure trust, what does the attacker get? So pick the
machine with that in mind.

### The Windows host runs with high privileges

To capture the secure desktop (UAC, lock screen) and stream across reboots with nobody logged in, the
Windows host installs a service that runs as **`LocalSystem` (SYSTEM)** — the highest local privilege on
Windows. This is the same design Sunshine and Apollo use, and it's what makes headless, log-in-optional
streaming possible. It also means the host is a high-value component: a compromise of the host, or a
device you paired that you shouldn't have, is a foothold at the most powerful level of that machine.

We mitigate this deliberately:

- **Zero kernel drivers.** The virtual display and all three virtual gamepads are **user-mode (UMDF)**
  drivers, so a driver bug is contained to a restricted service account — never ring-0, never
  full-system. (This is why punktfunk dropped ViGEmBus.)
- **Sealed internal channels.** The desktop-frame ring and the gamepad input/output channels are
  passed between the host and its drivers as duplicated handles to unnamed objects, so another local
  service can't open them by name to read your screen or forge controller input. (Details:
  [`idd-push-security.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/idd-push-security.md)
  and [`gamepad-channel-sealing.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/gamepad-channel-sealing.md).)
- **Secrets are locked down.** The management token, the host identity key, and the console password
  are stored with Administrators/SYSTEM-only permissions.

**The honest floor still applies.** None of this defends against an attacker who is *already* an
administrator or SYSTEM on the box — at that level they own the machine regardless of punktfunk. And a
virtual display is a real monitor: any process already running in your desktop session can capture it
through the ordinary OS screen-capture APIs, exactly as it could capture a physical monitor. That floor
is the same for every virtual-display streaming stack.

**Recommendation:** run the Windows host on a **dedicated or gaming PC**, not on a machine that also
holds your most sensitive material (work laptop, financial records, the box with your password vault).
A gaming rig you stream from is a great fit; your primary secrets machine is not.

### The Linux host runs as your desktop user

The Linux host runs inside your normal desktop session as your **regular user account**, not root — so a
worst-case compromise is scoped to that user rather than the whole system. The same network guidance
applies: keep it on a trusted LAN or a VPN, require pairing, and don't expose it to the internet.

## A short hardening checklist

- **Require pairing** — it's the default; don't run `--open` / `--allow-tofu` except on a network you
  fully trust and control.
- **Use a strong console password** and keep it out of shared documents.
- **Stay on a trusted network** — LAN or VPN. Never port-forward to the internet.
- **Leave GameStream off** unless you specifically need Moonlight compatibility.
- **Review paired devices** in the web console periodically; remove anything you don't recognize.
- **Keep the host updated** — security fixes ship in new builds.
- **On portable hosts**, stop the service when you're on an untrusted network.

## For the technically curious

The deeper security design lives in the repository, and it's candid about residual limits:

- [`design/idd-push-security.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/idd-push-security.md) — the sealed frame channel (why the Windows capture path is isolated), and its honest floor.
- [`design/gamepad-channel-sealing.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/gamepad-channel-sealing.md) — the sealed gamepad channel.
- [`design/security-review-2026-06-28.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/security-review-2026-06-28.md) and [`design/security-review.md`](https://git.unom.io/unom/punktfunk/src/branch/main/design/security-review.md) — the standing security reviews.

Found a security issue? Please report it privately rather than opening a public issue.
