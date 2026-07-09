//! D3D11VA hardware decode (Windows) for the Vulkan presenter — the vendor-agnostic DXVA
//! path that covers what Vulkan Video can't (Intel's Windows driver foremost, which has no
//! video-decode queue and previously landed on CPU decode).
//!
//! Ported from the retired in-process WinUI presenter's decoder (`clients/windows/src/video.rs`)
//! with one structural change: that presenter sampled D3D11 textures directly, while ours draws
//! with Vulkan. Bridging rules, all learned the hard way there:
//!
//! * The **decode pool stays libavcodec-derived** (`get_format` sets no frames context): a
//!   hand-built pool validated on NVIDIA was rejected by Intel at the first
//!   `SubmitDecoderBuffers` — and Intel is the GPU this backend exists for. That also means the
//!   decode surfaces carry no share flags, so they can't be imported into Vulkan directly.
//! * Each decoded slice goes through the fixed-function **`ID3D11VideoProcessor`**
//!   (`VideoProcessorBlt`, NV12/P010 → BGRA8 — the conversion every Windows video player
//!   exercises on every vendor) into a small ring of **shareable RGBA textures** created with
//!   `SHARED_NTHANDLE | SHARED_KEYEDMUTEX`. Single-plane RGBA is deliberate: the presenter's
//!   Vulkan import of a *multiplanar* NV12 D3D11 texture device-losts on NVIDIA no matter how
//!   it's consumed (plane-view sampling, DMA copy — all validation-clean, all TDR; bisected
//!   2026-07-09), while RGBA D3D11↔Vulkan interop is the path Chromium/ANGLE ship everywhere.
//!   The presenter imports a ring slot's NT handle per frame (`pf-presenter/src/d3d11.rs`,
//!   `VK_KHR_external_memory_win32`) and blits it straight into its video image — the frames
//!   arrive as ready sRGB, no CSC pass.
//! * Cross-API exclusion + write→read visibility ride the slot's keyed mutex
//!   (`VK_KHR_win32_keyed_mutex`); both sides take and release it with **key 0**: a frame the
//!   presenter drops (arrival-paced, newest wins) is simply never acquired, which a
//!   key-ping-pong protocol would deadlock on.
//! * An HDR (PQ/BT.2020) stream is tone-mapped to SDR by the video processor (input colour
//!   space `G2084_P2020`, output sRGB): correct picture, no HDR presentation on this backend —
//!   its targets (Intel iGPU laptops) are SDR panels; HDR-first boxes take Vulkan Video.
//!
//! The decode device is created on the **presenter's adapter** (matched by the Vulkan device's
//! LUID) so the shared textures never cross GPUs on a multi-adapter box.

use crate::video::ColorDesc;
use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg_next as ffmpeg;
use std::ffi::c_void;
use std::ptr;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};

/// Ring of shareable hand-off textures. Bounds how many decoded-but-unpresented frames can
/// exist without a slot being rewritten under an in-flight older frame: the pump's decoded
/// channel holds 2 and the presenter drains to newest with one frame in flight, so 3 are ever
/// outstanding — 6 leaves margin without meaningful VRAM cost.
const RING_SLOTS: usize = 6;

/// Keyed-mutex acquire budget (ms) on the DECODE side. The presenter holds a slot only for one
/// submit's GPU lifetime; multiple seconds means the render thread died — surface an error
/// (which demotes to software) instead of wedging the decode loop.
const ACQUIRE_TIMEOUT_MS: u32 = 2000;

/// Probe pool size — mirrors what libavcodec sizes for a worst-case DPB (legacy value).
const DECODE_POOL_SIZE: i32 = 12;

/// `D3D11_BIND_DECODER` — the decode pool's ONLY bind flag (see `get_format_d3d11`).
const BIND_DECODER: u32 = 0x200;

// DXVA decode-profile GUIDs (`dxva.h`), defined locally so no extra windows-rs feature or
// metadata surface is pulled in for four constants.
const PROFILE_H264_VLD_NOFGT: GUID = GUID::from_u128(0x1b81be68_a0c7_11d3_b984_00c04f2e73c5);
const PROFILE_HEVC_VLD_MAIN: GUID = GUID::from_u128(0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0);
const PROFILE_HEVC_VLD_MAIN10: GUID = GUID::from_u128(0x107af0e0_ef1a_4d19_aba8_67a163073d13);
const PROFILE_AV1_VLD_PROFILE0: GUID = GUID::from_u128(0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a);

