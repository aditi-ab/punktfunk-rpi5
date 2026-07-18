//! The encoder contract (plan §7, Tier 1): the [`Encoder`] trait plus the plain-data value types its
//! signatures use — [`EncodedFrame`], [`Codec`], [`ChromaFormat`], [`EncoderCaps`] — and the
//! dimension/VBV helpers [`validate_dimensions`] and [`vbv_frames_env`]. Backend selection, the
//! capability probes that mirror it, and `Codec::host_wire_caps` stay in the parent the `pf-encode` crate root
//! facade, which re-exports this module (`pub(crate) use codec::*;`) so every `crate::*` path
//! is unchanged.
use anyhow::Result;
use pf_frame::CapturedFrame;

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
    /// The AU is shard-aligned self-delimiting chunks (see [`Encoder::set_wire_chunking`]);
    /// the session stamps [`punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED`] so the client
    /// windows its parse and may opt into partial delivery. Only the PyroWave backend sets it.
    pub chunk_aligned: bool,
}

/// Codec selection negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    /// PyroWave — the opt-in wired-LAN intra-only wavelet codec (design/pyrowave-codec-plan.md).
    /// Only ever negotiated via the client's explicit `preferred_codec` (never the precedence
    /// ladder) and only emitted by the `pyrowave`-feature backend; every AU is a keyframe.
    PyroWave,
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
            punktfunk_core::quic::CODEC_PYROWAVE => Codec::PyroWave,
            _ => Codec::H265,
        }
    }

    /// The single `quic` codec bit for this codec (echoed in [`punktfunk_core::quic::Welcome::codec`]).
    pub fn to_wire(self) -> u8 {
        match self {
            Codec::H264 => punktfunk_core::quic::CODEC_H264,
            Codec::H265 => punktfunk_core::quic::CODEC_HEVC,
            Codec::Av1 => punktfunk_core::quic::CODEC_AV1,
            Codec::PyroWave => punktfunk_core::quic::CODEC_PYROWAVE,
        }
    }

    /// Lowercase stats/console label (`"h264"` / `"hevc"` / `"av1"`) — the codec string seeded into
    /// the web console's session meta (the host `stats_recorder::StatsRecorder::register_session`).
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "av1",
            Codec::PyroWave => "pyrowave",
        }
    }

    /// Whether this codec has a negotiable **10-bit** encode path (HEVC Main10 / AV1 10-bit;
    /// PyroWave rides 16-bit UNORM planes carrying P010-style studio codes — the wavelet is
    /// depth-agnostic, design/pyrowave-444-hdr.md). H.264 is always 8-bit (High10 is neither an
    /// NVENC nor a VCN encode mode — negotiation never asks). `true` here is only the
    /// *codec-level* gate: the active GPU/backend must still pass
    /// [`can_encode_10bit`](crate::can_encode_10bit) before the host negotiates 10-bit.
    pub fn supports_10bit(self) -> bool {
        matches!(self, Codec::H265 | Codec::Av1 | Codec::PyroWave)
    }

    /// The FFmpeg NVENC encoder name (selected by name, not codec id — the latter would
    /// pick the software encoder).
    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
            // Guarded by the open_video dispatch: a PyroWave session never reaches a
            // libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
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
            // Guarded by the open_video dispatch: a PyroWave session never reaches a
            // libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }

    /// The FFmpeg AMD **AMF** encoder name (the Windows AMD backend). Selected by name (the codec id
    /// would pick the software encoder). AV1 (`av1_amf`) is RDNA3+/RX 7000+ — probe, never assume.
    pub fn amf_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_amf",
            Codec::H265 => "hevc_amf",
            Codec::Av1 => "av1_amf",
            // Guarded by the open_video dispatch: a PyroWave session never reaches a
            // libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }

    /// The FFmpeg Intel **QSV** encoder name (the Windows Intel backend). Selected by name. AV1
    /// (`av1_qsv`) is Arc/Xe2+; HEVC Main10 is Gen9.5+ — probe, never assume.
    pub fn qsv_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_qsv",
            Codec::H265 => "hevc_qsv",
            Codec::Av1 => "av1_qsv",
            // Guarded by the open_video dispatch: a PyroWave session never reaches a
            // libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
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
    /// Retarget the encoder's rate control to `bps` (average == max, CBR) **in place** — same
    /// codec/resolution/fps, only the bitrate and its derived VBV move. Returns `true` when the
    /// live encoder accepted the change: the reference chain, the in-flight frames and the
    /// caller's wire-index prediction all survive, so an adaptive-bitrate step costs *nothing* on
    /// the wire (no IDR, no in-flight forfeit — the whole point vs. a rebuild). `false` = the
    /// backend can't (or the driver rejected the new rate, e.g. above the codec-level ceiling) —
    /// the caller falls back to its full rebuild path, which also owns the bitrate clamping.
    /// Default: no in-place retarget (the libavcodec/software paths).
    fn reconfigure_bitrate(&mut self, _bps: u64) -> bool {
        false
    }
    /// Wire-chunk the encoder's AUs at the session's shard payload size (the PyroWave
    /// datagram-aligned mode, plan §4.4): every `shard_payload` window of the emitted AU
    /// starts a fresh self-delimiting codec packet, zero-padded to the window — so a lost
    /// datagram costs a few coefficient blocks, not the frame. AUs produced this way are
    /// flagged [`EncodedFrame::chunk_aligned`] and the session marks them on the wire.
    /// Default: no-op (the H.26x backends' bitstreams cannot be cut losslessly).
    fn set_wire_chunking(&mut self, _shard_payload: usize) {}
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
            // PyroWave has no codec-level dimension cap (arbitrary even sizes); 8192 matches the
            // buffer-math guard the other codecs get.
            Codec::H265 | Codec::Av1 | Codec::PyroWave => 8192,
        }
    }

    /// The codec's *spec* top level/tier bitrate (bits/s) — the usual boundary at which NVENC
    /// starts rejecting `avcodec_open2` with EINVAL. NOT a hard cap: [`open_video`](crate::
    /// open_video) probes the actual GPU ceiling by stepping DOWN from the requested bitrate only on
    /// EINVAL, and uses this purely as the first step-down candidate (so a card that accepts more —
    /// an RTX 5070 Ti does >1 Gbps HEVC where a 4090 caps at ~800 Mbps — is never clamped to it).
    /// HEVC Level 6.2 High tier = 800 Mbps; H.264 High level 6.2 ≈ 480 Mbps; AV1's levels allow more.
    pub fn max_bitrate_bps(self) -> u64 {
        match self {
            Codec::H264 => 480_000_000,
            Codec::H265 => 800_000_000,
            Codec::Av1 => 1_200_000_000,
            // No spec level/tier: the rate is a plain per-frame byte budget. Use the protocol's
            // own bitrate clamp so the step-down probe logic never binds below it.
            Codec::PyroWave => 8_000_000_000,
        }
    }
}

/// `PUNKTFUNK_VBV_FRAMES` — HRD/VBV size in frame intervals (default 1.0, the strict low-latency
/// shape every backend ships: each frame must fit its rate share, keeping frame sizes uniform for
/// the pacer). The AMF/VAAPI/QSV paths parse the same variable locally; this helper brings the
/// direct-NVENC paths (which used to hardwire 1 frame) to parity. Larger values let complex
/// frames borrow bits — better rate utilization at the cost of per-frame size variance.
pub(crate) fn vbv_frames_env() -> f64 {
    std::env::var("PUNKTFUNK_VBV_FRAMES")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0)
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
    // PyroWave's 5-level wavelet decomposition needs ≥ 4·2⁵ px per axis (upstream
    // `MinimumImageSize` — the band mirroring breaks below it); reject a tiny mode here
    // (e.g. a match-window resize dragged to a sliver) instead of failing the encoder
    // rebuild after the switch was acked.
    if codec == Codec::PyroWave && (width < 128 || height < 128) {
        anyhow::bail!(
            "invalid PyroWave resolution {width}x{height}: the wavelet needs at least 128px per axis"
        );
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
