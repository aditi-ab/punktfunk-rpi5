//! Session lifecycle + plane wiring over JNI.
//!
//! A connected session is a [`SessionHandle`] — an `Arc<NativeClient>` plus the decode thread it
//! feeds — boxed and handed to Kotlin as an opaque `jlong`. The connector is `Sync`, so the decode
//! thread pulls the video plane (`next_frame`) directly while Kotlin still holds the handle.
//!
//! Wired so far: connect/close + the video plane (HEVC `next_frame` → NDK AMediaCodec → the
//! SurfaceView's `ANativeWindow`, see [`crate::decode`]).
//!
//! TODO(M4 Android stage 1): audio (`next_audio` → Opus → Oboe), input (`send_input` /
//! `send_rich_input`), rumble/HID feedback, pairing/identity (Keystore). Port the orchestration
//! from `crates/punktfunk-client-linux`.

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

/// `NativeBridge.nativeConnect(host, port, width, height, refreshHz): Long` — trust-on-first-use,
/// anonymous. Returns an opaque session handle, or `0` on failure (logged to logcat).
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
        0,    // bitrate_kbps: host default
        None, // launch: default app
        None, // pin: trust on first use
        None, // identity: anonymous (TODO: Keystore-backed identity + pairing)
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