/// One decoded frame, parked in a ring slot the presenter imports by NT handle. Plain POD —
/// the ring (and its handles) belong to the decoder and outlive every in-flight frame; the
/// presenter must NOT close the handle. Cross-API exclusion + visibility ride the slot's
/// keyed mutex (key 0 on both sides), not this struct.
pub struct D3d11Frame {
    pub width: u32,
    pub height: u32,
    /// What the ring slot actually CONTAINS after the video processor's conversion: sRGB
    /// BT.709 full-range RGB — regardless of the stream's own CICP (a PQ stream was
    /// tone-mapped). The presenter keys SDR/HDR handling off this, so it always reads SDR.
    pub color: ColorDesc,
    /// The ring slot's NT shared handle (`IDXGIResource1::CreateSharedHandle`), stable for the
    /// ring's lifetime. Raw `isize` so the frame crosses the pump→presenter channel.
    pub handle: isize,
    /// Ring generation — bumped when the ring is rebuilt (stream size change), so a
    /// presenter-side import cache could never alias a stale handle. Informational today
    /// (the presenter imports per frame).
    pub generation: u32,
}

// --- FFmpeg hwcontext_d3d11va ABI (repr(C) mirrors, same as the legacy decoder) --------------

/// `hwcontext_d3d11va.h` — `AVHWDeviceContext::hwctx` for D3D11VA. FFmpeg installs the
/// `ID3D11Multithread` default lock + multithread protection during init, which is what lets
/// the presenter-side device share textures with the decode thread safely.
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

/// `hwcontext_d3d11va.h` — `AVHWFramesContext::hwctx`. A user-built frames context gets NO
/// default bind flags (BindFlags 0 → `CreateTexture2D` E_INVALIDARG); only the probe below
/// builds one, and it sets `BIND_DECODER` exactly like libavcodec's own path.
#[repr(C)]
struct AVD3D11VAFramesContext {
    texture: *mut c_void, // ID3D11Texture2D* (null → FFmpeg allocates the pool)
    bind_flags: u32,      // UINT BindFlags
    misc_flags: u32,      // UINT MiscFlags
    texture_infos: *mut c_void, // AVD3D11FrameDescriptor* (FFmpeg-managed)
}

fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
}

/// libavcodec's `get_format` callback: pick the D3D11 hw surface format and nothing else.
/// Deliberately does NOT build a frames context — with `hw_device_ctx` set and `hw_frames_ctx`
/// left null, libavcodec derives the decode pool itself (`ff_decode_get_hw_frames_ctx`),
/// applying every vendor quirk: DXVA surface alignment (128 for HEVC/AV1), DPB-based pool
/// sizing, and the decoder-only `D3D11_BIND_DECODER` flags. A hand-built context validated on
/// NVIDIA was rejected by Intel at the first `SubmitDecoderBuffers` (E_INVALIDARG) — the
/// vendor-proof path is the one the ffmpeg CLI/mpv ship.
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

/// Does the adapter expose a DXVA decode profile for `codec_id`? Checked before building the
/// FFmpeg hwdevice because hwaccel selection (`get_format`) only runs on the FIRST access
/// unit — an unsupported profile would otherwise burn the opening IDR and recover through the
/// mid-stream demotion path instead of committing to software up front.
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
    // 10-bit (a mid-session HDR upgrade needs Main10): informational — if it's missing, the
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

/// Predict whether D3D11VA decode will work by doing EXACTLY what the decoder's `get_format`
/// leads to — allocate an `AVHWFramesContext` (decoder-only pool) and initialize it, which
/// creates the real NV12 decode surface array. On a GPU/driver that can't create the pool this
/// fails here, up front, so the session commits to software from the first frame (a clean,
/// gap-free stream) instead of dying mid-stream on the opening IDR.
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
        (*frames).height = 1152; // 128-aligned 1080p surface (the HEVC DXVA alignment)
        (*frames).initial_pool_size = DECODE_POOL_SIZE;
        let fhw = (*frames).hwctx as *mut AVD3D11VAFramesContext;
        (*fhw).bind_flags = BIND_DECODER;
        let r = av_hwframe_ctx_init(frames_ref);
        let mut fr = frames_ref;
        av_buffer_unref(&mut fr);
        r >= 0
    }
}

