---
title: "Windows Host"
description: "Run the punktfunk streaming host on a Windows PC with an NVIDIA GPU."
---


**Status: implemented and shipping — NVIDIA-only, x64-only.** punktfunk is Linux-first, but it also
runs as a native **Windows host**: a signed installer registers a `LocalSystem` service that streams
your Windows desktop or games to any punktfunk or Moonlight client, at the client's exact resolution
via a virtual display — including **HDR10** (10-bit BT.2020 PQ) when your Windows desktop is in HDR
mode. It's newer and less battle-tested than the Linux host, and it is built specifically around
NVIDIA hardware. (The Linux host is 8-bit only — HDR there is blocked upstream.)

> This page is about the Windows **host** (streaming *from* a Windows PC). To stream *to* a Windows
> PC, see the [Windows client](/docs/clients#windows-desktop-client).

## Requirements

- **Windows 10/11, x64.** ARM64 is not supported — both NVENC and the virtual-display driver are
  x64-only.
- **An NVIDIA GPU + driver.** The host encodes with NVENC (`nvEncodeAPI64.dll`); there is no other
  encoder backend on Windows.
- **(Optional) ViGEmBus** for virtual gamepads — a manual prerequisite for now
  ([releases](https://github.com/nefarius/ViGEmBus/releases)).

## Install

Download the signed `punktfunk-host-setup-<ver>.exe` from the package registry and run it — it
installs the host into `C:\Program Files\punktfunk`, optionally installs the bundled **SudoVDA**
virtual-display driver, and registers + starts the service. Full steps (including the silent install
and the CLI `punktfunk-host service install` path) are in
[Running as a Service → Windows](/docs/running-as-a-service#windows); packaging internals live in
[`packaging/windows`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/README.md).

## How it works

The host installs a **`LocalSystem` SCM service** that runs from Session 0 and launches a worker into
the interactive session (`CreateProcessAsUserW`). That lets it **capture the secure desktop** (UAC
prompts, the lock screen) and keep streaming across reboots with nobody logged in — the same model
Sunshine and Apollo use. Service registration, firewall rules, and the supervisor all live in
`punktfunk-host service install`; the installer just lays the exe down and calls it elevated.

### One core, Windows backends

Most of punktfunk is platform-agnostic. `punktfunk-core` (protocol, FEC, crypto, session, transport,
the C ABI), the QUIC control plane, the GameStream wire logic, the management API, and the per-frame
pipeline orchestration are all shared with the Linux host. The Windows host is a set of
`#[cfg(windows)]` backends behind the same traits the Linux host uses:

| Subsystem | Linux backend | Windows backend |
|---|---|---|
| **Capture** | xdg ScreenCast portal → PipeWire (dmabuf) | **Windows.Graphics.Capture** (+ Desktop Duplication for the secure desktop) → D3D11 texture; FP16/10-bit when the desktop is HDR |
| **Virtual display** | KWin / Mutter / Sway / gamescope | **SudoVDA** signed IDD — create a `WxH@Hz` monitor per session, capture it, tear it down |
| **Encode** | `ffmpeg-next` NVENC (CUDA hwframes) | **NVENC** with a D3D11 device (`--features nvenc`); HEVC Main10 / BT.2020 PQ for HDR |
| **Input — mouse/keyboard** | libei / wlr protocols | **SendInput** (Win32 VK + absolute mouse) |
| **Input — gamepads** | uinput Xbox 360 pad + rumble | **ViGEm** virtual pad + rumble back-channel |
| **Audio capture** | PipeWire sink-monitor | **WASAPI loopback** |
| **Virtual mic** | PipeWire `Audio/Source` | WASAPI virtual mic |

The virtual display uses **[SudoVDA](https://github.com/VirtualDrivers)** (the Sunshine Virtual
Display Adapter) — a pre-built, signed Indirect Display Driver — so there is **no kernel driver to
author or WHQL-sign**. The installer bundles and stages it; if it's absent, the host falls back to
capturing an existing monitor (losing the per-client native-resolution output).

## Limitations

- **NVIDIA-only.** NVENC is the only encoder backend — there is no AMD / Intel / software encode path
  on Windows.
- **x64-only.** No ARM64 build (no ARM64 NVIDIA driver, and SudoVDA is x64-only).
- **Newer than the Linux host.** The Linux host is the most battle-tested path; the Windows host is
  more recent, with the virtual-mic and gamepad backends the youngest pieces.
