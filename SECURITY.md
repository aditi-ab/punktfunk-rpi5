# Security Policy

punktfunk is a low-latency desktop/game streaming stack. A host is effectively remote control of a
machine, so we take security reports seriously and appreciate responsible disclosure.

## Reporting a vulnerability

**Please report security issues privately by email to security@punktfunk.com.**

Do **not** open a public issue, pull request, or chat/forum post for a suspected vulnerability — that
exposes other users before a fix exists.

### What to include

The more of this you can give us, the faster we can act:

- The component and version (e.g. `punktfunk-host 0.9.0`, Windows or Linux, which client).
- The impact — what an attacker can do, and from what position (same LAN, a local service account,
  admin, a paired client, …).
- Steps to reproduce, a proof-of-concept, or a crash/log if you have one.
- Any suggested fix or mitigation (optional).

## What to expect

We're a small team, so timelines are best-effort, but we commit to:

- **Acknowledge** your report within **3 business days**.
- Give an **initial assessment** (severity + whether we can reproduce) within about **7 days**.
- Keep you updated, and tell you when a fix ships.
- **Credit** you in the advisory / release notes when the fix is public — unless you'd rather stay
  anonymous.

We practice **coordinated disclosure**: please give us reasonable time to release a fix before
publishing details. We aim to resolve valid issues within **90 days** and will agree a disclosure
date with you.

## Scope

In scope — the code in this repository:

- The host (`punktfunk-host`), its Windows drivers, and the protocol/crypto core (`punktfunk-core`).
- The native clients (Apple, Linux, Windows, Android), the web management console, and the management
  API.

Known limits — documented behavior, not vulnerabilities (see
https://docs.punktfunk.unom.io/docs/security):

- **Admin/SYSTEM already on the host = out of scope.** An attacker who is already administrator or
  SYSTEM on the host owns the machine regardless of punktfunk.
- **The virtual display is a real monitor** — any process already in the interactive desktop session
  can capture it via the normal OS screen-capture APIs, exactly as it could a physical monitor.
- **GameStream/Moonlight compatibility** (`--gamestream`) uses legacy encryption and is documented as
  opt-in, trusted-LAN-only.
- **Public-internet exposure is unsupported** — issues that only arise from exposing the host to the
  WAN are expected; keep the host on a trusted LAN or a VPN.

If you're unsure whether something is in scope, report it anyway — we'd rather hear about it.

## Safe harbor

We consider good-faith security research that follows this policy to be authorized, and we won't
pursue legal action against researchers who:

- make a good-faith effort to avoid privacy violations, data loss, and service disruption,
- only test systems they own or have explicit permission to test,
- give us reasonable time to remediate before public disclosure,
- don't exfiltrate more data than needed to demonstrate the issue.

Thank you for helping keep punktfunk and its users safe.
