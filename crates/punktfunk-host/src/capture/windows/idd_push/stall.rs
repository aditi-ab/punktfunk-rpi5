//! Capture-stall detection (plan §W4, carved out of the IDD-push capturer): flags multi-hundred-ms
//! holes in DWM frame delivery that open while the desktop was actively composing.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::*;

/// A detected capture stall: a multi-hundred-ms hole in DWM's frame delivery that opened while the
/// desktop was actively composing right beforehand (see [`StallWatch`]).
pub(super) struct Stall {
    /// How long the hole lasted (last fresh frame → the frame that ended it).
    pub(super) gap: Duration,
    /// `Some(mean period)` when this stall completes a metronomic cycle (see
    /// [`crate::metronome::Metronome`]).
    pub(super) metronomic: Option<Duration>,
}

/// Capture-stall watch — the "sole virtual display" stutter diagnostic (field reports: Exclusive
/// topology = periodic double-jolt, Extend = smooth, i.e. the disturbance lives in the display/present
/// path BELOW capture and only while no physical output is active).
///
/// On a damage-driven capture an idle desktop legitimately goes quiet (no damage → no frames), so a
/// gap only counts as a stall when the [`Self::RECENT`] frames before it all arrived within
/// [`Self::ACTIVE_SPAN`] — sustained ≥ ~20 fps flow (a game or video), not a blinking caret or a
/// mouse twitch. Each stall feeds a [`crate::metronome::Metronome`], so periodic stalls self-diagnose
/// in the log WITHOUT needing any client keyframe request — discriminating "DWM stopped composing"
/// from encode/network causes that the recovery-cadence detector covers. Pure logic — unit-tested
/// below; the caller does the logging.
pub(super) struct StallWatch {
    /// The last [`Self::RECENT`] fresh-frame instants (pre-gap history for the activity gate).
    recent: std::collections::VecDeque<Instant>,
    cadence: crate::metronome::Metronome,
}

impl StallWatch {
    /// Frames of pre-gap history that must be tight for flow to count as active. Stalls are thus
    /// naturally spaced ≥ RECENT frame times apart — no extra log rate limit needed.
    const RECENT: usize = 8;
    /// The RECENT pre-gap frames must all fit in this span (8 frames in 400 ms ≈ ≥ 20 fps flow —
    /// loose enough for a 30 fps-capped game, tight enough to reject idle-desktop damage).
    const ACTIVE_SPAN: Duration = Duration::from_millis(400);
    /// The smallest hole that counts as a stall (~9 missed frames at 60 Hz) — well below the
    /// reported 300–700 ms freezes, above encode/present jitter.
    const STALL_MIN: Duration = Duration::from_millis(150);

    pub(super) fn new() -> Self {
        Self {
            recent: std::collections::VecDeque::with_capacity(Self::RECENT + 1),
            cadence: crate::metronome::Metronome::new(),
        }
    }

    /// Forget the flow history (a ring recreate's gap is self-inflicted, not a DWM stall — without
    /// the reset the first post-recreate frame would read as one).
    pub(super) fn reset(&mut self) {
        self.recent.clear();
    }

    /// Record a fresh driver frame at `now`; `Some` exactly when it ended a stall.
    pub(super) fn note_fresh(&mut self, now: Instant) -> Option<Stall> {
        let was_active = self.recent.len() == Self::RECENT
            && self
                .recent
                .back()
                .zip(self.recent.front())
                .is_some_and(|(b, f)| b.duration_since(*f) <= Self::ACTIVE_SPAN);
        let gap = self.recent.back().map(|last| now.duration_since(*last));
        self.recent.push_back(now);
        if self.recent.len() > Self::RECENT {
            self.recent.pop_front();
        }
        let gap = gap?;
        if !was_active || gap < Self::STALL_MIN {
            return None;
        }
        Some(Stall {
            gap,
            metronomic: self.cadence.note(now),
        })
    }
}
