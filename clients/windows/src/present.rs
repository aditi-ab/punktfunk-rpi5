//! Direct3D11 presenter for a WinUI 3 `SwapChainPanel`. It draws a decoded frame Contain-fit into a
//! **composition** flip-model swapchain, which the reactor stream page binds to the panel via
//! `SwapChainPanelHandle::set_swap_chain`. After that one UI-thread bind, the presenter lives on
//! the dedicated render thread ([`crate::render`]) — presenting never touches (or is stalled by)
//! the XAML thread.
//!
//! Two frame sources, ONE Y′CbCr→RGB shader whose conversion rows arrive per frame in a constant
//! buffer (`pf_client_core::video::csc_rows` from the frame's CICP signaling — identical colour
//! math for both sources, and the stream's signaled matrix/range is honored, not assumed):
//!
//! * **GPU (D3D11VA)** — [`crate::video::GpuFrame`] is a slice of the decoder-only NV12/P010
//!   texture array. One `CopySubresourceRegion` with a display-size box moves the slice — **both
//!   planes; in D3D11 a planar slice is a single subresource** (unlike D3D12) — into our
//!   sampleable texture, which per-plane SRVs (R8/R8G8, R16/R16G16) expose to the shaders. The
//!   source box is mandatory: the decode array is coded-size (e.g. 1920×1088), the target
//!   display-size (1920×1080), and D3D11 silently drops size-mismatched full-resource copies.
//! * **CPU upload** — [`crate::video::CpuFrame`] carries NV12/P010 planes from the software
//!   decoder; they upload into two dynamic plane textures feeding the same SRV slots/shaders.
//!
//! **Pacing**: the swapchain is created with `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`
//! and `SetMaximumFrameLatency(1)` (flagless fallback for odd drivers). The render thread waits
//! on the latency waitable before drawing, so at most one present is ever queued (minimum compose
//! latency) and a stream faster than the display drops frames *before* any GPU work. Every
//! `ResizeBuffers` must re-pass the creation flags — that's `swap_flags`.
//!
//! **HiDPI**: buffers are sized in physical pixels and `IDXGISwapChain2::SetMatrixTransform`
//! (scale 96/DPI) maps them to the panel's DIP coordinate space — without it XAML samples a
//! DIP-sized buffer up and the video is blurry at 125/150 % scaling.
//!
//! **HDR10**: when a frame is BT.2020 PQ the swapchain flips to `R10G10B10A2` +
//! `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (+ HDR10 metadata) via `ResizeBuffers`/
//! `SetColorSpace1`; the shader output is already PQ-encoded so the compositor maps PQ→display. SDR
//! stays 8-bit B8G8R8A8.
//!
//! All `windows` types here come from the same windows-rs commit as `windows-reactor`, so the
//! `IDXGISwapChain1` handed to `set_swap_chain` satisfies reactor's `windows_core::Interface`.

use crate::video::{CpuFrame, DecodedFrame, GpuFrame};
use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObject;

// One vertex shader (fullscreen triangle) + ONE pixel shader for every colour combination:
// tex0 is the luma plane, tex1 the chroma plane, and the Y′CbCr→RGB conversion arrives as three
// constant-buffer rows precomputed on the CPU per frame (`pf_client_core::video::csc_rows` —
// bit-depth exact, range expansion + the P010 ×65535/65472 high-bit repack folded in). One shader
// honors whatever the stream signals (BT.601/709/2020, full/limited, 8/10-bit) instead of the old
// two hardcoded matrices — a BT.601-signaled stream (a Linux host's RGB-input NVENC) used to
// render with BT.709 coefficients, a constant hue error. A PQ stream's rows yield PQ-encoded
// R′G′B′ passed through as-is to the HDR10 swapchain, exactly as before.
const SHADER_HLSL: &str = r#"
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VSOut o;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}
Texture2D tex0 : register(t0);
Texture2D tex1 : register(t1);
SamplerState smp : register(s0);
cbuffer Csc : register(b0) {
    float4 r0;   // rgb[i] = dot(ri.xyz, yuv) + ri.w
    float4 r1;
    float4 r2;
};

