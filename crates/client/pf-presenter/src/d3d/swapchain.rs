//! The DXGI flip-model swapchain on the SDL window's HWND: three buffers, flip-discard, a
//! frame-latency waitable object at latency 1, tearing where the factory offers it. The
//! buffers start BGRA8 sRGB; `set_format` rebuilds them in the `hdr_model` format a frame
//! takes, with its colour space and, for HDR10, the host's mastering metadata.

use super::{dxgi_error, OwnedHandle};
use crate::hdr_model::Format;
use crate::vk::PresentPref;
use anyhow::{Context as _, Result};
use punktfunk_core::quic::HdrMeta;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::d3d11::{ID3D11Device5, ID3D11DeviceContext4, ID3D11RenderTargetView};
use windows::Win32::dxgi::{
    IDXGIFactory2, IDXGIFactory5, IDXGISwapChain2, IDXGISwapChain3, IDXGISwapChain4,
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_HDR_METADATA_HDR10,
    DXGI_HDR_METADATA_TYPE_HDR10, DXGI_HDR_METADATA_TYPE_NONE, DXGI_MWA_NO_ALT_ENTER,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SAMPLE_DESC, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::windef::HWND;
use windows::Win32::winnt::HANDLE;

pub(super) struct Swapchain {
    chain: IDXGISwapChain2,
    /// The back buffer's view. Flip-model D3D11 renames buffer 0 to the current back buffer,
    /// so one view lives until the next rebuild.
    target: Option<ID3D11RenderTargetView>,
    extent: (u32, u32),
    format: Format,
    /// Creation flags; `ResizeBuffers` must pass the same ones.
    flags: u32,
    tearing: bool,
    waitable: OwnedHandle,
    /// Frame-latency signals the glass waiter took and `ready` has not.
    slots: Arc<AtomicU32>,
    /// A signal was taken and no present has spent it yet.
    slot: bool,
    sent: Option<Sent>,
    /// The last `Present` call's duration.
    present_us: u32,
}

/// A present for the glass clock: its count in the statistics, when its call returned, and
/// whether it tore.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sent {
    pub count: u32,
    pub submitted_ns: u64,
    pub tearing: bool,
}

