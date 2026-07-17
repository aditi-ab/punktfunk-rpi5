//! VAAPI encoder via `ffmpeg-next` — AMD (Mesa `radeonsi`) and Intel (`iHD`/`i965`) over one
//! libavcodec backend (`h264_vaapi`/`hevc_vaapi`/`av1_vaapi`). The kernel driver differs per
//! vendor; the libva userspace API is identical, so a single encoder covers both. This is the
//! sibling of [`super::linux`] (NVENC/CUDA) behind the shared [`Encoder`] trait — selected in
//! [`super::open_video`] (NVIDIA → NVENC, AMD/Intel → here).
//!
//! Two input paths, chosen lazily from the FIRST frame's payload (so `open_video`'s signature
//! is unchanged and the encoder self-configures for whatever the capturer produces):
//! * **CPU upload** ([`CpuInner`]): the portal hands packed RGB/BGR CPU frames; we swscale to
//!   BT.709-limited NV12 and `av_hwframe_transfer_data` it into a pooled VA surface. Works on any
//!   VAAPI GPU with no capture changes (the capturer falls back to CPU frames on non-NVIDIA).
//! * **Zero-copy dmabuf** ([`DmabufInner`], `PUNKTFUNK_ZEROCOPY=1`): the capturer hands a packed-RGB
//!   dmabuf. We wrap it as an `AV_PIX_FMT_DRM_PRIME` frame and push it through a tiny filter graph
//!   `buffer(drm_prime) → hwmap=derive_device=vaapi → scale_vaapi=format=nv12 → buffersink`, so
//!   the import AND the RGB→NV12 colour conversion run on the GPU's video engine — no host CSC, no
//!   upload. The encoder takes the NV12 surfaces straight from the filter sink.
//!
//! Raw FFI: `ffmpeg-next` has no hwcontext/filter wrappers for what we need, so the
//! hwdevice/hwframes/buffersrc/buffersink calls go through `ffmpeg::ffi` (= `ffmpeg_sys_next`),
//! as the CUDA encode path and the clients' decode paths already do. The encoder is opened
//! *without* a global header, so VPS/SPS/PPS are in-band on every IDR.
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary};
use ffmpeg_next as ffmpeg;
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::ptr;
use std::sync::atomic::{AtomicU8, Ordering};

