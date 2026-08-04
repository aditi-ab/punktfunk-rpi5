---
title: Uninstalling
description: Remove the Punktfunk host or client for every install method — and what each one deliberately leaves behind.
---

Every install method has a clean removal path. This page walks through each one, and — just as
important — says what stays on the machine afterwards: the Linux packages run no removal scripts of
their own, and the Windows uninstaller leaves a few things in place on purpose.

> **Your configuration always survives.** Removing Punktfunk never deletes its config directory —
> `~/.config/punktfunk` on Linux, `%ProgramData%\punktfunk` for the Windows host. It holds the
> host's identity certificate and key, its management token, your paired devices, the web-console
> login password, `host.env`, the game library, the logs, and any installed
> [plugins](/docs/plugins) and their state. Keeping it is what lets a reinstall pick up where you
> left off — each section below gives the one command that clears it, for when you want a clean
> slate instead.

Jump to what you installed:

- Linux host — [apt](#ubuntu-apt) · [dnf](#fedora-dnf) · [rpm-ostree layer](#fedora-atomic--bazzite-rpm-ostree-layer) · [Bazzite sysext](#bazzite--fedora-atomic-systemd-sysext) · [pacman](#arch--cachyos-pacman) · [SteamOS on-device build](#steamos--steam-deck-host-on-device-build) · [NixOS](#nixos)
- [Windows host](#windows-host) (installer or winget)
- [Clients](#clients) — Flatpak, Linux packages, Windows, macOS, iOS/tvOS, Android, Steam Deck, LG webOS
- [Plugins and the script runner](#plugins-and-the-script-runner)

## Linux hosts

### Stop the services first

The Linux packages ship systemd **user** units, and `systemctl --user enable` writes symlinks into
your home directory that package removal cannot see. Disable them before you remove anything, or
you'll be left with dangling links and a unit that fails at every login:

```sh
systemctl --user disable --now punktfunk-host punktfunk-web
```

Add `punktfunk-scripting` to that line if you enabled the [plugin runner](/docs/plugins), and
`punktfunk-kde-session` if you set up the [headless KDE session](/docs/kde#headless-session).

If you turned on linger so the host ran without a login, and nothing else on the box needs it:

```sh
sudo loginctl disable-linger "$USER"
```

### Ubuntu (apt)

```sh
sudo apt purge punktfunk-host punktfunk-web punktfunk-client punktfunk-scripting
sudo apt autoremove
```

Name only the packages you actually installed — the others are simply reported as not installed.
Then drop the repository and its key, so `apt update` stops contacting it:

```sh
sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc
sudo apt update
```

**Left behind:** `~/.config/punktfunk`, and the empty `punktfunk-update` system group the package
created for [one-click updates](/docs/updating). Clear them with:

```sh
rm -rf ~/.config/punktfunk
sudo groupdel punktfunk-update
```

Your `input` group membership is harmless to keep (it is a stock Ubuntu group). Drop it with
`sudo gpasswd -d "$USER" input` if you'd rather not have it. If you opened the firewall, close it
again: `sudo ufw delete allow punktfunk-native` (and `punktfunk-gamestream` / `punktfunk-web` if you
allowed those too).

### Fedora (dnf)

The host package is called **`punktfunk`** on RPM, not `punktfunk-host`:

```sh
sudo dnf remove punktfunk punktfunk-web punktfunk-client punktfunk-scripting
sudo rm -f /etc/yum.repos.d/punktfunk.repo
```

**Left behind:** `~/.config/punktfunk`, the `punktfunk-update` group, and the signing key dnf
imported into the rpm keyring when it first installed a Punktfunk package. Clear the first two with
`rm -rf ~/.config/punktfunk` and `sudo groupdel punktfunk-update`. The key is harmless to leave — on
its own it only marks packages from our registry as trusted, and nothing fetches them once the repo
file is gone.

On firewalld, close the ports you opened:

```sh
sudo firewall-cmd --permanent --remove-service=punktfunk-native
sudo firewall-cmd --permanent --remove-service=punktfunk-gamestream   # if you opened it
sudo firewall-cmd --permanent --remove-service=punktfunk-web          # if you opened it
sudo firewall-cmd --reload
```

### Fedora Atomic / Bazzite (rpm-ostree layer)

If you layered the RPMs rather than using the sysext:

```sh
sudo rpm-ostree uninstall punktfunk punktfunk-web
systemctl reboot
```

The change only takes effect in the new deployment, so the reboot is part of the removal. Remove
`/etc/yum.repos.d/punktfunk.repo` as well if you added it. `~/.config/punktfunk` is untouched.

### Bazzite / Fedora Atomic (systemd-sysext)

This is the supported Bazzite path and the tidiest one — the whole install is a single image under
`/var/lib/extensions/`. Stop the services **before** you unmerge, because once the image is gone
their binaries are gone and the units just keep failing:

```sh
systemctl --user disable --now punktfunk-host punktfunk-web
sudo punktfunk-sysext remove
```

`remove` deletes the image, its version sidecar, `/etc/punktfunk-sysext.conf`, the tray autostart
entry, and the gamescope session drop-in unless you edited it — then prints
`punktfunk sysext removed (user config in ~/.config/punktfunk is untouched)`.

Three things it created outside `/usr` stay behind:

```sh
sudo rm -f /etc/modules-load.d/punktfunk.conf /etc/udev/rules.d/60-punktfunk.rules
sudo groupdel punktfunk-update
```

And your config, if you want it gone: `rm -rf ~/.config/punktfunk`. See
[Bazzite](/docs/bazzite#install) for the same sequence in context.

### Arch / CachyOS (pacman)

```sh
sudo pacman -Rns punktfunk-host punktfunk-web punktfunk-gamescope \
  punktfunk-client punktfunk-scripting
```

Name only what you installed. `-Rns` also takes the dependencies nothing else needs and removes the
packages' own configuration files.

Then delete the `[punktfunk]` section (or `[punktfunk-canary]`) from `/etc/pacman.conf` — the two
lines you appended when you [added the repo](/docs/arch#2-add-the-signed-repo). Optionally drop the
repo's signing key from pacman's keyring:

```sh
sudo pacman-key --delete E0CA04465C99C936E0B0C6510A317015A34DDD69
```

**Left behind:** `~/.config/punktfunk` and the `punktfunk-update` group —
`rm -rf ~/.config/punktfunk` and `sudo groupdel punktfunk-update` clear them. On CachyOS, close the
ufw rules you opened: `sudo ufw delete allow punktfunk-native`.

### SteamOS / Steam Deck host (on-device build)

**There is no uninstall script for this install method.** The on-device build is spread across your
user session and a handful of root-owned files, so stop the user services first:

```sh
systemctl --user disable --now punktfunk-host punktfunk-web \
  punktfunk-scripting punktfunk-rebuild-check
rm -f ~/.config/systemd/user/punktfunk-*.service
systemctl --user daemon-reload
```

Then follow [SteamOS (Host) → Uninstalling](/docs/steamos-host#uninstalling) for the build
container, the files under your home, and the root-owned tuning. Don't skip the last of those: the
atomic-update keep list is what carries those files through every SteamOS update, so left alone they
stay on the device indefinitely.

**Left behind:** `~/.config/punktfunk` (`rm -rf ~/.config/punktfunk` for a clean slate), your
`input` group membership, and — if the installer seeded it because you had none — the KDE
RemoteDesktop portal grant at `~/.local/share/flatpak/db/kde-authorized`.

### NixOS

There is nothing to uninstall imperatively — remove what you declared:

1. Delete the `services.punktfunk.*` options from your configuration.
2. Remove `punktfunk.nixosModules.default` from the system's module list and the `punktfunk` flake
   input.
3. Rebuild: `sudo nixos-rebuild switch`.

The unit, udev rules, sysctl tuning, firewall ports and `input` group membership all disappear with
the generation. The store paths stay until you garbage-collect, and `~/.config/punktfunk` — which
the module never managed — stays regardless.

## Windows host

Uninstall from Add/Remove Programs (**Settings → Apps → Installed apps**) → **Punktfunk Host**, or,
if you installed with winget:

```powershell
winget uninstall unom.PunktfunkHost
```

Both run the same uninstaller, which takes the `PunktfunkHost` service, the scheduled tasks, the
virtual-display and gamepad drivers and every firewall rule it added back off the machine — the
full inventory is on [Windows Host → Uninstalling](/docs/windows-host#uninstalling).

Three things are left on purpose:

- **`%ProgramData%\punktfunk`** — `host.env`, the host certificate and key, the management token,
  the console password, your paired devices and the logs. For a clean slate:

  ```powershell
  Remove-Item -Recurse -Force "$env:ProgramData\punktfunk"
  ```

- **VB-CABLE**, unless you cleared its checkbox during setup — it is ticked by default. It is a
  third-party VB-Audio component other apps may be using, so the Punktfunk uninstaller never touches
  it. Remove it with its own uninstaller —
  `VBCABLE_Setup_x64.exe -u -h` — or the **VB-Audio Virtual Cable** entry in Installed apps.
- **The publisher certificate**, if you imported it by hand to silence the Unknown Publisher prompt.
  Remove it in `certlm.msc` under **Trusted Publishers** and **Trusted Root Certification
  Authorities**. (This is *not* the driver certificate above, which the uninstaller does remove.)

If you registered the winget source, drop it too — in an **admin** PowerShell, the same as
registering it:

```powershell
winget source remove -n punktfunk
```

**If a Punktfunk display or gamepad survives in Device Manager** — an older build could leave one
behind — run the host's own cleanup from an elevated prompt, which is exactly what the uninstaller
calls. Do this **while the host is still installed**:

```powershell
punktfunk-host driver uninstall
punktfunk-host driver uninstall --gamepad
```

If you have already uninstalled, `punktfunk-host.exe` went with it. Install the current version
again and uninstall it — its uninstaller runs both commands for you.

See [Windows Host → Install](/docs/windows-host#install) for the installer's side of the same story.

## Clients

Removing a client does **not** tell the host to forget it. Unpair the device from the host's
[web console](/docs/web-console) (Pairing → unpair) if you want its pairing gone as well.

### Linux — Flatpak

```sh
flatpak uninstall --user --delete-data io.unom.Punktfunk
```

`--delete-data` clears the Flatpak's own per-app directory. It does **not** clear
`~/.config/punktfunk` — the client keeps its identity, known hosts and settings in your real config
directory (that is what lets the Flatpak, a native package and the Decky plugin share one paired
identity). Remove it with `rm -rf ~/.config/punktfunk`.

The remote it was installed from also stays. Its name depends on how you installed: if you added the
repo by hand it is **`unom`**, and if you installed straight from the `.flatpakref` — the route
[Install a Client](/docs/install-client#linux-desktop-flatpak) gives — Flatpak named its own origin
remote for it. List them and delete the one that served Punktfunk, if no other app uses it:

```sh
flatpak remotes --user
flatpak remote-delete --user <name>
```

### Linux — apt / dnf / pacman packages

```sh
sudo apt purge punktfunk-client       # Ubuntu
sudo dnf remove punktfunk-client      # Fedora
sudo pacman -Rns punktfunk-client     # Arch / CachyOS
```

Then remove the repository as described under the host sections above, if this box had no host on
it. To clear the client's own state without uninstalling — saved hosts and stream settings, keeping
the paired identity — run `punktfunk-client --reset` instead.

### Windows client (MSIX)

```powershell
Get-AppxPackage unom.Punktfunk | Remove-AppxPackage
```

The client's saved hosts, settings and pairing identity live under `%APPDATA%\punktfunk` and are not
removed with the package — delete that folder for a clean slate. The publisher certificate you
imported to install the MSIX stays in **Trusted People**; remove it in `certlm.msc` if you're done
with Punktfunk on that machine.

### macOS

Quit Punktfunk and drag it from **Applications** to the Trash. If you installed it through
TestFlight instead, remove it from TestFlight.

### iPhone, iPad, Apple TV

Delete the app the usual way. To leave the beta entirely, open **TestFlight**, select Punktfunk, and
stop testing — that removes the app and its data with it.

### Android / Android TV

Uninstall the app from Google Play or from Settings → Apps. That's the whole job — it's a public
Play listing, so there's no tester list to leave. If you were on the invite-only **canary**
(Internal testing) track and want off that too, say so on
[Discord](https://discord.gg/kaPNvzMuGU).

### Steam Deck — Decky plugin

Uninstall **Punktfunk** from Decky's own plugin list (Quick Access Menu → the **plug** icon (Decky)
→ the **gear** (Settings), where the installed plugins are listed). Decky's uninstall hook does
nothing beyond that — the two Steam shortcuts it created, the Steam Input template, the client it
launched and `~/.config/punktfunk` all survive. The step-by-step is on
[Steam Deck → Uninstalling](/docs/steam-deck#uninstalling); the client itself comes off as in
[Linux — Flatpak](#linux--flatpak) above.

### LG webOS TV

Remove the app from the TV's launcher like any other Homebrew Channel app. [`pf-webos`](https://github.com/dyptan-io/pf-webos)
is a community project — its repository is the place for anything beyond that.

## Plugins and the script runner

Plugins are installed into the host's config directory, so they survive host removal. Take them off
before you uninstall the host, or delete the directories afterwards.

```sh
punktfunk-host plugins list             # what's installed
punktfunk-host plugins remove <name>    # uninstall one
punktfunk-host plugins disable          # stop and disable the runner
```

On Windows run these from an elevated PowerShell; if `punktfunk-host` isn't on your `PATH` yet, use
the full path: `& "$env:ProgramFiles\punktfunk\punktfunk-host.exe" plugins list`.

`plugins remove` takes the plugin's code out of `plugins/`, but nothing else. These stay in the
config directory — `~/.config/punktfunk` (Linux) or `%ProgramData%\punktfunk` (Windows) — whether
you removed the plugins first or uninstalled the host with them still installed:

- `plugins/` — the plugin code itself, for any plugin you did **not** `plugins remove`.
- `plugin-state/<plugin>/` — each plugin's own config and cache, including any API keys you put in
  a plugin's `config.json`. `plugins remove` does not touch this.
- `plugin-token` — the runner's scoped credential for the management API.

Deleting the whole config directory removes all three. The runner package itself
(`punktfunk-scripting` on Linux) comes off with your package manager, as in the sections above; on
Windows it is part of the host installer and goes with it.

## Removing the pairing, not the software

If you only want to undo a pairing, you don't need to uninstall anything. The two halves are
separate:

- **On the host** — unpair the device from the [web console](/docs/web-console); it stops being
  trusted immediately.
- **On a Linux client** — `punktfunk-client --forget-host <fingerprint|host[:port]>` drops a saved
  host from that client's list, and `punktfunk-client --reset` clears all of them plus the stream
  settings (the client keeps its identity, so a re-pair doesn't look like a brand-new device).
- **On a Linux or Windows client** — the headless `punktfunk` command that ships with the same
  package does the same two jobs: `punktfunk hosts forget <host-ref>` for one host,
  `punktfunk reset` for all of them plus the stream settings.

See [Pairing](/docs/pairing) for the full model.
