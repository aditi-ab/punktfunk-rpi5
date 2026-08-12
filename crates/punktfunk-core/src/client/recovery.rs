//! The client-side loss-range detector (`RfiRecovery::observe`) shared by every embedder.

use std::time::{Duration, Instant};

/// At most one client→host RFI request per this window, so a burst of frame-index gaps (a
/// full-screen pan shedding shards) can't storm the control stream. Matches the shared Vulkan pump's
/// recovery-request throttle; the host coalesces further.
const RFI_THROTTLE: Duration = Duration::from_millis(100);

/// State for [`NativeClient::note_frame_index`] — the client-side loss-range detector shared by every
/// embedder (Android, the C-ABI Apple client, the Windows shell pump) so none re-derives the wrapping
/// frame-index arithmetic. `next_expected` is the `frame_index` expected next in receive order;
/// `last_req` throttles the RFI requests a gap fires.
#[derive(Default)]
pub(crate) struct RfiRecovery {
    next_expected: Option<u32>,
    last_req: Option<Instant>,
}

/// What a forward gap should ask the host for: a precise RFI for a recoverable range, a plain
/// keyframe for a range wider than any encoder's reference history
/// ([`crate::packet::RFI_MAX_RANGE`] — a seconds-long outage, or a phantom index jump such as an
/// old host's speed-test burst consuming video indexes), or nothing (contiguous / straggler /
/// throttled).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAsk {
    None,
    Rfi(u32, u32),
    Keyframe,
}

impl RfiRecovery {
    /// Pure decision behind [`NativeClient::note_frame_index`]: fold one received `frame_index` (in
    /// receive order) observed at `now`, advancing the expectation and returning `(gap, ask)`.
    /// `gap` is how many frames this arrival revealed as missing — 0 for contiguous/straggler; the
    /// embedder arms its post-loss display freeze on a non-zero gap, and the WIDTH lets it
    /// pre-credit the reassembler's later `frames_dropped` climb for the same loss
    /// ([`crate::reanchor::ReanchorGate::arm_expecting_drops`] — the double-arm race). `ask` is the
    /// (throttled) recovery request to fire — an RFI naming the exact lost span, or a keyframe when
    /// the span exceeds [`crate::packet::RFI_MAX_RANGE`] (RFI is hopeless there: no encoder holds
    /// references that old, and a huge jump is more likely a resync — e.g. the first real AU after
    /// an old host's speed test — than a real loss). Split out from the connection so the wrapping
    /// arithmetic + [`RFI_THROTTLE`] are unit-testable without a live session (see the tests below).
    pub(crate) fn observe(&mut self, frame_index: u32, now: Instant) -> (u32, RecoveryAsk) {
        match self.next_expected {
            Some(exp) => {
                // Wrapping split at the half-space: a small positive delta is a forward gap
                // (missing frames); a delta in the top half is a straggler behind us.
                let ahead = frame_index.wrapping_sub(exp);
                if ahead == 0 {
                    self.next_expected = Some(frame_index.wrapping_add(1)); // contiguous
                    (0, RecoveryAsk::None)
                } else if ahead < u32::MAX / 2 {
                    // Forward gap: [exp, frame_index-1] lost (`ahead` frames). Advance past this
                    // frame so the same gap isn't re-detected, then fire a throttled recovery ask
                    // for the lost range.
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    let send = self
                        .last_req
                        .is_none_or(|t| now.duration_since(t) >= RFI_THROTTLE);
                    if send {
                        self.last_req = Some(now);
                    }
                    let ask = if !send {
                        RecoveryAsk::None
                    } else if ahead > crate::packet::RFI_MAX_RANGE {
                        RecoveryAsk::Keyframe
                    } else {
                        RecoveryAsk::Rfi(exp, frame_index.wrapping_sub(1))
                    };
                    (ahead, ask)
                } else {
                    // Straggler behind the delivery point — leave the expectation.
                    (0, RecoveryAsk::None)
                }
            }
            None => {
                self.next_expected = Some(frame_index.wrapping_add(1));
                (0, RecoveryAsk::None)
            }
        }
    }
}