float4 ps_yuv(VSOut i) : SV_Target {
    // 4:2:0 chroma is left-cosited (H.273 type 0 — the default inference when unsignaled, and
    // what the hosts produce), but sampling the half-res plane at the luma UV assumes CENTER
    // siting — a ~0.5-luma-px rightward chroma shift on hard colored edges. Offset +0.25 chroma
    // texels to re-align (the same correction the Apple client applies). Self-disables when the
    // plane widths match (a full-size 4:4:4 chroma plane has no subsampling to correct).
    float lw, lh, cw, ch;
    tex0.GetDimensions(lw, lh);
    tex1.GetDimensions(cw, ch);
    float2 cuv = i.uv;
    if (cw < lw) { cuv.x += 0.25 / cw; }
    float3 yuv = float3(tex0.Sample(smp, i.uv).r, tex1.Sample(smp, cuv).rg);
    float3 rgb = float3(dot(r0.xyz, yuv) + r0.w,
                        dot(r1.xyz, yuv) + r1.w,
                        dot(r2.xyz, yuv) + r2.w);
    return float4(saturate(rgb), 1.0);
}
"#;

/// The currently bound frame: per-plane SRVs (over the GPU sample texture or the CPU plane
/// textures). Redraws (resize, letterbox) re-present it — the CSC constant buffer still holds
/// this frame's rows, and the swapchain mode was latched by `set_hdr` when the frame arrived.
struct Bound {
    y: ID3D11ShaderResourceView,
    c: ID3D11ShaderResourceView,
}

pub struct Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_yuv: ID3D11PixelShader,
    /// Dynamic constant buffer holding the bound frame's three CSC rows (`csc_rows`), rewritten
    /// on every bind (colour signaling can flip in-band, e.g. the host's SDR→HDR re-init).
    csc_buf: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    swap: IDXGISwapChain1,
    /// Creation flags — MUST be re-passed to every `ResizeBuffers` or it fails.
    swap_flags: u32,
    /// The frame-latency waitable (owned; closed in `Drop`), `None` on the flagless fallback.
    waitable: Option<HANDLE>,
    rtv: Option<ID3D11RenderTargetView>,
    /// GPU path: sampleable copy target for the decoded slice — `(tex, w, h, ten_bit)`, recreated
    /// when the decoded size/bit depth changes. Format must equal the decode array's (NV12/P010).
    sample_tex: Option<(ID3D11Texture2D, u32, u32, bool)>,
    /// The last GPU frame, held until the NEXT bind so its decode surface stays out of the reuse
    /// pool at least until this frame's copy has been queued ahead of any later decoder write.
    gpu_frame: Option<GpuFrame>,
    /// CPU path: dynamic luma + chroma plane textures + their SRVs — `(y, uv, y_srv, uv_srv, w, h,
    /// ten_bit)`, recreated when the decoded size/bit depth changes.
    #[allow(clippy::type_complexity)]
    plane_tex: Option<(
        ID3D11Texture2D,
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        ID3D11ShaderResourceView,
        u32,
        u32,
        bool,
    )>,
    bound: Option<Bound>,
    /// Source frame dimensions, for the Contain-fit letterbox.
    src_w: u32,
    src_h: u32,
    /// Panel (swapchain) size in physical pixels + the window DPI, updated on resize.
    panel_w: u32,
    panel_h: u32,
    dpi: u32,
    /// Whether the swapchain is currently in 10-bit HDR10 (R10G10B10A2 + ST.2084) mode.
    hdr: bool,
    /// The source's static HDR mastering metadata received over the protocol (`0xCE`), applied via
    /// `SetHDRMetaData` so the display tone-maps from the real grade instead of a generic 1000-nit
    /// guess. `None` until the first update arrives (then the generic baseline is used).
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
}

/// Latest source HDR mastering metadata, written by the session pump (`session.rs`, the sole
/// `next_hdr_meta` consumer) and read by the render thread before each present — decoupled so the
/// presenter doesn't need the connector. One session at a time on the client, so a single slot.
pub static LATEST_HDR_META: std::sync::Mutex<Option<punktfunk_core::quic::HdrMeta>> =
    std::sync::Mutex::new(None);

