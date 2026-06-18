//! Direct3D11 presenter for a WinUI 3 `SwapChainPanel`. It draws a decoded frame Contain-fit into a
//! **composition** flip-model swapchain, which the reactor stream page binds to the panel via
//! `SwapChainPanelHandle::set_swap_chain`.
//!
//! Two frame sources, one swapchain:
//!
//! * **GPU (zero-copy)** — [`crate::video::GpuFrame`] is a decoder-owned NV12/P010 `ID3D11Texture2D`
//!   array slice (D3D11VA). We create per-plane shader-resource views over the slice and convert
//!   YUV→RGB in a pixel shader: NV12 via BT.709 (`ps_nv12`), P010 via BT.2020 with the PQ transfer
//!   left intact (`ps_p010`). No CPU copy. The decoder uses the **same** shared device
//!   ([`crate::gpu`]) so the texture is bindable here.
//! * **CPU upload** — [`crate::video::CpuFrame`] is packed RGBA (SDR) or X2BGR10 (HDR) from the
//!   software decoder; we upload it into a dynamic texture and draw it with a passthrough shader
//!   (`ps_rgba`). The fallback path.
//!
//! **HDR10**: when a frame is BT.2020 PQ the swapchain flips to `R10G10B10A2` +
//! `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` (+ HDR10 metadata) via `ResizeBuffers`/
//! `SetColorSpace1`; the shader output is already PQ-encoded so the compositor maps PQ→display. SDR
//! stays 8-bit B8G8R8A8.
//!
//! All `windows` types here come from the same windows-rs commit as `windows-reactor`, so the
//! `IDXGISwapChain1` handed to `set_swap_chain` satisfies reactor's `windows_core::Interface`.

use crate::video::{DecodedFrame, GpuFrame};
use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

// One vertex shader (fullscreen triangle) + three pixel shaders, selected per frame source. tex0 is
// RGBA (passthrough) or the luma plane; tex1 is the chroma plane. The YUV→RGB matrices fold the
// limited→full range scale into the coefficients; for P010 the R16 sample is rescaled (×65535/65472)
// to undo the 10-bits-in-the-high-bits packing, then converted with BT.2020 NCL, PQ preserved.
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

float4 ps_rgba(VSOut i) : SV_Target { return tex0.Sample(smp, i.uv); }

float4 ps_nv12(VSOut i) : SV_Target {
    float  y  = tex0.Sample(smp, i.uv).r;
    float2 uv = tex1.Sample(smp, i.uv).rg;
    float yy = (y - 0.0627451) * 1.164384;   // (Y-16/255)*255/219
    float u  = uv.x - 0.5;
    float v  = uv.y - 0.5;                    // BT.709 limited, chroma scale folded
    float r = yy + 1.792741 * v;
    float g = yy - 0.213249 * u - 0.532909 * v;
    float b = yy + 2.112402 * u;
    return float4(saturate(float3(r, g, b)), 1.0);
}

float4 ps_p010(VSOut i) : SV_Target {
    const float S = 65535.0 / 65472.0;       // undo P010 high-bit packing → exact 10-bit / 1023
    float  y  = tex0.Sample(smp, i.uv).r  * S;
    float2 uv = tex1.Sample(smp, i.uv).rg * S;
    float yy = (y - 0.0625611) * 1.167808;   // (Y-64/1023)*1023/876
    float u  = uv.x - 0.5;
    float v  = uv.y - 0.5;                    // BT.2020 NCL limited, chroma scale folded; PQ kept
    float r = yy + 1.683611 * v;
    float g = yy - 0.187877 * u - 0.652337 * v;
    float b = yy + 2.148072 * u;
    return float4(saturate(float3(r, g, b)), 1.0);
}
"#;

