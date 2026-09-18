---
title: The Web Console
description: Enable the Punktfunk browser console, read or reset its login password, arm PIN pairing, and what every page in it does.
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

- **Linux packages (apt / RPM / Arch / Bazzite):** `punktfunk-web` is its own package, and the
  install line on every distro page names it (the Bazzite sysext image already contains it).
  Enable it as your desktop user:

  ```sh
  systemctl --user enable --now punktfunk-web
  # then browse to https://<host-ip>:47992
  ```

  **No console on a box that has the host?** That is the one way this goes wrong: the host package
  only *recommends* the console on apt and RPM, and lists it as an *optional* dependency on Arch
  (pacman never installs those). So a host put on by hand, or by a package manager configured to
  skip weak dependencies (`install_weak_deps=False` in `/etc/dnf/dnf.conf`,
  `APT::Install-Recommends "0"`), has no console. Install it from the same repo the host came
  from — on Arch as a full `-Syu`, never a bare `pacman -S`, to avoid a partial upgrade:

  ```sh
  sudo dnf install punktfunk-web        # Fedora
  sudo apt install punktfunk-web        # Debian / Ubuntu
  sudo pacman -Syu punktfunk-web        # Arch / CachyOS
  systemctl --user enable --now punktfunk-web
  ```

- **Windows host:** the installer sets up the console and its runtime; the Punktfunk Host service
  runs it and brings it back if it ever stops. Nothing to enable — open `https://<this-PC>:47992`.

- **SteamOS host:** the install script builds and starts the console as a user service and prints
  the URL when it finishes.

## Login password

The console is password-protected. The password is stored as a **salted argon2id hash**, so it is
readable exactly once — while it is still the clear line a generated or typed password was written
as. The moment you first sign in, the console replaces that line with the hash. After that there is
nothing to read back, and a forgotten password is **reset**, not recovered.

**Linux packages (apt / RPM / Bazzite).** The guided installer asks, right before it installs: take
a generated password, or type your own. Either way it lands in `~/.config/punktfunk/web-password`
— a generated one is written by `punktfunk-web-init` on the console's first start. Read it before
your first sign-in (the journal names the file but never the password, so the secret stays 0600):

```sh
sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password
```

Prints nothing? Then it is already hashed. **Reset it** — write a clear line back and restart; the
next sign-in hashes it and signs every other session out:

```sh
printf 'PUNKTFUNK_UI_PASSWORD=%s\n' 'your-password' > ~/.config/punktfunk/web-password
chmod 600 ~/.config/punktfunk/web-password
systemctl --user restart punktfunk-web
```

**SteamOS host.** Same idea, but the install script writes the generated password to
`~/.config/punktfunk/web.env` and prints it at the end of the install run. Read it with the `sed`
line above against `web.env`. To reset, **edit** that file — it also holds the session secret, so
replace the `PUNKTFUNK_UI_PASSWORD_HASH=` line with `PUNKTFUNK_UI_PASSWORD=<your-password>` rather
than overwriting the whole file — then `systemctl --user restart punktfunk-web`.

**Windows host.** How you got the password depends on how you installed:

- **The wizard** pre-fills a secure random default, lets you change it, and shows it again on its
  final page.
- **A silent install** — winget, or `/VERYSILENT` — has no wizard. It generates the password and
  displays nothing, so you read it back before you first sign in.

Either way it lives in `%ProgramData%\punktfunk\web-password`, readable only by Administrators and
SYSTEM. Print it from an **elevated** PowerShell:

```powershell
punktfunk-host web password
```

Once it is hashed that command says so instead. To reset, edit the file and restart the Punktfunk
Host service from the same elevated PowerShell:

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

![Live status during a stream: video and audio streaming, the running game, the session's codec, resolution, frame rate and bitrate](/img/console-live-status.png)

- **Dashboard** — the live status above: what's streaming, which games run, how many clients are
  paired. The **Sessions** card lists every connected client — one row each, with its display mode,
  whether it has its own display or joined another session, and how long it has been up. Each row
  stops, keyframes, mutes or changes the access level of that one session; the buttons in the card
  below are host-wide and take every session at once. A Moonlight session has no row controls: the
  host holds no per-session handle for the compat plane.
- **Host** — this host's identity (hostname, OS, local IP, version, unique id), the codecs it
  advertises, its ports, the **Updates** card (see [Updating the Host](/docs/updating)), the
  **GPUs** card — Automatic, or prefer one GPU for capture and encode, applied to the next session
  — and the compositor backends it found. **Host → Settings** changes how the host behaves:
  GameStream, the shared clipboard, video and audio policy. Each change applies to the next stream,
  or after a restart where the setting says so. A setting pinned in `host.env` shows as locked. See
  [Configuration](/docs/configuration#settings-in-the-web-console).
- **Virtual displays** — the policy for the display each session gets, and the Streamed screen
  picker. See [Virtual displays](/docs/virtual-displays).
- **Library** — the games every client sees: turn a launcher source on or off, add or edit a custom
  title with its own art and launch command. See [Your game library](/docs/game-library).
- **Performance** — arm a capture, run a session, stop it, and read the recording back: per-stage
  latency in milliseconds against one frame at the stream's rate, throughput, drops and the round
  trip. See [Recording a capture](/docs/stats#recording-a-capture-for-a-bug-report).
- **Troubleshooting** — the host's health checks above its live log stream, your plugins' lines and
  the logs your clients sent: follow it live, filter by level or source, search it. **Export all**
  saves everything as one file for a bug report — see [Reporting an Issue](/docs/report-an-issue).
- **Pairing** — arm a PIN, approve or deny devices waiting for approval, and unpair a device. A
  second PIN box for [Moonlight/GameStream](/docs/moonlight) clients appears only when this host
  runs the GameStream plane.
- **Plugins** — the plugin store's **Browse**, **Installed** and **Sources** tabs plus the plugin
  runner switch; an installed plugin with a UI gets its own entry below. See
  [Plugins](/docs/plugins).
- **Settings** — the console's language, and **Sign out**.
