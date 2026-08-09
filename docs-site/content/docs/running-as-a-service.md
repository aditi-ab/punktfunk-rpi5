---
title: Running as a Service
description: Start the host at boot — for a desktop you log into, or a fully headless always-on machine.
---

Running `serve` in a terminal is fine for trying Punktfunk out. To make a machine an
always-available host, run it as a service. First, what that service starts — then the two cases, a
desktop you log into and a fully headless box.

## What the unit starts

The bundled unit runs `serve --gamestream`, so it serves both the native `punktfunk/1` plane and
stock [Moonlight](/docs/moonlight) clients. Every Linux package installs that unit as it ships and
only rewrites the binary path, so a host you installed from apt, dnf, pacman or the Bazzite sysext
has **GameStream on**. See what yours starts:

```sh
systemctl --user cat punktfunk-host
```

For a **native-only** host (no GameStream — its pairing runs over plain HTTP and its legacy
encryption is weaker; see [Security & Safe Use](/docs/security)), drop the flag. The packaged unit is
a package file that an upgrade replaces, so override `ExecStart` with a drop-in rather than editing
it:

```sh
systemctl --user edit punktfunk-host
```

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/punktfunk-host serve
```

The empty `ExecStart=` is required — without it systemd adds a second command instead of replacing
the first — and the path must match the one `systemctl --user cat` printed (the distro packages use
`/usr/bin`). Save, then `systemctl --user restart punktfunk-host`.

Windows is the other way round: an install from the setup `.exe` leaves GameStream **off** unless you
tick it, and it is configured differently — see [Windows](#windows) below.

## A. A desktop you log into

If you sit at the machine (or it auto-logs-in to a desktop), run the host as a **systemd user
service** that starts with your session.

**Put your `host.env` in place first.** The unit reads `~/.config/punktfunk/host.env` and won't start
until that file exists — no package creates it for you, they only ship a template to copy. The
defaults in it are right for an ordinary desktop; your distro and desktop guides say if yours wants
a different template (on Bazzite it's `host.env.bazzite`):

```sh
mkdir -p ~/.config/punktfunk
# /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu
cp /usr/share/punktfunk/host.env.example ~/.config/punktfunk/host.env
```

**Installed from a package** (apt, dnf, pacman, or the Bazzite sysext) — the unit is already at
`/usr/lib/systemd/user/punktfunk-host.service`, with its `ExecStart` pointing at the installed
binary. There's nothing to copy:

```sh
systemctl --user daemon-reload             # the sysext route needs this; harmless elsewhere
systemctl --user enable --now punktfunk-host
```

**Built from source** — install the unit from your checkout, and take `host.env` from there too
(`cp scripts/host.env.example ~/.config/punktfunk/host.env`). The unit's `ExecStart` points at
`%h/punktfunk/target/release/punktfunk-host` (`%h` is your home directory), so edit the copy if your
checkout lives somewhere else:

```sh
mkdir -p ~/.config/systemd/user
cp scripts/punktfunk-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-host
```

Don't do the copy on a packaged install: a unit in `~/.config/systemd/user/` shadows the packaged
one, and the source unit points at a build tree you don't have — the service then fails with
`status=203/EXEC`.

The host now starts whenever you log in. Check it with `systemctl --user status punktfunk-host`.

**You don't need to export anything for it.** The host finds the live compositor session itself on
every connect and works out where to reach it (`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, the session bus,
sway's `SWAYSOCK`, Hyprland's instance signature) from the running compositor — so `host.env` is for
policy, not session plumbing, and `systemctl --user import-environment` is not a prerequisite.

### Restart the host with your desktop

Add one drop-in so the host follows your session's lifetime:

```sh
mkdir -p ~/.config/systemd/user/punktfunk-host.service.d
# /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu,
# scripts/ in a source checkout
cp /usr/share/punktfunk/punktfunk-host-desktop-session.conf \
   ~/.config/systemd/user/punktfunk-host.service.d/desktop-session.conf
systemctl --user daemon-reload
systemctl --user reenable punktfunk-host
systemctl --user restart punktfunk-host
```

Without it, restarting Plasma or GNOME — a crash, a log out and back in, "restart the shell" — leaves
the host running against a compositor that no longer exists. It keeps listening and answering, and
every session after that fails at capture, which is a confusing way to find out. The drop-in makes a
compositor restart a host restart.

