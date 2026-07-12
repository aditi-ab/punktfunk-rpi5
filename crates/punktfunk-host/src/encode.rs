//! Hardware video encode (plan §7). Binds FFmpeg; never rewrites codecs. Low-latency preset,
//! B-frames off. The backend is per-GPU: NVENC on NVIDIA (`*_nvenc`, accepts `bgr0` and does
//! RGB→YUV on the GPU, so no host-side CSC) and VAAPI on AMD/Intel (`*_vaapi`; the CPU-input
//! fallback swscales RGB→NV12, the zero-copy path imports the capture dmabuf straight into a
//! VA surface). One [`Encoder`] trait, selected in [`open_video`].
// Every unsafe block in this module tree carries a `// SAFETY:` proof; enforce it (unsafe-proof
// program). As a parent module this also covers the child modules (encode::windows/linux::*).
#![deny(clippy::undocumented_unsafe_blocks)]

use crate::capture::{CapturedFrame, PixelFormat};
use anyhow::Result;

/// An encoded access unit (one NAL/AU) to hand to `punktfunk_core` for FEC + packetization.
/// `data` is in-band Annex-B (the encoder is opened without a global header), so each
/// keyframe carries its own VPS/SPS/PPS — the bytes are both a playable elementary
/// stream and a self-contained AU for the wire.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    /// True for IDR/keyframes (sets the SOF/keyframe wire flags).
    pub keyframe: bool,
    /// True when this AU is a **reference-frame-invalidation recovery frame** — a clean P-frame the
    /// encoder coded against a known-good reference in response to
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames). The pump tags it
    /// [`punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR`] so the client lifts its post-loss
    /// freeze on it without an IDR. Set by BOTH RFI backends: native AMF (the LTR force-reference
    /// frame) and Windows direct-NVENC (the first frame encoded after `nvEncInvalidateRefFrames` —
    /// the invalidation applies at the next `encode_picture`, so that AU is by construction the
    /// clean re-anchor). Without it the client's freeze can only lift on an IDR — which the host
    /// suppresses after a successful RFI (the cooldown), a ~1 s frozen stall per loss event.
    pub recovery_anchor: bool,
}

/// Codec selection negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

/// Chroma subsampling the encoder emits, negotiated with the client (the `PUNKTFUNK_444` gate + the
/// client's `VIDEO_CAP_444` + a GPU probe). `Yuv420` is the universal default; `Yuv444` is HEVC-only,
/// native-protocol-only (GameStream stays 4:2:0), and the host only ever passes it after
/// [`can_encode_444`] confirmed the active backend supports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromaFormat {
    #[default]
    Yuv420,
    Yuv444,
}

impl ChromaFormat {
    /// The HEVC `chroma_format_idc` this maps to: `1` (4:2:0) or `3` (4:4:4). Also the wire value
    /// echoed in [`punktfunk_core::quic::Welcome::chroma_format`].
    pub fn idc(self) -> u8 {
        match self {
            ChromaFormat::Yuv420 => punktfunk_core::quic::CHROMA_IDC_420,
            ChromaFormat::Yuv444 => punktfunk_core::quic::CHROMA_IDC_444,
        }
    }

    /// True for full-chroma 4:4:4.
    pub fn is_444(self) -> bool {
        matches!(self, ChromaFormat::Yuv444)
    }
}

impl Codec {
    /// Map a negotiated `quic` codec bit ([`punktfunk_core::quic::CODEC_H264`] etc.) to the encoder
    /// [`Codec`]. Unknown / `0` → HEVC (the pre-negotiation default). Inverse of [`Codec::to_wire`].
    pub fn from_wire(bit: u8) -> Codec {
        match bit {
            punktfunk_core::quic::CODEC_H264 => Codec::H264,
            punktfunk_core::quic::CODEC_AV1 => Codec::Av1,
            _ => Codec::H265,
        }
    }

