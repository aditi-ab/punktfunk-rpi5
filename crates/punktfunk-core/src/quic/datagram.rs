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

/// Redundant audio datagram, host → client: the [`AUDIO_MAGIC`] plane plus a copy of the PREVIOUS
/// frame, so a single lost datagram is *reconstructed* rather than concealed.
///
/// `[0xD2][u32 seq LE][u64 pts_ns LE][u16 primary_len LE][primary opus][previous opus]`
///
/// **Why this and not Opus in-band FEC.** LBRR is a SILK-layer feature: the desktop-audio encoder
/// runs `RESTRICTED_LOWDELAY` (CELT-only) at 5 ms frames, which is below SILK's 10 ms minimum, so
/// `set_inband_fec(true)` on that encoder is a no-op. Nothing in libopus can protect this plane —
/// the redundancy has to be at the application layer. (The mic uplink is a different encoder, VoIP
/// mode at 10 ms, and *does* use real in-band FEC.)
///
/// **Why it costs no latency.** The copy rides the SUCCESSOR of the frame it protects, and the
/// client is already holding 15–90 ms of de-jitter buffer — far more than the 5 ms the successor
/// takes to arrive. So the recovery happens inside slack that already exists.
///
/// The previous frame's sequence is implicitly `seq - 1`; a host with nothing to duplicate yet
/// (the first frame of a session, or straight after a capture reopen) simply sends an empty tail,
/// which decodes to `None`.
///
/// Sent ONLY when the client advertised [`CLIENT_CAP_AUDIO_RED`](super::caps::CLIENT_CAP_AUDIO_RED)
/// and the host answered [`HOST_CAP_AUDIO_RED`](super::caps::HOST_CAP_AUDIO_RED) — the
/// capable-and-agreed handshake the cursor and 4:4:4 planes already use. Every other session keeps
/// the plain [`AUDIO_MAGIC`] wire byte-for-byte.
///
/// NB `0xD1` is deliberately skipped: the DualSense pad-audio program has reserved it for the
/// per-pad audio plane.
pub const AUDIO_RED_MAGIC: u8 = 0xD2;

/// Fixed header length of an [`AUDIO_RED_MAGIC`] datagram (tag + seq + pts + primary length).
pub const AUDIO_RED_HEADER: usize = 1 + 4 + 8 + 2;