impl Swapchain {
    pub(super) fn new(
        factory: &IDXGIFactory2,
        device: &ID3D11Device5,
        hwnd: HWND,
        extent: (u32, u32),
    ) -> Result<Swapchain> {
        let tearing = factory.cast::<IDXGIFactory5>().is_ok_and(|f| {
            let mut allowed = windows::core::BOOL(0);
            // SAFETY: a COM call on the live factory, writing one BOOL-sized local whose
            // size is the one passed.
            let hr = unsafe {
                f.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    std::ptr::from_mut(&mut allowed).cast(),
                    std::mem::size_of::<windows::core::BOOL>() as u32,
                )
            };
            hr.is_ok() && allowed.as_bool()
        });
        let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT as u32;
        if tearing {
            flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING as u32;
        }
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: extent.0.max(1),
            Height: extent.1.max(1),
            Format: dxgi(Format::Sdr8).0,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: flags,
            ..Default::default()
        };
        // SAFETY: COM calls on the live factory and device; `hwnd` is SDL's live window
        // handle and `desc` a local that outlives the call. The waitable object handle is
        // owned by the `OwnedHandle` built from it, which closes it once.
        let (chain, waitable) = unsafe {
            let chain = factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .context("create the DXGI swapchain")?;
            // Alt+Enter would take DXGI exclusive fullscreen; SDL owns fullscreen here.
            factory
                .MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER as u32)
                .ok()
                .context("MakeWindowAssociation")?;
            let chain: IDXGISwapChain2 = chain.cast().context("IDXGISwapChain2")?;
            chain
                .SetMaximumFrameLatency(1)
                .ok()
                .context("SetMaximumFrameLatency")?;
            let waitable = OwnedHandle(chain.GetFrameLatencyWaitableObject());
            (chain, waitable)
        };
        tracing::info!(
            width = desc.Width,
            height = desc.Height,
            tearing,
            "D3D11 swapchain"
        );
        Ok(Swapchain {
            chain,
            target: None,
            extent: (desc.Width, desc.Height),
            format: Format::Sdr8,
            flags,
            tearing,
            waitable,
            slots: Arc::default(),
            slot: false,
            sent: None,
            present_us: 0,
        })
    }

    pub(super) fn extent(&self) -> (u32, u32) {
        self.extent
    }

    /// Resize the buffers to `extent`, keeping their count, format and flags.
    pub(super) fn resize(
        &mut self,
        context: &ID3D11DeviceContext4,
        extent: (u32, u32),
    ) -> Result<()> {
        self.rebuild(context, extent, DXGI_FORMAT_UNKNOWN)?;
        self.extent = extent;
        Ok(())
    }

    /// `ResizeBuffers` on this chain, so the waitable object and the glass waiter stay. The
    /// back buffer's view goes first: DXGI refuses while anything references a buffer.
    fn rebuild(
        &mut self,
        context: &ID3D11DeviceContext4,
        extent: (u32, u32),
        format: DXGI_FORMAT,
    ) -> Result<()> {
        self.target = None;
        // SAFETY: COM calls on the live context and swapchain; no view of a back buffer is
        // bound or held once the targets are unbound and `target` is dropped above.
        unsafe {
            context.OMSetRenderTargets(None, None);
            context.Flush();
            self.chain
                .ResizeBuffers(0, extent.0, extent.1, format, self.flags)
                .ok()
                .map_err(|e| dxgi_error(e, "resize the DXGI swapchain"))
        }
    }

    pub(super) fn format(&self) -> Format {
        self.format
    }

    /// Rebuild the buffers in `format` with its colour space, keeping their size, count and
    /// flags; HDR10 takes `meta` too. `false`: the chain cannot present that colour space,
    /// and it is BGRA8 sRGB again.
    pub(super) fn set_format(
        &mut self,
        context: &ID3D11DeviceContext4,
        format: Format,
        meta: Option<&HdrMeta>,
    ) -> Result<bool> {
        let (buffers, space) = dxgi(format);
        self.rebuild(context, self.extent, buffers)?;
        let chain: IDXGISwapChain3 = self.chain.cast().context("IDXGISwapChain3")?;
        // SAFETY: a COM call on the live chain; the colour space is passed by value.
        let support = unsafe { chain.CheckColorSpaceSupport(space) }.unwrap_or(0);
        if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT as u32 == 0
            && format != Format::Sdr8
        {
            self.set_format(context, Format::Sdr8, None)?;
            return Ok(false);
        }
        // SAFETY: a COM call on the live chain; the colour space is passed by value.
        unsafe { chain.SetColorSpace1(space) }
            .ok()
            .map_err(|e| dxgi_error(e, "set the swapchain colour space"))?;
        self.format = format;
        self.set_metadata(meta.filter(|_| format == Format::Hdr10));
        tracing::info!(?format, "D3D11 swapchain format");
        Ok(true)
    }

    /// The host's HDR10 mastering metadata on the chain, or none. A refusal costs the
    /// metadata, not the picture.
    pub(super) fn set_metadata(&self, meta: Option<&HdrMeta>) {
        let Ok(chain) = self.chain.cast::<IDXGISwapChain4>() else {
            return;
        };
        let hdr10 = meta.map(hdr10_metadata);
        let applied = match &hdr10 {
            Some(m) => {
                // SAFETY: `m` is a live `repr(C)` struct of integers with no padding, borrowed
                // for the slice's whole life; the slice covers exactly its bytes.
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        std::ptr::from_ref(m).cast::<u8>(),
                        std::mem::size_of_val(m),
                    )
                };
                // SAFETY: a COM call on the live chain; DXGI reads `bytes` as the HDR10
                // struct its type names, during the call.
                unsafe { chain.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes)) }
            }
            // SAFETY: a COM call on the live chain with no metadata buffer.
            None => unsafe { chain.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_NONE, None) },
        };
        if let Err(e) = applied.ok() {
            tracing::warn!(error = %e, "HDR10 metadata not applied");
        }
    }

    /// Whether a present now would not block: the glass waiter took a frame-latency signal.
    /// A taken signal is kept until a present spends it.
    pub(super) fn ready(&mut self) -> bool {
        if !self.slot {
            let take = |n: u32| n.checked_sub(1);
            self.slot = self
                .slots
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, take)
                .is_ok();
        }
        self.slot
    }

    /// The count `ready` takes from, which the glass waiter fills.
    pub(super) fn slots(&self) -> Arc<AtomicU32> {
        self.slots.clone()
    }

    /// The frame-latency waitable object; the glass waiter waits a duplicate.
    pub(super) fn waitable(&self) -> HANDLE {
        self.waitable.0
    }

    /// The chain, for the glass waiter's statistics reads.
    pub(super) fn chain(&self) -> IDXGISwapChain2 {
        self.chain.clone()
    }

    /// The last present, once: what `note_presented` claims.
    pub(super) fn take_sent(&mut self) -> Option<Sent> {
        self.sent.take()
    }

    pub(super) fn present_us(&self) -> u32 {
        self.present_us
    }

    /// The back buffer's render-target view, made on first use after a create or resize.
    pub(super) fn target(&mut self, device: &ID3D11Device5) -> Result<ID3D11RenderTargetView> {
        if let Some(t) = &self.target {
            return Ok(t.clone());
        }
        let mut view = None;
        // SAFETY: COM calls on the live swapchain and device; the out-param is a local
        // `Option` checked below.
        unsafe {
            let buffer: windows::Win32::d3d11::ID3D11Texture2D = self
                .chain
                .GetBuffer(0)
                .map_err(|e| dxgi_error(e, "get the DXGI back buffer"))?;
            device
                .CreateRenderTargetView(&buffer, None, Some(&mut view))
                .ok()
                .map_err(|e| dxgi_error(e, "create the back buffer view"))?;
        }
        let view = view.context("CreateRenderTargetView returned no view")?;
        self.target = Some(view.clone());
        Ok(view)
    }

    /// Present with `mode`'s interval and flags, spending the slot `ready` took, and record
    /// it for the glass clock ([`Swapchain::take_sent`]).
    pub(super) fn present(&mut self, mode: Mode) -> Result<()> {
        self.slot = false;
        let start = Instant::now();
        // SAFETY: COM calls on the live swapchain; tearing flags are only passed when the
        // swapchain was created with `ALLOW_TEARING`.
        let count = unsafe {
            self.chain
                .Present(mode.interval, mode.flags)
                .ok()
                .map_err(|e| dxgi_error(e, "present the DXGI swapchain"))?;
            self.chain.GetLastPresentCount().unwrap_or(0)
        };
        self.present_us = u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX);
        self.sent = (count != 0).then(|| Sent {
            count,
            submitted_ns: pf_client_core::session::now_ns(),
            tearing: mode.tears(),
        });
        Ok(())
    }

    pub(super) fn tearing(&self) -> bool {
        self.tearing
    }
}

