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
    find_output, make_device, nudge_cursor_onto, D3d11Frame, HdrConverter, VideoConverter,
    WinCaptureTarget,
};
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::{IInspectable, Interface};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIOutput6};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// Output texture ring depth. The encode loop pipelines one frame deep (NVENC encodes frame N while
/// the capturer produces N+1), so two live textures suffice; three gives headroom against a slow
/// `lock_bitstream` and matches the WGC frame-pool depth.
// Sized for the deep encode pipeline (`PUNKTFUNK_ENCODE_DEPTH`, default 4, clamped ≤ 6): up to DEPTH
// frames are in flight in NVENC at once, so the HDR convert ring and the SDR held-frame set must each
// keep DEPTH(+headroom) live textures, and the WGC pool needs spare buffers beyond what we hold.
const OUT_RING: usize = 8;

/// SDR zero-copy: how many recent WGC frames to keep alive so NVENC can encode the pool texture in
/// place (no `CopyResource`). Each in-flight encode reads a distinct frame, so this must exceed the
/// pipeline depth; the oldest is released once `HELD_FRAMES` newer ones exist.
const HELD_FRAMES: usize = 8;
/// WGC frame-pool buffer count. Must exceed `HELD_FRAMES` so the compositor always has free buffers
/// to render into while we hold frames for in-place (zero-copy) SDR encode.
const WGC_POOL_BUFFERS: i32 = 10;

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
    /// Ring of host-owned output textures (BGRA for SDR, R10G10B10A2 for HDR), rotated per processed
    /// frame. A ring — not one texture — is required because the encode loop is PIPELINED: NVENC
    /// encodes frame N (in place, registered by pointer) while this capturer produces frame N+1, so
    /// N+1 must land in a DIFFERENT texture or it clobbers the in-flight encode. (`fp16_src` stays
    /// single: it's only touched within the D3D11 immediate context, whose op ordering already
    /// serializes the convert's read against the next copy's write — NVENC's async engine read is the
    /// only consumer that escapes that ordering, and it reads the ring output, never `fp16_src`.)
    out_ring: Vec<ID3D11Texture2D>,
    ring_idx: usize,
    /// Video-processor RGB→YUV converter (off the 3D engine where possible) + its NV12/P010 output
    /// ring. Preferred path: the OS-composited capture (cursor already in it) is converted DIRECTLY to
    /// NVENC's native YUV — no `CopyResource`, no cursor draw, and NVENC skips its internal RGB→YUV.
    /// `None`/error → falls back to the legacy SDR-zero-copy / HDR-shader paths.
    video_conv: Option<VideoConverter>,
    yuv_out: Vec<ID3D11Texture2D>,
    yuv_idx: usize,
    yuv_is_hdr: bool,
    vp_disabled: bool,
    /// SDR zero-copy: the recent WGC frames we hand to NVENC in place. Held so the pool doesn't
    /// recycle the texture mid-encode; the oldest is released once `HELD_FRAMES` newer ones exist.
    held: VecDeque<Direct3D11CaptureFrame>,
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
            // Extra buffers: SDR zero-copy holds the last `HELD_FRAMES` frames (encoded in place), so
            // the pool needs headroom beyond that for the producer to keep rendering at 240 Hz.
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &d3d_device,
                pixel_format,
                WGC_POOL_BUFFERS,
                size,
            )
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
                out_ring: Vec::new(),
                ring_idx: 0,
                video_conv: None,
                yuv_out: Vec::new(),
                yuv_idx: 0,
                yuv_is_hdr: false,
                vp_disabled: std::env::var_os("PUNKTFUNK_NO_VIDEO_PROCESSOR").is_some(),
                held: VecDeque::new(),
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

    /// Lazily allocate the HDR output texture ring (R10G10B10A2, the convert pass's render target →
    /// NVENC input), `RENDER_TARGET`-bindable. SDR is zero-copy (encodes the WGC pool texture in
    /// place) and uses no ring.
    unsafe fn ensure_out_ring(
        &mut self,
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Result<()> {
        if !self.out_ring.is_empty() {
            return Ok(());
        }
        let desc = tex_desc(
            self.width,
            self.height,
            format,
            D3D11_BIND_RENDER_TARGET.0 as u32,
        );
        for _ in 0..OUT_RING {
            let mut t = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .context("CreateTexture2D(wgc out ring)")?;
            self.out_ring.push(t.context("wgc out ring tex")?);
        }
        Ok(())
    }

    /// Convert `input` (the OS-composited WGC pool texture: BGRA or scRGB FP16) → NVENC's native YUV
    /// (NV12 / P010) on the video processor. Returns the YUV texture (from a ring so consecutive
    /// encodes don't collide), or `None` to fall back to the legacy RGB paths.
    unsafe fn convert_to_yuv(
        &mut self,
        input: &ID3D11Texture2D,
        hdr: bool,
    ) -> Option<ID3D11Texture2D> {
        if self.vp_disabled {
            return None;
        }
        if self.video_conv.is_none() || self.yuv_out.is_empty() || self.yuv_is_hdr != hdr {
            self.video_conv = None;
            self.yuv_out.clear();
            self.yuv_idx = 0;
            let vc = match VideoConverter::new(
                &self.device,
                &self.context,
                self.width,
                self.height,
                hdr,
            ) {
                Ok(vc) => vc,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "WGC: video processor unavailable — falling back to RGB path");
                    self.vp_disabled = true;
                    return None;
                }
            };
            let fmt = if hdr {
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_P010
            } else {
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12
            };
            let desc = tex_desc(
                self.width,
                self.height,
                fmt,
                D3D11_BIND_RENDER_TARGET.0 as u32,
            );
            for _ in 0..OUT_RING {
                let mut t = None;
                if self
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .is_err()
                {
                    tracing::warn!("WGC: CreateTexture2D(YUV) failed — falling back to RGB path");
                    self.vp_disabled = true;
                    self.yuv_out.clear();
                    return None;
                }
                let Some(tex) = t else {
                    self.vp_disabled = true;
                    self.yuv_out.clear();
                    return None;
                };
                self.yuv_out.push(tex);
            }
            self.video_conv = Some(vc);
            self.yuv_is_hdr = hdr;
            tracing::info!(
                hdr,
                "WGC: video-processor YUV path active ({})",
                if hdr { "P010" } else { "NV12" }
            );
        }
        let slot = self.yuv_idx;
        self.yuv_idx = (self.yuv_idx + 1) % self.yuv_out.len();
        let out = self.yuv_out[slot].clone();
        if let Err(e) = self.video_conv.as_ref()?.convert(input, &out) {
            tracing::warn!(error = %format!("{e:#}"),
                "WGC: VideoProcessorBlt failed — falling back to RGB path");
            self.vp_disabled = true;
            self.video_conv = None;
            self.yuv_out.clear();
            return None;
        }
        Some(out)
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

            // Preferred path: convert the OS-composited capture (cursor already in it) DIRECTLY to
            // NVENC's native YUV on the video processor — no CopyResource, no cursor draw, and NVENC
            // skips its internal RGB→YUV (the contended 3D step). WGC's multi-buffer pool + held set
            // means reading the pool texture directly does NOT serialize (unlike DDA's single-frame
            // model). The frame is held until the async Blt finishes.
            if let Some(yuv) = self.convert_to_yuv(&src, self.hdr) {
                let fmt = if self.hdr {
                    PixelFormat::P010
                } else {
                    PixelFormat::Nv12
                };
                self.last_present = Some((yuv.clone(), fmt));
                let out = self.d3d11_frame(yuv, fmt);
                self.held.push_back(frame);
                while self.held.len() > HELD_FRAMES {
                    self.held.pop_front();
                }
                return Ok(out);
            }

            // --- fallback (video processor unavailable) ---
            if self.hdr {
                // Next ring slot — the in-flight encode reads the slot we handed out last time, so
                // this capture must land in a different one (see `out_ring`).
                let slot = self.ring_idx;
                self.ring_idx = (self.ring_idx + 1) % OUT_RING;
                // FP16 (cursor already composited by the OS) → BT.2020 PQ 10-bit for NVENC.
                self.ensure_fp16_src()?;
                let fp16 = self.fp16_src.clone().context("fp16 src")?;
                self.context.CopyResource(&fp16, &src);
                self.ensure_out_ring(DXGI_FORMAT_R10G10B10A2_UNORM)?;
                let out = self.out_ring[slot].clone();
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
                // SDR ZERO-COPY: hand NVENC the WGC pool texture DIRECTLY — no `CopyResource`. The
                // per-frame copy otherwise queues on the graphics engine behind a GPU-saturating game
                // and stalls `lock_bitstream` ~20 ms (NVENC sits idle waiting for its input). Encoding
                // the pool texture in place removes that graphics-queue dependency (Apollo's model).
                // We must keep the frame alive until its async encode finishes, so retain the last
                // `HELD_FRAMES`; the pool has spare buffers so the producer never starves.
                self.last_present = Some((src.clone(), PixelFormat::Bgra));
                let out = self.d3d11_frame(src, PixelFormat::Bgra);
                self.held.push_back(frame);
                while self.held.len() > HELD_FRAMES {
                    self.held.pop_front();
                }
                Ok(out)
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