/// Encode a redundant audio datagram. `prev` is the immediately-preceding frame's Opus payload
/// (empty when there is none yet).
pub fn encode_audio_red_datagram(seq: u32, pts_ns: u64, opus: &[u8], prev: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(AUDIO_RED_HEADER + opus.len() + prev.len());
    b.push(AUDIO_RED_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    // A frame longer than u16::MAX cannot occur (5 ms of Opus is tens of bytes; the buffer the
    // encoder writes into is 4 KiB) — but truncating silently would desync the split, so clamp
    // the redundancy off instead of the primary.
    let primary_len = u16::try_from(opus.len()).unwrap_or(u16::MAX);
    b.extend_from_slice(&primary_len.to_le_bytes());
    b.extend_from_slice(opus);
    if opus.len() == primary_len as usize {
        b.extend_from_slice(prev);
    }
    b
}

/// Parse a redundant audio datagram → `(seq, pts_ns, primary, previous)`. `previous` is `None`
/// when the host had nothing to duplicate. `None` overall on bad tag/length, including a
/// `primary_len` that overruns the datagram (a truncated or hostile packet must not panic).
///
/// The tuple shape deliberately mirrors [`decode_audio_datagram`] (one extra slot for the
/// redundant copy) so the two planes read the same at every call site; a named struct here would
/// be the odd one out on this module's decode surface, and cbindgen would then have to be taught
/// to skip it.
#[allow(clippy::type_complexity)]
pub fn decode_audio_red_datagram(b: &[u8]) -> Option<(u32, u64, &[u8], Option<&[u8]>)> {
    if b.len() < AUDIO_RED_HEADER || b[0] != AUDIO_RED_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    let primary_len = u16::from_le_bytes(b[13..15].try_into().unwrap()) as usize;
    let rest = &b[AUDIO_RED_HEADER..];
    if primary_len > rest.len() {
        return None; // truncated: the split point is outside the datagram
    }
    let (primary, prev) = rest.split_at(primary_len);
    Some((seq, pts_ns, primary, (!prev.is_empty()).then_some(prev)))
}

/// Legacy rumble datagram (v1), host → client: `[0xCA][u16 pad LE][u16 low LE][u16 high LE]`.
/// Force-feedback state for pad `pad` (0xFFFF amplitudes, 0/0 = stop) as *level-triggered* state
/// — it persists until superseded, which is why the host re-sends it periodically as its loss
/// heal. New hosts emit the self-terminating [`encode_rumble_datagram_v2`] instead; this is kept
/// for the loopback tests and as the wire an old host still speaks (a new client decodes both via
/// [`decode_rumble_envelope`]).
pub fn encode_rumble_datagram(pad: u16, low: u16, high: u16) -> [u8; 7] {
    let mut b = [0u8; 7];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b
}

/// Wire length of a v1 (legacy, level) rumble datagram.
pub const RUMBLE_V1_LEN: usize = 7;
/// Wire length of a v2 (envelope) rumble datagram — the v1 body plus a `[u8 seq][u16 ttl_ms LE]`
/// tail. Decoders are length-tolerant (see [`decode_rumble_envelope`]): an old client reads the
/// first 7 bytes as a plain level and ignores the tail, so no wire-version bump is needed — the
/// same dual-size idiom the HDR-luminance `AddRequest` tail uses.
pub const RUMBLE_V2_LEN: usize = 10;

/// Rumble envelope datagram (v2), host → client:
/// `[0xCA][u16 pad LE][u16 low LE][u16 high LE][u8 seq][u16 ttl_ms LE]`.
///
/// A *self-terminating* force-feedback command: the level is authorized for at most `ttl_ms`, so
/// a rumble the host stops renewing (or a host that dies) silences on its own — "stuck forever"
/// is inexpressible on the wire. `seq` is a per-pad wrapping counter (bumped on every send,
/// changes *and* renewals) compared with [`GamepadSnapshot::seq_newer`](crate::input::GamepadSnapshot::seq_newer)
/// so a reordered stale start can't re-light the motors after a stop. Renewals fully replace the
/// prior envelope's deadline; they never stack. An explicit stop is still `low == high == 0` sent
/// immediately (expiry is the safety net, never the stop mechanism).
pub fn encode_rumble_datagram_v2(pad: u16, low: u16, high: u16, seq: u8, ttl_ms: u16) -> [u8; 10] {
    let mut b = [0u8; RUMBLE_V2_LEN];
    b[0] = RUMBLE_MAGIC;
    b[1..3].copy_from_slice(&pad.to_le_bytes());
    b[3..5].copy_from_slice(&low.to_le_bytes());
    b[5..7].copy_from_slice(&high.to_le_bytes());
    b[7] = seq;
    b[8..10].copy_from_slice(&ttl_ms.to_le_bytes());
    b
}

/// The self-termination tail of a v2 rumble envelope (see [`encode_rumble_datagram_v2`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleEnvelope {
    /// Per-pad wrapping send counter — the reorder gate (see [`decode_rumble_envelope`]).
    pub seq: u8,
    /// How long, in ms, this envelope authorizes the stated level before the client must silence.
    pub ttl_ms: u16,
}

/// A decoded rumble update. `envelope` is `None` for a legacy 7-byte datagram (an old host, which
/// has no seq/ttl — the client applies its own staleness policy), `Some` for a v2 envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleUpdate {
    pub pad: u16,
    pub low: u16,
    pub high: u16,
    pub envelope: Option<RumbleEnvelope>,
}

/// Parse a rumble datagram → `(pad, low, high)`, tolerating (and ignoring) a v2 envelope tail.
/// `None` on bad tag/length. Kept for callers that only need the level (the probe, the loopback
/// assertions); clients that honor TTL use [`decode_rumble_envelope`].
pub fn decode_rumble_datagram(b: &[u8]) -> Option<(u16, u16, u16)> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    Some((u16at(1), u16at(3), u16at(5)))
}

