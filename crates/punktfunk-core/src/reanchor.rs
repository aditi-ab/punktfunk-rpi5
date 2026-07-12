//! Post-loss display freeze — the shared "freeze-until-reanchor" gate.
//!
//! After an unrecoverable reference loss the hardware decoder does **not** error: it *conceals* the
//! reference-missing delta frames (on RADV, the DPB-and-output-COINCIDE path paints a gray plate with
//! the new frame's motion on top) and returns Ok. Displaying that is the "gray frames mid-stream"
//! artifact. Instead every client freezes on the last good picture — withholds the concealed frames
//! from its presenter, which keeps redrawing the held frame — and lifts the freeze ONLY on a proven
//! clean re-anchor: a real IDR, an LTR-RFI recovery anchor ([`USER_FLAG_RECOVERY_ANCHOR`]), or the
//! second intra-refresh recovery mark ([`USER_FLAG_RECOVERY_POINT`]) since the loss.
//!
//! This module owns that decision so every embedder shares ONE implementation instead of re-deriving
//! it (the Linux/Deck pump in `pf-client-core`, the Windows in-process pump, the Android decode loops,
//! and — over the C ABI — the Apple client). The state machine is time-driven but takes `now` as a
//! parameter so it is unit-testable without a clock; the C ABI wrappers supply `Instant::now()`.
//!
//! [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
//! [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR

use crate::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};
use std::time::{Duration, Instant};

/// Consecutive no-output AUs that force a keyframe request. ~50 ms at 60 Hz — long enough not to fire
/// on a one-frame decoder hiccup, short enough that a lost initial IDR (or a mid-GOP join) unfreezes
/// almost immediately instead of never.
pub const NO_OUTPUT_KEYFRAME_STREAK: u32 = 3;

/// Longest the gate holds the last good frame waiting for a post-loss re-anchor keyframe before it
/// re-asks. After a reference loss the hardware decoder does not error — it conceals the
/// reference-missing deltas (on RADV, the DPB-and-output-COINCIDE path renders them as a gray plate
/// with the new frame's motion painted over it) and returns Ok, so displaying them is the "gray frames
/// mid-stream" artifact. We instead freeze on the last good picture until a fresh IDR re-anchors decode
/// — the behaviour NVIDIA already shows (its DISTINCT output image + different concealment reads as a
/// brief freeze, not gray). This cap only bounds the freeze when recovery genuinely stalls (host
/// ignores the request, or an RFI recovery that never emits a keyframe): the freeze is NEVER lifted to
/// the concealed picture — the deadline re-asks for a keyframe and keeps holding, so a glitch can never
/// become a permanent freeze while a clean re-anchor is what un-freezes. A recovery IDR round-trips well
/// under this on any live link.
pub const REANCHOR_FREEZE_MAX: Duration = Duration::from_millis(500);

/// How many host intra-refresh recovery marks ([`USER_FLAG_RECOVERY_POINT`]) must arrive since the
/// latest loss before the gate lifts its freeze on an IDR-free stream. TWO, not one: with a continuous
/// rolling wave the host marks phase-fixed wave boundaries, so the FIRST boundary after a loss is only
/// partially healed — stripes swept BEFORE the loss still reference the lost frame — and lifting there
/// would flash a partially-stale picture. The SECOND boundary guarantees a full wave swept entirely
/// after the loss, so the picture is clean. This stays correct under repeated loss because every fresh
/// arm resets the count. The cost is up to ~2 wave periods of holding the last good frame — the
/// deliberate "hold longer, never show garbage" trade.
///
/// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
pub const REANCHOR_MARKS_TO_LIFT: u32 = 2;

/// Backstop patience while a host intra-refresh heal is visibly in progress. Each recovery mark pushes
/// the freeze deadline out by this much, so a live mark stream (the host actively healing via its wave)
/// keeps the gate patiently holding the last good frame instead of tripping the IDR floor mid-heal.
/// Must exceed the inter-mark interval (one wave period, ~0.5 s) with margin; if the marks STOP (heal
/// stalled, or the host isn't running intra-refresh) the deadline lapses and the normal recovery-IDR
/// floor fires, so a real stall still recovers.
pub const RECOVERY_MARK_PATIENCE: Duration = Duration::from_millis(1500);

