---
title: "Windows Host"
description: "Run the Punktfunk streaming host on a Windows PC — a first-class, all-vendor, virtual-display host."
---

Set up a Punktfunk host on a **Windows 11 PC (22H2 or newer)** and stream its desktop or games to any Punktfunk or
[Moonlight](/docs/moonlight) client. A signed installer registers a Windows service that streams at the
client's **exact resolution and refresh** via Punktfunk's own **virtual display** — including
**HDR10** (10-bit BT.2020 PQ) when your Windows desktop is in HDR mode. The virtual display is created
on the fly, so you need **no second monitor and no dummy HDMI plug**, and capture keeps working even on
the secure desktop (UAC prompts, the lock screen).

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
  ("punktfunk Virtual Display" shows **Code 10** in Device Manager and streaming fails); the
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
- optionally installs **VB-CABLE** (VB-Audio, donationware) as the virtual microphone for client
  mic passthrough — a checkbox in the installer,
- sets up the **web management console** (see below).

For an unattended install, append `/VERYSILENT`. Upgrades and uninstall go through **Add/Remove
Programs**; your config and pairings are kept across upgrades. Prefer the CLI, or want the full
service/firewall details? See [Running as a Service → Windows](/docs/running-as-a-service#windows).
Packaging internals live in
[`packaging/windows`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/README.md).

### Web console & pairing

See [The Web Console](/docs/web-console) for the console + pairing model shared with the Linux host;
the Windows specifics follow.

The installer also sets up the **web management console** (status, paired devices, the PIN pairing
flow): it bundles the console plus its own runtime and runs it as the **`PunktfunkWeb`** task on
**`https://<this-PC>:47992`**, starting at boot and at sign-in.

#### Console login password

You choose the console **login password** during setup — a secure random default is pre-filled and
shown on the installer's final page. It's stored in `%ProgramData%\punktfunk\web-password`, readable
only by Administrators and SYSTEM. To read or change it (with the `schtasks` restart), see
[The Web Console → Login password](/docs/web-console#login-password); forgot it entirely?
[Forgot your Password?](/docs/forgot-password).

The host **requires PIN pairing** by default (secure on a LAN). To connect the first time, open the
console from any browser on the LAN, log in, go to **Devices → [arm pairing](/docs/web-console#arm-pairing)**,
and enter the PIN on your [client](/docs/clients). The host's own management API keeps every admin action loopback-only; off-loopback it serves only read-only status and game-library browsing to paired clients.

### Configure

The service reads `%ProgramData%\punktfunk\host.env`. The defaults work out of the box; common knobs:

- `PUNKTFUNK_ENCODER=auto` — `auto` picks NVENC/AMF/QSV by GPU vendor. Force one with `nvenc`, `amf`,
  `qsv`, or `sw` (software).
- `PUNKTFUNK_HOST_CMD` — the service runs `serve --gamestream` by default (native punktfunk/1 **plus**
  the GameStream/Moonlight-compat planes). Set it to `serve` for a **secure native-only** host with no
  GameStream surface (GameStream pairs over plain HTTP and uses weaker legacy encryption — trusted LAN
  only).

Edit the file, then restart: `punktfunk-host service stop` / `punktfunk-host service start`. See the
[Configuration reference](/docs/configuration) for every option.

## How it works

The host installs a **`LocalSystem` SCM service** that runs from Session 0 and launches a worker into
the interactive session (`CreateProcessAsUserW`). That lets it **capture the secure desktop** (UAC
prompts, the lock screen) and keep streaming across reboots with nobody logged in — the same model
Sunshine and Apollo use. Service registration, firewall rules, and the supervisor all live in
`punktfunk-host service install`; the installer just lays the exe down and calls it elevated.

Running as SYSTEM is what makes headless, log-in-optional streaming work — and it's why the host is a
high-privilege component worth being deliberate about. punktfunk mitigates this with **zero kernel
drivers** (the virtual display and gamepads are user-mode UMDF drivers), **sealed internal channels**
between the host and its drivers, and Administrators/SYSTEM-only permissions on its secrets. See
[Security & Safe Use](/docs/security) for the full picture, including why we recommend not hosting on
your most sensitive machine.

### One core, Windows backends

Most of Punktfunk is platform-agnostic. `punktfunk-core` (protocol, FEC, crypto, session, transport,
the C ABI), the QUIC control plane, the GameStream wire logic, the management API, and the per-frame
pipeline orchestration are all shared with the Linux host. The Windows host is a set of
`#[cfg(windows)]` backends behind the same traits the Linux host uses:

| Subsystem | Linux backend | Windows backend |
|---|---|---|
| **Capture** | xdg ScreenCast portal → PipeWire (dmabuf) | **IDD direct-push** — the `pf-vdisplay` driver copies finished frames into a host-owned shared GPU texture ring that the host consumes in-process (no Desktop Duplication, no Windows.Graphics.Capture); FP16/10-bit when the desktop is HDR |
| **Virtual display** | KWin / Mutter / Sway / gamescope | **pf-vdisplay** signed IDD — create a `WxH@Hz` monitor per session, capture it, tear it down |
| **Encode** | NVENC (CUDA) / VAAPI (AMD·Intel) / software | **NVENC** (NVIDIA) · **AMF** (AMD) · **QSV** (Intel) · software H.264; HEVC Main10 / BT.2020 PQ for HDR |
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

When your Windows desktop is in **HDR** mode, the host captures it as 10-bit, encodes **HEVC Main10 /
BT.2020 PQ**, and the client auto-detects HDR from the stream. A small always-on **Vulkan layer**
(bundled and registered by the installer) also lets **Vulkan games** enable HDR over the virtual
display — something the NVIDIA/AMD drivers otherwise refuse on an indirect display. The layer is
self-gating: it's a no-op on SDR and on real monitors. HDR is **Windows-only** (the Linux host is
8-bit, blocked upstream).

## Notes & limits

- **AMD / Intel encode is newer.** The NVENC path is the most exercised; AMF (AMD) and QSV (Intel) are
  built and tested in CI but less battle-tested on real hardware. Software H.264 is the GPU-less
  fallback.
- **x64-only.** No ARM64 build — no ARM64 NVIDIA driver, and the virtual-display driver is x64-only.
- **Newer than the Linux host.** The Linux host is the most battle-tested path; the Windows host is
  more recent, with the virtual-mic and AMD/Intel encode backends the youngest pieces.

Trouble? See [Troubleshooting](/docs/troubleshooting) and [Pairing](/docs/pairing).
