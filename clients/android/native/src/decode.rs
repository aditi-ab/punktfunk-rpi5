//! Android video decode (android-only): pull HEVC access units from the connector and render them
//! to the SurfaceView via NDK `AMediaCodec` — hardware decode, zero per-frame JNI.
//!
//! One-in/one-out: the host opens every stream with an IDR carrying VPS/SPS/PPS **in-band**, so the
//! decoder needs no out-of-band codec-specific data — we configure with mime + the negotiated
//! WxH (from [`NativeClient::mode`]) and feed each access unit as it arrives. The decode thread owns
//! the codec + window for its whole life; [`crate::session`] signals it to stop via the shared flag.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::{
    AsyncNotifyCallback, DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec,
    MediaCodecDirection, OutputBuffer,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Cap on AUs parked in the async loop awaiting a free codec input slot. Matches the connector's
/// own frame-channel depth; on sustained overflow the oldest is dropped and a keyframe requested
/// (same recovery as a reassembler drop). In steady state this stays near-empty.
const FRAME_PARK_CAP: usize = 16;

/// Cap on the pts→received-timestamp map below: MediaCodec holds only a handful of frames in
/// flight, so anything beyond this is stale (codec flushed / HUD toggled) and gets evicted.
const IN_FLIGHT_CAP: usize = 64;

/// Cap on received AUs awaiting their 0xCF host timing (Phase 2 host/network split): the timing
/// datagram trails its AU by at most the wire, so a match lands within a frame or two — anything
/// this deep is a lost datagram (or an old host that never sends any) and gets evicted.
const PENDING_SPLIT_CAP: usize = 256;

/// Cap on rendered frames parked in [`DisplayTracker`] awaiting their `OnFrameRendered` render
/// timestamp: the callback trails its release by at most a vsync or two, so anything this deep
/// means the platform stopped delivering render callbacks (allowed under load, per the docs) and
/// gets evicted.
const RENDERED_CAP: usize = 64;

/// Whether low-latency mode uses the event-driven async decode loop (default) or the synchronous
/// poll loop. Flip to `false` to A/B the two on the HUD (`design/…`); the async loop presents a
/// decoded frame the instant it's ready instead of waiting out a poll interval. Only consulted when
/// the user's "Low-latency mode" toggle is ON (now the default) — off, the sync loop always runs (the
/// original pipeline, kept as the per-device escape hatch).
const USE_ASYNC_DECODE: bool = true;

/// Per-session decode configuration, resolved by the JNI layer (`nativeStartVideo`) and passed to
/// the decode loop. Bundled so the loop entry points don't sprout a wide argument list.
pub(crate) struct DecodeOptions {
    /// The decoder Kotlin ranked from `MediaCodecList` (`VideoDecoders.pickDecoder`). `None`/empty ⇒
    /// let the platform resolve the default decoder for the MIME.
    pub decoder_name: Option<String>,
    /// Whether Kotlin found the chosen decoder advertises `FEATURE_LowLatency` (queryable only via
    /// the Java `CodecCapabilities` API) — surfaced on the HUD next to the decoder name.
    pub ll_feature: bool,
    /// The user's "Low-latency mode" master toggle. On (default) ⇒ the full fast pipeline: async
    /// decode loop, per-SoC vendor keys, pipeline thread boosts, ADPF max-performance, forced TV
    /// mode switch. Off ⇒ the original synchronous pre-overhaul pipeline, kept as the per-device
    /// escape hatch.
    pub low_latency_mode: bool,
    /// TV form factor (Kotlin's `UiModeManager`): actively drive the HDMI output into the stream's
    /// refresh mode, vs. the softer seamless hint on a phone/tablet.
    pub is_tv: bool,
}

/// The decode entry point on the `pf-decode` thread: dispatches to the async or synchronous loop.
/// Both run until `shutdown` is set or the session closes.
pub fn run(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    if opts.low_latency_mode && USE_ASYNC_DECODE {
        run_async(client, window, shutdown, stats, opts);
    } else {
        run_sync(client, window, shutdown, stats, opts);
    }
}

