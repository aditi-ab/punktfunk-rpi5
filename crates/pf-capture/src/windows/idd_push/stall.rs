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

/// Driver-telemetry evidence for one stall window (the v2 header tail — see
/// `pf_driver_proto::frame::SharedHeader`), sampled by the capturer between the last pre-gap
/// frame and the frame that ended the stall.
pub(super) struct StallEvidence {
    /// Surfaces the driver OFFERED to the ring publisher during the window (delta of
    /// `offered_total`); `None` = pre-telemetry driver (it never wrote the tail).
    pub(super) offered_delta: Option<u64>,
    /// The STALEST the driver's drain heartbeat ever read while the host starved (max of
    /// now − heartbeat over the window), in milliseconds.
    pub(super) max_heartbeat_age_ms: u64,
}

/// The attribution a stall's evidence supports — the Branch-1/Branch-2 fork of the
/// vdisplay-disturbance-immunity program, computed per stall instead of argued per field report.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StallVerdict {
    /// Pre-telemetry driver: no verdict, the log says only what the host observed.
    NoTelemetry,
    /// The drain worker's heartbeat went silent for a large share of the hole — the swap-chain
    /// thread starved (CPU/MMCSS) or the WUDFHost died. Ours, host/driver side.
    WorkerStalled,
    /// The worker drained E_PENDING throughout: DWM composed NOTHING for the hole. The
    /// disturbance is below capture (adapter servicing / DDC lock / present clock) — the
    /// micro-probe + ETW phases discriminate further.
    ComposeSilence,
    /// DWM composed frames all through the hole and the driver offered them, but none became a
    /// consumable ring slot — OUR publish/ring/consume leg lost them. Fully killable.
    DeliveryLeg,
}

impl std::fmt::Display for StallVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoTelemetry => "pre-telemetry driver (no verdict)",
            Self::WorkerStalled => "driver-worker-stalled (heartbeat silent) — host CPU/MMCSS or a dead WUDFHost, NOT the display path",
            Self::ComposeSilence => "compose-silence (driver drained E_PENDING) — DWM composed nothing; the disturbance is below capture",
            Self::DeliveryLeg => "delivery-leg (frames were composed + offered but never consumable) — OUR ring/publish/consume leg",
        })
    }
}

/// Turn one stall window's evidence into a [`StallVerdict`]. Pure — unit-tested beside the
/// [`StallWatch`] tests.
///
/// Thresholds: a heartbeat that was ever `max(gap/2, 250 ms)` stale convicts the worker (its
/// scheduled cadence is ≤16 ms, so 250 ms of silence is real starvation, and gap/2 scales the bar
/// for long holes); `offered_delta ≥ 8` acquits DWM (8 composed frames during the "hole" mirrors
/// [`StallWatch::RECENT`]'s definition of sustained flow — the stall-ending frame plus a resume
/// burst stay well under it).
pub(super) fn attribute(gap: Duration, evidence: &StallEvidence) -> StallVerdict {
    let Some(offered) = evidence.offered_delta else {
        return StallVerdict::NoTelemetry;
    };
    let gap_ms = gap.as_millis() as u64;
    if evidence.max_heartbeat_age_ms >= (gap_ms / 2).max(250) {
        StallVerdict::WorkerStalled
    } else if offered >= 8 {
        StallVerdict::DeliveryLeg
    } else {
        StallVerdict::ComposeSilence
    }
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
    /// Running per-verdict tally (worker-stalled / compose-silence / delivery-leg / no-telemetry),
    /// in [`StallVerdict`] order — the metronomic WARN prints it, so one pasted line attributes the
    /// whole session's beat, not just the stall that tripped the metronome.
    verdicts: [u32; 4],
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
            verdicts: [0; 4],
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
    /// `evidence` is the capturer's driver-telemetry sample for the window; its [`attribute`]
    /// verdict rides every stall line, so a field log names which leg lost the frames instead of
    /// leaving it to hypothesis.
    pub(super) fn report(&mut self, stall: &Stall, now: Instant, evidence: &StallEvidence) {
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
        let verdict = attribute(stall.gap, evidence);
        self.verdicts[match verdict {
            StallVerdict::NoTelemetry => 0,
            StallVerdict::WorkerStalled => 1,
            StallVerdict::ComposeSilence => 2,
            StallVerdict::DeliveryLeg => 3,
        }] += 1;
        // debug (not warn): a single hole also happens when content legitimately pauses;
        // the reportable signal is the metronomic cycle below. Mounjay-class triage runs
        // at debug level, and the web-console debug ring captures these.
        tracing::debug!(
            gap_ms = stall.gap.as_millis() as u64,
            os_display_events = %pf_win_display::display_events::summarize(&events),
            verdict = %verdict,
            offered_during_gap = evidence.offered_delta,
            max_heartbeat_age_ms = evidence.max_heartbeat_age_ms,
            "IDD-push capture stall — the desktop was composing at speed, then the ring \
             delivered no frame for the gap; the verdict names the leg that lost them"
        );
        if let Some(period) = stall.metronomic {
            let suspects = pf_win_display::display_events::connected_inactive_physicals();
            let suspects = if suspects.is_empty() {
                "none".to_string()
            } else {
                suspects.join(", ")
            };
            let correlated = format!("{}/{}", self.with_os_events, self.seen);
            // The session's attribution in one token: which leg the evidence convicted, per stall.
            let verdict_tally = format!(
                "worker-stalled {}, compose-silence {}, delivery-leg {}, no-telemetry {}",
                self.verdicts[1], self.verdicts[2], self.verdicts[3], self.verdicts[0]
            );
            // Half-or-more of the stalls carrying a coinciding OS event = the reaction
            // cascade is OS-visible; otherwise the disturbance never surfaces above the
            // driver. Different classes, different cures — say which one this box has.
            if self.with_os_events * 2 >= self.seen {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    verdicts = %verdict_tally,
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
                    verdicts = %verdict_tally,
                    "capture stalls are METRONOMIC with NO coinciding OS display event — \
                     the disturbance is BELOW Windows: the GPU driver servicing a \
                     connected-but-asleep sink (standby HPD/DDC/link probing), \
                     display-poller software (the SteelSeries-GG/SignalRGB class — \
                     correlate 'slow display-descriptor poll' lines), or the DWM present \
                     clock (try a different refresh rate). If connected_inactive lists a \
                     display, its standby servicing is the prime suspect. For a LAPTOP \
                     PANEL (the exclusive isolate deactivated it — the dark-but-connected \
                     head is itself the disturbance on hybrid laptops): keep it active \
                     with `topology: primary`, or try the `pnp_disable_monitors` axis. \
                     For an external display: unplug it at the GPU, disable its OSD auto \
                     input scan (TVs: instant-on/quick-start + CEC off), use an \
                     HPD-holding adapter/dummy, or keep it active while streaming"
                );
            }
        }
    }
}
