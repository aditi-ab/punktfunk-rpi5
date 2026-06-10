# punktfunk Android client (later)

Kotlin UI + MediaCodec (decode) + a thin JNI layer over the `punktfunk-core` C ABI.

## Wiring

1. Build the core as a shared library per Android ABI:
   ```sh
   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
   cargo build -p punktfunk-core --release --target aarch64-linux-android   # libpunktfunk_core.so
   ```
   (Use `cargo-ndk` to handle the NDK toolchain/linker.)
2. JNI shim: small C/Rust glue mapping `punktfunk_*` to Kotlin `external fun`s, bundling
   `libpunktfunk_core.so` into the APK's `jniLibs/`.
3. Kotlin: client `PunktfunkSession` → `punktfunk_client_poll_frame` on a decode thread → feed
   `MediaCodec` → render to a `SurfaceView` aligned to the display refresh.

## Status

Placeholder — scheduled after the Apple client (M5).
