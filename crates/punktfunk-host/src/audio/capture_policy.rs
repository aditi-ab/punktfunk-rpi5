//! Desktop-audio capture POLICY — the parts of [`wasapi_cap`](super::wasapi_cap) that are pure
//! decisions rather than WASAPI plumbing, split out for the same reason
//! [`wiring_plan`](super::wiring_plan) is: so they compile and their unit tests RUN on every
//! platform. Both of these encode field-report behaviour, and regressing either must fail CI on
//! Linux too, not only on a Windows box.
//!
//! * [`FightDamper`] — how hard to fight another program for the default playback device.
//! * [`CaptureStats`] — the audio plane's vitals, so a log can tell a quiet host from a broken
//!   endpoint from one we are damaging ourselves.

use std::time::{Duration, Instant};

/// Default-playback re-assertions inside [`FIGHT_WINDOW`] before we stop fighting.
pub(crate) const FIGHT_LIMIT: u32 = 4;
pub(crate) const FIGHT_WINDOW: Duration = Duration::from_secs(20);
/// How long to leave the default alone once another program has proven it will take it back.
pub(crate) const FIGHT_BACKOFF: Duration = Duration::from_secs(60);

/// Damping for the default-playback tug-of-war (WP2.4).
///
/// The 2026-08-03 field log recorded seven full re-assert cycles in sixteen seconds — something on
/// that box re-set the default playback to CABLE Input every ~4 s and we snapped it back every
/// time, each round a capture teardown plus a wiring pass with `IPolicyConfig` writes. Winning that
/// argument is not possible and every round was an audible dropout, so: re-assert a few times
/// (transient churn does settle), then concede for a minute and say so once.
///
/// Time is passed IN rather than read here, which keeps the policy pure and testable.
pub(crate) struct FightDamper {
    /// Re-assertions in the current window, and when the window opened.
    count: u32,
    window_started: Instant,
    /// Set while we are deliberately not fighting.
    paused_until: Option<Instant>,
    /// One warning per fight burst, and one per concession.
    warned_fighting: bool,
    warned_giving_up: bool,
    now: Instant,
}

impl FightDamper {
    pub(crate) fn new(now: Instant) -> FightDamper {
        FightDamper {
            count: 0,
            window_started: now,
            paused_until: None,
            warned_fighting: false,
            warned_giving_up: false,
            now,
        }
    }

    /// A dud default-device change was observed at `now`.
    pub(crate) fn observed_at(&mut self, now: Instant) {
        self.now = now;
        if now.duration_since(self.window_started) >= FIGHT_WINDOW {
            self.window_started = now;
            self.count = 0;
            self.warned_fighting = false;
        }
        if self.paused_until.is_some_and(|t| now >= t) {
            self.paused_until = None;
            self.warned_giving_up = false;
            self.count = 0;
            self.window_started = now;
        }
    }

    /// Should we put the default back? False while paused, or once this window's budget is spent.
    pub(crate) fn should_reassert(&mut self) -> bool {
        if self.paused_until.is_some() {
            return false;
        }
        if self.count >= FIGHT_LIMIT {
            self.paused_until = Some(self.now + FIGHT_BACKOFF);
            return false;
        }
        self.count += 1;
        true
    }

    /// Warn on the FIRST re-assert of a burst only (the rest are noise).
    pub(crate) fn warn_now(&mut self) -> bool {
        !std::mem::replace(&mut self.warned_fighting, true)
    }

    /// Warn once when we concede.
    pub(crate) fn warn_giving_up(&mut self) -> bool {
        self.paused_until.is_some() && !std::mem::replace(&mut self.warned_giving_up, true)
    }

    /// Currently conceding (test/diagnostic accessor).
    pub(crate) fn is_paused(&self) -> bool {
        self.paused_until.is_some()
    }
}

/// How often the capture loop reports its vitals (WP0.2).
pub(crate) const STATS_EVERY: Duration = Duration::from_secs(30);

/// Shortest callback-to-callback delta that can be called a gap, whatever the quantum. At the
/// 5 ms quantum we ask for, `2 × quantum` would be 10 ms anyway; this floor is what stops a
/// graph running an even smaller quantum (the 2026-08-15 field host negotiated 128 frames =
/// 2.7 ms) from scoring ordinary scheduling noise as a hole.
const GAP_FLOOR: Duration = Duration::from_millis(10);

