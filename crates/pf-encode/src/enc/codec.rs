//! Encoder contract: [`Encoder`], the value types its signatures use
//! ([`EncodedFrame`], [`AuChunk`], [`Codec`], [`ChromaFormat`], [`EncoderCaps`]),
//! and the shared dimension / VBV / NVENC-split helpers.
//!
//! One encoder per session, on the encode thread. [`submit`](Encoder::submit)
//! does not own the frame: keep the GPU payload alive until the matching AU
//! returns from [`poll`](Encoder::poll). Loss recovery routes through
//! [`EncoderCaps`] (RFI, intra-refresh, cursor blend), not no-op defaults.
//! Backend selection and capability probes live in the crate root, which
//! re-exports this module.
//!
//! Pin split-mode values with `nvenc_split_constants_match_the_sdk`.
//! Dimension and VBV contracts are in the tests below.
use anyhow::Result;
use pf_frame::CapturedFrame;

/// Arriving pixels are 10-bit. Encoder depth, colour signalling, and the
/// staging surface follow this, not `negotiated_depth`.
///
/// A client can advertise 10-bit and still deliver 8-bit NV12. Opening a
/// P010 encoder against that capture fails at `open` (native AMF/QSV) or
/// on every `submit` (libavcodec). Negotiated depth is an upper bound; a
/// session that still labels itself HDR is a negotiation mismatch.
#[cfg(target_os = "windows")]
pub(crate) fn ten_bit_input(format: pf_frame::PixelFormat, negotiated_depth: u8) -> bool {
    use pf_frame::PixelFormat;
    let ten = matches!(
        format,
        PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Rgb10a2Sdr
    );
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

/// One encoded access unit for FEC + packetization.
/// `data` is in-band Annex-B (the encoder opens without a global header),
/// so each keyframe carries VPS/SPS/PPS.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    /// IDR; sets the SOF/keyframe wire flags.
    pub keyframe: bool,
    /// RFI recovery AU: a clean P against a known-good reference after
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames). The pump tags
    /// [`punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR`]. After RFI the host
    /// suppresses IDR, so without this flag the freeze only lifts on a later IDR.
    pub recovery_anchor: bool,
    /// Shard-aligned self-delimiting chunks ([`Encoder::set_wire_chunking`]).
    /// The session stamps [`punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED`].
    /// Only PyroWave sets it.
    pub chunk_aligned: bool,
}

/// One slice-boundary chunk of an encoded AU, from [`Encoder::poll_chunk`].
/// Chunks of one AU concatenate to the bytes [`Encoder::poll`] would return;
/// every cut is an Annex-B NAL start. AU metadata is authoritative on the
/// first chunk (the host opens the wire frame from it); `last` closes the AU.
/// `keyframe` on a non-final chunk is a prediction; the final chunk re-checks
/// the driver's picture type.
pub struct AuChunk {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    pub keyframe: bool,
    /// Same meaning as [`EncodedFrame::recovery_anchor`].
    pub recovery_anchor: bool,
    /// Same meaning as [`EncodedFrame::chunk_aligned`].
    pub chunk_aligned: bool,
    pub first: bool,
    /// Closes the AU and releases the encoder's in-flight slot.
    pub last: bool,
}

impl AuChunk {
    /// Whole AU as a single self-closing chunk. Non-chunked backends' default
    /// [`Encoder::poll_chunk`]: a chunk consumer needs no per-backend fork.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
    Av1,
    /// Opt-in wired-LAN intra-only wavelet codec (`design/pyrowave-codec-plan.md`).
    /// Negotiated only via the client's explicit `preferred_codec`, never the
    /// precedence ladder. Only the `pyrowave` backend emits it; every AU is a
    /// keyframe.
    PyroWave,
}

