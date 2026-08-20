//! Plane start/stop: video (HEVC decode → Surface), host→client audio, mic uplink — plus the
//! ~1 Hz decode-stats drain for the HUD.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JDoubleArray, JIntArray, JObject, JString};
use jni::sys::{jboolean, jlong};
use jni::EnvUnowned;

use super::{jni_guard, lock_recover, SessionHandle};

/// `NativeBridge.nativeStartVideo(handle, surface, decoderName, lowLatencyMode, lowLatencyFeature,
/// isTv, presentPriority, smoothBuffer)` — wrap the SurfaceView's `Surface` as an `ANativeWindow`
/// and start the decode thread rendering onto it. `decoderName` is the codec Kotlin ranked from
/// `MediaCodecList` (`""` = let the platform resolve the default for the MIME); `lowLatencyMode`
/// is the user's master toggle; `lowLatencyFeature` is whether that decoder advertised
/// `FEATURE_LowLatency` (HUD label only); `presentPriority`/`smoothBuffer` are the timeline
/// presenter's intent (0 = lowest latency / 1 = smoothness; buffer 0 = auto, 1..=3 frames).
/// No-op if already started.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo(
    mut env: EnvUnowned,
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
    surface_w: jni::sys::jint,
    surface_h: jni::sys::jint,
) {
    use super::VideoThread;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    env.with_env(|env| -> jni::errors::Result<()> {
        if handle == 0 {
            return Ok(());
        }
        // The decoder name Kotlin picked (empty string / read failure ⇒ None ⇒ default resolver).
        let decoder = decoder_name
            .try_to_string(env)
            .ok()
            .filter(|s| !s.is_empty());
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        let mut guard = lock_recover(&h.video);
        if guard.is_some() {
            return Ok(()); // already streaming
        }
        // SAFETY: `env`/`surface` are valid JNI pointers for this call. `as *mut _` bridges any
        // jni-sys version skew between the `jni` and `ndk` crates (both are raw `*mut _` pointers)
        // — a real skew here, not a hypothetical one: `jni` is on jni-sys 0.4 while the vendored
        // `ndk` is still on 0.3.
        let window = match unsafe {
            ndk::native_window::NativeWindow::from_surface(
                env.get_raw() as *mut _,
                surface.as_raw() as *mut _,
            )
        } {
            Some(w) => w,
            None => {
                log::error!("nativeStartVideo: no ANativeWindow from Surface");
                return Ok(());
            }
        };
        let shutdown = Arc::new(AtomicBool::new(false));
        let client = h.client.clone();
        let sd = shutdown.clone();
        let st = h.stats.clone(); // session-lifetime stats (gate survives surface recreate)

        // Seed the live view size with what the view measures right now; `surfaceChanged` keeps it
        // current from here on (the bars hide and the cutout mode changes AFTER this call).
        h.surface_size.store(
            super::pack_surface_size(surface_w, surface_h),
            std::sync::atomic::Ordering::Relaxed,
        );
        let opts = crate::decode::DecodeOptions {
            decoder_name: decoder,
            ll_feature,
            low_latency_mode,
            is_tv,
            present_priority,
            smooth_buffer,
            panel_hz: panel_fps,
            surface_size: h.surface_size.clone(),
        };
        let join = std::thread::Builder::new()
            .name("pf-decode".into())
            .spawn(move || crate::decode::run(client, window, sd, st, opts))
            .ok();
        *guard = Some(VideoThread { shutdown, join });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoSurfaceSize(handle, width, height)` — the video `SurfaceView`'s
/// on-screen pixel size, re-reported on every `surfaceChanged`.
///
/// The ASurfaceControl presenter composites its child layer into exactly this rectangle, and the
/// view resizes UNDER a surface that is never recreated: the stream screen hides the system bars
/// and asks to draw into the display cutout a frame or two after `surfaceCreated`, both of which
/// grow it. Without this the layer would keep painting the picture at its start-up size, in the
/// corner of a bigger surface. Non-positive values are ignored (they'd blank the picture).
/// No-op on a `0` handle. Stored whether or not video is running — the next `nativeStartVideo`
/// then starts from a measured view rather than the window's guess. Not android-gated: pure `jni`
/// + an atomic store, so it links on the host build too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSurfaceSize(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    width: jni::sys::jint,
    height: jni::sys::jint,
) {
    jni_guard((), || {
        let packed = super::pack_surface_size(width, height);
        if handle == 0 || packed == 0 {
            return;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        h.surface_size
            .store(packed, std::sync::atomic::Ordering::Relaxed);
    })
}

/// `NativeBridge.nativeVideoMime(handle): String` — the MediaCodec MIME for the codec the host
/// resolved (`"video/hevc"` / `"video/avc"` / `"video/av01"`), so Kotlin can rank `MediaCodecList`
/// decoders for it before calling [`Java_io_unom_punktfunk_kit_NativeBridge_nativeStartVideo`].
/// Empty string on a `0` handle. Cheap; safe on the UI thread.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        if handle == 0 {
            return Ok(JString::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        env.new_string(crate::decode::codec_mime(h.client.codec))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoCodecLabel(handle): String` — a short human label for the codec the
/// host resolved (`"H.264"` / `"HEVC"` / `"AV1"` / `"PyroWave"`), for the stats HUD's video-feed
/// line. Distinct from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoMime`] because the MIME
/// collapses PyroWave onto `video/hevc` and can't name it. Empty string on a `0` handle. Cheap;
/// safe on the UI thread. Android-gated (reads `crate::decode`), matching `nativeVideoMime`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoCodecLabel<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        if handle == 0 {
            return Ok(JString::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        env.new_string(crate::decode::codec_label(h.client.codec))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoDecoderLabel(handle): String` — the resolved decoder identity for the
/// HUD, e.g. `c2.qti.avc.decoder · low-latency`, or `""` before the decode thread has resolved one.
/// One-shot (the decoder is fixed for the session); poll once after the HUD appears. Not
/// android-gated — pure `jni` + a lock, so it links on the host build too (Kotlin only calls it on
/// device).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoDecoderLabel<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        if handle == 0 {
            return Ok(JString::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        env.new_string(h.stats.decoder_label())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeStopVideo(handle)` — stop + join the decode thread (without closing the
/// session). No-op on `0`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopVideo(
    _env: EnvUnowned,
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
/// (unified stats spec, `design/stats-unification.md`). Returns 38 doubles
/// `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skewCorrected, width, height, refreshHz, framesLost,
/// bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms,
/// netP50Ms, lostWindow, skippedWindow, fecWindow, framesWindow, dispValid, displayP50Ms,
/// e2eDispP50Ms, e2eDispP95Ms, paceP50Ms, latchP50Ms, presentsWindow, presenterActive,
/// feedP50Ms, codecP50Ms, skippedOverflowWindow, audioBufferMs, audioAvOffsetMs, audioCodec,
/// audioRateHz, audioBits]`
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
/// newest-wins pacing); 33/34 are the AUDIO plane's latency — the playback ring's live depth in ms
/// and the A/V sync loop's smoothed offset in ms (positive = audio behind the picture) — both live
/// gauges rather than windowed samples, like the cumulative drop total at 9; 35–37 are the audio
/// FORMAT the host resolved at the handshake — `audioCodec` (`0` = Opus on `0xC9`, `2` = lossless
/// PCM on `0xD3`), the resolved rate in Hz and the resolved depth in bits. Static for the session,
/// and here because `design/hi-res-audio.md` §10 requires a surface for the RESOLVED format rather
/// than the requested one: a session that spends 4.6 Mbps and a session whose host quietly
/// declined look identical from the outside, which is §4.3's failure wearing a UI hat), or `null`
/// when no decode thread is running.
/// Poll ~1 Hz from the UI; each call
/// resets the measurement window. Not android-gated — pure `jni` + connector reads, so it links on
/// the host build too (Kotlin only ever calls it on device).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoStats<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JDoubleArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JDoubleArray<'local>> {
        if handle == 0 {
            return Ok(JDoubleArray::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        if lock_recover(&h.video).is_none() {
            return Ok(JDoubleArray::default()); // not streaming → no stats
        }
        let snap = h
            .stats
            .drain(h.client.frames_dropped(), h.client.fec_recovered_shards());
        let mode = h.client.mode();
        let color = h.client.color;
        let buf: [f64; 38] = [
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
            // The audio plane's own latency (`design/audio-latency-overhaul.md`): how much decoded
            // audio is queued ahead of the speaker, and where the A/V sync loop measures that
            // PUTS it relative to the picture (+ = audio behind). Both, because a deep ring on a
            // jittery link is correct behaviour and only the offset tells that apart from audio
            // simply held late. Live gauges written by the audio thread — before this the whole
            // plane published nothing any surface could render, so a "the audio delay is way too
            // high" report had no instrument behind it at all.
            h.client.audio_buffer_ms() as f64,
            h.client.audio_av_offset_ms() as f64,
            // The audio format the host RESOLVED (`Welcome`), not what this device asked for.
            // A lossless session and a session whose host declined lossless are indistinguishable
            // from the outside — same picture, same latency figures, one of them quietly spending
            // 2.3–4.6 Mbps of the link on nothing — so the HUD has to be able to name which
            // (`design/hi-res-audio.md` §10, and §4.3 for why it matters). Static for the session:
            // the plane is settled at the handshake and the host never switches it underneath a
            // client whose output device is already open.
            h.client.audio_codec as f64,
            h.client.audio_sample_rate_hz as f64,
            h.client.audio_bits as f64,
        ];
        let arr = env.new_double_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeVideoSize(handle): IntArray?` — the negotiated video mode as
/// `[width, height, refreshHz]`. Resolved at the handshake (Welcome), so it is known before a
/// single frame arrives: the UI sizes the video surface to the STREAM's aspect rather than
/// stretching it to the panel's, and pins the panel's display mode to the stream refresh. The
/// trailing `refreshHz` was appended later — old readers index only 0/1 and never see it. `null`
/// on a `0` handle. Not android-gated — pure `jni` + a connector read, so it links on the host
/// build too. Cheap; safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeVideoSize<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        if handle == 0 {
            return Ok(JIntArray::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        let mode = h.client.mode();
        let buf: [i32; 3] = [
            mode.width as i32,
            mode.height as i32,
            mode.refresh_hz as i32,
        ];
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetVideoStatsEnabled(handle, enabled)` — gate per-frame stats sampling on the
/// HUD actually being visible: while disabled the decode thread skips the clock read + lock per AU.
/// Enabling resets the measurement window so a later show never reports stale data. Sticky for the
/// session (survives video stop/start across surface recreation). No-op on `0`. Not android-gated —
/// pure `jni` + an atomic store, so it links on the host build too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetVideoStatsEnabled(
    _env: EnvUnowned,
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
                enabled,
                h.client.frames_dropped(),
                h.client.fec_recovered_shards(),
            );
        }
    })
}

/// `NativeBridge.nativeStartAudio(handle, lowLatencyMode, isTv)` — start the Opus→AAudio playback
/// supervisor. `lowLatencyMode` (the experimental toggle) tags the stream usage=Game for the HAL's
/// game-audio routing; `isTv` steers the AAudio open ladder (see `crate::audio::open_ladder`).
/// No-op if already started or on a `0` handle. Best-effort: a failure leaves video streaming.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    low_latency_mode: jboolean,
    is_tv: jboolean,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = lock_recover(&h.audio);
    if guard.is_some() {
        return; // already playing
    }
    match crate::audio::AudioPlayback::start(h.client.clone(), low_latency_mode, is_tv) {
        Some(p) => *guard = Some(p),
        None => log::error!("nativeStartAudio: playback init failed (video unaffected)"),
    }
}

/// `NativeBridge.nativeStopAudio(handle)` — stop + join the audio thread and close AAudio (without
/// closing the session). No-op on `0`.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopAudio(
    _env: EnvUnowned,
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartMic(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    echo_cancel: jboolean,
) -> jni::sys::jint {
    if handle == 0 {
        return 0;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let mut guard = lock_recover(&h.mic);
    if let Some(m) = guard.as_ref() {
        return m.session_id(); // already capturing — same stream, same session
    }
    // The capture SHARES the session's mute flag, so one started while muted stays muted (and
    // sends nothing) from its very first frame — see `SessionHandle::mic_muted`.
    match crate::mic::MicCapture::start(h.client.clone(), echo_cancel, h.mic_muted.clone()) {
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopMic(
    _env: EnvUnowned,
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

/// `NativeBridge.nativeStartPadAudio(handle, pad, fd, haptics, speaker): Boolean` — start tier-A
/// DualSense pad audio on a descriptor Kotlin has already obtained.
///
/// `fd` comes from `UsbDeviceConnection.getFileDescriptor()` **after** claiming the pad's audio
/// streaming interface. Kotlin owns that connection and **must keep it open until
/// `nativeStopPadAudio` returns**: the renderer borrows the descriptor and never closes it, so
/// closing early would pull it out from under an in-flight isochronous transfer.
///
/// Returns `false` when there is nothing to render (both kinds disabled) or the thread would not
/// start. A kernel that refuses the interface claim is NOT reported here — the renderer discovers
/// that on its own thread and degrades to tier C, because some OEM kernels refuse and there is no
/// app-side fix worth blocking a session on.
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStartPadAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jni::sys::jint,
    fd: jni::sys::jint,
    haptics: jboolean,
    speaker: jboolean,
) -> jboolean {
    jni_guard(false, || {
        if handle == 0 || fd < 0 || !(0..16).contains(&pad) {
            return false;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        // Replace any previous renderer first: dropping it joins the old thread, so two of them
        // can never hold the same descriptor at once.
        h.stop_pad_audio();
        // The capability declaration and the rumble suppression are NOT done here: the renderer
        // makes both only once its USB stream actually opens (see `pad_audio::render`). Doing them
        // at spawn time would, on a kernel that refuses the interface claim, take the pad off wire
        // rumble and give it nothing in return — no haptics of any kind.
        match crate::pad_audio::start(
            std::sync::Arc::clone(&h.client),
            pad as u8,
            fd,
            haptics,
            speaker,
        ) {
            Some(p) => {
                *lock_recover(&h.pad_audio) = Some(p);
                true
            }
            None => false,
        }
    })
}

/// `NativeBridge.nativePadAudioSelfTest(fd, seconds, hz): Int` — drive the pad directly with a
/// tone through the real client render path, with no host and no session involved.
///
/// The check a standalone harness cannot make: it owns its descriptor by construction, so it can
/// never reveal that the client handed the renderer a descriptor something else was already
/// driving. Returns sample frames written, or negative on failure (see `pad_audio::SelfTest`).
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePadAudioSelfTest(
    _env: EnvUnowned,
    _this: JObject,
    fd: jni::sys::jint,
    seconds: jni::sys::jint,
    hz: jni::sys::jint,
) -> jni::sys::jint {
    jni_guard(-1, || {
        if fd < 0 {
            return -1;
        }
        // SAFETY: Kotlin holds the owning UsbDeviceConnection open across this call and drives no
        // other transfers on it (it opens a dedicated connection for exactly this).
        unsafe { crate::pad_audio::self_test(fd, seconds, hz) }
    })
}

/// `NativeBridge.nativeStopPadAudio(handle, pad)` — stop tier-A pad audio and join its thread.
///
/// Returns only once the render thread is joined, which is the point: Kotlin may close the
/// `UsbDeviceConnection` as soon as this returns and not before.
#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeStopPadAudio(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jni::sys::jint,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the nativeConnect/nativeClose contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.stop_pad_audio();
            if (0..16).contains(&pad) {
                // Withdraw the capability and hand the pad back to wire rumble, in that order:
                // the host stops sending 0xD1 before tier C resumes, so the two never overlap.
                h.client.set_pad_audio_caps(pad as u8, 0);
                crate::pad_audio::set_tier_a(pad as u8, false);
                crate::pad_audio::clear_haptics_liveness(pad as u8);
            }
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetMicMuted(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    muted: jboolean,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: live handle per the nativeConnect/nativeClose contract.
            let h = unsafe { &*(handle as *const SessionHandle) };
            h.mic_muted
                .store(muted, std::sync::atomic::Ordering::Relaxed);
        }
    })
}

/// `NativeBridge.nativeMicActive(handle): Boolean` — is a mic capture actually RUNNING? `true` only
/// between a `nativeStartMic` that opened a stream and the matching `nativeStopMic`. The in-stream
/// mute control is offered on this evidence rather than on the user's setting, so a device that
/// refused every AAudio input rung (or a missing RECORD_AUDIO grant) shows no control instead of a
/// lie about a mic that is being heard. `false` on a `0` handle. Cheap (one uncontended lock).
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeMicActive(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        if handle == 0 {
            return false;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        lock_recover(&h.mic).is_some()
    })
}
