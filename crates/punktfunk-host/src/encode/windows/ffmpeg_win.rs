//! AMD **AMF** and Intel **QSV** hardware encode on Windows via `ffmpeg-next` — the Windows
//! analogue of the Linux [`super::vaapi`] backend (one libavcodec backend per vendor, selected by
//! encoder name: `*_amf` / `*_qsv`). This is the sibling of the direct-SDK [`super::nvenc`] path
//! behind the shared [`Encoder`] trait, selected in [`super::open_video`] (NVIDIA → NVENC,
//! AMD → AMF, Intel → QSV).
//!
//! The capturer hands a `FramePayload::D3d11` texture (NV12/P010 from the D3D11 video processor, or
//! BGRA/Rgb10a2 as a fallback) on the capturer's own `ID3D11Device`. Two input paths, chosen lazily
//! from the first frame and the `PUNKTFUNK_ZEROCOPY` knob:
//!
//! * **System-memory** ([`SystemInner`]): read the captured D3D11 surface back to a CPU
//!   NV12/P010 [`AVFrame`] (a same-format `CopyResource` → staging → `Map`, plus a `swscale` step for
//!   the BGRA fallback) and `avcodec_send_frame` it. AMF/QSV upload it internally. One
//!   GPU→CPU→GPU round-trip per frame — the robust path, the QSV default, and the automatic
//!   fallback when the zero-copy setup fails (it is the analogue of the VAAPI "CPU input" fallback).
//! * **Zero-copy D3D11** ([`ZeroCopyInner`], the AMF default; see [`zerocopy_enabled`]): wrap the
//!   capturer's `ID3D11Device` as an `AV_HWDEVICE_TYPE_D3D11VA` hwdevice (shared, *not* a second
//!   device — the capture textures are not shared-handle, so a different device couldn't read them),
//!   keep an FFmpeg D3D11 frames pool, `CopySubresourceRegion` the captured texture into a pooled
//!   array slice (a GPU-local copy, like NVENC's CUDA path), then feed AMF `AV_PIX_FMT_D3D11`
//!   directly, or map the D3D11 frame to a derived QSV surface for QSV. If the hw setup fails to
//!   open, this falls back to the system-memory path for the session.
//!
//! **Status:** AMF on-glass validated 2026-07-06 (Ryzen 7000 iGPU, 1080p120 HDR P010, both input
//! paths; zero-copy cut `submit_us` p50 2.8 ms → 0.26 ms) — zero-copy is the AMF default. QSV is
//! still not on-glass validated (no Intel Windows box in the lab), so its zero-copy path stays
//! opt-in via `PUNKTFUNK_ZEROCOPY=1`.
//!
//! Raw FFI: `ffmpeg-next` has no hwcontext wrappers for D3D11VA, so the hwdevice/hwframes calls go
//! through `ffmpeg::ffi` (= `ffmpeg_sys_next`), exactly as the Linux CUDA/VAAPI paths do. The
//! `AVD3D11VADeviceContext`/`AVD3D11VAFramesContext` layouts are mirrored (the bindings don't
//! allowlist `hwcontext_d3d11va.h`), as [`super::linux`] mirrors `AVCUDADeviceContext`.
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{ChromaFormat, Codec, EncodedFrame, Encoder};
use crate::capture::{dxgi::D3d11Frame, CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary, Packet, Rational};
use ffmpeg_next as ffmpeg;
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_DECODER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_SAMPLE_DESC,
};

use ffmpeg::ffi; // = ffmpeg_sys_next

// libswscale scaler-flag + colour-space constants (not exported as Rust consts by the bindings —
// the stable `<libswscale/swscale.h>` #defines, same as the VAAPI path uses).
const SWS_POINT: c_int = 0x10;
const SWS_CS_ITU709: c_int = 1;
const SWS_CS_BT2020: c_int = 9;

/// `AVD3D11VADeviceContext` (libavutil/hwcontext_d3d11va.h) — mirrored (the ffmpeg-sys bindings
/// don't allowlist that header). We set `device` to the capturer's `ID3D11Device` so AMF/QSV share
/// it; `av_hwdevice_ctx_init` fills `device_context`/`video_device`/`video_context`/the default
/// lock from a non-null `device`.
#[repr(C)]
struct AVD3D11VADeviceContext {
    device: *mut c_void,         // ID3D11Device*
    device_context: *mut c_void, // ID3D11DeviceContext*
    video_device: *mut c_void,   // ID3D11VideoDevice*
    video_context: *mut c_void,  // ID3D11VideoContext*
    lock: *mut c_void,           // void (*)(void*)
    unlock: *mut c_void,         // void (*)(void*)
    lock_ctx: *mut c_void,
}

/// `AVD3D11VAFramesContext` (libavutil/hwcontext_d3d11va.h) — mirrored. `BindFlags`/`MiscFlags`
/// customise the texture-array FFmpeg allocates for the pool; `texture` (we leave null) would let us
/// supply our own array.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void,       // ID3D11Texture2D*
    bind_flags: c_uint,         // UINT BindFlags
    misc_flags: c_uint,         // UINT MiscFlags
    texture_infos: *mut c_void, // AVD3D11FrameDescriptor* (FFmpeg-owned; we never touch it)
}

/// AMD AMF vs Intel QSV — the two libavcodec vendor backends this module covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinVendor {
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

/// Is the zero-copy D3D11 path enabled for this vendor? An explicit `PUNKTFUNK_ZEROCOPY`
/// (`0|false|off|no` = off, anything else = on) overrides; unset defers to the per-vendor default:
/// **on for AMF** — on-glass validated 2026-07-06 (Ryzen iGPU, 1080p120 HDR P010: `submit_us` p50
/// 2.8 ms → 0.26 ms vs readback) — and **off for QSV** until validated on Intel glass (the
/// open-failure fallback only catches *setup* errors; a derive that opens but maps wrong would
/// corrupt silently, so it stays opt-in per the probe-never-assume rule).
fn zerocopy_enabled(vendor: WinVendor) -> bool {
    crate::config::config()
        .zerocopy
        .unwrap_or(matches!(vendor, WinVendor::Amf))
}

