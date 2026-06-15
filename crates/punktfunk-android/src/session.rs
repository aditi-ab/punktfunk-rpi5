//! Session lifecycle + plane wiring over JNI.
//!
//! A connected session is a [`SessionHandle`] — an `Arc<NativeClient>` plus the decode thread it
//! feeds — boxed and handed to Kotlin as an opaque `jlong`. The connector is `Sync`, so the decode
//! thread pulls the video plane (`next_frame`) directly while Kotlin still holds the handle.
//!
//! Wired: connect/close, the video plane (HEVC `next_frame` → NDK AMediaCodec → the SurfaceView's
//! `ANativeWindow`, see [`crate::decode`]), host→client audio ([`crate::audio`]), input
//! (`send_input` — mouse/keyboard/gamepad), rumble/DualSense HID feedback ([`crate::feedback`]),
//! and the trust surface: `nativeGenerateIdentity` (persistent identity, Keystore-wrapped on the
//! Kotlin side), `nativeConnect` with identity + pin (TOFU / pinned), and `nativePair` (SPAKE2 PIN).
//!
//! TODO(M4 Android stage 1): client→host DualSense rich input (`send_rich_input`), mode
//! renegotiation. Port the remaining orchestration from `crates/punktfunk-client-linux`.

use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use punktfunk_core::input::{InputEvent, InputKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A live session behind the `jlong` handle: the connector + the decode thread it feeds.
pub(crate) struct SessionHandle {
    // Read only by the android decode path (`nativeStartVideo` → `crate::decode`); on the host
    // build (CI's workspace clippy/build) those readers are cfg'd out, so it's intentionally unused.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub client: Arc<NativeClient>,
    video: Mutex<Option<VideoThread>>,
    #[cfg(target_os = "android")]
    audio: Mutex<Option<crate::audio::AudioPlayback>>,
}

struct VideoThread {
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Signal the decode thread to stop and join it. Idempotent.
    fn stop_video(&self) {
        if let Some(mut vt) = self.video.lock().unwrap().take() {
            vt.shutdown.store(true, Ordering::SeqCst);
            if let Some(j) = vt.join.take() {
                let _ = j.join();
            }
        }
    }

    /// Stop + close audio playback. Dropping the [`crate::audio::AudioPlayback`] joins its decode
    /// thread and closes the AAudio stream. Idempotent.
    #[cfg(target_os = "android")]
    fn stop_audio(&self) {
        let _ = self.audio.lock().unwrap().take();
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop_video();
        #[cfg(target_os = "android")]
        self.stop_audio();
    }
}

/// SHA-256 fingerprint → 64 lowercase hex chars (matches the host log + client-rs).
fn hex32(fp: &[u8; 32]) -> String {
    use std::fmt::Write;
    fp.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// 64-hex → [u8; 32]; `None` on bad length/char.
fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// `NativeBridge.nativeGenerateIdentity(): String` — mint a fresh persistent self-signed identity.
/// Returns `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on failure (logged). Kotlin
/// persists it (Keystore-wrapped) and only calls this again when the store is genuinely empty.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeGenerateIdentity<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jni::sys::jstring {
    let out = match punktfunk_core::quic::endpoint::generate_identity() {
        Ok((cert, key)) => format!("{cert}\n-----PUNKTFUNK-KEY-----\n{key}"),
        Err(e) => {
            log::error!("nativeGenerateIdentity failed: {e}");
            String::new()
        }
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

/// `NativeBridge.nativeConnect(host, port, w, h, hz, certPem, keyPem, pinHex, bitrateKbps,
/// compositorPref, gamepadPref): Long`. `certPem`/`keyPem` empty = anonymous, else presented as the
/// persistent identity. `pinHex` empty = TOFU (read `nativeHostFingerprint` after), else 64-hex
/// SHA-256 to pin the host (mismatch → 0). `bitrateKbps` 0 = host default. `compositorPref`/
/// `gamepadPref` are `CompositorPref`/`GamepadPref` wire bytes (0 = Auto; unknown → Auto).
/// Returns an opaque handle, or 0 on failure (logged).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    width: jint,
    height: jint,
    refresh_hz: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin_hex: JString<'local>,
    bitrate_kbps: jint,
    compositor_pref: jint,
    gamepad_pref: jint,
) -> jlong {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let cert: String = env
        .get_string(&cert_pem)
        .map(Into::into)
        .unwrap_or_default();
    let key: String = env.get_string(&key_pem).map(Into::into).unwrap_or_default();
    let pin_hex: String = env.get_string(&pin_hex).map(Into::into).unwrap_or_default();

    let identity: Option<(String, String)> = if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some((cert, key))
    };
    let pin: Option<[u8; 32]> = if pin_hex.is_empty() {
        None
    } else {
        match parse_hex32(&pin_hex) {
            Some(fp) => Some(fp),
            None => {
                log::error!("nativeConnect: bad pin hex (len {})", pin_hex.len());
                return 0;
            }
        }
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
        CompositorPref::from_u8(compositor_pref.clamp(0, u8::MAX as jint) as u8),
        GamepadPref::from_u8(gamepad_pref.clamp(0, u8::MAX as jint) as u8),
        bitrate_kbps.max(0) as u32, // 0 = host default
        None,                       // launch: default app
        pin,                        // Some → Crypto on host-fp mismatch
        identity,                   // owned (cert, key) PEM, or None (anonymous)
        Duration::from_secs(10),
    ) {
        Ok(client) => {
            let handle = SessionHandle {
                client: Arc::new(client),
                video: Mutex::new(None),
                #[cfg(target_os = "android")]
                audio: Mutex::new(None),
            };
            Box::into_raw(Box::new(handle)) as jlong
        }
        Err(e) => {
            log::error!("nativeConnect to {host}:{port} failed: {e}");
            0
        }
    }
}

/// `NativeBridge.nativeClose(handle)` — drop the session (stops the decode thread, then RAII-tears
/// down the connector). No-op on `0`.
///
/// # Safety contract
/// `handle` must be `0` or a live handle from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect`],
/// closed exactly once and not concurrently with other calls on the same handle (Kotlin owns this).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: per the contract, `handle` is a live `Box<SessionHandle>` pointer.
        unsafe { drop(Box::from_raw(handle as *mut SessionHandle)) };
    }
}

