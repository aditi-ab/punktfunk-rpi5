//! Intel **QSV** (and, retained for the latency A/B, AMD **AMF**) libavcodec encode on Windows.
//! Analogue of Linux [`super::vaapi`]; sibling of the direct-SDK [`super::nvenc`] path. Encoder
//! name selects the vendor (`*_qsv` / `*_amf`). Evidence: `design/native-amf-encoder.md`.
//!
//! Production dispatch: [`super::open_video`] sends AMD to [`super::amf`], not here — the
//! libavcodec AMF wrapper holds ~2 frames and can wedge silently. `WinVendor::Amf` remains
//! because `amf::tests::amf_latency_ab_bench` compares native vs libavcodec; treat those arms
//! as benchmark-only.
//!
//! Input is the capturer's `FramePayload::D3d11` on its `ID3D11Device`. [`SystemInner`] reads
//! back to CPU NV12/P010 (QSV default; fallback if zero-copy open fails). [`ZeroCopyInner`]
//! wraps that same device as D3D11VA (capture textures are not shared-handle — a second
//! device cannot read them), copies GPU-local into an FFmpeg pool, and feeds AMF D3D11 or a
//! derived QSV surface. QSV zero-copy is opt-in (`PUNKTFUNK_ZEROCOPY=1`); a derive that opens
//! but maps wrong would corrupt silently. FFI via `ffmpeg::ffi`; D3D11VA layouts are mirrored
//! because the bindings omit `hwcontext_d3d11va.h`.

use super::{ChromaFormat, Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary};
use ffmpeg_next as ffmpeg;
use pf_frame::{dxgi::D3d11Frame, CapturedFrame, FramePayload, PixelFormat};
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BIND_VIDEO_ENCODER, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_SAMPLE_DESC,
};

use super::libav::{
    apply_low_latency_rc, pixel_to_av, poll_encoder, AvBuffer, AvFrame, AvSwsContext, PollOutcome,
    SWS_CS_BT2020, SWS_CS_ITU709, SWS_POINT,
};
use ffmpeg::ffi;

/// Mirrored `AVD3D11VADeviceContext` (`hwcontext_d3d11va.h` is not in the ffmpeg-sys allowlist).
/// Store the capturer's `ID3D11Device` in `device`; `av_hwdevice_ctx_init` fills the rest.
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: *mut c_void,
    unlock: *mut c_void,
    lock_ctx: *mut c_void,
    // Stop at the FFmpeg 7.1 prefix: 8+ appends BindFlags/MiscFlags. libav sizes the alloc;
    // we only write `device` at offset 0. Matching 8 would mis-describe the Windows 7.1 ship.
}

/// Mirrored `AVD3D11VAFramesContext`. Null `texture` lets FFmpeg allocate the pool array;
/// `bind_flags`/`misc_flags` customise that array. `texture_infos` is FFmpeg-owned — never write.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void,
    bind_flags: c_uint,
    misc_flags: c_uint,
    texture_infos: *mut c_void,
}

// These asserts pin OUR layout, not libav's. A field FFmpeg inserts still compiles; re-check
// `hwcontext_d3d11va.h` on the next FFmpeg major. A wrong offset here is silent corruption.
const _: () = {
    use std::mem::{offset_of, size_of};
    type P = *mut c_void;
    assert!(size_of::<AVD3D11VADeviceContext>() == 7 * size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, device) == 0);
    assert!(offset_of!(AVD3D11VADeviceContext, device_context) == size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, video_device) == 2 * size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, video_context) == 3 * size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, lock) == 4 * size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, unlock) == 5 * size_of::<P>());
    assert!(offset_of!(AVD3D11VADeviceContext, lock_ctx) == 6 * size_of::<P>());
    // ptr, u32, u32, ptr — the two 32-bit flags pack into one pointer-sized slot with no padding.
    assert!(size_of::<AVD3D11VAFramesContext>() == 3 * size_of::<P>());
    assert!(offset_of!(AVD3D11VAFramesContext, texture) == 0);
    assert!(offset_of!(AVD3D11VAFramesContext, bind_flags) == size_of::<P>());
    assert!(offset_of!(AVD3D11VAFramesContext, misc_flags) == size_of::<P>() + 4);
    assert!(offset_of!(AVD3D11VAFramesContext, texture_infos) == 2 * size_of::<P>());
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinVendor {
    /// Benchmark-only: production AMD goes through [`super::amf`]. The lib target never
    /// constructs this (`dead_code`); `amf::tests::amf_latency_ab_bench` is the remaining caller.
    #[allow(dead_code)]
    Amf,
    Qsv,
}

impl WinVendor {
    fn encoder_name(self, codec: Codec) -> &'static str {
        match self {
            WinVendor::Amf => codec.amf_name(),
            WinVendor::Qsv => codec.qsv_name(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            WinVendor::Amf => "AMF",
            WinVendor::Qsv => "QSV",
        }
    }
}

/// `PUNKTFUNK_ZEROCOPY` override, else AMF on / QSV off. QSV stays opt-in: open-failure fallback
/// catches setup errors only; a derive that opens but maps wrong would corrupt silently.
fn zerocopy_enabled(vendor: WinVendor) -> bool {
    zerocopy_active(pf_host_config::config().zerocopy, vendor)
}

/// Pure half of [`zerocopy_enabled`] (tests). Operator override wins; unset is AMF on, QSV off.
fn zerocopy_active(override_: Option<bool>, vendor: WinVendor) -> bool {
    override_.unwrap_or(matches!(vendor, WinVendor::Amf))
}

/// Cap on `PUNKTFUNK_FFWIN_POLL_MS`. The knob spins the encode thread; past one frame period is
/// already useless. Clamping here is what makes the µs conversion overflow-free.
const MAX_POLL_SPIN_MS: u64 = 1_000;

/// Post-submit spin for [`FfmpegWinEncoder::poll`], microseconds, latched once per process
/// (`poll` is per tick). 0 = off.
///
/// Do not `saturating_mul` the ms→µs step: `Duration::from_micros(u64::MAX)` does not overflow
/// `Instant` (~584 kyr), and the loop exits only on Packet/Eof, so a saturated bad value wedges
/// the encode thread. Clamp the parsed ms to [`MAX_POLL_SPIN_MS`] first.
fn poll_spin_cap_us() -> u64 {
    static CAP_US: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CAP_US.get_or_init(|| {
        parse_poll_spin_cap_us(std::env::var("PUNKTFUNK_FFWIN_POLL_MS").ok().as_deref())
    })
}

/// Clamp to [`MAX_POLL_SPIN_MS`] **before** `* 1000`. Default 0: the libavcodec AMF hold cannot
/// be spun out.
fn parse_poll_spin_cap_us(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok())
        .map(|ms| ms.min(MAX_POLL_SPIN_MS) * 1000)
        .unwrap_or(0)
}

