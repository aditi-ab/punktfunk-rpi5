//! The `punktfunk/1` positional handshake — Hello / Welcome / Start — and their wire codecs.
//!
//! Trailing fields append. Older peers stop early; absence decodes to the documented default,
//! so a legacy Hello/Welcome stays byte-identical until a later field is non-default.
//! Emitting a later field forces every earlier placeholder, each encoding exactly what its
//! absence meant. The exception is [`Hello::display_hdr`]: a fixed-length block with no
//! placeholder, disambiguated by remaining length, which caps the post-HDR tail at
//! `HDR_META_BODY_LEN − 1` bytes.
//!
//! `Welcome`'s tail after offset 68 is conditional: ChaCha inserts 32 key bytes, so
//! `mgmt_port` and everything after it sit at 69 or 101. A field that lands at 68 is read
//! as `cipher` by shipped clients (fail-closed). Evidence: `design/hi-res-audio.md`,
//! `design/shard-payload-reneg.md`, tests in this module.

use super::*;
use crate::config::{
    CompositorPref, Config, FecConfig, FecScheme, GamepadPref, Mode, ProtocolPhase, Role,
};
use crate::crypto::SessionKey;
use crate::error::{PunktfunkError, Result};

/// `client → host`: open the session. The host creates its virtual output at exactly `mode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub abi_version: u32,
    pub mode: Mode,
    /// Preferred compositor (`Auto` = host decides). Honored only if that backend is available;
    /// the resolved choice is [`Welcome::compositor`]. Omitted by older clients → `Auto`.
    pub compositor: CompositorPref,
    /// Preferred virtual gamepad (`Auto` = host `PUNKTFUNK_GAMEPAD`, else X-Box 360). Echoed in
    /// [`Welcome::gamepad`]. Omitted by older clients → `Auto`.
    pub gamepad: GamepadPref,
    /// Requested encoder bitrate, kbps. `0` = host default. Clamped and echoed in
    /// [`Welcome::bitrate_kbps`]. Omitted by older clients → `0`.
    pub bitrate_kbps: u32,
    /// Device label for pairing approval, `len u8 || UTF-8` (≤ [`HELLO_NAME_MAX`]).
    /// Omitted by older clients → `None` (host uses a fingerprint-derived label).
    pub name: Option<String>,
    /// Store-qualified library id (`steam:570`) the host resolves against its own library.
    /// `None` = default session. After `name` as `len u8 || UTF-8` (≤ [`HELLO_LAUNCH_MAX`]);
    /// a zero-length name placeholder precedes it when `name` is absent. Omitted → `None`.
    pub launch: Option<String>,
    /// [`VIDEO_CAP_10BIT`] / [`VIDEO_CAP_HDR`]. Host enables 10-bit/HDR only when the bit is
    /// set, so `0` (older clients) stays 8-bit BT.709. After `launch`; forces name/launch
    /// placeholders. Omitted → `0`.
    pub video_caps: u8,
    /// Requested channels: `2` / `6` / `8`. Host echoes the capture count in
    /// [`Welcome::audio_channels`]. Non-stereo forces name/launch/video_caps placeholders.
    /// Omitted or `2` → stereo, so the stereo Hello stays byte-identical.
    pub audio_channels: u8,
    /// Decode bitfield: [`CODEC_H264`] / [`CODEC_HEVC`] / [`CODEC_AV1`]. Host reports the pick
    /// in [`Welcome::codec`]. A GPU-less host needs [`CODEC_H264`]. Omitted → `0`, which
    /// [`resolve_codec`] treats as HEVC-only.
    pub video_codecs: u8,
    /// Soft hint: one codec bit, or `0` = host precedence. Honored only if shared; else
    /// [`resolve_codec`] falls back. Omitted by older clients → `0`.
    pub preferred_codec: u8,
    /// Client-panel ST.2086 volume ([`HdrMeta`]) when [`VIDEO_CAP_HDR`] is set. Copied into
    /// the virtual-display EDID so the host tone-maps to this panel, and echoed as `0xCE`.
    /// Fixed [`super::datagram::HDR_META_BODY_LEN`]-byte body, no placeholder — presence is
    /// remaining length after `preferred_codec`. Omitted / no HDR display → `None`.
    pub display_hdr: Option<HdrMeta>,
    /// Non-video bits ([`CLIENT_CAP_CURSOR`]). After `display_hdr`; that block has no
    /// placeholder, so remaining length < `HDR_META_BODY_LEN` means no HDR and these bytes
    /// *are* the post-HDR tail. Budget: 1+2+4+1 = 8 of 27. Omitted / zero → `0`.
    pub client_caps: u8,
    /// Largest sealed video-shard payload this client accepts. Non-zero ⇒ mid-session
    /// `shard_payload` changes are safe, and the value is the jumbo ceiling. `0` = legacy:
    /// host must not change sealed geometry mid-session. 2 LE bytes after `client_caps`.
    pub max_shard_payload: u16,
    /// Requested capture rate (`48_000`, `96_000`, or the 44.1 kHz family). A request, never
    /// a fact — the client opens its device from [`Welcome::audio_rate_hz`]. Requires
    /// [`CLIENT_CAP_AUDIO_HIRES`]. `0` and absence both decode to
    /// [`SAMPLE_RATE_HZ`](crate::audio::SAMPLE_RATE_HZ).
    pub audio_rate_hz: u32,
    /// Requested depth: [`BITS_16`](crate::audio::pcm::BITS_16) or
    /// [`BITS_24`](crate::audio::pcm::BITS_24). Host answers in [`Welcome::audio_bits`].
    /// Last field: `0`/absence → 16-bit, and nothing can force this byte — a 96 kHz/16-bit
    /// request emits the rate and stops.
    pub audio_bits: u8,
}

/// QUIC application close: client deliberate quit. Host tears the virtual display down
/// immediately (no keep-alive linger). Any other close still lingers for reconnect.
pub const QUIT_CLOSE_CODE: u32 = 0x51;

/// QUIC application close: dedicated-session game process exited. Sibling of
/// [`QUIT_CLOSE_CODE`]; clients that ignore it still end the session.
pub const APP_EXITED_CLOSE_CODE: u32 = 0x52;

/// Longest [`Hello`] device name (UTF-8 bytes). Truncated on encode, rejected on decode.
pub const HELLO_NAME_MAX: usize = 64;

/// Longest [`Hello::launch`] id (UTF-8 bytes). Ids are short; 128 bounds the length prefix.
pub const HELLO_LAUNCH_MAX: usize = 128;

/// [`Welcome::cipher`]: AES-128-GCM. Default; the only id pre-cipher builds know.
pub const CIPHER_AES_128_GCM: u8 = 0;
/// [`Welcome::cipher`]: ChaCha20-Poly1305 (RFC 8439), via [`VIDEO_CAP_CHACHA20`].
pub const CIPHER_CHACHA20_POLY1305: u8 = 1;

/// [`Welcome::audio_codec`]: Opus on `0xC9` (48 kHz). `0` so absence and older hosts both
/// read as Opus; a declined hi-res session resolves here — silence is the unacceptable outcome.
pub const AUDIO_CODEC_OPUS: u8 = 0;
/// [`Welcome::audio_codec`] id `1`, reserved and unimplemented. The design numbers Opus=0,
/// FLAC=1, PCM=2; this id is burned so [`AUDIO_CODEC_PCM`] stays `2`. Why FLAC lost lives in
/// `crate::audio::pcm`. No host emits this; no client should accept it.
pub const AUDIO_CODEC_FLAC_RESERVED: u8 = 1;
/// [`Welcome::audio_codec`]: raw interleaved LE PCM on `0xD3` (`crate::audio::pcm`).
/// `2` because [`AUDIO_CODEC_FLAC_RESERVED`] holds `1`.
pub const AUDIO_CODEC_PCM: u8 = 2;

