//! The encoder contract (plan §7, Tier 1): the [`Encoder`] trait plus the plain-data value types its
//! signatures use — [`EncodedFrame`], [`Codec`], [`ChromaFormat`], [`EncoderCaps`] — and the
//! dimension/VBV helpers [`validate_dimensions`] and [`vbv_frames_env`]. Backend selection, the
//! capability probes that mirror it, and `Codec::host_wire_caps` stay in the parent the `pf-encode` crate root
//! facade, which re-exports this module (`pub(crate) use codec::*;`) so every `crate::*` path
//! is unchanged.
use anyhow::Result;
use pf_frame::CapturedFrame;

/// Whether an encoder fed `format` must be built 10-bit — decided by **the pixels that actually
/// arrive**, never by the negotiated `bit_depth`.
///
/// The three Windows backends each derived this as `bit_depth >= 10 || matches!(format, P010 |
/// Rgb10a2)`, i.e. the *negotiated* depth could force a 10-bit encoder over an 8-bit capture. That
/// combination is not hypothetical: a client advertises 10-bit, the handshake negotiates
/// `bit_depth = 10`, and then enabling advanced colour on the IDD virtual display fails — at which
/// point the capturer says so and delivers 8-bit NV12 anyway (`pf-capture`'s idd_push logs "10-bit
/// HDR was negotiated but enabling advanced color on the virtual display FAILED — encoding 8-bit
/// SDR"). Every backend then lost the session, each in its own way: native AMF and native QSV
/// `bail!` at open because the format does not match the P010 they derived, and the libavcodec
/// path accepted the open and then failed EVERY submit forever (its per-frame depth check
/// recomputes from the frame, which never matches), where `reset()` could not help because the
/// rebuild re-derived the same wrong depth.
///
/// Following the pixels keeps the stream alive and, more importantly, keeps it HONEST: the depth
/// also selects the colour signalling (BT.2020 PQ vs BT.709) and the staging surface format, so an
/// 8-bit capture now yields an 8-bit stream that says it is SDR — which is what the capturer
/// already reported it is sending. The negotiated depth remains an upper bound; the session label
/// may still claim HDR, and that mismatch belongs to the negotiation, not to the encoder.
/// Windows-only: the three backends that derive an encoder depth from a capture live there
/// (native AMF, native QSV, libavcodec AMF/QSV). The Linux backends take the depth from the
/// negotiated `bit_depth` alone because their capture formats carry it unambiguously.
#[cfg(target_os = "windows")]
pub(crate) fn ten_bit_input(format: pf_frame::PixelFormat, negotiated_depth: u8) -> bool {
    use pf_frame::PixelFormat;
    let ten = matches!(format, PixelFormat::P010 | PixelFormat::Rgb10a2);
    if negotiated_depth >= 10 && !ten {
        tracing::warn!(
            ?format,
            negotiated_depth,
            "session negotiated 10-bit but the capturer delivers an 8-bit format — encoding 8-bit \
             SDR (the stream's colour signalling follows the pixels; check whether advanced colour \
             failed to enable on the virtual display)"
        );
    }
    ten
}

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

/// One slice-boundary chunk of an encoded AU, emitted by a chunked-poll backend
/// ([`Encoder::poll_chunk`], latency plan §7 LN1): the encoder hands out completed slices while
/// the rest of the frame is still encoding, so packetize/FEC/pacing can overlap the encode tail.
/// The chunks of one AU concatenate to exactly the bytes [`Encoder::poll`] would have returned,
/// and every cut lands on an Annex-B NAL boundary (slice starts). AU-level metadata
/// (`pts_ns`/`keyframe`/`recovery_anchor`/`chunk_aligned`) is authoritative on the FIRST chunk
/// (`first`) — the host opens the wire frame from it; `last` closes the AU. `keyframe` on a
/// non-final chunk is the encoder's own prediction (exact under the P-only/infinite-GOP config —
/// the driver only ever emits an IDR we asked for); the final chunk re-checks it against the
/// driver's reported picture type.
pub struct AuChunk {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    pub keyframe: bool,
    /// See [`EncodedFrame::recovery_anchor`].
    pub recovery_anchor: bool,
    /// See [`EncodedFrame::chunk_aligned`].
    pub chunk_aligned: bool,
    /// Opens the AU (carries the authoritative AU metadata).
    pub first: bool,
    /// Closes the AU (the concatenation is complete; the encoder's in-flight slot is released).
    pub last: bool,
}

