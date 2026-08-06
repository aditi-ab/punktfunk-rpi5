//! Which planner warnings mean the PICTURE is damaged (M4 of the native-decode
//! program).
//!
//! Both planners emit two very different kinds of thing through one warning
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

use crate::{H265PlanWarning, PlanWarning};

/// Does this H.264 planner warning mean the PICTURE is damaged?
///
/// `Mmco5Rebase` does not: the AU carried an MMCO 5 and pf-bitstream planned it in
/// full (the plan holds the pre-rebase 8.2.1 values; later AUs reference the
/// rebased ones).
///
/// Written as an EXHAUSTIVE match with no wildcard, deliberately. A `matches!` (or
/// a `_ => false`) makes "damage" the opt-in and silence the default, so a
/// `PlanWarning` added later — by definition one nobody here has classified —
/// would be reported as clean and its picture shown. Invisible damage is the bug
/// this whole program exists to end; the compiler is the only reviewer guaranteed
/// to be present when that variant is written, so it gets the decision.
pub fn is_integrity_warning(w: &PlanWarning) -> bool {
    match w {
        PlanWarning::FrameNumGap { .. }
        | PlanWarning::MissingReference { .. }
        | PlanWarning::TruncatedAu { .. } => true,
        PlanWarning::Mmco5Rebase => false,
    }
}

/// The H.265 twin — the same set pf-bitstream's own `h265` conformance harness
/// calls integrity, `NonZeroReorder` deliberately excluded (module docs).
///
/// Exhaustive for the same reason as [`is_integrity_warning`]: a new H.265 warning
/// must not be able to mean "damaged" and read as clean.
pub fn is_integrity_warning_h265(w: &H265PlanWarning) -> bool {
    match w {
        H265PlanWarning::MissingReference { .. } | H265PlanWarning::TruncatedAu { .. } => true,
        H265PlanWarning::NonZeroReorder { .. } => false,
    }
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
}
