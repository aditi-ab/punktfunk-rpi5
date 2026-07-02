# punktfunk — Apple client (macOS · iOS · iPadOS · tvOS)

The native **Apple** app for streaming a punktfunk host to your Mac, iPhone, iPad, or Apple TV. A
SwiftUI app that finds hosts on your network, pairs with a PIN, and streams at your display's own
resolution and refresh rate — with VideoToolbox hardware decode and full controller support.

All the networking and protocol work — QUIC control plane, UDP data plane, GF(2¹⁶) FEC, AES-GCM,
Opus audio, cert pinning — lives in the shared Rust **`punktfunk-core`** (statically linked as
`PunktfunkCore.xcframework`). This package is the Swift shell: decode, present, input, and UI.

## Features

- **Hardware decode** — VideoToolbox HEVC, with a low-latency **stage-2 presenter**
  (`VTDecompressionSession` → `CAMetalLayer`, presented off a `CADisplayLink`, ~11 ms p50) as the
  default and an `AVSampleBufferDisplayLayer` fallback.
- **HDR & 4:4:4** — PQ passthrough with a correct reference-white anchor, mid-session SDR↔HDR
  reconfiguration, and hardware-probed 4:4:4 support.
- **Your display's native mode** — the host builds a virtual output at exactly your WxH@Hz;
  mid-stream resize renegotiates without reconnecting.
- **Audio both ways** — Opus playback (CoreAudio, no bundled libopus) with a jitter ring, plus mic
  uplink; speaker/mic selectable in Settings.
- **Full controller support** — one selected controller forwarded as pad 0, including **DualSense**
  feedback (rumble → CoreHaptics, lightbar, player LEDs, adaptive triggers) and touchpad/motion. The
  virtual pad type auto-resolves from your physical controller.
- **Mouse & keyboard** — `GCMouse`/`GCKeyboard` capture with click-to-capture and a ⌘⎋ release, plus
  iPad pointer lock and touch input.
- **Find hosts automatically** — mDNS discovery (`NWBrowser` over `_punktfunk._udp`); first connect
  does a one-time **SPAKE2 PIN pairing** (or TOFU on trusted LANs), then reconnects on a pinned,
  Keychain-stored identity.
- **Tune the stream** — a fps / Mb·s / **latency** HUD (skew-corrected across machines), a bitrate
  control, a per-host **network speed test** with a recommended bitrate, and a host-compositor picker.

Runs from one shared codebase across **macOS, iOS, iPadOS, and tvOS**.

## Get it