impl AuChunk {
    /// A whole AU as a single self-closing chunk — what every non-chunked backend's
    /// [`Encoder::poll_chunk`] default emits, so a chunk consumer needs no per-backend fork.
    pub fn whole(f: EncodedFrame) -> Self {
        AuChunk {
            data: f.data,
            pts_ns: f.pts_ns,
            keyframe: f.keyframe,
            recovery_anchor: f.recovery_anchor,
            chunk_aligned: f.chunk_aligned,
            first: true,
            last: true,
        }
    }
}

/// Codec selection negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Static capabilities an [`Encoder`] declares so the session glue routes loss-recovery and
/// cursor plumbing by *query* rather than relying on a method's no-op/`false` default. Cheap
/// `Copy`; fixed for the session (an HDR toggle re-initialises the encoder — re-query if that
/// matters).
///
/// (There is deliberately NO `supports_hdr_metadata` cap: in-band HDR SEI/OBU embedding needs no
/// host-side routing — every first-party client reads the static grade exclusively out-of-band
/// (the native 0xCE datagram / the GameStream 0x010e control message), both planes send it
/// unconditionally, and the in-band grade is a decoder-side bonus for stock clients. A cap field
/// nothing reads is a contract nobody honors; it was deleted after shipping write-only.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderCaps {
    /// The encoder can perform real reference-frame invalidation — i.e.
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) can return `true`. When `false`
    /// the caller skips that always-`false` call and forces a keyframe directly on loss recovery.
    /// Two backends implement RFI: Windows direct-NVENC (`nvEncInvalidateRefFrames`) and native
    /// AMF (user-LTR force-reference, when the driver accepted the LTR slots at open). The
    /// libavcodec paths (Linux NVENC, VAAPI, QSV) can't express it and always keyframe.
    pub supports_rfi: bool,
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
    /// The encoder composites [`CapturedFrame::cursor`] into the picture it encodes.
    ///
    /// `open_video`'s `cursor_blend` argument is a REQUEST, and for most of this crate's life it was
    /// nothing else: `lib.rs` literally did `let _ = cursor_blend;` and only three backends ever read
    /// `frame.cursor`. So a session could ask for a composited pointer, get a backend that silently
    /// discards it, and stream with no mouse cursor at all — the confirmed symptom on the VAAPI
    /// dmabuf path and the libav-NVENC CUDA path.
    ///
    /// This makes the answer queryable instead of assumed. It is deliberately a plain fact about the
    /// encoder, not a policy: what to DO when a session wants blending and the backend cannot is the
    /// host's call, since only the host can re-plan capture. That call is wired now — the
    /// negotiation consults the pre-open mirror ([`cursor_blend_capable`](crate::cursor_blend_capable))
    /// to gate the cursor channel and to keep capture on embedded-cursor / CSC-capable shapes for
    /// any backend that can't blend; `open_video`'s post-open check remains as the backstop for
    /// open-time fallbacks the plan can't see.
    pub blends_cursor: bool,
}

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    /// Submit one captured frame for encoding. Lifetime contract: the caller must keep `frame`
    /// (and its GPU payload) alive until this frame's AU has been returned by
    /// [`poll`](Self::poll) — a stream-ordered backend (Linux direct-NVENC's IO-stream binding)
    /// may still be reading the payload asynchronously after `submit` returns. Both host encode
    /// loops already hold the frame across their poll drain; new callers must do the same.
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
    /// This encoder's static [capabilities](EncoderCaps) (RFI, intra-refresh, chroma, cursor
    /// blending), so the session glue can route by query rather than rely on the no-op/`false`
    /// defaults of methods like [`invalidate_ref_frames`](Self::invalidate_ref_frames).
    /// Default: no optional capabilities (the software / libavcodec backends).
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    /// Force the next submitted frame to be an IDR keyframe (e.g. after a client
    /// reference-frame-invalidation request). Default: no-op.
    fn request_keyframe(&mut self) {}
    /// Set the source's static HDR mastering metadata (from the capturer). An HDR encoder emits it
    /// in-band (HEVC/H.264 `mastering_display_colour_volume` + `content_light_level_info` SEI, or
    /// AV1 metadata OBUs) on keyframes so a stock decoder — e.g. stock Moonlight — tone-maps from
    /// the source's real grade. Default: no-op (SDR encoders / paths that don't attach it).
    /// Cheap to call every frame; consumed by Windows direct-NVENC, native AMF, and native QSV.
    /// Every first-party client reads the grade out-of-band (the 0xCE datagram) regardless, so
    /// this is a bonus for stock decoders, never the primary channel.
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
    /// Escalate into a pipelined (two-thread) retrieve mode under sustained GPU contention — the
    /// encoder analog of the capturer depth escalation: AUs ride ~one loop tick behind (`poll`
    /// may return `None` while an encode is in flight) in exchange for capture/submit no longer
    /// serializing on the encode wait. Returns whether pipelined retrieve is (now) active; the
    /// switch may be deferred to the next safe point internally. `set_pipelined(true)` returning
    /// `false` (the default impl) = unsupported — the session loop stops asking.
    ///
    /// `set_pipelined(false)` requests the wind-back (de-escalation, latency recovery): the
    /// backend restores its sync-retrieve mode — and the latency features that mode carries
    /// (IO-stream binding, sub-frame chunking) — at its next safe point, usually via a session
    /// rebuild whose first frame is an IDR. The return is still "is pipelined retrieve active":
    /// the caller polls until it reads `false`. Backends that never escalate return `false`
    /// trivially. An operator pin (`PUNKTFUNK_NVENC_ASYNC=1`) refuses the wind-back.
    fn set_pipelined(&mut self, _on: bool) -> bool {
        false
    }
    /// Pull the next encoded AU if one is ready.
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
    /// Whether [`poll_chunk`](Self::poll_chunk) currently emits sub-AU chunks — i.e. the LIVE
    /// session has slice-level readback armed (Linux direct-NVENC with the
    /// `PUNKTFUNK_NVENC_SLICES` and `PUNKTFUNK_NVENC_SUBFRAME` knobs on a sync depth-1
    /// retrieve). Dynamic, not static: a pipelined-retrieve escalation or a session rebuild can
    /// turn it off — re-query per AU, never cache across frames. `false` (the default) means
    /// `poll_chunk` degrades to one whole-AU chunk per frame.
    fn supports_chunked_poll(&self) -> bool {
        false
    }
    /// Pull the next slice-boundary chunk of the oldest in-flight AU (latency plan §7 LN1).
    /// Semantics when chunking is live: BLOCKS until the next chunk is readable, and the final
    /// (`last`) chunk blocks exactly like [`poll`](Self::poll) does — the depth-1 pump treats
    /// `None` as re-poll-next-tick, so a non-blocking tail would ride the AU one tick late (the
    /// `6dc195f9` Vulkan bug class). `Ok(None)` only when no AU is in flight. Each AU must be
    /// drained through ONE method: calling `poll` on a partially-chunked AU is a caller bug (the
    /// backend errors rather than double-emit bytes). Default: delegates to `poll`, wrapping the
    /// whole AU as a single `first && last` chunk.
    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        Ok(self.poll()?.map(AuChunk::whole))
    }
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
    /// The bitrate (bps) the encoder is ACTUALLY running at (or will open at, for a lazily-opened
    /// backend) — the encoder-side truth after any internal clamp, e.g. the direct-NVENC
    /// codec-level ceiling search. The session loop reads this after every open/reconfigure and
    /// stores IT, not the requested rate, as the live bitrate — so the send pacer, the console
    /// and the client controller's ack all track what the ASIC really targets (a controller fed
    /// the requested rate keeps climbing from a phantom base, §ABR overdrive). `None` (the
    /// default) = the backend doesn't track an applied rate; the caller keeps the requested one.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        None
    }
    /// Wire-chunk the encoder's AUs at the session's shard payload size (the PyroWave
    /// datagram-aligned mode, plan §4.4): every `shard_payload` window of the emitted AU
    /// starts a fresh self-delimiting codec packet, zero-padded to the window — so a lost
    /// datagram costs a few coefficient blocks, not the frame. AUs produced this way are
    /// flagged [`EncodedFrame::chunk_aligned`] and the session marks them on the wire.
    /// Default: no-op (the H.26x backends' bitstreams cannot be cut losslessly).
    fn set_wire_chunking(&mut self, _shard_payload: usize) {}
    /// How long a whole AU's packets currently take to leave the socket (µs, smoothed) — the
    /// host's paced-send `spread_us`.
    ///
    /// Exists for ONE decision, and only the host can supply it. The Linux direct-NVENC split
    /// arbitration compares single-engine against split, but on HEVC engaging split costs
    /// sub-frame readback, and sub-frame's whole value is that the send overlaps the encode. So
    /// the real comparison is `encode_1eng + send_of_last_slice` against
    /// `encode_2eng + send_of_whole_AU`, and an encoder that measures only encode time would
    /// reliably pick split and make end-to-end latency WORSE. The backend turns this number into
    /// that handicap (it knows its own slice count); the host just reports what it observes.
    ///
    /// Optional by design: a backend that ignores it simply never arbitrates the sub-frame trade,
    /// which is the safe direction. `0` = unknown / not reported yet.
    fn set_send_spread_us(&mut self, _us: u32) {}
    /// How many frames the CAPTURER guarantees the encoder may hold in flight before it starts
    /// reusing an input texture (`Capturer::pipeline_depth`). Backends that encode the capturer's
    /// textures IN PLACE — no `CopyResource` — must not pipeline deeper than this: the capturer
    /// rotates its output ring per delivered frame with no regard for encode completion, so a
    /// deeper pipeline lets it overwrite a texture mid-encode. That is visual corruption (torn or
    /// mixed frames), not UB, so it fails silently and intermittently.
    ///
    /// Called once by the session glue after the capturer is known; a backend that copies its
    /// input, or is synchronous, ignores it. Default: no-op.
    fn set_input_ring_depth(&mut self, _depth: usize) {}
    /// Signal end-of-stream. After this, drain the remaining AUs with [`poll`](Self::poll)
    /// until it returns `None` — NVENC buffers frames internally even at `delay=0`.
    ///
    /// **The two production encode loops deliberately do not call this**, and that is not an
    /// oversight to be "fixed" by a later sweep. Both reach their exit only after the transport is
    /// already gone (the client disconnected, or the session was stopped), so the AUs a flush would
    /// recover have nowhere to go — while flushing is the one call on this trait that can BLOCK on a
    /// wedged encoder, on precisely the teardown path a stopped session needs to complete promptly.
    /// The Linux direct-SDK NVENC backend makes that concrete: its retrieve-thread join is untimed
    /// (see the note in `enc/linux/nvenc_cuda.rs`), so a flush there could hang a session that is
    /// already ending.
    ///
    /// It is kept rather than deleted because it does have real consumers: the `spike` dev
    /// subcommand, which encodes a FINITE clip and genuinely wants the tail, and the `#[ignore]`d
    /// hardware smoke tests across the backends, which assert the drain contract on real GPUs.
    /// Those are finite-stream users; a live session is not one.
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

