//! VAAPI encoder (`h264_vaapi` / `hevc_vaapi` / `av1_vaapi`) for AMD Mesa and Intel iHD/i965.
//! Sibling of [`super::linux`] (NVENC) behind [`Encoder`]; [`super::open_video`] picks NVIDIA →
//! NVENC, AMD/Intel → here. Kernel drivers differ; libva is the same, so one encoder covers both.
//!
//! First-frame payload picks the path: packed RGB/BGR CPU ([`CpuInner`]) swscales to BT.709
//! NV12 (or BT.2020 P010) and uploads; `PUNKTFUNK_ZEROCOPY=1` dmabufs ([`DmabufInner`]) go
//! `buffer(drm_prime) → hwmap → scale_vaapi → buffersink` on the GPU. Pins:
//! `PUNKTFUNK_VAAPI_LOW_POWER`, `PUNKTFUNK_VAAPI_ASYNC_DEPTH`, `PUNKTFUNK_RENDER_NODE`. Opened
//! without a global header so VPS/SPS/PPS ride every IDR. FFI via `ffmpeg::ffi`.

use super::{Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary};
use ffmpeg_next as ffmpeg;
use pf_frame::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::ptr;
use std::sync::{Mutex, OnceLock};

use super::libav::{
    apply_low_latency_rc, pixel_to_av, poll_encoder, AvBuffer, AvFilterGraph, AvFrame,
    AvSwsContext, PollOutcome, SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi;

/// Render node libva opens: web-console GPU pin, else `PUNKTFUNK_RENDER_NODE`, else the
/// single-GPU default ([`pf_gpu::linux_render_node`]).
fn render_node() -> CString {
    let p = pf_gpu::linux_render_node().to_string_lossy().into_owned();
    CString::new(p).unwrap_or_else(|_| CString::new("/dev/dri/renderD128").unwrap())
}

fn vaapi_sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        PixelFormat::X2Rgb10 => Pixel::X2RGB10LE,
        PixelFormat::X2Bgr10 => Pixel::X2BGR10LE,
        PixelFormat::Nv12
        | PixelFormat::P010
        | PixelFormat::Rgb10a2
        | PixelFormat::Rgb10a2Sdr
        | PixelFormat::Yuv444 => {
            bail!("VAAPI CPU-input path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Latched VAAPI entrypoint: `1` = full-feature `EncSlice`, `2` = low-power `EncSliceLP`/VDEnc.
/// Intel Gen12+/Arc only expose VDEnc; AMD radeonsi is the reverse. Caching skips the
/// known-failing open (and its libav spew).
///
/// Key is **(render node, codec, bit depth)** — all load-bearing. The node is what
/// `render_node()` hands libva (not `pf_gpu::selection_key()`); a codec-only latch would pin
/// Intel `low_power=1` onto a later AMD open with no retry. Depth is separate because Main10
/// and 8-bit can resolve to different entrypoints on one device.
static LP_MODE: OnceLock<Mutex<HashMap<LpKey, u8>>> = OnceLock::new();

type LpKey = (String, &'static str, bool);

/// Re-reads `render_node()` so a GPU-preference change is picked up on the next open.
fn lp_key(codec: Codec, ten_bit: bool) -> LpKey {
    lp_key_for(&render_node().to_string_lossy(), codec, ten_bit)
}

/// [`lp_key`] with the node explicit (tests).
fn lp_key_for(node: &str, codec: Codec, ten_bit: bool) -> LpKey {
    (node.to_owned(), codec.label(), ten_bit)
}

/// `PUNKTFUNK_VAAPI_LOW_POWER`: `1` = low-power only, `0` = full-feature only; unset = ladder.
fn low_power_override() -> Option<bool> {
    parse_low_power(&std::env::var("PUNKTFUNK_VAAPI_LOW_POWER").ok()?)
}

/// Anything outside the two literal sets is no pin — the ladder runs.
fn parse_low_power(raw: &str) -> Option<bool> {
    match raw.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Modes to try (`false` = EncSlice, `true` = VDEnc). A pin or a [`LP_MODE`] hit is one
/// attempt; otherwise full-feature first, then low-power. Never empty.
fn entrypoint_ladder(pin: Option<bool>, cached: u8) -> &'static [bool] {
    match pin {
        Some(true) => &[true],
        Some(false) => &[false],
        None => match cached {
            1 => &[false],
            2 => &[true],
            _ => &[false, true],
        },
    }
}

fn latched_mode(low_power: bool) -> u8 {
    if low_power {
        2
    } else {
        1
    }
}

struct Vui {
    colorspace: ffi::AVColorSpace,
    range: ffi::AVColorRange,
    primaries: ffi::AVColorPrimaries,
    trc: ffi::AVColorTransferCharacteristic,
}

/// VUI for the session depth. 10-bit: BT.2020 + PQ limited, matching the P010 CSC. SDR: BT.709
/// limited — an unspecified VUI lets the decoder treat full-range and wash out.
fn vui_for(ten_bit: bool) -> Vui {
    if ten_bit {
        Vui {
            colorspace: ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL,
            range: ffi::AVColorRange::AVCOL_RANGE_MPEG,
            primaries: ffi::AVColorPrimaries::AVCOL_PRI_BT2020,
            trc: ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084,
        }
    } else {
        Vui {
            colorspace: ffi::AVColorSpace::AVCOL_SPC_BT709,
            range: ffi::AVColorRange::AVCOL_RANGE_MPEG,
            primaries: ffi::AVColorPrimaries::AVCOL_PRI_BT709,
            trc: ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709,
        }
    }
}

/// Pin HEVC Main10 so depth cannot drop silently; 10-bit AV1 has no profile knob.
fn explicit_profile(codec: Codec, ten_bit: bool) -> Option<&'static str> {
    (ten_bit && codec == Codec::H265).then_some("main10")
}

/// `PUNKTFUNK_VAAPI_ASYNC_DEPTH`: 1..=8 verbatim; anything else is 1 (lowest latency).
fn async_depth(raw: Option<&str>) -> u32 {
    raw.and_then(|s| s.parse::<u32>().ok())
        .filter(|d| (1..=8).contains(d))
        .unwrap_or(1)
}

/// `scale_vaapi` output colour, pinned to [`vui_for`]. Unspecified output is Mesa BT.601 — a
/// hue shift against the signaled VUI. PQ is per-channel and rides the matrix untouched.
fn scale_vaapi_args(ten_bit: bool) -> &'static CStr {
    if ten_bit {
        c"format=p010:out_color_matrix=bt2020nc:out_range=limited"
    } else {
        c"format=nv12:out_color_matrix=bt709:out_range=limited"
    }
}

/// Open-time depth vs captured pixels. 10-bit rides on the pixels, not the negotiated depth —
/// see [`crate::ten_bit_input`] for the reverse-shape failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DepthResolution {
    /// PQ frames on a non-10-bit session: refuse rather than mislabel BT.709.
    RefuseMislabeledPq,
    /// 10-bit negotiated, capture stayed SDR: encode 8-bit (with a warning).
    SdrDowngrade,
    Agreed,
}

fn resolve_depth(format: PixelFormat, bit_depth: u8) -> DepthResolution {
    if format.is_hdr_rgb10() && bit_depth != 10 {
        DepthResolution::RefuseMislabeledPq
    } else if bit_depth == 10 && !format.is_hdr_rgb10() {
        DepthResolution::SdrDowngrade
    } else {
        DepthResolution::Agreed
    }
}

/// HEVC/AV1 only. PyroWave answers 10-bit on its own path; skip the VAAPI device open.
fn ten_bit_probe_eligible(codec: Codec) -> bool {
    codec.supports_10bit() && codec != Codec::PyroWave
}

/// Open the encoder, trying full-feature then `low_power=1`. Intel Gen12+/Arc is VDEnc-only;
/// AMD's first-try EncSlice stays unchanged. Mode caches per [`LP_MODE`]; env pins it.
/// Safety: borrowed `device_ref`/`frames_ref` — see [`open_vaapi_encoder_mode`].
#[allow(clippy::too_many_arguments)]
unsafe fn open_vaapi_encoder(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    ten_bit: bool,
) -> Result<encoder::video::Encoder> {
    let key = lp_key(codec, ten_bit);
    let cached = LP_MODE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map(|m| m.get(&key).copied().unwrap_or(0))
        .unwrap_or(0);
    let modes: &[bool] = entrypoint_ladder(low_power_override(), cached);
    let mut first_err = None;
    for &lp in modes {
        // SAFETY: `device_ref`/`frames_ref` are this fn's borrowed `AVBufferRef`s. The callee
        // only `av_buffer_ref`s them, so a retry reuses the still-owned buffers.
        let attempt = unsafe {
            open_vaapi_encoder_mode(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                device_ref,
                frames_ref,
                ten_bit,
                lp,
            )
        };
        match attempt {
            Ok(enc) => {
                if let Ok(mut m) = LP_MODE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
                    m.insert(key.clone(), latched_mode(lp));
                }
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
    // `modes` is never empty ([`entrypoint_ladder`]); the first error is the informative one.
    Err(first_err.unwrap())
}

/// Shared encoder context for both inner paths. `device_ref`/`frames_ref` are borrowed
/// (`av_buffer_ref`'d into the context), not consumed.
#[allow(clippy::too_many_arguments)]
unsafe fn open_vaapi_encoder_mode(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
    ten_bit: bool,
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
    video.set_format(if ten_bit { Pixel::P010LE } else { Pixel::NV12 });
    apply_low_latency_rc(&mut video, fps, bitrate_bps);
    // SAFETY: `as_mut_ptr` is the `AVCodecContext` behind `video`, which outlives these writes
    // (moved into the return). Colour/gop/pix_fmt stores are in-bounds scalars. Each
    // `av_buffer_ref` returns a NEW ref the context unrefs on free — the caller's buffers stay
    // owned by the caller.
    unsafe {
        let raw = video.as_mut_ptr();
        (*raw).gop_size = i32::MAX; // no periodic IDR (forced-IDR via pict_type=I on RFI)
        let vui = vui_for(ten_bit);
        (*raw).colorspace = vui.colorspace;
        (*raw).color_range = vui.range;
        (*raw).color_primaries = vui.primaries;
        (*raw).color_trc = vui.trc;
        (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*raw).hw_device_ctx = ffi::av_buffer_ref(device_ref);
        (*raw).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
    }

    let mut opts = Dictionary::new();
    if let Some(profile) = explicit_profile(codec, ten_bit) {
        opts.set("profile", profile);
    }
    // async_depth=1: `send_frame` blocks until this frame's encode completes. Depth ≥ 2 only
    // emits packet N once N+1 is queued — a structural +1-frame delay no poll beats. Raise it
    // when the ASIC cannot serialise the pixel rate (the env knob). The block tracks GPU
    // clocks; a paced 60 fps trickle downclocks VCN — see `gpuclocks`.
    let depth = async_depth(std::env::var("PUNKTFUNK_VAAPI_ASYNC_DEPTH").ok().as_deref());
    opts.set("async_depth", &depth.to_string());
    if low_power {
        opts.set("low_power", "1"); // VDEnc — the only encode entrypoint on modern Intel
    }
    video.open_with(opts).with_context(|| {
        format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps, low_power={low_power})")
    })
}

/// Tiny open/close: the driver rejects codecs the video engine cannot do. Feeds GameStream
/// advertisement so a client never negotiates an unencodable codec.
pub fn probe_can_encode(codec: Codec) -> bool {
    if ffmpeg::init().is_err() {
        return false;
    }
    // Missing VA device is an expected probe outcome. Held for the whole fn so the log
    // level restores either way. Shares the lock with the NVENC probes ([`crate::linux::QuietLibavLog`]).
    let _quiet = crate::linux::QuietLibavLog::new();
    // SAFETY: `ffmpeg::init()` returned Ok. `VaapiHw::new` builds a VAAPI device + NV12 pool
    // (640×480, pool=2) and unrefs both on drop. `open_vaapi_encoder` borrows the two non-null
    // refs; `hw` is a live local for the match arm, so the borrows outlive the call. Both drop
    // when the arm ends.
    unsafe {
        match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, 640, 480, 2) {
            Ok(hw) => open_vaapi_encoder(
                codec,
                640,
                480,
                30,
                2_000_000,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                false,
            )
            .is_ok(),
            Err(_) => false,
        }
    }
}

/// Tiny P010 + Main10 + PQ VUI open — the live HDR shape. Cached by [`crate::can_encode_10bit`].
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !ten_bit_probe_eligible(codec) {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // Missing VA / no Main10 is expected. Same quiet-log contract as [`probe_can_encode`].
    let _quiet = crate::linux::QuietLibavLog::new();
    // SAFETY: `ffmpeg::init()` returned Ok. `VaapiHw::new` builds a P010 pool; `open_vaapi_encoder`
    // borrows the two non-null refs for the match arm. Both drop at arm end.
    unsafe {
        match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_P010LE, 640, 480, 2) {
            Ok(hw) => open_vaapi_encoder(
                codec,
                640,
                480,
                30,
                2_000_000,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                true,
            )
            .is_ok(),
            Err(_) => false,
        }
    }
}