/// Frames skipped when `got` arrives while `expected` was the next index, or `None` if `got` is
/// contiguous (`== expected`) or a straggler we have already passed. Frame indices are u32 counters
/// that wrap, so the "ahead" test is a wrapping subtraction split at the half-space: a small positive
/// delta is a forward gap (missing frames whose dependents will decode against absent references); a
/// delta in the top half is an index behind us.
pub fn index_gap(expected: u32, got: u32) -> Option<u32> {
    let ahead = got.wrapping_sub(expected);
    (ahead != 0 && ahead < u32::MAX / 2).then_some(ahead)
}

/// Fold one decoded frame into the re-anchor state and decide whether it lifts the post-loss freeze.
///
/// `is_keyframe` — a real IDR (always a clean re-anchor). `has_anchor` — this AU carried
/// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR), the host's definitive
/// single-frame re-anchor from an LTR-RFI recovery (a clean P-frame coded against a known-good
/// reference), so it lifts on the FIRST occurrence exactly like an IDR — no two-mark wait. `has_mark` —
/// this AU carried [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT), a
/// host-signalled intra-refresh wave boundary (only *half* a re-anchor). `marks` — recovery marks seen
/// since the latest loss.
///
/// Returns `(lift, new_marks)`: `lift` clears the freeze; `new_marks` is the running count (reset to 0
/// on a lift). The two-mark rule ([`REANCHOR_MARKS_TO_LIFT`]) lives here so it is unit-tested
/// independent of the pump's channel/decoder plumbing — the first wave boundary after a loss is only
/// partially healed, so a single mark must NOT lift. An anchor (or IDR) is a *whole* re-anchor and
/// lifts immediately.
fn reanchor_after_frame(is_keyframe: bool, has_anchor: bool, has_mark: bool, marks: u32) -> (bool, u32) {
    let marks = if has_mark {
        marks.saturating_add(1)
    } else {
        marks
    };
    if is_keyframe || has_anchor || marks >= REANCHOR_MARKS_TO_LIFT {
        (true, 0)
    } else {
        (false, marks)
    }
}

/// Whether a decoded frame should be shown or withheld while the gate is (or isn't) frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Present this frame — the gate is not frozen, or this frame is the clean re-anchor that lifts it.
    Present,
    /// Withhold this frame — it is a post-loss concealment; the presenter keeps the last good picture.
    Hold,
}

/// The shared post-loss freeze state machine. A client feeds it three kinds of event — an *arm* (a
/// loss was detected: a frame-index gap, a dropped-count climb, or a decoder wedge/demotion), each
/// *decoded frame* ([`on_decoded`](Self::on_decoded), which decides present-vs-hold and interprets the
/// re-anchor wire flags), and each *no-output* AU ([`on_no_output`](Self::on_no_output)) — plus a
/// periodic [`poll`](Self::poll) that folds the dropped counter and fires the overdue backstop.
///
/// The gate emits *intents* only: [`on_no_output`](Self::on_no_output) and [`poll`](Self::poll) return
/// `true` when the client should ask the host for a keyframe. The client routes that through its own
/// ~100 ms request throttle (and the precise RFI-vs-keyframe range decision stays in the loss-range
/// tracker behind [`crate::client::NativeClient::note_frame_index`]) — the gate never touches the wire.
#[derive(Debug, Clone)]
pub struct ReanchorGate {
    /// Frozen on the last good frame, withholding the decoder's concealed output until a clean
    /// re-anchor. Armed by any loss signal; cleared only by [`on_decoded`](Self::on_decoded) lifting.
    awaiting: bool,
    /// Host intra-refresh recovery marks seen since the latest arm (see [`REANCHOR_MARKS_TO_LIFT`]).
    /// Reset to 0 whenever the freeze is (re-)armed, so a fresh loss always waits out two fresh marks.
    marks: u32,
    /// When the freeze becomes overdue and [`poll`](Self::poll) re-asks for a keyframe (holding, never
    /// resuming to the concealed picture). `None` when not frozen.
    deadline: Option<Instant>,
    /// Consecutive received AUs that produced no decoded frame — a decoder wedged on missing references
    /// with no reassembler drop to trigger recovery. A short streak forces a fresh IDR.
    no_output_streak: u32,
    /// The last `frames_dropped` value [`poll`](Self::poll) observed; a climb means the reassembler
    /// declared an AU unrecoverable and the following deltas will conceal, so arm.
    last_dropped: u64,
}

