---
title: Install the Host
description: Install the punktfunk host — on Linux from its package registry, or on Windows from a signed installer.
---

On Linux, the package registries are the real distribution channel. Pick your distro, add the repo, and
install with your native package manager. Each row links to the full per-distro guide (add the repo,
first-run steps, the web console) — those are the source of truth, so this page doesn't duplicate them.
On **Windows**, the host ships as a signed installer instead — see [Windows](#windows).

> **First, read [Security & Safe Use](/docs/security).** A streaming host is remote control of the
> machine. It's built for trusted local networks — don't expose it to the internet, and be thoughtful
> about which machine you host on (especially on Windows).

## Pick your distro

| Distro | Package manager | One-command happy path | Guide |
|--------|-----------------|------------------------|-------|
| **Ubuntu / Debian** | apt | `sudo apt install punktfunk-host` | [Ubuntu — GNOME](/docs/ubuntu-gnome) · [Ubuntu — KDE](/docs/ubuntu-kde) · [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Bazzite / Fedora Atomic** | systemd-sysext | `sudo bash punktfunk-sysext.sh install` (no layering, no reboot) | [Bazzite](/docs/bazzite) · [packaging/bazzite](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/bazzite/README.md) |
| **Fedora (dnf)** | dnf / rpm-ostree | `dnf install punktfunk punktfunk-web` | [Fedora — KDE](/docs/fedora-kde) · [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) |
| **Arch** | pacman | `pacman -Sy punktfunk-host` (binary repo) | [Arch Linux](/docs/arch) · [packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md) |
| **SteamOS (host)** | on-device script | `bash scripts/steamdeck/install.sh` | [SteamOS (Host)](/docs/steamos-host) |

Each registry is public — no auth, you just trust the repo's signing key. Adding the repo is a
one-time step covered in the linked guide; after that, normal `apt upgrade` / `dnf upgrade` /
`pacman -Syu` (or `sudo punktfunk-sysext update` on Bazzite) tracks new builds.

> **Stable vs canary.** The repos in the per-distro guides are the **stable** channel — it only
> moves when a `vX.Y.Z` release is cut. For the latest `main` build (fast, possibly broken), point
> at the **canary** channel instead (`canary` apt distribution / `*-canary` rpm group). See
> [Release Channels](/docs/channels).

## Windows

punktfunk also runs as a native host on **Windows 11 22H2+ (x64)**, shipped as a signed
installer — see [Windows Host](/docs/windows-host) for what it includes and its limitations.

1. From the [packages page](https://git.unom.io/unom/-/packages) (generic group), download the newest
   **`punktfunk-host-setup-<ver>.exe`** and its matching **`.cer`**.
2. **Trust the publisher certificate once.** The installer is signed with a self-signed certificate
   whose public `.cer` is published next to it — the **same certificate for every release**, so this is
   genuinely one-time and later updates need nothing. In an **admin** PowerShell:

   ```powershell
   Import-Certificate -FilePath .\punktfunk-host-setup.cer `
     -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
   ```

3. Run `punktfunk-host-setup-<ver>.exe` (elevated). It installs to `C:\Program Files\punktfunk`,
   installs the bundled **pf-vdisplay** virtual-display driver, and registers + starts the
   `LocalSystem` service (`/VERYSILENT` for an unattended install). Upgrades and uninstall go through
   Add/Remove Programs.

For hardware encode you need a GPU — NVIDIA (NVENC), AMD (AMF), or Intel (QSV); there's a software
fallback without one. More detail — including the CLI `punktfunk-host service install` path — is in
[Running as a Service → Windows](/docs/running-as-a-service#windows).

## What the packages are

- **`punktfunk-host`** — the streaming host. Install this on your Linux gaming machine.
- **`punktfunk-web`** — the browser management console (pairing + status). Recommended alongside the
  host; on RPM list it explicitly (`dnf install punktfunk punktfunk-web`) — the Bazzite sysext
  image already includes it.
- **`punktfunk-client`** — the GTK4 desktop client, for streaming *to* a Linux box (also shipped via
  apt / RPM / Arch / Flatpak). On a Steam Deck, this is the package you want.

## After installing

1. Add yourself to the `input` group (virtual gamepads need `/dev/uinput`), then re-login. The exact
   command differs per distro — see your guide (`usermod -aG input "$USER"`, or `ujust
   add-user-to-input-group` on Bazzite).
2. Start the host inside your desktop session:

   ```sh
   punktfunk-host serve
   ```

   Bare `serve` is the secure native-only default (native `punktfunk/1` + the web console). On a
   trusted LAN, add `--gamestream` to also serve stock [Moonlight](/docs/moonlight) clients.

3. Enable the web console and read its login password, then open `http://<host-ip>:47992`:

   ```sh
   systemctl --user enable --now punktfunk-web
   journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
   ```

From there, follow the [Quick Start](/docs/quickstart) to pair your first client. To run the host
automatically at boot, see [Running as a Service](/docs/running-as-a-service).

## Building from source

If no package exists for your platform, you can build from source — see the repository README. Source
builds are a fallback; the registries are the supported path.
