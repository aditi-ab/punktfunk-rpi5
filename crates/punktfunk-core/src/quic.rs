//! `punktfunk/1` — the native control plane, gated behind the `quic` feature.
//!
//! GameStream is punktfunk's compatibility layer; this is the start of its own protocol. A QUIC
//! connection (quinn, tokio — control plane only, never the per-frame path) carries a
//! length-prefixed binary handshake on one bidirectional stream:
//!
//! ```text
//!   client → host  Hello   { abi_version }
//!   host → client  Welcome { abi_version, session: full data-plane Config + mode + UDP port }
//!   client → host  Start   { client_udp_port }
//! ```
//!
//! after which both sides bring up a [`crate::session::Session`] over a plain
//! [`UdpTransport`](crate::transport::udp) (native threads, no async) and the host streams.
//! The Welcome carries everything the core negotiates — FEC scheme (including GF(2¹⁶)
//! Leopard, which GameStream can't express), shard sizing, crypto key/salt — so the data
//! plane is exactly the hardened core `Session`.
//!
//! Transport security: the host presents a long-lived self-signed certificate
//! ([`endpoint::server_with_identity`]) and the client pins its SHA-256 fingerprint
//! ([`endpoint::client_pinned`]; no pin = trust-on-first-use, with the observed fingerprint
//! reported back for persisting). The data plane adds AES-GCM on top.
//! All integers little-endian; every message is `u16 length || payload`.

use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::error::{PunktfunkError, Result};

/// Protocol magic + version, first bytes of the positional handshake (Hello/Welcome/Start).
pub const MAGIC: &[u8; 4] = b"PKF1";

/// Magic for typed post-handshake / pairing control messages. A distinct magic keeps the
/// typed namespace disjoint from the positional handshake: a `Hello` (whose abi_version
/// byte sits where a type byte would) can never be misparsed as a control message, and
/// vice-versa, regardless of field values.
pub const CTL_MAGIC: &[u8; 4] = b"PKFc";

/// `client → host`: open the session, requesting a display mode (the host creates its
/// virtual output at exactly this size/refresh — native resolution end to end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_version: u32,
    pub mode: Mode,
    /// Which compositor the client would like the host to drive (`Auto` = host decides). The
    /// host honors it only if that backend is available, else falls back and reports the real
    /// choice in [`Welcome::compositor`]. Appended to the wire form — omitted by older clients
    /// (decodes to `Auto`).
    pub compositor: CompositorPref,
    /// Which virtual gamepad the host should create for this session's pads (`Auto` = host
    /// decides: its `PUNKTFUNK_GAMEPAD` env var, else X-Box 360). Resolved choice echoed in
    /// [`Welcome::gamepad`]. Appended to the wire form — omitted by older clients (decodes
    /// to `Auto`).
    pub gamepad: GamepadPref,
    /// The client's desired video encoder bitrate, in kilobits per second. `0` = no preference
    /// (the host uses its default). The host clamps the request to a supported range and reports
    /// the value it actually configured in [`Welcome::bitrate_kbps`]. Appended to the wire form —
    /// omitted by older clients (decodes to `0`, i.e. host default).
    pub bitrate_kbps: u32,
    /// Human-readable device name ("Enrico's MacBook"), shown by the host when this device knocks
    /// on a pairing-required host (the delegated-approval pending list) and stored on approval.
    /// Appended to the wire form as `len u8 || UTF-8` (≤ [`HELLO_NAME_MAX`] bytes) — omitted by
    /// older clients (decodes to `None`; the host falls back to a fingerprint-derived label).
    pub name: Option<String>,
    /// Library entry the client wants this session to launch (the store-qualified `GameEntry.id`,
    /// e.g. `steam:570` / `custom:abc123`). The host resolves it against ITS OWN library and runs
    /// the matching launch recipe in the session — the client never sends a raw command, so a
    /// remote peer can't inject one. `None` = no game requested (the host's default session).
    /// Appended after `name` as `len u8 || UTF-8` (≤ [`HELLO_LAUNCH_MAX`] bytes); when present but
    /// `name` is absent, a zero-length name placeholder precedes it so the offset stays
    /// deterministic. Omitted by older clients (decodes to `None`).
    pub launch: Option<String>,
    /// Client video capabilities the host may use to upgrade the stream — a bitfield of
    /// [`VIDEO_CAP_10BIT`] (the client can decode 10-bit Main10 HEVC) and [`VIDEO_CAP_HDR`]
    /// (the client can present BT.2020 PQ HDR10). The host enables a 10-bit / HDR encode ONLY
    /// when the matching bit is set, so an older client (decodes to `0`) always gets the 8-bit
    /// BT.709 stream it understands. Appended after `launch` as a single trailing byte; a
    /// zero-length name/launch placeholder precedes it when those are absent so the offset stays
    /// deterministic. Omitted by older clients (decodes to `0`).
    pub video_caps: u8,
    /// Requested audio channel count: `2` (stereo, default), `6` (5.1) or `8` (7.1). The host
    /// resolves it against what it can capture and echoes the final count in
    /// [`Welcome::audio_channels`], which is what both ends build their Opus (multistream)
    /// codec from. Appended after `video_caps` as a single trailing byte; when it differs from
    /// the stereo default the name/launch/video_caps placeholders are forced (0) so it lands at a
    /// deterministic offset. Omitted by older clients / when `2` (decodes to `2`, i.e. stereo) so
    /// the stereo wire form stays byte-identical to the pre-surround build.
    pub audio_channels: u8,
    /// Which video codecs the client can decode — a bitfield of [`CODEC_H264`] / [`CODEC_HEVC`] /
    /// [`CODEC_AV1`]. The host picks one it can also produce (see [`resolve_codec`]) and reports it in
    /// [`Welcome::codec`]; a client that only reaches a GPU-less **software** host must set
    /// [`CODEC_H264`] (openh264 emits H.264). Appended after `audio_channels` as a single trailing
    /// byte (forcing the video_caps/audio_channels placeholders when present). Omitted by older
    /// clients (decodes to `0`, which [`resolve_codec`] treats as HEVC-only — every pre-negotiation
    /// build decoded HEVC).
    pub video_codecs: u8,
    /// The client's *preferred* codec (a single [`CODEC_H264`] / [`CODEC_HEVC`] / [`CODEC_AV1`] bit),
    /// or `0` = no preference (host decides by its own precedence). A **soft** hint: the host emits
    /// it when it can also produce it (and the client advertised it in `video_codecs`), else falls
    /// back to the best shared codec — see [`resolve_codec`]. Mirrors the [`Hello::compositor`] /
    /// [`Hello::gamepad`] preference pattern; the resolved codec is echoed in [`Welcome::codec`].
    /// Appended after `video_codecs` as a single trailing byte. Omitted by older clients (→ `0`).
    pub preferred_codec: u8,
}

/// [`Hello::video_caps`] bit: the client can decode a 10-bit (Main10) HEVC stream.
pub const VIDEO_CAP_10BIT: u8 = 0x01;
/// [`Hello::video_caps`] bit: the client can present BT.2020 PQ HDR10 (implies 10-bit).
pub const VIDEO_CAP_HDR: u8 = 0x02;
/// [`Hello::video_caps`] bit: the client can decode a full-chroma **4:4:4** HEVC stream (HEVC
/// Range Extensions / Rec.ITU-T H.265 `chroma_format_idc = 3`). The host emits 4:4:4 ONLY when this
/// bit is set, the host opted in (`PUNKTFUNK_444`), the codec is HEVC, **and** the GPU/driver
/// actually supports a 4:4:4 encode (probed) — otherwise the session stays 4:2:0 and
/// [`Welcome::chroma_format`] reflects the real resolved value. Independent of 10-bit/HDR (4:4:4 is a
/// chroma decision, bit depth is a depth decision; the two may combine where the hardware allows).
pub const VIDEO_CAP_444: u8 = 0x04;
/// [`Hello::video_caps`] bit: the client consumes per-AU host-timing datagrams
/// ([`HOST_TIMING_MAGIC`], 0xCF) — the host's capture→send duration per frame, letting the client
/// split its `host+network` latency stage into `host` and `network`
/// (design/stats-unification.md Phase 2). The host emits 0xCF ONLY when this bit is set (an older
/// host ignores it and simply never sends any); a client that doesn't set it keeps the combined
/// stage. Purely observability — never changes what the host encodes.
pub const VIDEO_CAP_HOST_TIMING: u8 = 0x08;

/// QUIC application error code a punktfunk/1 client closes the control connection with on a
/// **deliberate quit** (a user "stop", not a network drop). The host reads it off the connection's
/// `ApplicationClosed` reason and tears the session's virtual display down immediately, skipping the
/// keep-alive linger; any other close reason (idle timeout, reset, a bare code 0) still lingers so a
/// reconnect can resume. Shared so host + every client agree on the code.
pub const QUIT_CLOSE_CODE: u32 = 0x51;

/// QUIC application error code the **host** closes the control connection with when a **dedicated game
/// session's game process exits** (the nested gamescope died — the user quit the game), so a launcher
/// client can distinguish "the game ended" from an error and return to its library cleanly rather than
/// surfacing a failure (`design/gamemode-and-dedicated-sessions.md` §5.3). Sibling of
/// [`QUIT_CLOSE_CODE`]; a client that doesn't special-case it still ends the session (every client
/// returns to its launcher on session end), so it is purely refinement. Shared so host + clients agree.
pub const APP_EXITED_CLOSE_CODE: u32 = 0x52;

/// [`Hello::video_codecs`] bit: the client can decode H.264 / AVC. The GPU-less **software**
/// encode path (openh264) emits H.264, so a client that wants to stream from a software host MUST
/// advertise this.
pub const CODEC_H264: u8 = 0x01;
/// [`Hello::video_codecs`] bit: the client can decode H.265 / HEVC — the default every existing
/// build produces and decodes (a peer that omits [`Hello::video_codecs`] is treated as HEVC-only).
pub const CODEC_HEVC: u8 = 0x02;
/// [`Hello::video_codecs`] bit: the client can decode AV1.
pub const CODEC_AV1: u8 = 0x04;

/// Resolve which single codec the host will emit, from the client's advertised [`Hello::video_codecs`]
/// bitfield (`0` = an older client, treated as HEVC-only) intersected with what the host's chosen
/// encoder can produce (`host_capable`, also a bitfield). `preferred` is the client's soft preference
/// ([`Hello::preferred_codec`], `0` = none): when it's in the shared set it wins; otherwise the tie is
/// broken by **HEVC > AV1 > H.264** (HEVC is the established, best-tested path; H.264 is the
/// compatibility / software floor). Returns the single-bit codec value, or `None` when client and host
/// share nothing — the caller then refuses the session with a clear error rather than emitting a
/// stream the client can't decode.
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

/// Longest device name carried in a [`Hello`] (bytes of UTF-8; longer names are truncated on
/// encode, rejected on decode — a one-byte length prefix caps it at 255 anyway).
pub const HELLO_NAME_MAX: usize = 64;

/// Longest library id carried in a [`Hello::launch`] (bytes of UTF-8). Ids are short
/// (`steam:<appid>` / `custom:<12 hex>`); the cap just bounds an attacker-controlled field.
pub const HELLO_LAUNCH_MAX: usize = 128;

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub abi_version: u32,
    /// Host UDP port for the data plane.
    pub udp_port: u16,
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    pub encrypt: bool,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Seed/testing: how many frames the host will send (0 = unbounded).
    pub frames: u32,
    /// The compositor the host actually resolved for this session (the client's
    /// [`Hello::compositor`] preference if available, else the host's auto-detected choice).
    /// Appended to the wire form — `Auto` when an older host omitted it (i.e. "unknown").
    pub compositor: CompositorPref,
    /// The virtual gamepad backend the host actually resolved (the client's [`Hello::gamepad`]
    /// preference if available, else env var / X-Box 360). A client uses this to know whether
    /// DualSense feedback (0xCD) can arrive at all. Appended to the wire form — `Auto` when an
    /// older host omitted it (i.e. "unknown, assume X-Box 360").
    pub gamepad: GamepadPref,
    /// The encoder bitrate the host actually configured for this session, in kilobits per second
    /// (the client's [`Hello::bitrate_kbps`] clamped to the host's supported range, or the host
    /// default when the client requested `0`). Appended to the wire form — `0` when an older host
    /// omitted it (i.e. "unknown").
    pub bitrate_kbps: u32,
    /// The luma/chroma bit depth the host actually encodes at — `8` (default / older host) or
    /// `10` (Main10, enabled only when the client advertised [`VIDEO_CAP_10BIT`]). The client
    /// configures its decoder for 10-bit (P010) when this is `10`. Appended to the wire form as a
    /// single trailing byte; `8` when an older host omitted it.
    pub bit_depth: u8,
    /// The colour signalling (CICP primaries/transfer/matrix/range) the host encodes with — BT.709
    /// limited SDR by default, BT.2020 PQ when a 10-bit HDR session was negotiated. Appended after
    /// `bit_depth` as 4 trailing bytes; an older host that omits them decodes to
    /// [`ColorInfo::SDR_BT709`]. The client configures its decoder/presenter from this instead of
    /// guessing from the bitstream; the mastering metadata arrives separately on [`HDR_META_MAGIC`].
    pub color: ColorInfo,
    /// The chroma subsampling the host actually encodes at, as the HEVC `chroma_format_idc`:
    /// [`CHROMA_IDC_420`] (4:2:0, default / older host) or [`CHROMA_IDC_444`] (full-chroma 4:4:4,
    /// enabled only when the client advertised [`VIDEO_CAP_444`] *and* the host could open a real
    /// 4:4:4 encode). The client sizes its decoder/surface pool from this; the in-band SPS carries
    /// the authoritative value, so this is a hint (and the honest-downgrade channel — if the host
    /// requested 4:4:4 but the GPU declined, this reads `CHROMA_IDC_420`). Appended after the colour
    /// bytes as a single trailing byte; an older host that omits it decodes to [`CHROMA_IDC_420`].
    pub chroma_format: u8,
    /// The audio channel count the host actually resolved and **will** send on the `0xC9` plane:
    /// `2` (stereo, default), `6` (5.1) or `8` (7.1). Echoes [`Hello::audio_channels`] clamped to
    /// what the host can capture (Linux PipeWire always synthesizes the count; Windows WASAPI
    /// loopback is clamped to the render endpoint's mix-format channels). The client builds its Opus
    /// (multistream) decoder from THIS value via [`crate::audio::layout_for`] — never from its own
    /// request — so an older host that omits the byte (→ `2`) always yields working stereo. Appended
    /// after `chroma_format` as a single trailing byte.
    pub audio_channels: u8,
    /// The single video codec the host resolved and **will** emit — [`CODEC_H264`], [`CODEC_HEVC`]
    /// (default), or [`CODEC_AV1`] — from [`resolve_codec`] over the client's [`Hello::video_codecs`]
    /// and the host encoder's capability. The client builds its decoder from THIS (never assuming
    /// HEVC). Appended after `audio_channels` as a single trailing byte; an older host that omits it
    /// decodes to [`CODEC_HEVC`] (every pre-negotiation host sent HEVC).
    pub codec: u8,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
}

