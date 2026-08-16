//! The QUIC-datagram side planes, demultiplexed by their first byte (0xC9–0xD1):
//! audio, rumble, mic uplink, rich input, HID output, HDR metadata, host timing,
//! cursor state, pad audio.

/// Datagram wire tags. Video rides UDP; everything low-rate rides QUIC datagrams,
/// demultiplexed by the first byte: input = [`crate::input::INPUT_MAGIC`] (0xC8, client→host),
/// audio = [`AUDIO_MAGIC`] (0xC9, host→client), rumble = [`RUMBLE_MAGIC`] (0xCA, host→client),
/// mic = [`MIC_MAGIC`] (0xCB, client→host), rich-input = [`RICH_INPUT_MAGIC`] (0xCC, client→host),
/// HID-output = [`HIDOUT_MAGIC`] (0xCD, host→client), HDR metadata = [`HDR_META_MAGIC`]
/// (0xCE, host→client), host timing = [`HOST_TIMING_MAGIC`] (0xCF, host→client), cursor state =
/// [`CURSOR_STATE_MAGIC`] (0xD0, host→client), pad audio = [`PAD_AUDIO_MAGIC`] (0xD1,
/// host→client).
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

/// Lossless PCM audio, host → client: `[0xD3][u32 seq LE][u64 pts_ns LE][interleaved LE samples]`.
///
/// **Deliberately the same header as [`AUDIO_MAGIC`]**, so
/// [`AudioGapTracker`](crate::audio::AudioGapTracker) and the pts / A-V-sync plumbing work
/// unchanged and the only new logic on this plane is the payload format and its concealment
/// ([`crate::audio::pcm`]).
///
/// A session runs `0xC9`/`0xD2` **or** `0xD3`, never both, and never switches mid-session: the
/// client's output device is open at a fixed rate and depth, so a change means a re-open. If the
/// capture dies and comes back at a different format the host ends the audio plane rather than
/// changing tags underneath a client that cannot follow.
///
/// One frame per datagram, `audio_frame_us` long, **never fragmented** — the frame duration is
/// chosen at session start by [`crate::audio::pcm::frame_us_for`] so the payload cannot exceed
/// the path MTU. Redundancy ([`AUDIO_RED_MAGIC`]) is not defined for this plane and is never
/// sent with it: it would double a bitrate that is already the largest on the connection, and
/// `plan_audio_budget`'s ladder would never choose it.
pub const AUDIO_PCM_MAGIC: u8 = 0xD3;

/// Fixed header length of an [`AUDIO_PCM_MAGIC`] datagram — identical to the [`AUDIO_MAGIC`]
/// header by design.
pub const AUDIO_PCM_HEADER: usize = crate::audio::pcm::PCM_HEADER_LEN;

/// Encode one lossless PCM frame. `pcm` is already-quantised interleaved little-endian samples
/// at the negotiated depth ([`crate::audio::pcm::from_f32`]).
pub fn encode_audio_pcm_datagram(seq: u32, pts_ns: u64, pcm: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(AUDIO_PCM_HEADER + pcm.len());
    b.push(AUDIO_PCM_MAGIC);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(pcm);
    b
}

/// Parse a lossless PCM datagram → `(seq, pts_ns, payload)`. `None` on bad tag/length.
pub fn decode_audio_pcm_datagram(b: &[u8]) -> Option<(u32, u64, &[u8])> {
    if b.len() < AUDIO_PCM_HEADER || b[0] != AUDIO_PCM_MAGIC {
        return None;
    }
    let seq = u32::from_le_bytes(b[1..5].try_into().unwrap());
    let pts_ns = u64::from_le_bytes(b[5..13].try_into().unwrap());
    Some((seq, pts_ns, &b[AUDIO_PCM_HEADER..]))
}

