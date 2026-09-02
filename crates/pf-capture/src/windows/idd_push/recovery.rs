//! The capturer's recovery supervisor (immunity plan WP13 wiring): feeds the pure
//! [`health::Classifier`] from the clocks the capturer already keeps, hands its verdicts to the
//! pure [`recovery::Coordinator`], and tells the capturer which actuator to run. It retires the
//! WP3b interim watchdog: same 15 s floor, same "no evidence = idle", but the ladder replaces the
//! one-rebuild-then-terminal rule and the decision is evidence-classed.
//!
//! This first slice runs ON the capture thread: the only actuators reachable today — a ring
//! recreate and the same-mode presentation restart — are capture-thread-owned, and the host's
//! pipeline rebuild (its own 5-attempt budget) is the rung above `Failed`. `MonitorCycle`,
//! `DriverCycle` and `CaptureFallback` report `Unsupported` until the manager exposes them.

use std::time::{Duration, Instant};

use pf_driver_proto::frame::HealthState;
use pf_frame::health::{
    Activity, ActivityKind, Classifier, HealthClass, RingState, Snapshot, Thresholds,
};
use pf_frame::recovery::{Action, Budget, Coordinator, Event, Stage, StageOutcome, Summary};

/// Cursor travel over a frozen image that counts as INPUT evidence — a couple of real mouse
/// movements, comfortably above sub-pixel jitter (the WP3b value).
const INPUT_EVIDENCE_PX: u32 = 64;
/// No canary before this much missed source, and at most one per interval: the only canary we
/// have is the input kick (plan: "old-OS fallback"), which briefly parks the pointer.
const CANARY_AFTER: Duration = Duration::from_secs(5);
/// A canary that no source frame answered within this much is strong evidence.
const CANARY_ANSWER: Duration = Duration::from_secs(1);
/// Classification cadence outside an episode (the deadlines are seconds).
const TICK: Duration = Duration::from_millis(250);

/// What the capturer sampled this tick.
pub(super) struct Inputs {
    pub now: Instant,
    /// The last `FrameOrigin::Source` frame.
    pub last_source: Instant,
    pub source_seq: u64,
    /// Age of the driver worker's drain heartbeat (v2 telemetry); `None` before the first one.
    pub heartbeat_age: Option<Duration>,
    /// The driver's `offered_total` (composed frames it acquired); `None` on a pre-v2 driver.
    pub offered: Option<u64>,
    /// Age of the ring's last publish (v3 health tail); `None` when unknown.
    pub publish_age: Option<Duration>,
    /// Cursor travel since the last source frame.
    pub cursor_gap_px: u32,
    pub ring: Option<HealthState>,
    /// A ring recreate is in flight (`recovering_since`).
    pub recreating: bool,
    pub secure_desktop: bool,
    pub topology_held: bool,
}

/// What the capturer does now.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Nothing,
    /// Present the composition canary (the input kick).
    Canary,
    Run(Stage),
    Recovered(Summary),
    /// The ladder is exhausted; `gap` is the source gap to report in the typed fault.
    Failed {
        gap: Duration,
        summary: Summary,
    },
}

pub(super) struct Supervisor {
    classifier: Classifier,
    coordinator: Coordinator,
    last_tick: Instant,
    /// The last verdict's source gap, for the typed fault when a stage report ends the ladder.
    last_gap: Duration,
    offered_last: u64,
    /// When `offered_total` last advanced (the driver acquired a composed frame).
    offered_at: Option<Instant>,
    /// When cursor travel first crossed the evidence bar in this gap.
    input_at: Option<Instant>,
    /// When the last canary went out.
    canary_at: Option<Instant>,
}