impl Presenter {
    /// Create the presenter on the process-wide shared D3D11 device (the one the decoder uses), plus
    /// the composition swapchain + shaders, sized to the panel in physical pixels at `dpi`.
    pub fn new(width: u32, height: u32, dpi: u32) -> Result<Presenter> {
        let shared = crate::gpu::shared().ok_or_else(|| anyhow!("no shared D3D11 device"))?;
        let device = shared.device.clone();
        let context = shared.context.clone();
        let (vs, ps_yuv, sampler) = build_pipeline(&device)?;
        // The per-frame CSC rows (three float4s). Dynamic: rewritten with Map-discard on bind.
        let csc_desc = D3D11_BUFFER_DESC {
            ByteWidth: 48,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let csc_buf = unsafe {
            let mut b = None;
            device
                .CreateBuffer(&csc_desc, None, Some(&mut b))
                .context("CreateBuffer (CSC rows)")?;
            b.ok_or_else(|| anyhow!("null CSC constant buffer"))?
        };
        let (swap, swap_flags) =
            create_composition_swapchain(&device, width.max(1), height.max(1))?;
        // ≤1 queued present: the render thread blocks on the waitable, so a frame is only drawn
        // when the compositor is ready to take it — the newest-wins drain happens after the wait.
        let waitable = (swap_flags & DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32
            != 0)
            .then(|| unsafe {
                let sc2: IDXGISwapChain2 = swap.cast().ok()?;
                sc2.SetMaximumFrameLatency(1).ok()?;
                let h = sc2.GetFrameLatencyWaitableObject();
                (!h.is_invalid()).then_some(h)
            })
            .flatten();
        let p = Presenter {
            device,
            context,
            vs,
            ps_yuv,
            csc_buf,
            sampler,
            swap,
            swap_flags,
            waitable,
            rtv: None,
            sample_tex: None,
            gpu_frame: None,
            plane_tex: None,
            bound: None,
            src_w: 1,
            src_h: 1,
            panel_w: width.max(1),
            panel_h: height.max(1),
            dpi: dpi.max(96),
            hdr: false,
            hdr_meta: None,
        };
        p.apply_dpi_matrix();
        Ok(p)
    }

    /// Block until the swapchain can take another present (≤ `timeout_ms`). True when a present
    /// slot is free; also true on the flagless fallback (no throttle available, just present).
    pub fn wait_present_slot(&self, timeout_ms: u32) -> bool {
        match self.waitable {
            Some(h) => unsafe { WaitForSingleObject(h, timeout_ms) == WAIT_OBJECT_0 },
            None => true,
        }
    }

    /// Update the source HDR mastering metadata (from the `0xCE` plane). Stored for the next HDR
    /// swapchain switch, and applied immediately if already presenting HDR. A no-op when unchanged
    /// (so it's cheap to call every frame from the render loop).
    pub fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
        if self.hdr_meta == Some(meta) {
            return;
        }
        self.hdr_meta = Some(meta);
        if self.hdr {
            unsafe { self.apply_hdr_metadata() };
        }
    }

    /// The DXGI swapchain to hand to `SwapChainPanelHandle::set_swap_chain`.
    pub fn swap_chain(&self) -> &IDXGISwapChain1 {
        &self.swap
    }

