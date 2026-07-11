//! Frame capture (plan §7). On Linux: a PipeWire ScreenCast portal stream. The spike uses the
//! CPU-copy fallback (the portal delivers a CPU buffer; the encoder uploads it to the GPU
//! internally). Zero-copy dmabuf→NVENC import is deferred (plan §9 risk).

// Every unsafe block in this module tree carries a `// SAFETY:` proof; enforce it (unsafe-proof
// program). As a parent module this also covers the child modules (capture::windows/linux::*).
#![deny(clippy::undocumented_unsafe_blocks)]

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
    /// Planar 8-bit YUV **4:4:4** (BT.709; range per `PUNKTFUNK_444_FULLRANGE`). Produced by the
    /// Linux zero-copy worker's GPU convert for a 4:4:4 session ([`FramePayload::Cuda`] with
    /// `DeviceBuffer::yuv444` — three full-res planes stacked in one allocation); NVENC encodes
    /// it natively under the Range-Extensions profile. Never a CPU payload.
    Yuv444,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb | PixelFormat::Bgr => 3,
            // Three full-res 1-byte planes (GPU-resident only; no CPU payload carries this).
            PixelFormat::Yuv444 => 3,
            _ => 4,
        }
    }
}

/// What a Windows capturer should produce, resolved **once** per session and passed **into**
/// [`capture_virtual_output`] (Goal-1 stage 5, plan §2.3/§5). Passing the format in is what lets a
/// capturer stop re-deriving the encode backend itself — it kills the
/// `capture/dxgi.rs → encode::windows_resolved_backend()` back-reference (the highest-severity coupling:
/// capture and encode could otherwise disagree on whether frames are GPU-resident). Neutral type; the
/// Linux portal capturer ignores it (it negotiates its own format with PipeWire).
#[derive(Clone, Copy, Debug)]
pub struct OutputFormat {
    /// Produce GPU-resident D3D11 frames (zero-copy for a GPU encoder — NVENC/AMF/QSV) rather than CPU
    /// staging. `false` **only** for the GPU-less software encoder.
    pub gpu: bool,
    /// HDR: the capturer converts to 10-bit (IDD-push FP16 → `P010`, or `Rgb10a2` for a 4:4:4 source).
    /// `false` = 8-bit SDR.
    pub hdr: bool,
    /// Full-chroma 4:4:4 session: the capturer must keep full chroma. On Windows the IDD-push
    /// capturer hands the **BGRA** slot through (skipping the subsampling BGRA→NV12
    /// VideoConverter) so NVENC ingests full-chroma RGB and CSCs to 4:4:4 itself — measured
    /// on-glass (RTX 5070 Ti): ARGB + `chromaFormatIDC=3` yields TRUE 4:4:4 and the conversion
    /// follows the configured VUI matrix (BT.709 limited since the VUI is always written). On
    /// Linux it forces the CPU RGB path the encoder swscales to `YUV444P`. `false` on every
    /// 4:2:0 session.
    pub chroma_444: bool,
}

impl OutputFormat {
    /// Resolve the output format for an entry point that doesn't build a full [`SessionPlan`]
    /// (`crate::session_plan`) — the GameStream + spike paths: `gpu` from the resolved encode backend,
    /// `hdr` as given. The native punktfunk/1 path uses `SessionPlan::output_format()` instead (it already
    /// resolved the encoder), so neither path makes a capturer re-derive it.
    pub fn resolve(hdr: bool) -> Self {
        OutputFormat {
            gpu: gpu_encode(),
            hdr,
            // The GameStream + spike paths are always 4:2:0 (4:4:4 is punktfunk/1-native only).
            chroma_444: false,
        }
    }
}

