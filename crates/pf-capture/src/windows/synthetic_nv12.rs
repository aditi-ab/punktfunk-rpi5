//! A headless synthetic **NV12 D3D11** capture source for exercising the GPU encoders on Windows
//! without a real capture session.
//!
//! The native AMF path (and the D3D11 zero-copy NVENC/QSV paths) require an NV12 texture that lives
//! on the GPU — the CPU-Bgrx [`SyntheticCapturer`](crate::SyntheticCapturer) can't provide
//! one, and DXGI Desktop Duplication can't create one under an ssh session-0 (E_ACCESSDENIED). This
//! source builds an NV12 texture on the selected render adapter and fills it with a **moving** luma
//! ramp each frame, so the encoder sees genuine motion (P-frame residuals + the intra-refresh wave
//! under content change) — exactly what an intra-refresh recovery validation needs. Driven by
//! `spike --source synthetic-nv12`.

use crate::dxgi::{make_device, D3d11Frame};
use crate::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

/// Synthetic NV12 frames on the GPU. Owns its own D3D11 device + immediate context and two NV12
/// textures: a CPU-writable STAGING scratch it fills each frame, and a DEFAULT texture it copies
/// into and hands to the encoder. The encoder copies out of the DEFAULT texture synchronously
/// (spike drives capture→submit→poll on one thread), so reusing one DEFAULT texture is sound.
pub struct SyntheticNv12Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    default_tex: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
}

// SAFETY: mirrors `D3d11Frame`'s reasoning — the device is created free-threaded (`make_device`
// passes no `SINGLETHREADED` flag) and D3D11 uses interlocked COM refcounting, so moving the whole
// capturer (device + immediate context + textures) to its owning thread and using it only there is
// sound. The value is moved, never aliased (no `Sync`), so the single-threaded immediate context is
// never touched concurrently.
unsafe impl Send for SyntheticNv12Capturer {}

impl SyntheticNv12Capturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        // NV12 is 4:2:0 — both dimensions must be even (the chroma plane is width/2 × height/2).
        let width = (width & !1).max(2);
        let height = (height & !1).max(2);
        // SAFETY: a self-contained builder owning every handle it creates; each COM call is checked
        // and the returned owners drop with their wrappers.
        unsafe {
            let adapter =
                resolve_render_adapter().context("resolve render adapter for NV12 source")?;
            let (device, context) = make_device(&adapter).context("create D3D11 device")?;
            let default_tex = create_nv12(
                &device,
                width,
                height,
                D3D11_USAGE_DEFAULT,
                0,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
            )
            .context("create NV12 default texture")?;
            let staging = create_nv12(
                &device,
                width,
                height,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
                0,
            )
            .context("create NV12 staging texture")?;
            Ok(SyntheticNv12Capturer {
                device,
                context,
                default_tex,
                staging,
                width,
                height,
                fps,
                frame_idx: 0,
            })
        }
    }
}

impl Capturer for SyntheticNv12Capturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps.max(1) as u64;
        // SAFETY: Map/Unmap/CopyResource on this capturer's own single-threaded immediate context;
        // all writes stay within the mapped NV12 surface (Y: H rows of RowPitch; UV: H/2 rows of
        // RowPitch beginning at RowPitch*H — the standard NV12 plane layout).
        unsafe {
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut map))
                .context("Map(NV12 staging)")?;
            let pitch = map.RowPitch as usize;
            let base = map.pData as *mut u8;
            // A diagonal luma ramp that shifts 4 codes/frame — strong, deterministic motion.
            let shift = (self.frame_idx as u32).wrapping_mul(4);
            for y in 0..self.height {
                let row = base.add(y as usize * pitch);
                for x in 0..self.width {
                    *row.add(x as usize) = x.wrapping_add(y).wrapping_add(shift) as u8;
                }
            }
            // UV plane (neutral gray = 128) at offset RowPitch*H: H/2 rows, `width` bytes each
            // (width/2 interleaved Cb,Cr pairs).
            let uv = base.add(pitch * self.height as usize);
            for r in 0..(self.height / 2) {
                let row = uv.add(r as usize * pitch);
                for c in 0..self.width {
                    *row.add(c as usize) = 128;
                }
            }
            self.context.Unmap(&self.staging, 0);
            self.context.CopyResource(&self.default_tex, &self.staging);
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: self.default_tex.clone(),
                device: self.device.clone(),
            }),
            cursor: None,
        })
    }
}

/// Resolve the same render adapter the encoder will pick (`PUNKTFUNK_RENDER_ADAPTER` / preference /
/// max-VRAM LUID), falling back to adapter 0.
///
/// # Safety
/// Calls DXGI factory/adapter enumeration; returns owned COM objects or an error.
unsafe fn resolve_render_adapter() -> Result<IDXGIAdapter1> {
    let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
    if let Some(luid) = pf_gpu::resolve_render_adapter_luid() {
        if let Ok(a) = factory.EnumAdapterByLuid::<IDXGIAdapter1>(luid) {
            return Ok(a);
        }
    }
    factory.EnumAdapters1(0).context("EnumAdapters1(0)")
}

/// Create an NV12 `Texture2D` with the given usage/CPU-access/bind flags.
///
/// # Safety
/// `device` must be a live D3D11 device; the returned texture is owned by the caller.
unsafe fn create_nv12(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    cpu_access: u32,
    bind: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        ..Default::default()
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    device
        .CreateTexture2D(&desc, None, Some(&mut tex))
        .context("CreateTexture2D(NV12)")?;
    tex.context("CreateTexture2D returned a null NV12 texture")
}
