//! The `punktfunk/1` positional handshake — Hello / Welcome / Start — and their wire codecs.

use super::*;
use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::crypto::SessionKey;
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
    /// Non-video client capabilities — a bitfield of [`CLIENT_CAP_CURSOR`] (the client renders
    /// the host cursor locally; the host stops compositing it and forwards shape + state
    /// instead). Appended as a single byte AFTER `display_hdr`; because that block is a fixed
    /// [`super::datagram::HDR_META_BODY_LEN`]-byte optional with no placeholder form, presence is
    /// disambiguated by REMAINING LENGTH at decode: fewer than `HDR_META_BODY_LEN` bytes after
    /// `preferred_codec` ⇒ no HDR block, the tail bytes are the post-HDR fields directly. This
    /// caps everything after `display_hdr` at `HDR_META_BODY_LEN − 1` bytes total — document any
    /// future field here and mind the budget (`client_caps` 1 + `max_shard_payload` 2 = 3 of the
    /// 27 spent). Omitted when zero and by older clients (→ `0`).
    pub client_caps: u8,
    /// The largest video shard payload this client's receive path accepts — sealed datagrams for
    /// shards up to this size fit its transport buffers ([`crate::config::max_shard_payload`]).
    /// One field carries BOTH facts the host needs for mid-session shard renegotiation
    /// (design/shard-payload-reneg.md W0.3): non-zero ⇒ the client reassembles per-frame
    /// geometry (a mid-session `shard_payload` change is safe to send), and the value is the
    /// hard ceiling a jumbo grow may never exceed. Appended after `client_caps` as 2 trailing
    /// LE bytes (forcing the earlier placeholders). Omitted by older clients (decodes to `0`
    /// = legacy: the host must not change the sealed geometry mid-session, and never above
    /// the `Welcome` value).
    pub max_shard_payload: u16,
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

