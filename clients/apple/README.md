# punktfunk Apple client (SwiftUI)

The native macOS/iOS client for **`punktfunk/1`** (the post-GameStream protocol). All
networking/protocol work — QUIC control plane, UDP data plane, GF(2¹⁶) FEC, AES-GCM,
input datagrams, Opus audio, cert pinning — lives in the shared Rust core (statically
linked as `PunktfunkCore.xcframework`); this package is the Swift shell: decode
(VideoToolbox), present (SwiftUI), input capture.

## Status — first light achieved (2026-06-10)

Validated live, Mac ↔ Linux box over the LAN: gamescope virtual output → NVENC HEVC →
`punktfunk/1` (GF(2¹⁶) FEC + AES-GCM over UDP, QUIC control) → VideoToolbox →
`AVSampleBufferDisplayLayer` on glass at 1280×720@60, with mouse/keyboard flowing back as
QUIC datagrams into the host's gamescope EIS injector (thousands of events injected during
the session). Headless variant of the same proof: `RemoteFirstLightTests` decoded 60/60
received AUs spanning 983 ms of host capture clock.

The connector underneath (`punktfunk_core::client::NativeClient` over the C ABI) carries the
full session: video AUs, **Opus audio** (`nextAudio()`), **rumble** (`nextRumble()`),
input incl. gamepads, and **cert pinning + TOFU** (`pinSHA256:`/`hostFingerprint`) — see
`m3.rs::tests::c_abi_connection_roundtrip` (three sequential sessions: TOFU, pinned
reconnect, wrong-pin rejection). The host (`punktfunk-host m3-host`) is a persistent listener:
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
    motion isn't truncated away. Buttons use GameStream ids (1=left … 5=X2); scroll is
    WHEEL_DELTA(120)-scaled.
- **`PunktfunkClient`** (the app): hosts grid (saved in UserDefaults), "+" toolbar
  sheet to add hosts, stream mode in Settings (⌘,), trust-on-first-use fingerprint prompt
  over the live-but-blurred stream → pinned reconnects, fps/Mb-s HUD. (Audio playback and
  gamepad capture are not wired into the app yet — the connector surface is there; see
  notes 5–6.)
- **Tests** (`swift test`): byte-level Annex-B units; a real-codec round trip
  (VTCompressionSession-encoded HEVC rebuilt as the host's wire shape → `AnnexB` →
  VTDecompressionSession → pixels); loopback integration against a real local host
  (`test-loopback.sh`); the remote first-light test above.

## Build / run / test (on a Mac)

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-xcframework.sh        # → clients/apple/PunktfunkCore.xcframework
cd clients/apple
swift build && swift test                # loopback/remote tests self-skip without a host
swift run PunktfunkClient                # the unbundled dev shell (CLI)
open Punktfunk.xcodeproj                 # the real app: ⌘R builds + runs Punktfunk.app

bash test-loopback.sh                    # full loopback proof: builds punktfunk-host
                                         # (synthetic source — runs on macOS), streams
                                         # byte-verified frames into the Swift client