/// The synchronous poll loop — the original decode path: the only one when low-latency mode is off,
/// and the [`USE_ASYNC_DECODE`] A/B fallback when it's on. Feeds and drains on this one thread; the
/// only blocking wait is a short output dequeue while input is backed up.
fn run_sync(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    let DecodeOptions {
        decoder_name,
        ll_feature,
        low_latency_mode,
        is_tv,
    } = opts;
    boost_thread_priority();
    let mode = client.mode();
    // The MediaCodec MIME for the codec the host resolved (`Welcome.codec`). AMediaCodec needs no
    // out-of-band extradata — the in-band VPS/SPS/PPS on every IDR configure it either way.
    let mime = codec_mime(client.codec);
    let codec = match create_codec(mime, decoder_name.as_deref()) {
        Some(c) => c,
        None => {
            log::error!("decode: no {mime} decoder on this device");
            return;
        }
    };
    // The decoder's *actual* resolved name (Kotlin's pick, or the platform default when it fell
    // back) drives both the HUD label and which vendor low-latency keys apply below.
    let codec_name = codec.name().unwrap_or_default();
    stats.set_decoder(&codec_name, ll_feature);
    log::info!(
        "decode: codec mime = {mime}, decoder = {codec_name} (low-latency feature: {ll_feature})"
    );

    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    // Generous input buffer so a large keyframe AU is never truncated.
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    // Standard + per-SoC vendor low-latency keys and the clock hints, gated on the resolved decoder
    // name and the master toggle (see `configure_low_latency`).
    configure_low_latency(&mut format, &codec_name, low_latency_mode);

    // HDR static metadata (ST.2086 mastering + content light level): when an HDR session was
    // negotiated, set KEY_HDR_STATIC_INFO so the display tone-maps from the source's real grade.
    // MediaCodec wants it BEFORE configure(), and the host sends a 0xCE right after the handshake,
    // so it's typically already queued; wait briefly otherwise. The Surface DataSpace (applied on
    // OutputFormatChanged below) carries transfer/primaries regardless — this adds the luminance the
    // tone-mapper needs. A non-HDR display still gets sensible SurfaceFlinger tone-mapping.
    if client.color.is_hdr() {
        match client.next_hdr_meta(Duration::from_millis(250)) {
            Ok(meta) => {
                format.set_buffer("hdr-static-info", &android_hdr_static_info(&meta));
                log::info!("decode: HDR static metadata applied (KEY_HDR_STATIC_INFO)");
            }
            Err(_) => {
                log::info!("decode: HDR session but no mastering metadata yet — DataSpace only")
            }
        }
    }

    if let Err(e) = codec.configure(&format, Some(&window), MediaCodecDirection::Decoder) {
        log::error!("decode: configure failed: {e}");
        return;
    }
    if let Err(e) = codec.start() {
        log::error!("decode: start failed: {e}");
        return;
    }
    log::info!(
        "decode: HEVC decoder started at {}x{}",
        mode.width,
        mode.height
    );
    // Tell the display the stream's refresh so Android can pick a matching display mode and align
    // vsync (no 60-in-120 judder on high-refresh panels). `ANativeWindow_setFrameRate` is NDK API 30,
    // above our API-28 floor, so we resolve it at runtime (see `try_set_frame_rate`) rather than link
    // it — a hard import would stop `libpunktfunk_android.so` loading at all on API 28/29. Absent
    // there ⇒ we simply skip the hint (non-fatal; the stream renders fine without it).
    // The forced TV mode switch (`is_tv` ⇒ ALWAYS strategy) is part of the experimental stack;
    // off, every form factor gets the original soft seamless hint.
    if mode.refresh_hz > 0
        && !try_set_frame_rate(&window, mode.refresh_hz as f32, is_tv && low_latency_mode)
    {
        log::debug!(
            "decode: set_frame_rate({} Hz) unavailable/declined (non-fatal)",
            mode.refresh_hz
        );
    }

    // ADPF: hint the platform that the whole video pipeline — this pf-decode feed/drain/present
    // loop, the core's data-plane pump (UDP receive + FEC reassembly), and the audio thread — runs a
    // per-frame real-time workload, so the CPU governor keeps those threads on fast cores at high
    // clocks instead of down-clocking between frames or parking them on a little core. Snapdragon's
    // ADPF backend responds well to this. We register this thread now but create the session lazily
    // on the first presented frame: by then the pump + audio threads have registered their ids too,
    // and ADPF `createSession` rejects a set with any not-yet-live/dead tid. No-op below API 33.
    let frame_period_ns = if mode.refresh_hz > 0 {
        1_000_000_000i64 / mode.refresh_hz as i64
    } else {
        0
    };
    client.register_hot_thread(); // this decode thread → the pipeline's hot-thread set
    let mut hint: Option<crate::adpf::HintSession> = None;
    let mut hint_tried = false;
    // Accumulates the loop's productive (feed+drain) time between displayed frames; reported to ADPF
    // once per rendered frame against the frame-period target.
    let mut work_accum_ns: i64 = 0;

    let mut fed: u64 = 0;
    let mut rendered: u64 = 0;
    let mut discarded: u64 = 0;
    // AUs larger than the codec input buffer, dropped whole (see `feed`/`feed_ready`).
    let mut oversized_dropped: u64 = 0;
    // The AU waiting for a free codec input buffer. `feed` is non-blocking; on transient input
    // pressure the AU stays parked here instead of being dropped (a drop forces a keyframe
    // round-trip) and we only pop the next one once it's queued.
    let mut pending: Option<Frame> = None;
    // Loss recovery: watch the host→client unrecoverable-drop count and ask for an IDR when it
    // climbs.
    let mut last_dropped = client.frames_dropped();
    let mut last_kf_req: Option<Instant> = None;
    // Skew-corrected latency stats (spec: design/stats-unification.md) use the negotiated
    // host-minus-client clock offset (0 if the host didn't answer the skew handshake — then the
    // HUD flags it "(same-host clock)").
    let clock_offset = client.clock_offset_shared();
    // Display stage (spec `display` + the capture→displayed headline): frames released with
    // render = true are parked in the tracker; the OnFrameRendered callback pairs them with
    // SurfaceFlinger's render timestamp. `render_cb` is the callback's leaked Arc refcount,
    // reclaimed after the codec is dropped below.
    let tracker = DisplayTracker::new(stats.clone(), clock_offset.clone());
    let render_cb = install_render_callback(&codec, &tracker);
    // HUD stage split: receipt timestamps keyed by the pts we queue into the codec, so the decoded
    // point (output-buffer dequeue — MediaCodec round-trips presentationTimeUs) can be paired back
    // to its receipt for the `decode` stage. Only fed while the HUD is visible.
    let mut in_flight: VecDeque<(u64, i128)> = VecDeque::new();
    // Phase-2 host/network split (design/stats-unification.md): received AUs awaiting their 0xCF
    // host timing, as (pts_ns, capture→received µs). The timings are drained non-blockingly right
    // where receipts are recorded and matched by pts; `network = hostnet − host` (saturating).
    // Only fed while the HUD is visible; an old host never sends a 0xCF, so entries just age out.
    let mut pending_split: VecDeque<(u64, u64)> = VecDeque::new();
    // The dataspace we've signalled on the Surface so far (None = default/SDR). Set reactively once
    // the decoder reports an HDR stream (see `drain`); avoids re-applying every format event.
    let mut applied_ds: Option<DataSpace> = None;
    // One thread feeds AND drains: the NDK AMediaCodec wrapper isn't documented thread-safe for
    // cross-thread feed/drain, so instead of splitting threads the loop decouples the two — input
    // dequeue is non-blocking (never stalls presentation of already-decoded frames) and the only
    // blocking wait is a short output dequeue while input is backed up (decoder progress is exactly
    // what frees the next input buffer).
    while !shutdown.load(Ordering::Relaxed) {
        if pending.is_none() {
            match client.next_frame(Duration::from_millis(5)) {
                Ok(frame) => {
                    if fed == 0 {
                        let p = &frame.data;
                        log::info!(
                            "decode: first AU {} bytes, head {:02x?}",
                            p.len(),
                            &p[..p.len().min(6)]
                        );
                    }
                    // HUD stat, `received` point: host+network = client_now + (host−client) −
                    // capture_pts. Gated on the HUD being visible — `enabled` first so the hidden
                    // steady state skips the wall-clock read and the lock entirely. The receipt
                    // stamp is also parked in `in_flight` (keyed by the pts the codec will echo on
                    // the output buffer) for the decoded-point pairing in `drain`.
                    if stats.enabled() {
                        let received_ns = now_realtime_ns();
                        let clock_offset = clock_offset.load(Ordering::Relaxed);
                        let lat_ns = received_ns + clock_offset as i128 - frame.pts_ns as i128;
                        let lat_us = (lat_ns > 0 && lat_ns < 10_000_000_000)
                            .then_some((lat_ns / 1000) as u64);
                        stats.note_received(frame.data.len(), lat_us, clock_offset != 0);
                        in_flight.push_back((frame.pts_ns / 1000, received_ns));
                        if in_flight.len() > IN_FLIGHT_CAP {
                            in_flight.pop_front(); // stale — codec never echoed it back
                        }
                        // Phase-2 split: park this AU's capture→received sample, then match any
                        // 0xCF host timings that have arrived — host = the host's own
                        // capture→sent, network = our capture→received minus it (per-frame
                        // tiling; saturating in case of clock jitter).
                        if let Some(hostnet_us) = lat_us {
                            pending_split.push_back((frame.pts_ns, hostnet_us));
                            if pending_split.len() > PENDING_SPLIT_CAP {
                                pending_split.pop_front(); // 0xCF lost / old host — evict
                            }
                        }
                        while let Ok(t) = client.next_host_timing(Duration::ZERO) {
                            if let Some(i) = pending_split.iter().position(|&(p, _)| p == t.pts_ns)
                            {
                                let (_, hostnet_us) = pending_split.remove(i).unwrap();
                                stats.note_host_split(
                                    t.host_us as u64,
                                    hostnet_us.saturating_sub(t.host_us as u64),
                                );
                            }
                        }
                    }
                    pending = Some(frame);
                }
                Err(PunktfunkError::NoFrame) => {} // timeout — still drain output below
                Err(_) => break,                   // session closed
            }
        }
        // Time the productive work (feed + drain) only — the `next_frame` poll wait above is idle
        // and excluded, so ADPF sees this thread's real per-frame CPU cost, not the poll timeout.
        let work_t0 = Instant::now();
        if let Some(frame) = pending.take() {
            if feed(
                &codec,
                &client,
                &frame.data,
                frame.pts_ns / 1000,
                &mut oversized_dropped,
            ) {
                fed += 1;
                if fed % 300 == 0 {
                    log::info!("decode: fed={fed} rendered={rendered} discarded={discarded}");
                }
            } else {
                // No input buffer free — transient back-pressure. Keep the AU and let `drain` block
                // briefly below; a released output buffer is what recycles an input slot.
                pending = Some(frame);
            }
        }
        // Drain every iteration. When input is blocked, wait ~2 ms on output so the loop rides
        // decoder progress instead of busy-spinning against a full input queue.
        let wait = if pending.is_some() {
            Duration::from_millis(2)
        } else {
            Duration::ZERO
        };
        let (r, d) = drain(
            &codec,
            &window,
            &mut applied_ds,
            wait,
            &stats,
            &mut in_flight,
            clock_offset.load(Ordering::Relaxed),
            &tracker,
        );
        rendered += r;
        discarded += d;

        // ADPF: attribute this iteration's feed+drain time to the frame being produced, and report
        // the accumulated per-frame work once one is actually presented (r > 0). Under back-pressure
        // the short output-dequeue wait is included in the tally — for a latency-first client,
        // biasing the governor toward "boost" is the desired behaviour. Cheap when `hint` is None
        // (one `Instant` diff, no report).
        work_accum_ns += work_t0.elapsed().as_nanos() as i64;
        if r > 0 {
            if !hint_tried {
                // First presented frame: the pump + audio threads have registered their ids by now.
                // Build one ADPF session over the whole pipeline's thread set (empty below API 33,
                // or where the platform declines → `None`, and the loop runs unhinted).
                hint_tried = true;
                let tids = client.hot_thread_ids();
                // The pump/audio priority boost is part of the experimental low-latency stack; the
                // ADPF session itself predates it and always runs (max-performance bias gated inside).
                if low_latency_mode {
                    boost_hot_threads(&tids);
                }
                hint = crate::adpf::HintSession::create(frame_period_ns, &tids, low_latency_mode);
                log::info!(
                    "decode: ADPF hint session {} — {} hot thread(s), target {frame_period_ns} ns",
                    if hint.is_some() {
                        "active"
                    } else {
                        "unavailable"
                    },
                    tids.len(),
                );
            }
            if let Some(h) = &hint {
                h.report_actual(work_accum_ns);
            }
            work_accum_ns = 0;
        }

        // Loss recovery: under infinite GOP the only recovery keyframe is one we request. The
        // reassembler drops unrecoverable AUs (frames_dropped); the decoder then conceals the
        // reference-missing delta frames that follow and renders them without error, so keying off
        // a decode error rarely fires. Request an IDR when the drop count climbs, throttled — the
        // decode stays wedged for several frames until the IDR lands, so requesting every frame
        // would flood the control stream.
        let dropped = client.frames_dropped();
        if dropped > last_dropped {
            last_dropped = dropped;
            let now = Instant::now();
            if last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                last_kf_req = Some(now);
                let _ = client.request_keyframe();
                log::debug!("decode: requested keyframe (loss recovery, dropped={dropped})");
            }
        }
    }

    let _ = codec.stop();
    drop(codec); // AMediaCodec_delete — after this no render callback can fire
    if let Some(ud) = render_cb {
        // SAFETY: the codec was dropped above; this registration's single reclaim.
        unsafe { release_render_callback(ud) };
    }
    log::info!("decode: stopped (fed={fed} rendered={rendered} discarded={discarded})");
}

