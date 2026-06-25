//! Frame capture (plan §7). On Linux: a PipeWire ScreenCast portal stream. The spike uses the
//! CPU-copy fallback (the portal delivers a CPU buffer; the encoder uploads it to the GPU
//! internally). Zero-copy dmabuf→NVENC import is deferred (plan §9 risk).

use anyhow::Result;

/// Packed pixel layout of a [`CapturedFrame`]. The ScreenCast portal negotiates the
/// format; on wlroots it is commonly packed `RGB` (3 bytes/pixel). The encoder maps these
/// to an NVENC-accepted input format (`rgb0`/`bgr0`/`rgba`/`bgra`), expanding 3→4 bytes
/// where needed — no host-side colour conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// `[B,G,R,x]`, 4 bpp.
    Bgrx,
    /// `[R,G,B,x]`, 4 bpp.
    Rgbx,
    /// `[B,G,R,A]`, 4 bpp.
    Bgra,
    /// `[R,G,B,A]`, 4 bpp.
    Rgba,
    /// `[R,G,B]`, 3 bpp.
    Rgb,
    /// `[B,G,R]`, 3 bpp.
    Bgr,
    /// 10-bit RGB packed as `R10G10B10A2` (DXGI `R10G10B10A2_UNORM`), 4 bpp. The HDR capture path
    /// produces this: scRGB FP16 desktop pixels are converted to BT.2020 PQ and written here, then
    /// handed to NVENC as `ABGR10` for an HEVC Main10 / HDR10 encode.
    Rgb10a2,
    /// `NV12` (DXGI `NV12`): 8-bit BT.709 limited-range YUV 4:2:0. Produced by the D3D11 **video
    /// processor** (video engine, not the 3D engine) so the per-frame colour conversion doesn't fight a
    /// GPU-saturating game; handed to NVENC as `NV12` (it encodes YUV natively — no internal RGB→YUV).
    Nv12,
    /// `P010` (DXGI `P010`): 10-bit BT.2020 PQ limited-range YUV 4:2:0. HDR analogue of [`Nv12`]:
    /// video-processor output for HEVC Main10 / HDR10, handed to NVENC as `YUV420_10BIT`.
    P010,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb | PixelFormat::Bgr => 3,
            _ => 4,
        }
    }
}

/// A captured frame. [`format`](Self::format)/dimensions describe the pixels regardless of
/// where they live — [`payload`](Self::payload) is either a CPU buffer (the spike/fallback path)
/// or a GPU buffer already on the device (the zero-copy path, plan §9).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pts_ns: u64,
    /// Pixel layout of the payload.
    pub format: PixelFormat,
    pub payload: FramePayload,
}

/// A captured frame still living in a single-plane packed-RGB dmabuf (the VAAPI zero-copy path).
/// Owns a *dup* of the PipeWire buffer's fd, so the frame can travel to the encode thread and be
/// imported into a VA surface there without the compositor's buffer being closed underneath it.
/// (Content stability across the brief import window relies on the compositor's buffer pool depth,
/// same as any zero-copy capture — the VAAPI importer copies into its own NV12 surface promptly.)
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    pub fd: std::os::fd::OwnedFd,
    /// DRM FourCC of the packed-RGB plane (e.g. `XR24` for BGRx).
    pub fourcc: u32,
    /// DRM format modifier the compositor allocated (0 = LINEAR).
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

/// Where a captured frame's pixels live.
pub enum FramePayload {
    /// Tightly-packed CPU pixels in `format`, `width*height*bytes_per_pixel` (no row padding).
    Cpu(Vec<u8>),
    /// A pitched GPU buffer (BGRA-order, on the shared CUDA context) — the NVIDIA zero-copy path.
    /// The dmabuf has already been imported + copied into this owned device buffer.
    #[cfg(target_os = "linux")]
    Cuda(crate::zerocopy::DeviceBuffer),
    /// A raw packed-RGB dmabuf — the AMD/Intel (VAAPI) zero-copy path. The encoder imports it into
    /// a VA surface and does RGB→NV12 on the GPU video engine (no host CSC, no upload).
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
    /// A GPU-resident D3D11 texture (Windows zero-copy path for NVENC). Owns the copied frame.
    #[cfg(target_os = "windows")]
    D3d11(dxgi::D3d11Frame),
}