/// Parse a rumble datagram → [`RumbleUpdate`], detecting the v2 envelope tail by length. A
/// `>= RUMBLE_V2_LEN` buffer carries `seq`/`ttl_ms`; a 7..RUMBLE_V2_LEN buffer is a legacy level
/// (`envelope: None`) — the same tolerance as an old client would apply, so a torn/short tail
/// degrades to a level rather than dropping. `None` on bad tag/length.
pub fn decode_rumble_envelope(b: &[u8]) -> Option<RumbleUpdate> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let envelope = (b.len() >= RUMBLE_V2_LEN).then(|| RumbleEnvelope {
        seq: b[7],
        ttl_ms: u16::from_le_bytes([b[8], b[9]]),
    });
    Some(RumbleUpdate {
        pad: u16at(1),
        low: u16at(3),
        high: u16at(5),
        envelope,
    })
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
pub(super) const RICH_HID_REPORT: u8 = 0x04;
/// Claimed by the stylus plane ([`PenBatch`](super::pen::PenBatch)), which is NOT a
/// [`RichInput`] variant: rich input is pad-indexed controller state consumed by the gamepad
/// backends, while a pen batch routes to the (P1) tablet injector — so it gets its own decoder
/// and [`RichInput::decode`] keeps returning `None` here (= the documented unknown-kind drop
/// on a pre-pen host). Registered in this list so 0xCC kind bytes stay unique.
pub(super) const RICH_PEN: u8 = 0x05;

/// Longest raw HID report a [`RichInput::HidReport`] / [`HidOutput::HidRaw`] can carry — the
/// 64-byte interrupt/feature report size every Valve controller uses (Triton input reports are
/// 46–54 bytes; feature and output reports are at most 64).
pub const HID_REPORT_MAX: usize = 64;

/// A rich client→host controller input beyond the fixed [`InputEvent`](crate::input::InputEvent):
/// the DualSense touchpad and motion sensors. `pad` is the gamepad index. Wire form is
/// `[0xCC][kind][fields…]` — variable-length and kind-tagged (forward-compatible: an unknown
/// kind decodes to `None` and is dropped). Kind `0x05` on this plane is the stylus batch
/// ([`PenBatch`](super::pen::PenBatch)) with its own decoder — see [`RICH_PEN`].
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
    /// One raw HID input report from a client-captured controller, forwarded verbatim for a
    /// host backend that mirrors the physical device as-is (the Steam Controller 2 / Triton
    /// passthrough — [`GamepadPref::SteamController2`](crate::config::GamepadPref)). `data[..len]`
    /// is exactly what the device produced on its interrupt endpoint / GATT notify, report-id
    /// byte first (`0x42`/`0x45`/`0x47` state, `0x43` battery, …). Best-effort like the rest of
    /// the plane: state reports are idempotent snapshots at the device's own rate, so a lost
    /// datagram self-heals on the next one. Fixed-size body keeps the type `Copy` on a path that
    /// runs at the controller's report rate.
    HidReport {
        pad: u8,
        len: u8,
        data: [u8; HID_REPORT_MAX],
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
            RichInput::HidReport { pad, len, ref data } => {
                let len = (len as usize).min(HID_REPORT_MAX);
                out.extend_from_slice(&[RICH_HID_REPORT, pad, len as u8]);
                out.extend_from_slice(&data[..len]);
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
            RICH_HID_REPORT if b.len() >= 4 => {
                // Every byte read below is bounded: `len` is clamped to the fixed body size AND
                // to what the buffer actually holds (a torn datagram truncates, never over-reads).
                let len = (b[3] as usize).min(HID_REPORT_MAX).min(b.len() - 4);
                let mut data = [0u8; HID_REPORT_MAX];
                data[..len].copy_from_slice(&b[4..4 + len]);
                Some(RichInput::HidReport {
                    pad: b[2],
                    len: len as u8,
                    data,
                })
            }
            _ => None,
        }
    }
}

const HIDOUT_LED: u8 = 0x01;
const HIDOUT_PLAYER_LEDS: u8 = 0x02;
const HIDOUT_TRIGGER: u8 = 0x03;
const HIDOUT_TRACKPAD_HAPTIC: u8 = 0x04;
const HIDOUT_HID_RAW: u8 = 0x05;

