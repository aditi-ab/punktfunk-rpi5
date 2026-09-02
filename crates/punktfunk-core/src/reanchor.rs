//! Post-loss display freeze: withhold concealed decoder output until a proven re-anchor.
//!
//! Hardware decoders return Ok on a missing reference and conceal; presenting that is the
//! gray-plate artifact. Every client holds the last good picture instead, and lifts only on
//! a real IDR, an honoured [`USER_FLAG_RECOVERY_ANCHOR`], the second [`USER_FLAG_RECOVERY_POINT`]
//! since the loss, or a local recovery-point SEI ([`ReanchorGate::on_local_recovery`]).
//!
//! One shared state machine so embedders do not re-derive it. Time-driven but takes `now`
//! so tests need no clock; C ABI wrappers pass `Instant::now()`. Lanes without a bitstream
//! parser never call the local or corroborated paths; the wire contract is unchanged.
//!
//! [`USER_FLAG_RECOVERY_ANCHOR`] is a host claim about the client decoder. A parser that can
//! prove the named references were concealed passes [`AnchorEvidence::ReferencesDamaged`]
//! through [`ReanchorGate::on_decoded_corroborated`]: the freeze stays up, the backstop
//! fires on its original deadline, and the client escalates to an IDR. Silence is
//! [`AnchorEvidence::Unavailable`].
//!
//! [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
//! [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR

use crate::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};
use std::time::{Duration, Instant};

/// Consecutive no-output AUs that force a keyframe request. 3 ≈ 50 ms at 60 Hz: skip a one-frame
/// decoder hiccup, still recover a lost initial IDR before the picture stays dark.
pub const NO_OUTPUT_KEYFRAME_STREAK: u32 = 3;

/// Longest the freeze waits for a re-anchor before [`ReanchorGate::poll`] re-asks. The deadline
/// never presents the concealed picture: it re-asks and keeps holding. 500 ms is well above a
/// recovery-IDR round-trip on a live link and short enough that a stalled host still recovers.
pub const REANCHOR_FREEZE_MAX: Duration = Duration::from_millis(500);

/// Intra-refresh [`USER_FLAG_RECOVERY_POINT`]s since the latest loss before the freeze lifts
/// without an IDR. Two, not one: the first wave boundary after a loss is only half-healed
/// (stripes swept before the loss still reference the lost frame). Every arm resets the count.
///
/// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
pub const REANCHOR_MARKS_TO_LIFT: u32 = 2;

/// Extra freeze time each live recovery mark buys. Must exceed one intra-refresh wave
/// (~0.5 s) with margin so a healing stream is not pre-empted by the IDR floor. When marks
/// stop, the deadline lapses and the recovery-IDR floor still fires.
pub const RECOVERY_MARK_PATIENCE: Duration = Duration::from_millis(1500);

/// How long a gap-arm's expected `frames_dropped` climb stays pre-credited in
/// [`ReanchorGate::poll`]. The reassembler books the same loss ~120 ms later; without the
/// credit a fast LTR-RFI lift between the two signals re-freezes a healed stream. 1 s covers
/// the 120 ms loss window plus jitter, and expires so leftover credit cannot mask a later climb.
pub const DROP_CREDIT_WINDOW: Duration = Duration::from_millis(1000);

/// Frames skipped when `got` is ahead of `expected`, else `None`. Indices wrap: wrapping
/// subtraction split at the half-space — small positive is a forward gap, top half is a
/// straggler already passed.
pub fn index_gap(expected: u32, got: u32) -> Option<u32> {
    let ahead = got.wrapping_sub(expected);
    (ahead != 0 && ahead < u32::MAX / 2).then_some(ahead)
}