/// Wall-clock now in nanoseconds (CLOCK_REALTIME basis), to compare against the host-stamped
/// capture `pts_ns` after the skew offset is applied.
fn now_realtime_ns() -> i128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// `CLOCK_MONOTONIC` now in nanoseconds — the base of the `systemNano` render timestamp the
/// `OnFrameRendered` callback reports (Android's `System.nanoTime`), read only to re-base that
/// stamp onto `CLOCK_REALTIME` (see [`on_frame_rendered`]).
fn now_monotonic_ns() -> i128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` with a valid out-pointer is an always-safe syscall.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
}

/// State shared between the decode loop and the `AMediaCodec` `OnFrameRendered` callback (which
/// fires on a codec-internal thread): rendered frames awaiting their render timestamp, so the HUD
/// gets the spec's `display` stage (decoded→displayed) and the `capture→displayed` end-to-end
/// headline (`design/stats-unification.md` — this replaces Android's v1 `capture→decoded`
/// endpoint whenever the platform delivers render callbacks).
struct DisplayTracker {
    stats: Arc<crate::stats::VideoStats>,
    /// Live host-minus-client clock offset (ns) for the skew-corrected end-to-end sample —
    /// loaded per callback so mid-stream re-syncs apply. Holding the handle (not the client)
    /// keeps the leaked render-callback refcount from pinning the whole session alive.
    clock_offset: Arc<AtomicI64>,
    /// `(pts_us, decoded_real_ns)` of frames released with `render = true`, in release order,
    /// awaiting their callback. Pushes are HUD-gated by the caller, so this stays empty (and the
    /// callback early-outs) while the overlay is hidden.
    rendered: Mutex<VecDeque<(u64, i128)>>,
}

impl DisplayTracker {
    fn new(
        stats: Arc<crate::stats::VideoStats>,
        clock_offset: Arc<AtomicI64>,
    ) -> Arc<DisplayTracker> {
        Arc::new(DisplayTracker {
            stats,
            clock_offset,
            rendered: Mutex::new(VecDeque::new()),
        })
    }

    /// Park one just-rendered frame's `(pts, decoded stamp)` for the render callback to pair.
    /// Caller gates on the HUD being visible.
    fn note_rendered(&self, pts_us: u64, decoded_ns: i128) {
        let mut g = self
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.push_back((pts_us, decoded_ns));
        if g.len() > RENDERED_CAP {
            g.pop_front(); // render callbacks stopped coming (allowed under load) — evict
        }
    }
}

/// Register [`on_frame_rendered`] on the codec (`AMediaCodec_setOnFrameRenderedCallback`,
/// **API 33** — "Available since Android T" per the NDK header; only the *Java* listener dates
/// back further). That sits above the API-28 floor, so the entry point is dlsym-resolved at
/// runtime like [`try_set_frame_rate`] — hard-linking it (as 0.9.0 shipped) made
/// `System.loadLibrary` fail on every pre-Android-13 device, taking down all of `NativeBridge`.
/// The `ndk` wrapper has no binding and the call needs the raw codec pointer, which is what the
/// vendored crate's public `as_ptr` patch is for. Returns the userdata pointer holding a leaked
/// `Arc<DisplayTracker>` refcount; the caller MUST reclaim it with [`release_render_callback`]
/// AFTER dropping the codec (`AMediaCodec_delete` is what guarantees no further callback can
/// fire). `None` (nothing to reclaim) if the symbol is absent (API < 33) or the platform refused —
/// the HUD then simply has no `display` stage, exactly the pre-callback behaviour.
fn install_render_callback(
    codec: &MediaCodec,
    tracker: &Arc<DisplayTracker>,
) -> Option<*const DisplayTracker> {
    // media_status_t AMediaCodec_setOnFrameRenderedCallback(
    //     AMediaCodec*, AMediaCodecOnFrameRendered, void*)                            (API 33)
    type SetOnFrameRenderedFn = unsafe extern "C" fn(
        *mut ndk_sys::AMediaCodec,
        ndk_sys::AMediaCodecOnFrameRendered,
        *mut c_void,
    ) -> ndk_sys::media_status_t;
    // SAFETY: `dlopen` of `libmediandk.so`, which the `ndk` media wrapper already links — always
    // mapped, so this only bumps its refcount (never closed — process-lifetime handle). `dlsym`
    // returns null when the symbol is absent (device below API 33), checked before transmuting the
    // non-null pointer to its fn-pointer type.
    let set_on_frame_rendered = unsafe {
        let lib = libc::dlopen(c"libmediandk.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let sym = libc::dlsym(lib, c"AMediaCodec_setOnFrameRenderedCallback".as_ptr());
        if sym.is_null() {
            log::info!("decode: no render callback on this API level (<33) — no display stage");
            return None;
        }
        std::mem::transmute::<*mut c_void, SetOnFrameRenderedFn>(sym)
    };
    let ud = Arc::into_raw(tracker.clone());
    // SAFETY: `codec.as_ptr()` is the live codec this thread owns; `ud` outlives the registration
    // (reclaimed only after the codec is deleted, per this function's contract).
    let status = unsafe {
        set_on_frame_rendered(codec.as_ptr(), Some(on_frame_rendered), ud as *mut c_void)
    };
    if status == ndk_sys::media_status_t::AMEDIA_OK {
        Some(ud)
    } else {
        log::warn!("decode: setOnFrameRenderedCallback failed ({status:?}) — no display stage");
        // SAFETY: registration failed, so the codec never took the reference — reclaim it now.
        unsafe { drop(Arc::from_raw(ud)) };
        None
    }
}

/// Reclaim [`install_render_callback`]'s leaked `Arc` refcount.
///
/// # Safety
/// Call exactly once, and only after the codec the callback was registered on has been dropped —
/// deleting the codec stops its internal threads, so no callback can still be running (or run
/// later) against this pointer.
unsafe fn release_render_callback(ud: *const DisplayTracker) {
    drop(Arc::from_raw(ud));
}

/// The `AMediaCodecOnFrameRendered` trampoline: fires (possibly batched) on a codec-internal
/// thread once per output frame actually placed on the output surface, with SurfaceFlinger's
/// render timestamp. That timestamp (`system_nano`) is on `CLOCK_MONOTONIC`, so it is re-based
/// onto `CLOCK_REALTIME` here — against monotonic-now at callback time, which also cancels any lag
/// between the frame rendering and the (batchable) callback delivery — to subtract against the
/// receipt/decode stamps and the host capture pts. Records the HUD's `displayed` point:
/// `end-to-end` = capture→displayed (skew-corrected) and `display` = decoded→displayed
/// (single-clock local). Panic-free by construction (poison-proof lock, saturating math) — an
/// unwind out of an `extern "C"` fn would abort the process.
unsafe extern "C" fn on_frame_rendered(
    _codec: *mut ndk_sys::AMediaCodec,
    userdata: *mut c_void,
    media_time_us: i64,
    system_nano: i64,
) {
    let t = &*(userdata as *const DisplayTracker);
    if !t.stats.enabled() {
        return; // HUD hidden — the ring is empty too (pushes are caller-gated)
    }
    let displayed_ns = now_realtime_ns() - (now_monotonic_ns() - system_nano as i128);
    let pts_us = media_time_us.max(0) as u64;
    // Pair the frame back to its release record, evicting older entries (their callbacks were
    // dropped by the platform, or the entry predates a HUD toggle) — same monotonic-eviction
    // discipline as `note_decoded_pts`.
    let mut decoded_ns = None;
    {
        let mut g = t
            .rendered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while let Some(&(p, d)) = g.front() {
            if p > pts_us {
                break; // future frame — leave it for its own callback
            }
            g.pop_front();
            if p == pts_us {
                decoded_ns = Some(d);
                break;
            }
        }
    }
    let e2e_ns =
        displayed_ns + t.clock_offset.load(Ordering::Relaxed) as i128 - pts_us as i128 * 1000;
    let e2e_us = (e2e_ns > 0 && e2e_ns < 10_000_000_000).then_some((e2e_ns / 1000) as u64);
    let display_us = decoded_ns.map(|d| ((displayed_ns - d).max(0) / 1000) as u64);
    t.stats.note_displayed(e2e_us, display_us);
}

/// The MediaCodec MIME for the codec the host resolved (`Welcome.codec`). Shared by the decode
/// thread and `nativeVideoMime` (which tells Kotlin what to rank decoders for). AV1 uses the
/// AOSP `video/av01` type; anything not H.264/AV1 is treated as HEVC (every pre-negotiation host
/// emitted HEVC).
pub(crate) fn codec_mime(codec: u8) -> &'static str {
    match codec {
        punktfunk_core::quic::CODEC_H264 => "video/avc",
        punktfunk_core::quic::CODEC_AV1 => "video/av01",
        _ => "video/hevc",
    }
}

/// Create the decoder: prefer the specific codec Kotlin ranked from `MediaCodecList`
/// (`from_codec_name`), falling back to the platform's default decoder for the MIME
/// (`from_decoder_type`) if that name can't be created (codec busy / renamed across an OS update).
fn create_codec(mime: &str, preferred: Option<&str>) -> Option<MediaCodec> {
    if let Some(name) = preferred.filter(|n| !n.is_empty()) {
        if let Some(c) = MediaCodec::from_codec_name(name) {
            return Some(c);
        }
        log::warn!(
            "decode: from_codec_name({name}) failed — falling back to default {mime} decoder"
        );
    }
    MediaCodec::from_decoder_type(mime)
}

/// Apply the low-latency MediaFormat keys for `codec_name`.
///
/// `aggressive` = the "Low-latency mode" master toggle. **Off** ⇒ the pre-overhaul key set,
/// byte-for-byte — the standard `low-latency` key, the blind Qualcomm vendor twin, `priority = 0` AND
/// `operating-rate = MAX` set together — kept as the per-device escape hatch (the profile every device
/// streamed with before the overhaul). **On** (default) ⇒ the Moonlight-parity
/// profile: MediaTek's `vdec-lowlatency` (unconditionally — ignored off MediaTek), the per-SoC
/// vendor extension keys (gated on the decoder-name prefix the way Moonlight-Android does, since a
/// key one vendor honours is meaningless on another), and one *mutually exclusive* clock hint.
///
/// Vendor keys mirror Moonlight's `MediaCodecHelper` (verified against current source): Qualcomm
/// picture-order + low-latency, Exynos (also Google Tensor), Amlogic, HiSilicon, MediaTek. NVIDIA
/// Tegra / Rockchip / Realtek expose no such key (nor does Moonlight) — they're covered by the
/// standard key + clock hint + being ranked first in `VideoDecoders`.
fn configure_low_latency(format: &mut MediaFormat, codec_name: &str, aggressive: bool) {
    // Standard key: request the no-reorder low-latency path where the platform decoder supports it.
    format.set_i32("low-latency", 1);
    if !aggressive {
        // The original profile: the Qualcomm vendor twin set blind (unknown keys are ignored by
        // other vendors' codecs), realtime priority, and the AOSP "unbounded" operating-rate
        // sentinel — decode each frame at max clocks rather than pacing to the frame rate.
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        format.set_i32("priority", 0); // 0 = realtime
        format.set_i32("operating-rate", i16::MAX as i32); // 32767 = "as fast as possible"
        return;
    }
    // MediaTek's low-latency key — very common (mid/budget phones + many Google TV / Fire TV boxes).
    // Set unconditionally like the standard key: MediaTek decoders honour it, others ignore it, so it
    // covers MediaTek whatever the exact decoder name (omx.mtk / c2.mtk / an OEM rename). Moonlight
    // does the same, and also relies on it for Amazon's Amlogic fork.
    format.set_i32("vdec-lowlatency", 1);
    let name = codec_name.to_ascii_lowercase();
    let is = |prefix: &str| name.starts_with(prefix);
    // Qualcomm Snapdragon (the most common phone SoC): picture-order forces decode-order output
    // (kills the reorder buffer on decoders that predate the standard key); low-latency is the older
    // vendor twin.
    if is("omx.qcom") || is("c2.qti") {
        format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    }
    // Samsung Exynos — also covers Google Tensor (Pixel 6+), whose hardware decoder is `c2.exynos.*`.
    if is("omx.exynos") || is("c2.exynos") {
        format.set_i32("vendor.rtc-ext-dec-low-latency.enable", 1);
    }
    // Amlogic — the Android TV boxes (onn 4K, Chromecast w/ Google TV, Homatics).
    if is("omx.amlogic") || is("c2.amlogic") {
        format.set_i32("vendor.low-latency.enable", 1);
    }
    // HiSilicon / Kirin (older Huawei; paired req/rdy keys).
    if is("omx.hisi") || is("c2.hisi") {
        format.set_i32(
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
            1,
        );
        format.set_i32(
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy",
            -1,
        );
    }
    // NVIDIA Tegra (Shield TV) and Rockchip/Realtek (budget TV boxes / smart TVs) expose no
    // low-latency vendor key (Moonlight has none either) — their decoders are already low-latency
    // oriented, so the standard `low-latency` key + the clock hint below + being ranked first
    // (see `VideoDecoders`) is their treatment.
    //
    // Clock hint, mutually exclusive (matching Moonlight): the AOSP "unbounded" operating-rate
    // sentinel (Short.MAX) tells the decoder to run each frame at max clocks and finish ASAP rather
    // than pace to the frame rate — shaving per-frame decode latency at a power/heat cost. Only
    // Qualcomm is known to handle the sentinel; every other vendor mis-paces on it, so they get the
    // plain realtime `priority` hint instead.
    if decoder_supports_max_operating_rate(&name) {
        format.set_i32("operating-rate", i16::MAX as i32); // 32767 = "as fast as possible"
    } else {
        format.set_i32("priority", 0); // 0 = realtime
    }
}

/// Whether a decoder tolerates `operating-rate = Short.MAX` rather than regressing on it. Follows
/// Moonlight's allowlist: Qualcomm decoders honour the sentinel (the Adreno 620 generation is the
/// known exception Moonlight excludes by GPU model — undetectable from native code here, so it
/// rides the master toggle as its escape hatch). Other vendors fall back to the plain `priority`
/// hint above.
fn decoder_supports_max_operating_rate(name_lower: &str) -> bool {
    name_lower.starts_with("omx.qcom") || name_lower.starts_with("c2.qti")
}

/// One decoded output buffer ready to release: its codec buffer index + the pts the codec echoed
/// (from the output callback's `BufferInfo`), used to pair the `decode` HUD stat, and the
/// wall-clock instant the output callback fired — the spec's `decoded` point ("decoder output
/// frame available"), stamped at the callback so the event-channel hop + coalescing wait in the
/// loop never inflates the decode stage.
struct OutputReady {
    index: usize,
    pts_us: u64,
    decoded_ns: i128,
}

/// Events the async decode loop reacts to. The codec's async-notify callbacks (which run on its
/// internal looper thread) push the codec ones; the feeder thread pushes `Au`. Each carries only
/// owned/`Copy` data so the callback closures satisfy the `Send` bound and never touch the codec.
enum DecodeEvent {
    /// A received access unit from the feeder, ready to queue into the decoder.
    Au(Frame),
    /// An input buffer slot freed (index) — we can queue an AU into it.
    InputAvailable(usize),
    /// A decoded frame is ready (buffer index + echoed pts + the callback-time `decoded` stamp).
    OutputAvailable {
        index: usize,
        pts_us: u64,
        decoded_ns: i128,
    },
    /// The output format changed — re-check the stream's colour signalling (HDR DataSpace).
    FormatChanged,
    /// The codec reported an error; `fatal` when neither recoverable nor transient.
    Error { fatal: bool },
}

/// The event-driven async decode loop (default; see [`run`]/[`USE_ASYNC_DECODE`]). The codec drives
/// us: an async-notify callback fires the instant an input buffer frees or a frame finishes
/// decoding, so a decoded frame is presented immediately instead of waiting out a poll interval (the
/// latency the sync loop left on the table). The callbacks run on the codec's internal looper thread
/// and only *push events* — every `AMediaCodec` buffer op stays on this thread, which owns the codec,
/// sidestepping the self-reference that would arise from a callback calling back into the codec it's
/// stored in. A small `pf-decode-feed` thread blocks on the network so this loop never does.
fn run_async(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    let DecodeOptions {
        decoder_name,
        ll_feature,
        low_latency_mode,
        is_tv,
    } = opts;
    boost_thread_priority();
    let mode = client.mode();
    let mime = codec_mime(client.codec);
    let mut codec = match create_codec(mime, decoder_name.as_deref()) {
        Some(c) => c,
        None => {
            log::error!("decode: no {mime} decoder on this device");
            return;
        }
    };
    let codec_name = codec.name().unwrap_or_default();
    stats.set_decoder(&codec_name, ll_feature);
    log::info!(
        "decode: codec mime = {mime}, decoder = {codec_name} (async, low-latency feature: {ll_feature})"
    );

    // The event channel: the callbacks + feeder push, this loop pulls. `Sender` is `Send`, so the
    // callback closures (each capturing a clone) satisfy the async-notify `Send` bound.
    let (ev_tx, ev_rx) = mpsc::channel::<DecodeEvent>();
    // Install the callbacks BEFORE configure()/start() so we're in async mode from the first buffer.
    // Each just forwards an index/flag — no codec access here (the codec owns these closures).
    {
        let out_tx = ev_tx.clone();
        let in_tx = ev_tx.clone();
        let fmt_tx = ev_tx.clone();
        let err_tx = ev_tx.clone();
        let cb = AsyncNotifyCallback {
            on_input_available: Some(Box::new(move |idx| {
                let _ = in_tx.send(DecodeEvent::InputAvailable(idx));
            })),
            on_output_available: Some(Box::new(move |idx, info| {
                let _ = out_tx.send(DecodeEvent::OutputAvailable {
                    index: idx,
                    pts_us: info.presentation_time_us().max(0) as u64,
                    // The `decoded` HUD point: stamp HERE, on the codec's looper thread, so the
                    // decode stage ends when the frame actually became available — not after the
                    // channel hop + whatever work the loop coalesces in front of presenting it.
                    decoded_ns: now_realtime_ns(),
                });
            })),
            on_format_changed: Some(Box::new(move |_fmt| {
                let _ = fmt_tx.send(DecodeEvent::FormatChanged);
            })),
            on_error: Some(Box::new(move |e, code, _detail| {
                let fatal = !code.is_recoverable() && !code.is_transient();
                log::warn!("decode: codec error {e:?} (fatal={fatal})");
                let _ = err_tx.send(DecodeEvent::Error { fatal });
            })),
        };
        if let Err(e) = codec.set_async_notify_callback(Some(cb)) {
            log::error!("decode: set_async_notify_callback failed: {e}");
            return;
        }
    }

    // Build the low-latency format (identical keys to the sync path).
    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    configure_low_latency(&mut format, &codec_name, low_latency_mode);
    if client.color.is_hdr() {
        match client.next_hdr_meta(Duration::from_millis(250)) {
            Ok(meta) => {
                format.set_buffer("hdr-static-info", &android_hdr_static_info(&meta));
                log::info!("decode: HDR static metadata applied (KEY_HDR_STATIC_INFO)");
            }
            Err(_) => {
                log::info!("decode: HDR session but no mastering metadata yet — DataSpace only")
            }
        }
    }
    if let Err(e) = codec.configure(&format, Some(&window), MediaCodecDirection::Decoder) {
        log::error!("decode: configure failed: {e}");
        return;
    }
    if let Err(e) = codec.start() {
        log::error!("decode: start failed: {e}");
        return;
    }
    log::info!(
        "decode: decoder started (async) at {}x{}",
        mode.width,
        mode.height
    );
    // The forced TV mode switch (`is_tv` ⇒ ALWAYS strategy) is part of the experimental stack;
    // off, every form factor gets the original soft seamless hint.
    if mode.refresh_hz > 0
        && !try_set_frame_rate(&window, mode.refresh_hz as f32, is_tv && low_latency_mode)
    {
        log::debug!(
            "decode: set_frame_rate({} Hz) unavailable/declined (non-fatal)",
            mode.refresh_hz
        );
    }

    // Skew-corrected latency stats (spec: design/stats-unification.md). Receipt stamps (keyed by the
    // pts we queue) live in a shared map: the feeder writes them at receipt, this loop pairs decoded
    // output back to them. Behind a `Mutex` since two threads touch it — only ever locked while the
    // HUD is visible.
    let clock_offset = client.clock_offset_shared();
    let in_flight = Arc::new(Mutex::new(VecDeque::<(u64, i128)>::new()));
    // Display stage (spec `display` + the capture→displayed headline): the rendered frame is
    // parked in the tracker at release; the OnFrameRendered callback pairs it with
    // SurfaceFlinger's render timestamp. `render_cb` is the callback's leaked Arc refcount,
    // reclaimed after the codec is dropped below.
    let tracker = DisplayTracker::new(stats.clone(), clock_offset.clone());
    let render_cb = install_render_callback(&codec, &tracker);

    // Feeder thread: block on the network so this loop doesn't (an AU's arrival becomes an event that
    // wakes us immediately, with no input-side poll latency). It also records the `received` HUD stat.
    let feeder = {
        let client = client.clone();
        let stats = stats.clone();
        let in_flight = in_flight.clone();
        let clock_offset = clock_offset.clone();
        let shutdown = shutdown.clone();
        let ev_tx = ev_tx.clone();
        std::thread::Builder::new()
            .name("pf-decode-feed".into())
            .spawn(move || {
                feeder_loop(client, stats, in_flight, clock_offset, shutdown, ev_tx);
            })
            .ok()
    };
    drop(ev_tx); // only the feeder + callbacks keep the channel alive now

    // ADPF: same as the sync path — register this thread now, create the session lazily on the first
    // presented frame (by when the pump + audio + feeder threads have registered their tids too).
    let frame_period_ns = if mode.refresh_hz > 0 {
        1_000_000_000i64 / mode.refresh_hz as i64
    } else {
        0
    };
    client.register_hot_thread();
    let mut hint: Option<crate::adpf::HintSession> = None;
    let mut hint_tried = false;

    let mut free_inputs: VecDeque<usize> = VecDeque::new();
    let mut pending_aus: VecDeque<Frame> = VecDeque::new();
    let mut ready: Vec<OutputReady> = Vec::new();
    let mut applied_ds: Option<DataSpace> = None;
    let mut fed: u64 = 0;
    let mut rendered: u64 = 0;
    let mut discarded: u64 = 0;
    // AUs larger than the codec input buffer, dropped whole (see `feed`/`feed_ready`).
    let mut oversized_dropped: u64 = 0;
    let mut last_dropped = client.frames_dropped();
    let mut last_kf_req: Option<Instant> = None;
    // Productive (dispatch+feed+present) time between displayed frames; reported to ADPF once one is
    // presented. The blocking event wait is excluded (idle, not work) — same accounting as the sync loop.
    let mut work_accum_ns: i64 = 0;
    let mut fatal = false;

    while !shutdown.load(Ordering::Relaxed) && !fatal {
        // Block for the next event (idle wait — excluded from the work tally). The short timeout
        // drives loss-recovery housekeeping when the pipeline is momentarily quiet.
        let ev0 = match ev_rx.recv_timeout(Duration::from_millis(5)) {
            Ok(ev) => Some(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let work_t0 = Instant::now();
        let mut fmt_dirty = false;
        let mut aus_dropped: u64 = 0;
        if let Some(ev) = ev0 {
            aus_dropped += u64::from(dispatch_event(
                ev,
                &mut pending_aus,
                &mut free_inputs,
                &mut ready,
                &mut fmt_dirty,
                &mut fatal,
            ));
        }
        // Coalesce every other event already queued into this one work pass — correct newest-only
        // presentation across a decode burst, and batched feeding.
        while let Ok(ev) = ev_rx.try_recv() {
            aus_dropped += u64::from(dispatch_event(
                ev,
                &mut pending_aus,
                &mut free_inputs,
                &mut ready,
                &mut fmt_dirty,
                &mut fatal,
            ));
        }
        stats.note_skipped(aus_dropped); // parked-AU overflow drops are client-side skips too
        if fmt_dirty {
            apply_hdr_dataspace(&codec, &window, &mut applied_ds);
        }
        feed_ready(
            &codec,
            &client,
            &mut pending_aus,
            &mut free_inputs,
            &mut fed,
            &mut oversized_dropped,
        );
        let had_output = !ready.is_empty();
        present_ready(
            &codec,
            &mut ready,
            &stats,
            &in_flight,
            clock_offset.load(Ordering::Relaxed),
            &tracker,
            &mut rendered,
            &mut discarded,
        );

        work_accum_ns += work_t0.elapsed().as_nanos() as i64;
        if had_output {
            if !hint_tried {
                hint_tried = true;
                let tids = client.hot_thread_ids();
                // The pump/audio priority boost is part of the experimental low-latency stack; the
                // ADPF session itself predates it and always runs (max-performance bias gated inside).
                if low_latency_mode {
                    boost_hot_threads(&tids);
                }
                hint = crate::adpf::HintSession::create(frame_period_ns, &tids, low_latency_mode);
                log::info!(
                    "decode: ADPF hint session {} — {} hot thread(s), target {frame_period_ns} ns",
                    if hint.is_some() {
                        "active"
                    } else {
                        "unavailable"
                    },
                    tids.len(),
                );
            }
            if let Some(h) = &hint {
                h.report_actual(work_accum_ns);
            }
            work_accum_ns = 0;
            if rendered > 0 && rendered % 300 == 0 {
                log::info!("decode: fed={fed} rendered={rendered} discarded={discarded}");
            }
        }
        // Loss recovery: request an IDR when the reassembler's unrecoverable-drop count climbs (or we
        // dropped a parked AU on overflow), throttled so a multi-frame recovery gap doesn't flood the
        // control stream.
        let dropped = client.frames_dropped();
        if dropped > last_dropped || aus_dropped > 0 {
            last_dropped = dropped;
            let now = Instant::now();
            if last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100)) {
                last_kf_req = Some(now);
                let _ = client.request_keyframe();
            }
        }
    }

    let _ = codec.stop();
    shutdown.store(true, Ordering::SeqCst); // ensure the feeder wakes and exits, then join it
    if let Some(j) = feeder {
        let _ = j.join();
    }
    drop(codec); // AMediaCodec_delete — after this no render callback can fire
    if let Some(ud) = render_cb {
        // SAFETY: the codec was dropped above; this registration's single reclaim.
        unsafe { release_render_callback(ud) };
    }
    log::info!("decode: stopped (async, fed={fed} rendered={rendered} discarded={discarded})");
}

/// The `pf-decode-feed` thread: block on the connector for the next access unit so the async loop
/// never has to. Records the `received` HUD stat (receipt point) — including the Phase-2 host/network
/// split from any matching 0xCF host timings — then hands the AU to the loop via the event channel.
/// Exits when `shutdown` is set, the session closes, or the loop's receiver is gone.
fn feeder_loop(
    client: Arc<NativeClient>,
    stats: Arc<crate::stats::VideoStats>,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    clock_offset: Arc<AtomicI64>,
    shutdown: Arc<AtomicBool>,
    ev_tx: mpsc::Sender<DecodeEvent>,
) {
    // Received AUs awaiting their 0xCF host timing (Phase-2 split), as (pts_ns, capture→received µs).
    let mut pending_split: VecDeque<(u64, u64)> = VecDeque::new();
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(5)) {
            Ok(frame) => {
                if stats.enabled() {
                    let received_ns = now_realtime_ns();
                    let clock_offset = clock_offset.load(Ordering::Relaxed) as i128;
                    let lat_ns = received_ns + clock_offset - frame.pts_ns as i128;
                    let lat_us =
                        (lat_ns > 0 && lat_ns < 10_000_000_000).then_some((lat_ns / 1000) as u64);
                    stats.note_received(frame.data.len(), lat_us, clock_offset != 0);
                    {
                        let mut g = in_flight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        g.push_back((frame.pts_ns / 1000, received_ns));
                        if g.len() > IN_FLIGHT_CAP {
                            g.pop_front(); // stale — codec never echoed it back
                        }
                    }
                    if let Some(hostnet_us) = lat_us {
                        pending_split.push_back((frame.pts_ns, hostnet_us));
                        if pending_split.len() > PENDING_SPLIT_CAP {
                            pending_split.pop_front();
                        }
                    }
                    while let Ok(t) = client.next_host_timing(Duration::ZERO) {
                        if let Some(i) = pending_split.iter().position(|&(p, _)| p == t.pts_ns) {
                            let (_, hostnet_us) = pending_split.remove(i).unwrap();
                            stats.note_host_split(
                                t.host_us as u64,
                                hostnet_us.saturating_sub(t.host_us as u64),
                            );
                        }
                    }
                }
                if ev_tx.send(DecodeEvent::Au(frame)).is_err() {
                    break; // the decode loop is gone
                }
            }
            Err(PunktfunkError::NoFrame) => {} // timeout — re-check shutdown and poll again
            Err(_) => break,                   // session closed
        }
    }
}

/// Route one [`DecodeEvent`] into the loop's working sets. Returns `true` only when a parked AU was
/// dropped on overflow (the caller then requests a keyframe).
fn dispatch_event(
    ev: DecodeEvent,
    pending_aus: &mut VecDeque<Frame>,
    free_inputs: &mut VecDeque<usize>,
    ready: &mut Vec<OutputReady>,
    fmt_dirty: &mut bool,
    fatal: &mut bool,
) -> bool {
    match ev {
        DecodeEvent::Au(f) => {
            pending_aus.push_back(f);
            if pending_aus.len() > FRAME_PARK_CAP {
                pending_aus.pop_front(); // sustained overflow — drop oldest, signal a keyframe request
                return true;
            }
        }
        DecodeEvent::InputAvailable(i) => free_inputs.push_back(i),
        DecodeEvent::OutputAvailable {
            index,
            pts_us,
            decoded_ns,
        } => ready.push(OutputReady {
            index,
            pts_us,
            decoded_ns,
        }),
        DecodeEvent::FormatChanged => *fmt_dirty = true,
        DecodeEvent::Error { fatal: f } => {
            if f {
                *fatal = true;
            }
        }
    }
    false
}

/// Queue as many parked AUs as there are free input buffer slots (async mode: the indices come from
/// `InputAvailable` callbacks, not a dequeue). Each AU is copied into its codec input buffer and
/// submitted; an AU larger than the buffer is DROPPED (+ a recovery keyframe requested) — a
/// truncated AU is corrupt input the decoder chews on silently, poisoning the reference chain.
fn feed_ready(
    codec: &MediaCodec,
    client: &NativeClient,
    pending_aus: &mut VecDeque<Frame>,
    free_inputs: &mut VecDeque<usize>,
    fed: &mut u64,
    oversized_dropped: &mut u64,
) {
    while !pending_aus.is_empty() && !free_inputs.is_empty() {
        let idx = free_inputs.pop_front().unwrap();
        let frame = pending_aus.pop_front().unwrap();
        let pts_us = frame.pts_ns / 1000;
        let Some(dst) = codec.input_buffer(idx) else {
            log::warn!("decode: input_buffer({idx}) returned None — dropping AU");
            continue;
        };
        let au = &frame.data;
        if au.len() > dst.len() {
            // The slot was never queued, so it stays ours — recycle it for the next AU.
            free_inputs.push_front(idx);
            *oversized_dropped += 1;
            log::warn!(
                "decode: AU {} > input buffer {} — dropped ({} so far), requesting keyframe",
                au.len(),
                dst.len(),
                *oversized_dropped
            );
            let _ = client.request_keyframe();
            continue;
        }
        let n = au.len();
        // SAFETY: `au` (wire AU) and `dst` (codec input buffer) are distinct allocations, both valid
        // for `n` bytes; `MaybeUninit<u8>` is layout-identical to `u8`, so this initializes dst[..n].
        unsafe {
            std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n);
        }
        if let Err(e) = codec.queue_input_buffer_by_index(idx, 0, n, pts_us, 0) {
            log::warn!("decode: queue_input_buffer_by_index: {e}");
        } else {
            *fed += 1;
        }
    }
}

/// Present only the NEWEST ready output (render = true) and release the rest without rendering — a
/// burst of stale frames on glass is worse than skipping to the freshest (the sync loop's newest-ready
/// policy, callback-driven). Every dequeued buffer, rendered or not, is the HUD's `decoded`
/// measurement point (it finished decoding either way); samples are recorded in pts order so the
/// receipt-map eviction stays monotonic. The presented frame's `(pts, decoded stamp)` is parked in
/// `tracker` for the OnFrameRendered callback — the `display` stage's other endpoint. `ready` is
/// drained.
#[allow(clippy::too_many_arguments)] // one call site; mirrors the sync loop's drain
fn present_ready(
    codec: &MediaCodec,
    ready: &mut Vec<OutputReady>,
    stats: &crate::stats::VideoStats,
    in_flight: &Mutex<VecDeque<(u64, i128)>>,
    clock_offset: i64,
    tracker: &DisplayTracker,
    rendered: &mut u64,
    discarded: &mut u64,
) {
    if ready.is_empty() {
        return;
    }
    if stats.enabled() {
        let mut g = in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for o in ready.iter() {
            note_decoded_pts(stats, &mut g, clock_offset, o.pts_us, o.decoded_ns);
        }
    }
    let last = ready.len() - 1;
    let mut skipped: u64 = 0;
    for (i, o) in ready.drain(..).enumerate() {
        let render = i == last;
        match codec.release_output_buffer_by_index(o.index, render) {
            Ok(()) if render => {
                *rendered += 1;
                if stats.enabled() {
                    tracker.note_rendered(o.pts_us, o.decoded_ns);
                }
            }
            Ok(()) => {
                *discarded += 1;
                skipped += 1;
            }
            Err(e) => {
                log::warn!(
                    "decode: release_output_buffer_by_index({}, {render}): {e}",
                    o.index
                )
            }
        }
    }
    stats.note_skipped(skipped); // HUD `skipped` counter (newest-wins drops); no-op while hidden
}

/// React to an output-format change by signalling the stream's HDR dataspace on the Surface (SDR
/// streams leave the default alone). The AMediaCodec analogue of the sync loop's `OutputFormatChanged`
/// handling; safe to call repeatedly (`applied_ds` dedups).
fn apply_hdr_dataspace(
    codec: &MediaCodec,
    window: &NativeWindow,
    applied_ds: &mut Option<DataSpace>,
) {
    if let Some(ds) = hdr_dataspace(codec) {
        if *applied_ds != Some(ds) {
            match window.set_buffers_data_space(ds) {
                Ok(()) => {
                    *applied_ds = Some(ds);
                    log::info!("decode: HDR stream → Surface dataspace {ds}");
                }
                Err(e) => {
                    log::warn!("decode: set_buffers_data_space({ds}) failed (non-fatal): {e}")
                }
            }
        }
    }
}

/// Raise the pipeline's OTHER hot threads — the core's data-plane pump (UDP receive + FEC
/// reassembly) and the audio decode thread — toward the display band, matching this decode thread's
/// own boost. `setpriority(PRIO_PROCESS, tid)` targets any task in the process, so we do it from
/// here once their tids are known (the same set ADPF hints), without a per-platform priority hook
/// in the shared core. Slightly below the decode thread's -10 so the display path still wins.
/// Best-effort; skips this thread (already boosted) and is non-fatal if the platform refuses.
fn boost_hot_threads(tids: &[i32]) {
    // SAFETY: `gettid` is an always-safe syscall on the calling thread.
    let self_tid = unsafe { libc::gettid() };
    for &tid in tids {
        if tid == self_tid {
            continue;
        }
        // SAFETY: `setpriority` with PRIO_PROCESS + a live tid in our own process is an always-safe
        // syscall; a refusal is reported via the return value, not UB.
        unsafe {
            if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8) != 0 {
                log::debug!("decode: setpriority(-8) on hot tid {tid} failed (non-fatal)");
            }
        }
    }
}

/// Best-effort: raise the decode thread toward Android's URGENT_DISPLAY band so background work
/// can't preempt it under load (which shows up as late/dropped frames). Non-fatal if the platform
/// refuses (foreground apps may set their own threads; the exact floor is policy-dependent).
fn boost_thread_priority() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls. PRIO_PROCESS
    // with a TID targets that one task on Linux — the same idiom `Process.setThreadPriority` uses.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) != 0 {
            log::warn!(
                "decode: setpriority(-10) failed (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Set the surface's frame-rate hint to the stream's refresh so SurfaceFlinger picks a matching
/// display mode and aligns vsync (no 60-in-120 judder). Both NDK entry points sit above our API-28
/// floor, so both are dlsym-resolved at runtime (a hard import of a >floor symbol makes
/// `dlopen`/`System.load` fail on every API-28/29 device, even where this path is never hit —
/// mirrors [`crate::adpf`]):
///   - On a **TV** (`is_tv`): `ANativeWindow_setFrameRateWithChangeStrategy` (**API 31**) with
///     `changeFrameRateStrategy = ALWAYS`, which actively drives the HDMI output into the matching
///     mode (e.g. 60↔120) instead of leaving the panel at its default and judder-matching. The
///     forced switch may blank the panel briefly — acceptable once at stream start, not wanted on a
///     phone. Falls through to the 2-arg hint on API 30.
///   - Otherwise: `ANativeWindow_setFrameRate` (**API 30**) with `compatibility = DEFAULT` — the
///     softer, seamless-preferred hint for phones/tablets and the universal fallback.
///
/// Returns `true` when the platform accepted a hint; `false` on API < 30 (symbols absent) or a
/// decline.
fn try_set_frame_rate(window: &NativeWindow, frame_rate: f32, is_tv: bool) -> bool {
    // int32_t ANativeWindow_setFrameRate(ANativeWindow*, float frameRate, int8_t compatibility)
    type SetFrameRateFn = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
    // int32_t ANativeWindow_setFrameRateWithChangeStrategy(
    //     ANativeWindow*, float frameRate, int8_t compatibility, int8_t changeFrameRateStrategy)
    type SetFrameRateStrategyFn = unsafe extern "C" fn(*mut c_void, f32, i8, i8) -> i32;
    // SAFETY: `dlopen` of the always-mapped `libandroid.so` (only bumps its refcount; never closed —
    // process-lifetime handle). Each `dlsym` returns null when the symbol is absent (device below the
    // symbol's API level), checked before transmuting the non-null pointer to its fn-pointer type.
    // `window.ptr()` is the live `ANativeWindow` this `NativeWindow` owns for the call's duration.
    unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return false;
        }
        // TV: prefer the API-31 change-strategy form to force the mode switch (strategy 1 = ALWAYS,
        // compatibility 0 = DEFAULT). Absent on API 30 ⇒ fall through to the 2-arg hint below.
        if is_tv {
            let sym = libc::dlsym(
                lib,
                c"ANativeWindow_setFrameRateWithChangeStrategy".as_ptr(),
            );
            if !sym.is_null() {
                let set = std::mem::transmute::<*mut c_void, SetFrameRateStrategyFn>(sym);
                return set(window.ptr().as_ptr().cast(), frame_rate, 0, 1) == 0;
            }
        }
        let sym = libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr());
        if sym.is_null() {
            return false; // device API < 30 — no per-surface frame-rate hint
        }
        let set_frame_rate = std::mem::transmute::<*mut c_void, SetFrameRateFn>(sym);
        set_frame_rate(window.ptr().as_ptr().cast(), frame_rate, 0) == 0
    }
}

/// Try to copy one access unit into a codec input buffer and queue it, without blocking. Returns
/// `false` only on `TryAgainLater` (no input buffer free) — the caller keeps the AU pending and
/// retries; a hard dequeue/queue error counts as consumed (retrying can't salvage the AU, and
/// parking it forever would wedge the loop on a broken codec). An AU larger than the input
/// buffer is DROPPED (+ a recovery keyframe requested), never truncated — a truncated AU is
/// corrupt input the decoder chews on silently, poisoning the reference chain.
fn feed(
    codec: &MediaCodec,
    client: &NativeClient,
    au: &[u8],
    pts_us: u64,
    oversized_dropped: &mut u64,
) -> bool {
    match codec.dequeue_input_buffer(Duration::ZERO) {
        Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
            let n = {
                let dst = buf.buffer_mut();
                if au.len() > dst.len() {
                    *oversized_dropped += 1;
                    log::warn!(
                        "decode: AU {} > input buffer {} — dropped ({} so far), requesting keyframe",
                        au.len(),
                        dst.len(),
                        *oversized_dropped
                    );
                    let _ = client.request_keyframe();
                    0 // return the slot with zero valid bytes — a no-op input, not corrupt data
                } else {
                    let n = au.len();
                    // SAFETY: `au` and `dst` are distinct allocations (wire AU vs. codec buffer),
                    // both valid for `n` bytes; `MaybeUninit<u8>` is layout-identical to `u8`, so
                    // the cast write initializes exactly `dst[..n]`.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            au.as_ptr(),
                            dst.as_mut_ptr().cast::<u8>(),
                            n,
                        );
                    }
                    n
                }
            };
            if let Err(e) = codec.queue_input_buffer(buf, 0, n, pts_us, 0) {
                log::warn!("decode: queue_input_buffer: {e}");
            }
            true
        }
        Ok(DequeuedInputBufferResult::TryAgainLater) => false, // caller keeps the AU pending
        Err(e) => {
            log::warn!("decode: dequeue_input_buffer: {e}");
            true
        }
    }
}

/// Dequeue every ready output buffer and present only the NEWEST (render = true), discarding the
/// rest (render = false) — when decode falls behind, a back-to-back burst of stale frames on glass
/// is worse than skipping straight to the freshest one (the Apple client's 1-slot newest-ready
/// ring, ported). `first_wait` is the timeout for the first dequeue only: zero normally, ~2 ms when
/// the caller's input is blocked so the loop waits on decoder progress instead of busy-spinning.
/// Returns `(rendered, discarded)`. Also reacts to `OutputFormatChanged` (which can interleave
/// between buffers — handled without losing the held buffer) to signal HDR on the Surface.
///
/// Each dequeued buffer is also the HUD's `decoded` measurement point (rendered or not — the frame
/// finished decoding either way): end-to-end = decoded + clock_offset − capture pts, and the
/// `decode` stage pairs the buffer's echoed presentationTimeUs back to the receipt stamp in
/// `in_flight` (single-clock local difference, no skew involved). The presented frame's
/// `(pts, decoded stamp)` is additionally parked in `tracker` for the OnFrameRendered callback —
/// the `display` stage's other endpoint.
#[allow(clippy::too_many_arguments)] // one call site; mirrors the async loop's present_ready
fn drain(
    codec: &MediaCodec,
    window: &NativeWindow,
    applied_ds: &mut Option<DataSpace>,
    first_wait: Duration,
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    clock_offset: i64,
    tracker: &DisplayTracker,
) -> (u64, u64) {
    // Newest ready buffer so far (presented after the loop) with its HUD metadata —
    // `Some((pts_us, decoded_ns))` only while the HUD is visible (the stamp read is gated).
    let mut held: Option<(OutputBuffer<'_>, Option<(u64, i128)>)> = None;
    let mut discarded: u64 = 0;
    let mut wait = first_wait;
    loop {
        match codec.dequeue_output_buffer(wait) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                wait = Duration::ZERO; // only the first dequeue may block
                let meta = if stats.enabled() {
                    // The dequeue IS the sync loop's decoded-availability instant.
                    let pts_us = buf.info().presentation_time_us().max(0) as u64;
                    let decoded_ns = now_realtime_ns();
                    note_decoded_pts(stats, in_flight, clock_offset, pts_us, decoded_ns);
                    Some((pts_us, decoded_ns))
                } else {
                    None
                };
                if let Some((stale, _)) = held.replace((buf, meta)) {
                    // A newer frame is ready — drop the held one without rendering.
                    if let Err(e) = codec.release_output_buffer(stale, false) {
                        log::warn!("decode: release_output_buffer(discard): {e}");
                    }
                    discarded += 1;
                    stats.note_skipped(1); // HUD `skipped` counter; no-op while hidden
                }
            }
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                // The decoder has parsed the SPS and now reports the stream's real colour signalling
                // (the AMediaCodec analogue of VideoToolbox's format description on the Apple client).
                // If it's HDR (BT.2020 PQ/HLG), tell the Surface so the compositor/display switch to
                // HDR; SDR streams leave the default dataspace alone. The decoder itself picks a
                // Main10 path from the SPS — no profile override needed. Keep looping (buffers
                // follow, and any held buffer stays held across this event).
                wait = Duration::ZERO;
                if let Some(ds) = hdr_dataspace(codec) {
                    if *applied_ds != Some(ds) {
                        match window.set_buffers_data_space(ds) {
                            Ok(()) => {
                                *applied_ds = Some(ds);
                                log::info!("decode: HDR stream → Surface dataspace {ds}");
                            }
                            Err(e) => log::warn!(
                                "decode: set_buffers_data_space({ds}) failed (non-fatal): {e}"
                            ),
                        }
                    }
                }
            }
            // TryAgainLater / OutputBuffersChanged — nothing more to dequeue now.
            Ok(_) => break,
            Err(e) => {
                log::warn!("decode: dequeue_output_buffer: {e}");
                break;
            }
        }
    }
    // Present the newest ready frame, if any, and park its metadata for the render callback.
    let mut rendered = 0;
    if let Some((buf, meta)) = held {
        match codec.release_output_buffer(buf, true) {
            Ok(()) => {
                rendered = 1;
                if let Some((pts_us, decoded_ns)) = meta {
                    tracker.note_rendered(pts_us, decoded_ns);
                }
            }
            Err(e) => log::warn!("decode: release_output_buffer: {e}"),
        }
    }
    (rendered, discarded)
}

/// HUD `decoded` point for one dequeued output frame, keyed by the echoed `presentationTimeUs`:
/// build the end-to-end (capture→decoded, skew-corrected, clamped to (0, 10 s)) and `decode`
/// (received→decoded, single-clock local, ≥ 0) samples and hand them to
/// [`crate::stats::VideoStats::note_decoded`]. The pts keys the receipt stamp in `in_flight`;
/// entries older than it are evicted (decode order == input order here — low-latency, no
/// B-frames — so anything before it was dropped inside the codec or stamped before a flush).
/// `decoded_ns` is the availability instant: the dequeue (sync loop) or the output callback's
/// stamp (async loop).
fn note_decoded_pts(
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    clock_offset: i64,
    pts_us: u64,
    decoded_ns: i128,
) {
    // Pair the echoed pts back to its receipt stamp, evicting stale (older) entries as we go.
    let mut received_ns = None;
    while let Some(&(p, r)) = in_flight.front() {
        if p > pts_us {
            break; // future frame — leave it for its own output buffer
        }
        in_flight.pop_front();
        if p == pts_us {
            received_ns = Some(r);
            break;
        }
    }
    // pts_us is the truncated frame.pts_ns/1000 we queued, so ×1000 re-approximates capture time
    // to < 1 µs — negligible against the ms-scale figures shown.
    let e2e_ns = decoded_ns + clock_offset as i128 - pts_us as i128 * 1000;
    let e2e_us = (e2e_ns > 0 && e2e_ns < 10_000_000_000).then_some((e2e_ns / 1000) as u64);
    let decode_us = received_ns.map(|r| ((decoded_ns - r).max(0) / 1000) as u64);
    stats.note_decoded(e2e_us, decode_us);
}

/// Map the decoder's reported output colour to a BT.2020 HDR dataspace, or `None` for SDR. The
/// integer values are the Android MediaFormat colour constants the NDK shares: COLOR_TRANSFER
/// ST2084 = 6 (PQ/HDR10), HLG = 7; COLOR_RANGE FULL = 1, LIMITED = 2 (the host encodes limited).
fn hdr_dataspace(codec: &MediaCodec) -> Option<DataSpace> {
    let fmt = codec.output_format();
    let full_range = fmt.i32("color-range") == Some(1);
    match fmt.i32("color-transfer") {
        Some(6) => Some(if full_range {
            DataSpace::Bt2020Pq
        } else {
            DataSpace::Bt2020ItuPq
        }),
        Some(7) => Some(if full_range {
            DataSpace::Bt2020Hlg
        } else {
            DataSpace::Bt2020ItuHlg
        }),
        _ => None, // SDR (BT.709 / SDR_VIDEO) or unspecified
    }
}

/// Serialize [`HdrMeta`](punktfunk_core::quic::HdrMeta) into Android's `KEY_HDR_STATIC_INFO`
/// (`hdr-static-info`) layout: a 25-byte CTA-861.3 / `HDRStaticInfo.Type1` blob — descriptor id 0,
/// then primaries in **R, G, B** order, white point, max/min display luminance, MaxCLL, MaxFALL, all
/// **little-endian** `u16`. Two conversions vs our wire form: HdrMeta stores primaries in ST.2086
/// **G, B, R** order (reorder to R, G, B), and `max_display_mastering_luminance` is in 0.0001-cd/m²
/// units while Android wants **whole nits** (min stays 0.0001-nit). Chromaticities (1/50000) and
/// MaxCLL/MaxFALL (nits) match 1:1.
fn android_hdr_static_info(m: &punktfunk_core::quic::HdrMeta) -> [u8; 25] {
    let [g, b_, r] = m.display_primaries; // ST.2086 G, B, R
    let max_nits = (m.max_display_mastering_luminance / 10_000).min(u16::MAX as u32) as u16;
    let min_units = m.min_display_mastering_luminance.min(u16::MAX as u32) as u16;
    let fields: [u16; 12] = [
        r[0],
        r[1],
        g[0],
        g[1],
        b_[0],
        b_[1], // R, G, B primaries
        m.white_point[0],
        m.white_point[1], // white point
        max_nits,
        min_units, // max (nits) / min (0.0001-nit) display luminance
        m.max_cll,
        m.max_fall, // MaxCLL / MaxFALL (nits)
    ];
    let mut out = [0u8; 25]; // out[0] = 0 (Type 1 descriptor id), already zero
    for (i, v) in fields.iter().enumerate() {
        out[1 + i * 2..3 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    out
}
