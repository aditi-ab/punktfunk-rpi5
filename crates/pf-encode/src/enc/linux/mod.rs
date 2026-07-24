//! NVENC encoder via `ffmpeg-next` (binds the system FFmpeg — `ffmpeg-sys-next` auto-detects the
//! installed version, so this builds against FFmpeg 7.x/libavcodec 61 *or* 8.x/libavcodec 62;
//! validated live on Ubuntu 26.04 (FFmpeg 8) and Bazzite F43 (FFmpeg 7.1)).
//!
//! Input is a packed RGB/BGR CPU frame; `*_nvenc` accepts `rgb0`/`bgr0`/`rgba`/`bgra`
//! directly and does the RGB→YUV conversion on the GPU, so the host stays off the
//! colour-conversion path. The portal commonly negotiates packed 24-bit `RGB`, which NVENC
//! does *not* accept — we expand it to `rgb0` (one padding byte/pixel, no colour math).
//! The encoder is opened *without* a global header so VPS/SPS/PPS are emitted in-band on
//! every IDR — the output is both a playable raw Annex-B stream and self-contained AUs.
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

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
    apply_low_latency_rc, pixel_to_av, poll_encoder, PollOutcome, SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi; // = ffmpeg_sys_next

/// The swscale *source* pixel format for a captured packed RGB/BGR layout (the real byte order, not
/// the NVENC-padded `*0` form). Used by the CPU conversion paths: 4:4:4 RGB→YUV444P, and HDR
/// X2RGB10/X2BGR10→P010. Mirrors the VAAPI CPU-input mapping; YUV inputs can't feed this path.
fn sws_src_pixel(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        // The GNOME 50+ HDR capture formats (PQ/BT.2020 packed 2:10:10:10) — the HDR CPU path's
        // swscale source for the X2RGB10→P010 conversion.
        PixelFormat::X2Rgb10 => Pixel::X2RGB10LE,
        PixelFormat::X2Bgr10 => Pixel::X2BGR10LE,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Yuv444 => {
            bail!("NVENC CPU-input conversion supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// `AVCUDADeviceContext` (libavutil/hwcontext_cuda.h) — not in the ffmpeg-sys bindings (the
/// crate doesn't allowlist that header), so mirror its stable 3-pointer layout. We set the
/// first field to *our* `CUcontext` so NVENC shares the context the EGL importer maps into.
#[repr(C)]
struct AVCUDADeviceContext {
    cuda_ctx: *mut std::ffi::c_void, // CUcontext
    stream: *mut std::ffi::c_void,   // CUstream (null = default)
    internal: *mut std::ffi::c_void, // filled by ctx_init
}

/// CUDA hardware-frame contexts that wrap our shared `CUcontext`, so `hevc_nvenc` reads the
/// imported device buffer directly. Owns two `AVBufferRef`s, unref'd on drop.
struct CudaHw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl CudaHw {
    /// Build a CUDA hwdevice wrapping `cu_ctx` and a frames pool (`sw_format` = `pixel`).
    unsafe fn new(cu_ctx: *mut std::ffi::c_void, sw_format: Pixel, w: u32, h: u32) -> Result<Self> {
        let mut device_ref = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
        if device_ref.is_null() {
            bail!("av_hwdevice_ctx_alloc(CUDA) failed");
        }
        let dev_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
        let cu = (*dev_ctx).hwctx as *mut AVCUDADeviceContext;
        (*cu).cuda_ctx = cu_ctx; // share the importer's context
        let r = ffi::av_hwdevice_ctx_init(device_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwdevice_ctx_init failed ({r})");
        }

        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_alloc failed");
        }
        let fc = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*fc).sw_format = pixel_to_av(sw_format);
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = 0; // we supply the device pointers
        let r = ffi::av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_init failed ({r})");
        }
        Ok(CudaHw {
            device_ref,
            frames_ref,
        })
    }
}