/// [`Welcome::cipher`] id: AES-128-GCM — the default session AEAD every peer speaks (and the
/// only one pre-cipher builds know).
pub const CIPHER_AES_128_GCM: u8 = 0;
/// [`Welcome::cipher`] id: ChaCha20-Poly1305 (RFC 8439) — negotiated via
/// [`VIDEO_CAP_CHACHA20`] for clients without hardware AES.
pub const CIPHER_CHACHA20_POLY1305: u8 = 1;

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
    /// The session AEAD the data plane seals with — [`CIPHER_AES_128_GCM`] (`0`, the default
    /// every peer speaks) or [`CIPHER_CHACHA20_POLY1305`] (`1`). The host sets `1` ONLY toward
    /// a client that advertised [`VIDEO_CAP_CHACHA20`] (the soft-AES armv7 targets). Appended
    /// after `host_caps` at offset 68 and — unlike the earlier trailing fields — emitted only
    /// when non-zero, so an AES session's Welcome stays **byte-identical** to the pre-cipher
    /// wire form; an older host omits it (→ `0`, AES). Decode is fail-closed: an unknown id is
    /// an `Err`, never a silent AES fallback — the host only picks a cipher this client
    /// advertised, so an unknown id reaching us is a bug, and falling back would yield an
    /// undecryptable session with a confusing failure signature.
    pub cipher: u8,
    /// The host's management-API port — where its game library is served, distinct from every
    /// other port here (`udp_port` is the data plane; the control plane is the QUIC port the
    /// client already dialed). `0` = not advertised (an older host), and the client falls back to
    /// the compiled-in 47990.
    ///
    /// **Why this is on the wire at all:** the port was previously discoverable ONLY from the
    /// mDNS `mgmt` TXT. A host that moved it off 47990 — the supported way to share a machine with
    /// a Sunshine fork, whose web UI owns that port — therefore had a working library only where
    /// multicast worked. Carrying it in the `Welcome` means the client learns it over the
    /// connection it has already authenticated, so a VPN-only, routed-subnet or manually-added
    /// host needs no discovery at all.
    ///
    /// Appended AFTER the cipher block (offset 69, or 101 when a ChaCha key precedes it) rather
    /// than at the next free fixed offset, and emitting it forces the `cipher` placeholder — see
    /// the note in [`Welcome::encode`]. `0` when an older host omitted it.
    pub mgmt_port: u16,
    /// The session's effective access grants — the [`GRANT_GAMEPAD`](super::GRANT_GAMEPAD)-family
    /// bitmask (per-client access, `design/per-client-access.md` §7). Courtesy, not authority:
    /// the HOST enforces the mask regardless; the client uses this to not capture what can't
    /// land (no keyboard grab without the bit — "my keyboard does nothing and nobody says why"
    /// is the failure mode this prevents) and to label the session ("Controller only").
    /// Appended after `mgmt_port` as 4 trailing bytes; absent (an older host) →
    /// [`GRANT_ALL`](super::GRANT_ALL), the pre-grants behavior.
    pub grants: u32,
    /// Seconds until this device's access expires, measured when the Welcome is built; `0` =
    /// permanent (also what an older host's omission decodes to). Mid-session changes ride
    /// [`AccessUpdate`](super::AccessUpdate); the expiry itself closes with
    /// [`ACCESS_EXPIRED_CLOSE_CODE`](crate::reject). Appended after `grants` as 4 trailing
    /// bytes.
    pub expires_in_secs: u32,
    /// The 256-bit ChaCha20-Poly1305 session key (RFC 8439 requires the full 32 bytes; wire
    /// cost is once per handshake) — present iff `cipher == 1`, at offsets 69..101. The legacy
    /// 16-byte `key` keeps its offset and stays independently random, so nothing downstream
    /// ever observes an all-zero key. Decode rejects `cipher == 1` with fewer than 32 key
    /// bytes following.
    pub key_chacha: Option<[u8; 32]>,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary (so a multi-byte char straddling
/// the cap is dropped whole, never split). Shared by Hello's length-prefixed name/launch fields
/// and [`PairRequest`](super::PairRequest)'s copy of the same name cap.
pub(super) fn truncate_to(s: &str, max: usize) -> &str {
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
        let ccaps_present = self.client_caps != 0;
        let msp_present = self.max_shard_payload != 0;
        let need_placeholders = self.video_caps != 0
            || ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present;
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
        if ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
        {
            b.push(self.audio_channels);
        }
        // video_codecs: emitted when non-zero OR a later field follows.
        if vcodecs_present || pref_present || hdr_present || ccaps_present || msp_present {
            b.push(self.video_codecs);
        }
        // preferred_codec: emitted when non-zero OR a later field follows.
        if pref_present || hdr_present || ccaps_present || msp_present {
            b.push(self.preferred_codec);
        }
        // display_hdr: fixed HDR_META_BODY_LEN-byte HdrMeta body; omitted when `None` even if
        // later fields follow (no placeholder form — the decoder disambiguates by remaining
        // length, which caps the post-HDR tail at HDR_META_BODY_LEN − 1 bytes).
        if let Some(m) = &self.display_hdr {
            super::datagram::write_hdr_meta_body(m, &mut b);
        }
        // client_caps: single byte after the (optional) HDR block. Emitted when non-zero OR a
        // later field follows.
        if ccaps_present || msp_present {
            b.push(self.client_caps);
        }
        // max_shard_payload: 2 trailing LE bytes after client_caps. Emitted when non-zero.
        if msp_present {
            b.extend_from_slice(&self.max_shard_payload.to_le_bytes());
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
            // Presence is decided by REMAINING LENGTH (there is no placeholder form for the
            // fixed block): ≥ HDR_META_BODY_LEN bytes after `preferred_codec` ⇒ the block is
            // there and post-HDR fields follow it; fewer ⇒ no block, the bytes ARE the post-HDR
            // fields. Sound as long as the post-HDR tail stays under HDR_META_BODY_LEN bytes.
            display_hdr: (b.len().saturating_sub(tail + 4) >= super::datagram::HDR_META_BODY_LEN)
                .then(|| {
                    b.get(tail + 4..tail + 4 + super::datagram::HDR_META_BODY_LEN)
                        .map(super::datagram::read_hdr_meta_body)
                })
                .flatten(),
            // client_caps: the byte after the HDR block when present, else directly at tail+4.
            client_caps: {
                let off = if b.len().saturating_sub(tail + 4) >= super::datagram::HDR_META_BODY_LEN
                {
                    tail + 4 + super::datagram::HDR_META_BODY_LEN
                } else {
                    tail + 4
                };
                b.get(off).copied().unwrap_or(0)
            },
            // max_shard_payload: 2 LE bytes after client_caps (same post-HDR offset rule).
            // Absent on an older client → 0 = no mid-session renegotiation, no jumbo.
            max_shard_payload: {
                let off = if b.len().saturating_sub(tail + 4) >= super::datagram::HDR_META_BODY_LEN
                {
                    tail + 4 + super::datagram::HDR_META_BODY_LEN
                } else {
                    tail + 4
                };
                b.get(off + 1..off + 3)
                    .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
                    .unwrap_or(0)
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
        // Host input caps at offset 67 — older clients stop before this → 0 (legacy input only).
        b.push(self.host_caps);
        // Session cipher at offset 68 + the 32-byte ChaCha key at 69..101 — emitted ONLY when a
        // non-default cipher was negotiated, so an AES session's Welcome stays byte-identical
        // to the pre-cipher wire form. The host only sets cipher toward a client that
        // advertised VIDEO_CAP_CHACHA20, so an old client never sees these bytes at all.
        debug_assert_eq!(
            self.cipher == CIPHER_CHACHA20_POLY1305,
            self.key_chacha.is_some(),
            "key_chacha present iff cipher == 1"
        );
        //
        // ⚠ `mgmt_port` follows the cipher block, so emitting it FORCES the cipher byte even for
        // an AES session — the placeholder discipline `Hello::encode` already uses for
        // `audio_channels`/`preferred_codec`. Without that, an AES Welcome carrying a mgmt port
        // would put the port's low byte at offset 68, exactly where every 0.28.x client reads
        // `cipher` — and that decode is deliberately fail-closed on an unknown id, so the whole
        // handshake would break against currently-shipped clients. An explicit `cipher = 0` is
        // harmless by comparison: a current client reads AES (correct), and a pre-cipher client
        // stops before 68 regardless.
        // The access advert (grants + expiry) follows `mgmt_port`, extending the same chain:
        // emitting it forces BOTH placeholders before it — the cipher byte (as 0 = AES) and the
        // mgmt port (as 0 = not advertised, exactly what its absence decodes to) — so the two
        // u32s always land at a deterministic offset. A full-control permanent session
        // (`GRANT_ALL`, no deadline) is what every absent-field decode yields anyway, so it is
        // omitted and the common case stays byte-identical to the pre-grants wire form.
        let mgmt_present = self.mgmt_port != 0;
        let access_present = self.grants != super::access::GRANT_ALL || self.expires_in_secs != 0;
        if self.cipher != CIPHER_AES_128_GCM || mgmt_present || access_present {
            b.push(self.cipher);
            if let Some(k) = &self.key_chacha {
                b.extend_from_slice(k);
            }
            if mgmt_present || access_present {
                b.extend_from_slice(&self.mgmt_port.to_le_bytes());
            }
            if access_present {
                b.extend_from_slice(&self.grants.to_le_bytes());
                b.extend_from_slice(&self.expires_in_secs.to_le_bytes());
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Layout (LE): magic[0..4] abi[4..8] port[8..10] w[10..14] h[14..18] hz[18..22]
        // scheme[22] pct[23] max_data[24..26] shard[26..28] encrypt[28] key[29..45]
        // salt[45..49] frames[49..53] compositor[53] gamepad[54] bitrate_kbps[55..59]
        // bit_depth[59] color.primaries[60] color.transfer[61] color.matrix[62] color.range[63]
        // chroma_format[64] audio_channels[65] codec[66] host_caps[67] cipher[68]
        // key_chacha[69..101] mgmt_port[69..71 | 101..103] grants[71..75 | 103..107]
        // expires_in_secs[75..79 | 107..111] (everything from compositor on is an
        // optional trailing byte; an older host stops earlier; cipher/key_chacha are present only
        // when ChaCha was negotiated). `mgmt_port` and the access pair are the fields whose
        // offsets are NOT fixed: they follow the cipher block, shifted by 32 when a ChaCha key
        // precedes them. Emitting a later field forces every earlier one (see `encode`), so
        // "cipher absent" and "mgmt_port present" — or "mgmt_port absent" and "grants present" —
        // can never both hold.
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        // Session cipher at 68 — absent on an older host → AES-128-GCM. Fail-closed on
        // anything else: `cipher == 1` with fewer than 32 key bytes must be an error (a silent
        // AES fallback would yield an undecryptable session with a confusing failure
        // signature), and an unknown id (≥ 2) reaching us is a bug — a host only picks a
        // cipher this client advertised — never a legitimate negotiation.
        let cipher = b.get(68).copied().unwrap_or(CIPHER_AES_128_GCM);
        let key_chacha = match cipher {
            CIPHER_AES_128_GCM => None,
            CIPHER_CHACHA20_POLY1305 => {
                let bytes = b
                    .get(69..101)
                    .ok_or(PunktfunkError::InvalidArg("bad Welcome"))?;
                let mut k = [0u8; 32];
                k.copy_from_slice(bytes);
                Some(k)
            }
            _ => return Err(PunktfunkError::InvalidArg("bad Welcome")),
        };
        // The mgmt port sits after the cipher block, so its offset depends on whether a ChaCha key
        // preceded it. Absent (an older host, or one that did not advertise) → `0` = unknown, and
        // the client falls back to the compiled-in default.
        let mgmt_off = if cipher == CIPHER_CHACHA20_POLY1305 {
            101
        } else {
            69
        };
        let mgmt_port = b
            .get(mgmt_off..mgmt_off + 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // The access advert trails the mgmt port. Absent (an older host, or a full-control
        // permanent session — encode omits the default) → GRANT_ALL / no deadline, which is
        // exactly the pre-grants behavior; a truncated tail is never half an advert.
        let grants_off = mgmt_off + 2;
        let grants = b
            .get(grants_off..grants_off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(super::access::GRANT_ALL);
        let expires_in_secs = b
            .get(grants_off + 4..grants_off + 8)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
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
            mgmt_port,
            grants,
            expires_in_secs,
            cipher,
            key_chacha,
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // wire phase id pending the P2 packet rev
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        // The negotiated AEAD: the ChaCha key when cipher == 1 (guaranteed present by decode —
        // the `(1, None)` shape is unreachable off the wire), the legacy AES key otherwise.
        c.key = match (self.cipher, self.key_chacha) {
            (CIPHER_CHACHA20_POLY1305, Some(k)) => SessionKey::ChaCha20Poly1305(k),
            _ => SessionKey::Aes128Gcm(self.key),
        };
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

#[cfg(test)]
mod tests {
    use crate::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Mode, Role};
    use crate::quic::*;

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
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);

        // Client-side reassembler ceiling derives from the negotiated rate: 4x the average frame at
        // 50 Mbps/240 Hz is ~104 KB, so the 8 MiB floor governs. The host keeps the p1_defaults
        // bound (it never reassembles video), as does a client of a bitrate-0 (older) host.
        let cc = w.session_config(Role::Client);
        assert_eq!(cc.max_frame_bytes, 8 << 20);
        cc.validate().expect("derived client config validates");
        assert_eq!(w.session_config(Role::Host).max_frame_bytes, 64 << 20);
        let old_host = Welcome {
            bitrate_kbps: 0,
            ..w
        };
        assert_eq!(
            old_host.session_config(Role::Client).max_frame_bytes,
            64 << 20
        );
        // A high-rate mode scales past the floor: 1.5 Gbps at 60 Hz = 4 x 3.125 MB = 12.5 MB.
        let fat = Welcome {
            bitrate_kbps: 1_500_000,
            mode: Mode {
                width: 5120,
                height: 1440,
                refresh_hz: 60,
            },
            ..w
        };
        let derived = fat.session_config(Role::Client).max_frame_bytes;
        assert_eq!(derived, 4 * 1_500_000 * 125 / 60);
        assert!(derived > (8 << 20) && derived < (64 << 20));
    }

    #[test]
    fn welcome_cipher_negotiation_wire_and_back_compat() {
        use crate::crypto::SessionKey;
        let base = Welcome {
            abi_version: 2,
            udp_port: 7000,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 20,
                max_data_per_block: 4096,
            },
            shard_payload: 1200,
            encrypt: true,
            key: [7u8; 16],
            salt: [9, 8, 7, 6],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 50_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: 0,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
        };
        // An AES session's Welcome is byte-identical to the pre-cipher wire form (68 bytes) —
        // the old-client × new-host interop guarantee.
        let enc = base.encode();
        assert_eq!(enc.len(), 68);
        assert_eq!(Welcome::decode(&enc).unwrap(), base);

        // ChaCha roundtrip: cipher byte at 68, the 32-byte key at 69..101.
        let k32: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let cenc = cha.encode();
        assert_eq!(cenc.len(), 68 + 1 + 32);
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);

        // A truncated old-host Welcome (no cipher byte) decodes to the AES default.
        let old_host = Welcome::decode(&cenc[..68]).unwrap();
        assert_eq!(old_host.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_host.key_chacha, None);

        // cipher == 1 with a missing / short key → Err, fail-closed (a silent AES fallback
        // would yield an undecryptable session with a confusing failure signature).
        assert!(Welcome::decode(&cenc[..69]).is_err());
        assert!(Welcome::decode(&cenc[..100]).is_err());

        // An unknown cipher id (≥ 2) → Err: the host only picks a cipher we advertised, so an
        // unknown id reaching us is a bug, never a legitimate negotiation.
        let mut bad = cenc.clone();
        bad[68] = 2;
        assert!(Welcome::decode(&bad).is_err());

        // session_config maps both variants onto the data-plane key, and both validate.
        let aes_cfg = base.session_config(Role::Client);
        assert_eq!(aes_cfg.key, SessionKey::Aes128Gcm([7u8; 16]));
        aes_cfg.validate().expect("AES config validates");
        let cha_cfg = cha.session_config(Role::Client);
        assert_eq!(cha_cfg.key, SessionKey::ChaCha20Poly1305(k32));
        cha_cfg.validate().expect("ChaCha config validates");

        // ── mgmt_port, the trailing field after the cipher block ──────────────────────────────
        //
        // ⚠ THE HAZARD THIS PINS: `mgmt_port` follows `cipher`, and `cipher` is emitted only when
        // non-default. Appending the port to an AES Welcome without forcing the cipher byte would
        // land the port's LOW BYTE at offset 68 — exactly where every shipped client reads
        // `cipher`, whose decode is fail-closed on an unknown id. 47991 is 0xBB57, so byte 68
        // would read 0x57 = 87, an unknown id, and EVERY 0.28.x client would fail the handshake
        // against a host that had merely moved its mgmt port. Assert the placeholder is there.
        let mgmt = Welcome {
            mgmt_port: 47991,
            ..base
        };
        let menc = mgmt.encode();
        assert_eq!(menc.len(), 68 + 1 + 2, "cipher placeholder + LE u16 port");
        assert_eq!(
            menc[68], CIPHER_AES_128_GCM,
            "the cipher byte MUST be present (as 0) so a current client still reads AES here"
        );
        assert_eq!(&menc[69..71], &47991u16.to_le_bytes());
        assert_eq!(Welcome::decode(&menc).unwrap(), mgmt);

        // With ChaCha the port sits after the 32-byte key instead, at 101..103.
        let both = Welcome {
            mgmt_port: 47991,
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let benc = both.encode();
        assert_eq!(benc.len(), 68 + 1 + 32 + 2);
        assert_eq!(&benc[101..103], &47991u16.to_le_bytes());
        assert_eq!(Welcome::decode(&benc).unwrap(), both);

        // A host that advertises no mgmt port emits nothing extra — an AES Welcome stays exactly
        // 68 bytes, so this field costs the common case zero and cannot perturb an old client.
        assert_eq!(base.encode().len(), 68);
        // ...and an old host's Welcome decodes to 0 = unknown, never to a port we might dial.
        assert_eq!(Welcome::decode(&enc).unwrap().mgmt_port, 0);
        assert_eq!(Welcome::decode(&cenc).unwrap().mgmt_port, 0);
        // A truncated tail (one byte of the port) is not half a port: it reads as unknown.
        assert_eq!(Welcome::decode(&menc[..70]).unwrap().mgmt_port, 0);

        // ── grants + expiry, the access advert after the mgmt port ────────────────────────────
        //
        // Same chain discipline one link further: emitting the access pair forces BOTH the
        // cipher byte (as 0 = AES) and the mgmt port (as 0 = not advertised) so the two u32s
        // land at a deterministic offset — 71..79 for AES, 103..111 behind a ChaCha key.
        let guest = Welcome {
            grants: GRANT_PRESET_CONTROLLER_ONLY,
            expires_in_secs: 4 * 3600,
            ..base
        };
        let genc = guest.encode();
        assert_eq!(
            genc.len(),
            68 + 1 + 2 + 8,
            "cipher + mgmt placeholders + 2 u32s"
        );
        assert_eq!(genc[68], CIPHER_AES_128_GCM, "forced cipher placeholder");
        assert_eq!(
            &genc[69..71],
            &0u16.to_le_bytes(),
            "forced mgmt placeholder"
        );
        assert_eq!(&genc[71..75], &GRANT_PRESET_CONTROLLER_ONLY.to_le_bytes());
        assert_eq!(&genc[75..79], &(4u32 * 3600).to_le_bytes());
        assert_eq!(Welcome::decode(&genc).unwrap(), guest);
        // The forced-zero mgmt placeholder decodes exactly like its absence: unknown.
        assert_eq!(Welcome::decode(&genc).unwrap().mgmt_port, 0);

        // All three trailing features together, behind a ChaCha key: 103..111.
        let full_chain = Welcome {
            mgmt_port: 47991,
            grants: GRANT_PRESET_VIEW_ONLY,
            expires_in_secs: 60,
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let fenc = full_chain.encode();
        assert_eq!(fenc.len(), 68 + 1 + 32 + 2 + 8);
        assert_eq!(&fenc[103..107], &GRANT_PRESET_VIEW_ONLY.to_le_bytes());
        assert_eq!(&fenc[107..111], &60u32.to_le_bytes());
        assert_eq!(Welcome::decode(&fenc).unwrap(), full_chain);

        // Old-welcome-decodes-with-defaults: every shorter wire form — the pre-cipher 68 bytes,
        // a cipher-only form, and a mgmt-port form — reads as full control, permanent. Grants
        // arriving as GRANT_ALL from an old host is the CORRECT meaning: that host enforces
        // nothing, exactly like a full-control session.
        for old in [&enc[..], &cenc[..], &menc[..]] {
            let w = Welcome::decode(old).unwrap();
            assert_eq!(w.grants, GRANT_ALL);
            assert_eq!(w.expires_in_secs, 0);
        }
        // A truncated advert (partial u32) is never half a mask; grants-without-expiry reads
        // the mask and leaves the deadline permanent.
        assert_eq!(Welcome::decode(&genc[..73]).unwrap().grants, GRANT_ALL);
        let g_only = Welcome::decode(&genc[..75]).unwrap();
        assert_eq!(g_only.grants, GRANT_PRESET_CONTROLLER_ONLY);
        assert_eq!(g_only.expires_in_secs, 0);

        // New-welcome-decoded-by-old-reader semantics: a 0.29-era reader stops at the bytes it
        // knows. The mgmt-port-era reader consumes [..71] of the guest Welcome and sees a valid
        // session (cipher 0 = AES, port 0 = unknown) — the appended advert never perturbs it.
        let old_view = Welcome::decode(&genc[..71]).unwrap();
        assert_eq!(old_view.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_view.mgmt_port, 0);
        assert_eq!(old_view, base);

        // A full-control permanent session emits NO advert — the common case stays
        // byte-identical to the pre-grants wire form (and to the pre-cipher one).
        assert_eq!(base.encode().len(), 68);
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

        // PyroWave is opt-in ONLY (plan §3): mutual support NEVER auto-selects it — the ladder
        // ignores it entirely...
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC | CODEC_PYROWAVE, 0),
            Some(CODEC_HEVC)
        );
        // ...even when it is the ONLY shared codec (an all-intra 200 Mbps stream must never be a
        // silent fallback)...
        assert_eq!(resolve_codec(CODEC_PYROWAVE, CODEC_PYROWAVE, 0), None);
        // ...it is reachable exclusively through the client's explicit preference.
        assert_eq!(
            resolve_codec(
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_PYROWAVE
            ),
            Some(CODEC_PYROWAVE)
        );
        // A pyrowave preference against a host without the backend falls back to the ladder.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC, CODEC_PYROWAVE),
            Some(CODEC_HEVC)
        );
        // And the negotiated bit SURVIVES the Welcome wire roundtrip — the decode whitelist
        // once folded unknown codec bytes (incl. PyroWave) to HEVC, which sent wavelet AUs
        // into an FFmpeg HEVC decoder on the first on-glass run.
        let mut pw_w = Welcome::decode(
            &Welcome {
                abi_version: 2,
                udp_port: 1,
                mode: Mode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                },
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
                codec: CODEC_PYROWAVE,
                host_caps: 0,
                mgmt_port: 0,
                grants: GRANT_ALL,
                expires_in_secs: 0,
                cipher: 0,
                key_chacha: None,
            }
            .encode(),
        )
        .unwrap();
        assert_eq!(pw_w.codec, CODEC_PYROWAVE);
        // A genuinely unknown future bit still folds to the HEVC default.
        pw_w.codec = 0x40;
        assert_eq!(Welcome::decode(&pw_w.encode()).unwrap().codec, CODEC_HEVC);

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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
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
                host_caps: 0,
                mgmt_port: 0,
                grants: GRANT_ALL,
                expires_in_secs: 0,
                cipher: 0,
                key_chacha: None,
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
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
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
        };
        let wenc = w.encode();
        assert_eq!(wenc.len(), 68); // 60 base + 4 colour + chroma + audio-channels + codec + host-caps
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
                                                                       // A pre-host-caps (67-byte) Welcome → 0 (legacy input only); the full form carries the bit.
        assert_eq!(Welcome::decode(&wenc[..67]).unwrap().host_caps, 0);
        assert_eq!(
            Welcome::decode(&wenc).unwrap().host_caps,
            HOST_CAP_GAMEPAD_STATE
        );
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
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
    fn hello_display_hdr_roundtrip_and_back_compat() {
        let base = Hello {
            abi_version: 2,
            mode: Mode {
                width: 3840,
                height: 2160,
                refresh_hz: 120,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: None,
            launch: None,
            video_caps: VIDEO_CAP_10BIT | VIDEO_CAP_HDR,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
        };
        // A real client-panel volume (P3 primaries, 800-nit peak, 0.05-nit floor, 400-nit FALL).
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], // G, B, R
            white_point: [15635, 16450],                                       // D65
            max_display_mastering_luminance: 8_000_000,                        // 800 nits
            min_display_mastering_luminance: 500,                              // 0.05 nits
            max_cll: 0,
            max_fall: 400,
        };
        let with_hdr = Hello {
            display_hdr: Some(vol),
            ..base.clone()
        };
        // Full roundtrip, including the forced placeholders for the earlier trailing fields.
        assert_eq!(Hello::decode(&with_hdr.encode()).unwrap(), with_hdr);
        // display_hdr alone (every earlier optional at its default) still lands at a deterministic
        // offset — the placeholder discipline holds through the whole tail.
        let hdr_only = Hello {
            video_caps: 0,
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        // An older host reading a display_hdr-bearing Hello ignores the trailing block (its decode
        // stops at preferred_codec); a new host reading an older client's Hello gets None.
        let enc = with_hdr.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - HDR_META_BODY_LEN]).unwrap(),
            Hello {
                display_hdr: None,
                ..with_hdr.clone()
            }
        );
        assert_eq!(Hello::decode(&base.encode()).unwrap().display_hdr, None);
        // A TRUNCATED trailing block (mid-datagram cut) degrades to None, never a partial read.
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 1]).unwrap().display_hdr,
            None
        );
        // Exact wire length: 26 bitrate-era bytes + the 6 forced single-byte placeholders
        // (name len, launch len, video_caps, audio_channels, video_codecs, preferred_codec) + the body.
        assert_eq!(hdr_only.encode().len(), 26 + 6 + HDR_META_BODY_LEN);
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
                display_hdr: None,
                client_caps: 0,
                max_shard_payload: 0,
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
    fn hello_client_caps_roundtrip_and_back_compat() {
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
        };
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        // caps WITHOUT an HDR block: the single byte after preferred_codec (remaining < the
        // fixed block length, so the decoder must NOT read it as a truncated HdrMeta).
        let caps_only = Hello {
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&caps_only.encode()).unwrap(), caps_only);
        // caps AND the HDR block: caps lands after the fixed block.
        let both = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // HDR without caps stays byte-identical to the pre-caps wire form and decodes caps 0.
        let hdr_only = Hello {
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        // An older client (no trailing byte at all) decodes to 0.
        assert_eq!(Hello::decode(&base.encode()).unwrap().client_caps, 0);
        // An older HOST reading a caps-bearing Hello: its decode simply never looks past the
        // fields it knows — nothing before the caps byte moved.
        let enc = both.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 1]).unwrap(),
            Hello {
                client_caps: 0,
                max_shard_payload: 0,
                ..both.clone()
            }
        );
    }

    /// `max_shard_payload` (mid-session shard renegotiation, design/shard-payload-reneg.md
    /// W0.3): roundtrips, forces the earlier placeholders (deterministic offset), composes
    /// with the optional HDR block, and degrades to 0 = legacy in BOTH directions.
    #[test]
    fn hello_max_shard_payload_roundtrip_and_back_compat() {
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
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
        };
        // The advertisement alone: every earlier trailing field is emitted as a placeholder
        // so the 2 LE bytes land at a deterministic offset — and the whole thing roundtrips.
        let adv = Hello {
            max_shard_payload: crate::config::max_shard_payload() as u16,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&adv.encode()).unwrap(), adv);
        // Composes with client_caps AND the fixed HDR block (the remaining-length
        // disambiguation must still find both fields after it).
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        let full = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 8908,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&full.encode()).unwrap(), full);
        // An older client (no trailing bytes at all) decodes to 0 = legacy: the host must
        // not change the sealed geometry mid-session.
        assert_eq!(Hello::decode(&base.encode()).unwrap().max_shard_payload, 0);
        // An older HOST reading an advertising Hello never looks past the fields it knows —
        // truncating the 2 trailing bytes yields the same Hello minus the advertisement.
        let enc = full.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 2]).unwrap(),
            Hello {
                max_shard_payload: 0,
                ..full.clone()
            }
        );
    }
}
