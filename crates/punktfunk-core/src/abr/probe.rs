//! Measuring the link: a bring-up ramp before the first frame, and the
//! legacy in-session burst.
//!
//! The ramp is a sequence of short `ProbeRequest`s, doubling in rate, issued
//! while the host's pipeline is still building — no video exists yet, so
//! nothing it does can cost a frame (law L4). It stops itself at the first
//! step the link does not deliver, at what the stream can use, at a sender
//! that cannot offer the rate, or at the first video frame.
//!
//! The legacy burst runs against a host without [`HOST_CAP2_RAMP`]: one
//! 800 ms burst beside live video, two seconds in. It damages the window it
//! lands in, so that window is discarded; if it took the keyframe with it the
//! session asks for a new one. Every deadline here exists because a host may
//! simply not answer: an unanswered burst that latched `active` would
//! suppress the report tick for the rest of the session.
//!
//! [`HOST_CAP2_RAMP`]: crate::quic::HOST_CAP2_RAMP

use std::time::{Duration, Instant};

/// Burst length. Long enough to fill the bottleneck queue and drain it,
/// short enough that the picture survives it on most links.
const PROBE_MS: u32 = 800;
/// Wait after video flows before bursting. The first frames are the encoder's
/// IDR and the decoder's bring-up; a burst on top of them measures neither.
const PROBE_DELAY: Duration = Duration::from_secs(2);
/// Queue and QUIC loss recovery sit between the host's "complete" and our
/// receipt. A result later than this is not about the burst.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One ramp step. A 5 Mbps step is a dozen packets at this length, and the
/// whole ramp fits inside one bring-up.
const RAMP_STEP_MS: u32 = 25;
/// The ramp's first rate. Under the controller's floor there is nothing worth
/// measuring.
const RAMP_START_KBPS: u32 = 5_000;
/// A step is capped in bytes as well as in time: 25 ms at 2 Gbps is 6 MB, and
/// a step the application cannot drain measures the receive buffer.
const RAMP_STEP_BYTES: u64 = 3_000_000;
/// Delivered ÷ offered under this is a wall.
const RAMP_WALL_PCT: u64 = 90;
/// What a wall licenses: the ceiling sits under what the link delivered.
const RAMP_CEILING_PCT: u32 = 85;
/// Arrivals quiet this long mean the step has drained out of the receive
/// buffer and the next step measures its own bytes, not the last one's.
const RAMP_DRAIN_MS: u64 = 20;
/// A step whose result never comes. One RTT plus the step; a host that is
/// slower than this is not going to finish the ramp either.
const RAMP_STEP_TIMEOUT: Duration = Duration::from_millis(1_500);

/// What the burst or step delivered, as the pump's probe state holds it.
///
/// Two domains, deliberately: `delivered_*` and `wire_packets_sent` are wire
/// packets (header plus shard, parity included), `host_bytes_sent` is the
/// payload the host offered. Only compare like with like.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeReport {
    /// Wire bytes the burst delivered. `0` = declined.
    pub delivered_bytes: u64,
    /// Wire packets behind those bytes.
    pub delivered_packets: u64,
    /// Throughput denominator: the client receive interval when the burst
    /// produced one, else the host's send window.
    pub window_ms: u32,
    /// Host send-window duration. `0` = the host declined the burst.
    pub host_duration_ms: u32,
    /// The measured client interval, the ramp's own denominator. `0` = none.
    pub client_interval_ms: u32,
    /// Payload bytes the host put on the wire, against what was asked.
    pub host_bytes_sent: u64,
    /// Wire packets the host's kernel accepted.
    pub wire_packets_sent: u32,
    /// Wire packets the send buffer refused: the sender was the limit.
    pub send_dropped: u32,
}

/// What a burst's report was worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measured {
    /// Not the burst this client asked for — an embedder speed test, or a
    /// report already consumed. Nothing is learned, nothing is rebased.
    NotOurs,
    /// The host declined the burst: the negotiated ceiling stands.
    Declined,
    /// Link capacity, headroom already taken off.
    Ceiling(u32),
}

/// What the ramp proved by the time it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ramped {
    /// The link refused a step: it holds about `delivered_kbps` and no more.
    Wall { delivered_kbps: u32 },
    /// Nothing refused up to `proven_kbps`, which is a floor under the
    /// capacity and says nothing about a limit. `0` = nothing measured.
    NoWall { proven_kbps: u32 },
}

