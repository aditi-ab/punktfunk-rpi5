# punktfunk — Android client (phone & TV)

The native **Android** app for streaming a punktfunk host to your phone, tablet, or Android TV. A
Compose app that finds hosts on your network, pairs with a PIN, and streams at the display's own
resolution — with hardware HEVC decode, HDR10, and controller support, built for both touch and the
couch (D-pad / gamepad focus navigation).

## Features

- **Hardware decode** — NDK `AMediaCodec` HEVC → `SurfaceView`, including **HDR10** (Main10 /
  BT.2020 PQ), with low-latency tuning and a live stats HUD.
- **Audio both ways** — Opus + AAudio playback with a jitter ring, plus mic uplink to the host.
- **Controller support** — buttons + axes with rumble and HID feedback (lightbar / adaptive
  triggers); D-pad / gamepad focus navigation for TV and phone.
- **Find hosts automatically** — native mDNS discovery; first connect does a one-time **SPAKE2 PIN
  pairing** (or TOFU on trusted LANs), then reconnects on a Keystore-wrapped, pinned identity.
- **Compose UI** — Connect / Settings / Stream screens with Material You theming.

Built for `arm64-v8a` + `armeabi-v7a` + `x86_64` — the 32-bit `armeabi-v7a` slice is what keeps the
app installable on the many 32-bit Google TV / Android TV streamers (Walmart onn. 4K, Chromecast with
Google TV, budget Amlogic boxes) that otherwise reject a 64-bit-only build as "not compatible".

## Get it

Published to **Google Play (Open Testing)** — join via the
[public opt-in link](https://play.google.com/apps/testing/io.unom.punktfunk) or the
[Discord](https://discord.gg/kaPNvzMuGU). Per-device setup and pairing:
**[docs.punktfunk.unom.io/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.

## How it's built — Rust-heavy

Kotlin can't `import` the cbindgen C header the way Swift can, so a native bridge is unavoidable. We
write it in **Rust** and link `punktfunk-core` directly — so the Android client reuses the Linux
client's orchestration (audio jitter ring, VK keymap inverse, latency/skew math, capture state
machine, trust logic) instead of re-porting it into Kotlin.

| Side | Owns |
|------|------|
| **Rust** (`native/` → `libpunktfunk_android.so`) | the JNI seam, `NativeClient` (QUIC control + UDP data plane), AnnexB → `AMediaCodec` decode (incl. HDR10), Opus + AAudio audio + mic, controller feedback, latency math, trust/pairing, `mdns-sd` discovery |
| **Kotlin** (`app/`, `kit/`) | Compose UI, `SurfaceView` lifecycle, input capture, the Wi-Fi `MulticastLock` + permission UX, Keystore identity |

The single seam is `io.unom.punktfunk.kit.NativeBridge` ⇄ `Java_io_unom_punktfunk_kit_NativeBridge_*`.

```
native/           Rust cdylib (workspace member) — links punktfunk-core directly
  src/lib.rs        crate doc · JNI_OnLoad · version probes
  src/session/      session lifecycle: connect/pair + trust, plane start/stop, input shims
  src/decode.rs     AnnexB → AMediaCodec HEVC hardware decode → SurfaceView (incl. HDR10)
  src/audio.rs · src/mic.rs   Opus + AAudio playback / mic uplink
  src/feedback.rs · src/stats.rs   rumble + HID feedback; live video stats
  src/discovery.rs  native mdns-sd browse of the host's _punktfunk._udp advert
app/              :app — Compose UI: Connect / Settings / Stream (phone + TV)
kit/              :kit — NativeBridge · native mDNS discovery · Gamepad · Keymap · Keystore identity
```

## Build & run

**Prerequisites:** Android SDK + **NDK r30** (`30.0.14904198`), `platforms;android-37.0`,
`build-tools;37.0.0`, **`cmake;3.22.1`** (builds libopus); **JDK 21** (AGP 9.2 runs on JDK 17–21, not
a newer default); Rust with `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android` and
`cargo install cargo-ndk`. Toolchain is pinned (AGP 9.2 · Gradle 9.4.1 · Kotlin 2.3.21 · Compose BOM
2026.05.01 · compileSdk 37 · minSdk 28).

**Android Studio:** open `clients/android` — it uses its bundled JBR 21, and the `cargoNdk*` task
builds the `.so` as part of the normal build.

**CLI** (point Gradle at JDK 21 if your machine default is newer):

```sh
export JAVA_HOME="$(/usr/libexec/java_home -v 21)"   # or your Temurin 21 path
cd clients/android
./gradlew :app:assembleDebug     # cargo-ndk cross-compiles libpunktfunk_android.so first
./gradlew :app:installDebug      # onto a running emulator/device
# emulators from env setup:  emulator -avd pf_phone   |   emulator -avd pf_tv
```

The debug APK lands in `app/build/outputs/apk/debug/`. Launch it, pick a host, pair, and stream.

## Related

- **[Documentation](https://docs.punktfunk.unom.io)** — quick start, pairing, troubleshooting
- **[Project README](../../README.md)** — the host, the other clients, and how it all fits together
