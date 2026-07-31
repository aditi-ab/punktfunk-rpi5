---
title: Requirements
description: What you need to run a Punktfunk host — GPU, driver, desktop, and network.
---

## Supported setups

A Punktfunk host runs primarily on a Linux machine with a dedicated GPU — NVIDIA (NVENC) is the
most-exercised path, and AMD/Intel GPUs work via Vulkan Video or VAAPI. A native [Windows host](/docs/windows-host)
is also available. Setup splits along two axes: you **install** the package per distro, then
**configure** the host — and learn its quirks — per desktop/compositor.

> New here? Read [Security & Safe Use](/docs/security) first — a streaming host is remote control of
> the machine, so keep it on a trusted LAN or VPN and require pairing.

**Distros — install the package:**

- [Ubuntu](/docs/ubuntu)
- [Fedora](/docs/fedora)
- [Arch](/docs/arch)
- [Bazzite](/docs/bazzite)
- [SteamOS](/docs/steamos-host)

**Desktops — configure and quirks:**

- [KDE Plasma (KWin)](/docs/kde)
- [GNOME (Mutter)](/docs/gnome)
- [Steam / gamescope](/docs/gamescope)
- [Hyprland](/docs/hyprland)
- [Sway / wlroots](/docs/sway)

Pick your distro to install, then your desktop to configure — the two are independent. The host
needs one of these compositor backends to create a virtual display.

Support is deliberately non-uniform: each compositor and each GPU vendor gets its own capture,
display and input backend, and they are not equally capable. The [Support
matrix](/docs/support-matrix) has a row for every host desktop, GPU and client app, with each cell
taken from the code that makes the decision — read it before assuming a feature is available on your
combination.

> **Windows host:** Punktfunk also runs as a native host on **Windows 11 22H2 or newer (x64)** — a
> signed installer that registers a service and bundles a virtual-display driver whose driver
> framework (IddCx 1.10) makes 22H2 the hard floor — Windows 10 is not supported. It encodes on NVIDIA
> (NVENC), AMD (AMF), or Intel (QSV), with a software fallback, and is newer than the Linux host; see
> [Windows Host](/docs/windows-host).

## GPU and driver

- **An NVIDIA GPU** with NVENC — effectively any GeForce RTX or workstation card. NVENC is what
  encodes the video in hardware.
- **NVIDIA driver 535 or newer** (550+ recommended). The driver must include the **GL/EGL userspace**,
  not just `nvidia-utils` — without it the compositor can't initialise the GPU and capture fails. Each
  install guide installs the right package (e.g. `libnvidia-gl-<version>` on Ubuntu).
- **`nvidia-drm modeset=1`** must be enabled (Wayland on NVIDIA needs it). The install guides cover this.
- **AMD / Intel GPUs** encode without any of the NVIDIA pieces above. For **HEVC and AV1** the host
  goes through **Vulkan Video** by default, so you want an up-to-date Mesa and the matching Vulkan
  driver — `mesa-vulkan-drivers` on Ubuntu, `vulkan-radeon` / `vulkan-intel` on Arch.
  **VAAPI** (`mesa-va-drivers` or `intel-media-driver`) is the H.264 path and the fallback for
  everything else: a machine with only the VAAPI driver still streams, it just gives up the Vulkan
  path's cleaner recovery from packet loss. `PUNKTFUNK_VULKAN_ENCODE=0` pins VAAPI. Validated live
  on AMD RDNA3. On modern Intel (Gen12/Tiger Lake and newer, including Arc) the VAAPI driver only
  offers the **low-power (VDEnc)** encode entrypoint — the host detects this and falls back automatically
  (`PUNKTFUNK_VAAPI_LOW_POWER=1|0` pins it) — and low-power encode needs the **HuC firmware**
  loaded (the kernel default on those platforms; check `dmesg | grep -i huc` if encoding fails).
  A GPU-less software H.264 encoder also exists (`PUNKTFUNK_ENCODER=software`), meant as a
  fallback rather than a daily driver.

> Consumer GeForce cards historically cap the number of **concurrent** NVENC sessions (a few at once);
> workstation cards don't. This only matters if you stream to many devices simultaneously.

### HDR and 10-bit

HDR (10-bit BT.2020 PQ) is on by default, and what a Linux box needs for it is a **gamescope**
session running the patched `punktfunk-gamescope` build, or **GNOME 50 or newer** mirroring a real
HDR monitor on the GameStream plane — the ordinary KWin, Mutter and wlroots virtual displays are
8-bit upstream and stream SDR. [HDR](/docs/hdr) has the full chain, per host and per client, and how
to find the link that is missing.

## Desktop session

The host attaches to a **Wayland** desktop session and creates virtual displays in it, so either a
session is running for the user the host runs as, or the host brings one up itself. This can be:

- a **normal logged-in desktop** (you're sitting at the machine, or it auto-logs-in),
- a **headless session** that comes up at boot with no monitor or login — see
  [Running as a Service](/docs/running-as-a-service), or
- **no session at all** — on the **gamescope** backend the host spawns its own headless gamescope
  per client connect (on a Steam appliance it can bring up the whole Steam session), so nothing has
  to be running beforehand. Auto-detection reads the *live* session, so on a box that boots to
  nothing, set `PUNKTFUNK_COMPOSITOR=gamescope` in `host.env` — with a gamescope session already
  running the host finds it by itself. See [Steam / gamescope](/docs/gamescope).

Minimum compositor versions (newer is fine):

- **KWin ≥ 6.5.6** ([KDE Plasma](/docs/kde)) — headless virtual outputs.
- **GNOME ≥ 48** ([Mutter](/docs/gnome)) — virtual-monitor screen-cast.
- **gamescope ≥ 3.16.22** ([Bazzite/Steam](/docs/gamescope)) — below this, headless capture
  deadlocks against PipeWire ≥ 1.6.
- **gamescope ≥ 3.16.23** for the Steam overlay (Shift+Tab / Quick Access Menu) to reach the stream
  at all — older builds never paint it into the node the host captures, so no host setting can bring
  it back.

For **HDR** on gamescope you additionally need the patched `punktfunk-gamescope` build — see
[HDR and 10-bit](#hdr-and-10-bit) above.

The same floors, with what each one gates, are in [Version floors worth
knowing](/docs/support-matrix#version-floors-worth-knowing); where the two disagree, the matrix is
the one checked against the code.

## Network

- Host and client on the **same network** — a LAN, or a VPN that puts them on one subnet. Punktfunk
  assumes a trusted local network; it's **not built to be exposed to the public internet — don't
  port-forward it.** To stream from outside your home, use a VPN so the remote client is on the same
  private subnet.
- For best results, a wired or fast Wi-Fi link. The host can run a built-in **speed test** to pick a
  bitrate for your link (see [Configuration](/docs/configuration)).

## A client

You also need something to stream *to* — see [Connect a Client](/docs/clients). There are native
Punktfunk clients for **Apple (macOS, iOS, iPadOS, tvOS), Linux, Windows, and Android**, and any
Moonlight client works too. All of them can discover the host on your network automatically.