/// `client → host`, any time after [`Start`]: switch the session to a new display mode
/// (window resized, refresh changed) without reconnecting. The host answers with
/// [`Reconfigured`]; on acceptance it rebuilds its virtual output + encoder at the new
/// mode and the stream continues over the unchanged data plane — the first new-mode frame
/// is an IDR with in-band parameter sets, which is all a decoder needs to follow.
///
/// Post-handshake messages carry a type byte after the magic (the handshake itself is
/// positional and stays untyped for wire compatibility).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigure {
    pub mode: Mode,
}

/// `host → client`: answer to [`Reconfigure`]. `accepted = false` means the requested
/// mode was rejected (e.g. exceeds encoder limits) and the session continues at `mode`
/// (the still-active one); `true` means `mode` is now being switched to live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconfigured {
    pub accepted: bool,
    pub mode: Mode,
}

/// `client → host`, any time after [`Start`]: ask the host's encoder to emit a fresh IDR
/// keyframe NOW. The infinite-GOP stream opens with one IDR then sends P-frames only, so a
/// decoder that wedges (a lost/corrupt opening IDR, a bad early P-frame — most likely on the
/// cold first session) would otherwise stay frozen until the next loss-triggered recovery
/// keyframe, which may be far off. The client sends this when it detects a stalled decode;
/// the host forces the next frame to be an IDR with in-band parameter sets, recovering the
/// picture in ~one frame. Fire-and-forget — no reply (the recovered IDR is the ack).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestKeyframe;

/// `client → host`, periodic: the client's observed data-plane loss, so the host can size FEC to
/// the link instead of a flat percentage (adaptive FEC). `loss_ppm` is parts-per-million of shards
/// that arrived missing-but-recovered (plus a bump when frames went unrecoverable) over the report
/// window — i.e. the loss FEC is currently absorbing. The host maps it to a recovery percentage,
/// clamped to a sane band, and applies it live; a clean link decays toward the floor (fewer packets,
/// which directly helps a packet-rate-bound uplink like the Steam Deck's WiFi tx). Fire-and-forget.
/// A host that predates this ignores it (unknown control message) and keeps its static FEC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LossReport {
    pub loss_ppm: u32,
}

/// `client → host`, any time after [`Start`]: reconfigure the encoder to a new target bitrate
/// without reconnecting — the mid-stream lever of adaptive bitrate. The host clamps the request
/// exactly like [`Hello::bitrate_kbps`] (its `[MIN, MAX]` band; `0` → host default), answers with
/// [`BitrateChanged`] carrying the value it actually configured, and rebuilds the encoder in
/// place at the same mode — the first new-rate frame is an IDR with in-band parameter sets, which
/// every client decoder already follows (same discipline as a [`Reconfigure`] mode switch).
///
/// Sent by the client's automatic-bitrate controller (active when the user's bitrate setting is
/// "Automatic", i.e. `Hello::bitrate_kbps == 0`) when the link can't sustain the current rate —
/// or can sustain more again. A host that predates this ignores it (unknown control message) and
/// never answers; the client's controller detects the silence and disables itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetBitrate {
    /// Requested encoder bitrate in kilobits per second (`0` = host default, like Hello's field).
    pub bitrate_kbps: u32,
}

/// `host → client`: answer to [`SetBitrate`] — the bitrate the host actually configured (the
/// request clamped to its supported band). The encoder switches on the next frame (an IDR); the
/// stream never pauses. Also the controller's liveness signal: no answer ⇒ an old host that
/// doesn't renegotiate bitrate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitrateChanged {
    pub bitrate_kbps: u32,
}

/// `client → host`, any time after [`Start`]: run a bandwidth speed test. The host bursts
/// filler access units (flagged [`crate::packet::FLAG_PROBE`]) over the data plane at
/// `target_kbps` of application goodput for `duration_ms`, *pausing video for the duration*, then
/// replies with [`ProbeResult`]. The client measures the received probe bytes + time to estimate
/// the link's sustainable rate (and the loss vs. the host's reported send count) so it can pick a
/// [`Hello::bitrate_kbps`]. The host clamps both fields to sane bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeRequest {
    /// Goodput rate the host should send the probe at, in kilobits per second.
    pub target_kbps: u32,
    /// How long to burst, in milliseconds.
    pub duration_ms: u32,
}

/// `host → client`: the probe burst is finished. Reports what the host actually put on the wire so
/// the client can split the two failure modes apart: **host-side** drops (the send buffer couldn't
/// keep up — raise `net.core.wmem_max`) vs **link** loss (wire packets the air dropped). The client
/// measures delivered wire packets itself and computes:
///
/// - link loss   = `(wire_packets_sent − received) / wire_packets_sent`
/// - host drop   = `send_dropped / (wire_packets_sent + send_dropped)`
/// - throughput  = `received_wire_bytes * 8 / duration_ms`
///
/// Counting delivered traffic at the *packet* level (not whole reassembled AUs) makes the figure
/// degrade gracefully past the FEC budget instead of cliffing to zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeResult {
    /// Total access-unit payload bytes the host emitted for the probe (application goodput offered).
    pub bytes_sent: u64,
    /// Number of probe access units the host emitted.
    pub packets_sent: u32,
    /// The burst's actual duration in milliseconds (the host clamps/measures the request).
    pub duration_ms: u32,
    /// Wire packets the kernel ACCEPTED for transmission — what actually went on the link (offered
    /// minus the send-buffer drops below). `0` from a pre-wire-stats host (back-compat decode).
    pub wire_packets_sent: u32,
    /// Wire packets the host could NOT hand to the kernel (send buffer full): the host-side ceiling.
    pub send_dropped: u32,
}

/// `client → host`, right after [`Start`]: one round of the wall-clock skew handshake. The client
/// stamps `t1_ns` (its monotonic-since-epoch clock) and sends; the host echoes it in [`ClockEcho`]
/// with its own receive/send stamps. A few rounds let the client estimate the host↔client clock
/// offset, so the per-frame `capture→received` latency (the AU `pts_ns` is the host's capture
/// clock) is meaningful across machines, not just same-host. An old host ignores it (the client
/// times out and assumes a shared clock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockProbe {
    pub t1_ns: u64,
}

/// `host → client`: answer to [`ClockProbe`]. `t2_ns` is when the host received the probe and
/// `t3_ns` when it sent this echo (both the host clock); `t1_ns` is the client's send stamp echoed
/// back. With the client's receive time `t4`, offset = ((t2−t1)+(t3−t4))/2 (host minus client) and
/// RTT = (t4−t1)−(t3−t2). See [`clock_offset_ns`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockEcho {
    pub t1_ns: u64,
    pub t2_ns: u64,
    pub t3_ns: u64,
}

/// Estimate the host↔client clock offset (**host minus client**, ns) and RTT (ns) from skew-handshake
/// samples `(t1, t2, t3, t4)` — NTP's formula, taking the **minimum-RTT** sample (least queuing
/// noise; also discards the first round's host-setup latency). Offset is positive when the host
/// clock is ahead of the client's; add it to a client timestamp to express it in the host clock.
/// Returns `None` for an empty sample set.
pub fn clock_offset_ns(samples: &[(u64, u64, u64, u64)]) -> Option<(i64, u64)> {
    samples
        .iter()
        .map(|&(t1, t2, t3, t4)| {
            let rtt = ((t4 as i128 - t1 as i128) - (t3 as i128 - t2 as i128)).max(0) as u64;
            let offset = (((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2) as i64;
            (offset, rtt)
        })
        .min_by_key(|&(_, rtt)| rtt)
}

/// Type byte of [`Reconfigure`] (first byte after the magic).
pub const MSG_RECONFIGURE: u8 = 0x01;
/// Type byte of [`Reconfigured`].
pub const MSG_RECONFIGURED: u8 = 0x02;
/// Type byte of [`RequestKeyframe`].
pub const MSG_REQUEST_KEYFRAME: u8 = 0x03;
/// Type byte of [`LossReport`].
pub const MSG_LOSS_REPORT: u8 = 0x04;
/// Type byte of [`SetBitrate`].
pub const MSG_SET_BITRATE: u8 = 0x05;
/// Type byte of [`BitrateChanged`].
pub const MSG_BITRATE_CHANGED: u8 = 0x06;
/// Type byte of [`ProbeRequest`].
pub const MSG_PROBE_REQUEST: u8 = 0x20;
/// Type byte of [`ProbeResult`].
pub const MSG_PROBE_RESULT: u8 = 0x21;
/// Type byte of [`ClockProbe`].
pub const MSG_CLOCK_PROBE: u8 = 0x30;
/// Type byte of [`ClockEcho`].
pub const MSG_CLOCK_ECHO: u8 = 0x31;

// ---------------------------------------------------------------------------------------------
// Pairing ceremony (typed control messages): instead of a session Hello, a client may open
// the control stream with PairRequest. The host shows a short PIN out-of-band (log/UI); the
// user types it into the client.
//
// Trust is established by **SPAKE2** (a balanced PAKE), NOT a hash of the PIN. SPAKE2 turns
// the low-entropy PIN into a high-entropy shared key via a Diffie-Hellman exchange; the only
// thing an active man-in-the-middle who terminates the (TOFU) ceremony learns is whether a
// single PIN guess was right — there is no transcript value that reveals the PIN to an
// *offline* dictionary search (the fatal flaw of an HMAC-of-PIN proof over a 4-digit space).
// Both peers' certificate fingerprints are bound in as the SPAKE2 identities, so the
// established key — and the key-confirmation MACs derived from it — only agree when both
// sides saw the same two certificates. After mutual key confirmation the host persists the
// client's fingerprint and the client pins the host's.
// ---------------------------------------------------------------------------------------------

/// Type byte of [`PairRequest`].
pub const MSG_PAIR_REQUEST: u8 = 0x10;
/// Type byte of [`PairChallenge`].
pub const MSG_PAIR_CHALLENGE: u8 = 0x11;
/// Type byte of [`PairProof`].
pub const MSG_PAIR_PROOF: u8 = 0x12;
/// Type byte of [`PairResult`].
pub const MSG_PAIR_RESULT: u8 = 0x13;

/// `client → host`: begin pairing. `name` is the human label the host stores (≤64 bytes
/// UTF-8); `spake_a` is the client's SPAKE2 message (see [`SpakeRole::start`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub spake_a: Vec<u8>,
}

/// `host → client`: the host's SPAKE2 message + its key-confirmation MAC. The client
/// finishes SPAKE2, verifies `confirm` (proving the host derived the same key, i.e. knows
/// the PIN and saw the same certs), then sends its own confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairChallenge {
    pub spake_b: Vec<u8>,
    pub confirm: [u8; 32],
}

/// `client → host`: the client's key-confirmation MAC (its single proof attempt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairProof {
    pub confirm: [u8; 32],
}

/// `host → client`: ceremony outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairResult {
    pub ok: bool,
}

/// A length-prefixed (u16 LE) byte field within a control message.
fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

/// Read a length-prefixed field at `off`, returning the bytes and the next offset.
fn get_bytes(b: &[u8], off: usize) -> Result<(&[u8], usize)> {
    if off + 2 > b.len() {
        return Err(PunktfunkError::InvalidArg("truncated field"));
    }
    let n = u16::from_le_bytes([b[off], b[off + 1]]) as usize;
    let start = off + 2;
    if start + n > b.len() {
        return Err(PunktfunkError::InvalidArg("field overruns message"));
    }
    Ok((&b[start..start + n], start + n))
}

impl PairRequest {
    pub fn encode(&self) -> Vec<u8> {
        let name = self.name.as_bytes();
        let n = name.len().min(64);
        let mut b = Vec::with_capacity(8 + n + self.spake_a.len());
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_REQUEST);
        b.push(n as u8);
        b.extend_from_slice(&name[..n]);
        put_bytes(&mut b, &self.spake_a);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairRequest> {
        if b.len() < 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad PairRequest"));
        }
        let n = b[5] as usize;
        if n > 64 || b.len() < 6 + n {
            return Err(PunktfunkError::InvalidArg("bad PairRequest name"));
        }
        let name = String::from_utf8_lossy(&b[6..6 + n]).into_owned();
        let (spake_a, end) = get_bytes(b, 6 + n)?;
        if end != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(PairRequest {
            name,
            spake_a: spake_a.to_vec(),
        })
    }
}

impl PairChallenge {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(7 + self.spake_b.len() + 32);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_CHALLENGE);
        put_bytes(&mut b, &self.spake_b);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairChallenge> {
        if b.len() < 5 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_CHALLENGE {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge"));
        }
        let (spake_b, end) = get_bytes(b, 5)?;
        if end + 32 != b.len() {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge confirm"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[end..end + 32]);
        Ok(PairChallenge {
            spake_b: spake_b.to_vec(),
            confirm,
        })
    }
}

impl PairProof {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(37);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_PROOF);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairProof> {
        if b.len() != 37 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_PROOF {
            return Err(PunktfunkError::InvalidArg("bad PairProof"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[5..37]);
        Ok(PairProof { confirm })
    }
}

impl PairResult {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_RESULT);
        b.push(self.ok as u8);
        b
    }

    pub fn decode(b: &[u8]) -> Result<PairResult> {
        if b.len() != 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_RESULT {
            return Err(PunktfunkError::InvalidArg("bad PairResult"));
        }
        Ok(PairResult { ok: b[5] != 0 })
    }
}