/// Pixel rate (luma samples/s) at or above which NVENC split-frame encoding is FORCED 2-way —
/// one number shared by the direct-SDK selector ([`resolve_split_mode`]) and the libav
/// `split_encode_mode` option author (`linux::NvencEncoder`), so the two paths can never disagree
/// about which modes split. A single NVENC engine tops out ~1 Gpix/s on HEVC, and AUTO doesn't
/// engage below ~2112 px height, so the sessions that need the second engine must be forced. Set
/// BELOW 1 Gpix/s deliberately: 4K120 — the mode this threshold exists for — is 3840×2160×120 =
/// 995,328,000, which a `> 1_000_000_000` gate missed by 0.47% and left on AUTO (pinned ~107 fps
/// on a 4090). 950 M keeps margin for fractional refresh rates while leaving 1440p240 (884.7 M,
/// comfortably single-engine) on AUTO.
pub const SPLIT_FORCE_PIXEL_RATE: u64 = 950_000_000;

/// The `NV_ENC_SPLIT_ENCODE_MODE` values, as plain constants.
///
/// They live HERE, not in `nvenc_core`, because the split policy below has to be shared with the
/// **libav** NVENC path — which compiles with the `nvenc` feature OFF (that is the whole
/// `PUNKTFUNK_NVENC_DIRECT=0` / featureless-package build), where the SDK enum does not exist.
/// One policy, no drift, was the point of extracting it; gating it behind the feature would have
/// left the libav copy free to diverge again, which is exactly what it had already done.
///
/// `nvenc_split_constants_match_the_sdk` (feature-gated) pins these against the real enum, so the
/// hand-written values cannot rot.
pub(crate) const SPLIT_AUTO: u32 = 0;
pub(crate) const SPLIT_AUTO_FORCED: u32 = 1;
pub(crate) const SPLIT_TWO_FORCED: u32 = 2;
pub(crate) const SPLIT_THREE_FORCED: u32 = 3;
pub(crate) const SPLIT_DISABLE: u32 = 15;

