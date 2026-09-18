//! Link simulator: today's controller in a closed loop with models of the
//! link, the host and the client.
//!
//! Integer arithmetic (kbps, bytes, µs) and an inline splitmix64 seeded per
//! scenario, so a run is bit-identical on macOS arm64 and Linux x86_64 and
//! survives a `rand` bump. Time is a 1 ms tick and an `Instant` is
//! `base + Duration`. [`scenarios`] holds the scenario table and the field
//! calibration; [`baseline`] pins what today's controller does on each one.
//!
//! Sessions of one scenario share one modelled link, so the host's
//! [`governor`] runs over them here exactly as it runs over the sessions of
//! one client address in the field — the same function, fed the facts a host
//! has.
//!
//! Nothing here changes production behaviour: the controller is the fixed
//! point, and a behaviour that will not reproduce is a finding about the
//! model, not licence to tune the controller.

mod baseline;
mod client;
mod host;
mod link;
mod scenarios;

use crate::abr::governor;
use crate::quic::AckReason;
use client::{Action, Client, ClientCfg, WindowRec, PROBE_FRAME};
use host::{Host, HostCfg};
use link::{Link, LinkCfg};
use std::time::Instant;

/// splitmix64. One line of state, no dependency, identical everywhere.
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x2545F491_4F6CDD1D) ^ 0x9E3779B9_7F4A7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B9_7F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D_1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB_133111EB);
        z ^ (z >> 31)
    }

    /// Uniform over `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn chance_ppm(&mut self, ppm: u32) -> bool {
        self.below(1_000_000) < u64::from(ppm)
    }
}

struct SessionCfg {
    /// When this session connects. A newcomer joins a link already in use.
    pub join_ms: u64,
    /// When it disconnects, leaving the path to whoever is left.
    pub leave_ms: u64,
    pub host: HostCfg,
    pub client: ClientCfg,
}

struct Scenario {
    pub name: &'static str,
    pub seed: u64,
    pub duration_ms: u64,
    pub link: LinkCfg,
    pub sessions: Vec<SessionCfg>,
    /// Rate this scenario could hold if nothing went wrong — the yardstick
    /// for "time to 90 % of achievable".
    pub achievable_kbps: u32,
    /// One unrecoverable frame injected here, for the recovery metric.
    pub blip_at_ms: Option<u64>,
}

/// One scenario's integer metrics. Every number is a whole unit so the
/// baseline can be compared for equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Metrics {
    pub under5_pct: u32,
    pub to90_s: u32,
    pub cuts_per_10min: u32,
    pub lost_per_10min: u32,
    pub queue_p95_ms: u32,
    pub over_cap_kb_10s: u32,
    pub blip_recover_s: u32,
    pub fairness_x1000: u32,
    /// FNV-1a over every session's `(window index, requested kbps)`. Eight
    /// coarse metrics cannot see a retarget that moved by one window; this
    /// can, so "bit-identical" means the whole decision sequence.
    pub decisions_fnv1a: u32,
}

/// A metric that never happened. Visible in the table rather than silent.
const NEVER: u32 = 99_999;

/// One session's bring-up ramp: every step it asked for, and what it came
/// to (`None` = it never stopped, or never ran).
struct RampTrace {
    pub asks: Vec<(u64, u32)>,
    pub done: Option<(u64, crate::abr::probe::RampSummary)>,
}

struct Run {
    pub metrics: Metrics,
    pub windows: Vec<Vec<WindowRec>>,
    pub ramps: Vec<RampTrace>,
}

impl Run {
    /// Every rate the first session asked the host for, in order.
    fn steps(&self) -> Vec<u32> {
        let mut out = Vec::new();
        let mut last = 0;
        for w in &self.windows[0] {
            if w.rate_kbps != last {
                out.push(w.rate_kbps);
                last = w.rate_kbps;
            }
        }
        out
    }

    /// Every session's rate on one time base, sampled each second: the shape
    /// a shared path is judged by. A session that has not joined, or has
    /// left, reads `0`.
    fn pairs(&self) -> Vec<(u64, Vec<u32>)> {
        let last = self
            .windows
            .iter()
            .filter_map(|w| w.last())
            .map(|w| w.t_ms)
            .max()
            .unwrap_or(0);
        (0..=last / 1_000)
            .map(|s| {
                let t_ms = s * 1_000;
                let rates = self
                    .windows
                    .iter()
                    .map(|ws| match ws.iter().rev().find(|w| w.t_ms <= t_ms) {
                        Some(w) if w.t_ms + 2_000 >= t_ms => w.rate_kbps,
                        _ => 0,
                    })
                    .collect();
                (t_ms, rates)
            })
            .collect()
    }

