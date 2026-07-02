//! Video decode: reassembled HEVC access units → frames for the D3D11 presenter.
//!
//! Two backends, picked at session start (override via [`DecoderPref`] / the Settings UI):
//!
//! * **D3D11VA** (any GPU — the vendor-agnostic DXVA path on NVIDIA/AMD/Intel): libavcodec decodes
//!   on the GPU into an `ID3D11Texture2D` decode array (decoder-only bind — NVIDIA rejects a
//!   decoder array that is also a shader resource). The presenter copies each decoded slice into
//!   its own sampleable NV12/P010 texture and converts YUV→RGB in a shader — one cheap GPU-to-GPU
//!   copy per frame (no swscale, no CPU readback). The decode array is created by the process-wide
//!   shared device ([`crate::gpu`]) the presenter also draws with, so the copy stays on-GPU. This
//!   is the big latency/throughput win over software.
//! * **Software**: libavcodec on the CPU + swscale to the same planar layout the hardware path
//!   produces (NV12, or P010 for 10-bit) — the presenter uploads the two planes and runs the SAME
//!   YUV→RGB shaders, so hw/sw color math is identical. The fallback on a GPU-less box (WARP),
//!   when D3D11VA init fails, or when a mid-session hardware error demotes us — the host's
//!   IDR/RFI recovery resynchronizes on the next keyframe either way.
//!
//! D3D11VA viability is settled **before the session's first frame** by two probes: the adapter
//! must expose the negotiated codec's DXVA decode profile ([`decode_profile_supported`] — hwaccel
//! init otherwise only fails at the first AU, burning the IDR), and it must be able to create the
//! decode surface pool ([`d3d11va_decode_supported`]). Either failing commits to software decode
//! from frame one (a clean, gap-free stream) instead of dying mid-stream.
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
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11VideoDevice};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

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

/// A software-decoded frame in the same planar layout the hardware path produces: an NV12 (or
/// P010 for 10-bit) luma plane + interleaved chroma plane, each with its swscale row stride
/// (≥ the row bytes — swscale pads rows for SIMD). The presenter uploads them into two dynamic
/// plane textures sampled by the same shaders as the D3D11VA path.
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// Luma plane (`W×H` samples, 1 byte each; 2 for 10-bit) + its row stride in bytes.
    pub y: Vec<u8>,
    pub y_stride: usize,
    /// Interleaved chroma plane (`⌈W/2⌉×⌈H/2⌉` UV pairs) + its row stride in bytes.
    pub uv: Vec<u8>,
    pub uv_stride: usize,
    /// P010 sample layout (10 bits in the high bits of 16) vs NV12. Selects texture/SRV formats.
    pub ten_bit: bool,
    /// BT.2020 PQ HDR10 vs ordinary BT.709 SDR. Selects shader + swapchain colour space.
    pub hdr: bool,
}

/// A decoded frame still on the GPU: a D3D11 texture **array** plus the slice index the decoder
/// wrote this frame into. The presenter copies the slice into its own sampleable texture and
/// converts YUV→RGB in a pixel shader. The underlying surface stays alive — and out of the decoder's
/// reuse pool — for exactly as long as `guard` (an `av_frame_clone` of the decoded frame) lives.
pub struct GpuFrame {
    pub width: u32,
    pub height: u32,
    /// Texture-array slice this frame occupies (`AVFrame::data[1]`).
    pub index: u32,
    /// The decode pool is P010 (10 bits in the high bits) vs NV12 — from the frames context's
    /// `sw_format`. The presenter keys its copy-texture/SRV formats off this: they must match the
    /// source array exactly for `CopySubresourceRegion`.
    pub ten_bit: bool,
    /// BT.2020 PQ HDR10 (ST.2084 transfer) vs ordinary BT.709 SDR. Selects shader + swapchain
    /// colour space only (the host couples 10-bit ⟺ HDR today, but formats key off `ten_bit`).
    pub hdr: bool,
    guard: D3d11FrameGuard,
}

impl GpuFrame {
    /// The decoder's D3D11 texture array holding this frame's slice, borrowed from the live cloned
    /// `AVFrame`. Construct the windows-rs interface on the thread that will use it (the render
    /// thread): COM interfaces are `!Send`, but the raw pointer is fine to carry across threads.
    pub fn texture_ptr(&self) -> *mut c_void {
        unsafe { (*self.guard.0).data[0] as *mut c_void }
    }
}

/// Owns a cloned decoded `AVFrame` (which refs the D3D11 surface in the decoder pool). Dropping it
/// releases the surface back for reuse. The clone is plain refcounted data; freeing it from the
/// render thread is fine.
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
    /// The negotiated codec, so a mid-session D3D11VA→software demotion rebuilds for the same codec.
    codec_id: ffmpeg::codec::Id,
}

