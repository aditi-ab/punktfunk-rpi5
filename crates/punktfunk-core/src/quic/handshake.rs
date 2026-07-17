//! The `punktfunk/1` positional handshake — Hello / Welcome / Start — and their wire codecs.

use super::*;
use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::error::{PunktfunkError, Result};

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
    /// The client's **display** HDR colour volume — primaries / white point / luminance range in
    /// the ST.2086 units of [`HdrMeta`] — read from the client OS (e.g. Windows
    /// `IDXGIOutput6::GetDesc1`) when it advertised [`VIDEO_CAP_HDR`]. The host forwards it into
    /// the virtual display's EDID (the pf-vdisplay CTA-861.3 HDR static-metadata block), so host
    /// apps and the OS tone-map to the CLIENT's real panel instead of the driver's built-in
    /// ~1000-nit placeholder — the client can then present the PQ stream untouched. Also echoed
    /// back as the session's `0xCE` mastering metadata. Appended after `preferred_codec` as a
    /// fixed [`super::datagram::HDR_META_BODY_LEN`]-byte block (the [`HdrMeta`] wire body, no tag),
    /// forcing the earlier placeholders. Omitted by older clients / when the client has no HDR
    /// display (decodes to `None` — the host keeps its built-in EDID defaults).
    pub display_hdr: Option<HdrMeta>,
}

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
        let hdr_present = self.display_hdr.is_some();
        let need_placeholders =
            self.video_caps != 0 || ac_present || vcodecs_present || pref_present || hdr_present;
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
        if need_placeholders {
            b.push(self.video_caps);
        }
        // audio_channels: emitted when non-stereo OR a later field follows.
        if ac_present || vcodecs_present || pref_present || hdr_present {
            b.push(self.audio_channels);
        }
        // video_codecs: emitted when non-zero OR a later field follows.
        if vcodecs_present || pref_present || hdr_present {
            b.push(self.video_codecs);
        }
        // preferred_codec: emitted when non-zero OR display_hdr follows.
        if pref_present || hdr_present {
            b.push(self.preferred_codec);
        }
        // display_hdr: fixed HDR_META_BODY_LEN-byte HdrMeta body. Last field; omitted when `None`.
        if let Some(m) = &self.display_hdr {
            super::datagram::write_hdr_meta_body(m, &mut b);
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
            // Optional trailing HdrMeta body (fixed length) — absent on an older client / a
            // client without an HDR display → `None` (the host keeps its EDID defaults).
            display_hdr: b
                .get(tail + 4..tail + 4 + super::datagram::HDR_META_BODY_LEN)
                .map(super::datagram::read_hdr_meta_body),
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
                Some(CODEC_PYROWAVE) => CODEC_PYROWAVE,
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
        // Client-side reassembler ceiling: p1_defaults' 64 MiB hostile-header memory bound is
        // ~10x larger than any real access unit. Derive it from the negotiated rate instead:
        // 4x the average frame size at the resolved bitrate (IDR headroom), floored at 8 MiB,
        // capped at the old 64 MiB. Purely local — the host never reassembles video and the
        // wire is self-describing, so old hosts are unaffected; a host that reports bitrate 0
        // (pre-negotiation) keeps the old bound.
        if role == Role::Client && self.bitrate_kbps > 0 {
            let per_frame = (self.bitrate_kbps as usize).saturating_mul(125)
                / self.mode.refresh_hz.max(1) as usize;
            c.max_frame_bytes = per_frame.saturating_mul(4).clamp(8 << 20, 64 << 20);
        }
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
