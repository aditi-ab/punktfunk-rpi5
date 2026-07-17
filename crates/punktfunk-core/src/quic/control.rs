//! Typed post-handshake control messages (`CTL_MAGIC` + type byte): reconfigure, keyframe,
//! RFI, loss reports, bitrate, bandwidth probes, and clock sync.

use super::*;
use crate::config::Mode;
use crate::error::{PunktfunkError, Result};

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

/// `client → host`: reference-frame-invalidation recovery — the loss-aware sibling of
/// [`RequestKeyframe`]. The client detected a `frame_index` gap and reports the range `[first_frame,
/// last_frame]` of access units it can no longer trust (from the first missing index through the
/// newest received). Instead of a full IDR (a 20-40× spike that deepens the loss it recovers), a host
/// whose encoder supports RFI re-references a known-good picture *before* `first_frame` — an AMD LTR
/// force-reference or an NVENC `nvEncInvalidateRefFrames` — emitting a single clean P-frame it tags
/// [`crate::packet::USER_FLAG_RECOVERY_ANCHOR`] so the client lifts its freeze on it. A host that
/// can't RFI (no valid reference / libavcodec backend) forces an IDR instead, exactly as for a bare
/// [`RequestKeyframe`]; a host that predates this ignores the unknown message and the client's
/// keyframe backstop still recovers. Fire-and-forget — the recovered frame is the only ack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RfiRequest {
    /// First access-unit `frame_index` the client can no longer trust (the gap start).
    pub first_frame: u32,
    /// Newest received `frame_index` at the time of the report (the invalidation range end).
    pub last_frame: u32,
}

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
/// request clamped to its supported band). The encoder retargets in place where the backend can
/// (no IDR — the stream carries straight on); a backend without in-place reconfigure rebuilds and
/// switches on the next frame (an IDR). The stream never pauses either way. Also the controller's
/// liveness signal: no answer ⇒ an old host that doesn't renegotiate bitrate.
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
/// Type byte of [`RfiRequest`].
pub const MSG_RFI_REQUEST: u8 = 0x07;
/// Type byte of [`ProbeRequest`].
pub const MSG_PROBE_REQUEST: u8 = 0x20;
/// Type byte of [`ProbeResult`].
pub const MSG_PROBE_RESULT: u8 = 0x21;
/// Type byte of [`ClockProbe`].
pub const MSG_CLOCK_PROBE: u8 = 0x30;
/// Type byte of [`ClockEcho`].
pub const MSG_CLOCK_ECHO: u8 = 0x31;

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

impl RfiRequest {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] first_frame[5..9] last_frame[9..13]
        let mut b = Vec::with_capacity(13);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_RFI_REQUEST);
        b.extend_from_slice(&self.first_frame.to_le_bytes());
        b.extend_from_slice(&self.last_frame.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<RfiRequest> {
        if b.len() != 13 || &b[0..4] != CTL_MAGIC || b[4] != MSG_RFI_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad RfiRequest"));
        }
        Ok(RfiRequest {
            first_frame: u32::from_le_bytes(b[5..9].try_into().unwrap()),
            last_frame: u32::from_le_bytes(b[9..13].try_into().unwrap()),
        })
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
/// (the loss it absorbed), recovered-but-then-arrived shards (`late` — reordered delivery lets a
/// block reconstruct early, so those were never lost; netting them out keeps plain reordering from
/// reading as packet loss and spooking adaptive FEC + the bitrate controller), shards received,
/// and frames that went unrecoverable. Loss ≈ (recovered − late) / (received + recovered − late) —
/// the fraction of shards that truly never arrived (a late shard is inside `received`, so the
/// denominator nets it too; saturating, so reorder straddling a window boundary can't go
/// negative). A frame drop means loss exceeded the current FEC budget (so `recovered` plateaus),
/// so add a fixed bump to push the host's FEC up past the cap on the next adjustment. Returns
/// parts-per-million, capped at 1e6.
pub fn window_loss_ppm(recovered: u64, late: u64, received: u64, frames_dropped: u64) -> u32 {
    let lost = recovered.saturating_sub(late);
    let denom = received.saturating_add(lost);
    let mut ppm = lost
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