/// Always `false`. No validated HEVC 4:4:4 encode entrypoint, so a VAAPI host advertises 4:2:0
/// and the client never builds a 4:4:4 decoder for 4:2:0 frames.
pub fn probe_can_encode_444(_codec: Codec) -> bool {
    tracing::info!("VAAPI HEVC 4:4:4 encode is not implemented yet — declining (encoding 4:2:0)");
    false
}

struct VaapiHw {
    // frames-BEFORE-device: drop order matches the frames ctx holding a ref on the device.
    // Do not reorder.
    frames_ref: AvBuffer,
    device_ref: AvBuffer,
}

impl VaapiHw {
    /// Opens the device from scalars — no caller `CUcontext` contract, unlike [`super::CudaHw::new`].
    /// The `unsafe` below is libav FFI, not an obligation on the caller.
    fn new(sw_format: ffi::AVPixelFormat, w: u32, h: u32, pool: c_int) -> Result<Self> {
        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        let node = render_node();
        // SAFETY: `device_ref` is a live local out-param; `node` is a NUL-terminated `CString` that
        // outlives the call, and the remaining arguments are the documented "no options" pair. On
        // success libav writes ONE owned reference into `device_ref`.
        let r = unsafe {
            ffi::av_hwdevice_ctx_create(
                &mut device_ref,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            )
        };
        if r < 0 {
            bail!("no VAAPI device ({:?}): {}", node, ffmpeg::Error::from(r));
        }
        // Take ownership now so every later `bail!` drops the device (and frames, once built).
        // SAFETY: `r >= 0`, so `device_ref` is that owned reference and this is its only owner.
        let device_ref = unsafe { AvBuffer::from_raw(device_ref) }
            .context("av_hwdevice_ctx_create(VAAPI) gave no device")?;
        // SAFETY: `av_hwframe_ctx_alloc` is handed the live device ref and returns null (rejected
        // by `from_raw`, so `?` leaves before the writes) or a ref whose `data` is already an
        // `AVHWFramesContext`. Stores are in-bounds scalars, done before `av_hwframe_ctx_init`.
        let frames_ref = unsafe {
            let frames_ref = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(device_ref.as_ptr()))
                .context("av_hwframe_ctx_alloc(VAAPI) failed")?;
            let fc = (*frames_ref.as_ptr()).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*fc).sw_format = sw_format;
            (*fc).width = w as c_int;
            (*fc).height = h as c_int;
            (*fc).initial_pool_size = pool;
            let r = ffi::av_hwframe_ctx_init(frames_ref.as_ptr());
            if r < 0 {
                bail!("av_hwframe_ctx_init(VAAPI) failed ({r})");
            }
            frames_ref
        };
        Ok(VaapiHw {
            frames_ref,
            device_ref,
        })
    }
}

