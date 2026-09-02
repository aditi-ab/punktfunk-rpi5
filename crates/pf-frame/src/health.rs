//! Live capture-health classifier (vdisplay immunity plan WP12; decisions D7 "recovery follows
//! evidence" and D8 "recovery is a state machine"). PURE: it reads timestamped progress clocks the
//! capturer collected and names the class of a gap — it does no I/O, owns no thread, and fires no
//! actuator. The WP13 coordinator turns a [`HealthClass::Stalled`] verdict into its first actuator;
//! everything before that stage is a label.
//!
//! The independent clocks (D7): the driver worker's drain heartbeat, the driver's last composed
//! ACQUIRE, the ring PUBLISH, the last [`FrameOrigin::Source`](crate::FrameOrigin::Source) frame the
//! consumer received, and the last encoded access unit. Which of them kept moving through a gap is
//! what tells a starved worker from a dead ring from a presentation path that composes nothing.
//! A gap with no activity evidence at all is IDLE — a static desktop composes nothing, and that is
//! healthy — so the classifier never resets a quiet display.

use std::time::{Duration, Instant};

/// Something that shows the desktop SHOULD have produced a new image. Strength decides how far a
/// gap may escalate on it (immunity plan WP12, "activity sources, strongest first").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivityKind {
    /// Tightly spaced recent `Source` frames just before the gap — a provisional gate only.
    RecentSource,
    /// Host input or cursor motion aimed at this display. With a hardware cursor a moving pointer
    /// composes nothing, so on its own this can only raise suspicion (and request a canary).
    Input,
    /// A targeted composition canary was presented after suspicion and no source frame followed.
    Canary,
    /// Process-attributed ETW presents landed since the last source frame.
    Presents,
}

impl ActivityKind {
    /// Whether this evidence may carry a gap past the stall floor into a recovery verdict.
    /// Weak evidence stops at [`HealthClass::Suspect`] and asks for a canary instead.
    pub fn is_strong(self) -> bool {
        matches!(self, Self::Canary | Self::Presents)
    }
}

/// One activity observation: when, and what kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activity {
    pub at: Instant,
    pub kind: ActivityKind,
}

impl Activity {
    /// The strongest observation newer than `after` (ties: the latest).
    pub fn strongest_since<'a>(
        all: impl IntoIterator<Item = &'a Activity>,
        after: Option<Instant>,
    ) -> Option<Activity> {
        all.into_iter()
            .filter(|a| after.is_none_or(|t| a.at > t))
            .copied()
            .max_by(|a, b| a.kind.cmp(&b.kind).then(a.at.cmp(&b.at)))
    }
}

/// The ring's own word on itself (shared-header v3 `HealthState`); `Unknown` on a pre-v3 driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RingState {
    #[default]
    Unknown,
    Active,
    /// A generation is being superseded — a quiet ring is expected, not evidence.
    Rebuilding,
    /// The generation is poisoned (terminal error recorded) — transport recovery, no floor.
    Dead,
}

/// Everything the classifier looks at, sampled at `now`. `None` clocks mean "never observed".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub now: Instant,
    /// Driver worker drain-loop heartbeat (v2 telemetry). Stale = the worker is starved or wedged.
    pub worker_heartbeat: Option<Instant>,
    /// The driver last ACQUIRED a composed frame from the swap-chain.
    pub last_acquire: Option<Instant>,
    /// The ring's publish token last advanced.
    pub last_publish: Option<Instant>,
    /// The consumer last received a `FrameOrigin::Source` frame, and its sequence.
    pub last_source: Option<Instant>,
    pub source_seq: u64,
    /// The encoder last produced an access unit from a source frame.
    pub last_encoded: Option<Instant>,
    /// The strongest activity evidence newer than `last_source` ([`Activity::strongest_since`]).
    pub activity: Option<Activity>,
    pub ring: RingState,
    /// A display-actor topology transaction owns the display right now.
    pub topology_in_transaction: bool,
    /// UAC / Winlogon secure desktop is up: a separate state, never a failed canary.
    pub secure_desktop: bool,
}