impl Drop for CudaHw {
    fn drop(&mut self) {
        // SAFETY: `frames_ref`/`device_ref` are the two non-null `AVBufferRef`s `CudaHw::new` created
        // (it bails before returning `Self` if either alloc fails, so a live `CudaHw` always holds
        // both). `av_buffer_unref` drops one reference and nulls the pointer through the `&mut`. This
        // `Drop` runs exactly once and `CudaHw` owns these refs exclusively → no double-free /
        // use-after-free. Frames are unref'd before the device (the frames ctx internally refs the
        // device; refcounted, so the order is sound regardless).
        unsafe {
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

/// Map a captured layout to the NVENC input pixel format, and whether a 3→4 byte expand is
/// needed (packed RGB/BGR have no padding byte; the NVENC `*0` formats do).
fn nvenc_input(format: PixelFormat) -> (Pixel, bool) {
    match format {
        PixelFormat::Bgrx => (Pixel::BGRZ, false), // bgr0
        PixelFormat::Rgbx => (Pixel::RGBZ, false), // rgb0
        PixelFormat::Bgra => (Pixel::BGRA, false),
        PixelFormat::Rgba => (Pixel::RGBA, false),
        PixelFormat::Rgb => (Pixel::RGBZ, true), // RGB -> rgb0
        PixelFormat::Bgr => (Pixel::BGRZ, true), // BGR -> bgr0
        // NV12 is native YUV: NVENC encodes it with NO internal RGB→YUV CSC (the Tier 2A win). On
        // Linux it's produced by the GPU convert on the zero-copy tiled path (`PUNKTFUNK_NV12`); on
        // Windows by the D3D11 video processor.
        PixelFormat::Nv12 => (Pixel::NV12, false),
        // Planar YUV444 from the zero-copy worker's GPU convert (a 4:4:4 session) — native
        // full-chroma YUV in, `hevc_nvenc` emits Range-Extensions 4:4:4.
        PixelFormat::Yuv444 => (Pixel::YUV444P, false),
        // Rgb10a2 (HDR) and P010 (the Windows 10-bit video-processor output) are produced only by
        // the Windows paths; the Linux capturer never emits them. Map to BGRA so the match is
        // exhaustive — unreachable here.
        PixelFormat::Rgb10a2 | PixelFormat::P010 => (Pixel::BGRA, false),
        // The Linux HDR capture formats never take the RGB-passthrough input: `open` intercepts
        // them onto the X2RGB10→P010 swscale path before consulting this mapping (like 4:4:4).
        PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10 => (Pixel::BGRA, false),
    }
}

/// The [`NvencEncoder::open`] arguments, kept on the encoder so [`Encoder::reset`] can rebuild it
/// in place with the session's negotiated parameters — the encode-stall watchdog's recovery lever
/// (drop the wedged libavcodec encoder, reopen fresh, forfeit the owed AUs, restart at an IDR).
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
    enc: encoder::video::Encoder,
    /// Reusable 4-bpp CPU input frame (CPU path only; `None` for the zero-copy/CUDA path).
    /// Mutating it in place across frames is sound only because the encoder is opened with
    /// `delay=0`/`bf=0`/`max_b_frames=0` and the caller drains `poll()` after each `submit`,
    /// so libavcodec holds no reference to the previous frame's buffer when we overwrite it.
    frame: Option<VideoFrame>,
    /// Zero-copy path: CUDA hwdevice/hwframes contexts (the encoder takes `AV_PIX_FMT_CUDA`).
    cuda: Option<CudaHw>,
    /// CPU CSC paths only: swscale context converting the captured packed source into
    /// [`Self::frame`] — RGB/BGR → planar YUV444P for a 4:4:4 session (`hevc_nvenc` only emits
    /// 4:4:4 from a YUV444 *input*; RGB-in is always 4:2:0), or X2RGB10/X2BGR10 → P010 (BT.2020
    /// limited) for an HDR session. `None` on the plain RGB paths AND on the zero-copy paths (the
    /// worker's GPU convert delivers ready CUDA frames). Freed in `Drop`.
    sws_csc: Option<*mut ffi::SwsContext>,
    /// This session opened as full-chroma 4:4:4 (FREXT) — via either input path.
    want_444: bool,
    src_format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic presentation index, in `1/fps` time-base units.
    frame_idx: i64,
    /// Force the next submitted frame to be an IDR (set by [`request_keyframe`]).
    force_kf: bool,
    /// Opened in intra-refresh mode (surfaced via [`caps`](Encoder::caps) so the session glue
    /// rate-limits forced IDRs — the wave heals loss without them).
    intra_refresh: bool,
    /// Resolved wave length in frames when [`intra_refresh`](Self::intra_refresh), else 0. Cached at
    /// open so the pump's per-AU `caps()` doesn't re-read `PUNKTFUNK_IR_PERIOD_FRAMES`; the pump marks
    /// every Nth AU with `USER_FLAG_RECOVERY_POINT` for the client's clean re-anchor.
    intra_refresh_period: u32,
    /// The open arguments, for the in-place [`reset`](Encoder::reset) rebuild.
    args: OpenArgs,
}

// `CudaHw` holds raw `AVBufferRef`s and `sws_csc` a raw `SwsContext`; the encoder lives on a single
// thread. The CPU encoder is already `Send` via ffmpeg-next; assert it for the raw fields too.
// SAFETY: `NvencEncoder` owns an ffmpeg-next `Encoder`/`VideoFrame` (already `Send`) plus a `CudaHw`
// holding raw `AVBufferRef`s and an optional raw `SwsContext`, none of which are `Send` by default.
// The `SwsContext` is a self-contained swscale state object with no thread affinity, touched only
// through `&mut self` on the one encode thread. The encoder is owned and driven by
// exactly ONE thread — the per-session encode thread it is moved to — and is only touched through
// `&mut self` methods, so it is never aliased or accessed concurrently. The wrapped libav contexts
// (and the shared `CUcontext` the `CudaHw` references) have no thread affinity, so transferring
// ownership across threads is sound. This asserts `Send` (transfer) only, extending ffmpeg-next's
// existing `Send` to the raw CUDA fields; `Sync` (shared `&`) is deliberately NOT implemented.
unsafe impl Send for NvencEncoder {}

/// Latched true once an intra-refresh open failed with the device-capability error (ENOSYS from
/// `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH`), so later sessions skip the doomed attempt. Never set by
/// other open failures (a bitrate EINVAL must not permanently disable the feature).
static IR_UNSUPPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this open should run the NVENC **intra-refresh** loss-recovery mode
/// (`PUNKTFUNK_INTRA_REFRESH` truthy, opt-in until on-glass validated): a moving intra band +
/// recovery-point SEI refreshes the whole picture every [`intra_refresh_period`] frames, so
/// FEC-unrecoverable loss heals without the 20-40× full-IDR spike (which under loss causes more
/// loss — the cascade). The session glue then rate-limits client keyframe requests
/// ([`EncoderCaps::intra_refresh`](super::EncoderCaps)).
fn intra_refresh_requested() -> bool {
    std::env::var("PUNKTFUNK_INTRA_REFRESH")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
        && !IR_UNSUPPORTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The intra-refresh wave length in frames — ffmpeg derives `intraRefreshPeriod`/`Cnt` from
/// `gop_size` before forcing the real GOP infinite, so this is what `gop_size` is set to in IR
/// mode. Default = half a second of frames (heals fast, spreads the intra cost to ~2-3% per
/// frame); `PUNKTFUNK_IR_PERIOD_FRAMES` overrides.
fn intra_refresh_period(fps: u32) -> i32 {
    std::env::var("PUNKTFUNK_IR_PERIOD_FRAMES")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|v| *v >= 2)
        .unwrap_or_else(|| (fps.max(16) / 2) as i32)
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
        // HDR / 10-bit (GNOME 50+ HDR screencast): a 10-bit session whose capture negotiated a
        // packed 2:10:10:10 PQ/BT.2020 format (`X2Rgb10`/`X2Bgr10`) encodes HEVC Main10 / 10-bit
        // AV1 from a P010 input frame we produce by swscale (BT.2020 limited; the PQ transfer
        // rides through per-channel — BT.2020 NCL Y'CbCr *is* derived from the PQ-encoded R'G'B').
        // A 10-bit request whose capture stayed SDR (HDR offer downgraded) honestly encodes 8-bit.
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
            // A 10-bit PQ capture on an 8-bit session would be encoded with a BT.709 VUI and
            // garbage bit-packing — never silently; the session must renegotiate.
            bail!(
                "captured 10-bit HDR frames ({format:?}) on an 8-bit/{} session — refusing to \
                 mislabel PQ content",
                codec.nvenc_name()
            );
        }
        // Full-chroma 4:4:4 (HEVC Range Extensions). `hevc_nvenc` only emits 4:4:4 from a YUV444
        // *input* frame — feeding RGB always subsamples to 4:2:0 regardless of profile (verified on
        // the RTX 5070 Ti). Two ways to produce that input: the zero-copy worker's GPU convert
        // (planar-YUV444 CUDA frames — `cuda` true), or the CPU path's swscale RGB→YUV444P. Both
        // feed `profile=rext`; the range follows `PUNKTFUNK_444_FULLRANGE` in both.
        let want_444 = chroma.is_444() && codec == Codec::H265;
        if want_444 && want_hdr10 {
            // The handshake resolves 4:4:4∧10-bit down to 8-bit on Linux, so this can't happen —
            // fail loudly if it ever does rather than picking one silently.
            bail!("4:4:4 + 10-bit HDR is not a supported Linux NVENC combination");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `av_log_set_level` sets libav's global integer log level; `48` (= AV_LOG_DEBUG)
            // is a valid level with no pointer args, and libav was just initialized by `ffmpeg::init()`
            // above — always sound.
            unsafe { ffi::av_log_set_level(48) }; // AV_LOG_DEBUG — surface NVENC hw-frame rejects
        }
        let name = codec.nvenc_name();
        let av_codec = encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("{name} not built into libavcodec"))?;
        let (rgb_pixel, rgb_expand) = nvenc_input(format);
        // 4:4:4 feeds NVENC a planar YUV444P frame we produce by swscale; HDR feeds it a P010
        // frame likewise; the ordinary path feeds the captured RGB straight in and lets NVENC's
        // internal CSC subsample to 4:2:0.
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
        video.set_format(nvenc_pixel); // NVENC converts RGB→YUV internally
                                       // Fixed rate, CBR, no B-frames, ~1-frame VBV — the shared low-latency RC contract.
        apply_low_latency_rc(&mut video, fps, bitrate_bps);
        // Infinite GOP — NO periodic IDR. A keyframe at 5120x1440 is ~20-40x a P-frame, so a
        // periodic IDR is a recurring multi-millisecond encode+packetize+send spike — the ~2s
        // "freeze". NVENC emits one IDR at stream start, then P-frames only; `forced-idr` (below)
        // turns a client recovery request (RFI, via `request_keyframe`) into an IDR on demand.
        // This is the Moonlight/Sunshine low-latency model.
        // In intra-refresh mode the GOP is still infinite — ffmpeg reads `gop_size` as the refresh
        // WAVE length (`intraRefreshPeriod`/`Cnt`) and then forces `gopLength` infinite itself, so
        // a positive `gop_size` here does NOT reintroduce periodic IDRs.
        let intra_refresh = intra_refresh_requested();
        // SAFETY: same `video` builder as above — a non-null, properly-aligned, sole-owned, not-yet-
        // opened `AVCodecContext`. We write the plain `gop_size` int field (-1 = infinite GOP, or the
        // intra-refresh wave length) before `open_with`, which ffmpeg-next has no setter for. No
        // aliasing; synchronous scalar write.
        unsafe {
            (*video.as_mut_ptr()).gop_size = if intra_refresh {
                intra_refresh_period(fps)
            } else {
                -1
            };
        }

