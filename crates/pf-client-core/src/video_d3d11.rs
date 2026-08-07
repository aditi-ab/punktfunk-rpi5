//! The D3D11 side of the DXVA rung (Windows): the decode DEVICE and the shareable
//! hand-off ring the decoded surfaces are converted into. Auto's first choice on
//! Intel/unknown vendors — Intel's Windows driver DOES advertise Vulkan Video (Arc drivers
//! since 2023 — don't trust the capability gate to keep Intel off it), but Vulkan decode
//! on it was field-broken (B580, 2026-07: strobing + ~7 ms decodes) where this path
//! streams clean; on NVIDIA/AMD it is the fallback rung below Vulkan Video, in `auto` and
//! via mid-session demotion.
//!
//! **What decodes into these surfaces is [`crate::video_d3d11_native`]** (M5: pf-dxvadec
//! plans driven into `ID3D11VideoDecoder`). This module held libavcodec's D3D11VA hwaccel
//! as well until M10 excised FFmpeg from the client; what is left is the half both rungs
//! always shared, and it is the field-proven half.
//!
//! Ported from the retired in-process WinUI presenter's decoder (`clients/windows/src/video.rs`)
//! with one structural change: that presenter sampled D3D11 textures directly, while ours draws
//! with Vulkan. Bridging rules, all learned the hard way there:
//!
//! * The decode POOL is not built here. libavcodec's rung let libavcodec derive it
//!   (`get_format` set no frames context) after a hand-built pool validated on NVIDIA was
//!   rejected by Intel at the first `SubmitDecoderBuffers` — and Intel is the GPU this
//!   backend exists for; the native rung declares its own pool in `video_d3d11_native`,
//!   against pf-dxvadec's pinned bind flags. Either way the decode surfaces carry no share
//!   flags, so they can't be imported into Vulkan directly — hence the ring below.
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
//! * An HDR (PQ/BT.2020) stream passes through when the presenter can take it (RGB10A2
//!   import + an HDR10 swapchain — [`crate::video::VulkanDecodeDevice::d3d11_hdr10`]): the
//!   video processor converts YCbCr G2084 → RGB G2084 into an RGB10A2 ring, colorspace
//!   only, no tone mapping. On an SDR-only path it tone-maps to sRGB instead (input
//!   `G2084_P2020`, output sRGB) — correct picture, no HDR presentation.
//!
//! The decode device is created on the **presenter's adapter** (matched by the Vulkan device's
//! LUID) so the shared textures never cross GPUs on a multi-adapter box.
//!
//! Device creation ([`create_device`]) and the video-processor ring ([`HandoffRing`]) are
//! `pub(crate)` because `video_d3d11_native` is their only consumer.

