---
title: Quick Start
description: From nothing to streaming — set up a host and connect your first client.
---

This is the shortest path to a working stream. Each step links to the details.

> A streaming host is remote control of the machine, so it's built for **trusted local networks** — keep
> it on your LAN or a VPN and don't expose it to the internet. Two minutes on
> [Security & Safe Use](/docs/security) before you start is worth it.

## 1. Set up the host

On your gaming machine (NVIDIA, AMD, or Intel GPU), follow the install guide for your system:

- [Ubuntu](/docs/ubuntu)
- [Fedora](/docs/fedora)
- [Arch](/docs/arch)
- [Bazzite](/docs/bazzite)
- [SteamOS](/docs/steamos-host)
- [Windows host](/docs/windows-host)

Each one covers the GPU driver, the dependencies, and how to install and run the host. After
installing, configure for your desktop ([KDE](/docs/kde) / [GNOME](/docs/gnome) /
[gamescope](/docs/gamescope) / [Hyprland](/docs/hyprland) / [Sway](/docs/sway)). Check the
[Requirements](/docs/requirements) first if you're not sure your machine is a fit.

## 2. Start the host

**On Windows there's nothing to start.** The installer registered and started the `PunktfunkHost`
service, so the host is already running and comes back on every boot. Confirm it:

```powershell
Get-Service PunktfunkHost
```

Don't run `punktfunk-host serve` on top of it — a second host process refuses to touch the
virtual-display driver and collides with the service on its ports. Details:
[Running as a Service → Windows](/docs/running-as-a-service#windows).

**On Linux you don't run the host by hand either.** Every Linux package ships a systemd **user** unit.
That unit reads `~/.config/punktfunk/host.env` and won't start until the file exists, so put it in
place first — your distro and desktop pages (step 1) have the template to copy and what to put in it.
Then enable the unit once, from a terminal **inside your desktop session**, and it comes back at
every login:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f   # watch it come up and print its identity fingerprint
```

Once up, the host advertises itself on your local network, so clients find it by name. It works out
where your compositor is by itself, so there is nothing to export.

That unit runs the **secure native-only host** (Punktfunk clients + the management API). Want stock
[Moonlight](/docs/moonlight) clients too? GameStream compat is **opt-in** — add
`PUNKTFUNK_GAMESTREAM=1` to `~/.config/punktfunk/host.env` and restart the unit (trusted LANs only;
its pairing runs over plain HTTP) — see
[What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

If the host runs a firewall (Fedora enables firewalld, CachyOS enables ufw), open its ports — the
firewall step in your distro guide has the exact commands: `punktfunk-native` always, plus
`punktfunk-gamestream` only if you enabled Moonlight compat.

On **SteamOS** even that is done for you — the install script wrote its own `punktfunk-host` user
service and started it (GameStream on by default there too; pass `--no-gamestream` to the install
script for a native-only host). Check it with `systemctl --user status punktfunk-host`.

## 3. Open the web console

The console is a **separate** process from the host, and you need it in the next step — arming PIN
pairing is done there.

- **Windows:** the installer already set it up and starts it at boot. Open
  `https://<host-ip>:47992`. The login password is the one the installer showed you on its final
  page; it's stored in `%ProgramData%\punktfunk\web-password`.
- **Linux:** on Ubuntu, Fedora and Bazzite the `punktfunk-web` package comes in with the host
  but isn't enabled for you. On Arch it's an optional dependency, so install it first
  (`sudo pacman -Syu punktfunk-web` — a full `-Syu`, never a bare `-S`). Either way, start it as
  your desktop user, then open `https://<host-ip>:47992`:

  ```sh
  systemctl --user enable --now punktfunk-web
  sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
  ```

  The second command prints the login password generated on first start.
- **SteamOS:** the install script already started the console and printed its URL when it finished.
  It generated the login password into `~/.config/punktfunk/web.env`.

The certificate is the host's own self-signed one, so your browser warns once — trust it and
continue. Full details: [The Web Console](/docs/web-console).

## 4. Connect and pair a client

On the device you want to stream to, use a [native Punktfunk client](/docs/clients) for the lowest
latency, or any Moonlight client:

- **Native client (Apple, Linux, Windows, Android):** install it first —
  [Install a Client](/docs/install-client) has the download for every device (Steam Deck: the
  [Decky plugin](/docs/steam-deck)). Then open the Punktfunk app — your host appears in the list of
  hosts found on your network. Select it, and when prompted, **pair**.
- **Anything with Moonlight:** add the host (it should be discovered automatically), then pair.

To pair, the host needs to show a PIN. [Arm pairing](/docs/web-console#arm-pairing) from the web
console you opened in step 3 — the host displays a 4-digit PIN, you type it into the client, and they
trust each other from then on. Pairing is required by default. Full details:
[Pairing & Trust](/docs/pairing).

## 5. Stream

Once paired, select the host and start streaming. The host creates a virtual display at your device's
resolution and refresh, and the picture comes up. Mouse, keyboard, and controllers flow back to the
host.

Worth knowing before you need it: on the desktop clients the stream *takes* your mouse and keyboard
when it starts, and again whenever you click into it. **Ctrl+Alt+Shift+Q** (**⌃⌥⇧Q** on macOS) hands
them back. The other in-stream shortcuts, and the mouse, touch and pen modes, are on
[Mouse, touch and pen](/docs/input).

## Now that it works

Punktfunk does more than mirror a screen:

- Browse the host's installed games and launch one straight into the stream —
  [Game library](/docs/game-library).
- Save named stream settings, bind them to a host, and start a session from a shortcut or a script —
  [Profiles and links](/docs/profiles-and-links).
- Copy on one machine and paste on the other — [Shared clipboard](/docs/clipboard).
- Connect to a host that's asleep — [Wake-on-LAN](/docs/wake-on-lan).
- Get a 10-bit HDR picture where the whole chain allows it — [HDR](/docs/hdr).

## Keep it running

- Tune the picture — resolution, refresh, bitrate, codec and HDR are all
  [client settings](/docs/client-settings); the host's own knobs are in
  [Configuration](/docs/configuration).
- Make it always-on — no login, no monitor: [Running as a Service](/docs/running-as-a-service).
- Keep it current with [Updating](/docs/updating); changed your mind? [Uninstall](/docs/uninstall).
- Hit a snag? See [Troubleshooting](/docs/troubleshooting).