/// Map a negotiated `quic` codec bit to the FFmpeg decoder id the client opens.
pub fn ffmpeg_codec_id(wire: u8) -> ffmpeg::codec::Id {
    match wire {
        punktfunk_core::quic::CODEC_H264 => ffmpeg::codec::Id::H264,
        punktfunk_core::quic::CODEC_AV1 => ffmpeg::codec::Id::AV1,
        _ => ffmpeg::codec::Id::HEVC,
    }
}

/// The `quic` codec bitfield this client can decode — whatever FFmpeg has a decoder for (HEVC/H.264
/// always; AV1 when built in). Advertised to the host so it never emits a codec we can't decode.
/// Deliberately NOT gated on the DXVA profiles: software decode covers anything FFmpeg can.
pub fn decodable_codecs() -> u8 {
    let _ = ffmpeg::init();
    let mut bits = 0u8;
    for (id, bit) in [
        (ffmpeg::codec::Id::HEVC, punktfunk_core::quic::CODEC_HEVC),
        (ffmpeg::codec::Id::H264, punktfunk_core::quic::CODEC_H264),
        (ffmpeg::codec::Id::AV1, punktfunk_core::quic::CODEC_AV1),
    ] {
        if ffmpeg::decoder::find(id).is_some() {
            bits |= bit;
        }
    }
    bits
}

impl Decoder {
    pub fn new(pref: DecoderPref, codec_id: ffmpeg::codec::Id) -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        if pref != DecoderPref::Software {
            match D3d11vaDecoder::new(codec_id) {
                Ok(d) => {
                    tracing::info!(?codec_id, "D3D11VA hardware decode active");
                    return Ok(Decoder {
                        backend: Backend::D3d11va(d),
                        codec_id,
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
            backend: Backend::Software(SoftwareDecoder::new(codec_id)?),
            codec_id,
        })
    }

    /// True for the GPU hardware backend (shown in the stream HUD).
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
                    self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
                    Ok(None)
                }
            },
            Backend::Software(s) => Ok(s.decode(au)?.map(DecodedFrame::Cpu)),
        }
    }
}

// --- DXVA decode-profile probe --------------------------------------------------------

/// DXVA decode-profile GUIDs (`dxva.h`), defined locally so no extra windows-rs feature or
/// metadata surface is pulled in for four constants.
const PROFILE_H264_VLD_NOFGT: GUID = GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
const PROFILE_HEVC_VLD_MAIN: GUID = GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0);
const PROFILE_HEVC_VLD_MAIN10: GUID = GUID::from_u128(0x107af0e0_ef1a_4d19_aba8_67a163073d13);
const PROFILE_AV1_VLD_PROFILE0: GUID = GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a);

/// Does the shared device's adapter expose a DXVA decode profile for `codec_id`? Checked before
/// building the FFmpeg hwdevice because hwaccel selection (`get_format`) only runs on the FIRST
/// access unit — an unsupported profile would otherwise burn the opening IDR and recover through
/// the mid-stream demotion path instead of committing to software up front. Also logs (once) the
/// adapter's full profile list plus Main10 availability — the forensics for a new GPU/driver.
fn decode_profile_supported(device: &ID3D11Device, codec_id: ffmpeg::codec::Id) -> Result<()> {
    let video: ID3D11VideoDevice = device
        .cast()
        .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
    let profiles: Vec<GUID> = unsafe {
        let n = video.GetVideoDecoderProfileCount();
        (0..n)
            .filter_map(|i| video.GetVideoDecoderProfile(i).ok())
            .collect()
    };
    log_profiles_once(&profiles);

    let (wanted, format, name): (GUID, DXGI_FORMAT, &str) = match codec_id {
        ffmpeg::codec::Id::H264 => (PROFILE_H264_VLD_NOFGT, DXGI_FORMAT_NV12, "H.264 VLD NoFGT"),
        ffmpeg::codec::Id::HEVC => (PROFILE_HEVC_VLD_MAIN, DXGI_FORMAT_NV12, "HEVC Main"),
        ffmpeg::codec::Id::AV1 => (PROFILE_AV1_VLD_PROFILE0, DXGI_FORMAT_NV12, "AV1 Profile 0"),
        other => bail!("no DXVA profile known for {other:?}"),
    };
    let ok = profiles.contains(&wanted)
        && unsafe { video.CheckVideoDecoderFormat(&wanted, format) }
            .map(|b| b.as_bool())
            .unwrap_or(false);
    if !ok {
        bail!("adapter exposes no {name} decode profile");
    }
    // 10-bit (a mid-session HDR upgrade needs Main10): informational — if it's missing the
    // decode error → software demotion + keyframe re-request path covers the switch.
    if codec_id == ffmpeg::codec::Id::HEVC {
        let main10 = profiles.contains(&PROFILE_HEVC_VLD_MAIN10)
            && unsafe { video.CheckVideoDecoderFormat(&PROFILE_HEVC_VLD_MAIN10, DXGI_FORMAT_P010) }
                .map(|b| b.as_bool())
                .unwrap_or(false);
        tracing::info!(main10, "HEVC Main10 (10-bit/HDR) decode profile");
    }
    Ok(())
}

