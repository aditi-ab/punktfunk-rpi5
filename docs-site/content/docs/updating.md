---
title: Updating the Host
description: How to see when a newer Punktfunk host is available — the web console's update card — and the update command for every install method.
---

The web console tells you when a newer host is out. The **Host** page has an **Updates** card
showing the version you run, the channel you follow (stable or canary), how this host was
installed, and — once a newer release exists — the exact command that updates it. The
"update available" state also fires an `update.available` event on the host
[event stream](/docs/automation), and a successful update fires `update.applied` (with `from`
and `to`) once the host is back up, so hooks and scripts can react to both.

Your channel comes from the repository this host installs from — see
[Release Channels](/docs/channels) for what each track means and how to move a host between
them. The Updates card never switches channels for you.

The check is a small signed manifest the host fetches from the Punktfunk release feed and
verifies against keys built into the host itself — a tampered or replayed feed is rejected, and
the console says when a check failed rather than silently showing stale facts.

## Updating, per install method

The console shows the right one of these automatically; for reference:

| How you installed | How to update |
|---|---|
| Windows installer | download the newer `punktfunk-host-setup-<version>.exe` from the [releases page](https://git.unom.io/unom/punktfunk/releases) and run it — it upgrades in place, keeping your settings, console password and paired devices |
| Windows via winget | `winget upgrade unom.PunktfunkHost` — **register the Punktfunk source once first**, in an elevated terminal: `winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest` |
| Ubuntu (apt) | `sudo apt update && sudo apt install --only-upgrade punktfunk-host` |
| Fedora (dnf) | `sudo dnf upgrade punktfunk` |
| Bazzite sysext (recommended) | `sudo punktfunk-sysext update` |
| Bazzite / Fedora Atomic rpm-ostree layer | see below — `rpm-ostree upgrade` alone is not enough (staged — reboot to finish) |
| Arch / CachyOS (pacman) | `sudo pacman -Syu` (a normal full system upgrade) |
| Steam Deck (on-device build) | `bash ~/punktfunk/scripts/steamdeck/update.sh --pull` |
| NixOS (flake) | `nix flake update punktfunk` in your flake directory, then rebuild your system |

winget carries **stable** releases only. A Windows host on the canary channel updates by running the
canary installer again — `…/generic/punktfunk-host-windows/canary/punktfunk-host-setup.exe`, see
[Release Channels](/docs/channels).

### rpm-ostree layer: `rpm-ostree upgrade` is not enough

`rpm-ostree upgrade` upgrades the **base image** and only re-resolves layered packages when the
base actually changes — so on a base that sits still (a pinned tag, a paused rebase) it keeps
reporting "No updates available" while newer Punktfunk RPMs sit in the repo. Force it to
re-resolve just the Punktfunk layer, removing and re-adding the same names in one transaction:

```bash
sudo rpm-ostree refresh-md --force
sudo rpm-ostree update \
  --uninstall punktfunk --uninstall punktfunk-web \
  --install   punktfunk --install   punktfunk-web
systemctl reboot
```

Name only the packages you actually layered — `rpm-ostree status` lists them. The new version is
staged; it activates on the next boot.

The re-resolve picks the highest version across **every enabled** `/etc/yum.repos.d/punktfunk*.repo`,
so if the canary repo is enabled alongside the stable one, canary wins and the box quietly tracks
canary — enable exactly the channel you want (see [Release Channels](/docs/channels)). And if this
box runs the Bazzite **sysext**, the sysext shadows any layered copy: update with
`sudo punktfunk-sysext update` instead.

### Bazzite sysext: channels, rollback and rebases

`sudo punktfunk-sysext update` fetches and merges the newest build of whatever channel the last
install wrote to `/etc/punktfunk-sysext.conf` (`status` shows channel, installed and latest
version). To **switch channel**, re-run the install: `sudo punktfunk-sysext install --channel canary`
(or `--channel stable`) — `update` takes no channel flag. To be able to **go back** to a build that
worked, keep a copy of the image before updating, and re-install that file afterwards:

```sh
sudo cp /var/lib/extensions/punktfunk.raw ~/punktfunk-known-good.raw   # before updating
sudo punktfunk-sysext install --from-file ~/punktfunk-known-good.raw   # to go back to it
```

- **After a Bazzite major rebase** (Fedora 43 → 44) the old image **refuses to load** rather than run
  against mismatched system libraries — run `sudo punktfunk-sysext update` once and it fetches the
  image built for the new base.
- **If it refuses the feed.** `refusing to install from an unsigned feed` means that Fedora major's
  feed predates signing; it gets sealed on the next publish. To install from it anyway, accepting
  unauthenticated images, `sudo env PUNKTFUNK_SYSEXT_ALLOW_UNSIGNED=1 bash punktfunk-sysext.sh install`.
  The other message, `the feed's SHA256SUMS is NOT signed by packages@unom.io`, is not the same
  thing — don't install; re-download the script and try again.
- The feed's checksum manifest is OpenPGP-signed by packages@unom.io (key `AF245C506F4E4763`, the
  same one that signs the RPMs) and `punktfunk-sysext` verifies it against a key baked into the
  script, so it needs `gpg` on the box.

