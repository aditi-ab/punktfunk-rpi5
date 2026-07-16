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
use ffmpeg_next::{encoder, Packet};
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
