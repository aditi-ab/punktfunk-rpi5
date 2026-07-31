---
title: The Web Console
description: Enable the Punktfunk browser console, read or change its login password, arm PIN pairing, and what every page in it does.
---

The web console is the browser UI for a Punktfunk host — live status, pairing, display policy, the
game library, logs, plugins and host updates. It ships as the **`punktfunk-web`** systemd user unit
on Linux and the **`PunktfunkWeb`** task on Windows, and serves on **`https://<host-ip>:47992`**
(HTTPS with the host's own self-signed identity cert — your browser warns once; trust it and
continue). It's the surface you expose on the LAN to administer the host; the host's own management
API (47990) keeps every admin action loopback-only and off-loopback serves only read-only status +
game-library browsing to paired clients.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## Enable the console

- **Linux packages (apt / RPM / Bazzite):** on Ubuntu the host package is `punktfunk-host`
  and on Fedora/Bazzite it's `punktfunk`; either way it *recommends* `punktfunk-web`, so your
  package manager pulls the console in with the host (the Bazzite sysext image already contains
  it). Enable and start it as your desktop user, then open the URL:

  ```sh
  systemctl --user enable --now punktfunk-web
  # then browse to https://<host-ip>:47992
  ```

- **Arch / CachyOS (pacman):** the console is an *optional* package here, and pacman never installs
  optional dependencies — so install it yourself from the same repo the host came from (see
  [Arch Linux](/docs/arch)), then enable it exactly as above. Take it as a full `-Syu`, never a bare
  `pacman -S`, so you don't end up on a partial upgrade:

  ```sh
  sudo pacman -Syu punktfunk-web
  systemctl --user enable --now punktfunk-web
  ```

- **Windows host:** the installer sets up the console, its runtime, and the `PunktfunkWeb` task and
  starts it at boot. There is nothing to enable — open `https://<this-PC>:47992`.

- **SteamOS host:** the install script builds and starts the console as a user service for you. It
  prints the URL when it finishes.

## Login password

The console is password-protected. Where that password lives and how you change it depends on the
host platform.

**Linux packages (apt / RPM / Bazzite).** On first start `punktfunk-web-init` generates a random
password and saves it to `~/.config/punktfunk/web-password` (as `PUNKTFUNK_UI_PASSWORD=…`). Read it
back from the init service's journal or straight from the file:

```sh
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
```

To set your own, edit that file (`PUNKTFUNK_UI_PASSWORD=<your-password>`) and restart the console:
`systemctl --user restart punktfunk-web`.

**SteamOS host.** Same idea, but the install script writes the generated password to
`~/.config/punktfunk/web.env` and prints it at the end of the install run:

```sh
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web.env
```

Edit that file and `systemctl --user restart punktfunk-web` to change it.

**Windows host.** You choose the password during install — a secure random default is pre-filled and
shown again on the installer's final page. It's stored in `%ProgramData%\punktfunk\web-password` (as
`PUNKTFUNK_UI_PASSWORD=…`), readable only by Administrators and SYSTEM. To change it, edit the file
and restart the task in an **elevated** PowerShell:

```powershell
notepad "$env:ProgramData\punktfunk\web-password"   # set PUNKTFUNK_UI_PASSWORD=<your-password>
schtasks /End /TN PunktfunkWeb; schtasks /Run /TN PunktfunkWeb
```

Forgot it? See [Forgot your Password?](/docs/forgot-password).

## Arm pairing

The host **requires PIN pairing** by default (secure on a LAN). To connect the first time, open the
console, log in, then open **Pairing** in the sidebar and click **Pair a device**. The host shows a
one-time 4-digit PIN — enter it on your [client](/docs/clients) to pair. If the device already tried
to connect it appears under **Waiting for approval** instead; approving it pairs it immediately, no
PIN needed. See [Pairing & Trust](/docs/pairing) for the full trust model and how to approve or
remove devices later.

## What's in it

Nine destinations in the sidebar (a **More** tab on a phone holds the last five):

- **Dashboard** — live status: whether video and audio are streaming, the active sessions with
  their codec, resolution, frame rate and bitrate, which games are running, and how many clients
  are paired. Buttons here stop a session or ask the encoder for a fresh keyframe.
- **Host** — this host's identity (hostname, OS, local IP, version, unique id), the codecs it
  advertises, its ports, the **Updates** card (see [Updating the Host](/docs/updating)), the
  **GPUs** card — Automatic, or prefer one GPU for capture and encode, applied to the next session
  — and the compositor backends it found.
- **Virtual displays** — the policy for the display each session gets, and the Streamed screen
  picker. See [Virtual displays](/docs/virtual-displays).
- **Library** — the games every client sees: turn a launcher source on or off, add or edit a custom
  title with its own art and launch command. See [Your game library](/docs/game-library).
- **Performance** — arm a capture, run a session, stop it, and read the recording back as
  per-stage latency, throughput and health graphs.
- **Logs** — the host's recent log stream: follow it live, filter by level, search it, and download
  or share it for a bug report.
- **Pairing** — arm a PIN, approve or deny devices waiting for approval, and unpair a device. A
  second PIN box for [Moonlight/GameStream](/docs/moonlight) clients appears only when this host
  runs the GameStream plane.
- **Plugins** — the plugin store's **Browse**, **Installed** and **Sources** tabs plus the plugin
  runner switch; an installed plugin with a UI gets its own entry below. See
  [Plugins](/docs/plugins).
- **Settings** — the console's language, and **Sign out**.
