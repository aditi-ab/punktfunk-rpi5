//! Which decoded pictures came off a fully-available reference chain — the fact a
//! client needs to CORROBORATE a host's claim that a frame is a clean re-anchor.
//!
//! # Why this exists
//!
//! After a loss the client freezes on its last good picture and lifts only on a proven
//! re-anchor. One of those proofs — `USER_FLAG_RECOVERY_ANCHOR`, the host's LTR-RFI
//! recovery frame — lifts the freeze on its FIRST occurrence, exactly like a real IDR,
//! because the host says the frame was coded against a known-good reference.
//!
//! The host's "known-good" is an inference from what the client RECEIVED. The client's
//! own DPB is the only place that knows what it actually DECODED, and it did not
//! previously record it: a picture the planner concealed (a reference the DPB could not
//! resolve, an AU that stopped early) entered the DPB looking exactly like a clean one.
//! So an anchor naming that picture lifted the freeze onto a gray plate, and every
//! frame after it chained off the corruption — the freeze gone, nothing left to
//! re-arm it, and the picture stayed broken until an unrelated signal forced an IDR.
//!
//! This ledger is the missing fact, and it is deliberately the SMALLEST one that
//! answers the question: a set of picture ids that are NOT clean. Membership is
//! per-picture, so it costs one `u64` per damaged picture and nothing at all on a
//! healthy stream — the overwhelmingly common case, where the set stays empty for the
//! life of the session.
//!
//! # Damage propagates; that is the whole point
//!
//! A picture is unclean when the AU that produced it needed concealment, OR when
//! ANYTHING it predicted from was unclean. Without the second half the ledger would be
//! useless: the concealed picture itself is rarely the one an anchor names — it is the
//! chain of ordinary P-frames DESCENDING from it, each of which planned perfectly and
//! raised no warning of its own, that carries the corruption forward.
//!
//! # It errs toward "unclean", never toward "clean"
//!
//! Every rule here is one-way. An id the ledger has forgotten (evicted from the DPB,
//! dropped at a flush) reads as clean, which is correct — a picture no longer in the
//! DPB cannot be referenced. An id it holds stays unclean until the picture leaves the
//! DPB. There is no path that clears the mark on a picture that is still resident, so
//! the ledger can only ever make a consumer MORE conservative: hold the freeze longer
//! and take an IDR it might not have needed. The opposite mistake — reporting a damaged
//! chain as clean — is the failure this exists to end, so the asymmetry is deliberate.

use std::collections::BTreeSet;

/// Per-picture "this came off a broken chain" marks for one planner.
///
/// Keyed by the planner's own `PicId` (a `u64` in all three codecs), so this type is
/// codec-agnostic and the H.264, H.265 and AV1 planners share ONE implementation rather
/// than three hand-copies that can drift apart.
#[derive(Debug, Clone, Default)]
pub struct CleanLedger {
    /// Ids of resident pictures that are NOT clean. Empty on a healthy stream — the
    /// set only ever gains an entry when a plan needed concealment.
    unclean: BTreeSet<u64>,
}