/// A bound GPU frame: per-plane SRVs over the decoder's texture-array slice, plus the `GpuFrame`
/// itself kept alive so the decoder won't recycle the slice while we re-present it.
struct GpuView {
    y: ID3D11ShaderResourceView,
    c: ID3D11ShaderResourceView,
    /// Held only for its `Drop` (returns the decoder surface to the reuse pool) — never read.
    #[allow(dead_code)]
    frame: GpuFrame,
}

/// Current draw source.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Empty,
    Rgba,
    Nv12,
    P010,
}

pub struct Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_rgba: ID3D11PixelShader,
    ps_nv12: ID3D11PixelShader,
    ps_p010: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    /// CPU-upload texture + SRV + dimensions; recreated when the decoded size/format changes.
    cpu_tex: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
    /// Bound zero-copy GPU frame (held to keep its decoder surface alive).
    gpu: Option<GpuView>,
    mode: Mode,
    /// Source frame dimensions, for the Contain-fit letterbox.
    src_w: u32,
    src_h: u32,
    /// Panel (swapchain) size in pixels, updated on resize.
    panel_w: u32,
    panel_h: u32,
    /// Whether the swapchain is currently in 10-bit HDR10 (R10G10B10A2 + ST.2084) mode.
    hdr: bool,
}

impl Presenter {
    /// Create the presenter on the process-wide shared D3D11 device (the one the decoder uses), plus
    /// the composition swapchain + shaders, sized to the panel.
    pub fn new(width: u32, height: u32) -> Result<Presenter> {
        let shared = crate::gpu::shared().ok_or_else(|| anyhow!("no shared D3D11 device"))?;
        let device = shared.device.clone();
        let context = shared.context.clone();
        let (vs, ps_rgba, ps_nv12, ps_p010, sampler) = build_pipeline(&device)?;
        let swap = create_composition_swapchain(&device, width.max(1), height.max(1))?;
        Ok(Presenter {
            device,
            context,
            vs,
            ps_rgba,
            ps_nv12,
            ps_p010,
            sampler,
            swap,
            rtv: None,
            cpu_tex: None,
            gpu: None,
            mode: Mode::Empty,
            src_w: 1,
            src_h: 1,
            panel_w: width.max(1),
            panel_h: height.max(1),
            hdr: false,
        })
    }

    /// The DXGI swapchain to hand to `SwapChainPanelHandle::set_swap_chain`.
    pub fn swap_chain(&self) -> &IDXGISwapChain1 {
        &self.swap
    }

