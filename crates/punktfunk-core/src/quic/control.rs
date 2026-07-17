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

// ---------------------------------------------------------------------------------------------
// Shared clipboard & file transfer — wire codecs (ported from the pre-W7 quic/msgs.rs on the
// clipboard-feature merge; the control-stream metadata messages live beside the clock codecs).
// ---------------------------------------------------------------------------------------------

// ---------------------------------------------------------------------------------------------
// Shared clipboard & file transfer (design/clipboard-and-file-transfer.md §3). The small
// metadata messages ride the control stream (0x40-0x42); the two fetch-stream messages
// (0x43-0x44) travel on a per-transfer bi-stream (see the [`super::clipstream`] helpers), never
// the control stream, so they are never dispatched by the control loops. All are typed
// (`CTL_MAGIC` + type byte), so an older peer hits its "unknown control message" arm and drops
// any it doesn't know — the whole feature is forward-safe.
// ---------------------------------------------------------------------------------------------

/// Type byte of [`ClipControl`] (client → host): enable/disable the shared clipboard for this
/// session. Idempotent; opt-in is enforced here, not just in UI.
pub const MSG_CLIP_CONTROL: u8 = 0x40;
/// Type byte of [`ClipState`] (host → client): ack + unsolicited policy/backend updates.
pub const MSG_CLIP_STATE: u8 = 0x41;
/// Type byte of [`ClipOffer`] (symmetric): the lazy announcement — format list only, no bytes.
pub const MSG_CLIP_OFFER: u8 = 0x42;
/// Type byte of [`ClipFetch`] (requester → holder, **fetch stream only**): pull one format of the
/// current offer.
pub const MSG_CLIP_FETCH: u8 = 0x43;
/// Type byte of [`ClipFetchHdr`] (holder → requester, **fetch stream only**): the fetch response
/// header that precedes the data chunks.
pub const MSG_CLIP_FETCH_HDR: u8 = 0x44;

/// [`ClipControl::flags`] bit: the client permits file kinds to be offered/fetched this session.
/// Absent ⇒ files are filtered out of offers in both directions (text/rich/image only).
pub const CLIP_FLAG_FILES: u8 = 0x01;

/// [`ClipState::policy`] bit: the host permits non-file formats (text/RTF/HTML/image). Always set
/// while enabled unless a future direction limit clears it.
pub const CLIP_POLICY_TEXT: u8 = 0x01;
/// [`ClipState::policy`] bit: the host permits file formats. Cleared by the operator `no-files`
/// / `text-only` policy so the client can grey out "Include files".
pub const CLIP_POLICY_FILES: u8 = 0x02;

/// [`ClipState::reason`]: normal ack, nothing exceptional.
pub const CLIP_REASON_OK: u8 = 0;
/// [`ClipState::reason`]: this session type has no working clipboard backend (e.g. a gamescope
/// session with no data-control global) — the client shows "not supported in this session type".
pub const CLIP_REASON_BACKEND_UNAVAILABLE: u8 = 1;
/// [`ClipState::reason`]: another client took over the single per-desktop clipboard binding; this
/// one was disabled (last `ClipControl{enabled}` wins).
pub const CLIP_REASON_TAKEN_OVER: u8 = 2;
/// [`ClipState::reason`]: the host operator policy (`PUNKTFUNK_CLIPBOARD=off`) disables clipboard.
pub const CLIP_REASON_POLICY_DISABLED: u8 = 3;
/// [`ClipState::reason`]: enabled, but the host policy forbids file transfer (`no-files` /
/// `text-only`) — surfaced so the client greys "Include files" with a footnote.
pub const CLIP_REASON_NO_FILES: u8 = 4;

/// [`ClipFetchHdr::status`]: the requested format is being served; data chunks follow until FIN.
pub const CLIP_FETCH_OK: u8 = 0;
/// [`ClipFetchHdr::status`]: the fetch named a `seq` that is no longer the holder's current offer;
/// the requester degrades the paste to "nothing inserted" rather than wrong data. No chunks follow.
pub const CLIP_FETCH_STALE: u8 = 1;
/// [`ClipFetchHdr::status`]: the format/index is not available (no backend, or it vanished). No
/// chunks follow.
pub const CLIP_FETCH_UNAVAILABLE: u8 = 2;
/// [`ClipFetchHdr::status`]: policy/cap denies this fetch (e.g. a file fetch under `no-files`). No
/// chunks follow.
pub const CLIP_FETCH_DENIED: u8 = 3;

