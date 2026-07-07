//! punktfunk Android client — the JNI bridge ("nativecore") over `punktfunk-core`.
//!
//! Architecture: the **Rust-heavy** client model (like `punktfunk-client-linux`, *not* the
//! thin-native-over-C-ABI Apple model). This `cdylib` links `punktfunk-core` directly and drives
//! the whole `punktfunk/1` protocol through [`punktfunk_core::client::NativeClient`]; Kotlin owns
//! only the Android-framework surface (Compose UI, `SurfaceView` lifecycle, input capture, the
//! Wi-Fi `MulticastLock` + permission UX, Keystore). The JNI seam below is the one place the two
//! languages meet.
//!
//! Why Rust-heavy: Kotlin cannot `import` the cbindgen C header the way Swift can, so a native
//! bridge is unavoidable. Writing it in Rust lets the Android client reuse the Linux client's
//! orchestration verbatim — audio jitter ring, the VK keymap inverse, latency/skew math, the
//! input capture state machine, trust/pairing logic, **mDNS discovery** ([`discovery`], the same
//! `mdns-sd` browse the Linux/Windows clients use) — instead of re-porting it into Kotlin. Kotlin
//! keeps only the Android-framework surface it must (Compose UI, `SurfaceView`, input capture, the
//! Wi-Fi `MulticastLock` + permission UX, Keystore identity).
//!
//! JNI symbols map to `io.unom.punktfunk.kit.NativeBridge` in the `:kit` Gradle module
//! (`clients/android`). The surface: the native-link proof (`abiVersion`/`coreVersion`), mDNS host
//! discovery ([`discovery`]), and the session lifecycle in [`session`] — connect/pair + the trust
//! surface, the per-plane pumps (video → AMediaCodec, audio ↔ AAudio, mic uplink), input, and
//! rumble/HID feedback ([`feedback`]). Mode renegotiation is still TODO (see [`session`]).

use jni::objects::JObject;
use jni::sys::jint;
use jni::JNIEnv;

#[cfg(target_os = "android")]
mod adpf;
#[cfg(target_os = "android")]
mod audio;
#[cfg(target_os = "android")]
mod decode;
// Ungated: pure `mdns-sd` + `jni`, so the browse + its JNI seam link into the host workspace build
// (and its unit test runs there) exactly like `session`/`stats`. Kotlin only ever calls it on device.
mod discovery;
mod feedback;
#[cfg(target_os = "android")]
mod mic;
mod session;
mod stats;
// Ungated like `discovery`: pure `jni` + `punktfunk_core::wol` (no Android framework), so it links
// into the host workspace build too. Kotlin only ever calls it on device.
mod wol;

/// Initialize `android_logger` once when the JVM loads the library. Logs land in logcat under the
/// `punktfunk` tag. Core `tracing` events (transport warnings: socket-buffer clamp, QoS failures)
/// arrive here too: tracing's "log" feature — declared explicitly in Cargo.toml rather than relied
/// on via quinn's defaults — forwards them as `log` records since no tracing subscriber is ever
/// installed. Android-only — there is no JVM (and no logcat) on the host build.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn JNI_OnLoad(
    _vm: *mut jni::sys::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jint {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("punktfunk"),
    );
    log::info!(
        "punktfunk_android loaded (core ABI v{})",
        punktfunk_core::ABI_VERSION
    );
    jni::sys::JNI_VERSION_1_6
}

/// `NativeBridge.abiVersion(): Int` — the core's C-ABI version. A non-error return is the
/// scaffold's proof that `System.loadLibrary` found the `.so`, the JNI symbol resolved, and the
/// linked `punktfunk-core` is the one we expect.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_abiVersion(
    _env: JNIEnv,
    _this: JObject,
) -> jint {
    punktfunk_core::ABI_VERSION as jint
}

/// `NativeBridge.coreVersion(): String` — the crate version, proving JNI string marshaling works.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_coreVersion<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jni::sys::jstring {
    match env.new_string(env!("CARGO_PKG_VERSION")) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}
