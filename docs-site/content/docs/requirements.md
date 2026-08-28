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

## The floor for a working host

**On apt distros that means Ubuntu 26.04 or newer, or Debian 13 or newer.** Both are supported
targets and both install from the same repository.

This floor is about the **desktop**, not the package. A host needs a compositor that can create a
virtual display, and those have version floors of their own ([below](#desktop-session)). Older
releases will happily install `punktfunk-host` and then have nothing that can produce a stream —
so read this as the real requirement, not the package's:

| Release | Package installs | Can actually host |
|---|---|---|
| **Ubuntu 26.04+** | ✅ | ✅ KWin 6.5+, GNOME 48+, gamescope |
| **Debian 13+** | ✅ | ✅ GNOME 48.7, sway 1.10, gamescope (its KWin 6.3.6 is below the floor) |
| Ubuntu 24.04 LTS | ✅ | ❌ KWin 5.27 (floor 6.5.6), GNOME 46 (floor 48), no gamescope available |
| Debian 12 | ❌ glibc 2.36 | ❌ |

Ubuntu 24.04 is called out because the package *does* install there — it is built on 24.04 precisely
so one package spans the range — which makes the gap easy to mistake for a bug. It is not: 24.04
ships no compositor new enough, and no `gamescope` (the patched
[`punktfunk-gamescope`](/docs/gamescope) cannot run there either — 24.04 is too old on wayland,
libinput, libavif and pixman). The same gap is why
[Linux Mint 22.x cannot host](#cinnamon-linux-mint-and-lmde).

### Cinnamon, Linux Mint and LMDE

**A Cinnamon desktop cannot host a virtual display, and no setting changes that.** Punktfunk gives
each client its own screen at that device's exact resolution by asking the compositor to create a
virtual output. Cinnamon's compositor, **Muffin**, has no such API: it forked from Mutter 3.36, and
its `org.cinnamon.Muffin.ScreenCast` interface offers only `RecordMonitor` and `RecordWindow` —
never the `RecordVirtual` that Mutter gained in 42. Its portal backend
(`xdg-desktop-portal-xapp`) implements no ScreenCast either, so the route that serves Sway and
Hyprland is closed too. This is upstream's to fix, not a Punktfunk setting.

Which Mint you run decides whether there is any route at all:

| Edition | Base | Can it host? |
|---|---|---|
| **LMDE 7 "Gigi"** | Debian 13 | ✅ Yes — via gamescope ([Debian page](/docs/debian#4-start-it)) |
| **Linux Mint 22.x** ("Wilma"…"Zena") | Ubuntu 24.04 | ❌ No — see below |
| **Linux Mint 23** | Ubuntu 26.04 | ✅ Expected — due December 2026 |

On **LMDE 7** what works is **gamescope**: the host starts its own headless gamescope for each
connecting client and runs the game inside it, so it needs no desktop compositor at all. Your
Cinnamon session keeps running untouched; the stream is the game, not the desktop. Install
`punktfunk-gamescope` (Debian ships no gamescope of its own; the patched build is what gives the
stream HDR, a visible cursor and the client's real refresh rate) and pin
`PUNKTFUNK_COMPOSITOR=gamescope` — auto-detection reads the live session, finds Cinnamon, and stops
with an error rather than guessing. To stream the *desktop* from an LMDE box, log into a GNOME or
Sway session instead — Debian 13 ships GNOME 48.7 and sway 1.10, both above the floors (its KDE is
KWin 6.3.6, below the 6.5.6 floor, so Plasma is not an option there yet).

**Linux Mint 22.x cannot host.** `punktfunk-host` installs, which makes this easy to miss, but
nothing on the box can produce a stream: Cinnamon can't (above); gamescope isn't packaged for Ubuntu
24.04 and the patched `punktfunk-gamescope` can't run there either (24.04 is short on wayland ≥ 1.23.1,
libinput ≥ 1.26, libavif ≥ 1.2.1, pixman ≥ 0.44, and has no `libdisplay-info2` or `libxcb-errors0`);
and switching desktop doesn't rescue it — 24.04's KWin 5.27 and GNOME Shell 46 are below the
floors, leaving only sway 1.9 as a candidate. Use **LMDE 7** on Mint hardware today, or wait for
**Mint 23**.

**Distros — install the package:**

- [Ubuntu](/docs/ubuntu) — 26.04 or newer for a working host
- [Debian](/docs/debian) — 13 or newer, including LMDE
- [Fedora](/docs/fedora)
- [Arch](/docs/arch)
- [Bazzite](/docs/bazzite)
- [SteamOS](/docs/steamos-host)
- [NixOS](/docs/nixos)

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
- **Hyprland — no version floor** ([Hyprland](/docs/hyprland)): the `hyprctl` path is
  version-independent, and both config eras (hyprlang and the newer Lua one) are handled. Contracts
  are verified against **0.55.4** and **0.56.2**. What Hyprland *does* need is
  **`xdg-desktop-portal-hyprland`** — capture goes through it, and Hyprland does not pull it in.
  On **0.49+**, if you have turned `ecosystem.enforce_permissions` on (off by default), grant the
  host screencopy and virtual input: a denial is *silent black frames and dropped input*, never an
  error.
- **Omarchy ≥ 4.0** ([Omarchy](/docs/omarchy)) — not a compositor floor but an integration one: 4.0
  replaced the shell, the menu format and the Hyprland config language at once, so every point
  `punktfunk-omarchy` touches is different below it.
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
