# punktfunk — Linux client

The native **Linux** app for streaming a punktfunk host to your desktop, laptop, or Steam Deck.
It's a clean relm4/GTK4/libadwaita **shell** that finds hosts on your network, pairs with a PIN,
and manages your settings and library — the stream itself runs in the sibling
**`punktfunk-session`** Vulkan binary ([`clients/session`](../session/README.md)), which the shell
spawns, putting the picture on glass at your display's own resolution and refresh rate.

Built in Rust end to end (no C ABI): the shell shares its plumbing with the session binary through
**`crates/pf-client-core`**, which links the **`punktfunk-core`** protocol crate and speaks the fast
**`punktfunk/1`** protocol — QUIC control plane, GF(2¹⁶) FEC + AES-GCM data plane.

## Features

- **Zero-copy hardware decode** — the session presenter decodes via **Vulkan Video** on every GPU
  vendor (including NVIDIA), falling back to FFmpeg VAAPI → DRM-PRIME dmabuf and then software when
  Vulkan Video is unavailable.
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
- **Gamepad library launcher** (`--browse host`) — a console-style, controller-driven library view
  of a paired host's games, rendered by the session binary's Skia console UI: A plays the focused
  title, B quits, L1/R1 jump. Built for the Steam Deck plugin's "Open library" launch; session end
  returns to the launcher. Arrow keys/Enter/Esc drive it too (no pad needed).

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

The binary is named **`punktfunk-client`** — the relm4/libadwaita desktop shell (hosts,
pairing/trust, settings, the desktop library page). Every stream and the console game
library run in the sibling **`punktfunk-session`** Vulkan binary; the shell spawns it
for connects, and `--connect`/`--browse` on the shell exec it directly (so the Decky
wrapper keeps working unchanged). Headless flags stay in the shell:
`--pair <PIN> --connect host[:port]` (pairing ceremony), `--wake host[:port]`, and
`--library host[:mgmt_port]` (print a host's game library).

## Layout

```
src/
  main.rs · app.rs        entry point, relm4 AppModel (window, trust gate, session child
                          lifecycle, typed messages), primary menu, CSS
  cli.rs                  headless paths (--pair/--wake/--library), the --connect/--browse
                          exec handoff to punktfunk-session, screenshot scenes
  ui_hosts.rs             hosts page component (FactoryVecDeque cards, saved + discovered
                          grids, add-host dialog, banner)
  ui_library.rs           game-library poster grid (per-host, launches titles)
  ui_trust.rs             TOFU / PIN-pairing / request-access dialogs
  ui_settings.rs          resolution · refresh · decoder · bitrate · compositor · mic
  spawn.rs                the session-child plumbing (stdout contract → AppMsg)
tools/screenshots.sh      store screenshot capture (app self-capture; Xvfb fallback)
```

The UI-agnostic plumbing — session pump, FFmpeg decode, PipeWire audio, SDL3 gamepads +
keymap, trust store, mDNS discovery, library client, Wake-on-LAN — lives in
`crates/pf-client-core`, shared with the Vulkan session binary.

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Steam Deck plugin](../decky/README.md)** — launches this client fullscreen in Gaming Mode
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