impl Supervisor {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            classifier: Classifier::new(Thresholds::default(), now),
            coordinator: Coordinator::new(Budget::default()),
            last_tick: now,
            last_gap: Duration::ZERO,
            offered_last: 0,
            offered_at: None,
            input_at: None,
            canary_at: None,
        }
    }

    /// An episode is open: the capturer's own recover-or-drop timers stand down.
    pub(super) fn owns_episode(&self) -> bool {
        self.coordinator.owns_episode()
    }

    /// One tick of the classifier and coordinator over `i`.
    pub(super) fn tick(&mut self, i: Inputs) -> Step {
        if !self.owns_episode() && i.now.saturating_duration_since(self.last_tick) < TICK {
            return Step::Nothing;
        }
        self.last_tick = i.now;
        if let Some(offered) = i.offered {
            if offered != self.offered_last {
                self.offered_last = offered;
                self.offered_at = Some(i.now);
            }
        }
        if i.cursor_gap_px >= INPUT_EVIDENCE_PX && self.input_at.is_none() {
            self.input_at = Some(i.now);
        }
        let snap = Snapshot {
            now: i.now,
            worker_heartbeat: i.heartbeat_age.and_then(|a| i.now.checked_sub(a)),
            last_acquire: self.offered_at,
            last_publish: i.publish_age.and_then(|a| i.now.checked_sub(a)),
            last_source: Some(i.last_source),
            source_seq: i.source_seq,
            last_encoded: None,
            activity: evidence(
                i.now,
                i.last_source,
                self.input_at,
                self.offered_at,
                self.canary_at,
            ),
            ring: ring_state(i.ring, i.recreating),
            topology_in_transaction: i.topology_held,
            secure_desktop: i.secure_desktop,
        };
        let (verdict, changed) = self.classifier.observe(&snap);
        self.last_gap = verdict.source_gap;
        if changed {
            match verdict.class {
                HealthClass::Suspect | HealthClass::Stalled(_) => tracing::info!(
                    class = ?verdict.class,
                    gap_s = verdict.source_gap.as_secs(),
                    evidence = ?verdict.evidence,
                    "IDD push: capture health changed"
                ),
                _ => tracing::debug!(class = ?verdict.class, "IDD push: capture health changed"),
            }
        }
        let action = match self.coordinator.step(i.now, Event::Verdict(verdict.class)) {
            Action::None => self.coordinator.step(i.now, Event::Tick),
            a => a,
        };
        match self.act(action, verdict.source_gap) {
            Step::Nothing => {}
            step => return step,
        }
        if verdict.wants_canary
            && verdict.source_gap >= CANARY_AFTER
            && self
                .canary_at
                .is_none_or(|t| i.now.saturating_duration_since(t) >= CANARY_AFTER)
        {
            self.canary_at = Some(i.now);
            return Step::Canary;
        }
        Step::Nothing
    }

    /// The running stage's actuator finished.
    pub(super) fn stage_done(&mut self, now: Instant, stage: Stage, outcome: StageOutcome) -> Step {
        let action = self.coordinator.step(now, Event::StageDone(stage, outcome));
        self.act(action, self.last_gap)
    }

    /// A NEW source frame arrived (never a regen or hold). Clears the gap's evidence; closes a
    /// proving episode once the budgeted count has landed.
    pub(super) fn source_frame(&mut self, now: Instant) -> Option<Summary> {
        self.input_at = None;
        self.canary_at = None;
        let progress = Event::Progress {
            new_source_frames: 1,
            assignment_changed: false,
        };
        match self.coordinator.step(now, progress) {
            Action::Recovered => self.coordinator.last_summary().cloned(),
            _ => None,
        }
    }

    fn act(&mut self, action: Action, gap: Duration) -> Step {
        match action {
            Action::None => Step::Nothing,
            Action::Run(stage) => Step::Run(stage),
            Action::Recovered => self
                .coordinator
                .last_summary()
                .cloned()
                .map_or(Step::Nothing, Step::Recovered),
            Action::Failed => {
                let summary = self
                    .coordinator
                    .last_summary()
                    .cloned()
                    .expect("a failed episode leaves its summary");
                Step::Failed { gap, summary }
            }
        }
    }
}

/// The strongest activity evidence newer than the last source frame. An unanswered canary and
/// driver-side acquire progress are strong; cursor travel alone is weak (a hardware-cursor
/// desktop composes nothing while the pointer moves).
fn evidence(
    now: Instant,
    last_source: Instant,
    input_at: Option<Instant>,
    offered_at: Option<Instant>,
    canary_at: Option<Instant>,
) -> Option<Activity> {
    let mut all = Vec::with_capacity(3);
    if let Some(at) = input_at {
        all.push(Activity {
            at,
            kind: ActivityKind::Input,
        });
    }
    if let Some(at) = offered_at {
        all.push(Activity {
            at,
            kind: ActivityKind::Presents,
        });
    }
    if let Some(at) = canary_at.filter(|t| now.saturating_duration_since(*t) >= CANARY_ANSWER) {
        all.push(Activity {
            at,
            kind: ActivityKind::Canary,
        });
    }
    Activity::strongest_since(&all, Some(last_source))
}

fn ring_state(state: Option<HealthState>, recreating: bool) -> RingState {
    if recreating {
        return RingState::Rebuilding;
    }
    match state {
        Some(HealthState::Active) => RingState::Active,
        Some(HealthState::Rebuilding) => RingState::Rebuilding,
        Some(HealthState::Dead) => RingState::Dead,
        Some(HealthState::Initializing) | None => RingState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(now: Instant, last_source: Instant, cursor_gap_px: u32, offered: u64) -> Inputs {
        Inputs {
            now,
            last_source,
            source_seq: 1,
            heartbeat_age: Some(Duration::from_millis(5)),
            offered: Some(offered),
            publish_age: None,
            cursor_gap_px,
            ring: Some(HealthState::Active),
            recreating: false,
            secure_desktop: false,
            topology_held: false,
        }
    }

    /// The WP3b contract survives the hand-over: under the floor nothing runs; over it, cursor
    /// travel alone asks for a canary and never an actuator; undelivered driver offers past the
    /// floor open a TRANSPORT episode whose first rung is the ring reset, and three new source
    /// frames close it.
    #[test]
    fn supervisor_walks_idle_canary_ring_reset_recovered() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        // Static desktop, no evidence: idle forever.
        assert_eq!(sv.tick(inputs(s(60), t0, 0, 0)), Step::Nothing);
        // Cursor travel past the floor: canary, not an actuator (weak evidence).
        assert_eq!(sv.tick(inputs(s(61), t0, 200, 0)), Step::Canary);
        // An unanswered canary is strong: the presentation ladder opens.
        assert_eq!(
            sv.tick(inputs(s(62), t0, 200, 0)),
            Step::Run(Stage::PresentationReset)
        );
        assert!(sv.owns_episode());

        // A fresh supervisor: the driver keeps acquiring frames the ring never delivers.
        let mut sv = Supervisor::new(t0);
        assert_eq!(sv.tick(inputs(s(1), t0, 0, 0)), Step::Nothing);
        assert_eq!(sv.tick(inputs(s(10), t0, 0, 5)), Step::Nothing);
        assert_eq!(
            sv.tick(inputs(s(16), t0, 0, 9)),
            Step::Run(Stage::RingReset)
        );
        assert_eq!(
            sv.stage_done(s(16), Stage::RingReset, StageOutcome::Applied),
            Step::Nothing
        );
        assert_eq!(sv.source_frame(s(17)), None);
        assert_eq!(sv.source_frame(s(17)), None);
        let summary = sv.source_frame(s(17)).expect("three source frames recover");
        assert!(summary.recovered);
        assert!(!sv.owns_episode());
    }
}
