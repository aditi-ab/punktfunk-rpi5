---
title: Install the Host
description: Install the Punktfunk host — on Linux from its package registry, or on Windows from a signed installer.
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
| **Ubuntu 26.04+** ¹ | apt | `sudo apt install punktfunk-host` | [Ubuntu](/docs/ubuntu) · [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Debian 13+** (incl. LMDE) | apt | `sudo apt install punktfunk-host` | [Debian](/docs/debian) · [packaging/debian](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/debian/README.md) |
| **Bazzite / Fedora Atomic** | systemd-sysext | `curl -fsSLO https://git.unom.io/unom/punktfunk/raw/branch/main/packaging/bazzite/punktfunk-sysext.sh && sudo bash punktfunk-sysext.sh install` (no layering, no reboot) | [Bazzite](/docs/bazzite) · [packaging/bazzite](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/bazzite/README.md) |
| **Fedora (dnf)** | dnf / rpm-ostree | `sudo dnf install punktfunk` | [Fedora](/docs/fedora) · [packaging/rpm](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/rpm/README.md) |
| **Arch** | pacman | `sudo pacman -Syu punktfunk-host` (binary repo — always a full `-Syu`, never `-Sy`) | [Arch Linux](/docs/arch) · [packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md) |
| **SteamOS (host)** | on-device script | clone the repo, then `bash ~/punktfunk/scripts/steamdeck/install.sh` (builds on-device) | [SteamOS (Host)](/docs/steamos-host) |
| **NixOS / Nix** | nix flake | `nix run git+https://git.unom.io/unom/punktfunk#punktfunk-host -- serve --gamestream` | [NixOS](#nixos) · [packaging/nix](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/nix/README.md) |

> ¹ **Ubuntu 24.04 LTS installs the package but cannot host.** It ships no compositor that meets
> the [version floors](/docs/requirements) — KWin 5.27 against 6.5.6, GNOME Shell 46 against 48 —
> and no `gamescope`. Use 26.04 or newer. This is also why
> [Linux Mint 22.x cannot host](/docs/debian#linux-mint-22x-cannot-host-yet); LMDE 7 (Debian 13)
> can.

Each registry is public — no auth, you just trust the repo's signing key. Adding the repo is a
one-time step covered in the linked guide; after that, normal `apt upgrade` / `dnf upgrade` /
`pacman -Syu` (or `sudo punktfunk-sysext update` on Bazzite) tracks new builds. On **NixOS** you add
the flake as an input and enable its module rather than adding a package repo — but do add the
[binary cache](#nixos), or every build compiles from source.

> **Stable vs canary.** The repos in the per-distro guides are the **stable** channel — it only
> moves when a `vX.Y.Z` release is cut. For the latest `main` build (fast, possibly broken), point
> at the **canary** channel instead (`canary` apt distribution / `*-canary` rpm group). See
> [Release Channels](/docs/channels).

## Windows

Punktfunk also runs as a native host on **Windows 11 22H2+ (x64)**, shipped as a signed
installer — see [Windows Host](/docs/windows-host) for what it includes and its limitations.

For hardware encode you need a GPU — NVIDIA (NVENC), AMD (AMF), or Intel (QSV); there's a software
fallback without one. More detail — including the CLI `punktfunk-host service install` path — is in
[Running as a Service → Windows](/docs/running-as-a-service#windows).

### winget (recommended)

In an **admin** PowerShell, register the Punktfunk source once, then install:

```powershell
winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest
winget install unom.PunktfunkHost
```

Later, `winget upgrade unom.PunktfunkHost` updates it in place. Add `--interactive` to get the full
wizard instead (the optional task checkboxes, the web-console password page). winget carries
**stable** releases only — canary builds are not published there.

### Manual download

Download `punktfunk-host-setup-<ver>.exe` and run it elevated. The full procedure — where to get it,
everything the installer puts on the machine, its optional tasks, the console password, and the
`/VERYSILENT` unattended switch — lives on one page: [Windows Host → Install](/docs/windows-host#install).
This is also the path for **canary** builds, which winget doesn't carry — see
[Release Channels](/docs/channels) for that download.

> **About signing.** The installer is signed with a publicly trusted certificate, so Windows shows
> the publisher by name at the UAC prompt — there is no Unknown Publisher warning and nothing to
> import. The winget route is no different: it downloads and runs that same installer.
>
> SmartScreen is a separate mechanism that builds reputation per publisher, so shortly after a new
> signing certificate starts being used it can still show *"Windows protected your PC"* on the first
> downloads — **More info → Run anyway**. It settles as installs accumulate.
>
> The bundled **drivers** are a separate matter — they carry their own certificate, and the installer
> imports that one for you. [Windows Host](/docs/windows-host#about-the-signatures) has the detail.
>
> Releases **0.28.1 and earlier** were signed with our own self-signed certificate, and older docs
> told you to import it. Nothing needs it any more: if you imported
> `punktfunk-host-windows_<ver>.cer` back then, you can remove it from `Cert:\LocalMachine\Root` and
> `Cert:\LocalMachine\TrustedPublisher` (look for the certificate issued to **unom**, thumbprint
> `CD1EFDEEEC9743AFC38F56C5AF30C5A3009BE941`).

## NixOS

The repo's `flake.nix` is a supported install path: it builds `punktfunk-host`, `punktfunk-client`,
`punktfunk-web` and `punktfunk-scripting`, and ships a NixOS module. **`x86_64-linux` only**, and
NixOS **24.11 or newer**.

**Add the binary cache first.** Without it, a build compiles the whole Rust workspace *and*
gamescope from source — about an hour. With it you get prebuilt binaries:

```nix
nix.settings = {
  substituters = [ "https://nix.unom.io" ];
  trusted-public-keys = [ "punktfunk-cache-1:<key>" ];   # curl https://nix.unom.io/punktfunk-cache.pub
};
```

Off NixOS, put the same two values in `/etc/nix/nix.conf` as `extra-substituters` /
`extra-trusted-public-keys`. One caveat worth knowing before you copy a flake snippet from
elsewhere: setting `inputs.punktfunk.inputs.nixpkgs.follows = "nixpkgs"` changes every store path
and so misses the cache entirely — details in
[packaging/nix](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/nix/README.md#binary-cache-do-this-before-your-first-build).

You can run it straight from the flake without NixOS (on other distros, wrap it in
[nixGL](https://github.com/nix-community/nixGL) so the GPU drivers resolve):

```sh
nix run git+https://git.unom.io/unom/punktfunk#punktfunk-host -- serve --gamestream
```

On NixOS, add the flake as an input, add `punktfunk.nixosModules.default` to your system's modules,
and enable the host:

```nix
services.punktfunk.host = {
  enable = true;
  users = [ "alice" ];     # added to the `input` group, for virtual gamepads
  openFirewall = true;
  desktopSession = true;   # on a machine you log into — see below
  settings = { RUST_LOG = "info"; };   # these become host.env
};
```

The module does declaratively what the deb/RPM scriptlets do — the systemd user service, udev rules,
kernel modules, sysctl tuning, the firewall ports and `input` group membership — and brings in the
web console alongside the host. Because `settings` writes the environment file for you, skip the
`host.env` step in [After installing](#after-installing).

**Set `desktopSession = true` on any machine somebody logs into.** It ties the host to
`graphical-session.target`, so restarting Plasma or GNOME restarts the host with it. Without it the
host keeps running against a compositor that no longer exists — still listening, still answering,
and failing at capture on every session after that. Leave it off for the headless appliance route
(a pinned compositor or a gamescope box), which may never reach that target. Same reasoning, and
the same caveats for Sway and Hyprland, as [Restart the host with your
desktop](/docs/running-as-a-service#restart-the-host-with-your-desktop).

The host and console user services are defined but not started (set `autoStart = true` for an
appliance), so from your graphical session enable them:

```sh
systemctl --user enable --now punktfunk-host punktfunk-web
```

The plugin runner needs no such step — like the deb and RPM, the module starts it for you, because
the game-library scanners ship as plugins. Opt out with
`services.punktfunk.scripting.autoStart = false;`.

The full option reference (client, console and scripting options, GPU driver notes, headless
appliance setup) is in
[packaging/nix](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/nix/README.md). To
update, run `nix flake update punktfunk` in your flake directory, then `sudo nixos-rebuild switch`.

## What the packages are

- **`punktfunk-host`** — the streaming host. Install this on your Linux gaming machine.
- **`punktfunk-web`** — the browser management console (pairing + status). Recommended alongside the
  host. On apt and RPM the host package *recommends* it, so your package manager pulls it in by
  default, and the Bazzite sysext image already contains it. On **Arch** it's an optional
  dependency, so name it yourself: `sudo pacman -Syu punktfunk-web`.
- **`punktfunk-client`** — the GTK4 desktop client, for streaming *to* a Linux box (shipped via
  apt / RPM / Arch, and as a Flatpak). On a **Steam Deck** take the Flatpak instead — SteamOS's
  `/usr` is read-only, so the native package isn't the path there:

  ```sh
  flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.flatpakref
  ```

  For Gaming Mode, add the [Decky plugin](/docs/steam-deck) on top of it. Full client instructions
  for every device: [Install a Client](/docs/install-client).

- **`punktfunk-scripting`** — the plugin/script runner, behind [plugins](/docs/plugins) and
  [automation](/docs/automation). The game-library scanners ship as plugins, so a host without the
  runner can come up with an empty library — which is why **apt, dnf, the Bazzite sysext and the
  NixOS module all start it for you**. On **Arch** and source installs it is not started, so enable
  it yourself:

  ```sh
  systemctl --user enable --now punktfunk-scripting
  ```

  To opt out where it *is* on: `systemctl --user mask punktfunk-scripting` (`mask`, not `disable` —
  a plain disable cannot remove a symlink that lives in `/etc` or `/usr`), or on NixOS
  `services.punktfunk.scripting.autoStart = false;`.

## After installing

These three steps are for the **Linux packages**. On Windows the installer does the equivalent for
you; on NixOS the module does steps 1 and 2, and [NixOS](#nixos) above has the units to enable.

1. Add yourself to the `input` group — virtual gamepads and [pen
   input](/docs/input#pen-and-stylus) both need `/dev/uinput` — then re-login. The exact
   command differs per distro — see your guide (`usermod -aG input "$USER"`, or `ujust
   add-user-to-input-group` on Bazzite).

   Also join `punktfunk` — `sudo usermod -aG punktfunk "$USER"`, then log out and back in — if
   **either** of these is true: you want the **virtual Steam Deck controller** (paddles,
   trackpads, gyro), or this box autologins into Steam **Gaming Mode** and you want the host to
   take that session over at your client's resolution
   ([gamescope](/docs/gamescope#nobara-and-other-autologin-display-managers)). Your package created
   that group at install time and left it empty. It gates the usbip nodes the pad attaches through
   *and* the helper that stops the display manager for a takeover, and it is separate from `input`
   on purpose, because writing those nodes can present arbitrary emulated USB hardware — so join it
   only on a machine you trust. On a plain desktop host that streams no Gaming Mode, skipping it
   costs you nothing but that one pad type.
2. Put your `host.env` in place, then start the host. Every Linux package ships a systemd **user**
   unit, so you don't run the host by hand — but that unit reads `~/.config/punktfunk/host.env` and
   won't start until the file exists. Each package ships a template to copy; your distro and desktop
   guides say which one to pick (on Bazzite it's `host.env.bazzite`):

   ```sh
   mkdir -p ~/.config/punktfunk
   # /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu
   cp /usr/share/punktfunk/host.env.example ~/.config/punktfunk/host.env
   systemctl --user enable --now punktfunk-host
   ```

   The shipped unit runs `serve --gamestream` — the native `punktfunk/1` plane **plus** the
   GameStream/Moonlight-compatible planes, so stock [Moonlight](/docs/moonlight) clients work out of
   the box. Those extra planes are only appropriate on a trusted LAN. To run native-only, drop the
   flag with a drop-in (`systemctl --user edit punktfunk-host`):

   ```ini
   [Service]
   ExecStart=
   ExecStart=/usr/bin/punktfunk-host serve
   ```

   The empty `ExecStart=` is required — without it systemd adds a second command instead of
   replacing the first — and the binary path has to match your install (`systemctl --user cat
   punktfunk-host` shows it; the distro packages use `/usr/bin`). Save the drop-in, then
   `systemctl --user restart punktfunk-host`. For what each mode starts, see
   [Host CLI → `serve`](/docs/host-cli#serve).

3. Enable the web console:

   ```sh
   systemctl --user enable --now punktfunk-web
   ```

   Then open `https://<host-ip>:47992`. Reading its [login password](/docs/web-console#login-password)
   and [arming PIN pairing](/docs/web-console#arm-pairing) are covered in
   [The Web Console](/docs/web-console).

### Configure your desktop

How the virtual display and input work depends on your desktop — see [KDE](/docs/kde),
[GNOME](/docs/gnome), [Steam / gamescope](/docs/gamescope), [Hyprland](/docs/hyprland), or
[Sway](/docs/sway) for the compositor-specific setup.

From there, follow the [Quick Start](/docs/quickstart) to pair your first client. To run the host
automatically at boot, see [Running as a Service](/docs/running-as-a-service). If something doesn't
come up, [Troubleshooting](/docs/troubleshooting) starts from the symptom.

## Updating and removing

The web console's **Host → Updates** card tells you when a newer host is out and shows the exact
command for the way you installed — the full list, plus one-click updating and how to turn the check
off, is on [Updating the Host](/docs/updating).

To take it back off, see [Uninstall](/docs/uninstall) — it covers every install method and what is
deliberately left behind (your `~/.config/punktfunk` — identity certificate, paired devices, console
password — survives package removal).

## Building from source

If no package exists for your platform, you can build from source — see the repository README. Source
builds are a fallback; the registries are the supported path.