    /// The single `quic` codec bit for this codec (echoed in [`punktfunk_core::quic::Welcome::codec`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Codec::H264 => punktfunk_core::quic::CODEC_H264,
            Codec::H265 => punktfunk_core::quic::CODEC_HEVC,
            Codec::Av1 => punktfunk_core::quic::CODEC_AV1,
        }
    }

    /// The `quic` codec bitfield the host can currently **emit** on the punktfunk/1 native path,
    /// given the resolved encode backend — the same GPU-aware advertisement GameStream builds for
    /// Moonlight ([`crate::gamestream::serverinfo`]), in `quic::CODEC_*` bits. The GPU-less software
    /// encoder (openh264) produces H.264 only; the probed backends (Linux VAAPI, Windows AMF/QSV)
    /// advertise exactly what the GPU encodes ([`vaapi_codec_support`] / [`windows_codec_support`] —
    /// AV1 encode is narrow, an old iGPU might lack HEVC); NVENC keeps the Moonlight-validated
    /// static superset. An empty probe means the GPU wasn't usable at probe time (GPU-less CI,
    /// wrong-vendor pref), not that it encodes nothing — fall back to the superset so `resolve_codec`
    /// still lands on HEVC for an auto client, exactly the pre-probe behaviour. Fed to
    /// [`punktfunk_core::quic::resolve_codec`] against the client's advertised codecs.
    pub fn host_wire_caps() -> u8 {
        /// The static GPU superset (H.264 | HEVC | AV1) — mirrors the GameStream
        /// `SERVER_CODEC_MODE_SUPPORT` advertisement for the unprobed backends.
        const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
            | punktfunk_core::quic::CODEC_HEVC
            | punktfunk_core::quic::CODEC_AV1;
        #[cfg(target_os = "linux")]
        {
            if matches!(
                crate::config::config().encoder_pref.as_str(),
                "software" | "sw" | "openh264"
            ) {
                return punktfunk_core::quic::CODEC_H264;
            }
            if linux_zero_copy_is_vaapi() {
                if let Some(m) = vaapi_codec_support().wire_mask() {
                    return m;
                }
            }
            // NVENC (static superset, like GameStream) — or an empty VAAPI probe (see above).
            GPU_SUPERSET
        }
        #[cfg(target_os = "windows")]
        {
            if windows_resolved_backend() == WindowsBackend::Software {
                return punktfunk_core::quic::CODEC_H264;
            }
            if windows_backend_is_probed() {
                if let Some(m) = windows_codec_support().wire_mask() {
                    return m;
                }
            }
            // NVENC (static superset, like GameStream) — or an empty AMF/QSV probe (see above).
            GPU_SUPERSET
        }
        // The macOS dev/test host has no GPU encode backend — keep the pre-probe advertisement.
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = GPU_SUPERSET;
            match crate::config::config().encoder_pref.as_str() {
                "software" | "sw" | "openh264" => punktfunk_core::quic::CODEC_H264,
                _ => punktfunk_core::quic::CODEC_HEVC,
            }
        }
    }

    /// Lowercase stats/console label (`"h264"` / `"hevc"` / `"av1"`) — the codec string seeded into
    /// the web console's session meta ([`crate::stats_recorder::StatsRecorder::register_session`]).
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "av1",
        }
    }

    /// The FFmpeg NVENC encoder name (selected by name, not codec id — the latter would
    /// pick the software encoder).
    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }

    /// The FFmpeg VAAPI encoder name (AMD via Mesa `radeonsi`, Intel via `iHD`/`i965`). One
    /// libavcodec encoder per codec covers both vendors — the kernel driver differs, the libva
    /// userspace API is identical. Selected by name (the codec id would pick the SW encoder).
    /// AV1 VAAPI encode is narrow (Intel Arc/Xe2+, AMD RDNA3+/RDNA4) — gate it on a capability
    /// probe, never assume it (see [`open_video`]).
    pub fn vaapi_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_vaapi",
            Codec::H265 => "hevc_vaapi",
            Codec::Av1 => "av1_vaapi",
        }
    }

    /// The FFmpeg AMD **AMF** encoder name (the Windows AMD backend). Selected by name (the codec id
    /// would pick the software encoder). AV1 (`av1_amf`) is RDNA3+/RX 7000+ — probe, never assume.
    pub fn amf_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_amf",
            Codec::H265 => "hevc_amf",
            Codec::Av1 => "av1_amf",
        }
    }

    /// The FFmpeg Intel **QSV** encoder name (the Windows Intel backend). Selected by name. AV1
    /// (`av1_qsv`) is Arc/Xe2+; HEVC Main10 is Gen9.5+ — probe, never assume.
    pub fn qsv_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_qsv",
            Codec::H265 => "hevc_qsv",
            Codec::Av1 => "av1_qsv",
        }
    }
}