/// Fold one decoded frame: IDR or honoured LTR-RFI anchor lifts immediately; a recovery mark
/// is only half a re-anchor so [`REANCHOR_MARKS_TO_LIFT`] must accumulate. Returns `(lift,
/// new_marks)` with the count reset to 0 on a lift. The caller applies [`AnchorEvidence`]
/// before `has_anchor` so this stays a pure statement of the wire rules.
fn reanchor_after_frame(
    is_keyframe: bool,
    has_anchor: bool,
    has_mark: bool,
    marks: u32,
) -> (bool, u32) {
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

/// Local bitstream-parser view of intra-refresh recovery on one frame — the in-band counterpart
/// of [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT). Two facts, not one
/// verdict: a recovery-point SEI promises a correct picture N frames later for a decoder that
/// lost references *before* the SEI, and nothing for a loss *after* it. Only
/// [`ReanchorGate::on_local_recovery`] pairs the SEI against the arm, because only the gate
/// knows when the loss was. Lanes without a parser leave this [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalRecovery {
    /// This AU carried a recovery-point SEI: a heal *starts* here.
    pub sei_here: bool,
    /// This frame is the picture a previously-seen SEI named: the heal *completed*.
    pub is_recovery_point: bool,
}

impl LocalRecovery {
    /// Both flags false. What a client without a local parser passes on every frame.
    pub const NONE: LocalRecovery = LocalRecovery {
        sei_here: false,
        is_recovery_point: false,
    };
}

/// What a local parser can say about a [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR)
/// on this frame. The host tracks whether the client *received* a reference, not whether it
/// *decoded* it from a complete chain — concealment is the gap. Three states so a lane that
/// cannot answer is not folded into "nothing wrong". Only [`Self::ReferencesDamaged`] changes
/// behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnchorEvidence {
    /// No local parser: the host's claim stands. Default for MediaCodec, VideoToolbox, C ABI.
    #[default]
    Unavailable,
    /// Predicts only from pictures that decoded from a complete reference chain.
    ReferencesClean,
    /// This AU predicts from a concealed picture, so the anchor must not lift.
    ReferencesDamaged,
}

impl AnchorEvidence {
    /// Honour the wire anchor unless refuted. Silence is not refutation — a lane that cannot
    /// corroborate must not become stricter than it was.
    fn honours_anchor(self) -> bool {
        !matches!(self, AnchorEvidence::ReferencesDamaged)
    }
}

/// Whether this decoded frame may reach the presenter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Show it: not frozen, or this frame is the re-anchor that lifts.
    Present,
    /// Withhold: post-loss concealment; the presenter keeps the last good picture.
    Hold,
}

/// Shared post-loss freeze. A client feeds *arm* (loss), each decoded frame
/// ([`on_decoded`](Self::on_decoded)), each no-output AU ([`on_no_output`](Self::on_no_output)),
/// and a periodic [`poll`](Self::poll). The gate emits *intents* only: `true` means the client
/// should ask for a keyframe through its own ~100 ms throttle. The gate never touches the wire.
#[derive(Debug, Clone)]
pub struct ReanchorGate {
    /// Freeze is up: withhold concealed output until a lift. Armed by any loss; cleared only
    /// by a lift in [`on_decoded`](Self::on_decoded) / [`on_local_recovery`](Self::on_local_recovery).
    awaiting: bool,
    /// Recovery marks since the latest arm. Zeroed on every arm so a fresh loss waits out two
    /// new marks ([`REANCHOR_MARKS_TO_LIFT`]).
    marks: u32,
    /// When [`poll`](Self::poll) re-asks while still holding. Never presents concealment. `None`
    /// when not frozen.
    deadline: Option<Instant>,
    /// Consecutive AUs with no decoded frame — a wedged decoder with no reassembler drop.
    /// [`NO_OUTPUT_KEYFRAME_STREAK`] trips a fresh IDR.
    no_output_streak: u32,
    /// Last `frames_dropped` [`poll`](Self::poll) saw. A climb is an unrecoverable AU.
    last_dropped: u64,
    /// A recovery-point SEI observed *since* the latest arm. Zeroed on arm so a wave that
    /// started before the loss cannot lift the freeze that loss raised.
    local_sei_since_arm: bool,
    /// Times the freeze has armed — monotonic, never reset. A local parser stamps its
    /// decode-order watermark on each move: a DPB flush can return pictures decoded *before*
    /// the loss, and wall-clock pairing cannot tell. Other clients ignore it.
    arms: u64,
    /// `frames_dropped` climb still expected from gap-armed losses. [`poll`](Self::poll)
    /// consumes this before treating a climb as a new loss ([`DROP_CREDIT_WINDOW`]).
    drop_credit: u64,
    /// [`DROP_CREDIT_WINDOW`] after the latest credited arm; `None` when credit is 0.
    drop_credit_expiry: Option<Instant>,
}