/// `host → client`: the complete session offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub abi_version: u32,
    pub udp_port: u16,
    pub mode: Mode,
    pub fec: FecConfig,
    pub shard_payload: u16,
    pub encrypt: bool,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Seed/testing: frames the host will send (`0` = unbounded).
    pub frames: u32,
    /// Resolved compositor. [`Hello::compositor`] if available, else auto-detect.
    /// Older host omit → `Auto` (unknown).
    pub compositor: CompositorPref,
    /// Resolved virtual-gamepad backend. DualSense feedback (0xCD) only arrives if this is
    /// DualSense. Older host omit → `Auto` (assume X-Box 360).
    pub gamepad: GamepadPref,
    /// Encoder bitrate the host configured, kbps. Older host omit → `0` (unknown).
    pub bitrate_kbps: u32,
    /// Encode bit depth: `8` or `10` (Main10, only if [`VIDEO_CAP_10BIT`]). Omit → `8`.
    pub bit_depth: u8,
    /// CICP colour the host encodes with. Omit → [`ColorInfo::SDR_BT709`]. Mastering metadata
    /// arrives separately on [`HDR_META_MAGIC`].
    pub color: ColorInfo,
    /// HEVC `chroma_format_idc`: [`CHROMA_IDC_420`] or [`CHROMA_IDC_444`] (only if
    /// [`VIDEO_CAP_444`] and the GPU opened 4:4:4). Hint; SPS is authoritative. Omit → 4:2:0.
    pub chroma_format: u8,
    /// Channels the host will send on `0xC9`. Build the Opus decoder from this via
    /// [`crate::audio::layout_for`], never from the Hello request. Omit → `2`.
    pub audio_channels: u8,
    /// Codec the host will emit ([`resolve_codec`]). Build the decoder from this.
    /// Omit → [`CODEC_HEVC`].
    pub codec: u8,
    /// Host input bits ([`HOST_CAP_GAMEPAD_STATE`]): snapshots vs legacy per-transition
    /// events. Omit → `0`.
    pub host_caps: u8,
    /// Session AEAD: [`CIPHER_AES_128_GCM`] or [`CIPHER_CHACHA20_POLY1305`]. Emitted only when
    /// non-zero, so AES Welcome stays byte-identical. Decode is fail-closed: unknown id is
    /// `Err`, never a silent AES fallback (that session would not decrypt).
    pub cipher: u8,
    /// Management-API port (game library). Distinct from `udp_port` and the QUIC control port.
    /// `0` = not advertised (older host; client uses 47990). After the cipher block (69, or 101
    /// with ChaCha); emitting it forces the `cipher` placeholder — see [`Welcome::encode`].
    pub mgmt_port: u16,
    /// [`GRANT_GAMEPAD`](super::GRANT_GAMEPAD)-family mask. The client uses this to skip
    /// capture that cannot land. Omit → [`GRANT_ALL`](super::GRANT_ALL).
    pub grants: u32,
    /// Seconds until access expires, measured when this Welcome is built. `0` = permanent
    /// (also older-host omit). Mid-session changes: [`AccessUpdate`](super::AccessUpdate).
    pub expires_in_secs: u32,
    /// 32-byte ChaCha20-Poly1305 key, present iff `cipher == 1`, at 69..101. The 16-byte `key`
    /// keeps its offset and stays independently random. Decode rejects `cipher == 1` with
    /// fewer than 32 key bytes.
    pub key_chacha: Option<[u8; 32]>,
    /// Audio plane: [`AUDIO_CODEC_OPUS`] (`0xC9`/`0xD2`) or [`AUDIO_CODEC_PCM`] (`0xD3`).
    /// Never both, never switched mid-session. Non-Opus forces the four audio fields and every
    /// earlier placeholder so Opus Welcome stays 68 bytes; a slip onto offset 68 is `cipher`.
    /// Offset 79 (AES) / 111 (ChaCha). Omit → Opus.
    pub audio_codec: u8,
    /// Resolved capture rate — open the client device from this, never from Hello.
    /// WASAPI `AUTOCONVERTPCM` accepts 96 kHz against a 48 kHz engine and returns interpolated
    /// samples with no error; the host must decline rather than pad. Absent / `0` →
    /// [`SAMPLE_RATE_HZ`](crate::audio::SAMPLE_RATE_HZ).
    pub audio_rate_hz: u32,
    /// Resolved depth. Wrong stride desynchronises every sample (24-bit read at 2 bytes is
    /// noise). Absent / `0` / unsupported
    /// ([`depth_is_supported`](crate::audio::pcm::depth_is_supported)) → `BITS_16`.
    pub audio_bits: u8,
    /// Resolved `0xD3` frame duration, microseconds. `0` for Opus (fixed 5 ms on `0xC9`).
    /// Do not hardcode: [`frame_us_for`](crate::audio::pcm::frame_us_for) sizes against the
    /// path MTU (this plane is never fragmented; 96 kHz/24-bit at 1472 B only fits 2 ms).
    pub audio_frame_us: u16,
    /// Second host-capability byte ([`HOST_CAP2_REPEAT_MARK`](super::HOST_CAP2_REPEAT_MARK),
    /// [`HOST_CAP2_TOUCH`](super::HOST_CAP2_TOUCH)). Nonzero forces the audio-block placeholders.
    /// Last field: offset 87 (AES) / 119 (ChaCha). Omit → `0`.
    pub host_caps2: u8,
}