/// Static capabilities an [`Encoder`] declares so the session glue routes loss-recovery and HDR
/// plumbing by *query* rather than relying on a method's no-op/`false` default. Cheap `Copy`; fixed
/// for the session (an HDR toggle re-initialises the encoder — re-query if that matters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderCaps {
    /// The encoder can perform real reference-frame invalidation — i.e.
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) can return `true`. When `false`
    /// the caller skips that always-`false` call and forces a keyframe directly on loss recovery.
    /// Two backends implement RFI: Windows direct-NVENC (`nvEncInvalidateRefFrames`) and native
    /// AMF (user-LTR force-reference, when the driver accepted the LTR slots at open). The
    /// libavcodec paths (Linux NVENC, VAAPI, QSV) can't express it and always keyframe.
    pub supports_rfi: bool,
    /// The encoder emits in-band HDR mastering/CLL SEI from [`set_hdr_meta`](Encoder::set_hdr_meta).
    /// When `false`, `set_hdr_meta` is a no-op and no in-band grade reaches the client. Only the
    /// Windows direct-NVENC path attaches it today.
    pub supports_hdr_metadata: bool,
    /// The opened encoder is actually producing a full-chroma 4:4:4 (`chroma_format_idc = 3`) stream.
    /// `false` on every 4:2:0 session (the default) and on a backend that declined 4:4:4. Set by the
    /// NVENC backends (Linux + Windows). The chroma is committed to the wire (`Welcome::chroma_format`)
    /// from the pre-open probe, so this is a *post-open cross-check*: the session glue logs loudly if
    /// the encoder's real chroma disagrees with what was negotiated (the in-band SPS is authoritative
    /// for the decoder either way).
    pub chroma_444: bool,
    /// The encoder runs a periodic **intra-refresh wave** — a moving band of intra blocks that
    /// re-codes the whole picture over ~0.5 s, no periodic IDR. FEC-unrecoverable loss self-heals as
    /// the band sweeps, so the session glue rate-limits client keyframe requests instead of answering
    /// each with a full IDR (the 20-40× frame-size spike that cascades under loss). Linux NVENC / AMF
    /// set it when `PUNKTFUNK_INTRA_REFRESH` opened the encoder in that mode; VAAPI/QSV/software never
    /// do. NOTE — the wave carries NO decoder-visible clean-point: FFmpeg never sets `AV_FRAME_FLAG_KEY`
    /// at a recovery point (H.264 flags key only when `recovery_frame_cnt == 0`; HEVC only on IRAP),
    /// and AMF emits no recovery-point SEI at all. So this cap ALONE does not let the client lift its
    /// post-loss freeze without an IDR — that needs [`intra_refresh_recovery`](Self::intra_refresh_recovery).
    pub intra_refresh: bool,
    /// The intra-refresh wave is a *validated constrained GDR* — verified on real hardware to fully
    /// heal a lost picture within one wave period with no residual artifacts. Only then does the host
    /// tag each wave-boundary AU with [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT),
    /// so the client can lift its freeze on the second mark (a proven clean re-anchor) instead of
    /// waiting out its backstop and forcing a full IDR. Default `false` on every backend until on-glass
    /// validation flips it — an un-validated encoder keeps the IDR recovery path, so this is inert and
    /// cannot regress. Meaningless unless [`intra_refresh`](Self::intra_refresh) is also set.
    pub intra_refresh_recovery: bool,
    /// Length of the intra-refresh wave in frames — the boundary period the host marks on (it sets
    /// `USER_FLAG_RECOVERY_POINT` on every Nth emitted AU, re-phased at each IDR). 0 when intra-refresh
    /// is off. Only consulted when [`intra_refresh_recovery`](Self::intra_refresh_recovery) is set.
    pub intra_refresh_period: u32,
}

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// [`submit`](Self::submit) with the **wire frame index** this frame's AU will carry — the
    /// number the packetizer stamps on it and the client's loss reports/RFI requests name. The
    /// session glue predicts it exactly as `AUs sent so far + frames in flight` (AUs are emitted
    /// FIFO, one per submission; anything that would break the prediction — an in-place reset, a
    /// device-change teardown, an encoder rebuild — forfeits the in-flight frames on BOTH sides
    /// and clears the encoder's reference state, so stale predictions die with it). The RFI
    /// backends pin their frame numbering (LTR marks, DPB timestamps) to this so
    /// [`invalidate_ref_frames`](Self::invalidate_ref_frames) compares client frame numbers
    /// against the same domain — an encoder-internal counter desyncs from the wire on the first
    /// mid-stream rebuild (adaptive bitrate steps do this under congestion, exactly when losses
    /// happen). Default: ignore the index and delegate to `submit` (backends without per-frame
    /// reference bookkeeping don't care).
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        let _ = wire_index;
        self.submit(frame)
    }
    /// This encoder's static [capabilities](EncoderCaps) (RFI, HDR SEI), so the session glue can
    /// route by query rather than rely on the no-op/`false` defaults of
    /// [`invalidate_ref_frames`](Self::invalidate_ref_frames) / [`set_hdr_meta`](Self::set_hdr_meta).
    /// Default: no optional capabilities (the SDR / libavcodec backends) — only the direct-NVENC
    /// path overrides it.
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    /// Force the next submitted frame to be an IDR keyframe (e.g. after a client
    /// reference-frame-invalidation request). Default: no-op.
    fn request_keyframe(&mut self) {}
    /// Set the source's static HDR mastering metadata (from the capturer). An HDR encoder emits it
    /// as in-band SEI (`mastering_display_colour_volume` + `content_light_level_info`) on each
    /// keyframe so any decoder — including stock Moonlight — tone-maps from the source's real grade.
    /// Default: no-op (SDR encoders / libavcodec paths that don't attach it yet). Cheap to call
    /// every frame; only the direct-NVENC path consumes it.
    fn set_hdr_meta(&mut self, _meta: Option<punktfunk_core::quic::HdrMeta>) {}
    /// Invalidate a contiguous range of previously-encoded reference frames (client frame numbers
    /// — WIRE frame indexes, the domain [`submit_indexed`](Self::submit_indexed) pins the encoder's
    /// bookkeeping to) so the encoder re-references an older still-valid frame instead of emitting
    /// a full IDR. Returns `true` if a real reference invalidation was performed; `false` means the
    /// encoder couldn't (range older than the DPB/LTR history, or the backend has no RFI) and the
    /// caller should fall back to [`request_keyframe`](Self::request_keyframe). Default: `false` —
    /// the Windows direct-NVENC path (`nvEncInvalidateRefFrames`) and native AMF (LTR
    /// force-reference) implement true RFI; the libavcodec paths can't express it, so they keyframe.
    fn invalidate_ref_frames(&mut self, _first_frame: i64, _last_frame: i64) -> bool {
        false
    }
    /// Pull the next encoded AU if one is ready.
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
    /// Tear the underlying hardware encoder down and rebuild it in place, keeping the session's
    /// negotiated parameters — the encode-stall watchdog's recovery lever (a wedged AMF/QSV
    /// driver stops emitting AUs or accepting frames without ever returning an error). Returns
    /// `true` when the encoder was rebuilt: every submitted-but-unpolled frame is forfeited and
    /// the next submitted frame starts a fresh stream (IDR). Default `false`: the backend has no
    /// in-place rebuild and the caller must treat the stall as fatal instead.
    fn reset(&mut self) -> bool {
        false
    }
    /// Signal end-of-stream. After this, drain the remaining AUs with [`poll`](Self::poll)
    /// until it returns `None` — NVENC buffers frames internally even at `delay=0`.
    fn flush(&mut self) -> Result<()>;
}

impl Codec {
    /// Maximum encodable dimension (px) per side for this codec on NVENC. H.264 tops out at
    /// 4096 (level constraint); HEVC and AV1 allow 8192. Used to reject out-of-range client
    /// modes up front (see [`validate_dimensions`]).
    pub fn max_dimension(self) -> u32 {
        match self {
            Codec::H264 => 4096,
            Codec::H265 | Codec::Av1 => 8192,
        }
    }

    /// The codec's *spec* top level/tier bitrate (bits/s) — the usual boundary at which NVENC
    /// starts rejecting `avcodec_open2` with EINVAL. NOT a hard cap: [`open_video`](crate::encode::
    /// open_video) probes the actual GPU ceiling by stepping DOWN from the requested bitrate only on
    /// EINVAL, and uses this purely as the first step-down candidate (so a card that accepts more —
    /// an RTX 5070 Ti does >1 Gbps HEVC where a 4090 caps at ~800 Mbps — is never clamped to it).
    /// HEVC Level 6.2 High tier = 800 Mbps; H.264 High level 6.2 ≈ 480 Mbps; AV1's levels allow more.
    pub fn max_bitrate_bps(self) -> u64 {
        match self {
            Codec::H264 => 480_000_000,
            Codec::H265 => 800_000_000,
            Codec::Av1 => 1_200_000_000,
        }
    }
}

/// Validate a requested encode resolution before we allocate buffers or open NVENC. Rejects
/// zero/odd-sized and out-of-range modes with a clear error instead of letting buffer math
/// overflow or the encoder open fail with an opaque NVENC code. A client can request any
/// `mode=WxHxFPS`, so this is the gate on attacker/typo-controlled dimensions.
pub fn validate_dimensions(codec: Codec, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be non-zero");
    }
    // NVENC requires even dimensions for the chroma subsampling it does internally.
    if width % 2 != 0 || height % 2 != 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be even");
    }
    let max = codec.max_dimension();
    if width > max || height > max {
        anyhow::bail!(
            "{codec:?} max dimension is {max}px; requested {width}x{height} \
             (use HEVC/AV1 above 4096, or lower the client resolution)"
        );
    }
    Ok(())
}