// No `Drop`: each `AvBuffer` unrefs itself, frames then device (see field comment).

struct CpuInner {
    enc: encoder::video::Encoder,
    hw: VaapiHw,
    // nv12 before sws: drop order is declaration order (`repr(Rust)` layout is not). Do not
    // reorder — `offset_of` cannot pin this.
    nv12: AvFrame,
    sws: AvSwsContext,
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
        let ten_bit = format.is_hdr_rgb10();
        let staging_av = if ten_bit {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        const POOL: c_int = 16;
        let hw = VaapiHw::new(staging_av, width, height, POOL)?;
        // SAFETY: `open_vaapi_encoder` borrows `hw.device_ref`/`hw.frames_ref` — both non-null
        // (`VaapiHw::new`) and live for this call. It `av_buffer_ref`s them into the encoder;
        // `hw` is then moved into `CpuInner` next to `enc`, so the device outlives the encoder.
        let enc = unsafe {
            open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                hw.device_ref.as_ptr(),
                hw.frames_ref.as_ptr(),
                ten_bit,
            )?
        };
        let src_av = pixel_to_av(src_pixel);
        // SAFETY: `sws_getContext` for the encoder's positive `width`/`height`. `src_av` is a
        // valid `AVPixelFormat` (`vaapi_sws_src` then `pixel_to_av`); dst is NV12/P010. Trailing
        // srcFilter/dstFilter/param are null = documented defaults. No Rust memory is borrowed;
        // ownership of the returned context passes to `AvSwsContext` (null rejected by `from_raw`).
        let sws = unsafe {
            AvSwsContext::from_raw(ffi::sws_getContext(
                width as c_int,
                height as c_int,
                src_av,
                width as c_int,
                height as c_int,
                staging_av,
                SWS_POINT,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            ))
        };
        let Some(sws) = sws else {
            bail!(
                "sws_getContext(RGB→{})",
                if ten_bit { "P010" } else { "NV12" }
            );
        };
        // SAFETY: `sws` is the owned context from above. `sws_getCoefficients` returns a
        // process-lifetime static table (ITU-709 or BT.2020 NCL, matching the VUI), reused for
        // both inverse and forward matrices. `sws_setColorspaceDetails` only reads it.
        unsafe {
            let cs = ffi::sws_getCoefficients(if ten_bit {
                super::libav::SWS_CS_BT2020
            } else {
                SWS_CS_ITU709
            });
            ffi::sws_setColorspaceDetails(sws.as_ptr(), cs, 1, cs, 0, 0, 1 << 16, 1 << 16);
        }
        let nv12 = AvFrame::alloc().context("av_frame_alloc(staging) failed")?;
        // SAFETY: `format`/`width`/`height` writes stay inside the owned frame. `av_frame_get_buffer`
        // allocates backing; on failure `nv12` and `sws` drop.
        unsafe {
            (*nv12.as_ptr()).format = staging_av as c_int;
            (*nv12.as_ptr()).width = width as c_int;
            (*nv12.as_ptr()).height = height as c_int;
            if ffi::av_frame_get_buffer(nv12.as_ptr(), 0) < 0 {
                bail!("av_frame_get_buffer(staging) failed");
            }
        }
        tracing::info!(
            encoder = codec.vaapi_name(),
            "VAAPI encode active ({width}x{height}@{fps}, CPU→{} upload path)",
            if ten_bit { "P010 (HDR10)" } else { "NV12" }
        );
        Ok(CpuInner {
            enc,
            hw,
            nv12,
            sws,
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
        // SAFETY: the `ensure!`s cover format and `bytes.len() >= src_row * h`. `sws_scale` reads
        // `h` packed-RGB rows from `bytes`; `self.sws` writes `self.nv12` (buffer-sized in `open`).
        // `hwf` is owned — every exit drops it once. `av_hwframe_get_buffer` pulls from live
        // `self.hw.frames_ref`; `av_hwframe_transfer_data` uploads; `avcodec_send_frame` takes its
        // own ref. Encoder is this thread only (`unsafe impl Send`).
        unsafe {
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                self.sws.as_ptr(),
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.nv12.as_ptr()).data.as_ptr(),
                (*self.nv12.as_ptr()).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            let hwf = AvFrame::alloc().context("av_frame_alloc(hw) failed")?;
            if ffi::av_hwframe_get_buffer(self.hw.frames_ref.as_ptr(), hwf.as_ptr(), 0) < 0 {
                bail!("av_hwframe_get_buffer(VAAPI) failed");
            }
            if ffi::av_hwframe_transfer_data(hwf.as_ptr(), self.nv12.as_ptr(), 0) < 0 {
                bail!("av_hwframe_transfer_data(→VAAPI) failed");
            }
            (*hwf.as_ptr()).pts = pts;
            (*hwf.as_ptr()).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), hwf.as_ptr());
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        Ok(())
    }
}

