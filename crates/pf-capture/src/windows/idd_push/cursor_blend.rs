//! Host-side cursor compositing for the CAPTURE mouse model (design/remote-desktop-sweep.md §8).
//!
//! Why the host draws it: once a monitor has ever declared an IddCx hardware cursor, DWM will
//! not composite the software cursor back into its frames — there is no un-declare DDI (the
//! empty-caps re-setup is rejected `STATUS_INVALID_PARAMETER`), and a successful same-mode
//! re-commit with the driver's re-declare provably suppressed still leaves the pointer excluded
//! (all observed on-glass, 26100). So the driver keeps its hardware cursor declared for the
//! session's whole life — the state that works — and when the client flips to the capture model
//! the HOST composites the pointer into the frame itself: a slot→scratch copy plus one
//! alpha-blended quad (the GDI poller's full-fidelity shape at its polled position), entirely
//! GPU-side on the capture device, before the normal conversion runs from the scratch.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::*;
use windows::core::s;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11PixelShader, ID3D11SamplerState, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE,
    D3D11_BLEND_OP_ADD, D3D11_BLEND_SRC_ALPHA, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_CPU_ACCESS_WRITE, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_WRITE_DISCARD, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

/// Straight-alpha sample of the cursor bitmap. `linear_scale` = 0 passes sRGB through (SDR
/// ring); non-zero linearizes sRGB→scRGB AND multiplies by the target's SDR-white scale
/// (`sdr_white_level_scale` — 1.0 would put cursor-white at 80 nits, visibly DARKER than the
/// surrounding SDR desktop content DWM composes at the user's SDR-brightness setting).
const CURSOR_PS: &str = r"
Texture2D<float4> tx : register(t0);
SamplerState sm : register(s0);
cbuffer C : register(b0) { float linear_scale; float3 pad; };
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_Target {
    float4 c = tx.Sample(sm, uv);
    if (linear_scale != 0.0) {
        c.rgb = pow(abs(c.rgb), 2.2) * linear_scale;
    }
    return c;
}
";

/// The cursor-quad blend pass + its shape-texture cache. One per capturer (device-scoped).
pub(super) struct CursorBlendPass {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    cbuf: ID3D11Buffer,
    cbuf_scale: Option<f32>,
    /// The uploaded shape (serial-keyed): SRV + dims in host pixels.
    shape: Option<(u64, ID3D11ShaderResourceView, u32, u32)>,
}

impl CursorBlendPass {
    pub(super) unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        // SAFETY: `?`-checked D3D11 resource creation on the live `device` borrow, over
        // fully-initialized stack descriptors and live out-params; `compile_shader` receives `s!()`
        // literals (its contract).
        unsafe {
            let vsb = crate::dxgi::compile_shader(crate::dxgi::HDR_VS, s!("main"), s!("vs_5_0"))?;
            let psb = crate::dxgi::compile_shader(CURSOR_PS, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(&psb, None, Some(&mut ps))?;
            let sd = D3D11_SAMPLER_DESC {
                // LINEAR: the quad is drawn 1:1 in frame pixels, so this only matters at the
                // half-texel edges; linear keeps them soft instead of ringing.
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            // Straight-alpha over: dst.rgb = src.rgb*a + dst.rgb*(1-a); keep dst alpha.
            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_SRC_ALPHA,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_ONE,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: 0x0F,
            };
            let mut blend = None;
            device.CreateBlendState(&bd, Some(&mut blend))?;
            let cbd = D3D11_BUFFER_DESC {
                ByteWidth: 16, // float to_linear + float3 pad
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut cbuf = None;
            device.CreateBuffer(&cbd, None, Some(&mut cbuf))?;
            Ok(Self {
                vs: vs.context("cursor blend vs")?,
                ps: ps.context("cursor blend ps")?,
                sampler: sampler.context("cursor blend sampler")?,
                blend: blend.context("cursor blend state")?,
                cbuf: cbuf.context("cursor blend cbuf")?,
                cbuf_scale: None,
                shape: None,
            })
        }
    }

    /// Upload `ov`'s bitmap if its serial is new; reuse the cached SRV otherwise.
    unsafe fn ensure_shape(
        &mut self,
        device: &ID3D11Device,
        ov: &pf_frame::CursorOverlay,
    ) -> Result<()> {
        // SAFETY: `CreateTexture2D`/`CreateShaderResourceView` are `?`-checked calls on the live
        // `device` borrow. `init.pSysMem` points into `ov.rgba`, which the length check above proves
        // holds at least `ov.w * ov.h * 4` bytes for the declared `SysMemPitch = ov.w * 4`, and which
        // outlives the synchronous upload.
        unsafe {
            if self.shape.as_ref().is_some_and(|(s, ..)| *s == ov.serial) {
                return Ok(());
            }
            if ov.rgba.len() < (ov.w as usize) * (ov.h as usize) * 4 || ov.w == 0 || ov.h == 0 {
                bail!("malformed cursor overlay ({}x{})", ov.w, ov.h);
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: ov.w,
                Height: ov.h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: ov.rgba.as_ptr().cast(),
                SysMemPitch: ov.w * 4,
                SysMemSlicePitch: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .context("CreateTexture2D(cursor shape)")?;
            let tex = tex.context("null cursor shape texture")?;
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))
                .context("CreateShaderResourceView(cursor shape)")?;
            self.shape = Some((ov.serial, srv.context("null cursor shape srv")?, ov.w, ov.h));
            Ok(())
        }
    }