Skip it on the headless/appliance route below (which has its own session unit), and on **Sway or
Hyprland**, which don't hand their session to systemd: they never reach `graphical-session.target`, so
the drop-in is harmless there but does nothing. To make the host come and go with the session on
those, start it from the compositor's own config instead of enabling the unit — add
`exec systemctl --user start punktfunk-host` to your sway config, or
`exec-once = systemctl --user start punktfunk-host` to Hyprland's — and leave the unit itself
disabled (`systemctl --user disable punktfunk-host`), so it isn't also started at login.

## B. A headless, always-on host

To run with **no monitor and no login** — a machine in a closet that's always ready — you need two
things: a desktop session that comes up at boot, and the host service started without a login.

Start by making the host service start at boot even when nobody logs in:

```sh
sudo loginctl enable-linger "$USER"
```

Then bring up a session automatically. How you do that is desktop-specific — auto-login, lock
disable, and the session unit differ per compositor, so each is documented on its own page:

- GNOME: [GNOME → Headless session](/docs/gnome#headless-session).
- KDE Plasma: [KDE → Headless session](/docs/kde#headless-session).
- Steam / gamescope: [gamescope](/docs/gamescope) — the host launches its own session per client, so
  there's no separate session unit.

Once a session comes up at boot, enable the host user service (section A) and reboot. The host comes up
on that session.

### Headless Bazzite

On Bazzite, the host launches its own gamescope/Steam session per client, so you don't need a separate
session unit — see [Bazzite](/docs/bazzite) and [gamescope](/docs/gamescope).

## Windows

> Punktfunk has first-class **Linux and Windows** hosts. On Windows it ships as a signed installer
> with an SCM service and a virtual-display driver — including Punktfunk's own **indirect display
> driver** the host pushes frames straight into. The Windows host is newer than the Linux host. (Not
> to be confused with the Windows *client*, which streams *to* a Windows PC.)

On Windows the host runs as a `LocalSystem` service that launches into the interactive session, so it
captures the secure desktop (UAC / lock screen) and survives reboots with nobody logged in — the same
model Sunshine/Apollo use. Because it runs at that privilege level, keep it on a trusted network and be
deliberate about which machine you host on — see [Security & Safe Use](/docs/security).

The easy path is the **signed installer**: download `punktfunk-host-setup-<ver>.exe` from the package
registry ([`punktfunk-host-windows`](https://git.unom.io/unom/-/packages)) and run it. It drops the host
into `C:\Program Files\punktfunk`, installs the bundled **pf-vdisplay** virtual-display driver, and
registers + starts the service for you (`/VERYSILENT` for unattended). Upgrades and uninstall are
handled through Add/Remove Programs.

Prefer the CLI? Run `punktfunk-host service install` from an elevated prompt — see
[Windows Host](/docs/windows-host). For hardware encode you need a GPU — NVIDIA (NVENC), AMD (AMF), or
Intel (QSV); the host falls back to software H.264 without one.

**GameStream on Windows.** Unlike the Linux unit, the installer leaves Moonlight compatibility
**off** — it's a checkbox in the wizard (`/MERGETASKS="gamestream"` to select it unattended). There's
no `ExecStart` to edit here: the service launches whatever `PUNKTFUNK_HOST_CMD` in
`%ProgramData%\punktfunk\host.env` says, which is also where the rest of the Windows host's
configuration lives. To change it later, from an elevated prompt:

```powershell
punktfunk-host service install --gamestream=on   # or --gamestream=off
punktfunk-host service restart
```

Registering the service by hand is the exception. A bare `punktfunk-host service install` writes a
fresh `host.env` with `PUNKTFUNK_HOST_CMD` left commented out, and with no value set the service
falls back to `serve --gamestream` — so add `--gamestream=off` to that command if you want the
native-only host.

> **Firewall scope.** The installer opens the streaming + console ports on **Private and Domain**
> networks only — not **Public**. If your LAN is (mis)classified Public, clients won't connect until
> you set it to Private (Windows Settings → Network), and the host logs a warning when it's on a Public
> network. For a trusted network Windows insists is Public, tick **"Allow connections on Public
> networks"** at install (or pass `--allow-public-network` to `service install`). See
> [Security & Safe Use](/docs/security) for the reasoning.

## Verifying

After a reboot, from another machine on the network:

```sh
punktfunk reachable 192.168.1.50   # exit 0 = the host answered, 2 = it didn't
punktfunk hosts list --probe       # every saved host, online or offline
```

`punktfunk` is the headless client CLI — it ships in the Linux client packages (`punktfunk-client`)
and with the Windows client. From a source checkout, `punktfunk-probe --discover` browses the LAN
instead; it's a dev tool and isn't packaged. Or just open a native client / Moonlight and look for
the host.

If the host answers, it's up. If not, check `journalctl --user -u punktfunk-host` on the host — on
a Windows host, run `punktfunk-host service status` from an elevated prompt on the machine itself.

## GPU scheduling priority

The [PyroWave](/docs/pyrowave) codec encodes on the same GPU shader cores your game is using, so a
demanding game can crowd it out and the stream's frame rate drops with it. The fix is to ask the
driver to schedule that encode ahead of the game, and every driver we tested gates the request on a
single Linux capability, `CAP_SYS_NICE`. The other codecs use a separate video engine on the GPU and
are unaffected either way.

That capability cannot live on the host, so it lives next to it. Every way of installing a Linux
host — apt, dnf, pacman, the Bazzite sysext, the NixOS module, the Steam Deck script — ships a
second, deliberately small program alongside it, **`punktfunk-encode-worker`**, and grants
`cap_sys_nice=ep` to *that*. The host starts one per PyroWave session, hands it the captured frames,
and takes the compressed video back; the worker talks to nothing else, not your desktop and not the
network.
`punktfunk-host` itself carries **no capability, on any channel** — the posture it has had since
0.25.0, and the one KDE needs.

> **Never `setcap` `punktfunk-host`.** Not by hand, not through a systemd `AmbientCapabilities=`
> line, not through a NixOS `security.wrappers` entry. All three put the capability in the same
> place, and all three take KDE desktop streaming away completely. There is no capability the host
> wants: the worker is the thing that needs one, and your packages already gave it one.

Here is why a privileged host is so much worse than a privileged worker. To hand the host its
virtual display, KWin first has to work out *which* program is asking, which it does by reading the
connecting process's `/proc/<pid>/exe` and matching it against the `.desktop` file the packages
install. Linux refuses that read unless the reader already holds every capability the target holds —
and KWin holds none. So a host carrying a capability is a host KWin cannot identify, its restricted
protocols are never offered, and every session fails at capture with:

```
KWin virtual output failed: KWin does not expose zkde_screencast_unstable_v1 to this client
```

which reads exactly like a missing or mis-installed `.desktop` file and survives reinstalling both
ends. Version 0.26.0-1 granted the host the capability for the reason above and shipped precisely
this, on every Linux channel; 0.26.0-2 revoked it everywhere. If you ever see that error, check the
binaries before anything else — the host's own message names the capability when it finds one:

```sh
getcap /usr/bin/punktfunk-host              # correct output is nothing at all
getcap /usr/bin/punktfunk-encode-worker     # /usr/bin/punktfunk-encode-worker cap_sys_nice=ep

sudo setcap -r /usr/bin/punktfunk-host      # clear it, then restart the host
```

On the Bazzite image `/usr` is read-only, so there is nothing to repair in place — take the next
image (`sudo punktfunk-sysext update`). On NixOS the worker *is* wrapped, because a file capability
cannot live on a read-only store path: the module creates the wrapper and points the host at it, and
the host's own `ExecStart` stays on the plain store path.

**The grant is best-effort, and no session depends on it.** A worker without the capability still
encodes — it asks for the elevated priority, is refused, says so once, and runs at the normal one.
So does a host that cannot find or start a worker at all: it encodes in-process, logs one line, and
streams. The only thing at stake is frame pacing under a GPU-bound game.
`PYROWAVE_QUEUE_PRIORITY=off` stops the host asking for priority, and `PUNKTFUNK_ENCODE_WORKER=off`
keeps the encode in the host process — both on
[Configuration](/docs/configuration#advanced-performance-tuning).

## Stopping and removing

After a Linux package update the user service keeps running the old binary until it's restarted, and
a package can't restart another user's `--user` units for you — [Updating](/docs/updating) has the
update command for every install method and the restart that finishes the job. The Windows installer
restarts its own service.

To stop the host for now:

```sh
systemctl --user stop punktfunk-host          # Linux
punktfunk-host service stop                   # Windows, elevated prompt
```

To stop it for good, so it doesn't come back at login or boot:

```sh
# add punktfunk-web (the console) and punktfunk-kde-session (the headless KDE route) if you enabled them
systemctl --user disable --now punktfunk-host
rm -rf ~/.config/systemd/user/punktfunk-host.service.d   # any drop-ins you added
sudo loginctl disable-linger "$USER"                     # only if you enabled lingering
```

On Windows, `punktfunk-host service uninstall` from an elevated prompt stops the service, removes it,
and removes the firewall rules it added. To remove the whole install instead, use Add/Remove Programs.

Neither removes `~/.config/punktfunk` (Linux) or `%ProgramData%\punktfunk` (Windows) — your
certificate, pairings and console password stay, so a reinstall picks up where you left off. See
[Uninstall](/docs/uninstall) to clear them out.