impl Ramped {
    /// The rate the ramp proved the link carries, whichever way it ended.
    fn proven_kbps(self) -> u32 {
        match self {
            Ramped::Wall { delivered_kbps } => delivered_kbps,
            Ramped::NoWall { proven_kbps } => proven_kbps,
        }
    }
}

/// What the ramp came to, for a test that pins its arithmetic.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RampSummary {
    pub wall: bool,
    pub proven_kbps: u32,
    pub steps: u32,
    /// Payload bytes asked for over every step.
    pub asked_bytes: u64,
}

/// One step in flight.
struct Step {
    target_kbps: u32,
    /// Payload bytes the host was asked for: `target_kbps` over the step.
    asked_bytes: u64,
    /// Delivered bytes the last report showed, and when they last grew.
    /// Arrivals going quiet, after some arrived, is the client's own drain.
    seen_bytes: u64,
    seen_at: Instant,
    last: Option<ProbeReport>,
    deadline: Instant,
}

/// The bring-up ramp: short steps at doubling rates until one of the stop
/// rules fires.
struct Ramp {
    /// The ramp never asks above this: what the stream can use, or
    /// `PUNKTFUNK_ABR_PROBE_KBPS`.
    max_kbps: u32,
    /// Rate the next step asks for.
    next_kbps: u32,
    step: Option<Step>,
    /// Highest rate the link delivered whole.
    proven_kbps: u32,
    /// Payload bytes asked for over the whole ramp, and steps issued.
    spent_bytes: u64,
    steps: u32,
    started: Instant,
    /// The ramp has stopped, and whether it stopped at a wall. Kept apart
    /// from `outcome`, which the driver takes: a consumed verdict must not
    /// look like an unfinished ramp.
    done: bool,
    wall: bool,
    outcome: Option<Ramped>,
}

/// Step length for a rate: the step's own duration, shortened when the byte
/// cap binds first.
fn step_ms(target_kbps: u32) -> u32 {
    let by_bytes = (RAMP_STEP_BYTES * 8 / u64::from(target_kbps.max(1))) as u32;
    by_bytes.clamp(1, RAMP_STEP_MS)
}

impl Ramp {
    fn new(max_kbps: u32, now: Instant) -> Self {
        Ramp {
            max_kbps: max_kbps.max(RAMP_START_KBPS),
            next_kbps: RAMP_START_KBPS,
            step: None,
            proven_kbps: 0,
            spent_bytes: 0,
            steps: 0,
            started: now,
            done: false,
            wall: false,
            outcome: None,
        }
    }

    /// Stop here. The first stop wins: a later one would overwrite evidence
    /// with the absence of it.
    fn stop(&mut self, outcome: Ramped) {
        self.step = None;
        if !self.done {
            self.done = true;
            self.wall = matches!(outcome, Ramped::Wall { .. });
            self.proven_kbps = outcome.proven_kbps();
            tracing::info!(
                proven_kbps = outcome.proven_kbps(),
                wall = matches!(outcome, Ramped::Wall { .. }),
                steps = self.steps,
                asked_kb = self.spent_bytes / 1_000,
                took_ms = self.started.elapsed().as_millis() as u64,
                "adaptive bitrate: bring-up ramp done"
            );
            self.outcome = Some(outcome);
        }
    }

    fn no_wall(&mut self) {
        self.stop(Ramped::NoWall {
            proven_kbps: self.proven_kbps,
        });
    }

    /// Judge a settled step. `None` = it proved the rate and the ramp goes on.
    fn judge(&self, step: &Step, r: &ProbeReport) -> Option<Ramped> {
        let interval = u64::from(r.client_interval_ms);
        // Under two packets there is no interval, so there is no rate either.
        if interval == 0 || r.delivered_packets < 2 || r.wire_packets_sent == 0 {
            return Some(Ramped::NoWall {
                proven_kbps: self.proven_kbps,
            });
        }
        let delivered_kbps = (r.delivered_bytes.saturating_mul(8) / interval) as u32;
        // The SENDER could not offer the rate: what it managed is a floor
        // under the link, never a wall (the link was never asked).
        if r.send_dropped > 0 || r.host_bytes_sent * 100 < step.asked_bytes * 90 {
            tracing::info!(
                target_kbps = step.target_kbps,
                delivered_kbps,
                send_dropped = r.send_dropped,
                host_bytes_sent = r.host_bytes_sent,
                asked_bytes = step.asked_bytes,
                "adaptive bitrate: ramp step limited by the sender, not the link"
            );
            return Some(Ramped::NoWall {
                proven_kbps: self.proven_kbps.max(delivered_kbps),
            });
        }
        // Delivered ÷ offered as packets a millisecond on each side: the host
        // sent `wire_packets_sent` over its window, we received
        // `delivered_packets` over ours. A queue that stretches the arrivals
        // and loss that thins them both land here.
        let delivered = r.delivered_packets * u64::from(r.host_duration_ms);
        let offered = u64::from(r.wire_packets_sent) * interval;
        if delivered * 100 < offered * RAMP_WALL_PCT {
            tracing::info!(
                target_kbps = step.target_kbps,
                delivered_kbps,
                client_interval_ms = r.client_interval_ms,
                host_duration_ms = r.host_duration_ms,
                "adaptive bitrate: ramp found the link's wall"
            );
            return Some(Ramped::Wall { delivered_kbps });
        }
        None
    }