/// True if the resolved encode backend produces GPU frames (anything but the software encoder). The single
/// source for [`OutputFormat::resolve`]'s `gpu`; on Linux always true (the portal/VAAPI/CUDA path is GPU).
#[cfg(target_os = "windows")]
pub(crate) fn gpu_encode() -> bool {
    !matches!(
        crate::encode::windows_resolved_backend(),
        crate::encode::WindowsBackend::Software
    )
}
#[cfg(not(target_os = "windows"))]
pub(crate) fn gpu_encode() -> bool {
    // The GPU-less software encoder (openh264) needs CPU-staged RGB frames; every other Linux
    // backend (NVENC/CUDA, VAAPI) is GPU-resident. Mirrors `session_plan::resolve_encoder`, for the
    // GameStream/spike entry points that use `OutputFormat::resolve` instead of a full `SessionPlan`.
    !matches!(
        crate::config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    )
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
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    // The Linux host stays 8-bit (HDR is blocked upstream) and the portal negotiates its own pixel
    // format, so `want.gpu` gates GPU zero-copy capture (the capture backend is always the portal —
    // the `CaptureBackend` arg is a Windows-only dispatch) and `want.chroma_444` selects the
    // worker's planar-YUV444 GPU convert for tiled dmabufs (the 4:4:4 zero-copy path). `gpu =
    // false` (4:4:4 without zero-copy) forces the CPU mmap path so the encoder gets CPU-resident
    // RGB to swscale into YUV444P.
    linux::PortalCapturer::from_virtual_output(vout, want.gpu, want.chroma_444)
        .map(|c| Box::new(c) as Box<dyn Capturer>)
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "SudoVDA target not yet an active display (needs a WDDM GPU to activate it)"
        )
    })?;
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // IDD direct-push is the sole Windows capture path: consume frames straight from the pf-vdisplay
    // driver's shared ring (in-process, Session 0 — it captures the secure desktop too; no Desktop
    // Duplication, no WGC helper). A FRESH monitor + ring is created per session: a REUSED monitor's
    // swap-chain dies after ~2 sessions and can't be revived. The ring is always FP16 when the display
    // is HDR (the driver composes the IDD in FP16); `want.hdr` proactively enables advanced color and
    // selects the per-frame conversion (FP16 → P010 vs BGRA → NV12, or BGRA → AYUV for a
    // `want.chroma_444` SDR session). `IddPushCapturer` takes the keepalive (it owns the virtual
    // display). There is NO fallback (DDA + the WGC relay were removed): if it can't open or the
    // driver doesn't attach, the session fails cleanly and the client reconnects.
    idd_push::IddPushCapturer::open(target, pref, want.hdr, want.chroma_444, keep)
        .map(|c| Box::new(c) as Box<dyn Capturer>)
        .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

/// Whether the active capturer can deliver a full-chroma (RGB) source for a 4:4:4 HEVC encode. The
/// negotiator gates 4:4:4 on this so the host honestly downgrades to 4:2:0 when the capturer can only
/// produce subsampled frames. Linux (the portal capturer feeding CPU RGB → `yuv444p`) can; the Windows
/// IDD-push path delivers subsampled NV12/P010 today, so full-chroma capture there is a follow-up.
#[cfg(target_os = "linux")]
pub(crate) fn capturer_supports_444() -> bool {
    true
}
#[cfg(target_os = "windows")]
pub(crate) fn capturer_supports_444() -> bool {
    // IDD-push delivers full-chroma BGRA for an SDR 4:4:4 session (skipping the NV12
    // VideoConverter) — but only the direct-NVENC backend ingests RGB and CSCs it to 4:4:4
    // (measured on-glass: true full chroma, matrix follows the configured VUI), so gate on it
    // (AMF can't 4:4:4 at all; the QSV/ffmpeg path has no RGB-input 4:4:4 wiring). An HDR
    // display can't be known here (the virtual display's mode settles after the Welcome); that
    // combination downgrades at capture time — the capturer emits P010 and the encoder's caps
    // cross-check reports the 4:2:0 truth (the in-band SPS keeps the client correct either way).
    crate::encode::windows_resolved_backend() == crate::encode::WindowsBackend::Nvenc
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn capturer_supports_444() -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    _want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}

// Goal-1 stage 6: the Windows backend lives under `capture/windows/`, the Linux one under `capture/linux/`
// (`#[path]` keeps the module names flat, so every `crate::capture::*` path is unchanged). Windows capture
// is IDD direct-push only — DXGI Desktop Duplication (DDA) and the WGC two-process relay were removed.
#[cfg(target_os = "windows")]
#[path = "capture/windows/dxgi.rs"]
pub mod dxgi;
#[cfg(target_os = "windows")]
#[path = "capture/windows/idd_push.rs"]
pub mod idd_push;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
#[path = "capture/windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;