Install from the App Store / TestFlight, or build from source below. Per-device install steps and the
pairing walkthrough:
**[docs.punktfunk.unom.io/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.

## Build / run / test (on a Mac)

Requires Xcode 26.5 / Swift 6.3. First build the Rust core into an xcframework, then build the app:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-xcframework.sh            # → clients/apple/PunktfunkCore.xcframework
#   BUILD_IOS=1  also builds the iOS slices  (add the ios rustup targets)
#   BUILD_TVOS=1 also builds tvOS            (tier-3 targets, built from source — see below)

cd clients/apple
open Punktfunk.xcodeproj                      # the real app: ⌘R builds + runs Punktfunk.app
swift run PunktfunkClient                     # or the unbundled dev shell (CLI)
swift build && swift test                     # unit + loopback/remote tests (self-skip w/o a host)
```

tvOS slices are tier-3 Rust targets, built from source:
`rustup toolchain install nightly && rustup component add rust-src --toolchain nightly`.

### Test against a host

```sh
# full loopback proof — builds punktfunk-host (synthetic source, runs on macOS) and streams
# byte-verified frames into the Swift client, incl. the PIN pairing ceremony:
bash test-loopback.sh

# against a real Linux host on the LAN (see the repo README "Running on this box"):
PUNKTFUNK_REMOTE_HOST=<box-ip> swift test --filter RemoteFirstLightTests            # headless
PUNKTFUNK_AUTOCONNECT=<box-ip> PUNKTFUNK_MODE=1280x720x60 swift run PunktfunkClient # on glass
```

## Project layout

- **`PunktfunkKit`** (library) — the reusable pieces:
  - `PunktfunkConnection` — the wrapper over the C ABI (thread-safe `close()`, per-plane locks,
    pinning + TOFU).
  - `AnnexB` / `StreamView` / `VideoDecoder` / `MetalVideoPresenter` — format handling, the stage-1
    (`AVSampleBufferDisplayLayer`) and stage-2 (`VTDecompressionSession` → `CAMetalLayer`) presenters.
  - `InputCapture` — `GCMouse`/`GCKeyboard` → host VK/mouse, with fractional-delta accumulation.
  - `GamepadManager` / `GamepadCapture` / `GamepadFeedback` / `DualSenseTriggerEffect` — controller
    discovery + selection, capture (buttons/axes/touchpad/motion), and host-feedback rendering.
  - `HostDiscovery` — `NWBrowser` over `_punktfunk._udp`.
- **`PunktfunkClient`** (the app) — hosts grid with an *On this network* section, add-host sheet,
  the two trust flows (TOFU prompt + SPAKE2 `PairSheet`), the stream view with the HUD, a
  tabbed Settings pane (General / Display / Audio / Controllers / Advanced), and the network speed
  test. A Scene-level **Stream** menu carries Disconnect (⌘D) and the HUD toggle (⌘⇧S).
  On iOS/iPadOS **and macOS** a connected controller swaps the whole home for the **gamepad UI**
  (`Home/Gamepad*`, `Settings/GamepadSettingsView`): a console-style host carousel (A connect · Y
  library · X settings), a controller-navigable settings screen, an add-host flow with an
  on-screen controller keyboard (no touch required anywhere), and the coverflow library browser —
  all driven by the shared `GamepadMenuInput` poller + `GamepadCarousel`/`GamepadMenuList` focus
  machinery, with dual-channel haptics (device Taptic + controller `MenuHaptics`), over an
  animated "aurora" backdrop (`GamepadScreenBackground` — TimelineView-driven drifting color
  blobs; deliberately pure SwiftUI, since a .metal library only reliably bundles in one of the
  two build systems these sources compile under). macOS presents the settings/add-host screens as
  sheets (no `fullScreenCover` there); `PUNKTFUNK_FORCE_GAMEPAD_UI=1` forces the mode without a
  physical pad (dev/screenshots).
- **Tests** (`swift test`) — Annex-B units, a real-codec VideoToolbox round trip, DualSense
  trigger-effect and gamepad-wire conversions, loopback integration against real local hosts, and the
  remote first-light test.

## Notes for contributors

- **Xcode project** (`Punktfunk.xcodeproj`) wraps the same sources as the `swift run` shell (a
  synchronized folder — no duplication). The macOS target is **App-Sandboxed** (needs
  `network.server` — the raw-UDP plane and quinn both `bind()`); iOS/tvOS use the shared
  entitlements file (keep `app-sandbox` **out** of it). Verify with
  `codesign -d --entitlements :- <built .app>`.
- **Decode flow**: the host opens every stream with an IDR carrying VPS/SPS/PPS in-band, and recovery
  keyframes re-send them — refresh the format description on every IDR; there is no out-of-band
  extradata, ever.
- **ABI threading**: one video pump thread per connection, one optional audio drain thread, and one
  optional feedback drain thread (rumble + HID-output). `send()` is enqueue-only and safe alongside
  all of them. The wrapper's per-plane locks make `close()` safe from anywhere.
- **DualSense motion scale** (`GamepadWire`) is derived from hid-playstation's math, not yet
  live-verified — if gyro/accel feel wrong in a game, correct sign/scale there and `evtest` the
  host's virtual pad.
- **App Store screenshots** are automated — `tools/screenshots.sh all` renders the real UI at the
  required pixel sizes via a DEBUG-only shot mode; the `apple` CI workflow captures the iOS sizes on
  every main push. See the script header for details.
- Deeper design notes live in [`design/apple-stage2-presenter.md`](../../design/apple-stage2-presenter.md).

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
