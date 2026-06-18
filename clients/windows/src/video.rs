//! Video decode: reassembled HEVC access units → frames for the D3D11 presenter.
//!
//! Two backends, picked at session start (override via [`DecoderPref`] / the Settings UI):
//!
//! * **D3D11VA** (any GPU): libavcodec decodes on the GPU straight into `ID3D11Texture2D`s that
//!   carry `D3D11_BIND_SHADER_RESOURCE`, so the presenter samples the decoded NV12/P010 surface
//!   directly — **zero copy** (no swscale, no CPU readback, no per-frame upload). The textures are
//!   created by the process-wide shared device ([`crate::gpu`]) the presenter also draws with, which
//!   is what makes them bindable there. This is the big latency/throughput win over software decode.
//! * **Software**: libavcodec on the CPU + swscale to a packed 4-byte format the presenter uploads
//!   (`RGBA` for SDR, `X2BGR10` for HDR). The fallback on a GPU-less box (WARP), when D3D11VA init
//!   fails, or when a mid-session hardware error demotes us — the host's IDR/RFI recovery
//!   resynchronizes on the next keyframe either way.
//!
//! Both run `AV_CODEC_FLAG_LOW_DELAY`; the host encodes zero-reorder streams (no B-frames, in-band
//! parameter sets on every IDR), so decode is strictly one-in/one-out.
//!
//! HDR is detected in-band from the decoded frame's transfer characteristic (`SMPTE2084` / PQ in the
//! HEVC VUI) — the same signal every other punktfunk client keys off — not from a protocol field.

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;
use std::ffi::c_void;
use std::ptr;
use windows::core::Interface; // ID3D11Device::clone().into_raw() for the FFmpeg hwdevice ctx

/// Which decode backend to use; the Settings UI persists this as a string.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DecoderPref {
    /// Try D3D11VA, fall back to software.
    #[default]
    Auto,
    /// Force D3D11VA (error out if unavailable, for debugging).
    Hardware,
    /// Force software decode.
    Software,
}

impl DecoderPref {
    pub fn from_name(s: &str) -> DecoderPref {
        match s {
            "hardware" => DecoderPref::Hardware,
            "software" => DecoderPref::Software,
            _ => DecoderPref::Auto,
        }
    }
}

pub enum DecodedFrame {
    Cpu(CpuFrame),
    Gpu(GpuFrame),
}

impl DecodedFrame {
    pub fn dims(&self) -> (u32, u32) {
        match self {
            DecodedFrame::Cpu(c) => (c.width, c.height),
            DecodedFrame::Gpu(g) => (g.width, g.height),
        }
    }
    pub fn hdr(&self) -> bool {
        match self {
            DecodedFrame::Cpu(c) => c.hdr,
            DecodedFrame::Gpu(g) => g.hdr,
        }
    }
}

/// Packed 4-byte-per-pixel frame for a D3D11 dynamic-texture upload (which takes a row pitch). The
/// bytes are `R8G8B8A8` for SDR and `X2BGR10` (== DXGI `R10G10B10A2`, R in the low 10 bits) for HDR.
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub pixels: Vec<u8>,
    /// BT.2020 PQ HDR10 frame: `pixels` is `X2BGR10` and the presenter switches to a 10-bit
    /// R10G10B10A2 + ST.2084 swapchain. `false` = ordinary 8-bit BT.709 SDR.
    pub hdr: bool,
}

/// A decoded frame still on the GPU: a D3D11 texture **array** plus the slice index the decoder
/// wrote this frame into. The presenter creates per-plane shader-resource views over the slice and
/// converts YUV→RGB in a pixel shader. The underlying surface stays alive — and out of the decoder's
/// reuse pool — for exactly as long as `guard` (an `av_frame_clone` of the decoded frame) lives.
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    /// Texture-array slice this frame occupies (`AVFrame::data[1]`).
    pub index: u32,
    /// BT.2020 PQ HDR10 (P010, ST.2084) vs ordinary 8-bit BT.709 SDR (NV12).
    pub hdr: bool,
    /// 10-bit (P010, R16 planes) vs 8-bit (NV12, R8 planes) — kept for the first-frame log; the
    /// present path keys colour/format off `hdr` (the host couples 10-bit ⟺ HDR).
    pub ten_bit: bool,
    guard: D3d11FrameGuard,
}