/// Chroma the encoder emits (`PUNKTFUNK_444` + client `VIDEO_CAP_444` + GPU
/// probe). `Yuv420` is the default. `Yuv444` is HEVC-only, native-protocol
/// only (GameStream stays 4:2:0), and only after [`can_encode_444`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChromaFormat {
    #[default]
    Yuv420,
    Yuv444,
}

impl ChromaFormat {
    /// HEVC `chroma_format_idc`: `1` (4:2:0) or `3` (4:4:4). Same numeric
    /// value as [`punktfunk_core::quic::Welcome::chroma_format`].
    pub fn idc(self) -> u8 {
        match self {
            ChromaFormat::Yuv420 => punktfunk_core::quic::CHROMA_IDC_420,
            ChromaFormat::Yuv444 => punktfunk_core::quic::CHROMA_IDC_444,
        }
    }

    pub fn is_444(self) -> bool {
        matches!(self, ChromaFormat::Yuv444)
    }
}

impl Codec {
    /// Map a `quic` codec bit ([`punktfunk_core::quic::CODEC_H264`] etc.) to
    /// [`Codec`]. Unknown / `0` maps to HEVC (pre-negotiation default). Inverse
    /// of [`Codec::to_wire`].
    pub fn from_wire(bit: u8) -> Codec {
        match bit {
            punktfunk_core::quic::CODEC_H264 => Codec::H264,
            punktfunk_core::quic::CODEC_AV1 => Codec::Av1,
            punktfunk_core::quic::CODEC_PYROWAVE => Codec::PyroWave,
            _ => Codec::H265,
        }
    }

    pub fn to_wire(self) -> u8 {
        match self {
            Codec::H264 => punktfunk_core::quic::CODEC_H264,
            Codec::H265 => punktfunk_core::quic::CODEC_HEVC,
            Codec::Av1 => punktfunk_core::quic::CODEC_AV1,
            Codec::PyroWave => punktfunk_core::quic::CODEC_PYROWAVE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "hevc",
            Codec::Av1 => "av1",
            Codec::PyroWave => "pyrowave",
        }
    }

    /// Negotiable 10-bit encode (HEVC Main10 / AV1 10-bit; PyroWave uses 16-bit
    /// UNORM planes with P010-style studio codes — `design/pyrowave-444-hdr.md`).
    /// H.264 is always 8-bit (High10 is not an NVENC or VCN encode mode).
    /// Codec-level gate only: the GPU/backend must still pass
    /// [`can_encode_10bit`](crate::can_encode_10bit).
    pub fn supports_10bit(self) -> bool {
        matches!(self, Codec::H265 | Codec::Av1 | Codec::PyroWave)
    }

    /// FFmpeg NVENC encoder name. Selected by name: a codec id would pick the
    /// software encoder.
    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
            // `open_video` never routes PyroWave to a libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }

    /// FFmpeg VAAPI encoder name. One libavcodec encoder per codec covers AMD
    /// and Intel. Selected by name (codec id would pick SW). AV1 VAAPI is
    /// narrow — probe, never assume (see [`open_video`]).
    pub fn vaapi_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_vaapi",
            Codec::H265 => "hevc_vaapi",
            Codec::Av1 => "av1_vaapi",
            // `open_video` never routes PyroWave to a libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }

    /// FFmpeg AMD AMF encoder name (Windows). Selected by name. AV1 (`av1_amf`)
    /// is RDNA3+ — probe, never assume.
    pub fn amf_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_amf",
            Codec::H265 => "hevc_amf",
            Codec::Av1 => "av1_amf",
            // `open_video` never routes PyroWave to a libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }

    /// FFmpeg Intel QSV encoder name (Windows). Selected by name. AV1
    /// (`av1_qsv`) is Arc/Xe2+ and HEVC Main10 is Gen9.5+ — probe, never assume.
    pub fn qsv_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_qsv",
            Codec::H265 => "hevc_qsv",
            Codec::Av1 => "av1_qsv",
            // `open_video` never routes PyroWave to a libavcodec backend.
            Codec::PyroWave => unreachable!("PyroWave has no FFmpeg encoder"),
        }
    }
}