/// Resolved NVENC split-frame encode mode for a session — ONE selector shared by the Windows and
/// Linux direct-SDK backends (they had drifted into byte-identical duplicates, one of which
/// logged and one didn't). Precedence:
/// 1. `PUNKTFUNK_SPLIT_ENCODE` = `0`/`disable` | `1`/`auto` (AUTO_FORCED) | `2` | `3` — operator
///    override, always wins, except that `2`/`3` are clamped to the GPU's real engine count (see
///    [`clamp_to_engines`]; the driver honours an over-ask and silently encodes narrower).
/// 2. Pixel rate ≥ [`SPLIT_FORCE_PIXEL_RATE`] → force the WIDEST split the GPU can deliver
///    ([`max_forced_split_mode`]), not a hard-coded 2 (AUTO never engages below ~2112 px height,
///    so 4K120 must be forced onto the other engines; and a 3-NVENC part left at 2-way wastes a
///    third of its encode silicon).
/// 3. **HEVC** Main10 below that bar → DISABLE: 2-way split measured SLOWER on Ada for Main10 — at
///    5120×1440@240 forced-2 took 7.6 ms/frame (~131 fps) vs 2.8 ms (~357 fps) single-engine, the
///    "broken animations in HDR" cap. ⚠ This rule used to sit ABOVE the pixel-rate arm and take no
///    codec, so it (a) vetoed 10-bit **4K120** — the very case the pixel-rate arm exists for — and
///    (b) applied an HEVC-on-Ada result to **AV1 10-bit**, which has no such measurement. Both
///    fixed; what remains is a conservative default in the regime where a second engine buys
///    nothing anyway.
///    ⚠⚠ **UNVALIDATED CONSEQUENCE:** 5120×1440@240 Main10 (1.77 Gpix/s) now clears the pixel-rate
///    bar and WILL be forced to split — i.e. the exact configuration that measurement came from
///    flips behaviour. That is deliberate (the datapoint is one sample, at low bits/frame, and the
///    bits/frame hypothesis predicts it should not generalise) but it is **the first thing to
///    re-measure on Ada**; `PUNKTFUNK_SPLIT_ENCODE=0` is the escape if it regresses.
/// 4. Else AUTO — ⚠ whose behaviour is **conditional on sub-frame**, measured on `.21` at 4K:
///    - sub-frame **ON** (the fleet default): AUTO **does not split** — 5023/5157 µs against
///      DISABLE's 4979/5000. Split and sub-frame are mutually unsupported for HEVC, so the driver
///      resolves AUTO to no-split and this arm silently means DISABLE.
///    - sub-frame **OFF**: AUTO **does split** — 2401/2352 µs against TWO_FORCED's 2319/2378.
///
///    So AUTO is NOT dead in general and must not be retired: doing so would lose a real split on
///    every sub-frame-off session. It is dead only in the sub-frame-on combination, which
///    [`resolve_split_subframe`] logs rather than silently accepting.
///
/// The caller still owns the rejection fallback (retry split-disabled) — a codec/config that
/// rejects the chosen mode downgrades at open, not here.
///
/// `engines` is the GPU's `NV_ENC_CAPS_NUM_ENCODER_ENGINES`; pass `0` when it could not be probed
/// (treated as "unknown", which keeps the pre-probe behaviour of assuming a second engine exists
/// and letting the open-time rejection fallback sort it out).
pub(crate) fn resolve_split_mode(
    codec: Codec,
    bit_depth: u8,
    pixel_rate: u64,
    engines: u32,
) -> u32 {
    let hw_max = max_forced_split_mode(engines);
    let mode = match std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok().as_deref() {
        Some("0") | Some("disable") => SPLIT_DISABLE,
        Some("1") | Some("auto") => SPLIT_AUTO_FORCED,
        Some("3") => clamp_to_engines(SPLIT_THREE_FORCED, hw_max, engines),
        Some("2") => clamp_to_engines(SPLIT_TWO_FORCED, hw_max, engines),
        // Use every engine the card has, not a hard-coded two: on a 3-NVENC part (GB202, AD102
        // workstation) forcing 2 leaves a third of the silicon idle.
        //
        // ⚠ This arm now comes FIRST, ahead of the 10-bit rule. That reordering is the D1 fix: a
        // 10-bit 4K120 session (995.3 Mpix/s) used to be vetoed by the depth rule before ever
        // reaching the pixel-rate arm written for exactly it.
        _ if pixel_rate >= SPLIT_FORCE_PIXEL_RATE => hw_max,
        // Below that bar, HEVC Main10 keeps the conservative single-engine default. The one Ada
        // measurement we have says split can be *slower* for Main10, and nothing under this bar
        // needs a second engine anyway — so the cost of being wrong here is ~nil, unlike above it.
        //
        // ⚠ Now codec-scoped (the D2 fix): the measurement behind this was HEVC Main10 on Ada, and
        // it used to veto **AV1 10-bit** too, which has neither the sub-frame conflict nor any
        // measurement against it.
        _ if codec == Codec::H265 && bit_depth >= 10 => SPLIT_DISABLE,
        _ => SPLIT_AUTO,
    };
    tracing::debug!(
        split_mode = mode,
        ?codec,
        bit_depth,
        pixel_rate,
        engines,
        "NVENC split-encode mode selected"
    );
    mode
}