impl CapturedFrame {
    /// True if the frame's pixels are a GPU/CUDA buffer (the NVIDIA zero-copy path).
    pub fn is_cuda(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self.payload, FramePayload::Cuda(_))
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// True if the frame is a raw dmabuf (the VAAPI zero-copy path).
    pub fn is_dmabuf(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self.payload, FramePayload::Dmabuf(_))
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

/// Produces frames from a captured output. Lives on its own thread, feeding the encoder
/// over a bounded drop-oldest channel (never block the compositor).
pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Non-blocking: the freshest frame available since the last call, or `None` if none has
    /// arrived (the caller reuses its last frame to hold a steady output rate). The default
    /// just produces a frame each call — fine for instant synthetic sources; the portal
    /// overrides it to drain its channel without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }

    /// Gate expensive per-frame work so the capturer can be kept alive (reused) between
    /// streams without burning CPU. The portal capturer skips the de-pad copy while inactive;
    /// the default is a no-op (synthetic sources are produced on demand). Set `true` for the
    /// duration of a stream, `false` when it ends.
    fn set_active(&self, _active: bool) {}

    /// The source's static HDR mastering metadata (SMPTE ST.2086 + content light level), when the
    /// capturer can read it from the output (Windows `IDXGIOutput6::GetDesc1`). `None` = unknown /
    /// SDR / a backend that doesn't expose it (the default — Linux capture has no HDR path yet).
    /// The stream loop forwards this to the encoder (in-band SEI) and the client (`0xCE` datagram),
    /// so the two stay a single source of truth. May change mid-session if the source is regraded.
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        None
    }

    /// How many frames the encode loop may keep in flight (submitted but not yet polled) before it
    /// blocks. `1` (the default) is the synchronous loop: capture → submit → poll-blocks, so the
    /// per-frame wall time is `capture+convert + encode`. A capturer that hands a fresh output texture
    /// per frame (so the encode of N reads a different texture than the convert of N+1 writes) can return
    /// `>1` to PIPELINE: the loop submits N+1 before polling N, overlapping the convert/copy on the 3D
    /// engine with the NVENC-ASIC encode of the prior frame, dropping per-frame wall toward `max(...)`.
    fn pipeline_depth(&self) -> usize {
        1
    }
}

/// A deterministic moving test pattern (BGRx). Lets the spike exercise the encode → file →
/// `punktfunk_core` path with no live capture session, and produces obviously non-static
/// content (a sweeping bar + animated gradient) so the encoded output is verifiable.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl SyntheticCapturer {
    const BPP: usize = 4; // emits BGRx

    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(width > 0 && height > 0 && fps > 0);
        let buf = vec![0u8; width as usize * height as usize * Self::BPP];
        SyntheticCapturer {
            width,
            height,
            fps,
            frame_idx: 0,
            buf,
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bpp = Self::BPP;
        let t = self.frame_idx;
        // A vertical bar sweeps left→right once every ~2s; the background is a gradient
        // whose phase advances each frame, so every pixel changes frame-to-frame.
        let bar_x = ((t * w as u64) / (self.fps as u64 * 2)) % w as u64;
        let phase = (t % 256) as usize;
        for y in 0..h {
            let row = y * w * bpp;
            for x in 0..w {
                let i = row + x * bpp;
                let on_bar = (x as u64).abs_diff(bar_x) < 8;
                // BGRx byte order: [B, G, R, x]
                self.buf[i] = if on_bar {
                    255
                } else {
                    ((x + phase) & 0xff) as u8
                };
                self.buf[i + 1] = if on_bar {
                    255
                } else {
                    ((y + phase) & 0xff) as u8
                };
                self.buf[i + 2] = if on_bar { 255 } else { ((x + y) & 0xff) as u8 };
                self.buf[i + 3] = 0;
            }
        }
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps as u64;
        self.frame_idx += 1;
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
        })
    }
}

