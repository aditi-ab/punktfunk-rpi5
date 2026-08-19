---
title: Switching from Sunshine, Apollo or Vibeshine
description: Run Punktfunk next to your existing Sunshine-family host while you try it — one port to move — then what maps to what, and how to migrate for good.
---

Sunshine and its forks (**Apollo**, **Vibeshine**, Vibepollo, LuminalShine, …) are Moonlight hosts.
Punktfunk is a different protocol with a Moonlight-compatible mode on the side — so the two can
live on one machine while you decide, as long as you know which port they both want.

## Can I keep Sunshine installed while I try it?

**Yes, with one setting.** Out of the box Punktfunk runs **native-only**: Punktfunk clients, its own
discovery, and a management API. In that mode the only port it shares with a Sunshine-family host
is **TCP 47990** — Sunshine's web UI, Punktfunk's management API. Whoever starts first gets it, and
the loser isn't symmetric: Sunshine merely loses its config page, Punktfunk treats it as fatal and
exits. That's why a shared box can "work until one day it doesn't" — it's a boot race. Move ours:

```sh
# ~/.config/punktfunk/host.env  (Windows: %ProgramData%\punktfunk\host.env)
PUNKTFUNK_MGMT_BIND=0.0.0.0:47991
```

Then restart the host (`systemctl --user restart punktfunk-host`; Windows: `punktfunk-host service
restart`). Nothing else changes: clients learn the port from discovery; the web console, the plugin
runner and the tray read it from `mgmt-endpoint`, which the host rewrites on every start. (Or move
the other host instead — Sunshine and its forks derive every port from one base setting.) Two
caveats: a host you added to a client **by IP address** assumes the default port, so re-add it from
discovery; and if you run a firewall, the `punktfunk-native` profile opens the default port, so allow
the new one too ([Ports & firewall](/docs/ports)).

**On Windows** there's a second overlap: Punktfunk's default display topology is *exclusive* — while
streaming it switches the other displays off so its virtual one is the whole desktop, and re-asserts
that every two seconds. Apollo-family forks are virtual-display-driven, so *their* monitor is what
keeps getting switched off. Pick a different topology in the console (**Host → Virtual displays**) or
set `PUNKTFUNK_NO_ISOLATE=1` — [Virtual displays](/docs/virtual-displays).

**Leave Moonlight compat off while both are installed.** With `PUNKTFUNK_GAMESTREAM=1` Punktfunk
binds the same fixed GameStream ports as Sunshine and advertises the same mDNS name — only one
GameStream host can run at a time. Your Moonlight clients keep talking to Sunshine until you switch;
use a [native Punktfunk client](/docs/install-client) to try Punktfunk meanwhile.

To see what's there and who holds the port:

```sh
punktfunk-host detect-conflicts      # lists Sunshine-family installs; exit 1 only if one runs or autostarts
ss -lptn 'sport = :47990'            # Linux — who has the port right now
netstat -ano | findstr :47990        # Windows
```

A dormant leftover (files on disk, a disabled service) is listed but doesn't fail the check — the
Windows installer and the host's startup log use the same rule. Running both at once is tolerated
for a trial, not supported: if something's odd, stop the other host first.

## What maps to what

| In Sunshine / Apollo | In Punktfunk |
|---|---|
| Web UI on 47990 | [Web console](/docs/web-console) on 47992 — pairing, status, library, displays, logs, updates |
| PIN pairing from the web UI | Same, plus **Approve** without a PIN: connect from the device, approve it in the console ([Pairing](/docs/pairing)) |
| Moonlight clients | Still work, once you turn GameStream compat on ([Moonlight](/docs/moonlight)); the native apps for Mac, iPhone/iPad/Apple TV, Linux, Windows, Android and the Steam Deck are faster and get every feature |
| A virtual display driver (SudoVDA, Vibeshine's, …) | Built in — a display per client at its exact resolution and refresh, on Linux via the compositor, on Windows via Punktfunk's own driver ([Virtual displays](/docs/virtual-displays)) |
| `apps.json` | The [game library](/docs/game-library): launchers come in through [plugins](/docs/plugins) (Steam, Lutris, Heroic, Epic, GOG, Playnite, ROMs), custom titles from the console |
| Per-app prep/undo commands | [Per-app prep/undo](/docs/automation#per-app-prepundo), plus events and hooks |
| `sunshine.conf` | `host.env` for host knobs ([Configuration](/docs/configuration)), the console for display and library policy |
| HDR via the VDD | [HDR](/docs/hdr) — Windows out of the box, Linux on gamescope / GNOME 50 |
| Clipboard, wake-on-LAN | [Shared clipboard](/docs/clipboard), [Wake-on-LAN](/docs/wake-on-lan) |

## Migrating for good

1. **Install** Punktfunk for your system ([Install the Host](/docs/install)) and move the port as
   above. Pair a native client and stream for a while.
2. **Bring your library over** — install the [plugin](/docs/plugins) for each launcher you had in
   `apps.json`; add anything custom from the console's **Library** page.
3. **Stop and uninstall the other host** — e.g. `sudo systemctl disable --now sunshine` on Linux,
   `sc stop SunshineService` then its uninstaller on Windows. On Windows also remove its virtual
   display driver, so only one driver claims the desktop.
4. **Optionally turn on Moonlight compat** for clients that don't have a native app
   (`PUNKTFUNK_GAMESTREAM=1`, open the `punktfunk-gamestream` firewall profile — [Moonlight](/docs/moonlight)).
   Your Moonlight clients pair again, against Punktfunk this time.
5. **Undo the port move** if you like — delete the `PUNKTFUNK_MGMT_BIND` line and restart; clients
   relearn the port from discovery.

Something not behaving? [Troubleshooting → Another streaming host is installed](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed).