/// One reporting window's worth of capture vitals.
///
/// The point is to make three states that used to look identical in a log tell themselves apart: a
/// genuinely quiet host (`peak` ~0, no drops), a working stream (`peak` > 0), and a stream we are
/// damaging ourselves (`dropped_chunks` > 0). The 2026-08-03 field log — 3,600 lines, filed over an
/// audio-quality complaint — could distinguish none of them, because the audio plane logged nothing
/// at all between "capturing" and the session ending.
#[derive(Default)]
pub(crate) struct CaptureStats {
    pub(crate) frames: u64,
    /// Interleaved SAMPLES seen — the RMS denominator. Deliberately separate from `frames`:
    /// dividing the sum of squares by the frame count instead inflates RMS by sqrt(channels),
    /// which made a sine report an RMS equal to its own peak.
    pub(crate) samples: u64,
    /// Loudest |sample| in the window — tells a silent endpoint from a working one.
    pub(crate) peak: f32,
    /// Sum of squares, for the window's RMS: a level far below peak means a badly attenuated
    /// endpoint (a parked device sitting at 20 % volume costs ~14 dB before Opus ever sees it).
    pub(crate) sumsq: f64,
    /// Chunks the encode thread was too slow to take. Silent data loss, previously uncounted:
    /// the encoder simply concatenates across the hole, so it is a click AND a permanent shift of
    /// everything after it.
    pub(crate) dropped_chunks: u64,
    /// Windows in which the callback simply did not run on time (WP-A2). `delivered_pct` proves
    /// audio is missing but structurally cannot say HOW: one 2 s hole and three hundred 8 ms
    /// hiccups produce the same percentage and want completely different answers (a device or
    /// graph fault vs. a scheduling fault). The 2026-08-15 field log sat at 84–97 % for 24
    /// minutes of loud gameplay with `dropped_chunks=0` and no way to tell those apart.
    pub(crate) gaps: u64,
    /// The largest of those, µs. Reported in ms; kept in µs so a sub-ms threshold is expressible.
    pub(crate) max_gap_us: u64,
    /// Callbacks that ran but carried nothing — no buffer to dequeue, no `datas`, no mapped
    /// memory. Every one of these used to `return` silently, so a stream that fired its callback
    /// on time and handed us nothing looked identical to a stream nobody was feeding.
    pub(crate) missed_dequeues: u64,
    /// Spans this window spent with the stream NOT in `Streaming`, and how long they totalled.
    ///
    /// `gaps` deliberately cannot see these (see [`Self::observe_callback`]) — a paused stream
    /// fires no callbacks at all, so there is no delta to score and the caller drops its cadence
    /// stamp on every transition. The cost of that correct decision was that the outage went
    /// somewhere else entirely: into `delivered_pct`, as an unattributed shortfall, because the
    /// reporting window is flushed from the callback and therefore STRETCHES by exactly the time
    /// we were not being scheduled.
    ///
    /// Measured on a live host on 2026-08-15: a 16.2 s pause produced
    /// `delivered_pct=63 gaps=0 max_gap_ms=0`. Every number was correct and the line still could
    /// not say what happened — the explanation existed only in the state DEBUG lines, which a
    /// field journal at INFO does not carry. These two fields are that explanation, at INFO,
    /// beside the percentage they explain.
    pub(crate) pauses: u64,
    pub(crate) paused_us: u64,
}

impl CaptureStats {
    pub(crate) fn observe(&mut self, samples: &[f32], channels: u32) {
        self.frames += (samples.len() / channels.max(1) as usize) as u64;
        self.samples += samples.len() as u64;
        for &s in samples {
            let a = s.abs();
            if a > self.peak {
                self.peak = a;
            }
            self.sumsq += (s as f64) * (s as f64);
        }
    }