/// `format`'s DXGI buffer format and colour space.
fn dxgi(format: Format) -> (DXGI_FORMAT, DXGI_COLOR_SPACE_TYPE) {
    match format {
        Format::Sdr8 => (
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        ),
        Format::Sdr10 => (
            DXGI_FORMAT_R10G10B10A2_UNORM,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        ),
        Format::Hdr10 => (
            DXGI_FORMAT_R10G10B10A2_UNORM,
            DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
        ),
    }
}

/// `HdrMeta` (HDR10 SEI units, primaries G, B, R) as DXGI's HDR10 metadata: chromaticity
/// keeps its 1/50000 units, the mastering max goes to whole nits, the min stays in
/// 1/10000 nit, MaxCLL and MaxFALL stay in nits.
fn hdr10_metadata(m: &HdrMeta) -> DXGI_HDR_METADATA_HDR10 {
    let [g, b, r] = m.display_primaries;
    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: r,
        GreenPrimary: g,
        BluePrimary: b,
        WhitePoint: m.white_point,
        MaxMasteringLuminance: m.max_display_mastering_luminance / 10_000,
        MinMasteringLuminance: m.min_display_mastering_luminance,
        MaxContentLightLevel: m.max_cll,
        MaxFrameAverageLightLevel: m.max_fall,
    }
}

