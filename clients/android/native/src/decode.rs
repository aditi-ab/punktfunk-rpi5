//! Android video decode (android-only): pull HEVC access units from the connector and render them
//! to the SurfaceView via NDK `AMediaCodec` — hardware decode, zero per-frame JNI.
//!
//! One-in/one-out: the host opens every stream with an IDR carrying VPS/SPS/PPS **in-band**, so the
//! decoder needs no out-of-band codec-specific data — we configure with mime + the negotiated
//! WxH (from [`NativeClient::mode`]) and feed each access unit as it arrives. The decode thread owns
//! the codec + window for its whole life; [`crate::session`] signals it to stop via the shared flag.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    OutputBuffer,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::{FrameRateCompatibility, NativeWindow};
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cap on the pts→received-timestamp map below: MediaCodec holds only a handful of frames in
/// flight, so anything beyond this is stale (codec flushed / HUD toggled) and gets evicted.
const IN_FLIGHT_CAP: usize = 64;

/// The decode loop. Runs on the `pf-decode` thread until `shutdown` is set or the session closes.
pub fn run(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
) {
    boost_thread_priority();
    let mode = client.mode();
    // The MediaCodec MIME for the codec the host resolved (`Welcome.codec`): HEVC or H.264. AMediaCodec
    // needs no out-of-band extradata — the in-band VPS/SPS/PPS on every IDR configure it either way.
    let mime = match client.codec {
        punktfunk_core::quic::CODEC_H264 => "video/avc",
        _ => "video/hevc",
    };
    let codec = match MediaCodec::from_decoder_type(mime) {
        Some(c) => c,
        None => {
            log::error!("decode: no {mime} decoder on this device");
            return;
        }
    };
    log::info!("decode: codec mime = {mime}");

    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    // Generous input buffer so a large keyframe AU is never truncated.
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    // Ask for the low-latency decode path where the decoder supports it (no reordering buffer).
    format.set_i32("low-latency", 1);
    // Best-effort vendor twin of the standard key: older Qualcomm decoders only honor their own
    // extension. Unknown keys are ignored by other vendors' codecs, so this is safe to set blind.
    format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    // Advisory low-latency hints (KEY_PRIORITY / KEY_OPERATING_RATE), ignored where unsupported:
    // realtime priority + the target frame rate, so vendor decoders (e.g. Qualcomm) run at full
    // clocks instead of a power-saving cadence that adds dequeue latency.
    format.set_i32("priority", 0); // 0 = realtime
                                   // Operating rate = the codec's clock hint. Setting it to the display rate merely asks the
                                   // decoder to *sustain* that cadence — a Qualcomm decoder can meet 60/120 fps at a power-saving
                                   // clock that adds a millisecond-plus of decode latency per frame. Setting it to the AOSP
                                   // "unbounded" sentinel (Short.MAX) instead asks the decoder to run each frame at max clocks and
                                   // finish ASAP, minimising per-frame decode latency — the right trade for a real-time stream
                                   // (costs power/heat; the dial to lower if a device thermally throttles over a long session).
                                   // Ignored where unsupported.
    format.set_i32("operating-rate", i16::MAX as i32); // 32767 = "as fast as possible"

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
    // vsync (no 60-in-120 judder on high-refresh panels). minSdk 31 ≥ API 30, so the underlying
    // ANativeWindow_setFrameRate is always present; non-fatal if the platform declines.
    if let Err(e) = window.set_frame_rate(mode.refresh_hz as f32, FrameRateCompatibility::Default) {
        log::warn!(
            "decode: set_frame_rate({} Hz) failed (non-fatal): {e}",
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
    let clock_offset = client.clock_offset_ns;
    // HUD stage split: receipt timestamps keyed by the pts we queue into the codec, so the decoded
    // point (output-buffer dequeue — MediaCodec round-trips presentationTimeUs) can be paired back
    // to its receipt for the `decode` stage. Only fed while the HUD is visible.
    let mut in_flight: VecDeque<(u64, i128)> = VecDeque::new();
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
                        let lat_ns = received_ns + clock_offset as i128 - frame.pts_ns as i128;
                        let lat_us = (lat_ns > 0 && lat_ns < 10_000_000_000)
                            .then_some((lat_ns / 1000) as u64);
                        stats.note_received(frame.data.len(), lat_us, clock_offset != 0);
                        in_flight.push_back((frame.pts_ns / 1000, received_ns));
                        if in_flight.len() > IN_FLIGHT_CAP {
                            in_flight.pop_front(); // stale — codec never echoed it back
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
            if feed(&codec, &frame.data, frame.pts_ns / 1000) {
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
            clock_offset,
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
                hint = crate::adpf::HintSession::create(frame_period_ns, &tids);
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

/// Try to copy one access unit into a codec input buffer and queue it, without blocking. Returns
/// `false` only on `TryAgainLater` (no input buffer free) — the caller keeps the AU pending and
/// retries; a hard dequeue/queue error counts as consumed (retrying can't salvage the AU, and
/// parking it forever would wedge the loop on a broken codec).
fn feed(codec: &MediaCodec, au: &[u8], pts_us: u64) -> bool {
    match codec.dequeue_input_buffer(Duration::ZERO) {
        Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
            let n = {
                let dst = buf.buffer_mut();
                let n = au.len().min(dst.len());
                if n < au.len() {
                    log::warn!(
                        "decode: AU {} > input buffer {}, truncated",
                        au.len(),
                        dst.len()
                    );
                }
                // SAFETY: `au` and `dst` are distinct allocations (wire AU vs. codec buffer), both
                // valid for `n` bytes; `MaybeUninit<u8>` is layout-identical to `u8`, so the cast
                // write initializes exactly `dst[..n]`.
                unsafe {
                    std::ptr::copy_nonoverlapping(au.as_ptr(), dst.as_mut_ptr().cast::<u8>(), n);
                }
                n
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
/// `in_flight` (single-clock local difference, no skew involved).
fn drain(
    codec: &MediaCodec,
    window: &NativeWindow,
    applied_ds: &mut Option<DataSpace>,
    first_wait: Duration,
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    clock_offset: i64,
) -> (u64, u64) {
    let mut held = None; // newest ready buffer so far, presented after the loop
    let mut discarded: u64 = 0;
    let mut wait = first_wait;
    loop {
        match codec.dequeue_output_buffer(wait) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                wait = Duration::ZERO; // only the first dequeue may block
                if stats.enabled() {
                    note_decoded(stats, in_flight, clock_offset, &buf);
                }
                if let Some(stale) = held.replace(buf) {
                    // A newer frame is ready — drop the held one without rendering.
                    if let Err(e) = codec.release_output_buffer(stale, false) {
                        log::warn!("decode: release_output_buffer(discard): {e}");
                    }
                    discarded += 1;
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
    // Present the newest ready frame, if any.
    let mut rendered = 0;
    if let Some(buf) = held {
        match codec.release_output_buffer(buf, true) {
            Ok(()) => rendered = 1,
            Err(e) => log::warn!("decode: release_output_buffer: {e}"),
        }
    }
    (rendered, discarded)
}

/// HUD `decoded` point for one dequeued output buffer: build the end-to-end (capture→decoded,
/// skew-corrected, clamped to (0, 10 s)) and `decode` (received→decoded, single-clock local, ≥ 0)
/// samples and hand them to [`crate::stats::VideoStats::note_decoded`]. The codec echoes the input
/// `presentationTimeUs` on the output buffer, which keys the receipt stamp in `in_flight`; entries
/// older than the echoed pts are evicted (decode order == input order here — low-latency, no
/// B-frames — so anything before it was dropped inside the codec or stamped before a flush).
fn note_decoded(
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    clock_offset: i64,
    buf: &OutputBuffer<'_>,
) {
    let pts_us = buf.info().presentation_time_us().max(0) as u64;
    let decoded_ns = now_realtime_ns();
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
