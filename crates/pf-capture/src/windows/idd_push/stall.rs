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
    /// [`pf_frame::metronome::Metronome`]).
    pub(super) metronomic: Option<Duration>,
}

/// Capture-stall watch — the "sole virtual display" stutter diagnostic (field reports: Exclusive
/// topology = periodic double-jolt, Extend = smooth, i.e. the disturbance lives in the display/present
/// path BELOW capture and only while no physical output is active).
///
/// On a damage-driven capture an idle desktop legitimately goes quiet (no damage → no frames), so a
/// gap only counts as a stall when the [`Self::RECENT`] frames before it all arrived within
/// [`Self::ACTIVE_SPAN`] — sustained ≥ ~20 fps flow (a game or video), not a blinking caret or a
/// mouse twitch. Each stall feeds a [`pf_frame::metronome::Metronome`], so periodic stalls self-diagnose
/// in the log WITHOUT needing any client keyframe request — discriminating "DWM stopped composing"
/// from encode/network causes that the recovery-cadence detector covers. Pure logic — unit-tested
/// below; the caller does the logging.
pub(super) struct StallWatch {
    /// The last [`Self::RECENT`] fresh-frame instants (pre-gap history for the activity gate).
    recent: std::collections::VecDeque<Instant>,
    cadence: pf_frame::metronome::Metronome,
    /// Stalls seen this session, and how many had a coinciding OS display event — the discriminator
    /// [`Self::report`] uses. They were capturer fields that nothing outside the report touched.
    seen: u32,
    with_os_events: u32,
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
            cadence: pf_frame::metronome::Metronome::new(),
            seen: 0,
            with_os_events: 0,
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
    /// Log a detected stall, correlate it against OS display events, and — once the cadence turns
    /// metronomic — name the class of disturbance and its cures.
    ///
    /// Lives here rather than in `try_consume` (sweep Phase 5.4): it is ~65 lines of log prose plus
    /// a running tally, all of it about stalls and none of it about consuming a frame, in a function
    /// that runs per frame. `now` is the instant of the frame that ENDED the stall — the same one
    /// passed to [`Self::note_fresh`] — which is what bounds the event-correlation window.
    pub(super) fn report(&mut self, stall: &Stall, now: Instant) {
        // OS display events inside the gap (plus a lead-in margin: the event that CAUSED the
        // hole lands just before DWM stops delivering) — the attribution that turns "DWM
        // stopped composing" into "…because Windows re-enumerated SAMSUNG on HDMI".
        let window = stall.gap + Duration::from_millis(300);
        let events = now
            .checked_sub(window)
            .map(|from| pf_win_display::display_events::events_between(from, now))
            .unwrap_or_default();
        self.seen = self.seen.saturating_add(1);
        if !events.is_empty() {
            self.with_os_events = self.with_os_events.saturating_add(1);
        }
        // debug (not warn): a single hole also happens when content legitimately pauses;
        // the reportable signal is the metronomic cycle below. Mounjay-class triage runs
        // at debug level, and the web-console debug ring captures these.
        tracing::debug!(
            gap_ms = stall.gap.as_millis() as u64,
            os_display_events = %pf_win_display::display_events::summarize(&events),
            "IDD-push capture stall — the desktop was composing at speed, then DWM \
             delivered no frame for the gap; the present path stalled below capture"
        );
        if let Some(period) = stall.metronomic {
            let suspects = pf_win_display::display_events::connected_inactive_externals();
            let suspects = if suspects.is_empty() {
                "none".to_string()
            } else {
                suspects.join(", ")
            };
            let correlated = format!("{}/{}", self.with_os_events, self.seen);
            // Half-or-more of the stalls carrying a coinciding OS event = the reaction
            // cascade is OS-visible; otherwise the disturbance never surfaces above the
            // driver. Different classes, different cures — say which one this box has.
            if self.with_os_events * 2 >= self.seen {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    "capture stalls are METRONOMIC and coincide with Windows monitor \
                     hot-plug/re-enumeration events — a connected display (or its \
                     cable/switch/AVR) re-probes the link on a timer and Windows re-reacts \
                     each time. Cures, best-first: that display's OSD 'auto input \
                     scan/detect' OFF (and on TVs: instant-on/quick-start + CEC off), \
                     unplug its cable at the GPU, an HPD-holding adapter/dummy plug, or \
                     keep it active while streaming; the pnp_disable_monitors policy axis \
                     suppresses the Windows-side reaction (see connected_inactive for the \
                     suspects)"
                );
            } else {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    "capture stalls are METRONOMIC with NO coinciding OS display event — \
                     the disturbance is BELOW Windows: the GPU driver servicing a \
                     connected-but-asleep sink (standby HPD/DDC/link probing), \
                     display-poller software (the SteelSeries-GG/SignalRGB class — \
                     correlate 'slow display-descriptor poll' lines), or the DWM present \
                     clock (try a different refresh rate). If connected_inactive lists a \
                     display, its standby probing is the prime suspect: unplug it at the \
                     GPU, disable its OSD auto input scan (TVs: instant-on/quick-start + \
                     CEC off), use an HPD-holding adapter/dummy, or keep it active while \
                     streaming"
                );
            }
        }
    }
}