use crate::video::ColorDesc;
use anyhow::{anyhow, Context as _, Result};
use std::ptr;
use windows::core::Interface;
use windows::Win32::d3d11::{
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
use windows::Win32::d3dcommon::{D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::windef::RECT;
use windows::Win32::winnt::HANDLE;

/// Ring of shareable hand-off textures. Bounds how many decoded-but-unpresented frames can
/// exist without a slot being rewritten under an in-flight older frame: the pump's decoded
/// channel holds 2 and the presenter drains to newest with one frame in flight, so 3 are ever
/// outstanding — 6 leaves margin without meaningful VRAM cost.
const RING_SLOTS: usize = 6;

/// Keyed-mutex acquire budget (ms) on the DECODE side. The presenter holds a slot only for one
/// submit's GPU lifetime; multiple seconds means the render thread died — surface an error
/// (which demotes to software) instead of wedging the decode loop.
const ACQUIRE_TIMEOUT_MS: u32 = 2000;

/// One decoded frame, parked in a ring slot the presenter imports by NT handle. Plain POD —
/// the ring (and its handles) belong to the decoder and outlive every in-flight frame; the
/// presenter must NOT close the handle. Cross-API exclusion + visibility ride the slot's
/// keyed mutex (key 0 on both sides), not this struct.
pub struct D3d11Frame {
    pub width: u32,
    pub height: u32,
    /// What the ring slot actually CONTAINS after the video processor's conversion:
    /// sRGB BT.709 full-range RGB normally (a PQ stream was tone-mapped), or PQ BT.2020
    /// full-range RGB when the HDR pass-through ring is active (`rgb10`) — the presenter
    /// keys its SDR/HDR handling off this.
    pub color: ColorDesc,
    /// The ring slot's texture format: `false` = BGRA8, `true` = RGB10A2 (the HDR PQ
    /// pass-through flavor) — the presenter's Vulkan import must match it exactly.
    pub rgb10: bool,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor signal. See
    /// [`crate::video::DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// The ring slot's NT shared handle (`IDXGIResource1::CreateSharedHandle`), stable for the
    /// ring's lifetime. Raw `isize` so the frame crosses the pump→presenter channel.
    pub handle: isize,
    /// Ring generation — bumped when the ring is rebuilt (stream size change), so a
    /// presenter-side import cache could never alias a stale handle. Informational today
    /// (the presenter imports per frame).
    pub generation: u32,
}

// ⚠ This struct carried a `native: bool` until M10, because TWO rungs filled the ring —
// libavcodec's D3D11VA hwaccel and `video_d3d11_native` — and both delivered
// `DecodedImage::D3d11`, so the `stats:` decode-path tag read `d3d11va` for either and no
// soak log could tell them apart (fixed in `1573a987`). With the libavcodec rung deleted
// there is one filler, the tag is unconditionally `native-d3d11va`, and a permanently-true
// flag would be a claim nothing can falsify. If a second rung ever shares this ring again,
// the flag has to come back WITH it — the lesson is that the tag must name the rung that
// actually wrote the pixels, not the family it belongs to.

/// Create the decode device on the presenter's adapter. `luid` is the Vulkan device's
/// `VkPhysicalDeviceIDProperties::deviceLUID` (little-endian LowPart‖HighPart) — matching it
/// keeps the shared textures on one GPU. `None`/no match falls back to the first hardware
/// adapter (single-GPU boxes; a WARP-only box fails out to software decode).
pub(crate) fn create_device(luid: Option<[u8; 8]>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: DXGI factory creation takes no pointer and returns an owned factory or an error,
    // checked by `?`.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut chosen: Option<IDXGIAdapter1> = None;
    let mut fallback: Option<IDXGIAdapter1> = None;
    for i in 0.. {
        // SAFETY: a COM call on the live factory; the `Ok` binding is what proves an adapter came
        // back.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: `DXGI_ADAPTER_DESC1` is plain-old-data, so all-zeroes is a valid value.
        let mut desc: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
        // SAFETY: a COM call on the adapter just enumerated, filling the zeroed local descriptor
        // through the out-param; checked before the descriptor is read.
        if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
            continue;
        }
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE as u32 != 0 {
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
    // SAFETY: `adapter` is the live adapter chosen above; the two out-params are local `Option`s
    // the callee only writes, and both are checked before use.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            windows::Win32::d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::minwindef::HINSTANCE(std::ptr::null_mut()),
            (D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT) as u32,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION as u32,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ok()
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
    // The decode video context and our copy (immediate context) run on the decode thread,
    // and D3D11's own driver threads touch the device too — the same protection the legacy
    // shared device enabled, and the same one libavcodec's hwdevice init used to install
    // for us. Explicit keeps the invariant obvious now that nothing else sets it.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // Returns the PREVIOUS protection state — nothing to act on.
        // SAFETY: a COM call on the live `ID3D11Multithread` from a checked `cast`; it takes a
        // BOOL and returns the previous state.
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
        // SAFETY: `self.handle` is the shared-texture NT handle this slot owns; `Drop` runs once,
        // so it is closed exactly once and never used after.
        unsafe {
            let _ = windows::Win32::handleapi::CloseHandle(self.handle);
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
    /// HDR flavor: RGB10A2 slots the processor fills with PQ BT.2020 RGB (colorspace
    /// conversion only — both sides G2084, no tone mapping). `false` = BGRA8 sRGB.
    pq_out: bool,
}

impl SharedRing {
    fn build(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        width: u32,
        height: u32,
        generation: u32,
        pq_out: bool,
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
        // SAFETY: COM calls on the live video device, with a borrowed local descriptor and a
        // checked out-param.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("CreateVideoProcessorEnumerator")?;
        // SAFETY: same live device, borrowed enumerator from the line above.
        let vp = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            // Single-plane RGB: the ONLY hand-off family whose Vulkan import is a
            // universally exercised driver path (see the module docs — NV12 import TDRs
            // on NVIDIA despite being advertised). RGB10A2 for the HDR pass-through
            // flavor (gated on the presenter's probe), BGRA8 otherwise.
            Format: if pq_out {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // RENDER_TARGET: the video processor's output view renders into it.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX)
                as u32,
        };
        let mut slots = Vec::with_capacity(RING_SLOTS);
        for _ in 0..RING_SLOTS {
            let mut tex = None;
            // SAFETY: a `?`-checked `CreateTexture2D` on the live device, over a fully-initialized
            // stack descriptor and a live `Option` out-param.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .ok()
                .context("create shared hand-off texture")?;
            let tex: ID3D11Texture2D = tex.expect("CreateTexture2D succeeded");
            let mutex: IDXGIKeyedMutex =
                tex.cast().context("shared texture lacks IDXGIKeyedMutex")?;
            let resource: IDXGIResource1 =
                tex.cast().context("shared texture lacks IDXGIResource1")?;
            // SAFETY: the shared-handle creation runs on the live texture just created; the
            // returned NT handle is owned by the `Slot` built below, which closes it in `Drop`.
            let handle = unsafe {
                resource.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE as u32,
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
            // SAFETY: COM calls on the live video device with borrowed local descriptors and a
            // checked out-param.
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &ov_desc,
                    Some(&mut out_view),
                )
            }
            .ok()
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
            hdr = pq_out,
            "D3D11 shared hand-off ring built (VideoProcessor → RGB)"
        );
        Ok(SharedRing {
            slots,
            vp,
            enumerator,
            width,
            height,
            next: 0,
            generation,
            pq_out,
        })
    }
}

/// One decoded picture, as [`HandoffRing::present`] needs to see it.
///
/// A struct rather than seven positional parameters because six of them are integers and
/// booleans: a caller that swaps `width` and `height`, or `array_slice` and a dimension,
/// compiles clean and renders a wrong picture. Named fields make each of those a build error.
pub(crate) struct HandoffSource<'a> {
    /// The decode pool's texture ARRAY — the pool `video_d3d11_native` created.
    pub texture: &'a ID3D11Texture2D,
    /// The picture's slice within that array — the decoder's DPB slot, which IS the DXVA
    /// surface index.
    pub array_slice: u32,
    /// The FRAME size. The surface is taller (DXVA alignment), which is exactly what the
    /// stream source rect excludes — see the blit below.
    pub width: u32,
    pub height: u32,
    /// The picture's colour signalling, per frame and never latched (the host flips PQ
    /// in-band with a new SPS).
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor signal.
    pub keyframe: bool,
    /// Which decoder produced it, for the one-time layout log a field report leans on.
    pub decoder: &'a str,
}

/// The shipping hand-off: the video processor, its ring of shareable RGBA textures, and the
/// D3D11 objects they live on. Everything from "here is a decoded NV12/P010 surface" to "here
/// is a [`D3d11Frame`] the presenter can import".
///
/// Extracted verbatim from `D3d11vaDecoder`, the libavcodec D3D11VA rung that owned this ring
/// until M10 deleted it, so the M5 native rung ([`crate::video_d3d11_native`]) filled the
/// identical ring rather than growing a second copy of it: this is the half with the field
/// history (the NVIDIA NV12-import TDR, the Intel green bar, the keyed-mutex protocol), and two
/// copies of it would have been two chances to lose that history. Nothing about the hand-off
/// changed in the extraction; only its owner did, and today that owner is the sole one.
pub(crate) struct HandoffRing {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Creates the per-ring video processor + views.
    video_device: ID3D11VideoDevice,
    /// Runs the per-frame `VideoProcessorBlt`; the `1` interface for the DXGI colour-space
    /// setters (Win10 1703+, universally present — init fails to software without it).
    video_context1: ID3D11VideoContext1,
    ring: Option<SharedRing>,
    /// The presenter can import RGB10A2 AND offers an HDR10 swapchain
    /// ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`]) — PQ streams get the HDR
    /// pass-through ring; without it they keep the tonemap-to-sRGB ring.
    hdr10_out: bool,
}

impl HandoffRing {
    /// Take the interfaces the hand-off needs off a decode device, up front — their absence
    /// must route the session to another rung NOW, not burn the opening IDR.
    pub(crate) fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        hdr10_out: bool,
    ) -> Result<HandoffRing> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
        let video_context1: ID3D11VideoContext1 = context
            .cast()
            .context("context lacks ID3D11VideoContext1 (pre-1703 Windows?)")?;
        Ok(HandoffRing {
            device,
            context,
            video_device,
            video_context1,
            ring: None,
            hdr10_out,
        })
    }

    /// The video device, for a caller that also needs it (the native rung enumerates decode
    /// profiles and creates its decoder through the same interface).
    pub(crate) fn video_device(&self) -> &ID3D11VideoDevice {
        &self.video_device
    }

    /// Convert one decoded surface into the next ring slot (`VideoProcessorBlt`, NV12/P010 →
    /// BGRA8/RGB10A2) under its keyed mutex and describe the hand-off. The mutex acquire also
    /// back-pressures against the presenter still reading this slot (only possible if the
    /// stream runs `RING_SLOTS` ahead of present).
    ///
    pub(crate) fn present(&mut self, source: HandoffSource<'_>) -> Result<D3d11Frame> {
        let HandoffSource {
            texture: src,
            array_slice,
            width,
            height,
            color,
            keyframe,
            decoder,
        } = source;
        // AddRef'd locals so the mutable `ring` borrow below doesn't lock all of `self`.
        let video_device = self.video_device.clone();
        let video_context1 = self.video_context1.clone();
        let context = self.context.clone();
        // (Re)build the ring + video processor on first use, a stream size change, or a
        // flavor change (the host flips PQ in-band; SDR↔HDR swaps the slot format, so
        // it rebuilds like a resize — bit DEPTH alone still never rebuilds: an SDR
        // 10-bit stream and an 8-bit one share the same output flavor).
        let pq_out = self.hdr10_out && color.is_pq();
        let rebuild = self
            .ring
            .as_ref()
            .is_none_or(|r| r.width != width || r.height != height || r.pq_out != pq_out);
        if rebuild {
            let generation = self.ring.as_ref().map_or(0, |r| r.generation + 1);
            self.ring = Some(SharedRing::build(
                &self.device,
                &video_device,
                width,
                height,
                generation,
                pq_out,
            )?);
        }
        let ring = self.ring.as_mut().expect("ring built above");
        let slot_idx = ring.next;
        ring.next = (ring.next + 1) % ring.slots.len();
        let slot = &ring.slots[slot_idx];

        // SAFETY: every call below is a COM call on a live interface — the video device and
        // context AddRef'd above, the ring's processor/enumerator/views built by
        // `SharedRing::build`, and the caller's `src` texture, whose liveness for the call is
        // this method's contract. Out-params are local `Option`s checked before use; the
        // `ManuallyDrop` refs the stream struct carries are balanced explicitly below.
        unsafe {
            // Input view over THIS slice of the decode array (cheap per-frame object).
            let mut iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0, // surface format speaks for itself
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                // Anonymous.Texture2D zeroed (MipSlice 0); ArraySlice is per-frame below.
                ..Default::default()
            };
            iv_desc.Anonymous.Texture2D.ArraySlice = array_slice;
            let mut in_view = None;
            video_device
                .CreateVideoProcessorInputView(src, &ring.enumerator, &iv_desc, Some(&mut in_view))
                .ok()
                .context("CreateVideoProcessorInputView")?;
            let in_view = in_view.expect("input view created");

            // Colour spaces per frame (the host flips PQ in-band): YCbCr in, sRGB out — a PQ
            // stream is tone-mapped to SDR by the processor (module docs). CICP → DXGI enums.
            // BT.601 (5/6) matters in practice: a Linux host's RGB-input NVENC paths signal
            // BT470BG limited (NVENC's fixed internal RGB→YUV is BT.601 — ffmpeg force-writes
            // that VUI), and mapping it to P709 here was a constant hue error on those streams.
            // DXGI has no full-range G2084 YCbCr enum, so PQ is studio regardless of range.
            let in_cs = match (color.transfer, color.matrix, color.full_range) {
                (16, _, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
                (_, 9 | 10, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
                (_, 9 | 10, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
                (_, 5 | 6, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                (_, 5 | 6, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
                (_, _, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            };
            // The DECODE surface is DXVA-aligned (height rounded up to the profile's
            // macroblock/tile alignment — 128 for HEVC/AV1), so it is TALLER than the
            // frame: a 2400-line stream decodes into a 2432-line texture. Without an
            // explicit source rect the processor blits the WHOLE surface — the padding
            // rows (uninitialized NV12: Y=0,U=V=0, which converts to vivid green) land at
            // the bottom of the output and the picture is squashed to fit. Clamp the
            // source to the real frame; the dest stays the whole (frame-sized) slot.
            // Live-hit on Intel 3840x2400 as a ~32 px green bar (2026-07-19).
            video_context1.VideoProcessorSetStreamSourceRect(
                &ring.vp,
                0,
                true,
                Some(&RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }),
            );
            video_context1.VideoProcessorSetStreamColorSpace1(&ring.vp, 0, in_cs);
            video_context1.VideoProcessorSetOutputColorSpace1(
                &ring.vp,
                // HDR ring: PQ in, PQ out — a pure colorspace conversion (YCbCr→RGB),
                // no tone mapping; the presenter passes the values through to its HDR10
                // swapchain. SDR ring: sRGB out (a PQ stream is tone-mapped here).
                if ring.pq_out {
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                } else {
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
                },
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

            let mut src_desc = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut src_desc);
            log_layout_once(
                width,
                height,
                src_desc.Width,
                src_desc.Height,
                array_slice,
                color.is_pq(),
                decoder,
            );
            Ok(D3d11Frame {
                width,
                height,
                // What the slot now CONTAINS. HDR ring: PQ BT.2020 full-range RGB (the
                // presenter reads is_pq() and flips its HDR10 swapchain). SDR ring: sRGB
                // BT.709 full-range RGB (PQ was tone-mapped above).
                color: if ring.pq_out {
                    ColorDesc {
                        primaries: 9,
                        transfer: 16, // PQ / SMPTE ST.2084
                        matrix: 0,    // identity — RGB
                        full_range: true,
                    }
                } else {
                    ColorDesc {
                        primaries: 1,
                        transfer: 13, // sRGB (H.273)
                        matrix: 0,    // identity — RGB
                        full_range: true,
                    }
                },
                rgb10: ring.pq_out,
                keyframe,
                handle,
                generation,
            })
        }
    }
}

/// One-time dump of the first decoded surface's layout — the forensics for a new GPU/driver.
/// `tex_*` is the DXVA-aligned decode surface (>= the frame); the gap is the padding the
/// stream source rect excludes.
///
/// Keyed by DECODER rather than latched once per process. Two rungs shared this hand-off
/// until M10 (libavcodec's D3D11VA and the native one), and a single process-wide latch
/// meant a session that pinned the native rung and then demoted logged the native layout
/// and nothing else, leaving the rung that actually painted the session's frames
/// undocumented in exactly the report that needs it. One rung fills the ring today, so the
/// set holds one short entry — kept keyed because the property is about which decoder
/// wrote the surface, and that is the question a new-GPU forensics report asks.
fn log_layout_once(
    width: u32,
    height: u32,
    tex_w: u32,
    tex_h: u32,
    index: u32,
    pq: bool,
    decoder: &str,
) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    // A poisoned lock costs a log line, never a frame: a panic while holding it can only have
    // happened inside the set, and the worst outcome of ignoring it is a repeated line.
    let first = match seen.lock() {
        Ok(mut seen) => seen.insert(decoder.to_owned()),
        Err(_) => false,
    };
    if first {
        tracing::info!(
            width,
            height,
            tex_w,
            tex_h,
            slice = index,
            pq,
            decoder,
            "D3D11VA first frame"
        );
    }
}

/// This desktop's HDR colour volume (`IDXGIOutput6::GetDesc1`) → the Hello's
/// `display_hdr`, so the host's virtual-display EDID matches THIS panel instead of its
/// generic defaults (host apps then tone-map to the real glass). `pos` picks the output
/// containing that desktop point — the `--window-pos` monitor, where the stream window
/// will open; no `pos` or no match falls back to the output holding the desktop origin
/// (the primary). Returns `None` when that output's advanced color is off (an SDR
/// colorspace): claiming an HDR volume for a desktop that won't present HDR would steer
/// host tone mapping wrong, and the host's EDID defaults are the honest answer there.
/// (`PUNKTFUNK_CLIENT_PEAK_NITS` still overrides whatever this reports — see
/// `punktfunk_core::client::display_hdr_env_override`.)
pub fn display_hdr_volume(pos: Option<(i32, i32)>) -> Option<punktfunk_core::quic::HdrMeta> {
    use windows::Win32::dxgi::{IDXGIOutput6, DXGI_OUTPUT_DESC1};
    // SAFETY: plain DXGI factory creation — no arguments to get wrong; the returned
    // interface is owned by this scope and dropped with it.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut fallback: Option<DXGI_OUTPUT_DESC1> = None;
    for a in 0.. {
        // SAFETY: read-only enumeration on the live factory; the returned adapter is
        // owned by this scope.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            // Out-pointer convention in this windows-rs rev (no retval annotation).
            let mut output: Option<windows::Win32::dxgi::IDXGIOutput> = None;
            // SAFETY: read-only enumeration on the live adapter, writing a local
            // out-pointer that outlives the call.
            if unsafe { adapter.EnumOutputs(o, &mut output) }.ok().is_err() {
                break;
            }
            let Some(output) = output else {
                break;
            };
            let Ok(out6) = output.cast::<IDXGIOutput6>() else {
                continue; // pre-1809 DXGI — no advanced-color facts to read
            };
            let mut desc = DXGI_OUTPUT_DESC1::default();
            // SAFETY: fills a local, correctly-sized DXGI_OUTPUT_DESC1 that outlives
            // the call; the interface is live (owned just above).
            if unsafe { out6.GetDesc1(&mut desc) }.ok().is_err() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let contains =
                |x: i32, y: i32| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
            if let Some((x, y)) = pos {
                if contains(x, y) {
                    return hdr_meta_from_output(&desc);
                }
            }
            if fallback.is_none() || contains(0, 0) {
                fallback = Some(desc);
            }
        }
    }
    hdr_meta_from_output(&fallback?)
}

/// The ST.2086 shape of one output's colour facts; `None` for an SDR colorspace.
fn hdr_meta_from_output(
    d: &windows::Win32::dxgi::DXGI_OUTPUT_DESC1,
) -> Option<punktfunk_core::quic::HdrMeta> {
    use windows::Win32::dxgi::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    if d.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
        return None;
    }
    // Chromaticity → 1/50000 units; luminance → 0.0001 cd/m² units (the HdrMeta contract).
    let c = |v: [f32; 2]| {
        [
            (v[0] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
            (v[1] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
        ]
    };
    Some(punktfunk_core::quic::HdrMeta {
        // ST.2086 primary order is G, B, R (see the HdrMeta docs); DXGI reports R/G/B.
        display_primaries: [c(d.GreenPrimary), c(d.BluePrimary), c(d.RedPrimary)],
        white_point: c(d.WhitePoint),
        max_display_mastering_luminance: (f64::from(d.MaxLuminance) * 10_000.0) as u32,
        min_display_mastering_luminance: (f64::from(d.MinLuminance) * 10_000.0) as u32,
        max_cll: d.MaxLuminance.round().clamp(0.0, 65_535.0) as u16,
        max_fall: d.MaxFullFrameLuminance.round().clamp(0.0, 65_535.0) as u16,
    })
}