use super::libav::{
    apply_low_latency_rc, pixel_to_av, poll_encoder, PollOutcome, SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi; // = ffmpeg_sys_next

/// `fourcc(a,b,c,d)` — DRM FourCC packing (`a | b<<8 | c<<16 | d<<24`).
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// The render node a VAAPI/DRM device should open, from [`crate::gpu::linux_render_node`]: a
/// matched web-console GPU preference pins it, else `PUNKTFUNK_RENDER_NODE`, else the single-GPU
/// default.
fn render_node() -> CString {
    let p = crate::gpu::linux_render_node()
        .to_string_lossy()
        .into_owned();
    CString::new(p).unwrap_or_else(|_| CString::new("/dev/dri/renderD128").unwrap())
}

/// The swscale *source* pixel format for a captured CPU layout (packed RGB/BGR only).
fn vaapi_sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Yuv444 => {
            bail!("VAAPI CPU-input path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Which VAAPI entrypoint mode opened successfully, cached per codec (index = [`lp_idx`]):
/// 0 = unknown, 1 = default (full-feature `EncSlice`), 2 = low-power (`EncSliceLP`/VDEnc).
/// Modern Intel (Gen12+/Arc) removed the full-feature encode entrypoints, so the default open
/// fails there and only `low_power=1` works; AMD (radeonsi) is the reverse. Caching the resolved
/// mode lets later sessions/probes skip the known-failing attempt (and its libav error spew).
static LP_MODE: [AtomicU8; 3] = [AtomicU8::new(0), AtomicU8::new(0), AtomicU8::new(0)];

fn lp_idx(codec: Codec) -> usize {
    match codec {
        Codec::H264 => 0,
        Codec::H265 => 1,
        Codec::Av1 => 2,
        // Guarded by the open_video dispatch: PyroWave never opens the VAAPI backend.
        Codec::PyroWave => unreachable!("PyroWave has no VAAPI encoder"),
    }
}

/// `PUNKTFUNK_VAAPI_LOW_POWER` pins the entrypoint mode (`1` = low-power only, `0` = full-feature
/// only); unset → try full-feature first, fall back to low-power.
fn low_power_override() -> Option<bool> {
    match std::env::var("PUNKTFUNK_VAAPI_LOW_POWER").ok()?.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Open the VAAPI encoder, resolving the entrypoint mode: try the full-feature entrypoint first
/// and, if the driver rejects it, retry with `low_power=1` — modern Intel (Gen12+/Arc) exposes
/// ONLY the low-power VDEnc entrypoint (ffmpeg's `vaapi_encode` defaults `low_power=0` and errors
/// "no usable encoding entrypoint" there; LP additionally needs the HuC firmware, loaded by
/// default on those kernels). AMD keeps its first-try full-feature open byte-for-byte unchanged.
/// The resolved mode is cached per codec; `PUNKTFUNK_VAAPI_LOW_POWER` pins it.
/// Safety contract is [`open_vaapi_encoder_mode`]'s (borrowed `device_ref`/`frames_ref`).
unsafe fn open_vaapi_encoder(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
) -> Result<encoder::video::Encoder> {
    let idx = lp_idx(codec);
    let modes: &[bool] = match low_power_override() {
        Some(true) => &[true],
        Some(false) => &[false],
        None => match LP_MODE[idx].load(Ordering::Relaxed) {
            1 => &[false],
            2 => &[true],
            _ => &[false, true],
        },
    };
    let mut first_err = None;
    for &lp in modes {
        match open_vaapi_encoder_mode(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            device_ref,
            frames_ref,
            lp,
        ) {
            Ok(enc) => {
                LP_MODE[idx].store(if lp { 2 } else { 1 }, Ordering::Relaxed);
                if lp {
                    tracing::info!(
                        encoder = codec.vaapi_name(),
                        "VAAPI using the low-power (VDEnc) entrypoint"
                    );
                }
                return Ok(enc);
            }
            Err(e) => {
                tracing::debug!(
                    encoder = codec.vaapi_name(),
                    low_power = lp,
                    "VAAPI encoder open failed: {e:#}"
                );
                first_err.get_or_insert(e);
            }
        }
    }
    // `modes` is never empty, so at least one attempt ran and recorded its error. The first
    // (full-feature) error is the informative one — "no VA display" etc.
    Err(first_err.unwrap())
}

/// Build the FFmpeg encoder context (shared by both inner paths): name, mode, low-latency RC,
/// infinite GOP, BT.709-limited VUI, `pix_fmt=VAAPI`, and the given hw device + frames contexts.
/// Returns the opened encoder. `device_ref`/`frames_ref` are borrowed (ref'd into the context).
#[allow(clippy::too_many_arguments)]
unsafe fn open_vaapi_encoder_mode(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    low_power: bool,
) -> Result<encoder::video::Encoder> {
    let name = codec.vaapi_name();
    let av_codec = encoder::find_by_name(name).ok_or_else(|| {
        anyhow!("{name} not built into libavcodec (no VAAPI encoder for {codec:?})")
    })?;
    let mut video = codec::context::Context::new_with_codec(av_codec)
        .encoder()
        .video()
        .context("alloc video encoder")?;
    video.set_width(width);
    video.set_height(height);
    video.set_format(Pixel::NV12); // sw view; pix_fmt overridden to VAAPI below
                                   // Fixed rate, CBR, no B-frames, ~1-frame VBV — the shared low-latency RC contract.
    apply_low_latency_rc(&mut video, fps, bitrate_bps);
    let raw = video.as_mut_ptr();
    (*raw).gop_size = i32::MAX; // no periodic IDR (forced-IDR via pict_type=I on RFI)
                                // We hand the encoder BT.709 *limited* NV12 (swscale CSC on the CPU path; scale_vaapi pinned
                                // to `out_color_matrix=bt709:out_range=limited` on the zero-copy path, with the full-range
                                // RGB input tagged), so signal that VUI — else the client decoder washes the picture out.
    (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
    (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
    (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
    (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
    (*raw).hw_device_ctx = ffi::av_buffer_ref(device_ref);
    (*raw).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);

    let mut opts = Dictionary::new();
    // async_depth=1: `send_frame` blocks until THIS frame's ASIC encode completes — the lowest
    // latency structure libavcodec's vaapi_encode offers. Measured on the 780M at 1440p60: depth 1
    // = 8.3 ms end-to-end p50 vs depth 2 = 18 ms, because with depth ≥ 2 frame N's packet only
    // materializes once frame N+1 is queued (a structural +1-frame delay no poll can beat). The
    // knob exists for pixel rates beyond the ASIC's serial budget (e.g. 1440p120+ on an iGPU),
    // where depth 2 restores throughput at that one-frame cost. NOTE: the per-frame block tracks
    // GPU CLOCKS — a paced 60 fps trickle lets the VCN downclock (~8 ms/frame vs ~4.4 ms hot);
    // see `gpuclocks` for the session clock pin that removes the ramp tax.
    let depth = std::env::var("PUNKTFUNK_VAAPI_ASYNC_DEPTH")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|d| (1..=8).contains(d))
        .unwrap_or(1);
    opts.set("async_depth", &depth.to_string());
    if low_power {
        opts.set("low_power", "1"); // VDEnc — the only encode entrypoint on modern Intel
    }
    video.open_with(opts).with_context(|| {
        format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps, low_power={low_power})")
    })
}

/// Probe whether THIS GPU can VAAPI-encode `codec`, by opening a tiny encoder: the driver rejects
/// codecs its video engine can't do (e.g. AV1 on pre-RDNA3 AMD / pre-Arc Intel). Used to build the
/// GameStream codec advertisement so a client never negotiates a codec the GPU can't encode. The
/// device + encoder are torn down immediately (RAII).
pub fn probe_can_encode(codec: Codec) -> bool {
    if ffmpeg::init().is_err() {
        return false;
    }
    // SAFETY: `ffmpeg::init()` returned Ok above, so libav is initialized. `av_log_get_level`/
    // `av_log_set_level` only read/write libav's global integer log level (no pointer args) and are
    // always sound to call post-init. `VaapiHw::new` (an `unsafe fn`) builds a VAAPI device + NV12
    // frames pool from the literal NV12/640x480/pool=2 args and hands back a RAII handle that unrefs
    // both `AVBufferRef`s on drop. `open_vaapi_encoder` (an `unsafe fn`) borrows `hw.device_ref`/
    // `hw.frames_ref` — the two non-null refs `VaapiHw::new` just created — and `av_buffer_ref`s them
    // into the encoder; `hw` is a live local for the whole match arm, so the borrows outlive the
    // synchronous call, and both `hw` and the probe encoder are dropped (RAII) when the arm ends.
    unsafe {
        // A missing VA device (non-VAAPI host, GPU-less CI) is an expected probe outcome — quiet
        // ffmpeg's "No VA display found" error for the probe, then restore the level.
        let prev = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_FATAL);
        let ok = match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, 640, 480, 2) {
            Ok(hw) => {
                open_vaapi_encoder(codec, 640, 480, 30, 2_000_000, hw.device_ref, hw.frames_ref)
                    .is_ok()
            }
            Err(_) => false,
        };
        ffi::av_log_set_level(prev);
        ok
    }
}

/// Whether the active VAAPI GPU can encode HEVC **4:4:4** (Range Extensions). **Deferred in v1 —
/// always `false`.** VAAPI HEVC 4:4:4 encode is narrow and vendor-specific (the lab's AMD Phoenix1 /
/// RDNA3 exposes only `VAProfileHEVCMain`/`Main10` `EncSlice`, no `Main444`), and there is no
/// validated hardware to build + verify the 4:4:4 surface/profile path against. Returning `false`
/// keeps the negotiation honest: a VAAPI host resolves every session to 4:2:0 before the Welcome, so
/// the client never builds a 4:4:4 decoder it would only get 4:2:0 frames for. (Follow-up: implement
/// and validate on an Intel Arc / RDNA4-class box that advertises a HEVC 4:4:4 encode entrypoint.)
pub fn probe_can_encode_444(_codec: Codec) -> bool {
    tracing::info!("VAAPI HEVC 4:4:4 encode is not implemented yet — declining (encoding 4:2:0)");
    false
}

// ---------------------------------------------------------------------------------------------
// CPU upload path (Phase 1): swscale RGB→NV12 → upload into a pooled VA surface → encode.
// ---------------------------------------------------------------------------------------------

/// VAAPI device + NV12 frames pool (the encoder's input surfaces for the CPU path).
struct VaapiHw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl VaapiHw {
    unsafe fn new(sw_format: ffi::AVPixelFormat, w: u32, h: u32, pool: c_int) -> Result<Self> {
        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        let node = render_node();
        let r = ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if r < 0 {
            bail!("no VAAPI device ({:?}): {}", node, ffmpeg::Error::from(r));
        }
        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_alloc(VAAPI) failed");
        }
        let fc = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = sw_format;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = pool;
        let r = ffi::av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_init(VAAPI) failed ({r})");
        }
        Ok(VaapiHw {
            device_ref,
            frames_ref,
        })
    }
}

impl Drop for VaapiHw {
    fn drop(&mut self) {
        // SAFETY: `frames_ref`/`device_ref` are the two non-null `AVBufferRef`s `VaapiHw::new`
        // created (it bails before constructing `Self` if either alloc fails, so a live `VaapiHw`
        // always holds both). `av_buffer_unref` drops one reference and nulls the pointer through the
        // `&mut`. This `Drop` runs exactly once and `VaapiHw` owns these refs exclusively, so there
        // is no double-free / use-after-free. Frames are unref'd before the device because the frames
        // ctx internally holds a ref on the device (refcounted, so the order is sound either way).
        unsafe {
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

struct CpuInner {
    enc: encoder::video::Encoder,
    hw: VaapiHw,
    sws: *mut ffi::SwsContext,
    nv12: *mut ffi::AVFrame, // reusable software NV12 staging frame (swscale dst → upload src)
    src_format: PixelFormat,
    width: u32,
    height: u32,
}

impl CpuInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let src_pixel = vaapi_sws_src(format)?;
        const POOL: c_int = 16;
        // SAFETY: `VaapiHw::new` (an `unsafe fn`) requires libav initialized — guaranteed because the
        // only path here is `VaapiEncoder::open` → `ensure_inner` → `CpuInner::open`, and `open` ran
        // `ffmpeg::init()`. The args are valid: NV12 sw_format, the validated positive `width`/`height`,
        // pool=16. It returns a RAII `VaapiHw` that unrefs its two `AVBufferRef`s on drop.
        let hw = unsafe { VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, width, height, POOL)? };
        // SAFETY: `open_vaapi_encoder` (an `unsafe fn`) borrows `hw.device_ref`/`hw.frames_ref` — both
        // non-null (`VaapiHw::new` guarantees it) and from the `hw` just built above, which is a live
        // local that outlives this synchronous call. The fn `av_buffer_ref`s them into the encoder, so
        // the encoder holds its own references; `hw` is also moved into the returned `CpuInner` next to
        // `enc`, keeping the device/frames alive for the encoder's whole lifetime.
        let enc = unsafe {
            open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                hw.device_ref,
                hw.frames_ref,
            )?
        };
        // swscale RGB→NV12, BT.709 limited (matches the VUI), no rescale.
        let src_av = pixel_to_av(src_pixel);
        // SAFETY: `sws_getContext` allocates a swscale context for the given src/dst dimensions and
        // pixel formats. All four dims are the encoder's positive `width`/`height` cast to `c_int`;
        // `src_av` is a valid `AVPixelFormat` (from `pixel_to_av` of the `vaapi_sws_src`-validated
        // `src_pixel`), the dst is NV12. The three trailing pointers (srcFilter, dstFilter, param) are
        // explicitly null = "use defaults", which the API documents as accepted. No Rust memory is
        // borrowed — only by-value ints/enums — and the returned pointer is null-checked just below.
        let sws = unsafe {
            ffi::sws_getContext(
                width as c_int,
                height as c_int,
                src_av,
                width as c_int,
                height as c_int,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_POINT,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if sws.is_null() {
            bail!("sws_getContext(RGB→NV12) failed");
        }
        // SAFETY: `sws` is the non-null `SwsContext` from `sws_getContext` above (the `is_null()`
        // check immediately preceding returned false). `sws_getCoefficients(SWS_CS_ITU709)` returns a
        // pointer into a libswscale static const coefficient table valid for the whole process, reused
        // here for both the inverse (src) and forward (dst) matrices. `sws_setColorspaceDetails` only
        // reads those tables and writes scalar CSC settings into `sws`; the table pointer outlives the
        // synchronous call and no Rust memory is passed.
        unsafe {
            let cs709 = ffi::sws_getCoefficients(SWS_CS_ITU709);
            ffi::sws_setColorspaceDetails(sws, cs709, 1, cs709, 0, 0, 1 << 16, 1 << 16);
        }
        // SAFETY: `av_frame_alloc` returns a fresh, uniquely-owned heap `AVFrame` (null-checked — on
        // null we free the already-built `sws` and bail). We then write the plain `format`/`width`/
        // `height` fields through the non-null, properly-aligned `f` (sole owner, not yet shared).
        // `av_frame_get_buffer(f, 0)` allocates backing storage for those dims/format; on failure we
        // free `f` and `sws` (unwinding the half-built state) and bail. On success `f` is a fully-owned
        // NV12 frame stored in `CpuInner.nv12` and freed once in `CpuInner::drop`. `f` is a unique
        // fresh pointer, so none of these writes alias anything.
        let nv12 = unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                ffi::sws_freeContext(sws);
                bail!("av_frame_alloc(NV12) failed");
            }
            (*f).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*f).width = width as c_int;
            (*f).height = height as c_int;
            if ffi::av_frame_get_buffer(f, 0) < 0 {
                let mut f = f;
                ffi::av_frame_free(&mut f);
                ffi::sws_freeContext(sws);
                bail!("av_frame_get_buffer(NV12) failed");
            }
            f
        };
        tracing::info!(
            encoder = codec.vaapi_name(),
            "VAAPI encode active ({width}x{height}@{fps}, CPU→NV12 upload path)"
        );
        Ok(CpuInner {
            enc,
            hw,
            sws,
            nv12,
            src_format: format,
            width,
            height,
        })
    }

    fn submit(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {format:?} != encoder source {:?}",
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_row = w * self.src_format.bytes_per_pixel();
        anyhow::ensure!(bytes.len() >= src_row * h, "captured buffer too small");
        // SAFETY: The `ensure!`s above guarantee `format == self.src_format` and
        // `bytes.len() >= src_row * h`. `sws_scale` reads `h` rows of `src_row` bytes from
        // `src_data[0] = bytes.as_ptr()` (the other planes null/0 — packed RGB is single-plane), all
        // in bounds; `bytes`, `src_data`, `src_stride` are live locals for this synchronous call.
        // `self.sws` is the non-null context built in `open`; it writes into `self.nv12` (a non-null
        // owned frame whose `data`/`linesize` in-struct arrays were sized by `av_frame_get_buffer`).
        // `av_frame_alloc` (null-checked) yields a fresh `hwf`; `av_hwframe_get_buffer` pulls a pooled
        // VAAPI surface from the live non-null `self.hw.frames_ref`; `av_hwframe_transfer_data` uploads
        // the staged NV12 into it — both frames live, failures free `hwf` and bail. We then write
        // `pts`/`pict_type` through the non-null `hwf` and `avcodec_send_frame` it into the live
        // owned `self.enc` context (which takes its own ref), then free our `hwf` ref exactly once.
        // The encoder runs only on this thread (see `unsafe impl Send`), so no aliasing/data race.
        unsafe {
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.nv12).data.as_ptr(),
                (*self.nv12).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            let mut hwf = ffi::av_frame_alloc();
            if hwf.is_null() {
                bail!("av_frame_alloc(hw) failed");
            }
            if ffi::av_hwframe_get_buffer(self.hw.frames_ref, hwf, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_get_buffer(VAAPI) failed");
            }
            if ffi::av_hwframe_transfer_data(hwf, self.nv12, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_transfer_data(→VAAPI) failed");
            }
            (*hwf).pts = pts;
            (*hwf).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), hwf);
            ffi::av_frame_free(&mut hwf);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        Ok(())
    }
}

