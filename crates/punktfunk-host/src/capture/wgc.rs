//! Windows.Graphics.Capture (WGC) capture backend — the HDR/animation-correct path.
//!
//! Why WGC over DXGI Desktop Duplication: DDA duplicates only the DWM-composed primary surface, so
//! HDR desktop animations the OS routes onto hardware overlay / independent-flip / MPO planes (Start
//! menu, Win11 Mica/acrylic, window resize) never enter the surface DDA reads — the stream shows a
//! frozen desktop ("broken HDR animations"). Engaging WGC capture pulls that content back through DWM
//! composition, so the surface WGC hands back contains the animations. WGC also has no
//! ACCESS_LOST-on-overlay-flip churn.
//!
//! It reuses the rest of the pipeline UNCHANGED: the frame's GPU texture (the OS already composited
//! the cursor into it — `IsCursorCaptureEnabled(true)`) goes through the same scRGB→BT.2020-PQ shader
//! ([`super::dxgi::HdrConverter`]) into a host-owned `R10G10B10A2` texture (HDR) or is copied into a
//! BGRA texture (SDR), which is handed to NVENC zero-copy (registered by pointer, encoded in place).
//! Shares the D3D11 device with NVENC via `FramePayload::D3d11`.
//!
//! Limitation: WGC cannot capture the secure desktop (lock / UAC / login) — the caller falls back to
//! the DDA backend ([`super::dxgi::DuplCapturer`]) for those (see capture.rs).

use super::dxgi::{
    find_output, make_device, nudge_cursor_onto, D3d11Frame, HdrConverter, WinCaptureTarget,
};
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::{IInspectable, Interface};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIOutput6};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// The host runs as SYSTEM (so the DDA secure-desktop path works), but WGC will NOT activate under
/// the SYSTEM account (`CreateForMonitor` → 0x80070424). Impersonate the interactive console user
/// for the WGC activation. Returns the user token (the caller reverts + closes it after activation)
/// or `None` (no active user, or the host already runs AS the user — WTSQueryUserToken then fails and
/// WGC works without impersonation). SYSTEM-only; harmless under a user-token host.
unsafe fn impersonate_active_user() -> Option<HANDLE> {
    let session = WTSGetActiveConsoleSessionId();
    if session == 0xFFFF_FFFF {
        return None;
    }
    let mut token = HANDLE::default();
    if WTSQueryUserToken(session, &mut token).is_ok() {
        if ImpersonateLoggedOnUser(token).is_ok() {
            return Some(token);
        }
        let _ = CloseHandle(token);
    }
    None
}

/// RAII: reverts the WGC-activation impersonation when it drops (covers every `?` early-return).
struct Deimpersonate(Option<HANDLE>);
impl Drop for Deimpersonate {
    fn drop(&mut self) {
        if let Some(tok) = self.0.take() {
            unsafe {
                let _ = RevertToSelf();
                let _ = CloseHandle(tok);
            }
        }
    }
}

/// Signal from the free-threaded FrameArrived callback to the encode thread: a monotonically
/// increasing count of arrived frames + a condvar to wake `next_frame`. The encode thread tracks how
/// many it has consumed; `TryGetNextFrame` is called exactly `available - consumed` times so we never
/// hit the empty-pool ambiguity, and draining to the newest keeps latency at one frame.
struct WgcSignal {
    available: AtomicU64,
    mtx: Mutex<()>,
    cv: Condvar,
}

pub struct WgcCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    // WGC objects — kept alive for the session's lifetime.
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    _frame_arrived_token: i64,
    signal: Arc<WgcSignal>,
    consumed: u64,

    width: u32,
    height: u32,
    timeout_ms: u64,
    first_frame: bool,

    hdr: bool,
    hdr_conv: Option<HdrConverter>,
    fp16_src: Option<ID3D11Texture2D>,
    fp16_srv: Option<ID3D11ShaderResourceView>,
    hdr10_out: Option<ID3D11Texture2D>,
    bgra_copy: Option<ID3D11Texture2D>,
    /// Last presentable GPU texture + format, repeated when no new frame arrived (static desktop).
    last_present: Option<(ID3D11Texture2D, PixelFormat)>,

    /// Owns the SudoVDA keepalive once attached (after WGC is confirmed open) — dropping the capturer
    /// then REMOVEs the virtual output. `None` between open and attach so a WGC-open failure leaves
    /// the keepalive with the caller for the DDA fallback.
    _keepalive: Option<Box<dyn Send>>,
}

