//! The stylus plane: full-fidelity pen input on the 0xCC rich-input datagram
//! (design/pen-tablet-input.md).
//!
//! A pen datagram is a batch of **state-full samples**: every sample carries the *complete*
//! pen state — in-range/touching/buttons/tool plus all axes — never an edge ("down"/"up")
//! event. The host diffs each sample against its own tracked state ([`PenTracker`]) and
//! synthesizes the transitions, so a lost datagram self-heals on the next sample: a dropped
//! "first contact" batch becomes a tip-down when the next in-contact sample arrives, and a
//! dropped lift heals on the next hover/out-of-range sample (with the
//! [`PEN_TOUCH_TIMEOUT_MS`] failsafe for a client that dies mid-stroke). That is what makes
//! the lossy datagram plane sound for a 240 Hz stylus without any reliable-delivery
//! machinery — the same idempotent-snapshot argument as [`GamepadSnapshot`]
//! (crate::input::GamepadSnapshot) and [`RichInput::HidReport`](super::RichInput).
//!
//! Batches are ordered by a wrapping `u16` sequence number and dropped **whole** when stale
//! ([`pen_seq_newer`]) — applying a stale state-full sample would rewind the stroke.
//!
//! Clients send this only after the host advertised [`HOST_CAP_PEN`](super::HOST_CAP_PEN);
//! a pre-pen host drops the unknown 0xCC kind by the plane's documented forward-compat rule.

use super::datagram::{RICH_INPUT_MAGIC, RICH_PEN};

/// [`PenSample::state`] bit: the pen is in the hover range of the surface. Implied by
/// [`PEN_TOUCHING`] (decode normalizes, so a client that only sets TOUCHING still produces a
/// coherent contact).
pub const PEN_IN_RANGE: u8 = 0x01;
/// [`PenSample::state`] bit: the tip is in contact with the surface.
pub const PEN_TOUCHING: u8 = 0x02;
/// [`PenSample::state`] bit: the primary barrel button (or the client's squeeze mapping) is held.
pub const PEN_BARREL1: u8 = 0x04;
/// [`PenSample::state`] bit: the secondary barrel button (or the client's double-tap mapping)
/// is held.
pub const PEN_BARREL2: u8 = 0x08;
/// [`PenSample::state`] bit, RESERVED: a predicted (not yet observed) sample. Never sent v1;
/// receivers MUST ignore samples carrying it until a capability negotiates otherwise
/// (design/pen-tablet-input.md §8).
pub const PEN_PREDICTED: u8 = 0x80;

/// The button subset of [`PenSample::state`].
const PEN_BUTTONS_MASK: u8 = PEN_BARREL1 | PEN_BARREL2;

/// [`PenSample::tilt_deg`] sentinel: the client has no tilt sensor / no reading.
pub const PEN_TILT_UNKNOWN: u8 = 0xFF;
/// [`PenSample::azimuth_deg`] / [`PenSample::roll_deg`] sentinel: no reading.
pub const PEN_ANGLE_UNKNOWN: u16 = 0xFFFF;
/// [`PenSample::distance`] sentinel: no hover-distance reading.
pub const PEN_DISTANCE_UNKNOWN: u16 = 0xFFFF;

/// Most samples one [`PenBatch`] can carry. Sized for coalesced capture at video-frame cadence
/// (240 Hz pen ÷ 30 fps = 8); a client producing more splits into consecutive batches.
pub const PEN_BATCH_MAX: usize = 8;

/// Wire length of one encoded [`PenSample`].
pub const PEN_SAMPLE_WIRE_LEN: usize = 21;

/// `[0xCC][0x05][flags][count][u16 seq LE]` — bytes before the first sample.
const PEN_HEADER_LEN: usize = 6;

/// Host-side failsafe (design/pen-tablet-input.md §2): a tracker still touching after this many
/// ms without a sample force-releases ([`PenTracker::force_release`]) — a client that died
/// mid-stroke must not leave the host's virtual pen inked-down forever. Far above any real
/// send cadence (a touching pen streams samples continuously), so it never fires on a live
/// slow stroke.
pub const PEN_TOUCH_TIMEOUT_MS: u32 = 200;

