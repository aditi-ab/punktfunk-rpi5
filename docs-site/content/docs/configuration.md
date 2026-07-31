---
title: Configuration
description: Every host.env setting and PUNKTFUNK_* environment variable — compositor, video, input, gamepads, tuning — and how to use them.
---

The host reads its settings from **`~/.config/punktfunk/host.env`** (a simple `KEY=value` file, `#`
starts a comment; keys are **case-sensitive** — `punktfunk_compositor` sets nothing, use the exact
uppercase names). On Windows the service reads **`%ProgramData%\punktfunk\host.env`** instead. Your
[setup guide](/docs/requirements) gives you a starting `host.env` for your desktop; this page is the
full reference for every setting.

> **You rarely need most of these.** The host **auto-detects** the compositor, input backend, and
> encoder from your live session — a box that flips between Steam Gaming Mode and a KDE/GNOME desktop
> is followed automatically. The `PUNKTFUNK_*` knobs below are mostly **optional overrides** for
> forcing a specific backend, tuning performance, or debugging. The starter `host.env` for your
> platform sets only the few you actually need.

## Session anchors

**Leave these unset on a normal setup.** Running as a `systemctl --user` service the host inherits
the correct `XDG_RUNTIME_DIR` from systemd, derives the session bus from it, and **rewrites
`WAYLAND_DISPLAY` / `XDG_CURRENT_DESKTOP` / `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` on every
connect** to follow the active session (Gaming ↔ Desktop) — a value written here can only be
redundant or stale.