/// The strongest split mode this GPU's engine count can actually deliver.
///
/// ⚠ **The driver will NOT tell you when you over-ask.** Measured on `.21` (RTX 5070 Ti, 2 NVENC,
/// driver 610.57.04, 4K HEVC): requesting `THREE_FORCED` was **HONOURED** — session opened in mode
/// 3 — and ran at **2303 µs/frame, identical to `TWO_FORCED`'s 2308**. No rejection, no warning,
/// no third engine; just a log line claiming 3-way over a 2-way encode. So the rejection fallback
/// cannot be relied on to find the ceiling and the clamp has to happen here.
///
/// `NV_ENC_SPLIT_ENCODE_MODE` can only *name* counts up to three (SDK 0.4.0 / NVENCAPI 12.1;
/// values 4..14 are unallocated, so a future API may extend it). Above that we fall back to
/// `AUTO_FORCED` = "split, driver picks how many", which measurably does force a split (2.01× vs
/// disabled on the same box) and is the only way to express "use everything you have".
pub(crate) fn max_forced_split_mode(engines: u32) -> u32 {
    match engines {
        // Unknown (cap unreadable / not probed): keep the historical assumption of a second
        // engine and let the open-time rejection fallback correct it.
        0 => SPLIT_TWO_FORCED,
        1 => SPLIT_DISABLE,
        2 => SPLIT_TWO_FORCED,
        3 => SPLIT_THREE_FORCED,
        // More engines than the enum can name — let the driver use them all.
        _ => SPLIT_AUTO_FORCED,
    }
}