impl ReanchorGate {
    /// Seed the gate with the session's current `frames_dropped` so the first [`poll`](Self::poll)
    /// doesn't read the baseline as a loss.
    pub fn new(frames_dropped: u64) -> Self {
        ReanchorGate {
            awaiting: false,
            marks: 0,
            deadline: None,
            no_output_streak: 0,
            last_dropped: frames_dropped,
        }
    }

    /// Arm the freeze: a loss was detected (a frame-index gap, a dropped-count climb, or a decoder
    /// wedge/demotion). Zeroes the mark count so a fresh loss waits out two fresh recovery marks, and
    /// (re-)sets the backstop deadline. Idempotent while already frozen (re-arming just re-zeroes the
    /// marks and pushes the deadline — the correct behaviour when a second loss lands mid-freeze).
    pub fn arm(&mut self, now: Instant) {
        self.awaiting = true;
        self.marks = 0;
        self.deadline = Some(now + REANCHOR_FREEZE_MAX);
    }

    /// Fold one decoded frame and decide whether to present or withhold it.
    ///
    /// `wire_flags` is the AU's `user_flags` word ([`crate::session::Frame::flags`] /
    /// `PunktfunkFrame.flags`); the gate reads [`FLAG_SOF`](crate::packet::FLAG_SOF) (the host sets it
    /// only on IDR AUs — the codec-agnostic keyframe signal the platform decoders don't expose),
    /// [`USER_FLAG_RECOVERY_ANCHOR`] and [`USER_FLAG_RECOVERY_POINT`]. `decoder_keyframe` is an optional
    /// belt from decoders that flag IDRs themselves (libavcodec's `AV_FRAME_FLAG_KEY` on Linux/Windows);
    /// pass `false` where the decoder doesn't (Android MediaCodec, Apple VideoToolbox) and rely on the
    /// wire `FLAG_SOF`.
    ///
    /// A decoded frame always clears the no-output streak. When frozen, a live mark stream pushes the
    /// backstop out ([`RECOVERY_MARK_PATIENCE`]) so a healing wave isn't pre-empted by a mid-heal IDR.
    ///
    /// [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR
    /// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
    pub fn on_decoded(&mut self, wire_flags: u32, decoder_keyframe: bool, now: Instant) -> GateVerdict {
        self.no_output_streak = 0;
        let is_keyframe = decoder_keyframe || (wire_flags & FLAG_SOF as u32 != 0);
        let has_anchor = wire_flags & USER_FLAG_RECOVERY_ANCHOR != 0;
        let has_mark = wire_flags & USER_FLAG_RECOVERY_POINT != 0;
        if has_mark && self.awaiting {
            self.deadline = Some(now + RECOVERY_MARK_PATIENCE);
        }
        let (lift, marks) = reanchor_after_frame(is_keyframe, has_anchor, has_mark, self.marks);
        self.marks = marks;
        if lift {
            self.awaiting = false;
            self.deadline = None;
        }
        if self.awaiting {
            GateVerdict::Hold
        } else {
            GateVerdict::Present
        }
    }

    /// A received AU produced no decoded frame (decode error, or the decoder swallowed a
    /// reference-missing delta). Returns `true` when the streak has tripped and the client should
    /// (throttled) request a keyframe — arming the freeze at the same time, since the stream is broken
    /// regardless of whether the throttle lets the request through this iteration.
    pub fn on_no_output(&mut self, now: Instant) -> bool {
        self.no_output_streak += 1;
        if self.no_output_streak >= NO_OUTPUT_KEYFRAME_STREAK {
            self.arm(now);
            self.no_output_streak = 0;
            true
        } else {
            false
        }
    }

