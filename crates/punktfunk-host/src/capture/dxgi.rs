//! DXGI Desktop Duplication capture (Windows) — the analogue of the PipeWire portal capturer.
//! Creates a D3D11 device on the SudoVDA adapter (by LUID), finds the matching output (by GDI
//! name), duplicates it, and on each `AcquireNextFrame` copies the desktop image into a CPU-readable
//! staging texture → tightly-packed BGRA (the GPU-less path that feeds the software encoder). A
//! future zero-copy path returns `FramePayload::D3d11` for NVENC.
//!
//! Validates only with a real GPU + an *activated* SudoVDA monitor (`DuplicateOutput` needs a live
//! WDDM output). Compiles on the GPU-less VM; the pure helpers are unit-tested there.

use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{anyhow, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_FLAG,
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC,
    DXGI_OUTDUPL_FRAME_INFO,
};

/// The Windows capture identity carried out of the SudoVDA backend in
/// [`crate::vdisplay::VirtualOutput`]: which adapter + which GDI output to duplicate.
#[derive(Clone, Debug)]
pub struct WinCaptureTarget {
    /// Packed DXGI adapter LUID (`(HighPart << 32) | (LowPart & 0xffff_ffff)`).
    pub adapter_luid: i64,
    /// The output's GDI device name, e.g. `\\.\DISPLAY3`.
    pub gdi_name: String,
}

/// A GPU-resident captured texture (future NVENC-D3D11 zero-copy path).
pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
}
// COM pointers, used only from the single owning thread.
unsafe impl Send for D3d11Frame {}

pub fn pack_luid(luid: LUID) -> i64 {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64 & 0xffff_ffff)
}

/// Does a fixed-size UTF-16 GDI device name (NUL-padded, e.g. `DXGI_OUTPUT_DESC::DeviceName`)
/// equal `target`?
fn gdi_name_matches(name16: &[u16], target: &str) -> bool {
    let s = String::from_utf16_lossy(name16);
    s.trim_end_matches('\u{0}') == target
}

/// Copy a row-padded BGRA surface (`pitch` >= `w*4`) into a tightly-packed `w*4*h` buffer.
fn depad_bgra(src: &[u8], pitch: usize, w: usize, h: usize) -> Vec<u8> {
    let row = w * 4;
    let mut out = vec![0u8; row * h];
    for y in 0..h {
        out[y * row..y * row + row].copy_from_slice(&src[y * pitch..y * pitch + row]);
    }
    out
}

pub struct DuplCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dupl: IDXGIOutputDuplication,
    width: u32,
    height: u32,
    refresh_hz: u32,
    staging: Option<ID3D11Texture2D>,
    holding_frame: bool,
    active: AtomicBool,
    timeout_ms: u32,
    last: Option<Vec<u8>>,
    /// GPU-output mode (zero-copy → NVENC): produce `FramePayload::D3d11` instead of CPU BGRA.
    /// Selected by `PUNKTFUNK_ENCODER=nvenc` so the capturer's output matches the encoder's input.
    gpu_mode: bool,
    /// Reused owned texture the duplication frame is copied into for the D3D11 path (the duplication
    /// surface is transient and released each frame).
    gpu_copy: Option<ID3D11Texture2D>,
    have_gpu_frame: bool,
    _keepalive: Box<dyn Send>,
}
// COM objects used only from the one thread that owns the capturer (the encode thread).
unsafe impl Send for DuplCapturer {}

