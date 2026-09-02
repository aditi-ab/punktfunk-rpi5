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

use crate::{CaptureEpisode, CaptureHealth};
use pf_driver_proto::frame::HealthState;
use pf_frame::health::{
    Activity, ActivityKind, Classifier, HealthClass, RingState, Snapshot, StallClass, Thresholds,
};
use pf_frame::recovery::{Action, Budget, Coordinator, Event, Stage, StageOutcome, Summary};

fn stall_name(c: StallClass) -> &'static str {
    match c {
        StallClass::Worker => "worker",
        StallClass::Transport => "transport",
        StallClass::Conversion => "conversion",
        StallClass::Presentation => "presentation",
    }
}

fn stage_name(s: Stage) -> &'static str {
    match s {
        Stage::EncoderReset => "encoder_reset",
        Stage::RingReset => "ring_reset",
        Stage::SwapChainReset => "swap_chain_reset",
        Stage::PresentationReset => "presentation_reset",
        Stage::MonitorCycle => "monitor_cycle",
        Stage::DriverCycle => "driver_cycle",
        Stage::CaptureFallback => "capture_fallback",
    }
}

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
    /// Composed frames the driver acquired that the ring has NOT delivered: `offered_total`
    /// minus its value at the last source frame. `None` on a pre-v2 driver. The raw counter is
    /// unusable here — read one tick after a delivered frame it is "newer than the source" on
    /// every desktop pause, and a static desktop became a 15 s transport stall.
    pub offered_undelivered: Option<u64>,
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
    /// The source gap when the open episode began, and when: together they measure the outage.
    opened: Option<(Duration, Instant)>,
    undelivered_last: u64,
    /// When the undelivered-offer count last grew (the driver acquired a composed frame the
    /// ring did not deliver).
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
            opened: None,
            undelivered_last: 0,
            offered_at: None,
            input_at: None,
            canary_at: None,
        }
    }

    /// An episode is open: the capturer's own recover-or-drop timers stand down.
    pub(super) fn owns_episode(&self) -> bool {
        self.coordinator.owns_episode()
    }

    /// The operator-surface report (WP18): the last verdict, the ring's self-report, and the
    /// last closed episode, with every enum spelled as its lowercase name.
    pub(super) fn report(&self, now: Instant, ring: Option<&super::RingHealth>) -> CaptureHealth {
        let verdict = self.classifier.last();
        let (class, stall_class) = match verdict.map(|v| v.class) {
            None => ("healthy", None),
            Some(HealthClass::Healthy) => ("healthy", None),
            Some(HealthClass::Idle) => ("idle", None),
            Some(HealthClass::Suspect) => ("suspect", None),
            Some(HealthClass::Stalled(c)) => ("stalled", Some(stall_name(c))),
            Some(HealthClass::Recovering) => ("recovering", None),
            Some(HealthClass::Rebuilding) => ("rebuilding", None),
            Some(HealthClass::SecureDesktop) => ("secure_desktop", None),
        };
        CaptureHealth {
            class,
            stall_class,
            source_gap: verdict.map_or(Duration::ZERO, |v| v.source_gap),
            evidence: verdict.and_then(|v| v.evidence).map(|k| match k {
                ActivityKind::RecentSource => "recent_source",
                ActivityKind::Input => "input",
                ActivityKind::Canary => "canary",
                ActivityKind::Presents => "presents",
            }),
            ring_state: ring.map(|r| match r.state {
                HealthState::Initializing => "initializing",
                HealthState::Active => "active",
                HealthState::Rebuilding => "rebuilding",
                HealthState::Dead => "dead",
            }),
            fence_ring: ring
                .is_some_and(|r| r.negotiated & pf_driver_proto::frame::CAP_FENCE_RING != 0),
            published_total: ring.map_or(0, |r| r.published_total),
            dropped_total: ring.map_or(0, |r| r.dropped_total),
            current_stage: self.coordinator.current_stage().map(stage_name),
            last_episode: self.coordinator.last_summary().map(|s| CaptureEpisode {
                stall_class: stall_name(s.class),
                recovered: s.recovered,
                took: s.took,
                stages: s
                    .stages
                    .iter()
                    .map(|r| {
                        let outcome = match r.outcome {
                            Some(StageOutcome::Applied) => "applied",
                            Some(StageOutcome::Failed) => "failed",
                            Some(StageOutcome::Unsupported) => "unsupported",
                            None => "timed_out",
                        };
                        (stage_name(r.stage), outcome, r.took)
                    })
                    .collect(),
                consecutive_failures: s.consecutive_failures,
                cooldown: s.cooldown,
            }),
            episodes_suppressed: self.coordinator.suppressed(),
            cooldown_remaining: self
                .coordinator
                .cooldown_until()
                .and_then(|t| t.checked_duration_since(now)),
        }
    }

    /// One tick of the classifier and coordinator over `i`.
    pub(super) fn tick(&mut self, i: Inputs) -> Step {
        if !self.owns_episode() && i.now.saturating_duration_since(self.last_tick) < TICK {
            return Step::Nothing;
        }
        self.last_tick = i.now;
        if let Some(undelivered) = i.offered_undelivered {
            if undelivered != self.undelivered_last {
                self.undelivered_last = undelivered;
                self.offered_at = (undelivered > 0).then_some(i.now);
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
        let owned = self.owns_episode();
        let action = match self.coordinator.step(i.now, Event::Verdict(verdict.class)) {
            Action::None => self.coordinator.step(i.now, Event::Tick),
            a => a,
        };
        if !owned && self.owns_episode() {
            self.opened = Some((verdict.source_gap, i.now));
        }
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
    /// proving episode once the budgeted count has landed, returning its summary and the
    /// measured outage (last source frame before the stall to this one).
    pub(super) fn source_frame(&mut self, now: Instant) -> Option<(Summary, Duration)> {
        self.input_at = None;
        self.canary_at = None;
        let progress = Event::Progress {
            new_source_frames: 1,
            assignment_changed: false,
        };
        match self.coordinator.step(now, progress) {
            Action::Recovered => {
                let outage = self.opened.take().map_or(Duration::ZERO, |(gap, at)| {
                    gap + now.saturating_duration_since(at)
                });
                self.coordinator
                    .last_summary()
                    .cloned()
                    .map(|s| (s, outage))
            }
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

/// Undelivered offers count as "presents continue" only while they keep coming: a single
/// trailing frame the ring dropped (slot busy, descriptor mismatch) before the desktop went
/// static is a stale image, not a stalled transport, and a ring reset would cost more than it
/// returns. A stalled transport under a changing desktop grows the count every tick.
const PRESENTS_CONTINUE: Duration = Duration::from_secs(3);

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
    if let Some(at) = offered_at.filter(|t| now.saturating_duration_since(*t) <= PRESENTS_CONTINUE)
    {
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

    fn inputs(now: Instant, last_source: Instant, cursor_gap_px: u32, undelivered: u64) -> Inputs {
        Inputs {
            now,
            last_source,
            source_seq: 1,
            heartbeat_age: Some(Duration::from_millis(5)),
            offered_undelivered: Some(undelivered),
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
        let (summary, outage) = sv.source_frame(s(17)).expect("three source frames recover");
        assert!(summary.recovered);
        // The outage is the whole hole: 16 s of missed source before the episode plus 1 s in it.
        assert_eq!(outage, Duration::from_secs(17));
        assert!(!sv.owns_episode());
    }

    /// One undelivered offer left behind by a dropped frame, then a static desktop: the evidence
    /// goes stale and the gap is idle — no ring reset for a stale image. Offers that keep coming
    /// past the floor are the transport stall they always were.
    #[test]
    fn single_trailing_undelivered_offer_is_idle_not_a_transport_stall() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        assert_eq!(sv.tick(inputs(s(1), t0, 0, 1)), Step::Nothing);
        assert_eq!(sv.tick(inputs(s(16), t0, 0, 1)), Step::Nothing);
        assert_eq!(sv.tick(inputs(s(30), t0, 0, 1)), Step::Nothing);
        assert!(!sv.owns_episode());
        assert_eq!(
            sv.tick(inputs(s(31), t0, 0, 2)),
            Step::Run(Stage::RingReset)
        );
    }
}