    /// Score one callback arrival against the previous one.
    ///
    /// `since_last` is `None` for the first callback of a stream — and, deliberately, for the
    /// first after a state transition: the caller drops its stamp when the stream pauses, so a
    /// legitimately Paused span is not scored as one enormous hole.
    ///
    /// That leaves this counter about ONE thing — holes inside a stream that is running — and
    /// pushes the other kind onto [`Self::observe_pause`]. The split matters because the two want
    /// opposite answers: a run of sub-10 ms holes is a scheduling problem on the box, whereas a
    /// multi-second pause is our node not being in the graph at all. A single "gap" number that
    /// mixed them would be worse than either.
    ///
    /// `quantum` is the NEGOTIATED buffer duration, not the one we asked for: a graph handing us
    /// 21.3 ms buffers is not gapping when its callbacks are 21.3 ms apart — it is doing exactly
    /// what it negotiated, and the quantum warning above already said so.
    pub(crate) fn observe_callback(&mut self, since_last: Option<Duration>, quantum: Duration) {
        let Some(delta) = since_last else { return };
        if delta > (quantum * 2).max(GAP_FLOOR) {
            self.gaps += 1;
            // The MISSING audio, not the callback delta: one quantum of that delta is the buffer
            // we were legitimately handed. Reporting the delta would inflate every gap by the
            // quantum and — worse — mean something different from the Windows feed, which sizes
            // its holes from the device position and so reports missing audio by construction.
            self.max_gap_us = self
                .max_gap_us
                .max(delta.saturating_sub(quantum).as_micros() as u64);
        }
    }

    /// The window's worst gap in whole ms — the unit the log line and the field reports speak.
    pub(crate) fn max_gap_ms(&self) -> u64 {
        self.max_gap_us / 1_000
    }

    /// Record one span the stream spent away from `Streaming`.
    ///
    /// Called on the transition BACK, so the whole span lands in the window that is flushed after
    /// the resume — which is the same window whose `delivered_pct` the span diluted. Keeping the
    /// two together is the entire point: apart, neither is interpretable.
    pub(crate) fn observe_pause(&mut self, span: Duration) {
        self.pauses += 1;
        self.paused_us += span.as_micros() as u64;
    }

    /// Total time away from `Streaming` this window, in whole ms.
    pub(crate) fn paused_ms(&self) -> u64 {
        self.paused_us / 1_000
    }

    /// `(peak dBFS, rms dBFS, delivered %)` for this window. Silence reports -120 dB rather than
    /// -inf so the log line stays parseable.
    pub(crate) fn summary(&self, elapsed: Duration, sample_rate: u32) -> (f64, f64, f64) {
        let rms = (self.sumsq / (self.samples as f64).max(1.0)).sqrt();
        let db = |v: f64| if v > 0.0 { 20.0 * v.log10() } else { -120.0 };
        // Expected frames for the window — a shortfall means the endpoint is not delivering at
        // real time (a stalling virtual device), which a peak/RMS alone cannot show.
        let expected = elapsed.as_secs_f64() * sample_rate as f64;
        (
            db(self.peak as f64),
            db(rms),
            (self.frames as f64 / expected.max(1.0)) * 100.0,
        )
    }
}

/// A departure this far past its slot is a slip worth counting rather than ordinary jitter: one
/// whole protocol frame, so a frame that merely rounds late never scores.
const LATE_DEPARTURE: Duration = Duration::from_millis(FRAME_MS as u64);

/// One reporting window of AUDIO EGRESS vitals (WP-C).
///
/// Capture has been instrumented since WP-A2 and the send path has not, so a field log could show
/// audio arriving at the tap and say nothing whatsoever about how it left. That asymmetry is not
/// neutral: it made "the host paces audio badly" unfalsifiable, and an unfalsifiable suspect stays
/// on the list forever. Across five 2026-08-15 field logs the entire egress path emitted 14 lines,
/// all of them the same session-open banner.
///
/// The point of these counters is to be *boring*. If departures are clean while capture reports
/// holes, the pacing rework introduced in v0.25 is acquitted permanently and the search moves
/// upstream for good.
#[derive(Default)]
pub(crate) struct SendStats {
    pub(crate) sent: u64,
    /// Frames synthesized to cover a capture hole. Wire continuity and captured continuity are
    /// different claims and a log that conflates them cannot be used to judge either.
    pub(crate) infilled: u64,
    /// Departures that missed their paced slot by at least [`LATE_DEPARTURE`].
    pub(crate) late: u64,
    /// The worst such miss, µs — kept even when the count is zero, because "never late" and
    /// "never late by a whole frame" are different statements.
    pub(crate) max_late_us: u64,
    /// Widest gap between two consecutive departures, µs. The number a client-side starvation
    /// complaint is actually about: the wire going quiet, whatever the reason.
    pub(crate) max_spacing_us: u64,
    /// Times the schedule fell more than `PACE_REANCHOR` behind and was re-anchored instead of
    /// chased. Each one silently forgives accumulated debt, which is exactly the kind of event
    /// that leaves no trace and then gets blamed on the network.
    pub(crate) reanchors: u64,
}