    /// Windows where the controller asked for less than it had.
    fn cuts(&self) -> Vec<&WindowRec> {
        self.windows[0]
            .iter()
            .filter(|w| w.cut_from_kbps.is_some())
            .collect()
    }
}

struct Session {
    join_ms: u64,
    leave_ms: u64,
    host: Host,
    client: Client,
}

impl Session {
    fn live(&self, now_ms: u64) -> bool {
        (self.join_ms..self.leave_ms).contains(&now_ms)
    }

    /// This session as the host's governor sees it: the encoder target it set,
    /// the wire rate it put out, what the client last reported arriving, and
    /// whether the source is still. The host has all four without asking.
    fn member(&self, now_ms: u64, offered_kbps: u32, share_kbps: Option<u32>) -> governor::Member {
        governor::Member {
            automatic: self.client.automatic(),
            current_kbps: self.host.budget_kbps(),
            offered_kbps,
            delivered_kbps: self.client.delivered_kbps(),
            idle: self.host.idle(now_ms),
            share_kbps,
        }
    }
}

/// The host's send counter as it stood at the last delivery report, and the
/// rate it came to since the one before that.
///
/// Read on a report rather than on a timer so both sides of "short" describe
/// the same stretch of link: what went out over one window against what
/// arrived over it.
#[derive(Clone, Copy, Default)]
struct Offered {
    reports: usize,
    at_ms: u64,
    bytes: u64,
    kbps: u32,
}

impl Offered {
    fn update(&mut self, s: &Session, now_ms: u64) {
        if s.client.reports() == self.reports {
            return;
        }
        let bytes = s.host.offered_bytes();
        self.kbps = ((bytes - self.bytes) * 8 / (now_ms - self.at_ms).max(1)) as u32;
        self.reports = s.client.reports();
        self.at_ms = now_ms;
        self.bytes = bytes;
    }
}

/// How often the host re-takes the shares. It runs on delivery reports, which
/// arrive a few times a second; only [`governor::SHARE_CLOCK`] lets a share
/// rise, so this cadence decides how fast a cut lands and nothing else.
const GOVERN_EVERY_MS: u64 = 250;

