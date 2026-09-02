//! Which planner warnings mean the decoded picture is damaged.
//!
//! Planners share one warning channel for two kinds:
//!
//! * Integrity — missing DPB reference, `frame_num` gap, truncated AU. The plan
//!   substituted; release the picture unshown and re-anchor.
//! * Spec-legal envelope — H.265 `NonZeroReorder` (`sps_max_num_reorder_pics > 0`)
//!   and H.264 `Mmco5Rebase`. Fully planned; not concealment. `NonZeroReorder`
//!   fires on the AU that activates an SPS (opening IDR and every ABR IDR);
//!   treating it as damage costs a keyframe round-trip on a correctly planned
//!   stream.
//!
//! Classification lives on the warning enums in pf-bitstream
//! ([`PlanWarning::is_integrity`] and twins). These functions are the crate's
//! public spelling so [`crate::fault`] and the client assert against one list.

use crate::{Av1PlanWarning, H265PlanWarning, PlanWarning};

/// True when this H.264 warning means the decoded picture is damaged.
///
/// Not `Mmco5Rebase` (MMCO 5 planned in full) or `LevelDerivedDpb` (SPS omitted
/// DPB depth; fail to open a session, do not show a damaged frame).
pub fn is_integrity_warning(w: &PlanWarning) -> bool {
    w.is_integrity()
}

/// True when this H.265 warning means the decoded picture is damaged.
///
/// Excludes `NonZeroReorder` (module docs).
pub fn is_integrity_warning_h265(w: &H265PlanWarning) -> bool {
    w.is_integrity()
}

/// True when this AV1 warning means the decoded picture is damaged.
///
/// AV1 has no reorder/MMCO envelope; every current variant is damage.
/// `MissingShowExisting` is a lost display picture (stale screen, re-anchor).
/// `MissingReference` is refused by [`crate::VkAv1Decoder`]
/// ([`crate::VkDecodeError::MissingReferenceAv1`]) — no legal substitute — and
/// still classified here so the fault harness matches the decoder.
pub fn is_integrity_warning_av1(w: &Av1PlanWarning) -> bool {
    w.is_integrity()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Guards reclassification of an AV1 warning as clean. A new variant is
    /// caught by the enum match in pf-bitstream, not this hand list.
    /// `MissingShowExisting` is damage: the screen keeps the previous picture.
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
