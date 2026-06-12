---
title: Troubleshooting
description: Common problems setting up or using a punktfunk host, and how to fix them.
---

## The host isn't found on the network

- Make sure the host is actually running (`systemctl --user status punktfunk-host`, or you see it
  listening in the terminal).
- Host and client must be on the **same network/subnet**. Discovery uses mDNS, which doesn't cross
  routed subnets or most VPNs-without-multicast. As a fallback, add the host by **IP address** in your
  client.
- A firewall on the host can block it. The native protocol uses UDP port **9777** (plus the data
  port); GameStream/Moonlight uses its standard ports. Allow them on the host's firewall.

## `nvidia-smi` says it can't communicate with the driver

- The NVIDIA kernel module didn't load. With **Secure Boot** enabled, enrol the module's signing key:
  `sudo mokutil --import /var/lib/shim-signed/mok/MOK.der`, reboot, **Enrol MOK** at the blue screen
  (or disable Secure Boot). On Fedora, follow RPM Fusion's Secure Boot steps.
- After a kernel update the module may need a rebuild — reinstall the driver package.

## The desktop won't start, or "GPU … not supported by EGL"

The NVIDIA **GL/EGL userspace** is missing — the base driver package doesn't always include it.

- **Ubuntu:** `sudo apt install libnvidia-gl-<version>` (matching your driver).
- Confirm `/usr/share/glvnd/egl_vendor.d/10_nvidia.json` exists and `nvidia-drm modeset` is `Y`.

## Black screen / no picture, but the client connects

- You must be on a **Wayland** session, not X11 (check the login-screen session picker).
- KWin must be **≥ 6.5.6** (`kwin_wayland --version`); GNOME **≥ 48**; gamescope **≥ 3.16.22**.
- Confirm `PUNKTFUNK_COMPOSITOR` in [`host.env`](/docs/configuration) matches your desktop.

## Capture fails: "Session creation inhibited" (GNOME)

A **locked** GNOME session blocks screen capture. On an always-on/headless host, disable the lock:

```sh
gsettings set org.gnome.desktop.screensaver lock-enabled false
gsettings set org.gnome.desktop.session idle-delay 0
```

See [Running as a Service](/docs/running-as-a-service).

## A controller is detected but does nothing (Bazzite)

The host user needs to be in the `input` group. On Bazzite:

```sh
ujust add-user-to-input-group
```

Then log out and back in. On other distros this is `sudo usermod -aG input $USER` + re-login.

## Pairing is rejected / the client can't connect

- The host **requires pairing** by default. Arm pairing (web console, or `--allow-pairing`), then
  enter the PIN on the client. See [Pairing & Trust](/docs/pairing).
- If you re-installed the host, its identity changed — re-pair the client.

## Stutter, drops, or high latency

- Lower the **bitrate**. On a busy or Wi-Fi link, the requested bitrate may be too high — the Apple
  app's [speed test](/docs/configuration#bitrate) picks a safe value; with Moonlight, set it manually.
- Prefer a **wired** connection or 5 GHz Wi-Fi between host and client.
- Streaming to **many devices at once** shares the GPU encoder; cap concurrency with
  `--max-concurrent`.

## Still stuck?

Run the host with `RUST_LOG=info` (or `debug`) and check `journalctl --user -u punktfunk-host` for the
error around the failed connect or capture.