/// Open a hardware video encoder for frames of the given `format` and mode, selecting the GPU
/// backend for this host: **NVENC** on NVIDIA (Linux/Windows), **VAAPI** on AMD/Intel (Linux).
/// When `cuda` is true the encoder takes GPU frames (`AV_PIX_FMT_CUDA`) from the NVIDIA zero-copy
/// path; otherwise it takes packed RGB/BGR CPU frames (and, on VAAPI, a future dmabuf payload).
/// `format`/`bitrate_bps`/`codec`/mode come from session negotiation; the caller derives `cuda`
/// from the first captured frame's payload. The Linux backend is auto-detected (override:
/// `PUNKTFUNK_ENCODER=auto|nvenc|vaapi`).
#[allow(clippy::too_many_arguments)]
pub fn open_video(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
) -> Result<Box<dyn Encoder>> {
    let inner = open_video_backend(
        codec,
        format,
        width,
        height,
        fps,
        bitrate_bps,
        cuda,
        bit_depth,
        chroma,
    )?;
    // Record what this session encodes on (the mgmt API's "currently used GPU"): the backend label
    // mirrors the dispatch `open_video_backend` just took, the GPU identity is the same selection
    // the capturer was created on ([`crate::gpu::selected_gpu`]). Dropping the returned encoder
    // ends the record, so the live count is correct by construction.
    let backend = resolved_backend_label(cuda);
    let gpu = if backend == "software" {
        crate::gpu::ActiveGpu {
            id: String::new(),
            name: "CPU (openh264)".into(),
            vendor_id: 0,
            backend,
        }
    } else {
        match crate::gpu::selected_gpu() {
            Some(sel) => crate::gpu::ActiveGpu {
                id: sel.info.id,
                name: sel.info.name,
                vendor_id: sel.info.vendor_id,
                backend,
            },
            None => crate::gpu::ActiveGpu {
                id: String::new(),
                name: "GPU".into(),
                vendor_id: 0,
                backend,
            },
        }
    };
    Ok(Box::new(TrackedEncoder {
        inner,
        _session: crate::gpu::session_begin(gpu),
    }))
}

/// The display label of the backend [`open_video_backend`] resolves — kept in lockstep with its
/// dispatch (`windows_resolved_backend` on Windows; the `PUNKTFUNK_ENCODER`/auto match on Linux).
#[cfg(target_os = "windows")]
fn resolved_backend_label(_cuda: bool) -> &'static str {
    match windows_resolved_backend() {
        WindowsBackend::Nvenc => "nvenc",
        WindowsBackend::Amf => "amf",
        WindowsBackend::Qsv => "qsv",
        WindowsBackend::Software => "software",
    }
}