/// Static capabilities an [`Encoder`] declares so session glue routes
/// loss-recovery and cursor plumbing by query, not by a method's no-op/`false`
/// default. `Copy`; fixed for the session (an HDR toggle re-inits the encoder).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderCaps {
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) can return `true`.
    /// When `false` the caller skips the call and keyframes on loss. Windows
    /// direct-NVENC and native AMF implement it; libavcodec paths cannot.
    pub supports_rfi: bool,
    /// Opened encoder is producing `chroma_format_idc = 3`. Post-open
    /// cross-check against `Welcome::chroma_format` from the pre-open probe.
    /// Session glue logs a mismatch; the in-band SPS is authoritative.
    pub chroma_444: bool,
    /// Periodic intra-refresh wave: a moving intra band recodes the picture
    /// over ~0.5 s, no periodic IDR. FEC-unrecoverable loss self-heals, so the
    /// session rate-limits client keyframe requests. The wave has no
    /// decoder-visible clean-point (FFmpeg never sets `AV_FRAME_FLAG_KEY` at a
    /// recovery point; AMF emits no recovery-point SEI), so this cap alone
    /// cannot lift the freeze — that needs
    /// [`intra_refresh_recovery`](Self::intra_refresh_recovery).
    pub intra_refresh: bool,
    /// Constrained GDR heals a lost picture within one wave. The host then tags
    /// wave-boundary AUs with
    /// [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT)
    /// so the client can lift its freeze on the second mark. Default `false`
    /// (the IDR path stays until this is set). Meaningless unless
    /// [`intra_refresh`](Self::intra_refresh) is also set.
    pub intra_refresh_recovery: bool,
    /// Intra-refresh wave length in frames. Host marks `USER_FLAG_RECOVERY_POINT`
    /// every Nth AU, re-phased at each IDR. 0 when off. Read only when
    /// [`intra_refresh_recovery`](Self::intra_refresh_recovery) is set.
    pub intra_refresh_period: u32,
    /// Encoder composites [`CapturedFrame::cursor`] into the picture.
    /// `open_video`'s `cursor_blend` is a request; most backends ignore
    /// `frame.cursor`. Query this instead of assuming. Negotiation gates the
    /// cursor channel with [`cursor_blend_capable`](crate::cursor_blend_capable);
    /// `open_video`'s post-open check is the backstop.
    pub blends_cursor: bool,
}