# against the real host (Linux box, see CLAUDE.md "Running on this box") — m3-host is a
# persistent listener, reconnect at will:
#   PUNKTFUNK_COMPOSITOR=gamescope PUNKTFUNK_GAMESCOPE_APP=vkcube PUNKTFUNK_ZEROCOPY=1 \
#   cargo run -rp punktfunk-host -- m3-host --source virtual --seconds 60
PUNKTFUNK_REMOTE_HOST=<box-ip> swift test --filter RemoteFirstLightTests   # headless
PUNKTFUNK_AUTOCONNECT=<box-ip> PUNKTFUNK_MODE=1280x720x60 swift run PunktfunkClient # on glass
```

## Xcode project (`Punktfunk.xcodeproj`)

The app target **Punktfunk** wraps the same sources as the `swift run` shell
(`Sources/PunktfunkClient`, a synchronized folder — no duplication) plus `App/` (asset
catalog) and links `PunktfunkKit` from the local package. Generated Info.plist, ad-hoc
signing, bundle id `io.unom.punktfunk`. Notes:

- **App icon**: `App/Assets.xcassets` ships an empty `AppIcon` slot. For an Icon Composer
  `.icon`: add the file to the project (target Punktfunk), set it as the App Icon in the
  target's General tab, and delete the placeholder `AppIcon.appiconset`. Heads-up: CLI
  `actool` (Xcode 26.5) crashed compiling `punktfunk_Logo.icon` — if Xcode does the same,
  suspect the icon bundle (it has a duplicate-named layer, "…Layer-3 2.svg"), not the
  project.
- **Tests from Xcode**: the package tests run with `swift test`; to get them on ⌘U, add
  `PunktfunkKitTests` once via Edit Scheme → Test → + (Xcode persists it into the shared
  scheme — a hand-written package-test reference doesn't resolve headlessly).
- `xcodebuild -project Punktfunk.xcodeproj -scheme Punktfunk build` works headlessly.

## Notes for whoever picks this up next

1. **cbindgen import quirk** (the predicted "small compile fixes", now fixed): the
   C17-compatible header spells `PunktfunkStatus`/`PunktfunkInputKind` as integer typedefs while
   the enum *constants* import into Swift as a distinct same-named type — bridge with
   `.rawValue` (see the top of `PunktfunkConnection.swift`). Don't fight the generated header.
2. **ABI contract**: one video pump thread per connection, plus optionally one *separate*
   audio drain thread for `nextAudio()`/`nextRumble()` (the core keeps per-plane borrow
   slots, so the planes never alias); `send()` is enqueue-only and safe alongside all of
   them. The wrapper's per-plane locks make `close()` safe from anywhere (it waits out
   in-flight polls, ≤ their timeouts).
3. **Decode flow**: the host opens every stream with an IDR carrying VPS/SPS/PPS in-band
   and recovery keyframes re-send them — "refresh the format description on every IDR"
   (what `StreamView` does) is sufficient; there is no out-of-band extradata, ever.
4. **Stage 2 (next)**: explicit `VTDecompressionSession` + `CAMetalLayer` for frame-pacing
   control (ProMotion/120 Hz), glass-to-glass measurement via `tools/latency-probe` (the
   host stamps `pts_ns` with its capture wall clock; across machines you need a clock
   offset estimate from the QUIC RTT).
5. **Audio**: `nextAudio()` yields raw Opus packets (48 kHz stereo, one 5 ms frame each,
   sequence-numbered). Decode with libopus or `AVAudioConverter`/`kAudioFormatOpus` into an
   `AVAudioEngine` source node; conceal gaps (drop/dup) rather than blocking — the Rust
   side buffers 320 ms and drops the newest packet when the puller lags. Wall-clock
   `ptsNs` shares the host clock with video AUs for A/V sync. Wiring this into
   `PunktfunkClient` is the next app-side task.
6. **Gamepads**: `GCController` → `.gamepadButton(...)`/`.gamepadAxis(...)` events (wire
   contract documented on the constructors; the host accumulates them into a virtual
   Xbox 360 pad). Poll `nextRumble()` and feed `GCDeviceHaptics` for force feedback.
   Client-side capture isn't in `InputCapture` yet.
7. **Trust**: connect once with `pinSHA256: nil` (TOFU), persist `hostFingerprint` keyed
   by host, pass it on every later connect — a mismatch throws `.connectFailed`. The host
   logs its fingerprint at startup ("clients pin this fingerprint") for out-of-band
   verification UX; a PIN-style pairing ceremony is a later punktfunk-core task.
   `PunktfunkClient` implements exactly this: explicit fingerprint confirmation on first
   connect (input/cursor capture held back until confirmed), pin stored per host
   (`HostStore`), "Forget Identity" in the card's context menu for legitimate host
   reinstalls. Note the OTHER direction is still open: the host authorizes no one — any
   client that reaches the port gets a session (fine on a LAN, not on the internet).
8. **Input capture caveats** (stage 1): GC handlers only fire while the app has focus —
   on focus loss `InputCapture` auto-releases everything still held (keys + buttons) so
   nothing sticks down host-side. While the stream has focus the LOCAL cursor is hidden
   and frozen mid-view (`CursorCapture` in StreamView.swift — the host renders its own
   cursor; the local one diverges from it and a stray click would focus another app);
   Cmd+Tab frees it, ⌘D disconnects. Local shortcuts (⌘-anything) still also reach the
   host; a capture toggle is a small follow-up. One live capture per process (the GC
   mouse/keyboard singletons have a single handler slot — ownership is tracked so a stale
   capture's stop() can't clobber a newer one).
9. **iOS**: same package (`BUILD_IOS=1` for the xcframework slice); `StreamView` needs the
   `UIViewRepresentable` twin and touch→input mapping.

## Known limitations of the current host (relevant to client UX)

- One session **at a time** (the listener is persistent, but a second concurrent client
  waits in the accept queue until the current session ends — the virtual output and
  encoder are single-tenant).
- Mid-stream renegotiation (resolution change without reconnect) is designed-for but not
  implemented (the Welcome is one-shot today).
- Host-side gamepad injection needs `/dev/uinput` access on the box (udev rule from
  `docs/linux-setup.md`).