/// `client → host`: data plane is bound, begin streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start {
    pub client_udp_port: u16,
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary (a straddling char is
/// dropped whole). Shared by Hello name/launch and [`PairRequest`](super::PairRequest).
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
        b.push(self.compositor.to_u8()); // offset 20; older hosts read [0..20]
        b.push(self.gamepad.to_u8()); // offset 21
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // offset 22..26
        // name at 26: `len u8 || UTF-8`. Omitted when None and no later field, so a Hello
        // with neither name nor launch stays 26 bytes. A later non-default field forces
        // every earlier placeholder (0-length name/launch, default trailing bytes) so
        // that field lands at a deterministic offset.
        let ac_present = self.audio_channels != 2;
        let vcodecs_present = self.video_codecs != 0;
        let pref_present = self.preferred_codec != 0;
        let hdr_present = self.display_hdr.is_some();
        let ccaps_present = self.client_caps != 0;
        let msp_present = self.max_shard_payload != 0;
        // Wire `0` and the legacy 48 kHz / 16-bit both count as default: decode maps
        // absence to those values, so a struct that spells them must encode identical
        // bytes to one left at zero.
        let arate_present =
            self.audio_rate_hz != 0 && self.audio_rate_hz != crate::audio::SAMPLE_RATE_HZ;
        let abits_present = self.audio_bits != 0 && self.audio_bits != crate::audio::pcm::BITS_16;
        let audio_present = arate_present || abits_present;
        let need_placeholders = self.video_caps != 0
            || ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present;
        match (&self.name, &self.launch) {
            (None, None) if !need_placeholders => {}
            (name, _) => {
                let n = truncate_to(name.as_deref().unwrap_or(""), HELLO_NAME_MAX);
                b.push(n.len() as u8);
                b.extend_from_slice(n.as_bytes());
            }
        }
        if self.launch.is_some() || need_placeholders {
            let l = truncate_to(self.launch.as_deref().unwrap_or(""), HELLO_LAUNCH_MAX);
            b.push(l.len() as u8);
            b.extend_from_slice(l.as_bytes());
        }
        if need_placeholders {
            b.push(self.video_caps);
        }
        if ac_present
            || vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present
        {
            b.push(self.audio_channels);
        }
        if vcodecs_present
            || pref_present
            || hdr_present
            || ccaps_present
            || msp_present
            || audio_present
        {
            b.push(self.video_codecs);
        }
        if pref_present || hdr_present || ccaps_present || msp_present || audio_present {
            b.push(self.preferred_codec);
        }
        // No placeholder. Decoder uses remaining length, which caps the post-HDR tail at
        // HDR_META_BODY_LEN − 1 bytes.
        if let Some(m) = &self.display_hdr {
            super::datagram::write_hdr_meta_body(m, &mut b);
        }
        if ccaps_present || msp_present || audio_present {
            b.push(self.client_caps);
        }
        if msp_present || audio_present {
            b.extend_from_slice(&self.max_shard_payload.to_le_bytes());
        }
        // Emitted as 48 000 when only `audio_bits` is non-default: struct `0` still means
        // 48 kHz, and the bytes a decoder reads must match the struct.
        if audio_present {
            let rate = if arate_present {
                self.audio_rate_hz
            } else {
                crate::audio::SAMPLE_RATE_HZ
            };
            b.extend_from_slice(&rate.to_le_bytes());
        }
        // Last field: nothing can force it. A 96 kHz/16-bit request stops after the rate.
        if abits_present {
            b.push(self.audio_bits);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Hello> {
        if b.len() < 20 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Hello"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        // name/launch raw length bytes (including 0 placeholders and oversized garbage)
        // locate the tail, so a corrupt name never panics — later fields just miss and
        // decode to defaults.
        let name_len = b.get(26).copied().unwrap_or(0) as usize;
        let launch_off = 27 + name_len;
        let launch_len = b.get(launch_off).copied().unwrap_or(0) as usize;
        let tail = launch_off + 1 + launch_len;

        // display_hdr presence is remaining length after preferred_codec: ≥ HDR_META_BODY_LEN
        // ⇒ the block is there. Sound only while the post-HDR tail stays under that many
        // bytes (budget on [`Hello::client_caps`]). Computed once; every later field
        // reads off `post_hdr`.
        let has_hdr = b.len().saturating_sub(tail + 4) >= super::datagram::HDR_META_BODY_LEN;
        let post_hdr = if has_hdr {
            tail + 4 + super::datagram::HDR_META_BODY_LEN
        } else {
            tail + 4
        };
        Ok(Hello {
            abi_version: u32at(4),
            mode: Mode {
                width: u32at(8),
                height: u32at(12),
                refresh_hz: u32at(16),
            },
            compositor: b
                .get(20)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(21)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            bitrate_kbps: b
                .get(22..26)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent / oversized / non-UTF-8 → None. Never fail the handshake over a label.
            name: (name_len > 0 && name_len <= HELLO_NAME_MAX)
                .then(|| {
                    b.get(27..27 + name_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            launch: (launch_len > 0 && launch_len <= HELLO_LAUNCH_MAX)
                .then(|| {
                    b.get(launch_off + 1..launch_off + 1 + launch_len)
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .map(String::from)
                })
                .flatten(),
            video_caps: b.get(tail).copied().unwrap_or(0),
            // Unsupported channel count must not build a decoder.
            audio_channels: crate::audio::normalize_channels(b.get(tail + 1).copied().unwrap_or(2)),
            // 0 = older client; resolve_codec treats it as HEVC-only.
            video_codecs: b.get(tail + 2).copied().unwrap_or(0),
            preferred_codec: b.get(tail + 3).copied().unwrap_or(0),
            // Presence is remaining length (`has_hdr`), not a flag.
            display_hdr: has_hdr
                .then(|| {
                    b.get(tail + 4..tail + 4 + super::datagram::HDR_META_BODY_LEN)
                        .map(super::datagram::read_hdr_meta_body)
                })
                .flatten(),
            client_caps: b.get(post_hdr).copied().unwrap_or(0),
            // 0 = no mid-session renegotiation, no jumbo.
            max_shard_payload: b
                .get(post_hdr + 1..post_hdr + 3)
                .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent or 0 → 48 kHz, as a real rate, so nothing downstream treats 0 as
            // 48 000. Unknown non-zero rates pass through: rewriting them would mislabel
            // the stream. The host answers in Welcome::audio_rate_hz.
            audio_rate_hz: b
                .get(post_hdr + 3..post_hdr + 7)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .filter(|&hz| hz != 0)
                .unwrap_or(crate::audio::SAMPLE_RATE_HZ),
            // Depth is a byte stride: unsupported values would unpack 0xD3 wrongly.
            // Fallback to 16 costs a hi-res session, never correctness.
            audio_bits: match b.get(post_hdr + 7).copied() {
                Some(d) if crate::audio::pcm::depth_is_supported(d) => d,
                _ => crate::audio::pcm::BITS_16,
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
        b.push(self.compositor.to_u8()); // offset 53; older clients read [0..53]
        b.push(self.gamepad.to_u8()); // offset 54
        b.extend_from_slice(&self.bitrate_kbps.to_le_bytes()); // offset 55..59
        b.push(self.bit_depth); // offset 59
        b.push(self.color.primaries); // 60..64; older clients → SDR BT.709
        b.push(self.color.transfer);
        b.push(self.color.matrix);
        b.push(self.color.full_range);
        b.push(self.chroma_format); // offset 64; omit → 4:2:0
        b.push(self.audio_channels); // offset 65; omit → stereo
        b.push(self.codec); // offset 66; omit → HEVC
        b.push(self.host_caps); // offset 67; omit → 0
        // Cipher at 68 + ChaCha key at 69..101, emitted only when non-AES so an AES
        // Welcome stays byte-identical. Host only sets cipher toward VIDEO_CAP_CHACHA20.
        debug_assert_eq!(
            self.cipher == CIPHER_CHACHA20_POLY1305,
            self.key_chacha.is_some(),
            "key_chacha present iff cipher == 1"
        );
        // Later tail fields force every earlier placeholder (cipher=0, mgmt=0, …).
        // Without that, an AES Welcome carrying mgmt_port puts the port's low byte at
        // 68, where shipped clients fail-close on unknown cipher. Audio presence is
        // codec ≠ Opus so a 48 kHz/16-bit PCM session is not silent-Opus on the wire.
        let mgmt_present = self.mgmt_port != 0;
        let access_present = self.grants != super::access::GRANT_ALL || self.expires_in_secs != 0;
        let audio_present = self.audio_codec != AUDIO_CODEC_OPUS;
        let caps2_present = self.host_caps2 != 0;
        if self.cipher != CIPHER_AES_128_GCM
            || mgmt_present
            || access_present
            || audio_present
            || caps2_present
        {
            b.push(self.cipher);
            if let Some(k) = &self.key_chacha {
                b.extend_from_slice(k);
            }
            if mgmt_present || access_present || audio_present || caps2_present {
                b.extend_from_slice(&self.mgmt_port.to_le_bytes());
            }
            if access_present || audio_present || caps2_present {
                b.extend_from_slice(&self.grants.to_le_bytes());
                b.extend_from_slice(&self.expires_in_secs.to_le_bytes());
            }
            if audio_present || caps2_present {
                b.push(self.audio_codec);
                b.extend_from_slice(&self.audio_rate_hz.to_le_bytes());
                b.push(self.audio_bits);
                b.extend_from_slice(&self.audio_frame_us.to_le_bytes());
            }
            if caps2_present {
                b.push(self.host_caps2);
            }
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<Welcome> {
        // Trailing from compositor (53) on is optional. mgmt_port, grants, audio, and
        // host_caps2 follow the cipher block — shifted 32 when a ChaCha key precedes
        // them — so they are read from `mgmt_off`, not a constant. Emitting a later
        // field forces every earlier one; full length 87 AES / 119 ChaCha.
        if b.len() < 53 || &b[0..4] != MAGIC {
            return Err(PunktfunkError::InvalidArg("bad Welcome"));
        }
        let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let mut key = [0u8; 16];
        key.copy_from_slice(&b[29..45]);
        let mut salt = [0u8; 4];
        salt.copy_from_slice(&b[45..49]);
        // Absent → AES. Fail-closed: cipher==1 with a short key, or id ≥ 2, is Err.
        // A silent AES fallback would not decrypt.
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
        // After the cipher block. Absent → 0; client uses the compiled-in default.
        let mgmt_off = if cipher == CIPHER_CHACHA20_POLY1305 {
            101
        } else {
            69
        };
        let mgmt_port = b
            .get(mgmt_off..mgmt_off + 2)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // Absent (older host, or encode omitted GRANT_ALL/permanent) → GRANT_ALL / 0.
        let grants_off = mgmt_off + 2;
        let grants = b
            .get(grants_off..grants_off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(super::access::GRANT_ALL);
        let expires_in_secs = b
            .get(grants_off + 4..grants_off + 8)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // Absent → Opus at 48 kHz / 16-bit. Same bytes as a declined hi-res session.
        let audio_off = grants_off + 8;
        // Codec is verbatim: folding an unknown id onto Opus would play the wrong
        // plane as silence. Client refuses a plane it cannot play.
        let audio_codec = b.get(audio_off).copied().unwrap_or(AUDIO_CODEC_OPUS);
        // Rate is not clamped; only 0/absent → legacy. Clamping 44100 to 48000
        // would mislabel the stream.
        let audio_rate_hz = b
            .get(audio_off + 1..audio_off + 5)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .filter(|&hz| hz != 0)
            .unwrap_or(crate::audio::SAMPLE_RATE_HZ);
        // Depth feeds unpack stride: unsupported → 16, matching audio_channels.
        // Only a corrupt or future wire reaches this (pinned-TLS control stream).
        let audio_bits = match b.get(audio_off + 5).copied() {
            Some(d) if crate::audio::pcm::depth_is_supported(d) => d,
            _ => crate::audio::pcm::BITS_16,
        };
        let audio_frame_us = b
            .get(audio_off + 6..audio_off + 8)
            .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
            .unwrap_or(0);
        // Trails the audio block (87 AES / 119 ChaCha). Absent → 0.
        let host_caps2 = b.get(audio_off + 8).copied().unwrap_or(0);
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
            compositor: b
                .get(53)
                .map(|&v| CompositorPref::from_u8(v))
                .unwrap_or_default(),
            gamepad: b
                .get(54)
                .map(|&v| GamepadPref::from_u8(v))
                .unwrap_or_default(),
            bitrate_kbps: b
                .get(55..59)
                .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
                .unwrap_or(0),
            // Absent → 8 (the only depth those hosts encode).
            bit_depth: b.get(59).copied().unwrap_or(8),
            // Absent → SDR BT.709 limited.
            color: ColorInfo {
                primaries: b.get(60).copied().unwrap_or(ColorInfo::CP_BT709),
                transfer: b.get(61).copied().unwrap_or(ColorInfo::TRC_BT709),
                matrix: b.get(62).copied().unwrap_or(ColorInfo::MC_BT709),
                full_range: b.get(63).copied().unwrap_or(0),
            },
            // Absent / 0 / unknown → 4:2:0. Only CHROMA_IDC_444 flips the client.
            chroma_format: match b.get(64).copied() {
                Some(CHROMA_IDC_444) => CHROMA_IDC_444,
                _ => CHROMA_IDC_420,
            },
            // Absent → stereo. Non-{6,8} normalizes so a corrupt byte cannot build a decoder.
            audio_channels: crate::audio::normalize_channels(b.get(65).copied().unwrap_or(2)),
            // Absent / unknown → HEVC.
            codec: match b.get(66).copied() {
                Some(CODEC_H264) => CODEC_H264,
                Some(CODEC_AV1) => CODEC_AV1,
                Some(CODEC_PYROWAVE) => CODEC_PYROWAVE,
                _ => CODEC_HEVC,
            },
            // Absent → 0 (legacy per-transition events).
            host_caps: b.get(67).copied().unwrap_or(0),
            mgmt_port,
            grants,
            expires_in_secs,
            cipher,
            key_chacha,
            audio_codec,
            audio_rate_hz,
            audio_bits,
            audio_frame_us,
            host_caps2,
        })
    }

    /// Build the data-plane [`Config`] this offer describes (for `role`).
    pub fn session_config(&self, role: Role) -> Config {
        let mut c = Config::p1_defaults(role);
        c.phase = ProtocolPhase::P1GameStream; // P1GameStream until the P2 packet rev lands
        c.fec = self.fec;
        c.shard_payload = self.shard_payload as usize;
        c.encrypt = self.encrypt;
        // ChaCha key when cipher==1 (decode guarantees Some); AES key otherwise.
        c.key = match (self.cipher, self.key_chacha) {
            (CIPHER_CHACHA20_POLY1305, Some(k)) => SessionKey::ChaCha20Poly1305(k),
            _ => SessionKey::Aes128Gcm(self.key),
        };
        c.salt = self.salt;
        // Client reassembler ceiling from the negotiated rate: 4× average frame at
        // bitrate_kbps (IDR headroom), floor 8 MiB, cap 64 MiB. Host never reassembles
        // video. bitrate 0 (pre-negotiation) keeps the 64 MiB p1_defaults bound.
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
    use crate::audio::pcm::{depth_is_supported, frame_us_for, BITS_16, BITS_24};
    use crate::audio::SAMPLE_RATE_HZ;
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
            codec: CODEC_H264,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        assert_eq!(Welcome::decode(&w.encode()).unwrap(), w);

        // 50 Mbps / 240 Hz → ~104 KB; the 8 MiB floor governs. Host never reassembles video.
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
        // 1.5 Gbps at 60 Hz = 4 × 3.125 MB = 12.5 MB, between the 8 MiB floor and 64 MiB cap.
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
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // AES Welcome is 68 bytes — the pre-cipher wire form.
        let enc = base.encode();
        assert_eq!(enc.len(), 68);
        assert_eq!(Welcome::decode(&enc).unwrap(), base);

        // Cipher byte at 68, 32-byte key at 69..101.
        let k32: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..base
        };
        let cenc = cha.encode();
        assert_eq!(cenc.len(), 68 + 1 + 32);
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);

        let old_host = Welcome::decode(&cenc[..68]).unwrap();
        assert_eq!(old_host.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_host.key_chacha, None);

        // cipher==1 with a short key, or id ≥ 2, is Err. A silent AES fallback would not decrypt.
        assert!(Welcome::decode(&cenc[..69]).is_err());
        assert!(Welcome::decode(&cenc[..100]).is_err());
        let mut bad = cenc.clone();
        bad[68] = 2;
        assert!(Welcome::decode(&bad).is_err());

        let aes_cfg = base.session_config(Role::Client);
        assert_eq!(aes_cfg.key, SessionKey::Aes128Gcm([7u8; 16]));
        aes_cfg.validate().expect("AES config validates");
        let cha_cfg = cha.session_config(Role::Client);
        assert_eq!(cha_cfg.key, SessionKey::ChaCha20Poly1305(k32));
        cha_cfg.validate().expect("ChaCha config validates");

        // mgmt_port after cipher: without a cipher placeholder the port's low byte lands at
        // 68. 47991 is 0xBB57 → byte 68 = 0x57, an unknown id; shipped clients fail-close.
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

        // ChaCha: port at 101..103, after the 32-byte key.
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

        // No advertised port: AES Welcome stays 68 bytes.
        assert_eq!(base.encode().len(), 68);
        assert_eq!(Welcome::decode(&enc).unwrap().mgmt_port, 0);
        assert_eq!(Welcome::decode(&cenc).unwrap().mgmt_port, 0);
        // Truncated tail is not half a port.
        assert_eq!(Welcome::decode(&menc[..70]).unwrap().mgmt_port, 0);

        // Access advert forces cipher=0 and mgmt=0 so the u32s land at 71..79 (AES)
        // or 103..111 (ChaCha).
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
        assert_eq!(Welcome::decode(&genc).unwrap().mgmt_port, 0);

        // All three trailing features, behind a ChaCha key: 103..111.
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

        // Shorter wire forms (pre-cipher, cipher-only, mgmt-port) → GRANT_ALL / permanent.
        for old in [&enc[..], &cenc[..], &menc[..]] {
            let w = Welcome::decode(old).unwrap();
            assert_eq!(w.grants, GRANT_ALL);
            assert_eq!(w.expires_in_secs, 0);
        }
        // Partial u32 is never half a mask; grants without expiry stay permanent.
        assert_eq!(Welcome::decode(&genc[..73]).unwrap().grants, GRANT_ALL);
        let g_only = Welcome::decode(&genc[..75]).unwrap();
        assert_eq!(g_only.grants, GRANT_PRESET_CONTROLLER_ONLY);
        assert_eq!(g_only.expires_in_secs, 0);

        // A reader that stops at 71 sees AES and unknown port; the advert does not perturb it.
        let old_view = Welcome::decode(&genc[..71]).unwrap();
        assert_eq!(old_view.cipher, CIPHER_AES_128_GCM);
        assert_eq!(old_view.mgmt_port, 0);
        assert_eq!(old_view, base);

        // GRANT_ALL / permanent emits no advert — still 68 bytes.
        assert_eq!(base.encode().len(), 68);
    }

    #[test]
    fn codec_negotiation_and_back_compat() {
        // Precedence HEVC > AV1 > H.264; preference 0.
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_HEVC | CODEC_AV1, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_AV1, CODEC_AV1 | CODEC_H264, 0),
            Some(CODEC_AV1)
        );
        assert_eq!(resolve_codec(CODEC_H264, CODEC_H264, 0), Some(CODEC_H264));
        // Software host (H.264 only) + HEVC-only client share nothing → refuse.
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, 0), None);
        // Older client (0 = no codec byte) is HEVC-only.
        assert_eq!(
            resolve_codec(0, CODEC_HEVC | CODEC_H264, 0),
            Some(CODEC_HEVC)
        );
        assert_eq!(resolve_codec(0, CODEC_H264, 0), None);

        // Soft preference overrides precedence when the host can emit it.
        assert_eq!(
            resolve_codec(CODEC_H264 | CODEC_HEVC, CODEC_H264 | CODEC_HEVC, CODEC_H264),
            Some(CODEC_H264)
        );
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_AV1, CODEC_HEVC | CODEC_AV1, CODEC_AV1),
            Some(CODEC_AV1)
        );
        // Preferred codec not in the shared set → precedence.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_H264, CODEC_HEVC | CODEC_H264, CODEC_AV1),
            Some(CODEC_HEVC)
        );
        assert_eq!(resolve_codec(CODEC_HEVC, CODEC_H264, CODEC_HEVC), None);

        // PyroWave is opt-in only: mutual support never auto-selects it.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC | CODEC_PYROWAVE, 0),
            Some(CODEC_HEVC)
        );
        // Only shared codec, still refused — an all-intra 200 Mbps stream must not be a fallback.
        assert_eq!(resolve_codec(CODEC_PYROWAVE, CODEC_PYROWAVE, 0), None);
        assert_eq!(
            resolve_codec(
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_HEVC | CODEC_PYROWAVE,
                CODEC_PYROWAVE
            ),
            Some(CODEC_PYROWAVE)
        );
        // Preference against a host without the backend falls back to the ladder.
        assert_eq!(
            resolve_codec(CODEC_HEVC | CODEC_PYROWAVE, CODEC_HEVC, CODEC_PYROWAVE),
            Some(CODEC_HEVC)
        );
        // Decode must not fold PyroWave to HEVC (that sent wavelet AUs into an HEVC decoder).
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
                audio_codec: AUDIO_CODEC_OPUS,
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
                audio_frame_us: 0,
                host_caps2: 0,
            }
            .encode(),
        )
        .unwrap();
        assert_eq!(pw_w.codec, CODEC_PYROWAVE);
        // Unknown future bit still folds to HEVC.
        pw_w.codec = 0x40;
        assert_eq!(Welcome::decode(&pw_w.encode()).unwrap().codec, CODEC_HEVC);

        // Extra trailing codec bytes are skipped by a build that ignores them.
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
            audio_channels: 2,
            video_codecs: CODEC_H264 | CODEC_HEVC,
            preferred_codec: CODEC_H264,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        let enc = h.encode();
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec.video_codecs, CODEC_H264 | CODEC_HEVC);
        assert_eq!(dec.preferred_codec, CODEC_H264);
        // Drop preferred_codec: video_codecs intact, preference 0.
        let no_pref = &enc[..enc.len() - 1];
        assert_eq!(
            Hello::decode(no_pref).unwrap().video_codecs,
            CODEC_H264 | CODEC_HEVC
        );
        assert_eq!(Hello::decode(no_pref).unwrap().preferred_codec, 0);
        // No video_codecs/preferred bytes → 0 (HEVC-only).
        let legacy = &enc[..enc.len() - 2];
        assert_eq!(Hello::decode(legacy).unwrap().video_codecs, 0);
        assert_eq!(Hello::decode(legacy).unwrap().preferred_codec, 0);

        // No codec byte → HEVC.
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
                audio_codec: AUDIO_CODEC_OPUS,
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
                audio_frame_us: 0,
                host_caps2: 0,
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
            video_codecs: CODEC_H264 | CODEC_HEVC,
            preferred_codec: CODEC_HEVC,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        assert_eq!(Hello::decode(&h.encode()).unwrap(), h);
        let s = Start {
            client_udp_port: 1234,
        };
        assert_eq!(Start::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn hello_welcome_compositor_back_compat() {
        // Truncation both ways: missing trailing bytes → Auto; extra bytes ignored.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        let enc = h.encode();
        assert_eq!(enc.len(), 26);
        // 20-byte Hello → both Auto, no bitrate.
        let legacy = Hello::decode(&enc[..20]).unwrap();
        assert_eq!(legacy.compositor, CompositorPref::Auto);
        assert_eq!(legacy.gamepad, GamepadPref::Auto);
        assert_eq!(legacy.bitrate_kbps, 0);
        assert_eq!(legacy.mode, h.mode);
        // 21-byte Hello → compositor intact, gamepad Auto.
        let mid = Hello::decode(&enc[..21]).unwrap();
        assert_eq!(mid.compositor, CompositorPref::Mutter);
        assert_eq!(mid.gamepad, GamepadPref::Auto);
        // 22-byte Hello → gamepad intact, bitrate 0.
        let pre_bitrate = Hello::decode(&enc[..22]).unwrap();
        assert_eq!(pre_bitrate.gamepad, GamepadPref::DualSense);
        assert_eq!(pre_bitrate.bitrate_kbps, 0);
        // Full message carries bitrate.
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
            audio_channels: 6,
            codec: CODEC_HEVC,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: GRANT_ALL,
            expires_in_secs: 0,
            cipher: 0,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        let wenc = w.encode();
        assert_eq!(wenc.len(), 68); // 60 base + colour + chroma + audio + codec + host-caps
        let legacy_w = Welcome::decode(&wenc[..53]).unwrap();
        assert_eq!(legacy_w.compositor, CompositorPref::Auto);
        assert_eq!(legacy_w.gamepad, GamepadPref::Auto);
        assert_eq!(legacy_w.bitrate_kbps, 0);
        assert_eq!(legacy_w.frames, 0);
        assert_eq!(legacy_w.key, w.key);
        let mid_w = Welcome::decode(&wenc[..54]).unwrap();
        assert_eq!(mid_w.compositor, CompositorPref::Kwin);
        assert_eq!(mid_w.gamepad, GamepadPref::Auto);
        // 55-byte Welcome → gamepad intact, bitrate 0.
        let pre_bitrate_w = Welcome::decode(&wenc[..55]).unwrap();
        assert_eq!(pre_bitrate_w.gamepad, GamepadPref::Xbox360);
        assert_eq!(pre_bitrate_w.bitrate_kbps, 0);
        assert_eq!(pre_bitrate_w.bit_depth, 8); // no trailing byte → 8-bit
        assert_eq!(legacy_w.bit_depth, 8);
        // 60-byte Welcome → SDR BT.709.
        let pre_color_w = Welcome::decode(&wenc[..60]).unwrap();
        assert_eq!(pre_color_w.bit_depth, 10);
        assert_eq!(pre_color_w.color, ColorInfo::SDR_BT709);
        assert_eq!(pre_color_w.chroma_format, CHROMA_IDC_420); // no chroma byte → 4:2:0
        assert_eq!(legacy_w.color, ColorInfo::SDR_BT709);
        assert_eq!(legacy_w.chroma_format, CHROMA_IDC_420);
        // 64-byte Welcome: colour, no chroma/audio → 4:2:0 + stereo.
        let pre_chroma_w = Welcome::decode(&wenc[..64]).unwrap();
        assert_eq!(pre_chroma_w.color, ColorInfo::HDR10_BT2020_PQ);
        assert_eq!(pre_chroma_w.chroma_format, CHROMA_IDC_420);
        assert_eq!(pre_chroma_w.audio_channels, 2); // offset 65 absent → stereo
        // 65-byte Welcome: chroma, no audio → 4:4:4 + stereo.
        let pre_audio_w = Welcome::decode(&wenc[..65]).unwrap();
        assert_eq!(pre_audio_w.chroma_format, CHROMA_IDC_444);
        assert_eq!(pre_audio_w.audio_channels, 2);
        assert_eq!(Welcome::decode(&wenc).unwrap().bitrate_kbps, 120_000);
        assert_eq!(Welcome::decode(&wenc).unwrap().bit_depth, 10);
        assert_eq!(
            Welcome::decode(&wenc).unwrap().color,
            ColorInfo::HDR10_BT2020_PQ
        );
        assert_eq!(
            Welcome::decode(&wenc).unwrap().chroma_format,
            CHROMA_IDC_444
        );
        assert_eq!(Welcome::decode(&wenc).unwrap().audio_channels, 6);
        // 67-byte Welcome → host_caps 0; full form carries the bit.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        let enc = base.encode();
        assert_eq!(
            Hello::decode(&enc).unwrap().name.as_deref(),
            Some("Enrico's MacBook")
        );
        // 26-byte peer ignores the trailing name; named host reading 26 bytes → None.
        assert_eq!(Hello::decode(&enc[..26]).unwrap().name, None);
        // No name → 26 bytes, same as the bitrate-era form.
        let unnamed = Hello {
            name: None,
            ..base.clone()
        };
        assert_eq!(unnamed.encode().len(), 26);
        // Over-long names truncate on a char boundary within HELLO_NAME_MAX.
        let long = Hello {
            name: Some(format!("{}ü", "x".repeat(HELLO_NAME_MAX - 1))), // ü straddles HELLO_NAME_MAX
            ..base.clone()
        };
        let dec = Hello::decode(&long.encode()).unwrap();
        let n = dec.name.expect("truncated name still present");
        assert!(n.len() <= HELLO_NAME_MAX && n.starts_with('x'));
        // Corrupt length or bad UTF-8 → None, never Err.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        // Launch alone: a zero-length name placeholder keeps the offset deterministic.
        let with_launch = Hello {
            launch: Some("steam:570".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&with_launch.encode()).unwrap(), with_launch);
        let both = Hello {
            name: Some("Enrico's Mac".into()),
            launch: Some("custom:abc123".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // Name, no launch → launch None.
        let name_only = Hello {
            name: Some("Enrico's Mac".into()),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&name_only.encode()).unwrap().launch, None);
        // Neither field → 26 bytes, no launch placeholder.
        assert_eq!(base.encode().len(), 26);
        assert_eq!(Hello::decode(&base.encode()).unwrap().launch, None);
        // 26-byte peer ignores a trailing launch.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
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
        assert_eq!(Hello::decode(&with_hdr.encode()).unwrap(), with_hdr);
        // display_hdr alone still lands at a deterministic offset (placeholders through the tail).
        let hdr_only = Hello {
            video_caps: 0,
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        // Decode that stops at preferred_codec ignores the block; older Hello → None.
        let enc = with_hdr.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - HDR_META_BODY_LEN]).unwrap(),
            Hello {
                display_hdr: None,
                ..with_hdr.clone()
            }
        );
        assert_eq!(Hello::decode(&base.encode()).unwrap().display_hdr, None);
        // Truncated block → None, never a partial HdrMeta.
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 1]).unwrap().display_hdr,
            None
        );
        // 26 + 6 placeholders (name/launch/caps/channels/codecs/pref) + body.
        assert_eq!(hdr_only.encode().len(), 26 + 6 + HDR_META_BODY_LEN);
    }

    #[test]
    fn control_messages_disjoint_from_hello() {
        // Hello uses MAGIC (PKF1); control uses CTL_MAGIC (PKFc). No overlap at any abi.
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
                audio_rate_hz: SAMPLE_RATE_HZ,
                audio_bits: BITS_16,
            }
            .encode();
            assert!(PairRequest::decode(&h).is_err(), "abi {abi} parsed as pair");
            assert!(Reconfigure::decode(&h).is_err());
        }
        // PairRequest never parses as Hello.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        let vol = HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 8_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 0,
            max_fall: 400,
        };
        // Caps without HDR: remaining < HDR_META_BODY_LEN, so not a truncated HdrMeta.
        let caps_only = Hello {
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&caps_only.encode()).unwrap(), caps_only);
        // Caps after the fixed HDR block.
        let both = Hello {
            display_hdr: Some(vol),
            client_caps: CLIENT_CAP_CURSOR,
            max_shard_payload: 0,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&both.encode()).unwrap(), both);
        // HDR without caps is the pre-caps wire form (caps 0).
        let hdr_only = Hello {
            display_hdr: Some(vol),
            ..base.clone()
        };
        assert_eq!(Hello::decode(&hdr_only.encode()).unwrap(), hdr_only);
        assert_eq!(Hello::decode(&base.encode()).unwrap().client_caps, 0);
        // Truncating the caps byte: nothing before it moved.
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

    /// `max_shard_payload` forces earlier placeholders, composes with HDR, degrades to 0.
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        // Advertisement alone: earlier trailing fields are placeholders so the 2 LE bytes land.
        let adv = Hello {
            max_shard_payload: crate::config::max_shard_payload() as u16,
            ..base.clone()
        };
        assert_eq!(Hello::decode(&adv.encode()).unwrap(), adv);
        // Remaining-length disambiguation must still find caps and payload after HDR.
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
        // No trailing bytes → 0: host must not change sealed geometry mid-session.
        assert_eq!(Hello::decode(&base.encode()).unwrap().max_shard_payload, 0);
        // Truncating the 2 trailing bytes drops the advertisement only.
        let enc = full.encode();
        assert_eq!(
            Hello::decode(&enc[..enc.len() - 2]).unwrap(),
            Hello {
                max_shard_payload: 0,
                ..full.clone()
            }
        );
    }

    /// Codec ids are a registry, not a compact enum. `1` is burned: compacting PCM to `1`
    /// would make every shipped `2` read the wrong plane (silence, not an error).
    #[test]
    fn audio_codec_ids_match_the_design_doc_numbering() {
        assert_eq!(AUDIO_CODEC_OPUS, 0);
        assert_eq!(AUDIO_CODEC_FLAC_RESERVED, 1);
        assert_eq!(AUDIO_CODEC_PCM, 2);
        // Opus is 0 so an absent field is the legacy wire.
        assert_eq!(AUDIO_CODEC_OPUS, 0, "absence decodes to Opus");
    }

    /// Welcome tail is conditional: cipher at 68, ChaCha key at 69..101, so later fields sit
    /// at 79 (AES) or 111 (ChaCha). A decoder that uses a fixed offset, or a test that only
    /// covers AES, breaks the ChaCha path. See `design/hi-res-audio.md`.
    #[test]
    fn welcome_hires_audio_wire_under_both_ciphers() {
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
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // Opus session stays 68 bytes — the pre-cipher / pre-hi-res wire.
        assert_eq!(base.encode().len(), 68);
        assert_eq!(Welcome::decode(&base.encode()).unwrap(), base);

        // Presence is codec alone. Rate/depth must not put the block on an Opus Welcome.
        let opus_with_stray_format = Welcome {
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            audio_frame_us: 2000,
            ..base
        };
        assert_eq!(
            opus_with_stray_format.encode().len(),
            68,
            "only audio_codec puts the block on the wire"
        );

        // Frame duration from the ladder: 96 kHz/24-bit stereo in ~1400 B is 2 ms.
        // This plane is never fragmented; a hardcoded 2.5 ms would not send.
        let frame_us = frame_us_for(96_000, BITS_24, 2, 1400).expect("a rung fits the default MTU");
        assert_eq!(
            frame_us, 2000,
            "the documented rung at the default MTU ceiling"
        );
        let hires = Welcome {
            host_caps: HOST_CAP_AUDIO_HIRES,
            audio_codec: AUDIO_CODEC_PCM,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            audio_frame_us: frame_us as u16,
            ..base
        };

        // AES: audio block at 79..87 (cipher 1 + mgmt 2 + grants 4 + expiry 4 past 68).
        let enc = hires.encode();
        assert_eq!(enc.len(), 87, "68 + cipher 1 + mgmt 2 + access 8 + audio 8");
        assert_eq!(enc[68], CIPHER_AES_128_GCM, "forced cipher placeholder");
        assert_eq!(&enc[69..71], &0u16.to_le_bytes(), "forced mgmt placeholder");
        assert_eq!(&enc[71..75], &GRANT_ALL.to_le_bytes(), "forced grants");
        assert_eq!(&enc[75..79], &0u32.to_le_bytes(), "forced expiry");
        assert_eq!(enc[79], AUDIO_CODEC_PCM);
        assert_eq!(&enc[80..84], &96_000u32.to_le_bytes());
        assert_eq!(enc[84], BITS_24);
        assert_eq!(&enc[85..87], &(frame_us as u16).to_le_bytes());
        assert_eq!(Welcome::decode(&enc).unwrap(), hires);

        // ChaCha: same eight bytes at 111..119.
        let k32: [u8; 32] = core::array::from_fn(|i| i as u8 + 1);
        let cha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some(k32),
            ..hires
        };
        let cenc = cha.encode();
        assert_eq!(cenc.len(), 119, "87 + the 32-byte ChaCha key");
        assert_eq!(&cenc[101..103], &0u16.to_le_bytes(), "forced mgmt");
        assert_eq!(&cenc[103..107], &GRANT_ALL.to_le_bytes(), "forced grants");
        assert_eq!(&cenc[107..111], &0u32.to_le_bytes(), "forced expiry");
        assert_eq!(cenc[111], AUDIO_CODEC_PCM);
        assert_eq!(&cenc[112..116], &96_000u32.to_le_bytes());
        assert_eq!(cenc[116], BITS_24);
        assert_eq!(&cenc[117..119], &(frame_us as u16).to_le_bytes());
        assert_eq!(Welcome::decode(&cenc).unwrap(), cha);
        // Same eight bytes, 32-byte offset: decoder position comes from `cipher`, not a constant.
        assert_eq!(&enc[79..87], &cenc[111..119]);

        // Forced placeholders must decode as their own absence, or hi-res would invent a
        // mgmt port / access mask.
        for w in [
            Welcome::decode(&enc).unwrap(),
            Welcome::decode(&cenc).unwrap(),
        ] {
            assert_eq!(w.mgmt_port, 0, "forced placeholder, not an advertised port");
            assert_eq!(w.grants, GRANT_ALL, "forced placeholder, full control");
            assert_eq!(w.expires_in_secs, 0, "forced placeholder, permanent");
            assert!(depth_is_supported(w.audio_bits));
        }

        // Composes with real mgmt/grants values, not only zeros.
        let guest_hires = Welcome {
            mgmt_port: 47991,
            grants: GRANT_PRESET_CONTROLLER_ONLY,
            expires_in_secs: 4 * 3600,
            ..hires
        };
        let genc = guest_hires.encode();
        assert_eq!(
            genc.len(),
            87,
            "same length — the placeholders were already paid for"
        );
        assert_eq!(&genc[69..71], &47991u16.to_le_bytes());
        assert_eq!(&genc[71..75], &GRANT_PRESET_CONTROLLER_ONLY.to_le_bytes());
        assert_eq!(&genc[79..87], &enc[79..87], "the audio block is unmoved");
        assert_eq!(Welcome::decode(&genc).unwrap(), guest_hires);

        // Shorter wire forms are Opus at 48 kHz / 16-bit (a real rate, not raw 0).
        let mgmt_era = Welcome {
            mgmt_port: 47991,
            ..base
        }
        .encode();
        for old in [&base.encode()[..], &mgmt_era[..], &enc[..79], &cenc[..111]] {
            let w = Welcome::decode(old).unwrap();
            assert_eq!(w.audio_codec, AUDIO_CODEC_OPUS);
            assert_eq!(w.audio_rate_hz, SAMPLE_RATE_HZ);
            assert_eq!(w.audio_bits, BITS_16);
            assert_eq!(w.audio_frame_us, 0);
        }
        // Prefix through 79: HOST_CAP_AUDIO_HIRES rides host_caps at 67, not the audio block.
        assert_eq!(
            Welcome::decode(&enc[..79]).unwrap(),
            Welcome {
                host_caps: HOST_CAP_AUDIO_HIRES,
                ..base
            }
        );

        // Cut mid-u32: rate is the legacy value, not two bytes of 96 000.
        let torn = Welcome::decode(&enc[..82]).unwrap();
        assert_eq!(torn.audio_codec, AUDIO_CODEC_PCM, "the codec byte survived");
        assert_eq!(torn.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(torn.audio_frame_us, 0);

        // Unsupported depth → 16 so a 0xD3 unpack never walks the wrong stride.
        let mut bad_depth = enc.clone();
        bad_depth[84] = 32;
        assert_eq!(Welcome::decode(&bad_depth).unwrap().audio_bits, BITS_16);
        // Rate is verbatim. Clamping 44100 to 48000 would mislabel the stream.
        let mut odd_rate = enc.clone();
        odd_rate[80..84].copy_from_slice(&44_100u32.to_le_bytes());
        assert_eq!(Welcome::decode(&odd_rate).unwrap().audio_rate_hz, 44_100);
    }

    /// `host_caps2` past the audio block: presence forces earlier placeholders to their
    /// absence-defaults; older host omit → 0.
    #[test]
    fn welcome_host_caps2_wire_under_both_ciphers() {
        let base = Welcome {
            abi_version: 1,
            udp_port: 1,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            },
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 10,
                max_data_per_block: 4096,
            },
            shard_payload: 1408,
            encrypt: true,
            key: [7; 16],
            salt: [3; 4],
            frames: 0,
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 20_000,
            bit_depth: 8,
            color: ColorInfo::SDR_BT709,
            chroma_format: CHROMA_IDC_420,
            audio_channels: 2,
            codec: CODEC_HEVC,
            host_caps: HOST_CAP_GAMEPAD_STATE,
            mgmt_port: 0,
            grants: super::super::access::GRANT_ALL,
            expires_in_secs: 0,
            cipher: CIPHER_AES_128_GCM,
            key_chacha: None,
            audio_codec: AUDIO_CODEC_OPUS,
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
            audio_frame_us: 0,
            host_caps2: 0,
        };
        // Zero is not emitted: Welcome stays 68 bytes.
        assert_eq!(base.encode().len(), 68);

        // AES: placeholders forced, byte at 87.
        let marked = Welcome {
            host_caps2: HOST_CAP2_REPEAT_MARK,
            ..base
        };
        let enc = marked.encode();
        assert_eq!(enc.len(), 88);
        let got = Welcome::decode(&enc).unwrap();
        assert_eq!(got, marked);
        // Placeholders decode as absence: not hi-res, not empty grants, not a mgmt port.
        assert_eq!(got.audio_codec, AUDIO_CODEC_OPUS);
        assert_eq!(got.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(got.grants, super::super::access::GRANT_ALL);
        assert_eq!(got.mgmt_port, 0);

        // ChaCha: byte at 119, total 120.
        let chacha = Welcome {
            cipher: CIPHER_CHACHA20_POLY1305,
            key_chacha: Some([9; 32]),
            host_caps2: HOST_CAP2_REPEAT_MARK,
            ..base
        };
        let enc = chacha.encode();
        assert_eq!(enc.len(), 120);
        assert_eq!(Welcome::decode(&enc).unwrap(), chacha);

        // Shorter wire → 0, never an error.
        assert_eq!(Welcome::decode(&base.encode()).unwrap().host_caps2, 0);
    }

    /// Hello trailing fields have placeholders except `display_hdr` (fixed 28-byte block,
    /// remaining-length). That caps the post-HDR tail at 27 bytes.
    #[test]
    fn hello_hires_audio_request_roundtrip_and_back_compat() {
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
            audio_rate_hz: SAMPLE_RATE_HZ,
            audio_bits: BITS_16,
        };
        // Legacy request is still 26 bytes.
        assert_eq!(base.encode().len(), 26);
        assert_eq!(Hello::decode(&base.encode()).unwrap(), base);

        // 26 + 6 placeholders + client_caps 1 + max_shard_payload 2 + rate 4.
        // No HDR, no depth byte (16-bit is default and last).
        let rate_only = Hello {
            audio_rate_hz: 96_000,
            ..base.clone()
        };
        let enc = rate_only.encode();
        assert_eq!(enc.len(), 26 + 6 + 1 + 2 + 4);
        assert_eq!(&enc[26..28], &[0, 0], "name + launch length placeholders");
        assert_eq!(enc[28], 0, "video_caps placeholder");
        assert_eq!(enc[29], 2, "audio_channels placeholder = stereo");
        assert_eq!(
            &enc[30..32],
            &[0, 0],
            "video_codecs + preferred placeholders"
        );
        assert_eq!(enc[32], 0, "client_caps placeholder");
        assert_eq!(
            &enc[33..35],
            &0u16.to_le_bytes(),
            "max_shard_payload placeholder"
        );
        assert_eq!(&enc[35..39], &96_000u32.to_le_bytes());
        let dec = Hello::decode(&enc).unwrap();
        assert_eq!(dec, rate_only);
        assert_eq!(
            dec.client_caps, 0,
            "the forced placeholder reads as absence"
        );
        assert_eq!(dec.max_shard_payload, 0, "…and so does this one");
        assert_eq!(dec.audio_bits, BITS_16, "absent depth → the legacy 16");

        // Last field forces the rate out as 48 000, not struct `0` — otherwise 16-bit and
        // 24-bit at the default rate would disagree about where the depth byte lives.
        let bits_only = Hello {
            audio_bits: BITS_24,
            ..base.clone()
        };
        let benc = bits_only.encode();
        assert_eq!(benc.len(), 26 + 6 + 1 + 2 + 4 + 1);
        assert_eq!(&benc[35..39], &SAMPLE_RATE_HZ.to_le_bytes(), "forced rate");
        assert_eq!(benc[39], BITS_24);
        assert_eq!(Hello::decode(&benc).unwrap(), bits_only);

        // Capable and opted-in, both parameters set.
        let req = Hello {
            client_caps: CLIENT_CAP_AUDIO_HIRES,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            ..base.clone()
        };
        let renc = req.encode();
        assert_eq!(Hello::decode(&renc).unwrap(), req);
        assert_eq!(renc[32], CLIENT_CAP_AUDIO_HIRES);
        // 8-byte tail < HDR_META_BODY_LEN, so remaining-length says no HDR.
        assert_eq!(Hello::decode(&renc).unwrap().display_hdr, None);

        // Post-HDR tail must stay under HDR_META_BODY_LEN (8 spent, 19 free) or a
        // Hello without HDR is read as one with.
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
            client_caps: CLIENT_CAP_AUDIO_HIRES | CLIENT_CAP_CURSOR,
            max_shard_payload: 8908,
            audio_rate_hz: 96_000,
            audio_bits: BITS_24,
            ..base.clone()
        };
        let fenc = full.encode();
        let post_hdr = fenc.len() - (26 + 6 + HDR_META_BODY_LEN);
        assert_eq!(
            post_hdr, 8,
            "client_caps 1 + max_shard_payload 2 + audio_rate_hz 4 + audio_bits 1"
        );
        assert!(
            post_hdr < HDR_META_BODY_LEN,
            "the post-display_hdr tail must stay under {HDR_META_BODY_LEN} bytes — \
             at or past it, a Hello WITHOUT an HDR block is read as one WITH"
        );
        assert_eq!(Hello::decode(&fenc).unwrap(), full);

        // Omit or truncate before the audio fields → 48 kHz / 16-bit.
        assert_eq!(
            Hello::decode(&base.encode()).unwrap().audio_rate_hz,
            SAMPLE_RATE_HZ
        );
        assert_eq!(Hello::decode(&base.encode()).unwrap().audio_bits, BITS_16);
        let pre_audio = Hello::decode(&renc[..35]).unwrap();
        assert_eq!(pre_audio.audio_rate_hz, SAMPLE_RATE_HZ);
        assert_eq!(pre_audio.audio_bits, BITS_16);
        assert_eq!(pre_audio.max_shard_payload, 0);
        // Torn rate is never half a rate.
        assert_eq!(
            Hello::decode(&renc[..37]).unwrap().audio_rate_hz,
            SAMPLE_RATE_HZ
        );
        // Unsupported depth → 16. A request is only a request.
        let mut bad_depth = renc.clone();
        bad_depth[39] = 32;
        assert_eq!(Hello::decode(&bad_depth).unwrap().audio_bits, BITS_16);
        assert!(depth_is_supported(Hello::decode(&renc).unwrap().audio_bits));
    }
}