/// SPAKE2 over Ed25519 for the pairing ceremony. The two roles use the asymmetric flow so
/// the identities are ordered; each side binds **both** certificate fingerprints as the
/// SPAKE2 identities, so the derived key only matches when client and host agree on the PIN
/// *and* saw the same two certificates (a MITM, presenting different certs to each leg,
/// cannot reach a shared key).
pub mod pake {
    use super::*;
    use hmac::{Hmac, Mac};
    use spake2::{Ed25519Group, Identity, Password, Spake2};

    /// In-progress SPAKE2 state plus the identity transcript for key confirmation.
    pub struct PairingPake {
        state: Spake2<Ed25519Group>,
        transcript: Vec<u8>,
    }

    /// Start the exchange. `client_fp`/`host_fp` are the two certificate fingerprints (the
    /// client passes what it observed via TOFU; the host passes its own + the client's
    /// presented cert). Returns the state and this side's outbound SPAKE2 message.
    pub fn start(
        is_client: bool,
        pin: &str,
        client_fp: &[u8; 32],
        host_fp: &[u8; 32],
    ) -> (PairingPake, Vec<u8>) {
        let pw = Password::new(pin.as_bytes());
        let id_client = Identity::new(client_fp);
        let id_host = Identity::new(host_fp);
        let (state, msg) = if is_client {
            Spake2::<Ed25519Group>::start_a(&pw, &id_client, &id_host)
        } else {
            Spake2::<Ed25519Group>::start_b(&pw, &id_client, &id_host)
        };
        let mut transcript = Vec::with_capacity(64);
        transcript.extend_from_slice(client_fp);
        transcript.extend_from_slice(host_fp);
        (PairingPake { state, transcript }, msg)
    }

    /// Key confirmation MAC for one direction (`label` distinguishes host vs client), keyed
    /// by the SPAKE2 shared key and bound to the fingerprint transcript.
    fn confirm(key: &[u8], label: &[u8], transcript: &[u8]) -> [u8; 32] {
        let mut mac =
            <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("hmac takes any key length");
        mac.update(label);
        mac.update(transcript);
        mac.finalize().into_bytes().into()
    }

    /// `Hmac` verification is constant-time via `ct_eq` in the underlying crate; we compare
    /// our recomputed tag the same way.
    fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
        a.iter()
            .zip(b.iter())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
    }

    /// Confirmation tags both sides expect, given the agreed SPAKE2 key.
    pub struct Confirmations {
        /// MAC the host sends (client verifies).
        pub host: [u8; 32],
        /// MAC the client sends (host verifies).
        pub client: [u8; 32],
    }

    impl PairingPake {
        /// Finish SPAKE2 with the peer's message → the pair of confirmation tags. `Err` if
        /// the peer's message is malformed (a wrong PIN does NOT error here — it yields a
        /// *different* key, so the confirmation MACs simply won't match).
        pub fn finish(self, peer_msg: &[u8]) -> Result<Confirmations> {
            let key = self
                .state
                .finish(peer_msg)
                .map_err(|_| PunktfunkError::Crypto)?;
            Ok(Confirmations {
                host: confirm(&key, b"punktfunk-pair-host", &self.transcript),
                client: confirm(&key, b"punktfunk-pair-client", &self.transcript),
            })
        }
    }

    /// Constant-time tag comparison for the confirmation step.
    pub fn verify(expected: &[u8; 32], got: &[u8; 32]) -> bool {
        ct_eq(expected, got)
    }
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary (so a multi-byte char straddling
/// the cap is dropped whole, never split). Shared by Hello's length-prefixed name/launch fields.
fn truncate_to(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(22);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(self.compositor.to_u8()); // appended at offset 20 — older hosts read [0..20] and skip it
        b.push(self.gamepad.to_u8()); // appended at offset 21 — same back-compat discipline
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // appended at offset 22..26
                                                               // name at offset 26: len u8 || UTF-8. Omitted when `None` *and* there is no later field —
                                                               // so a Hello with neither name nor launch stays byte-identical to the bitrate-era form
                                                               // (26 bytes). When `launch` is present we must still emit name's length byte (0 for None)
                                                               // so `launch` lands at a deterministic offset.
                                                               // `video_caps`/`audio_channels` are the trailing fields, after `launch`; when either is
                                                               // present (video_caps non-zero / audio_channels not stereo) the name/launch length bytes
                                                               // AND the video_caps byte must still be emitted (0 / 0) so the later byte lands at a
                                                               // deterministic offset — the same discipline `launch` already imposes on `name`.
                                                               // Trailing single-byte fields, in wire order. Each is emitted when it (or ANY later field)
                                                               // carries a non-default value, so a present field always lands at a deterministic offset.
        let ac_present = self.audio_channels != 2;
        let vcodecs_present = self.video_codecs != 0;
        let pref_present = self.preferred_codec != 0;
        let need_placeholders =
            self.video_caps != 0 || ac_present || vcodecs_present || pref_present;
        match (&self.name, &self.launch) {
            (None, None) if !need_placeholders => {}
            (name, _) => {
                let n = truncate_to(name.as_deref().unwrap_or(""), HELLO_NAME_MAX);
                b.push(n.len() as u8);
                b.extend_from_slice(n.as_bytes());
            }
        }
        // launch after name: len u8 || UTF-8.
        if self.launch.is_some() || need_placeholders {
            let l = truncate_to(self.launch.as_deref().unwrap_or(""), HELLO_LAUNCH_MAX);
            b.push(l.len() as u8);
            b.extend_from_slice(l.as_bytes());
        }
        // video_caps: single trailing byte. Emitted when non-zero OR when a later field follows (so
        // that field lands at a deterministic offset right after it).
        if self.video_caps != 0 || ac_present || vcodecs_present || pref_present {
            b.push(self.video_caps);
        }
        // audio_channels: emitted when non-stereo OR a later field follows.
        if ac_present || vcodecs_present || pref_present {
            b.push(self.audio_channels);
        }
        // video_codecs: emitted when non-zero OR preferred_codec follows.
        if vcodecs_present || pref_present {
            b.push(self.video_codecs);
        }
        // preferred_codec: single trailing byte. Last field; omitted when `0` (no preference).
        if pref_present {
            b.push(self.preferred_codec);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Hello> {
        if b.len() < 20 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Hello"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Hello {
            abi_version: u32at(4),
            mode: Mode {
                width: u32at(8),
                height: u32at(12),
                refresh_hz: u32at(16),
            },
            // Optional trailing bytes — an older client that omits them requests `Auto`.
            compositor: b
                .get(20)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(21)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            // Optional trailing 4 bytes (LE) — absent on an older client → `0` (host default).
            bitrate_kbps: b
                .get(22..26)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Optional trailing device name: len u8 || UTF-8. Absent / oversized / non-UTF-8 →
            // `None` (never fail the handshake over a label).
            name: b.get(26).and_then(|&len| {
                let len = len as usize;
                if len == 0 || len > HELLO_NAME_MAX {
                    return None;
                }
                b.get(27..27 + len)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .map(String::from)
            }),
            // Optional trailing launch id, positioned right after name's `len u8 || UTF-8` block.
            // The raw name-length byte (even when oversized/zero) determines where launch starts,
            // so a corrupt name never panics — it just pushes the launch offset out of range → None.
            launch: b.get(26).and_then(|&name_len| {
                let off = 27 + name_len as usize; // start of launch's length byte
                let len = *b.get(off)? as usize;
                if len == 0 || len > HELLO_LAUNCH_MAX {
                    return None;
                }
                b.get(off + 1..off + 1 + len)
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .map(String::from)
            }),
            // Optional trailing video-caps byte, positioned right after launch's `len u8 || bytes`
            // block. Uses the raw (possibly zero/placeholder) name/launch length bytes to locate it,
            // so it's robust to absent name/launch; absent entirely on an older client → `0`.
            video_caps: {
                let name_len = b.get(26).copied().unwrap_or(0) as usize;
                let launch_off = 27 + name_len; // launch's length byte
                let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
                b.get(launch_off + 1 + launch_len).copied().unwrap_or(0)
            },
            // Optional trailing audio-channel byte, one past video_caps. Absent on an older client
            // → stereo. Normalized so a corrupt/unsupported value can't build a bad decoder.
            audio_channels: {
                let name_len = b.get(26).copied().unwrap_or(0) as usize;
                let launch_off = 27 + name_len;
                let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
                let video_caps_off = launch_off + 1 + launch_len;
                crate::audio::normalize_channels(b.get(video_caps_off + 1).copied().unwrap_or(2))
            },
            // Optional trailing video-codecs byte, one past audio_channels. Absent on an older client
            // → `0` (which `resolve_codec` treats as HEVC-only).
            video_codecs: {
                let name_len = b.get(26).copied().unwrap_or(0) as usize;
                let launch_off = 27 + name_len;
                let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
                let video_caps_off = launch_off + 1 + launch_len;
                b.get(video_caps_off + 2).copied().unwrap_or(0)
            },
            // Optional trailing preferred-codec byte, one past video_codecs. Absent on an older
            // client → `0` (no preference; the host decides by precedence).
            preferred_codec: {
                let name_len = b.get(26).copied().unwrap_or(0) as usize;
                let launch_off = 27 + name_len;
                let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
                let video_caps_off = launch_off + 1 + launch_len;
                b.get(video_caps_off + 3).copied().unwrap_or(0)
            },
        })
    }
}

impl Welcome {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.abi_version.to_le_bytes());
        b.extend_from_slice(&self.udp_port.to_le_bytes());
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b.push(match self.fec.scheme {
            FecScheme::Gf8 => 0,
            FecScheme::Gf16 => 1,
        });
        b.push(self.fec.fec_percent);
        b.extend_from_slice(&self.fec.max_data_per_block.to_le_bytes());
        b.extend_from_slice(&self.shard_payload.to_le_bytes());
        b.push(self.encrypt as u8);
        b.extend_from_slice(&self.key);
        b.extend_from_slice(&self.salt);
        b.extend_from_slice(&self.frames.to_le_bytes());
        b.push(self.compositor.to_u8()); // appended at offset 53 — older clients read [0..53] and skip it
        b.push(self.gamepad.to_u8()); // appended at offset 54 — same back-compat discipline
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // appended at offset 55..59
        b.push(self.bit_depth); // appended at offset 59 — older clients read [0..59] and skip it
                                // Colour signalling at offsets 60..64 — older clients stop before these → SDR BT.709.
        b.push(self.color.primaries);
        b.push(self.color.transfer);
        b.push(self.color.matrix);
        b.push(self.color.full_range);
        // Chroma subsampling at offset 64 — older clients stop before this → 4:2:0 (CHROMA_IDC_420).
        b.push(self.chroma_format);
        // Audio channel count at offset 65 — older clients stop before this → stereo (2).
        b.push(self.audio_channels);
        // Resolved video codec at offset 66 — older clients stop before this → HEVC.
        b.push(self.codec);
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Layout (LE): magic[0..4] abi[4..8] port[8..10] w[10..14] h[14..18] hz[18..22]
        // scheme[22] pct[23] max_data[24..26] shard[26..28] encrypt[28] key[29..45]
        // salt[45..49] frames[49..53] compositor[53] gamepad[54] bitrate_kbps[55..59]
        // bit_depth[59] color.primaries[60] color.transfer[61] color.matrix[62] color.range[63]
        // chroma_format[64] audio_channels[65] codec[66] (everything from compositor on is an
        // optional trailing byte; an older host stops earlier).
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        Ok(Welcome {
            abi_version: u32at(4),
            udp_port: u16at(8),
            mode: Mode {
                width: u32at(10),
                height: u32at(14),
                refresh_hz: u32at(18),
            },
            fec: FecConfig {
                scheme: if b[22] == 1 {
                    FecScheme::Gf16
                } else {
                    FecScheme::Gf8
                },
                fec_percent: b[23],
                max_data_per_block: u16at(24),
            },
            shard_payload: u16at(26),
            encrypt: b[28] != 0,
            key,
            salt,
            frames: u32at(49),
            // Optional trailing bytes — an older host that omits them leaves the resolved
            // compositor / gamepad backend unknown (`Auto`).
            compositor: b
                .get(53)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(54)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            // Optional trailing 4 bytes (LE) — absent on an older host → `0` (unknown).
            bitrate_kbps: b
                .get(55..59)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Optional trailing byte — absent on an older host → `8` (8-bit, the only depth they
            // encode).
            bit_depth: b.get(59).copied().unwrap_or(8),
            // Optional trailing colour bytes — absent on an older host → SDR BT.709 limited.
            color: ColorInfo {
                primaries: b.get(60).copied().unwrap_or(ColorInfo::CP_BT709),
                transfer: b.get(61).copied().unwrap_or(ColorInfo::TRC_BT709),
                matrix: b.get(62).copied().unwrap_or(ColorInfo::MC_BT709),
                full_range: b.get(63).copied().unwrap_or(0),
            },
            // Optional trailing chroma byte — absent on an older host (or an explicit 0 / unknown
            // value) → 4:2:0. Only `CHROMA_IDC_444` flips the client to a 4:4:4 decode.
            chroma_format: match b.get(64).copied() {
                Some(CHROMA_IDC_444) => CHROMA_IDC_444,
                _ => CHROMA_IDC_420,
            },
            // Optional trailing audio-channel byte — absent on an older host → stereo. Any
            // non-{6,8} value normalizes to stereo so a corrupt byte never builds a bad decoder.
            audio_channels: crate::audio::normalize_channels(b.get(65).copied().unwrap_or(2)),
            // Optional trailing codec byte — absent on an older host (or an unknown value) → HEVC,
            // the codec every pre-negotiation host emitted.
            codec: match b.get(66).copied() {
                Some(CODEC_H264) => CODEC_H264,
                Some(CODEC_AV1) => CODEC_AV1,
                _ => CODEC_HEVC,
            },
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // wire phase id pending the P2 packet rev
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        c.key = self.key;
        c.salt = self.salt;
        c
    }
}

impl Start {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.client_udp_port.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Start> {
        if b.len() < 6 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Start"));
        }
        Ok(Start {
            client_udp_port: u16::from_le_bytes([b[4], b[5]]),
        })
    }
}