/// What a stalled gap points at — and thereby its first actuator (D7 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallClass {
    /// Worker heartbeat stale → replace the swap-chain and device.
    Worker,
    /// Acquire advanced, ring publish did not (or the ring says Dead) → recreate the ring endpoint.
    Transport,
    /// Publish (or the source) advanced, the encoded AU did not → video-worker / encoder reset.
    Conversion,
    /// Presents continue, the source sequence does not → one targeted presentation reset.
    Presentation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthClass {
    /// Source frames arrive within the expected cadence.
    Healthy,
    /// No source frames and no activity evidence — a static desktop. No recovery, ever.
    Idle,
    /// Source missed several intervals with activity evidence, below the stall floor — or past it
    /// on WEAK evidence only. The verdict asks for a canary here.
    Suspect,
    /// Past the stall floor on strong evidence (or a Dead ring): the coordinator's first actuator.
    Stalled(StallClass),
    /// After a stall: source frames are back but fewer than the recovery count have landed.
    Recovering,
    /// A ring rebuild or topology transaction owns the display — hold, count nothing.
    Rebuilding,
    SecureDesktop,
}

/// Tunables. Frame-relative where the plan says so, with absolute floors anchored to the recorded
/// field envelope (benign vendor holes run 1.6–10 s; multi-second holes recur every 20–45 s), so the
/// defaults are the values the plan fixes, not fresh guesses. Tests pin them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// The display's frame interval (1 / refresh).
    pub frame_interval: Duration,
    /// Missed source intervals before suspicion (subject to `suspect_floor`).
    pub suspect_missed_intervals: u32,
    pub suspect_floor: Duration,
    /// No actuator before this much continuously missed source (WP3b's floor carries over).
    pub stall_floor: Duration,
    /// A worker heartbeat older than this at stall time names the WORKER class.
    pub heartbeat_stale: Duration,
    /// Real source frames required after a stall before the class is `Healthy` again.
    pub recover_frames: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            frame_interval: Duration::from_micros(16_667),
            suspect_missed_intervals: 30,
            suspect_floor: Duration::from_millis(1_500),
            stall_floor: Duration::from_secs(15),
            heartbeat_stale: Duration::from_secs(2),
            recover_frames: 3,
        }
    }
}

impl Thresholds {
    /// How long without a source frame counts as suspicious.
    pub fn suspect_after(&self) -> Duration {
        (self.frame_interval * self.suspect_missed_intervals).max(self.suspect_floor)
    }
}

/// One classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub class: HealthClass,
    /// Time since the last source frame (since the classifier started, if none yet).
    pub source_gap: Duration,
    /// The evidence the verdict rests on, if any.
    pub evidence: Option<ActivityKind>,
    /// Suspicion on weak-or-no strong evidence: the coordinator should issue a targeted canary
    /// (never move the user's cursor) so the next snapshot can carry strong evidence.
    pub wants_canary: bool,
}

