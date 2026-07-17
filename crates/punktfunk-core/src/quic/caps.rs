//! Client/host video capability bits, codec + chroma negotiation, and colour signalling.

/// [`Hello::video_caps`] bit: the client can decode a 10-bit (Main10) HEVC stream.
pub const VIDEO_CAP_10BIT: u8 = 0x01;
/// [`Hello::video_caps`] bit: the client can present BT.2020 PQ HDR10 (implies 10-bit).
pub const VIDEO_CAP_HDR: u8 = 0x02;
/// [`Hello::video_caps`] bit: the client can decode a full-chroma **4:4:4** HEVC stream (HEVC
/// Range Extensions / Rec.ITU-T H.265 `chroma_format_idc = 3`) AND its user turned 4:4:4 on (a
/// client-side setting, default OFF — the per-session policy switch). The host emits 4:4:4 ONLY
/// when this bit is set, the host allows it (`PUNKTFUNK_444`, default on), the codec is HEVC,
/// **and** the GPU/driver actually supports a 4:4:4 encode (probed) — otherwise the session stays
/// 4:2:0 and [`Welcome::chroma_format`] reflects the real resolved value. Independent of
/// 10-bit/HDR (4:4:4 is a chroma decision, bit depth is a depth decision; the two may combine
/// where the hardware allows).
pub const VIDEO_CAP_444: u8 = 0x04;
/// [`Hello::video_caps`] bit: the client consumes per-AU host-timing datagrams
/// ([`HOST_TIMING_MAGIC`], 0xCF) — the host's capture→send duration per frame, letting the client
/// split its `host+network` latency stage into `host` and `network`
/// (design/stats-unification.md Phase 2). The host emits 0xCF ONLY when this bit is set (an older
/// host ignores it and simply never sends any); a client that doesn't set it keeps the combined
/// stage. Purely observability — never changes what the host encodes.
pub const VIDEO_CAP_HOST_TIMING: u8 = 0x08;
/// [`Hello::video_caps`] bit: the client's reassembler keeps **speed-test probe filler in its own
/// frame-index space** (a second reassembly window keyed on the [`crate::packet::FLAG_PROBE`]
/// user-flag), so probe bursts no longer consume video `frame_index`es. Without this, a mid-session
/// speed test burns thousands of video indexes that are invisible to every client-side gap detector
/// (probe frames are filtered before the pump sees them) — the first real AU afterwards reads as a
/// phantom multi-thousand-frame loss (spurious freeze + a nonsense RFI). It also lets the host's
/// encode loop own the video numbering outright (the wire-index contract
/// [`crate::packet::Packetizer::packetize_each`] documents), which reference-frame invalidation
/// depends on. The host runs mid-session probe bursts ONLY against clients that set this bit — an
/// older client gets a declined (zeroed) [`ProbeResult`] instead of a measurement its single-window
/// reassembler would silently drop as stale.
pub const VIDEO_CAP_PROBE_SEQ: u8 = 0x10;

/// [`Welcome::host_caps`] bit: the host applies [`InputKind::GamepadState`]
/// (crate::input::InputKind::GamepadState) snapshot events — full per-pad state with a reorder
/// sequence number. A capable client then sends gamepad state as snapshots (idempotent on the
/// lossy datagram plane, periodically refreshed) instead of the fragile per-transition
/// button/axis events; toward a host that doesn't set the bit it keeps the legacy events.
pub const HOST_CAP_GAMEPAD_STATE: u8 = 0x01;

/// [`Hello::video_codecs`] bit: the client can decode H.264 / AVC. The GPU-less **software**
/// encode path (openh264) emits H.264, so a client that wants to stream from a software host MUST
/// advertise this.
pub const CODEC_H264: u8 = 0x01;
/// [`Hello::video_codecs`] bit: the client can decode H.265 / HEVC — the default every existing
/// build produces and decodes (a peer that omits [`Hello::video_codecs`] is treated as HEVC-only).
pub const CODEC_HEVC: u8 = 0x02;
/// [`Hello::video_codecs`] bit: the client can decode AV1.
pub const CODEC_AV1: u8 = 0x04;
/// [`Hello::video_codecs`] bit: the client can decode **PyroWave** — the opt-in wired-LAN
/// intra-only wavelet codec (design/pyrowave-codec-plan.md; 100–400 Mbps class, 8-bit SDR,
/// every frame independently decodable). Deliberately **absent from [`resolve_codec`]'s
/// precedence ladder**: it is selected only when the client also names it
/// [`Hello::preferred_codec`] (or the host operator forces the advertisement mask) — a codec
/// that needs a wired-LAN bitrate must never win a negotiation just because both ends support
/// it. The bit means "PyroWave bitstream as of the punktfunk-vendored pin"
/// (`crates/pyrowave-sys/vendor/pyrowave/PUNKTFUNK-VENDOR.txt`): upstream has no bitstream
/// version field, so a vendored bump that changes the bitstream bumps the punktfunk protocol
/// version instead (plan §4.2).
pub const CODEC_PYROWAVE: u8 = 0x08;

