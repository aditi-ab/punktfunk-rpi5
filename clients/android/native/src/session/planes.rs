//! Plane start/stop: video (HEVC decode → Surface), host→client audio, mic uplink — plus the
//! ~1 Hz decode-stats drain for the HUD.

use jni::objects::JObject;
// Used only by the android-gated `nativeStartVideo`; on the host build that fn is cfg'd out.
#[cfg(target_os = "android")]
use jni::objects::JString;
use jni::sys::{jboolean, jdoubleArray, jlong, jsize, jstring};
use jni::JNIEnv;

use super::{jni_guard, SessionHandle};

/// `NativeBridge.nativeStartVideo(handle, surface, decoderName, lowLatencyMode, lowLatencyFeature)`
/// — wrap the SurfaceView's `Surface` as an `ANativeWindow` and start the decode thread rendering
/// onto it. `decoderName` is the codec Kotlin ranked from `MediaCodecList` (`""` = let the platform
/// resolve the default for the MIME); `lowLatencyMode` is the user's master toggle;
/// `lowLatencyFeature` is whether that decoder advertised `FEATURE_LowLatency` (HUD label only).
/// No-op if already started.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo(
    mut env: JNIEnv,
    _this: JObject,
    handle: jlong,
    surface: JObject,
    decoder_name: JString,
    low_latency_mode: jboolean,
    ll_feature: jboolean,
    is_tv: jboolean,
) {
    use super::VideoThread;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    if handle == 0 {
        return;
    }
    // The decoder name Kotlin picked (empty string / read failure ⇒ None ⇒ default resolver).
    let decoder = env
        .get_string(&decoder_name)
        .ok()
        .map(String::from)
        .filter(|s| !s.is_empty());
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
    let opts = crate::decode::DecodeOptions {
        decoder_name: decoder,
        ll_feature: ll_feature != 0,
        low_latency_mode: low_latency_mode != 0,
        is_tv: is_tv != 0,
    };
    let join = std::thread::Builder::new()
        .name("pf-decode".into())
        .spawn(move || crate::decode::run(client, window, sd, st, opts))
        .ok();
    *guard = Some(VideoThread { shutdown, join });
}

/// `NativeBridge.nativeVideoMime(handle): String` — the MediaCodec MIME for the codec the host
/// resolved (`"video/hevc"` / `"video/avc"` / `"video/av01"`), so Kotlin can rank `MediaCodecList`
/// decoders for it before calling [`Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo`].
/// Empty string on a `0` handle. Cheap; safe on the UI thread.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    jni_guard(std::ptr::null_mut(), || {
        if handle == 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        match env.new_string(crate::decode::codec_mime(h.client.codec)) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// `NativeBridge.nativeVideoDecoderLabel(handle): String` — the resolved decoder identity for the
/// HUD, e.g. `c2.qti.avc.decoder · low-latency`, or `""` before the decode thread has resolved one.
/// One-shot (the decoder is fixed for the session); poll once after the HUD appears. Not
/// android-gated — pure `jni` + a lock, so it links on the host build too (Kotlin only calls it on
/// device).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoDecoderLabel<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jstring {
    jni_guard(std::ptr::null_mut(), || {
        if handle == 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        match env.new_string(h.stats.decoder_label()) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
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

/// `NativeBridge.nativeVideoStats(handle): DoubleArray?` — drain ~1 s of decode stats for the HUD
/// (unified stats spec, `design/stats-unification.md`). Returns 18 doubles
/// `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skewCorrected, width, height, refreshHz, framesLost,
/// bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms,
/// netP50Ms]`
/// (the two flags are 1.0/0.0; indexes 0–15 match the previous 16-double layout — 0–13 the original
/// 14-double one with the latency pair re-based to the end-to-end capture→decoded headline, 14/15
/// the stage p50s tiling it: `host+network` = capture→received, `decode` = received→decoded; 16/17
/// are the Phase-2 split of the `host+network` term from the per-AU 0xCF host timings — `host` =
/// the host's capture→sent, `network` = the remainder — both 0.0 when no timing matched this
/// window, i.e. an old host), or `null` when no decode thread is running. Poll ~1 Hz from the UI; each call
/// resets the measurement window. Not android-gated — pure `jni` + connector reads, so it links on
/// the host build too (Kotlin only ever calls it on device).
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
        let buf: [f64; 18] = [
            snap.fps,
            snap.mbps,
            snap.e2e_p50_ms,
            snap.e2e_p95_ms,
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
            // Stage p50s tiling the end-to-end headline (appended to keep 0–13 index-compatible).
            snap.hostnet_p50_ms,
            snap.decode_p50_ms,
            // Phase-2 host/network split of the `host+network` stage (0xCF host timings): 0.0
            // when no timing matched this window (old host) — the HUD keeps the combined term.
            snap.host_p50_ms,
            snap.net_p50_ms,
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