// COM + WinRT pointers; confined to the single owning (encode) thread, like DuplCapturer.
unsafe impl Send for WgcCapturer {}

impl WgcCapturer {
    /// Open WGC capture. Does NOT take the keepalive — the caller attaches it via
    /// [`attach_keepalive`](Self::attach_keepalive) only after open succeeds, so a failure leaves the
    /// keepalive with the caller to hand to the DDA fallback.
    pub fn open(target: WinCaptureTarget, preferred: Option<(u32, u32, u32)>) -> Result<Self> {
        unsafe {
            // WGC is WinRT — the calling thread needs a COM/WinRT apartment for the GraphicsCaptureItem
            // activation factory (RoGetActivationFactory). Initialize MTA; ignore "already initialized"
            // / "changed mode" (another component on this thread may have init'd a compatible apartment).
            let ro = RoInitialize(RO_INIT_MULTITHREADED);
            // Impersonate the interactive user for the duration of WGC activation (host runs as
            // SYSTEM; WGC won't activate under SYSTEM). Reverted by the guard's Drop on return. The
            // WGC objects, once created, are accessed from the (SYSTEM) encode thread thereafter.
            let imp = impersonate_active_user();
            let _deimp = Deimpersonate(imp);
            tracing::info!(ro_result = ?ro, impersonated = imp.is_some(), "WGC: RoInitialize(MTA)");
            // The SudoVDA output appears a beat after the display is created — settle-retry like DDA.
            let deadline = Instant::now() + Duration::from_millis(2000);
            let (adapter, output) = loop {
                if let Some(n) = crate::vdisplay::sudovda::resolve_gdi_name(target.target_id) {
                    if let Ok(found) = find_output(&n) {
                        break found;
                    }
                }
                if let Ok(found) = find_output(&target.gdi_name) {
                    break found;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "WGC: no DXGI output for SudoVDA target {} yet",
                        target.target_id
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            };

            let (device, context) = make_device(&adapter)?;
            let od = output.GetDesc().context("output GetDesc")?;
            let hmonitor = od.Monitor;

            // HDR iff the output's colour space is BT.2020 PQ (G2084) — matches the DDA FP16 detection.
            let hdr = output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|o6| o6.GetDesc1().ok())
                .map(|d1| d1.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                .unwrap_or(false);

            // Wrap our D3D11 device as a WinRT IDirect3DDevice so the frame pool allocates on it (the
            // pool textures land on our device → CopyResource + NVENC are same-device, no readback).
            let dxgi_device: IDXGIDevice = device.cast().context("ID3D11Device as IDXGIDevice")?;
            let inspectable: IInspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .context("CreateDirect3D11DeviceFromDXGIDevice")?;
            let d3d_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = inspectable
                .cast()
                .context("IInspectable as IDirect3DDevice")?;

            tracing::info!(hdr, "WGC: device ready, creating capture item");
            // GraphicsCaptureItem for the monitor (the SudoVDA output enumerates as a normal monitor).
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .context("GraphicsCaptureItem interop factory")?;
            let item: GraphicsCaptureItem = interop
                .CreateForMonitor(hmonitor)
                .context("CreateForMonitor")?;
            let size = item.Size().context("item Size")?;
            let (width, height) = (size.Width.max(0) as u32, size.Height.max(0) as u32);
            tracing::info!(
                width,
                height,
                "WGC: capture item created, creating frame pool"
            );

            let pixel_format = if hdr {
                DirectXPixelFormat::R16G16B16A16Float // scRGB FP16 — same surface DDA gives on HDR
            } else {
                DirectXPixelFormat::B8G8R8A8UIntNormalized
            };
            // ≥3 buffers for 240 Hz headroom (avoid the producer waiting on a free buffer).
            let pool =
                Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d_device, pixel_format, 3, size)
                    .context("CreateFreeThreaded frame pool")?;

            let signal = Arc::new(WgcSignal {
                available: AtomicU64::new(0),
                mtx: Mutex::new(()),
                cv: Condvar::new(),
            });
            let sig = signal.clone();
            let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |_pool, _arg| {
                    sig.available.fetch_add(1, Ordering::Release);
                    sig.cv.notify_one();
                    Ok(())
                },
            );
            let token = pool.FrameArrived(&handler).context("FrameArrived")?;

            tracing::info!("WGC: creating capture session");
            let session = pool
                .CreateCaptureSession(&item)
                .context("CreateCaptureSession")?;
            // OS composites the cursor into the frame (HDR-correct, no manual composite pass).
            let _ = session.SetIsCursorCaptureEnabled(true);
            // Drop the yellow capture border (best-effort — older builds reject it).
            let _ = session.SetIsBorderRequired(false);
            // Lift the 60 Hz cap: allow up to the client's refresh (Win11 24H2+; below that this is a
            // no-op and WGC caps ~60). 100 ns ticks per frame.
            let refresh = preferred
                .map(|(_, _, hz)| hz)
                .filter(|&hz| hz > 0)
                .unwrap_or(60);
            let ticks = (10_000_000i64 / refresh.max(1) as i64).max(1);
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: ticks });
            tracing::info!("WGC: StartCapture");
            session.StartCapture().context("StartCapture")?;
            // WGC fires FrameArrived on CHANGE; a static desktop may never deliver the first frame
            // (→ black, then the next_frame deadline ends the session). Nudge the cursor onto the
            // output to force the first composition change, exactly like the DDA path does.
            nudge_cursor_onto(&output);

            let timeout_ms = (2000 / refresh.max(1) as u64).max(8);
            tracing::info!(
                width,
                height,
                hdr,
                refresh,
                "WGC capture started ({})",
                if hdr {
                    "HDR FP16→BT.2020 PQ"
                } else {
                    "SDR BGRA"
                }
            );

            Ok(Self {
                device,
                context,
                pool,
                session,
                _item: item,
                _frame_arrived_token: token,
                signal,
                consumed: 0,
                width,
                height,
                timeout_ms,
                first_frame: true,
                hdr,
                hdr_conv: None,
                fp16_src: None,
                fp16_srv: None,
                hdr10_out: None,
                bgra_copy: None,
                last_present: None,
                _keepalive: None,
            })
        }
    }

    /// Take ownership of the SudoVDA keepalive once the WGC session is confirmed open.
    pub fn attach_keepalive(&mut self, keepalive: Box<dyn Send>) {
        self._keepalive = Some(keepalive);
    }

    /// Block until a new frame arrives (cv), then drain `TryGetNextFrame` to the NEWEST queued frame
    /// (skip stale → one-frame latency). Returns `None` on timeout (no new frame → caller repeats).
    fn wait_and_drain(&mut self) -> Option<Direct3D11CaptureFrame> {
        let wait_ms = if self.first_frame {
            2000
        } else {
            self.timeout_ms
        };
        {
            let mut g = self.signal.mtx.lock().unwrap();
            while self.signal.available.load(Ordering::Acquire) <= self.consumed {
                let (ng, res) = self
                    .signal
                    .cv
                    .wait_timeout(g, Duration::from_millis(wait_ms))
                    .unwrap();
                g = ng;
                if res.timed_out() {
                    return None;
                }
            }
        }
        let target = self.signal.available.load(Ordering::Acquire);
        let mut last = None;
        while self.consumed < target {
            if let Ok(f) = self.pool.TryGetNextFrame() {
                last = Some(f);
            }
            self.consumed += 1;
        }
        last
    }

    unsafe fn ensure_fp16_src(&mut self) -> Result<()> {
        if self.fp16_src.is_some() {
            return Ok(());
        }
        let desc = tex_desc(
            self.width,
            self.height,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        );
        let mut t = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(wgc fp16 src)")?;
        let t = t.context("fp16 src")?;
        let mut srv = None;
        self.device
            .CreateShaderResourceView(&t, None, Some(&mut srv))?;
        self.fp16_srv = Some(srv.context("fp16 srv")?);
        self.fp16_src = Some(t);
        Ok(())
    }

    unsafe fn ensure_hdr10_out(&mut self) -> Result<()> {
        if self.hdr10_out.is_none() {
            let desc = tex_desc(
                self.width,
                self.height,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                D3D11_BIND_RENDER_TARGET.0 as u32,
            );
            let mut t = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D(wgc hdr10 out)")?;
            self.hdr10_out = t;
        }
        Ok(())
    }

    unsafe fn ensure_bgra(&mut self) -> Result<()> {
        if self.bgra_copy.is_none() {
            let desc = tex_desc(
                self.width,
                self.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_RENDER_TARGET.0 as u32,
            );
            let mut t = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D(wgc bgra)")?;
            self.bgra_copy = t;
        }
        Ok(())
    }

    fn process_frame(&mut self, frame: Direct3D11CaptureFrame) -> Result<CapturedFrame> {
        unsafe {
            let surface = frame.Surface().context("frame Surface")?;
            let access: IDirect3DDxgiInterfaceAccess = surface
                .cast()
                .context("surface as IDirect3DDxgiInterfaceAccess")?;
            let src: ID3D11Texture2D = access
                .GetInterface()
                .context("GetInterface ID3D11Texture2D")?;

            if self.hdr {
                // FP16 (cursor already composited by the OS) → BT.2020 PQ 10-bit for NVENC.
                self.ensure_fp16_src()?;
                let fp16 = self.fp16_src.clone().context("fp16 src")?;
                self.context.CopyResource(&fp16, &src);
                self.ensure_hdr10_out()?;
                let out = self.hdr10_out.clone().context("hdr10 out")?;
                if self.hdr_conv.is_none() {
                    self.hdr_conv = Some(HdrConverter::new(&self.device)?);
                }
                let srv = self.fp16_srv.clone().context("fp16 srv")?;
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                self.device
                    .CreateRenderTargetView(&out, None, Some(&mut rtv))?;
                let rtv = rtv.context("hdr10 rtv")?;
                self.hdr_conv.as_ref().unwrap().convert(
                    &self.context,
                    &srv,
                    &rtv,
                    self.width,
                    self.height,
                );
                self.last_present = Some((out.clone(), PixelFormat::Rgb10a2));
                Ok(self.d3d11_frame(out, PixelFormat::Rgb10a2))
            } else {
                // SDR: copy out of the recycled pool texture (cursor already composited) and hand off.
                self.ensure_bgra()?;
                let bgra = self.bgra_copy.clone().context("bgra copy")?;
                self.context.CopyResource(&bgra, &src);
                self.last_present = Some((bgra.clone(), PixelFormat::Bgra));
                Ok(self.d3d11_frame(bgra, PixelFormat::Bgra))
            }
        }
    }

    fn d3d11_frame(&self, texture: ID3D11Texture2D, format: PixelFormat) -> CapturedFrame {
        CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format,
            payload: FramePayload::D3d11(D3d11Frame {
                texture,
                device: self.device.clone(),
            }),
        }
    }
}

impl Capturer for WgcCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let overall = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(frame) = self.wait_and_drain() {
                self.first_frame = false;
                return self.process_frame(frame);
            }
            // No new frame within the wait — repeat the last presented frame (static desktop).
            if let Some((tex, fmt)) = &self.last_present {
                return Ok(self.d3d11_frame(tex.clone(), *fmt));
            }
            if Instant::now() > overall {
                bail!("no WGC frame within 20s (SudoVDA monitor not lit / no capture access?)");
            }
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        let target = self.signal.available.load(Ordering::Acquire);
        if target <= self.consumed {
            return Ok(None);
        }
        let mut last = None;
        while self.consumed < target {
            if let Ok(f) = self.pool.TryGetNextFrame() {
                last = Some(f);
            }
            self.consumed += 1;
        }
        match last {
            Some(frame) => self.process_frame(frame).map(Some),
            None => Ok(None),
        }
    }
    // set_active: the trait default (no-op) is correct — WGC keeps its session running across the
    // active/idle gate (cheap; the frame pool just recycles), like the DDA duplication.
}

impl Drop for WgcCapturer {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
        // _keepalive drops after, REMOVEing the SudoVDA monitor.
    }
}

fn tex_desc(
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind: u32,
) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
