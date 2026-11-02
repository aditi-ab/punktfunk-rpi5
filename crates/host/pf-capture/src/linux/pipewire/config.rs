//! [`StreamConfig`]: what one capture stream decides before its loop thread exists, resolved
//! once by `spawn_pipewire`. The thread adds only what it asks at run time — the producer's
//! probe, the GPU's modifier lists, the identity's ladder at offer time — so the offer it
//! builds is a function of data.

use super::plan::NegotiationPlan;
use super::CaptureOpts;
use crate::ZeroCopyPolicy;
use pf_zerocopy::{Ladder, Refusal};

/// The stream's decisions. No field is read again from the environment or a shared cell.
pub(super) struct StreamConfig {
    /// The producer's PipeWire node.
    pub(super) node_id: u32,
    /// The open's options, `want_hdr` already dropped under `PUNKTFUNK_FORCE_SHM`. Its `knobs`
    /// are the open's.
    pub(super) opts: CaptureOpts,
    /// Seeds negotiation and sizes a Mutter virtual monitor; `(w, h, hz)`.
    pub(super) preferred: Option<(u32, u32, u32)>,
    /// Encode-backend facts from the facade.
    pub(super) policy: ZeroCopyPolicy,
    /// The zero-copy negotiation.
    pub(super) plan: NegotiationPlan,
}

impl StreamConfig {
    /// The offer carries 10-bit formats: PQ, or the producer's own 10-bit SDR.
    pub(super) fn ten_bit(&self) -> bool {
        self.opts.want_hdr || self.opts.sdr10_native
    }

    /// Tiled 10-bit goes out for NVENC's raw convert, which reads it; such a frame is held,
    /// never imported.
    pub(super) fn hdr_tiled_raw(&self, ladder: &Ladder) -> bool {
        self.ten_bit()
            && self.policy.gamescope_tiled
            && self.plan.nvenc_raw
            && !ladder.refused(Refusal::HdrTiled)
    }

    /// Explicit sync needs a dmabuf lane and a DRM node that serves syncobjs; whether a buffer
    /// then carries sync points is the producer's call at negotiation.
    pub(super) fn wants_sync(&self, want_dmabuf: bool) -> bool {
        self.opts.knobs.explicit_sync && (self.opts.want_hdr || want_dmabuf)
    }
}