/// Which end of the stylus (or which mapped mode) a sample describes. A tool *switch* while in
/// range is a physical re-entry — [`PenTracker`] emits a full release + re-proximity, matching
/// how a real tablet treats each tool as its own proximity session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum PenTool {
    #[default]
    Pen = 0,
    Eraser = 1,
    /// An unrecognized wire value (a future tool from a newer client) — injectors treat it as
    /// [`PenTool::Pen`]. Inside a proximity session it inherits the session's tool instead of
    /// forcing a spurious re-entry.
    Unknown = 0xFF,
}

impl PenTool {
    fn from_u8(v: u8) -> PenTool {
        match v {
            0 => PenTool::Pen,
            1 => PenTool::Eraser,
            _ => PenTool::Unknown,
        }
    }
}

/// One complete stylus state at one instant. All axes ride every sample (state-full — see the
/// module doc); unknown axes carry their sentinel, never 0.
///
/// `x`/`y` are normalized `0.0..=1.0` in **video-frame space** — the client maps its letterbox
/// / viewport before sending (exactly as its wire touches already do), so the host scales
/// straight to the streamed output. f32 keeps sub-pixel precision at any resolution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenSample {
    /// Bitfield of `PEN_*` state bits ([`PEN_IN_RANGE`] … [`PEN_PREDICTED`]).
    pub state: u8,
    /// Active tool. Meaningful while in range; [`PenTool::Unknown`] inherits (see [`PenTool`]).
    pub tool: PenTool,
    /// Normalized `0.0..=1.0` across the video frame (decode clamps; see [`PenBatch::decode`]).
    pub x: f32,
    /// Normalized `0.0..=1.0` across the video frame.
    pub y: f32,
    /// Tip force, `0..=65535` full scale, `0` while hovering. Injectors rescale (Windows pens
    /// are 0..1024, uinput declares its own range) — full u16 keeps every source's precision.
    pub pressure: u16,
    /// Hover distance, `0..=65534` normalized (0 = touching the hover floor), or
    /// [`PEN_DISTANCE_UNKNOWN`].
    pub distance: u16,
    /// Tilt from the surface normal, degrees `0..=90`, or [`PEN_TILT_UNKNOWN`]. Polar form —
    /// what Apple capture and the GameStream wire both produce; injectors needing tiltX/tiltY
    /// convert (design/pen-tablet-input.md §2).
    pub tilt_deg: u8,
    /// Tilt azimuth, degrees `0..=359` clockwise from north, or [`PEN_ANGLE_UNKNOWN`].
    pub azimuth_deg: u16,
    /// Barrel roll (Apple Pencil Pro `rollAngle`), degrees `0..=359`, or [`PEN_ANGLE_UNKNOWN`].
    pub roll_deg: u16,
    /// µs since the previous sample in the same batch (`0` for the first) — preserves the
    /// coalesced capture spacing for injectors/consumers that pace.
    pub dt_us: u16,
}

impl Default for PenSample {
    fn default() -> PenSample {
        PenSample {
            state: 0,
            tool: PenTool::Pen,
            x: 0.0,
            y: 0.0,
            pressure: 0,
            distance: PEN_DISTANCE_UNKNOWN,
            tilt_deg: PEN_TILT_UNKNOWN,
            azimuth_deg: PEN_ANGLE_UNKNOWN,
            roll_deg: PEN_ANGLE_UNKNOWN,
            dt_us: 0,
        }
    }
}

