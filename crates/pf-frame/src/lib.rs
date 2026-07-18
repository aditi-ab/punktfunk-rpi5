//! The shared media-pipeline vocabulary (plan §W6): the frame + pixel-format types that capture
//! (producer) and encode (consumer) both speak, extracted into a leaf crate so `pf-capture` and
//! `pf-encode` depend on the vocabulary WITHOUT depending on each other. The GPU payloads pull
//! their heavy backends in from below: `FramePayload::Cuda` owns a [`pf_zerocopy::DeviceBuffer`],
//! `FramePayload::D3d11` a [`dxgi::D3d11Frame`].
//!
//! Alongside the vocabulary live the small pure helpers that ride the same capture-encode seam:
//! [`hdr`] (HDR static metadata / in-band SEI), [`metronome`] (the metronomic-stall detector),
//! [`thread_qos`] (per-thread scheduling QoS), [`session_tuning`] (Windows process session
//! tuning), and — on Windows — [`dxgi`] (the capture identity + D3D11 device creation).

// Unsafe-proof program: every `unsafe {}` / `unsafe impl` must carry a `// SAFETY:` proof.
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod hdr;
pub mod metronome;
pub mod session_tuning;
pub mod thread_qos;

// The Windows DXGI capture identity + shared D3D11 device creation (plan §W6). Consumed by the
// capture IDD-push path, the encode D3D11 backends, and pf-vdisplay's `WinCaptureTarget`.
#[cfg(target_os = "windows")]
pub mod dxgi;

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

/// DRM FourCC for a packed 32-bit format name (little-endian, e.g. `b"XR24"`).
#[cfg(target_os = "linux")]
const fn drm_fourcc_code(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// Map a SPA/our [`PixelFormat`] to the DRM FourCC EGL expects for import. SPA byte order `BGRx`
/// ⇒ DRM `XRGB8888` (memory B,G,R,X), etc. Lives with the frame vocabulary (not in
/// `pf-zerocopy`) because it consumes [`PixelFormat`], which sits above that crate.
#[cfg(target_os = "linux")]
pub fn drm_fourcc(format: PixelFormat) -> Option<u32> {
    use PixelFormat::*;
    Some(match format {
        Bgrx => drm_fourcc_code(b"XR24"), // DRM_FORMAT_XRGB8888
        Bgra => drm_fourcc_code(b"AR24"), // DRM_FORMAT_ARGB8888
        Rgbx => drm_fourcc_code(b"XB24"), // DRM_FORMAT_XBGR8888
        Rgba => drm_fourcc_code(b"AB24"), // DRM_FORMAT_ABGR8888
        // 24-bit packed RGB/BGR have no straightforward dmabuf import here; use the CPU path.
        // Rgb10a2/Nv12/P010 are the Windows HDR / video-processor formats — never produced on
        // Linux; Yuv444 is OUR convert's OUTPUT, never a capture source format.
        Rgb | Bgr | Rgb10a2 | Nv12 | P010 | Yuv444 => return None,
    })
}

/// What a Windows capturer should produce, resolved **once** per session and passed **into**
/// `capture_virtual_output` (Goal-1 stage 5, plan §2.3/§5). Passing the format in is what lets a
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
    /// A PyroWave (wavelet) session on Windows: the IDD-push capturer must make its NV12 out-ring
    /// **shareable** (`SHARED | SHARED_NTHANDLE`) and signal a **shared fence** after each convert,
    /// so the pyrowave encoder can zero-copy-import the texture into its own Vulkan device
    /// (design/pyrowave-windows-host-zerocopy.md). Also forces the NV12 4:2:0 SDR convert branch
    /// (never BGRA-passthrough / P010). `false` on every non-PyroWave session and on Linux (the
    /// wavelet encoder ingests dmabufs / CPU RGB there, not a D3D11 texture).
    pub pyrowave: bool,
}

impl OutputFormat {
    /// Resolve the output format for an entry point that doesn't build a full [`SessionPlan`]
    /// (`crate::session_plan`) — the GameStream + spike paths. `gpu` is the encoder's GPU-residency,
    /// resolved by the caller via `pf_encode::resolved_backend_is_gpu` and passed **in** (capture
    /// never re-derives the backend — the one-way capture→encode edge, plan §2.4 / §W4); `hdr` as given.
    /// The native punktfunk/1 path uses `SessionPlan::output_format()` instead (it already resolved the
    /// encoder), so neither path makes a capturer re-derive it.
    pub fn resolve(hdr: bool, gpu: bool) -> Self {
        OutputFormat {
            gpu,
            hdr,
            // The GameStream + spike paths are always 4:2:0 (4:4:4 is punktfunk/1-native only).
            chroma_444: false,
            // GameStream never negotiates PyroWave (native punktfunk/1 only).
            pyrowave: false,
        }
    }
}

/// A mouse-cursor overlay to composite onto a frame at encode time (cursor-as-metadata). Rides on
/// [`CapturedFrame::cursor`] for the GPU zero-copy payloads (Cuda/Dmabuf), whose pixels never touch
/// the CPU — the encoder blends this small bitmap into its owned surface (Vulkan CSC image / CUDA
/// devbuf / VA surface). The CPU de-pad path composites the cursor inline instead, so it leaves
/// this `None`. `rgba` is `Arc` so attaching the (unchanged) bitmap to every frame is a refcount
/// bump, not a copy; `serial` bumps only when the bitmap image changes, so the encoder re-uploads
/// its small GPU texture on change and just moves a push-constant otherwise.
#[derive(Clone)]
pub struct CursorOverlay {
    /// Top-left in frame pixels where the bitmap is drawn (already = reported position − hotspot).
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Straight-alpha RGBA pixels, `w*h*4` (bytes R,G,B,A).
    pub rgba: std::sync::Arc<Vec<u8>>,
    /// Bumps whenever `rgba`/`w`/`h` change; stable across position-only moves.
    pub serial: u64,
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
    /// Cursor overlay to blend at encode time (GPU zero-copy payloads only); `None` when there's no
    /// visible cursor or the pixels were already composited on the CPU de-pad path. See
    /// [`CursorOverlay`].
    pub cursor: Option<CursorOverlay>,
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
    Cuda(pf_zerocopy::DeviceBuffer),
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