impl Drop for CpuInner {
    fn drop(&mut self) {
        // SAFETY: `self.nv12` (an owned `AVFrame`) and `self.sws` (an owned `SwsContext`) are each
        // freed exactly once here, guarded by `is_null()` so a never-set pointer is skipped (no double
        // free). `CpuInner` owns both exclusively and `Drop` runs once. `av_frame_free` takes `&mut`
        // and nulls the pointer. `self.enc`/`self.hw` are freed afterward by their own `Drop` impls;
        // the encoder holds its own `av_buffer_ref`'d device/frames copies, so field-drop order is
        // irrelevant to soundness.
        unsafe {
            if !self.nv12.is_null() {
                ffi::av_frame_free(&mut self.nv12);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Zero-copy dmabuf path: DRM-PRIME → hwmap(vaapi) → scale_vaapi(nv12) filter graph → encode.
// ---------------------------------------------------------------------------------------------

struct DmabufInner {
    enc: encoder::video::Encoder,
    /// DRM device the source dmabuf frames reference (the buffersrc's `hw_frames_ctx` device).
    drm_device: *mut ffi::AVBufferRef,
    /// VAAPI device driving `hwmap`/`scale_vaapi`/the encoder.
    vaapi_device: *mut ffi::AVBufferRef,
    /// DRM-PRIME frames context for the imported dmabufs (buffersrc input).
    drm_frames: *mut ffi::AVBufferRef,
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    width: u32,
    height: u32,
    fourcc: u32,
    /// Frames submitted — drives the sampled `PUNKTFUNK_PERF` breakdown of the synchronous
    /// submit (import+push vs CSC pull vs encoder send), the stage that dominates AMD/Intel
    /// host latency (7.9 ms p50 at 1440p on the 780M).
    frames: u64,
}

impl DmabufInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let drm_fourcc = crate::zerocopy::drm_fourcc(format)
            .ok_or_else(|| anyhow!("no DRM fourcc for {format:?} (VAAPI zero-copy)"))?;
        let node = render_node();
        // SAFETY: libav is initialized (`VaapiEncoder::open` ran `ffmpeg::init()` before
        // `ensure_inner` → `DmabufInner::open`). Every raw pointer dereferenced below is either freshly
        // allocated by the immediately-preceding ffmpeg call and null-checked, or an in-struct field of
        // such an object:
        //  * `node` is a `CString` (from `render_node`) live for the whole block; its `.as_ptr()` is a
        //    NUL-terminated path read only during `av_hwdevice_ctx_create`.
        //  * `av_hwdevice_ctx_create(&mut drm_device, DRM, …)` / `…_create_derived(&mut vaapi_device,
        //    VAAPI, drm_device, …)`: on `r < 0` the out-param stays null and we bail (the derive path
        //    unrefs `drm_device` first); on success each is a non-null owned `AVBufferRef`.
        //  * `av_hwframe_ctx_alloc(drm_device)` → `drm_frames` (null-checked); `(*drm_frames).data` is
        //    its `AVHWFramesContext` payload, written before `av_hwframe_ctx_init`.
        //  * `avfilter_graph_alloc` → `graph` (null-checked); `avfilter_get_by_name` returns a static
        //    const `AVFilter` (process-lifetime) or null; `avfilter_graph_alloc_filter` allocates each
        //    filter ctx inside `graph`; the four are null-checked together. `inst`/arg strings are
        //    'static C literals.
        //  * `(*hwmap/scale).hw_device_ctx = av_buffer_ref(vaapi_device)` attaches a NEW ref owned by
        //    the filter (freed by `avfilter_graph_free`); our `vaapi_device` ref is untouched.
        //  * `av_buffersink_get_hw_frames_ctx(sink)` → `nv12_ctx` is a borrowed ref owned by the sink,
        //    valid while `graph` lives (and `graph` is moved into the returned `DmabufInner`).
        //  * `open_vaapi_encoder` borrows `vaapi_device` (our live owned ref) and `nv12_ctx` (sink's
        //    live ref) and `av_buffer_ref`s both into the encoder.
        // Every early-error path unref's the allocated buffers and frees the graph in the right order
        // before bailing; on success the four `AVBufferRef`s + `graph` + `src`/`sink` are moved into
        // `DmabufInner` and freed in its `Drop`. (Two non-UB leaks noted below: `av_buffersrc_*` and
        // the final `?`.)
        unsafe {
            // DRM device (source dmabuf frames) + a VAAPI device derived from it (same GPU) for
            // hwmap/scale_vaapi/the encoder.
            let mut drm_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut drm_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!(
                    "av_hwdevice_ctx_create(DRM {:?}): {}",
                    node,
                    ffmpeg::Error::from(r)
                );
            }
            let mut vaapi_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create_derived(
                &mut vaapi_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                drm_device,
                0,
            );
            if r < 0 {
                ffi::av_buffer_unref(&mut drm_device);
                bail!("derive VAAPI from DRM: {}", ffmpeg::Error::from(r));
            }

            // DRM-PRIME frames context for the imported dmabufs.
            let mut drm_frames = ffi::av_hwframe_ctx_alloc(drm_device);
            if drm_frames.is_null() {
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_hwframe_ctx_alloc(DRM) failed");
            }
            let fc = (*drm_frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0; // packed XR24 RGB plane
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            if ffi::av_hwframe_ctx_init(drm_frames) < 0 {
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_hwframe_ctx_init(DRM) failed");
            }

            // Filter graph: buffer(drm_prime) → hwmap=derive_device=vaapi:mode=read →
            // scale_vaapi=format=nv12 → buffersink.
            let mut graph = ffi::avfilter_graph_alloc();
            if graph.is_null() {
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_graph_alloc failed");
            }

            let mk = |name: &CStr, inst: &CStr| -> *mut ffi::AVFilterContext {
                let f = ffi::avfilter_get_by_name(name.as_ptr());
                if f.is_null() {
                    return ptr::null_mut();
                }
                ffi::avfilter_graph_alloc_filter(graph, f, inst.as_ptr())
            };
            let src = mk(c"buffer", c"in");
            let hwmap = mk(c"hwmap", c"map");
            let scale = mk(c"scale_vaapi", c"csc");
            let sink = mk(c"buffersink", c"out");
            if src.is_null() || hwmap.is_null() || scale.is_null() || sink.is_null() {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("a VAAPI filter (buffer/hwmap/scale_vaapi/buffersink) is missing");
            }
            // hwmap maps the DRM-PRIME input onto THIS vaapi device; scale_vaapi runs the CSC on
            // it. Giving both our device (rather than `hwmap=derive_device`) keeps every surface —
            // and the sink's output frames ctx the encoder adopts — on one VADisplay.
            (*hwmap).hw_device_ctx = ffi::av_buffer_ref(vaapi_device);
            (*scale).hw_device_ctx = ffi::av_buffer_ref(vaapi_device);

            // buffersrc params: DRM-PRIME frames, the drm_frames ctx.
            let par = ffi::av_buffersrc_parameters_alloc();
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*par).width = width as c_int;
            (*par).height = height as c_int;
            // Declare the link's colour up front (full-range RGB — the compositor's desktop) so
            // the per-frame tags in `submit` match the negotiated link instead of reading as a
            // mid-stream property change.
            (*par).color_space = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*par).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: fps as c_int,
            };
            // Assign `drm_frames` BORROWED (no extra ref): `av_buffersrc_parameters_set` takes its
            // own ref of `par->hw_frames_ctx` (via av_buffer_replace), and `av_free(par)` frees only
            // the struct, not the ref. Our single owned `drm_frames` ref is retained, lives in
            // `DmabufInner`, and is unref'd in `Drop`. Wrapping it in `av_buffer_ref` here would leak
            // that extra ref every session (the persistent listener would accumulate them).
            (*par).hw_frames_ctx = drm_frames;
            let r = ffi::av_buffersrc_parameters_set(src, par);
            ffi::av_free(par as *mut _);
            if r < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_buffersrc_parameters_set failed ({r})");
            }
            macro_rules! init {
                ($ctx:expr, $args:expr, $what:literal) => {{
                    let r = ffi::avfilter_init_str($ctx, $args);
                    if r < 0 {
                        ffi::avfilter_graph_free(&mut graph);
                        ffi::av_buffer_unref(&mut drm_frames);
                        ffi::av_buffer_unref(&mut vaapi_device);
                        ffi::av_buffer_unref(&mut drm_device);
                        bail!(concat!("init ", $what, " failed ({})"), r);
                    }
                }};
            }
            init!(src, ptr::null(), "buffer");
            init!(hwmap, c"mode=read".as_ptr(), "hwmap");
            // Pin the VPP's output colour to what the encoder's VUI signals (BT.709 limited).
            // Without the explicit options the conversion matrix is whatever the driver defaults
            // to for an unspecified output (Mesa: BT.601) — a hue shift against the signaled VUI.
            init!(
                scale,
                c"format=nv12:out_color_matrix=bt709:out_range=limited".as_ptr(),
                "scale_vaapi"
            );
            init!(sink, ptr::null(), "buffersink");