impl PenSample {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.state);
        out.push(self.tool as u8);
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.pressure.to_le_bytes());
        out.extend_from_slice(&self.distance.to_le_bytes());
        out.push(self.tilt_deg);
        out.extend_from_slice(&self.azimuth_deg.to_le_bytes());
        out.extend_from_slice(&self.roll_deg.to_le_bytes());
        out.extend_from_slice(&self.dt_us.to_le_bytes());
    }

    /// Decode one sample from exactly [`PEN_SAMPLE_WIRE_LEN`] bytes (caller bounds-checks).
    /// `None` on a non-finite coordinate — an attacker-forged NaN/∞ must never reach an
    /// injector's pixel scaling. Finite out-of-range coordinates clamp to `0.0..=1.0` (a
    /// stroke drifting a hair past the letterbox edge is real input, not corruption).
    fn decode(b: &[u8]) -> Option<PenSample> {
        let f32at = |o: usize| f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
        let (x, y) = (f32at(2), f32at(6));
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        Some(PenSample {
            state: b[0],
            tool: PenTool::from_u8(b[1]),
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            pressure: u16at(10),
            distance: u16at(12),
            tilt_deg: b[14],
            azimuth_deg: u16at(15),
            roll_deg: u16at(17),
            dt_us: u16at(19),
        })
    }
}

/// One pen datagram: `[0xCC][0x05][flags][count][u16 seq LE]` + `count` ×
/// [`PEN_SAMPLE_WIRE_LEN`]-byte samples, oldest first. `flags` is reserved (sent 0, ignored on
/// decode — semantic changes take a new 0xCC kind, never a flag reinterpretation). `seq` is the
/// sender's wrapping batch counter, the reorder gate ([`pen_seq_newer`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenBatch {
    pub seq: u16,
    count: u8,
    samples: [PenSample; PEN_BATCH_MAX],
}

impl PenBatch {
    /// Build a batch from up to [`PEN_BATCH_MAX`] samples (a longer slice truncates — senders
    /// with more coalesced samples split into consecutive batches so nothing is lost).
    pub fn new(seq: u16, samples: &[PenSample]) -> PenBatch {
        let count = samples.len().min(PEN_BATCH_MAX);
        let mut buf = [PenSample::default(); PEN_BATCH_MAX];
        buf[..count].copy_from_slice(&samples[..count]);
        PenBatch {
            seq,
            count: count as u8,
            samples: buf,
        }
    }

    /// The batch's samples, oldest first.
    pub fn samples(&self) -> &[PenSample] {
        &self.samples[..self.count as usize]
    }

    pub fn encode(&self) -> Vec<u8> {
        let n = self.count as usize;
        let mut out = Vec::with_capacity(PEN_HEADER_LEN + n * PEN_SAMPLE_WIRE_LEN);
        out.extend_from_slice(&[RICH_INPUT_MAGIC, RICH_PEN, 0, self.count]);
        out.extend_from_slice(&self.seq.to_le_bytes());
        for s in self.samples() {
            s.encode_into(&mut out);
        }
        out
    }

    /// Parse a pen datagram. `None` on bad tag/kind, an empty batch, or a forged coordinate
    /// (see [`PenSample::decode`]). Every read is bounded: `count` clamps to the declared
    /// value, the fixed maximum, AND what the buffer actually holds — a torn datagram yields
    /// the complete samples that arrived, never an over-read (the
    /// [`RichInput::HidReport`](super::RichInput) truncation contract).
    pub fn decode(b: &[u8]) -> Option<PenBatch> {
        if b.len() < PEN_HEADER_LEN || b[0] != RICH_INPUT_MAGIC || b[1] != RICH_PEN {
            return None;
        }
        let count = (b[3] as usize)
            .min(PEN_BATCH_MAX)
            .min((b.len() - PEN_HEADER_LEN) / PEN_SAMPLE_WIRE_LEN);
        if count == 0 {
            return None;
        }
        let mut samples = [PenSample::default(); PEN_BATCH_MAX];
        for (i, slot) in samples.iter_mut().enumerate().take(count) {
            let o = PEN_HEADER_LEN + i * PEN_SAMPLE_WIRE_LEN;
            *slot = PenSample::decode(&b[o..o + PEN_SAMPLE_WIRE_LEN])?;
        }
        Some(PenBatch {
            seq: u16::from_le_bytes([b[4], b[5]]),
            count: count as u8,
            samples,
        })
    }
}

