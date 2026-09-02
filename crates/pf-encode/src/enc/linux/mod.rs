//! NVENC via `ffmpeg-next` (system FFmpeg; `ffmpeg-sys-next` emits a per-version cfg). One tree
//! spans FFmpeg 7.x/libavcodec 61 through 9.x/63. The crate MAJOR is a ceiling: 8.x refused
//! libavcodec 63, so FFmpeg 9 needs a crate bump, not a rebuild. The soname bound is generated
//! at package time — see packaging/arch/PKGBUILD.
//!
//! Packed RGB/BGR CPU input; `*_nvenc` accepts `rgb0`/`bgr0`/`rgba`/`bgra` and does RGB→YUV on
//! the GPU. Packed 24-bit `RGB` is expanded to `rgb0` (one padding byte, no colour math).
//! Opened without a global header so VPS/SPS/PPS ride in-band on every IDR — raw Annex-B and
//! self-contained AUs.

use super::{ChromaFormat, Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video as VideoFrame;
use ffmpeg::{codec, encoder, Dictionary};
use ffmpeg_next as ffmpeg;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::os::raw::c_int;
use std::ptr;

use super::libav::{
    apply_low_latency_rc, pixel_to_av, poll_encoder, AvBuffer, AvFrame, AvSwsContext, PollOutcome,
    SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi; // = ffmpeg_sys_next

/// Captured packed RGB/BGR as swscale source (real byte order, not NVENC-padded `*0`).
/// CPU CSC only: RGB→YUV444P and X2RGB10/X2BGR10→P010. YUV inputs cannot feed this path.
fn sws_src_pixel(format: PixelFormat) -> Result<Pixel> {
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
            bail!("NVENC CPU-input conversion supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Mirror of libav's `AVCUDADeviceContext` (hwcontext_cuda.h) — ffmpeg-sys does not bind it.
/// Three-pointer layout; `cuda_ctx` is our importer `CUcontext` so NVENC shares that context.
#[repr(C)]
struct AVCUDADeviceContext {
    cuda_ctx: *mut std::ffi::c_void, // CUcontext
    stream: *mut std::ffi::c_void,   // CUstream (null = default)
    internal: *mut std::ffi::c_void, // filled by ctx_init
}

// `CudaHw::new` writes `cuda_ctx` through this mirror. A wrong offset compiles and does not
// crash; it scribbles libav's internal pointer. These asserts fail the build on a field reorder.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AVCUDADeviceContext>() == 3 * size_of::<*mut std::ffi::c_void>());
    assert!(offset_of!(AVCUDADeviceContext, cuda_ctx) == 0);
    assert!(offset_of!(AVCUDADeviceContext, stream) == size_of::<*mut std::ffi::c_void>());
    assert!(offset_of!(AVCUDADeviceContext, internal) == 2 * size_of::<*mut std::ffi::c_void>());
};

/// CUDA hwframes wrapping our shared `CUcontext`, so `hevc_nvenc` reads the imported buffer.
/// Owns two `AVBufferRef`s, unref'd on drop.
struct CudaHw {
    // frames before device: drop order follows declaration; the frames ctx holds a ref on the
    // device. Do not reorder.
    frames_ref: AvBuffer,
    device_ref: AvBuffer,
}

impl CudaHw {
    /// CUDA hwdevice wrapping `cu_ctx` plus a frames pool (`sw_format` = `pixel`).
    ///
    /// `bail!` formats raw AVERROR ints on purpose: `open_nvenc_probed` treats typed EINVAL as a
    /// bitrate step. A hwdevice EINVAL is not bitrate-related; typing it burns ~10 doomed opens.
    unsafe fn new(cu_ctx: *mut std::ffi::c_void, sw_format: Pixel, w: u32, h: u32) -> Result<Self> {
        // Each `?`/`bail!` below drops whatever has been built so far — `AvBuffer`'s `Drop` is the
        // single unref path, so the failure branches carry no cleanup of their own.

        // SAFETY: `av_hwdevice_ctx_alloc` returns null (`AvBuffer::from_raw` rejects, `?` returns)
        // or a fresh ref whose `data` is an initialized `AVHWDeviceContext`. For CUDA, `hwctx` is
        // our `AVCUDADeviceContext` mirror, so writing `cuda_ctx` is in-bounds on a live allocation;
        // `cu_ctx` is a valid `CUcontext` by this fn's contract. `av_hwdevice_ctx_init` takes the
        // same live ref and must see `cuda_ctx` already set.
        let device_ref = unsafe {
            let device_ref = AvBuffer::from_raw(ffi::av_hwdevice_ctx_alloc(
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            ))
            .context("av_hwdevice_ctx_alloc(CUDA) failed")?;
            let dev_ctx = (*device_ref.as_ptr()).data as *mut ffi::AVHWDeviceContext;
            let cu = (*dev_ctx).hwctx as *mut AVCUDADeviceContext;
            (*cu).cuda_ctx = cu_ctx;
            let r = ffi::av_hwdevice_ctx_init(device_ref.as_ptr());
            if r < 0 {
                bail!("av_hwdevice_ctx_init failed ({r})");
            }
            device_ref
        };

        // SAFETY: `av_hwframe_ctx_alloc` takes the live initialized device ref and returns null
        // (rejected by `from_raw`) or a ref whose `data` is a live `AVHWFramesContext`. Stores
        // below are in-bounds scalar writes on that allocation, done before `av_hwframe_ctx_init`.
        let frames_ref = unsafe {
            let frames_ref = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(device_ref.as_ptr()))
                .context("av_hwframe_ctx_alloc failed")?;
            let fc = (*frames_ref.as_ptr()).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
            (*fc).sw_format = pixel_to_av(sw_format);
            (*fc).width = w as c_int;
            (*fc).height = h as c_int;
            (*fc).initial_pool_size = 0; // we supply the device pointers
            let r = ffi::av_hwframe_ctx_init(frames_ref.as_ptr());
            if r < 0 {
                bail!("av_hwframe_ctx_init failed ({r})");
            }
            frames_ref
        };
        Ok(CudaHw {
            frames_ref,
            device_ref,
        })
    }
}

// No `Drop` for `CudaHw`: each `AvBuffer` unrefs itself in declaration order (frames, then device).

/// NVENC input pixel format, plus whether packed RGB/BGR needs a 3→4 byte expand (`*0` padding).
fn nvenc_input(format: PixelFormat) -> (Pixel, bool) {
    match format {
        PixelFormat::Bgrx => (Pixel::BGRZ, false), // bgr0
        PixelFormat::Rgbx => (Pixel::RGBZ, false), // rgb0
        PixelFormat::Bgra => (Pixel::BGRA, false),
        PixelFormat::Rgba => (Pixel::RGBA, false),
        PixelFormat::Rgb => (Pixel::RGBZ, true), // RGB -> rgb0
        PixelFormat::Bgr => (Pixel::BGRZ, true), // BGR -> bgr0
        // Native YUV — no internal RGB→YUV. Linux GPU-convert (`PUNKTFUNK_NV12`) or Windows D3D11 VP.
        PixelFormat::Nv12 => (Pixel::NV12, false),
        // Zero-copy GPU convert — `hevc_nvenc` emits Range-Extensions 4:4:4.
        PixelFormat::Yuv444 => (Pixel::YUV444P, false),
        // Windows packed-10 / P010 only; Linux capturer never emits them. BGRA keeps the match exhaustive.
        PixelFormat::Rgb10a2 | PixelFormat::Rgb10a2Sdr | PixelFormat::P010 => (Pixel::BGRA, false),
        // HDR never takes RGB-passthrough: `open` routes X2RGB10→P010 before this mapping.
        PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10 => (Pixel::BGRA, false),
    }
}

/// [`NvencEncoder::open`] args, stored so [`Encoder::reset`] can drop a wedged encoder and reopen
/// with the session's negotiated parameters (forfeit owed AUs, restart at an IDR).
#[derive(Clone, Copy)]
struct OpenArgs {
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
}

pub struct NvencEncoder {
    // Field order is load-bearing: `reset` does `*self = fresh`, so drop follows declaration.
    // `sws_csc` must free ahead of `enc`/`frame`/`cuda`. `offset_of` cannot pin this — repr(Rust)
    // may reorder fields in memory.
    /// CPU CSC only: packed source → [`Self::frame`]. RGB/BGR → YUV444P (`hevc_nvenc` emits 4:4:4
    /// only from YUV444 *input*; RGB-in is always 4:2:0), or X2RGB10/X2BGR10 → P010. `None` on
    /// plain RGB and on zero-copy (the worker already delivers CUDA frames).
    sws_csc: Option<AvSwsContext>,
    enc: encoder::video::Encoder,
    /// Reusable 4-bpp CPU input (`None` on CUDA). Overwrite is sound only because the encoder
    /// opens with `delay=0`/`bf=0` and the caller drains `poll()` after each `submit`, so
    /// libavcodec holds no reference to the previous buffer.
    frame: Option<VideoFrame>,
    /// Zero-copy: CUDA hwdevice/hwframes (`AV_PIX_FMT_CUDA`).
    cuda: Option<CudaHw>,
    /// Session opened as full-chroma 4:4:4 (FREXT).
    want_444: bool,
    src_format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic presentation index, in `1/fps` time-base units.
    frame_idx: i64,
    /// Next submit is an IDR ([`request_keyframe`]).
    force_kf: bool,
    /// Intra-refresh mode — [`caps`](Encoder::caps) so session glue rate-limits forced IDRs.
    intra_refresh: bool,
    /// Wave length in frames when intra-refresh, else 0. Cached at open so per-AU `caps()` does
    /// not re-read `PUNKTFUNK_IR_PERIOD_FRAMES`; the pump marks every Nth AU `USER_FLAG_RECOVERY_POINT`.
    intra_refresh_period: u32,
    args: OpenArgs,
}

// SAFETY: `NvencEncoder` owns ffmpeg-next `Encoder`/`VideoFrame` (already `Send`) plus `CudaHw`
// raw `AVBufferRef`s and an optional raw `SwsContext`, none of which are `Send` by default.
// `SwsContext` has no thread affinity and is touched only through `&mut self` on the encode
// thread. The encoder is owned by exactly one thread and only accessed via `&mut self`, so it
// is never aliased. The libav contexts and the shared `CUcontext` have no thread affinity.
// `Send` only; `Sync` is deliberately not implemented.
unsafe impl Send for NvencEncoder {}

/// Latched once intra-refresh open fails with ENOSYS (`NV_ENC_CAPS_SUPPORT_INTRA_REFRESH`).
/// Other open failures must not set this (a bitrate EINVAL must not disable the feature).
static IR_UNSUPPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Intra-refresh when `PUNKTFUNK_INTRA_REFRESH` is truthy and not latched unsupported.
/// A moving intra band + recovery-point SEI refreshes the picture every
/// [`intra_refresh_period`] frames, so unrecoverable loss heals without a 20-40× IDR spike.
fn intra_refresh_requested() -> bool {
    super::policy::intra_refresh_requested()
        && !IR_UNSUPPORTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Intra-refresh wave length in frames. ffmpeg derives `intraRefreshPeriod`/`Cnt` from
/// `gop_size` before forcing GOP infinite, so IR mode sets `gop_size` to this.
fn intra_refresh_period(fps: u32) -> i32 {
    super::policy::intra_refresh_period(fps) as i32
}

impl NvencEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        cuda: bool,
        bit_depth: u8,
        chroma: ChromaFormat,
    ) -> Result<Self> {
        // 10-bit session with packed 2:10:10:10 PQ/BT.2020 (`X2Rgb10`/`X2Bgr10`) encodes HEVC
        // Main10 / 10-bit AV1 from a P010 frame (BT.2020 limited; PQ rides through per-channel).
        // A 10-bit request whose capture stayed SDR encodes 8-bit.
        let want_hdr10 = bit_depth == 10 && format.is_hdr_rgb10() && codec.supports_10bit();
        if bit_depth == 10 && !want_hdr10 {
            tracing::warn!(
                bit_depth,
                ?format,
                codec = codec.nvenc_name(),
                "10-bit requested but the capture format/codec has no 10-bit path — encoding 8-bit"
            );
        }
        if format.is_hdr_rgb10() && !want_hdr10 {
            // 10-bit PQ on an 8-bit session would get a BT.709 VUI and garbage packing — refuse.
            bail!(
                "captured 10-bit HDR frames ({format:?}) on an 8-bit/{} session — refusing to \
                 mislabel PQ content",
                codec.nvenc_name()
            );
        }
        // HEVC Range Extensions. `hevc_nvenc` emits 4:4:4 only from a YUV444 *input* — RGB always
        // subsamples to 4:2:0. Input is either the worker's planar-YUV444 CUDA frames or CPU
        // swscale RGB→YUV444P. Both feed `profile=rext`; range follows `PUNKTFUNK_444_FULLRANGE`.
        let want_444 = chroma.is_444() && codec == Codec::H265;
        if want_444 && want_hdr10 {
            // Handshake resolves 4:4:4∧10-bit down to 8-bit on Linux; fail if it ever reaches here.
            bail!("4:4:4 + 10-bit HDR is not a supported Linux NVENC combination");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `av_log_set_level` writes libav's global integer log level; `48` is
            // AV_LOG_DEBUG (no pointer args). libav was just initialized by `ffmpeg::init()`.
            unsafe { ffi::av_log_set_level(48) }; // AV_LOG_DEBUG — surface NVENC hw-frame rejects
        }
        let name = codec.nvenc_name();
        let av_codec = encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("{name} not built into libavcodec"))?;
        let (rgb_pixel, rgb_expand) = nvenc_input(format);
        // 4:4:4 → YUV444P via swscale; HDR → P010; otherwise captured RGB in, NVENC CSC to 4:2:0.
        let (nvenc_pixel, expand) = if want_444 {
            (Pixel::YUV444P, false)
        } else if want_hdr10 {
            (Pixel::P010LE, false)
        } else {
            (rgb_pixel, rgb_expand)
        };

        let mut video = codec::context::Context::new_with_codec(av_codec)
            .encoder()
            .video()
            .context("alloc video encoder")?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(nvenc_pixel); // RGB path: NVENC CSC; 4:4:4/HDR already YUV
        apply_low_latency_rc(&mut video, fps, bitrate_bps);
        // Infinite GOP — no periodic IDR. A 5120x1440 keyframe is ~20-40× a P-frame; a periodic
        // IDR is a multi-ms encode+send spike. NVENC emits one IDR at start; `forced-idr` turns
        // `request_keyframe` into an on-demand IDR. In IR mode ffmpeg still reads `gop_size` as
        // the wave length then forces `gopLength` infinite — a positive value is not a periodic IDR.
        let intra_refresh = intra_refresh_requested();
        // SAFETY: same `video` builder — a non-null, properly-aligned, sole-owned, not-yet-opened
        // `AVCodecContext`. Write `gop_size` (-1 = infinite GOP, or the IR wave length) before
        // `open_with`; ffmpeg-next has no setter. No aliasing; synchronous scalar write.
        unsafe {
            (*video.as_mut_ptr()).gop_size = if intra_refresh {
                intra_refresh_period(fps)
            } else {
                -1
            };
        }

        // VUI on every session: libav's nvenc wrapper derives colourDescriptionPresentFlag from
        // these fields. Unspecified → no colour description; TVs then guess from resolution.
        // BT.709 limited matches the RGB→YUV both direct-SDK backends stamp. `PUNKTFUNK_444_FULLRANGE=1`
        // (4:4:4 only) converts and signals full range — Linux-only; Windows CSC range is unmeasured.
        let full_range_444 =
            want_444 && std::env::var("PUNKTFUNK_444_FULLRANGE").is_ok_and(|v| v.trim() == "1");
        if want_hdr10 {
            // HDR10: BT.2020 + SMPTE-2084 (PQ), limited — matches the swscale CSC. Static metadata is OOB.
            // SAFETY: `raw = video.as_mut_ptr()` is the non-null, properly-aligned, sole-owned,
            // not-yet-opened `AVCodecContext`; we set its four VUI colour enum fields to valid
            // variants before `open_with`. Sole owner → no aliasing; synchronous writes.
            unsafe {
                let raw = video.as_mut_ptr();
                (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
                (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
                (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
                (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
            }
        } else {
            // SAFETY: same `video` builder — `raw = video.as_mut_ptr()` is the non-null, properly-
            // aligned, sole-owned, not-yet-opened `AVCodecContext`. Four VUI colour enum fields set
            // to valid variants before `open_with`. Sole owner → no aliasing; synchronous writes.
            unsafe {
                let raw = video.as_mut_ptr();
                (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
                (*raw).color_range = if full_range_444 {
                    ffi::AVColorRange::AVCOL_RANGE_JPEG // full
                } else {
                    ffi::AVColorRange::AVCOL_RANGE_MPEG // limited/studio
                };
                (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
                (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            }
        }

        // Zero-copy: wrap the shared CUcontext; set `pix_fmt = CUDA` before open so NVENC takes `hw_frames_ctx`.
        let cuda_hw = if cuda {
            let cu_ctx = pf_zerocopy::cuda::context().context("shared CUDA context")?;
            // SAFETY: `CudaHw::new` requires libav initialized (`ffmpeg::init()` above) and a valid
            // `CUcontext`; `cu_ctx` is the shared importer context, non-null on `Ok`. `nvenc_pixel`
            // is a valid `Pixel`; `width`/`height` are validated positive dims. Returns a RAII
            // `CudaHw` wrapping (not owning) `cu_ctx` and owning two `AVBufferRef`s freed on drop.
            let hw = unsafe { CudaHw::new(cu_ctx, nvenc_pixel, width, height)? };
            // SAFETY: `raw = video.as_mut_ptr()` is the non-null, sole-owned, not-yet-opened
            // `AVCodecContext`. Set `pix_fmt = CUDA` and attach NEW refs (`av_buffer_ref`) of
            // `hw.device_ref`/`hw.frames_ref` — both non-null (`CudaHw::new`) from live `hw`, moved
            // into `NvencEncoder.cuda` next to `enc` so it outlives the encoder. No aliasing.
            unsafe {
                let raw = video.as_mut_ptr();
                (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
                (*raw).hw_device_ctx = ffi::av_buffer_ref(hw.device_ref.as_ptr());
                (*raw).hw_frames_ctx = ffi::av_buffer_ref(hw.frames_ref.as_ptr());
            }
            Some(hw)
        } else {
            None
        };

        let mut opts = Dictionary::new();
        opts.set("preset", "p1"); // fastest
        opts.set("tune", "ull"); // ultra-low-latency
        opts.set("rc", "cbr");
        opts.set("bf", "0");
        opts.set("delay", "0");
        opts.set("forced-idr", "1"); // request_keyframe → real IDR under the infinite GOP
        if intra_refresh {
            // Moving intra band + recovery-point SEI (period = gop_size). Glue rate-limits forced IDRs.
            opts.set("intra-refresh", "1");
        }
        if want_444 {
            // HEVC Range Extensions (`chroma_format_idc=3`). Auto-selected from YUV444P; pin so a
            // future libavcodec cannot silently drop chroma.
            opts.set("profile", "rext");
        }
        if want_hdr10 && codec == Codec::H265 {
            // HEVC Main10. Auto-selected from P010; pin so depth cannot silently drop. AV1 Main
            // already carries 10-bit from the input format.
            opts.set("profile", "main10");
        }

        // Split-frame encode. Policy is [`resolve_split_mode`] (same as the direct-SDK backends).
        // Only FORCED widths are set: libav's `split_encode_mode` is its own vocabulary, and our
        // `DISABLE` is NVENC enum `15` — passing it through would fail the open. Unset = driver auto.
        // `engines = 0` = unprobed; [`max_forced_split_mode`] maps unknown to 2-way.
        let pix_rate = width as u64 * height as u64 * fps as u64;
        let split = std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok();
        match split.as_deref() {
            // Operator override: H.264 has no split AVOption — setting it fails the open.
            Some(mode) if matches!(codec, Codec::H265 | Codec::Av1) => {
                opts.set("split_encode_mode", mode)
            }
            Some(_) => tracing::warn!(
                codec = codec.nvenc_name(),
                "PUNKTFUNK_SPLIT_ENCODE ignored — split encoding is not applicable to H.264 \
                 (nvEncodeAPI.h)"
            ),
            None if matches!(codec, Codec::H265 | Codec::Av1) => {
                let resolved = super::resolve_split_mode(codec, bit_depth, pix_rate, 0);
                if let Some(n) = super::forced_split_width(resolved) {
                    opts.set("split_encode_mode", &n.to_string());
                    tracing::info!(
                        pix_rate,
                        bit_depth,
                        split_encode_mode = n,
                        "NVENC (libav): forcing split encode (shared selector)"
                    );
                }
            }
            None => {}
        }

        // NVENC init failure can take the host down: `ff_cuda_check` hands `av_log` an
        // uninitialized `err_name`/`err_string` and glibc `strlen`s it. Drop the log level to
        // AV_LOG_FATAL for this call only — `QuietLibavLog` is a non-reentrant mutex, and the
        // ENOSYS arm recurses into `Self::open`. The `Err` still surfaces below.
        let opened = {
            let _quiet = QuietLibavLog::new();
            video.open_with(opts)
        };
        let enc = match opened {
            Ok(enc) => enc,
            // GPU lacks `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH` (ENOSYS). Latch and reopen without IR.
            // Other failures, and any failure when IR was not requested, propagate; EINVAL is the
            // bitrate probe key and must not trip the latch.
            Err(e)
                if intra_refresh
                    && matches!(
                        e,
                        ffmpeg::Error::Other {
                            errno: ffmpeg::util::error::ENOSYS
                        }
                    ) =>
            {
                tracing::warn!(
                    encoder = name,
                    "NVENC intra-refresh not supported by this GPU — falling back to IDR-only \
                     recovery"
                );
                IR_UNSUPPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
                return Self::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    cuda,
                    bit_depth,
                    chroma,
                );
            }
            Err(e) => {
                // libav's message was suppressed (formatter can fault). There is no env switch;
                // `PUNKTFUNK_FFMPEG_DEBUG` is outranked for this call. The AVERROR still travels in `e`.
                return Err(e).with_context(|| {
                    format!(
                        "open {name} ({width}x{height}@{fps}, {bitrate_bps} bps) — libav's own \
                         diagnostic is silenced across this call because its CUDA error formatter \
                         can fault the process"
                    )
                });
            }
        };
        if intra_refresh {
            tracing::info!(
                encoder = name,
                period_frames = intra_refresh_period(fps),
                "NVENC intra-refresh recovery active (no periodic IDR; wave heals loss)"
            );
        }

        // After the fallible open: packed-RGB → encoder input (no rescale). 4:4:4 RGB→YUV444P,
        // HDR X2RGB10→P010, or 3-bpp expand RGB24/BGR24→rgb0/bgr0 — mutually exclusive (`expand`
        // only on packed-RGB 4:2:0). Skipped on CUDA: the worker already delivers device frames.
        let sws_csc = if (want_444 || want_hdr10 || expand) && !cuda {
            let src_av = pixel_to_av(sws_src_pixel(format)?);
            let dst_av = pixel_to_av(nvenc_pixel);
            // SAFETY: `sws_getContext` allocates a swscale context for src/dst dims + pixel formats.
            // Both dims are the encoder's positive `width`/`height` as `c_int`; `src_av` is a valid
            // `AVPixelFormat` (from `sws_src_pixel`); dst is YUV444P or P010LE. Trailing filter/param
            // pointers are null = defaults. No Rust memory borrowed; `AvSwsContext` takes ownership
            // (null rejected by `from_raw`).
            let sws = unsafe {
                AvSwsContext::from_raw(ffi::sws_getContext(
                    width as c_int,
                    height as c_int,
                    src_av,
                    width as c_int,
                    height as c_int,
                    dst_av,
                    SWS_POINT,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null(),
                ))
            };
            let Some(sws) = sws else {
                bail!("sws_getContext(RGB→{nvenc_pixel:?}) failed");
            };
            // CSC users only. Expand is a byte shuffle (3-bpp → 4-bpp); a matrix here would
            // range-convert packed-RGB sessions, which the module header promises does not happen.
            if want_444 || want_hdr10 {
                // SAFETY: `sws` is the non-null context from the call above (null-checked). The
                // coefficient tables from `sws_getCoefficients` (ITU-709 / BT.2020 NCL, matching the
                // VUI) are process-lifetime libswscale statics; `sws_setColorspaceDetails` only reads
                // them and writes scalar CSC into `sws` (dstRange 0 = limited, 1 = full-range 4:4:4).
                // No Rust memory is passed.
                unsafe {
                    let cs = ffi::sws_getCoefficients(if want_hdr10 {
                        super::libav::SWS_CS_BT2020
                    } else {
                        SWS_CS_ITU709
                    });
                    let dst_range = i32::from(full_range_444);
                    ffi::sws_setColorspaceDetails(
                        sws.as_ptr(),
                        cs,
                        1,
                        cs,
                        dst_range,
                        0,
                        1 << 16,
                        1 << 16,
                    );
                }
            }
            Some(sws)
        } else {
            None
        };

        let frame = if cuda {
            None
        } else {
            Some(VideoFrame::new(nvenc_pixel, width, height))
        };
        Ok(NvencEncoder {
            sws_csc,
            enc,
            frame,
            cuda: cuda_hw,
            want_444,
            src_format: format,
            width,
            height,
            fps,
            frame_idx: 0,
            force_kf: false,
            intra_refresh,
            intra_refresh_period: if intra_refresh {
                intra_refresh_period(fps).max(1) as u32
            } else {
                0
            },
            args: OpenArgs {
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                cuda,
                bit_depth,
                chroma,
            },
        })
    }
}

impl Encoder for NvencEncoder {
    fn caps(&self) -> super::EncoderCaps {
        super::EncoderCaps {
            // libav NVENC never reads `frame.cursor` — a cursor-as-metadata session loses its pointer.
            blends_cursor: false,
            // FREXT iff this session opened 4:4:4. RFI/HDR-SEI stay at the trait defaults.
            chroma_444: self.want_444,
            intra_refresh: self.intra_refresh,
            // GDR: mark boundary AUs so the client re-anchors on the wave instead of a full IDR.
            // Tied to `intra_refresh` (`PUNKTFUNK_INTRA_REFRESH`); AMF/QSV stay unvalidated.
            intra_refresh_recovery: self.intra_refresh,
            intra_refresh_period: self.intra_refresh_period,
            ..super::EncoderCaps::default()
        }
    }

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
            FramePayload::Cuda(buf) => self.submit_cuda(buf, pts, idr),
            FramePayload::Cpu(bytes) => self.submit_cpu(bytes, captured.format, pts, idr),
            FramePayload::Dmabuf(_) => {
                bail!("NVENC got a VAAPI dmabuf frame — capture/encoder backend mismatch")
            }
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Drop the wedged libavcodec encoder and reopen with stored [`OpenArgs`]. Owed AUs are
    /// forfeited; the fresh encoder starts on an IDR.
    fn reset(&mut self) -> bool {
        let a = self.args;
        match Self::open(
            a.codec,
            a.format,
            a.width,
            a.height,
            a.fps,
            a.bitrate_bps,
            a.cuda,
            a.bit_depth,
            a.chroma,
        ) {
            Ok(mut fresh) => {
                fresh.force_kf = true;
                *self = fresh; // drops the wedged encoder (frees its contexts) in the same step
                true
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "NVENC in-place reopen failed");
                false
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Non-blocking: a packet ships; EAGAIN and EOF both mean nothing this tick.
        match poll_encoder(&mut self.enc, self.fps)? {
            PollOutcome::Packet(au) => Ok(Some(au)),
            PollOutcome::Again | PollOutcome::Eof => Ok(None),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.enc.send_eof().context("send_eof")?;
        Ok(())
    }
}

impl NvencEncoder {
    /// CPU path: expand/copy the packed RGB/BGR bytes into the reusable 4-bpp frame, then send.
    fn submit_cpu(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            format,
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_bpp = self.src_format.bytes_per_pixel();
        let src_row = w * src_bpp;
        anyhow::ensure!(
            bytes.len() >= src_row * h,
            "captured buffer {} bytes < required {}",
            bytes.len(),
            src_row * h
        );
        // Packed RGB → encoder input: 4:4:4, HDR, or 3-bpp expand. The branch below is 4-bpp copy.
        if let Some(sws) = self.sws_csc.as_ref().map(AvSwsContext::as_ptr) {
            let frame = self
                .frame
                .as_mut()
                .context("CPU frame missing (encoder opened in CUDA mode)")?;
            // SAFETY: `format == self.src_format` and `bytes.len() >= src_row * h` (the `ensure!`s
            // above), so `sws_scale` reads `h` rows of `src_row` bytes from `src_data[0] = bytes`
            // (packed RGB is single-plane; other src planes null/0) — all in bounds. `sws` is the
            // non-null context built in `open`. The dst is `frame`'s `AVFrame`, sized by
            // `VideoFrame::new` for this `nvenc_pixel` — so swscale writes the planes it allocated,
            // at the strides it reports. Pointers are live locals; the encoder runs only on this
            // thread (`unsafe impl Send`), so no aliasing/race.
            unsafe {
                let dst_av = frame.as_mut_ptr();
                let src_data: [*const u8; 4] =
                    [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
                let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
                let r = ffi::sws_scale(
                    sws,
                    src_data.as_ptr(),
                    src_stride.as_ptr(),
                    0,
                    h as c_int,
                    (*dst_av).data.as_ptr(),
                    (*dst_av).linesize.as_ptr(),
                );
                if r < 0 {
                    bail!("sws_scale(CPU CSC → encoder input) failed ({r})");
                }
            }
            frame.set_pts(Some(pts));
            frame.set_kind(if idr {
                ffmpeg::picture::Type::I
            } else {
                ffmpeg::picture::Type::None
            });
            self.enc.send_frame(frame).context("send_frame(swscale)")?;
            return Ok(());
        }
        let frame = self
            .frame
            .as_mut()
            .context("CPU frame missing (encoder opened in CUDA mode)")?;
        let stride = frame.stride(0); // dst is 4-bpp, aligned
        let dst = frame.data_mut(0);
        {
            // 4-bpp → 4-bpp, honouring a possibly larger dst stride.
            for y in 0..h {
                dst[y * stride..y * stride + src_row]
                    .copy_from_slice(&bytes[y * src_row..y * src_row + src_row]);
            }
        }
        frame.set_pts(Some(pts));
        frame.set_kind(if idr {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        self.enc.send_frame(frame).context("send_frame")?;
        Ok(())
    }

    /// Zero-copy: imported CUDA buffer → NVENC with no CPU touch.
    ///
    /// Take a pooled surface (`av_hwframe_get_buffer`) and device-copy into it. A bare frame
    /// is rejected (`buf[0]` null; `av_frame_ref` needs a refcounted buffer). NVENC caches
    /// CUDA registrations by device pointer with a bounded table — a fresh pointer every
    /// frame would overflow it; the pool recycles a small set. The copy is device-local.
    fn submit_cuda(&mut self, buf: &pf_zerocopy::DeviceBuffer, pts: i64, idr: bool) -> Result<()> {
        let frames_ref = self
            .cuda
            .as_ref()
            .context("CUDA hw context missing (encoder opened in CPU mode)")?
            .frames_ref
            .as_ptr();
        // Device→device copy uses our shared context; make it current on this thread (ffmpeg
        // pushes its own around the pool alloc, so order is fine).
        pf_zerocopy::cuda::make_current().context("CUDA context current (encode thread)")?;
        // SAFETY: `frames_ref` is the non-null CUDA frames ctx from `self.cuda`; the shared CUDA
        // context was just made current on this thread (`make_current()?`), the precondition for
        // the device-pointer copies below.
        //  * `f` is an owned `AvFrame` — every exit drops it once, releasing its ref on the pooled
        //    surface. `av_hwframe_get_buffer` fills `data[]`/`linesize[]`/`buf[0]`/`hw_frames_ctx`.
        //  * NV12 reads `data[0..2]`/`linesize[0..2]`; else `data[0]`/`linesize[0]` — in-struct
        //    fields of the live frame. `buf` is the imported `DeviceBuffer`, live for this call.
        //  * `avcodec_send_frame` takes its own ref; the drop afterwards is the owning free.
        //    Single-threaded encoder → no race.
        unsafe {
            let f = AvFrame::alloc().context("av_frame_alloc failed")?;
            // Pooled CUDA surface: format, dims, data/linesize, buf[0], hw_frames_ctx. Recycled.
            let r = ffi::av_hwframe_get_buffer(frames_ref, f.as_ptr(), 0);
            if r < 0 {
                bail!("av_hwframe_get_buffer(CUDA) failed ({r})");
            }
            // NV12 is two-plane, YUV444 three-plane (`yuv444p` frames ctx), RGB single-plane.
            // A 4:4:4 session whose buffer is not YUV444 (LINEAR/gamescope, no GPU convert)
            // fails here rather than letting `hevc_nvenc` silently subsample RGB to 4:2:0.
            let copy_res = if buf.yuv444 {
                let dsts = core::array::from_fn(|i| {
                    (
                        (*f.as_ptr()).data[i] as pf_zerocopy::cuda::CUdeviceptr,
                        (*f.as_ptr()).linesize[i] as usize,
                    )
                });
                pf_zerocopy::cuda::copy_yuv444_to_device(buf, dsts, true)
            } else if self.want_444 {
                bail!(
                    "4:4:4 session but the zero-copy frame is not YUV444 (LINEAR/gamescope \
                     capture has no GPU 4:4:4 convert) — unset PUNKTFUNK_ZEROCOPY to use the \
                     CPU 4:4:4 path on this compositor"
                );
            } else if buf.is_nv12() {
                let y_ptr = (*f.as_ptr()).data[0] as pf_zerocopy::cuda::CUdeviceptr;
                let y_pitch = (*f.as_ptr()).linesize[0] as usize;
                let uv_ptr = (*f.as_ptr()).data[1] as pf_zerocopy::cuda::CUdeviceptr;
                let uv_pitch = (*f.as_ptr()).linesize[1] as usize;
                pf_zerocopy::cuda::copy_nv12_to_device(buf, y_ptr, y_pitch, uv_ptr, uv_pitch, true)
            } else {
                let dst_ptr = (*f.as_ptr()).data[0] as pf_zerocopy::cuda::CUdeviceptr;
                let dst_pitch = (*f.as_ptr()).linesize[0] as usize;
                pf_zerocopy::cuda::copy_device_to_device(buf, dst_ptr, dst_pitch, true)
            };
            copy_res.context("copy imported buffer into NVENC surface")?;
            (*f.as_ptr()).pts = pts;
            (*f.as_ptr()).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), f.as_ptr());
            if r < 0 {
                bail!("avcodec_send_frame(CUDA) failed ({r})");
            }
        }
        Ok(())
    }
}

// No `Drop` for `NvencEncoder`: `sws_csc` frees itself first (field #1), ahead of `enc`/`frame`/`cuda`.

/// Serialises the save → `AV_LOG_FATAL` → restore window every capability probe opens around
/// an encoder open it expects to fail.
///
/// libav's log level is one process-global `int`. NVENC and VAAPI probes race from `/serverinfo`
/// and session bring-up; interleaved get/set pins the process at `AV_LOG_FATAL` and later
/// diagnostics vanish. Probes already run process-once.
static LIBAV_LOG_LEVEL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII quiet-window: `AV_LOG_FATAL` on construct, restore on drop, holding [`LIBAV_LOG_LEVEL`].
///
/// Callers must have completed `ffmpeg::init()`. Not re-entrant. `pub(crate)` so VAAPI probes
/// share the lock with NVENC (same global).
pub(crate) struct QuietLibavLog {
    prev: c_int,
    // Held for the guard's lifetime. `Drop for QuietLibavLog` runs before fields drop, so restore
    // still happens under the lock.
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl QuietLibavLog {
    pub(crate) fn new() -> Self {
        // Poison-tolerant: a panic mid-window already restored via `Drop`; refusing the lock forever is worse.
        let lock = LIBAV_LOG_LEVEL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: libav is initialized by the caller; `av_log_{get,set}_level` only read/write the
        // global int level (no pointer args) and are always sound post-init.
        let prev = unsafe {
            let p = ffi::av_log_get_level();
            ffi::av_log_set_level(ffi::AV_LOG_FATAL);
            p
        };
        Self { prev, _lock: lock }
    }
}

impl Drop for QuietLibavLog {
    fn drop(&mut self) {
        // SAFETY: restore the saved global level (scalar arg, no pointers); libav was initialized
        // before this guard was constructed.
        unsafe { ffi::av_log_set_level(self.prev) };
    }
}

/// Probe HEVC 4:4:4 (Range Extensions) by opening a tiny `hevc_nvenc` session — the same path
/// [`NvencEncoder::open`] takes. Cached by [`crate::can_encode_444`]. Failure → host downgrades
/// to 4:2:0 before Welcome.
///
/// Only when libav will serve the session (`PUNKTFUNK_NVENC_DIRECT=0` or no `--features nvenc`).
/// A direct-SDK host must use the driver's caps bit (`nvenc_cuda::probe_support`): one
/// `hevc_nvenc` FREXT open+close wedges later NVENC opens (`NV_ENC_ERR_INVALID_VERSION`).
pub fn probe_can_encode_444(codec: Codec) -> bool {
    if codec != Codec::H265 {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // Expected-fail open: hold the quiet window until return so the level restores either way.
    let _quiet = QuietLibavLog::new();
    NvencEncoder::open(
        codec,
        PixelFormat::Bgra,
        640,
        480,
        30,
        2_000_000,
        false, // CPU input (the 4:4:4 path never uses CUDA)
        8,
        ChromaFormat::Yuv444,
    )
    .is_ok()
}

/// Probe 10-bit (HEVC Main10 / 10-bit AV1) from P010 — the HDR path in [`NvencEncoder::open`].
/// Cached by [`crate::can_encode_10bit`]. Failure → host downgrades to 8-bit SDR before Welcome.
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // Expected-fail open: hold the quiet window until return so the level restores either way.
    let _quiet = QuietLibavLog::new();
    NvencEncoder::open(
        codec,
        PixelFormat::X2Rgb10,
        640,
        480,
        30,
        2_000_000,
        false, // CPU input (the HDR swscale path)
        10,
        ChromaFormat::Yuv420,
    )
    .is_ok()
}

#[cfg(test)]
mod cuda_hw_tests {
    use super::*;

    /// `CudaHw` owns two `AVBufferRef`s through `AvBuffer`: construct-and-drop is the contract.
    /// A missed unref leaks; a doubled one aborts in glibc. Loop so a leak shows as growth.
    ///
    /// `#[ignore]` (needs a CUDA device):
    /// `cargo test -p pf-encode cuda_hw_alloc_drop_cycles -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real CUDA device (run on an NVIDIA host, not the build box)"]
    fn cuda_hw_alloc_drop_cycles() {
        ffmpeg::init().expect("libav init");
        let cu_ctx = pf_zerocopy::cuda::context().expect("shared CUDA context");
        for i in 0..8 {
            // SAFETY: `CudaHw::new` requires libav initialized (asserted above) and a valid
            // `CUcontext` — `cu_ctx` is the live shared context from `pf_zerocopy`. NV12 at
            // 640x480 are a valid format and positive dims. The handle drops each iteration.
            let hw = unsafe { CudaHw::new(cu_ctx.cast(), Pixel::NV12, 640, 480) }
                .unwrap_or_else(|e| panic!("CudaHw::new failed on iteration {i}: {e:#}"));
            assert!(!hw.device_ref.as_ptr().is_null(), "device ref went null");
            assert!(!hw.frames_ref.as_ptr().is_null(), "frames ref went null");
        }
        eprintln!("8 CudaHw alloc/drop cycles completed without abort");
    }
}

#[cfg(test)]
mod hdr_tests {
    use super::*;

    /// HDR encode on a real NVIDIA GPU: synthetic X2RGB10 → swscale BT.2020 → P010 →
    /// `hevc_nvenc` Main10, drained to an AU.
    /// `cargo test -p pf-encode nvenc_hdr10_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn nvenc_hdr10_smoke() {
        let (w, h) = (640u32, 480u32);
        let mut enc = NvencEncoder::open(
            Codec::H265,
            PixelFormat::X2Rgb10,
            w,
            h,
            30,
            2_000_000,
            false,
            10,
            ChromaFormat::Yuv420,
        )
        .expect("open hevc_nvenc Main10 (P010 input)");
        // Packed x:R:G:B 2:10:10:10 gradient (values treated as PQ — enough for a smoke).
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let r = (x * 1023 / w.max(1)) & 0x3ff;
                let g = (y * 1023 / h.max(1)) & 0x3ff;
                let b = ((x + y) * 1023 / (w + h)) & 0x3ff;
                let px: u32 = (r << 20) | (g << 10) | b;
                let i = ((y * w + x) * 4) as usize;
                bytes[i..i + 4].copy_from_slice(&px.to_le_bytes());
            }
        }
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cpu(bytes),
            cursor: None,
        };
        let mut au = None;
        for _ in 0..30 {
            enc.submit(&frame).expect("submit X2Rgb10 frame");
            if let Some(a) = enc.poll().expect("poll") {
                au = Some(a);
                break;
            }
        }
        let au = au.expect("no AU produced within 30 frames");
        assert!(!au.data.is_empty(), "empty AU");
        assert!(au.keyframe, "first AU should be the IDR");
        println!("HDR10 smoke: first AU {} bytes (IDR)", au.data.len());
        // PF_HDR_SMOKE_DUMP=/path.h265 writes the Annex-B AU; ffprobe should show Main 10, bt2020/smpte2084.
        if let Ok(path) = std::env::var("PF_HDR_SMOKE_DUMP") {
            std::fs::write(&path, &au.data).expect("dump AU");
            println!("HDR10 smoke: AU written to {path}");
        }
    }
}
