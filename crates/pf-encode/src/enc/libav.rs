//! Shared libavcodec (FFmpeg) glue for the three libav encode backends — Linux NVENC
//! (`encode/linux/mod.rs`), VAAPI (`encode/linux/vaapi.rs`), and Windows AMF/QSV
//! (`encode/windows/ffmpeg_win.rs`) — so the byte-identical pieces live once (plan §2.2, the Tier-2
//! gap). Free functions and consts over borrowed handles; nothing here is per-frame `dyn`,
//! allocating, or on the zero-copy ingest path.
use crate::EncodedFrame;
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

/// An owned `AVBufferRef` — unref'd exactly once, when it drops.
///
/// The hwdevice/hwframes constructors used to unref by hand on *every* failure branch (three in the
/// CUDA path, two in VAAPI) and then once more in a hand-written `Drop`. That shape leaks the moment
/// somebody adds a branch and forgets the cleanup, and double-unrefs the moment two of them run —
/// and neither mistake is visible at the call site or catchable by the compiler. Ownership lives in
/// this type instead: an early `?` drops whatever was built so far, in reverse construction order,
/// with no cleanup code at the call site at all.
///
/// **Drop order** matters to the callers holding two of these. A frames context internally holds its
/// own reference on its device, and the code this replaced deliberately unref'd frames *before*
/// device. Rust drops struct fields in DECLARATION order, so a struct holding both must declare
/// frames before device to keep that. Refcounting makes either order sound in principle —
/// the device cannot die while a frames ctx still references it — but the observable order is kept
/// exactly as it shipped rather than quietly inverted by a field reorder.
pub(crate) struct AvBuffer(*mut ffi::AVBufferRef);

impl AvBuffer {
    /// Take ownership of a freshly-created `AVBufferRef`, rejecting the null that an ffmpeg
    /// allocator returns on failure (so the `is_null` check every caller used to open-code happens
    /// once, here).
    ///
    /// # Safety
    /// `p` must be null, or a live `AVBufferRef` whose ownership passes to the returned value —
    /// nothing else may unref it.
    pub(crate) unsafe fn from_raw(p: *mut ffi::AVBufferRef) -> Option<Self> {
        (!p.is_null()).then_some(AvBuffer(p))
    }

    /// The borrowed pointer, for the ffmpeg calls that read a ref without consuming it. Borrowed
    /// only — the `AvBuffer` stays the owner, so callers must not unref what this returns.
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.0
    }
}

impl Drop for AvBuffer {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null ref `from_raw` took ownership of, and this type is its
        // sole owner (it is neither `Clone` nor `Copy`, and `as_ptr` only lends), so this runs
        // exactly once for that reference. `av_buffer_unref` drops the one reference and nulls the
        // pointer through the `&mut`.
        unsafe { ffi::av_buffer_unref(&mut self.0) };
    }
}

/// An owned `AVFilterGraph`, freed exactly once when it drops.
///
/// The dmabuf path built its graph beside three `AvBuffer`s and unwound all four by hand on each
/// of eight failure branches — a four-line cleanup block copied eight times, once inside a macro.
/// Freeing the graph is the same ownership question as unref'ing a buffer, so it gets the same
/// answer.
///
/// Linux-only: the VAAPI dmabuf path is the sole filter-graph user in this crate. The Windows
/// AMF/QSV backends feed the encoder directly and build no graph, so on Windows this type would be
/// dead code — cfg'd out rather than `allow`ed, because "nothing here uses it" is the honest
/// statement and an `allow` would keep it compiling after it stopped being true anywhere.
#[cfg(target_os = "linux")]
pub(crate) struct AvFilterGraph(*mut ffi::AVFilterGraph);

#[cfg(target_os = "linux")]
impl AvFilterGraph {
    /// Allocate a filter graph, rejecting the null `avfilter_graph_alloc` returns on OOM.
    ///
    /// Safe: the call takes no arguments and has no precondition a caller could violate — the only
    /// contract is what happens to the result, and that is exactly what this type owns.
    pub(crate) fn alloc() -> Option<Self> {
        // SAFETY: parameterless allocator; it returns either a fresh graph whose ownership passes
        // to the value returned here, or null (rejected below).
        let g = unsafe { ffi::avfilter_graph_alloc() };
        (!g.is_null()).then_some(AvFilterGraph(g))
    }

    /// The borrowed pointer, for the `avfilter_*` calls that build into the graph without taking
    /// ownership of it.
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVFilterGraph {
        self.0
    }
}

#[cfg(target_os = "linux")]
impl Drop for AvFilterGraph {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null graph `alloc` took ownership of, and this type is its
        // sole owner (neither `Clone` nor `Copy`; `as_ptr` only lends), so this runs exactly once.
        // `avfilter_graph_free` frees the graph together with the filter contexts and per-filter
        // device refs it owns, and nulls the pointer through the `&mut`.
        unsafe { ffi::avfilter_graph_free(&mut self.0) };
    }
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
    let vbv_bits = ((bitrate_bps as f64 / fps.max(1) as f64) * crate::vbv_frames_env())
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
