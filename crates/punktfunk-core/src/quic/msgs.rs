//! The `punktfunk/1` handshake (Hello/Welcome/Start) and every typed control message
//! (`CTL_MAGIC` + type byte), including the pairing-ceremony messages. Wire codecs only
//! — no transport state.

use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::error::{PunktfunkError, Result};
use super::{CTL_MAGIC, MAGIC};

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
    /// Host input capabilities — a bitfield of [`HOST_CAP_GAMEPAD_STATE`]. The client picks the
    /// wire form its gamepad events take from this (snapshots for a capable host, the legacy
    /// per-transition events otherwise). Appended after `codec` as a single trailing byte; an
    /// older host that omits it decodes to `0` (no capabilities — legacy events only).
    pub host_caps: u8,
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
        // Locate the trailing single-byte fields once. name (26) and launch are `len u8 || UTF-8`
        // blocks; their RAW length bytes (even when zero placeholders, or oversized garbage)
        // determine where the tail starts, so a corrupt name never panics — it just pushes the
        // later offsets out of range and those fields decode to their defaults.
        let name_len = b.get(26).copied().unwrap_or(0) as usize;
        let launch_off = 27 + name_len; // launch's length byte
        let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
        let tail = launch_off + 1 + launch_len; // first trailing byte: video_caps
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
            name: (name_len > 0 && name_len <= HELLO_NAME_MAX)
                .then(|| {
                    b.get(27..27 + name_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            // Optional trailing launch id, right after name's block (same len/UTF-8 discipline).
            launch: (launch_len > 0 && launch_len <= HELLO_LAUNCH_MAX)
                .then(|| {
                    b.get(launch_off + 1..launch_off + 1 + launch_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            // The trailing single bytes, in wire order from `tail` (see the encode-side layout).
            // Each is absent on an older client and decodes to its documented default.
            video_caps: b.get(tail).copied().unwrap_or(0),
            // Normalized so a corrupt/unsupported channel count can't build a bad decoder.
            audio_channels: crate::audio::normalize_channels(b.get(tail + 1).copied().unwrap_or(2)),
            // `0` = an older client (which `resolve_codec` treats as HEVC-only).
            video_codecs: b.get(tail + 2).copied().unwrap_or(0),
            // `0` = no preference; the host decides by precedence.
            preferred_codec: b.get(tail + 3).copied().unwrap_or(0),
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
        // Host input caps at offset 67 — older clients stop before this → 0 (legacy input only).
        b.push(self.host_caps);
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
            // Optional trailing host-caps byte — absent on an older host → 0 (no gamepad-state
            // snapshots; the client keeps sending legacy per-transition events).
            host_caps: b.get(67).copied().unwrap_or(0),
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
