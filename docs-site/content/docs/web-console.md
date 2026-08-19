---
title: The Web Console
description: Enable the Punktfunk browser console, read or change its login password, arm PIN pairing, and what every page in it does.
---

The web console is the browser UI for a Punktfunk host — live status, pairing, display policy, the
game library, logs, plugins and host updates. It ships as the **`punktfunk-web`** systemd user unit
on Linux, runs under the **Punktfunk Host service** on Windows, and serves on **`https://<host-ip>:47992`**
(HTTPS with the host's own self-signed identity cert — your browser warns once; trust it and
continue). It's the surface you expose on the LAN to administer the host; the host's own management
API (47990) keeps every admin action loopback-only and off-loopback serves only read-only status and
game-library browsing to paired clients.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

## Two ports, not one

The console also listens on **TCP 47993**, where plugin interfaces are served — same host, same
certificate, **different port**.

That is a deliberate boundary. A plugin's interface is third-party code; on the console's own port
the browser would let it act as you, with your logged-in session, against every admin action the
console can reach. A different port is a different *origin*, so the browser keeps the two apart —
but the same *site*, so your login still carries over.

In practice:

- **Open 47993 alongside 47992** on the host's firewall if you browse the console from another
  device. The packaged firewall profiles already list both.
- **Trust the certificate twice.** Browsers store a self-signed certificate exception *per port*.
  The first time you open a plugin, the console notices it can't reach 47993 yet and offers a link
  to open it in a tab — accept the warning there once and it works from then on.
- If a plugin's page is an empty panel, see
  [A plugin's interface doesn't load](/docs/troubleshooting#a-plugins-interface-doesnt-load).

## Enable the console

- **Linux packages (apt / RPM / Bazzite):** the host package (`punktfunk-host` on Ubuntu,
  `punktfunk` on Fedora/Bazzite) *recommends* `punktfunk-web`, so your package manager pulls the
  console in with the host (the Bazzite sysext image already contains it). Enable it as your
  desktop user:

  ```sh
  systemctl --user enable --now punktfunk-web
  # then browse to https://<host-ip>:47992
  ```

- **Arch / CachyOS (pacman):** the console is an *optional* package and pacman never installs
  optional dependencies — install it from the same repo the host came from (see
  [Arch Linux](/docs/arch)), then enable it as above. Use a full `-Syu`, never a bare `pacman -S`,
  to avoid a partial upgrade:

  ```sh
  sudo pacman -Syu punktfunk-web
  systemctl --user enable --now punktfunk-web
  ```

- **Windows host:** the installer sets up the console and its runtime; the Punktfunk Host service
  runs it and brings it back if it ever stops. Nothing to enable — open `https://<this-PC>:47992`.

- **SteamOS host:** the install script builds and starts the console as a user service and prints
  the URL when it finishes.

## Login password

The console is password-protected; where the password lives and how you change it depends on the
host platform.

**Linux packages (apt / RPM / Bazzite).** On first start `punktfunk-web-init` generates a random
password and saves it to `~/.config/punktfunk/web-password` (as `PUNKTFUNK_UI_PASSWORD=…`). Read it
from the init service's journal or the file:

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
and restart the Punktfunk Host service from an **elevated** PowerShell:

```powershell
notepad "$env:ProgramData\punktfunk\web-password"   # set PUNKTFUNK_UI_PASSWORD=<your-password>
punktfunk-host service restart
```

Forgot it? See [Forgot your Password?](/docs/forgot-password).

## Arm pairing

The host **requires PIN pairing** by default (secure on a LAN). To connect the first time, log in to
the console, open **Pairing** in the sidebar and click **Pair a device**. The host shows a one-time
4-digit PIN — enter it on your [client](/docs/clients). If the device already tried to connect it
appears under **Waiting for approval** instead; approving it pairs it immediately, no PIN needed.
[Pairing & Trust](/docs/pairing) has the full trust model and how to approve or remove devices later.

## What's in it

Nine destinations in the sidebar (a **More** tab on a phone holds the last five):

- **Dashboard** — live status: whether video and audio are streaming, the active sessions with
  their codec, resolution, frame rate and bitrate, which games are running, and how many clients
  are paired. Buttons stop a session or ask the encoder for a fresh keyframe.
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
- **Logs** — the host's recent log stream *and your plugins'*: follow it live, filter by level or
  producer, search it, and download or share it for a bug report. Plugin lines are tagged
  `plugin:<name>` and the **Host / Plugins** switch isolates either side.
- **Pairing** — arm a PIN, approve or deny devices waiting for approval, and unpair a device. A
  second PIN box for [Moonlight/GameStream](/docs/moonlight) clients appears only when this host
  runs the GameStream plane.
- **Plugins** — the plugin store's **Browse**, **Installed** and **Sources** tabs plus the plugin
  runner switch; an installed plugin with a UI gets its own entry below. See
  [Plugins](/docs/plugins).
- **Settings** — the console's language, and **Sign out**.
