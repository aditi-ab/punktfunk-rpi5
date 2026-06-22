# punktfunk Android client

Native Android client for **punktfunk/1**, targeting **phone + TV** (Compose, D-pad + touch).

## Architecture — Rust-heavy (like the Linux client, not thin-native like Apple)

Kotlin cannot `import` the cbindgen C header the way Swift can, so a native bridge is unavoidable.
We write it in **Rust** and link `punktfunk-core` directly — so the Android client reuses the Linux
client's orchestration (audio jitter ring, VK keymap inverse, latency/skew math, capture state
machine, trust logic) instead of re-porting it into Kotlin.

| Side | Owns |
|------|------|
| **Rust** (`clients/android/native` → `libpunktfunk_android.so`) | the JNI seam, `NativeClient` (QUIC control + UDP data plane), AnnexB→`AMediaCodec` decode, Opus+Oboe audio, VK keymap, latency math, trust/pairing, **mDNS discovery** (`mdns-sd`, the same browse the Linux/Windows clients use) |
| **Kotlin** (`clients/android`) | Compose UI (host grid / settings / stream), `SurfaceView` lifecycle, input capture, the Wi-Fi `MulticastLock` + permission UX, Keystore identity, permissions |

The single seam is `io.unom.punktfunk.kit.NativeBridge` ⇄ `Java_io_unom_punktfunk_kit_NativeBridge_*`.

## Layout

```
clients/android/native/          Rust cdylib (workspace member) — links punktfunk-core directly
  src/lib.rs                       JNI seam (connect/pair, input, plane getters, abi/core version)
  src/session.rs                   session lifecycle + plane pumps
  src/decode.rs                    AnnexB → AMediaCodec HEVC hardware decode → SurfaceView (incl. HDR10)
  src/audio.rs · src/mic.rs        Opus + Oboe playback / mic uplink (jitter ring)
  src/feedback.rs                  rumble + HID output (lightbar / adaptive triggers)
  src/stats.rs                     live video stats

clients/android/                   Gradle project (this dir)
  settings.gradle.kts · build.gradle.kts · gradle.properties · gradlew
  app/                             :app — Compose UI: Connect / Settings / Stream screens (phone + TV)
  kit/                             :kit — NativeBridge · discovery (native mdns-sd, polled) · Gamepad · Keymap ·
                                         security (Keystore identity + known-host store) · cargo-ndk build
```

## Prerequisites

- Android SDK + **NDK r30** (`30.0.14904198`), `platforms;android-37.0`, `build-tools;37.0.0`,
  **`cmake;3.22.1`** (`sdkmanager "cmake;3.22.1"` — the `cmake` crate builds libopus with it)
- **JDK 21** for Gradle/AGP (AGP 9.2 runs on JDK 17–21, *not* a newer default JDK like 25)
- Rust + `rustup target add aarch64-linux-android x86_64-linux-android` + `cargo install cargo-ndk`

Toolchain pinned: AGP 9.2.0 · Gradle 9.4.1 · Kotlin 2.3.21 · Compose BOM 2026.05.01 ·
compileSdk 37 · targetSdk 36 · minSdk 31 · ABIs arm64-v8a + x86_64.

## Build & run

**Android Studio:** open `clients/android` — it uses its bundled JBR 21 automatically. The
`cargoNdk*` task builds the `.so` as part of the normal build.

**CLI** (point Gradle at a JDK 21 if your machine default is newer, e.g. JDK 25):

```sh
# Adoptium/Temurin 21 (installed by the Android Studio setup, or `brew install temurin@21`):
export JAVA_HOME="$(/usr/libexec/java_home -v 21)"
cd clients/android
./gradlew :app:assembleDebug      # cargo-ndk cross-compiles libpunktfunk_android.so first
./gradlew :app:installDebug       # onto a running emulator/device

# Emulators (created during env setup):  emulator -avd pf_phone   |   emulator -avd pf_tv
```

The debug APK lands in `app/build/outputs/apk/debug/`. Launch it, pick a host from the list, pair,
and stream.

## Status

A working native client (phone + Android TV), at parity with the Linux and Apple apps for the core
streaming experience:

- **Video** — `AMediaCodec` hardware HEVC decode → `SurfaceView`, including **HDR10** (Main10 /
  BT.2020 PQ), with low-latency decode tuning and a live stats HUD.
- **Audio** — Opus + Oboe playback with a jitter ring, plus mic uplink to the host.
- **Input** — game controllers (buttons + axes) with rumble and HID feedback; D-pad /
  game-controller focus navigation for the couch (TV + phone).
- **Discovery & trust** — native `mdns-sd` mDNS host list (polled over JNI; the same browse the
  Linux/Windows clients use, not `NsdManager`), SPAKE2 PIN pairing and TOFU, with a
  Keystore-wrapped client identity and a known-host store.
- **UI** — Compose host list / settings / stream screens, Material You theming.
- **Shipping** — built for `arm64-v8a` + `x86_64`; published to Google Play (Internal Testing).

`crates/punktfunk-core` uses the `ring` `rcgen` backend so the client `.so` is aws-lc-free.
