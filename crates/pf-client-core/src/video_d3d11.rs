//! D3D11 decode device and shareable hand-off ring for the Windows DXVA rung.
//! [`crate::video_d3d11_native`] fills the decode surfaces; this module converts
//! each slice through `ID3D11VideoProcessor` into RGBA textures the presenter
//! imports (`pf-presenter/src/d3d11.rs`, `VK_KHR_external_memory_win32`).
//!
//! Auto's first choice on Intel — the driver advertises Vulkan Video, but that
//! decode path is not the shipping one. NVIDIA/AMD fall back here below Vulkan
//! Video, including mid-session demotion.
//!
//! Decode surfaces carry no share flags. Ring slots are single-plane RGBA
//! (`SHARED_NTHANDLE | SHARED_KEYEDMUTEX`): a multiplanar NV12 import TDRs on
//! NVIDIA. Both sides acquire the keyed mutex with **key 0**; a dropped frame
//! is never acquired, which a ping-pong key would deadlock on. The device is
//! created on the presenter's adapter (Vulkan LUID) so shares stay on one GPU.
//!
//! PQ streams pass through as RGB10A2 when the presenter has an HDR10 swapchain
//! ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`]); otherwise the processor
//! tone-maps to sRGB. [`create_device`] and [`HandoffRing`] are `pub(crate)`
//! for [`crate::video_d3d11_native`].

use crate::video::ColorDesc;
use anyhow::{anyhow, Context as _, Result};
use std::ptr;
use windows::core::Interface;
use windows::Win32::d3d11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorEnumerator1, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::d3dcommon::{D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_P010, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::windef::RECT;
use windows::Win32::winnt::HANDLE;

/// Six slots: the pump holds 2 decoded frames and the presenter has one in flight, so 3 are
/// outstanding. Double that leaves margin without meaningful VRAM cost.
const RING_SLOTS: usize = 6;

/// Decode-side keyed-mutex acquire budget, milliseconds. The presenter holds a slot for one
/// submit; a multi-second wait means the render thread died — error (and demote) rather than
/// wedge the decode loop.
const ACQUIRE_TIMEOUT_MS: u32 = 2000;

/// One decoded frame in a ring slot the presenter imports by NT handle. The ring owns the
/// handle for its lifetime; the presenter must not close it. Exclusion and visibility ride
/// the slot's keyed mutex (key 0), not this struct.
pub struct D3d11Frame {
    pub width: u32,
    pub height: u32,
    /// Colour after the video processor: sRGB BT.709 full-range, or PQ BT.2020 full-range
    /// when the HDR pass-through ring is active (`rgb10`). The presenter keys SDR/HDR off this.
    pub color: ColorDesc,
    /// Slot format: `false` = BGRA8, `true` = RGB10A2. The presenter's Vulkan import must match.
    pub rgb10: bool,
    /// Intra (IDR/I) — the pump's post-loss re-anchor. See [`crate::video::DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// Slot NT handle (`CreateSharedHandle`), stable for the ring's lifetime. Raw `isize` so
    /// the frame can cross the pump→presenter channel.
    pub handle: isize,
    /// Bumped when the ring is rebuilt (size or flavour change) so an import cache cannot
    /// alias a stale handle.
    pub generation: u32,
}

/// Decode device on the presenter's adapter. `luid` is the Vulkan `deviceLUID`
/// (little-endian LowPart‖HighPart); a match keeps shares on one GPU. `None` or no match
/// uses the first hardware adapter. A WARP-only box fails out.
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
            continue; // WARP cannot hardware-decode
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
    // Decode and driver threads both touch this device; without multithread protection
    // those concurrent COM calls race.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: a COM call on the live `ID3D11Multithread` from a checked `cast`; it takes a
        // BOOL and returns the previous protection state, which we ignore.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

/// Whether this adapter's video processor can convert a PQ decode surface to sRGB —
/// the tonemap [`HandoffRing::present`] uses when the presenter has no HDR10 swapchain
/// ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`] false).
///
/// Setting the colorspaces is not a negotiation: `VideoProcessorSetStream/OutputColorSpace1`
/// accept anything and `VideoProcessorBlt` succeeds either way. A driver that cannot
/// convert renders garbage. Probe the SDR-ring pair: P010 `YCBCR_STUDIO_G2084_LEFT_P2020`
/// in, BGRA8 `RGB_FULL_G22_NONE_P709` out. Only a definitive "no" answers `false`; an
/// API failure answers `true` — a box whose D3D11 fails the probe fails D3D11VA
/// construction too. Paid once per connect; [`crate::video::hdr_presentable`] skips it
/// elsewhere.
pub(crate) fn pq_tonemap_supported(luid: Option<[u8; 8]>) -> bool {
    fn probe(luid: Option<[u8; 8]>) -> Result<bool> {
        let (device, _context) = create_device(luid)?;
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
        // The enumerator wants a content shape; conversion support is a format/colorspace
        // fact, so any plausible size asks the same question.
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: 1920,
            InputHeight: 1080,
            OutputFrameRate: rate,
            OutputWidth: 1920,
            OutputHeight: 1080,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: COM calls on the live device/enumerator just created, over a borrowed
        // fully-initialized stack descriptor; the conversion query fills a BOOL by value.
        unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .context("CreateVideoProcessorEnumerator")?;
            let enumerator1: ID3D11VideoProcessorEnumerator1 = enumerator
                .cast()
                .context("enumerator lacks ID3D11VideoProcessorEnumerator1 (pre-Win10?)")?;
            let ok = enumerator1
                .CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_P010,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                )
                .context("CheckVideoProcessorFormatConversion")?;
            Ok(ok.as_bool())
        }
    }
    match probe(luid) {
        Ok(supported) => {
            if !supported {
                tracing::warn!(
                    "video processor reports NO P010 PQ→sRGB conversion — a PQ stream on the \
                     D3D11VA rung would render garbage (green) instead of tone-mapping"
                );
            }
            supported
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"),
                "PQ tonemap probe failed — assuming supported");
            true
        }
    }
}