    /// Fold a report into the step in flight. Reports repeat: the pump
    /// re-presents the probe state every iteration, and the bytes keep
    /// growing while the receive buffer drains.
    fn on_report(&mut self, r: ProbeReport, now: Instant) {
        let Some(step) = self.step.as_mut() else {
            return;
        };
        if r.delivered_bytes > step.seen_bytes {
            step.seen_bytes = r.delivered_bytes;
            step.seen_at = now;
        }
        step.last = Some(r);
    }

    /// A step is over once its bytes stopped arriving. Returns the next step
    /// to ask for.
    fn settle(&mut self, now: Instant) -> Option<(u32, u32)> {
        let step = self.step.as_ref()?;
        // Bytes have to have ARRIVED before their absence means drained: a
        // step queued behind 450 ms of someone else's traffic is late, not
        // empty, and judging it empty reads a busy link as no link at all.
        let drained = step.seen_bytes > 0
            && now.duration_since(step.seen_at).as_millis() as u64 >= RAMP_DRAIN_MS;
        if !drained {
            return None;
        }
        let step = self.step.take().expect("present on this branch");
        let Some(report) = step.last else {
            self.no_wall();
            return None;
        };
        match self.judge(&step, &report) {
            Some(end) => {
                self.stop(end);
                None
            }
            None => {
                let interval = u64::from(report.client_interval_ms.max(1));
                self.proven_kbps = (report.delivered_bytes.saturating_mul(8) / interval) as u32;
                if step.target_kbps >= self.max_kbps {
                    // The ramp asked for everything this stream can use and
                    // got it. Capacity above that is not this session's
                    // business.
                    self.no_wall();
                    return None;
                }
                self.next_kbps = step.target_kbps.saturating_mul(2).min(self.max_kbps);
                Some(self.begin(now))
            }
        }
    }

    /// Arm the next step and say what to ask the host for.
    fn begin(&mut self, now: Instant) -> (u32, u32) {
        let target_kbps = self.next_kbps.min(self.max_kbps);
        let duration_ms = step_ms(target_kbps);
        let asked_bytes = u64::from(target_kbps) * u64::from(duration_ms) / 8;
        self.spent_bytes += asked_bytes;
        self.steps += 1;
        self.step = Some(Step {
            target_kbps,
            asked_bytes,
            seen_bytes: 0,
            seen_at: now,
            last: None,
            deadline: now + RAMP_STEP_TIMEOUT,
        });
        (target_kbps, duration_ms)
    }
}

/// The measurement's whole life: the ramp before the first frame, or the
/// legacy burst armed, in flight, answered or abandoned.
pub(crate) struct CapacityProbe {
    /// Burst target. `PUNKTFUNK_ABR_PROBE_KBPS`, or twice the stream cap.
    target_kbps: u32,
    /// The bring-up ramp, against a host that serves it.
    ramp: Option<Ramp>,
    /// When to fire the legacy burst. `None` = fired already, or never armed.
    fire_at: Option<Instant>,
    /// Our own burst is out and its result is still owed.
    result_by: Option<Instant>,
    /// Any burst is in flight, ours or an embedder speed test.
    active: bool,
    /// An in-flight burst that outlives this is unanswered: let it go, or the
    /// report tick stays suppressed forever.
    watchdog: Option<Instant>,
    /// `frames_completed` when the burst started: "did any frame survive",
    /// not "has one ever arrived".
    frames_at_start: u64,
}