    /// Resize the back buffers to the panel's new size (drops the stale RTV).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || (width == self.panel_w && height == self.panel_h) {
            return;
        }
        self.rtv = None; // release all back-buffer refs before ResizeBuffers
        unsafe {
            let _ = self.swap.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
        }
        self.panel_w = width;
        self.panel_h = height;
    }

    /// Present one decoded frame (Contain-fit) — or, when `frame` is `None`, re-present the last one
    /// (or black). Called from the reactor `on_rendering` per-frame callback on the UI thread. Takes
    /// the frame by value so the GPU path can retain the decoder surface across re-presents.
    pub fn present(&mut self, frame: Option<DecodedFrame>) {
        match frame {
            Some(DecodedFrame::Cpu(c)) => {
                if c.hdr != self.hdr {
                    self.set_hdr(c.hdr);
                }
                if let Err(e) = self.upload(&c) {
                    tracing::warn!(error = %e, "frame upload failed");
                } else {
                    self.mode = Mode::Rgba;
                    self.src_w = c.width;
                    self.src_h = c.height;
                    self.gpu = None; // drop any held GPU frame
                }
            }
            Some(DecodedFrame::Gpu(g)) => {
                if g.hdr != self.hdr {
                    self.set_hdr(g.hdr);
                }
                match self.bind_gpu(g) {
                    Ok(()) => {}
                    Err(e) => tracing::warn!(error = %e, "GPU frame bind failed"),
                }
            }
            None => {}
        }
        self.draw();
    }

    /// Build per-plane SRVs over the decoded texture-array slice and retain the frame.
    fn bind_gpu(&mut self, g: GpuFrame) -> Result<()> {
        let tex: ID3D11Texture2D = unsafe {
            let raw = g.texture_ptr();
            ID3D11Texture2D::from_raw_borrowed(&raw)
                .ok_or_else(|| anyhow!("null D3D11 texture"))?
                .clone()
        };
        // NV12: R8 luma + R8G8 chroma. P010: R16 luma + R16G16 chroma (10 bits in the high bits).
        let (fy, fc) = if g.hdr {
            (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
        } else {
            (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
        };
        let y = self.array_srv(&tex, fy, g.index)?;
        let c = self.array_srv(&tex, fc, g.index)?;
        self.mode = if g.hdr { Mode::P010 } else { Mode::Nv12 };
        self.src_w = g.width;
        self.src_h = g.height;
        self.gpu = Some(GpuView { y, c, frame: g });
        Ok(())
    }

    /// A shader-resource view over a single slice of a texture array, reinterpreting the plane
    /// format (the NV12/P010 sub-format trick D3D11 allows on video textures).
    fn array_srv(
        &self,
        tex: &ID3D11Texture2D,
        format: DXGI_FORMAT,
        slice: u32,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: slice,
                    ArraySize: 1,
                },
            },
        };
        unsafe {
            let mut srv = None;
            self.device
                .CreateShaderResourceView(tex, Some(&desc), Some(&mut srv))
                .context("CreateShaderResourceView (array slice)")?;
            srv.ok_or_else(|| anyhow!("null SRV"))
        }
    }

    fn draw(&mut self) {
        let Ok(rtv) = self.rtv() else {
            return;
        };
        let (pw, ph) = (self.panel_w, self.panel_h);
        // Resolve the current source's shader + the (up to two) SRVs to bind — cheap interface
        // clones. Each arm yields `Option<(&pixel_shader, [Option<SRV>; 2])>`.
        let binding = match self.mode {
            Mode::Rgba => self
                .cpu_tex
                .as_ref()
                .map(|(_, srv, _, _)| (&self.ps_rgba, [Some(srv.clone()), None])),
            Mode::Nv12 => self
                .gpu
                .as_ref()
                .map(|g| (&self.ps_nv12, [Some(g.y.clone()), Some(g.c.clone())])),
            Mode::P010 => self
                .gpu
                .as_ref()
                .map(|g| (&self.ps_p010, [Some(g.y.clone()), Some(g.c.clone())])),
            Mode::Empty => None,
        };
        unsafe {
            let c = &self.context;
            c.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            if let Some((ps, srvs)) = binding {
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
                c.PSSetShader(ps, None);
                c.PSSetShaderResources(0, Some(&srvs));
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
        self.cpu_tex = None; // CPU texture format changes (R10G10B10A2 vs R8G8B8A8)
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
                DXGI_SWAP_CHAIN_FLAG(0),
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
                        let _ = sc3.SetColorSpace1(colorspace);
                    }
                }
            }
            if on {
                if let Ok(sc4) = self.swap.cast::<IDXGISwapChain4>() {
                    let md = hdr10_metadata();
                    let bytes = std::slice::from_raw_parts(
                        &md as *const DXGI_HDR_METADATA_HDR10 as *const u8,
                        std::mem::size_of::<DXGI_HDR_METADATA_HDR10>(),
                    );
                    let _ = sc4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes));
                }
            }
        }
        self.hdr = on;
        tracing::info!(hdr = on, "swapchain colour mode switched");
    }

    fn upload(&mut self, frame: &crate::video::CpuFrame) -> Result<()> {
        let (w, h) = (frame.width, frame.height);
        let need_new = !matches!(&self.cpu_tex, Some((_, _, tw, th)) if *tw == w && *th == h);
        if need_new {
            let format = if self.hdr {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_R8G8B8A8_UNORM
            };
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
            let texture = unsafe {
                let mut t = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D")?;
                t.unwrap()
            };
            let srv = unsafe {
                let mut s = None;
                self.device
                    .CreateShaderResourceView(&texture, None, Some(&mut s))
                    .context("CreateShaderResourceView")?;
                s.unwrap()
            };
            self.cpu_tex = Some((texture, srv, w, h));
        }
        let (texture, _, _, _) = self.cpu_tex.as_ref().unwrap();
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(texture, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .context("Map video texture")?;
            let dst = mapped.pData as *mut u8;
            let dst_pitch = mapped.RowPitch as usize;
            let src_pitch = frame.stride;
            let row_bytes = (w as usize) * 4;
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    frame.pixels.as_ptr().add(y * src_pitch),
                    dst.add(y * dst_pitch),
                    row_bytes.min(src_pitch),
                );
            }
            self.context.Unmap(texture, 0);
        }
        Ok(())
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

