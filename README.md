<p align="center">
  <img src="assets/punktfunk-logo.svg" alt="punktfunk" width="320" />
</p>

<p align="center"><b>Low-latency desktop and game streaming with first-class Linux and Windows hosts.</b></p>

Run the host on a Linux machine or a Windows PC, connect from a Mac, PC, phone, tablet, or TV, and
stream your desktop or games — each device at its **own native resolution and refresh rate**, over
your local network.

📖 **Documentation: [docs.punktfunk.unom.io](https://docs.punktfunk.unom.io)** — start with
[How It Works](https://docs.punktfunk.unom.io/docs/how-it-works) or the
[Quick Start](https://docs.punktfunk.unom.io/docs/quickstart).

💬 **Community: [Discord](https://discord.gg/kaPNvzMuGU)** — chat, support, and **Android beta
access** · **[r/Punktfunk](https://www.reddit.com/r/Punktfunk/)**.

🔒 **Security:** found a vulnerability? Report it privately to **security@punktfunk.com** — see
[SECURITY.md](SECURITY.md). Please don't open a public issue.

punktfunk pairs a **virtual-display streaming host** with native clients on every platform. It speaks
the existing **GameStream** protocol, so any [Moonlight](https://moonlight-stream.org/) client works
day one — and adds its own faster **`punktfunk/1`** protocol that breaks the ~1 Gbps FEC wall with a
**GF(2¹⁶) Leopard-RS** transport. A single shared **Rust core** (`punktfunk-core`) holds the
protocol, FEC, and crypto, linked into the host and every client over a stable C ABI.

## What makes it different

- **Your device's exact mode.** For each client that connects, the host spins up a virtual display
  sized to that device — 1080p60 to a laptop, 1440p120 to a desktop, 4K to a TV, all at once. No
  letterboxing, no scaling, no rearranging your real monitors.
- **Displays you configure, not just create.** Keep a game's display (and the game) alive across
  disconnects so a reconnect drops straight back in; make the stream your sole desktop or extend
  alongside your monitors; let several devices become monitors of one desktop; keep each client's
  scaling. One-click presets in the console — a dedicated couch box, a shared desktop, a multi-monitor
  workstation. See [Virtual displays](docs-site/content/docs/virtual-displays.md).
- **A real virtual display on Windows, too.** On Linux the host uses per-compositor virtual outputs;
  on Windows you get the same on-the-fly virtual display — at the client's exact mode, no physical
  monitor or dummy HDMI plug, even on the secure desktop (UAC / lock screen). It also has **its own
  indirect display driver (IDD)** the host pushes finished frames straight into, rather than scraping
  a screen — tight, push-based integration that's unusual for a Windows streaming host.
- **Low latency, GPU end to end.** Frames go straight from the compositor to the NVENC encoder with
  zero CPU copies (dmabuf → CUDA/Vulkan → NVENC), over a transport tuned for responsiveness rather
  than throughput. Stable 240 fps at 5120×1440; sub-millisecond capture-to-reassembly on-box,
  ~1.3 ms cross-machine on a LAN. (AMD/Intel encode via VAAPI, and a GPU-less software H.264
  encoder exists as a fallback.)
- **Works with what you already have.** Any Moonlight/Artemis client connects over GameStream — and
  native apps for macOS, Linux, Windows, and Android use the lower-latency `punktfunk/1` protocol.
- **Secure by default.** Hosts require a one-time SPAKE2 **PIN pairing**; after that, devices
  reconnect on a pinned identity. No accounts, no cloud. Hosts auto-advertise over mDNS, so clients
  find them on the network without typing an IP.

## Status

| Component | State |
|-----------|-------|
| **Core** — `punktfunk-core` + C ABI (protocol · FEC · crypto · QUIC) | ✅ Complete & hardened |
| **GameStream host** → stock Moonlight | ✅ Live end-to-end: pairing, RTSP, audio, per-client virtual output at native resolution, GPU zero-copy NVENC, gamepads |
| **Native protocol** — `punktfunk/1` | ✅ Validated live: QUIC control + GF(2¹⁶) FEC/AES-GCM data plane, PIN pairing, mDNS discovery, mid-stream mode renegotiation |
| **Windows host** (Windows 11 22H2+, x64) | 🟡 Implemented & shipping as a signed installer: its own all-Rust IddCx **virtual display** (secure-desktop capable) with a **sealed IDD-push** capture path — finished frames pushed straight into its own driver, not screen-scraped (no DDA/WGC) · GPU encode (NVENC on NVIDIA, AMF/QSV on AMD/Intel, software H.264 without a GPU) · WASAPI audio · bundled virtual-gamepad drivers (no ViGEmBus) · HDR incl. Vulkan-game HDR. NVIDIA live-validated; AMD/Intel CI-green |
| **macOS / iOS / tvOS client** (`clients/apple`) | ✅ Streaming live: VideoToolbox decode, controllers incl. DualSense, discovery, pairing, speed test |
| **Linux client** (`clients/linux`, GTK4) | ✅ Streaming live: FFmpeg + VAAPI zero-copy decode, PipeWire audio, SDL3 controllers; ships as Flatpak/apt/rpm/Arch |
| **Android client** (`clients/android`, phone + TV) | ✅ Streaming live: AMediaCodec decode + HDR10, AAudio audio, controllers, discovery, pairing |
| **Windows client** (`clients/windows`, WinUI 3) | ✅ Streaming live: D3D11VA hardware decode on all GPU vendors (NVIDIA + Intel validated on glass) with software fallback, WASAPI audio, SDL3 controllers, discovery, pairing; ships as signed MSIX (x64 + ARM64). HDR10 implemented, on-glass validation pending |
| **Web console + management API** (`web/`) | ✅ TanStack console over the OpenAPI mgmt API: host status, paired devices, on-demand PIN pairing, GPU selection, performance capture graphs, live host logs |

The **GameStream host works with a stock Moonlight client** — validated live on NVIDIA hardware
(RTX 5070 Ti, RTX 4090): PIN pairing that persists across restarts, an app catalog, RTSP/ENet/audio,
and **video at the client's exact resolution and refresh** via a per-session virtual output (KWin,
gamescope, Mutter, and Sway/wlroots backends), encoded with GPU **zero-copy** (dmabuf → CUDA/Vulkan →
NVENC) up to 5120×1440@240. The native **`punktfunk/1`** protocol adds a QUIC control plane and a
GF(2¹⁶) Leopard-FEC + AES-GCM data plane (p50 ~0.8 ms capture→received at 720p120), with
mid-stream mode renegotiation and a wall-clock skew handshake so latency stays valid across machines.
Both run from **one process**: bare `punktfunk-host serve` is the **secure native-only default**
(`punktfunk/1` + the management API/web console), and `serve --gamestream` additionally enables the
GameStream/Moonlight-compat planes (opt-in, trusted-LAN only — GameStream has inherent on-path
weaknesses). The host is managed through a REST API and web console. Builds against FFmpeg 7 or 8.

Full milestone status: **[docs.punktfunk.unom.io/docs/status](https://docs.punktfunk.unom.io/docs/status)** ·
roadmap: **[/docs/roadmap](https://docs.punktfunk.unom.io/docs/roadmap)**.

## Install the host

Pick your platform and install from its package registry — the per-platform guide covers adding the
repo, first run, and the web console. The Linux host is the primary, most battle-tested path; a
Windows host also ships as a signed installer (all-vendor: NVIDIA, AMD, Intel).

| Platform | Install | Guide |
|--------|---------|-------|
| **Ubuntu / Debian** (apt) | `sudo apt install punktfunk-host` *(after adding the repo)* | [Ubuntu — GNOME](https://docs.punktfunk.unom.io/docs/ubuntu-gnome) · [KDE](https://docs.punktfunk.unom.io/docs/ubuntu-kde) |
| **Bazzite / Fedora Atomic** (systemd-sysext) | `sudo bash punktfunk-sysext.sh install` *(no layering, no reboot; rpm-ostree + bootc also supported)* | [Bazzite](https://docs.punktfunk.unom.io/docs/bazzite) |
| **Fedora** (dnf) | `dnf install punktfunk punktfunk-web` *(after adding the repo)* | [Fedora — KDE](https://docs.punktfunk.unom.io/docs/fedora-kde) |
| **Arch / Steam Deck** (pacman / sysext) | `pacman -Sy punktfunk-host` *(binary repo)* · sysext `.raw` *(SteamOS)* | [packaging/arch](packaging/arch/README.md) |
| **Windows** (11 22H2+, x64) | signed `setup.exe` from the package registry | [Windows Host](https://docs.punktfunk.unom.io/docs/windows-host) |

`punktfunk-host` is the streaming host; `punktfunk-web` is the browser console (pairing + status).
After install, run `punktfunk-host serve` inside your desktop session (the secure native default;
add `--gamestream` on a trusted LAN if you also want stock Moonlight clients), then pair from the web
console. Full instructions: **[docs.punktfunk.unom.io/docs/install](https://docs.punktfunk.unom.io/docs/install)**.

## Connect a client

| Streaming to… | Use |
|---|---|
| Mac, iPhone, iPad, Apple TV | The **Apple app** (`clients/apple`) — also on TestFlight |
| Linux desktop / laptop, Steam Deck | **`punktfunk-client`** (Flatpak / apt / rpm / Arch) |
| Android phone or TV | The **Android app** (`clients/android`) |
| Windows | Native **`punktfunk-client`** (signed MSIX) or **Moonlight** |
| Anything else (browser, old phone, smart TV) | **Moonlight** over GameStream |

Each client discovers hosts on the network automatically and does a one-time
[PIN pairing](https://docs.punktfunk.unom.io/docs/pairing). Per-device install steps:
**[/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.

## Build & test (from source)

For development, or as an install fallback where no package is available:

```sh
cargo build --workspace          # the Rust core, host, Linux client, and probe (Linux & macOS)
cargo test  --workspace          # unit + loopback + proptest + C ABI harness
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

cargo run -p loss-harness        # FEC loss-resilience sweep (no network needed)
bash crates/punktfunk-core/tests/c/run.sh   # standalone C-ABI link + round-trip proof
```

The C header regenerates from `crates/punktfunk-core/src/abi.rs` on every build (cbindgen via
`build.rs`) into `include/punktfunk_core.h`. The Apple, Android, and Windows clients have their own
toolchains (Xcode/`swift build`, Gradle, and `cargo` on the MSVC target) — see each client's README
and the [docs site](https://docs.punktfunk.unom.io).

## Layout

```
crates/
  punktfunk-core/   protocol · FEC · pacing · crypto · QUIC control plane — the C ABI (lib + cdylib + staticlib)
  punktfunk-host/   the host (Linux + Windows): virtual displays · capture · encode · input · GameStream · punktfunk/1 · mgmt
clients/
  apple/    macOS / iOS / tvOS app (Swift · VideoToolbox · Metal · GameController)
  linux/    Linux desktop app (Rust · GTK4/libadwaita · FFmpeg/VAAPI · PipeWire · SDL3)
  windows/  Windows desktop app (Rust · WinUI 3 · D3D11 · WASAPI · SDL3)
  android/  Android phone + TV app (Kotlin · Rust JNI core · AMediaCodec · AAudio)
  probe/    headless reference / measurement client for punktfunk/1
  decky/    Steam Deck Decky plugin
web/                         web console (TanStack) over the management API — status · devices · pairing · GPUs · performance · logs
packaging/                   apt · rpm / COPR · Arch · Flatpak · Bazzite bootc image
docs-site/                   public documentation site (Fumadocs) — https://docs.punktfunk.unom.io
include/punktfunk_core.h     cbindgen-generated C header (checked in)
tools/                       latency-probe · loss-harness (measurement)
```

## Design invariants

- **One core, linked everywhere.** Protocol, FEC, and crypto live in `punktfunk-core` exactly once,
  exposed over a stable, versioned C ABI (`punktfunk_abi_version()`, `PunktfunkConfig` carries its own
  `struct_size`). Every native client links the same core.
- **No async on the hot path.** The per-frame pipeline uses native threads only; `tokio`/`quinn` are
  gated behind the off-by-default `quic` feature (control plane only).
- **Native client resolution, no scaling.** Each session gets a virtual output at exactly the
  client's WxH@Hz; each compositor keeps its own backend behind a shared `VirtualDisplay` trait.
- **FEC is the wall-breaker.** GF(2⁸) (≤255 shards/block) for Moonlight compatibility; GF(2¹⁶)
  (≤65535 shards/block, SIMD, O(n log n)) for `punktfunk/1` to push past ~1 Gbps.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option — `SPDX-License-Identifier: MIT OR Apache-2.0`.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).

### Third-party components

punktfunk's own source is MIT/Apache-2.0. Shipped binaries additionally link third-party components
under their own (permissive) licenses — see [`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt)
(regenerate with `scripts/gen-third-party-notices.sh`). The Windows host and client builds also
bundle FFmpeg under the **LGPL v2.1+** (dynamically linked, replaceable DLLs; the license text and
notice ship in the installed `licenses/` folder).

### Trademarks

punktfunk is an independent project and is **not affiliated with, endorsed by, or sponsored by**
NVIDIA, Microsoft, Sony, Valve, or the Moonlight project. "GameStream", "Moonlight", "Xbox",
"DualSense", "DualShock", and "PlayStation" are trademarks of their respective owners and are used
here only to describe interoperability.
