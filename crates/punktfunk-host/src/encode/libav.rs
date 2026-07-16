//! Shared libavcodec (FFmpeg) glue for the three libav encode backends — Linux NVENC
//! (`encode/linux/mod.rs`), VAAPI (`encode/linux/vaapi.rs`), and Windows AMF/QSV
//! (`encode/windows/ffmpeg_win.rs`) — so the byte-identical pieces live once (plan §2.2, the Tier-2
//! gap). Free functions and consts over borrowed handles; nothing here is per-frame `dyn`,
//! allocating, or on the zero-copy ingest path.
use crate::encode::EncodedFrame;
use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi; // = ffmpeg_sys_next
use ffmpeg_next::format::Pixel;
use ffmpeg_next::{encoder, Packet, Rational};
use std::os::raw::c_int;

/// swscale: nearest-neighbour scaler flag (`SWS_POINT`). We never rescale (src dims == dst dims), so
/// the resampler choice only governs the colour-conversion path; POINT is the cheapest.
pub(crate) const SWS_POINT: c_int = 0x10;
/// swscale colorspace id for ITU-R BT.709 (`SWS_CS_ITU709`) — the CSC coefficients for our RGB→YUV.
pub(crate) const SWS_CS_ITU709: c_int = 1;
/// swscale colorspace id for ITU-R BT.2020 non-constant-luminance (`SWS_CS_BT2020`) — the CSC
/// coefficients for the HDR X2BGR10→P010 path (Windows only today).
pub(crate) const SWS_CS_BT2020: c_int = 9;

/// `Pixel` → `AVPixelFormat`. `Pixel` is `#[repr(i32)]`-compatible with `AVPixelFormat` (the bindgen
/// enum) via this documented conversion in ffmpeg-next.
pub(crate) fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    ffi::AVPixelFormat::from(p)
}

/// One `receive_packet` attempt, with the not-ready states kept distinct so a blocking drain can
/// tell "still encoding" (retry) from "stream over" (stop). The Linux NVENC/VAAPI polls collapse
/// `Again`/`Eof` to `None`; the Windows AMF/QSV path keeps them apart for its deadline-driven loop.
pub(crate) enum PollOutcome {
    Packet(EncodedFrame),
    Again,
    Eof,
}

/// Apply the shared low-latency rate-control contract to a **not-yet-opened** encoder context: a
/// fixed frame rate, CBR (target == max bitrate), B-frames off, and a tight ~1-frame VBV/HRD buffer.
///
/// The VBV size bounds any single frame. Under CBR with no buffer set, libav's encoders use a loose
/// default VBV, so a high-motion P-frame can balloon to many times the average; those extra packets
/// overflow the bounded send queue + kernel socket buffer and get dropped, which the client sees as
/// framedrops/jitter (and, on the infinite-GOP path, as old/stale frames flashing until the next
/// RFI). A tight ~1-frame buffer makes the encoder hold frame size roughly constant and absorb motion
/// as a momentary QP (quality) dip instead — the trade we want. Default = 1 frame of bits
/// (bitrate/fps); `PUNKTFUNK_VBV_FRAMES` tunes it (larger = better motion quality, bigger bursts).
///
/// The caller still owns `set_format` (pixel format) and `gop_size` (GOP policy differs: NVENC's
/// infinite/intra-refresh wave vs the VAAPI/AMF `i32::MAX`), since those are backend-specific.
pub(crate) fn apply_low_latency_rc(video: &mut encoder::video::Video, fps: u32, bitrate_bps: u64) {
    video.set_time_base(Rational(1, fps as i32));
    video.set_frame_rate(Some(Rational(fps as i32, 1)));
    video.set_bit_rate(bitrate_bps as usize);
    video.set_max_bit_rate(bitrate_bps as usize);
    video.set_max_b_frames(0);
    let vbv_bits = ((bitrate_bps as f64 / fps.max(1) as f64) * crate::encode::vbv_frames_env())
        .clamp(1.0, i32::MAX as f64);
    // SAFETY: `video` wraps a freshly-allocated `AVCodecContext` we hold by value and have not opened
    // yet; `as_mut_ptr()` returns that non-null, aligned, exclusively-owned context. Writing the plain
    // `rc_buffer_size` int before `open_with` is the supported way to set a field ffmpeg-next exposes
    // no setter for. Sole owner → no aliasing; synchronous in-bounds scalar write.
    unsafe {
        (*video.as_mut_ptr()).rc_buffer_size = vbv_bits as i32;
    }
}

/// Drain the encoder for one packet (shared across the NVENC/VAAPI/AMF/QSV libav backends). The
/// `EncodedFrame`'s only allocation is the `to_vec()` of the bitstream — the same copy each backend
/// already made — so this stays off any per-frame `dyn`/`Box`/channel path.
pub(crate) fn poll_encoder(enc: &mut encoder::video::Encoder, fps: u32) -> Result<PollOutcome> {
    let mut pkt = Packet::empty();
    match enc.receive_packet(&mut pkt) {
        Ok(()) => {
            let data = pkt.data().map(|d| d.to_vec()).unwrap_or_default();
            let pts = pkt.pts().unwrap_or(0).max(0) as u64;
            Ok(PollOutcome::Packet(EncodedFrame {
                data,
                pts_ns: pts * 1_000_000_000 / fps as u64,
                keyframe: pkt.is_key(),
                recovery_anchor: false,
                chunk_aligned: false,
            }))
        }
        // No packet ready yet (need another input frame).
        Err(ffmpeg::Error::Other { errno })
            if errno == ffmpeg::util::error::EAGAIN
                || errno == ffmpeg::util::error::EWOULDBLOCK =>
        {
            Ok(PollOutcome::Again)
        }
        // Fully drained after flush().
        Err(ffmpeg::Error::Eof) => Ok(PollOutcome::Eof),
        Err(e) => Err(e).context("receive_packet"),
    }
}