### Restart after a Linux package update

Restart the host to pick up the new binary:

```bash
systemctl --user restart punktfunk-host
```

If the update also brought a new `punktfunk-web` (the console — a separate package and a separate
user service), restart that too, and first; the page blinks and reconnects:

```bash
systemctl --user restart punktfunk-web
systemctl --user restart punktfunk-host
```

A `pacman -Syu` upgrades every installed Punktfunk package, so it always needs both. The apt and
dnf commands above name one package, so they usually don't — to move the console and the host
together, name both: `sudo apt install --only-upgrade punktfunk-host punktfunk-web` (apt) or
`sudo dnf upgrade punktfunk punktfunk-web` (dnf). If you enabled the plugin/script runner, it is
a third unit: `systemctl --user restart punktfunk-scripting`.

(The Windows installer restarts the service itself; `punktfunk-sysext update` prints the same
restart hint when it's needed.)

## One-click updating (Windows)

On a Windows host the card shows an **Update now** button instead of a command. It asks for the
console password again (a saved login alone can't restart your host), then the host downloads the
installer, verifies it against the signed release manifest **and** its code signature, and runs it
silently — the service restarts at the end and the page reconnects by itself. If a stream is live
you're warned first: updating drops it.

Every attempt leaves a result in the card (and an installer log under
`C:\ProgramData\punktfunk\logs\update-<version>.log`) — including across the restart, so a failed
update is never silent.

If the newly installed host crash-loops, the service puts the previous installer back on its own
(the last two are kept) and says so in the card — you end up on the version you started from, not
on a dead host. That rollback writes
`C:\ProgramData\punktfunk\logs\update-rollback-from-<version>.log`.

## One-click updating (Linux — opt-in)

The apt, dnf, Bazzite-sysext, and rpm-ostree installs can one-click update too, via a small root
helper the packages ship (`pf-update` + a `punktfunk-update.service` oneshot). It's **off until you
opt in** — a web button that ends in root deserves an explicit decision:

```bash
sudo usermod -aG punktfunk-update $USER    # takes effect within a minute — no re-login needed
```

The console re-checks group membership every minute, so the **Update now** button replaces the
opt-in hint on its own.

That group membership is the entire grant — a polkit rule lets its members start exactly that one
service, whose only job is "run this system's normal package update for the Punktfunk packages,
then prove the new binary runs". The button never chooses versions or URLs; your package manager's
own signed repositories stay the source of truth. The manual command always keeps working.

Per method: on **rpm-ostree** the update is staged and the card says so — reboot to finish (the
console never reboots your machine). On **Arch/pacman** the button additionally requires
`PACMAN_FULL_SYSUPGRADE=1` in `/etc/punktfunk/update.conf`, because the only safe pacman update is
a full `pacman -Syu` — we won't run a partial upgrade. After a successful update the host restarts
itself and the page reconnects.

The **Steam Deck on-device build** gets the button too, with no opt-in (it's your own user's
install, no root involved): it runs the same `update.sh` rebuild the docs describe, which compiles
on the Deck — expect it to take a while; the card shows progress and the log lands in
`~/.config/punktfunk/logs/update-steamos.log`.

## Updating a client

This page is about the host. Clients update on their own tracks — `flatpak update` on Linux, your
normal `apt upgrade` / `dnf upgrade` for the packaged Linux client, a newer `.dmg` dragged over the
old app on macOS, TestFlight on iPhone/iPad/Apple TV, Google Play on Android, and the Decky panel's
own **Update** button on a Steam Deck. The per-platform table is in
[Install a Client → Keeping a client up to date](/docs/install-client#keeping-a-client-up-to-date).
A host and a client don't have to be on the same version, but keeping them close is least
surprising.

## Turning the check off

The check contacts `git.unom.io` (the Punktfunk forge) and nothing else, and sends nothing but a
normal download request. To stop the host checking, add this line to its `host.env`:

```bash
PUNKTFUNK_UPDATE_CHECK=0
```

`host.env` lives at `~/.config/punktfunk/host.env` on Linux and
`%ProgramData%\punktfunk\host.env` on Windows — see [Configuration](/docs/configuration). Then
restart the host: `systemctl --user restart punktfunk-host` on Linux, or restart the Punktfunk
host service on Windows. The card then shows checks as disabled; everything else keeps working.

`PUNKTFUNK_UPDATE_APPLY=0` in the same file removes the **Update now** button on any host,
Windows or Linux; the card shows the manual command instead.

## If the card says the feed is stale

"Feed hasn't changed in over 45 days" means checks *succeed* but nothing new arrives. Usually no
release happened for a while; if the [releases page](https://git.unom.io/unom/punktfunk/releases)
shows something newer than the card does, something between this host and the feed is pinning old
data — look at proxies or DNS on the way to `git.unom.io`. That comparison only works on a
**stable** host: the releases page is stable-only, so a canary host being "behind" it means
nothing.

## Next

- Something went wrong during or after an update? [Troubleshooting](/docs/troubleshooting).
- Want to remove Punktfunk instead, or find out what an uninstall leaves behind?
  [Uninstalling](/docs/uninstall).
