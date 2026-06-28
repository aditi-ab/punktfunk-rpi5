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
    /// (in-process, Session 0; no Desktop Duplication, no WGC helper).
    IddPush,
    /// Windows: DXGI Desktop Duplication (`PUNKTFUNK_CAPTURE=dda|dxgi` or `PUNKTFUNK_NO_WGC`).
    Dda,
    /// Windows: Windows.Graphics.Capture (the composed-desktop default), with a DDA watchdog fallback.
    Wgc,
}

impl CaptureBackend {
    /// Resolve the capture backend from [`config`](crate::config). This is the single resolver shared by
    /// [`SessionPlan::resolve`] and the standalone callers (GameStream / spike), so they can't drift.
    #[cfg(target_os = "linux")]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }

    /// Windows precedence (identical to the pre-stage-3 `capture_virtual_output` branch order):
    /// IDD-push wins; else an explicit `dda`/`dxgi` request or `PUNKTFUNK_NO_WGC` selects DDA; else WGC.
    #[cfg(target_os = "windows")]
    pub fn resolve() -> Self {
        let cfg = crate::config::config();
        if cfg.idd_push {
            CaptureBackend::IddPush
        } else if matches!(cfg.capture_backend.as_str(), "dda" | "dxgi")
            || crate::capture::wgc_disabled()
        {
            CaptureBackend::Dda
        } else {
            CaptureBackend::Wgc
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }
}

/// How a session is structured across processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTopology {
    /// One process captures + encodes (Linux; Windows non-SYSTEM / IDD-push / `NO_WGC`).
    SingleProcess,
    /// SYSTEM host + a user-session WGC helper relay (the Windows normal-desktop path under SYSTEM,
    /// where in-process WGC can't activate). See `virtual_stream_relay`.
    TwoProcessRelay,
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
    /// The IDD-push HDR hint (`bit_depth >= 10`) — the want-HDR flag the capturer was passed before.
    /// Non-IDD-push Windows backends ignore it and auto-detect HDR from the monitor; Linux is 8-bit.
    pub hdr: bool,
    /// Handshake-negotiated chroma subsampling (4:2:0, or full-chroma 4:4:4 when the client + host +
    /// GPU all support it). Resolved before the Welcome; `Yuv420` on every backend that declined it.
    pub chroma: crate::encode::ChromaFormat,
}

impl SessionPlan {
    /// Resolve the whole plan once from [`config`](crate::config) + the negotiated `bit_depth` and
    /// `chroma`.
    pub fn resolve(bit_depth: u8, chroma: crate::encode::ChromaFormat) -> Self {
        SessionPlan {
            capture: CaptureBackend::resolve(),
            topology: resolve_topology(),
            encoder: resolve_encoder(),
            bit_depth,
            hdr: bit_depth >= 10,
            chroma,
        }
    }

    /// The capturer's target output format (Goal-1 stage 5): `gpu` from the already-resolved `encoder`
    /// (no second backend probe), `hdr` from the plan. Handed into `capture::capture_virtual_output` so the
    /// capturer never re-derives the encode backend.
    pub fn output_format(&self) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        // Linux NVENC 4:4:4: libavcodec `hevc_nvenc` only emits 4:4:4 from a YUV444 *input* frame —
        // RGB-in is always subsampled to 4:2:0 (verified on the RTX 5070 Ti). So the encoder does an
        // RGB→YUV444P swscale and needs CPU-resident RGB frames; force the zero-copy GPU capture off
        // for a 4:4:4 NVENC session. (VAAPI 4:4:4, where the hardware supports it, keeps its dmabuf
        // path via `scale_vaapi`; Windows NVENC ingests ARGB directly and stays GPU.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 =
                self.chroma.is_444() && !crate::encode::linux_zero_copy_is_vaapi();
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

/// Process topology. On Windows this is the former `punktfunk1::should_use_helper` logic verbatim; on
/// every other platform the session is always single-process.
#[cfg(target_os = "windows")]
pub(crate) fn resolve_topology() -> SessionTopology {
    let cfg = crate::config::config();
    // `NO_HELPER`/`NO_WGC` force single-process; IDD-push captures in-process in Session 0 (no helper);
    // otherwise the helper runs when forced or when we're SYSTEM (in-process WGC can't activate there).
    let helper = if cfg.no_helper || crate::capture::wgc_disabled() || cfg.idd_push {
        false
    } else {
        cfg.force_helper || crate::capture::wgc_relay::running_as_system()
    };
    if helper {
        SessionTopology::TwoProcessRelay
    } else {
        SessionTopology::SingleProcess
    }
}

#[cfg(not(target_os = "windows"))]
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
    EncoderBackend::PlatformAuto
}