impl GpuFrame {
    /// The decoder's D3D11 texture array holding this frame's slice, borrowed from the live cloned
    /// `AVFrame`. Construct the windows-rs interface on the thread that will use it (the presenter /
    /// UI thread): COM interfaces are `!Send`, but the raw pointer is fine to carry across threads.
    pub fn texture_ptr(&self) -> *mut c_void {
        unsafe { (*self.guard.0).data[0] as *mut c_void }
    }
}

/// Owns a cloned decoded `AVFrame` (which refs the D3D11 surface in the decoder pool). Dropping it
/// releases the surface back for reuse. The clone is plain refcounted data; freeing it from the
/// presenter thread is fine.
pub struct D3d11FrameGuard(*mut ffmpeg::ffi::AVFrame);
unsafe impl Send for D3d11FrameGuard {}
impl Drop for D3d11FrameGuard {
    fn drop(&mut self) {
        unsafe { ffmpeg::ffi::av_frame_free(&mut self.0) };
    }
}

enum Backend {
    D3d11va(D3d11vaDecoder),
    Software(SoftwareDecoder),
}

pub struct Decoder {
    backend: Backend,
}

impl Decoder {
    pub fn new(pref: DecoderPref) -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        if pref != DecoderPref::Software {
            match D3d11vaDecoder::new() {
                Ok(d) => {
                    tracing::info!("D3D11VA hardware decode active (zero-copy)");
                    return Ok(Decoder {
                        backend: Backend::D3d11va(d),
                    });
                }
                Err(e) => {
                    if pref == DecoderPref::Hardware {
                        return Err(e.context("decoder=hardware but D3D11VA failed"));
                    }
                    tracing::info!(reason = %e, "D3D11VA unavailable — software decode");
                }
            }
        }
        Ok(Decoder {
            backend: Backend::Software(SoftwareDecoder::new()?),
        })
    }

    /// True for the zero-copy hardware backend (shown in the stream HUD).
    pub fn is_hardware(&self) -> bool {
        matches!(self.backend, Backend::D3d11va(_))
    }

    /// Feed one access unit; returns the decoded frame (the host's streams are one-in/one-out). A
    /// software decode error after packet loss is survivable — keep feeding. A D3D11VA error demotes
    /// to software for the rest of the session (the next IDR resynchronizes).
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedFrame>> {
        match &mut self.backend {
            Backend::D3d11va(d) => match d.decode(au) {
                Ok(f) => Ok(f.map(DecodedFrame::Gpu)),
                Err(e) => {
                    tracing::warn!(error = %e, "D3D11VA decode failed — falling back to software");
                    self.backend = Backend::Software(SoftwareDecoder::new()?);
                    Ok(None)
                }
            },
            Backend::Software(s) => Ok(s.decode(au)?.map(DecodedFrame::Cpu)),
        }
    }
}

// --- software backend ---------------------------------------------------------------

struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size **or output format** changes (mid-stream
    /// `Reconfigure`, or an SDR↔HDR flip): `(ctx, src_fmt, w, h, dst_fmt)`.
    sws: Option<(scaling::Context, Pixel, u32, u32, Pixel)>,
}

impl SoftwareDecoder {
    fn new() -> Result<SoftwareDecoder> {
        let codec =
            ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC).ok_or(anyhow!("no HEVC decoder"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open HEVC decoder")?;
        Ok(SoftwareDecoder { decoder, sws: None })
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<CpuFrame>> {
        let packet = ffmpeg::Packet::copy(au);
        self.decoder
            .send_packet(&packet)
            .map_err(|e| anyhow!("send_packet: {e}"))?;
        let mut frame = AvFrame::empty();
        let mut out = None;
        while self.decoder.receive_frame(&mut frame).is_ok() {
            out = Some(self.convert(&frame)?);
        }
        Ok(out)
    }

    /// Convert the decoded YUV frame to a packed 4-byte format the presenter uploads directly:
    /// SDR → `RGBA` (BT.709), HDR (SMPTE ST.2084 / PQ transfer) → `X2BGR10` (== DXGI R10G10B10A2)
    /// using the BT.2020 matrix. For HDR the PQ-encoded values pass through unchanged (swscale only
    /// applies the YUV→RGB matrix + range, never the transfer) — exactly what an HDR10 swapchain wants.
    fn convert(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        use ffmpeg::color::TransferCharacteristic;
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        let hdr = frame.color_transfer_characteristic() == TransferCharacteristic::SMPTE2084;
        let dst = if hdr { Pixel::X2BGR10LE } else { Pixel::RGBA };
        let rebuild = !matches!(&self.sws, Some((_, f, sw, sh, d)) if *f == fmt && *sw == w && *sh == h && *d == dst);
        if rebuild {
            let mut ctx = scaling::Context::get(fmt, w, h, dst, w, h, scaling::Flags::POINT)
                .context("swscale context")?;
            if hdr {
                // BT.2020 non-constant-luminance YUV (limited range) → full-range RGB. swscale
                // applies only the matrix + range here, so the samples stay PQ-encoded.
                unsafe {
                    let coef = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_BT2020);
                    ffmpeg::ffi::sws_setColorspaceDetails(
                        ctx.as_mut_ptr(),
                        coef,
                        0, // src range: limited (video)
                        coef,
                        1, // dst range: full
                        0,
                        1 << 16,
                        1 << 16, // brightness / contrast / saturation defaults (16.16)
                    );
                }
            }
            self.sws = Some((ctx, fmt, w, h, dst));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        let mut conv = AvFrame::empty();
        sws.run(frame, &mut conv).map_err(|e| anyhow!("sws: {e}"))?;
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: conv.stride(0),
            pixels: conv.data(0).to_vec(),
            hdr,
        })
    }
}