    /// Alpha-blend `ov` onto `dst` (frame-sized, RENDER_TARGET-capable — the blend scratch).
    /// `linear_scale`: 0 = SDR passthrough; non-zero = the frame is FP16 scRGB (HDR
    /// composition) — linearize and scale to the target's SDR white. The quad is placed purely
    /// via the viewport (the fullscreen-triangle VS fills whatever viewport is set), clipped by
    /// the target automatically.
    pub(super) unsafe fn blend(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        dst: &ID3D11Texture2D,
        ov: &pf_frame::CursorOverlay,
        linear_scale: f32,
    ) -> Result<()> {
        // SAFETY: all D3D11 work on the caller's live `device`/`ctx` borrows. The
        // `copy_nonoverlapping` writes `cb.len()` `f32`s into the pointer the immediately preceding
        // `Map` of `self.cbuf` (16 bytes = 4×`f32`, DYNAMIC/WRITE_DISCARD) returned, inside the
        // `is_ok()` arm and before the paired `Unmap`. `ensure_shape` forwards this fn's `device`
        // borrow, and every `*Set*`/`Draw` takes borrowed slices of live locals or clones of live COM
        // interfaces.
        unsafe {
            self.ensure_shape(device, ov)?;
            let (_, srv, w, h) = self.shape.as_ref().expect("shape just ensured");
            if self.cbuf_scale != Some(linear_scale) {
                let cb: [f32; 4] = [linear_scale, 0.0, 0.0, 0.0];
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                if ctx
                    .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                    .is_ok()
                {
                    std::ptr::copy_nonoverlapping(cb.as_ptr(), mapped.pData as *mut f32, cb.len());
                    ctx.Unmap(&self.cbuf, 0);
                    // Cache ONLY on a successful upload. Caching unconditionally meant one transient
                    // `Map` failure wedged the HDR/SDR cursor scale for the rest of the session: the
                    // buffer still held the OLD value while this believed it held the new one, and
                    // no later call would retry. On failure the cache is left alone and the next
                    // blend tries again — a stale scale for a frame instead of forever.
                    self.cbuf_scale = Some(linear_scale);
                }
            }
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(dst, None, Some(&mut rtv))
                .context("CreateRenderTargetView(cursor blend scratch)")?;
            let rtv = rtv.context("null cursor blend rtv")?;

            ctx.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            // Placement IS the viewport: the VS fills it, the OS clips it to the target.
            let vp = D3D11_VIEWPORT {
                TopLeftX: ov.x as f32,
                TopLeftY: ov.y as f32,
                Width: *w as f32,
                Height: *h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.Draw(3, 0);
            // Unbind so the scratch can be bound as a conversion INPUT without a hazard warning.
            ctx.OMSetRenderTargets(None, None);
            let none_srv: [Option<ID3D11ShaderResourceView>; 1] = [None];
            ctx.PSSetShaderResources(0, Some(&none_srv));
            Ok(())
        }
    }
}