/// Hardware encoder. One per session, on the encode thread.
pub trait Encoder: Send {
    /// Submit one captured frame. Keep `frame` and its GPU payload alive until
    /// this frame's AU returns from [`poll`](Self::poll): a stream-ordered
    /// backend may still read the payload after `submit` returns.
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// [`submit`](Self::submit) plus the wire frame index this AU will carry
    /// (packetizer stamp; client's loss reports / RFI name). Predicted as
    /// `AUs sent + frames in flight`. A reset or rebuild forfeits in-flight
    /// frames, so stale predictions die with it. RFI backends pin LTR/DPB to
    /// this; an encoder-internal counter desyncs on the first mid-stream
    /// rebuild. Default: ignore the index and delegate to `submit`.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        let _ = wire_index;
        self.submit(frame)
    }
    /// Static [capabilities](EncoderCaps). Session glue routes by query, not
    /// no-op/`false` defaults. Default: none (software / libavcodec).
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    /// Force the next submitted frame to an IDR. Default: no-op.
    fn request_keyframe(&mut self) {}
    /// Source static HDR mastering metadata from the capturer. An HDR encoder
    /// emits it in-band (HEVC/H.264 SEI or AV1 metadata OBUs) on keyframes so a
    /// stock decoder can tone-map. First-party clients read the grade out-of-band
    /// (0xCE datagram); this is never the primary channel. Default: no-op.
    fn set_hdr_meta(&mut self, _meta: Option<punktfunk_core::quic::HdrMeta>) {}
    /// Invalidate a contiguous range of previously-encoded references (wire
    /// frame indexes, the domain [`submit_indexed`](Self::submit_indexed) pins)
    /// so the encoder re-references an older still-valid frame instead of an
    /// IDR. `true` = real invalidation; `false` = range older than DPB/LTR or
    /// no RFI — caller should [`request_keyframe`](Self::request_keyframe).
    /// Default `false`. libavcodec cannot implement this.
    fn invalidate_ref_frames(&mut self, _first_frame: i64, _last_frame: i64) -> bool {
        false
    }
    /// Mark every resident reference untrusted as an RFI anchor.
    ///
    /// [`super::rfi`]'s taint sweep only runs inside [`invalidate_ref_frames`].
    /// Other damage goes through [`request_keyframe`](Self::request_keyframe),
    /// which carries no range, so unrepaired damage stays an anchor and the
    /// next loss can lift the freeze on a grey frame. Distrust is not
    /// unusable: ordinary prediction uses the backend's slot index. A re-mark
    /// or an IDR that flushes the DPB restores trust. Default: no-op.
    fn distrust_references(&mut self) {}
    /// Escalate to pipelined (two-thread) retrieve under GPU contention: AUs
    /// ride ~one loop tick behind (`poll` may return `None` while an encode is
    /// in flight). Returns whether pipelined retrieve is now active; the switch
    /// may defer. `true` returning `false` (the default) = unsupported — the
    /// session loop stops asking.
    ///
    /// `false` requests wind-back to sync-retrieve at the next safe point,
    /// usually a rebuild whose first frame is an IDR. Caller polls until it
    /// reads `false`. `PUNKTFUNK_NVENC_ASYNC=1` refuses the wind-back.
    fn set_pipelined(&mut self, _on: bool) -> bool {
        false
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
    /// Whether [`poll_chunk`](Self::poll_chunk) currently emits sub-AU chunks.
    /// Dynamic: a pipelined-retrieve escalation or rebuild can turn it off —
    /// re-query per AU, never cache. `false` (default) means `poll_chunk`
    /// degrades to one whole-AU chunk.
    fn supports_chunked_poll(&self) -> bool {
        false
    }
    /// Next slice-boundary chunk of the oldest in-flight AU. When chunking is
    /// live this blocks until the next chunk is readable; the final (`last`)
    /// chunk blocks like [`poll`](Self::poll). `Ok(None)` only when no AU is in
    /// flight. Drain each AU through one method: `poll` on a partially-chunked
    /// AU is a caller bug (the backend errors rather than double-emit).
    /// Default: wrap [`poll`](Self::poll) as a single `first && last` chunk.
    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        Ok(self.poll()?.map(AuChunk::whole))
    }
    /// Rebuild the hardware encoder in place, keeping negotiated parameters
    /// (encode-stall watchdog: a wedged driver stops emitting AUs without
    /// returning an error). `true` = rebuilt: every submitted-but-unpolled
    /// frame is forfeited and the next submit starts a fresh IDR stream.
    /// Default `false`: treat the stall as fatal.
    fn reset(&mut self) -> bool {
        false
    }
    /// Retarget rate control to `bps` (average == max, CBR) in place — same
    /// codec/resolution/fps, only bitrate and derived VBV move. `true` = the
    /// live encoder accepted it: reference chain, in-flight frames, and the
    /// caller's wire-index prediction survive. `false` = backend cannot or the
    /// driver rejected the rate; caller falls back to a full rebuild.
    /// Default: no in-place retarget (libavcodec/software).
    fn reconfigure_bitrate(&mut self, _bps: u64) -> bool {
        false
    }
    /// Bitrate (bps) the encoder is actually running at (or will open at, for a
    /// lazily-opened backend) after any internal clamp. The session stores this,
    /// not the requested rate, as the live bitrate so the send pacer, console,
    /// and client controller ack the ASIC target. `None` (default) = backend
    /// does not track an applied rate; caller keeps the requested one.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        None
    }
    /// Cut AUs at the session's shard payload size (PyroWave datagram-aligned
    /// mode): every `shard_payload` window starts a fresh self-delimiting
    /// codec packet, zero-padded to the window, so a lost datagram costs a few
    /// coefficient blocks, not the frame. Produced AUs are flagged
    /// [`EncodedFrame::chunk_aligned`]. Default: no-op (H.26x cannot cut losslessly).
    fn set_wire_chunking(&mut self, _shard_payload: usize) {}
    /// How long a whole AU's packets currently take to leave the socket (µs,
    /// smoothed) — the host's paced-send `spread_us`.
    ///
    /// Linux direct-NVENC split arbitration compares `encode_1eng +
    /// send_of_last_slice` vs `encode_2eng + send_of_whole_AU` (HEVC split
    /// costs sub-frame readback; the value is send overlapping encode). The
    /// backend turns this into that handicap. Optional: ignoring it never
    /// arbitrates the sub-frame trade (the safe direction). `0` = unknown.
    fn set_send_spread_us(&mut self, _us: u32) {}
    /// Frames the capturer guarantees the encoder may hold in flight before it
    /// reuses an input texture (`Capturer::pipeline_depth`). In-place backends
    /// must not pipeline deeper: the capturer rotates its output ring per
    /// delivered frame, so a deeper pipeline overwrites a texture mid-encode
    /// (torn frames, not UB — it fails silently). Called once after the
    /// capturer is known. Default: no-op (copying or synchronous backends).
    fn set_input_ring_depth(&mut self, _depth: usize) {}
    /// Signal end-of-stream. After this, drain remaining AUs with
    /// [`poll`](Self::poll) until `None` — NVENC buffers frames internally
    /// even at `delay=0`.
    ///
    /// Production encode loops do not call this: they exit after the transport
    /// is gone, so flushed AUs have nowhere to go, and this is the one trait
    /// method that can block on a wedged encoder (Linux direct-NVENC's
    /// retrieve-thread join is untimed — `enc/linux/nvenc_cuda.rs`). Used by
    /// `spike` and the `#[ignore]`d hardware smoke tests.
    fn flush(&mut self) -> Result<()>;
}