#[cfg(target_os = "linux")]
fn resolved_backend_label(cuda: bool) -> &'static str {
    match crate::config::config().encoder_pref.as_str() {
        "nvenc" | "nvidia" | "cuda" => "nvenc",
        "vaapi" | "amd" | "intel" => "vaapi",
        "software" | "sw" | "openh264" => "software",
        _ => {
            if cuda || !linux_auto_is_vaapi() {
                "nvenc"
            } else {
                "vaapi"
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn resolved_backend_label(_cuda: bool) -> &'static str {
    "none"
}

/// Ties the [`crate::gpu`] live-session record to the encoder's lifetime; pure delegation
/// otherwise.
struct TrackedEncoder {
    inner: Box<dyn Encoder>,
    _session: crate::gpu::ActiveSession,
}

impl Encoder for TrackedEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        self.inner.submit(frame)
    }
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.inner.submit_indexed(frame, wire_index)
    }
    fn caps(&self) -> EncoderCaps {
        self.inner.caps()
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe()
    }
    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.inner.set_hdr_meta(meta)
    }
    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        self.inner.invalidate_ref_frames(first_frame, last_frame)
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.inner.poll()
    }
    fn reset(&mut self) -> bool {
        self.inner.reset()
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

#[allow(clippy::too_many_arguments)]
fn open_video_backend(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
) -> Result<Box<dyn Encoder>> {
    validate_dimensions(codec, width, height)?;
    // Refresh/fps must be positive and sane: fps feeds the encoder time_base (`Rational(1, fps)`)
    // and the pts→ns conversion (`pts * 1e9 / fps`), so 0 builds a 1/0 rational / divides by zero.
    // The mid-stream Reconfigure path already guards `refresh_hz > 0`; enforcing it at this single
    // open chokepoint makes EVERY path (initial Hello, GameStream ANNOUNCE, Reconfigure) safe
    // regardless of which backend opens (security-review 2026-06-28 S5).
    if fps == 0 || fps > 1000 {
        anyhow::bail!("invalid refresh/fps {fps}: must be 1..=1000 Hz");
    }
    // 4:4:4 is HEVC-only. The negotiator should never pass `Yuv444` for another codec (it gates on
    // `codec == H265`), but defend the contract here so a future caller can't silently emit a stream
    // no decoder expects: a non-HEVC 4:4:4 request degrades to 4:2:0 with a warning.
    let chroma = if chroma.is_444() && codec != Codec::H265 {
        tracing::warn!(
            ?codec,
            "4:4:4 requested for a non-HEVC codec — encoding 4:2:0"
        );
        ChromaFormat::Yuv420
    } else {
        chroma
    };
    #[cfg(target_os = "linux")]
    {
        // Pick the GPU encode backend. NVIDIA → NVENC/CUDA (the original path, unchanged);
        // AMD/Intel → VAAPI (one libavcodec backend for both). Auto-detect by default so a single
        // Linux binary serves any GPU; `PUNKTFUNK_ENCODER` forces a specific backend (and surfaces
        // its errors crisply instead of silently trying the other).
        let pref = crate::config::config().encoder_pref.as_str();
        let open_vaapi = || -> Result<Box<dyn Encoder>> {
            vaapi::VaapiEncoder::open(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                bit_depth,
                chroma,
            )
            .map(|e| Box::new(e) as Box<dyn Encoder>)
        };
        match pref {
            "nvenc" | "nvidia" | "cuda" => open_nvenc_probed(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                cuda,
                bit_depth,
                chroma,
            ),
            "vaapi" | "amd" | "intel" => open_vaapi(),
            // GPU-less software H.264 (openh264) — for a headless / GPU-lost box. Explicit-only:
            // `auto` never picks it (a box with `/dev/nvidiactl` present but a dead driver would
            // otherwise wrongly resolve to NVENC). Needs H.264 (openh264 emits only that) and a CPU
            // RGB frame, which the capturer delivers because the software backend resolves `gpu=false`.
            "software" | "sw" | "openh264" => {
                if codec != Codec::H264 {
                    anyhow::bail!(
                        "the software encoder emits H.264 only; the session negotiated {codec:?} \
                         (a client must advertise CODEC_H264 to reach a software host)"
                    );
                }
                let _ = (cuda, bit_depth); // software path is CPU + 8-bit only
                sw::OpenH264Encoder::open(format, width, height, fps, bitrate_bps)
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
            }
            "auto" | "" => {
                // A CUDA frame can ONLY be consumed by NVENC. Otherwise the shared auto decision
                // (manual web-console GPU preference, else the NVIDIA-presence probe) picks the
                // backend — see `linux_auto_is_vaapi`.
                if cuda || !linux_auto_is_vaapi() {
                    open_nvenc_probed(
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        cuda,
                        bit_depth,
                        chroma,
                    )
                } else {
                    open_vaapi()
                }
            }
            other => anyhow::bail!(
                "unknown PUNKTFUNK_ENCODER={other:?} — use auto (default), nvenc, vaapi, or software"
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = cuda; // always false on Windows (no Cuda payload)
                      // NVIDIA → NVENC (direct SDK), AMD → AMF, Intel → QSV (both libavcodec), else → software
                      // H.264. `auto` (the default) resolves from the selected render adapter's vendor.
        let backend = windows_resolved_backend();
        // With `auto` the backend is derived from the selected GPU, so this can only fire when an
        // explicit PUNKTFUNK_ENCODER contradicts the GPU the pipeline sits on (e.g. `nvenc` forced
        // while the web-console preference pins the Intel iGPU) — the open below will then fail on
        // a wrong-vendor device; say why up front instead of leaving an opaque encoder error.
        if let Some(sel) = crate::gpu::selected_gpu() {
            let mismatched = match backend {
                WindowsBackend::Nvenc => sel.info.vendor_id != crate::gpu::VENDOR_NVIDIA,
                WindowsBackend::Amf => sel.info.vendor_id != crate::gpu::VENDOR_AMD,
                WindowsBackend::Qsv => sel.info.vendor_id != crate::gpu::VENDOR_INTEL,
                WindowsBackend::Software => false,
            };
            if mismatched {
                tracing::warn!(
                    adapter = sel.info.name,
                    ?backend,
                    "encoder backend does not match the selected GPU's vendor (explicit \
                     PUNKTFUNK_ENCODER conflicting with the GPU preference?) — the encoder \
                     open will likely fail on this device"
                );
            }
        }
        match backend {
            WindowsBackend::Nvenc => {
                // Hardware path: NVENC over D3D11. The DXGI capturer switches to its zero-copy
                // FramePayload::D3d11 output under the same env var so capture + encode share textures.
                #[cfg(feature = "nvenc")]
                {
                    nvenc::NvencD3d11Encoder::open(
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        bit_depth,
                        chroma,
                    )
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    anyhow::bail!(
                        "NVENC requested/detected but this host was built without it — rebuild \
                         with `--features nvenc`"
                    )
                }
            }
            WindowsBackend::Amf => {
                // AMD: the native AMF SDK encoder, unconditionally (design/native-amf-encoder.md
                // Phase 3). The libavcodec AMF fallback and the `PUNKTFUNK_AMF_FFMPEG` hatch were
                // removed once the native path was validated — two permanently-maintained AMF
                // paths double the driver-matrix burden, and the one kept "for safety" is exactly
                // the one with the wedge/latency pathology. No build feature: amfrt64.dll resolves
                // at runtime like NVENC's DLL. A missing/ancient runtime fails HERE with the
                // "install/update the AMD driver" message `AmfEncoder::open` raises (§6), rather
                // than silently degrading — FFmpeg now serves QSV only.
                amf::AmfEncoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    bit_depth,
                    chroma,
                )
                .map(|e| Box::new(e) as Box<dyn Encoder>)
                .map_err(|e| {
                    e.context(
                        "native AMF encode failed to open (update the AMD driver / amfrt64.dll \
                         runtime)",
                    )
                })
            }
            WindowsBackend::Qsv => {
                // Intel QSV via libavcodec (stays on FFmpeg — design/native-amf-encoder.md §2:
                // async_depth=1 + low_power VDEnc is already near the hardware latency floor).
                #[cfg(feature = "amf-qsv")]
                {
                    ffmpeg_win::FfmpegWinEncoder::open(
                        ffmpeg_win::WinVendor::Qsv,
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        bit_depth,
                        chroma,
                    )
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
                }
                #[cfg(not(feature = "amf-qsv"))]
                {
                    anyhow::bail!(
                        "Intel (QSV) encode requested/detected but this host was built without \
                         it — rebuild with `--features amf-qsv` (needs ffmpeg-next + a FFMPEG_DIR \
                         with the QSV encoders at build time)"
                    )
                }
            }
            WindowsBackend::Software => {
                anyhow::ensure!(
                    codec == Codec::H264,
                    "the Windows software encoder supports H.264 only; client negotiated {codec:?} \
                     (build a GPU backend: --features nvenc or amf-qsv, or request H264)"
                );
                let _ = (bit_depth, chroma); // the software H.264 path is 8-bit 4:2:0 only
                                             // Software H.264 realistically caps far below the negotiated hardware rates.
                const SW_BITRATE_CEIL: u64 = 100_000_000;
                sw::OpenH264Encoder::open(
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps.min(SW_BITRATE_CEIL),
                )
                .map(|e| Box::new(e) as Box<dyn Encoder>)
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
        );
        anyhow::bail!("video encode requires Linux or Windows")
    }
}

/// Open NVENC, probing this GPU's real max bitrate. NVENC rejects `avcodec_open2` with EINVAL
/// when the bitrate exceeds what any codec level can express, and that ceiling is
/// GPU/driver-specific (an RTX 4090 caps HEVC at ~800 Mbps; an RTX 5070 Ti accepts >1 Gbps). So
/// open at the requested rate first and step down ONLY if this GPU refuses it — each GPU then
/// runs at its own actual maximum, and a capable card is never clamped to a conservative guess.
/// The codec's theoretical level ceiling is just the first step-down candidate, not a blind cap.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn open_nvenc_probed(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
) -> Result<Box<dyn Encoder>> {
    const MIN_PROBE_BPS: u64 = 50_000_000;
    let mut candidates = vec![bitrate_bps];
    let cap = codec.max_bitrate_bps();
    if cap < bitrate_bps {
        candidates.push(cap);
    }
    let mut b = bitrate_bps.min(cap);
    while b > MIN_PROBE_BPS {
        b = b * 3 / 4;
        candidates.push(b);
    }
    let mut last: Option<anyhow::Error> = None;
    for (i, &b) in candidates.iter().enumerate() {
        match linux::NvencEncoder::open(
            codec, format, width, height, fps, b, cuda, bit_depth, chroma,
        ) {
            Ok(enc) => {
                if i > 0 {
                    tracing::warn!(
                        requested_mbps = bitrate_bps / 1_000_000,
                        opened_mbps = b / 1_000_000,
                        codec = codec.nvenc_name(),
                        "this GPU's NVENC refused the requested bitrate (EINVAL) — opened at the \
                         highest rate it accepts; request AV1 or a lower bitrate for more"
                    );
                }
                return Ok(Box::new(enc) as Box<dyn Encoder>);
            }
            // EINVAL = above this GPU's level ceiling → step down. Any other failure (no GPU,
            // bad mode, OOM) is real — surface it rather than masking it with bitrate retries.
            Err(e) if format!("{e:#}").contains("Invalid argument") => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("encoder open failed at every probed bitrate")))
}

/// Cheap, side-effect-free NVIDIA-presence probe for the `auto` backend selector: the NVIDIA
/// kernel driver exposes these device nodes, AMD/Intel boxes have neither. Deliberately does NOT
/// create a CUDA context (that would allocate GPU state on every host that merely *might* be
/// NVIDIA). `PUNKTFUNK_ENCODER` overrides this entirely.
#[cfg(target_os = "linux")]
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// The `auto` Linux backend decision, shared by [`open_video`] and [`linux_zero_copy_is_vaapi`]:
/// a manual web-console GPU preference (when that GPU is present — [`crate::gpu::manual_selection`])
/// picks its vendor's backend — AMD/Intel → VAAPI on that GPU's render node, NVIDIA → NVENC (still
/// requiring the proprietary driver's device nodes; a nouveau NVIDIA GPU can't NVENC) — otherwise
/// today's NVIDIA-presence probe, unchanged.
#[cfg(target_os = "linux")]
fn linux_auto_is_vaapi() -> bool {
    if let Some(g) = crate::gpu::manual_selection() {
        if g.vendor_id == crate::gpu::VENDOR_NVIDIA {
            return !nvidia_present();
        }
        return true;
    }
    !nvidia_present()
}

/// True if the Linux GPU encode backend resolves to VAAPI (AMD/Intel) rather than NVENC — mirrors
/// [`open_video`]'s dispatch so the capturer can choose the matching zero-copy path (raw dmabuf
/// passthrough for VAAPI vs the EGL→CUDA import for NVENC).
#[cfg(target_os = "linux")]
pub fn linux_zero_copy_is_vaapi() -> bool {
    match crate::config::config().encoder_pref.as_str() {
        "nvenc" | "nvidia" | "cuda" => false,
        "vaapi" | "amd" | "intel" => true,
        _ => linux_auto_is_vaapi(),
    }
}

/// Which codecs the active GPU can actually ENCODE. Used to build the GameStream codec
/// advertisement so a client never negotiates a codec the GPU can't do (AV1 encode is narrow —
/// Intel Arc/Xe2+, AMD RDNA3+/RDNA4 — so it must be probed, not assumed).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone, Copy, Debug)]
pub struct CodecSupport {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl CodecSupport {
    /// The probed codecs as a `quic::CODEC_*` bitfield, or `None` when the probe found nothing —
    /// meaning the GPU wasn't usable at probe time (GPU-less CI, a wrong-vendor pref), NOT that it
    /// encodes zero codecs; the caller then falls back to the static superset (the native-path
    /// analogue of `gamestream::serverinfo::probed_mask`).
    pub fn wire_mask(self) -> Option<u8> {
        let mut m = 0u8;
        if self.h264 {
            m |= punktfunk_core::quic::CODEC_H264;
        }
        if self.h265 {
            m |= punktfunk_core::quic::CODEC_HEVC;
        }
        if self.av1 {
            m |= punktfunk_core::quic::CODEC_AV1;
        }
        (m != 0).then_some(m)
    }
}

/// Probe the active Linux GPU backend for its encodable codecs (cached; opens a tiny encoder per
/// codec, once). Only the VAAPI (AMD/Intel) backend is probed — NVENC keeps its Moonlight-validated
/// static advertisement (callers gate on [`linux_zero_copy_is_vaapi`]).
#[cfg(target_os = "linux")]
pub fn vaapi_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
    static CACHE: OnceLock<CodecSupport> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let caps = CodecSupport {
            h264: vaapi::probe_can_encode(Codec::H264),
            h265: vaapi::probe_can_encode(Codec::H265),
            av1: vaapi::probe_can_encode(Codec::Av1),
        };
        tracing::info!(
            h264 = caps.h264,
            h265 = caps.h265,
            av1 = caps.av1,
            "VAAPI encode capabilities probed"
        );
        caps
    })
}

