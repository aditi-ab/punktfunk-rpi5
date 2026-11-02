//! The CSC pass: `shaders/csc.hlsl` compiled once by `D3DCompile`, its pipeline objects, and
//! the draw of a frame's planes into a placed rect of the back buffer.

use anyhow::{anyhow, bail, Context as _, Result};
use pf_client_core::video::ColorDesc;
use punktfunk_core::video_fit::Placement;
use windows::core::{s, PCSTR};
use windows::Win32::d3d11::{
    ID3D11Buffer, ID3D11Device5, ID3D11DeviceContext4, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX, D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::d3dcommon::{ID3D10Blob, ID3DInclude, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::d3dcompiler::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::windef::RECT;

const SOURCE: &str = include_str!("shaders/csc.hlsl");

/// The shader slot of a pool slice's luma view; its chroma view follows (`csc.hlsl`).
const SLICE_SLOT: u32 = 3;

/// The constant buffer: `csc_rows`' rows, then `params` as `vk/present.rs::record_csc`
/// fills its push constants (x: mode, y: tonemap peak, zw: uv scale, display / coded).
#[repr(C)]
pub(super) struct Constants {
    rows: [[f32; 4]; 3],
    params: [f32; 4],
}

impl Constants {
    /// Mode 1 tonemaps PQ to SDR at the `tonemap` peak; mode 0 passes the transfer through.
    pub(super) fn new(rows: [[f32; 4]; 3], tonemap: Option<f32>, uv_scale: [f32; 2]) -> Constants {
        Constants {
            rows,
            params: [
                f32::from(u8::from(tonemap.is_some())),
                tonemap.unwrap_or(0.0),
                uv_scale[0],
                uv_scale[1],
            ],
        }
    }
}

pub(super) struct Csc {
    vs: ID3D11VertexShader,
    ps_planar: ID3D11PixelShader,
    ps_nv12: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    raster: ID3D11RasterizerState,
}

/// `entry` of the shader source for `target`, or the compiler's own message.
fn compile(entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut code: Option<ID3D10Blob> = None;
    let mut errors: Option<ID3D10Blob> = None;
    // SAFETY: `SOURCE` is a static string read for its length; the names are static C
    // strings; both out-params are locals the callee only writes.
    let hr = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("csc.hlsl"),
            None,
            None::<&ID3DInclude>,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3 as u32,
            0,
            &mut code,
            &mut errors,
        )
    };
    let text = |blob: &ID3D10Blob| {
        // SAFETY: the blob owns `GetBufferSize` bytes at `GetBufferPointer` while it lives,
        // and the slice is copied out before it drops.
        let bytes = unsafe {
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        };
        bytes.to_vec()
    };
    if hr.is_err() {
        let message = errors
            .as_ref()
            .map(|b| {
                String::from_utf8_lossy(&text(b))
                    .trim_end_matches('\0')
                    .to_owned()
            })
            .unwrap_or_default();
        bail!("compile the CSC shader: {hr} {message}");
    }
    let code = code.ok_or_else(|| anyhow!("D3DCompile returned no code"))?;
    Ok(text(&code))
}

