---
title: Install the Host
description: Install the punktfunk host — on Linux from its package registry, or on Windows from a signed installer.
---

On Linux, the package registries are the real distribution channel. Pick your distro, add the repo, and
install with your native package manager. Each row links to the full per-distro guide (add the repo,
first-run steps, the web console) — those are the source of truth, so this page doesn't duplicate them.
On **Windows** (NVIDIA), the host ships as a signed installer instead — see [Windows](#windows-nvidia).

## Pick your distro

| Distro | Package manager | One-command happy path | Guide |
|--------|-----------------|------------------------|-------|
| **Ubuntu / Debian** | apt | `sudo apt install punktfunk-host` | [Ubuntu — GNOME](/docs/ubuntu-gnome) · [Ubuntu — KDE](/docs/ubuntu-kde) · [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Fedora / Bazzite** | rpm-ostree | `rpm-ostree install punktfunk punktfunk-web` | [Fedora — KDE](/docs/fedora-kde) · [Bazzite](/docs/bazzite) · [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) |
| **Arch / Steam Deck** | PKGBUILD / sysext | `makepkg -si` (Arch) · sysext `.raw` (SteamOS/Deck) | [packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md) |

Each registry is public — no auth, you just trust the repo's signing key. Adding the repo is a
one-time step covered in the linked guide; after that, normal `apt upgrade` / `rpm-ostree upgrade`
tracks new builds automatically.

## Windows (NVIDIA)

punktfunk also runs as a native host on **Windows 10/11 (x64) with an NVIDIA GPU**, shipped as a
signed installer — see [Windows Host](/docs/windows-host) for what it includes and its limitations.

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
   optionally installs the bundled **SudoVDA** virtual-display driver, and registers + starts the
   `LocalSystem` service (`/VERYSILENT` for an unattended install). Upgrades and uninstall go through
   Add/Remove Programs.

You need an NVIDIA GPU + driver (the host is NVENC-only on Windows). More detail — including the CLI
`punktfunk-host service install` path — is in
[Running as a Service → Windows](/docs/running-as-a-service#windows).

## What the packages are

- **`punktfunk-host`** — the streaming host. Install this on your Linux + NVIDIA gaming machine.
- **`punktfunk-web`** — the browser management console (pairing + status). Recommended alongside the
  host; on RPM list it explicitly (`rpm-ostree install punktfunk punktfunk-web`).
- **`punktfunk-client`** — the GTK4 desktop client, for streaming *to* a Linux box (also shipped via
  apt / RPM / Arch / Flatpak). On a Steam Deck, this is the package you want.

## After installing

1. Add yourself to the `input` group (virtual gamepads need `/dev/uinput`), then re-login. The exact
   command differs per distro — see your guide (`usermod -aG input "$USER"`, or `ujust
   add-user-to-input-group` on Bazzite).
2. Start the host inside your desktop session:

   ```sh
   punktfunk-host serve --native
   ```

3. Enable the web console and read its login password, then open `http://<host-ip>:3000`:

   ```sh
   systemctl --user enable --now punktfunk-web
   journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
   ```

From there, follow the [Quick Start](/docs/quickstart) to pair your first client. To run the host
automatically at boot, see [Running as a Service](/docs/running-as-a-service).

## Building from source

If no package exists for your platform, you can build from source — see the repository README. Source
builds are a fallback; the registries are the supported path.
