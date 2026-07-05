---
title: Running as a Service
description: Start the host at boot — for a desktop you log into, or a fully headless always-on machine.
---

Running `serve` in a terminal is fine for trying punktfunk out. To make a machine an
always-available host, run it as a service. There are two cases.

> The bundled unit `scripts/punktfunk-host.service` runs `serve --gamestream`, so it serves both the
> native `punktfunk/1` plane and stock [Moonlight](/docs/moonlight) clients. For a **secure native-only
> host** (no GameStream — its pairing runs over plain HTTP and its legacy encryption is weaker), drop
> `--gamestream` from the unit's `ExecStart` and use bare `serve`.

## A. A desktop you log into

If you sit at the machine (or it auto-logs-in to a desktop), run the host as a **systemd user
service** that starts with your session:

```sh
mkdir -p ~/.config/systemd/user
cp scripts/punktfunk-host.service ~/.config/systemd/user/
# Put your host.env in place first — see the setup guide for your desktop.
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-host
```

The host now starts whenever you log in. Check it with `systemctl --user status punktfunk-host`.

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

> punktfunk has first-class **Linux and Windows** hosts. On Windows it ships as a signed installer
> with an SCM service and a virtual-display driver — including punktfunk's own **indirect display
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

> **Firewall scope.** The installer opens the streaming + console ports on **Private and Domain**
> networks only — not **Public**. If your LAN is (mis)classified Public, clients won't connect until
> you set it to Private (Windows Settings → Network), and the host logs a warning when it's on a Public
> network. For a trusted network Windows insists is Public, tick **"Allow connections on Public
> networks"** at install (or pass `--allow-public-network` to `service install`). See
> [Security & Safe Use](/docs/security) for the reasoning.

## Verifying

After a reboot, from another machine on the network:

```sh
punktfunk-probe --discover     # or just look for the host in a native client / Moonlight
```

If the host is listed, it's up. If not, check `journalctl --user -u punktfunk-host` on the host.