impl SendStats {
    /// Score one frame leaving the host. `late` is how far past its paced slot it went (zero when
    /// the schedule is unanchored), `since_prev` the spacing from the previous departure.
    pub(crate) fn observe_departure(
        &mut self,
        late: Duration,
        since_prev: Option<Duration>,
        infilled: bool,
    ) {
        self.sent += 1;
        if infilled {
            self.infilled += 1;
        }
        self.max_late_us = self.max_late_us.max(late.as_micros() as u64);
        if late >= LATE_DEPARTURE {
            self.late += 1;
        }
        if let Some(gap) = since_prev {
            self.max_spacing_us = self.max_spacing_us.max(gap.as_micros() as u64);
        }
    }

    pub(crate) fn observe_reanchor(&mut self) {
        self.reanchors += 1;
    }

    pub(crate) fn max_late_ms(&self) -> u64 {
        self.max_late_us / 1_000
    }

    pub(crate) fn max_spacing_ms(&self) -> u64 {
        self.max_spacing_us / 1_000
    }
}

/// How long a capture hole may run before the wire starts covering it. Two protocol frames: long
/// enough that ordinary quantum jitter never trips it, short enough that the client's ring never
/// notices the hole.
pub(crate) const INFILL_AFTER: Duration = Duration::from_millis(2 * FRAME_MS as u64);
/// How much silence one hole may be covered with. Past this the host is not glitching, it is
/// QUIET — a desktop between games is legitimately silent for hours and paying a few kbps to keep
/// saying so is absurd — so the wire stops, which is exactly the behaviour that shipped before.
pub(crate) const INFILL_MAX: Duration = Duration::from_millis(500);

/// One protocol audio frame — the wire's unit, and the granularity infill works in.
const FRAME_MS: u32 = punktfunk_core::audio::FRAME_MS;

/// What the wire owes for the slot that is due now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Infill {
    /// Nothing: audio is flowing, or the hole is still too young to be worth covering.
    Wait,
    /// One frame of silence, on the pacer's schedule and continuous with what came before.
    Silence,
    /// The budget is spent. Say nothing — and the next real chunk begins a NEW continuity.
    Quiet,
}

/// Whether, and for how long, the wire covers a capture hole with silence (WP-B1).
///
/// A hole used to cost far more than the audio it swallowed. `audio_thread` blocked in
/// `next_chunk` for its whole duration, so nothing at all left the host: the client's de-jitter
/// ring drained, underran, de-primed, and then had to re-prime — turning a 30 ms hole into a much
/// longer audible artifact, and doing it 3–16 % of the time on the 2026-08-15 field host. Silence
/// on the same 5 ms schedule, with continuous `seq` and pts, keeps that ring fed and its playout
/// anchored, so what the listener loses shrinks to exactly the audio that was genuinely missing.
///
/// Time is passed IN, so the policy is pure and its tests run on every platform.
#[derive(Default)]
pub(crate) struct InfillPolicy {
    /// Silence already sent for the open hole. Denominated in TIME rather than in frames or
    /// callbacks, which is the recorded lesson from the client's de-prime fuse: a count there made
    /// an iPad give up three times sooner than a Mac for no reason anyone intended.
    filled_ms: u32,
    /// Latched once a hole outlives the budget and the wire falls silent.
    broke: bool,
}

impl InfillPolicy {
    /// Decide the slot that is due now. Call EXACTLY once per due frame — it consumes budget.
    pub(crate) fn decide(&mut self, since_last_chunk: Duration) -> Infill {
        if since_last_chunk < INFILL_AFTER {
            return Infill::Wait;
        }
        if self.filled_ms as u64 >= INFILL_MAX.as_millis() as u64 {
            self.broke = true;
            return Infill::Quiet;
        }
        self.filled_ms += FRAME_MS;
        Infill::Silence
    }