/// `NativeBridge.nativeHostFingerprint(handle): String` — the SHA-256 (64-hex) of the cert the host
/// presented on this connection. Valid after a successful `nativeConnect`; Kotlin pins it on a TOFU
/// connect. `""` on a `0` handle.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostFingerprint<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jni::sys::jstring {
    let out = if handle == 0 {
        String::new()
    } else {
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        hex32(&h.client.host_fingerprint)
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

/// `NativeBridge.nativePair(host, port, certPem, keyPem, pin, name): String` — run the SPAKE2 PIN
/// ceremony, presenting our persistent identity. On success returns the host's verified fingerprint
/// (64-hex) to persist + pin; on any failure (wrong PIN / MITM / host reject / unreachable) returns
/// `""` (logged). Blocking — Kotlin calls it off the UI thread.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePair<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin: JString<'local>,
    name: JString<'local>,
) -> jni::sys::jstring {
    let g = |e: &mut JNIEnv<'local>, j: &JString<'local>| -> String {
        e.get_string(j).map(Into::into).unwrap_or_default()
    };
    let host = g(&mut env, &host);
    let cert = g(&mut env, &cert_pem);
    let key = g(&mut env, &key_pem);
    let pin = g(&mut env, &pin);
    let name = g(&mut env, &name);

    let out = if host.is_empty() || cert.is_empty() || key.is_empty() {
        log::error!("nativePair: missing host/identity");
        String::new()
    } else {
        match NativeClient::pair(
            &host,
            port as u16,
            (&cert, &key), // borrowed identity
            &pin,
            &name,
            Duration::from_secs(60),
        ) {
            Ok(host_fp) => hex32(&host_fp),
            Err(e) => {
                // Crypto error == wrong PIN / MITM; anything else == transport/host reject.
                log::error!("nativePair to {host}:{port} failed: {e}");
                String::new()
            }
        }
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

/// `NativeBridge.nativeStartVideo(handle, surface)` — wrap the SurfaceView's `Surface` as an
/// `ANativeWindow` and start the HEVC decode thread rendering onto it. No-op if already started.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
    surface: JObject,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = h.video.lock().unwrap();
    if guard.is_some() {
        return; // already streaming
    }
    // SAFETY: `env`/`surface` are valid JNI pointers for this call. `as *mut _` bridges any
    // jni-sys version skew between the `jni` and `ndk` crates (both are raw `*mut _` pointers).
    let window = match unsafe {
        ndk::native_window::NativeWindow::from_surface(
            env.get_native_interface() as *mut _,
            surface.as_raw() as *mut _,
        )
    } {
        Some(w) => w,
        None => {
            log::error!("nativeStartVideo: no ANativeWindow from Surface");
            return;
        }
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let client = h.client.clone();
    let sd = shutdown.clone();
    let join = std::thread::Builder::new()
        .name("pf-decode".into())
        .spawn(move || crate::decode::run(client, window, sd))
        .ok();
    *guard = Some(VideoThread { shutdown, join });
}

/// `NativeBridge.nativeStopVideo(handle)` — stop + join the decode thread (without closing the
/// session). No-op on `0`.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopVideo(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: live handle per the contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        h.stop_video();
    }
}

