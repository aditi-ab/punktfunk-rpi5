//! Client-side loss-range detector (`RfiRecovery::observe`).

use std::time::{Duration, Instant};

/// Matches the Vulkan pump: one recovery ask per window so a burst of gaps
/// cannot storm the control stream. The host coalesces further.
const RFI_THROTTLE: Duration = Duration::from_millis(100);

/// Gap detector behind [`NativeClient::note_frame_index`]. Wrapping `frame_index`
/// arithmetic lives here so embedders do not each re-derive it.
#[derive(Default)]
pub(crate) struct RfiRecovery {
    next_expected: Option<u32>,
    last_req: Option<Instant>,
}

/// Recovery request for a forward gap. Keyframe when the span exceeds
/// [`crate::packet::RFI_MAX_RANGE`]: no encoder still holds that reference.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAsk {
    None,
    Rfi(u32, u32),
    Keyframe,
}

impl RfiRecovery {
    /// `gap` and `ask` are independent: throttle can yield [`RecoveryAsk::None`]
    /// with a non-zero gap. Pass that width to
    /// [`crate::reanchor::ReanchorGate::arm_expecting_drops`] or the reassembler's
    /// later `frames_dropped` climb is counted as a second loss.
    pub(crate) fn observe(&mut self, frame_index: u32, now: Instant) -> (u32, RecoveryAsk) {
        match self.next_expected {
            Some(exp) => {
                // Half-space wrap: wrapping_sub < u32::MAX/2 is a forward gap; top half is a straggler.
                let ahead = frame_index.wrapping_sub(exp);
                if ahead == 0 {
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    (0, RecoveryAsk::None)
                } else if ahead < u32::MAX / 2 {
                    // Advance past this frame so the same gap cannot re-fire; then throttle the ask.
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
                    // Leave next_expected: a rewind would false-gap the next in-order frame.
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
    use super::{RecoveryAsk, RfiRecovery, RFI_THROTTLE};
    use std::time::{Duration, Instant};

    // Offsets from this Instant model the throttle window; do not sleep.
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_arms_without_a_gap() {
        let mut r = RfiRecovery::default();
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
        r.observe(100, t);
        assert_eq!(r.observe(105, t), (4, RecoveryAsk::Rfi(101, 104)));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn single_frame_drop_names_a_unit_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (1, RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn throttle_suppresses_bursts_then_re_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(105, t0), (4, RecoveryAsk::Rfi(101, 104)));
        assert_eq!(
            r.observe(110, t0 + Duration::from_millis(50)),
            (4, RecoveryAsk::None)
        );
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
        r.observe(105, t);
        assert_eq!(r.observe(103, t), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn wraparound_is_contiguous_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(u32::MAX, t), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(0));
        assert_eq!(r.observe(0, t), (0, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(1));
    }

    #[test]
    fn gap_range_wraps_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(1, t), (2, RecoveryAsk::Rfi(u32::MAX, 0)));
        assert_eq!(r.next_expected, Some(2));
    }

    #[test]
    fn huge_gap_resyncs_via_keyframe_not_rfi() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        let jump = 100 + crate::packet::RFI_MAX_RANGE + 2;
        assert_eq!(r.observe(jump, t), (jump - 101, RecoveryAsk::Keyframe));
        assert_eq!(r.next_expected, Some(jump + 1));
        assert_eq!(r.observe(jump + 1, t), (0, RecoveryAsk::None));
        // Keyframe stamps last_req too; an immediate follow-up gap stays quiet.
        assert_eq!(
            r.observe(jump + 10, t + Duration::from_millis(1)),
            (8, RecoveryAsk::None)
        );
    }
}