    /// True once the budget is spent, so the caller can go back to blocking for real audio
    /// instead of waking every few milliseconds to decide to stay quiet.
    pub(crate) fn exhausted(&self) -> bool {
        self.filled_ms as u64 >= INFILL_MAX.as_millis() as u64
    }

    /// A real chunk arrived. Returns whether the hole it closed BROKE continuity — the wire went
    /// silent across it, so the redundancy predecessor and any partial frame straddling the hole
    /// both describe audio from before a discontinuity, and neither may be spliced onto what
    /// comes next.
    pub(crate) fn chunk_arrived(&mut self) -> bool {
        self.filled_ms = 0;
        std::mem::take(&mut self.broke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replays the 2026-08-03 field shape: a dud default change every ~2 s, forever. We must put
    /// the default back a few times, then concede — and warn exactly once for each.
    #[test]
    fn fight_damper_concedes_instead_of_looping_forever() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        let (mut warns_fighting, mut warns_giving_up) = (0, 0);
        for i in 0..8 {
            d.observed_at(t0 + Duration::from_millis(i * 2_000));
            if d.should_reassert() {
                reasserts += 1;
                if d.warn_now() {
                    warns_fighting += 1;
                }
            } else if d.warn_giving_up() {
                warns_giving_up += 1;
            }
        }
        assert_eq!(
            reasserts, FIGHT_LIMIT,
            "must stop after the window's budget"
        );
        assert_eq!(warns_fighting, 1, "one warning per burst, not one per flip");
        assert_eq!(warns_giving_up, 1, "concede exactly once");
    }

    /// Occasional, genuinely transient churn must ALWAYS be corrected — the damper must not
    /// accumulate across widely-spaced events and quietly stop doing its job.
    #[test]
    fn fight_damper_always_fixes_isolated_changes() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        let mut reasserts = 0;
        for i in 1..=10 {
            d.observed_at(t0 + FIGHT_WINDOW * i);
            if d.should_reassert() {
                reasserts += 1;
            }
        }
        assert_eq!(reasserts, 10, "isolated changes must always be corrected");
    }

    /// After the backoff expires the damper re-arms, so a program that goes quiet and comes back
    /// later is fought again rather than being conceded to for the rest of the session.
    #[test]
    fn fight_damper_rearms_after_the_backoff() {
        let t0 = Instant::now();
        let mut d = FightDamper::new(t0);
        for i in 0..FIGHT_LIMIT + 2 {
            d.observed_at(t0 + Duration::from_millis(i as u64 * 500));
            d.should_reassert();
        }
        assert!(d.is_paused(), "should have conceded");
        d.observed_at(t0 + FIGHT_BACKOFF + FIGHT_WINDOW * 2);
        assert!(d.should_reassert(), "must re-arm once the backoff expires");
    }

    /// Peak/RMS must separate the states a log could not previously tell apart.
    #[test]
    fn capture_stats_separate_silence_from_signal() {
        let mut quiet = CaptureStats::default();
        quiet.observe(&[0.0; 480], 2);
        let (peak, rms, _) = quiet.summary(Duration::from_secs(1), 48_000);
        assert_eq!(peak, -120.0, "digital silence reports the floor, not -inf");
        assert_eq!(rms, -120.0);

        let mut loud = CaptureStats::default();
        let tone: Vec<f32> = (0..480).map(|i| (i as f32 * 0.13).sin() * 0.5).collect();
        loud.observe(&tone, 2);
        assert_eq!(
            loud.frames, 240,
            "480 interleaved stereo samples = 240 frames"
        );
        let (peak, rms, _) = loud.summary(Duration::from_secs(1), 48_000);
        assert!(
            peak > -8.0 && peak <= 0.0,
            "peak {peak} dBFS should track a 0.5 tone"
        );
        // A sine's RMS is its amplitude / sqrt(2) — about 3 dB below peak. Getting this equal to
        // peak is exactly what a frames-vs-samples mix-up in the denominator looks like, so the
        // margin is asserted rather than just the ordering.
        assert!(
            rms < peak - 2.0,
            "RMS {rms} vs peak {peak}: a sine must sit ~3 dB below its peak"
        );
    }