impl CapacityProbe {
    /// `target_kbps` of `None` sizes the burst from the stream cap, and caps
    /// the ramp at what the stream can use. `armed` is `PUNKTFUNK_ABR_PROBE`
    /// plus the session being Automatic at all; `ramp` is the host's
    /// [`HOST_CAP2_RAMP`](crate::quic::HOST_CAP2_RAMP).
    pub(crate) fn new(
        armed: bool,
        ramp: bool,
        target_kbps: Option<u32>,
        stream_cap_kbps: u32,
        now: Instant,
    ) -> Self {
        CapacityProbe {
            target_kbps: target_kbps.unwrap_or_else(|| probe_target_kbps(stream_cap_kbps)),
            ramp: (armed && ramp)
                .then(|| Ramp::new(ramp_max_kbps(stream_cap_kbps, target_kbps), now)),
            // The ramp replaces the burst; it never runs beside video.
            fire_at: (armed && !ramp).then(|| now + PROBE_DELAY),
            result_by: None,
            active: false,
            watchdog: None,
            frames_at_start: 0,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// A burst went in or out of flight. `Some(frames_at_start)` on the
    /// trailing edge: the caller compares it against the frames completed to
    /// see whether the burst took every picture with it.
    pub(crate) fn on_active(
        &mut self,
        active: bool,
        duration_ms: u32,
        frames_completed: u64,
        now: Instant,
    ) -> Option<u64> {
        let ended = self.active && !active;
        if !self.active && active {
            let burst = Duration::from_millis(u64::from(duration_ms));
            self.watchdog = Some(now + burst + PROBE_TIMEOUT);
            self.frames_at_start = frames_completed;
        }
        if !active {
            self.watchdog = None;
        }
        self.active = active;
        ended.then_some(self.frames_at_start)
    }

    /// Fire what is due: the next ramp step, or the legacy burst once video
    /// is actually flowing. A slow host bring-up is still emitting its first
    /// IDR, so the burst waits another delay.
    pub(crate) fn poll(&mut self, now: Instant, frames_completed: u64) -> Option<(u32, u32)> {
        if self.ramp.is_some() {
            return self.poll_ramp(now, frames_completed);
        }
        let due = self.fire_at.is_some_and(|at| now >= at);
        if !due {
            return None;
        }
        if self.active || frames_completed == 0 {
            self.fire_at = Some(now + PROBE_DELAY);
            return None;
        }
        self.fire_at = None;
        self.result_by = Some(now + PROBE_TIMEOUT);
        tracing::info!(
            target_kbps = self.target_kbps,
            duration_ms = PROBE_MS,
            "adaptive bitrate: startup link-capacity probe"
        );
        Some((self.target_kbps, PROBE_MS))
    }

    /// One ramp step per call: the first, or the next once the last drained.
    /// The first video frame ends the ramp wherever it stands — from then on
    /// nothing the controller does may cost a frame.
    fn poll_ramp(&mut self, now: Instant, frames_completed: u64) -> Option<(u32, u32)> {
        let r = self.ramp.as_mut()?;
        if r.done {
            return None;
        }
        if frames_completed > 0 {
            r.no_wall();
            return None;
        }
        if r.step.is_some() {
            return r.settle(now);
        }
        Some(r.begin(now))
    }

    /// The request never reached the control task: nothing is in flight, so
    /// nothing is owed.
    pub(crate) fn on_dropped(&mut self) {
        self.result_by = None;
        if let Some(r) = self.ramp.as_mut() {
            r.no_wall();
        }
    }

    /// A burst nobody answered. `true` when the embedder's probe state has to
    /// be released so reports resume.
    pub(crate) fn expired(&mut self, now: Instant) -> bool {
        if let Some(r) = self.ramp.as_mut() {
            if r.step.as_ref().is_some_and(|s| now >= s.deadline) {
                tracing::info!(
                    "adaptive bitrate: ramp step unanswered — keeping what the ramp proved"
                );
                r.no_wall();
                self.active = false;
                self.watchdog = None;
                return true;
            }
        }
        if self.watchdog.is_some_and(|at| now >= at) {
            self.watchdog = None;
            self.active = false;
            tracing::warn!(
                "speed-test probe unanswered — clearing it so loss reports and ABR resume"
            );
            return true;
        }
        if self.result_by.is_some_and(|at| now >= at) {
            self.result_by = None;
            tracing::info!(
                "adaptive bitrate: capacity probe timed out (old host?) — keeping negotiated ceiling"
            );
            return true;
        }
        false
    }

    /// The ramp's verdict, once. `Some` exactly one tick after it stopped.
    pub(crate) fn take_ramped(&mut self) -> Option<Ramped> {
        self.ramp.as_mut()?.outcome.take()
    }

    /// What the ramp proved, once it has stopped.
    #[cfg(test)]
    pub(crate) fn ramp_summary(&self) -> Option<RampSummary> {
        let r = self.ramp.as_ref().filter(|r| r.done)?;
        Some(RampSummary {
            wall: r.wall,
            proven_kbps: r.proven_kbps,
            steps: r.steps,
            asked_bytes: r.spent_bytes,
        })
    }

    /// The next burst this fires would be a ramp step.
    pub(crate) fn ramping(&self) -> bool {
        self.ramp.as_ref().is_some_and(|r| !r.done)
    }

    /// The host's end-of-burst report. A ramp step folds it in (the bytes are
    /// still arriving); the legacy burst is answered once, because the
    /// embedder mirrors a finished probe's state for as long as it stands and
    /// the same report arrives again on the next iteration.
    pub(crate) fn on_result(&mut self, r: ProbeReport, now: Instant) -> Measured {
        if let Some(ramp) = self.ramp.as_mut() {
            ramp.on_report(r, now);
            return Measured::NotOurs;
        }
        if self.result_by.take().is_none() {
            return Measured::NotOurs;
        }
        if r.host_duration_ms == 0 || r.delivered_bytes == 0 {
            tracing::info!(
                "adaptive bitrate: capacity probe declined — keeping negotiated ceiling"
            );
            return Measured::Declined;
        }
        // Over the CLIENT receive interval: the host send window closes while
        // the bottleneck queue is still draining, so its duration overstates.
        let delivered_kbps =
            (r.delivered_bytes.saturating_mul(8) / u64::from(r.window_ms.max(1))) as u32;
        let ceiling = delivered_kbps.saturating_mul(7) / 10;
        tracing::info!(
            delivered_kbps,
            ceiling_kbps = ceiling,
            client_interval_ms = r.client_interval_ms,
            host_duration_ms = r.host_duration_ms,
            "adaptive bitrate: link-capacity probe done — climb ceiling set"
        );
        Measured::Ceiling(ceiling)
    }
}

/// Capacity-probe burst target in kbps. `set_ceiling` clamps to the stream
/// cap, so bits above `cap / 0.7` are discarded; ×2 clears the 1.43× bar with
/// margin.
///
/// `u32::MAX` (a mode [`stream_ceiling_kbps`](super::stream_ceiling_kbps)
/// declines to size) keeps 2 Gbps — also the hard ceiling; this can only
/// lower the target.
pub(crate) fn probe_target_kbps(stream_cap_kbps: u32) -> u32 {
    stream_cap_kbps.saturating_mul(2).min(2_000_000)
}

/// Where the ramp stops climbing: the rate that proves the stream cap
/// (`cap / 0.7`), or `PUNKTFUNK_ABR_PROBE_KBPS` when it is lower. Capacity
/// above what the stream can use is not this session's business, so the ramp
/// bounds itself and needs no absolute constant.
fn ramp_max_kbps(stream_cap_kbps: u32, env_kbps: Option<u32>) -> u32 {
    let by_stream = (u64::from(stream_cap_kbps) * 10)
        .div_ceil(7)
        .min(u64::from(u32::MAX)) as u32;
    env_kbps.map_or(by_stream, |k| k.min(by_stream))
}

/// The ceiling a wall licenses: under what the link actually delivered.
pub(crate) fn wall_ceiling_kbps(delivered_kbps: u32) -> u32 {
    (u64::from(delivered_kbps) * u64::from(RAMP_CEILING_PCT) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{CHROMA_IDC_420, CODEC_H264, CODEC_HEVC};

    fn report(delivered_bytes: u64, window_ms: u32) -> ProbeReport {
        ProbeReport {
            delivered_bytes,
            window_ms,
            host_duration_ms: 800,
            client_interval_ms: window_ms,
            ..ProbeReport::default()
        }
    }

    /// A ramp driver: fire steps, answer each with a link of `capacity_kbps`,
    /// and return every step's target plus the verdict.
    struct Rig {
        p: CapacityProbe,
        now: Instant,
        asked: Vec<u32>,
        /// The step `settle` armed while answering the last one.
        pending: Option<(u32, u32)>,
    }

    impl Rig {
        fn new(stream_cap_kbps: u32, env_kbps: Option<u32>) -> Self {
            let now = Instant::now();
            Rig {
                p: CapacityProbe::new(true, true, env_kbps, stream_cap_kbps, now),
                now,
                asked: Vec::new(),
                pending: None,
            }
        }

        fn at(&mut self, ms: u64) -> Instant {
            self.now += Duration::from_millis(ms);
            self.now
        }

        /// One step, answered by a link that carries `capacity_kbps` and a
        /// sender that manages `sender_kbps` of what was asked.
        fn step(&mut self, capacity_kbps: u32, sender_kbps: u32) -> Option<Ramped> {
            let now = self.now;
            let Some((target, duration_ms)) = self.pending.take().or_else(|| self.p.poll(now, 0))
            else {
                return self.p.take_ramped();
            };
            self.asked.push(target);
            let sent_kbps = target.min(sender_kbps);
            let packets = (u64::from(sent_kbps) * u64::from(duration_ms) / 8 / 1_408).max(1);
            // The link stretches the arrivals when it cannot take the rate.
            let interval = (u64::from(duration_ms) * u64::from(sent_kbps)
                / u64::from(capacity_kbps.min(sent_kbps).max(1)))
            .max(1) as u32;
            let r = ProbeReport {
                delivered_bytes: packets * 1_448,
                delivered_packets: packets,
                window_ms: interval,
                host_duration_ms: duration_ms,
                client_interval_ms: interval,
                host_bytes_sent: u64::from(sent_kbps) * u64::from(duration_ms) / 8,
                wire_packets_sent: packets as u32,
                send_dropped: 0,
            };
            let at = self.at(u64::from(interval));
            self.p.on_result(r, at);
            let at = self.at(RAMP_DRAIN_MS + 1);
            self.pending = self.p.poll(at, 0);
            self.p.take_ramped()
        }

        /// Run until the ramp stops, or the step budget runs out.
        fn run(&mut self, capacity_kbps: u32, sender_kbps: u32) -> Ramped {
            for _ in 0..24 {
                if let Some(end) = self.step(capacity_kbps, sender_kbps) {
                    return end;
                }
            }
            panic!("the ramp never stopped: {:?}", self.asked);
        }
    }

    /// The ramp doubles from 5 Mbps and stops at the first step the link
    /// cannot carry, with the rate that step actually delivered.
    #[test]
    fn the_ramp_stops_at_the_first_step_the_link_refuses() {
        let mut rig = Rig::new(46_656, None); // 1080p30 HEVC
        let end = rig.run(12_500, u32::MAX);
        assert_eq!(rig.asked, [5_000, 10_000, 20_000], "{:?}", rig.asked);
        let Ramped::Wall { delivered_kbps } = end else {
            panic!("a 12.5 Mbps link is a wall: {end:?}");
        };
        assert!(
            (11_000..=14_000).contains(&delivered_kbps),
            "the wall read {delivered_kbps} kbps"
        );
        // What it costs the link: three steps of 25 ms.
        let spent = rig.p.ramp_summary().expect("it stopped").asked_bytes;
        assert!(spent < 120_000, "the ramp asked for {spent} bytes");
    }

    /// A link with room to spare: the ramp stops at the rate that proves what
    /// the stream can use and latches no wall.
    #[test]
    fn the_ramp_stops_at_what_the_stream_can_use() {
        let cap = super::super::stream_ceiling_kbps(3840, 2160, 120, CODEC_HEVC, 8, CHROMA_IDC_420);
        let mut rig = Rig::new(cap, None);
        let end = rig.run(10_000_000, u32::MAX);
        let Ramped::NoWall { proven_kbps } = end else {
            panic!("a 10 GbE link has no wall: {end:?}");
        };
        assert!(
            proven_kbps >= cap,
            "{proven_kbps} kbps does not prove a {cap} kbps stream cap"
        );
        assert_eq!(
            *rig.asked.last().expect("steps"),
            ramp_max_kbps(cap, None),
            "the last step is the one that proves the cap"
        );
    }

    /// A sender that cannot offer the asked rate is not a wall: the link was
    /// never asked for it, so nothing about the link was learned.
    #[test]
    fn a_sender_limit_is_not_a_wall() {
        let mut rig = Rig::new(2_000_000, None);
        let end = rig.run(10_000_000, 60_000);
        let Ramped::NoWall { proven_kbps } = end else {
            panic!("the send path is the limit, not the link: {end:?}");
        };
        assert!(
            proven_kbps >= 50_000,
            "what the sender managed is still proved: {proven_kbps} kbps"
        );
    }

    /// `PUNKTFUNK_ABR_PROBE_KBPS` is the ramp's maximum — webOS pins 320 Mbps
    /// against a 4K165 stream cap of ~1 Gbps.
    #[test]
    fn the_env_target_caps_the_ramp() {
        let cap = super::super::stream_ceiling_kbps(3840, 2160, 165, CODEC_HEVC, 8, CHROMA_IDC_420);
        let mut rig = Rig::new(cap, Some(320_000));
        rig.run(10_000_000, u32::MAX);
        assert_eq!(*rig.asked.last().expect("steps"), 320_000);
        assert!(
            rig.asked.iter().all(|&k| k <= 320_000),
            "{:?} went past the pinned maximum",
            rig.asked
        );
    }

    /// Video is the end of the ramp, whatever step is in flight: from the
    /// first frame nothing the controller does may cost a picture (L4).
    #[test]
    fn video_ends_the_ramp_where_it_stands() {
        let mut rig = Rig::new(1_000_000, None);
        assert_eq!(rig.step(1_000_000, u32::MAX), None);
        assert!(rig.pending.is_some(), "a second step went out");
        let now = rig.now;
        assert!(rig.p.poll(now, 1).is_none(), "no step once video is here");
        let Some(Ramped::NoWall { proven_kbps }) = rig.p.take_ramped() else {
            panic!("the first frame ends the ramp with what it had")
        };
        assert!(proven_kbps >= 4_000, "step one still counts: {proven_kbps}");
    }

    /// A step nobody answers ends the ramp on its own deadline, and releases
    /// the embedder's probe state so the report tick resumes.
    #[test]
    fn an_unanswered_step_ends_the_ramp() {
        let mut rig = Rig::new(100_000, None);
        rig.p.poll(rig.now, 0).expect("the first step goes out");
        let at = rig.at(RAMP_STEP_TIMEOUT.as_millis() as u64 + 1);
        assert!(rig.p.expired(at), "the pump's probe state must be released");
        assert_eq!(
            rig.p.take_ramped(),
            Some(Ramped::NoWall { proven_kbps: 0 }),
            "nothing was measured, and nothing is claimed"
        );
    }

    /// A step is over when its bytes stop arriving, not when the host says it
    /// stopped sending: 25 ms at 1 Gbps is 3 MB and fits the receive buffer,
    /// so "all bytes arrived" proves nothing about when.
    #[test]
    fn a_step_ends_on_the_clients_drain() {
        let mut rig = Rig::new(1_000_000, None);
        let (target, duration_ms) = rig.p.poll(rig.now, 0).expect("the first step");
        let mut r = ProbeReport {
            delivered_packets: 2,
            delivered_bytes: 2 * 1_448,
            window_ms: duration_ms,
            host_duration_ms: duration_ms,
            client_interval_ms: duration_ms,
            host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
            wire_packets_sent: 8,
            send_dropped: 0,
        };
        // The host's report is in, but the buffer is still filling.
        for _ in 0..4 {
            let at = rig.at(RAMP_DRAIN_MS);
            r.delivered_packets += 2;
            r.delivered_bytes += 2 * 1_448;
            rig.p.on_result(r, at);
            assert!(
                rig.p.poll(at, 0).is_none(),
                "a step whose bytes are still arriving is not over"
            );
        }
        let at = rig.at(RAMP_DRAIN_MS + 1);
        rig.p.on_result(r, at);
        assert!(
            rig.p.poll(at, 0).is_some(),
            "and once they stop, the next step goes out"
        );
    }

    /// A step queued behind someone else's 450 ms of buffer is late, not
    /// empty. Judging it on the host's report alone reads a busy link as no
    /// link at all, and a newcomer would open on top of its sibling.
    #[test]
    fn a_step_still_in_the_queue_is_not_a_step_that_delivered_nothing() {
        let mut rig = Rig::new(46_656, None);
        let (target, duration_ms) = rig.p.poll(rig.now, 0).expect("the first step");
        // The host says it is done; not one byte has reached us yet.
        let empty = ProbeReport {
            window_ms: duration_ms,
            host_duration_ms: duration_ms,
            host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
            wire_packets_sent: 11,
            ..ProbeReport::default()
        };
        for _ in 0..8 {
            let at = rig.at(RAMP_DRAIN_MS * 2);
            rig.p.on_result(empty, at);
            assert!(rig.p.poll(at, 0).is_none(), "the step is not over");
            assert_eq!(rig.p.take_ramped(), None, "and the ramp has no verdict");
        }
        // The queue hands them over, late and stretched: that is the wall.
        let at = rig.at(1);
        rig.p.on_result(
            ProbeReport {
                delivered_bytes: 11 * 1_448,
                delivered_packets: 11,
                client_interval_ms: duration_ms * 4,
                ..empty
            },
            at,
        );
        let at = rig.at(RAMP_DRAIN_MS + 1);
        rig.p.poll(at, 0);
        assert!(
            matches!(rig.p.take_ramped(), Some(Ramped::Wall { .. })),
            "a step that took four times its window is a wall"
        );
    }

    /// The embedder's probe state keeps saying "done" until the next burst
    /// overwrites it, so the same report arrives on every iteration. Reading
    /// it twice would re-base the byte anchor forever, and the session would
    /// never see a window it could climb on.
    #[test]
    fn a_finished_burst_is_measured_exactly_once() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(true, false, Some(400_000), 100_000, now);
        assert_eq!(p.poll(now + PROBE_DELAY, 1), Some((400_000, PROBE_MS)));
        // 1 MB over 800 ms is 10 Mbps; the ceiling keeps 70 % of it.
        assert_eq!(
            p.on_result(report(1_000_000, 800), now),
            Measured::Ceiling(7_000)
        );
        assert_eq!(
            p.on_result(report(1_000_000, 800), now),
            Measured::NotOurs,
            "the same report must not be read twice"
        );
    }

    /// An embedder speed test finishes too, and its numbers are not the
    /// controller's to learn from.
    #[test]
    fn a_probe_nobody_asked_for_teaches_nothing() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(false, false, None, 100_000, now);
        assert_eq!(p.on_result(report(9_000_000, 800), now), Measured::NotOurs);
    }

    /// Burst must prove the stream cap and no more. Above `cap / 0.7`
    /// is discarded by `set_ceiling` (see
    /// `abr::tests::the_stream_bound_clamps_a_learned_ceiling_only`).
    #[test]
    fn the_probe_target_proves_the_stream_cap_without_overshooting_it() {
        for (w, h, hz, codec, depth) in [
            (1280, 720, 60, CODEC_HEVC, 8),
            (1920, 1080, 60, CODEC_H264, 8),
            (2560, 1440, 120, CODEC_HEVC, 8),
            (3840, 2160, 120, CODEC_HEVC, 10),
        ] {
            let cap = super::super::stream_ceiling_kbps(w, h, hz, codec, depth, CHROMA_IDC_420);
            let target = probe_target_kbps(cap);
            assert!(
                target.saturating_mul(7) / 10 >= cap,
                "{w}x{h}@{hz}: a {target} kbps burst cannot prove a {cap} kbps cap"
            );
            assert!(
                target <= cap.saturating_mul(2),
                "{w}x{h}@{hz}: {target} kbps chases capacity the clamp discards"
            );
            // And the ramp's own maximum proves the same cap with no margin.
            assert!(u64::from(ramp_max_kbps(cap, None)) * 7 / 10 >= u64::from(cap));
        }
        assert_eq!(probe_target_kbps(u32::MAX), 2_000_000);
        assert_eq!(probe_target_kbps(1_500_000), 2_000_000);
    }

    /// The byte cap shortens a step before the receive buffer can hide it.
    #[test]
    fn a_step_is_capped_in_bytes_as_well_as_time() {
        assert_eq!(step_ms(5_000), RAMP_STEP_MS);
        assert_eq!(step_ms(960_000), RAMP_STEP_MS, "3 MB is 25 ms at 960 Mbps");
        assert_eq!(step_ms(2_000_000), 12);
        for kbps in [5_000u32, 100_000, 960_000, 2_000_000, 8_000_000] {
            let bytes = u64::from(kbps) * u64::from(step_ms(kbps)) / 8;
            assert!(bytes <= RAMP_STEP_BYTES, "{kbps} kbps sends {bytes} bytes");
        }
    }

    /// A wall licenses a ceiling under what the link actually delivered.
    #[test]
    fn a_wall_licenses_less_than_it_delivered() {
        assert_eq!(wall_ceiling_kbps(12_500), 10_625);
        assert_eq!(wall_ceiling_kbps(1_000_000), 850_000);
    }
}
