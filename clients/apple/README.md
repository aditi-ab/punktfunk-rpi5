# lumen Apple client (SwiftUI) — handoff

The native macOS/iOS client for **`lumen/1`** (the post-GameStream protocol). All
networking/protocol work — QUIC control plane, UDP data plane, GF(2¹⁶) FEC, AES-GCM,
input datagrams — lives in the shared Rust core and is **done and tested**; this package
is the Swift shell: decode (VideoToolbox), present (SwiftUI), input capture.

## What exists (built + tested on the Linux host)

- **The connector**: `lumen_core::client::NativeClient` (Rust) exposed over the C ABI as
  `lumen_connect` / `lumen_connection_next_au` / `lumen_connection_send_input` /
  `lumen_connection_mode` / `lumen_connection_close` (see `include/lumen_core.h`, guarded
  by `LUMEN_FEATURE_QUIC`). **End-to-end tested through the C ABI** against an in-process
  host (`crates/lumen-host/src/m3.rs::tests::c_abi_connection_roundtrip`).
- **The host to test against**: `lumen-host m3-host --source virtual --seconds 60` on the
  Linux box (it creates a native virtual output at whatever mode the client requests and
  streams HEVC; `LUMEN_COMPOSITOR=gamescope LUMEN_GAMESCOPE_APP=vkcube` for moving content).
- **This package (SCAFFOLD — written on Linux, never compiled in Xcode)**:
  - `LumenConnection.swift` — Swift wrapper over the C ABI (AUs copied into `Data`).
  - `AnnexB.swift` — in-band VPS/SPS/PPS → `CMVideoFormatDescription`; Annex-B → AVCC
    `CMSampleBuffer` with `DisplayImmediately` set.
  - `StreamView.swift` — SwiftUI `NSViewRepresentable` over `AVSampleBufferDisplayLayer`
    (stage-1 presenter: the layer hardware-decodes compressed HEVC itself).
  - `InputCapture.swift` — `GCMouse` raw deltas + `GCKeyboard` HID→VK mapping →
    `lumen_connection_send_input`.

## Build steps (on the Mac)

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
bash scripts/build-xcframework.sh        # → clients/apple/LumenCore.xcframework
open clients/apple/Package.swift         # or add the package to an Xcode app project
```

Minimal app around it:

```swift
@main struct LumenApp: App {
    var body: some Scene { WindowGroup { ContentView() } }
}
struct ContentView: View {
    @State private var conn: LumenConnection?
    var body: some View {
        if let conn {
            StreamView(connection: conn)
                .onAppear { InputCapture(connection: conn).start() }
        } else {
            Button("Connect") {
                conn = try? LumenConnection(
                    host: "192.168.1.70", width: 2560, height: 1440, refreshHz: 120)
            }
        }
    }
}
```

## Handoff — what the next agent needs to know

1. **Expect small compile fixes.** Every Swift file is flagged SCAFFOLD: API-checked from
   documentation, never run through Xcode. Likely friction: the imported C enum spellings
   (`LUMEN_STATUS_OK` etc. — cbindgen emits `QualifiedScreamingSnakeCase`), `LumenFrame()`
   zero-init, `_pad` tuple shape on `LumenInputEvent`.
2. **ABI contract** (matches `lumen_core.h` docs): `next_au`'s pointer is valid only until
   the *next* call on that handle (we copy to `Data` immediately); one pump thread per
   connection; `send_input` is enqueue-only and thread-safe alongside it; `close` joins the
   Rust threads — never call it with a `next_au` call in flight.
3. **Decode flow**: the host opens every stream with an IDR carrying VPS/SPS/PPS in-band,
   and recovery keyframes re-send them — so "wait for the first format description, refresh
   it on every IDR" (already what `StreamView` does) is sufficient; there is no out-of-band
   extradata, ever.
4. **First-light test**: Linux box runs
   `PATH=/tmp/gamescope-src/build/src:$PATH LUMEN_COMPOSITOR=gamescope \
   LUMEN_GAMESCOPE_APP=vkcube LUMEN_ZEROCOPY=1 cargo run -rp lumen-host -- m3-host
   --source virtual --seconds 120`; Mac connects with the app. Success = the spinning
   vkcube on glass. Then mouse/keys should appear inside the gamescope session (verify
   with `LUMEN_GAMESCOPE_APP=xev` and the box-side log `/tmp/lumen-gamescope.log`).
5. **Stage 2 (after first light)**: replace `AVSampleBufferDisplayLayer` with explicit
   `VTDecompressionSession` + `CAMetalLayer` for frame-pacing control (ProMotion/120 Hz),
   and add glass-to-glass measurement (`tools/latency-probe` is the scaffold; the host
   already stamps `pts_ns` with its capture wall clock — across machines you'll need a
   clock-offset estimate from the QUIC RTT, or the probe's visual timestamp loop).
6. **Gamepads**: `GCController` → `GamepadButton`/`GamepadAxis` `LumenInputEvent`s. The
   host does NOT yet route those kinds in `m3.rs`'s injector path (mouse/keys work; the
   gamepad kinds need a `GamepadManager` hookup like the GameStream control stream has —
   small host-side task).
7. **Trust model is seed-stage**: the client accepts any host certificate
   (`endpoint::client_insecure`). Pairing + pinning is a planned lumen-core task; design it
   alongside this client's "add host" UX.
8. **iOS**: same package (`BUILD_IOS=1` for the xcframework slice); `StreamView` needs the
   `UIViewRepresentable` twin and touch→input mapping.

## Known limitations of the current host (relevant to client UX)

- `m3-host` serves **one session and exits** — fine for development; the persistent
  lumen/1 listener (serve-style) is a small host-side task.
- No audio on lumen/1 yet (the GameStream path has it; porting the Opus stream onto a
  second datagram flow is straightforward).
- Mid-stream renegotiation (resolution change without reconnect) is designed-for but not
  implemented (the Welcome is one-shot today).