impl Codec {
    /// Maximum encodable dimension (px) per side. H.264 is 4096 (level);
    /// HEVC and AV1 allow 8192. Rejects out-of-range client modes before
    /// open ([`validate_dimensions`]).
    pub fn max_dimension(self) -> u32 {
        match self {
            Codec::H264 => 4096,
            // No codec-level dimension cap (arbitrary even sizes). 8192 matches
            // the buffer-math guard the other codecs get.
            Codec::H265 | Codec::Av1 | Codec::PyroWave => 8192,
        }
    }

    /// Spec top level/tier bitrate (bits/s) — the usual boundary at which NVENC
    /// rejects `avcodec_open2` with EINVAL. Not a hard cap:
    /// [`open_video`](crate::open_video) probes the GPU ceiling by stepping
    /// down from the requested bitrate only on EINVAL, and uses this as the
    /// first step-down candidate so a card that accepts more is never clamped
    /// to it. HEVC Level 6.2 High = 800 Mbps; H.264 High 6.2 ≈ 480 Mbps.
    pub fn max_bitrate_bps(self) -> u64 {
        match self {
            Codec::H264 => 480_000_000,
            Codec::H265 => 800_000_000,
            Codec::Av1 => 1_200_000_000,
            // No spec level/tier: the rate is a per-frame byte budget. Use the
            // protocol bitrate clamp so the step-down probe never binds below it.
            Codec::PyroWave => 8_000_000_000,
        }
    }
}