            let link = |a: *mut ffi::AVFilterContext, b: *mut ffi::AVFilterContext| -> c_int {
                ffi::avfilter_link(a, 0, b, 0)
            };
            if link(src, hwmap) < 0 || link(hwmap, scale) < 0 || link(scale, sink) < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_link failed");
            }
            let r = ffi::avfilter_graph_config(graph, ptr::null_mut());
            if r < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_graph_config failed ({r})");
            }

            // The encoder takes NV12 surfaces from the sink's output frames context.
            let nv12_ctx = ffi::av_buffersink_get_hw_frames_ctx(sink);
            if nv12_ctx.is_null() {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("filter sink has no VAAPI frames context");
            }
            // On encoder-open failure, free the graph + our owned buffer refs before bailing (matching
            // every error path above) so a failed session doesn't leak them. `nv12_ctx` is borrowed
            // from the sink (owned by `graph`), so `avfilter_graph_free` reclaims it — don't unref it
            // separately. On success the encoder takes its own ref of `vaapi_device`, and `drm_frames`/
            // `vaapi_device`/`drm_device`/`graph` move into `DmabufInner` (freed in `Drop`).
            let enc = match open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                vaapi_device,
                nv12_ctx,
            ) {
                Ok(enc) => enc,
                Err(e) => {
                    ffi::avfilter_graph_free(&mut graph);
                    ffi::av_buffer_unref(&mut drm_frames);
                    ffi::av_buffer_unref(&mut vaapi_device);
                    ffi::av_buffer_unref(&mut drm_device);
                    return Err(e);
                }
            };

            tracing::info!(
                encoder = codec.vaapi_name(),
                "VAAPI encode active ({width}x{height}@{fps}, zero-copy dmabuf → GPU NV12)"
            );
            Ok(DmabufInner {
                enc,
                drm_device,
                vaapi_device,
                drm_frames,
                graph,
                src,
                sink,
                width,
                height,
                fourcc: drm_fourcc,
                frames: 0,
            })
        }
    }

    fn submit(&mut self, dmabuf: &DmabufFrame, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            dmabuf.fourcc == self.fourcc,
            "dmabuf fourcc {:#x} != encoder {:#x}",
            dmabuf.fourcc,
            self.fourcc
        );
        // Sampled breakdown of this synchronous submit under PUNKTFUNK_PERF: push = descriptor
        // build + buffersrc (the per-frame DRM→VA import happens inside hwmap on the pull path),
        // pull = buffersink (VPP CSC + any sync), send = avcodec_send_frame. One line per ~2 s.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;
        let t0 = std::time::Instant::now();
        let t_push: std::time::Duration;
        let t_pull: std::time::Duration;
        // SAFETY: The `ensure!` above checked `dmabuf.fourcc == self.fourcc`.
        //  * `std::mem::zeroed::<AVDRMFrameDescriptor>()` is sound: it is a `#[repr(C)]` POD of ints and
        //    nested int-struct arrays (no `NonNull`/refs), for which all-zero is a valid bit pattern;
        //    `Box` puts it on the heap with a unique owner.
        //  * `dmabuf.fd.as_raw_fd()` is the fd of the caller's `&DmabufFrame`, which owns it for the
        //    whole synchronous `submit`; we describe one object/layer/plane from its
        //    fourcc/modifier/offset/stride and pass `object.size = 0` (ffmpeg queries the real size).
        //  * `av_frame_alloc` → `drm` (null-checked); we set its scalar fields and
        //    `hw_frames_ctx = av_buffer_ref(self.drm_frames)` (new ref of the live owned ctx).
        //  * `data[0] = Box::into_raw(desc)` transfers the box into the frame; `buf[0] =
        //    av_buffer_create(.., free_desc, ..)` registers a destructor that reclaims it exactly once
        //    when the buffer's refcount hits zero — matched alloc/free, no leak/double-free.
        //  * `av_buffersrc_add_frame_flags(self.src, drm, KEEP_REF)` pushes a ref into the live
        //    buffersrc; KEEP_REF keeps our own `drm` ref, which we then `av_frame_free`. We pull the
        //    converted surface with `av_buffersink_get_frame(self.sink, nv12)` BEFORE returning, so the
        //    dmabuf (owned by the caller) is read while still valid. `nv12` is sent into the live owned
        //    `self.enc` (takes its own ref) and our ref freed once. Single-threaded encoder → no race.
        unsafe {
            // Build a DRM-PRIME AVFrame describing the dmabuf (one object/fd, one layer/plane).
            let mut desc: Box<ffi::AVDRMFrameDescriptor> = Box::new(std::mem::zeroed());
            desc.nb_objects = 1;
            desc.objects[0].fd = dmabuf.fd.as_raw_fd();
            desc.objects[0].size = 0;
            desc.objects[0].format_modifier = dmabuf.modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = self.fourcc;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = dmabuf.offset as isize;
            desc.layers[0].planes[0].pitch = dmabuf.stride as isize;

            let mut drm = ffi::av_frame_alloc();
            if drm.is_null() {
                bail!("av_frame_alloc(drm) failed");
            }
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*drm).width = self.width as c_int;
            (*drm).height = self.height as c_int;
            // The dmabuf is the compositor's rendered desktop: full-range RGB. Tag the frame so
            // the VPP's colour negotiation sees the real input instead of "unspecified" (an
            // untagged input lets the driver pick its own default for the RGB→NV12 conversion —
            // Mesa's is BT.601, contradicting the BT.709-limited VUI the encoder signals).
            (*drm).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*drm).colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*drm).hw_frames_ctx = ffi::av_buffer_ref(self.drm_frames);
            (*drm).data[0] = Box::into_raw(desc) as *mut u8;
            // Own the descriptor so it frees with the frame (the fd is owned by the DmabufFrame,
            // which outlives this call — the graph reads the surface before submit returns).
            extern "C" fn free_desc(_opaque: *mut std::ffi::c_void, data: *mut u8) {
                // SAFETY: `data` is exactly the pointer produced by `Box::into_raw(desc)` and passed as
                // `av_buffer_create`'s first arg, which libav hands back verbatim to this callback. It
                // is a valid, uniquely-owned `Box<AVDRMFrameDescriptor>` raw pointer; libav invokes the
                // callback exactly once (when the last buffer ref drops), so `from_raw` + `drop`
                // reclaims it exactly once — no double-free. `_opaque` is unused (we passed null).
                unsafe { drop(Box::from_raw(data as *mut ffi::AVDRMFrameDescriptor)) };
            }
            (*drm).buf[0] = ffi::av_buffer_create(
                (*drm).data[0],
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_desc),
                ptr::null_mut(),
                0,
            );

            // Push through hwmap → scale_vaapi; pull the NV12 surface back out.
            let r = ffi::av_buffersrc_add_frame_flags(
                self.src,
                drm,
                ffi::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
            );
            ffi::av_frame_free(&mut drm);
            if r < 0 {
                bail!("av_buffersrc_add_frame failed ({r})");
            }
            t_push = t0.elapsed();
            let mut nv12 = ffi::av_frame_alloc();
            if nv12.is_null() {
                bail!("av_frame_alloc(nv12) failed");
            }
            let r = ffi::av_buffersink_get_frame(self.sink, nv12);
            if r < 0 {
                ffi::av_frame_free(&mut nv12);
                bail!("av_buffersink_get_frame failed ({r})");
            }
            t_pull = t0.elapsed() - t_push;
            (*nv12).pts = pts;
            (*nv12).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), nv12);
            ffi::av_frame_free(&mut nv12);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        if sample {
            let t_send = t0.elapsed() - t_push - t_pull;
            tracing::info!(
                push_us = t_push.as_micros() as u64,
                pull_us = t_pull.as_micros() as u64,
                send_us = t_send.as_micros() as u64,
                "VAAPI submit split (sampled): push=desc+buffersrc pull=hwmap-import+VPP-CSC \
                 send=avcodec_send_frame"
            );
        }
        Ok(())
    }
}