/// [`HidOutput::HidRaw`] `kind`: an OUTPUT report — what the host's hidraw client wrote with
/// `write()`/`SDL_hid_write` (Triton rumble `0x80`, haptic pulse `0x81`, …). The client replays
/// it on the physical device's interrupt-OUT endpoint / GATT write.
pub const HID_RAW_OUTPUT: u8 = 0;
/// [`HidOutput::HidRaw`] `kind`: a FEATURE report — what the host's hidraw client sent with
/// `SET_REPORT` (`SDL_hid_send_feature_report`: lizard mode, IMU enable, settings). The client
/// replays it as a USB `SET_REPORT(Feature)` control transfer / GATT feature write.
pub const HID_RAW_FEATURE: u8 = 1;

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
    ///
    /// **STAGED SCAFFOLDING — deliberately unreachable today, do not delete.** Nothing on the host
    /// produces this variant and no client renders it; it codes/decodes and round-trips in tests
    /// and nothing else. It stays because `HIDOUT_TRACKPAD_HAPTIC` is an allocated tag on a
    /// SHIPPED wire: removing the variant would not reclaim the tag (a future peer could still
    /// send it), it would only lose the decoder that keeps such a datagram from being mistaken
    /// for something else. The producer is the Steam Controller coil path; the renderer is the
    /// client-side coil write. Wire up either half and this becomes live with no format change.
    TrackpadHaptic {
        pad: u8,
        side: u8,
        amplitude: u16,
        period: u16,
        count: u16,
    },
    /// A raw report the host's hidraw consumer (Steam) wrote to an as-is passthrough pad
    /// ([`RichInput::HidReport`]'s reverse direction), for the client to replay verbatim on the
    /// physical device. `kind` is [`HID_RAW_OUTPUT`] or [`HID_RAW_FEATURE`]; `data` is the full
    /// report, id byte first, at most [`HID_REPORT_MAX`] bytes. Best-effort is sound here by the
    /// device protocol's own design: Triton rumble is re-sent every ~40 ms against a ~50 ms
    /// hardware safety timeout, and settings (lizard/IMU) are refreshed every ~3 s against the
    /// firmware watchdog — a lost datagram heals on the next refresh.
    HidRaw { pad: u8, kind: u8, data: Vec<u8> },
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
            HidOutput::HidRaw { pad, kind, data } => {
                out.extend_from_slice(&[HIDOUT_HID_RAW, *pad, *kind]);
                out.extend_from_slice(&data[..data.len().min(HID_REPORT_MAX)]);
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
            HIDOUT_HID_RAW if b.len() >= 5 => Some(HidOutput::HidRaw {
                pad: b[2],
                kind: b[3],
                // Bounded: at most HID_REPORT_MAX bytes are kept from the (attacker-sized) tail.
                data: b[4..b.len().min(4 + HID_REPORT_MAX)].to_vec(),
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
    /// Per-stage split of `host_us` (latency plan T0.1). `None` from a host that predates the
    /// extended datagram — the 0xCF wire is APPEND-extensible (decode reads the 13-byte prefix
    /// and takes the stage tail only when present), so no capability bit is needed in either
    /// direction: old client + new host reads the prefix, new client + old host gets `None`.
    pub stages: Option<HostStages>,
    /// Phase-lock ACK (design/phase-locked-capture.md): the capture-tick hold the host is
    /// currently applying, ns. Rides the same append-extensible tail (after the stages block):
    /// `None` from a pre-phase-lock host or one sending the shorter forms. The client's
    /// presenter compares it against its own requested correction to see the loop close.
    pub applied_phase_ns: Option<i32>,
}

/// The extended 0xCF's per-stage split of [`HostTiming::host_us`], all µs against the same
/// capture anchor. The stages tile the host pipeline as
/// `host_us = queue + encode + (seal/FEC + channel-wait = the residual) + pace`, so the client
/// derives the residual as `host_us − queue_us − encode_us − pace_us` — no fifth field needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostStages {
    /// Capture delivery → encoder submit (the capture ring / channel-queue age; 0 for
    /// re-encoded hold frames, which never waited).
    pub queue_us: u32,
    /// Encoder submit → bitstream ready (scheduling wait + ASIC time).
    pub encode_us: u32,
    /// Paced send: first byte handed to the socket → last packet sent (the microburst spread).
    pub pace_us: u32,
}

/// Wire length of a legacy [`HOST_TIMING_MAGIC`] datagram: tag + u64 pts + u32 µs = 13 bytes.
const HOST_TIMING_LEN: usize = 1 + 8 + 4;
/// Wire length with the [`HostStages`] tail appended: + 3 × u32 = 25 bytes.
const HOST_TIMING_STAGES_LEN: usize = HOST_TIMING_LEN + 12;
/// Wire length with the phase-lock ACK appended after the stages: + i32 = 29 bytes. The tail
/// discipline holds: each form is a strict prefix of the next, so every reader takes what it
/// knows and ignores the rest.
const HOST_TIMING_PHASE_LEN: usize = HOST_TIMING_STAGES_LEN + 4;

/// Encode a [`HostTiming`] into a [`HOST_TIMING_MAGIC`] datagram (extended form when `stages`
/// is set — an older client parses the prefix and ignores the tail).
pub fn encode_host_timing_datagram(t: &HostTiming) -> Vec<u8> {
    let mut b = Vec::with_capacity(HOST_TIMING_PHASE_LEN);
    b.push(HOST_TIMING_MAGIC);
    b.extend_from_slice(&t.pts_ns.to_le_bytes());
    b.extend_from_slice(&t.host_us.to_le_bytes());
    if let Some(s) = &t.stages {
        b.extend_from_slice(&s.queue_us.to_le_bytes());
        b.extend_from_slice(&s.encode_us.to_le_bytes());
        b.extend_from_slice(&s.pace_us.to_le_bytes());
        // The phase ACK only ever rides AFTER a stages tail — a prefix-discipline wire can't
        // express "phase but no stages", and every host new enough to phase-lock sends stages.
        if let Some(p) = t.applied_phase_ns {
            b.extend_from_slice(&p.to_le_bytes());
        }
    }
    b
}

/// Parse a [`HOST_TIMING_MAGIC`] datagram → [`HostTiming`]. `None` on bad tag or a short buffer
/// (the fixed lengths bound every read before it happens). A datagram carrying only the 13-byte
/// prefix (an older host) yields `stages: None`.
pub fn decode_host_timing_datagram(b: &[u8]) -> Option<HostTiming> {
    if b.len() < HOST_TIMING_LEN || b[0] != HOST_TIMING_MAGIC {
        return None;
    }
    let stages = (b.len() >= HOST_TIMING_STAGES_LEN).then(|| HostStages {
        queue_us: u32::from_le_bytes(b[13..17].try_into().unwrap()),
        encode_us: u32::from_le_bytes(b[17..21].try_into().unwrap()),
        pace_us: u32::from_le_bytes(b[21..25].try_into().unwrap()),
    });
    let applied_phase_ns = (b.len() >= HOST_TIMING_PHASE_LEN)
        .then(|| i32::from_le_bytes(b[25..29].try_into().unwrap()));
    Some(HostTiming {
        pts_ns: u64::from_le_bytes(b[1..9].try_into().unwrap()),
        host_us: u32::from_le_bytes(b[9..13].try_into().unwrap()),
        stages,
        applied_phase_ns,
    })
}

/// Cursor-state datagram tag, host → client (design/remote-desktop-sweep.md M2). Next tag after
/// [`HOST_TIMING_MAGIC`]. Sent once per captured frame while the cursor channel is negotiated
/// ([`CLIENT_CAP_CURSOR`](super::caps::CLIENT_CAP_CURSOR) ∧
/// [`HOST_CAP_CURSOR`](super::caps::HOST_CAP_CURSOR)) — per-frame resend makes the plane
/// self-healing under loss (latest-wins, no refresh timer). The bitmap itself rides the
/// reliable control stream ([`CursorShape`](super::control::CursorShape)); this 14-byte
/// datagram only moves/hides the pointer.
pub const CURSOR_STATE_MAGIC: u8 = 0xD0;

/// [`CursorState::flags`] bit: the host cursor is visible.
pub const CURSOR_VISIBLE: u8 = 0x01;
/// [`CursorState::flags`] bit: a host app captured/hid the pointer — the client SHOULD run
/// relative/captured (M3 auto-flip; advisory, user override always wins).
pub const CURSOR_RELATIVE_HINT: u8 = 0x02;

/// Per-frame host-cursor state (position, visibility, mode hint). `x`/`y` are the pointer
/// position (hotspot point, not bitmap top-left) in the host OUTPUT's pixel space — the same
/// space the video mode describes, so the client maps through its letterbox exactly like it
/// maps touches, in reverse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorState {
    /// The [`CursorShape`](super::control::CursorShape) serial this state refers to. A client
    /// that has no cached shape for it keeps its previous cursor until the (reliable) shape
    /// message lands — at worst one control-stream RTT of stale shape, never a wrong position.
    pub serial: u32,
    /// Bitfield of [`CURSOR_VISIBLE`] / [`CURSOR_RELATIVE_HINT`].
    pub flags: u8,
    pub x: i32,
    pub y: i32,
}