/// Resolve which single codec the host will emit, from the client's advertised [`Hello::video_codecs`]
/// bitfield (`0` = an older client, treated as HEVC-only) intersected with what the host's chosen
/// encoder can produce (`host_capable`, also a bitfield). `preferred` is the client's soft preference
/// ([`Hello::preferred_codec`], `0` = none): when it's in the shared set it wins; otherwise the tie is
/// broken by **HEVC > AV1 > H.264** (HEVC is the established, best-tested path; H.264 is the
/// compatibility / software floor). [`CODEC_PYROWAVE`] is intentionally NOT in that ladder — it can
/// only be returned via the `preferred` path (plan §3: opt-in, pinned, honest). Returns the
/// single-bit codec value, or `None` when client and host share nothing the ladder may pick — the
/// caller then refuses the session with a clear error rather than emitting a stream the client
/// can't decode.
pub fn resolve_codec(client_codecs: u8, host_capable: u8, preferred: u8) -> Option<u8> {
    // An older client (no codec byte) decodes HEVC — the only codec every pre-negotiation build sent.
    let client = if client_codecs == 0 {
        CODEC_HEVC
    } else {
        client_codecs
    };
    let shared = client & host_capable;
    if shared == 0 {
        return None;
    }
    // Honor the client's preference when the host can also emit it; else fall back to precedence.
    if preferred != 0 && shared & preferred != 0 {
        return Some(preferred);
    }
    // Precedence: HEVC > AV1 > H.264.
    [CODEC_HEVC, CODEC_AV1, CODEC_H264]
        .into_iter()
        .find(|&c| shared & c != 0)
}

/// HEVC `chroma_format_idc` for 4:2:0 — what every pre-4:4:4 build produced and the back-compat
/// default when a peer omits [`Welcome::chroma_format`].
pub const CHROMA_IDC_420: u8 = 1;
/// HEVC `chroma_format_idc` for full-chroma 4:4:4 (Range Extensions).
pub const CHROMA_IDC_444: u8 = 3;

/// Per-session colour signalling (CICP / ITU-T H.273 code points) the host resolved for the
/// encoded video, carried on [`Welcome`]. A client configures its decoder/presenter from these
/// instead of inferring them from the bitstream VUI. An older host omits the bytes on the wire →
/// [`ColorInfo::SDR_BT709`] (the 8-bit BT.709 limited stream every pre-HDR build produced).
///
/// The *static* HDR mastering metadata (ST.2086 + content light level) is larger and can change
/// mid-stream, so it rides the [`HDR_META_MAGIC`] datagram rather than this fixed struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    /// CICP colour primaries: 1 = BT.709, 9 = BT.2020.
    pub primaries: u8,
    /// CICP transfer characteristics: 1 = BT.709, 16 = PQ (SMPTE ST.2084), 18 = HLG.
    pub transfer: u8,
    /// CICP matrix coefficients: 1 = BT.709, 9 = BT.2020 non-constant-luminance.
    pub matrix: u8,
    /// `video_full_range_flag`: 0 = limited/studio range, 1 = full range.
    pub full_range: u8,
}

impl ColorInfo {
    /// CICP colour-primaries code point: BT.709.
    pub const CP_BT709: u8 = 1;
    /// CICP colour-primaries code point: BT.2020.
    pub const CP_BT2020: u8 = 9;
    /// CICP transfer code point: BT.709.
    pub const TRC_BT709: u8 = 1;
    /// CICP transfer code point: PQ (SMPTE ST.2084).
    pub const TRC_PQ: u8 = 16;
    /// CICP transfer code point: HLG (ARIB STD-B67 / BT.2100).
    pub const TRC_HLG: u8 = 18;
    /// CICP matrix code point: BT.709.
    pub const MC_BT709: u8 = 1;
    /// CICP matrix code point: BT.2020 non-constant-luminance. (Never emit 10 / constant-luminance —
    /// no client decodes it.)
    pub const MC_BT2020_NCL: u8 = 9;

    /// 8-bit BT.709 limited-range SDR — what every pre-HDR build produced, and the back-compat
    /// default when a peer omits the colour bytes.
    pub const SDR_BT709: ColorInfo = ColorInfo {
        primaries: Self::CP_BT709,
        transfer: Self::TRC_BT709,
        matrix: Self::MC_BT709,
        full_range: 0,
    };

    /// BT.2020 PQ (HDR10), limited range — what the Windows host's HEVC VUI emits.
    pub const HDR10_BT2020_PQ: ColorInfo = ColorInfo {
        primaries: Self::CP_BT2020,
        transfer: Self::TRC_PQ,
        matrix: Self::MC_BT2020_NCL,
        full_range: 0,
    };

    /// True when the transfer is an HDR curve (PQ or HLG): the stream needs HDR present, and
    /// (for PQ) a [`HdrMeta`] datagram carries the mastering metadata.
    pub fn is_hdr(&self) -> bool {
        self.transfer == Self::TRC_PQ || self.transfer == Self::TRC_HLG
    }
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self::SDR_BT709
    }
}