/// Create the decode device on the presenter's adapter. `luid` is the Vulkan device's
/// `VkPhysicalDeviceIDProperties::deviceLUID` (little-endian LowPart‖HighPart) — matching it
/// keeps the shared textures on one GPU. `None`/no match falls back to the first hardware
/// adapter (single-GPU boxes; a WARP-only box fails out to software decode).
fn create_device(luid: Option<[u8; 8]>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut chosen: Option<IDXGIAdapter1> = None;
    let mut fallback: Option<IDXGIAdapter1> = None;
    for i in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue; // WARP can't hardware-decode; software decode covers that box anyway
        }
        if fallback.is_none() {
            fallback = Some(adapter.clone());
        }
        if let Some(want) = luid {
            let mut have = [0u8; 8];
            have[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
            have[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
            if have == want {
                chosen = Some(adapter);
                break;
            }
        }
    }
    if chosen.is_none() && luid.is_some() && fallback.is_some() {
        tracing::warn!(
            "no DXGI adapter matches the Vulkan device LUID — using the first hardware adapter"
        );
    }
    let adapter = chosen
        .or(fallback)
        .ok_or_else(|| anyhow!("no hardware DXGI adapter"))?;
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
    // The decode (FFmpeg video context) and our copy (immediate context) run on the decode
    // thread, but FFmpeg's own workers touch the device too — same protection the legacy
    // shared device enabled (FFmpeg would install it during hwdevice init anyway; explicit
    // keeps the invariant obvious).
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // Returns the PREVIOUS protection state — nothing to act on.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

/// One shareable ring slot: the NV12/P010 texture, its keyed mutex, and the NT handle the
/// presenter imports. Handle closed on drop (the presenter never owns it).
struct Slot {
    /// The shared texture itself — everything below views into it; kept for its lifetime.
    _tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: HANDLE,
    /// The video processor's render target over the texture — `VideoProcessorBlt`'s target.
    out_view: ID3D11VideoProcessorOutputView,
}

impl Drop for Slot {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// The hand-off ring + the video processor that fills it (both sized to the stream, so a
/// mid-stream `Reconfigure` rebuilds the whole bundle). See the module docs.
struct SharedRing {
    slots: Vec<Slot>,
    vp: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
    next: usize,
    generation: u32,
}

impl SharedRing {
    fn build(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        width: u32,
        height: u32,
        generation: u32,
    ) -> Result<SharedRing> {
        // The video processor: NV12/P010 in, BGRA8 out, 1:1 (no scaling — the Vulkan side
        // scales at composite time like every other path). Frame rates are advisory.
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("CreateVideoProcessorEnumerator")?;
        let vp = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            // Single-plane BGRA8: the ONLY hand-off format whose Vulkan import is a
            // universally exercised driver path (see the module docs — NV12 import TDRs
            // on NVIDIA despite being advertised).
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // RENDER_TARGET: the video processor's output view renders into it.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut slots = Vec::with_capacity(RING_SLOTS);
        for _ in 0..RING_SLOTS {
            let mut tex = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .context("create shared hand-off texture")?;
            let tex: ID3D11Texture2D = tex.expect("CreateTexture2D succeeded");
            let mutex: IDXGIKeyedMutex =
                tex.cast().context("shared texture lacks IDXGIKeyedMutex")?;
            let resource: IDXGIResource1 =
                tex.cast().context("shared texture lacks IDXGIResource1")?;
            let handle = unsafe {
                resource.CreateSharedHandle(
                    None,
                    (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
                    None,
                )
            }
            .context("CreateSharedHandle")?;
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                // Anonymous.Texture2D.MipSlice = 0 — the zeroed default.
                ..Default::default()
            };
            let mut out_view = None;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &ov_desc,
                    Some(&mut out_view),
                )
            }
            .context("CreateVideoProcessorOutputView")?;
            let out_view = out_view.expect("output view created");
            slots.push(Slot {
                _tex: tex,
                mutex,
                handle,
                out_view,
            });
        }
        tracing::info!(
            width,
            height,
            slots = RING_SLOTS,
            generation,
            "D3D11 shared hand-off ring built (VideoProcessor → BGRA8)"
        );
        Ok(SharedRing {
            slots,
            vp,
            enumerator,
            width,
            height,
            next: 0,
            generation,
        })
    }
}