// --- D3D11VA backend ------------------------------------------------------------------
//
// Raw FFI: ffmpeg-next has no hwaccel wrappers. The COM-typed hwcontext structs are declared here
// (stable FFmpeg public ABI) rather than relied on from ffmpeg-sys bindgen — the generic
// AVHWDeviceContext / AVHWFramesContext (whose payload is an opaque `void *hwctx`) come from
// ffmpeg-sys, and we cast `hwctx` to the structs below. All owned pointers are freed in Drop;
// decoded surfaces transfer out through D3d11FrameGuard.

const AVERROR_EAGAIN: i32 = -11; // -EAGAIN
const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8; // <d3d11.h>; FFmpeg ORs D3D11_BIND_DECODER itself

/// `hwcontext_d3d11va.h` — `AVHWDeviceContext::hwctx`. Leaving `lock` null makes FFmpeg install an
/// `ID3D11Multithread` default lock + set multithread protection on `device_context` during init,
/// which is what lets the presenter share this device's immediate context from the UI thread.
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

/// `hwcontext_d3d11va.h` — `AVHWFramesContext::hwctx`. `BindFlags` lets us add
/// `D3D11_BIND_SHADER_RESOURCE` so the decoded array texture is sampleable (zero copy).
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void, // ID3D11Texture2D* (null → FFmpeg allocates the pool)
    bind_flags: u32,      // UINT BindFlags
    misc_flags: u32,      // UINT MiscFlags
}

fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
}

/// libavcodec's `get_format` callback: accept the D3D11 hw surface, building a frames context whose
/// textures carry `BIND_SHADER_RESOURCE` (so the presenter can sample them). Returning anything but
/// `AV_PIX_FMT_D3D11` aborts hardware decode → the session demotes to software.
unsafe extern "C" fn get_format_d3d11(
    avctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi::*;
    unsafe {
        let mut found = false;
        while *list != AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == AVPixelFormat::AV_PIX_FMT_D3D11 {
                found = true;
                break;
            }
            list = list.add(1);
        }
        if !found {
            return AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let device_ref = (*avctx).hw_device_ctx;
        if device_ref.is_null() {
            return AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let frames_ref = av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            return AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let frames = (*frames_ref).data as *mut AVHWFramesContext;
        (*frames).format = AVPixelFormat::AV_PIX_FMT_D3D11;
        let sw = if (*avctx).sw_pix_fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            (*avctx).sw_pix_fmt
        } else {
            AVPixelFormat::AV_PIX_FMT_NV12
        };
        (*frames).sw_format = sw;
        (*frames).width = (*avctx).coded_width;
        (*frames).height = (*avctx).coded_height;
        // DPB + a few in-flight (decoded channel + the presenter's held frame); the host's
        // zero-reorder stream needs only a small DPB, so 20 is comfortable headroom.
        (*frames).initial_pool_size = 20;
        let fhw = (*frames).hwctx as *mut AVD3D11VAFramesContext;
        (*fhw).bind_flags = D3D11_BIND_SHADER_RESOURCE;
        let r = av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            let mut fr = frames_ref;
            av_buffer_unref(&mut fr);
            return AVPixelFormat::AV_PIX_FMT_NONE;
        }
        (*avctx).hw_frames_ctx = frames_ref; // decoder takes ownership
        AVPixelFormat::AV_PIX_FMT_D3D11
    }
}