/// The swscale *source* pixel format for a captured packed-RGB/BGR layout (8-bit BGRA fallback only).
fn sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ,
        PixelFormat::Rgbx => Pixel::RGBZ,
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 => {
            bail!("ffmpeg_win swscale path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Does this captured format imply a 10-bit encode (P010 / Rgb10a2)?
fn is_10bit_format(format: PixelFormat, bit_depth: u8) -> bool {
    bit_depth >= 10 || matches!(format, PixelFormat::P010 | PixelFormat::Rgb10a2)
}

/// `ffmpeg::format::Pixel` → raw `AVPixelFormat`.
fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    ffi::AVPixelFormat::from(p)
}

/// Build the FFmpeg encoder context shared by both inner paths: name, mode, low-latency RC,
/// infinite GOP, the BT.709-limited (SDR) or BT.2020-PQ (HDR) VUI, the given `pix_fmt`, and the
/// optional hw device/frames contexts (null for the system path). Returns the opened encoder.
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
    // Software view of the input layout (NV12 / P010). For the hw paths `pix_fmt` is overridden to
    // D3D11/QSV below; libavcodec still uses this as `sw_pix_fmt`.
    video.set_format(Pixel::from(sw_pix_fmt));
    video.set_time_base(Rational(1, fps as i32));
    video.set_frame_rate(Some(Rational(fps as i32, 1)));
    video.set_bit_rate(bitrate_bps as usize);
    video.set_max_bit_rate(bitrate_bps as usize); // target == max → CBR
    let vbv_frames = std::env::var("PUNKTFUNK_VBV_FRAMES")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);
    let vbv_bits =
        ((bitrate_bps as f64 / fps.max(1) as f64) * vbv_frames as f64).clamp(1.0, i32::MAX as f64);
    video.set_max_b_frames(0);
    let raw = video.as_mut_ptr();
    (*raw).rc_buffer_size = vbv_bits as i32;
    (*raw).gop_size = i32::MAX; // no periodic IDR (forced-IDR via pict_type=I on RFI)
    if ten_bit {
        // 10-bit HDR: BT.2020 primaries + SMPTE-2084 (PQ) transfer. The client auto-detects PQ from
        // the HEVC VUI; the static mastering metadata also rides the 0xCE datagram out-of-band.
        (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
        (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
        (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
    } else {
        // We hand the encoder BT.709 *limited* NV12 (video-processor or swscale CSC), so signal that
        // VUI — else the client decoder washes the picture out.
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

    // Low-latency tuning. Unknown private options are ignored by avcodec_open2 (left in the dict),
    // so vendor-specific keys are safe to set unconditionally.
    let mut opts = Dictionary::new();
    match vendor {
        WinVendor::Amf => {
            // Field-tuning override (ultralowlatency | lowlatency | lowlatency_high_quality |
            // transcoding): AMF usage presets bundle driver-side pipeline behavior that varies by
            // VCN generation/driver — measured on-box rather than assumed.
            let usage =
                std::env::var("PUNKTFUNK_AMF_USAGE").unwrap_or_else(|_| "ultralowlatency".into());
            opts.set("usage", &usage);
            opts.set("rc", "cbr");
            // Streaming is latency-first: `speed` trims per-frame motion-estimation depth — the
            // difference between ~encode-time and ~frame-budget on iGPU-class VCN (matches the
            // low-latency preset choice on the NVENC path).
            opts.set("quality", "speed");
            opts.set("preanalysis", "false");
            opts.set("enforce_hrd", "true");
            // AMF low-latency submission mode (FFmpeg ≥ 6.1; unknown-option-ignored on older).
            opts.set("latency", "true");
            // Never B-frames: h264_amf defaults >0 on RDNA3+ HW that supports them, and each
            // B-frame is a full frame period of added latency. (HEVC VCN has none; ignored there.)
            opts.set("bf", "0");
            // VPS/SPS/PPS on each IDR (clean mid-stream join) — HEVC/AV1 only; ignored elsewhere.
            opts.set("header_insertion_mode", "idr");
        }
        WinVendor::Qsv => {
            opts.set("preset", "veryfast");
            opts.set("async_depth", "1"); // bound in-flight frames — the big QSV latency lever
            opts.set("low_power", "1"); // VDEnc fixed-function path (lower latency)
            opts.set("look_ahead", "0"); // (h264_qsv only; ignored on hevc/av1)
            opts.set("forced_idr", "1"); // a forced key frame becomes a real IDR
            opts.set("scenario", "displayremoting");
        }
    }
    video
        .open_with(opts)
        .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))
}

/// Probe whether THIS GPU can `vendor`-encode `codec`, by opening a tiny system-input encoder. The
/// driver/runtime rejects codecs the video engine can't do (AV1 on pre-RDNA3 AMD / pre-Arc Intel,
/// or HEVC on a very old part). Used to build the GameStream codec advertisement so a client never
/// negotiates a codec the encoder can't open. Torn down immediately.
/// Whether the active AMD (AMF) / Intel (QSV) GPU can encode HEVC **4:4:4**. **Deferred in v1 —
/// always `false`.** AMF/QSV HEVC 4:4:4 encode is narrow (AMD RDNA3+, Intel Arc/Xe2+) and the
/// libavcodec profile/pixel-format incantation is vendor- and driver-specific — a wrong profile
/// `avcodec_open2` *silently* falls back to 4:2:0, so a positive probe would need a verify-by-frame,
/// and there is no AMD/Intel Windows box in the lab to build + validate that against. Returning
/// `false` keeps the negotiation honest: an AMF/QSV host resolves every session to 4:2:0 before the
/// Welcome. (Follow-up: implement + validate on an RDNA3+/Arc Windows box.)
pub fn probe_can_encode_444(_vendor: WinVendor, _codec: Codec) -> bool {
    tracing::info!("AMF/QSV HEVC 4:4:4 encode is not implemented yet — declining (encoding 4:2:0)");
    false
}

pub fn probe_can_encode(vendor: WinVendor, codec: Codec) -> bool {
    // Deliberately NOT pinned to the selected render adapter (unlike `nvenc::probe_can_encode_444`):
    // the system-input probe passes no hwdevice, and the AMF/QSV runtimes only ever bind their own
    // vendor's silicon — on a mixed-vendor box the probe lands on the right GPU by construction.
    // Only a two-same-vendor-GPU box could probe the wrong card (accepted; results are cached per
    // selected GPU in `windows_codec_support`, so a fix here slots in without churn).
    if ffmpeg::init().is_err() {
        return false;
    }
    // SAFETY: `ffmpeg::init()` succeeded above, so libav's global state is initialised.
    // `av_log_get_level`/`av_log_set_level` are global scalar getters/setters with no pointer args.
    // `open_win_encoder` (the `unsafe fn`) is called with null `device_ref`/`frames_ref` (the system
    // path), so it touches no D3D11/hwcontext — it only allocates and opens a self-contained
    // libavcodec encoder that is dropped at the end of `.is_ok()`. We restore the prior log level and
    // no raw pointer escapes the block.
    unsafe {
        // A missing AMF/QSV runtime (wrong-vendor host, GPU-less CI) is an expected probe outcome —
        // quiet ffmpeg's open error for the probe, then restore the level.
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

/// One `receive_packet` attempt, with the not-ready states kept distinct so the blocking poll
/// below can tell "still encoding" (retry) from "stream over" (stop).
enum PollOutcome {
    Packet(EncodedFrame),
    Again,
    Eof,
}

/// Drain the encoder for one packet (shared poll logic, identical to the VAAPI/NVENC paths).
fn poll_encoder(enc: &mut encoder::video::Encoder, fps: u32) -> Result<PollOutcome> {
    let mut pkt = Packet::empty();
    match enc.receive_packet(&mut pkt) {
        Ok(()) => {
            let data = pkt.data().map(|d| d.to_vec()).unwrap_or_default();
            let pts = pkt.pts().unwrap_or(0).max(0) as u64;
            Ok(PollOutcome::Packet(EncodedFrame {
                data,
                pts_ns: pts * 1_000_000_000 / fps as u64,
                keyframe: pkt.is_key(),
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

/// The immediate context of an `ID3D11Device` (for `CopyResource`/`CopySubresourceRegion`).
unsafe fn immediate_context(device: &ID3D11Device) -> ID3D11DeviceContext {
    // windows-rs 0.62: the inherent method takes no args and returns the context (the OutRef form is
    // only on the `_Impl` trait, for implementing the interface). Every D3D11 device has one.
    device
        .GetImmediateContext()
        .expect("ID3D11Device always has an immediate context")
}

// ---------------------------------------------------------------------------------------------
// System-memory path (default): read the captured D3D11 surface back to a CPU NV12/P010 frame.
// ---------------------------------------------------------------------------------------------

struct SystemInner {
    enc: encoder::video::Encoder,
    /// Reusable software NV12/P010 frame: swscale dst / readback dst, and the `send_frame` src.
    sw_frame: *mut ffi::AVFrame,
    /// swscale ctx for the BGRA→NV12 fallback (built lazily; null for the YUV-readback path).
    sws: *mut ffi::SwsContext,
    /// CPU-readable staging texture for the D3D11 readback (built lazily on the captured device).
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
        let ten_bit = is_10bit_format(format, bit_depth);
        let sw_av = if ten_bit {
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffi::AVPixelFormat::AV_PIX_FMT_NV12
        };
        // SAFETY: calls the `unsafe fn open_win_encoder` with null `device_ref`/`frames_ref`, so the
        // system path is taken (no hw device/frames context is touched); all other args are scalars.
        // The returned `encoder::video::Encoder` owns its `AVCodecContext` and frees it on drop; no raw
        // pointer is aliased.
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
        // SAFETY: `av_frame_alloc` returns a freshly-allocated, uniquely-owned `AVFrame` (null-checked
        // before any deref); writing `format`/`width`/`height` through `*f` stays inside that
        // allocation. `av_frame_get_buffer(f, 0)` allocates the backing planes — on failure we
        // `av_frame_free` the sole owner (no double-free) and bail; on success the raw `f` is moved into
        // `self.sw_frame` and freed exactly once in `Drop`.
        let sw_frame = unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                bail!("av_frame_alloc(sw) failed");
            }
            (*f).format = sw_av as c_int;
            (*f).width = width as c_int;
            (*f).height = height as c_int;
            if ffi::av_frame_get_buffer(f, 0) < 0 {
                let mut f = f;
                ffi::av_frame_free(&mut f);
                bail!("av_frame_get_buffer(sw) failed");
            }
            f
        };
        tracing::info!(
            encoder = vendor.encoder_name(codec),
            "{} encode active ({width}x{height}@{fps}, system-memory {} path)",
            vendor.label(),
            if ten_bit { "P010" } else { "NV12" }
        );
        Ok(SystemInner {
            enc,
            sw_frame,
            sws: ptr::null_mut(),
            staging: None,
            ctx: None,
            format,
            ten_bit,
            width,
            height,
        })
    }

    /// Lazily (re)build the staging texture matching `dxgi_fmt` on the captured device.
    unsafe fn ensure_staging(
        &mut self,
        device: &ID3D11Device,
        dxgi_fmt: DXGI_FORMAT,
    ) -> Result<()> {
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
        device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(staging readback)")?;
        self.staging = t;
        self.ctx = Some(immediate_context(device));
        Ok(())
    }

    /// Send the reusable `sw_frame` to the encoder with the given pts / IDR flag.
    unsafe fn send(&mut self, pts: i64, idr: bool) -> Result<()> {
        (*self.sw_frame).pts = pts;
        (*self.sw_frame).pict_type = if idr {
            ffi::AVPictureType::AV_PICTURE_TYPE_I
        } else {
            ffi::AVPictureType::AV_PICTURE_TYPE_NONE
        };
        let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), self.sw_frame);
        if r < 0 {
            bail!("avcodec_send_frame({} system) failed ({r})", "ffmpeg_win");
        }
        Ok(())
    }

    /// D3D11 path: read the captured surface back into `sw_frame`, then send. Dispatches on the
    /// CURRENT frame's `format` — the capturer's video processor latches off on failure and switches
    /// NV12→Bgra (SDR) or P010→Rgb10a2 (HDR) mid-session, so a fixed open-time format is wrong.
    fn submit_d3d11(
        &mut self,
        frame: &D3d11Frame,
        format: PixelFormat,
        pts: i64,
        idr: bool,
    ) -> Result<()> {
        let fmt_10 = matches!(format, PixelFormat::P010 | PixelFormat::Rgb10a2);
        anyhow::ensure!(
            fmt_10 == self.ten_bit,
            "captured format {format:?} bit-depth changed under the encoder (built {}-bit)",
            if self.ten_bit { 10 } else { 8 }
        );
        match format {
            PixelFormat::Nv12 | PixelFormat::P010 => self.readback_yuv(frame, pts, idr),
            PixelFormat::Bgra | PixelFormat::Bgrx => self.readback_bgra(frame, pts, idr),
            PixelFormat::Rgb10a2 => self.readback_rgb10(frame, pts, idr),
            other => {
                bail!("ffmpeg_win system path cannot read back captured D3D11 format {other:?}")
            }
        }
    }

    /// Read back a captured NV12/P010 surface plane-by-plane into the software frame.
    fn readback_yuv(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        let dxgi_fmt = if self.ten_bit {
            DXGI_FORMAT_P010
        } else {
            DXGI_FORMAT_NV12
        };
        // SAFETY: `ensure_staging` builds a STAGING texture (CPU_ACCESS_READ) matching `dxgi_fmt` on
        // `frame.device` — the same `ID3D11Device` that owns `frame.texture` — and caches that device's
        // immediate context in `self.ctx`. `src`/`dst` are that device's textures of identical NV12/P010
        // format and dimensions, so `CopyResource` on the single-threaded immediate context is valid.
        // `Map(.., D3D11_MAP_READ)` succeeds on a staging texture and yields `map.pData` valid for the
        // whole resource; for NV12/P010 the luma plane is `H` rows at `RowPitch` and the chroma plane
        // follows at byte offset `RowPitch*H` (`H/2` rows), so `total = pitch*(H+⌈H/2⌉)` is exactly the
        // mapped extent and `from_raw_parts(base, total)` stays in-bounds. Each `copy_nonoverlapping`
        // reads a bounds-checked `mapped[..]` sub-slice (`row_bytes ≤ pitch`) and writes `row_bytes ≤
        // linesize` into the `av_frame_get_buffer`-allocated plane at row `y < H`, so every destination
        // offset is inside the frame's plane allocation; src and dst never alias. `Unmap` pairs `Map`,
        // then `send` (the `unsafe fn`) hands `sw_frame` to the encoder.
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
            // NV12/P010 in a mapped staging surface: the Y plane occupies rows [0,H) at `pitch`; the
            // interleaved chroma plane (H/2 rows) starts at byte offset `pitch * H`. P010 samples are
            // 16-bit, so a "row" of width pixels is `width*2` bytes (and chroma `width*2` too).
            let bytes_per_sample = if self.ten_bit { 2 } else { 1 };
            let row_bytes = self.width as usize * bytes_per_sample;
            let base = map.pData as *const u8;
            let total = pitch.saturating_mul(h + h.div_ceil(2));
            let mapped = std::slice::from_raw_parts(base, total);
            let chroma_off = pitch * h;
            let y_dst = (*self.sw_frame).data[0];
            let y_stride = (*self.sw_frame).linesize[0] as usize;
            let uv_dst = (*self.sw_frame).data[1];
            let uv_stride = (*self.sw_frame).linesize[1] as usize;
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

    /// Read back a captured BGRA surface, then swscale BGRA→NV12 into the software frame (8-bit).
    fn readback_bgra(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        if self.ten_bit {
            bail!("ffmpeg_win: BGRA readback is 8-bit only (HDR needs the P010 capture path)");
        }
        // SAFETY: `ensure_staging` builds a B8G8R8A8 STAGING texture on `frame.device` and caches that
        // device's immediate context; `src`/`dst` are that device's textures of matching BGRA format,
        // so `CopyResource` on the single-threaded context is valid. `Map(READ)` on the staging texture
        // yields `base` valid for `pitch` × `h` rows. `ensure_sws` lazily builds the BGRA→NV12 context;
        // `sws_scale` reads `h` rows of `pitch` bytes from `base` (in-bounds — the staging surface is
        // `≥ pitch*h`) into the `sw_frame` planes addressed by its `data`/`linesize` (allocated for
        // `width`×`height` NV12). `Unmap` pairs `Map`; the cached `sws` is freed once in `Drop`. The
        // mapped read region never aliases the owned encoder frame.
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
            self.ensure_sws(
                pixel_to_av(Pixel::BGRA),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_CS_ITU709,
            )?;
            let src_data: [*const u8; 4] = [base, ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [pitch as c_int, 0, 0, 0];
            let r = ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame).data.as_ptr(),
                (*self.sw_frame).linesize.as_ptr(),
            );
            ctx.Unmap(&staging, 0);
            if r < 0 {
                bail!("sws_scale BGRA→NV12 failed");
            }
            self.send(pts, idr)
        }
    }

    /// Read back a captured Rgb10a2 (BT.2020 PQ, R10G10B10A2) surface and swscale it to P010
    /// (BT.2020 PQ, limited range) — the HDR path when the capturer's video processor emitted its
    /// R10 shader output instead of P010. DXGI `R10G10B10A2_UNORM` (R in the low 10 bits, X2 alpha in
    /// the top 2) == FFmpeg `AV_PIX_FMT_X2BGR10LE`. UNTESTED on glass (no AMD/Intel Windows box).
    fn readback_rgb10(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        // SAFETY: same shape as `readback_yuv`/`readback_bgra` — `ensure_staging` builds an
        // R10G10B10A2 STAGING texture on `frame.device` and caches its immediate context; `src`/`dst`
        // are that device's matching-format textures, so `CopyResource` on the single-threaded context
        // is valid. `Map(READ)` yields `base` valid for `pitch` × `h` rows. `ensure_sws` builds the
        // X2BGR10LE→P010 (BT.2020) context; `sws_scale` reads `h` rows of `pitch` bytes from `base`
        // (in-bounds) into the `sw_frame` P010 planes (`data`/`linesize`, allocated `width`×`height`).
        // `Unmap` pairs `Map`; `sws` is freed once in `Drop`. No aliasing between read and write.
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
            // RGB(BT.2020 PQ) → YUV(BT.2020 PQ): a matrix-only repack (same PQ transfer), full→limited.
            self.ensure_sws(
                ffi::AVPixelFormat::AV_PIX_FMT_X2BGR10LE,
                ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
                SWS_CS_BT2020,
            )?;
            let src_data: [*const u8; 4] = [base, ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [pitch as c_int, 0, 0, 0];
            let r = ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame).data.as_ptr(),
                (*self.sw_frame).linesize.as_ptr(),
            );
            ctx.Unmap(&staging, 0);
            if r < 0 {
                bail!("sws_scale Rgb10a2→P010 failed");
            }
            self.send(pts, idr)
        }
    }

    /// CPU path: swscale a packed RGB/BGR CPU buffer to NV12, then send (8-bit only). Used when the
    /// capturer hands `FramePayload::Cpu` (DDA without the video-processor path).
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
        // SAFETY: `ensure_sws` lazily builds the (packed RGB/BGR)→NV12 context for this fixed src/dst
        // format pair. `src_data[0] = bytes.as_ptr()` with `src_stride[0] = src_row`; the `ensure!`
        // above guarantees `bytes` holds at least `src_row*h` bytes, so `sws_scale` reads `h` rows of
        // `src_row` bytes in-bounds and writes the `sw_frame` NV12 planes (`data`/`linesize`, allocated
        // `width`×`height`). `bytes` is borrowed for the call only and never aliases the owned
        // `sw_frame`. `send` then hands `sw_frame` to the encoder.
        unsafe {
            self.ensure_sws(
                pixel_to_av(sws_src(format)?),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_CS_ITU709,
            )?;
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.sw_frame).data.as_ptr(),
                (*self.sw_frame).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            self.send(pts, idr)
        }
    }

    /// Lazily build the swscale context (src → NV12/P010, limited range, the given colorspace). A
    /// SystemInner uses exactly one src→dst conversion for its lifetime (8-bit RGB→NV12 BT.709, or
    /// 10-bit RGB10→P010 BT.2020), so caching a single context is sound.
    unsafe fn ensure_sws(
        &mut self,
        src_av: ffi::AVPixelFormat,
        dst_av: ffi::AVPixelFormat,
        cs: c_int,
    ) -> Result<()> {
        if !self.sws.is_null() {
            return Ok(());
        }
        let sws = ffi::sws_getContext(
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
        if sws.is_null() {
            bail!("sws_getContext(RGB→YUV) failed");
        }
        // Source full-range RGB → destination limited-range YUV (matches the limited-range VUI we
        // signal). For RGB input the src coefficient table is unused; pass the dst table for both.
        let coeff = ffi::sws_getCoefficients(cs);
        ffi::sws_setColorspaceDetails(sws, coeff, 1, coeff, 0, 0, 1 << 16, 1 << 16);
        self.sws = sws;
        Ok(())
    }
}