/// Whether the active GPU encode backend can actually produce a full-chroma **4:4:4** HEVC stream.
/// Resolved (and cached, once) *before* the Welcome so the host advertises the chroma it will really
/// encode — the honest-downgrade channel. 4:4:4 is HEVC-only; the probe opens a tiny encoder on the
/// active backend (NVENC FREXT is broad on NVIDIA, but VAAPI / AMF / QSV 4:4:4 is hardware-specific,
/// so it must be probed, never assumed). Non-HEVC codecs are always `false`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_444(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if codec != Codec::H265 {
        return false;
    }
    // Cached per selected GPU (was a process-lifetime OnceLock): a web-console preference change
    // re-probes on the newly selected adapter before the next Welcome.
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = crate::gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            // Mirror open_video's backend dispatch: VAAPI (AMD/Intel) vs NVENC (NVIDIA).
            if linux_zero_copy_is_vaapi() {
                vaapi::probe_can_encode_444(codec)
            } else {
                linux::probe_can_encode_444(codec)
            }
        }
        #[cfg(target_os = "windows")]
        {
            match windows_resolved_backend() {
                WindowsBackend::Nvenc => {
                    #[cfg(feature = "nvenc")]
                    {
                        nvenc::probe_can_encode_444(codec)
                    }
                    #[cfg(not(feature = "nvenc"))]
                    {
                        false
                    }
                }
                // AMD: native AMF never encodes 4:4:4 — VCN hardware limit, permanent, no probe
                // needed (design/native-amf-encoder.md §3.5, Phase 3).
                WindowsBackend::Amf => false,
                WindowsBackend::Qsv => {
                    #[cfg(feature = "amf-qsv")]
                    {
                        ffmpeg_win::probe_can_encode_444(ffmpeg_win::WinVendor::Qsv, codec)
                    }
                    #[cfg(not(feature = "amf-qsv"))]
                    {
                        false
                    }
                }
                WindowsBackend::Software => false,
            }
        }
    };
    tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
    cache.lock().unwrap().insert(key, supported);
    supported
}