    /// Resize the back buffers to the panel's new size in physical pixels at `dpi` (drops the
    /// stale RTV, re-applies the DIP↔pixel matrix).
    pub fn resize(&mut self, width: u32, height: u32, dpi: u32) {
        let dpi = dpi.max(96);
        if width == 0
            || height == 0
            || (width == self.panel_w && height == self.panel_h && dpi == self.dpi)
        {
            return;
        }
        self.rtv = None; // release all back-buffer refs before ResizeBuffers
        unsafe {
            if let Err(e) = self.swap.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(self.swap_flags as i32),
            ) {
                tracing::warn!(error = %e, "ResizeBuffers failed");
                return;
            }
        }
        self.panel_w = width;
        self.panel_h = height;
        self.dpi = dpi;
        self.apply_dpi_matrix();
    }

    /// Map the pixel-sized buffers into the panel's DIP coordinate space (scale 96/DPI) — XAML
    /// otherwise stretches whatever size the buffers are to the panel's DIP bounds (blurry).
    fn apply_dpi_matrix(&self) {
        let s = 96.0 / self.dpi as f32;
        if let Ok(sc2) = self.swap.cast::<IDXGISwapChain2>() {
            let m = DXGI_MATRIX_3X2_F {
                _11: s,
                _22: s,
                ..Default::default()
            };
            if let Err(e) = unsafe { sc2.SetMatrixTransform(&m) } {
                tracing::warn!(error = %e, "SetMatrixTransform failed");
            }
        }
    }

    /// Present one decoded frame (Contain-fit) — or, when `frame` is `None`, re-present the last
    /// one (or black). Called from the render thread. Takes the frame by value: the GPU path
    /// retains the decoder surface until the next bind.
    pub fn present(&mut self, frame: Option<DecodedFrame>) {
        match frame {
            Some(DecodedFrame::Cpu(c)) => {
                if c.hdr != self.hdr {
                    self.set_hdr(c.hdr);
                }
                if let Err(e) = self.upload(&c) {
                    tracing::warn!(error = %e, "frame upload failed");
                }
            }
            Some(DecodedFrame::Gpu(g)) => {
                if g.hdr != self.hdr {
                    self.set_hdr(g.hdr);
                }
                if let Err(e) = self.bind_gpu(g) {
                    tracing::warn!(error = %e, "GPU frame bind failed");
                }
            }
            None => {}
        }
        self.draw();
    }

    /// Copy the decoded slice into our sampleable texture and build per-plane SRVs over it. The
    /// decode array is decoder-only (NVIDIA won't bind a decoder array as a shader resource), so
    /// it can't be sampled directly — one GPU-to-GPU copy makes the frame sampleable on every
    /// vendor. D3D11 planar semantics: the slice is ONE subresource (both planes copy together),
    /// and the source box is display-size (the array is coded-size; a full-resource copy would
    /// size-mismatch and be silently dropped).
    fn bind_gpu(&mut self, g: GpuFrame) -> Result<()> {
        let src: ID3D11Texture2D = unsafe {
            let raw = g.texture_ptr();
            ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or_else(|| anyhow!("null D3D11 texture"))?
                .clone()
        };
        self.ensure_sample_tex(g.width, g.height, g.ten_bit)?;
        let dst = self.sample_tex.as_ref().unwrap().0.clone();
        // Even-aligned luma coordinates (NV12/P010 chroma is 2×2 subsampled).
        let src_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: g.width & !1,
            bottom: g.height & !1,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, g.index, Some(&src_box));
        }
        let (fy, fc) = plane_formats(g.ten_bit);
        let y = self.plane_srv(&dst, fy)?;
        let c = self.plane_srv(&dst, fc)?;
        self.write_csc_rows(g.color, g.ten_bit)?;
        self.src_w = g.width;
        self.src_h = g.height;
        self.bound = Some(Bound { y, c });
        // Hold the frame until the next bind: its decode surface stays out of the reuse pool
        // until this copy is queued ahead of any later decoder write (previous frame drops here).
        self.gpu_frame = Some(g);
        Ok(())
    }

    /// Ensure the sampleable copy texture matches the decoded frame's size + bit depth (NV12 for
    /// 8-bit, P010 for 10-bit — the same format as the decode array, a `CopySubresourceRegion`
    /// requirement), recreating it on a change.
    fn ensure_sample_tex(&mut self, w: u32, h: u32, ten_bit: bool) -> Result<()> {
        if matches!(&self.sample_tex, Some((_, tw, th, tb)) if *tw == w && *th == h && *tb == ten_bit)
        {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: if ten_bit {
                DXGI_FORMAT_P010
            } else {
                DXGI_FORMAT_NV12
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let tex = unsafe {
            let mut t = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D (sample target)")?;
            t.ok_or_else(|| anyhow!("null sample texture"))?
        };
        self.sample_tex = Some((tex, w, h, ten_bit));
        Ok(())
    }

    /// A shader-resource view over one plane of a single (non-array) NV12/P010 texture — the
    /// R8/R8G8 (or R16/R16G16) format selects the luma vs. chroma plane (the D3D11 video
    /// sub-format trick).
    fn plane_srv(
        &self,
        tex: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        unsafe {
            let mut srv = None;
            self.device
                .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))
                .context("CreateShaderResourceView (plane)")?;
            srv.ok_or_else(|| anyhow!("null SRV"))
        }
    }

    /// Upload a software-decoded frame's two planes into the dynamic plane textures (created to
    /// match size/bit depth), feeding the same SRV slots + shaders as the GPU path.
    fn upload(&mut self, frame: &CpuFrame) -> Result<()> {
        let (w, h) = (frame.width, frame.height);
        let rebuild = !matches!(&self.plane_tex,
            Some((.., tw, th, tb)) if *tw == w && *th == h && *tb == frame.ten_bit);
        if rebuild {
            let (fy, fc) = plane_formats(frame.ten_bit);
            let y = self.dynamic_tex(w, h, fy)?;
            let uv = self.dynamic_tex(w.div_ceil(2), h.div_ceil(2), fc)?;
            let y_srv = self.plane_srv(&y, fy)?;
            let uv_srv = self.plane_srv(&uv, fc)?;
            self.plane_tex = Some((y, uv, y_srv, uv_srv, w, h, frame.ten_bit));
        }
        let (y, uv, y_srv, uv_srv, ..) = self.plane_tex.as_ref().unwrap();
        let bytes = if frame.ten_bit { 2 } else { 1 };
        self.map_rows(y, &frame.y, frame.y_stride, w as usize * bytes, h as usize)?;
        self.map_rows(
            uv,
            &frame.uv,
            frame.uv_stride,
            w.div_ceil(2) as usize * 2 * bytes,
            h.div_ceil(2) as usize,
        )?;
        let (y_srv, uv_srv) = (y_srv.clone(), uv_srv.clone());
        self.write_csc_rows(frame.color, frame.ten_bit)?;
        self.src_w = w;
        self.src_h = h;
        self.bound = Some(Bound {
            y: y_srv,
            c: uv_srv,
        });
        self.gpu_frame = None; // drop any held GPU frame
        Ok(())
    }

    fn dynamic_tex(&self, w: u32, h: u32, format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
        };
        unsafe {
            let mut t = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D (plane)")?;
            t.ok_or_else(|| anyhow!("null plane texture"))
        }
    }

    /// Recompute the bound frame's Y′CbCr→RGB rows from its CICP signaling and Map-discard them
    /// into the CSC constant buffer. `ten_bit` selects the 10-bit code points AND the P010
    /// high-bit repack (the plane SRVs are R16/R16G16 UNORM for 10-bit).
    fn write_csc_rows(&self, color: pf_client_core::video::ColorDesc, ten_bit: bool) -> Result<()> {
        let rows = pf_client_core::video::csc_rows(color, if ten_bit { 10 } else { 8 }, ten_bit);
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(
                    &self.csc_buf,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .context("Map CSC constant buffer")?;
            std::ptr::copy_nonoverlapping(
                rows.as_ptr() as *const u8,
                mapped.pData as *mut u8,
                48, // [[f32; 4]; 3]
            );
            self.context.Unmap(&self.csc_buf, 0);
        }
        Ok(())
    }

    /// Map-discard `tex` and copy `rows` rows of `row_bytes` from `src` (stride `src_pitch`).
    fn map_rows(
        &self,
        tex: &ID3D11Texture2D,
        src: &[u8],
        src_pitch: usize,
        row_bytes: usize,
        rows: usize,
    ) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("Map plane texture")?;
            let dst = mapped.pData as *mut u8;
            let dst_pitch = mapped.RowPitch as usize;
            let n = row_bytes.min(src_pitch);
            for r in 0..rows {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(r * src_pitch),
                    dst.add(r * dst_pitch),
                    n,
                );
            }
            self.context.Unmap(tex, 0);
        }
        Ok(())
    }

    fn draw(&mut self) {
        let Ok(rtv) = self.rtv() else {
            return;
        };
        let (pw, ph) = (self.panel_w, self.panel_h);
        unsafe {
            let c = &self.context;
            c.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            if let Some(bound) = &self.bound {
                // Contain-fit viewport: scale to the smaller axis, centre, letterbox the rest.
                let (ww, wh, vfw, vfh) = (
                    pw as f32,
                    ph as f32,
                    self.src_w.max(1) as f32,
                    self.src_h.max(1) as f32,
                );
                let scale = (ww / vfw).min(wh / vfh);
                let (dw, dh) = (vfw * scale, vfh * scale);
                let (ox, oy) = ((ww - dw) / 2.0, (wh - dh) / 2.0);
                c.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
                let vp = D3D11_VIEWPORT {
                    TopLeftX: ox,
                    TopLeftY: oy,
                    Width: dw,
                    Height: dh,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                };
                c.RSSetViewports(Some(&[vp]));
                c.IASetInputLayout(None);
                c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                c.VSSetShader(&self.vs, None);
                c.PSSetShader(&self.ps_yuv, None);
                c.PSSetConstantBuffers(0, Some(&[Some(self.csc_buf.clone())]));
                c.PSSetShaderResources(0, Some(&[Some(bound.y.clone()), Some(bound.c.clone())]));
                c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                c.Draw(3, 0);
            }
            let _ = self.swap.Present(1, DXGI_PRESENT(0));
        }
    }

    /// Switch the swapchain between 8-bit SDR (B8G8R8A8, BT.709) and 10-bit HDR10 (R10G10B10A2,
    /// ST.2084 PQ BT.2020). `ResizeBuffers` changes the back-buffer format in place, so the panel
    /// binding (`set_swap_chain`) stays valid — no rebind. Both frame sources already produce
    /// PQ-encoded BT.2020 for HDR, so the colour space is all the compositor needs.
    fn set_hdr(&mut self, on: bool) {
        self.rtv = None; // release back-buffer refs before ResizeBuffers
        let format = if on {
            DXGI_FORMAT_R10G10B10A2_UNORM
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        };
        unsafe {
            if let Err(e) = self.swap.ResizeBuffers(
                0,
                self.panel_w,
                self.panel_h,
                format,
                DXGI_SWAP_CHAIN_FLAG(self.swap_flags as i32),
            ) {
                tracing::warn!(error = %e, "ResizeBuffers for HDR switch failed");
                return;
            }
            let colorspace = if on {
                DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            if let Ok(sc3) = self.swap.cast::<IDXGISwapChain3>() {
                // Only set a colour space the swapchain accepts for present (on an SDR desktop the
                // DWM still tone-maps HDR10 → SDR, so leaving the default there is fine).
                if let Ok(support) = sc3.CheckColorSpaceSupport(colorspace) {
                    if support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 != 0 {
                        if let Err(e) = sc3.SetColorSpace1(colorspace) {
                            // A silent failure here presents PQ content as SDR gamma (crushed/dark) —
                            // surface it instead of swallowing it.
                            tracing::warn!(error = %e, ?colorspace, "SetColorSpace1 failed");
                        }
                    } else if on {
                        tracing::warn!("swapchain rejects BT.2020 PQ present colour space (SDR display?) — DWM tone-maps");
                    }
                }
            }
            self.hdr = on;
            if on {
                self.apply_hdr_metadata();
            }
        }
        self.apply_dpi_matrix(); // belt-and-braces: keep the DIP mapping across the format switch
        tracing::info!(hdr = on, "swapchain colour mode switched");
    }

    /// Push the current `DXGI_HDR_METADATA_HDR10` to the swapchain. Uses the source's received
    /// mastering metadata when known, else a generic HDR10 baseline. Caller ensures HDR mode.
    unsafe fn apply_hdr_metadata(&self) {
        if let Ok(sc4) = self.swap.cast::<IDXGISwapChain4>() {
            let md = self
                .hdr_meta
                .map(hdr_meta_to_dxgi)
                .unwrap_or_else(generic_hdr10_metadata);
            let bytes = std::slice::from_raw_parts(
                &md as *const DXGI_HDR_METADATA_HDR10 as *const u8,
                std::mem::size_of::<DXGI_HDR_METADATA_HDR10>(),
            );
            if let Err(e) = sc4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes)) {
                tracing::warn!(error = %e, "SetHDRMetaData failed");
            }
        }
    }

    fn rtv(&mut self) -> Result<ID3D11RenderTargetView> {
        if self.rtv.is_none() {
            let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0).context("GetBuffer")? };
            let rtv = unsafe {
                let mut v = None;
                self.device
                    .CreateRenderTargetView(&back, None, Some(&mut v))
                    .context("CreateRenderTargetView")?;
                v.unwrap()
            };
            self.rtv = Some(rtv);
        }
        Ok(self.rtv.clone().unwrap())
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        if let Some(h) = self.waitable.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Luma + chroma plane view formats for NV12 (8-bit) vs P010 (10-in-16-bit).
fn plane_formats(ten_bit: bool) -> (DXGI_FORMAT, DXGI_FORMAT) {
    if ten_bit {
        (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
    } else {
        (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
    }
}

/// A composition flip-model swapchain (no HWND) for binding to a XAML `SwapChainPanel`, with the
/// frame-latency waitable when the driver allows it. Returns the swapchain + the flags it was
/// created with (every `ResizeBuffers` must re-pass them).
fn create_composition_swapchain(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(IDXGISwapChain1, u32)> {
    let dxdev: IDXGIDevice = device.cast().context("IDXGIDevice cast")?;
    let factory: IDXGIFactory2 = unsafe {
        let adapter = dxdev.GetAdapter().context("GetAdapter")?;
        adapter.GetParent().context("GetParent (IDXGIFactory2)")?
    };
    let mut desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        // IGNORE (opaque), not PREMULTIPLIED: the video fills the panel with opaque RGB either way.
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
    };
    unsafe {
        match factory.CreateSwapChainForComposition(device, &desc, None) {
            Ok(sc) => Ok((sc, desc.Flags)),
            Err(e) => {
                // Odd driver/WARP combinations can reject the waitable — fall back to plain
                // Present(1) pacing rather than failing the stream page.
                tracing::warn!(error = %e, "waitable swapchain rejected — creating without");
                desc.Flags = 0;
                let sc = factory
                    .CreateSwapChainForComposition(device, &desc, None)
                    .context("CreateSwapChainForComposition")?;
                Ok((sc, 0))
            }
        }
    }
}

fn build_pipeline(
    device: &ID3D11Device,
) -> Result<(ID3D11VertexShader, ID3D11PixelShader, ID3D11SamplerState)> {
    let vs_blob = compile(SHADER_HLSL, "vs_main", "vs_5_0")?;
    let yuv_blob = compile(SHADER_HLSL, "ps_yuv", "ps_5_0")?;
    unsafe {
        let mut vs = None;
        device
            .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
            .context("CreateVertexShader")?;
        let mut ps_yuv = None;
        device
            .CreatePixelShader(blob_bytes(&yuv_blob), None, Some(&mut ps_yuv))
            .context("CreatePixelShader (yuv)")?;
        let sdesc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: D3D11_FLOAT32_MAX,
            ..Default::default()
        };
        let mut sampler = None;
        device
            .CreateSamplerState(&sdesc, Some(&mut sampler))
            .context("CreateSamplerState")?;
        Ok((vs.unwrap(), ps_yuv.unwrap(), sampler.unwrap()))
    }
}

fn compile(src: &str, entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry_c = std::ffi::CString::new(entry).unwrap();
    let target_c = std::ffi::CString::new(target).unwrap();
    let mut code = None;
    let mut errors = None;
    let r = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if r.is_err() {
        let msg = errors
            .as_ref()
            .map(|b| unsafe {
                let p = b.GetBufferPointer() as *const u8;
                let n = b.GetBufferSize();
                String::from_utf8_lossy(std::slice::from_raw_parts(p, n)).to_string()
            })
            .unwrap_or_default();
        return Err(anyhow!("D3DCompile {entry}: {msg}"));
    }
    code.ok_or_else(|| anyhow!("D3DCompile produced no bytecode"))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        let p = blob.GetBufferPointer() as *const u8;
        let n = blob.GetBufferSize();
        std::slice::from_raw_parts(p, n)
    }
}

