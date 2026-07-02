//! Plane start/stop: video (HEVC decode → Surface), host→client audio, mic uplink — plus the
//! ~1 Hz decode-stats drain for the HUD.

use jni::objects::JObject;
use jni::sys::{jboolean, jdoubleArray, jlong, jsize};
use jni::JNIEnv;

use super::{jni_guard, SessionHandle};

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
    use super::VideoThread;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

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
    let st = h.stats.clone(); // session-lifetime stats (gate survives surface recreate)
    let join = std::thread::Builder::new()
        .name("pf-decode".into())
        .spawn(move || crate::decode::run(client, window, sd, st))
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
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.stop_video();
        }
    })
}

/// `NativeBridge.nativeVideoStats(handle): DoubleArray?` — drain ~1 s of decode stats for the HUD.
/// Returns 14 doubles
/// `[fps, mbps, latP50Ms, latP95Ms, latValid, skewCorrected, width, height, refreshHz, framesDropped,
/// bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc]`
/// (the two flags are 1.0/0.0; the trailing four describe the negotiated video feed — see below), or
/// `null` when no decode thread is running. Poll ~1 Hz from the UI; each call resets the measurement
/// window. Not android-gated — pure `jni` + connector reads, so it links on the host build too
/// (Kotlin only ever calls it on device).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoStats(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jdoubleArray {
    jni_guard(std::ptr::null_mut(), || {
        if handle == 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        if h.video.lock().unwrap().is_none() {
            return std::ptr::null_mut(); // not streaming → no stats
        }
        let snap = h.stats.drain();
        let mode = h.client.mode();
        let color = h.client.color;
        let buf: [f64; 14] = [
            snap.fps,
            snap.mbps,
            snap.lat_p50_ms,
            snap.lat_p95_ms,
            if snap.lat_valid { 1.0 } else { 0.0 },
            if snap.skew_corrected { 1.0 } else { 0.0 },
            mode.width as f64,
            mode.height as f64,
            mode.refresh_hz as f64,
            h.client.frames_dropped() as f64,
            // Video-feed properties the host resolved at the handshake (Welcome): encode bit depth
            // (8 / 10), the CICP colour primaries + transfer code points (Kotlin maps these to a
            // colour-space / HDR label — transfer 16 = PQ, 18 = HLG ⇒ HDR), and the HEVC
            // chroma_format_idc (1 = 4:2:0, 3 = 4:4:4). Static for the session unless renegotiated.
            h.client.bit_depth as f64,
            color.primaries as f64,
            color.transfer as f64,
            h.client.chroma_format as f64,
        ];
        let arr = match env.new_double_array(buf.len() as jsize) {
            Ok(a) => a,
            Err(_) => return std::ptr::null_mut(),
        };
        if env.set_double_array_region(&arr, 0, &buf).is_err() {
            return std::ptr::null_mut();
        }
        arr.into_raw()
    })
}

/// `NativeBridge.nativeSetVideoStatsEnabled(handle, enabled)` — gate per-frame stats sampling on the
/// HUD actually being visible: while disabled the decode thread skips the clock read + lock per AU.
/// Enabling resets the measurement window so a later show never reports stale data. Sticky for the
/// session (survives video stop/start across surface recreation). No-op on `0`. Not android-gated —
/// pure `jni` + an atomic store, so it links on the host build too.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetVideoStatsEnabled(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    enabled: jboolean,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the nativeConnect/nativeClose contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.stats.set_enabled(enabled != 0);
        }
    })
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
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.stop_audio();
        }
    })
}

/// `NativeBridge.nativeStartMic(handle)` — start mic capture (AAudio input → Opus → host `send_mic`).
/// No-op if already running or on a `0` handle. Caller MUST hold RECORD_AUDIO; a failure (e.g. no
/// permission) leaves the rest of the session streaming.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartMic(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = h.mic.lock().unwrap();
    if guard.is_some() {
        return; // already capturing
    }
    match crate::mic::MicCapture::start(h.client.clone()) {
        Some(m) => *guard = Some(m),
        None => log::error!("nativeStartMic: mic init failed (RECORD_AUDIO? — session unaffected)"),
    }
}

/// `NativeBridge.nativeStopMic(handle)` — stop + join the mic thread and close the AAudio input
/// stream (without closing the session). No-op on `0`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopMic(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.stop_mic();
        }
    })
}