impl Drop for SystemInner {
    fn drop(&mut self) {
        // SAFETY: `sw_frame` is the `AVFrame` allocated in `open` (or null) — `av_frame_free` drops it
        // once and nulls the pointer through the `&mut`; `sws` is the cached `SwsContext` (or null) —
        // `sws_freeContext` frees it once. This `Drop` runs exactly once and `SystemInner` owns both
        // exclusively, so there is no double-free or use-after-free.
        unsafe {
            if !self.sw_frame.is_null() {
                ffi::av_frame_free(&mut self.sw_frame);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Zero-copy D3D11 path (the AMF default; QSV opt-in — see `zerocopy_enabled`): share the capture
// device, pool D3D11 frames, copy the captured texture into a pooled slice, feed AMF directly /
// map to QSV. Falls back to the system path if the hw setup fails to open.
// ---------------------------------------------------------------------------------------------

struct D3d11Hw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl D3d11Hw {
    /// Wrap the capturer's `ID3D11Device` as a D3D11VA hwdevice and build an NV12/P010 frames pool.
    unsafe fn new(
        device: &ID3D11Device,
        sw_format: ffi::AVPixelFormat,
        bind_flags: u32,
        w: u32,
        h: u32,
        pool: c_int,
    ) -> Result<Self> {
        let mut device_ref =
            ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
        if device_ref.is_null() {
            bail!("av_hwdevice_ctx_alloc(D3D11VA) failed");
        }
        let dev_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
        let d11 = (*dev_ctx).hwctx as *mut AVD3D11VADeviceContext;
        // Share the capture device. FFmpeg's d3d11va teardown Releases `device`, so hand it an owned
        // reference (clone = AddRef, forget = don't Release ours). init() fills
        // device_context / video_device / video_context / the default lock from a non-null device.
        std::mem::forget(device.clone());
        (*d11).device = device.as_raw();
        let r = ffi::av_hwdevice_ctx_init(device_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwdevice_ctx_init(D3D11VA) failed ({r})");
        }

        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_alloc(D3D11VA) failed");
        }
        let fc = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*fc).sw_format = sw_format;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = pool;
        let f11 = (*fc).hwctx as *mut AVD3D11VAFramesContext;
        (*f11).bind_flags = bind_flags;
        let r = ffi::av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_init(D3D11VA) failed ({r})");
        }
        Ok(D3d11Hw {
            device_ref,
            frames_ref,
        })
    }
}

impl Drop for D3d11Hw {
    fn drop(&mut self) {
        // SAFETY: `frames_ref`/`device_ref` are the two non-null `AVBufferRef`s `D3d11Hw::new` created
        // (it bails before constructing `Self` if either alloc/init fails, so a live `D3d11Hw` always
        // holds both). `av_buffer_unref` drops one reference and nulls the pointer through the `&mut`.
        // This `Drop` runs exactly once and `D3d11Hw` owns these refs exclusively → no double-free /
        // use-after-free. Frames are unref'd before the device because the frames ctx internally holds
        // a ref on the device (refcounted, so the order is sound either way).
        unsafe {
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

struct ZeroCopyInner {
    vendor: WinVendor,
    enc: encoder::video::Encoder,
    hw: D3d11Hw,
    /// QSV only: the QSV device + frames ctx derived from the D3D11VA ones (the encoder's real
    /// input). `None` for AMF (which takes the D3D11 frames directly).
    qsv_device: *mut ffi::AVBufferRef,
    qsv_frames: *mut ffi::AVBufferRef,
    ctx: ID3D11DeviceContext,
    /// The pool's fixed sw_format (NV12 8-bit / P010 10-bit). A captured frame whose format differs
    /// (the capturer's video-processor fell back to Bgra/Rgb10a2) cannot be CopySubresourceRegion'd
    /// into this pool (format-group mismatch → UB), so the caller drops to the system path instead.
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
        let ten_bit = is_10bit_format(format, bit_depth);
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
        // Bind flags on the FFmpeg-allocated pool. AMF reads it as encoder input (RENDER_TARGET +
        // SHADER_RESOURCE, matching the video-processor output); QSV maps it as an mfx surface
        // (DECODER | VIDEO_ENCODER). The CopySubresourceRegion into the pool works with any usable
        // DEFAULT-usage texture regardless.
        let bind_flags = match vendor {
            WinVendor::Amf => (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            WinVendor::Qsv => (D3D11_BIND_DECODER.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        };
        const POOL: c_int = 8;
        // SAFETY: `D3d11Hw::new` wraps the capturer's `device` as a D3D11VA hwdevice (handing FFmpeg an
        // owned AddRef of it, balanced by FFmpeg's teardown Release) and builds an owned
        // device_ref/frames_ref pair freed by `D3d11Hw::Drop`; `hw` is a local, so it is dropped (and
        // both refs freed) on every early `return Err`. For QSV, `av_hwdevice_ctx_create_derived` and
        // `av_hwframe_ctx_create_derived` fill the null-initialised `qsv_device`/`qsv_frames` out-params
        // only on success (`r >= 0` checked); on the frames-derive failure we unref the already-created
        // `qsv_device` before bailing. `open_win_encoder` internally `av_buffer_ref`s the dev/frames
        // refs it is given (so ownership of `hw`'s and the derived refs stays here), and on its failure
        // we unref the still-owned derived `qsv_frames`/`qsv_device` (null for AMF → skipped) and return
        // — `hw` then drops its D3D11 refs. On success the derived refs are moved into `ZeroCopyInner`
        // (freed in its `Drop`) and the encoder holds its own AddRef'd copies. Every `AVBufferRef` is
        // unref'd exactly once across all paths — no leak, no double-free.
        unsafe {
            let hw = D3d11Hw::new(device, sw_av, bind_flags, width, height, POOL)?;
            let (pix_fmt, dev_ref, frames_ref, mut qsv_device, mut qsv_frames) = match vendor {
                WinVendor::Amf => (
                    ffi::AVPixelFormat::AV_PIX_FMT_D3D11,
                    hw.device_ref,
                    hw.frames_ref,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                WinVendor::Qsv => {
                    // Derive a QSV device that SHARES the D3D11 device, and a QSV frames ctx derived
                    // from the D3D11 frames pool (auto-mapped 1:1). The encoder takes AV_PIX_FMT_QSV.
                    let mut qsv_device: *mut ffi::AVBufferRef = ptr::null_mut();
                    let r = ffi::av_hwdevice_ctx_create_derived(
                        &mut qsv_device,
                        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                        hw.device_ref,
                        0,
                    );
                    if r < 0 {
                        bail!("derive QSV device from D3D11VA: {}", ffmpeg::Error::from(r));
                    }
                    let mut qsv_frames: *mut ffi::AVBufferRef = ptr::null_mut();
                    let r = ffi::av_hwframe_ctx_create_derived(
                        &mut qsv_frames,
                        ffi::AVPixelFormat::AV_PIX_FMT_QSV,
                        qsv_device,
                        hw.frames_ref,
                        ffi::AV_HWFRAME_MAP_DIRECT as c_int,
                    );
                    if r < 0 {
                        ffi::av_buffer_unref(&mut qsv_device);
                        bail!("derive QSV frames from D3D11VA: {}", ffmpeg::Error::from(r));
                    }
                    (
                        ffi::AVPixelFormat::AV_PIX_FMT_QSV,
                        qsv_device,
                        qsv_frames,
                        qsv_device,
                        qsv_frames,
                    )
                }
            };
            let enc = match open_win_encoder(
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
            ) {
                Ok(e) => e,
                Err(e) => {
                    if !qsv_frames.is_null() {
                        ffi::av_buffer_unref(&mut qsv_frames);
                    }
                    if !qsv_device.is_null() {
                        ffi::av_buffer_unref(&mut qsv_device);
                    }
                    return Err(e);
                }
            };
            tracing::info!(
                encoder = vendor.encoder_name(codec),
                "{} encode active ({width}x{height}@{fps}, zero-copy D3D11 {} path)",
                vendor.label(),
                if ten_bit { "P010" } else { "NV12" }
            );
            Ok(ZeroCopyInner {
                vendor,
                enc,
                hw,
                qsv_device,
                qsv_frames,
                ctx: immediate_context(device),
                pool_format,
            })
        }
    }

    fn submit(&mut self, frame: &D3d11Frame, pts: i64, idr: bool) -> Result<()> {
        // SAFETY: `d3d = av_frame_alloc()` is a fresh owned frame (null-checked) and is `av_frame_free`d
        // exactly once on every path below. `av_hwframe_get_buffer` fills it from the pool — on failure
        // we free it and bail. `(*d3d).data[0]` is the pool's texture-array and `data[1]` the array
        // index; `from_raw_borrowed` borrows that `ID3D11Texture2D` WITHOUT taking ownership (no Release
        // — the frame owns it) and is null-checked. `src` (the captured texture) and `dst` (the pooled
        // slice) live on the SAME D3D11 device wrapped by `self.hw`, and the caller guarantees
        // `captured.format == pool_format` before calling, so `CopySubresourceRegion(dst, dst_index, ..,
        // src, 0, ..)` on the single-threaded immediate context `self.ctx` is a valid same-format GPU
        // copy. For QSV the mapped `qsv` frame is a fresh owned frame whose `hw_frames_ctx` takes an
        // `av_buffer_ref` of `self.qsv_frames`; it is `av_frame_free`d (releasing that ref) on both the
        // map-failure and success paths. `avcodec_send_frame` only internally refs the input frame, so
        // the `av_frame_free(d3d)`/`av_frame_free(qsv)` afterwards are the sole owning frees — no leak,
        // no double-free, no use-after-free.
        unsafe {
            // Pull a pooled D3D11 surface; its data[0] is the pool's texture-ARRAY, data[1] the slice.
            let mut d3d = ffi::av_frame_alloc();
            if d3d.is_null() {
                bail!("av_frame_alloc(d3d11) failed");
            }
            let r = ffi::av_hwframe_get_buffer(self.hw.frames_ref, d3d, 0);
            if r < 0 {
                ffi::av_frame_free(&mut d3d);
                bail!("av_hwframe_get_buffer(D3D11) failed ({r})");
            }
            let dst_ptr = (*d3d).data[0] as *mut c_void;
            let dst_index = (*d3d).data[1] as usize as u32;
            let dst_tex = ID3D11Texture2D::from_raw_borrowed(&dst_ptr)
                .ok_or_else(|| anyhow!("pooled D3D11 frame has null texture"))?;
            // GPU-local copy of the captured slice into the pooled array slice (like NVENC's CUDA
            // device→device copy). Subresource = arrayIndex (MipLevels=1).
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = dst_tex.cast().context("pooled texture -> resource")?;
            self.ctx
                .CopySubresourceRegion(&dst, dst_index, 0, 0, 0, &src, 0, None);

            (*d3d).pts = pts;
            (*d3d).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };

            let send = match self.vendor {
                WinVendor::Amf => ffi::avcodec_send_frame(self.enc.as_mut_ptr(), d3d),
                WinVendor::Qsv => {
                    // Map the D3D11 frame to a QSV surface (1:1, no copy), then send the mapped frame.
                    let mut qsv = ffi::av_frame_alloc();
                    if qsv.is_null() {
                        ffi::av_frame_free(&mut d3d);
                        bail!("av_frame_alloc(qsv) failed");
                    }
                    (*qsv).format = ffi::AVPixelFormat::AV_PIX_FMT_QSV as c_int;
                    (*qsv).hw_frames_ctx = ffi::av_buffer_ref(self.qsv_frames);
                    // The map flags are a bindgen enum (no BitOr) — cast each to int before OR-ing.
                    let r = ffi::av_hwframe_map(
                        qsv,
                        d3d,
                        ffi::AV_HWFRAME_MAP_DIRECT as c_int | ffi::AV_HWFRAME_MAP_READ as c_int,
                    );
                    if r < 0 {
                        ffi::av_frame_free(&mut qsv);
                        ffi::av_frame_free(&mut d3d);
                        bail!("av_hwframe_map(D3D11→QSV) failed ({r})");
                    }
                    (*qsv).pts = pts;
                    (*qsv).pict_type = (*d3d).pict_type;
                    let s = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), qsv);
                    ffi::av_frame_free(&mut qsv);
                    s
                }
            };
            ffi::av_frame_free(&mut d3d);
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

impl Drop for ZeroCopyInner {
    fn drop(&mut self) {
        // SAFETY: `qsv_frames`/`qsv_device` are the derived QSV `AVBufferRef`s (or null for AMF); each
        // is `av_buffer_unref`'d once here (nulling the pointer through the `&mut`) — `ZeroCopyInner`
        // owns these handles exclusively and this `Drop` runs once, so no double-free. The `enc` and
        // `hw` fields free the encoder's AddRef'd copies and the D3D11 device/frames refs through their
        // own `Drop`, so all references stay balanced.
        unsafe {
            if !self.qsv_frames.is_null() {
                ffi::av_buffer_unref(&mut self.qsv_frames);
            }
            if !self.qsv_device.is_null() {
                ffi::av_buffer_unref(&mut self.qsv_device);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------

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
    /// Built lazily from the first frame (system readback vs zero-copy D3D11).
    inner: Option<Inner>,
    /// Raw `ID3D11Device` pointer the live inner is bound to — re-init on change (the capturer
    /// recreates its device across secure-desktop / HDR / resize transitions, like NVENC tracks).
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    /// Frames sent to libavcodec whose AUs haven't been received yet. `poll` blocks (bounded)
    /// while this is non-zero — see the poll-contract note on [`Encoder::poll`] below.
    in_flight: usize,
}

// Raw FFI pointers + COM objects; the encoder lives on a single thread (same contract as NVENC/VAAPI).
// SAFETY: `FfmpegWinEncoder` owns raw libav pointers (`AVFrame`/`SwsContext`/`AVBufferRef`) and
// windows-rs COM handles (`ID3D11Device`/`ID3D11DeviceContext`/textures) that are not auto-`Send`. The
// session creates the encoder, drives `submit`/`poll`/`flush`, and drops it all on one dedicated encode
// thread; it is never shared by reference across threads, and the D3D11 immediate context is only ever
// touched from that thread. The only cross-thread action is the initial move to the encode thread,
// after which every interior pointer/COM ref is used single-threaded — the same contract the
// NVENC/VAAPI encoders rely on. No interior state is accessed concurrently.
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
        // AMF/QSV 4:4:4 is deferred (see `probe_can_encode_444`): no validated AMD/Intel Windows
        // hardware in the lab, and the AMF/QSV HEVC 4:4:4 profile/format incantations are vendor- and
        // driver-specific (a wrong profile silently encodes 4:2:0). The probe returns false so the host
        // never negotiates 4:4:4 for an AMF/QSV session; if a request slips through, fall back to 4:2:0.
        if chroma.is_444() {
            tracing::warn!("AMF/QSV 4:4:4 encode not implemented — encoding 4:2:0");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            // SAFETY: `ffmpeg::init()` ran on the line above, so libav is initialised; `av_log_set_level`
            // is a global scalar setter with no pointer arguments.
            unsafe { ffi::av_log_set_level(48) };
        }
        // Make sure the encoder name exists in this libavcodec build up front (clear error vs a
        // first-frame failure).
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

    /// Build (or rebuild) the inner for a D3D11 frame, picking zero-copy or system. Zero-copy
    /// failures fall back to the system path so a session is never lost to the untested hw path. The
    /// device is re-bound on change (the capturer recreates it across secure-desktop / HDR / resize).
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
                // If zero-copy is active but the capturer fell back to a format the NV12/P010 pool
                // can't accept (no video processor → Bgra/Rgb10a2), a CopySubresourceRegion into the
                // pool would be a format-group mismatch (UB / device removal). Drop to the system
                // readback path, which handles every captured format.
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
                // DDA-without-video-processor hands CPU BGRA; build a system inner and swscale it.
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

    /// Encode-stall recovery: drop the wedged libavcodec encoder (its `Drop` releases the AMF/QSV
    /// runtime state) and let the next `submit` rebuild it lazily on the current device, exactly
    /// like first-frame bring-up. The owed AUs are forfeited (`in_flight` zeroed) and the rebuilt
    /// encoder's first frame is forced IDR so the client resyncs immediately.
    fn reset(&mut self) -> bool {
        self.inner = None;
        self.bound_device = 0;
        self.in_flight = 0;
        self.force_kf = true;
        true
    }

    /// Poll for the next finished AU (single non-blocking `receive_packet`).
    ///
    /// libavcodec's `hevc_amf`/`av1_amf` wrapper holds ~2 frames before releasing the oldest
    /// (it needs frame N+2 submitted to flush N), so the encode→retrieve latency floors at
    /// **~2 frame periods** — measured dead-stable at 36 ms p50 for 720p60 on the Ryzen 7000
    /// iGPU across depth 1/2, every `usage` preset, and any spin (a spin between submits provably
    /// never produces the owed AU — verified with a 150 ms cap pegging at exactly 150 ms). So the
    /// buffer is inherent to the libavcodec path, NOT host scheduling: the real fix is a direct
    /// AMF SDK encoder (the AMF analogue of `encode/windows/nvenc.rs`, whose delay=0 gives NVENC
    /// its ~1–2 ms) — tracked as the next AMD latency lever. `PUNKTFUNK_FFWIN_POLL_MS` keeps a
    /// bounded spin available for a future VCN/driver where the AU can land mid-spin (0 = off,
    /// the default and correct choice on measured hardware).
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let fps = self.fps;
        let enc = match &mut self.inner {
            Some(Inner::System(s)) => &mut s.enc,
            Some(Inner::ZeroCopy(z)) => &mut z.enc,
            None => return Ok(None),
        };
        let cap_us = std::env::var("PUNKTFUNK_FFWIN_POLL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|ms| ms * 1000)
            .unwrap_or(0); // default: no spin — the libavcodec AMF buffer can't be spun out
        let deadline = (cap_us > 0 && self.in_flight > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_micros(cap_us));
        loop {
            match poll_encoder(enc, fps)? {
                PollOutcome::Packet(au) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    return Ok(Some(au));
                }
                PollOutcome::Eof => {
                    self.in_flight = 0; // flushed: nothing further is owed
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
