//! Which planner warnings mean the PICTURE is damaged (M4 of the native-decode
//! program).
//!
//! The planners emit two very different kinds of thing through one warning
//! channel, and the split is what a consumer must branch on:
//!
//! * **Integrity** — a reference the DPB does not hold, a `frame_num` gap, an AU
//!   whose NALU walk stopped early. The plan was completed with a SUBSTITUTE in
//!   place of something that was lost, so the decoded picture is damaged: its
//!   output must be released unshown and the stream must ask for a re-anchor.
//! * **Spec-legal envelope signals** — h265's `NonZeroReorder` (the activated SPS
//!   sets `sps_max_num_reorder_pics > 0`) and h264's `Mmco5Rebase`. pf-bitstream
//!   documents both as spec-legal and fully planned; they exist as the field
//!   signal that a punktfunk-host ASSUMPTION broke, not as damage. `NonZeroReorder`
//!   in particular fires on the AU that ACTIVATES an SPS — the opening IDR, and the
//!   fresh IDR at every ABR resolution change — so treating it as concealment would
//!   cost a released-unshown frame plus a keyframe round trip at every
//!   renegotiation, on a stream the planner says it planned correctly.
//!
//! It lives here, in the crate the warnings are re-exported from, rather than in
//! the client that first needed it, because the fault-injection harness
//! ([`crate::fault`]) has to assert against the SAME predicate the client conceals
//! on. Two copies of this list would let a test prove detection that production
//! does not actually perform — the exact shape of the `nb_queries = 0` failure the
//! program exists to end.

use crate::{Av1PlanWarning, H265PlanWarning, PlanWarning};

/// Does this H.264 planner warning mean the PICTURE is damaged?
///
/// `Mmco5Rebase` does not: the AU carried an MMCO 5 and pf-bitstream planned it in
/// full (the plan holds the pre-rebase 8.2.1 values; later AUs reference the
/// rebased ones).
///
/// `LevelDerivedDpb` does not either: the picture is intact and fully planned. It
/// reports that the SPS never declared its DPB depth, so the plan had to size from
/// A.3.1's level ceiling and the result will not fit a mainstream slot pool — a
/// property of the STREAM's signalling, which the decoder answers by failing to open
/// a session, not by showing a damaged frame.
///
/// ⚠ The classification itself now lives on the warning enum, in pf-bitstream
/// ([`PlanWarning::is_integrity`]), and this function delegates. It moved there when
/// the planners gained the per-picture clean bit
/// ([`pf_bitstream::h264::PicturePlan::references_clean`]): that ledger has to mark a
/// picture damaged on exactly the warnings a consumer conceals on, and it lives one
/// crate DOWN from here. A copy of the list in each crate would let the two disagree —
/// the planner recording a picture as clean while the client concealed it, or the
/// reverse — which is the same invisible-damage failure the single-list rule below was
/// written to prevent, one layer lower. One list, in the crate that owns the enum.
///
/// This function stays as the crate's public spelling of the question (the fault
/// harness, the client and the tests all name it) and keeps its exact semantics.
pub fn is_integrity_warning(w: &PlanWarning) -> bool {
    w.is_integrity()
}

/// The H.265 twin — the same set pf-bitstream's own `h265` conformance harness
/// calls integrity, `NonZeroReorder` deliberately excluded (module docs).
///
/// Exhaustive for the same reason as [`is_integrity_warning`]: a new H.265 warning
/// must not be able to mean "damaged" and read as clean.
pub fn is_integrity_warning_h265(w: &H265PlanWarning) -> bool {
    w.is_integrity()
}