| Setting | When to set it |
|---|---|
| `XDG_RUNTIME_DIR` | Only when the host runs **outside** a user service (ssh, cron): `/run/user/<your uid>` — check `id -u`. A copy-pasted `1000` on a box where that isn't your uid points the host at another user's (nonexistent) PipeWire/D-Bus, and **everything** fails (audio `Creation failed`, no capture, clients report the host unreachable). |
| `DBUS_SESSION_BUS_ADDRESS` | Same cases only: `unix:path=/run/user/<your uid>/bus`. Otherwise derived automatically. |
| `WAYLAND_DISPLAY` | Only the dedicated [headless-KDE appliance](/docs/kde#headless-session) (`wayland-kde`, set by its shipped `host.env.kde`). |
| `XDG_CURRENT_DESKTOP` | Same — appliance-only. |

## Core

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_COMPOSITOR` | `kwin` · `mutter` · `gamescope` · `wlroots` · `hyprland` (aliases: `kde`/`plasma`, `gnome`, `sway`/`wlr`) | Which backend creates the virtual display. `wlroots` is sway/River; `hyprland` is its own backend. **Leave unset.** Setting it **pins** the backend and turns session-following **off** — per connect *and* mid-stream, so a Desktop ↔ Gaming switch kills the stream instead of being followed. For CI/tests and dedicated single-session appliances only. |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` · `portal` | `virtual` creates a per-client display at the client's exact mode (the normal choice). `portal` captures an existing monitor instead. |
| `PUNKTFUNK_CAPTURE_MONITOR` | a connector name (`HDMI-A-1`, `DP-2`, …) | Stream a **physical** monitor this host already has instead of creating a virtual display — see [Streamed screen](/docs/virtual-displays#stream-a-real-monitor-instead). List the names with `punktfunk-host list-monitors`. Setting it here **outranks the web console's** choice, so an appliance stays aimed where its operator pointed it; leave it unset to steer from the console. A name that matches no monitor fails the session loudly rather than streaming a different screen. Linux only. |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` *(default on)* | GPU zero-copy capture→encode (dmabuf → CUDA → NVENC, or D3D11 on Windows). **On by default** — no need to set it; it falls back to a CPU path automatically. Set `0` to force the CPU path. One exception: Windows **Intel/QSV** keeps the CPU path by default until zero-copy is validated on Intel hardware — set `1` to try it there. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `gamescope` · `wlr` · `uinput` | How input is injected. `libei` for GNOME/KDE, `gamescope` for Bazzite/gamescope, `wlr` for Sway/wlroots **and Hyprland**. Auto-detected with the compositor. |
| `PUNKTFUNK_ENCODER` | `auto` · `nvenc` · `vaapi` (Linux) · `amf` · `qsv` (Windows) · `software` | Encoder backend. `auto` (default) detects the GPU vendor: NVIDIA→NVENC, AMD→VAAPI/AMF, Intel→VAAPI/QSV. `software` (aliases `sw`/`openh264`) is the GPU-less H.264 path on both platforms — on Windows `auto` falls back to it when no GPU is found; on Linux it is **explicit-only** (`auto` never picks it). On a multi-GPU Windows box a forced hardware backend whose vendor contradicts the selected GPU (web-console preference) is **overridden** — the adapter wins and the host logs a warning; remove the stale pin. |
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
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` | **Attach** model: the box owns its gamescope session on its own display (you switch Gaming ↔ Desktop with the Steam UI); the host just captures whatever's live and never tears it down. On a **headless** box the box-owned autologin session is restarted at the client's resolution on a mismatch; a box driving a physical display, and any foreign/bare gamescope, streams at its own mode. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model (the default where session infra is detected): the host takes the box's gamescope over and relaunches it **headless** at the *client's* exact resolution — Game Mode on the virtual screen — restoring the box on idle. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode (headless appliance; no physical session running). |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover + capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_GAMESCOPE_HDR` | `1` · `0` *(default on)* | Allow HDR (10-bit BT.2020 PQ) sessions on the gamescope backend. Needs the `punktfunk-gamescope` build — see [HDR on gamescope](/docs/gamescope#hdr-on-gamescope); without the build, sessions stream SDR. Set `0` to force SDR. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | e.g. `400` | On an HDR gamescope session, the luminance SDR content (desktop, Steam overlay, SDR games) is mapped to inside the PQ container. Unset = gamescope's own default of 400. |
| `PUNKTFUNK_GAMESCOPE_BIN` | path | Force a specific gamescope binary for the sessions the host spawns. Unset = prefer `punktfunk-gamescope` on `PATH`, then `gamescope`. |
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
| `PUNKTFUNK_ON_CONNECT_CMD` | command | Fired (detached) when a client connects, on either plane — the event JSON on stdin plus `PF_EVENT_*` env vars. The zero-config little sibling of [hooks.json](/docs/automation), which adds filters, webhooks, and debounce. |
| `PUNKTFUNK_ON_DISCONNECT_CMD` | command | The `client.disconnected` counterpart of `PUNKTFUNK_ON_CONNECT_CMD` (its `PF_EVENT_REASON` is `quit`, `timeout`, or `error`). |

## Video quality

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_FEC_PCT` | `N` (percent) | Forward-error-correction redundancy for lossy links (the default is sensible for a normal LAN). Higher = more loss-resilient, more bandwidth. |
| `PUNKTFUNK_10BIT` | `1` · `0` *(default on)* | HEVC Main10 / HDR. **On by default** — the host permits 10-bit; a session goes 10-bit only when the client advertises it (behind the client's HDR setting). Set `0` to force 8-bit. Windows host, plus the Linux **GNOME 50+ GameStream desktop mirror** (`PUNKTFUNK_VIDEO_SOURCE=portal`, mirrored monitor in HDR mode — check with `punktfunk-host hdr-probe`). Also the Linux **gamescope** backend with the `punktfunk-gamescope` build (on by default too). The other Linux virtual displays (Mutter, KWin, wlroots) stay 8-bit: their virtual-monitor screencasts are SDR-only upstream. |
| `PUNKTFUNK_444` | `1` · `0` *(default on)* | Full-chroma HEVC 4:4:4 (Range Extensions) — sharper text/desktop, no chroma loss. **On by default** on the host; the client's own 4:4:4 setting (default off) is the real switch. Set `0` to force 4:2:0. **punktfunk/1 native only** (Moonlight stays 4:2:0), HEVC-only, honored only when the client advertises 4:4:4 **and** the GPU supports it (probed; NVENC is the validated path — VAAPI/AMF/QSV decline). Independent of 10-bit. |
| `PUNKTFUNK_CHACHA20` | `1` · `0` *(default on)* | ChaCha20-Poly1305 session encryption for clients without hardware AES (old ARM TVs, e.g. webOS), lifting their ~100 Mbps software-AES decrypt ceiling. **On by default** on the host; a session uses it only when the client requests it — everyone else stays on AES-GCM. Purely a performance choice (both ciphers are full-strength); set `0` to force AES-GCM for all sessions. |
| `PUNKTFUNK_PYROWAVE_MAX_MBPS` | `N` (Mbps) | Cap the [PyroWave](/docs/pyrowave) Automatic bitrate pin, for a host on a link that the open-loop pin can outrun (e.g. 4:4:4 + HDR at 5120×1440@240 pins ~5.3 Gbps, over a 5GbE link). Unset = no cap. Only affects Automatic (bitrate `0`) PyroWave sessions; an explicit client bitrate bypasses it. |
| `PUNKTFUNK_DSCP` | `1` | Opt-in DSCP / `SO_PRIORITY` QoS tagging on the media sockets. No-op on the wire on Windows without a qWAVE policy. |
| `PUNKTFUNK_OH264_THREADS` / `PUNKTFUNK_OH264_GOP` | `N` | Software (openh264) encoder tuning: encode threads (default 2 — latency over throughput) and GOP length (default 0 = encoder-auto). Only relevant with `PUNKTFUNK_ENCODER=software`. |
| `PUNKTFUNK_MAX_FPS` | `N` (fps) *(default: no limit)* | **Frame limiter for the game** — how fast the compositor lets it render. It does *not* cap the stream: the client still negotiates and receives its full rate, because the encode loop re-encodes the held frame whenever the compositor produced no new one (an almost-empty P-frame). A 60-capped game on a 120 Hz session still sends 120 frames a second, and the GPU time the game gives up goes to capture and encode instead — and to heat and battery on a laptop or handheld. **gamescope only today**: it takes this as `--nested-refresh`, the rate it clamps the game to; that is the nested output's rate, so everything gamescope composites moves at it. Other compositors have no equivalent lever and ignore it. |
| `PUNKTFUNK_VDISPLAY_HZ_MULT` | `1`–`4` *(default `1` = off)* | Run the **virtual display** at a multiple of the session's frame rate without sending a single extra frame. A compositor paints on its own vblank, so a frame finished just after the capture sampled waits nearly a whole interval to be picked up — the jittery part of the latency budget. At `2` that worst case halves. Costs the compositor and GPU the extra composites, so it's opt-in. If the backend won't give the multiplied rate it reports what it achieved and the stream paces to that. |

## Gamepads

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMEPAD` | `xbox360` · `xboxone` · `dualsense` · `dualsenseedge` · `dualshock4` · `steamdeck` · `switchpro` · `steamcontroller` · `steamcontroller2` (aliases: `ps5`, `edge`, `ps4`, `deck`, `switch`, `sc2`, `ibex`, …) | The virtual pad the host creates. Usually **auto-resolved from the client's physical controller** — set this only to force a type. `xbox360` (XInput) is the universal fallback. `dualsenseedge` gives the client's back paddles native buttons; `switchpro` gives Nintendo-family pads correct glyphs/layout + gyro. `steamcontroller2` (the 2026 Steam Controller) is passed through **as-is** — the host presents a real SC2 (`28DE:1302`) that Steam Input drives directly, mirroring the physical pad's raw reports (Linux only). DualSense (Edge)/DualShock 4 work on Linux (UHID) and Windows (UMDF); the Steam Deck pad too (Windows via the promoted UMDF identity); Switch Pro and the classic Steam Controller need Linux UHID. Unsupported choices fold to Xbox 360. |
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
| `PUNKTFUNK_EXCLUSIVE_REASSERT_MS` | ms (default `2000`), `0` = off | How often the host re-checks that **exclusive** display topology actually held. Windows (or a GPU driver / display-poller tool) can quietly re-activate a physical panel moments after the host disabled it — seen on hybrid Intel+NVIDIA laptops — putting windows, the cursor, and the lock screen on a screen that isn't streamed. The host re-asserts and logs when that happens; `0` restores the old fire-and-forget behavior. |
| `PUNKTFUNK_RENDER_ADAPTER` | description substring | Multi-GPU boxes only: force the NVENC/capture GPU by adapter Description substring (e.g. `4090`). Leave unset on single-GPU machines. |
| `PUNKTFUNK_HOST_CMD` | e.g. `serve --gamestream` | The host subcommand the service launches. Default `serve --gamestream`; use `serve` for a secure native-only host. |

## Network & discovery

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_HOST_NAME` | free text, e.g. `Living Room` | The name this host shows up under in Moonlight and in the Punktfunk clients. Default: the machine's own hostname — so a box called `bazzite-htpc` can present itself as `Living Room` without renaming the machine. Takes effect on host restart. Spaces and accents are fine; `.` becomes `-` (a dot would split the name in client lists) and it's capped at 63 characters. The machine's real hostname is still what the host answers to on the network. |
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
| `PUNKTFUNK_GSO` | `1` · `0` | UDP Generic Segmentation Offload on the send path (coalesce a frame's packets into kernel super-buffers) — cuts send CPU ~30%, but its line-rate packet trains can cost delivered throughput on constrained links (measured on a 2.5GbE hop). Off by default until send pacing spaces the super-buffers; set `1` to opt in (auto-falls back to `sendmmsg` on kernels/paths without support). |
| `PUNKTFUNK_SPLIT_ENCODE` | `0`/`disable` · `1`/`auto` · `2` · `3` | NVENC N-way split-encode for very high pixel rates (5K@240). `auto` picks automatically above ~1 Gpix/s. H.264 never splits (not applicable per the SDK); on HEVC a *forced* split disables sub-frame readback (mutually unsupported) — set `0` to choose sub-frame instead. |
| `PUNKTFUNK_NVENC_SUBFRAME` | `0` · `1` | NVENC sub-frame (slice-level) readback for lower latency on sync sessions. Default: on where the GPU supports it (Linux direct NVENC). `0` = never; `1` = force. On HEVC it yields to a forced split-encode (the SDK documents the pair unsupported). |
| `PUNKTFUNK_GPU_PRIORITY_CLASS` | `off` · `normal` · `high` · `realtime` · `auto` | **(Windows)** GPU scheduling priority for capture/encode under a GPU-saturating game. Default `auto` (starts `high`, upgrades to `realtime` when it's safe — e.g. HAGS off); `high` pins the static pre-gate behaviour; `realtime` is the strongest lever but can freeze NVENC on some setups. |
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
| `PUNKTFUNK_DECODER` | `software` · `vaapi` · `vulkan` (Linux) · `d3d11va` (Windows) | Force the decode path. Default auto-selects hardware per vendor (Linux: VAAPI on Intel/AMD, Vulkan Video on NVIDIA and the Steam Deck; Windows: Vulkan Video on NVIDIA/AMD, D3D11VA on Intel and others) with a software fallback. |
| `PUNKTFUNK_OSD_SCALE` | multiplier, e.g. `1.5` *(default `1`)* | Size of the in-stream overlay — the stats OSD, the capture hint and the start banner. They already follow your display's scaling setting (200 % display → twice the pixels), so set this only to nudge that: bigger for a TV across the room, smaller if your compositor reports an aggressive scale. Clamped to 0.5×–4×, and a line that would run off the screen is shrunk to fit. |

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