/// Legacy rumble datagram (v1), host → client: `[0xCA][u16 pad LE][u16 low LE][u16 high LE]`.
/// Force-feedback state for pad `pad` (0xFFFF amplitudes, 0/0 = stop) as *level-triggered* state
/// — it persists until superseded, which is why the host re-sends it periodically as its loss
/// heal. New hosts emit the self-terminating [`encode_rumble_datagram_v3`] instead; this is kept
/// for the loopback tests, as the wire an old host still speaks, and as what the
/// `PUNKTFUNK_RUMBLE_ENVELOPE=0` bisect hatch drops to (a new client decodes every form via
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
/// Wire length of a v3 (envelope + impulse-trigger motors) rumble datagram — the v2 form plus a
/// `[u16 left_trigger LE][u16 right_trigger LE]` tail (see [`encode_rumble_datagram_v3`]). Second
/// use of the same append-extension the v2 tail introduced, and for the same reason: every reader
/// on this plane gates with `>=`, so a 14-byte datagram satisfies the v1 predicate (level only),
/// the v2 predicate (level + envelope) and this one, and each peer takes the prefix it knows.
pub const RUMBLE_V3_LEN: usize = 14;

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

/// Rumble envelope datagram with the impulse-trigger motors (v3), host → client:
/// `[0xCA][u16 pad LE][u16 low LE][u16 high LE][u8 seq][u16 ttl_ms LE][u16 lt LE][u16 rt LE]`.
///
/// The [`encode_rumble_datagram_v2`] envelope with the Xbox trigger motors appended, on the same
/// `0..=0xFFFF` scale as `low`/`high` (design/trigger-rumble-plane.md §4).
///
/// **The four levels share ONE `seq` and ONE `ttl_ms`, deliberately.** They are a single statement
/// of the pad's feedback state at one instant; a second sequence space would let a reordered
/// datagram apply the handles from moment *t* and the triggers from *t−1*, a glitch nothing else
/// in the system can currently produce. Sharing also means the whole v2 apparatus — the renewal
/// cadence, the post-stop burst, the client's wrapping half-space `seq` gate, the receiver-side
/// lease clamp — governs the trigger motors with no new code, so a trigger rumble whose host dies
/// self-silences on the same lease as the handles.
///
/// Exactly one backend can ever source non-zero trigger levels: the Windows HID Xbox pad, whose
/// output report `0x03` carries them. Classic XInput's `XINPUT_VIBRATION` and evdev's `FF_RUMBLE`
/// have two members and no third, so every other producer passes `lt = rt = 0` — for those this is
/// a v2 datagram with four zero bytes on the end, which is exactly what the length tolerance is
/// for.
pub fn encode_rumble_datagram_v3(
    pad: u16,
    low: u16,
    high: u16,
    seq: u8,
    ttl_ms: u16,
    lt: u16,
    rt: u16,
) -> [u8; RUMBLE_V3_LEN] {
    let mut b = [0u8; RUMBLE_V3_LEN];
    b[..RUMBLE_V2_LEN].copy_from_slice(&encode_rumble_datagram_v2(pad, low, high, seq, ttl_ms));
    b[10..12].copy_from_slice(&lt.to_le_bytes());
    b[12..14].copy_from_slice(&rt.to_le_bytes());
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
///
/// `left_trigger`/`right_trigger` are the Xbox impulse-trigger motors from a v3 datagram, on the
/// same `0..=0xFFFF` scale as `low`/`high`, and they are **plain fields, not `Option`** even though
/// only a v3 datagram carries them. A v1/v2 datagram decodes to `left_trigger = right_trigger = 0`.
/// The temptation is to mirror `envelope` so a consumer could tell "old host" from "new host,
/// triggers idle", but `Option` invites "absent → keep the previous value", and on a
/// level-triggered plane that is the stuck-rumble bug in a new costume: `0xCA` means *these are the
/// levels now*, so an absent field is zero. (`envelope` is genuinely optional because its absence
/// selects a different *policy*, not a different level.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleUpdate {
    pub pad: u16,
    pub low: u16,
    pub high: u16,
    pub left_trigger: u16,
    pub right_trigger: u16,
    pub envelope: Option<RumbleEnvelope>,
}

/// Parse a rumble datagram → `(pad, low, high)`, tolerating (and ignoring) the v2 envelope and v3
/// trigger tails. `None` on bad tag/length. Kept for callers that only need the handle level (the
/// probe, the loopback assertions); clients that honor TTL use [`decode_rumble_envelope`].
pub fn decode_rumble_datagram(b: &[u8]) -> Option<(u16, u16, u16)> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    Some((u16at(1), u16at(3), u16at(5)))
}