impl DuplCapturer {
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
    ) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            // 1) the adapter whose LUID matches SudoVDA's AddOut.luid.
            let mut adapter: Option<IDXGIAdapter1> = None;
            let mut i = 0u32;
            while let Ok(a) = factory.EnumAdapters1(i) {
                let d = a.GetDesc1()?;
                if pack_luid(d.AdapterLuid) == target.adapter_luid {
                    adapter = Some(a);
                    break;
                }
                i += 1;
            }
            let adapter = adapter.context("no DXGI adapter matches the SudoVDA LUID")?;
            // 2) D3D11 device ON that adapter (driver_type MUST be UNKNOWN with an explicit adapter).
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("null D3D11 device")?;
            let context = context.context("null D3D11 context")?;
            // 3) the output (monitor) whose GDI DeviceName matches.
            let mut out1: Option<IDXGIOutput1> = None;
            let mut j = 0u32;
            while let Ok(o) = adapter.EnumOutputs(j) {
                let od = o.GetDesc()?;
                if gdi_name_matches(&od.DeviceName, &target.gdi_name) {
                    out1 = Some(o.cast::<IDXGIOutput1>()?);
                    break;
                }
                j += 1;
            }
            let output = out1
                .with_context(|| format!("adapter has no output named {}", target.gdi_name))?;
            // 4) duplicate the output.
            let dupl = output
                .DuplicateOutput(&device)
                .context("DuplicateOutput (already duplicated by another app?)")?;
            let dd: DXGI_OUTDUPL_DESC = dupl.GetDesc();
            let (width, height) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let refresh_hz = preferred
                .map(|(_, _, hz)| hz)
                .filter(|&hz| hz > 0)
                .unwrap_or_else(|| {
                    let r = dd.ModeDesc.RefreshRate;
                    if r.Denominator > 0 {
                        (r.Numerator / r.Denominator).max(1)
                    } else {
                        60
                    }
                });
            let timeout_ms = std::env::var("PUNKTFUNK_CAPTURE_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or((2000 / refresh_hz.max(1)).max(100));
            let gpu_mode = std::env::var("PUNKTFUNK_ENCODER")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "nvenc" | "hw" | "nvidia"))
                .unwrap_or(false);
            tracing::info!(
                "DXGI duplication: {}x{}@{} on {} ({})",
                width,
                height,
                refresh_hz,
                target.gdi_name,
                if gpu_mode { "D3D11 zero-copy" } else { "CPU staging" }
            );
            Ok(Self {
                device,
                context,
                output,
                dupl,
                width,
                height,
                refresh_hz,
                staging: None,
                holding_frame: false,
                active: AtomicBool::new(false),
                timeout_ms,
                last: None,
                gpu_mode,
                gpu_copy: None,
                have_gpu_frame: false,
                _keepalive: keepalive,
            })
        }
    }

    unsafe fn ensure_staging(&mut self) -> Result<()> {
        if self.staging.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: D3D11_BIND_FLAG(0).0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(staging)")?;
        self.staging = t;
        Ok(())
    }

    unsafe fn ensure_gpu_copy(&mut self) -> Result<()> {
        if self.gpu_copy.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(gpu copy)")?;
        self.gpu_copy = t;
        Ok(())
    }

    unsafe fn recreate_dupl(&mut self) -> Result<()> {
        if self.holding_frame {
            let _ = self.dupl.ReleaseFrame();
            self.holding_frame = false;
        }
        self.dupl = self
            .output
            .DuplicateOutput(&self.device)
            .context("re-DuplicateOutput after ACCESS_LOST")?;
        Ok(())
    }

    /// Acquire one frame: `Some` on a fresh image, `None` on timeout (no change → caller reuses last).
    unsafe fn acquire(&mut self) -> Result<Option<CapturedFrame>> {
        if self.holding_frame {
            let _ = self.dupl.ReleaseFrame();
            self.holding_frame = false;
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match self.dupl.AcquireNextFrame(self.timeout_ms, &mut info, &mut res) {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.recreate_dupl()?;
                return Ok(None);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        self.holding_frame = true;
        let res = res.context("AcquireNextFrame: null resource")?;
        let tex: ID3D11Texture2D = res.cast().context("resource -> Texture2D")?;
        if self.gpu_mode {
            // Zero-copy path: keep the frame on the GPU for NVENC. Copy the transient duplication
            // surface into a reused owned texture, release the duplication frame, hand off the texture.
            self.ensure_gpu_copy()?;
            let gpu = self.gpu_copy.clone().context("gpu copy texture")?;
            self.context.CopyResource(&gpu, &tex);
            let _ = self.dupl.ReleaseFrame();
            self.holding_frame = false;
            self.have_gpu_frame = true;
            return Ok(Some(CapturedFrame {
                width: self.width,
                height: self.height,
                pts_ns: now_ns(),
                format: PixelFormat::Bgra,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: gpu,
                    device: self.device.clone(),
                }),
            }));
        }
        self.ensure_staging()?;
        let staging = self.staging.clone().context("staging texture")?;
        self.context.CopyResource(&staging, &tex);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
            .context("Map staging")?;
        let (w, h) = (self.width as usize, self.height as usize);
        let pitch = map.RowPitch as usize;
        let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * h);
        let tight = depad_bgra(src, pitch, w, h);
        self.context.Unmap(&staging, 0);
        let _ = self.dupl.ReleaseFrame();
        self.holding_frame = false;
        self.last = Some(tight.clone());
        Ok(Some(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: PixelFormat::Bgra,
            payload: FramePayload::Cpu(tight),
        }))
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl Capturer for DuplCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(f) = unsafe { self.acquire() }? {
                return Ok(f);
            }
            if self.gpu_mode && self.have_gpu_frame {
                if let Some(gpu) = &self.gpu_copy {
                    return Ok(CapturedFrame {
                        width: self.width,
                        height: self.height,
                        pts_ns: now_ns(),
                        format: PixelFormat::Bgra,
                        payload: FramePayload::D3d11(D3d11Frame {
                            texture: gpu.clone(),
                            device: self.device.clone(),
                        }),
                    });
                }
            }
            if let Some(b) = &self.last {
                return Ok(CapturedFrame {
                    width: self.width,
                    height: self.height,
                    pts_ns: now_ns(),
                    format: PixelFormat::Bgra,
                    payload: FramePayload::Cpu(b.clone()),
                });
            }
            if Instant::now() > deadline {
                return Err(anyhow!(
                    "no DXGI frame within 10s (SudoVDA monitor not activated by a WDDM GPU?)"
                ));
            }
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        unsafe { self.acquire() }
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

impl Drop for DuplCapturer {
    fn drop(&mut self) {
        if self.holding_frame {
            unsafe {
                let _ = self.dupl.ReleaseFrame();
            }
        }
        // _keepalive drops after, REMOVEing the SudoVDA monitor.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_luid_roundtrip() {
        let l = LUID {
            LowPart: 0x1234_5678,
            HighPart: 0x0000_0009,
        };
        assert_eq!(pack_luid(l), (0x9i64 << 32) | 0x1234_5678);
    }

    #[test]
    fn gdi_name_match() {
        let mut buf = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY3".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert!(gdi_name_matches(&buf, r"\\.\DISPLAY3"));
        assert!(!gdi_name_matches(&buf, r"\\.\DISPLAY1"));
    }

    #[test]
    fn depad_removes_row_padding() {
        // 2x2 BGRA, pitch = 12 (row=8 + 4 pad bytes).
        let pitch = 12;
        let mut src = vec![0u8; pitch * 2];
        for y in 0..2 {
            for x in 0..8 {
                src[y * pitch + x] = (y * 8 + x) as u8;
            }
        }
        let out = depad_bgra(&src, pitch, 2, 2);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(&out[8..16], &[8, 9, 10, 11, 12, 13, 14, 15]);
    }
}