impl Drop for DmabufInner {
    fn drop(&mut self) {
        // SAFETY: `graph`/`drm_frames`/`vaapi_device`/`drm_device` are the non-null objects
        // `DmabufInner::open` built and moved into `self` (open bails before constructing `Self` if any
        // alloc fails). `avfilter_graph_free` frees the graph (and the per-filter device refs it owns);
        // each `av_buffer_unref` drops one ref and nulls the pointer via `&mut`. `DmabufInner` owns all
        // four exclusively and `Drop` runs once → no double-free/use-after-free. The graph is freed
        // first (it holds refs on the devices), then frames, then the derived VAAPI device, then DRM.
        // (`self.enc` drops via ffmpeg-next afterward, holding its own refs.)
        unsafe {
            ffi::avfilter_graph_free(&mut self.graph);
            ffi::av_buffer_unref(&mut self.drm_frames);
            ffi::av_buffer_unref(&mut self.vaapi_device);
            ffi::av_buffer_unref(&mut self.drm_device);
        }
    }
}

// ---------------------------------------------------------------------------------------------

enum Inner {
    Cpu(CpuInner),
    Dmabuf(DmabufInner),
}

pub struct VaapiEncoder {
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    /// Built lazily from the first frame's payload (CPU upload vs zero-copy dmabuf).
    inner: Option<Inner>,
    frame_idx: i64,
    force_kf: bool,
    /// Frames sent to the encoder but not yet returned as packets. Gates [`poll`](Encoder::poll)'s
    /// bounded wait: with `async_depth > 1` a submitted frame's AU lands ~ASIC-time later, so poll
    /// briefly waits for it (same-tick delivery) — but only when something is actually in flight.
    in_flight: u32,
}

