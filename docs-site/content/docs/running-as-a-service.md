---
title: Running as a Service
description: Start the host at boot — for a desktop you log into, or a fully headless always-on machine.
---

Running `serve --native` in a terminal is fine for trying punktfunk out. To make a machine an
always-available host, run it as a service. There are two cases.

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

Then bring up a session automatically, depending on your desktop:

### Headless GNOME

Have GDM auto-login your user, so a GNOME Wayland session is always running:

```ini
# /etc/gdm3/custom.conf  (Ubuntu)   ·   /etc/gdm/custom.conf  (Fedora)
[daemon]
AutomaticLoginEnable = true
AutomaticLogin = your-user
```

Then **disable the screen lock** — a locked GNOME session blocks screen capture, and there's no one to
unlock a headless box:

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

Enable the host user service (section A) and reboot. The host comes up on the auto-login session.

### Headless KDE

punktfunk ships a unit that brings up a headless KWin/Plasma session with no display manager, so the
host has a desktop to stream even with no monitor attached:

```sh
cp scripts/punktfunk-kde-session.service scripts/punktfunk-host.service ~/.config/systemd/user/
# host.env: PUNKTFUNK_COMPOSITOR=kwin, WAYLAND_DISPLAY=wayland-kde
systemctl --user daemon-reload
systemctl --user enable punktfunk-kde-session punktfunk-host
sudo loginctl enable-linger "$USER"
reboot
```

The session unit starts headless KWin; the host unit follows it and starts listening. (KWin only needs
to be up by the time a client connects, so the ordering is soft.)

### Headless Bazzite

On Bazzite, the host launches its own gamescope/Steam session per client, so you don't need a separate
session unit — see [Bazzite](/docs/bazzite).

## Verifying

After a reboot, from another machine on the network:

```sh
punktfunk-client-rs --discover     # or just look for the host in the Apple app / Moonlight
```

If the host is listed, it's up. If not, check `journalctl --user -u punktfunk-host` on the host.