impl CursorState {
    pub fn visible(&self) -> bool {
        self.flags & CURSOR_VISIBLE != 0
    }
    pub fn relative_hint(&self) -> bool {
        self.flags & CURSOR_RELATIVE_HINT != 0
    }
}

/// Wire length of a [`CURSOR_STATE_MAGIC`] datagram: tag + u32 serial + flags + 2 × i32 = 14.
const CURSOR_STATE_LEN: usize = 1 + 4 + 1 + 8;

/// Encode a [`CursorState`] into a [`CURSOR_STATE_MAGIC`] datagram.
pub fn encode_cursor_state_datagram(s: &CursorState) -> Vec<u8> {
    let mut b = Vec::with_capacity(CURSOR_STATE_LEN);
    b.push(CURSOR_STATE_MAGIC);
    b.extend_from_slice(&s.serial.to_le_bytes());
    b.push(s.flags);
    b.extend_from_slice(&s.x.to_le_bytes());
    b.extend_from_slice(&s.y.to_le_bytes());
    b
}

/// Parse a [`CURSOR_STATE_MAGIC`] datagram → [`CursorState`]. `None` on bad tag or a short
/// buffer (the fixed length bounds every read before it happens; a longer buffer is tolerated
/// for append-extension, like 0xCF).
pub fn decode_cursor_state_datagram(b: &[u8]) -> Option<CursorState> {
    if b.len() < CURSOR_STATE_LEN || b[0] != CURSOR_STATE_MAGIC {
        return None;
    }
    Some(CursorState {
        serial: u32::from_le_bytes(b[1..5].try_into().unwrap()),
        flags: b[5],
        x: i32::from_le_bytes(b[6..10].try_into().unwrap()),
        y: i32::from_le_bytes(b[10..14].try_into().unwrap()),
    })
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

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
            stages: None,
            applied_phase_ns: None,
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

        // Extended form (T0.1): the stage tail roundtrips; a truncated tail (an old host's 13-byte
        // datagram, or anything short of the full 25) degrades to `stages: None`, never a partial
        // read; the prefix fields stay identical in both forms (the append-extensibility contract).
        let ts = HostTiming {
            stages: Some(HostStages {
                queue_us: 900,
                encode_us: 3_100,
                pace_us: 2_500,
            }),
            ..t
        };
        let ds = encode_host_timing_datagram(&ts);
        assert_eq!(ds.len(), 25);
        assert_eq!(
            &ds[..13],
            &d[..13],
            "prefix is byte-identical to the legacy form"
        );
        assert_eq!(decode_host_timing_datagram(&ds), Some(ts));
        for n in 13..ds.len() {
            assert_eq!(
                decode_host_timing_datagram(&ds[..n]),
                Some(t),
                "partial stage tail ({n} B) must degrade to the legacy decode"
            );
        }

        // Phase-ACK form (design/phase-locked-capture.md): strict-prefix discipline holds a
        // third time — 29 B roundtrips, 25..28 degrade to the stages form, the prefix is
        // byte-identical, and a phase without stages is unencodable by construction.
        let tp = HostTiming {
            applied_phase_ns: Some(-2_750_000),
            ..ts
        };
        let dp = encode_host_timing_datagram(&tp);
        assert_eq!(dp.len(), 29);
        assert_eq!(&dp[..25], &ds[..25], "stages form is a strict prefix");
        assert_eq!(decode_host_timing_datagram(&dp), Some(tp));
        for n in 25..dp.len() {
            assert_eq!(
                decode_host_timing_datagram(&dp[..n]),
                Some(ts),
                "partial phase tail ({n} B) must degrade to the stages decode"
            );
        }
        let no_stages = HostTiming {
            stages: None,
            applied_phase_ns: Some(1),
            ..t
        };
        assert_eq!(
            encode_host_timing_datagram(&no_stages).len(),
            13,
            "phase without stages must encode as the legacy form (prefix discipline)"
        );
    }

    #[test]
    fn audio_datagram_roundtrip() {
        let opus = [0x42u8; 97];
        let d = encode_audio_red_datagram(7, 42, &opus, &[]);
        assert_eq!(d[0], AUDIO_RED_MAGIC);
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
    fn audio_red_datagram_roundtrip() {
        let cur = [0x42u8; 97];
        let prev = [0x37u8; 88];
        let d = encode_audio_red_datagram(7, 1_000_000_123, &cur, &prev);
        assert_eq!(d[0], AUDIO_RED_MAGIC);
        let (seq, pts, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!((seq, pts), (7, 1_000_000_123));
        assert_eq!(primary, cur);
        assert_eq!(previous, Some(&prev[..]));

        // No predecessor yet (first frame of a session / after a capture reopen).
        let d = encode_audio_red_datagram(0, 5, &cur, &[]);
        let (_, _, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!(primary, cur);
        assert_eq!(
            previous, None,
            "an empty tail must decode as absent, not as a zero-length frame"
        );

        // Frames of equal length must still split at the right place — the length prefix is the
        // only thing that can tell them apart.
        let a = [1u8; 64];
        let b = [2u8; 64];
        let d = encode_audio_red_datagram(9, 0, &a, &b);
        let (_, _, primary, previous) = decode_audio_red_datagram(&d).unwrap();
        assert_eq!(primary, a);
        assert_eq!(previous, Some(&b[..]));
    }

    /// A truncated or hostile `0xD2` must be rejected, never panic — the split point comes off
    /// the wire, so an over-long `primary_len` is the obvious attack on `split_at`.
    #[test]
    fn audio_red_datagram_rejects_bad_input() {
        let d = encode_audio_red_datagram(1, 2, &[0xAAu8; 30], &[0xBBu8; 20]);
        for n in 0..AUDIO_RED_HEADER {
            assert!(decode_audio_red_datagram(&d[..n]).is_none(), "len {n}");
        }
        // primary_len larger than the datagram: must be refused, not sliced.
        let mut bad = d.clone();
        bad[13..15].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(decode_audio_red_datagram(&bad).is_none());
        // Wrong tag.
        let mut wrong = d.clone();
        wrong[0] = AUDIO_MAGIC;
        assert!(decode_audio_red_datagram(&wrong).is_none());
    }

    /// The two audio planes must not alias each other or any neighbouring plane: a client
    /// demultiplexes purely on the first byte.
    #[test]
    fn audio_red_tag_is_disjoint() {
        for other in [
            AUDIO_MAGIC,
            RUMBLE_MAGIC,
            MIC_MAGIC,
            RICH_INPUT_MAGIC,
            HIDOUT_MAGIC,
            HDR_META_MAGIC,
            HOST_TIMING_MAGIC,
            CURSOR_STATE_MAGIC,
            crate::input::INPUT_MAGIC,
        ] {
            assert_ne!(AUDIO_RED_MAGIC, other);
        }
        let red = encode_audio_red_datagram(1, 2, &[9u8; 40], &[8u8; 40]);
        assert!(
            decode_audio_datagram(&red).is_none(),
            "0xC9 must not accept a 0xD2"
        );
        let plain = encode_audio_datagram(1, 2, &[9u8; 40]);
        assert!(
            decode_audio_red_datagram(&plain).is_none(),
            "0xD2 must not accept a 0xC9"
        );
    }

    #[test]
    fn rumble_datagram_roundtrip() {
        let d = encode_rumble_datagram(1, 0x1234, 0xFFFF);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(decode_rumble_datagram(&d), Some((1, 0x1234, 0xFFFF)));
        assert!(decode_rumble_datagram(&d[..6]).is_none());
    }

    #[test]
    fn rumble_envelope_roundtrip_and_legacy_tolerance() {
        // v2 envelope round-trips seq + ttl.
        let d = encode_rumble_datagram_v2(2, 0x4000, 0x8000, 7, 400);
        assert_eq!(d[0], RUMBLE_MAGIC);
        assert_eq!(d.len(), RUMBLE_V2_LEN);
        assert_eq!(
            decode_rumble_envelope(&d),
            Some(RumbleUpdate {
                pad: 2,
                low: 0x4000,
                high: 0x8000,
                envelope: Some(RumbleEnvelope {
                    seq: 7,
                    ttl_ms: 400
                }),
            })
        );
        // The legacy level decoder reads a v2 datagram as a plain level — the tail is ignored, so an
        // old client running against a new host still renders the right amplitudes.
        assert_eq!(decode_rumble_datagram(&d), Some((2, 0x4000, 0x8000)));

        // A legacy 7-byte datagram (old host) decodes as a level with no envelope — a new client then
        // applies its own staleness policy.
        let v1 = encode_rumble_datagram(3, 0x1111, 0x2222);
        assert_eq!(
            decode_rumble_envelope(&v1),
            Some(RumbleUpdate {
                pad: 3,
                low: 0x1111,
                high: 0x2222,
                envelope: None,
            })
        );

        // A torn/short tail (8 or 9 bytes) is not a valid envelope — degrade to a level, never panic
        // or drop. (The host never emits these; a truncating middlebox might.)
        assert_eq!(
            decode_rumble_envelope(&d[..8]).map(|u| u.envelope),
            Some(None)
        );
        assert_eq!(
            decode_rumble_envelope(&d[..9]).map(|u| u.envelope),
            Some(None)
        );

        // Bad tag / too short → None on both decoders.
        assert!(decode_rumble_envelope(&d[..6]).is_none());
        let mut wrong_tag = d;
        wrong_tag[0] = AUDIO_MAGIC;
        assert!(decode_rumble_envelope(&wrong_tag).is_none());
    }

    #[test]
    fn rumble_envelope_seq_gate_drops_reordered_stale_start() {
        use crate::input::GamepadSnapshot;
        // The client-side reorder gate (reused verbatim from gamepad snapshots): a stale start
        // arriving after a stop must not re-light the motors.
        let stop = decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0, 0, 10, 0)).unwrap();
        let stale_start =
            decode_rumble_envelope(&encode_rumble_datagram_v2(0, 0x8000, 0x8000, 9, 400)).unwrap();
        let stop_seq = stop.envelope.unwrap().seq;
        let stale_seq = stale_start.envelope.unwrap().seq;
        // Nothing applied yet → the first update always passes.
        assert!(GamepadSnapshot::seq_newer(stop_seq, None));
        // The reordered older start does NOT supersede the stop.
        assert!(!GamepadSnapshot::seq_newer(stale_seq, Some(stop_seq)));
        // A genuine later renewal does.
        assert!(GamepadSnapshot::seq_newer(11, Some(stop_seq)));
        // Wraps: seq 1 supersedes 254.
        assert!(GamepadSnapshot::seq_newer(1, Some(254)));
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
        // A raw Triton state report rides the plane verbatim (as-is SC2 passthrough).
        let mut data = [0u8; HID_REPORT_MAX];
        data[0] = 0x42; // ID_TRITON_CONTROLLER_STATE
        for (i, b) in data.iter_mut().enumerate().take(46).skip(1) {
            *b = i as u8;
        }
        let raw = RichInput::HidReport {
            pad: 3,
            len: 46,
            data,
        };
        let d = raw.encode();
        assert_eq!(d.len(), 4 + 46); // tag + kind + pad + len + body — no fixed-array padding
        assert_eq!(RichInput::decode(&d), Some(raw));
        // A torn HidReport truncates to what arrived rather than over-reading (len clamps).
        assert_eq!(
            RichInput::decode(&d[..20]),
            Some(RichInput::HidReport {
                pad: 3,
                len: 16,
                data: {
                    let mut t = [0u8; HID_REPORT_MAX];
                    t[..16].copy_from_slice(&data[..16]);
                    t
                },
            })
        );
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
            // A raw Triton rumble output report (as-is SC2 passthrough, host→client).
            HidOutput::HidRaw {
                pad: 1,
                kind: HID_RAW_OUTPUT,
                data: vec![0x80, 0, 0, 0, 0x34, 0x12, 0, 0x78, 0x56, 0],
            },
            // A raw 64-byte feature report (lizard-off / IMU-enable settings write).
            HidOutput::HidRaw {
                pad: 0,
                kind: HID_RAW_FEATURE,
                data: {
                    let mut f = vec![0u8; HID_REPORT_MAX];
                    f[0] = 1; // Triton feature reports ride report id 1
                    f[1] = 0x87; // ID_SET_SETTINGS_VALUES
                    f
                },
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
    fn cursor_state_roundtrip() {
        for (flags, x, y) in [
            (CURSOR_VISIBLE, 0i32, 0i32),
            (CURSOR_VISIBLE | CURSOR_RELATIVE_HINT, -5, 2160),
            (0, i32::MIN, i32::MAX),
        ] {
            let s = CursorState {
                serial: 42,
                flags,
                x,
                y,
            };
            let d = encode_cursor_state_datagram(&s);
            assert_eq!(decode_cursor_state_datagram(&d), Some(s));
            assert_eq!(s.visible(), flags & CURSOR_VISIBLE != 0);
            assert_eq!(s.relative_hint(), flags & CURSOR_RELATIVE_HINT != 0);
            // Append-extensible like 0xCF: a longer buffer still parses the known prefix.
            let mut ext = d.clone();
            ext.push(0xFF);
            assert_eq!(decode_cursor_state_datagram(&ext), Some(s));
            // Short / wrong tag are rejected before any read.
            assert_eq!(decode_cursor_state_datagram(&d[..d.len() - 1]), None);
            let mut bad = d.clone();
            bad[0] = HOST_TIMING_MAGIC;
            assert_eq!(decode_cursor_state_datagram(&bad), None);
        }
    }
}