        // NV12 / 4:4:4 paths: we do the RGB→YUV conversion ourselves as BT.709 (swscale), so
        // signal that in the bitstream VUI (colorspace/range/primaries/transfer) — otherwise the
        // client decoder assumes a default and the picture comes out washed-out / wrong-contrast.
        // The RGB-input 4:2:0 path leaves these unset (NVENC's internal CSC writes its own VUI).
        // Matches the Windows NV12 path's BT.709 limited-range signalling.
        //
        // PUNKTFUNK_444_FULLRANGE=1 (experimental, 4:4:4-only): convert AND signal FULL range —
        // recovers the ~12% of code space limited-range quantization gives up, for the exact
        // text/UI chroma 4:4:4 exists for. Every punktfunk client honors the signaled range
        // (csc_rows / the Apple rows port); ship as default only if the on-glass A/B shows a
        // visible win. Linux-only: the Windows path's NVENC-internal CSC range is unmeasured.
        let full_range_444 =
            want_444 && std::env::var("PUNKTFUNK_444_FULLRANGE").is_ok_and(|v| v.trim() == "1");
        if want_hdr10 {
            // HDR10: BT.2020 primaries + SMPTE-2084 (PQ) transfer, limited range — matches the
            // swscale BT.2020 CSC below and the Windows paths' signalling. The client decoder
            // auto-detects PQ from the VUI; static mastering metadata rides out-of-band.
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
        } else if matches!(format, PixelFormat::Nv12) || want_444 {
            // SAFETY: same `video` builder — `raw = video.as_mut_ptr()` is the non-null, properly-
            // aligned, sole-owned, not-yet-opened `AVCodecContext`. We set its four VUI colour enum
            // fields to valid `AVColorSpace`/`AVColorRange`/`AVColorPrimaries`/`AVColorTransfer-
            // Characteristic` variants before `open_with`. Sole owner → no aliasing; synchronous writes.
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

        // For the zero-copy path, take CUDA surfaces: wrap the shared CUcontext in CUDA
        // hwdevice/hwframes contexts and set `pix_fmt = CUDA` on the raw encoder context
        // *before* open (NVENC derives the device from `hw_frames_ctx`).
        let cuda_hw = if cuda {
            let cu_ctx = pf_zerocopy::cuda::context().context("shared CUDA context")?;
            // SAFETY: `CudaHw::new` (an `unsafe fn`) requires libav initialized (the `ffmpeg::init()`
            // above ran) and a valid `CUcontext`; `cu_ctx` is the shared importer context from
            // `zerocopy::cuda::context()?`, non-null on the `Ok` path. `nvenc_pixel` is a valid `Pixel`
            // and `width`/`height` are the validated positive dims. It returns a RAII `CudaHw` wrapping
            // (not owning) `cu_ctx` and owning two `AVBufferRef`s freed on drop.
            let hw = unsafe { CudaHw::new(cu_ctx, nvenc_pixel, width, height)? };
            // SAFETY: `raw = video.as_mut_ptr()` is the non-null, sole-owned, not-yet-opened
            // `AVCodecContext`. We set `pix_fmt = CUDA` and attach NEW refs (`av_buffer_ref`) of
            // `hw.device_ref`/`hw.frames_ref` — both non-null (`CudaHw::new` guarantees) and from the
            // live `hw`, which is moved into `NvencEncoder.cuda` next to `enc` and so outlives the
            // encoder. The context owns its own refs (freed when the context closes). No aliasing.
            unsafe {
                let raw = video.as_mut_ptr();
                (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
                (*raw).hw_device_ctx = ffi::av_buffer_ref(hw.device_ref);
                (*raw).hw_frames_ctx = ffi::av_buffer_ref(hw.frames_ref);
            }
            Some(hw)
        } else {
            None
        };

        // Low-latency NVENC tuning (plan §7 / linux-setup doc).
        let mut opts = Dictionary::new();
        opts.set("preset", "p1"); // fastest
        opts.set("tune", "ull"); // ultra-low-latency
        opts.set("rc", "cbr");
        opts.set("bf", "0");
        opts.set("delay", "0");
        opts.set("forced-idr", "1"); // RFI/request_keyframe → real IDR under the infinite GOP
        if intra_refresh {
            // Moving intra band + recovery-point SEI (period set via gop_size above). Loss now
            // self-heals within the wave; forced IDRs remain available (rate-limited by the glue).
            opts.set("intra-refresh", "1");
        }
        if want_444 {
            // HEVC Range Extensions — the profile that carries chroma_format_idc=3. With a YUV444P
            // input `hevc_nvenc` auto-selects it, but pin it explicitly so the chroma is never silently
            // dropped on a future libavcodec.
            opts.set("profile", "rext");
        }
        if want_hdr10 && codec == Codec::H265 {
            // HEVC Main10. `hevc_nvenc` auto-selects it from the P010 input, but pin it explicitly
            // so the depth is never silently dropped on a future libavcodec. (10-bit AV1 needs no
            // profile — AV1 Main carries 10-bit, driven by the input format.)
            opts.set("profile", "main10");
        }

        // Split-frame encode across both NVENC engines (GB203 has 2) when the pixel rate exceeds
        // a single engine's HEVC capacity (~1 Gpix/s); e.g. 5120x1440@240 = 1.77 Gpix/s needs it,
        // @120 = 0.88 Gpix/s does not. HEVC/AV1 only (not H.264). AUTO won't engage below ~2112px
        // height, so we force `2`; below the threshold we leave it AUTO (split costs ~2% BD-rate).
        // Output is standard HEVC — transparent to the client. Override with PUNKTFUNK_SPLIT_ENCODE.
        let pix_rate = width as u64 * height as u64 * fps as u64;
        let split = std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok();
        match split.as_deref() {
            Some(mode) => opts.set("split_encode_mode", mode),
            None if matches!(codec, Codec::H265 | Codec::Av1) && pix_rate > 1_000_000_000 => {
                opts.set("split_encode_mode", "2");
                tracing::info!(
                    pix_rate,
                    "NVENC: forcing 2-way split encode (high pixel rate)"
                );
            }
            None => {}
        }

        let enc = match video.open_with(opts) {
            Ok(enc) => enc,
            // The GPU lacks NV_ENC_CAPS_SUPPORT_INTRA_REFRESH — ffmpeg fails the open with
            // ENOSYS ("Function not implemented"). Latch it (skip the doomed attempt on later
            // sessions) and reopen this session without intra-refresh; any other failure — and
            // any failure when IR wasn't requested — propagates untouched (the bitrate probe
            // keys on EINVAL, which must not trip the latch).
            Err(e) if intra_refresh && format!("{e:#}").contains("Function not implemented") => {
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
                return Err(e).with_context(|| {
                    format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)")
                })
            }
        };
        if intra_refresh {
            tracing::info!(
                encoder = name,
                period_frames = intra_refresh_period(fps),
                "NVENC intra-refresh recovery active (no periodic IDR; wave heals loss)"
            );
        }

        // Built HERE, below the fallible encoder open, NOT above it. `sws_getContext` returns a raw
        // pointer whose only free is `Drop for NvencEncoder` — and `Drop` needs a CONSTRUCTED
        // `Self`, which does not exist on `open`'s early returns (the intra-refresh-unsupported
        // retry, which recurses into `Self::open`, and the plain error return). Creating the
        // context above them leaked one per failed attempt, and `open_nvenc_probed`'s EINVAL
        // bitrate ladder calls `open` up to ~10 times, so a host stepping its bitrate down leaked a
        // context per step. Nothing between here and the `Ok(NvencEncoder { … })` below can return,
        // so this placement makes the leak unrepresentable rather than merely unlikely.
        // CPU CSC paths: build the packed-RGB → planar swscale (no rescale) into the encoder's
        // input frame. THREE users: 4:4:4 (RGB→YUV444P, BT.709, range per the flag), HDR
        // (X2RGB10/X2BGR10→P010, BT.2020 limited — the PQ transfer is per-channel and rides
        // through the matrix untouched), and the packed 3-bpp expand (RGB24/BGR24→rgb0/bgr0).
        //
        // The expand used to be a hand-written per-pixel loop in `submit_cpu`: `w*h` iterations,
        // each building two bounds-checked sub-slices for a 3-byte copy — a shape LLVM will not
        // vectorise into the byte shuffle it is, on the COMMON CPU path (the portal and wlroots
        // both commonly fixate packed 24-bit RGB, and pf-capture offers it first). swscale's
        // packed-RGB expanders are SIMD, the sibling VAAPI backend already routes RGB24/BGR24
        // through them, and this file already owned the context lifecycle — so the change is net
        // subtractive. The three are mutually exclusive by construction: `expand` is only ever
        // true on the packed-RGB 4:2:0 path (see `nvenc_pixel`/`expand` above), never with
        // `want_444`, so one context serves whichever applies.
        //
        // Skipped on the zero-copy path (`cuda`): the worker's GPU convert already delivers ready
        // CUDA frames — no CPU pixels exist to scale.
        let sws_csc = if (want_444 || want_hdr10 || expand) && !cuda {
            let src_av = pixel_to_av(sws_src_pixel(format)?);
            let dst_av = pixel_to_av(nvenc_pixel);
            // SAFETY: `sws_getContext` allocates a swscale context for the given src/dst dims + pixel
            // formats. Both dims are the encoder's positive `width`/`height` as `c_int`; `src_av` is a
            // valid `AVPixelFormat` (from the `sws_src_pixel`-validated packed-RGB source), the dst is
            // YUV444P (4:4:4) or P010LE (HDR). The trailing filter/param pointers are null = "use
            // defaults" (documented as accepted). No Rust memory is borrowed; the returned pointer is
            // null-checked below.
            let sws = unsafe {
                ffi::sws_getContext(
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
                )
            };
            if sws.is_null() {
                bail!("sws_getContext(RGB→{nvenc_pixel:?}) failed");
            }
            // Colour math applies to the CSC users ONLY. The expand is a pure byte shuffle —
            // packed 3-bpp RGB/BGR to the same channels in 4 bytes, `nvenc_pixel` being `rgb0`/
            // `bgr0` — and NVENC does the RGB→YUV itself downstream. Handing it a matrix + range
            // here would silently range-convert every packed-RGB session, which is exactly what the
            // module header promises does not happen ("no colour math").
            if want_444 || want_hdr10 {
                // SAFETY: `sws` is the non-null context from the call above (null-checked). The
                // coefficient tables from `sws_getCoefficients` (ITU-709 for 4:4:4, BT.2020 NCL for
                // HDR — matching the VUI written above) are process-lifetime libswscale statics,
                // reused for src+dst matrices; `sws_setColorspaceDetails` only reads them and writes
                // scalar CSC settings into `sws` (dstRange matches the VUI: 0 = limited, 1 = the
                // PUNKTFUNK_444_FULLRANGE experiment; HDR is always limited). No Rust memory is passed.
                unsafe {
                    let cs = ffi::sws_getCoefficients(if want_hdr10 {
                        super::libav::SWS_CS_BT2020
                    } else {
                        SWS_CS_ITU709
                    });
                    let dst_range = i32::from(full_range_444);
                    ffi::sws_setColorspaceDetails(sws, cs, 1, cs, dst_range, 0, 1 << 16, 1 << 16);
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
            enc,
            frame,
            cuda: cuda_hw,
            sws_csc,
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
            // 4:4:4 iff this session opened FREXT — the CPU swscale path or the zero-copy GPU
            // convert. RFI/HDR-SEI stay unsupported on libavcodec NVENC (the trait defaults).
            chroma_444: self.want_444,
            intra_refresh: self.intra_refresh,
            // NVENC intra-refresh is purpose-built GDR loss recovery (moving band + recovery-point
            // SEI): the wave heals a lost picture within one period, so mark the boundary AUs and let
            // the client re-anchor on them instead of forcing a full IDR. Tied to `intra_refresh`
            // (already the `PUNKTFUNK_INTRA_REFRESH` opt-in), unlike AMF/QSV which stay unvalidated.
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
        // Force an IDR when requested (client RFI); otherwise let NVENC pick (GOP/P-frame).
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

    /// Encode-stall recovery: drop the wedged libavcodec encoder and reopen it fresh with the
    /// session's negotiated parameters (the stored [`OpenArgs`]) — the drop-and-reopen lever the
    /// QSV/VAAPI paths use, so the encode-stall watchdog can heal a wedged NVENC/driver instead of
    /// ending the session. Owed AUs are forfeited; the fresh encoder opens on an IDR.
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
        // Non-blocking single drain: a packet ships, EAGAIN (need another input frame) and EOF
        // (drained after flush) both mean "nothing this tick".
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
        // swscale the packed RGB straight into the encoder's input frame, then send it. Serves all
        // three CSC users (see `open`): 4:4:4 → planar YUV444P, HDR → P010, and the packed 3-bpp
        // expand → `rgb0`/`bgr0`. The remaining branch below is the 4-bpp source, which needs no
        // conversion at all — just a row copy honouring the destination stride.
        if let Some(sws) = self.sws_csc {
            let frame = self
                .frame
                .as_mut()
                .context("CPU frame missing (encoder opened in CUDA mode)")?;
            // SAFETY: `format == self.src_format` and `bytes.len() >= src_row * h` (the `ensure!`s
            // above), so `sws_scale` reads `h` rows of `src_row` bytes from `src_data[0] = bytes`
            // (packed RGB is single-plane; the other src planes are null/0) — all in bounds. `sws` is
            // the non-null context built in `open`. The dst is `frame`'s underlying `AVFrame`, whose
            // `data`/`linesize` in-struct arrays were sized by `VideoFrame::new` for the very
            // `nvenc_pixel` this context was built to output — 3 planes of `width`×`height` for
            // YUV444P, 2 for P010, 1 packed plane for `rgb0`/`bgr0` — so swscale writes exactly the
            // planes it allocated, at the strides it reports. All pointers are live locals for this
            // synchronous call; the encoder runs only on this thread (`unsafe impl Send`), so no
            // aliasing/race.
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
            // 4-bpp → 4-bpp, honoring the (possibly larger) dst stride. The 3-bpp expand that used
            // to live here as a per-pixel loop is now swscale's job (see the branch above).
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

    /// Zero-copy path: hand the imported CUDA device buffer to NVENC with no CPU touch.
    ///
    /// We take a *pooled* surface from the CUDA hwframes context (`av_hwframe_get_buffer`) and
    /// device→device-copy our imported buffer into it, rather than wrapping our own pointer in a
    /// bare frame. Two reasons: (1) NVENC's `nvenc_send_frame` ignores frames whose `buf[0]` is
    /// null and the generic encode path's `av_frame_ref` needs a refcounted buffer — a bare
    /// frame is rejected with `EINVAL`; (2) NVENC caches CUDA-resource *registrations* keyed by
    /// device pointer with a bounded table, so a fresh pointer every frame would thrash/overflow
    /// it — the pool recycles a small set of pointers. The extra copy is device-local (~8 MB at
    /// 1080p, sub-millisecond on the GPU) and keeps the host fully off the pixel path.
    fn submit_cuda(&mut self, buf: &pf_zerocopy::DeviceBuffer, pts: i64, idr: bool) -> Result<()> {
        let frames_ref = self
            .cuda
            .as_ref()
            .context("CUDA hw context missing (encoder opened in CPU mode)")?
            .frames_ref;
        // The device→device copy below uses our shared context directly; make it current on the
        // encode thread (ffmpeg pushes its own around the pool alloc, so order is fine).
        pf_zerocopy::cuda::make_current().context("CUDA context current (encode thread)")?;
        // SAFETY: `frames_ref` is the non-null CUDA frames ctx from `self.cuda` (unwrapped via
        // `.context(..)?` above), and the shared CUDA context was just made current on THIS thread
        // (`make_current()?`), the precondition for the device-pointer copies below.
        //  * `av_frame_alloc` → `f` (null-checked). `av_hwframe_get_buffer(frames_ref, f, 0)` fills `f`
        //    with a pooled CUDA surface (sets `data[]`/`linesize[]`/`buf[0]`/`hw_frames_ctx`); on
        //    failure we free `f` and bail.
        //  * For NV12 we read `(*f).data[0..2]` / `linesize[0..2]` (Y + interleaved UV), else
        //    `data[0]`/`linesize[0]` — in-struct fields of the non-null `f`, valid for the surface dims
        //    ffmpeg allocated — and pass them to the cuda copy helpers, which device→device copy `buf`
        //    (the imported `DeviceBuffer`, owned by the caller and live for this call) into the surface.
        //  * On copy error we free `f` and return. Otherwise we write `pts`/`pict_type` through `f` and
        //    `avcodec_send_frame` it into the live owned `self.enc` context (which takes its own ref of
        //    the pooled surface), then free our `f` ref exactly once. Single-threaded encoder → no race.
        unsafe {
            let mut f = ffi::av_frame_alloc();
            if f.is_null() {
                bail!("av_frame_alloc failed");
            }
            // Pooled CUDA surface: sets format, width/height, data[0]/linesize[0], buf[0] and
            // hw_frames_ctx. Reused across frames (the pool recycles), keeping NVENC's
            // registration cache warm.
            let r = ffi::av_hwframe_get_buffer(frames_ref, f, 0);
            if r < 0 {
                ffi::av_frame_free(&mut f);
                bail!("av_hwframe_get_buffer(CUDA) failed ({r})");
            }
            // NV12 surfaces are two-plane (Y in data[0], interleaved UV in data[1]); YUV444
            // surfaces are three-plane (`yuv444p` frames ctx — data[0..3]); the RGB surfaces are
            // single-plane. Copy the matching layout into NVENC's pooled surface. A 4:4:4 session
            // whose buffer ISN'T YUV444 (a LINEAR/gamescope capture the worker can't convert)
            // fails loudly here rather than letting `hevc_nvenc` silently subsample RGB to 4:2:0.
            let copy_res = if buf.yuv444 {
                let dsts = core::array::from_fn(|i| {
                    (
                        (*f).data[i] as pf_zerocopy::cuda::CUdeviceptr,
                        (*f).linesize[i] as usize,
                    )
                });
                pf_zerocopy::cuda::copy_yuv444_to_device(buf, dsts, true)
            } else if self.want_444 {
                ffi::av_frame_free(&mut f);
                bail!(
                    "4:4:4 session but the zero-copy frame is not YUV444 (LINEAR/gamescope \
                     capture has no GPU 4:4:4 convert) — unset PUNKTFUNK_ZEROCOPY to use the \
                     CPU 4:4:4 path on this compositor"
                );
            } else if buf.is_nv12() {
                let y_ptr = (*f).data[0] as pf_zerocopy::cuda::CUdeviceptr;
                let y_pitch = (*f).linesize[0] as usize;
                let uv_ptr = (*f).data[1] as pf_zerocopy::cuda::CUdeviceptr;
                let uv_pitch = (*f).linesize[1] as usize;
                pf_zerocopy::cuda::copy_nv12_to_device(buf, y_ptr, y_pitch, uv_ptr, uv_pitch, true)
            } else {
                let dst_ptr = (*f).data[0] as pf_zerocopy::cuda::CUdeviceptr;
                let dst_pitch = (*f).linesize[0] as usize;
                pf_zerocopy::cuda::copy_device_to_device(buf, dst_ptr, dst_pitch, true)
            };
            if let Err(e) = copy_res {
                ffi::av_frame_free(&mut f);
                return Err(e).context("copy imported buffer into NVENC surface");
            }
            (*f).pts = pts;
            (*f).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), f);
            ffi::av_frame_free(&mut f);
            if r < 0 {
                bail!("avcodec_send_frame(CUDA) failed ({r})");
            }
        }
        Ok(())
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if let Some(sws) = self.sws_csc.take() {
            // SAFETY: `sws` is the non-null `SwsContext` allocated by `sws_getContext` in `open` and
            // owned exclusively by this encoder (taken out of the field so it can't be freed twice).
            // `sws_freeContext` frees it; nothing else references it after this single-threaded drop.
            unsafe { ffi::sws_freeContext(sws) };
        }
    }
}

/// Probe whether this NVIDIA GPU + driver + libavcodec can actually encode HEVC **4:4:4** (Range
/// Extensions). Opens a tiny real `hevc_nvenc` 4:4:4 session — the exact path [`NvencEncoder::open`]
/// takes for a live 4:4:4 stream — and reports whether it succeeded. HEVC-only; the result is cached
/// by the caller ([`crate::can_encode_444`]). A GPU/driver/ffmpeg without RExt 4:4:4 fails
/// the open here, so the host resolves the session to 4:2:0 before the Welcome (honest downgrade).
pub fn probe_can_encode_444(codec: Codec) -> bool {
    if codec != Codec::H265 {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // Quiet ffmpeg's open error on a GPU that lacks 4:4:4 — the probe failing is an expected outcome.
    // SAFETY: libav initialized above; `av_log_{get,set}_level` only read/write the global int level
    // (no pointer args) and are always sound post-init.
    let prev = unsafe {
        let p = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_FATAL);
        p
    };
    let ok = NvencEncoder::open(
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
    .is_ok();
    // SAFETY: restore the saved global log level (scalar arg, no pointers).
    unsafe { ffi::av_log_set_level(prev) };
    ok
}

/// Probe whether this NVIDIA GPU + driver + libavcodec can actually encode 10-bit (HEVC Main10 /
/// 10-bit AV1) from a P010 input — the exact path [`NvencEncoder::open`] takes for a live HDR
/// stream (a tiny X2RGB10-sourced, P010-input open). The result is cached by the caller
/// ([`crate::can_encode_10bit`]); a GPU/driver/ffmpeg without the 10-bit encode fails the open
/// here, so the host resolves the session to 8-bit SDR before the Welcome (honest downgrade).
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    if ffmpeg::init().is_err() {
        return false;
    }
    // Quiet ffmpeg's open error on a GPU that lacks 10-bit — the probe failing is an expected outcome.
    // SAFETY: libav initialized above; `av_log_{get,set}_level` only read/write the global int level
    // (no pointer args) and are always sound post-init.
    let prev = unsafe {
        let p = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_FATAL);
        p
    };
    let ok = NvencEncoder::open(
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
    .is_ok();
    // SAFETY: restore the saved global log level (scalar arg, no pointers).
    unsafe { ffi::av_log_set_level(prev) };
    ok
}

#[cfg(test)]
mod hdr_tests {
    use super::*;

    /// The Linux HDR (GNOME 50 portal) encode path end-to-end on a real NVIDIA GPU: a synthetic
    /// PQ-ish X2RGB10 CPU frame → swscale BT.2020 → P010 → `hevc_nvenc` Main10, drained to a real
    /// AU. `#[ignore]`d (needs NVENC):
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
        // Packed x:R:G:B 2:10:10:10 gradient (values are treated as PQ-encoded — fine for a smoke).
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
        // PF_HDR_SMOKE_DUMP=/path.h265: write the Annex-B AU for external inspection —
        // `ffprobe -show_streams` should report Main 10, bt2020nc/smpte2084/bt2020 colours.
        if let Ok(path) = std::env::var("PF_HDR_SMOKE_DUMP") {
            std::fs::write(&path, &au.data).expect("dump AU");
            println!("HDR10 smoke: AU written to {path}");
        }
    }
}