/// Pixel rate (luma samples/s) at or above which NVENC split-frame encoding
/// is forced 2-way. Shared by the direct-SDK selector ([`resolve_split_mode`])
/// and the libav `split_encode_mode` option so the two paths cannot disagree.
/// A single NVENC engine tops out ~1 Gpix/s on HEVC, and AUTO does not
/// engage below ~2112 px height, so sessions that need the second engine
/// must be forced. 4K120 is 3840×2160×120 = 995,328,000; 950 M keeps
/// margin for fractional refresh while leaving 1440p240 (884.7 M) on AUTO.
pub const SPLIT_FORCE_PIXEL_RATE: u64 = 950_000_000;

/// `NV_ENC_SPLIT_ENCODE_MODE` values as plain constants.
///
/// They live here, not in `nvenc_core`, because the split policy must be
/// shared with the libav NVENC path, which compiles with the `nvenc`
/// feature off (`PUNKTFUNK_NVENC_DIRECT=0`). Gating them behind the feature
/// would let the libav copy diverge. `nvenc_split_constants_match_the_sdk`
/// pins these against the SDK enum.
// Split-policy cfg: union of callers. Linux always (libav NVENC in
// `enc/linux/mod.rs` calls `resolve_split_mode` with `nvenc` off). Windows
// only with `feature = "nvenc"`. Ungated this is `dead_code` on a
// featureless Windows build — an item lint, not a module one.
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) const SPLIT_AUTO: u32 = 0;
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) const SPLIT_AUTO_FORCED: u32 = 1;
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) const SPLIT_TWO_FORCED: u32 = 2;
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) const SPLIT_THREE_FORCED: u32 = 3;
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) const SPLIT_DISABLE: u32 = 15;

/// NVENC split-frame mode for a session. Shared by the Windows and Linux
/// direct-SDK backends. Precedence:
/// 1. `PUNKTFUNK_SPLIT_ENCODE` = `0`/`disable` | `1`/`auto` (AUTO_FORCED) |
///    `2` | `3` — operator override, always wins. `2`/`3` clamp to the GPU's
///    engine count ([`clamp_to_engines`]); the driver honours an over-ask
///    and silently encodes narrower.
/// 2. Pixel rate ≥ [`SPLIT_FORCE_PIXEL_RATE`] → widest split the GPU can
///    deliver ([`max_forced_split_mode`]). AUTO never engages below ~2112 px
///    height, so 4K120 must be forced.
/// 3. HEVC Main10 below that bar → DISABLE (split is slower there).
///    Codec-scoped and *below* the pixel-rate arm so it cannot veto AV1
///    10-bit or 10-bit 4K120.
/// 4. Else AUTO. AUTO splits only with sub-frame off; with sub-frame on
///    the driver resolves it to no-split (HEVC cannot do both).
///    [`resolve_split_subframe`] logs that.
///
/// Caller owns the rejection fallback (retry split-disabled). `engines` is
/// `NV_ENC_CAPS_NUM_ENCODER_ENGINES`; `0` = unknown (assume a second engine).
// Split-policy cfg — see the constants above.
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
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
        // Widest split the card can deliver, not a hard-coded two. Ahead of
        // the 10-bit rule so 10-bit 4K120 (~995 Mpix/s) is not vetoed.
        _ if pixel_rate >= SPLIT_FORCE_PIXEL_RATE => hw_max,
        // HEVC Main10 below the bar stays single-engine (split can be slower
        // there). Codec-scoped: this is HEVC Main10, not AV1 10-bit.
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

