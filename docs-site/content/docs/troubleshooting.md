---
title: Troubleshooting
description: Common problems setting up or using a punktfunk host, and how to fix them.
---

## Another streaming host (Sunshine, Apollo, …) is installed

Punktfunk is a Moonlight-compatible host. So are **Sunshine** and its forks (**Apollo**,
**Vibeshine**, **Vibepollo**, **LuminalShine**, …). Running one of them **at the same time** as
Punktfunk is **not supported**: they bind the *same* GameStream ports (47984/47989 and
47998–48010, plus a web UI on 47990 that collides with Punktfunk's management API), advertise the
*same* `_nvstream` mDNS name, and often install a *conflicting virtual-display driver*. The result
is `address already in use` errors, pairing that silently fails, the wrong host answering a client,
and capture/display glitches.

- Punktfunk warns about this automatically: the host logs it at startup (visible in the web
  console's **Logs** tab and the system tray tooltip), and the Windows installer warns before
  installing. To check on demand, run:

  ```
  punktfunk-host detect-conflicts
  ```

  It lists any conflicting host found (installed or running) and exits non-zero if there is one.
- **Fix:** stop and uninstall the other host, then start Punktfunk — e.g. stop the service
  (`sudo systemctl disable --now sunshine` / on Windows `sc stop SunshineService`) and uninstall it.
  If you only want to try Punktfunk without removing the other host, at least make sure the other
  host is fully **stopped** first (they cannot both run at once).

## The host isn't found on the network

- Make sure the host is actually running (`systemctl --user status punktfunk-host`, or you see it
  listening in the terminal).
- Host and client must be on the **same network/subnet**. Discovery uses mDNS, which doesn't cross
  routed subnets or most VPNs-without-multicast. As a fallback, add the host by **IP address** in your
  client.
- A firewall on the host can block it. The native protocol's **control plane** is a fixed UDP port,
  **9777** — open this one. The per-session **data plane** rides a *separate, random* UDP port and
  usually needs **no** firewall rule (see [Video is slow to start, or fails across
  subnets](#video-is-slow-to-start-or-fails-across-subnets) for why, and the one case where opening it
  helps). GameStream/Moonlight (only with `--gamestream`) uses TCP **47984/47989/48010** + UDP
  **47998–48010** (video/FEC 47998, ENet control 47999, audio 48000) + mDNS UDP **5353**. Allow those
  on the host's firewall.

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

To remove the ~2.5 s fallback delay, **pin the data port** with `--data-port` (or the
`PUNKTFUNK_DATA_PORT` env in `host.env`) and open exactly that one port. The host then binds that
fixed port, skips the punch-wait, and streams straight to the client — no timeout to pay:

```sh
punktfunk-host serve --data-port 9778     # or PUNKTFUNK_DATA_PORT=9778 in host.env
sudo ufw allow 9778/udp                    # open exactly that one port
```

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
- KWin must be **≥ 6.5.6** (`kwin_wayland --version`); GNOME **≥ 48**; gamescope **≥ 3.16.22**. See
  [KDE](/docs/kde) for the KWin/Wayland requirement and [gamescope](/docs/gamescope) for the
  gamescope one.
- If [`host.env`](/docs/configuration) sets `PUNKTFUNK_COMPOSITOR`, **remove it** — the host
  auto-detects the live compositor, and the pin points it at one backend even when a different
  session is live (it also disables Gaming ↔ Desktop following).

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

## A controller is detected but does nothing (Bazzite)

The host user needs to be in the `input` group. On Bazzite:

```sh
ujust add-user-to-input-group
```

Then log out and back in. On other distros this is `sudo usermod -aG input $USER` + re-login. See
[Bazzite](/docs/bazzite).

## Pairing is rejected / the client can't connect

- The host **requires pairing** by default. Arm pairing from the web console, then enter the PIN on
  the client. See [Pairing & Trust](/docs/pairing).
- If you re-installed the host, its identity changed — re-pair the client.

## Stutter, drops, or high latency

- Lower the **bitrate**. On a busy or Wi-Fi link, the requested bitrate may be too high — the native
  clients' [speed test](/docs/configuration#bitrate) picks a safe value; with Moonlight, set it
  manually.
- Prefer a **wired** connection or 5 GHz Wi-Fi between host and client.
- Streaming to **many devices at once** shares the GPU encoder. The host serves several
  concurrent native sessions (up to 4 by default); heavy load is usually bitrate-bound, so
  lower the bitrate first.

## Windows: "punktfunk Virtual Display" shows Code 10 in Device Manager

Sessions end with *"pf-vdisplay driver interface not found"* and Device Manager shows the
**punktfunk Virtual Display** device failed with **Code 10** (`STATUS_DEVICE_POWER_FAILURE`).

This means your Windows version is too old. The virtual-display driver requires the **IddCx 1.10**
driver framework, which first shipped in **Windows 11 22H2 (build 22621)** — on Windows 10
(including LTSC) and Windows 11 21H2 the driver installs but cannot start. Reinstalling won't help;
the fix is updating to Windows 11 22H2 or newer. (Current installers refuse to run on older
Windows for this reason; if you see this, the host was likely installed with an older installer.)

## Still stuck?

Run the host with `RUST_LOG=info` (or `debug`) and check `journalctl --user -u punktfunk-host` for the
error around the failed connect or capture.
