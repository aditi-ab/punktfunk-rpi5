---
title: Troubleshooting
description: Common problems setting up or using a Punktfunk host, and how to fix them.
---

## Another streaming host (Sunshine, Apollo, …) is installed

Punktfunk is a Moonlight-compatible host. So are **Sunshine** and its forks (**Apollo**,
**Vibeshine**, **Vibepollo**, **LuminalShine**, …). Running one of them **at the same time** as
Punktfunk is **not supported**: they bind the *same* GameStream ports (47984/47989 and
47998–48010, plus a web UI on 47990 that collides with Punktfunk's management API), advertise the
*same* `_nvstream` mDNS name, and often install a *conflicting virtual-display driver*. The result
is `address already in use` errors, pairing that silently fails, the wrong host answering a client,
and capture/display glitches.

- Punktfunk detects this automatically. It warns in the host's startup log — so it's on the web
  console's **Logs** page — and carries the finding in the status summary the management API
  serves. The Windows installer additionally warns before installing, but only when a competing
  host's *service* is set to start on its own; a dormant install isn't flagged. The tray icon
  doesn't flag it at all. To check on demand, run:

  ```sh
  punktfunk-host detect-conflicts
  ```

  It lists any conflicting host found (installed or running) and exits non-zero if there is one.
- **Fix:** stop and uninstall the other host, then start Punktfunk — e.g. stop the service
  (`sudo systemctl disable --now sunshine` / on Windows `sc stop SunshineService`) and uninstall it.
  If you only want to try Punktfunk without removing the other host, at least make sure the other
  host is fully **stopped** first (they cannot both run at once).

## The host isn't found on the network

- Make sure the host is actually running — on Linux `systemctl --user status punktfunk-host` (or you
  see it listening in the terminal); on Windows `punktfunk-host service status`, and if it isn't
  running see [Windows: the host or the web console won't
  start](#windows-the-host-or-the-web-console-wont-start).
- **On an Android phone or TV, check the app's local-network permission.** On Android 17 and newer,
  Android blocks Punktfunk from touching anything on your LAN — discovery, the connect itself,
  Wake-on-LAN, the game library — until you allow it. The app asks when you open the host list, and
  a denial looks exactly like a host that isn't there. Tap **Allow…** under
  *Local network access is off* at the top of the host list, or enable **Nearby devices** for
  Punktfunk in Android's app settings.
- Host and client must be on the **same network/subnet**. Discovery uses mDNS, which doesn't cross
  routed subnets or most VPNs-without-multicast. As a fallback, add the host by **IP address** in your
  client.
- A firewall on the host can block it. The native protocol needs **two** fixed UDP ports open:
  **9777** (the QUIC control plane) and **5353** (mDNS — this is the one discovery itself runs on).
  On Linux the packages ship a ready-made `punktfunk-native` rule that opens both, plus TCP 47990 for
  the library API:

  ```sh
  sudo ufw allow punktfunk-native                              # ufw (CachyOS, Ubuntu)
  sudo firewall-cmd --permanent --add-service=punktfunk-native \
    && sudo firewall-cmd --reload                              # firewalld (Fedora, some Arch spins)
  ```

  The per-session **data plane** rides a *separate, random* UDP port and usually needs **no** firewall
  rule (see [Video is slow to start, or fails across
  subnets](#video-is-slow-to-start-or-fails-across-subnets) for why, and the one case where opening it
  helps). GameStream/Moonlight (only with `--gamestream`) uses TCP **47984/47989/48010** + UDP
  **47998/47999/48000** (video/FEC 47998, ENet control 47999, audio 48000) + mDNS UDP **5353** —
  that's the packages' `punktfunk-gamestream` rule.
- **On a Windows host, check the network profile.** The installer opens the streaming and console
  ports on **Private** and **Domain** networks only. If Windows has classified your LAN as **Public**,
  no client can reach the host — the host logs a warning at startup when it sees this. Set the network
  to Private in **Windows Settings → Network & internet → your network → Network profile type**.
  For a trusted network Windows insists on marking Public, the installer's *Allow connections on
  Public networks* option (unattended: `/MERGETASKS="allowpublicfw"`) opts in — but it only takes
  effect on a **first** install, so on a PC that already has the host, re-scope the streaming ports
  from an elevated prompt instead:

  ```powershell
  punktfunk-host service install --allow-public-network=on
  ```

  That leaves your GameStream choice and the rest of `host.env` alone. It covers the streaming ports;
  the web console's own rule for TCP 47992 keeps the scope it was installed with. See
  [Running as a Service → Windows](/docs/running-as-a-service#windows).

## The Linux host service won't start

`systemctl --user status punktfunk-host` shows it failed instead of running. Two common causes:

- **There's no `host.env` yet.** The packaged unit reads `~/.config/punktfunk/host.env` and won't
  start until that file exists — no package creates it, they only ship a template to copy:

  ```sh
  mkdir -p ~/.config/punktfunk
  # /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu
  cp /usr/share/punktfunk/host.env.example ~/.config/punktfunk/host.env
  systemctl --user restart punktfunk-host
  ```

  On Bazzite copy `host.env.bazzite` instead of `host.env.example`.
- **`status=203/EXEC`** instead means the unit that ran points at a binary that isn't there —
  usually an old hand-copied unit in `~/.config/systemd/user/` shadowing the packaged one and still
  aimed at a source checkout. Remove it, run `systemctl --user daemon-reload`, and start the
  packaged unit — see [Running as a
  Service](/docs/running-as-a-service#a-a-desktop-you-log-into).

## The host is asleep and won't wake

Clients wake a saved host by themselves — auto-wake is on by default — but only once they have seen
it awake, which is how they learn its MAC address, and only if the machine is armed to answer a magic
packet. The arming is what's usually missing, and a **Linux** host tells you outright: search the web
console's **Logs** page for `Wake-on-` — `Wake-on-LAN` for a wired card, `Wake-on-WLAN` for a Wi-Fi
one — and the line either confirms the card is armed or names the interface and the exact command to
arm it. A Wi-Fi card is armed by a different command than a wired one, and the log line gives the
right one. Windows and macOS hosts don't run that check, so go straight to the BIOS/UEFI and
network-card steps in [Arming the machine](/docs/wake-on-lan#arming-the-machine).

## Video is slow to start, or fails across subnets

The native **data plane** (the raw UDP that carries video, separate from the 9777 control plane) uses
a **random, per-session UDP port** — the host binds `0.0.0.0:0`, then tells the client which port it
got during the connect handshake. There is no fixed data port.

Video flows host → client, but the **client sends the first packet**: a small *hole-punch* datagram to
that port. This is deliberate. It lets the host learn the client's real (possibly NAT-translated)
source address and stream back to it, so a session can cross a NAT or a stateful inter-VLAN firewall
**without** a forwarded data port. What it means for a host firewall:

- **Same LAN, no host firewall (or the port allowed):** the punch arrives immediately and video starts
  at once. Nothing to configure.
- **Same LAN, host firewall that denies inbound** (ufw/nftables/firewalld default): the punch is
  dropped, so the host waits **~2.5 s**, then falls back to the address the client reported and streams
  anyway — a stateful firewall admits the return traffic because the host sent first. **Net effect: it
  works, but each session takes ~2.5 s longer to start.** That slow start is the symptom of a
  data-plane rule you're missing.
- **Across subnets / NAT:** the same punch-then-fallback applies, as long as the host's outbound video
  can reach the client (the path's stateful firewall then admits the return). If the host itself is
  behind NAT reached only via a forwarded control port, the data path may not establish — this is the
  case a fixed, forwardable data port would solve.

To remove the ~2.5 s fallback delay, **pin the data port** in [`host.env`](/docs/configuration) and
open exactly that one port. The host then binds that fixed port, skips the punch-wait, and streams
straight to the client — no timeout to pay:

```ini
# ~/.config/punktfunk/host.env (Linux) · %ProgramData%\punktfunk\host.env (Windows)
PUNKTFUNK_DATA_PORT=9778
```

```sh
systemctl --user restart punktfunk-host    # pick the change up (Windows: punktfunk-host service restart)
sudo ufw allow 9778/udp                    # open exactly that one port
```

Running `serve` by hand instead? Pass `--data-port 9778` on that command line — but don't start one
alongside the service, which already holds these ports.

Two caveats. A fixed data port serves **one session at a time**; a second concurrent session finds it
busy and transparently falls back to a random port + hole-punch (logged). And `--data-port` streams
to the client's *reported* address, so use it only where that address is reachable — a flat LAN, or a
port-forward that doesn't remap the client's source. Leave it **off** (the default) to keep the
NAT-crossing hole-punch. On a normal single-LAN setup you can also just leave the data port closed and
accept the one-time ~2.5 s punch-timeout, or not run a host firewall on a trusted LAN at all.

## `nvidia-smi` says it can't communicate with the driver

- The NVIDIA kernel module didn't load. With **Secure Boot** enabled, enrol the module's signing key:
  `sudo mokutil --import /var/lib/shim-signed/mok/MOK.der`, reboot, **Enrol MOK** at the blue screen
  (or disable Secure Boot). On Fedora, follow RPM Fusion's Secure Boot steps.
- After a kernel update the module may need a rebuild — reinstall the driver package.

## The desktop won't start, or "GPU … not supported by EGL"

The NVIDIA **GL/EGL userspace** is missing — the base driver package doesn't always include it.

- **Ubuntu:** `sudo apt install libnvidia-gl-<version>` (matching your driver).
- Confirm `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` exists and `nvidia-drm modeset` is `Y`.

See [GNOME](/docs/gnome) for the GL/EGL userspace details.

## Black screen / no picture, but the client connects

- You must be on a **Wayland** session, not X11 (check the login-screen session picker).
- KWin must be **≥ 6.5.6** (`kwin_wayland --version`) for the *headless* appliance session
  (`kwin_wayland --virtual`); a normal Plasma 6 login needs no particular version, only the screencast
  grant. GNOME **≥ 48**; gamescope **≥ 3.16.22**. See [KDE](/docs/kde) for the KWin/Wayland
  requirement and [gamescope](/docs/gamescope) for the gamescope one.
- If [`host.env`](/docs/configuration) sets `PUNKTFUNK_COMPOSITOR`, **remove it** — the host
  auto-detects the live compositor, and the pin points it at one backend even when a different
  session is live (it also disables Gaming ↔ Desktop following).

## The screen stays black after switching to Game Mode (Nobara)

On distros whose Game Mode is display-manager autologin under **plasmalogin** (Nobara), a managed
takeover from a host **0.19.1 or older** could kill the display manager: it trips systemd's start
limit and the box stays black until someone restarts it. Recover from a VT (Ctrl+Alt+F3) or SSH:

```sh
systemctl --user unmask --runtime 'gamescope-session-plus@*.service'
sudo systemctl reset-failed plasmalogin && sudo systemctl restart plasmalogin
```

Current hosts detect the display-manager flavor and never mask the session unit there — see
[gamescope → autologin display managers](/docs/gamescope) for the polkit rule that enables the full
managed takeover on these boxes (without it the host mirrors Game Mode instead).

## Game Mode: black screen on connect, or the stream is stuck at the box's resolution

You connect to a box that autologins into Steam **Gaming Mode** and get a black picture every time
— or a picture at the box's own resolution instead of the one your client asked for, with the box's
monitor still lit. Nothing errors: the client connects, the host logs no failure, no unit is failed.

The managed takeover is being refused and the host is falling back to mirroring the box's own
session. On a box whose panel is off (a headless appliance, a TV that's been switched away) there
is nothing to mirror, so the fallback is a black screen. Almost always the cause is **group
membership**: the takeover stops the display manager through a root helper, and that helper serves
members of the `punktfunk` group only.

```sh
id -nG | tr ' ' '\n' | grep -x punktfunk      # are you in it?
journalctl --user -u punktfunk-host | grep -iE "punktfunk. group|takeover unavailable"
```

The host also checks at startup on any box that will need the takeover, so a fresh
`systemctl --user restart punktfunk-host` puts the answer at the top of the log. The fix is one
command and a fresh login:

```sh
sudo usermod -aG punktfunk "$USER"   # then log out and back in
```

> **Read the reason the log quotes before doing anything else.** The takeover has three other ways
> to be refused — no packaged helper (a tarball or source install), no polkit on the box, and
> polkit denying the action — and the host now prints which one it hit, verbatim from the
> privileged path. Hosts up to 0.27.0 printed a fixed guess instead ("reinstall the punktfunk
> package, or install the display-manager polkit rule from the docs"), and on the group case both
> of those suggestions were dead ends: neither adds anyone to a group.

Two things this is *not*: it isn't the [pad group problem](#the-pad-works-but-arrives-as-an-xbox-360-controller-instead-of-a-steam-deck)
(same group, different symptom), and it isn't lingering — though a host with no login session of
its own enables lingering through the same helper, so an unjoined user often sees "enabling
lingering failed" first. Both are covered in
[gamescope → autologin display managers](/docs/gamescope#nobara-and-other-autologin-display-managers).

## Session fails right after editing host.env

- Keys are **case-sensitive**: `punktfunk_gamescope_attach=1` sets nothing — use the exact
  uppercase names.
- Hardcoded session anchors with the wrong uid (`XDG_RUNTIME_DIR=/run/user/1000` when `id -u`
  isn't 1000) point the host at another user's PipeWire/D-Bus: audio errors like
  `pw audio connect … Creation failed`, no capture, and clients reporting the host as
  unreachable or asleep. **Delete both anchor lines** — a `systemctl --user` service doesn't need
  them — or fix the uid.
- `PUNKTFUNK_COMPOSITOR` pins the backend and disables Gaming ↔ Desktop following — remove it on
  any box that switches sessions.
- The env file is read at service start: `systemctl --user restart punktfunk-host` after edits.

## Capture fails: "Session creation inhibited" (GNOME)

A **locked** GNOME session blocks screen capture. On an always-on/headless host, disable the lock:

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

See [GNOME → Headless session](/docs/gnome#headless-session) and
[Running as a Service](/docs/running-as-a-service).

## My mouse and keyboard are stuck in the stream

Nothing is broken — the stream captures them on purpose, from the moment it starts and again
whenever you click into it, so your keys and pointer go to the host instead of your own desktop.
**Ctrl+Alt+Shift+Q** hands them back (**⌃⌥⇧Q** on macOS), and with a pad in your hands
**L1+R1+Start+Select** does the same on the Linux, Windows and Steam Deck clients. Whatever you
were holding down is released on the host, so nothing sticks. The rest of the in-stream chords —
switch mouse mode, disconnect, fullscreen — are in
[Getting your input back](/docs/input#getting-your-input-back).

## A controller is detected but games don't see it

- **Linux.** The host user needs to be in the `input` group. On Bazzite:

  ```sh
  ujust add-user-to-input-group
  ```

  Then log out and back in. On other distros this is `sudo usermod -aG input $USER` + re-login. See
  [Bazzite](/docs/bazzite).
- **Windows, if this PC ever ran 0.22.0 or 0.22.1.** On those two releases the default emulated
  controller bound one of Windows' own drivers instead of Punktfunk's, so the app responded to your
  controller normally but no game ever saw it. It's fixed from 0.22.2 on — but you have to update
  **through the installer**: the setup `.exe`, `winget upgrade`, or the console's **Update now**
  button (see [Updating](/docs/updating)). Swapping `punktfunk-host.exe` by hand does not fix it,
  because the stale controller device keeps the driver it was already bound to.

## The pad works, but arrives as an Xbox 360 controller instead of a Steam Deck

Only the **virtual Steam Deck controller** (paddles, trackpads, gyro) is missing here — ordinary
gamepad input is fine. That pad reaches games as a real USB device over usbip, and the sysfs files
it attaches through are owned by a group called `punktfunk`, separate from `input`. Four things
have to line up on the Linux host, and none of them announces itself when it doesn't:

```sh
getent group punktfunk                         # the group exists at all
id -nG | tr ' ' '\n' | grep -x punktfunk       # ...and you are in it
ls -l /sys/devices/platform/vhci_hcd.0/attach  # owned by punktfunk, mode 0660
lsmod | grep vhci_hcd                          # the transport module is loaded
```

If the group is missing entirely, the udev rule tried to `chgrp` to a group nobody created, so the
nodes stayed root-only. That was the case on installs that reached 0.25.0 by **upgrade** on Arch,
on NixOS, on the Bazzite sysext, and on Steam Deck source installs. Re-running your package
manager's upgrade (or `update.sh` on a Deck) creates it now; otherwise `sudo groupadd --system
punktfunk` by hand. Then `sudo usermod -aG punktfunk "$USER"` and **log out and back in** — group
changes only reach the host's `systemd --user` service on a fresh login, and on a Deck a reboot is
the reliable way to get one.

Joining the group is optional, and there is a real reason it is not automatic: writing that
`attach` file materialises an arbitrary emulated USB device. Skip it on a machine you share.

It is not only the pad, though: the same group authorizes the helper that stops the display manager
for a managed **Gaming Mode** takeover, so on a box that autologins into Game Mode, skipping it also
costs you [the takeover](#game-mode-black-screen-on-connect-or-the-stream-is-stuck-at-the-boxs-resolution).

## Copy and paste between host and client does nothing

The shared clipboard needs **two** separate switches on, and turning on only one looks exactly like
the feature not existing: the host operator has to allow it with `PUNKTFUNK_CLIPBOARD` in `host.env`
and restart the host, and you have to turn it on for that one host in your client's **Edit…** sheet.
Work through
[Why the toggle does nothing](/docs/clipboard#why-the-toggle-does-nothing-or-is-greyed-out) — it also
names the clients and host sessions where nothing crosses no matter what you set.

## A plugin's interface doesn't load

The plugin's page in the console opens — title, version, **Open in new tab** — but the panel below
it stays empty.

Plugin interfaces are served on **TCP 47993**, a separate port from the console's 47992, so that a
plugin can't act as you with your logged-in session (see
[Two ports, not one](/docs/web-console#two-ports-not-one)). An empty panel means the browser can't
load anything from that second port. Two reasons, in order of likelihood:

- **The port isn't open.** Only the console's port is reachable, so the frame has nothing to show.
  On a host you *upgraded*, this is the usual answer: an already-open firewall does not pick up a
  port that a later version added, because the rule it saved lists the ports it knew at the time.

  ```sh
  # ufw (CachyOS, Ubuntu): re-expand the profile, then reload
  sudo ufw app update punktfunk-web && sudo ufw reload

  # firewalld (Fedora, Bazzite, Nobara): re-read the shipped service definition
  sudo firewall-cmd --reload
  ```

  ```powershell
  # Windows: re-run the service installer, which re-adds both console rules
  punktfunk-host service install
  ```

  Check what's actually open with `sudo ufw status verbose` or
  `sudo firewall-cmd --info-service=punktfunk-web` — you want **47993** listed next to 47992.
- **The certificate isn't trusted for that port yet.** Browsers keep a self-signed certificate
  exception *per port*, and a warning page can't be shown inside a panel. The console detects this
  and offers a link to open the plugin in its own tab: accept the warning there once and come back.

If the panel is empty and the console shows *no* explanation at all, the plugin's own port is
probably being dropped rather than refused — open 47993 as above.

## Pairing is rejected / the client can't connect

- The host **requires pairing** by default. Arm pairing from the web console, then enter the PIN on
  the client. See [Pairing & Trust](/docs/pairing).
- If you re-installed the host, its identity changed — re-pair the client.

## The picture freezes for a moment, over and over (Windows)

A freeze that comes back on a **rhythm** — every few seconds, every minute, always the same gap — is
not a bandwidth problem, and lowering the bitrate won't touch it. The Windows capture path detects
that pattern itself and writes the cause and the cure into the log.

Open the web console's **Logs** page and search for `METRONOMIC`. You'll get one of two lines:

- **…and coincide with Windows monitor hot-plug/re-enumeration events** — a display (or its
  cable, switch or AVR) is re-probing its link on a timer and Windows reacts every time. Cures,
  best first: turn that display's **auto input scan/detect** off in its own OSD (on TVs also
  *instant-on / quick-start* and CEC), unplug its cable at the GPU, fit an HPD-holding adapter or
  dummy plug, or simply keep the display active while you stream. The console's **Virtual displays**
  page also has a *Disable monitor devices while streaming (PnP)* toggle that suppresses the
  Windows-side reaction; the log line's `connected_inactive` field names the displays it suspects.
- **…with NO coinciding OS display event** — the disturbance is below Windows: a connected but
  sleeping screen being serviced by the GPU driver, display-poller software (the SteelSeries GG /
  SignalRGB class), or the desktop present clock — try a different refresh rate. On a laptop panel
  that the host deactivated, keeping it active with the **primary** topology usually settles it —
  see [Virtual displays → Topology](/docs/virtual-displays#topology).

## Stutter, drops, or high latency

- Lower the **bitrate**. On a busy or Wi-Fi link, the requested bitrate may be too high — the native
  clients' [speed test](/docs/configuration#bitrate) picks a safe value; with Moonlight, set it
  manually.
- Prefer a **wired** connection or 5 GHz Wi-Fi between host and client.
- Streaming to **many devices at once** shares the GPU encoder. The host serves several
  concurrent native sessions (up to 4 by default); heavy load is usually bitrate-bound, so
  lower the bitrate first.

If the stream is *wrong* rather than late — a codec you didn't pick, 8-bit where you expected HDR,
4:2:0 where you asked for full chroma — the answer is usually that the host declined the request and
told your client so. [When the client and the host
disagree](/docs/client-settings#when-the-client-and-the-host-disagree) lists what it does with each
one.

## Streamed audio sounds worse than the host does

The host does not capture "the sound card" — it captures a **render endpoint**, and by default it
picks one that is *silent on the host* so the audio plays on your client only. On a PC with Steam
installed that silent endpoint is Steam's **Streaming Microphone**, which exists to carry remote
*voice*. If Windows has it configured as a narrow device — mono, or below 48 kHz — then the whole
desktop mix is squeezed through that before it is ever encoded, and no amount of bitrate will bring
it back.

Since 0.25 the host checks for this: it reads each candidate endpoint's real format, prefers a real
output device over a narrow virtual one, and says so in the log —

```
WARN  the desktop-audio loopback endpoint mixes at 24000 Hz, so the stream is band-limited …
INFO  audio loopback capturing device="…" engine_hz=48000 engine_ch=2 engine_bits=32
```

That `engine_*` line is the endpoint's **own** format, so it tells you directly whether the source
was ever full quality. To choose the routing yourself, set in `host.env`:

```ini
# client_only     — default; audio plays on the client only (a silent endpoint)
# host_and_client — capture a real output device; audio plays on BOTH ends
# follow_default  — capture whatever YOUR default playback device is, and never change it
PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client
```

`host_and_client` is also the quickest way to A/B the problem: if the stream sounds right that way
and wrong on the default, the endpoint was the cause.

Two related knobs:

```ini
PUNKTFUNK_AUDIO_QUALITY=high    # low | standard | high (default high — stereo 256 kbps)
PUNKTFUNK_AUDIO_REDUNDANCY=1    # force the loss-resilient audio plane on (default: automatic)
```

Both are a **request**, not a guarantee: the host budgets audio against the session's video
bitrate and steps it down on a narrow link, because audio is not managed by adaptive bitrate — so
whatever it takes is taken off the top. On a roomy link you get 256 kbps plus loss redundancy; as
the link narrows the host drops redundancy first, then the tier, and never goes below ~96 kbps. The
session log line says what it settled on:

```
INFO  punktfunk/1 audio streaming … tier=high kbps=512 redundancy=true
```

`standard` reproduces the pre-0.25 encoder exactly if you want to A/B it.

## Audio lags behind the picture

The client buffers a little audio to absorb network jitter. Since 0.25 that buffer **corrects
itself**: if it drifts deeper — a Wi-Fi burst, a stall, or just the two devices' clocks running at
fractionally different speeds — it trims itself back a few milliseconds at a time, inaudibly.
Before, it could only grow, so a single hiccup left audio permanently behind the video and the only
cure was reconnecting.

If audio is still noticeably late:

- **Reconnect once.** It confirms whether the delay was accumulated (gone after a reconnect) or
  constant (something else).
- **Check for underruns** rather than guessing. The client logs its buffer depth periodically; a
  rising `underruns` count means the buffer is being starved, which is a network or CPU problem, not
  a buffering one.
- **Wired or 5 GHz Wi-Fi.** Arrival jitter is what the buffer exists to absorb; less jitter lets it
  run shallower.

## Windows: the host or the web console won't start

The **`PunktfunkHost` service** runs both halves of the Windows host: the streaming host itself and
the web console. It restarts either one automatically if it stops, so most console outages heal
themselves within a minute. The service commands need an **elevated** PowerShell or Command Prompt.

1. **Is the service running?**

   ```powershell
   punktfunk-host service status
   punktfunk-host service restart
   ```

   `restart` stops it, waits for it to actually reach *Stopped*, and starts it again.
2. **Two `punktfunk-host.exe` processes in Task Manager is normal — don't kill one.** The service
   itself runs as SYSTEM in session 0, where it can neither capture the screen nor inject input, so
   it launches a second copy into the interactive session and supervises it. One supervises, one
   streams.
3. **The console page never loads.** The service restarts the console on any failure, so give it a
   minute first. If it stays down, the console's own log says why — check
   `%ProgramData%\punktfunk\logs\web.log` (and `service.log` next to it, which records every console
   start and exit), then restart the service:

   ```powershell
   punktfunk-host service restart
   ```

   Right after a very first install the console can lag the host by a few seconds on purpose: it
   waits for the host to finish writing its certificate before serving.
4. **The status icon is missing after an update.** Windows only launches the tray at sign-in, and an
   upgrade closes the running ones. Put it back without signing out — from your **normal** (not
   elevated) shell, so it runs as you:

   ```powershell
   punktfunk-host tray start
   ```

   `punktfunk-host tray status` says whether one is running and where it is installed. See
   [Windows Host → Status tray](/docs/windows-host).

## Windows: "Punktfunk Virtual Display" shows Code 10 in Device Manager

Sessions end with *"pf-vdisplay driver interface not found"* and Device Manager shows the
**Punktfunk Virtual Display** device failed with **Code 10** (`STATUS_DEVICE_POWER_FAILURE`).
(Installs older than 0.22.2 spell that device name in lower case.)

This means your Windows version is too old. The virtual-display driver requires the **IddCx 1.10**
driver framework, which first shipped in **Windows 11 22H2 (build 22621)** — on Windows 10
(including LTSC) and Windows 11 21H2 the driver installs but cannot start. Reinstalling won't help;
the fix is updating to Windows 11 22H2 or newer. (Current installers refuse to run on older
Windows for this reason; if you see this, the host was likely installed with an older installer.)

## Still stuck?

Read the host's log around the failed connect or capture.

1. Open the web console's **Logs** page. It always holds the host's recent output at *debug* detail,
   whatever the log level is set to — there's nothing to switch on and no restart needed.
2. Filter it down to the level or the text you're after. The **Host / Plugins** switch beside the
   level buttons picks the producer: your [plugins](/docs/plugins) log to the same page, tagged
   `plugin:<name>`, so a misbehaving plugin is one click away rather than a separate hunt through
   the journal.
3. Use **Download logs** to save exactly what you're filtering on as a timestamped `.log` file you
   can attach to a bug report. The button beside it hands the same text to your phone or tablet's
   share sheet, or copies it to the clipboard on a desktop.

The same output also lands outside the console — on Linux in the journal
(`journalctl --user -u punktfunk-host`), on Windows in `%ProgramData%\punktfunk\logs\host.log` (plus
`service.log` for the service that supervises it). Those *do* follow the log level: raise it with
`RUST_LOG=debug` in [`host.env`](/docs/configuration) and restart the host. `RUST_LOG=info` is
already the default, so setting it changes nothing.

None of that covers the **client** side. If the picture, the decoder or the presenter is what
failed, the Windows client keeps its own log at `%LOCALAPPDATA%\punktfunk\logs\client.log` (rotated
to `.old` at the next start once it passes 10 MB, one generation kept) — that's the only place a
receive, decode or present failure is recorded.

For a performance problem rather than a failure, attach a **recording** instead of a log: see
[Recording a capture for a bug report](/docs/stats#recording-a-capture-for-a-bug-report).
