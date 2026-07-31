//! Plane start/stop: video (HEVC decode → Surface), host→client audio, mic uplink — plus the
//! ~1 Hz decode-stats drain for the HUD.

use jni::objects::JObject;
// Used only by the android-gated `nativeStartVideo`; on the host build that fn is cfg'd out.
#[cfg(target_os = "android")]
use jni::objects::JString;
use jni::sys::{jboolean, jdoubleArray, jintArray, jlong, jsize, jstring};
use jni::JNIEnv;

use super::{jni_guard, SessionHandle};

/// `NativeBridge.nativeStartVideo(handle, surface, decoderName, lowLatencyMode, lowLatencyFeature,
/// isTv, presentPriority, smoothBuffer)` — wrap the SurfaceView's `Surface` as an `ANativeWindow`
/// and start the decode thread rendering onto it. `decoderName` is the codec Kotlin ranked from
/// `MediaCodecList` (`""` = let the platform resolve the default for the MIME); `lowLatencyMode`
/// is the user's master toggle; `lowLatencyFeature` is whether that decoder advertised
/// `FEATURE_LowLatency` (HUD label only); `presentPriority`/`smoothBuffer` are the timeline
/// presenter's intent (0 = lowest latency / 1 = smoothness; buffer 0 = auto, 1..=3 frames).
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
    present_priority: jni::sys::jint,
    smooth_buffer: jni::sys::jint,
    panel_fps: jni::sys::jint,
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
        present_priority,
        smooth_buffer,
        panel_hz: panel_fps,
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