// No `Drop`: `nv12` then `sws` free themselves (see field order). The encoder holds its own
// `av_buffer_ref`'d copies, so `enc`/`hw` order is not a soundness issue.

struct DmabufInner {
    // Drop order is declaration order: graph, frames, derived VAAPI, DRM, then `enc` last.
    // Each holds its own ref so any order is sound; do not silently change what ships.
    /// Owner-only: `src`/`sink` are borrowed ctxs the graph owns. Removing the field would
    /// free the graph while they still point into it.
    #[allow(dead_code)]
    graph: AvFilterGraph,
    /// DRM-PRIME frames ctx; `submit` tags each imported `AVFrame` with a new ref of it.
    drm_frames: AvBuffer,
    /// Owner-only: hwmap, scale_vaapi, and the encoder each took their own ref at open.
    #[allow(dead_code)]
    vaapi_device: AvBuffer,
    /// Owner-only: `drm_frames` holds its own ref on this DRM device.
    #[allow(dead_code)]
    drm_device: AvBuffer,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    width: u32,
    height: u32,
    fourcc: u32,
    /// Submit count for the sampled `PUNKTFUNK_PERF` split (one line per ~2 s at 60 fps).
    frames: u64,
    /// Last so it drops after the graph and the three buffers.
    enc: encoder::video::Encoder,
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
        let drm_fourcc = pf_frame::drm_fourcc(format)
            .ok_or_else(|| anyhow!("no DRM fourcc for {format:?} (VAAPI zero-copy)"))?;
        let ten_bit = format.is_hdr_rgb10();
        let sw_format = match format {
            PixelFormat::X2Rgb10 => ffi::AVPixelFormat::AV_PIX_FMT_X2RGB10LE,
            PixelFormat::X2Bgr10 => ffi::AVPixelFormat::AV_PIX_FMT_X2BGR10LE,
            // The 8-bit capture formats are all XR24-shaped packed RGB (the historical BGR0 view).
            _ => ffi::AVPixelFormat::AV_PIX_FMT_BGR0,
        };
        let node = render_node();
        // SAFETY: libav is initialized (`VaapiEncoder::open` ran `ffmpeg::init()`). Every raw
        // pointer below is a just-allocated, null-checked ffmpeg object or an in-struct field of one:
        //  * `node` is a live `CString`; `.as_ptr()` is read only during `av_hwdevice_ctx_create`.
        //  * device creates: `r < 0` leaves the out-param null and we bail; success is one owned ref.
        //  * `av_hwframe_ctx_alloc` → `drm_frames` (null-checked); `data` is the `AVHWFramesContext`,
        //    written before `av_hwframe_ctx_init`.
        //  * `avfilter_graph_alloc` / `avfilter_get_by_name` (static or null) /
        //    `avfilter_graph_alloc_filter` inside `graph`; the four ctxs are null-checked together.
        //  * `av_buffer_ref(vaapi_device)` on hwmap/scale is a NEW ref the graph frees; ours is untouched.
        //  * `av_buffersink_get_hw_frames_ctx` is borrowed from the sink, valid while `graph` lives.
        //  * `open_vaapi_encoder` borrows `vaapi_device` and `nv12_ctx` and `av_buffer_ref`s both.
        // Early `bail!` drops whatever `AvBuffer`/`AvFilterGraph` have been built. On success they
        // move into `DmabufInner`.
        unsafe {
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
            // Own each handle as it exists: later `bail!` drops whatever has been built so far.
            let drm_device = AvBuffer::from_raw(drm_device)
                .context("av_hwdevice_ctx_create(DRM) gave no device")?;
            let mut vaapi_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create_derived(
                &mut vaapi_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                drm_device.as_ptr(),
                0,
            );
            if r < 0 {
                bail!("derive VAAPI from DRM: {}", ffmpeg::Error::from(r));
            }
            let vaapi_device = AvBuffer::from_raw(vaapi_device)
                .context("av_hwdevice_ctx_create_derived(VAAPI) gave no device")?;

            let drm_frames = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(drm_device.as_ptr()))
                .context("av_hwframe_ctx_alloc(DRM) failed")?;
            let fc = (*drm_frames.as_ptr()).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            (*fc).sw_format = sw_format; // packed XR24 RGB plane, or XR30/XB30 for HDR
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            if ffi::av_hwframe_ctx_init(drm_frames.as_ptr()) < 0 {
                bail!("av_hwframe_ctx_init(DRM) failed");
            }

