---
title: Ubuntu — KDE Plasma
description: Set up a punktfunk host on Ubuntu with KDE Plasma (KWin).
---

Set up a punktfunk host on **Ubuntu** running **KDE Plasma**. The host uses KDE's KWin compositor to
create a per-client virtual display. Needs **KWin 6.5.6 or newer**.

> New to this? Skim [Requirements](/docs/requirements) first.

## NVIDIA driver, dependencies, and build

These steps are identical to the GNOME guide — follow **steps 1–3** of
[Ubuntu — GNOME](/docs/ubuntu-gnome#1-nvidia-driver):

1. Install the NVIDIA driver **and** the `libnvidia-gl-<version>` userspace; enable `nvidia-drm
   modeset=1`; reboot and verify with `nvidia-smi`.
2. Install the build toolchain and runtime libraries (the same `apt` line).
3. Clone and `cargo build --release -p punktfunk-host`.

## Configure

The host reads `~/.config/punktfunk/host.env`. For KDE Plasma:

```sh
mkdir -p ~/.config/punktfunk
cat > ~/.config/punktfunk/host.env <<'ENV'
WAYLAND_DISPLAY=wayland-0
XDG_CURRENT_DESKTOP=KDE
PUNKTFUNK_COMPOSITOR=kwin
PUNKTFUNK_VIDEO_SOURCE=virtual
PUNKTFUNK_ZEROCOPY=1
PUNKTFUNK_INPUT_BACKEND=libei
ENV
```

> Make sure you're on a **KDE Wayland** session (not X11) — the picker on the login screen. The
> virtual-display path is Wayland-only. See the [Configuration reference](/docs/configuration) for
> every option.

## Run

From a terminal **inside your Plasma session**:

```sh
cargo run --release -p punktfunk-host -- serve --native
```

The host starts listening and advertises itself on the network. Now [connect a client](/docs/clients).

To run it at boot — including fully **headless**, with KWin brought up automatically and no login —
see [Running as a Service](/docs/running-as-a-service); the headless appliance is built around KDE.

## Troubleshooting

- **KWin too old:** virtual outputs need KWin **≥ 6.5.6**. Check with `kwin_wayland --version`.
- **No picture / capture fails:** confirm you're on a Wayland session and the NVIDIA GL userspace is
  installed (`libnvidia-gl-<version>`). More in [Troubleshooting](/docs/troubleshooting).
