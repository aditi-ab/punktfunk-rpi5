//! Converts H.264/H.265 recovery-point SEI state into per-picture
//! [`RecoveryMark`] values; streams without such SEI produce no marks.
//!
//! `sei_here` and `is_recovery_point` are independent and may both be true.
//! Existing targets are charged before a new SEI is installed, so the announcing
//! picture is excluded from a fresh H.264 count and may finish the prior wave.
//! H.264 follows §D.2.8: count `frame_num` changes, conservatively charging gaps
//! as one increment so uncertain recovery is reported late, never early.
//! H.265 follows §D.3.8: target `poc + recovery_poc_cnt` and mark the first picture
//! at or past it; a negative count therefore marks the SEI picture itself.
//! IDR/IRAP clears any outstanding target because its numbering is no longer valid.
//! Repeated SEI starts a new wave only when its implied target advances beyond the
//! outstanding target; shrinking/equal announcements do not set `sei_here`.
//! This signal is independent of wire recovery flags. The consumer, which knows
//! loss timing, must pair an SEI at/after loss with its later recovery point before
//! using it to lift a post-loss freeze.

use pf_bitstream::h264::RecoveryPoint;
use pf_bitstream::h265::RecoveryPointHevc;

/// What one planned picture is worth to the recovery-point watch.
///
/// Two independent booleans rather than one enum because a single picture can be
/// both: an encoder that emits a recovery point SEI with a zero count is saying
/// "this very picture is the clean point", and a consumer pairing SEI-then-mark
/// must see both facts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryMark {
    /// The AU that produced this picture CARRIED a recovery point SEI — an
    /// intra-refresh wave starts (or restarts) here. The consumer uses it to
    /// decide that a later [`Self::is_recovery_point`] is about a wave that began
    /// after ITS loss, which is the only case a mark may be trusted in.
    pub sei_here: bool,
    /// This picture IS the recovery point an outstanding SEI named: decoding from
    /// that SEI's AU onward, this picture's output is correct.
    pub is_recovery_point: bool,
}

impl RecoveryMark {
    /// Nothing to report — the overwhelmingly common per-picture answer.
    pub const NONE: RecoveryMark = RecoveryMark {
        sei_here: false,
        is_recovery_point: false,
    };
}

/// The outstanding recovery point, in the codec's own counting unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// H.264: increments of `frame_num` still owed, plus the `frame_num` the last
    /// charged picture carried (an increment is "this picture's differs").
    FrameNumIncrements { owed: u32, last_frame_num: u16 },
    /// H.265: the absolute `PicOrderCntVal` at or past which output is correct.
    Poc(i32),
}

/// One decoder's outstanding recovery point. Pure — no Vulkan, no allocation, two
/// words of state — so the whole rule set below is CPU-testable.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryWatch {
    target: Option<Target>,
}

impl RecoveryWatch {
    pub fn new() -> RecoveryWatch {
        RecoveryWatch { target: None }
    }

    /// Is a recovery point still outstanding? (Diagnostics and tests; the decode
    /// path reads the per-picture [`RecoveryMark`] instead.)
    pub fn is_watching(&self) -> bool {
        self.target.is_some()
    }

    /// Fold one planned H.264 picture. `frame_num` and `is_idr` come off the
    /// plan's `PicturePlan`, `sei` is its `recovery_point` field.
    ///
    /// Order matters and is deliberate: the outstanding target is CHARGED for this
    /// picture first, then a NEW SEI replaces it. So a wave-start AU that also
    /// completes the previous wave reports both facts, and the fresh target is not
    /// charged for the picture that announced it (D.2.8 counts increments *from*
    /// the SEI's picture, exclusive).
    ///
    /// "New" is load-bearing — see [`Self::starts_a_new_wave_h264`].
    pub fn note_h264(
        &mut self,
        frame_num: u16,
        is_idr: bool,
        sei: Option<RecoveryPoint>,
    ) -> RecoveryMark {
        if is_idr {
            // A real keyframe is a whole re-anchor and resets `frame_num`; any
            // pending count is meaningless past it. The client lifts on the IDR
            // itself, so nothing is lost by dropping the watch here.
            self.target = None;
        }
        let mut mark = RecoveryMark::NONE;
        // How many increments the OUTSTANDING wave still owes, measured from THIS
        // picture (0 = it is reached here); `None` when no wave was outstanding
        // when this picture arrived. It is the yardstick a fresh SEI is judged
        // against below, so it has to be captured while charging.
        let mut owed_here: Option<u32> = None;
        if let Some(Target::FrameNumIncrements {
            owed,
            last_frame_num,
        }) = self.target
        {
            // One increment per picture whose `frame_num` differs from the last
            // charged one — the honest, conservative reading (module docs).
            let owed = if frame_num != last_frame_num {
                owed.saturating_sub(1)
            } else {
                owed
            };
            owed_here = Some(owed);
            if owed == 0 {
                mark.is_recovery_point = true;
                self.target = None;
            } else {
                self.target = Some(Target::FrameNumIncrements {
                    owed,
                    last_frame_num: frame_num,
                });
            }
        }
        if let Some(rp) = sei {
            if Self::starts_a_new_wave_h264(owed_here, rp.recovery_frame_cnt) {
                mark.sei_here = true;
                if rp.recovery_frame_cnt == 0 {
                    // "Start here and this picture is already exact" — the SEI's
                    // own picture is the recovery point, no waiting.
                    mark.is_recovery_point = true;
                    self.target = None;
                } else {
                    self.target = Some(Target::FrameNumIncrements {
                        owed: rp.recovery_frame_cnt,
                        last_frame_num: frame_num,
                    });
                }
            }
            // A RE-ANNOUNCEMENT changes nothing: the outstanding target the charge
            // above left in place is the FURTHER one, and keeping it is what makes
            // the mark land late rather than early (module docs).
        }
        mark
    }

