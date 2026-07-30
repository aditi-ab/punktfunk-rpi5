---
title: Updating the Host
description: How to see when a newer punktfunk host is available — the web console's update card — and the update command for every install method.
---

The web console tells you when a newer host is out. The **Host** page has an **Updates** card
showing the version you run, the channel you follow (stable or canary), how this host was
installed, and — once a newer release exists — the exact command that updates it. The
"update available" state also fires an `update.available` event on the host
[event stream](/docs/automation), so hooks and scripts can react to it too.

The check is a small signed manifest the host fetches from the punktfunk release feed and
verifies against keys built into the host itself — a tampered or replayed feed is rejected, and
the console will tell you when a check failed rather than silently showing stale facts.

## Updating, per install method

The console shows the right one of these automatically; for reference:

| How you installed | How to update |
|---|---|
| Windows installer / winget | `winget upgrade unom.PunktfunkHost`, or run the newer `punktfunk-host-setup.exe` |
| Ubuntu / Debian (apt) | `sudo apt update && sudo apt install --only-upgrade punktfunk-host` |
| Fedora (dnf) | `sudo dnf upgrade punktfunk` |
| Bazzite sysext (recommended) | `sudo punktfunk-sysext update` |
| Bazzite rpm-ostree layer | `sudo /usr/share/punktfunk/update-punktfunk.sh` (staged — reboot to finish) |
| Arch / CachyOS (pacman) | `sudo pacman -Syu` (a normal full system upgrade) |
| Steam Deck (on-device build) | `bash ~/punktfunk/scripts/steamdeck/update.sh --pull` |
| NixOS | update the flake input and rebuild |

After a Linux package update, restart the host to pick up the new binary:

```bash
systemctl --user restart punktfunk-host
```

(The Windows installer restarts the service itself; `punktfunk-sysext update` prints the same
restart hint when it's needed.)

## One-click updating (Windows)

On a Windows host the card shows an **Update now** button instead of a command. It asks for the
console password again (a saved login alone can't restart your host), then the host downloads
the installer, verifies it against the signed release manifest **and** its code signature, and
runs it silently — the service restarts at the end and the page reconnects by itself. If a
stream is live you'll be warned first: updating drops it.

Every attempt leaves a result in the card (and an installer log under
`C:\ProgramData\punktfunk\logs\update-<version>.log`) — including across the restart, so a
failed update is never silent. To disable the button entirely on a host, set
`PUNKTFUNK_UPDATE_APPLY=0` in `host.env`; the card then shows the manual command instead.

## One-click updating (Linux — opt-in)

The apt, dnf, Bazzite-sysext, and rpm-ostree installs can one-click update too, via a small
root helper the packages ship (`pf-update` + a `punktfunk-update.service` oneshot). It's **off
until you opt in**, because a web button that ends in root deserves an explicit decision:

```bash
sudo usermod -aG punktfunk-update $USER    # then log out and back in
```

That group membership is the entire grant — a polkit rule lets its members start exactly that
one service, whose only job is "run this system's normal package update for the punktfunk
packages, then prove the new binary runs". The button never chooses versions or URLs; your
package manager's own signed repositories stay the source of truth. The card shows the opt-in
command until you've done this, and the manual command always keeps working.

Notes per method: on **rpm-ostree** the update is staged and the card will say so — reboot to
finish (the console never reboots your machine). On **Arch/pacman** the button additionally
requires `PACMAN_FULL_SYSUPGRADE=1` in `/etc/punktfunk/update.conf`, because the only safe
pacman update is a full `pacman -Syu` — partial upgrades are how Arch boxes break, and we
won't run one. After a successful update the host restarts itself and the page reconnects.

## Turning the check off

The check contacts `git.unom.io` (the punktfunk forge) and nothing else, and sends nothing but a
normal download request. If you'd rather the host never checks, set:

```bash
PUNKTFUNK_UPDATE_CHECK=0
```

in the host's environment (`host.env` on Windows, the systemd user unit environment on Linux).
The card then shows checks as disabled; everything else keeps working.

## If the card says the feed is stale

"Feed hasn't changed in over 45 days" means checks *succeed* but nothing new arrives. Usually
that just means no release happened for a while; if the [releases page](https://git.unom.io/unom/punktfunk/releases)
shows something newer than the card does, something between this host and the feed is pinning old
data — worth a look at proxies or DNS on the way to `git.unom.io`.