impl Reconfigure {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] w[5..9] h[9..13] hz[13..17]
        let mut b = Vec::with_capacity(17);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_RECONFIGURE);
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigure> {
        if b.len() != 17 || &b[0..4] != CTL_MAGIC || b[4] != MSG_RECONFIGURE {
            return Err(PunktfunkError::InvalidArg("bad Reconfigure"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Reconfigure {
            mode: Mode {
                width: u32at(5),
                height: u32at(9),
                refresh_hz: u32at(13),
            },
        })
    }
}

impl Reconfigured {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] accepted[5] w[6..10] h[10..14] hz[14..18]
        let mut b = Vec::with_capacity(18);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_RECONFIGURED);
        b.push(self.accepted as u8);
        b.extend_from_slice(&self.mode.width.to_le_bytes());
        b.extend_from_slice(&self.mode.height.to_le_bytes());
        b.extend_from_slice(&self.mode.refresh_hz.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Reconfigured> {
        if b.len() != 18 || &b[0..4] != CTL_MAGIC || b[4] != MSG_RECONFIGURED {
            return Err(PunktfunkError::InvalidArg("bad Reconfigured"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(Reconfigured {
            accepted: b[5] != 0,
            mode: Mode {
                width: u32at(6),
                height: u32at(10),
                refresh_hz: u32at(14),
            },
        })
    }
}

impl RequestKeyframe {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] — no payload
        let mut b = Vec::with_capacity(5);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_REQUEST_KEYFRAME);
        b
    }

    pub fn decode(b: &[u8]) -> Result<RequestKeyframe> {
        if b.len() != 5 || &b[0..4] != CTL_MAGIC || b[4] != MSG_REQUEST_KEYFRAME {
            return Err(PunktfunkError::InvalidArg("bad RequestKeyframe"));
        }
        Ok(RequestKeyframe)
    }
}

impl LossReport {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] loss_ppm[5..9]
        let mut b = Vec::with_capacity(9);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_LOSS_REPORT);
        b.extend_from_slice(&self.loss_ppm.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<LossReport> {
        if b.len() != 9 || &b[0..4] != CTL_MAGIC || b[4] != MSG_LOSS_REPORT {
            return Err(PunktfunkError::InvalidArg("bad LossReport"));
        }
        Ok(LossReport {
            loss_ppm: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
    }
}

impl SetBitrate {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bitrate_kbps[5..9]
        let mut b = Vec::with_capacity(9);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_SET_BITRATE);
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<SetBitrate> {
        if b.len() != 9 || &b[0..4] != CTL_MAGIC || b[4] != MSG_SET_BITRATE {
            return Err(PunktfunkError::InvalidArg("bad SetBitrate"));
        }
        Ok(SetBitrate {
            bitrate_kbps: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
    }
}

impl BitrateChanged {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bitrate_kbps[5..9]
        let mut b = Vec::with_capacity(9);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_BITRATE_CHANGED);
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<BitrateChanged> {
        if b.len() != 9 || &b[0..4] != CTL_MAGIC || b[4] != MSG_BITRATE_CHANGED {
            return Err(PunktfunkError::InvalidArg("bad BitrateChanged"));
        }
        Ok(BitrateChanged {
            bitrate_kbps: u32::from_le_bytes(b[5..9].try_into().unwrap()),
        })
    }
}

/// Compute a [`LossReport`] `loss_ppm` from one window's session-stat deltas: shards FEC recovered
/// (the loss it absorbed), shards received, and frames that went unrecoverable. Loss ≈ recovered /
/// (received + recovered) — the fraction of shards that arrived missing. A frame drop means loss
/// exceeded the current FEC budget (so `recovered` plateaus), so add a fixed bump to push the host's
/// FEC up past the cap on the next adjustment. Returns parts-per-million, capped at 1e6.
pub fn window_loss_ppm(recovered: u64, received: u64, frames_dropped: u64) -> u32 {
    let denom = received.saturating_add(recovered);
    let mut ppm = recovered
        .saturating_mul(1_000_000)
        .checked_div(denom)
        .unwrap_or(0) as u32;
    if frames_dropped > 0 {
        ppm = ppm.saturating_add(50_000); // +5%: unrecoverable loss → raise FEC past the current cap
    }
    ppm.min(1_000_000)
}

impl ProbeRequest {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] target_kbps[5..9] duration_ms[9..13]
        let mut b = Vec::with_capacity(13);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PROBE_REQUEST);
        b.extend_from_slice(&self.target_kbps.to_le_bytes());
        b.extend_from_slice(&self.duration_ms.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<ProbeRequest> {
        if b.len() != 13 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PROBE_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad ProbeRequest"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Ok(ProbeRequest {
            target_kbps: u32at(5),
            duration_ms: u32at(9),
        })
    }
}

impl ProbeResult {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] bytes_sent[5..13] packets_sent[13..17] duration_ms[17..21]
        // wire_packets_sent[21..25] send_dropped[25..29]
        let mut b = Vec::with_capacity(29);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PROBE_RESULT);
        b.extend_from_slice(&self.bytes_sent.to_le_bytes());
        b.extend_from_slice(&self.packets_sent.to_le_bytes());
        b.extend_from_slice(&self.duration_ms.to_le_bytes());
        b.extend_from_slice(&self.wire_packets_sent.to_le_bytes());
        b.extend_from_slice(&self.send_dropped.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<ProbeResult> {
        // Back-compat: 21 bytes (pre-wire-stats host, new fields default 0) or 29 bytes (with the
        // wire_packets_sent + send_dropped tail). Accept either; reject anything shorter/garbled.
        if b.len() < 21 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PROBE_RESULT {
            return Err(PunktfunkError::InvalidArg("bad ProbeResult"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let (wire_packets_sent, send_dropped) = if b.len() >= 29 {
            (u32at(21), u32at(25))
        } else {
            (0, 0)
        };
        Ok(ProbeResult {
            bytes_sent: u64::from_le_bytes(b[5..13].try_into().unwrap()),
            packets_sent: u32at(13),
            duration_ms: u32at(17),
            wire_packets_sent,
            send_dropped,
        })
    }
}

impl ClockProbe {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] t1[5..13]
        let mut b = Vec::with_capacity(13);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLOCK_PROBE);
        b.extend_from_slice(&self.t1_ns.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClockProbe> {
        if b.len() != 13 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLOCK_PROBE {
            return Err(PunktfunkError::InvalidArg("bad ClockProbe"));
        }
        Ok(ClockProbe {
            t1_ns: u64::from_le_bytes(b[5..13].try_into().unwrap()),
        })
    }
}

impl ClockEcho {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] t1[5..13] t2[13..21] t3[21..29]
        let mut b = Vec::with_capacity(29);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLOCK_ECHO);
        b.extend_from_slice(&self.t1_ns.to_le_bytes());
        b.extend_from_slice(&self.t2_ns.to_le_bytes());
        b.extend_from_slice(&self.t3_ns.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClockEcho> {
        if b.len() != 29 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLOCK_ECHO {
            return Err(PunktfunkError::InvalidArg("bad ClockEcho"));
        }
        Ok(ClockEcho {
            t1_ns: u64::from_le_bytes(b[5..13].try_into().unwrap()),
            t2_ns: u64::from_le_bytes(b[13..21].try_into().unwrap()),
            t3_ns: u64::from_le_bytes(b[21..29].try_into().unwrap()),
        })
    }
}

/// Frame a message for the control stream: `u16 LE length || payload`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + payload.len());
    b.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

/// Datagram wire tags. Video rides UDP; everything low-rate rides QUIC datagrams,
/// demultiplexed by the first byte: input = [`crate::input::INPUT_MAGIC`] (0xC8, client→host),
/// audio = [`AUDIO_MAGIC`] (0xC9, host→client), rumble = [`RUMBLE_MAGIC`] (0xCA, host→client),
/// mic = [`MIC_MAGIC`] (0xCB, client→host), rich-input = [`RICH_INPUT_MAGIC`] (0xCC, client→host),
/// HID-output = [`HIDOUT_MAGIC`] (0xCD, host→client), HDR metadata = [`HDR_META_MAGIC`]
/// (0xCE, host→client).
pub const AUDIO_MAGIC: u8 = 0xC9;
pub const RUMBLE_MAGIC: u8 = 0xCA;
/// Microphone uplink: the client's mic, Opus-encoded, client → host (the inverse of
/// [`AUDIO_MAGIC`]). The host feeds it into a virtual PipeWire source so its apps can record it.
pub const MIC_MAGIC: u8 = 0xCB;
/// Rich client→host input: events too big for the fixed 18-byte [`InputEvent`]
/// (crate::input::InputEvent) — the DualSense touchpad and motion sensors. Variable-length,
/// kind-tagged (see [`RichInput`]).
pub const RICH_INPUT_MAGIC: u8 = 0xCC;
/// HID output, host → client: DualSense feedback a game wrote to the host's virtual controller
/// (lightbar, player LEDs, adaptive triggers) — the rich analog of [`RUMBLE_MAGIC`]. See
/// [`HidOutput`].
pub const HIDOUT_MAGIC: u8 = 0xCD;

/// Audio datagram, host → client: `[0xC9][u32 seq LE][u64 pts_ns LE][opus payload]`.
/// One Opus frame per datagram (5 ms — well under any MTU); QUIC already encrypts.
pub fn encode_audio_datagram(seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + opus.len());
    b.push(AUDIO_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

/// Parse an audio datagram → `(seq, pts_ns, opus payload)`. `None` on bad tag/length.
pub fn decode_audio_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < 13 || b[0] != AUDIO_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[13..]))
}

/// Rumble datagram, host → client: `[0xCA][u16 pad LE][u16 low LE][u16 high LE]`.
/// Force-feedback state for pad `pad` (0xFFFF amplitudes, 0/0 = stop).
pub fn encode_rumble_datagram(pad: u16, low: u16, high: u16) -> [u8; 7] {
    let mut b = [0u8; 7];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b
}

/// Parse a rumble datagram → `(pad, low, high)`. `None` on bad tag/length.
pub fn decode_rumble_datagram(b: &[u8]) -> Option<(u16, u16, u16)> {
    if b.len() < 7 || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    Some((u16at(1), u16at(3), u16at(5)))
}

/// Mic datagram, client → host: `[0xCB][u32 seq LE][u64 pts_ns LE][opus payload]` — the same
/// layout as [`encode_audio_datagram`] with [`MIC_MAGIC`], one Opus frame per datagram.
pub fn encode_mic_datagram(seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + opus.len());
    b.push(MIC_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

/// Parse a mic datagram → `(seq, pts_ns, opus payload)`. `None` on bad tag/length.
pub fn decode_mic_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < 13 || b[0] != MIC_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[13..]))
}

const RICH_TOUCHPAD: u8 = 0x01;
const RICH_MOTION: u8 = 0x02;
const RICH_TOUCHPAD_EX: u8 = 0x03;

/// A rich client→host controller input beyond the fixed [`InputEvent`](crate::input::InputEvent):
/// the DualSense touchpad and motion sensors. `pad` is the gamepad index. Wire form is
/// `[0xCC][kind][fields…]` — variable-length and kind-tagged (forward-compatible: an unknown
/// kind decodes to `None` and is dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RichInput {
    /// One touchpad contact. `x`/`y` are normalized `0..=65535` in SCREEN convention —
    /// origin top-left, +y DOWN, exactly what SDL/Windows/Android capture APIs produce
    /// (the host scales to the DualSense touchpad resolution); `active = false` lifts
    /// the finger.
    Touchpad {
        pad: u8,
        finger: u8,
        active: bool,
        x: u16,
        y: u16,
    },
    /// Motion sensors: `gyro` (pitch/yaw/roll) + `accel`, raw signed-16 in the sensor's own
    /// units — passed straight into the DualSense report.
    Motion {
        pad: u8,
        gyro: [i16; 3],
        accel: [i16; 3],
    },
    /// A richer trackpad contact that also identifies *which* physical pad (Steam Controller / Deck
    /// have two), carries a separate click vs touch state, and a pressure reading. `surface`:
    /// `0` = the single / DualSense touchpad, `1` = the Steam left pad, `2` = the Steam right pad.
    /// Coordinates are **signed** (centred at 0) in SCREEN convention — +x right, +y DOWN,
    /// what every client capture API produces. Device-raw quirks are the HOST applier's job
    /// (the Deck report is +y up: `steam_proto` flips it — the first live session shipped
    /// clients that sent screen-y straight through, so the wire meaning is fixed as screen-y
    /// and hosts translate). `pressure` is `0` for a surface with no force sensor. New clients
    /// send this for every touch surface; the host decodes both `Touchpad` (`0x01`) and
    /// `TouchpadEx` (`0x03`) indefinitely.
    TouchpadEx {
        pad: u8,
        surface: u8,
        finger: u8,
        touch: bool,
        click: bool,
        x: i16,
        y: i16,
        pressure: u16,
    },
}