fn run(sc: &Scenario) -> Run {
    let base = Instant::now();
    let mut link = Link::new(sc.link.clone(), sc.seed);
    let mut sessions: Vec<Session> = sc
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            // A session's clock starts when it joins, not when the run does:
            // its first report window is 750 ms after its first frame.
            let joined = base + std::time::Duration::from_millis(s.join_ms);
            let seed = sc.seed ^ (0x51_u64 << (i * 8));
            let mut client = Client::new(s.client.clone(), seed, base, joined);
            if let Some(at) = sc.blip_at_ms {
                if i == 0 {
                    client.inject_lost_frame(at);
                }
            }
            Session {
                join_ms: s.join_ms,
                leave_ms: s.leave_ms,
                host: Host::new(s.host.clone(), s.client.start_kbps, sc.seed ^ (0x9A << i)),
                client,
            }
        })
        .collect();
    let mut drained = Vec::new();
    let mut actions = Vec::new();
    let (mut offered_10s, mut capacity_10s) = (0u64, 0u64);
    // The host's own memory of what each session was last told, and the clock
    // an up-move waits for.
    let mut shares: Vec<Option<u32>> = vec![None; sessions.len()];
    let mut next_raise_ms = governor::SHARE_CLOCK.as_millis() as u64;
    let mut next_lift_ms = governor::SHARE_LIFT_CLOCK.as_millis() as u64;
    let mut members: Vec<usize> = Vec::new();
    // Who is on a shared path, and what it was last seen carrying for them.
    let mut grouped = vec![false; sessions.len()];
    let mut path_kbps = 0;
    // A session that has not joined has sent nothing, so its mark starts
    // where it does.
    let mut offered: Vec<Offered> = sessions
        .iter()
        .map(|s| Offered {
            at_ms: s.join_ms,
            ..Offered::default()
        })
        .collect();

    for now in 0..sc.duration_ms {
        if now < 10_000 {
            capacity_10s += u64::from(link.capacity_kbps(now)) / 8;
        }
        for (i, s) in sessions.iter_mut().enumerate() {
            if !s.live(now) {
                continue;
            }
            let id = i as u8;
            let mut offer = |link: &mut Link, client: &mut Client, frame: u32, bytes: u64| {
                if bytes == 0 {
                    return;
                }
                if now < 10_000 {
                    offered_10s += bytes;
                }
                let refused = link.offer(now, id, frame, bytes);
                if refused > 0 {
                    if let Some(shards) = client.refuse(frame, refused) {
                        let draw = link.draw_loss(shards);
                        client.complete(frame, draw, now);
                    }
                }
            };
            // Filler first: the burst pumps between AUs, so a video frame
            // lands on a queue the filler has just refilled. Offering video
            // first would hand it the whole of each tick's drain and the
            // burst would cost the session nothing.
            let filler = s.host.probe_release(now);
            offer(&mut link, &mut s.client, PROBE_FRAME, filler);
            if let Some(f) = s.host.tick(now) {
                s.client.expect(&f, now);
                if let Some((tail, bytes)) = s.host.take_flush() {
                    offer(&mut link, &mut s.client, tail, bytes);
                }
                let burst = s.host.burst_of(&f);
                offer(&mut link, &mut s.client, f.id, burst);
            }
            let (frame, bytes) = s.host.release(now);
            offer(&mut link, &mut s.client, frame, bytes);
        }
        drained.clear();
        link.tick(now, &mut drained);
        for &(id, frame, bytes) in &drained {
            let s = &mut sessions[id as usize];
            if frame == PROBE_FRAME {
                s.client.deliver_probe(bytes, now + link.base_delay_ms());
            } else if let Some(shards) = s.client.deliver(frame, bytes, now + link.base_delay_ms())
            {
                let draw = link.draw_loss(shards);
                s.client.complete(frame, draw, now + link.base_delay_ms());
            }
        }
        for (i, s) in sessions.iter_mut().enumerate() {
            if !s.live(now) {
                continue;
            }
            actions.clear();
            s.client.tick(now, base, &mut actions);
            for a in &actions {
                match *a {
                    Action::SetBitrate(kbps) => s.host.on_set_bitrate(now, kbps),
                    Action::Keyframe => s.host.on_keyframe_request(now),
                    Action::Loss { ppm, unrecovered } => s.host.on_loss_report(ppm, unrecovered),
                    Action::Probe {
                        target_kbps,
                        duration_ms,
                    } => s.host.on_probe_request(now, target_kbps, duration_ms),
                }
            }
            if let Some(done) = s.host.probe_done(now) {
                s.client.on_probe_result(now, done);
            }
            if let Some((kbps, why)) = s.host.apply_pending(now) {
                s.client.push_ack(kbps, why);
            }
            if let Some(share) = s.host.apply_governor(now) {
                s.client.push_ack(share, AckReason::Governor);
            }
            // Read the send counter where the report window closed, so both
            // sides of "short" cover the same stretch of link.
            offered[i].update(s, now);
        }
        if now % GOVERN_EVERY_MS != 0 {
            continue;
        }
        members.clear();
        members.extend((0..sessions.len()).filter(|&i| sessions[i].live(now)));
        let facts: Vec<governor::Member> = members
            .iter()
            .map(|&i| sessions[i].member(now, offered[i].kbps, shares[i]))
            .collect();
        if members.len() < 2 {
            // Alone on the path: hand over the whole of what the group proved
            // it carried, once. The wall this session measured beside them was
            // their residual, and nobody but the host knows they have gone.
            for &i in &members {
                if std::mem::take(&mut grouped[i]) && path_kbps > 0 {
                    sessions[i].host.govern(now, path_kbps);
                    shares[i] = Some(path_kbps);
                }
            }
            path_kbps = 0;
            continue;
        }
        path_kbps = path_kbps.max(governor::path_kbps(&facts));
        for &i in &members {
            grouped[i] = true;
        }
        let clocks = governor::Clocks {
            room: now >= next_raise_ms,
            lift: now >= next_lift_ms,
        };
        if clocks.room {
            next_raise_ms = now + governor::SHARE_CLOCK.as_millis() as u64;
        }
        if clocks.lift {
            next_lift_ms = now + governor::SHARE_LIFT_CLOCK.as_millis() as u64;
        }
        for (k, share) in governor::shares(&facts, path_kbps, clocks)
            .into_iter()
            .enumerate()
        {
            let Some(share) = share else { continue };
            let i = members[k];
            shares[i] = (share != governor::NO_SHARE_KBPS).then_some(share);
            sessions[i].host.govern(now, share);
        }
    }
    let metrics = measure(sc, &sessions, &mut link, offered_10s, capacity_10s);
    let (windows, ramps) = sessions
        .into_iter()
        .map(|s| {
            let c = s.client;
            (
                c.windows,
                RampTrace {
                    asks: c.ramp_asks,
                    done: c.ramp_done,
                },
            )
        })
        .unzip();
    Run {
        metrics,
        windows,
        ramps,
    }
}

