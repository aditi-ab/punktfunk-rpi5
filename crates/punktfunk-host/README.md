# punktfunk-host

The **streaming host** — the program you run on the machine whose desktop or games you want to
stream. For each client that connects, it spins up a **virtual display sized to that device**,
captures it on the GPU, encodes with hardware NVENC/VAAPI/AMF/QSV, and sends it out over a
low-latency transport — no physical monitor, no letterboxing, no rearranging your real screens.

It speaks two protocols from **one process**:

- **GameStream** — so any [Moonlight](https://moonlight-stream.org/) / Artemis client works day one.
- **`punktfunk/1`** — punktfunk's own faster protocol (QUIC control plane, GF(2¹⁶) FEC + AES-GCM data
  plane) that the native clients use.

Runs on **Linux** (the primary, most battle-tested path) and **Windows** (x64). The shared protocol,
FEC, and crypto live in [`punktfunk-core`](../punktfunk-core/README.md); this crate is everything
platform-facing around it.

## What it does

- **Per-client virtual displays at the exact WxH@Hz.** Linux uses per-compositor backends — **KWin**,
  **gamescope**, **Mutter**, and **Sway/wlroots**; Windows uses its own all-Rust IddCx virtual display,
  even on the secure desktop (UAC / lock screen).
- **GPU zero-copy capture → encode.** dmabuf → CUDA/Vulkan → NVENC on Linux; on Windows the host
  pushes frames straight into its own IDD (sealed IDD-push, no screen-scraping) → GPU encode.
  Encoders auto-select by GPU vendor: **NVENC** (NVIDIA), **VAAPI** (Linux AMD/Intel),
  **AMF/QSV** (Windows AMD/Intel), or software H.264 as a floor. HDR/10-bit and HEVC 4:4:4 supported.
- **Input injection.** Mouse/keyboard (libei / gamescope EIS / wlr / Windows SendInput) and virtual
  **gamepads** — Xbox 360/One, DualSense, DualShock 4 — with rumble and HID feedback back-channels.
- **Audio both ways.** Opus audio host→client, plus a virtual microphone the client can talk into.
- **Trust & discovery.** A persistent host identity, **SPAKE2 PIN pairing** (default) or TOFU, and
  mDNS auto-advertisement so clients find the host without typing an IP.
- **Management API + web console.** A REST API (`mgmt.rs`, OpenAPI at
  [`api/openapi.json`](../../api/openapi.json)) drives status, paired devices, and on-demand pairing;
  the browser UI is in [`web/`](../../web/README.md).

## Run it

`punktfunk-host serve` runs inside your desktop session. Bare `serve` is the **secure native-only
default** (`punktfunk/1` + the management API); add `--gamestream` on a trusted LAN to also accept
stock Moonlight clients.

```sh
# Linux, from the repo root (see the repo README "Running on this box" for the headless recipe):
cargo run -rp punktfunk-host -- serve                 # native-only (secure default)
cargo run -rp punktfunk-host -- serve --gamestream    # + Moonlight compatibility
```

Then pair from the web console (`https://<host-ip>:47992`) or the client app.

Most people should install a **package** rather than run from source — see
[`packaging/`](../../packaging/README.md) (apt · rpm/COPR/bootc · Arch/sysext · Windows installer) and
the per-platform guides at **[docs.punktfunk.unom.io/docs/install](https://docs.punktfunk.unom.io/docs/install)**.

### Subcommands

| Command | Purpose |
|---------|---------|
| `serve` | The host (native `punktfunk/1` + mgmt API; `--gamestream` adds Moonlight). |
| `punktfunk1-host` | Standalone native-protocol listener for testing/measurement (`--source virtual`, `--max-sessions`). |
| `openapi` | Print the management-API OpenAPI spec (regenerates `api/openapi.json`). |
| `library` | Inspect the multi-store game library. |
| `service` · `driver` · `web` | Windows: SCM service, driver install, bundled web console. |
| `*-test` / `*-selftest` / `*-probe` | Diagnostics (input, zero-copy, HDR, compositor, gamepads). |

`--help` lists them all.

## Layout

```
src/
  main.rs            CLI + subcommand dispatch
  config.rs · session_plan.rs · session_tuning.rs · pipeline.rs   session setup + the frame pipeline
  vdisplay/          per-compositor virtual outputs (kwin · gamescope · mutter · wlroots)
  capture/ · capture.rs    screen/dmabuf capture (+ Windows IDD-push)
  encode/ · encode.rs      per-GPU encoders (nvenc · vaapi · ffmpeg_win (AMF/QSV) · sw)
  zerocopy/          dmabuf → CUDA → NVENC bridges (EGL/GL tiled, Vulkan LINEAR)
  inject/ · inject.rs      input backends (libei · wlr · uinput gamepads · UHID DualSense/DS4)
  audio/ · audio.rs        Opus out + virtual mic (PipeWire / WASAPI)
  gamestream/        Moonlight compat: nvhttp · pairing · rtsp · control · stream · gamepad · apps
  punktfunk1.rs      the native punktfunk/1 host (QUIC control + native-thread UDP data plane)
  mgmt.rs · native_pairing.rs · stats_recorder.rs   management API, pairing, perf capture
  hdr.rs · library.rs      HDR metadata; multi-store game library
  linux/ · windows/  platform-confined backends
```

## Related

- **[`punktfunk-core`](../punktfunk-core/README.md)** — the shared protocol · FEC · crypto core
- **[Clients](../../clients/)** — the apps that connect (Apple · Linux · Windows · Android · probe)
- **[Packaging](../../packaging/README.md)** & **[docs](https://docs.punktfunk.unom.io)** — install & operate
- **[`design/`](../../design/README.md)** — architecture rationale and deep-dive plans