impl RichInput {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![RICH_INPUT_MAGIC];
        match *self {
            RichInput::Touchpad {
                pad,
                finger,
                active,
                x,
                y,
            } => {
                out.extend_from_slice(&[RICH_TOUCHPAD, pad, finger, active as u8]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
            }
            RichInput::Motion { pad, gyro, accel } => {
                out.extend_from_slice(&[RICH_MOTION, pad]);
                for v in gyro.iter().chain(accel.iter()) {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            RichInput::TouchpadEx {
                pad,
                surface,
                finger,
                touch,
                click,
                x,
                y,
                pressure,
            } => {
                let state = (touch as u8) | ((click as u8) << 1);
                out.extend_from_slice(&[RICH_TOUCHPAD_EX, pad, surface, finger, state]);
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&y.to_le_bytes());
                out.extend_from_slice(&pressure.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<RichInput> {
        if b.first() != Some(&RICH_INPUT_MAGIC) {
            return None;
        }
        match *b.get(1)? {
            RICH_TOUCHPAD if b.len() >= 9 => Some(RichInput::Touchpad {
                pad: b[2],
                finger: b[3],
                active: b[4] != 0,
                x: u16::from_le_bytes([b[5], b[6]]),
                y: u16::from_le_bytes([b[7], b[8]]),
            }),
            RICH_MOTION if b.len() >= 15 => {
                let i16at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
                Some(RichInput::Motion {
                    pad: b[2],
                    gyro: [i16at(3), i16at(5), i16at(7)],
                    accel: [i16at(9), i16at(11), i16at(13)],
                })
            }
            RICH_TOUCHPAD_EX if b.len() >= 12 => Some(RichInput::TouchpadEx {
                pad: b[2],
                surface: b[3],
                finger: b[4],
                touch: b[5] & 0x01 != 0,
                click: b[5] & 0x02 != 0,
                x: i16::from_le_bytes([b[6], b[7]]),
                y: i16::from_le_bytes([b[8], b[9]]),
                pressure: u16::from_le_bytes([b[10], b[11]]),
            }),
            _ => None,
        }
    }
}

const HIDOUT_LED: u8 = 0x01;
const HIDOUT_PLAYER_LEDS: u8 = 0x02;
const HIDOUT_TRIGGER: u8 = 0x03;
const HIDOUT_TRACKPAD_HAPTIC: u8 = 0x04;

/// DualSense feedback flowing host → client (what a game wrote to the host's virtual pad).
/// Wire form `[0xCD][kind][pad][fields…]`. The rich analog of the fixed rumble datagram;
/// rumble itself stays on [`RUMBLE_MAGIC`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HidOutput {
    /// Lightbar RGB.
    Led { pad: u8, r: u8, g: u8, b: u8 },
    /// Player-indicator LEDs (low 5 bits).
    PlayerLeds { pad: u8, bits: u8 },
    /// One adaptive-trigger effect: `which` 0 = L2, 1 = R2; `effect` is the raw DualSense
    /// trigger parameter block (mode + params) for the client to replay on a real controller.
    Trigger { pad: u8, which: u8, effect: Vec<u8> },
    /// A trackpad haptic pulse for a Steam Controller's voice-coil actuators (its only "rumble").
    /// `side` 0 = right pad, 1 = left pad; `amplitude` + `period` (µs off-time) + `count` (pulses)
    /// synthesize a buzz. A client without trackpad coils drops it (or maps it to ordinary rumble).
    TrackpadHaptic {
        pad: u8,
        side: u8,
        amplitude: u16,
        period: u16,
        count: u16,
    },
}

impl HidOutput {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![HIDOUT_MAGIC];
        match self {
            HidOutput::Led { pad, r, g, b } => {
                out.extend_from_slice(&[HIDOUT_LED, *pad, *r, *g, *b])
            }
            HidOutput::PlayerLeds { pad, bits } => {
                out.extend_from_slice(&[HIDOUT_PLAYER_LEDS, *pad, *bits])
            }
            HidOutput::Trigger { pad, which, effect } => {
                out.extend_from_slice(&[HIDOUT_TRIGGER, *pad, *which]);
                out.extend_from_slice(effect);
            }
            HidOutput::TrackpadHaptic {
                pad,
                side,
                amplitude,
                period,
                count,
            } => {
                out.extend_from_slice(&[HIDOUT_TRACKPAD_HAPTIC, *pad, *side]);
                out.extend_from_slice(&amplitude.to_le_bytes());
                out.extend_from_slice(&period.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Option<HidOutput> {
        if b.first() != Some(&HIDOUT_MAGIC) {
            return None;
        }
        match *b.get(1)? {
            HIDOUT_LED if b.len() >= 6 => Some(HidOutput::Led {
                pad: b[2],
                r: b[3],
                g: b[4],
                b: b[5],
            }),
            HIDOUT_PLAYER_LEDS if b.len() >= 4 => Some(HidOutput::PlayerLeds {
                pad: b[2],
                bits: b[3],
            }),
            HIDOUT_TRIGGER if b.len() >= 4 => Some(HidOutput::Trigger {
                pad: b[2],
                which: b[3],
                effect: b[4..].to_vec(),
            }),
            HIDOUT_TRACKPAD_HAPTIC if b.len() >= 10 => Some(HidOutput::TrackpadHaptic {
                pad: b[2],
                side: b[3],
                amplitude: u16::from_le_bytes([b[4], b[5]]),
                period: u16::from_le_bytes([b[6], b[7]]),
                count: u16::from_le_bytes([b[8], b[9]]),
            }),
            _ => None,
        }
    }
}

/// Static HDR metadata, host → client: SMPTE ST.2086 mastering display colour volume + CEA-861.3
/// content light level. Tag [`HDR_META_MAGIC`]. Carried on a datagram (not [`Welcome`]) because it
/// is larger and can change mid-stream when the source's mastering intent changes; the host
/// re-sends it on keyframes so a client that dropped the best-effort datagram converges. Omitted
/// for HLG (scene-referred — no mastering metadata).
///
/// All fields use the standard HDR10 SEI fixed-point units, so they pass straight to
/// `DXGI_HDR_METADATA_HDR10` / Android `KEY_HDR_STATIC_INFO` / Apple `CAEDRMetadata` — the
/// libavcodec `AVMasteringDisplayMetadata` side needs an `AVRational` conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HdrMeta {
    /// Display primaries G, B, R as (x, y) chromaticity in 1/50000 units (the ST.2086 RGB order
    /// is G, B, R).
    pub display_primaries: [[u16; 2]; 3],
    /// White point (x, y) in 1/50000 units.
    pub white_point: [u16; 2],
    /// Max display mastering luminance, 0.0001 cd/m² units.
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m² units.
    pub min_display_mastering_luminance: u32,
    /// Maximum content light level (MaxCLL), nits. `0` = unknown.
    pub max_cll: u16,
    /// Maximum frame-average light level (MaxFALL), nits. `0` = unknown.
    pub max_fall: u16,
}

/// HDR static-metadata datagram tag, host → client (the static analog of the per-frame VUI;
/// see [`HdrMeta`]). Next tag after [`HIDOUT_MAGIC`].
pub const HDR_META_MAGIC: u8 = 0xCE;

/// Wire length of an [`HDR_META_MAGIC`] datagram: tag + 6×u16 primaries + 2×u16 white + 2×u32
/// luminance + 2×u16 CLL/FALL = 29 bytes.
const HDR_META_LEN: usize = 1 + 12 + 4 + 8 + 4;

/// Encode an [`HdrMeta`] into a [`HDR_META_MAGIC`] datagram.
pub fn encode_hdr_meta_datagram(m: &HdrMeta) -> Vec<u8> {
    let mut b = Vec::with_capacity(HDR_META_LEN);
    b.push(HDR_META_MAGIC);
    for p in m.display_primaries.iter() {
        b.extend_from_slice(&p[0].to_le_bytes());
        b.extend_from_slice(&p[1].to_le_bytes());
    }
    b.extend_from_slice(&m.white_point[0].to_le_bytes());
    b.extend_from_slice(&m.white_point[1].to_le_bytes());
    b.extend_from_slice(&m.max_display_mastering_luminance.to_le_bytes());
    b.extend_from_slice(&m.min_display_mastering_luminance.to_le_bytes());
    b.extend_from_slice(&m.max_cll.to_le_bytes());
    b.extend_from_slice(&m.max_fall.to_le_bytes());
    b
}

/// Parse a [`HDR_META_MAGIC`] datagram → [`HdrMeta`]. `None` on bad tag or a short/truncated buffer
/// (every attacker-controlled field is bounds-checked by the fixed length before any read).
pub fn decode_hdr_meta_datagram(b: &[u8]) -> Option<HdrMeta> {
    if b.len() < HDR_META_LEN || b[0] != HDR_META_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    Some(HdrMeta {
        display_primaries: [
            [u16at(1), u16at(3)],
            [u16at(5), u16at(7)],
            [u16at(9), u16at(11)],
        ],
        white_point: [u16at(13), u16at(15)],
        max_display_mastering_luminance: u32at(17),
        min_display_mastering_luminance: u32at(21),
        max_cll: u16at(25),
        max_fall: u16at(27),
    })
}

/// Per-AU host-timing datagram tag, host → client (see [`HostTiming`]). Next tag after
/// [`HDR_META_MAGIC`]. Emitted once per access unit, right after its last packet left the host's
/// socket, and only when the client advertised [`VIDEO_CAP_HOST_TIMING`].
pub const HOST_TIMING_MAGIC: u8 = 0xCF;

/// One access unit's host-side processing time: capture → fully sent (the whole host pipeline —
/// capture read/convert, encode, FEC+seal, paced send). The client correlates it to the AU by
/// `pts_ns` (the AU's capture stamp, unique per frame) and derives
/// `network = (received + clock_offset − pts_ns) − host_us`, so the unified-stats equation's
/// `host+network` stage splits into two per-frame-tiling terms. Best-effort like every side-plane
/// datagram: a lost 0xCF just means that frame contributes no host/network sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostTiming {
    /// The AU's capture stamp (host capture clock — matches the AU's `pts_ns` exactly).
    pub pts_ns: u64,
    /// Host capture→sent duration, µs (saturated at `u32::MAX` ≈ 71 min — far past the 10 s
    /// client-side sanity clamp anyway).
    pub host_us: u32,
}

/// Wire length of a [`HOST_TIMING_MAGIC`] datagram: tag + u64 pts + u32 µs = 13 bytes.
const HOST_TIMING_LEN: usize = 1 + 8 + 4;

/// Encode a [`HostTiming`] into a [`HOST_TIMING_MAGIC`] datagram.
pub fn encode_host_timing_datagram(t: &HostTiming) -> Vec<u8> {
    let mut b = Vec::with_capacity(HOST_TIMING_LEN);
    b.push(HOST_TIMING_MAGIC);
    b.extend_from_slice(&t.pts_ns.to_le_bytes());
    b.extend_from_slice(&t.host_us.to_le_bytes());
    b
}

/// Parse a [`HOST_TIMING_MAGIC`] datagram → [`HostTiming`]. `None` on bad tag or a short buffer
/// (the fixed length bounds every read before it happens).
pub fn decode_host_timing_datagram(b: &[u8]) -> Option<HostTiming> {
    if b.len() < HOST_TIMING_LEN || b[0] != HOST_TIMING_MAGIC {
        return None;
    }
    Some(HostTiming {
        pts_ns: u64::from_le_bytes(b[1..9].try_into().unwrap()),
        host_us: u32::from_le_bytes(b[9..13].try_into().unwrap()),
    })
}

/// Async framed-message IO over a quinn stream (`u16 LE length || payload`).
pub mod io {
    /// Read one framed message (bounded at 64 KiB — control messages are tiny).
    pub async fn read_msg(recv: &mut quinn::RecvStream) -> std::io::Result<Vec<u8>> {
        let mut len = [0u8; 2];
        recv.read_exact(&mut len)
            .await
            .map_err(std::io::Error::other)?;
        let n = u16::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; n];
        recv.read_exact(&mut buf)
            .await
            .map_err(std::io::Error::other)?;
        Ok(buf)
    }

    /// Write one framed message.
    pub async fn write_msg(send: &mut quinn::SendStream, payload: &[u8]) -> std::io::Result<()> {
        send.write_all(&super::frame(payload))
            .await
            .map_err(std::io::Error::other)
    }
}

/// One wall-clock skew-handshake outcome (see [`clock_sync`]).
pub struct ClockSkew {
    /// Host clock minus client clock, ns: add it to a client timestamp to express it in host time.
    pub offset_ns: i64,
    /// Round-trip time of the minimum-RTT sample, ns.
    pub rtt_ns: u64,
    /// How many probe rounds the host answered.
    pub rounds: usize,
}

/// Run the wall-clock skew handshake from the client side over the (already-open) control stream:
/// `ROUNDS` [`ClockProbe`]/[`ClockEcho`] round-trips, returning the host↔client offset from the
/// minimum-RTT sample. `None` if the host never answers (an old host) — the caller then assumes a
/// shared clock. Each read is bounded so a silent host can't wedge session start. Shared by the
/// reference client and the embeddable connector; uses the realtime clock the host stamps `pts_ns`
/// with, so the offset aligns a client receive instant to the host's capture clock.
pub async fn clock_sync(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> Option<ClockSkew> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
    const ROUNDS: usize = 8;
    let read_timeout = Duration::from_secs(2);
    let mut samples: Vec<(u64, u64, u64, u64)> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t1 = now_ns();
        let probe = ClockProbe { t1_ns: t1 }.encode();
        if io::write_msg(send, &probe).await.is_err() {
            break;
        }
        let read = tokio::time::timeout(read_timeout, io::read_msg(recv)).await;
        let echo = match read {
            Ok(Ok(b)) => match ClockEcho::decode(&b) {
                Ok(e) => e,
                Err(_) => break,
            },
            _ => break, // timeout or stream error -> old host / no skew support
        };
        samples.push((echo.t1_ns, echo.t2_ns, echo.t3_ns, now_ns()));
    }
    clock_offset_ns(&samples).map(|(offset_ns, rtt_ns)| ClockSkew {
        offset_ns,
        rtt_ns,
        rounds: samples.len(),
    })
}

/// quinn endpoint constructors. Host: self-signed identity (fresh, or persisted PEMs via
/// [`endpoint::server_with_identity`]). Client: fingerprint pinning / TOFU via
/// [`endpoint::client_pinned`] ([`endpoint::client_insecure`] is the no-pin special case).
pub mod endpoint {
    use std::sync::{Arc, Mutex};

    /// Shared QUIC transport tuning for BOTH the host and client endpoints. Keep-alive is the
    /// load-bearing setting: with quinn's defaults it is OFF, so any quiet stretch on the
    /// connection (no input, audio muted or stalled, a capture hiccup, a mode change) lets the
    /// idle timer run out and quinn closes the session — surfacing to the embedder as
    /// `next_au` → Closed. The native equivalent of Moonlight's ENet keepalive: a small PING
    /// every `KEEP_ALIVE` keeps the path warm. The interval sits well under `MAX_IDLE` so
    /// several keepalives can be lost back-to-back (a wifi roam, a brief blip) without a false
    /// close, while a genuinely dead peer is still detected within `MAX_IDLE`.
    /// The default control-connection idle timeout (disconnect-detection latency). A vanished client
    /// is declared dead within this window — the Windows IDD-push path needs it short so a RECONNECT
    /// recreates a fresh virtual monitor instead of joining the still-lingering old session; the Linux
    /// path pairs it with the same-client reconnect preempt. Host-tunable via `server_with_identity_idle`.
    pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

    fn stream_transport() -> Arc<quinn::TransportConfig> {
        stream_transport_idle(DEFAULT_IDLE_TIMEOUT)
    }

    /// Transport config with a caller-chosen idle timeout (disconnect-detection latency). The
    /// keep-alive interval tracks it at half the idle window (capped at the default 4s), so a live
    /// path is PINGed at least twice per window and a single lost PING (wifi roam / brief blip) won't
    /// false-close. `idle` is clamped to a ≥1s floor so a misconfigured tiny value can't tear live
    /// sessions down. Active sessions are unaffected either way: video keeps the connection live and
    /// the keep-alive holds it open through quiet control periods.
    fn stream_transport_idle(idle: std::time::Duration) -> Arc<quinn::TransportConfig> {
        use std::time::Duration;
        let idle = idle.max(Duration::from_secs(1));
        let keep_alive = (idle / 2).min(Duration::from_secs(4));
        let mut t = quinn::TransportConfig::default();
        t.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(idle).expect("clamped idle timeout is a valid QUIC value"),
        ));
        t.keep_alive_interval(Some(keep_alive));
        // The datagram planes (audio/rumble/hidout/host-timing host→client; mic/rich-input
        // client→host) carry realtime state, not bulk data — but they are congestion-controlled,
        // unlike video, which rides its own latest-wins UDP path. quinn's default 1 MiB datagram
        // send buffer is a FIFO that only sheds oldest-first at the cap, so on a congested link
        // (Wi-Fi under streaming load) it holds tens of seconds of Opus: audio and rumble build a
        // standing delay that never drains while video stays live. Capping the buffer makes the
        // plane latest-wins at the source — ~200 ms of stereo Opus (proportionally less at
        // surround bitrates), so sustained congestion costs concealable drops, never lag.
        t.datagram_send_buffer_size(4 * 1024);
        Arc::new(t)
    }

    /// Server endpoint with a fresh self-signed certificate (tests/dev — production hosts
    /// persist an identity and use [`server_with_identity`] so clients can pin it).
    pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
        let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
            .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
        let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        server_from_der(cert_der, key_der.into(), addr, DEFAULT_IDLE_TIMEOUT)
    }

