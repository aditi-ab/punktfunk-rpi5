//! The event-driven async MediaCodec decode loop (default) + its feeder/dispatch/present helpers.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::{AsyncNotifyCallback, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::reanchor::{GateVerdict, ReanchorGate};
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::display::{
    apply_hdr_dataspace, install_render_callback, release_render_callback, DisplayTracker,
};
use super::latency::{note_decoded_pts, now_realtime_ns, take_flags};
use super::presenter::{presenter_disabled_by_sysprop, PresentMeter, PresentPriority, Presenter};
use super::setup::{
    android_hdr_static_info, boost_hot_threads, boost_thread_priority, codec_mime,
    configure_low_latency, create_codec, try_set_frame_rate,
};
use super::vsync::{now_monotonic_ns, VsyncClock};
use super::{
    DecodeOptions, FRAME_PARK_CAP, IN_FLIGHT_CAP, NO_OUTPUT_PATIENCE, NO_VIDEO_PATIENCE,
    NO_VIDEO_RETRY, PENDING_SPLIT_CAP,
};

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
    /// A received access unit from the feeder, ready to queue into the decoder. The `bool` is the
    /// feeder's [`NativeClient::note_frame_index`] verdict — `true` when this AU revealed a forward
    /// frame-index gap, so the loop arms the freeze gate (the feeder already fired the RFI request).
    Au(Frame, bool),
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
    /// A panel vsync (from the [`VsyncClock`] thread) — the presenter's retry/pacing tick.
    Vsync,
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
pub(super) fn run_async(
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
        present_priority,
        smooth_buffer,
        panel_hz,
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
                if fatal {
                    log::error!("decode: fatal codec error — stream will stop: {e:?}");
                } else {
                    log::warn!("decode: codec error {e:?} (recoverable)");
                }
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
    // Whether the adaptive-bitrate controller wants the `decode` stage as its decoder-backlog
    // signal (Automatic, non-PyroWave): then `in_flight` is fed regardless of the HUD.
    let measure_decode = client.wants_decode_latency();
    let in_flight = Arc::new(Mutex::new(VecDeque::<(u64, i128)>::new()));
    // Display stage (spec `display` + the capture→displayed headline): the rendered frame is
    // parked in the tracker at release; the OnFrameRendered callback pairs it with
    // SurfaceFlinger's render timestamp. `render_cb` is the callback's leaked Arc refcount,
    // reclaimed after the codec is dropped below.
    let meter = Arc::new(PresentMeter::new());
    let tracker = DisplayTracker::new(stats.clone(), clock_offset.clone(), meter.clone());
    let render_cb = install_render_callback(&codec, &tracker);

    // The timeline presenter (see `presenter.rs`): newest-wins / smoothing store, one-in-flight
    // glass budget, timeline-timed release. `debug.punktfunk.presenter = arrival` selects the
    // legacy release-immediately path for a rebuild-free on-device A/B.
    let mut presenter = if presenter_disabled_by_sysprop() {
        log::info!("decode: presenter = arrival (sysprop) — legacy immediate release");
        None
    } else {
        let priority = PresentPriority::resolve(present_priority, smooth_buffer);
        log::info!(
            "decode: presenter = timeline ({})",
            match priority {
                PresentPriority::Latency => "lowest latency".to_string(),
                PresentPriority::Smooth { buffer } => format!("smoothness, buffer {buffer}"),
            }
        );
        Some(Presenter::new(priority))
    };
    stats.set_presenter_active(presenter.is_some());
    // The vsync clock, started LAZILY on the first decoded frame (see `vsync.rs`); its ticks ride
    // the same event channel. The Sender parks here until that moment.
    let mut vsync: Option<VsyncClock> = None;
    let mut vsync_tx = presenter.is_some().then(|| ev_tx.clone());

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
                feeder_loop(
                    client,
                    stats,
                    measure_decode,
                    in_flight,
                    clock_offset,
                    shutdown,
                    ev_tx,
                );
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
    // Freeze-until-reanchor gate (see the sync loop for the rationale). Armed on a frame-index gap
    // (the feeder's Au verdict), a parked-AU overflow drop, a dropped-count climb, or a recoverable
    // codec error; `recovery_flags` carries each AU's user_flags from `dispatch_event` (feed) to
    // `present_ready` (present), keyed by the codec-echoed pts.
    let mut gate = ReanchorGate::new(client.frames_dropped());
    let mut recovery_flags: VecDeque<(u64, u32)> = VecDeque::new();
    let mut last_kf_req: Option<Instant> = None;
    // Productive (dispatch+feed+present) time between displayed frames; reported to ADPF once one is
    // presented. The blocking event wait is excluded (idle, not work) — same accounting as the sync loop.
    let mut work_accum_ns: i64 = 0;
    let mut fatal = false;
    // No-output backstop (see [`NO_OUTPUT_PATIENCE`]): the last time the decoder handed us a frame,
    // and how many AUs it had been fed by then. Silence only counts while AUs are actually going in,
    // so an idle stream never asks for anything. Seeded at start so a decoder that never produces a
    // first frame — the missed opening IDR — is caught by the same window.
    let mut last_output = Instant::now();
    let mut fed_at_output: u64 = 0;
    // Nothing-ever-arrived backstop (see [`NO_VIDEO_PATIENCE`]) — the mirror of the one above, for a
    // session whose video plane delivers no AU at all.
    let started = Instant::now();
    let mut last_no_video_req: Option<Instant> = None;

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
        let mut vsync_tick = false;
        let mut aus_dropped: u64 = 0;
        if let Some(ev) = ev0 {
            aus_dropped += u64::from(dispatch_event(
                ev,
                &mut pending_aus,
                &mut free_inputs,
                &mut ready,
                &mut fmt_dirty,
                &mut vsync_tick,
                &mut fatal,
                &mut gate,
                &mut recovery_flags,
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
                &mut vsync_tick,
                &mut fatal,
                &mut gate,
                &mut recovery_flags,
            ));
        }
        if vsync_tick {
            if let Some(p) = presenter.as_mut() {
                p.on_vsync();
            }
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
        let rendered_before = rendered;
        present_ready(
            &codec,
            &client,
            measure_decode,
            &mut ready,
            &stats,
            &in_flight,
            clock_offset.load(Ordering::Relaxed),
            &tracker,
            &mut presenter,
            &mut rendered,
            &mut discarded,
            &mut gate,
            &mut recovery_flags,
        );
        // The presenter's decision point runs EVERY pass — frame arrivals, vsync ticks and the
        // 5 ms housekeeping wake all land here, which is what reopens the glass budget on time
        // even when the choreographer clock is absent.
        if let Some(p) = presenter.as_mut() {
            let clock = vsync.as_ref().map(|v| v.shared().as_ref());
            if p.pump(&codec, clock, &tracker, &stats, now_monotonic_ns()) {
                rendered += 1;
            }
            // The 1 Hz window flush doubles as the phase-lock report tick: the CIRCULAR
            // (vector-mean) latch phase + coherence are the host capture controller's v2 error
            // signal (design/phase-locked-capture.md §6; a median is immovable under jitter).
            // Timestamps convert monotonic→realtime→host — the skew offset lives client-side.
            if let (Some((circ_latch_ns, coherence)), Some(c)) = (p.flush_log(&meter, clock), clock)
            {
                let period = c.panel_period_ns().max(c.period_ns());
                if period > 0 {
                    if let Some(t) = c.next_target(now_monotonic_ns(), 0) {
                        let mono_now = now_monotonic_ns();
                        let real_now = now_realtime_ns();
                        let latch_real_ns = real_now + (t.expected_present_ns - mono_now) as i128;
                        let latch_host_ns = (latch_real_ns
                            + clock_offset.load(Ordering::Relaxed) as i128)
                            .max(0) as u64;
                        client.report_phase(
                            latch_host_ns,
                            period.clamp(0, u32::MAX as i64) as u32,
                            1_000_000, // skew residual + latch jitter — conservative 1 ms
                            circ_latch_ns.min(u32::MAX as u64) as u32,
                            coherence,
                        );
                    }
                }
            }
        }
        let presented_now = rendered > rendered_before;
        // Start the vsync clock LAZILY on the first decoded output (eager, it ticks the panel
        // rate into a session that has no frame yet — the Apple deadline presenter's bootstrap
        // lesson). A `None` from start (no choreographer surface) simply leaves ASAP targets.
        if had_output && vsync.is_none() {
            if let Some(tx) = vsync_tx.take() {
                vsync = VsyncClock::start(
                    panel_hz,
                    Box::new(move || {
                        let _ = tx.send(DecodeEvent::Vsync);
                    }),
                );
                if vsync.is_none() {
                    log::info!("decode: no choreographer clock — presenter uses ASAP targets");
                }
            }
        }

        work_accum_ns += work_t0.elapsed().as_nanos() as i64;
        if presented_now {
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
            // The one line that separates "the stream never reached glass" from "it reached glass
            // and looked wrong" — the periodic tally below only starts at 300 frames, which is no
            // help at all on a session that renders none.
            if rendered == 1 {
                log::info!("decode: first frame presented (fed={fed} discarded={discarded})");
            }
            if rendered > 0 && rendered % 300 == 0 {
                log::info!("decode: fed={fed} rendered={rendered} discarded={discarded}");
            }
        }
        // Loss recovery + overdue backstop, folded through the gate. A parked-AU overflow drop is itself
        // a loss, so it arms the freeze directly; the gate's `poll` then arms on a dropped-count climb
        // and re-asks on an overdue freeze. All keyframe intents route through the shared 100 ms
        // throttle so a multi-frame recovery gap can't flood the control stream.
        let now = Instant::now();
        if aus_dropped > 0 {
            gate.arm(now);
        }
        // Fed but silent: the decoder is holding nothing it can decode — the opening IDR never
        // reached it, or its reference chain is gone. Ask for a fresh one and arm the freeze, so the
        // concealment it may start emitting on the way back is withheld until a clean re-anchor
        // (`gate.poll` keeps re-asking on the deadline until one arrives).
        let starved = !had_output
            && fed > fed_at_output
            && now.duration_since(last_output) >= NO_OUTPUT_PATIENCE;
        if had_output {
            last_output = now;
            fed_at_output = fed;
        } else if starved {
            log::warn!(
                "decode: no output for {} ms with {} AU(s) fed — requesting a re-anchor keyframe",
                now.duration_since(last_output).as_millis(),
                fed - fed_at_output
            );
            gate.arm(now);
            last_output = now; // one request per patience window, not per iteration
            fed_at_output = fed;
        }
        // Nothing has EVER arrived: not an idle stream but a session that never got a picture — the
        // `starved` test above cannot see it, because it needs `fed` to have moved. Evaluated after
        // `feed_ready`, so an AU that arrived this pass has either been fed or is parked in
        // `pending_aus`; both mean video IS flowing.
        let no_video_yet = fed == 0 && pending_aus.is_empty();
        if no_video_yet
            && now.duration_since(started) >= NO_VIDEO_PATIENCE
            && last_no_video_req.is_none_or(|t| now.duration_since(t) >= NO_VIDEO_RETRY)
        {
            log::warn!(
                "decode: no video received {} ms into the session — requesting a keyframe",
                now.duration_since(started).as_millis()
            );
            last_no_video_req = Some(now);
            let _ = client.request_keyframe();
            last_kf_req = Some(now); // share the throttle with the loss-recovery path below
        }
        if (gate.poll(client.frames_dropped(), now) || aus_dropped > 0 || starved)
            && last_kf_req.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
        {
            last_kf_req = Some(now);
            let _ = client.request_keyframe();
        }
    }

    if let Some(p) = presenter.as_mut() {
        p.release_all(&codec); // hand every held output buffer back before the codec stops
    }
    drop(vsync); // stop + join the choreographer thread; its channel sends are harmless after
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
    measure_decode: bool,
    in_flight: Arc<Mutex<VecDeque<(u64, i128)>>>,
    clock_offset: Arc<AtomicI64>,
    shutdown: Arc<AtomicBool>,
    ev_tx: mpsc::Sender<DecodeEvent>,
) {
    // Received AUs awaiting their 0xCF host timing (Phase-2 split), as (pts_ns, capture→received µs).
    let mut pending_split: VecDeque<(u64, u64)> = VecDeque::new();
    // Last logged phase-lock ACK (the host's applied capture hold, from the 0xCF tail) — logged
    // on change so `adb logcat -s pf.phase` shows the closed loop working (or not) at a glance.
    let mut last_phase_ack: Option<i32> = None;
    while !shutdown.load(Ordering::Relaxed) {
        match client.next_frame(Duration::from_millis(5)) {
            Ok(frame) => {
                // Loss recovery (RFI): a forward frame-index gap fires a throttled reference-frame-
                // invalidation request so an RFI-capable host recovers with a cheap clean P-frame
                // instead of a full IDR (the frames_dropped keyframe path is the backstop). The gap
                // verdict rides the Au event so the decode loop arms its freeze gate on the same signal.
                let gap = client.note_frame_index(frame.frame_index);
                // Park the receipt stamp (keyed by the pts the codec echoes) whenever the `decode`
                // stage is consumed: the HUD, or the ABR decode signal (`measure_decode`). The
                // HUD-only `received` point + host/network split stay gated on the overlay.
                if stats.enabled() || measure_decode {
                    // Core reassembly-completion stamp (ABI v9), NOT the pull instant: stamping
                    // here would fold the hand-off queue wait into the network latency figure
                    // (a client-side standing backlog masquerading as network). 0 = older core.
                    let received_ns = if frame.received_ns > 0 {
                        frame.received_ns as i128
                    } else {
                        now_realtime_ns()
                    };
                    {
                        let mut g = in_flight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        g.push_back((frame.pts_ns / 1000, received_ns));
                        if g.len() > IN_FLIGHT_CAP {
                            g.pop_front(); // stale — codec never echoed it back
                        }
                    }
                    if stats.enabled() {
                        let clock_offset = clock_offset.load(Ordering::Relaxed) as i128;
                        let lat_ns = received_ns + clock_offset - frame.pts_ns as i128;
                        let lat_us = (lat_ns > 0 && lat_ns < 10_000_000_000)
                            .then_some((lat_ns / 1000) as u64);
                        stats.note_received(frame.data.len(), lat_us, clock_offset != 0);
                        if let Some(hostnet_us) = lat_us {
                            pending_split.push_back((frame.pts_ns, hostnet_us));
                            if pending_split.len() > PENDING_SPLIT_CAP {
                                pending_split.pop_front();
                            }
                        }
                        while let Ok(t) = client.next_host_timing(Duration::ZERO) {
                            // Phase-lock closed-loop readout: the host's applied hold rides the
                            // 0xCF tail; log transitions (~1 Hz worst case — the host updates it
                            // once a second). None = a host without the tail (pre-phase-lock).
                            if t.applied_phase_ns != last_phase_ack {
                                log::info!(
                                    target: "pf.phase",
                                    "host applied_phase={:?}us",
                                    t.applied_phase_ns.map(|n| n / 1000)
                                );
                                last_phase_ack = t.applied_phase_ns;
                            }
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
                }
                if ev_tx.send(DecodeEvent::Au(frame, gap)).is_err() {
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
#[allow(clippy::too_many_arguments)] // two call sites; the freeze gate + flag map are threaded in
fn dispatch_event(
    ev: DecodeEvent,
    pending_aus: &mut VecDeque<Frame>,
    free_inputs: &mut VecDeque<usize>,
    ready: &mut Vec<OutputReady>,
    fmt_dirty: &mut bool,
    vsync_tick: &mut bool,
    fatal: &mut bool,
    gate: &mut ReanchorGate,
    recovery_flags: &mut VecDeque<(u64, u32)>,
) -> bool {
    match ev {
        DecodeEvent::Au(f, gap) => {
            // A forward frame-index gap arms the freeze; park this AU's flags for the present side to
            // fold `on_decoded` (keyed by the pts the codec will echo).
            if gap {
                gate.arm(Instant::now());
            }
            recovery_flags.push_back((f.pts_ns / 1000, f.flags));
            if recovery_flags.len() > IN_FLIGHT_CAP {
                recovery_flags.pop_front();
            }
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
        DecodeEvent::Vsync => *vsync_tick = true,
        DecodeEvent::Error { fatal: f } => {
            if f {
                *fatal = true;
            } else {
                // A recoverable/transient codec error is a decode hiccup on a broken reference chain —
                // arm the freeze so the concealed output it recovers into is held off the screen.
                gate.arm(Instant::now());
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

/// Route the ready outputs toward glass. With the timeline presenter (default): fold each output
/// through the re-anchor gate in pts order, hand the approved ones to the presenter's store
/// (newest-wins / smoothing FIFO — the actual release happens in `Presenter::pump`, budgeted and
/// timeline-timed), and release withheld concealment unrendered. Legacy (`arrival` sysprop):
/// present only the NEWEST ready output immediately and release the rest unrendered — the
/// original policy. Every dequeued buffer, rendered or not, is the HUD's `decoded` measurement
/// point (it finished decoding either way); samples are recorded in pts order so the receipt-map
/// eviction stays monotonic. `ready` is drained.
#[allow(clippy::too_many_arguments)] // one call site; mirrors the sync loop's drain
fn present_ready(
    codec: &MediaCodec,
    client: &NativeClient,
    measure_decode: bool,
    ready: &mut Vec<OutputReady>,
    stats: &crate::stats::VideoStats,
    in_flight: &Mutex<VecDeque<(u64, i128)>>,
    clock_offset: i64,
    tracker: &DisplayTracker,
    presenter: &mut Option<Presenter>,
    rendered: &mut u64,
    discarded: &mut u64,
    gate: &mut ReanchorGate,
    recovery_flags: &mut VecDeque<(u64, u32)>,
) {
    if ready.is_empty() {
        return;
    }
    // Pair each output's decode stage (feeds the ABR decode signal always; the HUD histogram only
    // while visible) — both consume the receipt map, so enter for either.
    if stats.enabled() || measure_decode {
        let mut g = in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for o in ready.iter() {
            note_decoded_pts(
                client,
                measure_decode,
                stats,
                &mut g,
                clock_offset,
                o.pts_us,
                o.decoded_ns,
            );
        }
    }
    // Fold EVERY output through the gate in pts (== decode) order — even the ones newest-wins discards —
    // so the two-mark re-anchor count stays correct; a `false` verdict is withheld concealment (the
    // SurfaceView keeps the last rendered frame frozen on).
    let now = Instant::now();
    let mut skipped: u64 = 0;
    if let Some(p) = presenter.as_mut() {
        for o in ready.drain(..) {
            let flags = take_flags(recovery_flags, o.pts_us);
            if gate.on_decoded(flags, false, now) == GateVerdict::Present {
                let dropped = p.submit(codec, o.index, o.pts_us, o.decoded_ns);
                skipped += dropped;
                *discarded += dropped;
            } else {
                if let Err(e) = codec.release_output_buffer_by_index(o.index, false) {
                    log::warn!("decode: release_output_buffer_by_index({}): {e}", o.index);
                }
                *discarded += 1;
                skipped += 1;
            }
        }
    } else {
        let last = ready.len() - 1;
        for (i, o) in ready.drain(..).enumerate() {
            let flags = take_flags(recovery_flags, o.pts_us);
            let present = gate.on_decoded(flags, false, now) == GateVerdict::Present;
            let render = i == last && present;
            match codec.release_output_buffer_by_index(o.index, render) {
                Ok(()) if render => {
                    *rendered += 1;
                    tracker.note_rendered(o.pts_us, o.decoded_ns, now_realtime_ns());
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
    }
    stats.note_skipped(skipped); // HUD `skipped` counter (newest-wins + held-off drops); no-op hidden
}
