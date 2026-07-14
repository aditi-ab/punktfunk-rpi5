---
title: Configuration
description: Every host.env setting and PUNKTFUNK_* environment variable — compositor, video, input, gamepads, tuning — and how to use them.
---

The host reads its settings from **`~/.config/punktfunk/host.env`** (a simple `KEY=value` file, `#`
starts a comment). On Windows the service reads **`%ProgramData%\punktfunk\host.env`** instead. Your
[setup guide](/docs/requirements) gives you a starting `host.env` for your desktop; this page is the
full reference for every setting.

> **You rarely need most of these.** The host **auto-detects** the compositor, input backend, and
> encoder from your live session — a box that flips between Steam Gaming Mode and a KDE/GNOME desktop
> is followed automatically. The `PUNKTFUNK_*` knobs below are mostly **optional overrides** for
> forcing a specific backend, tuning performance, or debugging. The starter `host.env` for your
> platform sets only the few you actually need.

## Session anchors

These tell the host which desktop session to attach to. Your setup guide sets them for you; they're
required when the host runs outside your interactive session (e.g. as a service).

| Setting | What it does |
|---|---|
| `XDG_RUNTIME_DIR` | Your session's runtime dir (e.g. `/run/user/1000`). Always needed for a service. |
| `DBUS_SESSION_BUS_ADDRESS` | Your session bus (e.g. `unix:path=/run/user/1000/bus`). Always needed for a service. |
| `WAYLAND_DISPLAY` | The Wayland socket of your session (`wayland-0` for a normal desktop, `wayland-kde` for the headless-KDE unit). |
| `XDG_CURRENT_DESKTOP` | Your desktop (`GNOME`, `KDE`). |

On Linux the host **rewrites `WAYLAND_DISPLAY` / `XDG_CURRENT_DESKTOP` / `XDG_RUNTIME_DIR` /
`DBUS_SESSION_BUS_ADDRESS` on every connect** to follow the active session (Gaming ↔ Desktop). Only
`XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` need to be pinned as trustworthy anchors.

## Core

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_COMPOSITOR` | `kwin` · `mutter` · `gamescope` · `wlroots` · `hyprland` (aliases: `kde`/`plasma`, `gnome`, `sway`/`wlr`) | Which backend creates the virtual display. `wlroots` is sway/River; `hyprland` is its own backend. **Leave unset to auto-detect;** set only to force one. |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` · `portal` | `virtual` creates a per-client display at the client's exact mode (the normal choice). `portal` captures an existing monitor instead. |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` *(default on)* | GPU zero-copy capture→encode (dmabuf → CUDA → NVENC, or D3D11 on Windows). **On by default** — no need to set it; it falls back to a CPU path automatically. Set `0` to force the CPU path. One exception: Windows **Intel/QSV** keeps the CPU path by default until zero-copy is validated on Intel hardware — set `1` to try it there. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `gamescope` · `wlr` · `uinput` | How input is injected. `libei` for GNOME/KDE, `gamescope` for Bazzite/gamescope, `wlr` for Sway/wlroots **and Hyprland**. Auto-detected with the compositor. |
| `PUNKTFUNK_ENCODER` | `auto` · `nvenc` · `vaapi` (Linux) · `amf` · `qsv` (Windows) · `software` | Encoder backend. `auto` (default) detects the GPU vendor: NVIDIA→NVENC, AMD→VAAPI/AMF, Intel→VAAPI/QSV. `software` (aliases `sw`/`openh264`) is the GPU-less H.264 path on both platforms — on Windows `auto` falls back to it when no GPU is found; on Linux it is **explicit-only** (`auto` never picks it). |
| `PUNKTFUNK_RENDER_NODE` | path | Linux DRM render node for zero-copy (default `/dev/dri/renderD128`). Set on multi-GPU boxes to pick the right GPU. |

