# punktfunk Apple client (SwiftUI)

The native macOS/iOS client for **`punktfunk/1`** (the post-GameStream protocol). All
networking/protocol work — QUIC control plane, UDP data plane, GF(2¹⁶) FEC, AES-GCM,
input datagrams, Opus audio, cert pinning — lives in the shared Rust core (statically
linked as `PunktfunkCore.xcframework`); this package is the Swift shell: decode
(VideoToolbox), present (SwiftUI), input capture.

## Status — working client (macOS, with iOS / tvOS in the shared build)

A full streaming client: VideoToolbox HEVC decode, controllers incl. DualSense feedback, host
discovery, PIN pairing, and a network speed test. The lower-latency **stage-2 presenter**
(`VTDecompressionSession` → `CAMetalLayer`) is built and opt-in (Settings → Presenter); see below.

First light was achieved 2026-06-10 — validated live, Mac ↔ a Linux host over the LAN: gamescope
virtual output → NVENC HEVC →
`punktfunk/1` (GF(2¹⁶) FEC + AES-GCM over UDP, QUIC control) → VideoToolbox →
`AVSampleBufferDisplayLayer` on glass at 1280×720@60, with mouse/keyboard flowing back as
QUIC datagrams into the host's gamescope EIS injector (thousands of events injected during
the session). Headless variant of the same proof: `RemoteFirstLightTests` decoded 60/60
received AUs spanning 983 ms of host capture clock.

The connector underneath (`punktfunk_core::client::NativeClient` over the C ABI) carries the
full session: video AUs, **Opus audio** (`nextAudio()`), **rumble** (`nextRumble()`),
**DualSense feedback** (`nextHidOutput()` — lightbar, player LEDs, adaptive-trigger
effects), input incl. gamepads + DualSense touchpad/motion (`sendTouchpad`/`sendMotion`),
and **cert pinning + TOFU** (`pinSHA256:`/`hostFingerprint`) — see
`punktfunk1.rs::tests::c_abi_connection_roundtrip` (three sequential sessions: TOFU, pinned
reconnect, wrong-pin rejection). The host (`punktfunk-host punktfunk1-host`) is a persistent listener:
reconnect at will during development.

What's here, all compiled and tested on macOS (Xcode 26.5 / Swift 6.3):