/// Shareable RGBA slot. The NT handle is closed on drop; the presenter never owns it.
struct Slot {
    /// Shared texture the views below point at; kept so they stay valid.
    _tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: HANDLE,
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

/// Video processor plus shareable slots, both sized to the stream. A mid-stream
/// `Reconfigure` rebuilds the whole bundle.
struct SharedRing {
    slots: Vec<Slot>,
    vp: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
    next: usize,
    generation: u32,
    /// `true` = RGB10A2 PQ BT.2020 (colorspace only, both sides G2084). `false` = BGRA8 sRGB.
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
        // 1:1, no scaling — Vulkan scales at composite. Frame rates are advisory.
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
            // Single-plane RGB: NV12 D3D11→Vulkan import TDRs on NVIDIA. RGB10A2 for
            // HDR pass-through, BGRA8 otherwise.
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

/// One decoded picture for [`HandoffRing::present`]. Named fields so a swapped
/// `width`/`height` or `array_slice` is a build error, not a wrong picture.
pub(crate) struct HandoffSource<'a> {
    /// Decode-pool texture array from `video_d3d11_native`.
    pub texture: &'a ID3D11Texture2D,
    /// Slice in that array: the decoder's DPB slot, which is the DXVA surface index.
    pub array_slice: u32,
    /// Frame size, not the DXVA-aligned surface. The blit source rect uses this so
    /// padding rows stay out of the picture.
    pub width: u32,
    pub height: u32,
    /// Per-frame colour signalling; never latched — the host flips PQ in-band with a new SPS.
    pub color: ColorDesc,
    /// Intra (IDR/I) — the pump's post-loss re-anchor.
    pub keyframe: bool,
    /// Decoder name for [`log_layout_once`]; the key includes it so a demotion logs the new rung.
    pub decoder: &'a str,
}

/// Video processor, shareable RGBA ring, and the D3D11 objects they live on.
/// Turns a decoded NV12/P010 surface into a [`D3d11Frame`] the presenter can import.
pub(crate) struct HandoffRing {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    /// `1` for the DXGI colour-space setters (Win10 1703+). Init fails to software without it.
    video_context1: ID3D11VideoContext1,
    ring: Option<SharedRing>,
    /// Presenter can import RGB10A2 and has an HDR10 swapchain
    /// ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`]). PQ then uses the pass-through ring.
    hdr10_out: bool,
}

impl HandoffRing {
    /// Bind the video interfaces now. Missing them must fail the rung before the opening IDR.
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