#[cfg(test)]
mod rfi_recovery_tests {
    //! The client-side loss-range detector shared by every embedder (Android, the C-ABI Apple
    //! client, the Windows shell pump). `observe` is pure over `(frame_index, now)`, so the wrapping
    //! frame arithmetic and the RFI throttle are exercised here without a live session.
    use super::{RecoveryAsk, RfiRecovery, RFI_THROTTLE};
    use std::time::{Duration, Instant};

    // A fixed base instant; offsets model the throttle window deterministically (no sleeping).
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_arms_without_a_gap() {
        let mut r = RfiRecovery::default();
        // The opening frame only seeds the expectation — there is no prior frame to be missing.
        assert_eq!(r.observe(100, base()), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(101));
    }

    #[test]
    fn contiguous_frames_never_gap() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(101, t), (0, RecoveryAsk::None));
        assert_eq!(r.observe(102, t), (0, RecoveryAsk::None));
        assert_eq!(r.observe(103, t), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn forward_gap_reports_the_exact_lost_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t); // expecting 101 next
                           // 101..=104 were lost; 105 arrived. The RFI must name exactly the missing span.
        assert_eq!(r.observe(105, t), (4, RecoveryAsk::Rfi(101, 104)));
        // The expectation advances past the delivered frame so the same gap can't re-fire.
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn single_frame_drop_names_a_unit_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        // Exactly one frame (101) lost → range is the single index [101, 101].
        assert_eq!(r.observe(102, t), (1, RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn throttle_suppresses_bursts_then_re_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        // First gap fires the request and stamps the throttle.
        assert_eq!(r.observe(105, t0), (4, RecoveryAsk::Rfi(101, 104)));
        // A second gap 50 ms later is still a gap, but the request is throttled away.
        assert_eq!(
            r.observe(110, t0 + Duration::from_millis(50)),
            (4, RecoveryAsk::None)
        );
        // Past the window, the request re-opens for the still-accurate lost span.
        assert_eq!(
            r.observe(120, t0 + RFI_THROTTLE + Duration::from_millis(1)),
            (9, RecoveryAsk::Rfi(111, 119))
        );
    }

    #[test]
    fn stragglers_behind_the_delivery_point_are_ignored() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        r.observe(105, t); // expecting 106 next
                           // A reordered late arrival (103, well behind 106) is neither a gap nor a request, and it
                           // must not rewind the expectation — otherwise the next in-order frame would false-gap.
        assert_eq!(r.observe(103, t), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn wraparound_is_contiguous_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t); // expecting u32::MAX next
        assert_eq!(r.observe(u32::MAX, t), (0, RecoveryAsk::None)); // contiguous, wraps to 0
        assert_eq!(r.next_expected, Some(0));
        assert_eq!(r.observe(0, t), (0, RecoveryAsk::None)); // still contiguous across the wrap
        assert_eq!(r.next_expected, Some(1));
    }

    #[test]
    fn gap_range_wraps_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t); // expecting u32::MAX next
                                    // u32::MAX was lost and 1 arrived → the lost span wraps: [u32::MAX, 0].
        assert_eq!(r.observe(1, t), (2, RecoveryAsk::Rfi(u32::MAX, 0)));
        assert_eq!(r.next_expected, Some(2));
    }

    #[test]
    fn huge_gap_resyncs_via_keyframe_not_rfi() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t); // expecting 101 next
                           // A jump wider than any encoder's reference history (RFI_MAX_RANGE): no valid
                           // reference exists for an RFI, and the jump may be a phantom (an old host's
                           // speed-test burst consuming video indexes) — ask for the IDR resync instead.
        let jump = 100 + crate::packet::RFI_MAX_RANGE + 2;
        assert_eq!(r.observe(jump, t), (jump - 101, RecoveryAsk::Keyframe));
        // The expectation still advances past the delivered frame (no re-fire on the next one).
        assert_eq!(r.next_expected, Some(jump + 1));
        assert_eq!(r.observe(jump + 1, t), (0, RecoveryAsk::None));
        // A huge gap consumes the shared throttle too — an immediate follow-up gap stays quiet.
        assert_eq!(
            r.observe(jump + 10, t + Duration::from_millis(1)),
            (8, RecoveryAsk::None)
        );
    }
}
