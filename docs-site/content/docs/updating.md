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

One-click updating from the console is on the way, per install method — the card will grow an
**Apply** button where the platform supports it.

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
