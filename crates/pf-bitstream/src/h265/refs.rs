//! Reference facts every H.265 backend derives the same way from an [`AuPlan`].

use super::AuPlan;

/// `NumDeltaPocsOfRefRpsIdx` derivation failures. Each backend converts this
/// into its own conversion error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefRpsIdxError {
    /// The first slice's inline `st_ref_pic_set()` predicts from a missing SPS
    /// candidate — the count cannot be derived, and hardware would misparse the
    /// slice header.
    Invalid {
        curr_rps_idx: u8,
        delta_idx_minus1: u8,
    },
    /// Predicted-from candidate `NumDeltaPocs` exceeds `u8`. Impossible off a
    /// real parse (≤ 32); an error rather than a clamp, because a clamped count
    /// makes hardware misparse the slice header.
    NumDeltaPocsOverflow(u32),
}

impl std::fmt::Display for RefRpsIdxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefRpsIdxError::Invalid {
                curr_rps_idx,
                delta_idx_minus1,
            } => {
                write!(
                    f,
                    "inline st_ref_pic_set predicts from a nonexistent candidate \
                     (CurrRpsIdx {curr_rps_idx}, delta_idx_minus1 {delta_idx_minus1})"
                )
            }
            RefRpsIdxError::NumDeltaPocsOverflow(count) => {
                write!(f, "candidate NumDeltaPocs {count} exceeds u8")
            }
        }
    }
}

impl std::error::Error for RefRpsIdxError {}

impl AuPlan {
    /// `NumDeltaPocsOfRefRpsIdx`: when the first slice's inline `st_ref_pic_set()`
    /// uses inter-RPS prediction, hardware re-parses those slice bits and needs
    /// `NumDeltaPocs[RefRpsIdx]` of the source candidate to size the
    /// `used_by_curr_pic_flag`/`use_delta_flag` loop (7.4.8); otherwise 0.
    ///
    /// Vulkan's `NumDeltaPocsOfRefRpsIdx` and DXVA's `ucNumDeltaPocsOfRefRpsIdx`
    /// both come from here. Panics on a plan with no slices; converters refuse
    /// those first.
    pub fn num_delta_pocs_of_ref_rps_idx(&self) -> Result<u8, RefRpsIdxError> {
        let hdr = &self
            .slices
            .first()
            .expect("caller validated the plan holds slices")
            .header;
        // Inline means CurrRpsIdx == num_short_term_ref_pic_sets (8.3.2 NOTE 2); an
        // SPS-indexed RPS re-parses nothing in the slice header.
        let inline = !hdr.short_term_ref_pic_set_sps_flag
            && hdr.curr_rps_idx == self.sps.num_short_term_ref_pic_sets;
        if !inline || !hdr.short_term_ref_pic_set.inter_ref_pic_set_prediction_flag {
            return Ok(0);
        }
        // RefRpsIdx = stRpsIdx - (delta_idx_minus1 + 1), stRpsIdx = CurrRpsIdx here
        // (equation 7-59). u16 so a hostile delta cannot wrap.
        let delta = hdr.short_term_ref_pic_set.delta_idx_minus1;
        let source = u16::from(hdr.curr_rps_idx)
            .checked_sub(u16::from(delta) + 1)
            .and_then(|idx| self.sps.short_term_ref_pic_set.get(usize::from(idx)))
            .ok_or(RefRpsIdxError::Invalid {
                curr_rps_idx: hdr.curr_rps_idx,
                delta_idx_minus1: delta,
            })?;
        // Real parses have NumDeltaPocs ≤ 32 (u8). A clamp would misparse the slice
        // header on hardware, so a constructed plan that exceeds it is an error.
        u8::try_from(source.num_delta_pocs)
            .map_err(|_| RefRpsIdxError::NumDeltaPocsOverflow(source.num_delta_pocs))
    }
}