// Raw FFI pointers; the encoder lives on a single thread (same contract as `NvencEncoder`).
// SAFETY: `VaapiEncoder`'s `Inner` holds raw FFI pointers (`SwsContext`, `AVFrame`, `AVBufferRef`,
// `AVFilterContext`, `AVCodecContext`) that are not `Send` by default. The encoder is owned and
// driven by exactly ONE thread — the host's per-session encode thread it is moved (transferred) to —
// and is only ever touched through `&mut self` methods, so it is never aliased or accessed
// concurrently from two threads. None of the underlying libav/libswscale objects have thread
// affinity (they are not thread-local), so transferring ownership across threads is sound. This
// asserts `Send` (transfer) only; `Sync` (shared `&`) is deliberately NOT implemented.
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: super::ChromaFormat,
    ) -> Result<Self> {
        if bit_depth != 8 {
            tracing::warn!(bit_depth, "VAAPI 10-bit not yet wired — encoding 8-bit");
        }
        // VAAPI 4:4:4 is deferred (see `probe_can_encode_444`): no validated AMD/Intel hardware in the
        // lab exposes a HEVC 4:4:4 encode entrypoint, and the probe returns false so the host never
        // negotiates 4:4:4 for a VAAPI session. If a request slips through, fall back to 4:2:0 rather
        // than emit an unverified stream — the host signalled 4:2:0 in the Welcome anyway.
        if chroma.is_444() {
            tracing::warn!("VAAPI 4:4:4 encode not implemented — encoding 4:2:0");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `av_log_set_level` sets libav's global integer log level; `48` (= AV_LOG_DEBUG)
            // is a valid level and there are no pointer args. libav was just initialized by the
            // `ffmpeg::init()` above, so the call is always sound.
            unsafe { ffi::av_log_set_level(48) };
        }
        // Validate the codec/format up front so a bad request fails at open, not on the first frame.
        let _ = vaapi_sws_src(format)?;
        Ok(VaapiEncoder {
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            inner: None,
            frame_idx: 0,
            force_kf: false,
            in_flight: 0,
        })
    }

    fn ensure_inner(&mut self, want_dmabuf: bool) -> Result<&mut Inner> {
        if self.inner.is_none() {
            let inner = if want_dmabuf {
                Inner::Dmabuf(DmabufInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            } else {
                Inner::Cpu(CpuInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            };
            self.inner = Some(inner);
        }
        Ok(self.inner.as_mut().unwrap())
    }
}

impl Encoder for VaapiEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let pts = self.frame_idx;
        self.frame_idx += 1;
        let idr = self.force_kf;
        self.force_kf = false;
        match &captured.payload {
            FramePayload::Cpu(bytes) => match self.ensure_inner(false)? {
                Inner::Cpu(c) => c.submit(bytes, captured.format, pts, idr),
                Inner::Dmabuf(_) => bail!("VAAPI encoder built for dmabuf got a CPU frame"),
            },
            FramePayload::Dmabuf(d) => match self.ensure_inner(true)? {
                Inner::Dmabuf(dm) => dm.submit(d, pts, idr),
                Inner::Cpu(_) => bail!("VAAPI encoder built for CPU got a dmabuf frame"),
            },
            FramePayload::Cuda(_) => bail!(
                "VAAPI encoder received a CUDA frame — that payload is NVENC-only; \
                 unset PUNKTFUNK_ZEROCOPY or don't force PUNKTFUNK_ENCODER=vaapi on an NVIDIA host"
            ),
        }?;
        self.in_flight += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Encode-stall recovery: drop the wedged libavcodec encoder (its `Drop` releases the VA
    /// surfaces/filter graph/devices) and let the next `submit` rebuild it lazily from the first
    /// frame's payload, exactly like first-frame bring-up — the same drop-and-reopen lever the
    /// Windows QSV path has. The owed AUs are forfeited (`in_flight` zeroed) and the rebuilt
    /// encoder's first frame is forced IDR so the client resyncs immediately. Without this the
    /// encode-stall watchdog had no lever on Linux AMD/Intel and a wedged driver ended the session.
    fn reset(&mut self) -> bool {
        self.inner = None;
        self.in_flight = 0;
        self.force_kf = true;
        true
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // With `async_depth > 1`, `submit` no longer waits for the ASIC — the AU for the frame we
        // just sent lands ~one hardware-encode-time later. Wait for it (bounded) so it still ships
        // this tick: the same blocking-retrieve model as NVENC's lock_bitstream, at the ASIC's
        // real per-frame latency instead of send_frame's synchronous ~2× wait. The budget is 3/4
        // of a frame interval (capped 12 ms); on expiry return None — the AU rides the next poll.
        let enc = match &mut self.inner {
            Some(Inner::Cpu(c)) => &mut c.enc,
            Some(Inner::Dmabuf(d)) => &mut d.enc,
            None => return Ok(None),
        };
        let budget = std::time::Duration::from_micros(750_000 / self.fps.max(1) as u64)
            .min(std::time::Duration::from_millis(12));
        let deadline = std::time::Instant::now() + budget;
        loop {
            match poll_encoder(enc, self.fps)? {
                PollOutcome::Packet(au) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    return Ok(Some(au));
                }
                // No AU yet (EAGAIN) or drained (EOF) both fall through to the budget check,
                // exactly as the previous `Option::None` did.
                PollOutcome::Again | PollOutcome::Eof => {}
            }
            // Nothing ready: only wait when a frame is actually in flight (a drained/EOF'd
            // encoder must not spin the budget), and give the ASIC ~250 µs between checks.
            if self.in_flight == 0 || std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
    }

    fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            Some(Inner::Cpu(c)) => c.enc.send_eof().context("send_eof")?,
            Some(Inner::Dmabuf(d)) => d.enc.send_eof().context("send_eof")?,
            None => {}
        }
        Ok(())
    }
}