/// A cheap moving test pattern (BGRx) for the streaming path: a pulsing field + a white band
/// sweeping down, generated with whole-buffer `fill`s so it stays real-time even at 5K.
pub struct FastSyntheticCapturer {
    width: u32,
    height: u32,
    frame_idx: u64,
    buf: Vec<u8>,
    /// PUNKTFUNK_SYNTH_NOISE: every frame is fresh high-entropy noise NVENC can't compress or
    /// predict, so the encoder hits its (CBR) bitrate target — a throughput test of the real
    /// encode→FEC→send→recv path. The default flat/band content compresses to ~nothing, so it
    /// can't generate real Mbps (the encoder is content-driven). xorshift over u64 chunks.
    noise: bool,
    rng: u64,
}

impl FastSyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0);
        FastSyntheticCapturer {
            width,
            height,
            frame_idx: 0,
            buf: vec![0u8; width as usize * height as usize * 4],
            noise: std::env::var_os("PUNKTFUNK_SYNTH_NOISE").is_some(),
            rng: 0x9e3779b97f4a7c15,
        }
    }
}

impl Capturer for FastSyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if self.noise {
            // Fresh, every-frame-decorrelated noise: reseed from the frame index so consecutive
            // frames share no structure (forces large P-frames too, not just the keyframe).
            let mut s = self
                .rng
                .wrapping_add(self.frame_idx.wrapping_mul(0x2545F491_4F6CDD1D))
                | 1;
            for c in self.buf.chunks_exact_mut(8) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                c.copy_from_slice(&s.to_le_bytes());
            }
            self.rng = s;
        } else {
            let (w, h) = (self.width as usize, self.height as usize);
            let row = w * 4;
            let shade = (self.frame_idx % 256) as u8;
            self.buf.fill(shade);
            let band_h = (h / 20).max(1);
            let band_y = (self.frame_idx as usize * 6) % h;
            for y in band_y..(band_y + band_h).min(h) {
                self.buf[y * row..(y + 1) * row].fill(0xff);
            }
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
        })
    }
}