/// Maximum number of [`ClipKind`] entries in one [`ClipOffer`] (resource cap, §7).
pub const CLIP_MAX_KINDS: usize = 16;
/// Maximum length in bytes of a [`ClipKind::mime`] string (resource cap, §7).
pub const CLIP_MAX_MIME: usize = 128;
/// [`ClipFetch::file_index`] sentinel meaning "not a file fetch" (a whole non-file format, or the
/// file *manifest* itself). Real file fetches use `0..n`.
pub const CLIP_FILE_INDEX_NONE: u32 = u32::MAX;

/// One advertised clipboard format inside a [`ClipOffer`] — a portable MIME name plus a size hint.
/// The bytes never ride here; they cross lazily on a fetch stream only when the destination pastes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipKind {
    /// Portable wire MIME, e.g. `text/plain;charset=utf-8`, `text/html`, `image/png`,
    /// `application/x-punktfunk-files`. Each end maps it to a platform type at fetch time. ≤
    /// [`CLIP_MAX_MIME`] bytes; a longer one is rejected on decode.
    pub mime: String,
    /// Best-effort total size of this format in bytes; `0` = unknown (a streaming provider).
    pub size_hint: u64,
}

/// `client → host` ([`MSG_CLIP_CONTROL`]): flip the shared clipboard on/off for this session.
/// Sent when the user toggles the per-host pref and once at session start if it is on. **Nothing
/// clipboard-related happens on either side until an `enabled: true` arrives** — opt-in at the
/// protocol layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipControl {
    pub enabled: bool,
    /// Bitfield of [`CLIP_FLAG_FILES`] (+ reserved bits for future direction limits).
    pub flags: u8,
}

/// `host → client` ([`MSG_CLIP_STATE`]): acknowledge a [`ClipControl`] and push unsolicited
/// updates (policy changed, backend lost). The client surfaces `reason`/`policy` in the toggle UI
/// instead of failing silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipState {
    pub enabled: bool,
    /// Bitfield of [`CLIP_POLICY_TEXT`] / [`CLIP_POLICY_FILES`] — what the host currently permits.
    pub policy: u8,
    /// One of the `CLIP_REASON_*` values explaining `enabled`/`policy`.
    pub reason: u8,
}

/// Symmetric ([`MSG_CLIP_OFFER`], either direction): the lazy announcement. Sent when the local
/// clipboard changes; carries the **format list only** (comfortably inside the 64 KiB control
/// frame). A new offer replaces the sender's previous one; `seq` lets the holder reject stale
/// fetches (§3.4). Files are announced as one `application/x-punktfunk-files` kind — the file
/// list itself is fetched lazily, never inlined here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipOffer {
    /// Monotonic per sender; newest wins.
    pub seq: u32,
    /// ≤ [`CLIP_MAX_KINDS`] entries.
    pub kinds: Vec<ClipKind>,
}

/// `requester → holder` ([`MSG_CLIP_FETCH`], **fetch stream only**): the first message on a
/// per-transfer bi-stream, naming which format (and, for files, which entry) of `seq` to pull.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipFetch {
    /// The offer `seq` this fetch is against; the holder answers [`CLIP_FETCH_STALE`] if it is no
    /// longer current.
    pub seq: u32,
    /// File index for a file transfer, or [`CLIP_FILE_INDEX_NONE`] for a non-file format / the
    /// file manifest.
    pub file_index: u32,
    /// The requested wire MIME (≤ [`CLIP_MAX_MIME`] bytes).
    pub mime: String,
}

/// `holder → requester` ([`MSG_CLIP_FETCH_HDR`], **fetch stream only**): the response header that
/// precedes the raw data chunks (which run until the stream's FIN). When `status` is anything
/// other than [`CLIP_FETCH_OK`] no chunks follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipFetchHdr {
    /// One of the `CLIP_FETCH_*` values.
    pub status: u8,
    /// Total byte count that will follow; `0` = unknown (a streaming provider — FIN ends it).
    pub total_size: u64,
}

/// Append one [`ClipKind`] to `b`: `mime_len u8 || mime bytes || size_hint u64 LE`.
fn put_clip_kind(b: &mut Vec<u8>, k: &ClipKind) {
    let mime = k.mime.as_bytes();
    let n = mime.len().min(CLIP_MAX_MIME);
    b.push(n as u8);
    b.extend_from_slice(&mime[..n]);
    b.extend_from_slice(&k.size_hint.to_le_bytes());
}