/// The batch reorder gate: is `new` strictly newer than `last` on the wrapping u16 circle?
/// `None` (nothing applied yet) always passes. The u16 analog of
/// [`GamepadSnapshot::seq_newer`](crate::input::GamepadSnapshot::seq_newer): newer ⇔ the
/// forward distance is `1..=0x7FFF`, so reordered stale batches drop and a wrap (65535 → 0)
/// still counts as newer.
pub fn pen_seq_newer(new: u16, last: Option<u16>) -> bool {
    match last {
        None => true,
        Some(last) => (new.wrapping_sub(last) as i16) > 0,
    }
}

/// One synthesized stroke transition, in the order [`PenTracker`] emits them for a sample:
/// `ProximityIn?` → `Motion` → `TipDown?` → `ButtonsChanged?` → `TipUp?` → `ProximityOut?`.
/// `Motion` precedes `TipDown` so the contact lands where the sample says; a release
/// ([`PenTracker::force_release`] or an out-of-range sample) orders
/// `ButtonsChanged?` → `TipUp?` → `ProximityOut` so nothing is left held.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PenTransition {
    /// The pen entered hover range. Injectors map [`PenTool::Unknown`] to a plain pen.
    ProximityIn { tool: PenTool },
    /// Position + all axes moved to this sample's values (emitted for every in-range sample).
    Motion { sample: PenSample },
    /// The tip made contact.
    TipDown,
    /// Barrel buttons changed: `pressed` / `released` are disjoint `PEN_BARREL*` subsets.
    ButtonsChanged { pressed: u8, released: u8 },
    /// The tip lifted.
    TipUp,
    /// The pen left hover range.
    ProximityOut,
}

/// The host-side stroke state machine (one per session): diffs state-full [`PenSample`]s
/// against tracked state and appends the synthesized [`PenTransition`]s. Pure and clock-free —
/// the owner arms its own [`PEN_TOUCH_TIMEOUT_MS`] timer over [`PenTracker::is_active`] and
/// calls [`PenTracker::force_release`] when it fires (and on session teardown).
#[derive(Debug, Default)]
pub struct PenTracker {
    last_seq: Option<u16>,
    in_range: bool,
    touching: bool,
    buttons: u8,
    tool: PenTool,
}

impl PenTracker {
    /// Apply one decoded batch, appending transitions to `out` (callers reuse the buffer). A
    /// stale batch ([`pen_seq_newer`]) is dropped whole with no transitions. Samples carrying
    /// the reserved [`PEN_PREDICTED`] bit are skipped (never injected — module doc).
    pub fn apply(&mut self, batch: &PenBatch, out: &mut Vec<PenTransition>) {
        if !pen_seq_newer(batch.seq, self.last_seq) {
            return;
        }
        self.last_seq = Some(batch.seq);
        for s in batch.samples() {
            if s.state & PEN_PREDICTED != 0 {
                continue;
            }
            self.apply_sample(s, out);
        }
    }

    /// The tracker holds live state a dead client could leave stuck (in range, or mid-stroke).
    pub fn is_active(&self) -> bool {
        self.in_range || self.touching
    }

    /// Release everything held (buttons → tip → proximity) — the [`PEN_TOUCH_TIMEOUT_MS`]
    /// failsafe and session teardown. Keeps the seq gate armed so a late stale datagram from
    /// the dead stroke cannot re-apply after the release.
    pub fn force_release(&mut self, out: &mut Vec<PenTransition>) {
        if self.buttons != 0 {
            out.push(PenTransition::ButtonsChanged {
                pressed: 0,
                released: self.buttons,
            });
            self.buttons = 0;
        }
        if self.touching {
            out.push(PenTransition::TipUp);
            self.touching = false;
        }
        if self.in_range {
            out.push(PenTransition::ProximityOut);
            self.in_range = false;
        }
    }

