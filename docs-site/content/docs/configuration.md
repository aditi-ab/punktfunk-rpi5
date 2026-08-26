---
title: Configuration
description: The host.env settings and PUNKTFUNK_* environment variables you'd actually set — compositor, video, audio, input, gamepads, clipboard, tuning — and what each one does.
---

The host reads its settings from **`~/.config/punktfunk/host.env`** (a simple `KEY=value` file, `#`
starts a comment; keys are **case-sensitive** — `punktfunk_compositor` sets nothing, use the exact
uppercase names). On Windows the service reads **`%ProgramData%\punktfunk\host.env`** instead. Your
[setup guide](/docs/requirements) gives you a starting `host.env` for your desktop; this page is the
reference for the settings you set there. A few settings are documented on the page that owns their
feature instead — they're listed under [Settings documented
elsewhere](#settings-documented-elsewhere) at the end.

The file is read when the host starts, so **an edit does nothing until you restart the host.** On
Linux, where the file is loaded by the `punktfunk-host` user service:

```bash
systemctl --user restart punktfunk-host
```

On Windows, where it is loaded by the `PunktfunkHost` service — from an Administrator prompt:

```powershell
punktfunk-host service restart
```

> **You rarely need most of these.** The host **auto-detects** the compositor, input backend, and
> encoder from your live session — a box that flips between Steam Gaming Mode and a KDE/GNOME desktop
> is followed automatically. The `PUNKTFUNK_*` knobs below are mostly **optional overrides** for
> forcing a specific backend, tuning performance, or debugging. The starter `host.env` for your
> platform sets only the few you actually need.

**Finding your way.** The sections below go in this order: what session the host attaches to
(*Session anchors*, *Core*, *gamescope / session following*, *Compositor-specific*, *Session
recovery*), what it streams (*Video quality*, *Gamepads*, *Audio / microphone*, *Clipboard*), the
platform and network bits (*Windows host*, *Network & discovery*, *Auth, API & paths*, *Updates*),
then *Advanced performance tuning* and *Diagnostics*. The last few sections are background rather
than host settings: the handful of variables the **clients** read, bitrate, several devices at once,
and codecs. Two things people come here for are **not** host settings: **resolution and refresh**
are chosen by the client, and so is the **bitrate** — see [Bitrate](#bitrate) near the end.

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
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` (default) · `portal` | **GameStream/Moonlight sessions only** — it has no effect on the native `punktfunk/1` plane. `virtual` creates a per-client display at the client's exact mode (the normal choice); `portal` captures an existing monitor instead, and is what the GNOME 50+ HDR monitor mirror needs — see [HDR](/docs/hdr#linux--gnome). To stream a physical monitor to a Punktfunk app, use `PUNKTFUNK_CAPTURE_MONITOR` below, or the console's **Streamed screen**. |
| `PUNKTFUNK_CAPTURE_MONITOR` | a connector name (`HDMI-A-1`, `DP-2`, …) | Stream a **physical** monitor this host already has instead of creating a virtual display — see [Streamed screen](/docs/virtual-displays#stream-a-real-monitor-instead). List the names with `punktfunk-host list-monitors`. Setting it here **outranks the web console's** choice, so an appliance stays aimed where its operator pointed it; leave it unset to steer from the console. A name that matches no monitor fails the session loudly rather than streaming a different screen. Linux only. |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` *(default on)* | GPU zero-copy capture→encode (dmabuf → CUDA → NVENC, or D3D11 on Windows). **On by default** — no need to set it; it falls back to a CPU path automatically. Set `0` to force the CPU path. One exception: Windows **Intel/QSV** keeps the CPU path by default until zero-copy is validated on Intel hardware — set `1` to try it there. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `kwin` · `gamescope` · `wlr` | How input is injected. `kwin` (KWin fake-input) for KDE — direct injection with no portal approval dialog, so it also works on a headless KDE box; `libei` (the RemoteDesktop portal) for GNOME; `gamescope` for Bazzite/gamescope; `wlr` for Sway/wlroots **and Hyprland**. Auto-detected with the compositor; a value that isn't one of the four is ignored and detection runs anyway. |
| `PUNKTFUNK_PEN` | `1` · `0` *(default on)* | Full-fidelity stylus input — pressure, tilt, hover, eraser, barrel buttons — for the clients that send it. **On by default**; `0` stops the host advertising pen at all and every client folds the stylus back into ordinary touch. The host also needs `/dev/uinput` on Linux (the same `input` group the virtual gamepads use) or Windows 10 1809+. See [Pen and stylus](/docs/input#pen-and-stylus). |
| `PUNKTFUNK_ENCODER` | `auto` · `nvenc` · `vaapi` · `vulkan` (Linux) · `amf` · `qsv` (Windows) · `software` | Encoder backend. `auto` (default) detects the GPU vendor: NVIDIA→NVENC; on **Linux** AMD/Intel→**Vulkan Video** for HEVC and AV1 (falling back to VAAPI when the device or the codec can't take it — H.264 is always VAAPI), on **Windows** AMD→AMF and Intel→QSV. `software` (aliases `sw`/`openh264`) is the GPU-less H.264 path on both platforms — on Windows `auto` falls back to it when no GPU is found; on Linux it is **explicit-only** (`auto` never picks it). On a multi-GPU Windows box a forced hardware backend whose vendor contradicts the selected GPU (web-console preference) is **overridden** — the adapter wins and the host logs a warning; remove the stale pin. |
| `PUNKTFUNK_VULKAN_ENCODE` | `1` · `0` *(default on)* | **(Linux, AMD/Intel)** Use the Vulkan Video encoder for HEVC/AV1 sessions. **On by default** — it recovers from packet loss without a full keyframe, which the VAAPI path can't express. `0` pins the libav VAAPI path; so does a device that can't encode the profile (the host falls back on its own). See [Requirements](/docs/requirements). |
| `PUNKTFUNK_VAAPI_LOW_POWER` | `1` · `0` | **(Linux, Intel)** Pin the VAAPI entrypoint. Modern Intel (Gen12/Tiger Lake and newer, incl. Arc) only offers the low-power (VDEnc) entrypoint and the host detects that by itself; set this only to force one way or the other. See [Requirements](/docs/requirements). |
| `PUNKTFUNK_RENDER_NODE` | path | Linux DRM render node for zero-copy (default `/dev/dri/renderD128`). Set on multi-GPU boxes to pick the right GPU. Superseded by a manual GPU preference in the console — see below. |

> **Picking a GPU** — on a multi-GPU box, choose the GPU in the **web console** (Host → *GPUs*),
> which writes `gpu-settings.json`. A **manual** preference there outranks both
> `PUNKTFUNK_RENDER_NODE` (Linux) and `PUNKTFUNK_RENDER_ADAPTER` (Windows); while the console is
> left on **Automatic** — or the preferred GPU isn't present — those two still decide. They stay
> useful on a headless/appliance box nobody opens the console on.

Resolution and refresh are **not** set here — **the client chooses them.** When a device connects,
the host creates a virtual display at that device's resolution and refresh rate. A 1080p60 laptop and
a 1440p120 desktop each get their own. (With Moonlight, set the mode in Moonlight; the native clients
let you pick a mode or default to the device's display.)

## gamescope / session following (Linux, Bazzite/SteamOS)

Two mutually-exclusive models for a Steam/gamescope box. See [Steam / gamescope](/docs/gamescope) for
the full picture (and [Bazzite](/docs/bazzite) for that distro's specifics).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` · `0` *(unset = auto)* | **Attach** model: the box owns its gamescope session on its own display (you switch Gaming ↔ Desktop with the Steam UI); the host just captures whatever's live and never tears it down. On a **headless** box the box-owned autologin session is restarted at the client's resolution on a mismatch; a box driving a physical display, and any foreign/bare gamescope, streams at its own mode — i.e. the client is served a **mirror**, not a display of its own. Setting this also outranks a dedicated game session. No template ships it set; `=0` is the same as leaving it out. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model (the default where session infra is detected): the host takes the box's gamescope over and relaunches it **headless** at the *client's* exact resolution — Game Mode on the virtual screen — restoring the box on idle. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode (headless appliance; no physical session running). |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover + capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_GAMESCOPE_HDR` | `1` · `0` *(default on)* | Allow HDR (10-bit BT.2020 PQ) sessions on the gamescope backend. Needs the `punktfunk-gamescope` build — see [HDR on gamescope](/docs/gamescope#hdr-on-gamescope); without the build, sessions stream SDR. Set `0` to force SDR. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | e.g. `400` | On an HDR gamescope session, the luminance SDR content (desktop, Steam overlay, SDR games) is mapped to inside the PQ container. Unset = 203, BT.2408 reference white, which is what our clients decode against (gamescope's own default of 400 sits nearly a stop above it). |
| `PUNKTFUNK_GAMESCOPE_BIN` | path | Force a specific gamescope binary for the sessions the host spawns. Unset = prefer `punktfunk-gamescope` on `PATH`, then `gamescope`. |
| `PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR` | path | Directory holding our Vulkan WSI layer's manifest — the layer that lets a game nested under gamescope get an HDR10 swapchain. Unset = `/usr/lib/punktfunk/vulkan/implicit_layer.d`, where every distro package installs it. The NixOS module sets this for you, since the layer lives inside the gamescope derivation there. If no manifest is found the host leaves the system's own layer alone and games stay SDR. |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch **mid-stream** (rebuild the backend in place, no reconnect). **On by default** on Bazzite/SteamOS; set `0` to disable. |
| `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` | `1` | Add `--force-grab-cursor` to a bare gamescope session the host spawns **to run an app or game** (never the empty keep-alive session), forcing relative-mouse capture so FPS mouselook works over the injected pointer. **Off by default** — relative mode breaks absolute-pointer titles and menus, so turn it on per host. |
| `PUNKTFUNK_GAMESCOPE_SPLASH` | `1` · `0` *(default on)* | Run the built-in splash client inside each bare gamescope session the host spawns. **Leave it on**: gamescope only produces capture buffers once something paints, and a Steam launch paints nothing for its whole bootstrap — without the splash a fresh session starves and times out. `0` is a debugging escape hatch. |
| `PUNKTFUNK_GAMESCOPE_STEAM` | `1` | Launch every bare gamescope session the host spawns in Steam integration mode (`--steam`). A Steam title turns that on by itself; this forces it for non-Steam launches too. Managed / `gamescope-session-plus` sessions own their own flags and ignore it. |

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
| `PUNKTFUNK_PORTAL_CURSOR_MODE` | `auto` *(default)* · `embedded` · `metadata` · `hidden` | **Hyprland / wlroots only, and a troubleshooting knob** — which ScreenCast cursor mode the host asks the portal for. Unset, the host asks for `metadata` when the client draws the pointer itself and `embedded` otherwise, then settles that against the modes your portal advertises; it never requests one your portal lacks. Set `embedded` if the pointer misbehaves on a portal that *claims* metadata support but implements it poorly — that is the one case the automatic negotiation cannot detect. A pin is still only a preference: it is checked against the advertised modes like any other. |

## Session recovery (Linux)

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_RECOVER_SESSION_CMD` | command | Operator hook fired (debounced) when a client connects while **no graphical session is live** for the host's user — the state a compositor crash leaves behind (gnome-shell SIGSEGV → GDM greeter, whose auto-login is once-per-boot). Typically `sudo -n systemctl restart gdm` with a matching NOPASSWD sudoers rule, or `systemctl restart display-manager` under a polkit rule; with auto-login enabled the restart brings the desktop back and the client's automatic retry lands in it. Unset/empty = disabled (the default). |
| `PUNKTFUNK_ON_CONNECT_CMD` | command | Fired (detached) when a client connects, on either plane — the event JSON on stdin plus `PF_EVENT_*` env vars. The zero-config little sibling of [hooks.json](/docs/automation), which adds filters, webhooks, and debounce. |
| `PUNKTFUNK_ON_DISCONNECT_CMD` | command | The `client.disconnected` counterpart of `PUNKTFUNK_ON_CONNECT_CMD` (its `PF_EVENT_REASON` is `quit`, `timeout`, or `error`). |

## Video quality

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_FEC_PCT` | `0`–`90` (percent) | **Pins** forward-error-correction redundancy and turns adaptive FEC **off**. Leave it unset on the native protocol: the host normally sizes recovery to the loss the client reports (a 1–50 % band, starting at 10 %), so pinning a number can leave a lossy link *worse* off than letting it adapt. Set it only when a fixed, known overhead matters — a measurement or a speed test; `0` disables FEC entirely. On the GameStream/Moonlight plane it is a plain override of that plane's fixed 20 %. |
| `PUNKTFUNK_10BIT` | `1` · `0` *(default on)* | Allow 10-bit (HEVC Main10 / AV1 10-bit) sessions at all; `0` forces every session to 8-bit SDR. Which hosts can actually deliver it, and the client half of the switch, are on [HDR](/docs/hdr). |
| `PUNKTFUNK_444` | `1` · `0` *(default on)* | Host **policy gate** for full chroma 4:4:4 — sharper text and thin lines, no chroma loss. **On by default**; `0` forces every session to 4:2:0. It only ever *allows*: the client's own 4:4:4 setting (default off) is the real per-session switch, and the codec, capture-path and GPU gates behind it are on [Client settings → Full chroma](/docs/client-settings#video). Which GPUs and which clients can actually do it is in the [support matrix](/docs/support-matrix#encoders); how it interacts with HDR is on [HDR](/docs/hdr). **punktfunk/1 native only** — Moonlight stays 4:2:0. |
| `PUNKTFUNK_CHACHA20` | `1` · `0` *(default on)* | ChaCha20-Poly1305 session encryption for clients without hardware AES (old ARM TVs, e.g. webOS), lifting their ~100 Mbps software-AES decrypt ceiling. **On by default** on the host; a session uses it only when the client requests it — everyone else stays on AES-GCM. Purely a performance choice (both ciphers are full-strength); set `0` to force AES-GCM for all sessions. |
| `PUNKTFUNK_PYROWAVE_MAX_MBPS` | `N` (Mbps) | Cap the [PyroWave](/docs/pyrowave) Automatic bitrate pin, for a host on a link that the open-loop pin can outrun (e.g. 4:4:4 + HDR at 5120×1440@240 pins ~5.3 Gbps, over a 5GbE link). Unset = no cap. Applies to every PyroWave session — a client-requested bitrate is treated as Automatic under PyroWave, so nothing bypasses the ceiling. |
| `PUNKTFUNK_DSCP` | `1` | Opt-in DSCP / `SO_PRIORITY` QoS tagging on the media sockets. No-op on the wire on Windows without a qWAVE policy. |
| `PUNKTFUNK_OH264_THREADS` / `PUNKTFUNK_OH264_GOP` | `N` | Software (openh264) encoder tuning: encode threads (default 2 — latency over throughput) and GOP length in frames (unset = about ten minutes' worth, `fps × 600`; set `0` for encoder-auto). Only relevant with `PUNKTFUNK_ENCODER=software`. |
| `PUNKTFUNK_MAX_FPS` | `N` (fps) *(default: no limit)* | **Frame limiter for the game** — how fast the compositor lets it render. It does *not* cap the stream: the client still negotiates and receives its full rate, because the encode loop re-encodes the held frame whenever the compositor produced no new one (an almost-empty P-frame). A 60-capped game on a 120 Hz session still sends 120 frames a second, and the GPU time the game gives up goes to capture and encode instead — and to heat and battery on a laptop or handheld. **gamescope only today**: it takes this as `--nested-refresh`, the rate it clamps the game to; that is the nested output's rate, so everything gamescope composites moves at it. Other compositors have no equivalent lever and ignore it. ⚠️ On gamescope that one number is also the refresh the session **reports**: Steam's in-session display settings and every game will read the display as `N` Hz, and a game that paces itself to the display will hold itself there. If you want a quieter box without games believing the panel changed, cap the client's requested refresh instead. |
| `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` | e.g. `60,90,120` *(default: just the session's own rate)* | Extra refresh rates a gamescope session **offers** in its in-session display settings. A headless gamescope has no EDID, so it cannot work out what else the display could run at — without this it advertises exactly one rate and Steam's refresh menu has a single entry. The rate the session actually runs at is always included, so this can only add options. Needs the `punktfunk-gamescope` build (`+pfhdr3`); ignored on a stock gamescope, which has no flag to take it. |
| `PUNKTFUNK_VDISPLAY_HZ_MULT` | `1`–`4` *(default `1` = off)* | Run the **virtual display** at a multiple of the session's frame rate without sending a single extra frame. A compositor paints on its own vblank, so a frame finished just after the capture sampled waits nearly a whole interval to be picked up — the jittery part of the latency budget. At `2` that worst case halves. Costs the compositor and GPU the extra composites, so it's opt-in. If the backend won't give the multiplied rate it reports what it achieved and the stream paces to that. |

## Gamepads

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMEPAD` | `xbox360` · `xboxone` · `dualsense` · `dualsenseedge` · `dualshock4` · `steamdeck` · `switchpro` · `steamcontroller` · `steamcontroller2` (aliases: `ps5`, `edge`, `ps4`, `deck`, `switch`, `sc2`, `ibex`, …) | The virtual pad the host creates. Usually **auto-resolved from the client's physical controller** — set this only to force a type. `xbox360` (XInput) is the universal fallback. `dualsenseedge` gives the client's back paddles native buttons; `switchpro` gives Nintendo-family pads correct glyphs/layout + gyro. `steamcontroller2` (the 2026 Steam Controller) is passed through **as-is** — the host presents a real SC2 (`28DE:1302`) that Steam Input drives directly, mirroring the physical pad's raw reports (Linux only). DualSense (Edge)/DualShock 4 work on Linux (UHID) and Windows (UMDF); the Steam Deck pad too (Windows via the promoted UMDF identity); Switch Pro and the classic Steam Controller need Linux UHID. Unsupported choices fold to Xbox 360. |
| `PUNKTFUNK_STEAM_GADGET` | `1` · `0` | Force the raw USB-gadget virtual Steam Deck on/off. **On by default on SteamOS**, off elsewhere. Lets Steam promote the virtual Deck to full Steam Input. |
| `PUNKTFUNK_DUALSENSE_USBIP` | `1` · `0` *(default off)* | **(Linux, experimental)** Present the virtual DualSense as a **real USB device** over `vhci_hcd`, carrying its own USB Audio Class sound card, instead of as a UHID device. This is what lets a libScePad-style title pair the pad with its own speaker: wine derives a Windows ContainerId by walking sysfs to a `usb_device` parent, which a UHID pad does not have, so on the default path the pad and its speaker both register as `GUID_NULL` and the game never opens the haptic stream. It also gives GE-Proton the real ALSA card its raw-`snd_pcm_open` haptic path scans for. With this on, the pad's audio is captured from its isochronous endpoint and **no PipeWire sinks are minted** — PipeWire builds the real ones from the card. Needs `vhci_hcd` loaded and the `punktfunk` group's write on its sysfs `attach` (both shipped by packaging); degrades to UHID otherwise. |
| `PUNKTFUNK_PAD_AUDIO` | `1` · `0` *(default on)* | Controller audio: what a game plays through the DualSense's built-in speaker and voice-coil haptics is streamed to the client's physical pad as its own low-latency plane. On by default and free while idle — silence is never encoded or sent; `0` turns it off host-wide. On Windows the pad's audio device is a pre-provisioned virtual endpoint; on Linux it is a per-pad PipeWire sink minted with the DualSense identity games match on — see [Controller speaker and haptics](/docs/controller-audio). |
| `PUNKTFUNK_PAD_AUDIO_SLOTS` | `1`–`4` *(default: Windows `1`, Linux `4`)* | How many controllers can have their own audio at once. On Windows each slot is a pre-provisioned virtual endpoint, so the default stays at one; a Linux sink is minted lazily and costs nothing idle, so every slot is on. |
| `PUNKTFUNK_PAD_SINK_NAME` / `PUNKTFUNK_PAD_SINK_DESC` | templates | **(Linux, field debugging)** Override the minted pad sink's `node.name` / `node.description`. `{pad}` and `{mac}` expand per pad. Only for chasing a title whose device matcher wants different strings — the defaults carry every known match surface. |
| `PUNKTFUNK_PAD_SINK_SPLIT_NAME` | node name · `0` *(default: the sink's own name)* | **(Linux, field debugging)** The `api.alsa.split.name` the pad sink advertises. GE-Proton opens that node as `pipewire:NODE=…` with AUX channels for its preferred haptic path; on a real pad it names the hidden 4-channel parent behind the mono speaker split, and our sink has no split, so it names itself. `0` drops the key, which pushes GE onto its Pulse-routed leg instead. |

## Audio / microphone

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_AUDIO_QUALITY` | `low` · `standard` · `high` *(default `high`)* | Desktop-audio encode quality. `high` (stereo 256 kbps Opus, effectively transparent) costs about 1 % of a normal video bitrate, so there's rarely a reason to go lower. `standard` is exactly the pre-0.25 encoder (stereo 128 kbps) — handy for an A/B comparison; `low` is for genuinely constrained links (noticeably lossy on music, still fine for game audio and voice). A typo warns in the log and keeps `high` rather than silently downgrading. Host-side only — clients play whatever arrives, no client setting involved. |
| `PUNKTFUNK_AUDIO_REDUNDANCY` | `1` · `0` *(default: automatic)* | Send audio packets redundantly so a lossy link doesn't crackle. Leave it unset: the host turns redundancy on by itself, only toward clients that support it and only while the link is actually losing packets. `1` forces it on for the whole session, `0` never sends it. |
| `PUNKTFUNK_AUDIO_HIRES` | `0` · `1` *(default: allowed)* | Whether this host will serve the **lossless** audio plane — uncompressed PCM (44.1 / 48 / 88.2 / 96 / 176.4 kHz, 16 or 24-bit, stereo through 7.1) instead of Opus. **You do not need to set this** — since 0.30 the host allows it and the *client's* audio-format setting is the opt-in, which is the switch belonging to the person whose bandwidth it spends. `0` refuses the plane on this host no matter what any client asks for. It was an operator opt-in until 2026-08-17, and what that produced was users picking "Lossless 96 kHz / 24-bit" in a client, silently getting Opus, and the reason existing only as one `INFO` line in the host's journal. What still protects the link is mechanical rather than a pre-agreement: the plane costs **1.4–8.5 Mbps** in stereo (up to 33.9 for 176.4 kHz/24-bit 7.1) against Opus's 256 kbps, it rides QUIC datagrams outside the adaptive-bitrate loop — off the top of the link, where ABR can neither see it nor claw it back — and so a session gets it only if the cost fits **a quarter of that session's video bitrate**. A 5 Mbps session can afford no rung of it at all. Be clear about what it buys: on game content it is very unlikely to be *audible* (256 kbps Opus is already effectively transparent, and nothing above 24 kHz is hearable at all), so the real win is **bit-exactness** — no lossy stage anywhere, and no resample for a host whose interface genuinely runs at 96 kHz. If any condition fails — the client didn't ask, this variable is `0`, the capture device can't genuinely deliver the rate, the link can't spare the bandwidth, or one frame of that format won't fit a datagram at that channel count — the session quietly stays on Opus and the host log names which one lost. ⚠️ **The desktop clients read a variable of this same name with a richer grammar** (see [Client-side](#client-side-native-clients) below), so on a box that is both host and client, one line configures both ends. `0` is now the interesting shared spelling, and it means *off* to each of them; this host gate reads anything that isn't `0`/`false`/`off`/`no` as *allow*, so a client-style `96000/24` leaves this half permissive. |
| `PUNKTFUNK_AUDIO_GAIN` | float (default `1.0`) | Gain applied to captured desktop audio — bump it for a quiet source. Applies to **both** the native `punktfunk/1` and Moonlight/GameStream paths. Peaks are rounded off by a soft limiter rather than clipped, so a boost distorts gracefully instead of abruptly; values above `8.0` (+18 dB) are capped, and a non-positive value is ignored. Note this buys **headroom, not loudness** — it cannot make a desktop mix as loud as already-limited streaming-app audio, and pushing it hard to try will audibly squash the signal. On Windows this is the only host-side control that works at all: loopback capture is tapped upstream of the endpoint's master volume, so the speaker slider does not affect what a client receives. |
| `PUNKTFUNK_STREAM_SINK` | *(unset — a host-owned virtual output)* · `stream` · `0` | **(Linux)** Where desktop audio is captured from. **Leave it unset.** The host creates its own virtual output — "Punktfunk Stream Speaker" — makes it the default while a session runs, and records that, so capture never depends on your speakers existing, on HDMI audio surviving a mode change, or on anything else the desktop does with its output devices; and because the host declares that output's format, a game can render real 5.1/7.1 into it even when this box's own hardware is stereo. Since 0.31 the output is a **real** PipeWire node (a `null-audio-sink`) rather than the capture stream wearing a sink's clothes, and the difference is which clock the graph runs on: a stream cannot drive, so PipeWire had to borrow a clock from whatever sound card happened to be running on the box. On one host that was a controller's sound card attached over the network, and **15 % of the audio that user heard was silence the host invented** over the gaps it left. `stream` restores the 0.30 arrangement for one release (a field A/B without a rebuild); `0` records whatever your current default output is playing instead — which follows the default around, so it hiccups every time that changes. While a session runs you will see both the output and a recording stream named `punktfunk-audio-…` in pavucontrol or KDE's audio settings: that is the capture, not a leak. The host log names the live topology (`desktop audio capture topology mode=…`) and, on every change, which node is clocking it (`audio capture graph driver`). |
| `PUNKTFUNK_MIC_DEVICE` | name substring | **(Windows)** Target mic-uplink device by friendly-name substring (first match wins). |
| `PUNKTFUNK_MIC_LEGACY_BUFFER` | `1` | Restore the fixed pre-adaptive mic buffering (a ~48 ms prime and ~120 ms cap on Windows; a buffer scaled to the recording app's audio quantum on Linux) instead of the adaptive per-client jitter target. One-release escape hatch: if the microphone coming out of the host only sounds right *with* this set, that's a bug — please report it. |
| `PUNKTFUNK_NO_MIC_INSTALL` | set | **(Windows)** Skip installing the virtual-mic driver (e.g. when the host runs as SYSTEM). |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `client_only` *(default)* · `host_and_client` · `follow_default` | **(Windows)** Where desktop audio is audible while a stream runs. `client_only` parks playback on a silent endpoint so sound comes out of the *client* only — that's why the PC goes quiet when a stream starts; everything is put back when it ends. `host_and_client` prefers a real output device, so the host's speakers keep playing too. `follow_default` never touches your default devices at all — the host just captures whatever your default playback device is (the mic uplink still picks a target device; you may have to select it yourself). A misspelled value warns in the log and uses `client_only`. The pre-0.25 flags `PUNKTFUNK_HOST_AUDIO=1` and `PUNKTFUNK_KEEP_DEFAULT=1` still work as aliases for the last two; `follow_default` wins if both are set. |
| `PUNKTFUNK_NO_AUDIO_MINT` | set | **(Windows)** Don't provision the host's own dedicated virtual audio endpoints at startup (they're minted from Steam's streaming-audio driver where it's installed, and give capture a stable target that renaming or unplugging hardware can't break). With this set — or whenever minting isn't possible — the host picks devices by name instead, exactly as before 0.25. |

## Clipboard

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_CLIPBOARD` | `off` *(default)* · `on`/`1` · `text-only` | Share the clipboard between client and host. `on` allows text, HTML/RTF and images **plus file transfer**; `text-only` (alias `no-files`) allows the text and image formats but refuses files. |

This line is only half the switch — your client has a per-host toggle that also has to be on, and
the host needs a clipboard backend underneath. Both, and what a greyed-out toggle means, are on
[Shared clipboard](/docs/clipboard).

## Windows host

Capture of the **secure desktop** — UAC prompts, the lock screen, the login screen — is always on
and has no setting: the host reads the pf-vdisplay driver's ring directly, and those surfaces are in
it. If an older `host.env` on your machine still carries a `PUNKTFUNK_SECURE_DDA` line, nothing reads
it — leave it or delete it, it makes no difference.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_VDISPLAY` | `pf` | Virtual-display backend. The bundled pf-vdisplay IddCx driver is the only backend now — informational; leave as `pf`. |
| `PUNKTFUNK_MONITOR_LINGER_MS` | ms (default `10000`) | Defer tearing a per-client virtual display down after disconnect. A reconnect inside the window preempts it and creates a fresh one (a reused IddCx swap-chain is dead); the stable per-client monitor id keeps Windows' saved display config applying either way. Superseded by the console's **Keep alive** setting — see [Virtual displays](/docs/virtual-displays). |
| `PUNKTFUNK_EXCLUSIVE_REASSERT_MS` | ms (default `2000`), `0` = off | How often the host re-checks that **exclusive** display topology actually held. Windows (or a GPU driver / display-poller tool) can quietly re-activate a physical panel moments after the host disabled it — seen on hybrid Intel+NVIDIA laptops — putting windows, the cursor, and the lock screen on a screen that isn't streamed. The host re-asserts and logs when that happens; `0` restores the old fire-and-forget behavior. |
| `PUNKTFUNK_RENDER_ADAPTER` | description substring | Multi-GPU boxes only: force the NVENC/capture GPU by adapter Description substring (e.g. `4090`). Leave unset on single-GPU machines. Superseded by a manual GPU preference in the console — see the *Picking a GPU* note under [Core](/docs/configuration#core); it still decides while the console is on Automatic. |
| `PUNKTFUNK_NO_ISOLATE` | set | Legacy topology knob: leave the virtual display **extended** alongside your physical monitors instead of making it the sole desktop. Superseded by the console's **Topology** setting — see [Virtual displays](/docs/virtual-displays). |
| `PUNKTFUNK_HOST_CMD` | `serve` · `serve --gamestream` | The host subcommand the service launches. A fresh install from the setup .exe writes **`serve`** — the secure, native-only host. `serve --gamestream` adds the Moonlight-compat planes; turn that on with `punktfunk-host service install --gamestream=on` (and `=off` to go back) rather than editing this by hand. If the line is missing entirely the service falls back to `serve --gamestream`. |

## Network & discovery

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_HOST_NAME` | free text, e.g. `Living Room` | The name this host shows up under in Moonlight and in the Punktfunk clients. Default: the machine's own hostname — so a box called `bazzite-htpc` can present itself as `Living Room` without renaming the machine. Takes effect on host restart. Spaces and accents are fine; `.` becomes `-` (a dot would split the name in client lists) and it's capped at 63 characters. The machine's real hostname is still what the host answers to on the network. |
| `PUNKTFUNK_MDNS` | `1` · `0` *(default on)* | mDNS adverts (native + GameStream). `0` skips them (same as `--no-mdns`) — for networks/containers where multicast doesn't work; add the host by address in the client instead. |
| `PUNKTFUNK_NATIVE_PORT` | port *(default: `9777`)* | The native punktfunk/1 (QUIC) control port clients connect on — same as `serve --native-port`, which overrides it. Clients discover the port over mDNS, and a host you added by hand keeps whatever port you added it with, so moving this needs no change on the client. A value that isn't a port is a startup error rather than a silent fall back to 9777. |
| `PUNKTFUNK_DATA_PORT` | port | Pin the per-session video data plane to a fixed UDP port and stream direct (no hole-punch) — open exactly that port in the host firewall. Same as `serve --data-port`; see [Troubleshooting](/docs/troubleshooting). Default: random port + hole-punch. |
| `PUNKTFUNK_IDLE_TIMEOUT_MS` | ms (default `8000`) | How long the host waits before declaring a client that vanished (cable pulled, Wi-Fi dropped) gone — which is when a kept virtual display starts its linger. Lower it (e.g. `3000`) to reclaim displays sooner; it's clamped to ≥1 s and the keep-alive scales with it, so a live session never false-disconnects. A deliberate quit is instant regardless. Same as `--idle-timeout-ms` on `punktfunk1-host`. |
| `PUNKTFUNK_JUMBO` | `1` | Stream in **jumbo frames** — ~9000-byte packets instead of the standard ~1500-byte ones, so a high-bitrate session spends less CPU and per-packet overhead on a wired LAN. Off by default, and safe to turn on: see the note below the table. |
| `PUNKTFUNK_WIRE_MTU` | on-wire IP MTU, e.g. `9000` | The pick-your-own-number version of the same switch — and also the escape hatch for **small**-MTU links. A value above 1500 enables jumbo frames with your number as the target (and outranks `PUNKTFUNK_JUMBO`); a value *below* 1500 shrinks every session's packets from the start, for a path that can't carry full-size ones (a VPN or tunnel — the host normally learns this by itself, but the override skips the one degraded first session). Use the on-wire IP MTU your NIC reports (`ip link` on Linux, `netsh interface ipv4 show subinterfaces` on Windows) — IP/UDP overheads are subtracted for you. |

> **Jumbo frames** need every hop to carry them, and the host verifies rather than trusts.
> Sessions still *start* on standard-size packets; with the opt-in set, the host probes the path,
> and only once the probe proves it — and the client acknowledges the switch — does the stream
> grow to the large packets, mid-session, with no reconnect. A path that can't take them, or an
> older client, simply stays at the standard size; the only cost of leaving the opt-in on is a few
> extra probe packets at connect. Two things it can't do for you: the host NIC, the client NIC and
> every switch in between must have jumbo frames enabled in *their* settings first (usually a
> field called MTU, set to `9000`) — and both ends need Punktfunk 0.25 or newer. Native
> `punktfunk/1` sessions only; Moonlight sessions always use standard packets.

## Auth, API & paths

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_MGMT_TOKEN` | token | Bearer token for the management API. If unset it's auto-generated and persisted to `~/.config/punktfunk/mgmt-token` (the bundled web console sources it). Set only to pin a specific token. |
| `PUNKTFUNK_UI_PASSWORD` | password | Web-console login password. Normally generated on first start and stored in `~/.config/punktfunk/web-password` — see [Forgot your Password?](/docs/forgot-password). |
| `PUNKTFUNK_PLUGIN_TOKEN` | token | The scoped token the [plugin/scripting runner](/docs/plugins) uses — a narrower credential than `PUNKTFUNK_MGMT_TOKEN`, never full admin. Same precedence: if unset it's generated and persisted to `~/.config/punktfunk/plugin-token`. Set only to pin a specific token. |
| `PUNKTFUNK_MGMT_BIND` | `IP:PORT` *(default: `0.0.0.0:47990`)* | Where the management API listens. The `--mgmt-bind` flag overrides it. Two reasons to set it: pin `127.0.0.1:47990` to keep the API off the LAN entirely (paired clients then can't browse your library), or **move the port to share the machine with Sunshine, Apollo or Vibeshine** — 47990 is their web UI as well as our management API, and it's the only port the two still share once GameStream compat is off. Everything downstream follows the port you pick: native clients learn it from discovery, and the web console, the plugin runner (and so every library plugin) and the status tray read it from `~/.config/punktfunk/mgmt-endpoint` (`%ProgramData%\punktfunk\mgmt-endpoint` on Windows), which the host writes on every start. See [another streaming host is installed](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed). |
| `PUNKTFUNK_CONFIG_DIR` | path | Override the config directory (default `~/.config/punktfunk`) — pairing state, certs, apps.json, captures. |
| `PUNKTFUNK_UI_PLUGIN_PORT` | port *(default: console port + 1)* | The separate port [plugin](/docs/plugins) UIs are served from. They get their own origin on purpose — a plugin page can never act as *you* on the console. If the console log says this port couldn't be opened (plugin UIs then stay disabled rather than sharing the console's origin), point it at a free port and restart. |
| `PUNKTFUNK_LIBRARY_ART_ROOTS` | directories, separated like `PATH` (`;` on Windows, `:` on Linux/macOS) | Where the host is allowed to read game artwork from when serving your library. Defaults to sensible platform roots: your home directory on Linux/macOS, and on Windows the users base (`C:\Users`) plus your Steam and Playnite installs, wherever they are — including a portable Playnite on another drive, which keeps its covers next to the program. Set it when box art lives somewhere else again — a second drive, a network mount, or a launcher installed outside all of those. Setting it **replaces** the defaults, so list every root you need. The host log's "dropped local art the proxy may not serve" line is this knob's cue: those entries still appear in your library, but their covers stay blank until the root is allowed. |

## Updates

The host checks for a newer release and, where the platform allows it, can install it from the web
console. Both halves have a kill switch — see [Updating the Host](/docs/updating).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_UPDATE_CHECK` | `0` · `false` · `off` | Never contact the update feed. The console's Updates card then shows checks as disabled; everything else keeps working. |
| `PUNKTFUNK_UPDATE_APPLY` | `0` · `false` · `off` | Keep the check but remove the **Update now** button, so the console only ever tells you the command to run. |

## Advanced performance tuning

Leave these at their defaults unless you're chasing latency; see the [troubleshooting](/docs/troubleshooting)
notes for context.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GSO` | `1` · `0` | UDP segmentation offload on the send path (coalesce a frame's packets into kernel super-buffers) — cuts send CPU ~30%, but its line-rate packet trains can cost delivered throughput on constrained links (measured on a 2.5GbE hop). The default differs by platform. **Windows: on by default** (Send Offload — the lever that gets past ~1 Gbps, since Windows otherwise does one send call per packet); set `0` if a constrained link shows lost throughput. It also latches itself off for the rest of the run the first time the OS/NIC/path rejects an offloaded send. **Linux: off by default** until send pacing spaces the super-buffers; set `1` to opt in (auto-falls back to `sendmmsg` on kernels/paths without support). |
| `PUNKTFUNK_SPLIT_ENCODE` | `0`/`disable` · `1`/`auto` · `2` · `3` | NVENC N-way split-encode for very high pixel rates (5K@240). `auto` picks automatically above ~1 Gpix/s. H.264 never splits (not applicable per the SDK); on HEVC a *forced* split disables sub-frame readback (mutually unsupported) — set `0` to choose sub-frame instead. |
| `PUNKTFUNK_NVENC_SUBFRAME` | `0` · `1` | NVENC sub-frame (slice-level) readback for lower latency on sync sessions. Default: on where the GPU supports it (Linux direct NVENC). `0` = never; `1` = force. On HEVC it yields to a forced split-encode (the SDK documents the pair unsupported). |
| `PUNKTFUNK_NVENC_SPLIT_ARBITRATE` | `1` | Opt-in: let the host change its split-encode decision **live**, mid-session, as the pixel rate moves, instead of only choosing once at session start. Currently wired on the Linux direct-NVENC path. Only interesting alongside `PUNKTFUNK_SPLIT_ENCODE=auto` at very high pixel rates. |
| `PUNKTFUNK_GPU_PRIORITY_CLASS` | `off` · `normal` · `high` · `realtime` · `auto` | **(Windows)** GPU scheduling priority for capture/encode under a GPU-saturating game. Default `auto` (starts `high`, upgrades to `realtime` when it's safe — e.g. HAGS off); `high` pins the static pre-gate behaviour; `realtime` is the strongest lever but can freeze NVENC on some setups. |
| `PUNKTFUNK_IDD_DEPTH` | `N` (default `2`) | **(Windows)** IDD-push pipeline depth. `1` cuts latency once GPU priority is raised; higher smooths a contended GPU. |
| `PYROWAVE_QUEUE_PRIORITY` | `realtime` *(default)* · `high` · `off` | [PyroWave](/docs/pyrowave) sessions only — the *intent*, forwarded to whichever process does the encode. PyroWave encodes on the same GPU shader cores a game uses, so a demanding game can starve it and the frame rate drops. This asks the driver to schedule the encode ahead of the game. `realtime` tries the strongest class and falls back to `high`; `high` asks only for the middle one; `off` disables the request. A driver that refuses simply encodes at normal priority — it can never stop a session starting. Granting the request needs the `CAP_SYS_NICE` capability, which the Linux packages give to `punktfunk-encode-worker` and **never** to `punktfunk-host` — a host holding any capability cannot be identified by KWin and loses desktop streaming entirely. Do not `setcap` the host to "make this work"; see [Running as a service](/docs/running-as-a-service#gpu-scheduling-priority). Set `off` if you see the desktop stutter while streaming. |
| `PUNKTFUNK_ENCODE_WORKER` | path · `off` | Where the host looks for `punktfunk-encode-worker`, the small capability-carrying helper that owns the priority-elevated [PyroWave](/docs/pyrowave) encode (previous row). Unset, the host looks beside its own binary and then on `PATH`, which is right for every package — set it only when the worker lives somewhere unusual. **NixOS needs it and the module sets it for you:** a file capability cannot live on a read-only nix store path, so the worker is exposed through `security.wrappers` and this points the host at that wrapper. `off` forces the encode back into the host process at default priority — a debug escape hatch, not a tuning knob. Every failure short of that is already handled: a missing binary, a worker that will not start, or one that dies mid-session falls back to encoding in-process with one line in the log, and never drops the session. |
| `PUNKTFUNK_SCRIPTING` | path | Where the host looks for `punktfunk-scripting`, the runner that performs every [plugin](/docs/plugins) package op (`plugins add`/`remove`/`list`, and the console's store installs). Unset, the host looks beside its own binary, then on `PATH`, then in the packaged `/usr` and `~/.local` layouts — right for every package, so set it only when the runner lives somewhere unusual. Like the row above it is **not** existence-checked: a path you name is a path you get, so a typo fails naming itself instead of quietly running a different runner. Worth knowing: the console runs installs inside the host *service*, whose `PATH` is normally much shorter than your login shell's — if `punktfunk-host plugins add` works and the console says the runner isn't installed, that gap is why, and this is the fix. |

## Diagnostics

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_PERF` | `1` | Log per-stage timing (capture, encode, send) — handy when tuning latency. |
| `RUST_LOG` | `info` · `debug` · `trace` | Log verbosity. On Windows, logs land in `%ProgramData%\punktfunk\logs\` (size-capped: a file over 10 MB is rotated to `.old` at the next service/host start, one generation kept). |
| `PUNKTFUNK_FFMPEG_DEBUG` | set | Verbose libavcodec/FFmpeg logging from the encoder. |
| `PUNKTFUNK_VIDEO_DROP` | `N` (percent) | Deliberately drop N% of video packets to exercise FEC recovery. **Testing only.** |

## Client-side (native clients)

A few knobs are read by the native **clients**, not the host — with one exception noted in the
table, where client and host read the *same* variable name for their own half of one feature:

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_DECODER` | `native-vulkan` · `native-vaapi` (Linux) · `native-d3d11va` (Windows) · `software` | Force the decode path. Default auto-selects hardware per GPU vendor and falls back on its own: **Linux** — Vulkan Video first on NVIDIA and AMD, VAAPI first on Intel and anything else; **Windows** — Vulkan Video first on NVIDIA and AMD, D3D11VA first on Intel and anything else. Whichever isn't first is the next thing tried, with software last (OpenH264 for H.264, rav1d for AV1 — there is no software HEVC, so a client that lands there reconnects on a codec it can decode). The names are the ones the [stats overlay](/docs/stats) prints, so a pin and a reading match. The older spellings `vulkan`, `vaapi` and `d3d11va` named the FFmpeg-backed decoders the clients used before and still work — each migrates onto the native path for the same hardware, and the client says so in its log. |
| `PUNKTFUNK_VAAPI_DEVICE` | path, e.g. `/dev/dri/renderD129` | **(Linux)** Pin the DRM render node the `native-vaapi` decoder opens. Unset, the client tries the nodes in order and takes the first that can decode the stream — set this on a multi-GPU box when it lands on the wrong one. |
| `PUNKTFUNK_PREFER_PYROWAVE` | `1` | Ask for the [PyroWave](/docs/pyrowave) wavelet codec on a wired link, where the client's own setting isn't reachable (the gamepad console, a headless launch). |
| `PUNKTFUNK_PAD_SPEAKER_PATH` · `PUNKTFUNK_PAD_SPEAKER_VOLUME` | byte, hex or decimal *(default `0x20` / `0x7F`)* | Which output a DualSense sends [controller audio](/docs/controller-audio) to, and how loud. A controller's channel 1 is shared between its headphone jack and its built-in speaker, and it powers up pointing at the jack — so with no headphones plugged in the speaker stays silent however correctly the audio is routed. Punktfunk points it at the speaker when controller-speaker is on. Change these only if your pad's speaker stays quiet; a game that sets its own audio levels still overrides them. |
| `PUNKTFUNK_PAD_AUDIO_PROFILE` | `0` | **(Linux)** Stop the client from switching a wired DualSense's sound card to **Pro Audio** while it streams [controller audio](/docs/controller-audio) to it. The switch exists because a controller's voice coils are channels 3 and 4 of its sound card, and a controller almost never presents four channels on its own — on any other profile the haptics are folded into the speaker pair and felt as nothing. Punktfunk restores the card's profile when the session ends and never saves it. Set this if you'd rather select the card's profile yourself. |
| `PUNKTFUNK_OSD_SCALE` | multiplier, e.g. `1.5` *(default `1`)* | Size of the in-stream overlay — the stats OSD, the capture hint and the start banner. They already follow your display's scaling setting (200 % display → twice the pixels), so set this only to nudge that: bigger for a TV across the room, smaller if your compositor reports an aggressive scale. Clamped to 0.5×–4×, and a line that would run off the screen is shrunk to fit. |
| `PUNKTFUNK_AUDIO_HIRES` | `1`/`on`/`true`/`yes` · `48000` · `96000` · `<rate>/<bits>` · `0`/`off`/`false`/`no` *(unset: the client's stored audio-format choice decides)* | ⚠️ **The same name as the host's policy gate in Audio / microphone above, and a different grammar** — so one line on a box that is both host and client sets both halves. This is the **request** half, and it overrides the client's stored audio-format choice for the run. `1` asks for 96 kHz / 24-bit, the rung the plane earns its bandwidth at. A bare rate — `48000` or `96000` — asks for that rate at 24-bit. `<rate>/<bits>` names both, which is the only way to reach `48000/16`: the cheapest lossless rung (~1.5 Mbps), and one no menu offers, because at 16-bit there is nothing left to *hear* over the 256 kbps Opus it replaces — only bit-exactness. `0` forces Opus even when the stored choice asks for lossless. Anything else is a typo: the client warns and **ignores** it, so the stored choice still decides rather than being silently switched off. And asking is not getting — a capture path that genuinely delivers the rate, the link budget, a frame that fits a datagram at that channel count, and the host not having set its own half to `0` all still have to agree, and the client plays whatever the host answers. (The host's half no longer has to be turned *on*, as it did before 2026-08-17 — so this request is usually the only one that matters.) Linux and Windows clients. |
| `PUNKTFUNK_NO_AEC` | `1` | Turn the microphone's echo cancellation off for this run, whatever **Echo cancellation** says in [client settings](/docs/client-settings#audio). One-way: it can only switch the processing off, never back on, and the setting is the normal way to control it. Linux and Windows clients. |
| `PUNKTFUNK_PRESENT_MODE` | `mailbox` *(default)* · `fifo` · `immediate` · `fifo_relaxed` | How decoded frames meet the display (the Vulkan present mode). The default prefers MAILBOX — tear-free without queueing behind the vertical refresh — and falls back to FIFO (classic vsync) where the driver doesn't offer it. **AMD's Windows driver offers no MAILBOX**, so those clients run FIFO, which adds a standing frame-pacing wait (up to one refresh interval). `immediate` removes that wait but can tear; `fifo_relaxed` only tears when a frame is late. If your latency floor matters more than tearing, try `immediate` and judge by eye. |
| `PUNKTFUNK_PRESENTER` | `arrival` | Turn the frame-pacing engine off for this run: frames present the instant they decode, exactly as they did before the **Prioritize** setting existed. A diagnostic — if a pacing change is suspected of causing judder or added delay, this switches it off without reinstalling anything. Linux and Windows clients. |
| `PUNKTFUNK_VRR_FIFO` | `1` | Force the display mode used to follow a **variable-refresh (VRR / FreeSync / G-Sync)** screen, on graphics drivers too old to offer the modern one. You almost certainly don't need this: where the driver supports the modern mode — which is what **Follow variable refresh rate** in [client settings](/docs/client-settings#video) uses — following the panel is already automatic and costs almost nothing. On an older driver the only way to follow the panel is a mode that measured roughly 27 ms *worse* on a fixed-refresh screen, so it stays off unless you ask for it, and it's only worth asking if you genuinely have a VRR screen and play fullscreen. Check the Detailed [stats overlay](/docs/stats): `vrr yes` means the panel really is following the stream. Linux and Windows clients. |
| `PUNKTFUNK_PRESENT_DEBUG` | `1` | Log the presenter's own 1-second summary (display mode, buffer drops, pacing counters) every second, even when nothing is going wrong. Without it the line appears only when there is something to report. |
| `PUNKTFUNK_ABR_PROBE_KBPS` | kbps, e.g. `90000` | The startup link-capacity probe's burst target. By default it's derived from the session — twice what your resolution, refresh rate and codec could plausibly use, which is the most the climb ceiling is ever allowed to reach — and capped at 2 Gbps. Lower it further on links the burst shouldn't slam, or when the measured ceiling comes out wrong for your setup. |
| `PUNKTFUNK_ABR_PROBE` | `0` | Skip the startup link-capacity probe entirely. The adaptive-bitrate climb ceiling then stays at the negotiated starting rate — a blunt instrument; prefer `PUNKTFUNK_ABR_MAX_MBPS`. |
| `PUNKTFUNK_ABR_MAX_MBPS` | Mbps, e.g. `300` | Hard cap on the adaptive bitrate's climb ceiling, whatever the startup probe measured. The escape hatch when adaptive sessions keep climbing past what your client's **decoder** can sustain (periodic hitch + "receive backlog stopped draining" in the client log). An explicit bitrate setting still bypasses ABR entirely. |

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

## Settings documented elsewhere

Not everything you can configure is a `host.env` line, and a few knobs are explained on the page
that owns their feature:

- **Virtual-display policy** — keep-alive, topology, per-client scaling — lives in the web console
  and `display-settings.json`: [Virtual displays](/docs/virtual-displays).
- **Which GPU to use** on a multi-GPU box is a console choice (`gpu-settings.json`) that outranks
  `PUNKTFUNK_RENDER_NODE` / `PUNKTFUNK_RENDER_ADAPTER` — see *Picking a GPU* above.
- **Event hooks and webhooks** are `hooks.json`, not environment variables:
  [Events & hooks](/docs/automation).
- **Updating**, including the one-click opt-in on Linux: [Updating the Host](/docs/updating).
- **Encoder prerequisites** (Mesa/VAAPI packages, Intel HuC firmware, NVIDIA driver bits):
  [Requirements](/docs/requirements).
- **Full chroma (4:4:4)** — which codecs, capture paths, GPUs and clients can carry it:
  [Client settings](/docs/client-settings#video).
- **Client settings** — resolution, bitrate, codec, decoder, HDR — are set in the client app, with
  what each one defaults to and which of them the host can overrule:
  [Client settings](/docs/client-settings).

The host also reads a number of debugging and development variables that aren't listed here; they
change between releases and are not meant for everyday use.