/// Read one [`ClipKind`] at `off`, returning it and the next offset.
fn get_clip_kind(b: &[u8], off: usize) -> Result<(ClipKind, usize)> {
    if off >= b.len() {
        return Err(PunktfunkError::InvalidArg("truncated ClipKind"));
    }
    let n = b[off] as usize;
    if n > CLIP_MAX_MIME {
        return Err(PunktfunkError::InvalidArg("ClipKind mime too long"));
    }
    let mime_start = off + 1;
    let size_start = mime_start + n;
    if size_start + 8 > b.len() {
        return Err(PunktfunkError::InvalidArg("ClipKind overruns message"));
    }
    let mime = String::from_utf8_lossy(&b[mime_start..size_start]).into_owned();
    let size_hint = u64::from_le_bytes(b[size_start..size_start + 8].try_into().unwrap());
    Ok((ClipKind { mime, size_hint }, size_start + 8))
}

impl ClipControl {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] enabled[5] flags[6]
        let mut b = Vec::with_capacity(7);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLIP_CONTROL);
        b.push(self.enabled as u8);
        b.push(self.flags);
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClipControl> {
        if b.len() != 7 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLIP_CONTROL {
            return Err(PunktfunkError::InvalidArg("bad ClipControl"));
        }
        Ok(ClipControl {
            enabled: b[5] != 0,
            flags: b[6],
        })
    }
}

impl ClipState {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] enabled[5] policy[6] reason[7]
        let mut b = Vec::with_capacity(8);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLIP_STATE);
        b.push(self.enabled as u8);
        b.push(self.policy);
        b.push(self.reason);
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClipState> {
        if b.len() != 8 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLIP_STATE {
            return Err(PunktfunkError::InvalidArg("bad ClipState"));
        }
        Ok(ClipState {
            enabled: b[5] != 0,
            policy: b[6],
            reason: b[7],
        })
    }
}

impl ClipOffer {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] seq[5..9] count[9] then `count` ClipKinds
        let mut b = Vec::with_capacity(10 + self.kinds.len() * 16);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLIP_OFFER);
        b.extend_from_slice(&self.seq.to_le_bytes());
        let count = self.kinds.len().min(CLIP_MAX_KINDS);
        b.push(count as u8);
        for k in &self.kinds[..count] {
            put_clip_kind(&mut b, k);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClipOffer> {
        if b.len() < 10 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLIP_OFFER {
            return Err(PunktfunkError::InvalidArg("bad ClipOffer"));
        }
        let seq = u32::from_le_bytes(b[5..9].try_into().unwrap());
        let count = b[9] as usize;
        if count > CLIP_MAX_KINDS {
            return Err(PunktfunkError::InvalidArg("ClipOffer too many kinds"));
        }
        let mut kinds = Vec::with_capacity(count);
        let mut off = 10;
        for _ in 0..count {
            let (k, next) = get_clip_kind(b, off)?;
            kinds.push(k);
            off = next;
        }
        if off != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(ClipOffer { seq, kinds })
    }
}

impl ClipFetch {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] seq[5..9] file_index[9..13] mime(len u8 || bytes)[13..]
        let mime = self.mime.as_bytes();
        let n = mime.len().min(CLIP_MAX_MIME);
        let mut b = Vec::with_capacity(14 + n);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLIP_FETCH);
        b.extend_from_slice(&self.seq.to_le_bytes());
        b.extend_from_slice(&self.file_index.to_le_bytes());
        b.push(n as u8);
        b.extend_from_slice(&mime[..n]);
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClipFetch> {
        if b.len() < 14 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLIP_FETCH {
            return Err(PunktfunkError::InvalidArg("bad ClipFetch"));
        }
        let seq = u32::from_le_bytes(b[5..9].try_into().unwrap());
        let file_index = u32::from_le_bytes(b[9..13].try_into().unwrap());
        let n = b[13] as usize;
        if n > CLIP_MAX_MIME || b.len() != 14 + n {
            return Err(PunktfunkError::InvalidArg("bad ClipFetch mime"));
        }
        let mime = String::from_utf8_lossy(&b[14..14 + n]).into_owned();
        Ok(ClipFetch {
            seq,
            file_index,
            mime,
        })
    }
}

impl ClipFetchHdr {
    pub fn encode(&self) -> Vec<u8> {
        // magic[0..4] type[4] status[5] total_size[6..14]
        let mut b = Vec::with_capacity(14);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_CLIP_FETCH_HDR);
        b.push(self.status);
        b.extend_from_slice(&self.total_size.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<ClipFetchHdr> {
        if b.len() != 14 || &b[0..4] != CTL_MAGIC || b[4] != MSG_CLIP_FETCH_HDR {
            return Err(PunktfunkError::InvalidArg("bad ClipFetchHdr"));
        }
        Ok(ClipFetchHdr {
            status: b[5],
            total_size: u64::from_le_bytes(b[6..14].try_into().unwrap()),
        })
    }
}
