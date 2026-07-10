//! `SessionPlan` — the per-session capture / topology / encoder decision, resolved **once** from
//! [`HostConfig`](crate::config) (+ the handshake-negotiated bit depth) into a typed, logged value.
//!
//! **Goal-1 stage 3** (`design/windows-host-rewrite.md` §2.2): before this, the Windows session decision was
//! re-derived at three call sites — the capture backend inside `capture::capture_virtual_output`, the
//! process topology in `punktfunk1::should_use_helper`, and the encode backend in
//! `encode::windows_resolved_backend` — each reading [`config`](crate::config) independently, with no
//! single owner (the latent "capture and encode disagree on the backend" hazard, plan §2.4). `SessionPlan`
//! resolves them together, once, so the deployed path reads one typed artifact.
//!
//! Stage 3 routes the **capture** and **topology** decisions through the plan (see
//! `capture::capture_virtual_output` taking [`CaptureBackend`] in, and `virtual_stream` reading
//! [`SessionTopology`]). The **encoder** is resolved by `encode::windows_resolved_backend` (config-backed
//! and GPU-vendor cached since stage 2, so already a single source) and *recorded* here as
//! [`EncoderBackend`]. Threading `encoder`/`input_format` into the encoder + capturer opens — which
//! removes the `capture → encode::windows_resolved_backend()` back-reference recomputed in `dxgi.rs` —
//! is **stage 5**.
//!
//! The type is platform-neutral so it threads through the shared `virtual_stream`/`build_pipeline`
//! signatures; on Linux it resolves to the single portal/single-process path (the 3-way dispatch is a
//! Windows-only concern).

/// Where a session's frames come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Linux: the xdg ScreenCast portal → PipeWire (the only Linux capture path).
    Portal,
    /// Windows: IDD direct-push — frames pulled straight from the pf-vdisplay driver's shared ring
    /// (in-process, Session 0; captures the secure desktop too). The sole Windows capture path —
    /// DXGI Desktop Duplication (DDA) and the WGC two-process relay were removed.
    IddPush,
}

impl CaptureBackend {
    /// Resolve the capture backend from [`config`](crate::config). This is the single resolver shared by
    /// [`SessionPlan::resolve`] and the standalone callers (GameStream / spike), so they can't drift.
    #[cfg(target_os = "linux")]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }

    /// Windows: IDD direct-push is the sole capture path (DDA + the WGC two-process relay were removed).
    #[cfg(target_os = "windows")]
    pub fn resolve() -> Self {
        CaptureBackend::IddPush
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }
}

/// How a session is structured across processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTopology {
    /// One process captures + encodes. The only topology: Linux (portal) and Windows (in-process
    /// IDD-push in Session 0). The SYSTEM-host + user-session WGC relay was removed with DDA/WGC.
    SingleProcess,
}

/// The resolved encode backend (recorded for logging / stages 4–5; the per-session encoder open still
/// resolves via `encode::windows_resolved_backend`, which is config-backed + GPU-vendor cached).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Linux: NVENC vs VAAPI is auto-detected inside `encode::open_video` (not modeled here).
    PlatformAuto,
    Nvenc,
    Amf,
    Qsv,
    Software,
}

impl EncoderBackend {
    /// True if this backend encodes on the GPU (so the capturer should produce GPU-resident frames). Only
    /// the software encoder takes CPU staging; `PlatformAuto` (Linux NVENC/VAAPI) is always GPU.
    pub fn is_gpu(self) -> bool {
        !matches!(self, EncoderBackend::Software)
    }
}

/// The per-session decision, resolved once. `Copy` so it threads through the capture/encode chain
/// without ceremony (stage 4 folds it, with the rest of the arg soup, into a `SessionContext`).
#[derive(Clone, Copy, Debug)]
pub struct SessionPlan {
    pub capture: CaptureBackend,
    pub topology: SessionTopology,
    pub encoder: EncoderBackend,
    /// Handshake-negotiated encode bit depth (8, or 10 = HEVC Main10).
    pub bit_depth: u8,
    /// The IDD-push HDR hint (`bit_depth >= 10`) — the want-HDR flag handed to the capturer so it
    /// proactively enables advanced color on the virtual display. Linux is 8-bit (HDR blocked upstream).
    pub hdr: bool,
    /// Handshake-negotiated chroma subsampling (4:2:0, or full-chroma 4:4:4 when the client + host +
    /// GPU all support it). Resolved before the Welcome; `Yuv420` on every backend that declined it.
    pub chroma: crate::encode::ChromaFormat,
    /// Handshake-negotiated video codec the encoder emits — HEVC by default, H.264 for a GPU-less
    /// software host (`resolve_codec` over the client's advertised codecs ∩ the host's capability).
    pub codec: crate::encode::Codec,
}