impl Csc {
    pub(super) fn new(device: &ID3D11Device5) -> Result<Csc> {
        let vs_code = compile(s!("vs_fullscreen"), s!("vs_5_0"))?;
        let ps_code = compile(s!("ps_planar"), s!("ps_5_0"))?;
        let nv12_code = compile(s!("ps_nv12"), s!("ps_5_0"))?;
        let (mut vs, mut ps, mut nv12, mut constants, mut sampler, mut raster) =
            (None, None, None, None, None, None);
        let buffer = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<Constants>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER as u32,
            ..Default::default()
        };
        let linear = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: D3D11_FLOAT32_MAX,
            ..Default::default()
        };
        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ScissorEnable: true.into(),
            ..Default::default()
        };
        // SAFETY: COM calls on the live device over the compiled bytecode and the local
        // descriptors above; each out-param is a local `Option` checked below.
        unsafe {
            device
                .CreateVertexShader(&vs_code, None, Some(&mut vs))
                .ok()
                .context("create the CSC vertex shader")?;
            device
                .CreatePixelShader(&ps_code, None, Some(&mut ps))
                .ok()
                .context("create the CSC pixel shader")?;
            device
                .CreatePixelShader(&nv12_code, None, Some(&mut nv12))
                .ok()
                .context("create the two-plane CSC pixel shader")?;
            device
                .CreateBuffer(&buffer, None, Some(&mut constants))
                .ok()
                .context("create the CSC constant buffer")?;
            device
                .CreateSamplerState(&linear, Some(&mut sampler))
                .ok()
                .context("create the CSC sampler")?;
            device
                .CreateRasterizerState(&raster_desc, Some(&mut raster))
                .ok()
                .context("create the CSC rasterizer state")?;
        }
        let missing = || anyhow!("a D3D11 create returned no object");
        Ok(Csc {
            vs: vs.ok_or_else(missing)?,
            ps_planar: ps.ok_or_else(missing)?,
            ps_nv12: nv12.ok_or_else(missing)?,
            constants: constants.ok_or_else(missing)?,
            sampler: sampler.ok_or_else(missing)?,
            raster: raster.ok_or_else(missing)?,
        })
    }

    /// Draw three 8-bit planes through `ps_planar` into `place`'s rect of the bound target,
    /// tonemapping PQ to SDR at `tonemap` peak when set.
    pub(super) fn draw_planar(
        &self,
        context: &ID3D11DeviceContext4,
        planes: &[Option<ID3D11ShaderResourceView>; 3],
        frame: (u32, u32),
        place: &Placement,
        color: ColorDesc,
        tonemap: Option<f32>,
    ) {
        let rows = pf_client_core::video::csc_rows(color, 8, false);
        let constants = Constants::new(rows, tonemap, [1.0, 1.0]);
        self.draw(context, false, planes, frame, place, &constants);
    }

    /// Draw a pool slice's two plane views through `ps_nv12`. `frame` is the display size;
    /// the constants' uv scale crops the coded slice to it.
    pub(super) fn draw_nv12(
        &self,
        context: &ID3D11DeviceContext4,
        planes: &[Option<ID3D11ShaderResourceView>; 2],
        frame: (u32, u32),
        place: &Placement,
        constants: &Constants,
    ) {
        self.draw(context, true, planes, frame, place, constants);
    }

    /// One fullscreen-triangle draw over `planes`: `ps_nv12` from `SLICE_SLOT`, else
    /// `ps_planar` from slot 0. The viewport spans the whole placed frame (it hangs past the
    /// target under Crop); the scissor keeps the draw inside `dst`.
    fn draw(
        &self,
        context: &ID3D11DeviceContext4,
        nv12: bool,
        planes: &[Option<ID3D11ShaderResourceView>],
        frame: (u32, u32),
        place: &Placement,
        constants: &Constants,
    ) {
        let viewport = D3D11_VIEWPORT {
            TopLeftX: (f64::from(place.dst_x) - place.src_x * place.scale_x) as f32,
            TopLeftY: (f64::from(place.dst_y) - place.src_y * place.scale_y) as f32,
            Width: (f64::from(frame.0) * place.scale_x) as f32,
            Height: (f64::from(frame.1) * place.scale_y) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let scissor = RECT {
            left: place.dst_x as i32,
            top: place.dst_y as i32,
            right: (place.dst_x + place.dst_w) as i32,
            bottom: (place.dst_y + place.dst_h) as i32,
        };
        let (ps, first) = if nv12 {
            (&self.ps_nv12, SLICE_SLOT)
        } else {
            (&self.ps_planar, 0)
        };
        // SAFETY: COM calls on the live immediate context. `constants` is a `Constants`, exactly
        // the buffer's byte width; every bound object is owned by `self` or the caller and
        // outlives the draw, which the context references until it executes.
        unsafe {
            context.UpdateSubresource(
                &self.constants,
                0,
                None,
                std::ptr::from_ref(constants).cast(),
                0,
                0,
            );
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.IASetInputLayout(None);
            context.RSSetState(&self.raster);
            context.RSSetViewports(Some(&[viewport]));
            context.RSSetScissorRects(Some(&[scissor]));
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(ps, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.PSSetShaderResources(first, Some(planes));
            context.Draw(3, 0);
            // Unbind the planes so a resize or re-upload never meets them bound.
            context.PSSetShaderResources(first, Some(&[None, None, None][..planes.len()]));
        }
    }
}