/// The AV1 twin (M7). Every variant the AV1 planner has today IS damage, and that
/// is a fact about the codec rather than an oversight: AV1 puts nothing in this
/// channel that resembles h265's `NonZeroReorder` or h264's `Mmco5Rebase`. It has
/// no reorder envelope to report (no bumping process, no `max_num_reorder_pics`)
/// and no MMCO to rebase — the frame header states the whole reference update
/// outright — so the only things left to warn about are pictures that went
/// missing and an OBU walk that stopped early.
///
/// `MissingShowExisting` is the one that could be argued, and it is damage: a
/// `show_existing_frame` naming an empty slot means the picture the STREAM chose
/// to display was lost upstream. Nothing is displayed for that frame, so the
/// screen keeps the previous one — exactly the "silently stale picture" state a
/// re-anchor exists to end.
///
/// ⚠ `MissingReference` is classified here for completeness and does NOT normally
/// reach a consumer through this predicate: [`crate::VkAv1Decoder`] refuses the
/// whole access unit for it ([`crate::VkDecodeError::MissingReferenceAv1`]),
/// because AV1's `refs` array is indexed by reference NAME and there is no legal
/// substitute to write into a hole — a `-1` for a name the frame really references
/// is a spec violation whose firmware behaviour is undefined. So the AV1 rung
/// answers a lost reference as a REFUSAL, not as concealment, and it is the
/// refusal counter that moves. Classifying it as damage here anyway keeps the two
/// statements consistent for any consumer that does see the warning (and for the
/// fault harness, which asserts detection against exactly this list).
///
/// Exhaustive for the same reason as [`is_integrity_warning`]: a new AV1 warning
/// must not be able to mean "damaged" and read as clean.
pub fn is_integrity_warning_av1(w: &Av1PlanWarning) -> bool {
    w.is_integrity()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The split, stated as the pair of lists it is. This test is the contract:
    /// the client drops a frame for everything on the left and shows the picture
    /// for everything on the right, and the fault harness asserts detection
    /// against the very same predicate.
    #[test]
    fn damage_is_a_lost_reference_or_a_short_au_and_nothing_else() {
        for w in [
            PlanWarning::FrameNumGap {
                expected: 4,
                got: 7,
            },
            PlanWarning::MissingReference {
                context: "list0",
                detail: "poc 12".into(),
            },
            PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(is_integrity_warning(&w), "{w:?} is damage");
        }
        assert!(
            !is_integrity_warning(&PlanWarning::Mmco5Rebase),
            "an MMCO 5 was planned in FULL — dropping its frame would hitch a \
             correct stream"
        );

        for w in [
            H265PlanWarning::MissingReference {
                context: "StCurrBefore",
                detail: "poc 12".into(),
            },
            H265PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(is_integrity_warning_h265(&w), "{w:?} is damage");
        }
        assert!(
            !is_integrity_warning_h265(&H265PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 1
            }),
            "SPS activation is not damage — it fires on the opening IDR and on \
             every ABR renegotiation's IDR"
        );
    }

    /// AV1's whole warning vocabulary is damage. Note plainly what this test does
    /// and does not guard, because the two are easy to confuse:
    ///
    /// * A NEW variant is caught by the EXHAUSTIVE MATCH in
    ///   [`is_integrity_warning_av1`], not here — this loop enumerates the variants
    ///   by hand, so a fourth one would simply not appear in it. That is the whole
    ///   reason the function is written as a match with no `_` arm.
    /// * What this test does guard is a RECLASSIFICATION: split one of these names
    ///   out of the `|` chain and give it a `false` arm — the shape a future
    ///   "spec-legal AV1 signal" would arrive in — and the assertion below fires.
    ///   `MissingShowExisting` is the one most likely to be argued down that way (a
    ///   frame that decoded nothing and displayed nothing reads as harmless), and
    ///   reading it as clean would leave the previous picture on the screen with no
    ///   re-anchor asked for.
    #[test]
    fn every_av1_warning_is_damage_because_av1_has_no_envelope_signal() {
        for w in [
            Av1PlanWarning::MissingReference {
                slot: 3,
                ref_index: 1,
            },
            Av1PlanWarning::MissingShowExisting { slot: 5 },
            Av1PlanWarning::TruncatedAu { offset: 900 },
        ] {
            assert!(is_integrity_warning_av1(&w), "{w:?} is damage");
        }
    }
}
