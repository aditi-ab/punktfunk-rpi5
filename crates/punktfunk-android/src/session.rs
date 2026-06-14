//! Session handle lifecycle over JNI.
//!
//! A connected [`NativeClient`] is boxed and handed to Kotlin as an opaque `jlong`; [`nativeClose`]
//! drops it, and the connector's `Drop` tears down the worker thread + QUIC connection (RAII). The
//! client is `Sync`, so the Kotlin side is free to pull each plane from its own thread later.
//!
//! TODO(M4 Android stage 1): build out the plane pumps + IO on top of this handle. Port the
//! orchestration from `crates/punktfunk-client-linux`:
//!
//! - video: `next_frame` → AnnexB access unit → `AMediaCodec` (NDK, async) → `SurfaceView`
//! - audio: `next_audio` → Opus decode → jitter ring → Oboe (port `client-linux/src/audio.rs`)
//! - input: Kotlin capture → `send_input` / `send_rich_input` (VK keymap from `keymap.rs`)
//! - rumble/HID feedback: `next_rumble` / `next_hidout` → VibratorManager / LightsManager
//! - trust: `generate_identity` + `pair` + pin (Keystore-wrapped), then pass `pin`/`identity` here
//!
//! The signatures below are deliberately minimal (TOFU, anonymous) so the scaffold can already
//! stand up a session against a host that does not require pairing.

use jni::objects::{JObject, JString};
use jni::sys::{jint, jlong};
use jni::JNIEnv;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::time::Duration;

/// `NativeBridge.nativeConnect(host, port, width, height, refreshHz): Long`.
///
/// Trust-on-first-use (no pin) and anonymous (no client identity) — enough to bring up a stream
/// against a host that does not require pairing. Returns an opaque session handle, or `0` on
/// failure (the cause is logged to logcat).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    width: jint,
    height: jint,
    refresh_hz: jint,
) -> jlong {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let mode = Mode {
        width: width as u32,
        height: height as u32,
        refresh_hz: refresh_hz as u32,
    };
    match NativeClient::connect(
        &host,
        port as u16,
        mode,
        CompositorPref::Auto,
        GamepadPref::Auto,
        0,    // bitrate_kbps: let the host choose its default
        None, // launch: default app
        None, // pin: trust on first use
        None, // identity: anonymous (TODO: Keystore-backed identity + pairing)
        Duration::from_secs(10),
    ) {
        Ok(client) => Box::into_raw(Box::new(client)) as jlong,
        Err(e) => {
            log::error!("nativeConnect to {host}:{port} failed: {e}");
            0
        }
    }
}

/// `NativeBridge.nativeClose(handle)` — drop the boxed [`NativeClient`] (RAII shutdown of the
/// worker thread + QUIC connection). No-op on a `0` handle.
///
/// # Safety contract
/// `handle` must be either `0` or a value previously returned by [`Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect`]
/// and not already closed. Kotlin owns this invariant (one `nativeClose` per non-zero `nativeConnect`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: per the contract above, `handle` is a live `Box<NativeClient>` pointer.
        unsafe { drop(Box::from_raw(handle as *mut NativeClient)) };
    }
}