/// True if any attached display is currently in HDR (BT.2020 PQ) mode. The client advertises HDR
/// caps only when this holds, so an SDR display gets a proper 8-bit BT.709 stream instead of PQ it
/// would mis-tone-map (the washed-out/dark failure); an HDR display self-tone-maps from the
/// mastering metadata. Coarse — checks ANY output, not the app's specific monitor; a mid-session
/// monitor move to/from HDR is a follow-up (the `Reconfigure` downgrade).
pub fn display_supports_hdr() -> bool {
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return false,
        };
        let mut ai = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                if let Ok(o6) = output.cast::<IDXGIOutput6>() {
                    if let Ok(desc) = o6.GetDesc1() {
                        if desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// The HDR display's colour volume from `IDXGIOutput6::GetDesc1` — the first output currently in
/// HDR (BT.2020 PQ) mode, as [`HdrMeta`](punktfunk_core::quic::HdrMeta) for `Hello::display_hdr`.
/// The host writes this volume into its virtual display's EDID, so host apps tone-map to THIS
/// panel and the PQ stream needs no client-side rescue. Chromaticities come as CIE xy floats
/// (×50000 → ST.2086 units, G/B/R order); luminances as nits floats (max ×10000 → 0.0001-cd/m²
/// units); `MaxFullFrameLuminance` → MaxFALL (whole nits); MaxCLL stays 0 (a display has no
/// content light level). Same ANY-output coarseness as [`display_supports_hdr`] — the session
/// gates on that check first, so both look at the same panel in the single-HDR-display case.
pub fn display_hdr_volume() -> Option<punktfunk_core::quic::HdrMeta> {
    let to_2086 = |v: f32| (v * 50000.0).round().clamp(0.0, 65535.0) as u16;
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut ai = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(o6) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(desc) = o6.GetDesc1() else { continue };
                if desc.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
                    continue;
                }
                return Some(punktfunk_core::quic::HdrMeta {
                    // ST.2086 order is G, B, R.
                    display_primaries: [
                        [to_2086(desc.GreenPrimary[0]), to_2086(desc.GreenPrimary[1])],
                        [to_2086(desc.BluePrimary[0]), to_2086(desc.BluePrimary[1])],
                        [to_2086(desc.RedPrimary[0]), to_2086(desc.RedPrimary[1])],
                    ],
                    white_point: [to_2086(desc.WhitePoint[0]), to_2086(desc.WhitePoint[1])],
                    max_display_mastering_luminance: (desc.MaxLuminance.max(0.0) * 10_000.0).round()
                        as u32,
                    min_display_mastering_luminance: (desc.MinLuminance.max(0.0) * 10_000.0).round()
                        as u32,
                    max_cll: 0,
                    max_fall: desc.MaxFullFrameLuminance.max(0.0).round() as u16,
                });
            }
        }
    }
    None
}