impl SessionPlan {
    /// Resolve the whole plan once from [`config`](crate::config) + the negotiated `bit_depth`,
    /// `chroma`, and `codec`.
    pub fn resolve(
        bit_depth: u8,
        chroma: crate::encode::ChromaFormat,
        codec: crate::encode::Codec,
    ) -> Self {
        SessionPlan {
            capture: CaptureBackend::resolve(),
            topology: resolve_topology(),
            encoder: resolve_encoder(),
            bit_depth,
            hdr: bit_depth >= 10,
            chroma,
            codec,
        }
    }

    /// The capturer's target output format (Goal-1 stage 5): `gpu` from the already-resolved `encoder`
    /// (no second backend probe), `hdr` from the plan. Handed into `capture::capture_virtual_output` so the
    /// capturer never re-derives the encode backend.
    pub fn output_format(&self) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        // Linux NVENC 4:4:4: libavcodec `hevc_nvenc` only emits 4:4:4 from a YUV444 *input* frame —
        // RGB-in is always subsampled to 4:2:0 (verified on the RTX 5070 Ti). With zero-copy
        // enabled the import worker produces that input ON the GPU (`ImportKind::Tiled444` — the
        // planar-YUV444 convert), so the session stays fully zero-copy at full chroma. Without
        // zero-copy the encoder swscales CPU RGB → YUV444P, which needs CPU-resident frames —
        // force the GPU capture off for that case only. (VAAPI 4:4:4, where the hardware supports
        // it, keeps its dmabuf path via `scale_vaapi`; Windows NVENC ingests BGRA directly.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 = self.chroma.is_444()
                && !crate::encode::linux_zero_copy_is_vaapi()
                && !crate::zerocopy::enabled();
            if gpu && force_cpu_for_nvenc_444 {
                // Surface the trade loudly: this is the single biggest per-frame cost a 4:4:4
                // session adds (full-res CPU readback + swscale RGB→YUV444P every frame), and
                // it looks like an unexplained fps ceiling if you don't know it happened.
                tracing::warn!(
                    "4:4:4 session on the NVENC path without PUNKTFUNK_ZEROCOPY: zero-copy GPU \
                     capture DISABLED — every frame is CPU RGB + swscale RGB→YUV444P; expect a \
                     lower fps ceiling than 4:2:0 at this mode (set PUNKTFUNK_ZEROCOPY=1 for the \
                     GPU 4:4:4 convert)"
                );
            }
            gpu && !force_cpu_for_nvenc_444
        };
        crate::capture::OutputFormat {
            gpu,
            hdr: self.hdr,
            // 4:4:4 needs a full-chroma source: on Windows this keeps the capturer on RGB (not the
            // default NV12/P010 video-engine output) so NVENC can CSC to 4:4:4.
            chroma_444: self.chroma.is_444(),
        }
    }
}

/// Process topology. Single-process is the only topology now: Linux (portal) and Windows (in-process
/// IDD-push in Session 0). The Windows SYSTEM-host + user-session WGC relay was removed with DDA/WGC.
pub(crate) fn resolve_topology() -> SessionTopology {
    SessionTopology::SingleProcess
}

#[cfg(target_os = "windows")]
fn resolve_encoder() -> EncoderBackend {
    match crate::encode::windows_resolved_backend() {
        crate::encode::WindowsBackend::Nvenc => EncoderBackend::Nvenc,
        crate::encode::WindowsBackend::Amf => EncoderBackend::Amf,
        crate::encode::WindowsBackend::Qsv => EncoderBackend::Qsv,
        crate::encode::WindowsBackend::Software => EncoderBackend::Software,
    }
}

#[cfg(not(target_os = "windows"))]
fn resolve_encoder() -> EncoderBackend {
    // `PUNKTFUNK_ENCODER=software` forces the GPU-less openh264 path — which must take CPU-staged
    // capture (`EncoderBackend::Software.is_gpu() == false` → `output_format().gpu = false`), so the
    // portal capturer delivers CPU RGB. Everything else stays `PlatformAuto` (NVENC/VAAPI resolved
    // inside `encode::open_video`).
    match crate::config::config().encoder_pref.as_str() {
        "software" | "sw" | "openh264" => EncoderBackend::Software,
        _ => EncoderBackend::PlatformAuto,
    }
}
