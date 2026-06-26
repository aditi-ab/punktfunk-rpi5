---
title: Install a Client
description: Install the punktfunk client for the device you're streaming to — Linux, Steam Deck, Windows, macOS, iOS, or Android.
---

This page is the **install path for each client device**. For what each client *is* and which to
pick, see [Clients](/docs/clients); to install the **host**, see [Install the Host](/docs/install).
Whichever client you install, the first connection needs a one-time [pairing](/docs/pairing).

> The links below are the **stable** channel (moves on `vX.Y.Z` releases). For the latest `main`
> build, use the **canary** channel — TestFlight / Play Internal, the `…Canary.flatpakref`, or the
> `canary/` download URLs. See [Release Channels](/docs/channels).

## Pick your device

| Device | Install |
|--------|---------|
| **Linux** desktop / laptop | [Flatpak](#linux-desktop-flatpak) (any distro) or native apt/rpm/Arch packages |
| **Steam Deck** | [Flatpak in Desktop Mode](#steam-deck) (or the Decky plugin) |
| **Windows** | [Signed MSIX](#windows) from the package registry |
| **macOS** | [Notarized `.dmg`](#macos) from the releases page |
| **iPhone / iPad / Apple TV** | [TestFlight beta](#ios-ipados-apple-tv) |
| **Android / Android TV** | [Beta — request access](#android) |
| Anything else (browser, old phone, TV) | [Moonlight](/docs/moonlight) |

## Linux desktop (Flatpak)

The **recommended** path on any Flatpak distro — install once, then `flatpak update` tracks new
builds. One command adds the signed `unom` remote, pulls the GNOME runtime from Flathub
automatically, and installs the client:

```sh
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
flatpak run io.unom.Punktfunk
```

Updates, from then on:

```sh
flatpak update                       # or: flatpak update io.unom.Punktfunk
```

Prefer your native package manager? The client also ships as real packages (add the repo once —
see the linked guide — then it tracks updates with your normal `apt upgrade` / `rpm-ostree upgrade`):

| Distro | Install | Guide |
|--------|---------|-------|
| **Ubuntu / Debian** | `sudo apt install punktfunk-client` | [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Fedora / Bazzite** | `rpm-ostree install punktfunk-client` | [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) |
| **Arch / SteamOS** | `punktfunk-client` from the `PKGBUILD` | [packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md) |

Then launch it, pick your host from the list, and stream. For scripting, skip the picker:

```sh
punktfunk-client --connect <host>:9777
```

## Steam Deck

In **Desktop Mode**, install the Flatpak exactly as [above](#linux-desktop-flatpak) — it carries
its own libadwaita + SDL3 and survives SteamOS updates:

```sh
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
```

Add it to Game Mode as a non-Steam app, or use the **Decky plugin**, which launches this same
Flatpak (`flatpak run io.unom.Punktfunk --connect …`). See
[packaging/flatpak](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/flatpak/README.md).

## Windows

The Windows client ships as a **signed MSIX** in the package registry. Builds use a self-signed
certificate, so you import that certificate once before Windows will install the package.

1. Open the [packages page](https://git.unom.io/unom/-/packages) (generic group), find
   **`punktfunk-client-windows`**, and download the newest **`.msix`** and its matching **`.cer`**.
2. **Trust the publisher certificate**, then install. The MSIX won't install until the certificate is
   trusted — but it's the **same certificate for every release**, so this is genuinely one-time and
   later updates need nothing. In an **admin** PowerShell:

   ```powershell
   Import-Certificate -FilePath .\punktfunk-client-windows.cer `
     -CertStoreLocation Cert:\LocalMachine\TrustedPeople
   Add-AppxPackage .\punktfunk-client-windows.msix
   ```

   If Windows reports a missing dependency, install the
   [Windows App Runtime 2.x](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads)
   (the MSIX depends on `Microsoft.WindowsAppRuntime.2`), then re-run `Add-AppxPackage`.

3. Launch **Punktfunk** from the Start menu and pick your host.

> The Windows client's hardware-decode (D3D11VA) and HDR paths are complete but still pending
> validation on real GPU hardware. If anything misbehaves, **[Moonlight](/docs/moonlight)** is a
> solid alternative for Windows.

## macOS

Download the notarized disk image from the [releases page](https://git.unom.io/unom/punktfunk/releases)
— `Punktfunk-<version>.dmg`. It's Developer-ID signed, notarized, and stapled, so Gatekeeper opens
it without warnings:

1. Open `Punktfunk-<version>.dmg` and drag **Punktfunk** to **Applications**.
2. Launch it, pick your host from *On this network*, and [pair](/docs/pairing).

The Mac app is also in the [TestFlight beta](https://testflight.apple.com/join/Qr7uSemk); the DMG
is the no-account path.

## iOS, iPadOS, Apple TV

The Apple app is in **TestFlight** beta — one universal build covers iPhone, iPad, Apple TV, and the
Mac. Install Apple's [TestFlight](https://apps.apple.com/app/testflight/id899247664) app, then join:

**[Join the punktfunk beta on TestFlight →](https://testflight.apple.com/join/Qr7uSemk)**

Open the app, and your hosts appear automatically under *On this network*.

## Android

The Android client (phone + Android TV) is in **Google Play Internal Testing**. To try it, request a
tester invite on our [**Discord**](https://discord.gg/kaPNvzMuGU) and we'll add your Google account to
the test track:

**[Request access on Discord →](https://discord.gg/kaPNvzMuGU)**

Once you're added, install it from Google Play, then open the app and pick your host:

**[Get punktfunk on Google Play →](https://play.google.com/store/apps/details?id=io.unom.punktfunk)**
_(only resolves once your account is on the tester list)_

## Anything else — Moonlight

Any device with a [Moonlight](https://moonlight-stream.org/) client (browser, old phone, smart TV)
connects over GameStream with no punktfunk-specific software. See
[Connect with Moonlight](/docs/moonlight).
