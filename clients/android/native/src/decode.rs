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
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::{FrameRateCompatibility, NativeWindow};
use punktfunk_core::client::NativeClient;
use punktfunk_core::error::PunktfunkError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The decode loop. Runs on the `pf-decode` thread until `shutdown` is set or the session closes.
pub fn run(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
) {
    boost_thread_priority();
    let mode = client.mode();
    let codec = match MediaCodec::from_decoder_type("video/hevc") {
        Some(c) => c,
        None => {
            log::error!("decode: no HEVC decoder on this device");
            return;
        }
    };

    let mut format = MediaFormat::new();
    format.set_str("mime", "video/hevc");
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    // Generous input buffer so a large keyframe AU is never truncated.
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    // Ask for the low-latency decode path where the decoder supports it (no reordering buffer).
    format.set_i32("low-latency", 1);
    // Advisory low-latency hints (KEY_PRIORITY / KEY_OPERATING_RATE), ignored where unsupported:
    // realtime priority + the target frame rate, so vendor decoders (e.g. Qualcomm) run at full
    // clocks instead of a power-saving cadence that adds dequeue latency.
    format.set_i32("priority", 0); // 0 = realtime
    format.set_i32("operating-rate", mode.refresh_hz as i32);

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

    let mut fed: u64 = 0;
    let mut rendered: u64 = 0;
    // Loss recovery: watch the host→client unrecoverable-drop count and ask for an IDR when it
    // climbs.
    let mut last_dropped = client.frames_dropped();
    let mut last_kf_req: Option<Instant> = None;
    // Capture→client-receipt latency uses the negotiated host-minus-client clock offset (0 if the
    // host didn't answer the skew handshake — then the HUD flags it "same-host").
    let clock_offset = client.clock_offset_ns;
    // The dataspace we've signalled on the Surface so far (None = default/SDR). Set reactively once
    // the decoder reports an HDR stream (see `drain`); avoids re-applying every format event.
    let mut applied_ds: Option<DataSpace> = None;
    while !shutdown.load(Ordering::Relaxed) {
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
                fed += 1;
                // HUD stat: capture→client-receipt latency = client_now + (host−client) − capture_pts.
                let lat_ns = now_realtime_ns() + clock_offset as i128 - frame.pts_ns as i128;
                let lat_us =
                    (lat_ns > 0 && lat_ns < 10_000_000_000).then_some((lat_ns / 1000) as u64);
                stats.note(frame.data.len(), lat_us, clock_offset != 0);
                feed(&codec, &frame.data, frame.pts_ns / 1000);
            }
            Err(PunktfunkError::NoFrame) => {} // timeout — still drain output below
            Err(_) => break,                   // session closed
        }
        rendered += drain(&codec, &window, &mut applied_ds);

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

        if fed > 0 && fed % 300 == 0 {
            log::info!("decode: fed={fed} rendered={rendered}");
        }
    }

    let _ = codec.stop();
    log::info!("decode: stopped (fed={fed} rendered={rendered})");
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

/// Copy one access unit into a codec input buffer and queue it.
fn feed(codec: &MediaCodec, au: &[u8], pts_us: u64) {
    match codec.dequeue_input_buffer(Duration::from_millis(10)) {
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
                for (slot, &b) in dst.iter_mut().zip(&au[..n]) {
                    slot.write(b);
                }
                n
            };
            if let Err(e) = codec.queue_input_buffer(buf, 0, n, pts_us, 0) {
                log::warn!("decode: queue_input_buffer: {e}");
            }
        }
        Ok(DequeuedInputBufferResult::TryAgainLater) => {
            // No input buffer free right now; the AU is dropped (FEC/keyframes recover).
        }
        Err(e) => log::warn!("decode: dequeue_input_buffer: {e}"),
    }
}

/// Release any ready output buffers to the surface (render = true), latency-first. Returns the
/// number of frames presented. Also reacts to `OutputFormatChanged` to signal HDR on the Surface.
fn drain(codec: &MediaCodec, window: &NativeWindow, applied_ds: &mut Option<DataSpace>) -> u64 {
    let mut n = 0;
    loop {
        match codec.dequeue_output_buffer(Duration::from_millis(0)) {
            Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                if let Err(e) = codec.release_output_buffer(buf, true) {
                    log::warn!("decode: release_output_buffer: {e}");
                    break;
                }
                n += 1;
            }
            Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                // The decoder has parsed the SPS and now reports the stream's real colour signalling
                // (the AMediaCodec analogue of VideoToolbox's format description on the Apple client).
                // If it's HDR (BT.2020 PQ/HLG), tell the Surface so the compositor/display switch to
                // HDR; SDR streams leave the default dataspace alone. The decoder itself picks a
                // Main10 path from the SPS — no profile override needed. Keep looping (buffers follow).
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
            // TryAgainLater / OutputBuffersChanged — nothing to render now.
            Ok(_) => break,
            Err(e) => {
                log::warn!("decode: dequeue_output_buffer: {e}");
                break;
            }
        }
    }
    n
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