    /// Does an H.264 SEI announce a wave that STARTS here, or merely re-announce
    /// the one already outstanding?
    ///
    /// D.2.8 permits — and x264's `--intra-refresh` does — re-emitting the current
    /// wave's recovery point on every picture with a DECREASING
    /// `recovery_frame_cnt`. Under a "any SEI is a new wave" reading, the first
    /// picture after a loss then carries a fresh-looking SEI whose target is the
    /// end of a wave that began BEFORE the loss, and the consumer's arm-pairing
    /// ([`RecoveryMark::sei_here`]) lifts its freeze on a picture whose
    /// already-swept stripes still reference the lost frame: a partially stale
    /// picture presented as clean, the one outcome the pairing exists to prevent.
    ///
    /// So a new wave is one whose target lies BEYOND the outstanding one. Both
    /// counts are increments measured from this picture (the outstanding one after
    /// this picture's charge), so they compare directly. With no wave outstanding
    /// — the ordinary case, and everything after an IDR — every SEI is new.
    fn starts_a_new_wave_h264(owed_here: Option<u32>, recovery_frame_cnt: u32) -> bool {
        match owed_here {
            Some(owed) => recovery_frame_cnt > owed,
            None => true,
        }
    }

    /// Fold one planned H.265 picture — `pic_order_cnt` and `is_irap` off the
    /// plan's `PicturePlan`, `sei` its `recovery_point`.
    ///
    /// `is_irap` rather than `is_idr`: a CRA/BLA also restarts the stream's
    /// prediction structure, and the POC target that predates it cannot be
    /// compared against the POCs that follow.
    ///
    /// As on the H.264 side, only an SEI whose target ADVANCES past the
    /// outstanding one starts a new wave — the arithmetic here is exact, so the
    /// test is `poc + delta` strictly greater than the outstanding `Target::Poc`
    /// (see [`Self::starts_a_new_wave_h264`] for why).
    pub fn note_h265(
        &mut self,
        pic_order_cnt: i32,
        is_irap: bool,
        sei: Option<RecoveryPointHevc>,
    ) -> RecoveryMark {
        if is_irap {
            self.target = None;
        }
        let mut mark = RecoveryMark::NONE;
        // The outstanding target as this picture ARRIVED — the yardstick a fresh
        // SEI is judged against, captured before the charge below can clear it.
        let outstanding = match self.target {
            Some(Target::Poc(target)) => Some(target),
            _ => None,
        };
        if let Some(target) = outstanding {
            if pic_order_cnt >= target {
                mark.is_recovery_point = true;
                self.target = None;
            }
        }
        if let Some(rp) = sei {
            // D.3.8: the target is this picture's POC plus the signed delta. A
            // delta of 0 (or negative — a recovery point among leading pictures)
            // lands at or behind this picture, so it is reached immediately.
            let target = pic_order_cnt.saturating_add(rp.recovery_poc_cnt);
            if outstanding.is_none_or(|t| target > t) {
                mark.sei_here = true;
                if pic_order_cnt >= target {
                    mark.is_recovery_point = true;
                    self.target = None;
                } else {
                    self.target = Some(Target::Poc(target));
                }
            }
            // Otherwise it re-announces the wave already outstanding (or an
            // earlier point within it): the further target the charge left in
            // place stands, and this picture reports no fresh wave start.
        }
        mark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_sei(recovery_frame_cnt: u32) -> Option<RecoveryPoint> {
        Some(RecoveryPoint {
            recovery_frame_cnt,
            // The realistic value for a rolling wave: an encoder that filters
            // across the refresh boundary cannot promise bit-exact output, and
            // says so. The watch must not care — see
            // `an_approximate_recovery_point_still_marks`.
            exact_match: false,
            broken_link: false,
        })
    }

    fn h265_sei(recovery_poc_cnt: i32) -> Option<RecoveryPointHevc> {
        Some(RecoveryPointHevc {
            recovery_poc_cnt,
            exact_match: false,
            broken_link: false,
        })
    }

    /// The wave: an SEI announces a recovery point N increments out, and exactly
    /// the Nth picture after it is marked — not the ones before, not the ones
    /// after. This is the whole point of the module, on the codec whose counting
    /// unit is the awkward one.
    #[test]
    fn an_h264_wave_marks_the_picture_the_sei_counted_to() {
        let mut w = RecoveryWatch::new();
        // The wave starts on frame_num 10 and is three increments long.
        let start = w.note_h264(10, false, h264_sei(3));
        assert!(start.sei_here, "the wave start is reported");
        assert!(
            !start.is_recovery_point,
            "a count of 3 is not reached on the picture that announced it"
        );
        assert!(w.is_watching());
        assert_eq!(w.note_h264(11, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(12, false, None), RecoveryMark::NONE);
        let healed = w.note_h264(13, false, None);
        assert!(healed.is_recovery_point, "the third increment is the heal");
        assert!(!healed.sei_here);
        assert!(!w.is_watching(), "and the watch is spent");
        // Nothing after it is marked — a mark is a moment, not a state.
        assert_eq!(w.note_h264(14, false, None), RecoveryMark::NONE);
    }

    /// `frame_num` does not advance for a non-reference picture, and D.2.8 counts
    /// INCREMENTS — so a repeated `frame_num` must not be charged, or the mark
    /// lands early on a picture the wave has not reached.
    #[test]
    fn a_repeated_frame_num_spends_no_increment() {
        let mut w = RecoveryWatch::new();
        w.note_h264(4, false, h264_sei(2));
        // Two pictures at the same frame_num: one increment, not two.
        assert_eq!(w.note_h264(5, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(5, false, None), RecoveryMark::NONE);
        assert!(
            w.note_h264(6, false, None).is_recovery_point,
            "the second true increment completes the count"
        );
    }

    /// A `frame_num` GAP (the lost reference frame this whole program exists for)
    /// skips several increments but is charged one, so the mark can only land
    /// LATE. Late is today's behaviour (the freeze backstop); early would present
    /// a half-swept picture as clean, which is the one outcome that must be
    /// impossible.
    #[test]
    fn a_frame_num_gap_delays_the_mark_it_never_advances_it() {
        let mut w = RecoveryWatch::new();
        w.note_h264(100, false, h264_sei(2));
        // frame_num jumps 101 → 104: three real increments, charged as one.
        assert_eq!(w.note_h264(104, false, None), RecoveryMark::NONE);
        assert!(
            w.note_h264(105, false, None).is_recovery_point,
            "the count still has to be walked off — never short-circuited"
        );
    }

    /// `recovery_frame_cnt == 0` means "start decoding here; this picture is
    /// already exact". Both facts land on the one picture.
    #[test]
    fn a_zero_count_marks_the_pictures_that_carries_the_sei() {
        let mut w = RecoveryWatch::new();
        let m = w.note_h264(7, false, h264_sei(0));
        assert!(m.sei_here && m.is_recovery_point);
        assert!(!w.is_watching());
    }

    /// A wave that reaches PAST the outstanding one supersedes it: the newest SEI
    /// is the encoder's current statement about where the picture becomes correct,
    /// and its count is measured from ITS own picture.
    #[test]
    fn a_new_sei_that_reaches_further_replaces_an_outstanding_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h264(1, false, h264_sei(2));
        // A second, LONGER wave two pictures in: 4 increments from here, against
        // the 1 the first wave still owed.
        let restart = w.note_h264(3, false, h264_sei(4));
        assert!(restart.sei_here, "it reaches further — a fresh wave starts");
        assert!(!restart.is_recovery_point);
        for fnum in 4..7 {
            assert_eq!(w.note_h264(fnum, false, None), RecoveryMark::NONE);
        }
        assert!(
            w.note_h264(7, false, None).is_recovery_point,
            "the SECOND wave's count is what completes, not the first's"
        );
    }

    /// The re-announcement rule, on the codec whose counting unit is the awkward
    /// one. x264's `--intra-refresh` re-emits the CURRENT wave's recovery point on
    /// every picture with a DECREASING `recovery_frame_cnt` (legal under D.2.8).
    /// Read as a wave START, every one of those would let a consumer that armed a
    /// freeze mid-wave lift on the tail of a wave that began BEFORE its loss — a
    /// half-stale picture presented as clean.
    #[test]
    fn a_decreasing_re_announcement_is_the_same_wave_not_a_new_one() {
        let mut w = RecoveryWatch::new();
        // The wave starts on frame_num 10, four increments long, and re-announces
        // itself on every picture: 4, 3, 2, 1, 0.
        let start = w.note_h264(10, false, h264_sei(4));
        assert!(start.sei_here, "the FIRST announcement is a wave start");
        for (fnum, cnt) in [(11, 3), (12, 2), (13, 1)] {
            let m = w.note_h264(fnum, false, h264_sei(cnt));
            assert!(
                !m.sei_here,
                "frame_num {fnum} re-announces the same wave — not a fresh start"
            );
            assert!(!m.is_recovery_point, "and the wave is not there yet");
        }
        // The wave completes, and the trailing `recovery_frame_cnt == 0` on that
        // very picture must NOT read as a brand-new wave either: a consumer that
        // armed mid-wave would otherwise see sei_here + is_recovery_point on one
        // picture and lift on the wave it already discounted.
        let healed = w.note_h264(14, false, h264_sei(0));
        assert!(healed.is_recovery_point, "the wave really did complete");
        assert!(
            !healed.sei_here,
            "…but nothing NEW started here — the count only walked to zero"
        );
        assert!(!w.is_watching());
    }

    /// The H.265 twin, in exact POC arithmetic: a wave announced at POC 10 for POC
    /// 20, re-announced on every picture with the same absolute target.
    #[test]
    fn an_h265_re_announcement_of_the_same_target_is_not_a_new_wave() {
        let mut w = RecoveryWatch::new();
        assert!(w.note_h265(10, false, h265_sei(10)).sei_here);
        for poc in 11..20 {
            let m = w.note_h265(poc, false, h265_sei(20 - poc));
            assert!(!m.sei_here, "poc {poc} re-announces target 20");
            assert!(!m.is_recovery_point);
        }
        let healed = w.note_h265(20, false, h265_sei(0));
        assert!(healed.is_recovery_point);
        assert!(!healed.sei_here, "the target did not advance past 20");
        // A genuinely later wave DOES start, and is reported.
        assert!(w.note_h265(21, false, h265_sei(8)).sei_here);
    }

    /// An SEI that reaches SHORT of the outstanding target is a re-announcement
    /// too, and the further target is what stands — the module's "late, never
    /// early" rule. (A real new-but-shorter wave therefore goes unreported: one
    /// heal missed, falling back to the consumer's backstop, which is the safe
    /// side of this trade.)
    #[test]
    fn an_sei_that_reaches_short_of_the_outstanding_target_never_shortens_the_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h265(0, false, h265_sei(20)); // target 20
        let short = w.note_h265(1, false, h265_sei(2)); // would target 3
        assert!(!short.sei_here);
        assert_eq!(w.note_h265(3, false, None), RecoveryMark::NONE, "not at 20");
        assert!(w.note_h265(20, false, None).is_recovery_point);

        let mut w = RecoveryWatch::new();
        w.note_h264(0, false, h264_sei(6));
        assert!(!w.note_h264(1, false, h264_sei(2)).sei_here);
        // The original six increments are still what has to be walked off.
        for fnum in 2..6 {
            assert_eq!(w.note_h264(fnum, false, None), RecoveryMark::NONE);
        }
        assert!(w.note_h264(6, false, None).is_recovery_point);
    }

    /// An IDR/IRAP clears the outstanding target, so the very next SEI is a wave
    /// start again however small its count — the re-announcement rule must not
    /// leave a stream unable to report a fresh wave after a keyframe.
    #[test]
    fn a_keyframe_makes_the_next_sei_a_wave_start_again() {
        let mut w = RecoveryWatch::new();
        w.note_h264(10, false, h264_sei(9));
        w.note_h264(0, true, None); // IDR
        assert!(
            w.note_h264(1, false, h264_sei(1)).sei_here,
            "a one-increment wave after an IDR is still a fresh wave"
        );

        let mut w = RecoveryWatch::new();
        w.note_h265(100, false, h265_sei(50));
        w.note_h265(0, true, None); // IRAP
        assert!(w.note_h265(1, false, h265_sei(2)).sei_here);
    }

    /// An IDR is a whole re-anchor and resets `frame_num`; a target counted
    /// against the old numbering must not survive it.
    #[test]
    fn an_idr_drops_a_pending_h264_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h264(200, false, h264_sei(2));
        assert!(w.is_watching());
        let idr = w.note_h264(0, true, None);
        assert_eq!(
            idr,
            RecoveryMark::NONE,
            "the IDR is not a recovery-point mark"
        );
        assert!(!w.is_watching());
        // The pictures after it are ordinary — no stale mark fires.
        assert_eq!(w.note_h264(1, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(2, false, None), RecoveryMark::NONE);
    }

    /// H.265 counts in POC, which is exact arithmetic — the target is hit at or
    /// past the announced value even when POC steps by more than one.
    #[test]
    fn an_h265_wave_marks_the_first_picture_at_or_past_the_target_poc() {
        let mut w = RecoveryWatch::new();
        let start = w.note_h265(10, false, h265_sei(4));
        assert!(start.sei_here && !start.is_recovery_point);
        assert_eq!(w.note_h265(11, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h265(13, false, None), RecoveryMark::NONE);
        // POC 14 is the target exactly.
        assert!(w.note_h265(14, false, None).is_recovery_point);
        assert!(!w.is_watching());

        // …and a stream whose POC steps OVER the target still marks: "at or past"
        // is the rule, because nothing guarantees the exact value is coded.
        let mut w = RecoveryWatch::new();
        w.note_h265(0, false, h265_sei(3));
        assert!(w.note_h265(8, false, None).is_recovery_point);
    }

    /// `recovery_poc_cnt` is se(v)-coded and may be negative (a recovery point
    /// among leading pictures) — the target is then behind us and the SEI's own
    /// picture is already the clean one. It must not become an infinite watch.
    #[test]
    fn a_negative_or_zero_poc_count_marks_immediately() {
        for count in [0, -1, -7] {
            let mut w = RecoveryWatch::new();
            let m = w.note_h265(30, false, h265_sei(count));
            assert!(m.sei_here && m.is_recovery_point, "count {count}");
            assert!(!w.is_watching(), "count {count}");
        }
    }

    /// Any IRAP — not just an IDR — restarts prediction and re-bases POC, so a
    /// target counted against the previous numbering cannot be compared past it.
    #[test]
    fn an_irap_drops_a_pending_h265_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h265(500, false, h265_sei(3));
        assert!(w.is_watching());
        assert_eq!(w.note_h265(0, true, None), RecoveryMark::NONE);
        assert!(!w.is_watching());
        assert_eq!(w.note_h265(1, false, None), RecoveryMark::NONE);
    }

    /// `exact_match_flag == 0` is the NORMAL value for a rolling wave (loop
    /// filtering bleeds across the refresh boundary, so the encoder promises
    /// approximate rather than bit-exact output). Requiring exactness would make
    /// this whole module never fire on the streams it was written for — and an
    /// approximately-correct picture is not the failure mode the freeze exists to
    /// hide, which is a gray plate with motion painted on it.
    #[test]
    fn an_approximate_recovery_point_still_marks() {
        let mut w = RecoveryWatch::new();
        assert!(
            w.note_h264(
                1,
                false,
                Some(RecoveryPoint {
                    recovery_frame_cnt: 0,
                    exact_match: false,
                    broken_link: true,
                })
            )
            .is_recovery_point,
            "neither exact_match nor broken_link may veto the mark"
        );
        let mut w = RecoveryWatch::new();
        assert!(
            w.note_h265(
                1,
                false,
                Some(RecoveryPointHevc {
                    recovery_poc_cnt: 0,
                    exact_match: false,
                    broken_link: true,
                })
            )
            .is_recovery_point
        );
    }

    /// A stream with no recovery point SEI at all — every punktfunk host today —
    /// produces no marks whatsoever. The signal is purely additive.
    #[test]
    fn a_stream_without_recovery_point_seis_never_marks() {
        let mut w = RecoveryWatch::new();
        for n in 0..64u16 {
            assert_eq!(w.note_h264(n, n == 0, None), RecoveryMark::NONE);
        }
        let mut w = RecoveryWatch::new();
        for n in 0..64i32 {
            assert_eq!(w.note_h265(n, n == 0, None), RecoveryMark::NONE);
        }
    }
}