- **`PunktfunkKit`** (library)
  - `PunktfunkConnection.swift` — wrapper over the C ABI. AUs/audio are copied into `Data`
    (the C pointer is only valid until the next call of the same kind). `close()` is safe
    from any thread: per-plane locks enforce the C contract ("never close with a
    `next_au`/`next_audio` in flight") instead of leaving it to callers. Pinning + TOFU
    via `pinSHA256:`/`hostFingerprint`.
  - `AnnexB.swift` — in-band VPS/SPS/PPS → `CMVideoFormatDescription`; Annex-B → AVCC
    `CMSampleBuffer` with `DisplayImmediately` set.
  - `StreamView.swift` — SwiftUI `NSViewRepresentable` over `AVSampleBufferDisplayLayer`
    (stage-1 presenter: the layer hardware-decodes compressed HEVC itself). One pump
    thread per view, token-cancelled so reconnects can't double-pump.
  - `InputCapture.swift` — `GCMouse` raw deltas + `GCKeyboard` HID→VK mapping (the host's
    `vk_to_evdev` consumes Windows VKs), with fractional-delta accumulation so sub-pixel
    motion isn't truncated away. Buttons use GameStream ids (1=left … 5=X2). Scroll is
    WHEEL_DELTA(120)-scaled: macOS via the stream view's `scrollWheel` override, iPad via
    GCMouse's scroll dpad when pointer-locked and a scroll-only `UIPanGestureRecognizer`
    otherwise (trackpad gestures never reach GC's scroll dpad).
  - `GamepadManager.swift` — app-lifetime controller discovery + selection (`.shared`):
    watches `GCController` connect/disconnect, fingerprints each pad for the Settings UI
    (name, capabilities, battery), and selects the ONE controller forwarded to the host
    (user pin via "Use controller", else most recently connected extended gamepad).
  - `GamepadCapture.swift` — the active controller → wire: snapshot-diff over
    `GCExtendedGamepad` into incremental `gamepadButton`/`gamepadAxis` events (pad 0),
    plus DualSense touchpad contacts and ~250 Hz motion samples on the rich-input plane
    (the GC→DualSense unit conversions live in `GamepadWire`, one place). Held state is
    released on the wire on controller switch / app deactivation / stop.
  - `GamepadFeedback.swift` + `DualSenseTriggerEffect.swift` — host feedback → the real
    controller: one drain thread for `nextRumble()` (→ `CHHapticEngine` per handle
    locality) and `nextHidOutput()` (lightbar → `GCDeviceLight`, player LEDs →
    `playerIndex`, adaptive-trigger effect blocks → a total, table-driven parser →
    `GCDualSenseAdaptiveTrigger`, exact for the 10-zone positional modes).
  - `HostDiscovery.swift` — LAN auto-discovery: an `NWBrowser` over `_punktfunk._udp`
    (the host's `crate::discovery` mDNS advert), resolving each service to an IP:port via a
    throwaway `NWConnection` and parsing the TXT (`fp` advisory cert fingerprint, `pair`,
    stable `id`). iOS/tvOS need `NSBonjourServices` (`Config/Info.plist`) or the system
    blocks the browse.
- **`PunktfunkClient`** (the app): hosts grid (saved in UserDefaults) with an **On this
  network** section listing mDNS-discovered hosts (tap to save + connect, or pair if the
  host requires it), "+" toolbar sheet to add hosts manually, stream mode in Settings (⌘,),
  two trust flows — the
  trust-on-first-use fingerprint prompt over the live-but-blurred stream, and SPAKE2 PIN
  pairing (`PairSheet`, from a host card's context menu or the trust prompt;
  `ClientIdentityStore` keeps the client identity in the Keychain and presents it on
  every connect) — then pinned reconnects, fps/Mb-s HUD + a **capture→client-receipt latency**
  line (`LatencyMeter`, p50/p95): the AU `pts_ns` (host capture clock) to the instant the client
  received it, **skew-corrected** across machines via `PunktfunkConnection.clockOffsetNs` (the
  connect-time wall-clock handshake, `punktfunk_connection_clock_offset_ns`). It excludes the
  layer's decode+present (stage-1 `AVSampleBufferDisplayLayer` has no per-frame present callback);
  the opt-in **stage-2 presenter** (Settings → Presenter) adds a **capture→present**
  (glass-to-glass) line via explicit decode + a Metal/display-link present. Settings also picks the HOST
  compositor (KWin/wlroots/Mutter/gamescope, default automatic — the host honors it
  only if that backend is available there) and has a **Controllers** section: every
  detected controller (capability glyphs, battery, "In use" badge), which one to forward
  ("Use controller", default automatic), and the virtual pad type the host creates
  ("Controller type": Automatic / Xbox 360 / DualSense — Automatic matches the physical
  pad; resolved at connect time, the host pad is fixed per session). Gamepad capture +
  feedback run with streaming (`SessionModel` owns them, same trust gate as audio).
  Settings also sets the **Bitrate** (Automatic toggle = host default; manual is a
  log-scale slider, 2 Mbps – 3 Gbps, snapped to two significant figures — above 1 Gbps
  an inline warning says to run a speed test first; tvOS uses a preset picker instead,
  Slider doesn't exist there; negotiated via the Hello on every connect), and a host
  card's context menu offers **"Test Network Speed…"** (`SpeedTestSheet`): connects, has
  the host burst probe filler over the real data plane (up to the host's 3 Gbps probe
  ceiling for 2 s, roadmap §9),
  shows measured goodput · loss · a recommended bitrate (≈70% of measured), and applies
  it in one tap. The streaming **statistics overlay** can be turned off and moved to any
  corner (Settings → Display → Statistics, `DefaultsKey.hudEnabled`/`hudPlacement`), and
  toggled live with **⌘⇧S** — a Scene-level **"Stream" menu** (`StreamCommands`) that also
  carries **Disconnect ⌘D**, so disconnect survives the HUD being hidden (on iOS a small
  exit chip appears instead; on tvOS the Siri-Remote Menu button still disconnects). The
  macOS Settings window is a **tabbed preferences pane** (General / Display / Audio /
  Controllers / Advanced) — the sections are shared with the iOS single-Form layout and the
  tvOS pushed-picker layout, defined once each.
- **Tests** (`swift test`): byte-level Annex-B units; a real-codec round trip
  (VTCompressionSession-encoded HEVC rebuilt as the host's wire shape → `AnnexB` →
  VTDecompressionSession → pixels); table-driven DualSense trigger-effect parsing
  (`DualSenseTriggerEffectTests`) and the gamepad wire conversions
  (`GamepadWireTests`); loopback integration against real local hosts
  (`test-loopback.sh` — stream round trip incl. gamepad/touchpad/motion sends, a
  host-scripted feedback burst asserted on the rumble + HID-output planes
  (`PUNKTFUNK_TEST_FEEDBACK=1`), the bitrate-negotiation echo and a real 20 Mbps
  bandwidth probe, plus the PIN pairing ceremony and the `--require-pairing` gate
  against a second, armed host); the remote first-light test above.

## Build / run / test (on a Mac)

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-xcframework.sh        # → clients/apple/PunktfunkCore.xcframework
# + BUILD_IOS=1 for the iOS slices (rustup target add aarch64-apple-ios{,-sim} x86_64-apple-ios)
# + BUILD_TVOS=1 for tvOS — TIER-3 Rust targets, built from source:
#   rustup toolchain install nightly && rustup component add rust-src --toolchain nightly
cd clients/apple
swift build && swift test                # loopback/remote tests self-skip without a host
swift run PunktfunkClient                # the unbundled dev shell (CLI)
open Punktfunk.xcodeproj                 # the real app: ⌘R builds + runs Punktfunk.app

bash test-loopback.sh                    # full loopback proof: builds punktfunk-host
                                         # (synthetic source — runs on macOS), streams
                                         # byte-verified frames into the Swift client

# against the real host (Linux box, see CLAUDE.md "Running on this box") — punktfunk1-host is a
# persistent listener, reconnect at will:
#   PUNKTFUNK_COMPOSITOR=gamescope PUNKTFUNK_GAMESCOPE_APP=vkcube PUNKTFUNK_ZEROCOPY=1 \
#   cargo run -rp punktfunk-host -- punktfunk1-host --source virtual --seconds 60
PUNKTFUNK_REMOTE_HOST=<box-ip> swift test --filter RemoteFirstLightTests   # headless
#   (+ PUNKTFUNK_REMOTE_PORT / PUNKTFUNK_REMOTE_COMPOSITOR=gamescope|kwin|… /
#    PUNKTFUNK_REMOTE_PIN=<arming-pin> for the remote pairing test)
PUNKTFUNK_AUTOCONNECT=<box-ip> PUNKTFUNK_MODE=1280x720x60 swift run PunktfunkClient # on glass
```

## Xcode project (`Punktfunk.xcodeproj`)

The app target **Punktfunk** wraps the same sources as the `swift run` shell
(`Sources/PunktfunkClient`, a synchronized folder — no duplication) plus `App/` (asset
catalog) and links `PunktfunkKit` from the local package. Generated Info.plist, ad-hoc
signing, bundle id `io.unom.punktfunk`. Notes:

- **Entitlements (sandbox)**: the macOS target uses
  `Config/Punktfunk-macOS.entitlements`; iOS/tvOS use the shared
  `Config/Punktfunk.entitlements`. The macOS app is **App-Sandboxed** (mandatory for the Mac
  App Store/TestFlight, and used for the Developer ID DMG too so the local build matches what
  ships): `com.apple.security.app-sandbox`, `network.client` + **`network.server`** (the
  sandbox gates `bind()`; quinn + the raw-UDP plane both bind, so receive breaks without it),
  `device.audio-input` (mic), `device.bluetooth` + `device.usb` (GameController over BT/USB),
  and the existing `keychain-access-groups`. `app-sandbox` is macOS-only — keep it OUT of the
  shared iOS/tvOS file (it fails upload validation there). Verify a build is sandboxed with
  `codesign -d --entitlements :- <built .app>`. Heads-up: `device.usb` draws some App Review
  scrutiny — justify it in the review notes ("reads input from USB game controllers").
- **App icon**: `App/Assets.xcassets` ships an empty `AppIcon` slot. For an Icon Composer
  `.icon`: add the file to the project (target Punktfunk), set it as the App Icon in the
  target's General tab, and delete the placeholder `AppIcon.appiconset`. Heads-up: CLI
  `actool` (Xcode 26.5) crashed compiling `punktfunk_Logo.icon` — if Xcode does the same,
  suspect the icon bundle (it has a duplicate-named layer, "…Layer-3 2.svg"), not the
  project.
- **Tests from Xcode**: the package tests run with `swift test`; to get them on ⌘U, add
  `PunktfunkKitTests` once via Edit Scheme → Test → + (Xcode persists it into the shared
  scheme — a hand-written package-test reference doesn't resolve headlessly).
- `xcodebuild -project Punktfunk.xcodeproj -scheme Punktfunk build` works headlessly;
  same for `-scheme Punktfunk-iOS -destination 'generic/platform=iOS Simulator'` (run it
  in a simulator via `xcrun simctl install/launch` — `SIMCTL_CHILD_PUNKTFUNK_AUTOCONNECT=…`
  passes the dev autoconnect env through).

## App Store screenshots

Automated, faithful screenshots of the real UI for App Store Connect — one set per platform at
exactly the accepted pixel sizes. Driver: **`tools/screenshots.sh`**.

```sh
tools/screenshots.sh all                 # macOS + (if full Xcode) iOS, iPadOS, tvOS → ./screenshots
tools/screenshots.sh macos               # just macOS
OUT=~/Desktop/shots tools/screenshots.sh ios ipad tvos
PUNKTFUNK_SHOT_HERO=~/frame.png tools/screenshots.sh ios   # real captured frame behind the hero
```

How it works: the app has a DEBUG-only **shot mode** (`Sources/PunktfunkClient/Screenshots/`).
Launched with `PUNKTFUNK_SHOT_SCENE=<name>` it renders **one** mock-populated screen full-bleed
(`ScreenshotHostView`) instead of `ContentView`, then the OS screenshots the *real, fully-rendered*
window — `screencapture` on macOS, `xcrun simctl io booted screenshot` on the Simulators. The five
scenes (`ShotScenes.all`): `01-stream` (the stream hero — a synthetic frame + the glass HUD, since
`StreamView` needs a live connection), `02-hosts`, `03-pair`, `04-trust`, `05-settings`. Mock data
is in `ShotMock`; nothing touches a host.

Output pixels are App Store Connect's required/largest sizes (Apple auto-derives the smaller ones):
`mac` 2880×1800 · `iphone-6.9` 1320×2868 (hero 2868×1320) · `ipad-13` 2064×2752 (hero 2752×2064) ·
`appletv` 1920×1080.

Why not `ImageRenderer` (the obvious offscreen route)? It can't rasterize this app's chrome —
`NavigationStack`, `Form`/`TabView`, and Liquid-Glass/`NSVisualEffect` materials all render black or
SwiftUI's "can't render" placeholder. Capturing the live window/Simulator avoids that entirely.

Requirements / gotchas:
- **macOS**: only the Swift toolchain is needed, **plus a one-time Screen Recording grant** for
  your terminal (System Settings → Privacy & Security → Screen Recording) — without it
  `screencapture -l` fails with "could not create image from window". (A no-permission fallback,
  `PUNKTFUNK_SHOT_SELFCAPTURE=<dir>`, uses `cacheDisplay` — but it omits material blur and can't
  read `ScrollView` content, so it's for quick checks, not submission.)
- **iOS/iPadOS/tvOS**: needs **full Xcode** (xcodebuild + Simulators), not just Command Line Tools,
  and the matching device Simulators installed (iPhone 16 Pro Max, iPad Pro 13", Apple TV). Run it
  on a full-Xcode Mac (e.g. the `macos-arm64` CI mini).
- The hero defaults to a synthetic synthwave frame — set `PUNKTFUNK_SHOT_HERO` to a real captured
  frame for a production-quality lead screenshot.

**CI**: the `apple` workflow's **`screenshots`** job runs this on the `macos-arm64` runner on every
main push + manual dispatch (skipped on PRs), and attaches the result as a single zip artifact,
**`punktfunk-appstore-screenshots`** (download it from the run's Artifacts). It's best-effort and
isolated from the build/test job — a missing Simulator runtime or a runner without the Screen
Recording grant only drops that platform, never reds the build. (The macOS window capture in
particular needs that grant on the runner; the Simulator shots don't.)

## Notes for whoever picks this up next

1. **cbindgen import quirk** (the predicted "small compile fixes", now fixed): the
   C17-compatible header spells `PunktfunkStatus`/`PunktfunkInputKind` as integer typedefs while
   the enum *constants* import into Swift as a distinct same-named type — bridge with
   `.rawValue` (see the top of `PunktfunkConnection.swift`). Don't fight the generated header.
2. **ABI contract**: one video pump thread per connection, plus optionally one *separate*
   audio drain thread for `nextAudio()` and one feedback drain thread for
   `nextRumble()`/`nextHidOutput()` (the core keeps per-plane borrow slots, so the planes
   never alias; rumble + HID-output are two planes drained sequentially by the one
   feedback thread); `send()` is enqueue-only and safe alongside all of them. The
   wrapper's per-plane locks make `close()` safe from anywhere (it waits out in-flight
   polls, ≤ their timeouts).
3. **Decode flow**: the host opens every stream with an IDR carrying VPS/SPS/PPS in-band
   and recovery keyframes re-send them — "refresh the format description on every IDR"
   (what `StreamView` does) is sufficient; there is no out-of-band extradata, ever.
4. **Stage 2 — built, opt-in (`punktfunk.presenter == "stage2"`, default stage 1).** Explicit
   `VTDecompressionSession` decode (`VideoDecoder`) → a `CAMetalLayer` + display-link present
   (`MetalVideoPresenter`/`Stage2Pipeline`), hosted as a sublayer by the same `StreamView`s with
   input capture + HUD unchanged. It adds a **capture→present** (glass-to-glass, modulo the host
   render→capture term) HUD line, skew-corrected via `PunktfunkConnection.clockOffsetNs`. The
   decode half is unit-tested (`testVideoDecoderAsyncCallbackDeliversPixels`); the Metal present
   is display-bound — **validate live** (flip the Settings "Presenter" picker, watch the HUD
   number and that the image looks right) before making it the default. 10-bit/HDR + a smoothing
   pacer are later. Plan: `docs-site/content/docs/apple-stage2-presenter.md`.
5. **Audio — wired, both directions.** Playback: `SessionAudio` drains `nextAudio()`
   on its own thread, decodes through CoreAudio's built-in Opus codec (`OpusCodec.swift`
   — kAudioFormatOpus, no bundled libopus; round-trip unit-tested) into a priming
   jitter ring feeding an `AVAudioSourceNode`. Mic: a second engine taps the input
   device, resamples to 48 kHz stereo, Opus-encodes 20 ms chunks and `sendMic()`s them
   (the host's virtual PipeWire source accepts any frame size ≤ 120 ms). Speaker/mic
   are chosen in Settings (`AudioDevices.swift` — persisted by UID; "System default"
   leaves the engines unpinned so they follow macOS device changes), mic on/off toggle
   included; the app asks for mic permission on first use
   (NSMicrophoneUsageDescription is in the Xcode target). A/V sync and packet-loss
   concealment beyond silence-fill are still open (AudioPacket.seq/ptsNs carry what's
   needed). Decode with libopus or `AVAudioConverter`/`kAudioFormatOpus` into an
   `AVAudioEngine` source node; conceal gaps (drop/dup) rather than blocking — the Rust
   side buffers 320 ms and drops the newest packet when the puller lags. Wall-clock
   `ptsNs` shares the host clock with video AUs for A/V sync. Wiring this into
   `PunktfunkClient` is the next app-side task.
6. **Gamepads — wired end to end.** Exactly ONE controller (the `GamepadManager`
   selection) forwards as pad 0; the host accumulates the incremental events into a
   virtual pad whose TYPE the client negotiates in the Hello (`gamepad:` connect
   parameter, echoed resolved in `resolvedGamepad` — Automatic resolves from the physical
   pad at connect time; host precedence: explicit client choice > host `PUNKTFUNK_GAMEPAD`
   env > Xbox 360). A DualSense session carries the full feel: adaptive-trigger blocks
   (`DualSenseTriggerEffect.parse` — mode bytes per the community convention
   (Nielk1/ds5w/inputtino), total, unknown → `.off`), lightbar, player LEDs, touchpad,
   motion. **Motion scale constants** (`GamepadWire.gyroLSBPerRadS` = 20 LSB per deg/s,
   `accelLSBPerG` = 10000) are derived from hid-playstation's math over the host's fixed
   calibration blob, not yet live-verified — if gyro/accel feel wrong in a real game,
   correct sign/scale in `GamepadCapture.forwardMotion`/`GamepadWire` and `evtest` the
   host's virtual pad. Twin identical controllers share a fingerprint base, so a manual
   pin can swap between them across reconnects (documented in the Settings footer).
7. **Trust — the full ceremony exists now (SPAKE2).** `generateIdentity()` once (persist
   both PEMs in the Keychain), then `pair(host:identity:pin:name:)` with the 4-digit PIN
   the host prints when it ARMS pairing (`--allow-pairing`/`--require-pairing`; one PIN
   per arming window, surfaced in the host's web console — port 3000 → Pairing — and
   printed at startup; the user reads it before pairing). Returns the
   host's VERIFIED fingerprint; persist it and pass `pinSHA256:` + `identity:` to every
   connect. Pairing is a real PAKE: a wrong PIN gets ONE online guess (no offline
   dictionary attack), throwing `.wrongPIN`; a wrong-size pin throws `.invalidPin`. `PunktfunkClient` implements both flows:
   the TOFU fingerprint sheet keeps working against hosts not running
   `--require-pairing`, and the PIN ceremony is wired in — `ClientIdentityStore`
   (Keychain) on every connect, `PairSheet` from a host card's context menu or the trust
   prompt's "Pair with PIN instead…" (the host's accept loop is sequential, so that path
   drops the live session before pairing). With `--require-pairing` the host now
   authorizes clients too (the "other direction" is no longer open, opt-in per host);
   the whole gate is regression-tested in `testPairingCeremonyAndRequirePairingGate`.
7b. **Resize without reconnect**: `requestMode(width:height:refreshHz:)` mid-stream —
   the host rebuilds at the new mode in ~90 ms; the first new-mode AU is an IDR with
   fresh parameter sets (the refresh-on-IDR decode flow handles it untouched) and
   `currentMode()` reflects the switch. Wire it to window-resize events.
8. **Input capture** (stage 1): capture is a deliberate, reversible STATE owned by
   `StreamLayerView`, Moonlight-style. Engaged when the stream starts / trust is
   confirmed and when the user clicks into the video (that click is suppressed toward
   the host); released by ⌘⎋ (toggles) or focus loss; NEVER engaged by mere app
   activation — activating clicks may be title-bar drags or resizes, which used to get
   their cursor warped away mid-drag. While captured: the local cursor is hidden +
   frozen mid-view (the host renders its own), all input is forwarded, and the view
   consumes key events as first responder so unhandled keyDowns don't beep — ⌘-combos
   still work locally (⌘D disconnect, ⌘Q) *and* reach the host via GC. While released:
   nothing is forwarded (`InputCapture.forwarding` gates the GC handlers; held
   keys/buttons are flushed host-side on release so nothing sticks down), the cursor is
   free, and the HUD shows "Click the stream to capture input". GC handlers only fire
   while the app has focus, and focus loss also auto-releases everything held. One live capture per process (the GC
   mouse/keyboard singletons have a single handler slot — ownership is tracked so a stale
   capture's stop() can't clobber a newer one).
9. **iOS/iPadOS — ported and first-lit** (iPad simulator ↔ the real host, 60 fps).
   `BUILD_IOS=1 bash scripts/build-xcframework.sh` builds device + universal-simulator
   slices; the Xcode project has a second target, **Punktfunk-iOS**, sharing the same
   synchronized sources. The iOS `StreamView` (StreamViewIOS.swift — same name/signature
   as the macOS one, so the SwiftUI shell is identical) hosts the shared `StreamPump` in
   a view controller for `prefersPointerLocked`: with a hardware mouse/trackpad that is
   the iPadOS cursor capture (system honors it fullscreen-and-frontmost; in Stage
   Manager it degrades to absolute-mouse forwarding). Input is routed by kind: DIRECT
   fingers / Pencil are touches (each gets a wire touch id, coordinates mapped through the
   aspect-fit letterbox into host-mode pixels — surface == host mode, so the host rescale is
   the identity), while a mouse/trackpad is a MOUSE — pointer-LOCKED it is GCMouse relative
   deltas; unlocked it is absolute moves + buttons + scroll over the UIKit pointer path
   (hover + `.indirectPointer` touches), the local cursor staying visible so you can aim. An
   indirect pointer is never sent as a touch. Touch is gated on trust (not forwarded under
   the TOFU prompt), and returning to the foreground restores the capture you had on leaving.
   `InputCapture` is cross-platform (GC works the same on iPadOS; ⌘⎋ is detected from
   the HID stream there); audio routes via `AVAudioSession` (the Settings device
   pickers are macOS-only). For the iPad-with-external-display setup: the target
   enables multiple scenes + indirect input events — on Stage Manager iPads, drag the
   punktfunk window onto the external screen and the stream runs there with full
   keyboard/mouse/touch. While streaming the session is immersive (edge-to-edge,
   status bar + home indicator hidden) and the iPadOS cursor is hidden over the video only
   while the scene is actually pointer-LOCKED (`UIPointerInteraction` `.hidden()`); when the
   lock isn't held it stays visible and the mouse forwards as an absolute cursor instead; on
   iOS first run the stream mode defaults to the device's native screen so the video
   fills the display. **tvOS** runs the same app (target **Punktfunk-tvOS**, first-lit
   in the Apple TV simulator at 720p60): playback-only audio (no mic on tvOS),
   focus-driven UI (`.card` host tiles), no kb/mouse capture yet — input lands with
   gamepad support, the natural tvOS input anyway. While streaming there is NO focusable
   control (a focusable Disconnect button would let the focus engine eat the controller's A
   before the host sees it); the Siri Remote's **Menu** button disconnects (`.onExitCommand`).
   Core slices are tier-3 Rust targets (see Build above). Known gaps: true pointer LOCK (`prefersPointerLocked`) isn't
   consulted through UIHostingController, so the hidden cursor can still drift onto a
   second screen (fixing it means putting the controller into the UIKit presentation
   chain); and
   AVAudioSession interruptions (calls, Siri) don't auto-restart the audio engines yet
   (reconnect recovers).

## Known limitations of the current host (relevant to client UX)

- One session **at a time** (the listener is persistent, but a second concurrent client
  waits in the accept queue until the current session ends — the virtual output and
  encoder are single-tenant).
- Mid-stream renegotiation (resolution change without reconnect) is designed-for but not
  implemented (the Welcome is one-shot today).
- Host-side gamepad injection needs `/dev/uinput` access on the box (udev rule from
  `docs/linux-setup.md`).