fn sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ,
        PixelFormat::Rgbx => Pixel::RGBZ,
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        // Exhaustive on purpose: a new PixelFormat must re-break this match. X2Rgb10/X2Bgr10
        // are Linux HDR screencast layouts and cannot arrive on this Windows capture path.
        PixelFormat::Nv12
        | PixelFormat::P010
        | PixelFormat::Rgb10a2
        | PixelFormat::Rgb10a2Sdr
        | PixelFormat::Yuv444
        | PixelFormat::X2Rgb10
        | PixelFormat::X2Bgr10 => {
            bail!("ffmpeg_win swscale path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Depth follows the pixels, not negotiated `bit_depth` ([`crate::ten_bit_input`]). A 10-bit
/// session over 8-bit capture would otherwise open P010 and fail every `submit_d3d11` forever —
/// `reset()` rebuilds from the same wrong answer.
fn is_10bit_format(format: PixelFormat) -> bool {
    // Rgb10a2Sdr cannot arrive: the 10-bit SDR chain is gated to direct-NVENC at handshake.
    matches!(
        format,
        PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Rgb10a2Sdr
    )
}

/// System-path lane for a captured D3D11 format. Device-free so the routing table is testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadbackRoute {
    Yuv,
    Bgra,
    Rgb10,
}

/// Guard fires only on a genuine mid-stream depth change: `ten_bit` is what the encoder was
/// built from ([`is_10bit_format`]), not the negotiated session depth.
fn readback_route(format: PixelFormat, ten_bit: bool) -> Result<ReadbackRoute> {
    anyhow::ensure!(
        is_10bit_format(format) == ten_bit,
        "captured format {format:?} bit-depth changed under the encoder (built {}-bit)",
        if ten_bit { 10 } else { 8 }
    );
    Ok(match format {
        PixelFormat::Nv12 | PixelFormat::P010 => ReadbackRoute::Yuv,
        PixelFormat::Bgra | PixelFormat::Bgrx => ReadbackRoute::Bgra,
        PixelFormat::Rgb10a2 => ReadbackRoute::Rgb10,
        other => {
            bail!("ffmpeg_win system path cannot read back captured D3D11 format {other:?}")
        }
    })
}

/// Per-vendor low-latency dict for [`open_win_encoder`]. Unknown keys are ignored by
/// `avcodec_open2`, so codec-specific entries are safe to set unconditionally.
fn vendor_opts(vendor: WinVendor, amf_usage: &str) -> Vec<(&'static str, String)> {
    match vendor {
        WinVendor::Amf => vec![
            ("usage", amf_usage.to_owned()),
            ("rc", "cbr".into()),
            // `speed` trims motion-estimation depth; matches the NVENC low-latency preset.
            ("quality", "speed".into()),
            ("preanalysis", "false".into()),
            ("enforce_hrd", "true".into()),
            ("latency", "true".into()), // FFmpeg ≥ 6.1; ignored on older
            // h264_amf defaults B-frames >0 on RDNA3+; each is a full frame period. Ignored on HEVC.
            ("bf", "0".into()),
            // VPS/SPS/PPS on each IDR — HEVC/AV1 only; ignored elsewhere.
            ("header_insertion_mode", "idr".into()),
        ],
        WinVendor::Qsv => vec![
            ("preset", "veryfast".into()),
            ("async_depth", "1".into()), // in-flight cap — the QSV latency lever
            ("low_power", "1".into()),   // VDEnc fixed-function path
            ("look_ahead", "0".into()),  // h264_qsv only; ignored on hevc/av1
            ("forced_idr", "1".into()),
            ("scenario", "displayremoting".into()),
        ],
    }
}

/// Zero-copy pool bind flags. AMF: encoder-input (RENDER_TARGET | SHADER_RESOURCE). QSV: mfx
/// surface (DECODER | VIDEO_ENCODER). The GPU copy into the pool accepts any DEFAULT texture.
fn pool_bind_flags(vendor: WinVendor) -> u32 {
    match vendor {
        WinVendor::Amf => (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        WinVendor::Qsv => (D3D11_BIND_DECODER.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
    }
}

/// Shared encoder open: low-latency RC, infinite GOP, BT.709-limited or BT.2020-PQ VUI.
/// `device_ref`/`frames_ref` null = system path.
#[allow(clippy::too_many_arguments)]
unsafe fn open_win_encoder(
    vendor: WinVendor,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    pix_fmt: ffi::AVPixelFormat,
    sw_pix_fmt: ffi::AVPixelFormat,
    ten_bit: bool,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
) -> Result<encoder::video::Encoder> {
    let name = vendor.encoder_name(codec);
    let av_codec = encoder::find_by_name(name).ok_or_else(|| {
        anyhow!(
            "{name} not built into libavcodec (no {} encoder)",
            vendor.label()
        )
    })?;
    let mut video = codec::context::Context::new_with_codec(av_codec)
        .encoder()
        .video()
        .context("alloc video encoder")?;
    video.set_width(width);
    video.set_height(height);
    // Software layout (NV12/P010). Hw paths override `pix_fmt` to D3D11/QSV; libav still
    // stores this as `sw_pix_fmt`.
    video.set_format(Pixel::from(sw_pix_fmt));
    apply_low_latency_rc(&mut video, fps, bitrate_bps);
    // SAFETY: `as_mut_ptr` is the `AVCodecContext` behind `video`, which outlives these writes
    // (opened and returned below). gop/colour/pix_fmt stores are in-bounds scalars.
    // `device_ref`/`frames_ref` are live `AVBufferRef`s or null (system path); the null
    // guards keep `av_buffer_ref` off that case. Each call returns a NEW ref the codec
    // adopts and unrefs — share, do not take over.
    let raw = unsafe { video.as_mut_ptr() };
    // SAFETY: `raw` is that live context; stores below are in-bounds; `av_buffer_ref` is
    // null-guarded.
    unsafe {
        (*raw).gop_size = i32::MAX; // no periodic IDR; RFI forces via pict_type=I
        if ten_bit {
            // Client auto-detects PQ from HEVC VUI; mastering metadata rides 0xCE out-of-band.
            (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
            (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
            (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        } else {
            // Input is BT.709 limited NV12; omit the VUI and the client washes the picture out.
            (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
            (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
        }
        (*raw).pix_fmt = pix_fmt;
        if !device_ref.is_null() {
            (*raw).hw_device_ctx = ffi::av_buffer_ref(device_ref);
        }
        if !frames_ref.is_null() {
            (*raw).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);
        }
    }

    let mut opts = Dictionary::new();
    let usage = std::env::var("PUNKTFUNK_AMF_USAGE").unwrap_or_else(|_| "ultralowlatency".into());
    for (k, v) in vendor_opts(vendor, &usage) {
        opts.set(k, &v);
    }
    video
        .open_with(opts)
        .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))
}

/// Always `false`: a wrong HEVC 4:4:4 profile `avcodec_open2` silently encodes 4:2:0, so a
/// positive probe would need a verify-by-frame. Negotiation stays 4:2:0.
pub fn probe_can_encode_444(_vendor: WinVendor, _codec: Codec) -> bool {
    tracing::debug!("AMF/QSV HEVC 4:4:4 encode not implemented — declining (4:2:0)");
    false
}

/// Tiny system-input open of `vendor`/`codec`. Compiled only when native VPL is out
/// (`not(feature = "qsv")`); the shipped `nvenc,amf-qsv,qsv` combo answers via `qsv::probe_can_encode`.
#[cfg(not(feature = "qsv"))]
pub fn probe_can_encode(vendor: WinVendor, codec: Codec) -> bool {
    // Not pinned to the selected adapter: no hwdevice is passed, and each runtime binds its own
    // vendor. Mixed-vendor boxes land on the right GPU; two same-vendor GPUs can miss (accepted;
    // `windows_codec_support` caches per selected GPU).
    if ffmpeg::init().is_err() {
        return false;
    }
    // SAFETY: `ffmpeg::init()` succeeded. `av_log_get_level`/`av_log_set_level` are scalar
    // getters/setters. `open_win_encoder` is called with null device/frames (system path), so
    // it touches no D3D11; the encoder drops at `.is_ok()`. Log level is restored; no raw
    // pointer escapes.
    unsafe {
        // Missing runtime is an expected probe miss — mute ffmpeg's open error, then restore.
        let prev = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_FATAL);
        let ok = open_win_encoder(
            vendor,
            codec,
            640,
            480,
            30,
            2_000_000,
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            ffi::AVPixelFormat::AV_PIX_FMT_NV12,
            false,
            ptr::null_mut(),
            ptr::null_mut(),
        )
        .is_ok();
        ffi::av_log_set_level(prev);
        ok
    }
}

fn immediate_context(device: &ID3D11Device) -> ID3D11DeviceContext {
    // SAFETY: COM call on the live `device` borrow; no pointers. Every D3D11 device has an
    // immediate context, so the `expect` is unreachable in practice.
    unsafe {
        device
            .GetImmediateContext()
            .expect("ID3D11Device always has an immediate context")
    }
}

struct SystemInner {
    enc: encoder::video::Encoder,
    // Drop order follows declaration. `sw_frame` must drop before `sws`. `repr(Rust)` may
    // reorder memory, so an offset_of assert cannot pin this — do not reorder these fields.
    sw_frame: AvFrame,
    sws: Option<AvSwsContext>,
    staging: Option<ID3D11Texture2D>,
    ctx: Option<ID3D11DeviceContext>,
    format: PixelFormat,
    ten_bit: bool,
    width: u32,
    height: u32,
}

impl SystemInner {
    #[allow(clippy::too_many_arguments)]
    fn open(
        vendor: WinVendor,
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
    ) -> Result<Self> {
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        let sw_av = if ten_bit {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        // SAFETY: `open_win_encoder` with null device/frames (system path); remaining args are
        // scalars. The returned encoder owns its `AVCodecContext`; no raw pointer is aliased.
        let enc = unsafe {
            open_win_encoder(
                vendor,
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                sw_av, // system input: pix_fmt == sw_format (no hw frames ctx)
                sw_av,
                ten_bit,
                ptr::null_mut(),
                ptr::null_mut(),
            )?
        };
        let sw_frame = AvFrame::alloc().context("av_frame_alloc(sw) failed")?;
        // SAFETY: format/width/height writes stay inside the owned frame. `av_frame_get_buffer`
        // allocates the planes; on failure `sw_frame` drops once via the wrapper.
        unsafe {
            (*sw_frame.as_ptr()).format = sw_av as c_int;
            (*sw_frame.as_ptr()).width = width as c_int;
            (*sw_frame.as_ptr()).height = height as c_int;
            if ffi::av_frame_get_buffer(sw_frame.as_ptr(), 0) < 0 {
                bail!("av_frame_get_buffer(sw) failed");
            }
        }
        tracing::info!(
            encoder = vendor.encoder_name(codec),
            "{} encode active ({width}x{height}@{fps}, system-memory {} path)",
            vendor.label(),
            if ten_bit { "P010" } else { "NV12" }
        );
        Ok(SystemInner {
            enc,
            sw_frame,
            sws: None,
            staging: None,
            ctx: None,
            format,
            ten_bit,
            width,
            height,
        })
    }

    fn ensure_staging(&mut self, device: &ID3D11Device, dxgi_fmt: DXGI_FORMAT) -> Result<()> {
        if self.staging.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi_fmt,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        // SAFETY: `CreateTexture2D` on the live `device` borrow; `desc` is fully initialized;
        // the `Option` out-param is live.
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D(staging readback)")?;
        }
        self.staging = t;
        self.ctx = Some(immediate_context(device));
        Ok(())
    }

    fn send(&mut self, pts: i64, idr: bool) -> Result<()> {
        // SAFETY: `sw_frame` is this struct's owned `AVFrame`; the two stores are in-bounds.
        // `avcodec_send_frame` borrows that frame and `enc`'s context for the call; libav refs
        // the frame's buffers itself and retains neither pointer.
        unsafe {
            (*self.sw_frame.as_ptr()).pts = pts;
            (*self.sw_frame.as_ptr()).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), self.sw_frame.as_ptr());
            if r < 0 {
                bail!("avcodec_send_frame({} system) failed ({r})", "ffmpeg_win");
            }
        }
        Ok(())
    }

    /// Dispatch on this frame's format: the video processor can latch off mid-session
    /// (NV12→Bgra / P010→Rgb10a2), so the open-time format is not the route.
    fn submit_d3d11(
        &mut self,
        frame: &D3d11Frame,
        format: PixelFormat,
        pts: i64,
        idr: bool,
    ) -> Result<()> {
        match readback_route(format, self.ten_bit)? {
            ReadbackRoute::Yuv => self.readback_yuv(frame, pts, idr),
            ReadbackRoute::Bgra => self.readback_bgra(frame, pts, idr),
            ReadbackRoute::Rgb10 => self.readback_rgb10(frame, pts, idr),
        }
    }

    fn readback_yuv(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        let dxgi_fmt = if self.ten_bit {
            DXGI_FORMAT_P010
        } else {
            DXGI_FORMAT_NV12
        };
        // SAFETY: staging is CPU_ACCESS_READ on `frame.device` (same device as `frame.texture`),
        // matching NV12/P010 size, so `CopyResource` on the immediate context is valid.
        // `Map(READ)` yields `pData` for the whole resource: Y is `H` rows at `RowPitch`, chroma
        // starts at `RowPitch*H` (`H/2` rows), so `total = pitch*(H+⌈H/2⌉)` is the mapped extent.
        // Each copy reads `row_bytes ≤ pitch` from `mapped` and writes `row_bytes ≤ linesize` at
        // row `y < H` in the `av_frame_get_buffer` planes; src and dst do not alias. `Unmap`
        // pairs `Map`; `send` then hands `sw_frame` to the encoder.
        unsafe {
            self.ensure_staging(&frame.device, dxgi_fmt)?;
            let staging = self.staging.clone().context("staging texture")?;
            let ctx = self.ctx.clone().context("d3d11 context")?;
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = staging.cast().context("staging -> resource")?;
            ctx.CopyResource(&dst, &src);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .context("Map staging (yuv readback)")?;
            let pitch = map.RowPitch as usize;
            let h = self.height as usize;
            // NV12/P010: Y is rows [0,H) at `pitch`; chroma (H/2 rows) starts at `pitch * H`.
            // P010 samples are 16-bit, so a width-pixel row is `width*2` bytes (chroma too).
            let bytes_per_sample = if self.ten_bit { 2 } else { 1 };
            let row_bytes = self.width as usize * bytes_per_sample;
            let base = map.pData as *const u8;
            let total = pitch.saturating_mul(h + h.div_ceil(2));
            let mapped = std::slice::from_raw_parts(base, total);
            let chroma_off = pitch * h;
            let y_dst = (*self.sw_frame.as_ptr()).data[0];
            let y_stride = (*self.sw_frame.as_ptr()).linesize[0] as usize;
            let uv_dst = (*self.sw_frame.as_ptr()).data[1];
            let uv_stride = (*self.sw_frame.as_ptr()).linesize[1] as usize;
            for y in 0..h {
                let s = &mapped[y * pitch..y * pitch + row_bytes];
                ptr::copy_nonoverlapping(s.as_ptr(), y_dst.add(y * y_stride), row_bytes);
            }
            for y in 0..h.div_ceil(2) {
                let s = &mapped[chroma_off + y * pitch..chroma_off + y * pitch + row_bytes];
                ptr::copy_nonoverlapping(s.as_ptr(), uv_dst.add(y * uv_stride), row_bytes);
            }
            ctx.Unmap(&staging, 0);
            self.send(pts, idr)
        }
    }

    fn readback_bgra(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        if self.ten_bit {
            bail!("ffmpeg_win: BGRA readback is 8-bit only (HDR needs the P010 capture path)");
        }
        // SAFETY: B8G8R8A8 staging on `frame.device`; `src`/`dst` match, so `CopyResource` is
        // valid. `Map(READ)` yields `base` for `pitch*h` rows. `sws_scale` reads that extent
        // into `sw_frame`'s NV12 planes (`width`×`height`). `Unmap` pairs `Map`; `sws` drops
        // once with `self`. Mapped read never aliases the encoder frame.
        unsafe {
            self.ensure_staging(&frame.device, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            let staging = self.staging.clone().context("staging texture")?;
            let ctx = self.ctx.clone().context("d3d11 context")?;
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = staging.cast().context("staging -> resource")?;
            ctx.CopyResource(&dst, &src);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .context("Map staging (bgra readback)")?;
            let pitch = map.RowPitch as usize;
            let h = self.height as usize;
            let base = map.pData as *const u8;
            let sws = self.ensure_sws(
                pixel_to_av(Pixel::BGRA),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_CS_ITU709,
            )?;
            let src_data: [*const u8; 4] = [base, ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [pitch as c_int, 0, 0, 0];
            let r = ffi::sws_scale(
                sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame.as_ptr()).data.as_ptr(),
                (*self.sw_frame.as_ptr()).linesize.as_ptr(),
            );
            ctx.Unmap(&staging, 0);
            if r < 0 {
                bail!("sws_scale BGRA→NV12 failed");
            }
            self.send(pts, idr)
        }
    }

    /// DXGI `R10G10B10A2_UNORM` (R in the low 10 bits) == FFmpeg `AV_PIX_FMT_X2BGR10LE`.
    fn readback_rgb10(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        // SAFETY: R10G10B10A2 staging on `frame.device`; matching-format `CopyResource` is valid.
        // `Map(READ)` yields `base` for `pitch*h`. `sws_scale` reads that into `sw_frame`'s P010
        // planes. `Unmap` pairs `Map`; `sws` drops once with `self`. Read and write do not alias.
        unsafe {
            self.ensure_staging(&frame.device, DXGI_FORMAT_R10G10B10A2_UNORM)?;
            let staging = self.staging.clone().context("staging texture")?;
            let ctx = self.ctx.clone().context("d3d11 context")?;
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = staging.cast().context("staging -> resource")?;
            ctx.CopyResource(&dst, &src);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .context("Map staging (rgb10 readback)")?;
            let pitch = map.RowPitch as usize;
            let h = self.height as usize;
            let base = map.pData as *const u8;
            // Same PQ transfer; matrix-only RGB(BT.2020) → YUV(BT.2020), full → limited.
            let sws = self.ensure_sws(
                ffi::AVPixelFormat::AV_PIX_FMT_X2BGR10LE,
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
                SWS_CS_BT2020,
            )?;
            let src_data: [*const u8; 4] = [base, ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [pitch as c_int, 0, 0, 0];
            let r = ffi::sws_scale(
                sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame.as_ptr()).data.as_ptr(),
                (*self.sw_frame.as_ptr()).linesize.as_ptr(),
            );
            ctx.Unmap(&staging, 0);
            if r < 0 {
                bail!("sws_scale Rgb10a2→P010 failed");
            }
            self.send(pts, idr)
        }
    }

    /// `FramePayload::Cpu` is DDA without the video processor (8-bit packed RGB/BGR → NV12).
    fn submit_cpu(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.format,
            "captured format {format:?} != encoder source {:?}",
            self.format
        );
        if self.ten_bit {
            bail!("ffmpeg_win: CPU swscale path is 8-bit only");
        }
        let w = self.width as usize;
        let h = self.height as usize;
        let src_row = w * format.bytes_per_pixel();
        anyhow::ensure!(bytes.len() >= src_row * h, "captured buffer too small");
        // SAFETY: `src_data[0]`/`src_stride[0]` cover `src_row*h` bytes (`ensure!` above).
        // `sws_scale` reads that and writes `sw_frame`'s NV12 planes. `bytes` is borrowed for
        // the call and does not alias `sw_frame`. `send` then hands `sw_frame` to the encoder.
        unsafe {
            let sws = self.ensure_sws(
                pixel_to_av(sws_src(format)?),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_CS_ITU709,
            )?;
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame.as_ptr()).data.as_ptr(),
                (*self.sw_frame.as_ptr()).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            self.send(pts, idr)
        }
    }

    /// One src→dst conversion per `SystemInner` (RGB→NV12 BT.709 or RGB10→P010 BT.2020), so a
    /// single cached context is sound. Returns a borrow; `self.sws` stays the owner.
    fn ensure_sws(
        &mut self,
        src_av: ffi::AVPixelFormat,
        dst_av: ffi::AVPixelFormat,
        cs: c_int,
    ) -> Result<*mut ffi::SwsContext> {
        if let Some(sws) = &self.sws {
            return Ok(sws.as_ptr());
        }
        // SAFETY: `sws_getContext` takes scalars plus a documented null-filter trio and returns
        // an owned context or null. `from_raw` rejects null, so `sws_setColorspaceDetails` sees
        // a live one. `sws_getCoefficients` points into libav's static tables for the process.
        let sws = unsafe {
            let raw = ffi::sws_getContext(
                self.width as c_int,
                self.height as c_int,
                src_av,
                self.width as c_int,
                self.height as c_int,
                dst_av,
                SWS_POINT,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            let Some(owned) = AvSwsContext::from_raw(raw) else {
                bail!("sws_getContext(RGB→YUV) failed");
            };
            // Full-range RGB → limited YUV (matches the VUI). RGB ignores the src table; pass dst twice.
            let coeff = ffi::sws_getCoefficients(cs);
            ffi::sws_setColorspaceDetails(owned.as_ptr(), coeff, 1, coeff, 0, 0, 1 << 16, 1 << 16);
            owned
        };
        Ok(self.sws.insert(sws).as_ptr())
    }
}

// No `Drop`: `sw_frame` then `sws` drop in declaration order. Do not reorder those fields.

struct D3d11Hw {
    // Frames before device: the frames ctx holds its own ref on the device. Do not reorder.
    frames_ref: AvBuffer,
    device_ref: AvBuffer,
}

impl D3d11Hw {
    /// Wrap the capturer's `ID3D11Device` as a D3D11VA hwdevice + NV12/P010 frames pool.
    /// No raw pointer from the caller; the `unsafe` below is the libav/D3D11 FFI.
    fn new(
        device: &ID3D11Device,
        sw_format: ffi::AVPixelFormat,
        bind_flags: u32,
        w: u32,
        h: u32,
        pool: c_int,
    ) -> Result<Self> {
        // SAFETY: `av_hwdevice_ctx_alloc` returns null (rejected by `from_raw`, so `?` leaves)
        // or a ref whose `data` is an `AVHWDeviceContext`. For D3D11VA, `hwctx` is an
        // `AVD3D11VADeviceContext`, so `d11` addresses a live, correctly-typed struct.
        let (device_ref, d11) = unsafe {
            let device_ref = AvBuffer::from_raw(ffi::av_hwdevice_ctx_alloc(
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
            ))
            .context("av_hwdevice_ctx_alloc(D3D11VA) failed")?;
            let dev_ctx = (*device_ref.as_ptr()).data as *mut ffi::AVHWDeviceContext;
            let d11 = (*dev_ctx).hwctx as *mut AVD3D11VADeviceContext;
            (device_ref, d11)
        };

        // `d3d11va_device_init` (we supply the capturer device) does not SetMultithreadProtected;
        // `d3d11va_device_create` does. Without it QSV derive fails MFX SetHandle, and AMF's
        // default lock is Enter/Leave — a no-op until protection is on. Idempotent on a device
        // the capturer owns.
        match device.cast::<ID3D11Multithread>() {
            Ok(mt) => {
                // SAFETY: COM call on the live `ID3D11Multithread` from a checked `cast`; BOOL in,
                // previous state out.
                let was = unsafe { mt.SetMultithreadProtected(true) };
                tracing::debug!(
                    previously_protected = was.as_bool(),
                    "D3D11 multithread protection enabled for the libav hwdevice"
                );
            }
            // Pre-11.1 has no ID3D11Multithread. Let QSV derive fail later rather than fail capture.
            Err(e) => tracing::warn!(
                error = %e,
                "no ID3D11Multithread on this device — QSV zero-copy will not derive"
            ),
        }

        // FFmpeg Releases `device` at teardown: clone = AddRef, forget = keep ours.
        std::mem::forget(device.clone());
        // SAFETY: `d11` is the live context; storing the device pointer is in-bounds.
        // `forget(clone())` makes that pointer an owned ref matching libav's teardown Release.
        // `av_hwdevice_ctx_init` reads the field, so the store precedes it.
        let r = unsafe {
            (*d11).device = device.as_raw();
            ffi::av_hwdevice_ctx_init(device_ref.as_ptr())
        };
        if r < 0 {
            bail!("av_hwdevice_ctx_init(D3D11VA) failed ({r})");
        }

        // SAFETY: `av_hwframe_ctx_alloc` takes the live device ref and returns null (rejected
        // by `from_raw`) or a ref whose `data` is an `AVHWFramesContext` whose `hwctx` is
        // `AVD3D11VAFramesContext`. Stores are in-bounds and precede `av_hwframe_ctx_init`.
        let frames_ref = unsafe {
            let frames_ref = AvBuffer::from_raw(ffi::av_hwframe_ctx_alloc(device_ref.as_ptr()))
                .context("av_hwframe_ctx_alloc(D3D11VA) failed")?;
            let fc = (*frames_ref.as_ptr()).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
            (*fc).sw_format = sw_format;
            (*fc).width = w as c_int;
            (*fc).height = h as c_int;
            (*fc).initial_pool_size = pool;
            let f11 = (*fc).hwctx as *mut AVD3D11VAFramesContext;
            (*f11).bind_flags = bind_flags;
            let r = ffi::av_hwframe_ctx_init(frames_ref.as_ptr());
            if r < 0 {
                bail!("av_hwframe_ctx_init(D3D11VA) failed ({r})");
            }
            frames_ref
        };
        Ok(D3d11Hw {
            frames_ref,
            device_ref,
        })
    }
}

// No `Drop`: each `AvBuffer` unrefs in declaration order (frames, then device).

struct ZeroCopyInner {
    vendor: WinVendor,
    /// QSV only (`None` for AMF). Frames before device, both before `enc`/`hw`: drop order
    /// releases the derived pair first. Refcounting makes any order sound; the pin is so a
    /// reorder cannot quietly change what ships.
    qsv_frames: Option<AvBuffer>,
    /// Owner only: send reads `qsv_frames`, not this. Removing it frees the QSV device while
    /// the frames ctx and encoder still hold refs. Same shape as the decoders' `hw_device`.
    #[allow(dead_code)]
    qsv_device: Option<AvBuffer>,
    enc: encoder::video::Encoder,
    hw: D3d11Hw,
    ctx: ID3D11DeviceContext,
    /// Pool sw_format (NV12/P010). A VP fallback to Bgra/Rgb10a2 cannot `CopySubresourceRegion`
    /// into this pool (format-group mismatch → UB); the caller drops to the system path.
    pool_format: PixelFormat,
}

impl ZeroCopyInner {
    #[allow(clippy::too_many_arguments)]
    fn open(
        vendor: WinVendor,
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        device: &ID3D11Device,
    ) -> Result<Self> {
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        let sw_av = if ten_bit {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        let pool_format = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let bind_flags = pool_bind_flags(vendor);
        const POOL: c_int = 8;
        // SAFETY: `D3d11Hw::new` AddRefs the capturer device (FFmpeg Releases at teardown) and
        // returns owned frames/device refs. QSV `create_derived` fills its out-param only on
        // `r >= 0`; each result is taken into `AvBuffer` immediately. Locals own every handle,
        // so `bail!`/`?` unref what exists. `open_win_encoder` takes its own refs of the
        // pointers it is handed, so `as_ptr()` transfers nothing; success moves owners into
        // `ZeroCopyInner`. Each `AVBufferRef` is unref'd exactly once on every path.
        unsafe {
            let hw = D3d11Hw::new(device, sw_av, bind_flags, width, height, POOL)?;
            // Own the derived pair here and lend pointers to the encoder. Handing the same
            // `AvBuffer` out twice would be two owners.
            let (qsv_frames, qsv_device) = match vendor {
                WinVendor::Amf => (None, None),
                WinVendor::Qsv => {
                    let mut qsv_device: *mut ffi::AVBufferRef = ptr::null_mut();
                    let r = ffi::av_hwdevice_ctx_create_derived(
                        &mut qsv_device,
                        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                        hw.device_ref.as_ptr(),
                        0,
                    );
                    if r < 0 {
                        bail!("derive QSV device from D3D11VA: {}", ffmpeg::Error::from(r));
                    }
                    let qsv_device = AvBuffer::from_raw(qsv_device)
                        .context("av_hwdevice_ctx_create_derived(QSV) gave no device")?;
                    let mut qsv_frames: *mut ffi::AVBufferRef = ptr::null_mut();
                    let r = ffi::av_hwframe_ctx_create_derived(
                        &mut qsv_frames,
                        ffi::AVPixelFormat::AV_PIX_FMT_QSV,
                        qsv_device.as_ptr(),
                        hw.frames_ref.as_ptr(),
                        ffi::AV_HWFRAME_MAP_DIRECT as c_int,
                    );
                    if r < 0 {
                        bail!("derive QSV frames from D3D11VA: {}", ffmpeg::Error::from(r));
                    }
                    let qsv_frames = AvBuffer::from_raw(qsv_frames)
                        .context("av_hwframe_ctx_create_derived(QSV) gave no frames ctx")?;
                    (Some(qsv_frames), Some(qsv_device))
                }
            };
            // Borrowed views: `open_win_encoder` takes its own refs; ownership stays here.
            let (pix_fmt, dev_ref, frames_ref) = match (&qsv_device, &qsv_frames) {
                (Some(d), Some(f)) => (ffi::AVPixelFormat::AV_PIX_FMT_QSV, d.as_ptr(), f.as_ptr()),
                _ => (
                    ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                    hw.device_ref.as_ptr(),
                    hw.frames_ref.as_ptr(),
                ),
            };
            let enc = open_win_encoder(
                vendor,
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                pix_fmt,
                sw_av,
                ten_bit,
                dev_ref,
                frames_ref,
            )?;
            tracing::info!(
                encoder = vendor.encoder_name(codec),
                "{} encode active ({width}x{height}@{fps}, zero-copy D3D11 {} path)",
                vendor.label(),
                if ten_bit { "P010" } else { "NV12" }
            );
            Ok(ZeroCopyInner {
                vendor,
                qsv_frames,
                qsv_device,
                enc,
                hw,
                ctx: immediate_context(device),
                pool_format,
            })
        }
    }

    fn submit(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        // SAFETY: `d3d`/`qsv` are owned `AvFrame`s, so every exit — including `?` between pool
        // pull and send — unrefs the pooled surface. `data[0]` is the pool texture-array,
        // `data[1]` the index; `from_raw_borrowed` borrows without Release (the frame owns it)
        // and is null-checked. `src` and `dst` are on `self.hw`'s device; the caller has
        // `captured.format == pool_format`, so `CopySubresourceRegion` is a same-format GPU
        // copy. QSV's mapped frame `av_buffer_ref`s `qsv_frames` and drops that ref with the
        // arm. `avcodec_send_frame` only internally refs the input; the drops are the owners.
        unsafe {
            let d3d = AvFrame::alloc().context("av_frame_alloc(d3d11) failed")?;
            let r = ffi::av_hwframe_get_buffer(self.hw.frames_ref.as_ptr(), d3d.as_ptr(), 0);
            if r < 0 {
                bail!("av_hwframe_get_buffer(D3D11) failed ({r})");
            }
            let dst_ptr = (*d3d.as_ptr()).data[0] as *mut c_void;
            let dst_index = (*d3d.as_ptr()).data[1] as usize as u32;
            let dst_tex = ID3D11Texture2D::from_raw_borrowed(&dst_ptr)
                .ok_or_else(|| anyhow!("pooled D3D11 frame has null texture"))?;
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = dst_tex.cast().context("pooled texture -> resource")?;
            self.ctx
                .CopySubresourceRegion(&dst, dst_index, 0, 0, 0, &src, 0, None);

            (*d3d.as_ptr()).pts = pts;
            (*d3d.as_ptr()).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let send = match self.vendor {
                WinVendor::Amf => ffi::avcodec_send_frame(self.enc.as_mut_ptr(), d3d.as_ptr()),
                WinVendor::Qsv => {
                    let qsv = AvFrame::alloc().context("av_frame_alloc(qsv) failed")?;
                    // `open` fills this for QSV and leaves `None` only for AMF. Bail, don't unwrap.
                    let Some(qsv_frames) = self.qsv_frames.as_ref() else {
                        bail!("QSV send path without a derived QSV frames context");
                    };
                    (*qsv.as_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_QSV as c_int;
                    (*qsv.as_ptr()).hw_frames_ctx = ffi::av_buffer_ref(qsv_frames.as_ptr());
                    // Bindgen enum has no BitOr — cast each flag to int before OR-ing.
                    let r = ffi::av_hwframe_map(
                        qsv.as_ptr(),
                        d3d.as_ptr(),
                        ffi::AV_HWFRAME_MAP_DIRECT as c_int | ffi::AV_HWFRAME_MAP_READ as c_int,
                    );
                    if r < 0 {
                        bail!("av_hwframe_map(D3D11→QSV) failed ({r})");
                    }
                    (*qsv.as_ptr()).pts = pts;
                    (*qsv.as_ptr()).pict_type = (*d3d.as_ptr()).pict_type;
                    ffi::avcodec_send_frame(self.enc.as_mut_ptr(), qsv.as_ptr())
                }
            };
            if send < 0 {
                bail!(
                    "avcodec_send_frame({}) failed ({send})",
                    self.vendor.label()
                );
            }
        }
        Ok(())
    }
}

// No `Drop`: `Option<AvBuffer>` unrefs when `Some` (QSV) and no-ops when `None` (AMF). Field
// order: QSV frames, QSV device, `enc`, then `hw`.

enum Inner {
    System(SystemInner),
    ZeroCopy(ZeroCopyInner),
}

pub struct FfmpegWinEncoder {
    vendor: WinVendor,
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    bit_depth: u8,
    inner: Option<Inner>,
    /// Raw `ID3D11Device` the inner is bound to. Re-init when the capturer recreates the device
    /// (secure-desktop / HDR / resize).
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    /// Submitted frames whose AUs have not arrived. `poll` may spin while this is non-zero.
    in_flight: usize,
}

// SAFETY: owns raw libav pointers and COM handles that are not auto-`Send`. The session
// creates, drives, and drops the encoder on one encode thread; the D3D11 immediate context
// is touched only there. The only cross-thread action is the initial move onto that thread.
unsafe impl Send for FfmpegWinEncoder {}

impl FfmpegWinEncoder {
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        vendor: WinVendor,
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
    ) -> Result<Self> {
        // `probe_can_encode_444` is always false; a slipped 4:4:4 request still encodes 4:2:0.
        if chroma.is_444() {
            tracing::warn!("AMF/QSV 4:4:4 encode not implemented — encoding 4:2:0");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `ffmpeg::init()` succeeded; `av_log_set_level` is a scalar setter.
            unsafe { ffi::av_log_set_level(48) };
        }
        let name = vendor.encoder_name(codec);
        if encoder::find_by_name(name).is_none() {
            bail!(
                "{name} not built into libavcodec (this FFmpeg lacks the {} encoder)",
                vendor.label()
            );
        }
        Ok(FfmpegWinEncoder {
            vendor,
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            bit_depth,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            in_flight: 0,
        })
    }

    /// Rebuild on device change. Zero-copy open failure falls back to system so the session lives.
    fn ensure_inner_d3d11(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let inner = if zerocopy_enabled(self.vendor) {
            match ZeroCopyInner::open(
                self.vendor,
                self.codec,
                self.format,
                self.width,
                self.height,
                self.fps,
                self.bitrate_bps,
                self.bit_depth,
                device,
            ) {
                Ok(zc) => Inner::ZeroCopy(zc),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "{} zero-copy D3D11 setup failed — falling back to system-memory readback",
                        self.vendor.label()
                    );
                    Inner::System(self.open_system()?)
                }
            }
        } else {
            Inner::System(self.open_system()?)
        };
        self.inner = Some(inner);
        Ok(())
    }

    fn open_system(&self) -> Result<SystemInner> {
        SystemInner::open(
            self.vendor,
            self.codec,
            self.format,
            self.width,
            self.height,
            self.fps,
            self.bitrate_bps,
            self.bit_depth,
        )
    }
}

impl Encoder for FfmpegWinEncoder {
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
        let submitted = match &captured.payload {
            FramePayload::D3d11(f) => {
                self.ensure_inner_d3d11(&f.device)?;
                // VP fallback to Bgra/Rgb10a2 cannot copy into the NV12/P010 pool (format-group
                // mismatch → UB). System readback handles every captured format.
                let pool_mismatch = matches!(
                    &self.inner,
                    Some(Inner::ZeroCopy(zc)) if captured.format != zc.pool_format
                );
                if pool_mismatch {
                    tracing::warn!(
                        captured = ?captured.format,
                        "{} zero-copy pool format mismatch (capturer video-processor fallback) — \
                         switching to system-memory readback",
                        self.vendor.label()
                    );
                    self.inner = Some(Inner::System(self.open_system()?));
                }
                match self.inner.as_mut().unwrap() {
                    Inner::ZeroCopy(zc) => zc.submit(f, pts, idr),
                    Inner::System(s) => s.submit_d3d11(f, captured.format, pts, idr),
                }
            }
            FramePayload::Cpu(bytes) => {
                if self.inner.is_none() {
                    self.inner = Some(Inner::System(self.open_system()?));
                }
                match self.inner.as_mut().unwrap() {
                    Inner::System(s) => s.submit_cpu(bytes, captured.format, pts, idr),
                    Inner::ZeroCopy(_) => {
                        bail!(
                            "{} encoder built for D3D11 got a CPU frame",
                            self.vendor.label()
                        )
                    }
                }
            }
        };
        if submitted.is_ok() {
            self.in_flight += 1;
        }
        submitted
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Drop the wedged encoder (runtime state goes with `Drop`). Next `submit` rebuilds; owed AUs
    /// are forfeited and the first rebuilt frame is IDR so the client resyncs.
    fn reset(&mut self) -> bool {
        self.inner = None;
        self.bound_device = 0;
        self.in_flight = 0;
        self.force_kf = true;
        true
    }

    /// Non-blocking `receive_packet`. libavcodec AMF holds ~2 frames (N+2 must be submitted to
    /// flush N); a spin between submits never produces the owed AU. `PUNKTFUNK_FFWIN_POLL_MS`
    /// is the bounded spin for a driver that can land mid-spin (0 = off, the default).
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let fps = self.fps;
        let enc = match &mut self.inner {
            Some(Inner::System(s)) => &mut s.enc,
            Some(Inner::ZeroCopy(z)) => &mut z.enc,
            None => return Ok(None),
        };
        let cap_us = poll_spin_cap_us();
        let deadline = (cap_us > 0 && self.in_flight > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_micros(cap_us));
        loop {
            match poll_encoder(enc, fps)? {
                PollOutcome::Packet(au) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    return Ok(Some(au));
                }
                PollOutcome::Eof => {
                    self.in_flight = 0; // flushed; nothing further is owed
                    return Ok(None);
                }
                PollOutcome::Again => match deadline {
                    Some(d) if std::time::Instant::now() < d => {
                        std::thread::sleep(std::time::Duration::from_micros(250));
                    }
                    _ => return Ok(None),
                },
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            Some(Inner::System(s)) => s.enc.send_eof().context("send_eof")?,
            Some(Inner::ZeroCopy(z)) => z.enc.send_eof().context("send_eof")?,
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefer `prefer_vendor`, else any hardware adapter. Not `EnumAdapters1(0)`: that can be
    /// our virtual display, and vendor 0x1414 is the WARP rasterizer (no video engine).
    #[cfg(test)]
    fn test_hw_device(prefer_vendor: u32) -> Option<ID3D11Device> {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
        // SAFETY: factory created here; each adapter owns its COM ref; every call is `.ok()`-
        // checked. `GetDesc1` fills a stack descriptor.
        let (factory, mut preferred, mut fallback): (IDXGIFactory1, _, _) =
            (unsafe { CreateDXGIFactory1() }.ok()?, None, None);
        for i in 0.. {
            // SAFETY: COM call on the live factory; `Ok` means an owned adapter came back.
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
                break; // DXGI_ERROR_NOT_FOUND
            };
            // SAFETY: COM call on that adapter; descriptor returned by value.
            let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
                continue;
            };
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            eprintln!("adapter {i}: vendor={:#06x} {name}", desc.VendorId);
            if desc.VendorId == prefer_vendor && preferred.is_none() {
                preferred = Some(adapter);
            } else if desc.VendorId != 0x1414 && fallback.is_none() {
                fallback = Some(adapter);
            }
        }
        let adapter = preferred.or(fallback)?;
        // SAFETY: `adapter` is an owned enumerated `IDXGIAdapter1`, borrowed only for this call.
        unsafe { pf_frame::dxgi::make_device(&adapter) }
            .ok()
            .map(|(d, _c)| d)
    }

    /// Loop construct/drop of `D3d11Hw`: a double-unref aborts in the CRT; a miss leaks a device
    /// and an 8-surface pool per iteration. Shared by both zero-copy vendors.
    #[test]
    #[ignore = "needs a real D3D11 GPU (run on a GPU host, not the build box)"]
    fn d3d11hw_alloc_drop_cycles() {
        let device = test_hw_device(0x8086).expect("a hardware D3D11 adapter");
        for i in 0..8 {
            let hw = D3d11Hw::new(
                &device,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                pool_bind_flags(WinVendor::Amf),
                640,
                480,
                8,
            )
            .unwrap_or_else(|e| panic!("D3d11Hw::new failed on iteration {i}: {e:#}"));
            assert!(!hw.device_ref.as_ptr().is_null(), "device ref went null");
            assert!(!hw.frames_ref.as_ptr().is_null(), "frames ref went null");
        }
        eprintln!("8 D3d11Hw alloc/drop cycles completed without abort");
    }

    /// Loop QSV `ZeroCopyInner` construct/drop. `open` derives a QSV pair from D3D11VA; a
    /// double-owner would abort in the CRT, a miss leaks a device per session. Sidesteps
    /// `zerocopy_enabled` (QSV default off) and native VPL. Also the regression for
    /// `SetMultithreadProtected` in `D3d11Hw::new` — without it derive fails MFX SetHandle.
    #[test]
    #[ignore = "needs a real Intel QSV device (run on an Intel host, not the build box)"]
    fn zerocopy_qsv_alloc_drop_cycles() {
        let device = test_hw_device(0x8086).expect("an Intel D3D11 adapter");
        for i in 0..8 {
            let zc = ZeroCopyInner::open(
                WinVendor::Qsv,
                Codec::H264,
                PixelFormat::Bgrx,
                640,
                480,
                30,
                8_000_000,
                8,
                &device,
            )
            .unwrap_or_else(|e| panic!("ZeroCopyInner::open(QSV) failed on iteration {i}: {e:#}"));
            // `None` here means the AMF arm ran and the derived-pair ownership never executed.
            assert!(zc.qsv_frames.is_some(), "QSV path derived no frames ctx");
            assert!(zc.qsv_device.is_some(), "QSV path derived no device");
        }
        eprintln!("8 ZeroCopyInner(QSV) alloc/drop cycles completed without abort");
    }

    #[test]
    fn zerocopy_default_is_per_vendor_and_override_wins() {
        assert!(zerocopy_active(None, WinVendor::Amf));
        assert!(!zerocopy_active(None, WinVendor::Qsv));
        for vendor in [WinVendor::Amf, WinVendor::Qsv] {
            assert!(zerocopy_active(Some(true), vendor));
            assert!(!zerocopy_active(Some(false), vendor));
        }
    }

    /// Clamp before `* 1000`: a slipped-digit ms value must hit [`MAX_POLL_SPIN_MS`], not a hang.
    #[test]
    fn poll_spin_cap_clamps_before_the_us_conversion() {
        assert_eq!(parse_poll_spin_cap_us(None), 0);
        assert_eq!(parse_poll_spin_cap_us(Some("0")), 0);
        assert_eq!(parse_poll_spin_cap_us(Some("5")), 5_000);
        assert_eq!(parse_poll_spin_cap_us(Some(" 12 ")), 12_000);
        assert_eq!(
            parse_poll_spin_cap_us(Some("100000000")),
            MAX_POLL_SPIN_MS * 1000
        );
        assert_eq!(
            parse_poll_spin_cap_us(Some(&u64::MAX.to_string())),
            MAX_POLL_SPIN_MS * 1000
        );
        assert_eq!(parse_poll_spin_cap_us(Some("junk")), 0);
        assert_eq!(parse_poll_spin_cap_us(Some("-1")), 0);
    }

    #[test]
    fn sws_src_accepts_packed_rgb_only() {
        assert_eq!(sws_src(PixelFormat::Bgrx).unwrap(), Pixel::BGRZ);
        assert_eq!(sws_src(PixelFormat::Rgbx).unwrap(), Pixel::RGBZ);
        assert_eq!(sws_src(PixelFormat::Bgra).unwrap(), Pixel::BGRA);
        assert_eq!(sws_src(PixelFormat::Rgba).unwrap(), Pixel::RGBA);
        assert_eq!(sws_src(PixelFormat::Rgb).unwrap(), Pixel::RGB24);
        assert_eq!(sws_src(PixelFormat::Bgr).unwrap(), Pixel::BGR24);
        for f in [
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Rgb10a2,
            PixelFormat::Yuv444,
            PixelFormat::X2Rgb10,
            PixelFormat::X2Bgr10,
        ] {
            assert!(sws_src(f).is_err(), "{f:?} must be refused");
        }
    }

    #[test]
    fn readback_routing_and_depth_guard() {
        assert_eq!(
            readback_route(PixelFormat::Nv12, false).unwrap(),
            ReadbackRoute::Yuv
        );
        assert_eq!(
            readback_route(PixelFormat::P010, true).unwrap(),
            ReadbackRoute::Yuv
        );
        assert_eq!(
            readback_route(PixelFormat::Bgra, false).unwrap(),
            ReadbackRoute::Bgra
        );
        assert_eq!(
            readback_route(PixelFormat::Bgrx, false).unwrap(),
            ReadbackRoute::Bgra
        );
        assert_eq!(
            readback_route(PixelFormat::Rgb10a2, true).unwrap(),
            ReadbackRoute::Rgb10
        );
        assert!(readback_route(PixelFormat::P010, false).is_err());
        assert!(readback_route(PixelFormat::Rgb10a2, false).is_err());
        assert!(readback_route(PixelFormat::Nv12, true).is_err());
        assert!(readback_route(PixelFormat::Bgra, true).is_err());
        assert!(readback_route(PixelFormat::Yuv444, false).is_err());
    }

    #[test]
    fn ten_bit_follows_the_pixels() {
        assert!(is_10bit_format(PixelFormat::P010));
        assert!(is_10bit_format(PixelFormat::Rgb10a2));
        assert!(!is_10bit_format(PixelFormat::Nv12));
        assert!(!is_10bit_format(PixelFormat::Bgra));
        assert!(!is_10bit_format(PixelFormat::Bgrx));
    }

    #[test]
    fn qsv_opts_pin_the_latency_contract() {
        let opts = vendor_opts(WinVendor::Qsv, "ignored");
        let get = |k: &str| {
            opts.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("async_depth"), Some("1"));
        assert_eq!(get("low_power"), Some("1"));
        assert_eq!(get("look_ahead"), Some("0"));
        assert_eq!(get("forced_idr"), Some("1"));
        assert_eq!(get("scenario"), Some("displayremoting"));
        assert_eq!(get("preset"), Some("veryfast"));
        assert_eq!(get("usage"), None, "AMF-only knob must not leak into QSV");
    }

    #[test]
    fn amf_opts_pin_no_bframes_and_the_usage_passthrough() {
        let opts = vendor_opts(WinVendor::Amf, "lowlatency");
        let get = |k: &str| {
            opts.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("usage"), Some("lowlatency"));
        assert_eq!(get("bf"), Some("0"));
        assert_eq!(get("rc"), Some("cbr"));
        assert_eq!(get("quality"), Some("speed"));
        assert_eq!(get("latency"), Some("true"));
        assert_eq!(get("header_insertion_mode"), Some("idr"));
        assert_eq!(get("preanalysis"), Some("false"));
        assert_eq!(get("enforce_hrd"), Some("true"));
    }

    #[test]
    fn pool_bind_flags_per_vendor() {
        assert_eq!(
            pool_bind_flags(WinVendor::Amf),
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
        );
        assert_eq!(
            pool_bind_flags(WinVendor::Qsv),
            (D3D11_BIND_DECODER.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32
        );
    }

    #[test]
    fn encoder_names_dispatch_by_vendor() {
        assert_eq!(WinVendor::Qsv.encoder_name(Codec::H264), "h264_qsv");
        assert_eq!(WinVendor::Qsv.encoder_name(Codec::H265), "hevc_qsv");
        assert_eq!(WinVendor::Qsv.encoder_name(Codec::Av1), "av1_qsv");
        assert_eq!(WinVendor::Amf.encoder_name(Codec::H265), "hevc_amf");
    }

    /// `false` without the Intel runtime is valid; print, don't assert. Compiled only when
    /// native VPL is out (`not(feature = "qsv")`).
    #[cfg(not(feature = "qsv"))]
    #[test]
    #[ignore = "needs a real FFmpeg runtime probe (run on the Windows CI runner, not a dev box)"]
    fn ffmpeg_win_probe_smoke() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            eprintln!(
                "probe_can_encode(Qsv, {codec:?}) = {}",
                probe_can_encode(WinVendor::Qsv, codec)
            );
        }
    }
}