/// The stateless core: name `snap`'s class given the clocks alone. `start` stands in for a source
/// frame that never came. Recovery hysteresis lives in [`Classifier`].
pub fn classify(th: &Thresholds, snap: &Snapshot, start: Instant) -> Verdict {
    let anchor = snap.last_source.unwrap_or(start);
    let source_gap = snap.now.saturating_duration_since(anchor);
    let evidence = snap.activity.map(|a| a.kind);
    let verdict = |class, wants_canary| Verdict {
        class,
        source_gap,
        evidence,
        wants_canary,
    };
    if snap.secure_desktop {
        return verdict(HealthClass::SecureDesktop, false);
    }
    if snap.topology_in_transaction || snap.ring == RingState::Rebuilding {
        return verdict(HealthClass::Rebuilding, false);
    }
    // The ring told us itself: a poisoned generation needs no floor and no activity evidence.
    if snap.ring == RingState::Dead {
        return verdict(HealthClass::Stalled(StallClass::Transport), false);
    }
    let advanced = |clock: Option<Instant>| clock.is_some_and(|t| t > anchor);
    if source_gap < th.suspect_after() {
        // Source is flowing. The one thing that can still be wrong is downstream of us: the ring
        // hands frames over, the encoder emits nothing through a whole stall floor.
        let encode_gap = snap
            .last_encoded
            .map_or(source_gap, |t| snap.now.saturating_duration_since(t));
        if snap.last_source.is_some() && encode_gap >= th.stall_floor {
            return verdict(HealthClass::Stalled(StallClass::Conversion), false);
        }
        return verdict(HealthClass::Healthy, false);
    }
    let heartbeat_stale = snap
        .worker_heartbeat
        .is_none_or(|t| snap.now.saturating_duration_since(t) >= th.heartbeat_stale);
    // The drain heartbeat ticks every pass, frames or not: silent past the floor it convicts the
    // worker by itself. A frozen driver over a still desktop has no other witness.
    if source_gap >= th.stall_floor && snap.worker_heartbeat.is_some() && heartbeat_stale {
        return verdict(HealthClass::Stalled(StallClass::Worker), false);
    }
    let Some(kind) = evidence else {
        return verdict(HealthClass::Idle, false);
    };
    if source_gap < th.stall_floor {
        return verdict(HealthClass::Suspect, !kind.is_strong());
    }
    if !kind.is_strong() {
        // Past the floor on cursor/input alone: a hardware-cursor desktop looks exactly like this.
        // Ask for the canary; the next snapshot decides.
        return verdict(HealthClass::Suspect, true);
    }
    let class = if heartbeat_stale {
        StallClass::Worker
    } else if advanced(snap.last_acquire) && !advanced(snap.last_publish) {
        StallClass::Transport
    } else if advanced(snap.last_publish) {
        // The ring moved but no Source frame reached the encoder: the consumer/conversion leg.
        StallClass::Conversion
    } else {
        StallClass::Presentation
    };
    verdict(HealthClass::Stalled(class), false)
}

/// [`classify`] plus the hysteresis D8 asks for: a stall clears only after `recover_frames` real
/// source frames, never on one stash republish or cursor regeneration.
#[derive(Clone, Debug)]
pub struct Classifier {
    th: Thresholds,
    start: Instant,
    last: Option<Verdict>,
    /// Set by a stall; cleared once `recover_frames` source frames have landed.
    recovering: Option<RecoverTrack>,
}

#[derive(Clone, Copy, Debug)]
struct RecoverTrack {
    seq_at_stall: u64,
    last_seq: u64,
    frames: u32,
}

impl Classifier {
    pub fn new(th: Thresholds, now: Instant) -> Self {
        Self {
            th,
            start: now,
            last: None,
            recovering: None,
        }
    }

    pub fn thresholds(&self) -> &Thresholds {
        &self.th
    }

    /// The previous verdict, if any.
    pub fn last(&self) -> Option<Verdict> {
        self.last
    }