            let graph = AvFilterGraph::alloc().context("avfilter_graph_alloc failed")?;

            let mk = |name: &CStr, inst: &CStr| -> *mut ffi::AVFilterContext {
                let f = ffi::avfilter_get_by_name(name.as_ptr());
                if f.is_null() {
                    return ptr::null_mut();
                }
                ffi::avfilter_graph_alloc_filter(graph.as_ptr(), f, inst.as_ptr())
            };
            let src = mk(c"buffer", c"in");
            let hwmap = mk(c"hwmap", c"map");
            let scale = mk(c"scale_vaapi", c"csc");
            let sink = mk(c"buffersink", c"out");
            if src.is_null() || hwmap.is_null() || scale.is_null() || sink.is_null() {
                bail!("a VAAPI filter (buffer/hwmap/scale_vaapi/buffersink) is missing");
            }
            // Bind both filters to this VAAPI device rather than `hwmap=derive_device`, so every
            // surface — and the sink frames ctx the encoder adopts — stays on one VADisplay.
            (*hwmap).hw_device_ctx = ffi::av_buffer_ref(vaapi_device.as_ptr());
            (*scale).hw_device_ctx = ffi::av_buffer_ref(vaapi_device.as_ptr());

            let par = ffi::av_buffersrc_parameters_alloc();
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*par).width = width as c_int;
            (*par).height = height as c_int;
            // Full-range RGB (compositor desktop) so per-frame tags in `submit` match the
            // negotiated link instead of reading as a mid-stream property change.
            (*par).color_space = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*par).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: fps as c_int,
            };
            // Borrowed (no extra ref): `av_buffersrc_parameters_set` takes its own ref of
            // `par->hw_frames_ctx`; `av_free(par)` frees only the struct. An extra `av_buffer_ref`
            // here leaks one ref per session.
            (*par).hw_frames_ctx = drm_frames.as_ptr();
            let r = ffi::av_buffersrc_parameters_set(src, par);
            ffi::av_free(par as *mut _);
            if r < 0 {
                bail!("av_buffersrc_parameters_set failed ({r})");
            }
            macro_rules! init {
                ($ctx:expr, $args:expr, $what:literal) => {{
                    let r = ffi::avfilter_init_str($ctx, $args);
                    if r < 0 {
                        bail!(concat!("init ", $what, " failed ({})"), r);
                    }
                }};
            }
            init!(src, ptr::null(), "buffer");
            init!(hwmap, c"mode=read".as_ptr(), "hwmap");
            init!(scale, scale_vaapi_args(ten_bit).as_ptr(), "scale_vaapi");
            init!(sink, ptr::null(), "buffersink");

            let link = |a: *mut ffi::AVFilterContext, b: *mut ffi::AVFilterContext| -> c_int {
                ffi::avfilter_link(a, 0, b, 0)
            };
            if link(src, hwmap) < 0 || link(hwmap, scale) < 0 || link(scale, sink) < 0 {
                bail!("avfilter_link failed");
            }
            let r = ffi::avfilter_graph_config(graph.as_ptr(), ptr::null_mut());
            if r < 0 {
                bail!("avfilter_graph_config failed ({r})");
            }

            let nv12_ctx = ffi::av_buffersink_get_hw_frames_ctx(sink);
            if nv12_ctx.is_null() {
                bail!("filter sink has no VAAPI frames context");
            }
            let enc = match open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                vaapi_device.as_ptr(),
                nv12_ctx,
                ten_bit,
            ) {
                Ok(enc) => enc,
                Err(e) => {
                    return Err(e);
                }
            };

            tracing::info!(
                encoder = codec.vaapi_name(),
                "VAAPI encode active ({width}x{height}@{fps}, zero-copy dmabuf → GPU {})",
                if ten_bit { "P010 (HDR10)" } else { "NV12" }
            );
            Ok(DmabufInner {
                graph,
                drm_frames,
                vaapi_device,
                drm_device,
                src,
                sink,
                width,
                height,
                fourcc: drm_fourcc,
                frames: 0,
                enc,
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
        // `PUNKTFUNK_PERF`: one sampled line per ~2 s (`frames % 120`). Push = desc+buffersrc,
        // pull = hwmap import + VPP CSC, send = `avcodec_send_frame`.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;
        let t0 = std::time::Instant::now();
        let t_push: std::time::Duration;
        let t_pull: std::time::Duration;
        // SAFETY: `dmabuf.fourcc == self.fourcc` (ensure above).
        //  * `zeroed::<AVDRMFrameDescriptor>()` is a `#[repr(C)]` POD of ints; all-zero is valid.
        //  * `dmabuf.fd` is owned by the caller's `&DmabufFrame` for this call; `lseek` only reads size.
        //  * `drm`/`nv12` are owned `AvFrame`s — every exit drops each once.
        //  * `data[0] = Box::into_raw(desc)` + `av_buffer_create(..., free_desc)` reclaims it once.
        //  * `av_buffersrc_add_frame_flags(..., KEEP_REF)` keeps our `drm` ref, dropped after the
        //    push. We pull from `self.sink` before return so the caller's dmabuf is still valid.
        //    `avcodec_send_frame` takes its own ref. Single-threaded encoder → no race.
        unsafe {
            let mut desc: Box<ffi::AVDRMFrameDescriptor> = Box::new(std::mem::zeroed());
            desc.nb_objects = 1;
            desc.objects[0].fd = dmabuf.fd.as_raw_fd();
            // Real object size, not 0. Both libav import paths pass this to libva as
            // `prime_desc.objects[i].size` / `buffer_desc.data_size`; 0 means "empty backing"
            // and `vaCreateSurfaces` fails on drivers that do not guess. `lseek(SEEK_END)` is
            // the dma-buf size query (`pf_zerocopy::imp::vulkan`). A refused lseek keeps 0.
            let obj_size = libc::lseek(dmabuf.fd.as_raw_fd(), 0, libc::SEEK_END);
            desc.objects[0].size = if obj_size > 0 { obj_size as _ } else { 0 };
            desc.objects[0].format_modifier = dmabuf.modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = self.fourcc;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = dmabuf.offset as isize;
            desc.layers[0].planes[0].pitch = dmabuf.stride as isize;

            let drm = AvFrame::alloc().context("av_frame_alloc(drm) failed")?;
            (*drm.as_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*drm.as_ptr()).width = self.width as c_int;
            (*drm.as_ptr()).height = self.height as c_int;
            // Full-range RGB desktop. Untagged input lets Mesa pick BT.601, against the
            // BT.709-limited VUI the encoder signals.
            (*drm.as_ptr()).color_range = ffi::AVColorRange::AVCOL_RANGE_JPEG;
            (*drm.as_ptr()).colorspace = ffi::AVColorSpace::AVCOL_SPC_RGB;
            (*drm.as_ptr()).hw_frames_ctx = ffi::av_buffer_ref(self.drm_frames.as_ptr());
            (*drm.as_ptr()).data[0] = Box::into_raw(desc) as *mut u8;
            // Descriptor frees with the frame. The fd stays with `DmabufFrame`, which outlives
            // this call — the graph reads the surface before submit returns.
            extern "C" fn free_desc(_opaque: *mut std::ffi::c_void, data: *mut u8) {
                // SAFETY: `data` is the `Box::into_raw(desc)` pointer passed to `av_buffer_create`,
                // handed back verbatim. Libav invokes this once when the last buffer ref drops, so
                // `from_raw` reclaims it once. `_opaque` is unused (we passed null).
                unsafe { drop(Box::from_raw(data as *mut ffi::AVDRMFrameDescriptor)) };
            }
            (*drm.as_ptr()).buf[0] = ffi::av_buffer_create(
                (*drm.as_ptr()).data[0],
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_desc),
                ptr::null_mut(),
                0,
            );

            let r = ffi::av_buffersrc_add_frame_flags(
                self.src,
                drm.as_ptr(),
                ffi::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
            );
            drop(drm); // KEEP_REF: drop our ref after the push so descriptor timing is unchanged.
                       // Import is this push + the pull below. Failure means this driver will not take
                       // this dmabuf — latch it; capture falls back to CPU next session. Do not count
                       // `avcodec_send_frame`: that stall is what the in-place rebuild recovers.
            if r < 0 {
                let e = format!("av_buffersrc_add_frame failed ({r})");
                pf_zerocopy::note_raw_dmabuf_import_failure(&e);
                bail!("{e}");
            }
            t_push = t0.elapsed();
            let nv12 = AvFrame::alloc().context("av_frame_alloc(nv12) failed")?;
            let r = ffi::av_buffersink_get_frame(self.sink, nv12.as_ptr());
            if r < 0 {
                let e = format!("av_buffersink_get_frame failed ({r})");
                pf_zerocopy::note_raw_dmabuf_import_failure(&e);
                bail!("{e}");
            }
            pf_zerocopy::note_raw_dmabuf_import_ok();
            t_pull = t0.elapsed() - t_push;
            (*nv12.as_ptr()).pts = pts;
            (*nv12.as_ptr()).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), nv12.as_ptr());
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