    /// Server endpoint from a persisted PEM identity (certificate + PKCS#8 private key) —
    /// the host's long-lived self-signed cert, so the fingerprint clients pin is stable
    /// across restarts. Uses the [`DEFAULT_IDLE_TIMEOUT`]; see [`server_with_identity_idle`] to tune it.
    pub fn server_with_identity(
        addr: std::net::SocketAddr,
        cert_pem: &str,
        key_pem: &str,
    ) -> anyhow_result::Result<quinn::Endpoint> {
        server_with_identity_idle(addr, cert_pem, key_pem, DEFAULT_IDLE_TIMEOUT)
    }

    /// Like [`server_with_identity`] but with a host-chosen control-connection idle timeout — the
    /// disconnect-detection latency (how long a vanished client takes to be declared dead). Shorter =
    /// faster teardown/linger of a dropped session; the value is clamped to a ≥1s floor and its
    /// keep-alive scales with it so a live session never false-closes.
    pub fn server_with_identity_idle(
        addr: std::net::SocketAddr,
        cert_pem: &str,
        key_pem: &str,
        idle: std::time::Duration,
    ) -> anyhow_result::Result<quinn::Endpoint> {
        use rustls::pki_types::pem::PemObject;
        let cert_der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
        let key_der = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("key pem: {e}")))?;
        server_from_der(cert_der, key_der, addr, idle)
    }

    /// Fixed ALPN for the punktfunk/1 QUIC handshake. Pinning it rejects a cross-protocol peer at the
    /// TLS layer (defense-in-depth) and makes the wire protocol explicit. Both ends set the SAME value;
    /// a host with ALPN configured rejects a client that offers none, so client + host must be updated
    /// together (acceptable while the protocol/ABI is still evolving).
    const QUIC_ALPN: &[u8] = b"pkf1";

    fn server_from_der(
        cert_der: rustls::pki_types::CertificateDer<'static>,
        key_der: rustls::pki_types::PrivateKeyDer<'static>,
        addr: std::net::SocketAddr,
        idle: std::time::Duration,
    ) -> anyhow_result::Result<quinn::Endpoint> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Client auth is OFFERED but optional: a client that presents its self-signed
        // identity is fingerprinted post-handshake (pairing / --require-pairing checks);
        // one that presents none still connects (and is rejected at the app layer when
        // pairing is required).
        let mut rustls_cfg = rustls::ServerConfig::builder()
            .with_client_cert_verifier(Arc::new(AcceptAnyClientCert))
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|e| anyhow_result::Error::msg(format!("server config: {e}")))?;
        rustls_cfg.alpn_protocols = vec![QUIC_ALPN.to_vec()];
        let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
            .map_err(|e| anyhow_result::Error::msg(format!("quic server config: {e}")))?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_cfg));
        server_config.transport_config(stream_transport_idle(idle)); // keep-alive — see stream_transport_idle
        Ok(quinn::Endpoint::server(server_config, addr)?)
    }

    /// Generate a fresh self-signed identity (certificate + PKCS#8 key, both PEM) — what a
    /// client persists once and presents on every connect so hosts can recognize it.
    pub fn generate_identity() -> anyhow_result::Result<(String, String)> {
        let cert = rcgen::generate_simple_self_signed(vec!["punktfunk-client".into()])
            .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
        Ok((cert.cert.pem(), cert.key_pair.serialize_pem()))
    }

    /// Fingerprint of the client certificate a connection presented (host side), if any.
    pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<[u8; 32]> {
        let identity = conn.peer_identity()?;
        let certs = identity
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        certs.first().map(|c| cert_fingerprint(c.as_ref()))
    }

    /// SHA-256 of a certificate's DER encoding — the fingerprint clients pin.
    pub fn cert_fingerprint(cert_der: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        sha2::Sha256::digest(cert_der).into()
    }

    /// Fingerprint of a PEM-encoded certificate (what a host logs/shows for pairing UX —
    /// must match what the client's verifier computes from the DER on the wire).
    pub fn fingerprint_of_pem(cert_pem: &str) -> anyhow_result::Result<[u8; 32]> {
        use rustls::pki_types::pem::PemObject;
        let der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
        Ok(cert_fingerprint(der.as_ref()))
    }

    /// Client endpoint that skips certificate verification (TOFU bootstrap — read the
    /// observed fingerprint off the slot and pin it on the next connect).
    pub fn client_insecure() -> anyhow_result::Result<quinn::Endpoint> {
        client_pinned(None).0
    }

    /// What [`client_pinned`] returns: the endpoint plus the slot the verifier writes the
    /// observed host fingerprint into during the handshake.
    pub type PinnedClient = (
        anyhow_result::Result<quinn::Endpoint>,
        Arc<Mutex<Option<[u8; 32]>>>,
    );

    /// Client endpoint that verifies the host by certificate fingerprint.
    ///
    /// `pin = Some(sha256)` rejects any host whose leaf cert doesn't hash to `sha256`;
    /// `None` accepts any (trust-on-first-use). Either way the observed fingerprint is
    /// written to the returned slot during the handshake, so a TOFU caller can persist it.
    pub fn client_pinned(pin: Option<[u8; 32]>) -> PinnedClient {
        client_pinned_with_identity(pin, None)
    }

    /// [`client_pinned`], additionally presenting a client identity (PEM cert + PKCS#8
    /// key) via TLS client auth — how a paired client identifies itself to the host.
    pub fn client_pinned_with_identity(
        pin: Option<[u8; 32]>,
        identity: Option<(&str, &str)>,
    ) -> PinnedClient {
        let observed = Arc::new(Mutex::new(None));
        let ep = (|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let builder = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinVerify {
                    pin,
                    observed: observed.clone(),
                }));
            let mut rustls_cfg = match identity {
                None => builder.with_no_client_auth(),
                Some((cert_pem, key_pem)) => {
                    use rustls::pki_types::pem::PemObject;
                    let cert =
                        rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
                            .map_err(|e| {
                                anyhow_result::Error::msg(format!("client cert pem: {e}"))
                            })?;
                    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                        .map_err(|e| anyhow_result::Error::msg(format!("client key pem: {e}")))?;
                    builder
                        .with_client_auth_cert(vec![cert], key)
                        .map_err(|e| anyhow_result::Error::msg(format!("client auth: {e}")))?
                }
            };
            // Must match the server's ALPN ([`QUIC_ALPN`]) or the handshake is rejected.
            rustls_cfg.alpn_protocols = vec![QUIC_ALPN.to_vec()];
            let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
                .map_err(|e| anyhow_result::Error::msg(format!("quic client config: {e}")))?;
            let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_cfg));
            client_cfg.transport_config(stream_transport()); // keep-alive — see stream_transport
            let mut ep = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())?;
            ep.set_default_client_config(client_cfg);
            Ok(ep)
        })();
        (ep, observed)
    }

    /// Minimal error plumbing without pulling anyhow into punktfunk-core's public API.
    pub mod anyhow_result {
        pub type Result<T> = std::result::Result<T, Error>;
        #[derive(Debug)]
        pub struct Error(String);
        impl Error {
            pub fn msg(s: String) -> Self {
                Error(s)
            }
        }
        impl std::fmt::Display for Error {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for Error {}
        impl From<std::io::Error> for Error {
            fn from(e: std::io::Error) -> Self {
                Error(e.to_string())
            }
        }
    }

    /// Fingerprint-pinning verifier: trust is the SHA-256 of the host's (self-signed) leaf
    /// cert, not a CA chain. With no pin it accepts any cert (TOFU) but still records what
    /// it saw, so the embedder can persist the fingerprint and pin it from then on.
    /// Server-side client-cert verifier: accept any (self-signed) client certificate but
    /// verify the handshake signature for real — possession of the presented cert's key is
    /// what makes the post-handshake fingerprint ([`peer_fingerprint`]) meaningful.
    /// Authorization (is this fingerprint paired?) happens at the application layer.
    #[derive(Debug)]
    struct AcceptAnyClientCert;

    impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
        fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
            &[]
        }

        fn client_auth_mandatory(&self) -> bool {
            false // unpaired/legacy clients still connect; gating is per-feature
        }

        fn verify_client_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error>
        {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    #[derive(Debug)]
    struct PinVerify {
        pin: Option<[u8; 32]>,
        observed: Arc<Mutex<Option<[u8; 32]>>>,
    }

    impl rustls::client::danger::ServerCertVerifier for PinVerify {
        fn verify_server_cert(
            &self,
            end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error>
        {
            let fp = cert_fingerprint(end_entity.as_ref());
            *self.observed.lock().unwrap() = Some(fp);
            if let Some(expected) = self.pin {
                if fp != expected {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::ApplicationVerificationFailure,
                    ));
                }
            }
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        // The handshake signatures MUST be verified for real even though we pin the cert:
        // CertificateVerify is what proves the peer *holds the pinned cert's private key* —
        // skip it and an active MITM can replay the host's (public) certificate, match the
        // pin, and complete the handshake with its own key.
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
        {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_roundtrip() {
        let w = Welcome {
            abi_version: 1,
            udp_port: 9999,
            mode: Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 240,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [1, 2, 3, 4],
            frames: 600,
            compositor: CompositorPref::Gamescope,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 50_000,
            bit_depth: 10,
            color: ColorInfo::HDR10_BT2020_PQ,
            chroma_format: CHROMA_IDC_444,
            audio_channels: 2,
            codec: CODEC_H264, // exercise a non-default codec through the roundtrip
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);
    }

    #[test]
    fn codec_negotiation_and_back_compat() {
        // resolve_codec precedence (HEVC > AV1 > H.264), no preference (0).
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_HEVC | CODEC_AV1, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_AV1, CODEC_AV1 | CODEC_H264, 0),
            Some(CODEC_AV1)
        );
        assert_eq!(resolve_codec(CODEC_H264, CODEC_H264, 0), Some(CODEC_H264));
        // A software host (H.264 only) + an HEVC-only client share nothing → refuse.
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, 0), None);
        // An older client (0 = no codec byte) is treated as HEVC-only.
        assert_eq!(
            resolve_codec(0, CODEC_HEVC | CODEC_H264, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(resolve_codec(0, CODEC_H264, 0), None);

        // Soft preference: honored when the host can also emit it, overriding precedence...
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_H264 | CODEC_HEVC, CODEC_H264),
            Some(CODEC_H264)
        );
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_AV1, CODEC_HEVC | CODEC_AV1, CODEC_AV1),
            Some(CODEC_AV1)
        );
        // ...but falls back to precedence when the preferred codec isn't in the shared set.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_H264, CODEC_HEVC | CODEC_H264, CODEC_AV1),
            Some(CODEC_HEVC)
        );
        // A preference the host can't emit still can't rescue a no-shared-codec case.
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, CODEC_HEVC), None);

        // A Hello advertising codecs roundtrips, and the wire form of a codec-only Hello decodes on
        // a build that ignores the trailing byte (back-compat: extra bytes are skipped).
        let h = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2, // stereo — forces the video_caps/audio_channels placeholders
            video_codecs: CODEC_H264 | CODEC_HEVC,
            preferred_codec: CODEC_H264,
        };
        let enc = h.encode();
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec.video_codecs, CODEC_H264 | CODEC_HEVC);
        assert_eq!(dec.preferred_codec, CODEC_H264);
        // Drop the preferred_codec byte → still decodes, video_codecs intact, preference gone.
        let no_pref = &enc[..enc.len() - 1];
        assert_eq!(
            Hello::decode(no_pref).unwrap().video_codecs,
            CODEC_H264 | CODEC_HEVC
        );
        assert_eq!(Hello::decode(no_pref).unwrap().preferred_codec, 0);
        // A pre-codec Hello (no video_codecs/preferred bytes) decodes to 0 → HEVC-only.
        let legacy = &enc[..enc.len() - 2];
        assert_eq!(Hello::decode(legacy).unwrap().video_codecs, 0);
        assert_eq!(Hello::decode(legacy).unwrap().preferred_codec, 0);

        // A pre-codec Welcome (no codec byte) decodes to HEVC.
        let mut w = Welcome::decode(
            &Welcome {
                abi_version: 2,
                udp_port: 1,
                mode: h.mode,
                fec: FecConfig {
                    scheme: FecScheme::Gf16,
                    fec_percent: 0,
                    max_data_per_block: 1024,
                },
                shard_payload: 1024,
                encrypt: false,
                key: [0; 16],
                salt: [0; 4],
                frames: 0,
                compositor: CompositorPref::Auto,
                gamepad: GamepadPref::Auto,
                bitrate_kbps: 0,
                bit_depth: 8,
                color: ColorInfo::SDR_BT709,
                chroma_format: CHROMA_IDC_420,
                audio_channels: 2,
                codec: CODEC_H264,
            }
            .encode(),
        )
        .unwrap();
        assert_eq!(w.codec, CODEC_H264);
        w.codec = CODEC_HEVC;
        let wenc = w.encode();
        assert_eq!(
            Welcome::decode(&wenc[..wenc.len() - 1]).unwrap().codec,
            CODEC_HEVC
        );
    }

    #[test]
    fn hdr_meta_datagram_roundtrip_and_truncation() {
        let m = HdrMeta {
            // BT.2020 display primaries in 1/50000 units (the DXGI/ST.2086 reference values).
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],                 // D65
            max_display_mastering_luminance: 10_000_000, // 1000 nits in 0.0001 cd/m²
            min_display_mastering_luminance: 1,          // 0.0001 nits
            max_cll: 1000,
            max_fall: 400,
        };
        let d = encode_hdr_meta_datagram(&m);
        assert_eq!(d[0], HDR_META_MAGIC);
        assert_eq!(decode_hdr_meta_datagram(&d), Some(m));
        // Truncated buffers and a wrong tag are rejected (never partially read).
        for n in 0..d.len() {
            assert_eq!(decode_hdr_meta_datagram(&d[..n]), None);
        }
        let mut bad = d.clone();
        bad[0] = HIDOUT_MAGIC;
        assert_eq!(decode_hdr_meta_datagram(&bad), None);
    }

    #[test]
    fn host_timing_datagram_roundtrip_and_truncation() {
        let t = HostTiming {
            pts_ns: 1_751_500_000_123_456_789, // a realistic 2026 CLOCK_REALTIME capture stamp
            host_us: 4_321,
        };
        let d = encode_host_timing_datagram(&t);
        assert_eq!(d[0], HOST_TIMING_MAGIC);
        assert_eq!(d.len(), 13);
        assert_eq!(decode_host_timing_datagram(&d), Some(t));
        // Truncated buffers and a wrong tag are rejected (never partially read).
        for n in 0..d.len() {
            assert_eq!(decode_host_timing_datagram(&d[..n]), None);
        }
        let mut bad = d.clone();
        bad[0] = HDR_META_MAGIC;
        assert_eq!(decode_host_timing_datagram(&bad), None);
    }

    #[test]
    fn hello_start_roundtrip() {
        let h = Hello {
            abi_version: 1,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 120,
            },
            compositor: CompositorPref::Kwin,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 25_000,
            name: Some("Test Device".into()),
            launch: Some("steam:570".into()),
            video_caps: VIDEO_CAP_10BIT,
            audio_channels: 2,
            video_codecs: CODEC_H264 | CODEC_HEVC, // exercise the codec bitfield roundtrip
            preferred_codec: CODEC_HEVC,
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn compositor_pref_wire_and_names() {
        for p in [
            CompositorPref::Auto,
            CompositorPref::Kwin,
            CompositorPref::Wlroots,
            CompositorPref::Mutter,
            CompositorPref::Gamescope,
        ] {
            assert_eq!(CompositorPref::from_u8(p.to_u8()), p);
            assert_eq!(CompositorPref::from_name(p.as_str()), Some(p));
        }
        // Aliases + unknowns.
        assert_eq!(CompositorPref::from_name("KDE"), Some(CompositorPref::Kwin));
        assert_eq!(
            CompositorPref::from_name("sway"),
            Some(CompositorPref::Wlroots)
        );
        assert_eq!(CompositorPref::from_name("nope"), None);
        // Unknown wire byte degrades to Auto (forward-compatible).
        assert_eq!(CompositorPref::from_u8(200), CompositorPref::Auto);
    }

    #[test]
    fn gamepad_pref_wire_and_names() {
        for p in [
            GamepadPref::Auto,
            GamepadPref::Xbox360,
            GamepadPref::DualSense,
            GamepadPref::XboxOne,
            GamepadPref::DualShock4,
        ] {
            assert_eq!(GamepadPref::from_u8(p.to_u8()), p);
            assert_eq!(GamepadPref::from_name(p.as_str()), Some(p));
        }
        // Distinct wire bytes (forward-compat with peers that only know 0..=2).
        assert_eq!(GamepadPref::XboxOne.to_u8(), 3);
        assert_eq!(GamepadPref::DualShock4.to_u8(), 4);
        // Aliases + unknowns.
        assert_eq!(GamepadPref::from_name("PS5"), Some(GamepadPref::DualSense));
        assert_eq!(GamepadPref::from_name("x360"), Some(GamepadPref::Xbox360));
        assert_eq!(GamepadPref::from_name("ps4"), Some(GamepadPref::DualShock4));
        assert_eq!(GamepadPref::from_name("DS4"), Some(GamepadPref::DualShock4));
        assert_eq!(
            GamepadPref::from_name("xbox-one"),
            Some(GamepadPref::XboxOne)
        );
        assert_eq!(GamepadPref::from_name("series"), Some(GamepadPref::XboxOne));
        assert_eq!(GamepadPref::from_name("nope"), None);
        // Unknown wire byte degrades to Auto (forward-compatible).
        assert_eq!(GamepadPref::from_u8(200), GamepadPref::Auto);
    }

    #[test]
    fn hello_welcome_compositor_back_compat() {
        // Trailing optional bytes (compositor at 20/53, gamepad at 21/54): a legacy peer's
        // shorter message still decodes (missing fields = Auto), and a legacy peer reading a
        // new message ignores the trailing bytes. Simulate both directions by truncation.
        let h = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Mutter,
            gamepad: GamepadPref::DualSense,
            bitrate_kbps: 80_000,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
        };
        let enc = h.encode();
        assert_eq!(enc.len(), 26);
        // Legacy (20-byte) Hello → both Auto, no bitrate, mode intact.
        let legacy = Hello::decode(&enc[..20]).unwrap();
        assert_eq!(legacy.compositor, CompositorPref::Auto);
        assert_eq!(legacy.gamepad, GamepadPref::Auto);
        assert_eq!(legacy.bitrate_kbps, 0);
        assert_eq!(legacy.mode, h.mode);
        // Compositor-era (21-byte) Hello → compositor intact, gamepad Auto.
        let mid = Hello::decode(&enc[..21]).unwrap();
        assert_eq!(mid.compositor, CompositorPref::Mutter);
        assert_eq!(mid.gamepad, GamepadPref::Auto);
        // Gamepad-era (22-byte) Hello → compositor + gamepad intact, bitrate 0 (host default).
        let pre_bitrate = Hello::decode(&enc[..22]).unwrap();
        assert_eq!(pre_bitrate.gamepad, GamepadPref::DualSense);
        assert_eq!(pre_bitrate.bitrate_kbps, 0);
        // Full message → bitrate intact.
        assert_eq!(Hello::decode(&enc).unwrap().bitrate_kbps, 80_000);

        let w = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: h.mode,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [3u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Kwin,
            gamepad: GamepadPref::Xbox360,
            bitrate_kbps: 120_000,
            bit_depth: 10,
            color: ColorInfo::HDR10_BT2020_PQ,
            chroma_format: CHROMA_IDC_444,
            audio_channels: 6, // 5.1 — exercises the non-default trailing byte
            codec: CODEC_HEVC,
        };
        let wenc = w.encode();
        assert_eq!(wenc.len(), 67); // 60 base + 4 colour + 1 chroma + 1 audio-channels + 1 codec byte
        let legacy_w = Welcome::decode(&wenc[..53]).unwrap();
        assert_eq!(legacy_w.compositor, CompositorPref::Auto);
        assert_eq!(legacy_w.gamepad, GamepadPref::Auto);
        assert_eq!(legacy_w.bitrate_kbps, 0);
        assert_eq!(legacy_w.frames, 0);
        assert_eq!(legacy_w.key, w.key);
        let mid_w = Welcome::decode(&wenc[..54]).unwrap();
        assert_eq!(mid_w.compositor, CompositorPref::Kwin);
        assert_eq!(mid_w.gamepad, GamepadPref::Auto);
        // Gamepad-era (55-byte) Welcome → gamepad intact, bitrate 0 (unknown).
        let pre_bitrate_w = Welcome::decode(&wenc[..55]).unwrap();
        assert_eq!(pre_bitrate_w.gamepad, GamepadPref::Xbox360);
        assert_eq!(pre_bitrate_w.bitrate_kbps, 0);
        assert_eq!(pre_bitrate_w.bit_depth, 8); // older host (no trailing byte) → 8-bit assumed
        assert_eq!(legacy_w.bit_depth, 8);
        // A pre-colour (60-byte) Welcome → SDR BT.709 (the only colour those hosts produced).
        let pre_color_w = Welcome::decode(&wenc[..60]).unwrap();
        assert_eq!(pre_color_w.bit_depth, 10);
        assert_eq!(pre_color_w.color, ColorInfo::SDR_BT709);
        assert_eq!(pre_color_w.chroma_format, CHROMA_IDC_420); // pre-chroma host → 4:2:0
        assert_eq!(legacy_w.color, ColorInfo::SDR_BT709);
        assert_eq!(legacy_w.chroma_format, CHROMA_IDC_420);
        // A pre-chroma (64-byte) Welcome carries colour but no chroma/audio bytes → 4:2:0 + stereo.
        let pre_chroma_w = Welcome::decode(&wenc[..64]).unwrap();
        assert_eq!(pre_chroma_w.color, ColorInfo::HDR10_BT2020_PQ);
        assert_eq!(pre_chroma_w.chroma_format, CHROMA_IDC_420);
        assert_eq!(pre_chroma_w.audio_channels, 2); // audio byte (offset 65) absent → stereo
                                                    // A pre-audio (65-byte) Welcome carries chroma but no audio byte → 4:4:4 + stereo.
        let pre_audio_w = Welcome::decode(&wenc[..65]).unwrap();
        assert_eq!(pre_audio_w.chroma_format, CHROMA_IDC_444);
        assert_eq!(pre_audio_w.audio_channels, 2);
        assert_eq!(Welcome::decode(&wenc).unwrap().bitrate_kbps, 120_000);
        assert_eq!(Welcome::decode(&wenc).unwrap().bit_depth, 10); // full form carries it
        assert_eq!(
            Welcome::decode(&wenc).unwrap().color,
            ColorInfo::HDR10_BT2020_PQ
        );
        assert_eq!(
            Welcome::decode(&wenc).unwrap().chroma_format,
            CHROMA_IDC_444
        ); // full form carries 4:4:4
        assert_eq!(Welcome::decode(&wenc).unwrap().audio_channels, 6); // ...and 5.1
    }

    #[test]
    fn hello_name_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: Some("Enrico's MacBook".into()),
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
        };
        let enc = base.encode();
        assert_eq!(
            Hello::decode(&enc).unwrap().name.as_deref(),
            Some("Enrico's MacBook")
        );
        // A bitrate-era (26-byte) peer reading a named Hello ignores the trailing name; a named
        // host reading a bitrate-era Hello decodes name = None.
        assert_eq!(Hello::decode(&enc[..26]).unwrap().name, None);
        // No name → wire form is byte-identical to the bitrate-era message (26 bytes).
        let unnamed = Hello {
            name: None,
            ..base.clone()
        };
        assert_eq!(unnamed.encode().len(), 26);
        // Over-long names truncate to a char boundary within HELLO_NAME_MAX on encode.
        let long = Hello {
            name: Some(format!("{}ü", "x".repeat(HELLO_NAME_MAX - 1))), // ü straddles the cap
            ..base.clone()
        };
        let dec = Hello::decode(&long.encode()).unwrap();
        let n = dec.name.expect("truncated name still present");
        assert!(n.len() <= HELLO_NAME_MAX && n.starts_with('x'));
        // A corrupt length byte (longer than the buffer) or bad UTF-8 degrades to None, never Err.
        let mut bad_len = unnamed.encode();
        bad_len.push(40); // claims 40 name bytes, none follow
        assert_eq!(Hello::decode(&bad_len).unwrap().name, None);
        let mut bad_utf8 = unnamed.encode();
        bad_utf8.extend_from_slice(&[2, 0xFF, 0xFE]);
        assert_eq!(Hello::decode(&bad_utf8).unwrap().name, None);
    }

    #[test]
    fn hello_launch_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
        };
        // launch alone (no name): a zero-length name placeholder keeps the offset deterministic.
        let with_launch = Hello {
            launch: Some("steam:570".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&with_launch.encode()).unwrap(), with_launch);
        // launch + name together.
        let both = Hello {
            name: Some("Enrico's Mac".into()),
            launch: Some("custom:abc123".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // name but no launch (a name-era client): launch decodes None.
        let name_only = Hello {
            name: Some("Enrico's Mac".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&name_only.encode()).unwrap().launch, None);
        // Neither field → still the 26-byte bitrate-era form (no launch placeholder emitted).
        assert_eq!(base.encode().len(), 26);
        assert_eq!(Hello::decode(&base.encode()).unwrap().launch, None);
        // A bitrate-era (26-byte) peer reading a launch-bearing Hello ignores it.
        assert_eq!(
            Hello::decode(&with_launch.encode()[..26]).unwrap().launch,
            None
        );
        // Over-long ids truncate on a char boundary within HELLO_LAUNCH_MAX.
        let long = Hello {
            launch: Some(format!("{}ü", "x".repeat(HELLO_LAUNCH_MAX - 1))),
            ..base.clone()
        };
        let dec = Hello::decode(&long.encode())
            .unwrap()
            .launch
            .expect("present");
        assert!(dec.len() <= HELLO_LAUNCH_MAX && dec.starts_with('x'));
    }

    #[test]
    fn reconfigure_roundtrip() {
        let rq = Reconfigure {
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 144,
            },
        };
        assert_eq!(Reconfigure::decode(&rq.encode()).unwrap(), rq);
        for accepted in [true, false] {
            let rs = Reconfigured {
                accepted,
                mode: rq.mode,
            };
            assert_eq!(Reconfigured::decode(&rs.encode()).unwrap(), rs);
        }
        // The type byte separates the post-handshake messages from each other.
        assert!(Reconfigure::decode(
            &Reconfigured {
                accepted: true,
                mode: rq.mode
            }
            .encode()
        )
        .is_err());
    }

    #[test]
    fn request_keyframe_roundtrip() {
        let bytes = RequestKeyframe.encode();
        assert!(RequestKeyframe::decode(&bytes).is_ok());
        // Distinct from the other control messages — its type byte must not collide.
        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        assert!(RequestKeyframe::decode(&Reconfigure { mode }.encode()).is_err());
        assert!(Reconfigure::decode(&bytes).is_err());
        // Length is exact (no trailing bytes accepted).
        assert!(RequestKeyframe::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
    }

    #[test]
    fn loss_report_roundtrip() {
        for loss_ppm in [0u32, 1, 12_345, 50_000, 1_000_000] {
            let r = LossReport { loss_ppm };
            assert_eq!(LossReport::decode(&r.encode()).unwrap(), r);
        }
        // Disjoint from the other control messages (type byte + length).
        assert!(LossReport::decode(&RequestKeyframe.encode()).is_err());
        assert!(RequestKeyframe::decode(&LossReport { loss_ppm: 0 }.encode()).is_err());
        assert!(LossReport::decode(
            &[LossReport { loss_ppm: 0 }.encode().as_slice(), &[0]].concat()
        )
        .is_err());
    }

    #[test]
    fn window_loss_ppm_estimates_and_caps() {
        // No traffic → 0. A clean window (nothing recovered) → 0.
        assert_eq!(window_loss_ppm(0, 0, 0), 0);
        assert_eq!(window_loss_ppm(0, 1000, 0), 0);
        // 50 recovered of 1000 total (950 received + 50 recovered) = 5%.
        assert_eq!(window_loss_ppm(50, 950, 0), 50_000);
        // An unrecoverable frame adds the +5% bump (push FEC past the current cap).
        assert_eq!(window_loss_ppm(50, 950, 1), 100_000);
        // A total-loss window with a drop but nothing received still reports the bump, capped at 1e6.
        assert_eq!(window_loss_ppm(0, 0, 3), 50_000);
        assert!(window_loss_ppm(u64::MAX, 1, 9) <= 1_000_000);
    }

    #[test]
    fn bitrate_messages_roundtrip() {
        let req = SetBitrate {
            bitrate_kbps: 14_000,
        };
        assert_eq!(SetBitrate::decode(&req.encode()).unwrap(), req);
        let ack = BitrateChanged {
            bitrate_kbps: 14_000,
        };
        assert_eq!(BitrateChanged::decode(&ack.encode()).unwrap(), ack);
        // Same payload shape as LossReport — the type byte alone must keep them disjoint.
        assert!(LossReport::decode(&req.encode()).is_err());
        assert!(SetBitrate::decode(&ack.encode()).is_err());
        assert!(BitrateChanged::decode(&req.encode()).is_err());
        assert!(SetBitrate::decode(&LossReport { loss_ppm: 7 }.encode()).is_err());
    }

    #[test]
    fn probe_messages_roundtrip() {
        let req = ProbeRequest {
            target_kbps: 250_000,
            duration_ms: 2000,
        };
        assert_eq!(ProbeRequest::decode(&req.encode()).unwrap(), req);
        let res = ProbeResult {
            bytes_sent: 62_500_000,
            packets_sent: 480,
            duration_ms: 2003,
            wire_packets_sent: 41_000,
            send_dropped: 1_200,
        };
        assert_eq!(ProbeResult::decode(&res.encode()).unwrap(), res);
        assert_eq!(res.encode().len(), 29);
        // A pre-wire-stats host's 21-byte ProbeResult still decodes, with the new fields zeroed.
        let legacy = {
            let full = res.encode();
            full[..21].to_vec()
        };
        let decoded = ProbeResult::decode(&legacy).unwrap();
        assert_eq!(decoded.wire_packets_sent, 0);
        assert_eq!(decoded.send_dropped, 0);
        assert_eq!(decoded.bytes_sent, res.bytes_sent);
        // Type bytes keep the control messages disjoint from each other.
        assert!(ProbeRequest::decode(&res.encode()).is_err());
        assert!(Reconfigure::decode(&req.encode()).is_err());
        assert!(ProbeResult::decode(&req.encode()).is_err());
    }

    #[test]
    fn clock_messages_roundtrip() {
        let probe = ClockProbe {
            t1_ns: 1_700_000_000_123,
        };
        assert_eq!(ClockProbe::decode(&probe.encode()).unwrap(), probe);
        let echo = ClockEcho {
            t1_ns: 1_700_000_000_123,
            t2_ns: 1_700_000_050_456,
            t3_ns: 1_700_000_050_789,
        };
        assert_eq!(ClockEcho::decode(&echo.encode()).unwrap(), echo);
        // Disjoint from the other control messages (distinct type bytes).
        assert!(ClockProbe::decode(&echo.encode()).is_err());
        assert!(ProbeRequest::decode(&probe.encode()).is_err());
        assert!(ClockEcho::decode(&probe.encode()).is_err());
    }

    #[test]
    fn clock_offset_picks_min_rtt_and_recovers_offset() {
        // Host clock is +1_000_000 ns ahead of the client. Construct samples where a symmetric
        // round-trip recovers exactly that offset, and a noisy (asymmetric, high-RTT) sample is
        // present but must be ignored by the min-RTT selection.
        const OFF: i64 = 1_000_000;
        // Clean sample: client t1=0, one-way=200µs each way → t2 = t1 + 200_000 + OFF (host clock),
        // t3 = t2 + 50_000 (host processing), t4 = t3 - OFF + 200_000 (back in client clock).
        let t1 = 0u64;
        let t2 = (t1 as i64 + 200_000 + OFF) as u64;
        let t3 = t2 + 50_000;
        let t4 = (t3 as i64 - OFF + 200_000) as u64;
        // Noisy sample: same offset but a fat, asymmetric RTT (slow return path) — higher RTT.
        let n1 = 1_000_000u64;
        let n2 = (n1 as i64 + 200_000 + OFF) as u64;
        let n3 = n2 + 50_000;
        let n4 = (n3 as i64 - OFF + 5_000_000) as u64; // 5 ms return → big RTT
        let (offset, rtt) =
            clock_offset_ns(&[(n1, n2, n3, n4), (t1, t2, t3, t4)]).expect("non-empty");
        // The min-RTT sample recovers the offset exactly; its RTT is 2x200us, and the noisy
        // (asymmetric, 5 ms return) sample is ignored by the min-RTT selection.
        assert_eq!(offset, OFF);
        assert_eq!(rtt, 400_000);
        assert!(clock_offset_ns(&[]).is_none());
    }

    #[test]
    fn control_messages_disjoint_from_hello() {
        // A Hello uses MAGIC (PKF1); control messages use CTL_MAGIC (PKFc). No Hello — at
        // any abi_version — can be misparsed as a control message, and vice-versa.
        for abi in [1u32, 2, 16, 0x10, 0x0113, 0x1410] {
            let h = Hello {
                abi_version: abi,
                mode: Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
                compositor: CompositorPref::Auto,
                gamepad: GamepadPref::Auto,
                bitrate_kbps: 0,
                name: None,
                launch: None,
                video_caps: 0,
                audio_channels: 2,
                video_codecs: 0,
                preferred_codec: 0,
            }
            .encode();
            assert!(PairRequest::decode(&h).is_err(), "abi {abi} parsed as pair");
            assert!(Reconfigure::decode(&h).is_err());
        }
        // And a PairRequest never parses as a Hello.
        let pr = PairRequest {
            name: "x".into(),
            spake_a: vec![0u8; 33],
        }
        .encode();
        assert!(Hello::decode(&pr).is_err());
    }

    #[test]
    fn pair_messages_roundtrip() {
        let pr = PairRequest {
            name: "Enrico's Mac".into(),
            spake_a: vec![1, 2, 3, 4, 5],
        };
        assert_eq!(PairRequest::decode(&pr.encode()).unwrap(), pr);
        let pc = PairChallenge {
            spake_b: vec![9; 33],
            confirm: [7u8; 32],
        };
        assert_eq!(PairChallenge::decode(&pc.encode()).unwrap(), pc);
        let pp = PairProof { confirm: [3u8; 32] };
        assert_eq!(PairProof::decode(&pp.encode()).unwrap(), pp);
        for ok in [true, false] {
            assert_eq!(
                PairResult::decode(&PairResult { ok }.encode()).unwrap().ok,
                ok
            );
        }
        // Length-exact: a truncated/padded PairProof is rejected.
        let mut bad = pp.encode();
        bad.push(0);
        assert!(PairProof::decode(&bad).is_err());
    }

    #[test]
    fn spake2_pairing_agrees_only_on_matching_pin_and_certs() {
        let cfp = [0x11u8; 32];
        let hfp = [0x22u8; 32];

        // Right PIN, same fingerprint views on both sides → both confirmations agree.
        let (ca, ma) = pake::start(true, "4321", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(pake::verify(&a.host, &b.host) && pake::verify(&a.client, &b.client));

        // Wrong PIN → different keys → confirmations DON'T match (one online guess wasted).
        let (ca, ma) = pake::start(true, "0000", &cfp, &hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));

        // MITM: the two legs saw different host certs → no agreement even with the right PIN.
        let attacker_hfp = [0x33u8; 32];
        let (ca, ma) = pake::start(true, "4321", &cfp, &attacker_hfp);
        let (cb, mb) = pake::start(false, "4321", &cfp, &hfp);
        let a = ca.finish(&mb).unwrap();
        let b = cb.finish(&ma).unwrap();
        assert!(!pake::verify(&a.client, &b.client));
    }

    #[test]
    fn audio_datagram_roundtrip() {
        let opus = [0x42u8; 97];
        let d = encode_audio_datagram(7, 1_000_000_123, &opus);
        assert_eq!(d[0], AUDIO_MAGIC);
        let (seq, pts, payload) = decode_audio_datagram(&d).unwrap();
        assert_eq!((seq, pts), (7, 1_000_000_123));
        assert_eq!(payload, opus);
        assert!(decode_audio_datagram(&d[..12]).is_none()); // truncated header
        assert!(decode_audio_datagram(&[0u8; 13]).is_none()); // bad magic

        // Empty payload is legal (DTX) — header-only datagram.
        let header_only = encode_audio_datagram(0, 0, &[]);
        let (_, _, empty) = decode_audio_datagram(&header_only).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn rumble_datagram_roundtrip() {
        let d = encode_rumble_datagram(1, 0x1234, 0xFFFF);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(decode_rumble_datagram(&d), Some((1, 0x1234, 0xFFFF)));
        assert!(decode_rumble_datagram(&d[..6]).is_none());
    }

    #[test]
    fn mic_datagram_roundtrip_and_disjoint_from_audio() {
        let opus = [0x5Au8; 80];
        let d = encode_mic_datagram(42, 9_999, &opus);
        assert_eq!(d[0], MIC_MAGIC);
        let (seq, pts, payload) = decode_mic_datagram(&d).unwrap();
        assert_eq!((seq, pts), (42, 9_999));
        assert_eq!(payload, opus);
        assert!(decode_mic_datagram(&d[..12]).is_none()); // truncated
                                                          // Tag separation: a mic datagram is not an audio datagram and vice-versa.
        assert!(decode_audio_datagram(&d).is_none());
        assert!(decode_mic_datagram(&encode_audio_datagram(1, 2, &opus)).is_none());
        // Empty payload (DTX) is legal.
        assert!(decode_mic_datagram(&encode_mic_datagram(0, 0, &[]))
            .unwrap()
            .2
            .is_empty());
    }

    #[test]
    fn rich_input_roundtrip() {
        for ev in [
            RichInput::Touchpad {
                pad: 1,
                finger: 0,
                active: true,
                x: 40000,
                y: 12345,
            },
            RichInput::Motion {
                pad: 0,
                gyro: [-100, 200, -300],
                accel: [16384, -8192, 1],
            },
            RichInput::TouchpadEx {
                pad: 2,
                surface: 1,
                finger: 1,
                touch: true,
                click: false,
                x: -12345,
                y: 30000,
                pressure: 4000,
            },
        ] {
            let d = ev.encode();
            assert_eq!(d[0], RICH_INPUT_MAGIC);
            assert_eq!(RichInput::decode(&d), Some(ev));
        }
        // Disjoint from the fixed input datagram (0xC8); unknown kind + truncation → None.
        assert!(RichInput::decode(&[crate::input::INPUT_MAGIC; 18]).is_none());
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, 0x7F]).is_none()); // unknown kind
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD, 0]).is_none()); // short
        assert!(RichInput::decode(&[RICH_INPUT_MAGIC, RICH_TOUCHPAD_EX, 0, 0, 0, 0]).is_none());
        // short
    }

    #[test]
    fn hid_output_roundtrip() {
        let cases = [
            HidOutput::Led {
                pad: 2,
                r: 0xAA,
                g: 0xBB,
                b: 0xCC,
            },
            HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b10101,
            },
            HidOutput::Trigger {
                pad: 1,
                which: 1,
                effect: vec![0x26, 0x90, 0xA0, 0xFF, 0x00, 0x00],
            },
            HidOutput::TrackpadHaptic {
                pad: 0,
                side: 1,
                amplitude: 0x1234,
                period: 0x5678,
                count: 9,
            },
        ];
        for ev in &cases {
            let d = ev.encode();
            assert_eq!(d[0], HIDOUT_MAGIC);
            assert_eq!(HidOutput::decode(&d).as_ref(), Some(ev));
        }
        assert!(HidOutput::decode(&[HIDOUT_MAGIC, 0x7F]).is_none()); // unknown kind
                                                                     // A rich-input datagram is not a HID-output datagram.
        assert!(HidOutput::decode(
            &RichInput::Motion {
                pad: 0,
                gyro: [0; 3],
                accel: [0; 3]
            }
            .encode()
        )
        .is_none());
    }

    #[test]
    fn fingerprint_is_sha256_of_der() {
        // Stable across calls, distinct for distinct certs.
        let a = endpoint::cert_fingerprint(b"cert-a");
        assert_eq!(a, endpoint::cert_fingerprint(b"cert-a"));
        assert_ne!(a, endpoint::cert_fingerprint(b"cert-b"));
    }
}