    fn apply_sample(&mut self, s: &PenSample, out: &mut Vec<PenTransition>) {
        let touching = s.state & PEN_TOUCHING != 0;
        // Touching implies in-range (a contact IS a proximity) — normalize here once so every
        // consumer sees coherent states.
        let in_range = touching || s.state & PEN_IN_RANGE != 0;
        if !in_range {
            self.force_release(out);
            return;
        }
        // Unknown inherits the session's tool (a newer client's future tool must not thrash
        // proximity); outside a session it grounds to the default.
        let tool = match s.tool {
            PenTool::Unknown if self.in_range => self.tool,
            t => t,
        };
        // A tool switch mid-session is a physical re-entry (see [`PenTool`]).
        if self.in_range && tool != self.tool {
            self.force_release(out);
        }
        if !self.in_range {
            out.push(PenTransition::ProximityIn { tool });
            self.in_range = true;
        }
        self.tool = tool;
        out.push(PenTransition::Motion { sample: *s });
        if touching && !self.touching {
            out.push(PenTransition::TipDown);
            self.touching = true;
        }
        let buttons = s.state & PEN_BUTTONS_MASK;
        if buttons != self.buttons {
            out.push(PenTransition::ButtonsChanged {
                pressed: buttons & !self.buttons,
                released: self.buttons & !buttons,
            });
            self.buttons = buttons;
        }
        if !touching && self.touching {
            out.push(PenTransition::TipUp);
            self.touching = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::RichInput;

    fn hover(x: f32, y: f32) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE,
            x,
            y,
            distance: 300,
            ..Default::default()
        }
    }

    fn touch(x: f32, y: f32, pressure: u16) -> PenSample {
        PenSample {
            state: PEN_IN_RANGE | PEN_TOUCHING,
            x,
            y,
            pressure,
            distance: 0,
            tilt_deg: 35,
            azimuth_deg: 180,
            roll_deg: 90,
            ..Default::default()
        }
    }

    #[test]
    fn pen_batch_roundtrip_and_truncation() {
        let samples = [hover(0.25, 0.5), touch(0.26, 0.5, 40000), {
            let mut s = touch(0.27, 0.51, 42000);
            s.state |= PEN_BARREL1;
            s.dt_us = 4167;
            s
        }];
        let b = PenBatch::new(7, &samples);
        let d = b.encode();
        assert_eq!(d[0], RICH_INPUT_MAGIC);
        assert_eq!(d.len(), 6 + 3 * PEN_SAMPLE_WIRE_LEN);
        let back = PenBatch::decode(&d).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.samples(), &samples);

