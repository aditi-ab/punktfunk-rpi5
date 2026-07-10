//! The QUIC-datagram side planes, demultiplexed by their first byte (0xC9–0xCF):
//! audio, rumble, mic uplink, rich input, HID output, HDR metadata, host timing.

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

pub(super) const RICH_TOUCHPAD: u8 = 0x01;
pub(super) const RICH_MOTION: u8 = 0x02;
pub(super) const RICH_TOUCHPAD_EX: u8 = 0x03;

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

/// Wire length of an [`HdrMeta`] body (no tag byte): 6×u16 primaries + 2×u16 white + 2×u32
/// luminance + 2×u16 CLL/FALL = 28 bytes. Shared by the [`HDR_META_MAGIC`] datagram (which
/// prefixes the tag) and the `Hello::display_hdr` trailing field (which carries the bare body).
pub const HDR_META_BODY_LEN: usize = 12 + 4 + 8 + 4;

/// Wire length of an [`HDR_META_MAGIC`] datagram: tag + body = 29 bytes.
const HDR_META_LEN: usize = 1 + HDR_META_BODY_LEN;

/// Append `m`'s [`HDR_META_BODY_LEN`]-byte wire body (LE, no tag byte) to `b`.
pub fn write_hdr_meta_body(m: &HdrMeta, b: &mut Vec<u8>) {
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
}

/// Read an [`HdrMeta`] from its wire body (no tag byte). The caller guarantees `b` holds at least
/// [`HDR_META_BODY_LEN`] bytes (both callers slice with an exact-length, bounds-checked `get`).
pub fn read_hdr_meta_body(b: &[u8]) -> HdrMeta {
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    HdrMeta {
        display_primaries: [
            [u16at(0), u16at(2)],
            [u16at(4), u16at(6)],
            [u16at(8), u16at(10)],
        ],
        white_point: [u16at(12), u16at(14)],
        max_display_mastering_luminance: u32at(16),
        min_display_mastering_luminance: u32at(20),
        max_cll: u16at(24),
        max_fall: u16at(26),
    }
}

/// Encode an [`HdrMeta`] into a [`HDR_META_MAGIC`] datagram.
pub fn encode_hdr_meta_datagram(m: &HdrMeta) -> Vec<u8> {
    let mut b = Vec::with_capacity(HDR_META_LEN);
    b.push(HDR_META_MAGIC);
    write_hdr_meta_body(m, &mut b);
    b
}

/// Parse a [`HDR_META_MAGIC`] datagram → [`HdrMeta`]. `None` on bad tag or a short/truncated buffer
/// (every attacker-controlled field is bounds-checked by the fixed length before any read).
pub fn decode_hdr_meta_datagram(b: &[u8]) -> Option<HdrMeta> {
    if b.len() < HDR_META_LEN || b[0] != HDR_META_MAGIC {
        return None;
    }
    Some(read_hdr_meta_body(&b[1..]))
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