/// FNV-1a over the decisions every session made, in order.
fn decision_checksum(sessions: &[Session]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for s in sessions {
        for (i, w) in s.client.windows.iter().enumerate() {
            let Some(kbps) = w.request_kbps else { continue };
            for b in (i as u32)
                .to_le_bytes()
                .into_iter()
                .chain(kbps.to_le_bytes())
            {
                h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
            }
        }
    }
    h
}

fn percentile(samples: &mut [u32], pct: usize) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[(samples.len() - 1) * pct / 100]
}

fn measure(
    sc: &Scenario,
    sessions: &[Session],
    link: &mut Link,
    offered_10s: u64,
    capacity_10s: u64,
) -> Metrics {
    let first = &sessions[0].client;
    let live: Vec<&WindowRec> = first.windows.iter().filter(|w| !w.discarded).collect();
    let under5 = live.iter().filter(|w| w.rate_kbps < 5_000).count();
    let under5_pct = if live.is_empty() {
        0
    } else {
        (under5 * 100 / live.len()) as u32
    };
    let want = sc.achievable_kbps / 10 * 9;
    let to90_s = live
        .iter()
        .find(|w| w.rate_kbps >= want)
        .map_or(NEVER, |w| (w.t_ms / 1_000) as u32);
    let scale = |n: u64| (n * 600_000 / sc.duration_ms.max(1)) as u32;
    let cuts = live.iter().filter(|w| w.cut_from_kbps.is_some()).count() as u64;
    let lost: u64 = live.iter().map(|w| w.dropped).sum();
    let mut owd = first.owd_samples.clone();
    let queue_p95_ms = percentile(&mut owd, 95).saturating_sub(link.base_delay_ms() as u32);
    let over_cap_kb_10s = offered_10s.saturating_sub(capacity_10s) / 1_000;
    // Recovery is measured from the blip to the first window back at the rate
    // it was holding — but only once the blip has actually cost something.
    // The window the verdict lands in still reports the old rate.
    let blip_recover_s = match sc.blip_at_ms {
        None => 0,
        Some(at) => {
            let before = live
                .iter()
                .rev()
                .find(|w| w.t_ms <= at)
                .map_or(0, |w| w.rate_kbps);
            match live.iter().find(|w| w.t_ms > at && w.rate_kbps < before) {
                None => 0,
                Some(dip) => live
                    .iter()
                    .find(|w| w.t_ms > dip.t_ms && w.rate_kbps >= before)
                    .map_or(NEVER, |w| ((w.t_ms - at) / 1_000) as u32),
            }
        }
    };
    // Jain over the sessions' mean rate. One session is fair by definition.
    let means: Vec<u64> = sessions
        .iter()
        .map(|s| {
            let w = &s.client.windows;
            if w.is_empty() {
                0
            } else {
                w.iter().map(|w| u64::from(w.rate_kbps)).sum::<u64>() / w.len() as u64
            }
        })
        .collect();
    let sum: u64 = means.iter().sum();
    let sq: u64 = means.iter().map(|m| m * m).sum();
    let fairness_x1000 = if sq == 0 {
        1_000
    } else {
        (sum * sum * 1_000 / (means.len() as u64 * sq)) as u32
    };
    Metrics {
        under5_pct,
        to90_s,
        cuts_per_10min: scale(cuts),
        lost_per_10min: scale(lost),
        queue_p95_ms,
        over_cap_kb_10s: over_cap_kb_10s as u32,
        blip_recover_s,
        fairness_x1000,
        decisions_fnv1a: decision_checksum(sessions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator is the same stream everywhere, so a baseline row is a
    /// fact and not a platform's opinion.
    #[test]
    fn the_generator_is_pinned() {
        let mut r = Rng::new(7);
        assert_eq!(
            [r.next_u64(), r.next_u64(), r.next_u64()],
            [
                16_557_362_563_216_862_149,
                430_200_180_043_962_517,
                5_998_290_083_107_941_422
            ]
        );
        let mut r = Rng::new(7);
        let hits = (0..10_000).filter(|_| r.chance_ppm(250_000)).count();
        assert_eq!(hits, 2_481, "a quarter of the draws, to the draw");
    }

    /// Ten minutes of a 1.3 Gbps session in under a second, unoptimized — the
    /// budget that keeps a scenario a unit test.
    #[test]
    fn ten_minutes_at_the_top_of_the_range_simulates_in_under_a_second() {
        let started = Instant::now();
        let r = run(&scenarios::fat_pipe_10min());
        let took = started.elapsed();
        assert!(
            r.windows[0].last().is_some_and(|w| w.rate_kbps > 1_000_000),
            "the session must actually reach the top of the range"
        );
        assert!(took.as_millis() < 1_000, "ten minutes took {took:?}");
    }
}