/// Non-Linux/Windows (the macOS dev/test build of the host — synthetic-source loopback only):
/// no GPU encode backend exists here, so 4:4:4 is never advertised.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_444(_codec: Codec) -> bool {
    false
}

// ---------------------------------------------------------------------------------------------
// Windows backend selection (the analogue of the Linux nvidia_present / linux_zero_copy_is_vaapi
// logic). NVIDIA → NVENC, AMD → AMF, Intel → QSV; `auto` (default) reads the vendor of the
// SELECTED render adapter (crate::gpu — web-console preference / env pin / max VRAM), so the
// backend always matches the GPU the capture ring and virtual display sit on.
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowsBackend {
    Nvenc,
    Amf,
    Qsv,
    Software,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// Resolve the active Windows encode backend from `PUNKTFUNK_ENCODER` (`auto` → the selected
/// render adapter's vendor). Shared by [`open_video`] and the GameStream codec advertisement so
/// both agree.
#[cfg(target_os = "windows")]
pub(crate) fn windows_resolved_backend() -> WindowsBackend {
    // Resolved ONCE in HostConfig (Goal-1) — was re-read from PUNKTFUNK_ENCODER on every call.
    match crate::config::config().encoder_pref.as_str() {
        "nvenc" | "hw" | "nvidia" | "cuda" => WindowsBackend::Nvenc,
        "amf" | "amd" => WindowsBackend::Amf,
        "qsv" | "intel" => WindowsBackend::Qsv,
        "sw" | "software" | "openh264" => WindowsBackend::Software,
        _ => match windows_gpu_vendor() {
            Some(GpuVendor::Nvidia) => WindowsBackend::Nvenc,
            Some(GpuVendor::Amd) => WindowsBackend::Amf,
            Some(GpuVendor::Intel) => WindowsBackend::Qsv,
            None => WindowsBackend::Software,
        },
    }
}

/// True if the active Windows backend's codec advertisement comes from a **real GPU probe**
/// ([`windows_codec_support`]) rather than the NVENC static superset. AMF always qualifies — the
/// native factory probe (`amf::probe_can_encode`) needs no build feature — while QSV still needs
/// the `amf-qsv` (libavcodec) build. Formerly `windows_backend_is_ffmpeg`, renamed when the
/// native AMF probe replaced the ffmpeg open-probe (design/native-amf-encoder.md §4, Phase 2).
#[cfg(target_os = "windows")]
pub fn windows_backend_is_probed() -> bool {
    match windows_resolved_backend() {
        WindowsBackend::Amf => true,
        WindowsBackend::Qsv => cfg!(feature = "amf-qsv"),
        WindowsBackend::Nvenc | WindowsBackend::Software => false,
    }
}

/// Detect the encode-GPU vendor from the **selected render adapter** ([`crate::gpu::selected_gpu`]:
/// web-console preference > `PUNKTFUNK_RENDER_ADAPTER` > max VRAM) — the same adapter the capture
/// ring and the IddCx render pin sit on, so the encoder backend can never disagree with where the
/// captured frames live. The old first-DXGI-adapter scan did exactly that on hybrid boxes: adapter
/// 0 is often the iGPU (e.g. Intel Arc) while capture/encode pin the dGPU — resolving QSV for a
/// pipeline whose textures sit on the NVIDIA card. Uncached: selection is preference-dependent and
/// only consulted at session setup / serverinfo time, never per-frame. Falls back to the first
/// known-vendor adapter when the selected one is an unknown vendor.
#[cfg(target_os = "windows")]
fn windows_gpu_vendor() -> Option<GpuVendor> {
    fn by_id(vendor_id: u32) -> Option<GpuVendor> {
        match vendor_id {
            crate::gpu::VENDOR_NVIDIA => Some(GpuVendor::Nvidia),
            crate::gpu::VENDOR_AMD => Some(GpuVendor::Amd),
            crate::gpu::VENDOR_INTEL => Some(GpuVendor::Intel),
            _ => None,
        }
    }
    let sel = crate::gpu::selected_gpu()?;
    by_id(sel.info.vendor_id).or_else(|| {
        crate::gpu::enumerate()
            .iter()
            .find_map(|g| by_id(g.vendor_id))
    })
}

/// Probe the active Windows AMF/QSV backend for its encodable codecs (cached **per (backend,
/// selected GPU)** — a web-console preference change re-probes on the newly selected adapter
/// instead of serving the old GPU's answer for the process lifetime). Mirrors
/// [`vaapi_codec_support`]; called only when [`windows_backend_is_probed`] is true. AV1 is narrow
/// (AMD RDNA3+, Intel Arc/Xe2+), so it must be probed, not assumed.
///
/// Mirrors the session dispatch (design/native-amf-encoder.md Phase 3): **AMD advertises from the
/// native AMF factory probe alone** (`amf::probe_can_encode`, on the selected adapter — the same
/// path the session opens, so the advertisement can never claim a codec the session can't emit);
/// **Intel/QSV uses the libavcodec probe** (all-`false` without the `amf-qsv` feature, matching a
/// build that cannot open QSV at all).
#[cfg(target_os = "windows")]
pub fn windows_codec_support() -> CodecSupport {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, CodecSupport>>> = OnceLock::new();
    let backend = windows_resolved_backend();
    let key = format!("{backend:?}:{}", crate::gpu::selection_key());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return *c;
    }
    let probe_one = |codec: Codec| -> bool {
        match backend {
            // AMD: the native factory probe is authoritative — it opens exactly the component the
            // session will, so the advertisement matches what the encoder can emit by construction.
            WindowsBackend::Amf => amf::probe_can_encode(codec),
            WindowsBackend::Qsv => {
                #[cfg(feature = "amf-qsv")]
                {
                    ffmpeg_win::probe_can_encode(ffmpeg_win::WinVendor::Qsv, codec)
                }
                #[cfg(not(feature = "amf-qsv"))]
                {
                    false
                }
            }
            // Callers gate on `windows_backend_is_probed` — defensively answer "nothing probed"
            // (the advertisement then falls back to the static superset).
            WindowsBackend::Nvenc | WindowsBackend::Software => false,
        }
    };
    let caps = CodecSupport {
        h264: probe_one(Codec::H264),
        h265: probe_one(Codec::H265),
        av1: probe_one(Codec::Av1),
    };
    tracing::info!(
        ?backend,
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "Windows AMF/QSV encode capabilities probed"
    );
    // A concurrent first call may double-probe; both arrive at the same answer, last insert wins.
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// Stage-W3 encoder session-budget seam (`design/windows-parallel-virtual-displays.md` §4.5):
/// whether one more encode session fits the hardware budget — consulted by the display admission
/// before admitting a parallel display, so an unaffordable display is DECLINED instead of silently
/// degrading a live sibling's encode. NVENC is the only backend with hard session caps today
/// (GeForce consumer limit); AMF/QSV equivalents follow the same seam when they grow accounting.
#[cfg(target_os = "windows")]
pub(crate) fn can_open_another_session() -> bool {
    #[cfg(feature = "nvenc")]
    {
        nvenc::can_open_another_session()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        true
    }
}

// Goal-1 stage 6: GPU/CPU encoders confined to `encode/windows/` (NVENC, native AMF, AMF/QSV
// ffmpeg, software) and `encode/linux/` (NVENC/CUDA + VAAPI); `#[path]` keeps the
// `crate::encode::*` module names flat.
// Native AMF (direct SDK, design/native-amf-encoder.md): compiled unconditionally on Windows —
// no build feature, the driver-installed amfrt64.dll resolves at runtime like NVENC's DLL.
#[cfg(target_os = "windows")]
#[path = "encode/windows/amf.rs"]
mod amf;
#[cfg(all(target_os = "windows", feature = "amf-qsv"))]
#[path = "encode/windows/ffmpeg_win.rs"]
mod ffmpeg_win;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "encode/windows/nvenc.rs"]
mod nvenc;
// Software (openh264) H.264 encoder — the GPU-less path on BOTH Windows and Linux (a headless /
// GPU-less test box, or a fallback when no hardware encoder is available). Platform-agnostic: it
// consumes CPU RGB `CapturedFrame`s and the statically-bundled openh264 build.
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod sw;
#[cfg(target_os = "linux")]
#[path = "encode/linux/vaapi.rs"]
mod vaapi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_odd_dimensions() {
        assert!(validate_dimensions(Codec::H265, 0, 1080).is_err());
        assert!(validate_dimensions(Codec::H265, 1920, 0).is_err());
        assert!(validate_dimensions(Codec::H265, 1921, 1080).is_err()); // odd width
        assert!(validate_dimensions(Codec::H265, 1920, 1081).is_err()); // odd height
    }

    #[test]
    fn h264_capped_at_4096() {
        assert!(validate_dimensions(Codec::H264, 3840, 2160).is_ok()); // 4K fits (width < 4096)
        assert!(validate_dimensions(Codec::H264, 4096, 4096).is_ok()); // exactly at the limit
        assert!(validate_dimensions(Codec::H264, 4098, 2160).is_err());
        assert!(validate_dimensions(Codec::H264, 3840, 4098).is_err());
    }

    #[test]
    fn hevc_and_av1_allow_up_to_8192() {
        for c in [Codec::H265, Codec::Av1] {
            assert!(validate_dimensions(c, 3840, 2160).is_ok());
            assert!(validate_dimensions(c, 7680, 4320).is_ok()); // 8K fits
            assert!(validate_dimensions(c, 8192, 8192).is_ok());
            assert!(validate_dimensions(c, 8194, 4320).is_err());
        }
    }

    #[test]
    fn common_modes_accepted() {
        for c in [Codec::H264, Codec::H265, Codec::Av1] {
            for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440)] {
                assert!(validate_dimensions(c, w, h).is_ok(), "{c:?} {w}x{h}");
            }
        }
    }

    /// The probed-capability → wire-bitfield mapping the native codec advertisement is built from.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn codec_support_wire_mask() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let all = CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        };
        assert_eq!(all.wire_mask(), Some(CODEC_H264 | CODEC_HEVC | CODEC_AV1));
        let hevc_only = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert_eq!(hevc_only.wire_mask(), Some(CODEC_HEVC));
        // An all-false probe means "GPU unusable at probe time", not "zero codecs" — `None` tells
        // the caller to advertise the static superset instead of refusing every handshake.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(none.wire_mask(), None);
    }

    /// Wire round-trip and the stats label stay in lockstep with the `quic::CODEC_*` bits.
    #[test]
    fn codec_wire_roundtrip_and_label() {
        for c in [Codec::H264, Codec::H265, Codec::Av1] {
            assert_eq!(Codec::from_wire(c.to_wire()), c);
        }
        assert_eq!(Codec::H264.label(), "h264");
        assert_eq!(Codec::H265.label(), "hevc");
        assert_eq!(Codec::Av1.label(), "av1");
    }
}