/// A present's sync interval, flags and the name the stats line shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mode {
    pub interval: u32,
    pub flags: u32,
    pub name: &'static str,
}

impl Mode {
    /// The present tears: no vblank takes it.
    pub(super) fn tears(self) -> bool {
        self.flags & DXGI_PRESENT_ALLOW_TEARING != 0
    }
}

/// D12: `pinned` (the `present_mode` knob) picks the interval, `fifo` 1 and `immediate` or
/// `mailbox` 0; else V-Sync does. Interval 0 tears where the swapchain allows it and the
/// player allows variable refresh.
pub(super) fn mode(pref: PresentPref, pinned: Option<&str>, tearing: bool) -> Mode {
    let interval = match pinned {
        Some("fifo") => 1,
        Some("immediate" | "mailbox") => 0,
        _ => u32::from(pref.vsync),
    };
    match interval {
        1 => Mode {
            interval,
            flags: 0,
            name: "fifo",
        },
        _ if tearing && pref.allow_vrr => Mode {
            interval,
            flags: DXGI_PRESENT_ALLOW_TEARING,
            name: "tearing",
        },
        _ => Mode {
            interval,
            flags: 0,
            name: "immediate",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(vsync: bool, allow_vrr: bool) -> PresentPref {
        PresentPref {
            vsync,
            allow_vrr,
            ..Default::default()
        }
    }

    #[test]
    fn the_present_mode_follows_vsync_the_pin_and_tearing() {
        let rows: [(&str, PresentPref, Option<&str>, bool, &str); 5] = [
            ("vsync on is fifo", pref(true, true), None, true, "fifo"),
            (
                "vsync off tears where allowed",
                pref(false, true),
                None,
                true,
                "tearing",
            ),
            (
                "no tearing support is immediate",
                pref(false, true),
                None,
                false,
                "immediate",
            ),
            (
                "vrr refused is immediate",
                pref(false, false),
                None,
                true,
                "immediate",
            ),
            (
                "a fifo pin beats vsync off",
                pref(false, true),
                Some("fifo"),
                true,
                "fifo",
            ),
        ];
        for (name, p, pinned, tearing, want) in rows {
            assert_eq!(mode(p, pinned, tearing).name, want, "{name}");
        }
        assert_eq!(mode(pref(true, true), Some("mailbox"), true).interval, 0);
    }

    #[test]
    fn hdr10_metadata_names_the_primaries_and_converts_the_mastering_max_to_nits() {
        let host = HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        };
        let want = DXGI_HDR_METADATA_HDR10 {
            RedPrimary: [35400, 14600],
            GreenPrimary: [8500, 39850],
            BluePrimary: [6550, 2300],
            WhitePoint: [15635, 16450],
            MaxMasteringLuminance: 1000,
            MinMasteringLuminance: 50,
            MaxContentLightLevel: 1000,
            MaxFrameAverageLightLevel: 400,
        };
        assert_eq!(hdr10_metadata(&host), want);
        assert_eq!(std::mem::size_of::<DXGI_HDR_METADATA_HDR10>(), 28);
    }
}