    /// Same `ID3D11VideoDevice` the native rung enumerates decode profiles on.
    pub(crate) fn video_device(&self) -> &ID3D11VideoDevice {
        &self.video_device
    }

    /// Blit one decoded surface into the next ring slot under its keyed mutex.
    /// The acquire also back-pressures if the presenter is still reading this slot
    /// (only possible `RING_SLOTS` frames ahead of present).
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
        // Rebuild on first use, size change, or SDR↔HDR flavour change (PQ flips in-band
        // and swaps the slot format). Bit depth alone does not: SDR 10-bit and 8-bit
        // share the same output flavour.
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
            // Per-frame view over this DPB slice.
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

            // Per-frame CICP → DXGI (host flips PQ in-band). Matrix 5/6 is BT.601; mapping
            // it to P709 is a hue error (NVENC's RGB→YUV is BT.601). DXGI has no full-range
            // G2084 YCbCr enum, so PQ is studio regardless of range.
            let in_cs = match (color.transfer, color.matrix, color.full_range) {
                (16, _, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
                (_, 9 | 10, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
                (_, 9 | 10, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
                (_, 5 | 6, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
                (_, 5 | 6, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
                (_, _, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
                _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            };
            // DXVA-aligned surfaces are taller than the frame (HEVC/AV1 round to 128).
            // Without a source rect the processor blits the padding too: uninit NV12
            // (Y=0,U=V=0) converts to green at the bottom and the picture is squashed.
            // Clamp to the real frame; dest stays the (frame-sized) slot.
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
                // HDR ring: PQ in, PQ out (YCbCr→RGB, no tone map). SDR ring: sRGB out
                // (PQ is tone-mapped here).
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
            // Flush now: the presenter's GPU acquire waits on this blit; an unflushed
            // deferred batch adds a driver-decided delay.
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
                // Slot contents after the blit, not the source signalling.
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

/// One-time layout log of a decoded surface. `tex_*` is the DXVA-aligned pool
/// (>= the frame); the gap is the padding the source rect excludes.
///
/// Keyed by decoder × (frame, pool, PQ) so a demotion or mid-stream `Reconfigure`
/// logs the new shape. `slice` stays out of the key: it varies per frame and
/// would log once per DPB slot.
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
    type LayoutKey = (String, u32, u32, u32, u32, bool);
    static SEEN: OnceLock<Mutex<HashSet<LayoutKey>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    // Poison costs a log line, never a frame: ignore it and the worst case is a repeat.
    let first = match seen.lock() {
        Ok(mut seen) => seen.insert((decoder.to_owned(), width, height, tex_w, tex_h, pq)),
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

/// This desktop's HDR volume (`IDXGIOutput6::GetDesc1`) for Hello `display_hdr`, so
/// the host EDID matches this panel. `pos` selects the output containing that point
/// (`--window-pos`); no `pos` or no match uses the output at the desktop origin.
/// `None` when advanced color is off — claiming HDR for an SDR desktop would steer
/// host tone-mapping wrong. `PUNKTFUNK_CLIENT_PEAK_NITS` still overrides; see
/// `punktfunk_core::client::display_hdr_env_override`.
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