/// Open a live capturer for a client-sized monitor via the xdg ScreenCast portal
/// (`ashpd`) → PipeWire (`pipewire`). Implemented in the `linux` submodule.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    // On RemoteDesktop-capable desktops (KWin/GNOME) anchor ScreenCast to a RemoteDesktop
    // session so it inherits that grant headlessly; wlroots/Sway has no RemoteDesktop portal,
    // so use a plain ScreenCast session there.
    let anchored = crate::inject::default_backend() == crate::inject::Backend::Libei;
    linux::PortalCapturer::open(anchored).map(|c| Box::new(c) as Box<dyn Capturer>)
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// Build a capturer from an already-created virtual output (see [`crate::vdisplay`]). Consumes
/// the output's PipeWire node + optional remote fd + keepalive — the capturer owns the keepalive,
/// so dropping the capturer releases the virtual output. Compositor-agnostic: works for any
/// [`crate::vdisplay::VirtualDisplay`] backend. The captured size is the size the output was
/// created at — native, no scaling.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    _want_hdr: bool,
) -> Result<Box<dyn Capturer>> {
    // The Linux host stays 8-bit (HDR is blocked upstream), so `want_hdr` is unused here.
    linux::PortalCapturer::from_virtual_output(vout).map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// `PUNKTFUNK_NO_WGC=1` forces the pure single-process DDA (Desktop Duplication) path everywhere: it
/// skips WGC in [`capture_virtual_output`] AND bypasses the two-process secure-desktop relay (so even a
/// SYSTEM host captures in-process via DDA, the way Apollo does — one capturer for the normal AND the
/// secure desktop). For bringing DDA up to parity / validating it on its own; all the WGC code stays
/// compiled and comes back the moment the flag is unset.
#[cfg(target_os = "windows")]
pub(crate) fn wgc_disabled() -> bool {
    crate::config::config().no_wgc
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want_hdr: bool,
) -> Result<Box<dyn Capturer>> {
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "SudoVDA target not yet an active display (needs a WDDM GPU to activate it)"
        )
    })?;
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // P2 direct frame push (kill DDA): consume frames straight from the pf-vdisplay driver's shared
    // ring — no Desktop Duplication, no win32u reparenting hook. Opt-in while it's A/B'd against DDA;
    // `idd_push` takes the keepalive (owns the virtual display) so there's no fall-through.
    if crate::config::config().idd_push {
        // Recreate the monitor + ring per session (fix-teardown): a FRESH monitor reliably gets a
        // working IddCx swap-chain, whereas a REUSED monitor's swap-chain dies after ~2 sessions and
        // the host can't revive it. The driver's recreate crash (target id resolved to 0) is fixed by
        // stamping target_id onto the monitor context. The ring is always FP16 (the driver composes
        // the IDD in FP16); `want_hdr` selects the per-frame conversion (FP16 → Rgb10a2 vs Bgra).
        // If IDD-push can't open OR the driver doesn't attach to the ring within a few seconds (e.g. a
        // hybrid-GPU render mismatch), fall back to DDA so the session is NEVER left black (audit §5.1).
        // `open()` hands the keepalive back on failure so DDA can take ownership of the virtual display.
        match idd_push::IddPushCapturer::open(target.clone(), pref, want_hdr, keep) {
            Ok(c) => return Ok(Box::new(c) as Box<dyn Capturer>),
            Err((e, keep)) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "IDD-push open/attach failed — falling back to DDA"
                );
                return dxgi::DuplCapturer::open(target, pref, keep, false)
                    .map(|c| Box::new(c) as Box<dyn Capturer>);
            }
        }
    }
    // WGC (Windows.Graphics.Capture) is the default: it captures the COMPOSED desktop including the
    // overlay/independent-flip planes DXGI Desktop Duplication misses (the frozen-HDR-animation bug),
    // and has no ACCESS_LOST-on-overlay churn. DDA stays available via PUNKTFUNK_CAPTURE=dda and is
    // the secure-desktop (lock/UAC) fallback (WGC can't capture those). `keep` is moved into the
    // chosen backend (it owns the SudoVDA keepalive), so there's no open-time auto-fallback.
    let backend = crate::config::config().capture_backend.as_str();
    if backend == "dda" || backend == "dxgi" || wgc_disabled() {
        return dxgi::DuplCapturer::open(target, pref, keep, false)
            .map(|c| Box::new(c) as Box<dyn Capturer>);
    }
    // WGC default, with a watchdog'd DDA fallback. WGC's Direct3D11CaptureFramePool::CreateFreeThreaded
    // intermittently HANGS on the headless SudoVDA (IddCx) display — a blocking call we can't error out
    // of in place. So run WGC open on a dedicated thread and bound it: if it doesn't finish in time
    // (hang) or errors, fall back to the reliable DDA path so the session is NEVER left black. WGC,
    // when it opens, captures the composed desktop (overlay/MPO-correct HDR — fixes frozen animations);
    // DDA is the safety net (+ the secure-desktop path). The encode thread is set MTA so the WGC
    // objects built on the watchdog thread (also MTA) are usable here; the keepalive is handed to WGC
    // only on success, else to DDA. A hung watchdog thread is abandoned (holds no keepalive).
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(
            windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
        );
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let t = target.clone();
    let _ = std::thread::Builder::new()
        .name("wgc-open".into())
        .spawn(move || {
            let _ = tx.send(wgc::WgcCapturer::open(t, pref));
        });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(mut c)) => {
            c.attach_keepalive(keep);
            Ok(Box::new(c) as Box<dyn Capturer>)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "WGC open failed — falling back to DDA");
            dxgi::DuplCapturer::open(target, pref, keep, false)
                .map(|c| Box::new(c) as Box<dyn Capturer>)
        }
        Err(_) => {
            tracing::warn!("WGC open timed out (CreateFreeThreaded hang on the virtual display) — falling back to DDA");
            dxgi::DuplCapturer::open(target, pref, keep, false)
                .map(|c| Box::new(c) as Box<dyn Capturer>)
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    _want_hdr: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}

#[cfg(target_os = "windows")]
pub mod composed_flip;
#[cfg(target_os = "windows")]
pub mod desktop_watch;
#[cfg(target_os = "windows")]
pub mod dxgi;
#[cfg(target_os = "windows")]
pub mod idd_push;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
pub mod wgc;
#[cfg(target_os = "windows")]
pub mod wgc_relay;