/// Strongest split mode this GPU's engine count can actually deliver.
///
/// The driver will not reject an over-ask: requesting `THREE_FORCED` on a
/// 2-NVENC part opens in mode 3 and encodes 2-way with no warning. The
/// rejection fallback cannot find the ceiling; the clamp happens here.
/// `NV_ENC_SPLIT_ENCODE_MODE` names counts up to three (4..14 unallocated).
/// Above that, `AUTO_FORCED` = "split, driver picks how many".
// Split-policy cfg — see the constants above.
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) fn max_forced_split_mode(engines: u32) -> u32 {
    match engines {
        // Cap unreadable or not probed: assume a second engine; open-time
        // rejection fallback corrects it.
        0 => SPLIT_TWO_FORCED,
        1 => SPLIT_DISABLE,
        2 => SPLIT_TWO_FORCED,
        3 => SPLIT_THREE_FORCED,
        // More engines than the enum can name: let the driver use them all.
        _ => SPLIT_AUTO_FORCED,
    }
}

/// N of an N-way forced split, or `None` for modes that do not name a width
/// (`DISABLE`, `AUTO`, `AUTO_FORCED` — the last forces a split but lets the
/// driver choose how wide).
///
/// For callers that can only say "split this many ways": the libav path,
/// whose `split_encode_mode` AVOption is libavcodec's enum, not NVENC's
/// (`DISABLE` is `15`, meaningless there).
// Linux-only: sole caller is the libav NVENC path (`enc/linux/mod.rs`).
// `codec.rs` compiles everywhere; without this cfg it is `dead_code` on
// Windows (item lint, not a module one).
#[cfg(target_os = "linux")]
pub(crate) fn forced_split_width(mode: u32) -> Option<u32> {
    match mode {
        m if m == SPLIT_TWO_FORCED => Some(2),
        m if m == SPLIT_THREE_FORCED => Some(3),
        _ => None,
    }
}

/// Hold `PUNKTFUNK_SPLIT_ENCODE=2|3` to what the hardware can deliver.
/// Without this the driver honours the knob as a silent narrower encode
/// (see [`max_forced_split_mode`]).
// Split-policy cfg — see the constants above.
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "nvenc")))]
pub(crate) fn clamp_to_engines(requested: u32, hw_max: u32, engines: u32) -> u32 {
    // Only named N-way modes are ordered; `hw_max` may be AUTO_FORCED (1) on
    // a >3-engine part, which is not less than TWO_FORCED and must not clamp.
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

/// `PUNKTFUNK_VBV_FRAMES` — HRD/VBV size in frame intervals (default 1.0:
/// each frame must fit its rate share, keeping sizes uniform for the pacer).
/// Direct-NVENC, AMF, VAAPI, and QSV parse the same variable. Larger
/// values let complex frames borrow bits at the cost of size variance.
pub(crate) fn vbv_frames_env() -> f64 {
    std::env::var("PUNKTFUNK_VBV_FRAMES")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0)
}

/// Same HRD/VBV window as [`vbv_frames_env`], as Vulkan Video wants it:
/// `(virtualBufferSizeInMs, initialVirtualBufferSizeInMs)`.
///
/// Other backends state the window in bits (`bitrate / fps × frames`);
/// Vulkan states it in milliseconds. Consumed only when the driver
/// advertises VBR: a tight window under CBR makes the driver stuff
/// underspent frames with filler NALs (Vulkan has no filler-suppression
/// control). VBR permits the underspend, so the tight window only bounds
/// a complex frame.
///
/// Initial fill is half the window. Window clamps to `>= 1` and
/// `window / 2 <= window` (`VUID-...-08358`). Linux `vulkan-encode` only;
/// ungated this is dead on Windows.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
pub(crate) fn vbv_window_ms(fps: u32) -> (u32, u32) {
    let frames = vbv_frames_env();
    let ms = (frames * 1000.0 / fps.max(1) as f64).round();
    // `f64 as u32` saturates at the bounds, so an absurd `PUNKTFUNK_VBV_FRAMES` cannot wrap.
    let window = (ms as u32).max(1);
    (window, window / 2)
}