/// `NativeBridge.nativeStartAudio(handle)` — start the Opus→AAudio playback thread. No-op if already
/// started or on a `0` handle. Best-effort: a failure leaves video streaming.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartAudio(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = h.audio.lock().unwrap();
    if guard.is_some() {
        return; // already playing
    }
    match crate::audio::AudioPlayback::start(h.client.clone()) {
        Some(p) => *guard = Some(p),
        None => log::error!("nativeStartAudio: playback init failed (video unaffected)"),
    }
}

/// `NativeBridge.nativeStopAudio(handle)` — stop + join the audio thread and close AAudio (without
/// closing the session). No-op on `0`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopAudio(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle != 0 {
        // SAFETY: live handle per the contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        h.stop_audio();
    }
}

// ---- Input plane: Kotlin capture → NativeClient::send_input ----------------------------------
// All four are `&self` on the `Sync` connector (send_input is a non-blocking datagram push), safe
// from the Kotlin UI thread. NOT android-gated — send_input exists on the host build too, so these
// compile everywhere (parity with nativeConnect/nativeClose). The wire codes are the GameStream
// conventions: buttons 1=left/2=middle/3=right/4=X1/5=X2; scroll axis 0=vertical/1=horizontal,
// signed 120-unit delta, +=up/right; keys are Windows VK (mapped from KEYCODE_* on the Kotlin side).

/// `NativeBridge.nativeSendPointerMove(handle, dx, dy)` — relative mouse motion (screen +y down).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerMove(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    dx: jint,
    dy: jint,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_input is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: InputKind::MouseMove,
        _pad: [0; 3],
        code: 0,
        x: dx,
        y: dy,
        flags: 0,
    });
}

/// `NativeBridge.nativeSendPointerButton(handle, button, down)` — one button transition.
/// `button`: GameStream id (1=left, 2=middle, 3=right, 4=X1, 5=X2). `down`: 1=press, 0=release.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerButton(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    button: jint,
    down: jboolean,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: if down != 0 {
            InputKind::MouseButtonDown
        } else {
            InputKind::MouseButtonUp
        },
        _pad: [0; 3],
        code: button as u32,
        x: 0,
        y: 0,
        flags: 0,
    });
}

/// `NativeBridge.nativeSendScroll(handle, axis, delta)` — one scroll step. `axis`: 0=vertical,
/// 1=horizontal. `delta`: signed, WHEEL_DELTA(120)-scaled, +=up/right.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendScroll(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    axis: jint,
    delta: jint,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: InputKind::MouseScroll,
        _pad: [0; 3],
        code: axis as u32,
        x: delta,
        y: 0,
        flags: 0,
    });
}

/// `NativeBridge.nativeSendKey(handle, vk, down, mods)` — one key transition. `vk`: Windows
/// Virtual-Key code (0 = unmapped → dropped). `down`: 1=press, 0=release. `mods`: VK modifier
/// bitmask (0 for now — the host folds modifiers from the L/R modifier key events themselves).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendKey(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    vk: jint,
    down: jboolean,
    mods: jint,
) {
    if handle == 0 || vk == 0 {
        return;
    }
    // SAFETY: live handle per the contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: if down != 0 {
            InputKind::KeyDown
        } else {
            InputKind::KeyUp
        },
        _pad: [0; 3],
        code: vk as u32,
        x: 0,
        y: 0,
        flags: mods as u32,
    });
}

// ---- Gamepad: Kotlin captures (KeyEvent/MotionEvent) → NativeClient::send_input ---------------
// Single-pad model: exactly one controller, forwarded as pad 0 (flags = 0). Buttons carry the
// gamepad::BTN_* bit in `code` and pressed/released in `x` (1/0); axes carry the gamepad::AXIS_* id
// in `code` and the value in `x` (sticks i16 −32768..32767, +y = up; triggers 0..255). The host
// accumulates the incremental events into its virtual xpad. Wire contract: input.rs::gamepad.

/// `NativeBridge.nativeSendGamepadButton(handle, bit, down)` — one gamepad button transition.
/// `bit`: a `gamepad::BTN_*` bit (e.g. BTN_A = 0x1000). `down`: 1=press, 0=release.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadButton(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    bit: jint,
    down: jboolean,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: InputKind::GamepadButton,
        _pad: [0; 3],
        code: bit as u32,
        x: i32::from(down != 0),
        y: 0,
        flags: 0, // pad index 0 — single-pad model
    });
}

/// `NativeBridge.nativeSendGamepadAxis(handle, axisId, value)` — one gamepad axis update.
/// `axisId`: a `gamepad::AXIS_*` id (LS_X=0..RT=5). `value`: stick i16 (−32768..32767, +y=up) or
/// trigger 0..255.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadAxis(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    axis_id: jint,
    value: jint,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind: InputKind::GamepadAxis,
        _pad: [0; 3],
        code: axis_id as u32,
        x: value,
        y: 0,
        flags: 0, // pad index 0 — single-pad model
    });
}