/// One-time dump of the adapter's DXVA decode profiles.
fn log_profiles_once(profiles: &[GUID]) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if ONCE.swap(false, Ordering::Relaxed) {
        let list: Vec<String> = profiles.iter().map(|g| format!("{g:?}")).collect();
        tracing::info!(count = profiles.len(), profiles = ?list, "adapter DXVA decode profiles");
    }
}

// --- software backend ---------------------------------------------------------------

struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size **or output format** changes (mid-stream
    /// `Reconfigure`, or an 8↔10-bit flip): `(ctx, src_fmt, w, h, dst_fmt)`.
    sws: Option<(scaling::Context, Pixel, u32, u32, Pixel)>,
}

impl SoftwareDecoder {
    fn new(codec_id: ffmpeg::codec::Id) -> Result<SoftwareDecoder> {
        let codec = ffmpeg::decoder::find(codec_id)
            .ok_or_else(|| anyhow!("no {codec_id:?} decoder in libavcodec"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open video decoder")?;
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

    /// Convert the decoded planar YUV to the hardware path's layout: NV12 for 8-bit, P010 for
    /// 10-bit — a chroma interleave (and 10→16-high-bits shift), NOT a colour conversion. The
    /// matrix/range/transfer handling all lives in the presenter's shaders, shared with the
    /// D3D11VA path, so software frames are bit-comparable with hardware ones.
    fn convert(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        use ffmpeg::color::TransferCharacteristic;
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        let hdr = frame.color_transfer_characteristic() == TransferCharacteristic::SMPTE2084;
        // Source bit depth from the pix-fmt descriptor (stable FFmpeg public API).
        let ten_bit = unsafe {
            let desc = ffmpeg::ffi::av_pix_fmt_desc_get(fmt.into());
            !desc.is_null() && (*desc).comp[0].depth > 8
        };
        let dst = if ten_bit { Pixel::P010LE } else { Pixel::NV12 };
        let rebuild = !matches!(&self.sws, Some((_, f, sw, sh, d)) if *f == fmt && *sw == w && *sh == h && *d == dst);
        if rebuild {
            let ctx = scaling::Context::get(fmt, w, h, dst, w, h, scaling::Flags::POINT)
                .context("swscale context")?;
            self.sws = Some((ctx, fmt, w, h, dst));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        let mut conv = AvFrame::empty();
        sws.run(frame, &mut conv).map_err(|e| anyhow!("sws: {e}"))?;
        Ok(CpuFrame {
            width: w,
            height: h,
            y: conv.data(0).to_vec(),
            y_stride: conv.stride(0),
            uv: conv.data(1).to_vec(),
            uv_stride: conv.stride(1),
            ten_bit,
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

/// D3D11VA decode surface pool depth: the zero-reorder DPB (1–2 refs) + the bounded decoded channel
/// (2) + the frame the presenter currently holds (until its copy flushes) + one in-flight decode —
/// 12 is comfortable. A GPU that can't create the pool at all is gated out by
/// `d3d11va_decode_supported` and the session uses software decode.
const DECODE_POOL_SIZE: i32 = 12;

/// `hwcontext_d3d11va.h` — `AVHWDeviceContext::hwctx`. Leaving `lock` null makes FFmpeg install an
/// `ID3D11Multithread` default lock + set multithread protection on `device_context` during init,
/// which is what lets the presenter share this device's immediate context from the render thread.
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

/// `hwcontext_d3d11va.h` — `AVHWFramesContext::hwctx`. The header is explicit: "The user must at
/// least set D3D11_BIND_DECODER if the frames context is to be used for video decoding" — a
/// user-built frames context gets NO default (BindFlags 0 → `CreateTexture2D` E_INVALIDARG); the
/// automatic OR-in lives only in libavcodec's own frames-param path, which we bypass.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void, // ID3D11Texture2D* (null → FFmpeg allocates the pool)
    bind_flags: u32,      // UINT BindFlags
    misc_flags: u32,      // UINT MiscFlags
    texture_infos: *mut c_void, // AVD3D11FrameDescriptor* (FFmpeg-managed)
}

/// `D3D11_BIND_DECODER` — the decode pool's ONLY bind flag. Adding `D3D11_BIND_SHADER_RESOURCE`
/// is what NVIDIA rejects on a decoder texture ARRAY; the presenter samples via its own copy.
const BIND_DECODER: u32 = 0x200;

fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
}

/// libavcodec's `get_format` callback: pick the D3D11 hw surface format and nothing else.
/// Deliberately does NOT build a frames context — with `hw_device_ctx` set and `hw_frames_ctx`
/// left null, libavcodec derives the decode pool itself (`ff_decode_get_hw_frames_ctx`), applying
/// every vendor quirk: DXVA surface alignment (128 for HEVC/AV1), DPB-based pool sizing, and the
/// decoder-only `D3D11_BIND_DECODER` flags. A hand-built context validated on NVIDIA was rejected
/// by Intel at the first `SubmitDecoderBuffers` (E_INVALIDARG) — the vendor-proof path is the one
/// the ffmpeg CLI/mpv ship. Returning anything but `AV_PIX_FMT_D3D11` aborts hardware decode →
/// the session demotes to software.
unsafe extern "C" fn get_format_d3d11(
    avctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi::*;
    unsafe {
        if (*avctx).hw_device_ctx.is_null() {
            return AVPixelFormat::AV_PIX_FMT_NONE;
        }
        while *list != AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == AVPixelFormat::AV_PIX_FMT_D3D11 {
                return AVPixelFormat::AV_PIX_FMT_D3D11;
            }
            list = list.add(1);
        }
        AVPixelFormat::AV_PIX_FMT_NONE
    }
}

/// Predict whether D3D11VA decode will work by doing EXACTLY what the decoder's `get_format` does —
/// allocate an `AVHWFramesContext` (decoder-only pool, no shader-resource bind) and initialize it,
/// which creates the real NV12 decode surface array. On a GPU/driver that can't create the pool this
/// fails here, up front, so the session commits to software decode from the first frame (a clean,
/// gap-free stream) rather than decoding the IDR then dying mid-stream on a texture error that a
/// software demotion can't reliably recover from (the host's infinite GOP won't re-send an IDR).
unsafe fn d3d11va_decode_supported(hw_device: *mut ffmpeg::ffi::AVBufferRef) -> bool {
    use ffmpeg::ffi::*;
    unsafe {
        let frames_ref = av_hwframe_ctx_alloc(hw_device);
        if frames_ref.is_null() {
            return false;
        }
        let frames = (*frames_ref).data as *mut AVHWFramesContext;
        (*frames).format = AVPixelFormat::AV_PIX_FMT_D3D11;
        (*frames).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames).width = 1920;
        (*frames).height = 1152; // 128-aligned 1080p surface (the HEVC DXVA alignment, see get_format)
        (*frames).initial_pool_size = DECODE_POOL_SIZE;
        // Decoder-only — matches get_format exactly.
        let fhw = (*frames).hwctx as *mut AVD3D11VAFramesContext;
        (*fhw).bind_flags = BIND_DECODER;
        let r = av_hwframe_ctx_init(frames_ref);
        let mut fr = frames_ref;
        av_buffer_unref(&mut fr);
        r >= 0
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
    fn new(codec_id: ffmpeg::codec::Id) -> Result<D3d11vaDecoder> {
        use ffmpeg::ffi;
        let shared = crate::gpu::shared().ok_or_else(|| anyhow!("no shared D3D11 device"))?;
        if !shared.hardware {
            bail!("shared device is WARP (no hardware video decode)");
        }
        // The adapter must expose the codec's DXVA profile — checked here, not at the first AU.
        decode_profile_supported(&shared.device, codec_id)?;
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

            // Up-front viability probe (see `d3d11va_decode_supported`): a GPU/driver that can't
            // create the decode surface pool commits to software NOW, so it decodes cleanly from the
            // first frame instead of failing mid-stream (which a demotion can't reliably recover).
            if !d3d11va_decode_supported(hw_device) {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("GPU can't create the D3D11VA decode surface pool — using software decode");
            }

            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(get_format_d3d11);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // hwaccel: threads only add latency.
            (*ctx).thread_count = 1;
            // On top of the DPB-based pool libavcodec sizes for us: the bounded decoded channel
            // (2) + the frame the presenter holds until its copy flushes + margin.
            (*ctx).extra_hw_frames = 4;
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
                ten_bit,
                hdr,
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
            "D3D11VA first frame"
        );
    }
}
