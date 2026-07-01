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
cargo run -p punktfunk-client-linux -- --discover   # list hosts on the LAN, then exit
cargo run -p punktfunk-client-linux -- --connect HOST[:PORT]   # skip the host list and connect
```

The binary is named **`punktfunk-client`**. Handy flags: `--connect host[:port]` (start a session
immediately — for scripting and the Steam Deck launcher), `--discover [secs]`, and
`--pair <PIN> --connect host[:port]` (run the pairing ceremony headlessly). Force a decoder with
`PUNKTFUNK_DECODER=software|vaapi`.

## Layout

```
src/
  main.rs · app.rs        entry point, GTK application, CLI paths
  ui_hosts.rs             host list (mDNS + saved), pairing / trust dialogs
  ui_settings.rs          resolution · refresh · decoder · bitrate · compositor · mic
  ui_stream.rs            the stream window (GtkGraphicsOffload present) + input capture
  session.rs              session lifecycle over the NativeClient connector
  video.rs                FFmpeg VAAPI / software decode → dmabuf / texture
  audio.rs                PipeWire playback + mic uplink
  gamepad.rs · keymap.rs  SDL3 controllers + feedback; keyboard VK mapping
  trust.rs · discovery.rs persistent identity, TOFU/PIN pairing, mDNS browse
tools/screenshots.sh      store screenshot capture
```

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Steam Deck plugin](../decky/README.md)** — launches this client fullscreen in Gaming Mode
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