pub(crate) struct D3d11vaDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    hw_device: *mut ffmpeg::ffi::AVBufferRef,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Creates the per-ring video processor + views.
    video_device: ID3D11VideoDevice,
    /// Runs the per-frame `VideoProcessorBlt`; the `1` interface for the DXGI colour-space
    /// setters (Win10 1703+, universally present — init fails to software without it).
    video_context1: ID3D11VideoContext1,
    ring: Option<SharedRing>,
}

// Single-owner pointers + COM interfaces, only touched from the session pump thread (the
// decode loop); the presenter reaches the shared textures exclusively via their NT handles.
unsafe impl Send for D3d11vaDecoder {}

impl D3d11vaDecoder {
    pub(crate) fn new(
        codec_id: ffmpeg::codec::Id,
        luid: Option<[u8; 8]>,
    ) -> Result<D3d11vaDecoder> {
        use ffmpeg::ffi;
        let (device, context) = create_device(luid)?;
        // The adapter must expose the codec's DXVA profile — checked here, not at the first AU.
        decode_profile_supported(&device, codec_id)?;
        // The hand-off converter's interfaces, up front (their absence must route to software
        // decode NOW, not burn the opening IDR).
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
        let video_context1: ID3D11VideoContext1 = context
            .cast()
            .context("context lacks ID3D11VideoContext1 (pre-1703 Windows?)")?;
        unsafe {
            let hw_device =
                ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if hw_device.is_null() {
                bail!("av_hwdevice_ctx_alloc(D3D11VA) failed");
            }
            let devctx = (*hw_device).data as *mut ffi::AVHWDeviceContext;
            let d3dctx = (*devctx).hwctx as *mut AVD3D11VADeviceContext;
            // Hand FFmpeg an owned ref to the device + immediate context (it Releases them when
            // the hwdevice ctx is freed). `into_raw()` transfers a +1 ref without releasing.
            (*d3dctx).device = device.clone().into_raw();
            (*d3dctx).device_context = context.clone().into_raw();
            // lock left null → FFmpeg installs the ID3D11Multithread default lock in init.
            let r = ffi::av_hwdevice_ctx_init(hw_device);
            if r < 0 {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("av_hwdevice_ctx_init: {}", ffmpeg::Error::from(r));
            }
            // Up-front viability probe (see `d3d11va_decode_supported`).
            if !d3d11va_decode_supported(hw_device) {
                let mut hw = hw_device;
                ffi::av_buffer_unref(&mut hw);
                bail!("GPU can't create the D3D11VA decode surface pool");
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
            (*ctx).thread_count = 1; // hwaccel: threads only add latency
                                     // On top of the DPB-based pool libavcodec sizes: margin for the frames briefly held
                                     // between decode and the ring copy (the copy runs immediately, so this is small).
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
                device,
                context,
                video_device,
                video_context1,
                ring: None,
            })
        }
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<D3d11Frame>> {
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
                if r == ffmpeg::ffi::AVERROR(ffmpeg::ffi::EAGAIN) {
                    break;
                }
                if r < 0 {
                    return Err(averr("receive_frame", r));
                }
                let lifted = self.lift();
                // The decode surface goes back to the pool NOW — the ring copy (queued ahead
                // of any later decoder write on the same immediate context) already owns the
                // pixels. No cross-thread AVFrame guard exists in this backend at all.
                ffi::av_frame_unref(self.frame);
                out = Some(lifted?); // newest wins (one-in/one-out streams make this moot)
            }
            Ok(out)
        }
    }

    /// Convert the decoded slice into the next ring slot (`VideoProcessorBlt`, NV12/P010 →
    /// BGRA8) under its keyed mutex and describe the hand-off. The mutex acquire also
    /// back-pressures against the presenter still reading this slot (only possible if the
    /// stream runs `RING_SLOTS` ahead of present).
    unsafe fn lift(&mut self) -> Result<D3d11Frame> {
        use ffmpeg::ffi;
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                bail!("decoder returned a software frame (no D3D11 surface)");
            }
            let width = (*self.frame).width as u32;
            let height = (*self.frame).height as u32;
            let color = ColorDesc::from_raw(self.frame);
            // AddRef'd locals so the mutable `ring` borrow below doesn't lock all of `self`.
            let video_device = self.video_device.clone();
            let video_context1 = self.video_context1.clone();
            let context = self.context.clone();
            // (Re)build the ring + video processor on first use or a stream size change (the
            // hand-off is BGRA8 regardless of the stream's bit depth, so depth never rebuilds).
            let rebuild = self
                .ring
                .as_ref()
                .is_none_or(|r| r.width != width || r.height != height);
            if rebuild {
                let generation = self.ring.as_ref().map_or(0, |r| r.generation + 1);
                self.ring = Some(SharedRing::build(
                    &self.device,
                    &video_device,
                    width,
                    height,
                    generation,
                )?);
            }
            let ring = self.ring.as_mut().expect("ring built above");
            let slot_idx = ring.next;
            ring.next = (ring.next + 1) % ring.slots.len();
            let slot = &ring.slots[slot_idx];

            let raw = (*self.frame).data[0] as *mut c_void;
            let src: ID3D11Texture2D = ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or_else(|| anyhow!("null D3D11 texture on decoded frame"))?
                .clone();
            let index = (*self.frame).data[1] as usize as u32;

            // Input view over THIS slice of the decode array (cheap per-frame object).
            let mut iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0, // surface format speaks for itself
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                // Anonymous.Texture2D zeroed (MipSlice 0); ArraySlice is per-frame below.
                ..Default::default()
            };
            iv_desc.Anonymous.Texture2D.ArraySlice = index;
            let mut in_view = None;
            video_device
                .CreateVideoProcessorInputView(&src, &ring.enumerator, &iv_desc, Some(&mut in_view))
                .context("CreateVideoProcessorInputView")?;
            let in_view = in_view.expect("input view created");

            // Colour spaces per frame (the host flips PQ in-band): YCbCr in, sRGB out — a PQ
            // stream is tone-mapped to SDR by the processor (module docs). CICP → DXGI enums.
            let in_cs = match (color.transfer, color.matrix, color.full_range) {
                (16, _, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
                (_, 9, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
                (_, _, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            };
            video_context1.VideoProcessorSetStreamColorSpace1(&ring.vp, 0, in_cs);
            video_context1.VideoProcessorSetOutputColorSpace1(
                &ring.vp,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
                ppFutureSurfaces: ptr::null_mut(),
                ppPastSurfacesRight: ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: ptr::null_mut(),
            };
            let handle = slot.handle.0 as isize;
            let generation = ring.generation;
            let mut streams = [stream];
            slot.mutex
                .AcquireSync(0, ACQUIRE_TIMEOUT_MS)
                .ok()
                .context("keyed-mutex acquire (decode side) timed out")?;
            let blt = video_context1.VideoProcessorBlt(&ring.vp, &slot.out_view, 0, &streams);
            // Balance the ManuallyDrop refs the stream struct carried BEFORE error-checking.
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurfaceRight);
            let release = slot.mutex.ReleaseSync(0);
            blt.ok().context("VideoProcessorBlt")?;
            release.ok().context("keyed-mutex release")?;
            // Get the conversion moving now — the presenter's GPU-side acquire waits on its
            // completion, and an unflushed deferred batch would add a driver-decided delay.
            context.Flush();

            log_layout_once(width, height, index, color.is_pq());
            Ok(D3d11Frame {
                width,
                height,
                // What the slot now CONTAINS: sRGB BT.709 full-range RGB (PQ was tone-mapped).
                color: ColorDesc {
                    primaries: 1,
                    transfer: 13, // sRGB (H.273)
                    matrix: 0,    // identity — RGB
                    full_range: true,
                },
                handle,
                generation,
            })
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
        // `ring` drops after the codec: no decode can be in flight past avcodec_free_context,
        // and the slots' CloseHandle only closes OUR handle — a presenter-side import that is
        // still parked keeps its own reference to the payload.
    }
}

/// One-time dump of the first decoded surface's layout — the forensics for a new GPU/driver.
fn log_layout_once(width: u32, height: u32, index: u32, pq: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if ONCE.swap(false, Ordering::Relaxed) {
        tracing::info!(width, height, slice = index, pq, "D3D11VA first frame");
    }
}