struct D3d11vaDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    hw_device: *mut ffmpeg::ffi::AVBufferRef,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
}

// Single-owner pointers, only touched from the session pump thread.
unsafe impl Send for D3d11vaDecoder {}

impl D3d11vaDecoder {
    fn new() -> Result<D3d11vaDecoder> {
        use ffmpeg::ffi;
        let shared = crate::gpu::shared().ok_or_else(|| anyhow!("no shared D3D11 device"))?;
        if !shared.hardware {
            bail!("shared device is WARP (no hardware video decode)");
        }
        unsafe {
            // Build a D3D11VA hwdevice context around the *shared* device, so decoded textures live
            // on the same device the presenter samples + draws with.
            let hw_device =
                ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if hw_device.is_null() {
                bail!("av_hwdevice_ctx_alloc(D3D11VA) failed");
            }
            let devctx = (*hw_device).data as *mut ffi::AVHWDeviceContext;
            let d3dctx = (*devctx).hwctx as *mut AVD3D11VADeviceContext;
            // Hand FFmpeg an owned ref to the device + immediate context (it Releases them when the
            // hwdevice ctx is freed). `into_raw()` transfers a +1 ref without releasing.
            (*d3dctx).device = shared.device.clone().into_raw();
            (*d3dctx).device_context = shared.context.clone().into_raw();
            // lock left null → FFmpeg installs the ID3D11Multithread default lock in init.
            let r = ffi::av_hwdevice_ctx_init(hw_device);
            if r < 0 {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("av_hwdevice_ctx_init: {}", ffmpeg::Error::from(r));
            }

            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_HEVC);
            if codec.is_null() {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("no HEVC decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(get_format_d3d11);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // hwaccel: threads only add latency
            let r = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                ffi::avcodec_free_context(&mut ctx);
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("avcodec_open2 (D3D11VA): {}", ffmpeg::Error::from(r));
            }
            Ok(D3d11vaDecoder {
                ctx,
                hw_device,
                packet: ffi::av_packet_alloc(),
                frame: ffi::av_frame_alloc(),
            })
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<GpuFrame>> {
        use ffmpeg::ffi;
        unsafe {
            let r = ffi::av_new_packet(self.packet, au.len() as i32);
            if r < 0 {
                return Err(averr("av_new_packet", r));
            }
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
            let r = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if r < 0 {
                return Err(averr("send_packet", r));
            }
            let mut out = None;
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    return Err(averr("receive_frame", r));
                }
                out = Some(self.lift()?); // newest wins; older guards drop here
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// Lift the decoded D3D11 surface into a `GpuFrame`. `data[0]` is the texture array, `data[1]`
    /// the slice index. We `av_frame_clone` so the surface stays referenced (kept out of the reuse
    /// pool) until the presenter drops the guard.
    unsafe fn lift(&mut self) -> Result<GpuFrame> {
        use ffmpeg::ffi;
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                bail!("decoder returned a software frame (no D3D11 surface)");
            }
            let hdr =
                (*self.frame).color_trc == ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
            let ten_bit = {
                let hwfc = (*self.frame).hw_frames_ctx;
                !hwfc.is_null()
                    && (*((*hwfc).data as *const ffi::AVHWFramesContext)).sw_format
                        == ffi::AVPixelFormat::AV_PIX_FMT_P010LE
            };
            let cloned = ffi::av_frame_clone(self.frame);
            if cloned.is_null() {
                bail!("av_frame_clone failed");
            }
            let frame = GpuFrame {
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                index: (*self.frame).data[1] as usize as u32,
                hdr,
                ten_bit,
                guard: D3d11FrameGuard(cloned),
            };
            log_layout_once(frame.width, frame.height, frame.index, hdr, ten_bit);
            Ok(frame)
        }
    }
}

impl Drop for D3d11vaDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

/// One-time dump of the first decoded surface's layout — so a new GPU/driver combination's real
/// format (slice index range, HDR/bit-depth) is visible in the logs without a debugger.
fn log_layout_once(width: u32, height: u32, index: u32, hdr: bool, ten_bit: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if ONCE.swap(false, Ordering::Relaxed) {
        tracing::info!(
            width,
            height,
            slice = index,
            hdr,
            ten_bit,
            "D3D11VA first frame (zero-copy)"
        );
    }
}