impl ReanchorGate {
    /// Seed `frames_dropped` so the first [`poll`](Self::poll) does not treat the baseline as a loss.
    pub fn new(frames_dropped: u64) -> Self {
        ReanchorGate {
            awaiting: false,
            marks: 0,
            deadline: None,
            no_output_streak: 0,
            last_dropped: frames_dropped,
            local_sei_since_arm: false,
            arms: 0,
            drop_credit: 0,
            drop_credit_expiry: None,
        }
    }

    /// Monotonic arm count. Every arm site moves it, including [`Self::on_no_output`] and
    /// [`Self::poll`]; the overdue backstop re-asks without re-arming and must not move it
    /// (re-stamping would discard a heal already in flight).
    pub fn arms(&self) -> u64 {
        self.arms
    }

    /// Arm the freeze. Zeroes the mark count and (re-)sets the backstop. Idempotent while
    /// frozen: a second loss mid-freeze re-zeroes marks and pushes the deadline.
    pub fn arm(&mut self, now: Instant) {
        self.awaiting = true;
        self.marks = 0;
        self.arms = self.arms.saturating_add(1);
        // A wave already in flight still references the picture just lost. Only an SEI seen
        // from here on may be trusted.
        self.local_sei_since_arm = false;
        self.deadline = Some(now + REANCHOR_FREEZE_MAX);
    }

    /// [`arm`](Self::arm) for a frame-index gap of known width. Pre-credits that many
    /// `frames_dropped` so the reassembler's ~120 ms later climb is not a second loss
    /// ([`DROP_CREDIT_WINDOW`]). Use plain [`arm`](Self::arm) for wedge/demotion: no climb.
    pub fn arm_expecting_drops(&mut self, now: Instant, expected_drops: u64) {
        self.arm(now);
        self.drop_credit = self.drop_credit.saturating_add(expected_drops);
        self.drop_credit_expiry = Some(now + DROP_CREDIT_WINDOW);
    }

