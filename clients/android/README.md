# punktfunk Android client

Native Android client for **punktfunk/1**, targeting **phone + TV** (Compose, D-pad + touch).

## Architecture — Rust-heavy (like the Linux client, not thin-native like Apple)

Kotlin cannot `import` the cbindgen C header the way Swift can, so a native bridge is unavoidable.
We write it in **Rust** and link `punktfunk-core` directly — so the Android client reuses the Linux
client's orchestration (audio jitter ring, VK keymap inverse, latency/skew math, capture state
machine, trust logic) instead of re-porting it into Kotlin.

| Side | Owns |
|------|------|
| **Rust** (`crates/punktfunk-android` → `libpunktfunk_android.so`) | the JNI seam, `NativeClient` (QUIC control + UDP data plane), AnnexB→`AMediaCodec` decode, Opus+Oboe audio, VK keymap, latency math, trust/pairing |
| **Kotlin** (`clients/android`) | Compose UI (host grid / settings / stream), `SurfaceView` lifecycle, input capture, `NsdManager` discovery, Keystore identity, permissions |

The single seam is `io.unom.punktfunk.kit.NativeBridge` ⇄ `Java_io_unom_punktfunk_kit_NativeBridge_*`.

## Layout

```
crates/punktfunk-android/          Rust cdylib (workspace member)
  src/lib.rs                       JNI_OnLoad + abiVersion/coreVersion (native-link proof)
  src/session.rs                   session handle lifecycle (connect/close); plane pumps = TODO

clients/android/                   Gradle project (this dir)
  settings.gradle.kts · build.gradle.kts · gradle.properties · gradlew
  app/                             :app — Compose application (MainActivity)
  kit/                             :kit — Android library: NativeBridge + the cargo-ndk build
    build.gradle.kts               cargoNdk{Debug,Release} → src/main/jniLibs/<abi>/*.so
```

## Prerequisites (already set up on the dev Mac)

- Android SDK + **NDK r30** (`30.0.14904198`), `platforms;android-37.0`, `build-tools;37.0.0`
- **JDK 21** for Gradle/AGP (the machine default JDK 25 is too new for AGP 9.2)
- Rust + `rustup target add aarch64-linux-android x86_64-linux-android` + `cargo install cargo-ndk`

Toolchain pinned: AGP 9.2.0 · Gradle 9.4.1 · Kotlin 2.3.21 · Compose BOM 2026.05.01 ·
compileSdk 37 · targetSdk 36 · minSdk 31 · ABIs arm64-v8a + x86_64.

## Build & run

**Android Studio:** open `clients/android` — it uses its bundled JBR 21 automatically. The
`cargoNdk*` task builds the `.so` as part of the normal build.

**CLI** (the machine default is JDK 25, so point Gradle at JDK 21):

```sh
export JAVA_HOME="$(brew --prefix openjdk@21)/libexec/openjdk.jdk/Contents/Home"
cd clients/android
./gradlew :app:assembleDebug      # cargo-ndk cross-compiles libpunktfunk_android.so first
./gradlew :app:installDebug       # onto a running emulator/device

# Emulators (created during env setup):  emulator -avd pf_phone   |   emulator -avd pf_tv
```

The debug APK lands in `app/build/outputs/apk/debug/`. The scaffold screen calls
`NativeBridge.abiVersion()` across JNI — a live ABI version proves the whole native stack is wired.

## Status

- **Scaffold (done):** Gradle modules, cargo-ndk wiring, JNI native-link proof, phone+TV-installable
  manifest. `crates/punktfunk-core` `rcgen` switched to the `ring` backend so the client `.so` is
  aws-lc-free.
- **Next (M4 Android stage 1):** video decode (`AMediaCodec` async → `SurfaceView`), audio
  (Opus + Oboe + jitter ring), input capture → `send_input`, pairing/identity (Keystore-wrapped),
  mDNS discovery, the phone/TV Compose UI. The Rust-side homes are stubbed in
  `crates/punktfunk-android/src/session.rs` with port pointers to `crates/punktfunk-client-linux`.
