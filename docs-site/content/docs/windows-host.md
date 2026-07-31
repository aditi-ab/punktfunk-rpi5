---
title: "Windows Host"
description: "Run the Punktfunk streaming host on a Windows PC — a first-class, all-vendor, virtual-display host."
---

Set up a Punktfunk host on a **Windows 11 PC (22H2 or newer)** and stream its desktop or games to any
Punktfunk client — or to [Moonlight](/docs/moonlight), once you turn on GameStream compatibility
(see [Configure](#configure)). A signed installer registers a Windows service that streams at the
client's **exact resolution and refresh** via Punktfunk's own **virtual display** — including
**HDR10** (10-bit BT.2020 PQ), which the host turns on for that display itself when a session
negotiates it, whatever your desktop's own HDR mode is. The virtual display is created on the fly, so
you need **no second monitor and no dummy HDMI plug**, and capture keeps working even on the secure
desktop (UAC prompts, the lock screen).

> New to this? Skim [Requirements](/docs/requirements) first.

> **Read [Security & Safe Use](/docs/security) before you set this up.** The Windows host runs as a
> `LocalSystem` service (so it can capture the secure desktop and stream headless), which makes it a
> high-privilege component — keep it on a trusted network, never expose it to the internet, and prefer
> a dedicated or gaming PC over a machine that holds your most sensitive data.

> This page is about the Windows **host** — streaming *from* a Windows PC. To stream *to* a Windows PC,
> see the [Windows client](/docs/clients#windows-desktop-client).

## Requirements

- **Windows 11 22H2 (build 22621) or newer, x64.** Windows 10 — including LTSC — and Windows 11
  21H2 are **not supported**: the virtual-display driver needs the IddCx 1.10 driver framework,
  which first shipped in Windows 11 22H2. On older Windows the driver installs but can't start
  ("Punktfunk Virtual Display" shows **Code 10** in Device Manager and streaming fails); the
  installer therefore refuses to run there. ARM64 is not built either (no ARM64 NVIDIA driver, and
  the virtual-display driver is x64-only).
- **A GPU for hardware encode** — the host auto-detects the vendor:
  - **NVIDIA** → NVENC
  - **AMD** → AMF
  - **Intel** → QSV

  No discrete GPU? The host falls back to a **software H.264** encoder (higher CPU use, lower quality —
  fine for light desktop use).
- **No gamepad prerequisite.** The virtual gamepad drivers are bundled in the installer — there is
  nothing else to download. (Earlier builds needed ViGEmBus; it is no longer used.)

## Install

Download the signed `punktfunk-host-setup-<ver>.exe` from the
[latest release](https://git.unom.io/unom/punktfunk/releases) and run it. The installer:

- drops the host into `C:\Program Files\punktfunk` and registers + starts the **`PunktfunkHost`**
  service,
- installs the bundled **virtual-display driver** (`pf-vdisplay`) so the host can create per-client
  displays,
- installs the bundled **virtual gamepad drivers** (DualSense, DualShock 4, Xbox 360),
- registers the bundled **HDR Vulkan layer** so Vulkan games can enable HDR over the virtual display,
- installs **VB-CABLE** (VB-Audio, donationware) as the virtual microphone for client mic
  passthrough — a checkbox in the installer, **ticked by default**; clear it, or pass
  `/MERGETASKS="!installaudiocable"`, if you don't want it,
- adds a **status icon** to the notification area (see [Status tray](#status-tray)),
- sets up the **web management console** (see below).

Prefer the CLI, or want the full service/firewall details? See
[Running as a Service → Windows](/docs/running-as-a-service#windows).
Packaging internals live in
[`packaging/windows`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/README.md).

### Install with winget

Punktfunk ships the same signed installer through its own winget source. Register the source once
per machine, from an **elevated** terminal:

```powershell
winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest
winget install unom.PunktfunkHost
```

Before it downloads anything, winget shows the package's agreements — the bundled VB-CABLE notice,
and that Moonlight compatibility is off by default — and asks you to accept them.

`winget install` runs setup silently with the same defaults the wizard shows, so the console
password is generated for you (see [Unattended install](#unattended-install)). Add `--interactive`
for the full wizard instead (the task checkboxes, the console-password page, the VB-CABLE notice).
To change an individual installer task on the silent path, pass the whole switch line through
`--override` — not `--custom`, which *appends* and would leave two `/MERGETASKS` on one command
line:

```powershell
winget install unom.PunktfunkHost --override "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP- /MERGETASKS=gamestream"
```

The source carries every **stable** release, so `--version <x.y.z>` installs a specific one instead
of the newest; canary builds are not published there — use the setup `.exe` for those. Later updates
are `winget upgrade unom.PunktfunkHost` and removal is `winget uninstall unom.PunktfunkHost`.

### About the signatures

Punktfunk signs with its own certificates rather than a publicly trusted one, so Windows warns about
an unknown publisher before setup runs. Accepting the prompt is enough. To silence it for good, the
matching **`punktfunk-host-windows_<ver>.cer`** is published next to the installer and it is the
same certificate for every release — the one-time import is in
[Install → Windows](/docs/install#windows). This applies to the winget path too: winget downloads
and runs that same installer.

The bundled drivers carry a **second, separate** self-signed certificate (`CN=punktfunk-driver`,
SHA-1 thumbprint `4B8493E7CD565758D335F8F4F05C5A7261A13E02`). The installer adds it to the machine's
**Trusted Root** and **Trusted Publishers** stores so Windows will load the drivers; uninstalling
removes every copy of it again.

### Unattended install

```powershell
punktfunk-host-setup-<ver>.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-
```

Keep `/SUPPRESSMSGBOXES`: without it an unattended run can stop on a message box nobody can see.

A silent run takes the same defaults as the wizard, so add or drop individual options with Inno's
`/MERGETASKS` — a bare name adds a task, a `!` prefix removes one. `/MERGETASKS="gamestream"` turns
on Moonlight compatibility; `/MERGETASKS="!trayicon"` skips the status icon. The task names are
`installdriver`, `installgamepad`, `installaudiocable`, `installhdrlayer`, `gamestream`,
`allowpublicfw`, `startservice` and `trayicon`.

Two things to know before you script it:

- Setup **aborts with a non-zero exit code** if another Moonlight-compatible host — Sunshine, Apollo,
  Vibeshine, Vibepollo or LuminalShine — is installed *and* its service is set to start on its own.
  Stop and disable that service first. An installed but disabled host is not flagged.
- There is no wizard, so the console password is generated for you. Read it afterwards from
  `%ProgramData%\punktfunk\web-password`.

### Web console & pairing

See [The Web Console](/docs/web-console) for the console + pairing model shared with the Linux host;
the Windows specifics follow.

The installer also sets up the **web management console** (status, paired devices, the PIN pairing
flow): it bundles the console plus its own runtime, and the **`PunktfunkHost`** service runs it on
**`https://<this-PC>:47992`** — started with the service and restarted automatically if it stops.

#### Console login password

You choose the console **login password** during setup — a secure random default is pre-filled and
shown on the installer's final page. It's stored in `%ProgramData%\punktfunk\web-password`, readable
only by Administrators and SYSTEM. To read or change it (with the service restart), see
[The Web Console → Login password](/docs/web-console#login-password); forgot it entirely?
[Forgot your Password?](/docs/forgot-password).

The host **requires PIN pairing** by default (secure on a LAN). To connect the first time, open the
console from any browser on the LAN, log in, open **Pairing** in the sidebar and click **Pair a
device** ([arm pairing](/docs/web-console#arm-pairing)), and enter the PIN on your
[client](/docs/clients). The host's own management API keeps every admin action loopback-only;
off-loopback it serves only read-only status and game-library browsing to paired clients.

### Status tray

The installer also adds a **status icon** to the notification area — the installer task *Show the
Punktfunk status icon in the notification area at sign-in*, ticked by default (skip it with
`/MERGETASKS="!trayicon"`). It shows at a glance whether the host is stopped, starting, running,
degraded or failed, and its menu opens the web console and the logs folder and starts, stops or
restarts the service without a terminal — each of those service actions raises its own UAC prompt.
Every user who signs in to this PC gets one.

The installer starts the icon straight away when you run the wizard; a **silent** install (winget,
`/VERYSILENT`) does not, so there it first appears at the next sign-in. Any upgrade closes the
running trays to replace the executable — the wizard puts the icon back, and an update you start
from the web console relaunches it itself, but after a silent upgrade it stays away until you sign
in again. If it is missing while the host is otherwise fine, start one without signing out:

```powershell
punktfunk-host tray start
```

`punktfunk-host tray status` reports whether one is running and where the executable is;
`punktfunk-host tray stop` closes it.

### Configure

The service reads `%ProgramData%\punktfunk\host.env`. The defaults work out of the box; common knobs:

- `PUNKTFUNK_ENCODER=auto` — `auto` picks NVENC/AMF/QSV by GPU vendor. Force one with `nvenc`, `amf`,
  `qsv`, or `sw` (software). On a multi-GPU box the [web console's GPU preference](/docs/web-console)
  wins: a forced backend whose vendor doesn't match the selected GPU is ignored (the host logs a
  warning) — remove the stale line rather than fighting it.
- `PUNKTFUNK_HOST_CMD` — what the service runs. A fresh install from the setup `.exe` writes `serve`:
  the **secure native-only** host, Punktfunk clients only. **Moonlight/GameStream compatibility is
  opt-in** — tick *Enable GameStream (Moonlight) compatibility* in the installer (unattended:
  `/MERGETASKS=gamestream`), or turn it on afterwards from an elevated prompt:

  ```powershell
  punktfunk-host service install --gamestream=on
  punktfunk-host service restart
  ```

  That writes `PUNKTFUNK_HOST_CMD=serve --gamestream` (native punktfunk/1 **plus** the
  GameStream/Moonlight-compat planes); `--gamestream=off` puts it back to `serve`. GameStream pairs
  over plain HTTP and uses weaker legacy encryption — trusted LAN only. An upgrade never changes your
  choice, and a hand-edited `PUNKTFUNK_HOST_CMD` is left alone.

Edit the file, then `punktfunk-host service restart` (it stops, waits for the service to actually
reach *Stopped*, and starts again); `punktfunk-host service status` shows the current state. See the
[Configuration reference](/docs/configuration) for every option.

### Logs

The host writes to `%ProgramData%\punktfunk\logs\` — `service.log` for the service supervisor and
`host.log` for the streaming host itself. Each is rotated to `.old` at the next start once it passes
10 MB, one generation kept. The web console's **Logs** tab shows the same stream live. For more
detail, set `RUST_LOG=debug` in `host.env` and restart the service.

### Updating

The web console's Host page shows an **Updates** card with an **Update now** button —
see [Updating the Host](/docs/updating). You can also run `winget upgrade unom.PunktfunkHost`, or
simply run the newer `punktfunk-host-setup-<ver>.exe` over the old install. All three upgrade in
place and keep your config, pairings and console password. If an update you started from the console
leaves the host crash-looping, the service rolls itself back to the cached previous installer and
records why in the Updates card.

### Uninstalling

Open **Settings → Apps → Installed apps → Punktfunk Host → Uninstall**, or run
`winget uninstall unom.PunktfunkHost`. Either way the uninstaller:

- closes the status icon,
- stops and removes the **`PunktfunkHost`** service and its firewall rules,
- removes the `pf-vdisplay` and virtual-gamepad drivers — the device nodes *and* their driver-store
  packages — together with the `CN=punktfunk-driver` certificate it had added,
- removes the **`PunktfunkScripting`** scheduled task (and the legacy **`PunktfunkWeb`** task older
  versions used for the console) and the console firewall rule,
- unregisters the HDR Vulkan layer,
- takes `C:\Program Files\punktfunk` back off the machine `PATH`.

Three things are left behind on purpose: **`%ProgramData%\punktfunk`** (`host.env`, the host
certificate and key, the management token, the console password, your paired devices and the logs —
keeping it is what makes a reinstall pick up where you left off), **VB-CABLE** unless you cleared its
checkbox, and **the publisher certificate** if you imported one by hand.
[Uninstalling → Windows host](/docs/uninstall#windows-host) shows how to clear each one, and has the
same walkthrough for the other platforms.

## How it works

The host installs a **`LocalSystem` SCM service** that runs from Session 0 and launches a worker into
the interactive session (`CreateProcessAsUserW`). That lets it **capture the secure desktop** (UAC
prompts, the lock screen) and keep streaming across reboots with nobody logged in — the same model
Sunshine and Apollo use. Service registration, firewall rules, and the supervisor all live in
`punktfunk-host service install`; the installer just lays the exe down and calls it elevated.

So a healthy Windows host shows **two** `punktfunk-host.exe` processes in Task Manager — the
Session 0 supervisor and the worker it launched into your session. That is normal. Ending the worker
only makes the supervisor start a new one; stop the host with `punktfunk-host service stop` or from
the status icon's menu.

Running as SYSTEM is what makes headless, log-in-optional streaming work — and it's why the host is a
high-privilege component worth being deliberate about. Punktfunk mitigates this with **user-mode
drivers** — the virtual display, the virtual gamepads and the virtual pointer are all UMDF, none of
ours is kernel-mode (the optional third-party VB-CABLE mic driver is the one exception) — **sealed
internal channels** between the host and its drivers, and Administrators/SYSTEM-only permissions on
its secrets. See
[Security & Safe Use](/docs/security) for the full picture, including why we recommend not hosting on
your most sensitive machine.

### One core, Windows backends

Most of Punktfunk is platform-agnostic. `punktfunk-core` (protocol, FEC, crypto, session, transport,
the C ABI), the QUIC control plane, the GameStream wire logic, the management API, and the per-frame
pipeline orchestration are all shared with the Linux host. The Windows host is a set of
`#[cfg(windows)]` backends behind the same traits the Linux host uses:

| Subsystem | Linux backend | Windows backend |
|---|---|---|
| **Capture** | xdg ScreenCast portal → PipeWire (dmabuf) | **IDD direct-push** — the `pf-vdisplay` driver copies finished frames into a host-owned shared GPU texture ring that the host consumes in-process (no Desktop Duplication, no Windows.Graphics.Capture); FP16/10-bit when the session negotiated HDR |
| **Virtual display** | KWin / Mutter / Sway / gamescope | **pf-vdisplay** signed IDD — create a `WxH@Hz` monitor per session, capture it, tear it down |
| **Encode** | NVENC (CUDA) / VAAPI (AMD·Intel) / Vulkan Video / software, plus [PyroWave](/docs/pyrowave) | **NVENC** (NVIDIA) · **AMF** (AMD) · **QSV** (Intel) · software H.264 — H.264, HEVC (Main10 / BT.2020 PQ for HDR) and AV1 where the GPU supports it, plus [PyroWave](/docs/pyrowave) |
| **Input — mouse/keyboard** | libei / wlr protocols | **SendInput** (Win32 VK + absolute mouse) |
| **Input — gamepads** | uinput Xbox 360 + UHID DualSense/DS4 | **UMDF** virtual pads — DualSense, DualShock 4, Xbox 360 (XUSB) + rumble |
| **Audio capture** | PipeWire sink-monitor | **WASAPI loopback** |
| **Virtual mic** | PipeWire `Audio/Source` | **VB-CABLE** virtual device (optional), captured via WASAPI |

The virtual display is **pf-vdisplay**, Punktfunk's own all-Rust **Indirect Display Driver (IDD)**. The
host creates a shared GPU texture ring and the driver pushes finished frames straight into it — a real
virtual display at the client's exact `WxH@Hz`, with no physical monitor and no dummy plug, captured
in-process from Session 0 so the secure desktop streams too. There is **no** Desktop Duplication or
Windows.Graphics.Capture path: IDD direct-push is the only capture path. The signed driver is bundled
and staged by the installer and is **required** — without it the host can't create a session (there is
no monitor-capture fallback).

### HDR

A Windows host is the easiest place to get HDR: for a session that negotiated 10-bit it turns
advanced colour on for the virtual display it just created, so you never touch "Use HDR" in Windows
Settings. Vulkan games additionally need the bundled layer this installer registers.
[HDR → Windows](/docs/hdr#windows) has the whole chain, the layer's switches, the one case where the
label can outrun the picture, and what to check when a stream comes out SDR.

## Notes & limits

- **AMD / Intel encode is newer.** The NVENC path is the most exercised; AMF (AMD) and QSV (Intel) are
  built and tested in CI but less battle-tested on real hardware. Software H.264 is the GPU-less
  fallback.
- **x64-only.** No ARM64 build — no ARM64 NVIDIA driver, and the virtual-display driver is x64-only.
- **Newer than the Linux host.** The Linux host is the most battle-tested path; the Windows host is
  more recent, with the virtual-mic and AMD/Intel encode backends the youngest pieces.

Trouble? See [Troubleshooting](/docs/troubleshooting) and [Pairing](/docs/pairing).