    /// Fold a local recovery-point observation *before* [`on_decoded`](Self::on_decoded).
    /// Returns `true` when it lifted. Trustworthy only when the SEI arrived at or after the
    /// arm ([`LocalRecovery`]); a pre-arm SEI is ignored and the backstop still covers it.
    /// Lifts on the first trusted recovery point: the SEI names a wave that started after the
    /// loss, so that picture is fully swept — the same guarantee as an honoured
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR), derived locally.
    /// On an unfrozen gate it only records the SEI.
    pub fn on_local_recovery(&mut self, obs: LocalRecovery) -> bool {
        if obs.sei_here {
            self.local_sei_since_arm = true;
        }
        if !(obs.is_recovery_point && self.local_sei_since_arm && self.awaiting) {
            return false;
        }
        self.awaiting = false;
        self.deadline = None;
        self.marks = 0;
        // Spent: the next heal needs its own SEI, or one wave would lift a later loss.
        self.local_sei_since_arm = false;
        true
    }

    /// Fold one decoded frame. [`FLAG_SOF`](crate::packet::FLAG_SOF) is the host's codec-agnostic
    /// IDR; `decoder_keyframe` is libavcodec's extra IDR bit — pass `false` where the decoder
    /// does not flag keys. A live mark while frozen pushes the backstop
    /// ([`RECOVERY_MARK_PATIENCE`]). Believes an anchor on sight; parsers call
    /// [`on_decoded_corroborated`](Self::on_decoded_corroborated) instead.
    pub fn on_decoded(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        now: Instant,
    ) -> GateVerdict {
        self.on_decoded_corroborated(
            wire_flags,
            decoder_keyframe,
            AnchorEvidence::Unavailable,
            now,
        )
    }

    /// [`on_decoded`](Self::on_decoded) when the client can check the host's re-anchor claim.
    /// [`AnchorEvidence::ReferencesDamaged`] withholds an
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) lift and nothing
    /// else: a real IDR still lifts (it predicts from nothing; refusing it is a permanent freeze),
    /// and the two-mark [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT) rule
    /// plus [`RECOVERY_MARK_PATIENCE`] stay untouched (a wave heals by overwrite, not by one
    /// named reference). A refusal leaves the backstop on the arm's original deadline so
    /// [`poll`](Self::poll) still escalates to an IDR.
    pub fn on_decoded_corroborated(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        evidence: AnchorEvidence,
        now: Instant,
    ) -> GateVerdict {
        self.no_output_streak = 0;
        let is_keyframe = decoder_keyframe || (wire_flags & FLAG_SOF as u32 != 0);
        // Refuted anchors are stripped here so `reanchor_after_frame` stays the pure wire rules.
        let has_anchor = wire_flags & USER_FLAG_RECOVERY_ANCHOR != 0 && evidence.honours_anchor();
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

    /// One received AU produced no decoded frame. `true` when the streak trips: arm the freeze
    /// and ask for a keyframe, even if the client's throttle drops this iteration's request.
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

    /// Fold `frames_dropped` and the overdue backstop. `true` means ask for a keyframe: a climb
    /// beyond gap-arm credit is a fresh loss (arm), or the freeze has held [`REANCHOR_FREEZE_MAX`]
    /// with no re-anchor (re-ask and keep holding — never present concealment). A credited climb
    /// is delayed bookkeeping of a loss already armed; treating it as new is the double-arm race.
    /// The gap-arm's original deadline still re-asks if recovery never arrives.
    pub fn poll(&mut self, frames_dropped: u64, now: Instant) -> bool {
        let mut want_keyframe = false;
        if frames_dropped > self.last_dropped {
            let climb = frames_dropped - self.last_dropped;
            self.last_dropped = frames_dropped;
            if self.drop_credit_expiry.is_some_and(|e| now >= e) {
                self.drop_credit = 0;
                self.drop_credit_expiry = None;
            }
            let credited = climb.min(self.drop_credit);
            self.drop_credit -= credited;
            if self.drop_credit == 0 {
                self.drop_credit_expiry = None;
            }
            if climb > credited {
                self.arm(now);
                want_keyframe = true;
            }
        }
        if self.awaiting && self.deadline.is_some_and(|d| now >= d) {
            self.deadline = Some(now + REANCHOR_FREEZE_MAX);
            want_keyframe = true;
        }
        want_keyframe
    }

    pub fn is_holding(&self) -> bool {
        self.awaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fold `(is_keyframe, has_mark)` through `reanchor_after_frame`; return the 0-based index
    // of the first lift, or `None`. Resetting the running count to 0 models a fresh arm.
    fn lift_at(frames: &[(bool, bool)]) -> Option<usize> {
        let mut marks = 0u32;
        for (i, &(is_kf, has_mark)) in frames.iter().enumerate() {
            // Intra-refresh-mark model: no LTR-RFI path here (`an_rfi_anchor_lifts_immediately`).
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
        assert_eq!(REANCHOR_MARKS_TO_LIFT, 2);
        assert_eq!(lift_at(&[(false, true)]), None);
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false)]),
            None
        );
    }

    #[test]
    fn the_second_recovery_mark_lifts() {
        assert_eq!(lift_at(&[(false, true), (false, true)]), Some(1));
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false), (false, true)]),
            Some(3)
        );
    }

    #[test]
    fn a_real_keyframe_lifts_immediately() {
        assert_eq!(lift_at(&[(true, false)]), Some(0));
        assert_eq!(lift_at(&[(false, true), (true, false)]), Some(1));
    }

    #[test]
    fn a_fresh_gap_resets_the_mark_count() {
        let mut marks = 0u32;
        let (_, m) = reanchor_after_frame(false, false, true, marks);
        marks = m;
        assert_eq!(marks, 1);
        marks = 0;
        let (lift, m) = reanchor_after_frame(false, false, true, marks);
        assert!(!lift, "a single post-gap mark must not lift");
        assert_eq!(m, 1);
    }

    #[test]
    fn an_rfi_anchor_lifts_immediately() {
        let (lift, marks) = reanchor_after_frame(false, true, false, 0);
        assert!(lift, "an RFI anchor must lift the freeze immediately");
        assert_eq!(marks, 0, "a lift resets the running mark count");
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
        assert_eq!(index_gap(5, 6), Some(1));
        assert_eq!(index_gap(5, 9), Some(4));
    }

    #[test]
    fn a_straggler_behind_us_is_not_a_gap() {
        // Reassembler can emit a newer frame first; the late one must not re-arm.
        assert_eq!(index_gap(9, 5), None);
        assert_eq!(index_gap(1, 0), None);
    }

    #[test]
    fn the_index_counter_wraps_cleanly() {
        assert_eq!(index_gap(0, 0), None);
        // wrapping_sub half-space: MAX → 0 is one skipped frame, not a straggler.
        assert_eq!(index_gap(u32::MAX, 0), Some(1));
        assert_eq!(index_gap(u32::MAX, 2), Some(3));
        assert_eq!(index_gap(0, u32::MAX), None);
    }

    const SOF: u32 = FLAG_SOF as u32;
    const ANCHOR: u32 = USER_FLAG_RECOVERY_ANCHOR;
    const POINT: u32 = USER_FLAG_RECOVERY_POINT;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_clean_link_never_holds() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
        assert_eq!(g.on_decoded(SOF, true, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }

    #[test]
    fn a_gap_holds_until_the_wire_keyframe_lifts() {
        // Android/Apple: no decoder keyframe flag — lift is FLAG_SOF alone.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
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
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_second_gap_mid_freeze_resets_the_marks() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present);
    }

    #[test]
    fn the_dropped_climb_arms_and_asks() {
        let mut g = ReanchorGate::new(5);
        let now = t0();
        assert!(!g.poll(5, now), "no climb → no ask");
        assert!(g.poll(6, now), "a climb asks for a keyframe");
        assert!(g.is_holding(), "and arms the freeze");
        assert!(
            !g.poll(6, now),
            "same value → no repeat ask from the drop path"
        );
    }

    #[test]
    fn an_rfi_anchor_is_not_refrozen_by_the_same_losss_drop_climb() {
        // Gap at T+10, LTR-RFI lift at T+60, reassembler climb ~T+130: credit must absorb
        // that climb or a healed stream re-freezes.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t + Duration::from_millis(10), 1);
        assert_eq!(
            g.on_decoded(ANCHOR, false, t + Duration::from_millis(60)),
            GateVerdict::Present,
            "the anchor lifts"
        );
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "the credited climb must not ask again"
        );
        assert!(!g.is_holding(), "and must not re-freeze the healed stream");
        assert_eq!(
            g.on_decoded(0, false, t + Duration::from_millis(141)),
            GateVerdict::Present,
            "healthy P-frames keep presenting"
        );
    }

    #[test]
    fn a_climb_beyond_the_credit_is_a_fresh_loss_and_arms() {
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 2);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50));
        assert!(
            g.poll(3, t + Duration::from_millis(130)),
            "one uncredited drop → ask"
        );
        assert!(g.is_holding(), "and re-arm for the uncredited part");
    }

    #[test]
    fn the_drop_credit_expires_so_a_late_climb_still_arms() {
        // A straggler can fill the gap so no climb ever comes; leftover credit must expire.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50));
        let late = t + DROP_CREDIT_WINDOW + Duration::from_millis(1);
        assert!(
            g.poll(1, late),
            "an expired credit no longer absorbs climbs"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_credited_climb_keeps_the_unhealed_freezes_original_deadline() {
        // Unhealed: consuming the credited climb must not silence the backstop, and must
        // not push the deadline out to a later re-arm.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1);
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "credited climb: no early re-ask"
        );
        assert!(g.is_holding(), "still frozen — nothing healed it");
        assert!(
            g.poll(1, t + REANCHOR_FREEZE_MAX + Duration::from_millis(1)),
            "the overdue backstop still re-asks on the gap-arm's own deadline"
        );
        assert!(g.is_holding(), "and keeps holding, never resuming to gray");
    }

    #[test]
    fn the_no_output_streak_trips_at_three() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now), "third no-output trips the streak");
        assert!(g.is_holding());
        g.on_decoded(SOF, false, now);
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now));
    }

    #[test]
    fn an_overdue_freeze_re_asks_but_keeps_holding() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        assert!(!g.poll(0, start));
        assert!(g.is_holding());
        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(0, later), "overdue freeze re-asks for a keyframe");
        assert!(g.is_holding(), "but never resumes to the concealed picture");
    }

    fn local(sei_here: bool, is_recovery_point: bool) -> LocalRecovery {
        LocalRecovery {
            sei_here,
            is_recovery_point,
        }
    }

    #[test]
    fn a_local_recovery_point_lifts_the_freeze_without_the_backstop() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        assert!(!g.on_local_recovery(local(true, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);
        assert!(!g.on_local_recovery(local(false, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        // Heal at 120 ms, well inside REANCHOR_FREEZE_MAX — the SEI path must not wait it out.
        let mid = start + Duration::from_millis(120);
        assert!(g.on_local_recovery(local(false, true)), "the heal lifts it");
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, mid), GateVerdict::Present);
        assert!(
            !g.poll(0, mid),
            "and no keyframe is ever asked for — no IDR spike on a stream that healed itself"
        );
    }

    #[test]
    fn a_recovery_point_from_a_wave_that_predates_the_loss_is_ignored() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_local_recovery(local(true, false)));
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "a pre-loss wave's recovery point must not lift"
        );
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert!(!g.on_local_recovery(local(true, false)));
        assert!(g.on_local_recovery(local(false, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_spent_recovery_point_cannot_lift_the_next_loss() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        g.on_local_recovery(local(true, false));
        assert!(g.on_local_recovery(local(false, true)));
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the previous wave's credit is gone"
        );
        assert!(g.is_holding());
    }

    /// Both facts on one AU (SEI count 0). The pairing is order, not "needs two frames".
    #[test]
    fn an_sei_that_is_its_own_recovery_point_lifts_on_that_frame() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.on_local_recovery(local(true, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_client_without_a_local_parser_sees_no_behaviour_change() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        for _ in 0..8 {
            assert!(!g.on_local_recovery(LocalRecovery::NONE));
            assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        }
        assert!(
            g.is_holding(),
            "still frozen — only the wire can lift this one"
        );
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_recovery_point_on_an_unfrozen_gate_is_not_banked() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_local_recovery(local(true, true)));
        assert!(!g.is_holding());
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the pre-arm SEI was cleared by the arm"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn every_arm_site_moves_the_arm_counter_and_the_backstop_does_not() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        assert_eq!(g.arms(), 0, "a fresh gate has never armed");

        g.arm(start);
        assert_eq!(g.arms(), 1);
        g.arm(start);
        assert_eq!(g.arms(), 2);

        assert!(g.poll(1, start));
        assert_eq!(g.arms(), 3);

        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(1, later));
        assert_eq!(g.arms(), 3, "the backstop re-asks, it does not re-arm");

        let mut g = ReanchorGate::new(0);
        assert!(!g.on_no_output(start));
        assert!(!g.on_no_output(start));
        assert_eq!(g.arms(), 0, "the streak has not tripped yet");
        assert!(g.on_no_output(start));
        assert_eq!(g.arms(), 1);
    }

    #[test]
    fn a_live_mark_stream_pushes_the_deadline_out() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // Mark after the original deadline: patience must suppress the overdue ask.
        let t = start + REANCHOR_FREEZE_MAX + Duration::from_millis(10);
        assert_eq!(g.on_decoded(POINT, false, t), GateVerdict::Hold);
        assert!(!g.poll(0, t + Duration::from_millis(1)));
        assert!(g.is_holding());
    }

    use AnchorEvidence::{ReferencesClean, ReferencesDamaged, Unavailable};

    #[test]
    fn an_anchor_whose_references_the_decoder_concealed_does_not_lift() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "a refuted anchor is not a re-anchor"
        );
        assert!(g.is_holding(), "and the freeze stays up");
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_real_idr_lifts_even_while_the_evidence_refutes_anchors() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(0, true, ReferencesDamaged, now),
            GateVerdict::Present,
            "the IDR re-anchors regardless of what the anchor evidence says"
        );
        assert!(!g.is_holding());

        // FLAG_SOF: lanes whose decoder does not flag IDRs.
        let mut g = ReanchorGate::new(0);
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(SOF, false, ReferencesDamaged, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    #[test]
    fn a_corroborated_anchor_lifts_on_the_first_occurrence() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesClean, now),
            GateVerdict::Hold,
            "an ordinary frame is still withheld"
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    #[test]
    fn an_uncorroborated_lane_behaves_exactly_as_it_always_has() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, Unavailable, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, Unavailable, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());

        let mut wire = ReanchorGate::new(0);
        let mut corroborated = ReanchorGate::new(0);
        wire.arm(now);
        corroborated.arm(now);
        for flags in [0, POINT, 0, ANCHOR, SOF, 0] {
            assert_eq!(
                wire.on_decoded(flags, false, now),
                corroborated.on_decoded_corroborated(flags, false, Unavailable, now),
                "flags {flags:#x} diverged between the two entry points"
            );
            assert_eq!(wire.is_holding(), corroborated.is_holding());
        }
    }

    #[test]
    fn a_refused_anchor_leaves_the_backstop_on_its_original_deadline() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        for ms in [10, 100, 300, 490] {
            assert_eq!(
                g.on_decoded_corroborated(
                    ANCHOR,
                    false,
                    ReferencesDamaged,
                    start + Duration::from_millis(ms)
                ),
                GateVerdict::Hold
            );
            assert!(!g.poll(0, start + Duration::from_millis(ms)), "not yet due");
        }
        let overdue = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(
            g.poll(0, overdue),
            "the backstop fires on the arm's own deadline — the refusals did not extend it"
        );
        assert!(
            g.is_holding(),
            "and it keeps holding, never resuming to gray"
        );
    }

    #[test]
    fn refuted_anchors_do_not_disturb_the_two_mark_rule() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "mark #1 is still only half a re-anchor"
        );
        // Refused anchor between marks must not consume or reset the count.
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "mark #2 lifts exactly as it does on the wire path"
        );
        assert!(!g.is_holding());
    }

    #[test]
    fn damaged_evidence_alone_neither_holds_nor_arms_an_unfrozen_gate() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "an unfrozen gate presents; the evidence is about anchors, not about frames"
        );
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }
}
