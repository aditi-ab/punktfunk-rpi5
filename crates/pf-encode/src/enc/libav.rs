//! Shared libavcodec glue for the Linux NVENC, VAAPI, and Windows AMF/QSV backends.
//!
//! Free functions and consts over borrowed handles. Nothing here is per-frame `dyn`,
//! allocating, or on the zero-copy ingest path.
use crate::EncodedFrame;
use anyhow::{Context, Result};
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi; // = ffmpeg_sys_next
use ffmpeg_next::format::Pixel;
use ffmpeg_next::{encoder, Packet, Rational};
use std::os::raw::c_int;

/// `SWS_POINT` (nearest-neighbour). Src dims == dst dims, so this only picks the CSC path;
/// POINT is cheapest.
pub(crate) const SWS_POINT: c_int = 0x10;
/// `SWS_CS_ITU709`. RGB→YUV CSC coefficients.
pub(crate) const SWS_CS_ITU709: c_int = 1;
/// `SWS_CS_BT2020` (non-constant luminance). HDR X2BGR10→P010 CSC coefficients.
pub(crate) const SWS_CS_BT2020: c_int = 9;

/// `Pixel` is `#[repr(i32)]`-compatible with bindgen `AVPixelFormat`; ffmpeg-next's
/// documented conversion.
pub(crate) fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    ffi::AVPixelFormat::from(p)
}

/// Owned `AVBufferRef`; unref'd once, on drop.
///
/// A frames ctx holds a ref on its device. Rust drops fields in declaration order, so a
/// struct holding both must declare frames before device. Do not invert the fields.
pub(crate) struct AvBuffer(*mut ffi::AVBufferRef);

impl AvBuffer {
    /// Take ownership of a freshly-created `AVBufferRef`. Null → `None`.
    ///
    // unsafe-fn-no-op-ok: contract-deferring constructor (`Vec::set_len` shape) — the body is
    // safe; the ownership transfer promised here is what Drop/as_ptr later rely on.
    /// # Safety
    /// `p` must be null, or a live `AVBufferRef` whose ownership passes to the returned value —
    /// nothing else may unref it.
    pub(crate) unsafe fn from_raw(p: *mut ffi::AVBufferRef) -> Option<Self> {
        (!p.is_null()).then_some(AvBuffer(p))
    }

    /// Lends the pointer; this type stays the owner — callers must not unref it.
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

/// Owned `AVFilterGraph`; freed once, on drop.
///
/// Linux-only: VAAPI dmabuf is the only filter-graph user. Cfg'd out on Windows rather than
/// `allow`ed — an `allow` would keep compiling after nothing used it.
#[cfg(target_os = "linux")]
pub(crate) struct AvFilterGraph(*mut ffi::AVFilterGraph);

#[cfg(target_os = "linux")]
impl AvFilterGraph {
    /// Parameterless allocator: no caller precondition. Null (OOM) → `None`.
    pub(crate) fn alloc() -> Option<Self> {
        // SAFETY: parameterless allocator; it returns either a fresh graph whose ownership passes
        // to the value returned here, or null (rejected below).
        let g = unsafe { ffi::avfilter_graph_alloc() };
        (!g.is_null()).then_some(AvFilterGraph(g))
    }

    /// Lends the pointer; this type stays the owner.
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

/// Owned `AVFrame`; freed once, on drop.
///
/// Not ffmpeg-next's `Frame::empty()`: that wraps a null on allocator failure, and the next
/// field write is UB. `alloc` returns `Option` instead.
pub(crate) struct AvFrame(std::ptr::NonNull<ffi::AVFrame>);

impl AvFrame {
    /// Parameterless allocator: no caller precondition. Null (OOM) → `None`.
    pub(crate) fn alloc() -> Option<Self> {
        // SAFETY: parameterless allocator; it returns either a fresh, uniquely-owned frame whose
        // ownership passes to the value returned here, or null (rejected by NonNull::new).
        std::ptr::NonNull::new(unsafe { ffi::av_frame_alloc() }).map(AvFrame)
    }

    /// Lends the pointer; this type stays the owner — callers must not free or move-from it.
    pub(crate) fn as_ptr(&self) -> *mut ffi::AVFrame {
        self.0.as_ptr()
    }
}

impl Drop for AvFrame {
    fn drop(&mut self) {
        let mut p = self.0.as_ptr();
        // SAFETY: `p` is the non-null frame `alloc` took ownership of, and this type is its
        // sole owner (neither `Clone` nor `Copy`; `as_ptr` only lends), so this runs exactly
        // once. `av_frame_free` unrefs any buffers the frame holds (returning pooled hwframe
        // surfaces to their pool) and frees the frame; it nulls only the local copy.
        unsafe { ffi::av_frame_free(&mut p) };
    }
}

pub(crate) struct AvSwsContext(std::ptr::NonNull<ffi::SwsContext>);

impl AvSwsContext {
    /// Take ownership of a freshly-created `SwsContext`. Null → `None`.
    ///
    // unsafe-fn-no-op-ok: contract-deferring constructor (`Vec::set_len` shape) — the body is
    // safe; the ownership transfer promised here is what Drop/as_ptr later rely on.
    /// # Safety
    /// `p` must be null, or a live `SwsContext` whose ownership passes to the returned value —
    /// nothing else may free it.
    pub(crate) unsafe fn from_raw(p: *mut ffi::SwsContext) -> Option<Self> {
        std::ptr::NonNull::new(p).map(AvSwsContext)
    }

    /// Lends the pointer; this type stays the owner.
    pub(crate) fn as_ptr(&self) -> *mut ffi::SwsContext {
        self.0.as_ptr()
    }
}

impl Drop for AvSwsContext {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null context `from_raw` took ownership of, and this type
        // is its sole owner (neither `Clone` nor `Copy`; `as_ptr` only lends), so this runs
        // exactly once.
        unsafe { ffi::sws_freeContext(self.0.as_ptr()) };
    }
}

/// One `receive_packet` attempt. `Again` vs `Eof` stay distinct so a blocking drain can retry
/// versus stop.
pub(crate) enum PollOutcome {
    Packet(EncodedFrame),
    Again,
    Eof,
}

/// Shared low-latency RC on a **not-yet-opened** encoder: fixed fps, CBR (target == max),
/// B-frames off, VBV ≈ 1 frame of bits (`bitrate/fps`, overridable via `PUNKTFUNK_VBV_FRAMES`).
///
/// Libav's default VBV is loose; a high-motion P-frame then overflows the bounded send queue.
/// A ~1-frame buffer holds size roughly constant and takes motion as a QP dip.
/// Caller still sets pixel format and `gop_size` (backend-specific).
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
        Err(ffmpeg::Error::Other { errno })
            if errno == ffmpeg::util::error::EAGAIN
                || errno == ffmpeg::util::error::EWOULDBLOCK =>
        {
            Ok(PollOutcome::Again)
        }
        Err(ffmpeg::Error::Eof) => Ok(PollOutcome::Eof),
        Err(e) => Err(e).context("receive_packet"),
    }
}