/// Generic HDR10 mastering metadata: BT.2020 primaries + D65 white, a 1000-nit mastering display,
/// MaxCLL 1000 / MaxFALL 400. The fallback used only until the host's real `0xCE` metadata arrives.
fn generic_hdr10_metadata() -> DXGI_HDR_METADATA_HDR10 {
    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: [35400, 14600],
        GreenPrimary: [8500, 39850],
        BluePrimary: [6550, 2300],
        WhitePoint: [15635, 16450],
        MaxMasteringLuminance: 1000,
        MinMasteringLuminance: 1, // 0.0001-nit units → 0.0001 nits
        MaxContentLightLevel: 1000,
        MaxFrameAverageLightLevel: 400,
    }
}

/// Map the protocol's [`HdrMeta`](punktfunk_core::quic::HdrMeta) to `DXGI_HDR_METADATA_HDR10`.
/// Two careful conversions: HdrMeta stores primaries in **ST.2086 G,B,R order**, DXGI wants
/// **R,G,B**; and HdrMeta mastering luminance is in **0.0001-cd/m² units** while DXGI's
/// `MaxMasteringLuminance` is in **whole nits** (MinMasteringLuminance stays 0.0001-nit). Chromaticity
/// units (1/50000) and MaxCLL/MaxFALL (nits) match 1:1.
fn hdr_meta_to_dxgi(m: punktfunk_core::quic::HdrMeta) -> DXGI_HDR_METADATA_HDR10 {
    let [g, b, r] = m.display_primaries; // ST.2086 order
    DXGI_HDR_METADATA_HDR10 {
        RedPrimary: r,
        GreenPrimary: g,
        BluePrimary: b,
        WhitePoint: m.white_point,
        MaxMasteringLuminance: m.max_display_mastering_luminance / 10_000, // 0.0001-nit → nit
        MinMasteringLuminance: m.min_display_mastering_luminance,          // already 0.0001-nit
        MaxContentLightLevel: m.max_cll,
        MaxFrameAverageLightLevel: m.max_fall,
    }
}