        // A torn datagram yields exactly the complete samples that arrived — never an
        // over-read, never a partial sample.
        let torn = PenBatch::decode(&d[..6 + 2 * PEN_SAMPLE_WIRE_LEN + 5]).unwrap();
        assert_eq!(torn.samples(), &samples[..2]);
        // Header-only / empty batches and short buffers are rejected whole.
        assert!(PenBatch::decode(&d[..PEN_HEADER_LEN]).is_none());
        assert!(PenBatch::decode(&PenBatch::new(0, &[]).encode()).is_none());
        // Wrong tag / wrong kind are None before any read.
        let mut bad = d.clone();
        bad[0] = 0xC8;
        assert!(PenBatch::decode(&bad).is_none());
        let mut bad = d;
        bad[1] = 0x01; // RICH_TOUCHPAD
        assert!(PenBatch::decode(&bad).is_none());
    }

    #[test]
    fn pen_batch_oversize_truncates_and_flags_reserved() {
        // 10 samples truncate to PEN_BATCH_MAX on construction (senders split instead).
        let many: Vec<PenSample> = (0..10).map(|i| hover(i as f32 / 10.0, 0.5)).collect();
        let b = PenBatch::new(1, &many);
        assert_eq!(b.samples().len(), PEN_BATCH_MAX);
        // A declared count larger than the payload clamps to what arrived.
        let mut d = b.encode();
        d[3] = 200;
        assert_eq!(PenBatch::decode(&d).unwrap().samples().len(), PEN_BATCH_MAX);
        // A nonzero reserved flags byte still parses (receivers MUST ignore).
        d[2] = 0xAA;
        assert!(PenBatch::decode(&d).is_some());
    }

    #[test]
    fn pen_batch_rejects_forged_floats_and_clamps_stragglers() {
        // NaN / ∞ coordinates kill the whole batch — nothing legitimate produces them.
        for forged in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut s = touch(0.5, 0.5, 1);
            s.x = forged;
            assert!(PenBatch::decode(&PenBatch::new(0, &[s]).encode()).is_none());
        }
        // A finite coordinate a hair outside the letterbox clamps instead (real input).
        let mut s = hover(0.5, 0.5);
        s.x = -0.01;
        s.y = 1.25;
        let back = PenBatch::decode(&PenBatch::new(0, &[s]).encode()).unwrap();
        assert_eq!((back.samples()[0].x, back.samples()[0].y), (0.0, 1.0));
    }

    #[test]
    fn pen_plane_is_disjoint_from_rich_input() {
        // A pen datagram shares the 0xCC tag but is NOT a RichInput: a pre-pen host takes the
        // documented unknown-kind drop, and the pen decoder rejects every RichInput kind.
        let d = PenBatch::new(3, &[touch(0.5, 0.5, 100)]).encode();
        assert!(RichInput::decode(&d).is_none());
        let rich = RichInput::Touchpad {
            pad: 0,
            finger: 0,
            active: true,
            x: 1,
            y: 2,
        }
        .encode();
        assert!(PenBatch::decode(&rich).is_none());
    }

    #[test]
    fn seq_gate_wraps_and_drops_stale() {
        assert!(pen_seq_newer(0, None));
        assert!(pen_seq_newer(6, Some(5)));
        assert!(!pen_seq_newer(5, Some(5)));
        assert!(!pen_seq_newer(4, Some(5)));
        assert!(pen_seq_newer(2, Some(0xFFFE))); // wrap
        assert!(!pen_seq_newer(0xFFFE, Some(2))); // stale across the wrap
    }

    /// Drives a tracker and returns the transitions of one batch.
    fn run(t: &mut PenTracker, seq: u16, samples: &[PenSample]) -> Vec<PenTransition> {
        let mut out = Vec::new();
        t.apply(&PenBatch::new(seq, samples), &mut out);
        out
    }

    #[test]
    fn tracker_full_stroke_lifecycle() {
        let mut t = PenTracker::default();
        // Hover in → ProximityIn + Motion.
        let h = hover(0.2, 0.2);
        assert_eq!(
            run(&mut t, 0, &[h]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: h },
            ]
        );
        // Contact: Motion precedes TipDown so ink lands at the sample's position.
        let c = touch(0.21, 0.2, 30000);
        assert_eq!(
            run(&mut t, 1, &[c]),
            vec![PenTransition::Motion { sample: c }, PenTransition::TipDown]
        );
        assert!(t.is_active());
        // Drag: motion only.
        let m = touch(0.3, 0.25, 45000);
        assert_eq!(
            run(&mut t, 2, &[m]),
            vec![PenTransition::Motion { sample: m }]
        );
        // Lift back to hover, then leave range: buttons(none) → TipUp, then ProximityOut.
        let l = hover(0.3, 0.25);
        assert_eq!(
            run(&mut t, 3, &[l]),
            vec![PenTransition::Motion { sample: l }, PenTransition::TipUp]
        );
        let gone = PenSample::default(); // state 0 = out of range
        assert_eq!(run(&mut t, 4, &[gone]), vec![PenTransition::ProximityOut]);
        assert!(!t.is_active());
    }

    #[test]
    fn tracker_self_heals_lost_transitions() {
        let mut t = PenTracker::default();
        // The hover batch AND the tip-down batch were lost: the first surviving mid-stroke
        // sample synthesizes the whole entry.
        let m = touch(0.5, 0.5, 20000);
        assert_eq!(
            run(&mut t, 10, &[m]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: m },
                PenTransition::TipDown,
            ]
        );
        // The lift batch was lost; the next out-of-range sample heals it fully.
        assert_eq!(
            run(&mut t, 11, &[PenSample::default()]),
            vec![PenTransition::TipUp, PenTransition::ProximityOut]
        );
    }

    #[test]
    fn tracker_drops_stale_batches_whole() {
        let mut t = PenTracker::default();
        let c = touch(0.5, 0.5, 100);
        assert!(!run(&mut t, 5, &[c]).is_empty());
        // A reordered older batch (a hover from before the contact) must not rewind the stroke.
        assert!(run(&mut t, 4, &[hover(0.4, 0.4)]).is_empty());
        assert!(t.is_active());
    }

    #[test]
    fn tracker_buttons_and_eraser_reentry() {
        let mut t = PenTracker::default();
        let mut held = touch(0.5, 0.5, 100);
        held.state |= PEN_BARREL1;
        // Buttons apply after TipDown on entry…
        assert_eq!(
            run(&mut t, 0, &[held]),
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: held },
                PenTransition::TipDown,
                PenTransition::ButtonsChanged {
                    pressed: PEN_BARREL1,
                    released: 0
                },
            ]
        );
        // …swap BARREL1 → BARREL2 in one sample: one delta, both directions.
        let mut swapped = held;
        swapped.state = (swapped.state & !PEN_BARREL1) | PEN_BARREL2;
        assert_eq!(
            run(&mut t, 1, &[swapped]),
            vec![
                PenTransition::Motion { sample: swapped },
                PenTransition::ButtonsChanged {
                    pressed: PEN_BARREL2,
                    released: PEN_BARREL1
                },
            ]
        );
        // Tool switch to eraser mid-contact = full release + re-entry as the eraser, with the
        // held button released first so nothing sticks across tools.
        let mut erase = touch(0.5, 0.5, 200);
        erase.tool = PenTool::Eraser;
        assert_eq!(
            run(&mut t, 2, &[erase]),
            vec![
                PenTransition::ButtonsChanged {
                    pressed: 0,
                    released: PEN_BARREL2
                },
                PenTransition::TipUp,
                PenTransition::ProximityOut,
                PenTransition::ProximityIn {
                    tool: PenTool::Eraser
                },
                PenTransition::Motion { sample: erase },
                PenTransition::TipDown,
            ]
        );
        // Unknown tool inside the session inherits (no re-entry thrash from a newer client).
        let mut unk = touch(0.51, 0.5, 210);
        unk.tool = PenTool::Unknown;
        assert_eq!(
            run(&mut t, 3, &[unk]),
            vec![PenTransition::Motion { sample: unk }]
        );
    }

    #[test]
    fn tracker_force_release_and_late_stale_datagram() {
        let mut t = PenTracker::default();
        let mut held = touch(0.5, 0.5, 100);
        held.state |= PEN_BARREL2;
        run(&mut t, 100, &[held]);
        // The 200 ms failsafe / teardown: buttons → tip → proximity, all released.
        let mut out = Vec::new();
        t.force_release(&mut out);
        assert_eq!(
            out,
            vec![
                PenTransition::ButtonsChanged {
                    pressed: 0,
                    released: PEN_BARREL2
                },
                PenTransition::TipUp,
                PenTransition::ProximityOut,
            ]
        );
        assert!(!t.is_active());
        // Idempotent.
        let mut out = Vec::new();
        t.force_release(&mut out);
        assert!(out.is_empty());
        // The seq gate survives the release: a late datagram from the dead stroke is stale.
        assert!(run(&mut t, 99, &[held]).is_empty());
        assert!(!t.is_active());
        // But the stroke after it proceeds normally.
        assert!(!run(&mut t, 101, &[held]).is_empty());
        assert!(t.is_active());
    }

    #[test]
    fn tracker_skips_reserved_predicted_samples() {
        let mut t = PenTracker::default();
        let mut p = touch(0.5, 0.5, 100);
        p.state |= PEN_PREDICTED;
        // A v1 host must never inject a predicted sample — and skipping must not corrupt
        // tracking for the real samples around it.
        let real = touch(0.6, 0.5, 120);
        let out = run(&mut t, 0, &[p, real]);
        assert_eq!(
            out,
            vec![
                PenTransition::ProximityIn { tool: PenTool::Pen },
                PenTransition::Motion { sample: real },
                PenTransition::TipDown,
            ]
        );
    }
}
