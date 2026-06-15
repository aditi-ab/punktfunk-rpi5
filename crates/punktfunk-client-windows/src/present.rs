//! Direct3D11 presenter for a WinUI 3 `SwapChainPanel`: upload a decoded `CpuFrame` (RGBA)
//! into a dynamic texture and draw it Contain-fit into a **composition** flip-model swapchain,
//! which the reactor stream page binds to the panel via `SwapChainPanelHandle::set_swap_chain`.
//!
//! The device prefers a hardware adapter and falls back to **WARP** (the GPU-less dev box runs
//! the whole present path in software). The draw is a single full-screen triangle sampling the
//! video texture; a letterbox is produced by clearing the back buffer black and setting the
//! viewport to the Contain-fit rect (no per-frame vertex buffer). SDR 8-bit path; the
//! 10-bit/HDR present (`R10G10B10A2` + `SetColorSpace1`) is a follow-up alongside P010 decode.
//!
//! All `windows` types here come from the same windows-rs commit as `windows-reactor`, so the
//! `IDXGISwapChain1` handed to `set_swap_chain` satisfies reactor's `windows_core::Interface`.

use crate::video::CpuFrame;
use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

const SHADER_HLSL: &str = r#"
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut vs_main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VSOut o;
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps_main(VSOut i) : SV_Target { return tex.Sample(smp, i.uv); }
"#;

pub struct Presenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    /// Video texture + SRV + dimensions; recreated when the decoded size changes.
    tex: Option<(ID3D11Texture2D, ID3D11ShaderResourceView, u32, u32)>,
    /// Panel (swapchain) size in pixels, updated on resize.
    panel_w: u32,
    panel_h: u32,
}

impl Presenter {
    /// Create the D3D11 device + composition swapchain + shaders, sized to the panel.
    pub fn new(width: u32, height: u32) -> Result<Presenter> {
        let (device, context) = create_device()?;
        let (vs, ps, sampler) = build_pipeline(&device)?;
        let swap = create_composition_swapchain(&device, width.max(1), height.max(1))?;
        Ok(Presenter {
            device,
            context,
            vs,
            ps,
            sampler,
            swap,
            rtv: None,
            tex: None,
            panel_w: width.max(1),
            panel_h: height.max(1),
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

    /// Present one decoded frame (Contain-fit) — or, when `frame` is `None`, just re-present the
    /// last texture (or black). Called from the reactor `on_rendering` per-frame callback.
    pub fn present(&mut self, frame: Option<&CpuFrame>) {
        if let Some(f) = frame {
            if let Err(e) = self.upload(f) {
                tracing::warn!(error = %e, "frame upload failed");
            }
        }
        let Ok(rtv) = self.rtv() else {
            return;
        };
        let (pw, ph) = (self.panel_w, self.panel_h);
        unsafe {
            let c = &self.context;
            c.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            if let Some((_, srv, vw, vh)) = &self.tex {
                // Contain-fit viewport: scale to the smaller axis, centre, letterbox the rest.
                let (ww, wh, vfw, vfh) = (
                    pw as f32,
                    ph as f32,
                    (*vw).max(1) as f32,
                    (*vh).max(1) as f32,
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
                c.PSSetShader(&self.ps, None);
                c.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
                c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
                c.Draw(3, 0);
            }
            let _ = self.swap.Present(1, DXGI_PRESENT(0));
        }
    }

    fn upload(&mut self, frame: &CpuFrame) -> Result<()> {
        let (w, h) = (frame.width, frame.height);
        let need_new = !matches!(&self.tex, Some((_, _, tw, th)) if *tw == w && *th == h);
        if need_new {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
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
            self.tex = Some((texture, srv, w, h));
        }
        let (texture, _, _, _) = self.tex.as_ref().unwrap();
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
                    frame.rgba.as_ptr().add(y * src_pitch),
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

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if r.is_ok() {
            let name = if driver == D3D_DRIVER_TYPE_HARDWARE {
                "hardware"
            } else {
                "WARP (software)"
            };
            tracing::info!(driver = name, "D3D11 device created");
            return Ok((device.unwrap(), context.unwrap()));
        }
    }
    Err(anyhow!(
        "D3D11CreateDevice failed for both hardware and WARP"
    ))
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
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
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
) -> Result<(ID3D11VertexShader, ID3D11PixelShader, ID3D11SamplerState)> {
    let vs_blob = compile(SHADER_HLSL, "vs_main", "vs_5_0")?;
    let ps_blob = compile(SHADER_HLSL, "ps_main", "ps_5_0")?;
    unsafe {
        let mut vs = None;
        device
            .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
            .context("CreateVertexShader")?;
        let mut ps = None;
        device
            .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))
            .context("CreatePixelShader")?;
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
        Ok((vs.unwrap(), ps.unwrap(), sampler.unwrap()))
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
