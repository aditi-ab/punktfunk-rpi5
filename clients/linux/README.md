# punktfunk — Linux client

The native **Linux** app for streaming a punktfunk host to your desktop, laptop, or Steam Deck.
It's a clean GTK4/libadwaita app that finds hosts on your network, pairs with a PIN, and puts a
low-latency stream on glass at your display's own resolution and refresh rate.

Built in Rust, it links the shared **`punktfunk-core`** directly (no C ABI) and speaks the fast
**`punktfunk/1`** protocol — QUIC control plane, GF(2¹⁶) FEC + AES-GCM data plane.

## Features

- **Zero-copy hardware decode** — FFmpeg VAAPI decode → DRM-PRIME dmabuf → `GdkDmabufTexture`
  (Tier-1 zero-copy on Intel and AMD), with an automatic software-HEVC fallback on NVIDIA or when
  VAAPI is unavailable.
- **Your display's native mode** — the host builds a virtual output at exactly your WxH@Hz; no
  scaling, no letterboxing. Steady 60 fps at 1080p60, ~6 ms capture→decoded on the LAN.
- **Audio both ways** — PipeWire playback with a jitter ring, plus mic uplink to the host.
- **Full controller support** — SDL3 gamepads with rumble and DualSense fidelity (lightbar, player
  LEDs, touchpad, motion, adaptive-trigger replay). Click-to-capture keyboard and mouse, with a
  release chord (Ctrl+Alt+Shift+Q) and focus-loss release.
- **Find hosts automatically** — mDNS discovery lists hosts on your LAN; saved hosts persist.
  First connect does a one-time **SPAKE2 PIN pairing** (or TOFU on trusted LANs), then reconnects on
  a pinned identity.
- **Per-host speed test** to pick a bitrate, plus compositor and mode preferences in Settings.
- **Game library browser** *(experimental, off by default)* — "Browse library…" on a saved host
  shows its games (Steam + custom) as a poster grid; click one to launch it in the session.
  Fetched from the host's management API over mTLS — paired devices are authorized by their
  certificate, no extra host setup.
- **Gamepad library launcher** (`--browse host`) — a console-style, controller-driven coverflow of
  a paired host's library (drifting aurora backdrop, center-focus posters, button hints): A plays
  the focused title, B quits, L1/R1 jump. Built for the Steam Deck plugin's "Open library" launch;
  session end returns to the launcher. Arrow keys/Enter/Esc drive it too (no pad needed).

## Get it

Most people should install a package rather than build from source:

| Distro | Install |
|--------|---------|
| **Flatpak** (any distro, Steam Deck) | `io.unom.Punktfunk` — see [`packaging/flatpak`](../../packaging/flatpak/README.md) |
| **Ubuntu / Debian** (apt) | `sudo apt install punktfunk-client` *(after adding the repo)* |
| **Fedora / Bazzite** (rpm) | `rpm-ostree install punktfunk-client` |
| **Arch** (PKGBUILD) | see [`packaging/arch`](../../packaging/arch/README.md) |

Per-device install steps and pairing walkthrough:
**[docs.punktfunk.unom.io/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.

## Build & run from source

Requires GTK ≥ 4.16, libadwaita ≥ 1.5, FFmpeg 7 or 8 (with VAAPI for hardware decode), PipeWire,
and SDL3 (with hidapi) development packages.

```sh
# from the repo root
cargo run -p punktfunk-client-linux                 # launch the app
cargo run -p punktfunk-client-linux -- --connect HOST[:PORT]   # skip the host list and connect
cargo run -p punktfunk-client-linux -- --browse HOST           # the gamepad library launcher
```

The binary is named **`punktfunk-client`**. Handy flags: `--connect host[:port]` (start a session
immediately — for scripting and the Steam Deck launcher) with optional `--launch <id>` (ask the
host to launch that library title, id from `--library`), `--browse host[:port]` (the gamepad
library launcher; `--mgmt <port>` overrides the management port it fetches from),
`--pair <PIN> --connect host[:port]` (run the pairing ceremony headlessly), and
`--library host[:mgmt_port]` (print a host's game library headlessly). Force a decoder with
`PUNKTFUNK_DECODER=software|vaapi`; `PUNKTFUNK_FAKE_LIBRARY=<file.json>` feeds the launcher
canned entries for UI work with no host.

## Layout

```
src/
  main.rs · app.rs        entry point, GTK application, primary menu, CSS
  cli.rs                  CLI paths (--connect/--launch, --browse, headless --pair, screenshot scenes)
  ui_hosts.rs             host card grids (saved + discovered) · add-host dialog · banner
  ui_library.rs           game-library poster grid (per-host, launches titles)
  ui_gamepad_library.rs   the --browse gamepad launcher (aurora · coverflow · hint bar)
  ui_trust.rs             TOFU / PIN-pairing / request-access dialogs
  ui_settings.rs          resolution · refresh · decoder · bitrate · compositor · mic
  ui_stream.rs            the stream window (GtkGraphicsOffload present) + input capture
  launch.rs               session launch/UI glue over the shared session pump
  spawn.rs                desktop connects → the punktfunk-session Vulkan binary
                          (PUNKTFUNK_LEGACY_PRESENTER=1 keeps them in-process)
  video_gl.rs             VAAPI dmabuf → RGBA GL presenter (EGL import, CICP-driven CSC;
                          the legacy/fallback presenter)
tools/screenshots.sh      store screenshot capture (app self-capture; Xvfb fallback)
```

The UI-agnostic plumbing — session pump, FFmpeg decode, PipeWire audio, SDL3 gamepads +
keymap, trust store, mDNS discovery, library client, Wake-on-LAN — lives in
`crates/pf-client-core`, shared with the Vulkan session binary.

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Steam Deck plugin](../decky/README.md)** — launches this client fullscreen in Gaming Mode
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
