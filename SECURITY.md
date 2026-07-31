# Security Policy

Punktfunk is a low-latency desktop/game streaming stack. A host is effectively remote control of a
machine, so we take security reports seriously and appreciate responsible disclosure.

## Supported versions

Punktfunk ships on two tracks — **stable** (a `vX.Y.Z` tag; the current line is **0.22.x**) and
**canary** (built from `main`). Fixes ship as a new release on those tracks; in practice
we don't backport to older minor versions, so the supported versions are the latest stable release
and the current canary build. If you're on an older build, please check that the issue still
reproduces on the latest stable before reporting it. See
[Release Channels](https://docs.punktfunk.unom.io/docs/channels).

## Reporting a vulnerability

**Please report security issues privately by email to security@punktfunk.com.**

Do **not** open a public issue, pull request, or chat/forum post for a suspected vulnerability — that
exposes other users before a fix exists.

### What to include

The more of this you can give us, the faster we can act:

- The component and version (e.g. `punktfunk-host 0.22.3`, Windows or Linux, which client).
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

## Verifying what you downloaded

Every distribution path is authenticated. Nothing below needs an account or a network round trip to
us beyond the download itself.

- **Release-page downloads** (DMG, MSIX, setup.exe, APK, decky zip, .deb/.rpm) each ship a
  `<file>.sha256` next to them. In your download directory:
  `sha256sum -c punktfunk-1.2.3.dmg.sha256` (macOS: `shasum -a 256 -c …`).
- **RPMs** from the dnf repo are OpenPGP-signed with `packages@unom.io` (`AF245C506F4E4763`); the
  repo file in [`packaging/rpm/README.md`](packaging/rpm/README.md) sets `gpgcheck=1`, so dnf
  checks every package for you. `rpmkeys --checksig` on a downloaded RPM verifies it by hand.
- **The Bazzite sysext feed** carries a detached signature over its `SHA256SUMS`, from that same
  key. `punktfunk-sysext` verifies it before installing and refuses a feed it cannot verify — the
  public key is baked into the script rather than fetched from the feed.
- **Windows installers and MSIX packages** are Authenticode-signed; a release build that cannot
  reach its code-signing certificate fails to build rather than falling back to a self-signed one.
  Check with `Get-AuthenticodeSignature punktfunk-host-setup-1.2.3.exe`.
- **The Windows drivers** (virtual display, virtual gamepads) are signed with a stable self-signed
  certificate, `CN=punktfunk-driver`
  (SHA-1 `4B8493E7CD565758D335F8F4F05C5A7261A13E02`), also published in
  [`packaging/windows/README.md`](packaging/windows/README.md). The installer has to add it to the
  machine's trusted roots for a self-signed driver to install at all, so — unlike the cases above —
  this signature does **not** authenticate the download: it gives the drivers a stable publisher
  identity you can compare against the published fingerprint, and it is removed again on uninstall.
  Verify with `Get-AuthenticodeSignature` on the installed `pf_vdisplay.dll`, or list what is
  trusted with `Get-ChildItem Cert:\LocalMachine\Root | ? Subject -like '*punktfunk*'`.

A checksum on its own only tells you the download wasn't corrupted in transit — it says nothing
about who produced the file, since anyone able to replace an artifact can replace its checksum.
Where that distinction matters (the update feeds, the package repos), the checksums are covered by
a signature. If a signature check fails, please don't work around it; report it.

## Safe harbor

We consider good-faith security research that follows this policy to be authorized, and we won't
pursue legal action against researchers who:

- make a good-faith effort to avoid privacy violations, data loss, and service disruption,
- only test systems they own or have explicit permission to test,
- give us reasonable time to remediate before public disclosure,
- don't exfiltrate more data than needed to demonstrate the issue.

Thank you for helping keep Punktfunk and its users safe.