/// Reject zero, odd, or out-of-range encode resolutions before buffer alloc
/// or NVENC open, instead of overflowing buffer math or failing with an
/// opaque NVENC code. A client can request any `mode=WxHxFPS`.
pub fn validate_dimensions(codec: Codec, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be non-zero");
    }
    // NVENC requires even dimensions for the chroma subsampling it does.
    if width % 2 != 0 || height % 2 != 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be even");
    }
    // 5-level wavelet needs ≥ 4·2⁵ px per axis (upstream `MinimumImageSize`;
    // band mirroring breaks below it). Reject a tiny mode here instead of
    // failing the encoder rebuild after the switch is acked.
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
    // Rate controller packs the 32×32 block index into the low 16 bits of
    // `RDOperation::block_offset_saving` (pyrowave-sys `patches/0002`). Past
    // `u16::MAX` the index collides. Check 4:2:0 here (most permissive);
    // 4:4:4 is re-checked at open.
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

    /// Window VUIDs on `VkVideoEncodeRateControlInfoKHR`: window must be
    /// non-zero (high-refresh can round a sub-1 ms window to nothing) and
    /// initial fill ≤ window (`VUID-...-08358`). Env-free so it pins the
    /// default shape. Same cfg as the helper.
    #[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
    #[test]
    fn vbv_window_is_about_one_frame_and_always_legal() {
        // One frame interval (~16.67 ms at 60 fps), not a 1000 ms window.
        assert_eq!(vbv_window_ms(60).0, 17);
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
        // fps 0 must not divide by zero: `open` clamps, but the helper is also called directly.
        assert!(vbv_window_ms(0).0 > 0);
    }

    #[test]
    fn rejects_zero_and_odd_dimensions() {
        assert!(validate_dimensions(Codec::H265, 0, 1080).is_err());
        assert!(validate_dimensions(Codec::H265, 1920, 0).is_err());
        assert!(validate_dimensions(Codec::H265, 1921, 1080).is_err());
        assert!(validate_dimensions(Codec::H265, 1920, 1081).is_err());
    }

    #[test]
    fn h264_capped_at_4096() {
        assert!(validate_dimensions(Codec::H264, 3840, 2160).is_ok());
        assert!(validate_dimensions(Codec::H264, 4096, 4096).is_ok());
        assert!(validate_dimensions(Codec::H264, 4098, 2160).is_err());
        assert!(validate_dimensions(Codec::H264, 3840, 4098).is_err());
    }

    /// PyroWave's hard cap is the rate controller's 16-bit block index, not
    /// just `max_dimension()`. Checked at 4:2:0 (most permissive chroma): a
    /// mode that cannot fit there cannot fit at any chroma, and the
    /// negotiator's 4:4:4 → 4:2:0 downgrade delivers oversized modes as
    /// 4:2:0. HEVC/AV1 at the same size must stay accepted.
    #[cfg(feature = "pyrowave")]
    #[test]
    fn pyrowave_rejects_modes_past_the_rdo_block_index() {
        // 8K 4:2:0 is 49125 blocks, under `u16::MAX`.
        assert!(validate_dimensions(Codec::PyroWave, 7680, 4320).is_ok());
        // Over `u16::MAX` blocks at 4:2:0 (73728 / 98304) even though both
        // sit within `Codec::PyroWave.max_dimension()` (8192).
        assert!(validate_dimensions(Codec::PyroWave, 8192, 6144).is_err());
        assert!(validate_dimensions(Codec::PyroWave, 8192, 8192).is_err());
        // H.26x/AV1 have no such rate controller: the cap must not leak.
        assert!(validate_dimensions(Codec::H265, 8192, 8192).is_ok());
        assert!(validate_dimensions(Codec::Av1, 8192, 6144).is_ok());
    }

    #[test]
    fn hevc_and_av1_allow_up_to_8192() {
        for c in [Codec::H265, Codec::Av1] {
            assert!(validate_dimensions(c, 3840, 2160).is_ok());
            assert!(validate_dimensions(c, 7680, 4320).is_ok());
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