// No `Drop`: field order is graph, frames, derived VAAPI, DRM, then `enc`.

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
    /// First-frame payload picks CPU upload vs zero-copy dmabuf.
    inner: Option<Inner>,
    frame_idx: i64,
    force_kf: bool,
    /// Submitted frames not yet returned as packets. [`poll`](Encoder::poll) waits only when
    /// `async_depth > 1` and something is actually in flight.
    in_flight: u32,
}

// SAFETY: `Inner` holds raw FFI pointers that are not `Send` by default. The encoder is owned
// by one thread (the session encode thread it is moved to) and only touched via `&mut self`,
// so never aliased. The libav objects have no thread affinity. `Send` only; `Sync` is
// deliberately not implemented.
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
        match resolve_depth(format, bit_depth) {
            DepthResolution::RefuseMislabeledPq => bail!(
                "captured 10-bit HDR frames ({format:?}) on an {bit_depth}-bit VAAPI session — \
                 refusing to mislabel PQ content"
            ),
            DepthResolution::SdrDowngrade => tracing::warn!(
                bit_depth,
                ?format,
                "10-bit requested but the capture stayed SDR — encoding 8-bit"
            ),
            DepthResolution::Agreed => {}
        }
        // 4:4:4 is unimplemented ([`probe_can_encode_444`] is false). If a request slips
        // through, encode 4:2:0 — the Welcome already advertised 4:2:0.
        if chroma.is_444() {
            tracing::warn!("VAAPI 4:4:4 encode not implemented — encoding 4:2:0");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `av_log_set_level` is a global integer; `48` = `AV_LOG_DEBUG`. No pointer
            // args. libav was just initialized by `ffmpeg::init()` above.
            unsafe { ffi::av_log_set_level(48) };
        }
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

    /// Drop the wedged encoder; the next `submit` rebuilds it from the first frame. Zero
    /// `in_flight` (owed AUs are gone) and force IDR so the client resyncs. Without this the
    /// encode-stall watchdog had no Linux AMD/Intel lever.
    fn reset(&mut self) -> bool {
        self.inner = None;
        self.in_flight = 0;
        self.force_kf = true;
        true
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // `async_depth > 1`: the AU lands ~one ASIC-time later. Wait up to 3/4 of a frame
        // interval (capped 12 ms) so it still ships this tick; on expiry it rides the next poll.
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
                PollOutcome::Again | PollOutcome::Eof => {}
            }
            // Wait only while a frame is in flight; ~250 µs between ASIC checks.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct/drop `DmabufInner` on real VAAPI silicon. `open` owns four objects; looping
    /// is what distinguishes a double-free (glibc abort) from a missed unref (leak per iter).
    /// The CPU smoke test never builds this graph.
    ///
    /// `cargo test -p pf-encode dmabuf_inner_alloc_drop_cycles -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn dmabuf_inner_alloc_drop_cycles() {
        for i in 0..8 {
            let inner = DmabufInner::open(Codec::H264, PixelFormat::Bgrx, 640, 480, 30, 8_000_000)
                .unwrap_or_else(|e| panic!("DmabufInner::open failed on iteration {i}: {e:#}"));
            assert!(!inner.graph.as_ptr().is_null(), "graph went null");
            assert!(!inner.drm_frames.as_ptr().is_null(), "drm_frames went null");
            assert!(
                !inner.vaapi_device.as_ptr().is_null(),
                "vaapi_device went null"
            );
            assert!(!inner.drm_device.as_ptr().is_null(), "drm_device went null");
        }
        eprintln!("8 DmabufInner alloc/drop cycles completed without abort");
    }

    #[test]
    fn entrypoint_ladder_orders_and_pins() {
        assert_eq!(entrypoint_ladder(None, 0), &[false, true]);
        assert_eq!(entrypoint_ladder(None, 1), &[false]);
        assert_eq!(entrypoint_ladder(None, 2), &[true]);
        // Unknown cache → full ladder, never a wrong pin.
        assert_eq!(entrypoint_ladder(None, 77), &[false, true]);
        // Pin beats cache in both directions — the env escape from a stale latch.
        for cached in [0u8, 1, 2, 77] {
            assert_eq!(entrypoint_ladder(Some(true), cached), &[true]);
            assert_eq!(entrypoint_ladder(Some(false), cached), &[false]);
        }
    }

    #[test]
    fn latch_round_trip_pins_the_resolved_mode() {
        for lp in [false, true] {
            assert_eq!(entrypoint_ladder(None, latched_mode(lp)), &[lp]);
        }
    }

    #[test]
    fn low_power_grammar() {
        for s in ["1", "true", "yes", "on", " on ", "yes\n"] {
            assert_eq!(parse_low_power(s), Some(true), "{s:?}");
        }
        for s in ["0", "false", "no", "off", " off "] {
            assert_eq!(parse_low_power(s), Some(false), "{s:?}");
        }
        for s in ["", "2", "TRUE", "On", "enabled", "low_power"] {
            assert_eq!(parse_low_power(s), None, "{s:?}");
        }
    }

    #[test]
    fn lp_key_separates_node_codec_and_depth() {
        let base = lp_key_for("/dev/dri/renderD128", Codec::H265, false);
        assert_ne!(base, lp_key_for("/dev/dri/renderD129", Codec::H265, false));
        assert_ne!(base, lp_key_for("/dev/dri/renderD128", Codec::H264, false));
        assert_ne!(base, lp_key_for("/dev/dri/renderD128", Codec::H265, true));
    }

    #[test]
    fn async_depth_grammar() {
        assert_eq!(async_depth(None), 1);
        assert_eq!(async_depth(Some("1")), 1);
        assert_eq!(async_depth(Some("2")), 2);
        assert_eq!(async_depth(Some("8")), 8);
        assert_eq!(async_depth(Some("0")), 1);
        assert_eq!(async_depth(Some("9")), 1);
        assert_eq!(async_depth(Some("-1")), 1);
        assert_eq!(async_depth(Some("fast")), 1);
    }

    #[test]
    fn sws_src_accepts_packed_rgb_only() {
        assert_eq!(vaapi_sws_src(PixelFormat::Bgrx).unwrap(), Pixel::BGRZ);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgbx).unwrap(), Pixel::RGBZ);
        assert_eq!(vaapi_sws_src(PixelFormat::Bgra).unwrap(), Pixel::BGRA);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgba).unwrap(), Pixel::RGBA);
        assert_eq!(vaapi_sws_src(PixelFormat::Rgb).unwrap(), Pixel::RGB24);
        assert_eq!(vaapi_sws_src(PixelFormat::Bgr).unwrap(), Pixel::BGR24);
        assert_eq!(
            vaapi_sws_src(PixelFormat::X2Rgb10).unwrap(),
            Pixel::X2RGB10LE
        );
        assert_eq!(
            vaapi_sws_src(PixelFormat::X2Bgr10).unwrap(),
            Pixel::X2BGR10LE
        );
        for f in [
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Rgb10a2,
            PixelFormat::Yuv444,
        ] {
            assert!(vaapi_sws_src(f).is_err(), "{f:?} must be refused");
        }
    }

    /// Signaled VUI and `scale_vaapi` output must name the same matrix/range, or Mesa BT.601
    /// shifts hue against the VUI.
    #[test]
    fn vui_and_scale_args_agree_per_depth() {
        let sdr = vui_for(false);
        assert!(matches!(sdr.colorspace, ffi::AVColorSpace::AVCOL_SPC_BT709));
        assert!(matches!(sdr.range, ffi::AVColorRange::AVCOL_RANGE_MPEG));
        assert!(matches!(
            sdr.primaries,
            ffi::AVColorPrimaries::AVCOL_PRI_BT709
        ));
        assert!(matches!(
            sdr.trc,
            ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709
        ));
        let args = scale_vaapi_args(false).to_str().unwrap();
        for needle in ["format=nv12", "out_color_matrix=bt709", "out_range=limited"] {
            assert!(
                args.contains(needle),
                "SDR scale args miss {needle}: {args}"
            );
        }

        let hdr = vui_for(true);
        assert!(matches!(
            hdr.colorspace,
            ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL
        ));
        assert!(matches!(hdr.range, ffi::AVColorRange::AVCOL_RANGE_MPEG));
        assert!(matches!(
            hdr.primaries,
            ffi::AVColorPrimaries::AVCOL_PRI_BT2020
        ));
        assert!(matches!(
            hdr.trc,
            ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
        ));
        let args = scale_vaapi_args(true).to_str().unwrap();
        for needle in [
            "format=p010",
            "out_color_matrix=bt2020nc",
            "out_range=limited",
        ] {
            assert!(
                args.contains(needle),
                "HDR scale args miss {needle}: {args}"
            );
        }
    }

    #[test]
    fn depth_resolution_table() {
        use DepthResolution::*;
        assert_eq!(resolve_depth(PixelFormat::X2Rgb10, 8), RefuseMislabeledPq);
        assert_eq!(resolve_depth(PixelFormat::X2Bgr10, 8), RefuseMislabeledPq);
        assert_eq!(resolve_depth(PixelFormat::Bgrx, 10), SdrDowngrade);
        assert_eq!(resolve_depth(PixelFormat::Bgrx, 8), Agreed);
        assert_eq!(resolve_depth(PixelFormat::X2Rgb10, 10), Agreed);
        assert_eq!(resolve_depth(PixelFormat::X2Bgr10, 10), Agreed);
    }

    #[test]
    fn explicit_profile_is_hevc_main10_only() {
        assert_eq!(explicit_profile(Codec::H265, true), Some("main10"));
        assert_eq!(explicit_profile(Codec::H265, false), None);
        assert_eq!(explicit_profile(Codec::Av1, true), None);
        assert_eq!(explicit_profile(Codec::Av1, false), None);
        assert_eq!(explicit_profile(Codec::H264, true), None);
        assert_eq!(explicit_profile(Codec::H264, false), None);
    }

    #[test]
    fn ten_bit_probe_gate() {
        assert!(ten_bit_probe_eligible(Codec::H265));
        assert!(ten_bit_probe_eligible(Codec::Av1));
        assert!(!ten_bit_probe_eligible(Codec::H264));
        assert!(!ten_bit_probe_eligible(Codec::PyroWave));
    }

    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn vaapi_probe_smoke() {
        assert!(
            probe_can_encode(Codec::H264),
            "H.264 VAAPI encode should open on any supported AMD/Intel GPU"
        );
        for codec in [Codec::H265, Codec::Av1] {
            eprintln!("probe_can_encode({codec:?}) = {}", probe_can_encode(codec));
            eprintln!(
                "probe_can_encode_10bit({codec:?}) = {}",
                probe_can_encode_10bit(codec)
            );
        }
    }

    #[test]
    #[ignore = "needs a real VAAPI device (run on an AMD/Intel host, not the build box)"]
    fn vaapi_cpu_encode_smoke() {
        let (w, h) = (256u32, 256u32);
        let mut enc = VaapiEncoder::open(
            Codec::H264,
            PixelFormat::Bgrx,
            w,
            h,
            30,
            2_000_000,
            8,
            crate::ChromaFormat::Yuv420,
        )
        .expect("open");
        let mut aus = Vec::new();
        for i in 0..30u32 {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&[(i * 8) as u8, 0x40, 0xC0, 0xFF]);
            }
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: u64::from(i) * 33_333_333,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(buf),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert!(!aus.is_empty(), "no AUs out of 30 submitted frames");
        assert!(aus[0].keyframe, "the first AU must be the IDR");
    }
}