/// Parse a rumble datagram → [`RumbleUpdate`], detecting each appended tail by length. A
/// `>= RUMBLE_V2_LEN` buffer carries `seq`/`ttl_ms`; a `>= RUMBLE_V3_LEN` buffer additionally
/// carries the two impulse-trigger levels; a 7..RUMBLE_V2_LEN buffer is a legacy level
/// (`envelope: None`) — the same tolerance as an old client would apply, so a torn/short tail
/// degrades to a level rather than dropping. `None` on bad tag/length.
///
/// The one decoder for all three forms: v3 is not a separate wire, it is the same wire with more
/// of it present. Absent trigger bytes read as zero rather than "unchanged" — see
/// [`RumbleUpdate`] for why that is not negotiable on a level-triggered plane.
pub fn decode_rumble_envelope(b: &[u8]) -> Option<RumbleUpdate> {
    if b.len() < RUMBLE_V1_LEN || b[0] != RUMBLE_MAGIC {
        return None;
    }
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let envelope = (b.len() >= RUMBLE_V2_LEN).then(|| RumbleEnvelope {
        seq: b[7],
        ttl_ms: u16::from_le_bytes([b[8], b[9]]),
    });
    let triggers = b.len() >= RUMBLE_V3_LEN;
    Some(RumbleUpdate {
        pad: u16at(1),
        low: u16at(3),
        high: u16at(5),
        left_trigger: if triggers { u16at(10) } else { 0 },
        right_trigger: if triggers { u16at(12) } else { 0 },
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

/// Longest [`HidOutput::Trigger`] `effect` the wire carries: the DualSense adaptive-trigger
/// parameter block is a mode byte plus ten parameters, and every consumer copies at most this many
/// into its report.
///
/// The single source for the clamp on BOTH sides. `Trigger` was the only variable-length variant
/// bounded on neither: encode appended whatever it was handed and decode took the entire tail, so
/// an attacker-sized datagram was reproduced verbatim into a `Vec` while its sibling `HidRaw` had
/// been bounded on both ends all along.
pub const TRIGGER_EFFECT_MAX: usize = 11;

const HIDOUT_LED: u8 = 0x01;
const HIDOUT_PLAYER_LEDS: u8 = 0x02;
const HIDOUT_TRIGGER: u8 = 0x03;
const HIDOUT_TRACKPAD_HAPTIC: u8 = 0x04;
const HIDOUT_HID_RAW: u8 = 0x05;
const HIDOUT_AUDIO_CTL: u8 = 0x06;

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
    /// The audio-control region of a DS5 output report `0x02` a game wrote to the host's virtual
    /// pad — the routing/volume side of pad audio (the audio SAMPLES ride the [`PAD_AUDIO_MAGIC`]
    /// plane). `raw` is bytes 5..=10 of the report verbatim (headphone/speaker/mic volumes +
    /// audio routing); `flags` condenses the report's audio valid-flags: bit0 = haptics-select
    /// (`valid_flag0` bit1 — the title asked for audio haptics on the voice coils), bits1..4 =
    /// `valid_flag0` bits 4..7 (the audio-valid flags gating `raw`). Wire form
    /// `[0xCD][0x06][u16 pad LE][u8 flags][6 raw bytes]`. Forwarded change-only (deduped by
    /// value host-side, like `Led`/`Trigger`) — a merely-rumbling pad re-sends unchanged audio
    /// state on every output report.
    AudioCtl { pad: u16, flags: u8, raw: [u8; 6] },
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
                out.extend_from_slice(&effect[..effect.len().min(TRIGGER_EFFECT_MAX)]);
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
            HidOutput::AudioCtl { pad, flags, raw } => {
                out.push(HIDOUT_AUDIO_CTL);
                out.extend_from_slice(&pad.to_le_bytes());
                out.push(*flags);
                out.extend_from_slice(raw);
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
            // `> 4`, not `>= 4`: a body with no effect bytes at all is malformed, and decoding it
            // as an EMPTY effect was actively harmful — downstream an empty block is written as an
            // all-zero trigger report, which is mode 0x00, which RELEASES a held effect. A
            // truncated datagram could therefore silently cancel the trigger a game was holding.
            // A genuine "no effect" is a full-length zero block and still decodes fine.
            HIDOUT_TRIGGER if b.len() > 4 => Some(HidOutput::Trigger {
                pad: b[2],
                which: b[3],
                // Bounded like `HidRaw` below: at most the parameter block is kept from the
                // (attacker-sized) tail.
                effect: b[4..b.len().min(4 + TRIGGER_EFFECT_MAX)].to_vec(),
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
            // B27: the pad is the only u16 index on this plane, and every consumer narrows it
            // with `as u8` on the stated assumption that pads are 0..MAX_PADS. Nothing enforced
            // that, so wire pad 256 silently ALIASED onto slot 0 — a malformed or hostile
            // datagram steering a real controller's speaker volumes. Rejected here, at the one
            // place the u16 exists, so the narrowings downstream are lossless by construction
            // (the same fix R10 applied to the rumble plane).
            HIDOUT_AUDIO_CTL
                if b.len() >= 11
                    && u16::from_le_bytes([b[2], b[3]]) < crate::input::MAX_PADS as u16 =>
            {
                Some(HidOutput::AudioCtl {
                    pad: u16::from_le_bytes([b[2], b[3]]),
                    flags: b[4],
                    raw: b[5..11].try_into().unwrap(),
                })
            }
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

/// Pad-audio datagram tag, host → client: per-gamepad audio a game routed
/// to the host's virtual DualSense — voice-coil haptics and the built-in speaker — for the client
/// to render on the matching real controller. Next tag after [`CURSOR_STATE_MAGIC`]. The
/// per-pad AUDIO plane (Opus frames, the [`AUDIO_MAGIC`]/[`MIC_MAGIC`] shape plus pad + kind);
/// the routing/volume CONTROL side rides [`HidOutput::AudioCtl`]. Emitted only when the session
/// negotiated it ([`CLIENT_CAP_PAD_AUDIO`](super::caps::CLIENT_CAP_PAD_AUDIO) ∧
/// [`HOST_CAP_PAD_AUDIO`](super::caps::HOST_CAP_PAD_AUDIO)) and the pad's arrival declared a
/// renderer for the kind ([`crate::input::ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`]/`_SPEAKER`).
/// Best-effort like every audio datagram: a lost frame is a concealed gap, never state.
pub const PAD_AUDIO_MAGIC: u8 = 0xD1;

/// [`PadAudioFrame::kind`]: the BACK channel pair — the DualSense voice-coil actuators (audio
/// haptics). 5 ms Opus frames, matching the [`AUDIO_MAGIC`] cadence: haptics are felt latency.
pub const PAD_AUDIO_KIND_HAPTICS: u8 = 0;
/// [`PadAudioFrame::kind`]: the FRONT channel pair — the controller's built-in speaker. 10 ms
/// Opus frames (speaker content tolerates the extra buffering for the better coding efficiency).
pub const PAD_AUDIO_KIND_SPEAKER: u8 = 1;

/// Wire length of a pad-audio datagram header: tag + pad + kind + u32 seq + u64 pts = 15 bytes.
const PAD_AUDIO_HEADER_LEN: usize = 1 + 1 + 1 + 4 + 8;

/// One decoded pad-audio frame (owned — the client's plane queue stores it). `seq`/`pts_ns` are
/// per-(pad, kind) counters from the host's capture clock, for gap concealment and lip-sync
/// against the main audio plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadAudioFrame {
    /// Gamepad index (the wire pad space, same as rumble/HID-output).
    pub pad: u8,
    /// [`PAD_AUDIO_KIND_HAPTICS`] or [`PAD_AUDIO_KIND_SPEAKER`].
    pub kind: u8,
    pub seq: u32,
    pub pts_ns: u64,
    /// The raw Opus payload — feed it to an Opus decoder as one frame. Empty = DTX silence.
    pub opus: Vec<u8>,
}

/// Pad-audio datagram, host → client:
/// `[0xD1][u8 pad][u8 kind][u32 seq LE][u64 pts_ns LE][opus payload]` — the
/// [`encode_audio_datagram`]/[`encode_mic_datagram`] layout with a pad + kind prefix, one Opus
/// frame per datagram (5/10 ms — well under any MTU); QUIC already encrypts.
pub fn encode_pad_audio_datagram(pad: u8, kind: u8, seq: u32, pts_ns: u64, opus: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(PAD_AUDIO_HEADER_LEN + opus.len());
    b.push(PAD_AUDIO_MAGIC);
    b.push(pad);
    b.push(kind);
    b.extend_from_slice(&seq.to_le_bytes());
    b.extend_from_slice(&pts_ns.to_le_bytes());
    b.extend_from_slice(opus);
    b
}

/// Parse a pad-audio datagram → [`PadAudioFrame`]. `None` on bad tag/length (the fixed header
/// length bounds every read before it happens).
pub fn decode_pad_audio_datagram(buf: &[u8]) -> Option<PadAudioFrame> {
    if buf.len() < PAD_AUDIO_HEADER_LEN || buf[0] != PAD_AUDIO_MAGIC {
        return None;
    }
    Some(PadAudioFrame {
        pad: buf[1],
        kind: buf[2],
        seq: u32::from_le_bytes(buf[3..7].try_into().unwrap()),
        pts_ns: u64::from_le_bytes(buf[7..15].try_into().unwrap()),
        opus: buf[15..].to_vec(),
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

    /// The lossless plane round-trips, keeps the `0xC9` header shape so the gap tracker and A/V
    /// sync need no new code, and refuses a foreign tag.
    #[test]
    fn audio_pcm_datagram_roundtrip() {
        let payload: Vec<u8> = (0..1152u32).map(|i| (i % 251) as u8).collect();
        let d = encode_audio_pcm_datagram(7, 1_234_567_890, &payload);
        assert_eq!(d[0], AUDIO_PCM_MAGIC);
        assert_eq!(d.len(), AUDIO_PCM_HEADER + payload.len());
        // Same header shape as 0xC9 — seq and pts sit at the same offsets, which is what lets
        // the gap tracker and the pts plumbing work unchanged.
        assert_eq!(AUDIO_PCM_HEADER, 13);
        let (seq, pts, out) = decode_audio_pcm_datagram(&d).expect("decode");
        assert_eq!((seq, pts), (7, 1_234_567_890));
        assert_eq!(out, &payload[..]);

        // An empty payload is structurally legal (nothing to say), and a short buffer is not.
        assert!(decode_audio_pcm_datagram(&encode_audio_pcm_datagram(1, 2, &[])).is_some());
        assert!(decode_audio_pcm_datagram(&d[..AUDIO_PCM_HEADER - 1]).is_none());
        let mut wrong = d.clone();
        wrong[0] = AUDIO_MAGIC;
        assert!(decode_audio_pcm_datagram(&wrong).is_none());
    }

    /// The whole point of the frame ladder: whatever it picks must survive the encoder and land
    /// inside the datagram budget it was given. Cheap to state, and it is the invariant that
    /// keeps this plane off any fragmentation path.
    #[test]
    fn a_ladder_sized_frame_fits_the_datagram_it_was_sized_for() {
        use crate::audio::pcm;
        // Both rate families — the 44.1 kHz one carries a FRACTIONAL number of samples in most
        // rungs, so its frame is the floor and the fit has margin rather than being eroded.
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400] {
            for bits in [pcm::BITS_16, pcm::BITS_24] {
                for budget in [900usize, 1200, 1400] {
                    let Some(us) = pcm::frame_us_for(rate, bits, 2, budget) else {
                        // 176 400/24-bit needs 1 069 B for even a 1 ms frame; a budget that small
                        // declines the plane outright, exactly as it declines hi-res surround.
                        continue;
                    };
                    let samples = pcm::samples_per_frame(rate, us, 2);
                    let mut wire = Vec::new();
                    pcm::from_f32(&vec![0.25f32; samples], bits, &mut wire);
                    let d = encode_audio_pcm_datagram(0, 0, &wire);
                    assert!(
                        d.len() <= budget,
                        "{rate}/{bits} at {budget} B produced a {} B datagram",
                        d.len()
                    );
                }
            }
        }
    }

    /// The three audio planes must not alias each other or any neighbouring plane: a client
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
            AUDIO_PCM_MAGIC,
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
            decode_audio_pcm_datagram(&plain).is_none(),
            "0xD3 must not accept a 0xC9"
        );
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

    /// `Trigger` is the only variable-length variant that used to be bounded on NEITHER side.
    /// Pinned here because both halves matter: an over-long effect must be clamped on the way out
    /// AND on the way in, and a body with no effect bytes must not decode at all.
    #[test]
    fn trigger_effect_is_clamped_on_both_encode_and_decode() {
        // Encode clamps: a caller handing over an over-long block cannot put it on the wire.
        let long = HidOutput::Trigger {
            pad: 1,
            which: 0,
            effect: vec![0xAB; 200],
        };
        let d = long.encode();
        assert_eq!(
            d.len(),
            4 + TRIGGER_EFFECT_MAX,
            "magic + kind + pad + which + at most the parameter block"
        );

        // Decode clamps independently of encode — a hostile peer does not use our encoder.
        let mut hostile = vec![HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 1, 0];
        hostile.extend_from_slice(&[0xCD; 500]);
        match HidOutput::decode(&hostile) {
            Some(HidOutput::Trigger { effect, .. }) => {
                assert_eq!(effect.len(), TRIGGER_EFFECT_MAX, "tail is bounded");
            }
            other => panic!("expected a clamped Trigger, got {other:?}"),
        }

        // An exact-length effect survives untouched, and round-trips.
        let ok = HidOutput::Trigger {
            pad: 2,
            which: 1,
            effect: vec![0x02, 0x90, 0xA0, 0xFF, 0, 0, 0, 0, 0, 0, 0],
        };
        assert_eq!(HidOutput::decode(&ok.encode()), Some(ok));
    }

    /// A body with no effect bytes is malformed and must be REJECTED, not read as an empty effect:
    /// downstream an empty block becomes an all-zero trigger report, which is mode 0x00 — it
    /// releases whatever effect the game was holding. A truncated datagram must not do that.
    #[test]
    fn a_trigger_with_no_effect_bytes_is_rejected_not_read_as_cancel() {
        let empty = [HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 0, 0];
        assert_eq!(HidOutput::decode(&empty), None);

        // One byte of effect is a legitimate short block (consumers zero-pad it) and still decodes.
        let one = [HIDOUT_MAGIC, super::HIDOUT_TRIGGER, 0, 0, 0x02];
        assert_eq!(
            HidOutput::decode(&one),
            Some(HidOutput::Trigger {
                pad: 0,
                which: 0,
                effect: vec![0x02]
            })
        );
    }

    /// `HidRaw`'s bound was already correct on both sides — pinned alongside `Trigger` so the pair
    /// cannot drift apart again.
    #[test]
    fn hid_raw_stays_bounded_on_both_sides() {
        let long = HidOutput::HidRaw {
            pad: 0,
            kind: HID_RAW_OUTPUT,
            data: vec![0x11; 500],
        };
        assert_eq!(long.encode().len(), 4 + HID_REPORT_MAX);

        let mut hostile = vec![HIDOUT_MAGIC, super::HIDOUT_HID_RAW, 0, HID_RAW_FEATURE];
        hostile.extend_from_slice(&[0x22; 900]);
        match HidOutput::decode(&hostile) {
            Some(HidOutput::HidRaw { data, .. }) => assert_eq!(data.len(), HID_REPORT_MAX),
            other => panic!("expected a clamped HidRaw, got {other:?}"),
        }
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
                left_trigger: 0,
                right_trigger: 0,
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
                left_trigger: 0,
                right_trigger: 0,
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

    /// v3 (design/trigger-rumble-plane.md §4) is the v2 envelope with the two impulse-trigger
    /// levels appended, and the prefix discipline the 0xCF plane uses three times over holds here
    /// too: the first 10 bytes must be byte-identical to what v2 would have produced, or the
    /// envelope a v2-era client reads is displaced and every TTL/seq guarantee on this plane
    /// silently changes meaning.
    #[test]
    fn rumble_v3_roundtrips_and_keeps_the_v2_envelope_in_place() {
        let v2 = encode_rumble_datagram_v2(2, 0x4000, 0x8000, 7, 400);
        let v3 = encode_rumble_datagram_v3(2, 0x4000, 0x8000, 7, 400, 0x1234, 0xFFFF);
        assert_eq!(v3.len(), RUMBLE_V3_LEN);
        assert_eq!(&v3[..RUMBLE_V2_LEN], &v2[..], "v2 is a strict prefix of v3");
        // The exact tail layout, LE, pinned as bytes: an endianness slip here reads a 0x1234
        // trigger as 0x3412 and is invisible in a round-trip that uses the same encoder both ways.
        assert_eq!(&v3[10..14], &[0x34, 0x12, 0xFF, 0xFF]);
        assert_eq!(
            decode_rumble_envelope(&v3),
            Some(RumbleUpdate {
                pad: 2,
                low: 0x4000,
                high: 0x8000,
                left_trigger: 0x1234,
                right_trigger: 0xFFFF,
                envelope: Some(RumbleEnvelope {
                    seq: 7,
                    ttl_ms: 400
                }),
            })
        );
        // A trigger-only rumble (racing titles drive the triggers hard and the handles not at all)
        // is expressible and survives the trip with the handles at rest.
        let trig_only = encode_rumble_datagram_v3(0, 0, 0, 3, 400, 0x8000, 0);
        let u = decode_rumble_envelope(&trig_only).unwrap();
        assert_eq!((u.low, u.high), (0, 0));
        assert_eq!((u.left_trigger, u.right_trigger), (0x8000, 0));
        assert_eq!(u.envelope.unwrap().ttl_ms, 400);
    }

    /// Cross-version tolerance, both directions — the compatibility table in
    /// design/trigger-rumble-plane.md §5, as code.
    #[test]
    fn rumble_v3_and_v2_parse_each_others_datagrams() {
        let v3 = encode_rumble_datagram_v3(1, 0x1111, 0x2222, 9, 250, 0xAAAA, 0xBBBB);

        // NEW host → OLD client: the v2-era readers see exactly what they saw before. The level
        // decoder ignores both tails; the envelope decoder reads the same seq/ttl off bytes 7..10.
        assert_eq!(decode_rumble_datagram(&v3), Some((1, 0x1111, 0x2222)));
        assert_eq!(
            decode_rumble_envelope(&v3).unwrap().envelope,
            Some(RumbleEnvelope {
                seq: 9,
                ttl_ms: 250
            })
        );

        // OLD host → NEW client: v1 and v2 decode with the triggers SILENT, not "unchanged".
        for (form, d) in [
            ("v1", encode_rumble_datagram(1, 0x1111, 0x2222).to_vec()),
            (
                "v2",
                encode_rumble_datagram_v2(1, 0x1111, 0x2222, 9, 250).to_vec(),
            ),
        ] {
            let u = decode_rumble_envelope(&d).unwrap();
            assert_eq!(
                (u.left_trigger, u.right_trigger),
                (0, 0),
                "{form} must decode to idle triggers"
            );
            assert_eq!((u.pad, u.low, u.high), (1, 0x1111, 0x2222));
        }

        // A torn trigger tail (11..14 bytes — the host never emits these, a truncating middlebox
        // might) degrades to the v2 decode rather than reading half a level: a 13-byte buffer must
        // not surface `rt` from one byte of it.
        let v2 = decode_rumble_envelope(&encode_rumble_datagram_v2(1, 0x1111, 0x2222, 9, 250));
        for n in RUMBLE_V2_LEN..RUMBLE_V3_LEN {
            assert_eq!(
                decode_rumble_envelope(&v3[..n]),
                v2,
                "partial trigger tail ({n} B) must degrade to the v2 decode"
            );
        }
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
            // The DS5 audio-control region (haptics-select + speaker volume asserted).
            HidOutput::AudioCtl {
                pad: 1,
                flags: 0b0_0101,
                raw: [0x50, 0x60, 0x70, 0x05, 0x00, 0x00],
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
    fn audio_ctl_wire_layout_and_truncation() {
        // The exact 11-byte layout: [0xCD][0x06][u16 pad LE][u8 flags][6 raw bytes].
        // The pad is deliberately a REPRESENTABLE one: this used to assert that 0x0201 (513)
        // round-tripped, which pinned B27's aliasing in place as if it were the contract.
        let a = HidOutput::AudioCtl {
            pad: 0x000B,
            flags: 0x17,
            raw: [1, 2, 3, 4, 5, 6],
        };
        let d = a.encode();
        assert_eq!(d, [0xCD, 0x06, 0x0B, 0x00, 0x17, 1, 2, 3, 4, 5, 6]);
        assert_eq!(HidOutput::decode(&d), Some(a));
        // Truncated buffers are rejected outright (fixed length — never a partial read).
        for n in 2..d.len() {
            assert_eq!(HidOutput::decode(&d[..n]), None);
        }
    }

    #[test]
    fn pad_audio_datagram_roundtrip_and_truncation() {
        let opus = [0x5Au8; 61];
        let d = encode_pad_audio_datagram(3, PAD_AUDIO_KIND_HAPTICS, 42, 9_999, &opus);
        assert_eq!(d[0], PAD_AUDIO_MAGIC);
        assert_eq!(d.len(), 15 + opus.len());
        let f = decode_pad_audio_datagram(&d).unwrap();
        assert_eq!((f.pad, f.kind, f.seq, f.pts_ns), (3, 0, 42, 9_999));
        assert_eq!(f.opus, opus);
        // Truncated headers are rejected outright (never partially read).
        for n in 0..15 {
            assert_eq!(decode_pad_audio_datagram(&d[..n]), None);
        }
        // Tag separation: a pad-audio datagram is not a session-audio/mic datagram and vice-versa.
        assert!(decode_audio_datagram(&d).is_none());
        assert!(decode_mic_datagram(&d).is_none());
        assert!(decode_pad_audio_datagram(&encode_audio_datagram(1, 2, &opus)).is_none());
        // Empty payload (DTX) is legal — header-only datagram.
        let hdr = encode_pad_audio_datagram(0, PAD_AUDIO_KIND_SPEAKER, 0, 0, &[]);
        assert_eq!(hdr.len(), 15);
        assert!(decode_pad_audio_datagram(&hdr).unwrap().opus.is_empty());
    }

    /// B27: the pad is the only u16 index on the 0xCD plane and every consumer narrows it with
    /// `as u8`. An out-of-range one used to alias onto a real slot instead of being refused —
    /// wire pad 256 steering pad 0's speaker volumes.
    #[test]
    fn audio_ctl_rejects_a_pad_outside_the_index_space() {
        let ok = HidOutput::AudioCtl {
            pad: (crate::input::MAX_PADS - 1) as u16,
            flags: 0x12,
            raw: [1, 2, 3, 4, 5, 6],
        };
        assert_eq!(
            HidOutput::decode(&ok.encode()),
            Some(ok),
            "the last valid pad must still decode"
        );

        // Anything at or above MAX_PADS is refused outright, not truncated.
        for pad in [crate::input::MAX_PADS as u16, 256, u16::MAX] {
            let d = HidOutput::AudioCtl {
                pad,
                flags: 0x12,
                raw: [1, 2, 3, 4, 5, 6],
            }
            .encode();
            assert_eq!(HidOutput::decode(&d), None, "pad {pad} must not decode");
        }

        // The specific alias the bug produced: 256 as u8 == 0.
        let d = HidOutput::AudioCtl {
            pad: 256,
            flags: 0,
            raw: [0; 6],
        }
        .encode();
        assert!(
            !matches!(
                HidOutput::decode(&d),
                Some(HidOutput::AudioCtl { pad: 0, .. })
            ),
            "wire pad 256 must never surface as pad 0"
        );
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