    /// Classify `snap`; returns the verdict and whether the CLASS changed since the last call (one
    /// transition log per change is the WP13 budget).
    pub fn observe(&mut self, snap: &Snapshot) -> (Verdict, bool) {
        let mut v = classify(&self.th, snap, self.start);
        match (&mut self.recovering, v.class) {
            // Every stall — a relapse included — restarts the count: frames before it are no proof.
            (_, HealthClass::Stalled(_)) => {
                self.recovering = Some(RecoverTrack {
                    seq_at_stall: snap.source_seq,
                    last_seq: snap.source_seq,
                    frames: 0,
                });
            }
            (Some(track), HealthClass::Healthy) => {
                // Only a NEW source sequence counts — a republish or cursor regen keeps the seq.
                if snap.source_seq > track.last_seq {
                    track.frames += 1;
                    track.last_seq = snap.source_seq;
                }
                if track.frames >= self.th.recover_frames && snap.source_seq > track.seq_at_stall {
                    self.recovering = None;
                } else {
                    v.class = HealthClass::Recovering;
                }
            }
            _ => {}
        }
        let changed = self.last.is_none_or(|p| p.class != v.class);
        self.last = Some(v);
        (v, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn quiet(now: Instant) -> Snapshot {
        Snapshot {
            now,
            worker_heartbeat: Some(now),
            last_acquire: None,
            last_publish: None,
            last_source: None,
            source_seq: 0,
            last_encoded: None,
            activity: None,
            ring: RingState::Active,
            topology_in_transaction: false,
            secure_desktop: false,
        }
    }

    /// A display streaming at cadence: source, acquire, publish, encode all fresh.
    fn flowing(start: Instant, now: Instant, seq: u64) -> Snapshot {
        Snapshot {
            last_acquire: Some(now),
            last_publish: Some(now),
            last_source: Some(now),
            source_seq: seq,
            last_encoded: Some(now),
            ..quiet(start)
        }
        .at(now)
    }

    impl Snapshot {
        /// Advance the clock with the worker alive (its heartbeat is per drain pass, ≤16 ms).
        fn at(mut self, now: Instant) -> Self {
            self.now = now;
            self.worker_heartbeat = Some(now);
            self
        }
        fn with(mut self, kind: ActivityKind, at: Instant) -> Self {
            self.activity = Some(Activity { at, kind });
            self
        }
    }

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn defaults_are_the_plan_values() {
        let th = Thresholds::default();
        assert_eq!(
            th.stall_floor,
            Duration::from_secs(15),
            "WP3b's floor carries over"
        );
        assert_eq!(th.suspect_after(), Duration::from_millis(1_500));
        assert_eq!(th.recover_frames, 3);
        assert!(th.suspect_after() < th.stall_floor);
    }

    #[test]
    fn active_to_idle_never_recovers() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let mut snap = flowing(s, last, 100);
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Healthy);
        // Ten minutes of nothing, no evidence at all: Idle at every point, never Stalled.
        for secs in [2u32, 15, 60, 600] {
            snap = snap.at(last + S * secs);
            let v = classify(&th, &snap, s);
            assert_eq!(v.class, HealthClass::Idle, "at +{secs}s");
            assert!(!v.wants_canary);
        }
    }

