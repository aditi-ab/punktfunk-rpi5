---
title: Requirements
description: What you need to run a punktfunk host — GPU, driver, desktop, and network.
---

## Supported setups

A punktfunk host runs primarily on a Linux machine with a dedicated GPU — NVIDIA (NVENC) is the
most-exercised path, and AMD/Intel GPUs work via VAAPI (a native
[Windows host](/docs/windows-host) is also available — see below). These are the Linux desktop
environments it supports today, each with its own guide:

| Setup | Desktop / compositor | Guide |
|---|---|---|
| **Ubuntu** (Desktop or Server) | GNOME (Mutter) | [Ubuntu — GNOME](/docs/ubuntu-gnome) |
| **Ubuntu** (Desktop or Server) | KDE Plasma (KWin) | [Ubuntu — KDE](/docs/ubuntu-kde) |
| **Fedora** | KDE Plasma (KWin) | [Fedora — KDE](/docs/fedora-kde) |
| **Bazzite** | gamescope (Steam) | [Bazzite](/docs/bazzite) |

Other wlroots compositors (Sway/Hyprland) also work but aren't a primary target. If your desktop isn't
listed, the host still needs one of these compositor backends to create a virtual display.

> **Windows host:** punktfunk also runs as a native host on **Windows 11 22H2 or newer (x64)** — a
> signed installer that registers a service and bundles a virtual-display driver (whose driver-
> framework needs make 22H2 the hard floor — Windows 10 is not supported). It encodes on NVIDIA
> (NVENC), AMD (AMF), or Intel (QSV), with a software fallback, and is newer than the Linux host; see
> [Windows Host](/docs/windows-host).

## GPU and driver

- **An NVIDIA GPU** with NVENC — effectively any GeForce RTX or workstation card. NVENC is what
  encodes the video in hardware.
- **NVIDIA driver 535 or newer** (550+ recommended). The driver must include the **GL/EGL userspace**,
  not just `nvidia-utils` — without it the compositor can't initialise the GPU and capture fails. Each
  setup guide installs the right package (e.g. `libnvidia-gl-<version>` on Ubuntu).
- **`nvidia-drm modeset=1`** must be enabled (Wayland on NVIDIA needs it). The setup guides cover this.
- **AMD / Intel GPUs** encode via **VAAPI** instead (install `mesa-va-drivers` or
  `intel-media-driver`; validated live on AMD RDNA3). The NVIDIA-specific notes above don't apply
  there. On modern Intel (Gen12/Tiger Lake and newer, including Arc) the driver only offers the
  **low-power (VDEnc)** encode entrypoint — the host detects this and falls back automatically
  (`PUNKTFUNK_VAAPI_LOW_POWER=1|0` pins it) — and low-power encode needs the **HuC firmware**
  loaded (the kernel default on those platforms; check `dmesg | grep -i huc` if encoding fails).
  A GPU-less software H.264 encoder also exists (`PUNKTFUNK_ENCODER=software`), meant as a
  fallback rather than a daily driver.

> Consumer GeForce cards historically cap the number of **concurrent** NVENC sessions (a few at once);
> workstation cards don't. This only matters if you stream to many devices simultaneously.

## Desktop session

The host attaches to a **Wayland** desktop session and creates virtual displays in it, so a session
needs to be running for the user the host runs as. This can be:

- a **normal logged-in desktop** (you're sitting at the machine, or it auto-logs-in), or
- a **headless session** that comes up at boot with no monitor or login — see
  [Running as a Service](/docs/running-as-a-service).

Minimum compositor versions (newer is fine):

- **KWin ≥ 6.5.6** (KDE Plasma) — headless virtual outputs.
- **GNOME ≥ 48** (Mutter) — virtual-monitor screen-cast.
- **gamescope ≥ 3.16.22** (Bazzite/Steam) — older versions deadlock during capture.

## Network

- Host and client on the **same network** — a LAN, or a VPN that puts them on one subnet. punktfunk
  assumes a trusted local network; it's **not built to be exposed to the public internet — don't
  port-forward it.** To stream from outside your home, use a VPN so the remote client is on the same
  private subnet.
- For best results, a wired or fast Wi-Fi link. The host can run a built-in **speed test** to pick a
  bitrate for your link (see [Configuration](/docs/configuration)).

> **Before you set up a host, read [Security & Safe Use](/docs/security).** A streaming host is
> remote control of the machine — it's important to understand what that exposes, why to keep it on a
> trusted network, and how pairing protects you.

## A client

You also need something to stream *to* — see [Connect a Client](/docs/clients). There are native
punktfunk clients for **Apple (macOS, iOS, iPadOS, tvOS), Linux, Windows, and Android**, and any
Moonlight client works too. All of them can discover the host on your network automatically.
