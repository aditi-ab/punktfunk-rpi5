---
title: Requirements
description: What you need to run a punktfunk host — GPU, driver, desktop, and network.
---

## Supported setups

A punktfunk host runs primarily on a Linux machine with an NVIDIA GPU (a native
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

> **Windows host:** punktfunk also runs as a native host on **Windows 10/11 (x64)** — a signed
> installer that registers a service and bundles a virtual-display driver. It encodes on NVIDIA
> (NVENC), AMD (AMF), or Intel (QSV), with a software fallback, and is newer than the Linux host; see
> [Windows Host](/docs/windows-host).

## GPU and driver

- **An NVIDIA GPU** with NVENC — effectively any GeForce RTX or workstation card. NVENC is what
  encodes the video in hardware.
- **NVIDIA driver 535 or newer** (550+ recommended). The driver must include the **GL/EGL userspace**,
  not just `nvidia-utils` — without it the compositor can't initialise the GPU and capture fails. Each
  setup guide installs the right package (e.g. `libnvidia-gl-<version>` on Ubuntu).
- **`nvidia-drm modeset=1`** must be enabled (Wayland on NVIDIA needs it). The setup guides cover this.

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
  assumes a trusted local network; it's not built to be exposed to the public internet.
- For best results, a wired or fast Wi-Fi link. The host can run a built-in **speed test** to pick a
  bitrate for your link (see [Configuration](/docs/configuration)).

## A client

You also need something to stream *to* — see [Connect a Client](/docs/clients). There are native
punktfunk clients for **Apple (macOS, iOS, iPadOS, tvOS), Linux, Windows, and Android**, and any
Moonlight client works too. All of them can discover the host on your network automatically.