Resolution and refresh are **not** set here — **the client chooses them.** When a device connects,
the host creates a virtual display at that device's resolution and refresh rate. A 1080p60 laptop and
a 1440p120 desktop each get their own. (With Moonlight, set the mode in Moonlight; the native clients
let you pick a mode or default to the device's display.)

## gamescope / session following (Linux, Bazzite/SteamOS)

Two mutually-exclusive models for a Steam/gamescope box. See [Steam / gamescope](/docs/gamescope) for
the full picture (and [Bazzite](/docs/bazzite) for that distro's specifics).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` | **Attach** model: the box owns its gamescope session (you switch Gaming ↔ Desktop with the Steam UI); the host just captures whatever's live and never tears it down. Rock-solid; streamed resolution is the box's gamescope mode. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model: the host tears the box's gamescope down on connect and launches its **own** at the *client's* exact resolution, restoring on idle. Client-mode-following, but doesn't coexist with a box-owned game-mode session. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode (headless appliance; no physical session running). |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover + capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch **mid-stream** (rebuild the backend in place, no reconnect). **On by default** on Bazzite/SteamOS; set `0` to disable. |

## Compositor-specific (Linux)

See your desktop page ([KDE](/docs/kde), [GNOME](/docs/gnome)) for when to set these.

> **Managing virtual displays** — keep-alive after disconnect, exclusive vs. extend, and (on
> Windows/KDE) persistent per-client scaling — now has its own settings surface in the web console
> and `display-settings.json`. See [Virtual displays](/docs/virtual-displays). The two
> `*_VIRTUAL_PRIMARY` knobs and `PUNKTFUNK_MONITOR_LINGER_MS` below still work but are superseded by
> it (a settings file wins over them).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_KWIN_VIRTUAL_PRIMARY` | `1` | Make the streamed per-session output the sole desktop so plasmashell + windows render on it (not on the headless bootstrap output). Set by the KDE appliance `host.env`. Superseded by the console's **Topology** setting. |
| `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY` | `1` | GNOME/Mutter equivalent of the above. |

## Session recovery (Linux)

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_RECOVER_SESSION_CMD` | command | Operator hook fired (debounced) when a client connects while **no graphical session is live** for the host's user — the state a compositor crash leaves behind (gnome-shell SIGSEGV → GDM greeter, whose auto-login is once-per-boot). Typically `sudo -n systemctl restart gdm` with a matching NOPASSWD sudoers rule, or `systemctl restart display-manager` under a polkit rule; with auto-login enabled the restart brings the desktop back and the client's automatic retry lands in it. Unset/empty = disabled (the default). |

## Video quality

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_FEC_PCT` | `N` (percent) | Forward-error-correction redundancy for lossy links (the default is sensible for a normal LAN). Higher = more loss-resilient, more bandwidth. |
| `PUNKTFUNK_10BIT` | `1` | HEVC Main10 / HDR. Honored only when the client also advertises 10-bit. **Windows host only** (the Linux host stays 8-bit). |
| `PUNKTFUNK_444` | `1` | Full-chroma HEVC 4:4:4 (Range Extensions) — sharper text/desktop, no chroma loss. **punktfunk/1 native only** (Moonlight stays 4:2:0), HEVC-only, honored only when the client advertises 4:4:4 **and** the GPU supports it (probed; NVENC is the validated path — VAAPI/AMF/QSV decline). Independent of 10-bit. |
| `PUNKTFUNK_DSCP` | `1` | Opt-in DSCP / `SO_PRIORITY` QoS tagging on the media sockets. No-op on the wire on Windows without a qWAVE policy. |
| `PUNKTFUNK_OH264_THREADS` / `PUNKTFUNK_OH264_GOP` | `N` | Software (openh264) encoder tuning: encode threads (default 2 — latency over throughput) and GOP length (default 0 = encoder-auto). Only relevant with `PUNKTFUNK_ENCODER=software`. |

## Gamepads

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMEPAD` | `xbox360` · `xboxone` · `dualsense` · `dualsenseedge` · `dualshock4` · `steamdeck` · `steamcontroller` (aliases: `ps5`, `edge`, `ps4`, `deck`, …) | The virtual pad the host creates. Usually **auto-resolved from the client's physical controller** — set this only to force a type. `xbox360` (XInput) is the universal fallback. `dualsenseedge` gives the client's back paddles native buttons. DualSense (Edge)/DualShock 4/Steam Deck need Linux UHID; unsupported choices fold to Xbox 360. |
| `PUNKTFUNK_STEAM_GADGET` | `1` · `0` | Force the raw USB-gadget virtual Steam Deck on/off. **On by default on SteamOS**, off elsewhere. Lets Steam promote the virtual Deck to full Steam Input. |

## Audio / microphone

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_AUDIO_GAIN` | float (default `1.0`) | Linear gain applied to capture — bump it for a quiet source. |
| `PUNKTFUNK_MIC_DEVICE` | name substring | **(Windows)** Target mic-uplink device by friendly-name substring (first match wins). |
| `PUNKTFUNK_NO_MIC_INSTALL` | set | **(Windows)** Skip installing the virtual-mic driver (e.g. when the host runs as SYSTEM). |

## Windows host

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_VDISPLAY` | `pf` | Virtual-display backend. The bundled pf-vdisplay IddCx driver is the only backend now — informational; leave as `pf`. |
| `PUNKTFUNK_SECURE_DDA` | `1` | Capture the secure desktop (UAC / lock / login) so the stream survives those transitions. |
| `PUNKTFUNK_MONITOR_LINGER_MS` | ms (default `10000`) | Defer tearing a per-client virtual display down after disconnect. A reconnect inside the window preempts it and creates a fresh one (a reused IddCx swap-chain is dead); the stable per-client monitor id keeps Windows' saved display config applying either way. Superseded by the console's **Keep alive** setting — see [Virtual displays](/docs/virtual-displays). |
| `PUNKTFUNK_RENDER_ADAPTER` | description substring | Multi-GPU boxes only: force the NVENC/capture GPU by adapter Description substring (e.g. `4090`). Leave unset on single-GPU machines. |
| `PUNKTFUNK_HOST_CMD` | e.g. `serve --gamestream` | The host subcommand the service launches. Default `serve --gamestream`; use `serve` for a secure native-only host. |

## Network & discovery

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_MDNS` | `1` · `0` *(default on)* | mDNS adverts (native + GameStream). `0` skips them (same as `--no-mdns`) — for networks/containers where multicast doesn't work; add the host by address in the client instead. |
| `PUNKTFUNK_DATA_PORT` | port | Pin the per-session video data plane to a fixed UDP port and stream direct (no hole-punch) — open exactly that port in the host firewall. Same as `serve --data-port`; see [Troubleshooting](/docs/troubleshooting). Default: random port + hole-punch. |

## Auth, API & paths

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_MGMT_TOKEN` | token | Bearer token for the management API. If unset it's auto-generated and persisted to `~/.config/punktfunk/mgmt-token` (the bundled web console sources it). Set only to pin a specific token. |
| `PUNKTFUNK_UI_PASSWORD` | password | Web-console login password. Normally generated on first start and stored in `~/.config/punktfunk/web-password` — see [Forgot your Password?](/docs/forgot-password). |
| `PUNKTFUNK_CONFIG_DIR` | path | Override the config directory (default `~/.config/punktfunk`) — pairing state, certs, apps.json, captures. |

## Advanced performance tuning

Leave these at their defaults unless you're chasing latency; see the [troubleshooting](/docs/troubleshooting)
notes for context.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GSO` | `1` · `0` | UDP Generic Segmentation Offload on the send path (coalesce a frame's packets into kernel super-buffers) — the dominant lever above ~1 Gbps. On by default; auto-falls back to `sendmmsg`. Set `0` if a NIC/middlebox mishandles GSO. |
| `PUNKTFUNK_SPLIT_ENCODE` | `0`/`disable` · `1`/`auto` · `2` · `3` | NVENC N-way split-encode for very high pixel rates (5K@240). `auto` picks automatically above ~1 Gpix/s. |
| `PUNKTFUNK_GPU_PRIORITY_CLASS` | `off` · `normal` · `high` · `realtime` | **(Windows)** GPU scheduling priority for capture/encode under a GPU-saturating game. Default `high`; `realtime` is the strongest lever but can freeze NVENC on some setups. |
| `PUNKTFUNK_IDD_DEPTH` | `N` (default `2`) | **(Windows)** IDD-push pipeline depth. `1` cuts latency once GPU priority is raised; higher smooths a contended GPU. |

## Diagnostics

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_PERF` | `1` | Log per-stage timing (capture, encode, send) — handy when tuning latency. |
| `RUST_LOG` | `info` · `debug` · `trace` | Log verbosity. On Windows, logs land in `%ProgramData%\punktfunk\logs\` (size-capped: a file over 10 MB is rotated to `.old` at the next service/host start, one generation kept). |
| `PUNKTFUNK_FFMPEG_DEBUG` | set | Verbose libavcodec/FFmpeg logging from the encoder. |
| `PUNKTFUNK_VIDEO_DROP` | `N` (percent) | Deliberately drop N% of video packets to exercise FEC recovery. **Testing only.** |

## Client-side (native clients)

A few knobs are read by the native **clients**, not the host:

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_DECODER` | `software` · `vaapi` (Linux) | Force the decode path. Default auto-selects hardware (VAAPI on Intel/AMD, D3D11VA on Windows) with a software fallback. |

## Bitrate

The client requests a bitrate; the host encodes to it. There's no host-side bitrate knob. To find a
good value:

- **Native clients (Apple, Linux, Windows, Android):** use the built-in **speed test** (from a
  host's menu). It measures your link, suggests a bitrate, and applies it.
- **Moonlight:** set the bitrate in Moonlight's settings. Start moderate and raise it.

## Multiple devices at once

The native `punktfunk/1` host (`serve`) streams up to **4 sessions at once** by default (an encoder
bound); further clients wait in the accept queue until a slot frees up. Each session gets its own
virtual display at the client's exact resolution, sharing the host's input/audio/mic services. The
limit isn't settable from `serve`'s command line yet — `punktfunk1-host`, the standalone test host,
exposes it as `--max-concurrent N` (see the [Host CLI](/docs/host-cli) reference).

## Codec and FEC

- Client and host **negotiate the codec**: **HEVC (H.265)** by default, **AV1** for clients that
  support it, and **H.264** when the session runs on the GPU-less software encoder.
- The native protocol adds forward error correction for lossy links — see `PUNKTFUNK_FEC_PCT` above.