impl CleanLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is every id in `references` clean? — i.e. may a picture predicted from exactly
    /// these be trusted?
    ///
    /// Vacuously true for an empty list, which is what makes an IRAP/IDR clean by
    /// construction: it predicts from nothing, so there is nothing to distrust.
    pub fn references_clean<I>(&self, references: I) -> bool
    where
        I: IntoIterator<Item = u64>,
    {
        // Short-circuits on the first unclean reference, and — because the set is
        // empty on a healthy stream — degenerates to one `is_empty`-cheap lookup per
        // reference in the case that matters for throughput.
        self.unclean.is_empty() || !references.into_iter().any(|id| self.unclean.contains(&id))
    }

    /// Record the verdict for the picture this AU stored.
    ///
    /// `references_clean` is what [`Self::references_clean`] answered for this AU's
    /// reference lists; `concealed` is whether the AU's own plan carried an integrity
    /// warning. Either one being bad makes the stored picture unclean, and its
    /// descendants inherit that through their own `references_clean` call.
    pub fn note_stored(&mut self, id: u64, references_clean: bool, concealed: bool) {
        if references_clean && !concealed {
            // The common path. Nothing is inserted, so a healthy stream never allocates
            // — and `remove` still runs below because an id can be REUSED after the
            // planner recycles it, and a stale mark would then condemn a fresh picture.
            self.unclean.remove(&id);
        } else {
            self.unclean.insert(id);
        }
    }

    /// Drop the marks of pictures that have left the DPB.
    ///
    /// Called with the ids still live after each plan. Bounding the set to DPB
    /// residency is what keeps it from growing without limit across a long lossy
    /// session, and it is safe precisely because a picture outside the DPB can never
    /// appear in a later reference list.
    pub fn retain_live<I>(&mut self, live: I)
    where
        I: IntoIterator<Item = u64>,
    {
        if self.unclean.is_empty() {
            return;
        }
        let live: BTreeSet<u64> = live.into_iter().collect();
        self.unclean.retain(|id| live.contains(id));
    }

    /// Forget everything — the DPB was drained (a flush, a stream discontinuity), so no
    /// mark describes a resident picture any more.
    pub fn clear(&mut self) {
        self.unclean.clear();
    }

    /// Is this picture known to have come off a broken chain? (Diagnostics and tests;
    /// the plan path uses [`Self::references_clean`].)
    pub fn is_unclean(&self, id: u64) -> bool {
        self.unclean.contains(&id)
    }

    /// How many resident pictures are marked unclean (diagnostics and tests).
    pub fn unclean_count(&self) -> usize {
        self.unclean.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The headline: damage propagates down the prediction chain. The concealed
    /// picture is rarely the one an anchor names — it is the ordinary P-frames
    /// descending from it, each of which planned perfectly and warned about nothing.
    #[test]
    fn damage_propagates_to_every_descendant() {
        let mut led = CleanLedger::new();
        // An IDR: no references, no concealment.
        assert!(led.references_clean([]));
        led.note_stored(0, true, false);
        assert!(!led.is_unclean(0));

        // A clean P off it.
        assert!(led.references_clean([0]));
        led.note_stored(1, true, false);

        // Picture 2's plan needed concealment.
        let refs_clean = led.references_clean([1]);
        assert!(refs_clean, "its reference was still fine");
        led.note_stored(2, refs_clean, true);
        assert!(led.is_unclean(2));

        // …and picture 3 predicts from it, raising NO warning of its own.
        let refs_clean = led.references_clean([2]);
        assert!(!refs_clean, "the chain is broken from here down");
        led.note_stored(3, refs_clean, false);
        assert!(led.is_unclean(3), "3 inherited 2's damage");

        // The rot keeps travelling, arbitrarily far from the original loss.
        let refs_clean = led.references_clean([3]);
        assert!(!refs_clean);
        led.note_stored(4, refs_clean, false);
        assert!(led.is_unclean(4));
    }

    /// A picture that references BOTH a clean and an unclean predecessor is unclean —
    /// one broken reference is enough to make the reconstruction wrong.
    #[test]
    fn one_unclean_reference_is_enough() {
        let mut led = CleanLedger::new();
        led.note_stored(0, true, false);
        led.note_stored(1, true, true); // damaged
        assert!(!led.references_clean([0, 1]));
        assert!(!led.references_clean([1, 0]), "order does not matter");
        assert!(led.references_clean([0]));
    }

    /// An IDR predicts from nothing, so it is clean however broken the stream was
    /// before it. This is the property that lets a real keyframe end a damaged run.
    #[test]
    fn a_picture_with_no_references_is_clean_however_bad_the_stream_was() {
        let mut led = CleanLedger::new();
        led.note_stored(0, true, true);
        led.note_stored(1, false, false);
        assert_eq!(led.unclean_count(), 2);
        // The IDR: an empty reference list is vacuously clean.
        assert!(led.references_clean([]));
        led.note_stored(2, true, false);
        assert!(!led.is_unclean(2));
    }

    /// Marks are bounded by DPB residency: a picture that left the DPB can never be
    /// referenced again, so keeping its mark would only grow the set forever.
    #[test]
    fn marks_are_dropped_when_their_picture_leaves_the_dpb() {
        let mut led = CleanLedger::new();
        led.note_stored(7, true, true);
        led.note_stored(8, false, false);
        assert_eq!(led.unclean_count(), 2);
        led.retain_live([8, 9]);
        assert!(!led.is_unclean(7), "7 was evicted");
        assert!(led.is_unclean(8), "8 is still resident and still damaged");
        assert_eq!(led.unclean_count(), 1);
    }

    /// A flush drains the whole DPB, so no mark describes anything resident.
    #[test]
    fn a_flush_forgets_every_mark() {
        let mut led = CleanLedger::new();
        led.note_stored(1, true, true);
        led.note_stored(2, false, false);
        led.clear();
        assert_eq!(led.unclean_count(), 0);
        assert!(led.references_clean([1, 2]));
    }

    /// Planners hand out ids from a counter the flush path can rewind, so an id CAN be
    /// reused. A stale mark must not condemn the fresh picture that inherits the id.
    #[test]
    fn a_reused_id_is_not_condemned_by_its_predecessors_mark() {
        let mut led = CleanLedger::new();
        led.note_stored(5, true, true);
        assert!(led.is_unclean(5));
        // The same id, planned cleanly this time.
        led.note_stored(5, true, false);
        assert!(!led.is_unclean(5));
        assert!(led.references_clean([5]));
    }

    /// A healthy stream never marks anything, forever — the property that makes this
    /// free to carry on every session that is working correctly.
    #[test]
    fn a_stream_without_loss_never_marks_a_picture() {
        let mut led = CleanLedger::new();
        for id in 0..512u64 {
            let refs = if id == 0 { vec![] } else { vec![id - 1] };
            let clean = led.references_clean(refs.iter().copied());
            assert!(clean, "picture {id} must read clean");
            led.note_stored(id, clean, false);
            led.retain_live(id.saturating_sub(3)..=id);
        }
        assert_eq!(led.unclean_count(), 0);
    }
}