/// The N of an N-way FORCED split, or `None` for the modes that do not name a width
/// (`DISABLE`, plain `AUTO`, and `AUTO_FORCED` — the last forces a split but lets the driver
/// choose how wide).
///
/// For callers that can only express "split this many ways" and have no vocabulary for our other
/// modes — the libav path, whose `split_encode_mode` AVOption is libavcodec's own enum, not the
/// NVENC one (our `DISABLE` is `15`, which would be meaningless there).
pub(crate) fn forced_split_width(mode: u32) -> Option<u32> {
    match mode {
        m if m == SPLIT_TWO_FORCED => Some(2),
        m if m == SPLIT_THREE_FORCED => Some(3),
        _ => None,
    }
}

/// Hold an operator's `PUNKTFUNK_SPLIT_ENCODE=2|3` to what the hardware can deliver, loudly.
/// Without this the knob silently lies (see [`max_forced_split_mode`]); an override that asks for
/// more engines than exist is a mistake worth surfacing, not honouring.
pub(crate) fn clamp_to_engines(requested: u32, hw_max: u32, engines: u32) -> u32 {
    // Only the named N-way modes are ordered; `hw_max` may be AUTO_FORCED (1) on a >3-engine part,
    // which is not "less than" TWO_FORCED and must not clamp a legitimate request down.
    let named = |m: u32| (2..=3).contains(&m);
    if engines != 0 && named(requested) && named(hw_max) && requested > hw_max {
        tracing::warn!(
            requested,
            engines,
            using = hw_max,
            "PUNKTFUNK_SPLIT_ENCODE asks for more NVENC engines than this GPU has — clamping. \
             (The driver would ACCEPT the over-ask and silently encode with fewer, so the log \
             would otherwise claim a split width that never happened.)"
        );
        return hw_max;
    }
    requested
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

/// The same HRD/VBV window as [`vbv_frames_env`], expressed the way the Vulkan Video encode API
/// wants it: `(virtualBufferSizeInMs, initialVirtualBufferSizeInMs)`.
///
/// Every other backend states the window in **bits** (`bitrate / fps × frames`); Vulkan states it
/// in **milliseconds**. `vulkan_video.rs` consumes this ONLY when the driver advertises VBR
/// (WP6.3): a tight window under CBR makes the driver stuff underspent frames with filler NALs up
/// to the exact rate share — measured 97 % filler on the 780M — because CBR must keep the CPB from
/// overflowing and Vulkan exposes no filler-suppression control. VBR permits the underspend, so
/// the tight window only ever *bounds* a complex frame.
///
/// The initial fill stays at half the window, preserving the RATIO the hardcoded (1000, 500)
/// pair had — the direct-NVENC house shape uses a FULL-window initial fill instead; measured on
/// RADV the difference is inert (the firmware showed no window sensitivity at all). Both
/// VUIDs on `VkVideoEncodeRateControlInfoKHR`'s window fields are satisfied by construction: the
/// window clamps to `>= 1` so it is non-zero, and `window / 2 <= window` always
/// (`VUID-...-08358` is `<=`, relaxed in Vulkan 1.3.299).
///
/// Carries its only caller's gate: `vulkan_video.rs` is the sole ms-form consumer, and with the
/// crate-wide `allow(dead_code)` gone (WP0.3) an item unused in ANY feature combination is a hard
/// error — this is dead on every Windows leg.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
pub(crate) fn vbv_window_ms(fps: u32) -> (u32, u32) {
    let frames = vbv_frames_env();
    let ms = (frames * 1000.0 / fps.max(1) as f64).round();
    // `f64 as u32` saturates at the bounds in Rust, so an absurd `PUNKTFUNK_VBV_FRAMES` cannot wrap.
    let window = (ms as u32).max(1);
    (window, window / 2)
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
    // PyroWave's vendored rate controller packs the 32×32 block index into the low 16 bits of
    // `RDOperation::block_offset_saving` (pyrowave-sys `patches/0002-rdo-saving-clamp.patch`).
    // Past `u16::MAX` blocks the index collides with the `saving` field, the resolve over-credits,
    // and the emitted payload can overshoot the buffer `pyrowave_encoder_packetize` writes into —
    // whose only bounds check is an `assert` that the Release (NDEBUG) vendored build compiles out.
    // So this is a hard cap, not a quality knob.
    //
    // Checked against 4:2:0, the *most permissive* chroma: a mode that cannot fit even there can
    // fit no PyroWave session at all, so it belongs at this single chokepoint (which both the
    // negotiator and `open_video_backend` run) rather than only in the per-backend opens. 4:4:4
    // has twice the block count and is checked again at open, where the real chroma is known —
    // and the negotiator's 4:4:4 → 4:2:0 downgrade means an oversized mode arrives at the encoder
    // as 4:2:0, which is exactly the case the old open-time guard skipped.
    #[cfg(feature = "pyrowave")]
    if codec == Codec::PyroWave && !crate::pyrowave_mode_fits_rdo(width, height, false) {
        anyhow::bail!(
            "invalid PyroWave resolution {width}x{height}: exceeds the rate controller's 16-bit \
             block index (pyrowave-sys patches/0002) — lower the client resolution"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WP6.3. The window VUIDs on `VkVideoEncodeRateControlInfoKHR` are the whole contract of
    /// this helper, and both are edge cases: the window must be non-zero (a high-refresh mode
    /// rounds a sub-1 ms window down to nothing) and the initial fill must be at most the window
    /// (`<=` — `VUID-...-08358` was relaxed in 1.3.299). Env-free so it pins the default shape —
    /// the scaled cases belong to whoever sets `PUNKTFUNK_VBV_FRAMES`. Carries the helper's own
    /// cfg gate (see its note), so it runs on the Linux `vulkan-encode` leg.
    #[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
    #[test]
    fn vbv_window_is_about_one_frame_and_always_legal() {
        // The house default is ~1 frame interval, not the 1000 ms the Vulkan backend hardwired.
        assert_eq!(vbv_window_ms(60).0, 17); // 16.67 ms
        assert_eq!(vbv_window_ms(30).0, 33);
        assert_eq!(vbv_window_ms(240).0, 4);
        for fps in [1, 24, 30, 60, 120, 144, 240, 480, 1000, 4000, u32::MAX] {
            let (window, initial) = vbv_window_ms(fps);
            assert!(window > 0, "virtualBufferSizeInMs must be > 0 (fps {fps})");
            assert!(
                initial <= window,
                "initialVirtualBufferSizeInMs must be <= virtualBufferSizeInMs (fps {fps})"
            );
        }
        // fps 0 must not divide by zero — `open` clamps, but the helper is called directly too.
        assert!(vbv_window_ms(0).0 > 0);
    }

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

    /// PyroWave's hard cap is the rate controller's 16-bit block index, not just
    /// `max_dimension()`. Checked at 4:2:0 (the most permissive chroma), because a mode that
    /// cannot fit there cannot fit at any chroma — and because the negotiator's 4:4:4 → 4:2:0
    /// downgrade delivers oversized modes to the encoder AS 4:2:0. HEVC/AV1 at the same
    /// dimensions must stay unaffected.
    #[cfg(feature = "pyrowave")]
    #[test]
    fn pyrowave_rejects_modes_past_the_rdo_block_index() {
        // Fits: 8K 4:2:0 is 49125 blocks.
        assert!(validate_dimensions(Codec::PyroWave, 7680, 4320).is_ok());
        // Does not fit at 4:2:0 (73728 / 98304 blocks) — must be refused even though both are
        // within `Codec::PyroWave.max_dimension()` (8192).
        assert!(validate_dimensions(Codec::PyroWave, 8192, 6144).is_err());
        assert!(validate_dimensions(Codec::PyroWave, 8192, 8192).is_err());
        // The same modes remain legal for the H.26x/AV1 codecs, which have no such rate
        // controller — the cap must not leak across codecs.
        assert!(validate_dimensions(Codec::H265, 8192, 8192).is_ok());
        assert!(validate_dimensions(Codec::Av1, 8192, 6144).is_ok());
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

    /// The whole-AU chunk (every non-chunked backend's `poll_chunk` shape) must carry the AU's
    /// metadata verbatim and be self-closing (`first && last`).
    #[test]
    fn whole_au_chunk_is_self_closing() {
        let c = AuChunk::whole(EncodedFrame {
            data: vec![0, 0, 0, 1, 0x40],
            pts_ns: 42,
            keyframe: true,
            recovery_anchor: true,
            chunk_aligned: false,
        });
        assert_eq!(c.data, vec![0, 0, 0, 1, 0x40]);
        assert_eq!(c.pts_ns, 42);
        assert!(c.keyframe && c.recovery_anchor && !c.chunk_aligned);
        assert!(c.first && c.last);
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