    /// The delivered-percentage is what shows an endpoint that has stopped feeding us in real
    /// time — invisible in peak/RMS, and the shape a stalling virtual device makes.
    #[test]
    fn capture_stats_report_a_delivery_shortfall() {
        let mut full = CaptureStats::default();
        full.observe(&vec![0.1f32; 48_000 * 2], 2); // exactly 1 s of stereo
        let (_, _, pct) = full.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 100.0).abs() < 1.0, "expected ~100 %, got {pct}");

        let mut half = CaptureStats::default();
        half.observe(&vec![0.1f32; 48_000], 2); // 0.5 s of stereo in a 1 s window
        let (_, _, pct) = half.summary(Duration::from_secs(1), 48_000);
        assert!((pct - 50.0).abs() < 1.0, "expected ~50 %, got {pct}");
    }

    /// The 5 ms quantum we ask for — the shape all three gap tests are measured against.
    const Q: Duration = Duration::from_millis(5);

    /// The product gap the 2026-08-15 field log left open, closed: `delivered_pct` alone reports
    /// the SAME 93 % for one 2 s hole and for three hundred 8 ms hiccups, and those are different
    /// faults with different fixes. The counters have to separate them without a second log.
    #[test]
    fn gap_accounting_tells_one_long_hole_from_many_short_ones() {
        let mut one = CaptureStats::default();
        one.observe_callback(None, Q); // first callback of the stream — nothing to compare to
        one.observe_callback(Some(Duration::from_secs(2)), Q);
        assert_eq!(one.gaps, 1);
        // Two seconds between callbacks, one quantum of which was audio we were handed.
        assert_eq!(one.max_gap_ms(), 2_000 - Q.as_millis() as u64);

        // …versus three hundred 8 ms holes, each arriving as a 13 ms callback delta.
        let mut many = CaptureStats::default();
        for _ in 0..300 {
            many.observe_callback(Some(Q + Duration::from_millis(8)), Q);
        }
        assert_eq!(many.gaps, 300);
        assert_eq!(many.max_gap_ms(), 8);

        // Both shapes lose comparable audio; only the counters tell them apart.
        assert!(
            one.max_gap_ms() > many.max_gap_ms() * 100,
            "the discriminator is the SHAPE, not the total"
        );
    }

    /// A stream delivering exactly what it negotiated is never a gap — including the clamped
    /// 21.3 ms quantum a VM's `default.clock.min-quantum` forces, which would otherwise score a
    /// gap on every single callback and bury the real ones.
    #[test]
    fn a_negotiated_cadence_is_never_a_gap() {
        let mut s = CaptureStats::default();
        for _ in 0..100 {
            s.observe_callback(Some(Duration::from_micros(5_100)), Q); // 5 ms + jitter
        }
        assert_eq!(s.gaps, 0, "ordinary jitter at the negotiated quantum");

        let clamped = Duration::from_micros(21_333); // 1024 frames @ 48 kHz
        let mut vm = CaptureStats::default();
        for _ in 0..100 {
            vm.observe_callback(Some(clamped), clamped);
        }
        assert_eq!(vm.gaps, 0, "a clamped quantum is slow, not gapping");
        // …and a real hole on that same graph still scores.
        vm.observe_callback(Some(Duration::from_millis(200)), clamped);
        assert_eq!(vm.gaps, 1);
    }

    /// Drive one hole from the moment it opens until the policy gives up on it, the way the
    /// encode loop does: one decision per due frame slot.
    fn cover_a_hole(p: &mut InfillPolicy) -> usize {
        let mut silence = 0usize;
        let mut open = INFILL_AFTER;
        loop {
            match p.decide(open) {
                Infill::Silence => {
                    silence += 1;
                    open += Duration::from_millis(FRAME_MS as u64);
                }
                Infill::Quiet => return silence,
                Infill::Wait => unreachable!("the hole is open — {open:?} is past INFILL_AFTER"),
            }
            assert!(silence < 10_000, "the budget must be finite");
        }
    }

    /// The wire covers a hole for exactly as long as the budget allows, and then admits the host
    /// is simply quiet. Both halves matter: without the first, a 30 ms hole costs the client a
    /// de-prime and a re-prime; without the second, an idle desktop pays for silence datagrams
    /// forever.
    #[test]
    fn infill_covers_a_hole_and_then_admits_the_host_is_quiet() {
        let mut p = InfillPolicy::default();
        // Ordinary quantum jitter, not a hole — nothing owed.
        assert_eq!(p.decide(Duration::ZERO), Infill::Wait);
        assert_eq!(
            p.decide(INFILL_AFTER - Duration::from_millis(1)),
            Infill::Wait
        );

        let silence = cover_a_hole(&mut p);
        assert_eq!(
            silence as u64 * FRAME_MS as u64,
            INFILL_MAX.as_millis() as u64,
            "the wire must cover exactly the budget, in frames of {FRAME_MS} ms"
        );
        assert!(p.exhausted(), "…and then stop asking");
    }

    /// A stream that is flowing must never synthesize anything — this policy is invisible until
    /// something is actually wrong.
    #[test]
    fn a_flowing_stream_never_infills() {
        let mut p = InfillPolicy::default();
        for _ in 0..1_000 {
            assert_eq!(
                p.decide(Duration::from_millis(FRAME_MS as u64)),
                Infill::Wait
            );
        }
        assert!(!p.exhausted());
        assert!(!p.chunk_arrived(), "no hole means no discontinuity");
    }

    /// Only a hole the wire could NOT cover breaks continuity. The distinction is the whole point
    /// of the budget: across a covered hole `seq` and pts never broke, so the redundancy
    /// predecessor still describes the frame before this one and the client can keep using it.
    #[test]
    fn only_an_uncovered_hole_breaks_continuity() {
        let mut covered = InfillPolicy::default();
        for k in 0..20u64 {
            covered.decide(INFILL_AFTER + Duration::from_millis(FRAME_MS as u64 * k));
        }
        assert!(
            !covered.chunk_arrived(),
            "a covered hole is continuous — the client heard silence, not a splice"
        );

        let mut lost = InfillPolicy::default();
        cover_a_hole(&mut lost);
        assert!(
            lost.chunk_arrived(),
            "past the budget the wire went quiet, so nothing before the hole may be spliced on"
        );
        // …and the next hole starts from a clean budget rather than an exhausted one.
        assert!(!lost.exhausted());
        assert_eq!(cover_a_hole(&mut lost) as u64 * FRAME_MS as u64, 500);
    }

    /// A deliberate pause is not a hole. The caller drops its stamp across a state transition, so
    /// the span reaches us as `None` — without that rule every Paused↔Streaming flap (three of
    /// them in minute 1 of the field log, around each format renegotiation) would report a gap
    /// the size of the pause and drown the sub-10 ms ones that actually matter.
    #[test]
    fn a_paused_span_is_not_scored() {
        let mut s = CaptureStats::default();
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        s.observe_callback(None, Q); // resumed: the pause spanned an unknowable amount of time
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        assert_eq!(s.gaps, 0);
        assert_eq!(s.max_gap_ms(), 0);
    }

    /// The companion to the test above, and the reason it is safe: a pause stays out of `gaps`,
    /// but it does NOT stay out of the log line. Numbers are the ones measured on a live host on
    /// 2026-08-15, where a 16.2 s pause reported `delivered_pct=63 gaps=0 max_gap_ms=0` and no
    /// field in the line could say why.
    #[test]
    fn a_paused_span_is_reported_even_though_it_is_not_a_gap() {
        let mut s = CaptureStats::default();
        s.observe_callback(Some(Duration::from_millis(5)), Q);
        s.observe_pause(Duration::from_millis(16_214));
        s.observe_callback(None, Q); // resumed
        s.observe_callback(Some(Duration::from_millis(5)), Q);

        assert_eq!(s.gaps, 0, "a pause is still not a delivery gap");
        assert_eq!(s.max_gap_ms(), 0);
        assert_eq!(s.pauses, 1, "…but it is now countable");
        assert_eq!(s.paused_ms(), 16_214);
    }

    /// One long outage and a burst of short flaps must not read alike — the same argument that
    /// makes `gaps` and `max_gap_ms` two fields instead of one. The triple here is the shape every
    /// Skynet and AVALON session start produced: three dwells, no format actually changing.
    #[test]
    fn pause_spans_accumulate_and_stay_countable() {
        let mut long = CaptureStats::default();
        long.observe_pause(Duration::from_millis(38_400));

        let mut flappy = CaptureStats::default();
        for ms in [12_534, 17_030, 8_765] {
            flappy.observe_pause(Duration::from_millis(ms));
        }

        assert_eq!(long.pauses, 1);
        assert_eq!(flappy.pauses, 3);
        assert_eq!(flappy.paused_ms(), 38_329);
        assert!(
            long.paused_ms().abs_diff(flappy.paused_ms()) < 100,
            "near-identical dead time, and the count is the only thing that separates them"
        );
    }

    /// The discriminator the field logs needed. A stream that is running and starved reports gaps
    /// and NO pause; a stream that was never scheduled reports the mirror image. Both dilute
    /// `delivered_pct` identically, which is exactly why neither can be diagnosed from it alone.
    #[test]
    fn starvation_and_absence_are_told_apart() {
        let mut starved = CaptureStats::default();
        for _ in 0..60 {
            starved.observe_callback(Some(Duration::from_millis(30)), Q);
        }

        let mut absent = CaptureStats::default();
        absent.observe_pause(Duration::from_millis(1_800));

        assert_eq!(starved.gaps, 60);
        assert_eq!(starved.pauses, 0, "a running stream was never absent");
        assert_eq!(absent.gaps, 0);
        assert_eq!(absent.pauses, 1, "an absent stream never got to be slow");
        assert_eq!(absent.paused_ms(), 1_800);
    }

    /// The acquittal case, and the whole reason [`SendStats`] exists: a pacer doing its job must
    /// produce a line a reader can dismiss at a glance.
    #[test]
    fn a_healthy_pacer_reports_nothing_alarming() {
        let mut s = SendStats::default();
        let frame = Duration::from_millis(FRAME_MS as u64);
        for i in 0..200 {
            s.observe_departure(Duration::ZERO, (i > 0).then_some(frame), false);
        }
        assert_eq!(s.sent, 200);
        assert_eq!(s.late, 0);
        assert_eq!(s.reanchors, 0);
        assert_eq!(s.infilled, 0);
        assert_eq!(s.max_late_ms(), 0);
        assert_eq!(s.max_spacing_ms(), FRAME_MS as u64);
    }

    /// Lateness under one frame is jitter, not a slip — but it must still be *visible*, or
    /// "never late" and "never late by a whole frame" become the same report.
    #[test]
    fn sub_frame_lateness_is_measured_without_being_counted() {
        let mut s = SendStats::default();
        s.observe_departure(Duration::from_micros(3_400), None, false);
        assert_eq!(s.late, 0, "3.4 ms has not slipped a whole 5 ms slot");
        assert_eq!(s.max_late_ms(), 3, "…and it is still on the record");
    }

    /// A slot missed by a whole frame or more is the event the field logs could never show.
    #[test]
    fn a_slipped_slot_is_counted_and_its_worst_case_kept() {
        let mut s = SendStats::default();
        s.observe_departure(Duration::from_millis(6), None, false);
        s.observe_departure(
            Duration::from_millis(41),
            Some(Duration::from_millis(47)),
            false,
        );
        s.observe_departure(Duration::ZERO, Some(Duration::from_millis(5)), false);
        s.observe_reanchor();

        assert_eq!(s.late, 2);
        assert_eq!(s.max_late_ms(), 41);
        assert_eq!(s.max_spacing_ms(), 47, "the wire's worst quiet stretch");
        assert_eq!(s.reanchors, 1);
    }

    /// Wire continuity is not captured continuity. A window whose frames were all synthesized
    /// looks perfect on every other counter, and must not be readable as healthy audio.
    #[test]
    fn synthesized_frames_stay_distinguishable_from_captured_ones() {
        let mut s = SendStats::default();
        let frame = Duration::from_millis(FRAME_MS as u64);
        for _ in 0..100 {
            s.observe_departure(Duration::ZERO, Some(frame), true);
        }
        assert_eq!(s.sent, 100);
        assert_eq!(
            s.infilled, 100,
            "every one of these was silence we invented"
        );
        assert_eq!(s.late, 0);
    }
}