/// `NativeBridge.nativeVideoCodecLabel(handle): String` — a short human label for the codec the
/// host resolved (`"H.264"` / `"HEVC"` / `"AV1"` / `"PyroWave"`), for the stats HUD's video-feed
/// line. Distinct from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime`] because the MIME
/// collapses PyroWave onto `video/hevc` and can't name it. Empty string on a `0` handle. Cheap;
/// safe on the UI thread. Android-gated (reads `crate::decode`), matching `nativeVideoMime`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoCodecLabel<'local>(
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
        match env.new_string(crate::decode::codec_label(h.client.codec)) {
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
/// (unified stats spec, `design/stats-unification.md`). Returns 33 doubles
/// `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skewCorrected, width, height, refreshHz, framesLost,
/// bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms,
/// netP50Ms, lostWindow, skippedWindow, fecWindow, framesWindow, dispValid, displayP50Ms,
/// e2eDispP50Ms, e2eDispP95Ms, paceP50Ms, latchP50Ms, presentsWindow, presenterActive,
/// feedP50Ms, codecP50Ms, skippedOverflowWindow]`
/// (the flags are 1.0/0.0; indexes 0–21 match the previous 22-double layout — 0–13 the original
/// 14-double one with the latency pair re-based to the end-to-end capture→decoded headline, 14/15
/// the stage p50s tiling it: `host+network` = capture→received, `decode` = received→decoded; 16/17
/// are the Phase-2 split of the `host+network` term from the per-AU 0xCF host timings — `host` =
/// the host's capture→sent, `network` = the remainder — both 0.0 when no timing matched this
/// window, i.e. an old host; 18–21 are the spec's per-window line-4 counters — `lost` =
/// unrecoverable drops this window, `skipped` = client newest-wins/pacing drops, `fec` = shards
/// recovered, `frames` = AUs received, so the HUD can compute `lost/(frames+lost)` — index 9 stays
/// the cumulative session total for older readers; 22–25 are the `display` stage from the
/// OnFrameRendered render timestamps — when `dispValid` is 1.0 the HUD headline becomes the
/// directly-measured capture→displayed pair at 24/25 with `display` = decoded→displayed p50 at 23
/// closing the equation, and when 0.0 — no render callback landed this window — it falls back to
/// the capture→decoded headline at 2/3; 26–29 are the timeline presenter's split of the `display`
/// term — `pace` = decoded→release (store + glass budget) p50 at 26, `latch` =
/// release→displayed (SurfaceFlinger) p50 at 27, the window's on-glass confirm count at 28
/// (`presents` vs `fps` is the presenter-health pair), and 29 = 1.0 while the timeline presenter
/// is active this session; 30/31 are the `decode` stage's split p50s — `feed` =
/// received→queued (hand-off + input-slot wait) at 30 and `codec` = queued→decoded (codec-pure,
/// from the AU's last piece) at 31, both 0.0 when no sample landed (sync loop); 32 is the
/// parked-AU overflow subset of the window's `skipped` at 19 (decoder fell behind, vs benign
/// newest-wins pacing)), or `null` when no decode thread is running.
/// Poll ~1 Hz from the UI; each call
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
        let snap = h
            .stats
            .drain(h.client.frames_dropped(), h.client.fec_recovered_shards());
        let mode = h.client.mode();
        let color = h.client.color;
        let buf: [f64; 33] = [
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
            // Spec line-4 counters, per-window: lost (unrecoverable drops), skipped (client
            // newest-wins/pacing drops), FEC shards recovered, and the received-AU count so the
            // HUD computes the loss percentage `lost/(frames+lost)` exactly.
            snap.lost as f64,
            snap.skipped as f64,
            snap.fec as f64,
            snap.frames as f64,
            // `display` stage (OnFrameRendered render timestamps): validity flag, the
            // decoded→displayed stage p50, and the directly-measured capture→displayed headline
            // pair that supersedes 2/3 whenever the flag is set (spec: the equation always tiles
            // the headline interval, so endpoint and terms move together).
            if snap.disp_valid { 1.0 } else { 0.0 },
            snap.display_p50_ms,
            snap.e2e_disp_p50_ms,
            snap.e2e_disp_p95_ms,
            // Timeline-presenter split of the `display` term (pace = decoded→release, latch =
            // release→displayed), the window's on-glass confirm count, and whether the presenter
            // is active at all (0.0 = legacy release-immediately path — split reads 0 too).
            snap.pace_p50_ms,
            snap.latch_p50_ms,
            snap.presents as f64,
            if h.stats.presenter_active() { 1.0 } else { 0.0 },
            // The `decode` stage's split (P3 science): feed = received→queued (hand-off +
            // input-slot wait), codec = queued→decoded (codec-pure) — and the parked-AU
            // overflow subset of `skipped` (decoder-health vs benign pacing drops).
            snap.feed_p50_ms,
            snap.codec_p50_ms,
            snap.skipped_overflow as f64,
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

/// `NativeBridge.nativeVideoSize(handle): IntArray?` — the negotiated video mode as
/// `[width, height, refreshHz]`. Resolved at the handshake (Welcome), so it is known before a
/// single frame arrives: the UI sizes the video surface to the STREAM's aspect rather than
/// stretching it to the panel's, and pins the panel's display mode to the stream refresh. The
/// trailing `refreshHz` was appended later — old readers index only 0/1 and never see it. `null`
/// on a `0` handle. Not android-gated — pure `jni` + a connector read, so it links on the host
/// build too. Cheap; safe on the UI thread.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSize(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jintArray {
    jni_guard(std::ptr::null_mut(), || {
        if handle == 0 {
            return std::ptr::null_mut();
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        let mode = h.client.mode();
        let buf: [i32; 3] = [
            mode.width as i32,
            mode.height as i32,
            mode.refresh_hz as i32,
        ];
        let arr = match env.new_int_array(buf.len() as jsize) {
            Ok(a) => a,
            Err(_) => return std::ptr::null_mut(),
        };
        if env.set_int_array_region(&arr, 0, &buf).is_err() {
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
            // The current cumulative counters seed the window baselines, so the first snapshot's
            // `lost`/`FEC` cover only time the HUD was actually up.
            h.stats.set_enabled(
                enabled != 0,
                h.client.frames_dropped(),
                h.client.fec_recovered_shards(),
            );
        }
    })
}

/// `NativeBridge.nativeStartAudio(handle, lowLatencyMode)` — start the Opus→AAudio playback thread.
/// `lowLatencyMode` (the experimental toggle) tags the stream usage=Game for the HAL's game-audio
/// routing. No-op if already started or on a `0` handle. Best-effort: a failure leaves video
/// streaming.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartAudio(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    low_latency_mode: jboolean,
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
    match crate::audio::AudioPlayback::start(h.client.clone(), low_latency_mode != 0) {
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

/// `NativeBridge.nativeStartMic(handle, echoCancel): Int` — start mic capture (AAudio input →
/// Opus → host `send_mic`). `echoCancel` opens the capture under the `VoiceCommunication` preset
/// (the HAL's echo canceller / noise suppressor) and allocates an audio session id; the return
/// value is that id (`> 0`), so Kotlin can attach the Java `AcousticEchoCanceler`/`NoiseSuppressor`
/// as a backstop — `0` when none was allocated (echoCancel off, the preset fell back to the plain
/// open, a `0` handle, or the mic failed entirely). Already running (a surface recreate) returns
/// the running capture's id. Caller MUST hold RECORD_AUDIO; a failure (e.g. no permission) leaves
/// the rest of the session streaming.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartMic(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    echo_cancel: jboolean,
) -> jni::sys::jint {
    if handle == 0 {
        return 0;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = h.mic.lock().unwrap();
    if let Some(m) = guard.as_ref() {
        return m.session_id(); // already capturing — same stream, same session
    }
    // The capture SHARES the session's mute flag, so one started while muted stays muted (and
    // sends nothing) from its very first frame — see `SessionHandle::mic_muted`.
    match crate::mic::MicCapture::start(h.client.clone(), echo_cancel != 0, h.mic_muted.clone()) {
        Some(m) => {
            let session_id = m.session_id();
            *guard = Some(m);
            session_id
        }
        None => {
            log::error!("nativeStartMic: mic init failed (RECORD_AUDIO? — session unaffected)");
            0
        }
    }
}

/// `NativeBridge.nativeStopMic(handle)` — stop + join the mic thread and close the AAudio input
/// stream (without closing the session). No-op on `0`. Leaves the session's mute state alone: a
/// surface recreate stops and restarts the mic, and a user who muted must stay muted through it.
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

/// `NativeBridge.nativeSetMicMuted(handle, muted)` — mute/unmute the mic uplink mid-stream.
///
/// Muting deliberately does NOT stop the capture: the AAudio input stream, the input-preset rung
/// it settled on and its primed buffers all stay exactly as they are, and the encode loop simply
/// drops each 10 ms frame instead of encoding + sending it. A stop/start would re-run the preset
/// fallback ladder and re-prime buffers on every toggle — hundreds of ms, and possibly a different
/// rung (echo cancellation silently lost). This way a toggle costs one atomic store here and one
/// relaxed load per frame there, and takes effect on the very next 10 ms boundary.
///
/// Sticky for the SESSION (the flag lives on the handle, not on the capture), so the mic restart a
/// surface recreate performs comes back muted with no window for an unmuted frame to escape; a
/// fresh session always starts unmuted. No-op on `0`. Not android-gated — pure `jni` + an atomic
/// store, so it links on the host build too.
///
/// One honest consequence of keeping the stream open: the platform's own recording indicator stays
/// lit while muted, because the mic really is still open. What stops is the encode and the send —
/// no captured audio leaves the process.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetMicMuted(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    muted: jboolean,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the nativeConnect/nativeClose contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.mic_muted
                .store(muted != 0, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

/// `NativeBridge.nativeMicActive(handle): Boolean` — is a mic capture actually RUNNING? `true` only
/// between a `nativeStartMic` that opened a stream and the matching `nativeStopMic`. The in-stream
/// mute control is offered on this evidence rather than on the user's setting, so a device that
/// refused every AAudio input rung (or a missing RECORD_AUDIO grant) shows no control instead of a
/// lie about a mic that is being heard. `false` on a `0` handle. Cheap (one uncontended lock).
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeMicActive(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(0, || {
        if handle == 0 {
            return 0;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        jboolean::from(h.mic.lock().unwrap().is_some())
    })
}