    #[test]
    fn active_to_permanent_stall_is_presentation() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = flowing(s, last, 100);
        // Presents keep landing; below the floor it is Suspect, past it Stalled(Presentation).
        let v = classify(
            &th,
            &snap
                .at(last + 5 * S)
                .with(ActivityKind::Presents, last + 4 * S),
            s,
        );
        assert_eq!(v.class, HealthClass::Suspect);
        assert!(!v.wants_canary, "strong evidence needs no canary");
        let v = classify(
            &th,
            &snap
                .at(last + 16 * S)
                .with(ActivityKind::Presents, last + 15 * S),
            s,
        );
        assert_eq!(v.class, HealthClass::Stalled(StallClass::Presentation));
        assert_eq!(v.evidence, Some(ActivityKind::Presents));
    }

    #[test]
    fn cursor_only_desktop_stops_at_suspect_and_asks_for_a_canary() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = flowing(s, last, 100);
        let v = classify(
            &th,
            &snap
                .at(last + 30 * S)
                .with(ActivityKind::Input, last + 29 * S),
            s,
        );
        assert_eq!(
            v.class,
            HealthClass::Suspect,
            "a hardware cursor composes nothing"
        );
        assert!(v.wants_canary);
        // The canary came back with no source frame behind it: now it is a real stall.
        let v = classify(
            &th,
            &snap
                .at(last + 31 * S)
                .with(ActivityKind::Canary, last + 30 * S),
            s,
        );
        assert_eq!(v.class, HealthClass::Stalled(StallClass::Presentation));
    }

    #[test]
    fn evidence_ranking_prefers_strength_then_recency() {
        let s = t0();
        let all = [
            Activity {
                at: s + 3 * S,
                kind: ActivityKind::Input,
            },
            Activity {
                at: s + 1 * S,
                kind: ActivityKind::Presents,
            },
            Activity {
                at: s + 2 * S,
                kind: ActivityKind::Presents,
            },
            Activity {
                at: s,
                kind: ActivityKind::Canary,
            },
        ];
        let best = Activity::strongest_since(&all, Some(s)).unwrap();
        assert_eq!((best.kind, best.at), (ActivityKind::Presents, s + 2 * S));
        // Everything at or before `after` is out.
        assert!(Activity::strongest_since(&all, Some(s + 3 * S)).is_none());
    }

    #[test]
    fn ring_delivery_loss_is_transport() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let now = last + 16 * S;
        // The driver keeps acquiring composed frames; the ring never publishes them.
        let snap = Snapshot {
            last_acquire: Some(now),
            last_publish: Some(last),
            worker_heartbeat: Some(now),
            ..flowing(s, last, 100)
        }
        .at(now)
        .with(ActivityKind::Presents, now - S);
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Transport)
        );
    }

    #[test]
    fn dead_ring_is_transport_without_floor_or_evidence() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = Snapshot {
            ring: RingState::Dead,
            ..flowing(s, last, 100)
        }
        .at(last + Duration::from_millis(100));
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Transport)
        );
    }

    #[test]
    fn worker_death_is_worker() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let now = last + 16 * S;
        let snap = Snapshot {
            worker_heartbeat: Some(last),
            ..flowing(s, last, 100)
                .at(now)
                .with(ActivityKind::Presents, now - S)
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Worker)
        );
        // A driver that never reported a heartbeat reads the same way.
        let snap = Snapshot {
            worker_heartbeat: None,
            ..snap
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Worker)
        );
    }

    /// A frozen driver over a still desktop: no acquire, no input, no canary — only the silent
    /// heartbeat. That alone names the worker past the floor; a pre-v2 driver with no heartbeat
    /// at all keeps the activity gate and reads idle.
    #[test]
    fn silent_heartbeat_over_a_still_desktop_is_worker() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let now = last + 16 * S;
        let snap = Snapshot {
            worker_heartbeat: Some(last),
            ..flowing(s, last, 100).at(now)
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Worker)
        );
        let snap = Snapshot {
            worker_heartbeat: None,
            ..snap
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Idle);
    }

    #[test]
    fn ring_advancing_without_source_is_conversion() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let now = last + 16 * S;
        let snap = Snapshot {
            last_acquire: Some(now),
            last_publish: Some(now),
            ..flowing(s, last, 100)
        }
        .at(now)
        .with(ActivityKind::Presents, now - S);
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Conversion)
        );
    }

    #[test]
    fn encoder_silence_under_flowing_source_is_conversion() {
        let th = Thresholds::default();
        let s = t0();
        let now = s + 40 * S;
        let snap = Snapshot {
            last_encoded: Some(now - 16 * S),
            ..flowing(s, now, 2_000)
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Conversion)
        );
        // Fourteen seconds of encoder silence is still inside the floor.
        let snap = Snapshot {
            last_encoded: Some(now - 14 * S),
            ..snap
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Healthy);
    }

    #[test]
    fn topology_transaction_and_rebuilding_hold() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let base = flowing(s, last, 100)
            .at(last + 20 * S)
            .with(ActivityKind::Presents, last + 19 * S);
        let snap = Snapshot {
            topology_in_transaction: true,
            ..base
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Rebuilding);
        let snap = Snapshot {
            ring: RingState::Rebuilding,
            ..base
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Rebuilding);
    }

    #[test]
    fn secure_desktop_is_its_own_state() {
        let th = Thresholds::default();
        let s = t0();
        let snap = Snapshot {
            secure_desktop: true,
            ..quiet(s + 60 * S)
        }
        .with(ActivityKind::Canary, s + 59 * S);
        assert_eq!(classify(&th, &snap, s).class, HealthClass::SecureDesktop);
    }

    #[test]
    fn no_first_frame_yet_anchors_on_start() {
        let th = Thresholds::default();
        let s = t0();
        let v = classify(&th, &quiet(s + S), s);
        assert_eq!(v.class, HealthClass::Healthy, "inside the suspect window");
        let v = classify(&th, &quiet(s + 20 * S), s);
        assert_eq!((v.class, v.source_gap), (HealthClass::Idle, 20 * S));
    }

    #[test]
    fn recovery_needs_several_real_frames_not_regens() {
        let th = Thresholds::default();
        let s = t0();
        let mut c = Classifier::new(th, s);
        let last = s + 10 * S;
        let (v, changed) = c.observe(&flowing(s, last, 100));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
        let stalled = flowing(s, last, 100)
            .at(last + 16 * S)
            .with(ActivityKind::Presents, last + 15 * S);
        let (v, changed) = c.observe(&stalled);
        assert_eq!(
            (v.class, changed),
            (HealthClass::Stalled(StallClass::Presentation), true)
        );
        // Same class again: no transition.
        assert!(!c.observe(&stalled.at(last + 17 * S)).1);
        // Frames come back. Seq unchanged = a stash republish / cursor regen: does not count.
        let back = last + 18 * S;
        let (v, _) = c.observe(&flowing(s, back, 100));
        assert_eq!(v.class, HealthClass::Recovering);
        for (i, seq) in [101u64, 102].iter().enumerate() {
            let (v, _) = c.observe(&flowing(s, back + (i as u32 + 1) * S / 10, *seq));
            assert_eq!(v.class, HealthClass::Recovering, "frame {seq}");
        }
        let (v, changed) = c.observe(&flowing(s, back + S, 103));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
    }

    #[test]
    fn relapse_during_recovery_restarts_the_count() {
        let th = Thresholds::default();
        let s = t0();
        let mut c = Classifier::new(th, s);
        let last = s + 10 * S;
        c.observe(&flowing(s, last, 100));
        c.observe(
            &flowing(s, last, 100)
                .at(last + 16 * S)
                .with(ActivityKind::Presents, last + 15 * S),
        );
        let back = last + 17 * S;
        c.observe(&flowing(s, back, 101));
        c.observe(&flowing(s, back + S / 10, 102));
        // Relapse: another full stall.
        let (v, _) = c.observe(
            &flowing(s, back + S / 10, 102)
                .at(back + 20 * S)
                .with(ActivityKind::Presents, back + 19 * S),
        );
        assert_eq!(v.class, HealthClass::Stalled(StallClass::Presentation));
        // Two frames after the relapse are still Recovering — the earlier two do not carry over;
        // the third clears it.
        let (v, _) = c.observe(&flowing(s, back + 21 * S, 103));
        assert_eq!(v.class, HealthClass::Recovering);
        let (v, _) = c.observe(&flowing(s, back + 21 * S + S / 10, 104));
        assert_eq!(
            v.class,
            HealthClass::Recovering,
            "two new frames since the relapse"
        );
        let (v, changed) = c.observe(&flowing(s, back + 21 * S + S / 5, 105));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
    }

    /// Non-vacuity (verification matrix): invert one expected outcome and watch it fail.
    #[test]
    fn weak_evidence_would_have_stalled_without_the_strength_gate() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = flowing(s, last, 100)
            .at(last + 30 * S)
            .with(ActivityKind::Input, last + 29 * S);
        assert_ne!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Presentation)
        );
        assert!(!ActivityKind::Input.is_strong());
        assert!(ActivityKind::Canary.is_strong());
    }
}