    /// Periodic fold of the session's `frames_dropped` counter plus the overdue backstop. Returns
    /// `true` when the client should (throttled) request a keyframe: either the drop count climbed (a
    /// fresh unrecoverable loss — arm the freeze) or the freeze has held a full [`REANCHOR_FREEZE_MAX`]
    /// window with no re-anchor (re-ask and keep holding — NEVER resume to the concealed picture; a
    /// genuinely dead stream is the QUIC idle-timeout watchdog's job, not the gate's).
    pub fn poll(&mut self, frames_dropped: u64, now: Instant) -> bool {
        let mut want_keyframe = false;
        if frames_dropped > self.last_dropped {
            self.last_dropped = frames_dropped;
            self.arm(now);
            want_keyframe = true;
        }
        if self.awaiting && self.deadline.is_some_and(|d| now >= d) {
            self.deadline = Some(now + REANCHOR_FREEZE_MAX);
            want_keyframe = true;
        }
        want_keyframe
    }

    /// Whether the gate is currently withholding concealed frames (frozen on the last good picture).
    pub fn is_holding(&self) -> bool {
        self.awaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Simulate the gate's re-anchor state across a sequence of decoded frames: each `(is_keyframe,
    // has_mark)` pair is folded through `reanchor_after_frame`, returning the frame index (0-based) at
    // which the freeze first lifts, or `None` if it never does. A reset to 0 models a fresh loss
    // re-arming the freeze (the gate zeroes the count at every arm site).
    fn lift_at(frames: &[(bool, bool)]) -> Option<usize> {
        let mut marks = 0u32;
        for (i, &(is_kf, has_mark)) in frames.iter().enumerate() {
            // The intra-refresh-mark model never carries an LTR-RFI anchor (that path is exercised by
            // `an_rfi_anchor_lifts_immediately`), so `has_anchor` is always false here.
            let (lift, m) = reanchor_after_frame(is_kf, false, has_mark, marks);
            marks = m;
            if lift {
                return Some(i);
            }
        }
        None
    }

    #[test]
    fn a_single_recovery_mark_does_not_lift() {
        // The first wave boundary after a loss is only half-healed — one mark must hold the freeze.
        assert_eq!(REANCHOR_MARKS_TO_LIFT, 2);
        assert_eq!(lift_at(&[(false, true)]), None);
        assert_eq!(lift_at(&[(false, false), (false, true), (false, false)]), None);
    }

    #[test]
    fn the_second_recovery_mark_lifts() {
        // Two marks = a full wave swept after the loss → clean re-anchor.
        assert_eq!(lift_at(&[(false, true), (false, true)]), Some(1));
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false), (false, true)]),
            Some(3)
        );
    }

    #[test]
    fn a_real_keyframe_lifts_immediately() {
        // An IDR is always a clean anchor — no marks needed.
        assert_eq!(lift_at(&[(true, false)]), Some(0));
        assert_eq!(lift_at(&[(false, true), (true, false)]), Some(1));
    }

    #[test]
    fn a_fresh_gap_resets_the_mark_count() {
        // The gate zeroes `marks` at each arm site, so one mark before a new gap plus one after must
        // NOT lift — the model resets the running count to imitate that.
        let mut marks = 0u32;
        let (_, m) = reanchor_after_frame(false, false, true, marks); // mark #1 (pre-gap)
        marks = m;
        assert_eq!(marks, 1);
        marks = 0; // a new gap re-arms the freeze → count reset
        let (lift, m) = reanchor_after_frame(false, false, true, marks); // first mark of the new wave
        assert!(!lift, "a single post-gap mark must not lift");
        assert_eq!(m, 1);
    }

    #[test]
    fn an_rfi_anchor_lifts_immediately() {
        // An LTR-RFI recovery anchor is a WHOLE re-anchor (a clean P-frame off a known-good reference),
        // so — like an IDR — it lifts on the FIRST occurrence, no two-mark wait.
        let (lift, marks) = reanchor_after_frame(false, true, false, 0);
        assert!(lift, "an RFI anchor must lift the freeze immediately");
        assert_eq!(marks, 0, "a lift resets the running mark count");
        // Even with zero prior marks and no keyframe, the anchor alone is sufficient.
        let (lift, _) = reanchor_after_frame(false, true, true, 1);
        assert!(lift, "an anchor lifts regardless of the pending mark count");
    }

    #[test]
    fn contiguous_indices_are_not_a_gap() {
        assert_eq!(index_gap(5, 5), None);
        assert_eq!(index_gap(0, 0), None);
    }

    #[test]
    fn a_forward_jump_reports_the_skip_count() {
        assert_eq!(index_gap(5, 6), Some(1)); // one frame missing
        assert_eq!(index_gap(5, 9), Some(4));
    }

    #[test]
    fn a_straggler_behind_us_is_not_a_gap() {
        // The reassembler emitted a newer frame first; the late one must not re-arm.
        assert_eq!(index_gap(9, 5), None);
        assert_eq!(index_gap(1, 0), None);
    }

    #[test]
    fn the_index_counter_wraps_cleanly() {
        // last frame = u32::MAX, so the next expected wraps to 0.
        assert_eq!(index_gap(0, 0), None);
        // waiting on u32::MAX, frame 0 arrived → MAX was skipped.
        assert_eq!(index_gap(u32::MAX, 0), Some(1));
        assert_eq!(index_gap(u32::MAX, 2), Some(3));
        // an old frame arriving just after the wrap is still a straggler.
        assert_eq!(index_gap(0, u32::MAX), None);
    }

    // ---- gate-level sequence tests (the whole behavioural contract) ----

    const SOF: u32 = FLAG_SOF as u32; // IDR wire flag
    const ANCHOR: u32 = USER_FLAG_RECOVERY_ANCHOR;
    const POINT: u32 = USER_FLAG_RECOVERY_POINT;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_clean_link_never_holds() {
        // Disarmed gate presents every frame, keyframe or not, and never asks for anything.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
        assert_eq!(g.on_decoded(SOF, true, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }

    #[test]
    fn a_gap_holds_until_the_wire_keyframe_lifts() {
        // Android/Apple path: no decoder keyframe flag, lift comes from the wire FLAG_SOF alone.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now); // frame-index gap
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold); // concealed delta withheld
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present); // IDR re-anchors
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present); // stays presenting
    }

    #[test]
    fn a_gap_lifts_on_the_first_rfi_anchor() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(ANCHOR, false, now), GateVerdict::Present);
        assert!(!g.is_holding());
    }

    #[test]
    fn a_gap_lifts_on_the_second_recovery_mark() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // first boundary: half-healed
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present); // second: clean
    }

    #[test]
    fn a_second_gap_mid_freeze_resets_the_marks() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // mark #1
        g.arm(now); // a fresh loss re-arms → mark count zeroed
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold); // this is mark #1 of the new wave
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present); // #2 lifts
    }

    #[test]
    fn the_dropped_climb_arms_and_asks() {
        let mut g = ReanchorGate::new(5);
        let now = t0();
        assert!(!g.poll(5, now), "no climb → no ask"); // baseline
        assert!(g.poll(6, now), "a climb asks for a keyframe");
        assert!(g.is_holding(), "and arms the freeze");
        assert!(!g.poll(6, now), "same value → no repeat ask from the drop path");
    }

    #[test]
    fn the_no_output_streak_trips_at_three() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now), "third no-output trips the streak");
        assert!(g.is_holding());
        // A decoded frame resets the streak.
        g.on_decoded(SOF, false, now); // lifts + resets streak
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now));
    }

    #[test]
    fn an_overdue_freeze_re_asks_but_keeps_holding() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // Before the deadline: holding, no re-ask.
        assert!(!g.poll(0, start));
        assert!(g.is_holding());
        // Past REANCHOR_FREEZE_MAX with no re-anchor: re-ask, still holding.
        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(0, later), "overdue freeze re-asks for a keyframe");
        assert!(g.is_holding(), "but never resumes to the concealed picture");
    }

    #[test]
    fn a_live_mark_stream_pushes_the_deadline_out() {
        // A healing wave (marks arriving) must not be pre-empted by the overdue IDR floor.
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // A mark past the original freeze deadline pushes it out by RECOVERY_MARK_PATIENCE.
        let t = start + REANCHOR_FREEZE_MAX + Duration::from_millis(10);
        assert_eq!(g.on_decoded(POINT, false, t), GateVerdict::Hold); // mark #1, deadline pushed
        // At a time that WOULD have been overdue on the original deadline, poll does not re-ask.
        assert!(!g.poll(0, t + Duration::from_millis(1)));
        assert!(g.is_holding());
    }
}
