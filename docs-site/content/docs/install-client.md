---
title: Install a Client
description: Install the Punktfunk client for the device you're streaming to — Linux, Steam Deck, Windows, macOS, iOS, or Android.
---

This page is the **install path for each client device**. For what each client *is* and which to
pick, see [Clients](/docs/clients); to install the **host**, see [Install the Host](/docs/install).
Whichever client you install, the first connection needs a one-time [pairing](/docs/pairing). If the
app installs but your host doesn't appear in its list, start at [Troubleshooting → The host isn't
found on the network](/docs/troubleshooting#the-host-isnt-found-on-the-network).

Already installed? Skip to [Keeping a client up to date](#keeping-a-client-up-to-date) or
[Removing a client](#removing-a-client).

> The links below are the **stable** channel (moves on `vX.Y.Z` releases). For the latest `main`
> build, use the **canary** channel — TestFlight / Play Internal, the `…Canary.flatpakref`, or the
> `canary/` download URLs. See [Release Channels](/docs/channels).

## Pick your device

| Device | Install |
|--------|---------|
| **Linux** desktop / laptop | [Flatpak](#linux-desktop-flatpak) (any distro) or native apt/rpm/Arch packages |
| **Steam Deck** | [Decky plugin](/docs/steam-deck) for Gaming Mode, or [Flatpak in Desktop Mode](#steam-deck) |
| **Windows** | [Signed MSIX](#windows) from the package registry |
| **macOS** | [Notarized `.dmg`](#macos) from the releases page |
| **iPhone / iPad / Apple TV** | [TestFlight beta](#ios-ipados-apple-tv) |
| **Android / Android TV** | [Google Play](#android), or sideload the APK |
| **LG webOS TV** | [Community client](#lg-webos-tv-community) (sideloaded `.ipk`) |
| Anything else (browser, old phone, TV) | [Moonlight](/docs/moonlight) |

## Linux desktop (Flatpak)

The **recommended** path on any Flatpak distro — install once, then `flatpak update` tracks new
builds. One command adds the signed `unom` remote, pulls the GNOME runtime from Flathub
automatically, and installs the client:

```sh
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
flatpak run io.unom.Punktfunk
```

Updates, from then on — **without `sudo`** (this is a `--user` install; `sudo flatpak update` only
touches the *system* scope and silently skips it):

```sh
flatpak update                       # or: flatpak update --user io.unom.Punktfunk
```

Prefer your native package manager? The client also ships as real packages (add the repo once —
see the linked guide — then it tracks updates with your normal `apt upgrade` / `dnf upgrade` /
`pacman -Syu`; a *layered* Atomic install needs one extra step, under
[Keeping a client up to date](#keeping-a-client-up-to-date)):

| Distro | Install | Guide |
|--------|---------|-------|
| **Ubuntu 26.04 or newer** | `sudo apt install punktfunk-client` | [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Fedora** | `sudo dnf install punktfunk-client` | [Fedora](/docs/fedora) for the `/etc/yum.repos.d/punktfunk.repo` block (pick the group matching your release), then [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) |
| **Fedora Atomic / Bazzite** | The Flatpak above — no layering. `rpm-ostree install punktfunk-client` only if you already layer packages | [Bazzite](/docs/bazzite) for why layering is a last resort; [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) for the `bazzite` group repo block |
| **Arch** | `sudo pacman -Syu punktfunk-client` (signed binary repo) | [Arch Linux](/docs/arch) |

> **The client `.deb` needs SDL3 and GTK4 ≥ 4.20**, which Ubuntu 24.04 LTS doesn't ship, so
> `apt install punktfunk-client` can't satisfy its dependencies there. On 24.04 (or any older
> distro) use the **Flatpak above** — it carries its own libadwaita and SDL3. This limit is the
> *client's* alone: the host `.deb` is built separately and installs on 24.04 LTS through 26.04.

Then launch it, pick your host from the list, and stream. Every one of these packages — Flatpak
included — also installs the headless **`punktfunk`** command for scripts:

```sh
punktfunk hosts list --probe    # saved hosts, each with a live reachability check
punktfunk launch <host-ref>     # stream to one, waking it first if it's asleep
```

Under the Flatpak, reach it as `flatpak run --command=punktfunk io.unom.Punktfunk hosts list`.
The older `punktfunk-client --connect <host>:9777` still works for existing scripts. Full verb
list: [Clients → the `punktfunk` CLI](/docs/clients#scripting-the-punktfunk-cli).

## Steam Deck

Most Deck users want **Gaming Mode**: install the **[Decky plugin](/docs/steam-deck)** and a
**Punktfunk** panel lands in the Quick Access Menu, so you can find a host, get let in (a PIN, or a
request the host's operator approves), and stream **without dropping to the desktop**. Everything
else — settings, the game library, adding a host by address — is one tap away in the client's own
gamepad UI. Follow the **[Steam Deck (Decky) guide](/docs/steam-deck)** — it walks through Decky
Loader, the plugin, and the one-time client install.

> The plugin doesn't decode video itself — it drives whichever `punktfunk-client` is installed on
> the Deck. The Flatpak below is the tested default; a native package or a sysext works too. If your
> client isn't one the plugin can update for you (a sysext, a nix profile, a source build), the panel
> shows you the update command instead of an **Update** button. The Gaming Mode panel comes from the
> plugin, so a client on its own won't add it. The Decky guide covers installing both, so start there.

For **Desktop Mode** (or to add the client to Game Mode as a non-Steam app yourself), install the
Flatpak exactly as [above](#linux-desktop-flatpak) — it carries its own libadwaita + SDL3 and
survives SteamOS updates:

```sh
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
```

See [packaging/flatpak](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/flatpak/README.md).

## Windows

The Windows client ships as a **signed MSIX** in the package registry. Builds use a self-signed
certificate, so you import that certificate once before Windows will install the package.

1. Download the package and its certificate. Each channel keeps one fixed URL, so these two lines
   always fetch the current build — in PowerShell:

   ```powershell
   curl.exe -LO https://git.unom.io/api/packages/unom/generic/punktfunk-client-windows/latest/punktfunk-client-windows_x64.msix
   curl.exe -LO https://git.unom.io/api/packages/unom/generic/punktfunk-client-windows/latest/punktfunk-client-windows_x64.cer
   ```

   Swap `_x64` for `_arm64` on an Arm device, and `latest` for `canary` to track `main`. The same
   two files are attached to every [release](https://git.unom.io/unom/punktfunk/releases), and every
   build is also kept under its own version on the
   [packages page](https://git.unom.io/unom/-/packages) (generic group, `punktfunk-client-windows`).
2. **Trust the publisher certificate**, then install. The MSIX won't install until the certificate is
   trusted — but it's the **same certificate for every release**, so this is genuinely one-time and
   later updates need nothing. In an **admin** PowerShell:

   ```powershell
   # use the _arm64 files instead on an Arm device
   Import-Certificate -FilePath .\punktfunk-client-windows_x64.cer `
     -CertStoreLocation Cert:\LocalMachine\TrustedPeople
   Add-AppxPackage .\punktfunk-client-windows_x64.msix
   ```

   If Windows reports a missing dependency, install the
   [Windows App Runtime 2.x](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads)
   (the MSIX depends on `Microsoft.WindowsAppRuntime.2`), then re-run `Add-AppxPackage`.

3. Launch **Punktfunk** from the Start menu and pick your host. The package also adds a second
   entry, **Punktfunk Console** — the same client as a controller-driven fullscreen interface for a
   TV or HTPC — and the headless `punktfunk` command on your PATH.

> The Windows client's hardware decode and HDR10 present are validated on glass on NVIDIA and Intel
> (including HDR pass-through on the Intel D3D11VA path). If anything misbehaves,
> **[Moonlight](/docs/moonlight)** is a solid alternative for Windows.

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

**[Join the Punktfunk beta on TestFlight →](https://testflight.apple.com/join/Qr7uSemk)**

Open the app, and your hosts appear automatically under *On this network*.

## Android

The Android client (phone + Android TV — one package, the TV layout is the same app in leanback
mode) is published on **Google Play**. It's a public listing: no invite, no tester list.

**[Get Punktfunk on Google Play →](https://play.google.com/store/apps/details?id=io.unom.punktfunk)**

Install it, open the app, and pick your host.

**Prefer not to go through Play?** The signed APK is published publicly on every build, so you can
sideload it instead — no Play account needed:

```text
https://git.unom.io/api/packages/unom/generic/punktfunk-android/latest/punktfunk-android.apk
```

Swap `latest` for `canary` to track `main`. Release APKs are also attached to each
[release](https://git.unom.io/unom/punktfunk/releases). Android asks you to allow installs from your
browser or file manager the first time.

**Canary on Play** is a separate **Internal testing** track, and that one *is* invite-only — ask on
[Discord](https://discord.gg/kaPNvzMuGU) and we'll add your Google account. The `canary` APK above
needs no invite.

## LG webOS TV (community)

> **Community project.** [`pf-webos`](https://github.com/dyptan-io/pf-webos) is built and maintained
> by [dyptan-io](https://github.com/dyptan-io), not the Punktfunk team — file issues and bugs on
> [its own repo](https://github.com/dyptan-io/pf-webos/issues).

LG's webOS doesn't allow apps outside the LG Content Store without sideloading, so install needs
**Developer Mode** and the **Homebrew Channel** once:

1. Enable Developer Mode on the TV and install the [Homebrew Channel](https://www.webosbrew.org/) —
   follow its [install guide](https://www.webosbrew.org/guide/getting-started.html) if you haven't
   done this before.
2. Grab the latest `.ipk` from the
   [pf-webos releases page](https://github.com/dyptan-io/pf-webos/releases/latest).
3. Install it: either sideload with `ares-install` / the project's `task deploy TV_HOST=root@<tv-ip>`
   (see the repo's README), or side-copy the `.ipk` onto the TV and install it from the Homebrew
   Channel's app.
4. Launch **Punktfunk** from the TV's launcher, discover your host over LAN (or add it by IP), and
   [pair](/docs/pairing) with a PIN.

## Anything else — Moonlight

Any device with a [Moonlight](https://moonlight-stream.org/) client (browser, old phone, smart TV)
connects over GameStream with no punktfunk-specific software. See
[Connect with Moonlight](/docs/moonlight).

## Keeping a client up to date

Every platform is released from one tag. A client and a host don't have to be on the same version,
but keeping them close is the least surprising. (Updating the **host** is its own page:
[Updating](/docs/updating).)

| Client | How it updates |
|---|---|
| **Linux Flatpak** | `flatpak update --user io.unom.Punktfunk` — **without `sudo`** (see the [Flatpak section](#linux-desktop-flatpak)) |
| **Linux apt / dnf / pacman** | your normal `sudo apt upgrade` / `sudo dnf upgrade` / `sudo pacman -Syu`, or the app's own updater below |
| **Fedora Atomic (layered)** | `rpm-ostree upgrade` on its own is not enough — see the note below the table |
| **Windows MSIX** | no self-update — download the newer `.msix` as in [Windows](#windows) and re-run `Add-AppxPackage`. The certificate is the same every release, so you don't import it again |
| **macOS `.dmg`** | download the newer `Punktfunk-<version>.dmg` and drag it over the copy in Applications |
| **iOS / iPadOS / tvOS** | TestFlight updates it |
| **Android** | Google Play updates it; if you sideloaded, download the APK again and install over it |
| **Steam Deck (Decky)** | the panel's **Update** button — see [Steam Deck → Updating](/docs/steam-deck#updating) |
| **LG webOS** | install the newer `.ipk` the same way you installed the first one |

**Fedora Atomic, if you layered the client.** `rpm-ostree upgrade` upgrades the *base image* and
only re-resolves layered packages when that base actually changes — so on a base that sits still it
keeps reporting no updates while a newer `punktfunk-client` waits in the repo. Force a re-resolve of
just that layer, in one transaction, then reboot to activate it:

```sh
sudo rpm-ostree refresh-md --force
sudo rpm-ostree update --uninstall punktfunk-client --install punktfunk-client
systemctl reboot
```

The client's own updater below runs exactly that dance for you, if you'd rather not remember it. (A
layered **host** has the same trap — [Updating](/docs/updating) covers it.)

### The Linux client can update itself

The native Linux client checks its own channel and can apply the update in place, so you don't have
to remember which package manager installed it:

```sh
punktfunk-client --check-update    # prints installed vs available for this box's channel
punktfunk-client --apply-update    # install it
```

`--check-update` is scriptable: it exits **0** when you're up to date, **10** when an update is
available, and **1** when it couldn't tell (offline, or the check is disabled) — "couldn't tell" is
deliberately not "up to date". Add `--json` to either for machine-readable output.

Applying an update needs root, so it's **opt-in**: join the `punktfunk-update` group once, and a
packaged root helper does the install (the same group and the same grant as the host's one-click
updating, described in [Updating](/docs/updating)).

```sh
sudo usermod -aG punktfunk-update $USER
```

Membership is re-read on every check, so there's no need to log out and back in. Without it,
`--check-update` prints the opt-in line and the plain package-manager command instead. On a Flatpak
install `--apply-update` isn't used at all — the client tells you to run `flatpak update`.

## Removing a client

The removal command for every client, and what removal deliberately leaves behind — this device's
identity and its saved hosts, and the pairing on the host — are on
[Uninstalling → Clients](/docs/uninstall#clients). The same page covers forgetting your saved hosts
[without uninstalling anything](/docs/uninstall#removing-the-pairing-not-the-software), and removing
the **host**.