/// A composition flip-model swapchain (no HWND) for binding to a XAML `SwapChainPanel`.
fn create_composition_swapchain(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    let dxdev: IDXGIDevice = device.cast().context("IDXGIDevice cast")?;
    let factory: IDXGIFactory2 = unsafe {
        let adapter = dxdev.GetAdapter().context("GetAdapter")?;
        adapter.GetParent().context("GetParent (IDXGIFactory2)")?
    };
    let desc = DXGI_SWAP_CHAIN_DESC1 {
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
        // IGNORE (opaque), not PREMULTIPLIED: the video fills the panel and the HDR `X2BGR10`
        // upload leaves the 2 padding/alpha bits 0 — premultiplied alpha would then make HDR frames
        // transparent. Opaque is correct for a full-frame video surface either way.
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    unsafe {
        factory
            .CreateSwapChainForComposition(device, &desc, None)
            .context("CreateSwapChainForComposition")
    }
}

fn build_pipeline(
    device: &ID3D11Device,
) -> Result<(
    ID3D11VertexShader,
    ID3D11PixelShader,
    ID3D11PixelShader,
    ID3D11PixelShader,
    ID3D11SamplerState,
)> {
    let vs_blob = compile(SHADER_HLSL, "vs_main", "vs_5_0")?;
    let rgba_blob = compile(SHADER_HLSL, "ps_rgba", "ps_5_0")?;
    let nv12_blob = compile(SHADER_HLSL, "ps_nv12", "ps_5_0")?;
    let p010_blob = compile(SHADER_HLSL, "ps_p010", "ps_5_0")?;
    unsafe {
        let mut vs = None;
        device
            .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
            .context("CreateVertexShader")?;
        let mut ps_rgba = None;
        device
            .CreatePixelShader(blob_bytes(&rgba_blob), None, Some(&mut ps_rgba))
            .context("CreatePixelShader (rgba)")?;
        let mut ps_nv12 = None;
        device
            .CreatePixelShader(blob_bytes(&nv12_blob), None, Some(&mut ps_nv12))
            .context("CreatePixelShader (nv12)")?;
        let mut ps_p010 = None;
        device
            .CreatePixelShader(blob_bytes(&p010_blob), None, Some(&mut ps_p010))
            .context("CreatePixelShader (p010)")?;
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
        Ok((
            vs.unwrap(),
            ps_rgba.unwrap(),
            ps_nv12.unwrap(),
            ps_p010.unwrap(),
            sampler.unwrap(),
        ))
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

/// Generic HDR10 mastering metadata: BT.2020 primaries + D65 white, a 1000-nit mastering display,
/// MaxCLL 1000 / MaxFALL 400. The protocol doesn't carry the stream's real mastering metadata yet
/// (host follow-up), so these are sane defaults the display tone-maps from.
fn hdr10_metadata() -> DXGI_HDR_METADATA_HDR10 {
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
