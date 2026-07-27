//! Wall-clock skew: the connect-time handshake ([`clock_sync`]), the NTP-style offset
//! estimator ([`clock_offset_ns`]), and the mid-stream re-sync state machine
//! ([`ClockResync`]).

use super::{io, ClockEcho, ClockProbe};

/// Estimate the host↔client clock offset (**host minus client**, ns) and RTT (ns) from skew-handshake
/// samples `(t1, t2, t3, t4)` — NTP's formula, taking the **minimum-RTT** sample (least queuing
/// noise; also discards the first round's host-setup latency). Offset is positive when the host
/// clock is ahead of the client's; add it to a client timestamp to express it in the host clock.
/// Returns `None` for an empty sample set.
pub fn clock_offset_ns(samples: &[(u64, u64, u64, u64)]) -> Option<(i64, u64)> {
    samples
        .iter()
        .map(|&(t1, t2, t3, t4)| {
            let rtt = ((t4 as i128 - t1 as i128) - (t3 as i128 - t2 as i128)).max(0) as u64;
            let offset = (((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2) as i64;
            (offset, rtt)
        })
        .min_by_key(|&(_, rtt)| rtt)
}

/// One wall-clock skew-handshake outcome (see [`clock_sync`]).
pub struct ClockSkew {
    /// Host clock minus client clock, ns: add it to a client timestamp to express it in host time.
    pub offset_ns: i64,
    /// Round-trip time of the minimum-RTT sample, ns.
    pub rtt_ns: u64,
    /// How many probe rounds the host answered.
    pub rounds: usize,
}

/// Run the wall-clock skew handshake from the client side over the (already-open) control stream:
/// `ROUNDS` [`ClockProbe`]/[`ClockEcho`] round-trips, returning the host↔client offset from the
/// minimum-RTT sample. `None` if the host never answers (an old host) — the caller then assumes a
/// shared clock. Each read is bounded so a silent host can't wedge session start. Shared by the
/// reference client and the embeddable connector; uses the realtime clock the host stamps `pts_ns`
/// with, so the offset aligns a client receive instant to the host's capture clock.
pub async fn clock_sync(
    send: &mut quinn::SendStream,
    recv: &mut io::MsgReader,
) -> Option<ClockSkew> {
    use std::time::Duration;
    const ROUNDS: usize = 8;
    let read_timeout = Duration::from_secs(2);
    let mut samples: Vec<(u64, u64, u64, u64)> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t1 = wall_clock_ns();
        let probe = ClockProbe { t1_ns: t1 }.encode();
        if io::write_msg(send, &probe).await.is_err() {
            break;
        }
        let read = tokio::time::timeout(read_timeout, recv.read_msg()).await;
        let echo = match read {
            Ok(Ok(b)) => match ClockEcho::decode(&b) {
                Ok(e) => e,
                Err(_) => break,
            },
            _ => break, // timeout or stream error -> old host / no skew support
        };
        samples.push((echo.t1_ns, echo.t2_ns, echo.t3_ns, wall_clock_ns()));
    }
    clock_offset_ns(&samples).map(|(offset_ns, rtt_ns)| ClockSkew {
        offset_ns,
        rtt_ns,
        rounds: samples.len(),
    })
}

/// Wall-clock now (ns since the Unix epoch) — the clock the skew handshake stamps and the host
/// stamps AU `pts_ns` with (CLOCK_REALTIME basis, deliberately NOT monotonic: steps/slew are
/// exactly what the handshake measures across machines).
pub fn wall_clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// What [`ClockResync::on_echo`] asks the driver to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum ResyncStep {
    /// Nothing — the echo was stale (a previous batch) or no batch is in flight.
    Idle,
    /// The round was recorded and the batch wants another: wait the inter-round spacing, then
    /// stamp + send [`ClockResync::next_probe`]. Spacing the rounds makes the batch sample
    /// several phases of the periodic video-burst cycle instead of completing inside one burst
    /// — at high bitrates the whole 8-round batch otherwise fits in a single ~6 ms burst and
    /// every round reads the same congested (or same quiet) instant.
    MoreRounds,
    /// The batch is complete: the min-RTT estimate over its rounds, per [`clock_offset_ns`].
    Done { offset_ns: i64, rtt_ns: u64 },
}

/// Mid-stream wall-clock re-sync (networking-audit deferred plan §2): the same 8-round
/// probe/echo estimate as the connect-time [`clock_sync`], restructured as a state machine so
/// the client's control task can drive it from its `select!` loop without blocking the stream —
/// echoes interleave with other control traffic; rounds are matched by the echoed `t1`.
///
/// A step or slow drift of either wall clock after connect silently corrupts the clock-based
/// jump-to-live signal, the ABR one-way-delay signal, and every latency stat. Re-syncing
/// restores them; the disarm heuristic stays as the final backstop.
pub struct ClockResync {
    /// `t1_ns` of the probe in flight; `None` = no batch active. An echo whose `t1` doesn't
    /// match is stale (an abandoned batch) and ignored.
    pending_t1: Option<u64>,
    samples: Vec<(u64, u64, u64, u64)>,
}

impl ClockResync {
    /// Rounds per batch — matches the connect-time [`clock_sync`].
    pub const ROUNDS: usize = 8;

    pub fn new() -> ClockResync {
        ClockResync {
            pending_t1: None,
            samples: Vec::with_capacity(Self::ROUNDS),
        }
    }

    /// Start a (new) batch, abandoning any batch still in flight — its late echoes won't match
    /// `pending_t1` and get ignored. Returns the first probe to send, stamped `now_ns`.
    pub fn begin(&mut self, now_ns: u64) -> ClockProbe {
        self.samples.clear();
        self.next_probe(now_ns)
    }

    /// Stamp + arm the next round's probe at `now_ns` — send it immediately. Called after
    /// [`ResyncStep::MoreRounds`] once the caller's inter-round spacing has elapsed; stamping
    /// at send time keeps that spacing out of the measured RTT.
    pub fn next_probe(&mut self, now_ns: u64) -> ClockProbe {
        self.pending_t1 = Some(now_ns);
        ClockProbe { t1_ns: now_ns }
    }

    /// Feed an inbound [`ClockEcho`] received at `now_ns` (the round's `t4`).
    pub fn on_echo(&mut self, echo: &ClockEcho, now_ns: u64) -> ResyncStep {
        if self.pending_t1 != Some(echo.t1_ns) {
            return ResyncStep::Idle; // stale (abandoned batch) or unsolicited
        }
        self.samples
            .push((echo.t1_ns, echo.t2_ns, echo.t3_ns, now_ns));
        // No probe in flight until the driver arms the next round (or a batch restarts) — a
        // duplicate of this round's echo must not double-record.
        self.pending_t1 = None;
        if self.samples.len() < Self::ROUNDS {
            return ResyncStep::MoreRounds;
        }
        match clock_offset_ns(&self.samples) {
            Some((offset_ns, rtt_ns)) => ResyncStep::Done { offset_ns, rtt_ns },
            None => ResyncStep::Idle, // unreachable: ROUNDS > 0 samples were just collected
        }
    }
}

impl Default for ClockResync {
    fn default() -> Self {
        Self::new()
    }
}

/// Acceptance predicate for a re-sync batch: its min RTT must be comparable to the best RTT
/// this session has evidenced — `≤ max(2 ms, 1.5 × floor)`. A congested window biases the
/// offset by its queueing delay, and frames already read late exactly then; better to keep the
/// old estimate and let the next batch try again.
pub fn accept_resync(batch_rtt_ns: u64, floor_rtt_ns: u64) -> bool {
    batch_rtt_ns <= (floor_rtt_ns + floor_rtt_ns / 2).max(2_000_000)
}

/// Admission decision for a completed re-sync batch (see [`ResyncGuard::admit`]).
#[derive(Debug, PartialEq, Eq)]
pub enum ResyncAdmit {
    /// Batch RTT is within the guard band of the session floor: apply this batch's offset.
    Fresh,
    /// Batch rejected (congested window) — keep the previous offset; `streak` counts the
    /// consecutive rejections since the last applied batch.
    Rejected { streak: u32 },
    /// The rejection streak hit [`ResyncGuard::MAX_REJECTED_STREAK`]: apply the best (min-RTT)
    /// batch of the streak instead of drifting further. Carries that batch's estimate.
    BestOfStreak { offset_ns: i64, rtt_ns: u64 },
}

/// Admission control for mid-stream re-sync batches. Two fixes over the original static
/// `≤ max(2 ms, 1.5 × connect RTT)` guard (2026-07 PyroWave-sawtooth field report):
///
/// - **The baseline is the session floor, not the connect-time RTT.** The connect handshake
///   runs before the video data plane exists; comparing loaded mid-stream batches against that
///   idle figure rejected essentially every batch of a high-bitrate LAN session, and the
///   offset went stale while the wall clocks drifted apart — the OSD latency ramped for
///   minutes and snapped back only when a lucky batch landed. The floor now folds in every
///   completed batch's min RTT (rejected ones included: their min-RTT round is still floor
///   evidence), so the baseline tracks what this path can actually do under load.
/// - **Staleness is bounded.** After [`Self::MAX_REJECTED_STREAK`] consecutive rejections the
///   best batch of the streak is applied anyway: its queueing bias is at most ~half its RTT
///   (a few ms), while unbounded wall-clock drift is worth that many ms *per minute* on a
///   slewing clock. A bounded bias beats an unbounded drift.
pub struct ResyncGuard {
    /// Best RTT this session has evidenced: connect-time RTT, then min over every batch.
    floor_rtt_ns: u64,
    rejected_streak: u32,
    /// Min-RTT batch among the current rejection streak.
    best_pending: Option<(i64, u64)>,
}

impl ResyncGuard {
    /// Consecutive rejected batches tolerated before the best of them is applied anyway.
    pub const MAX_REJECTED_STREAK: u32 = 3;

    pub fn new(connect_rtt_ns: u64) -> ResyncGuard {
        ResyncGuard {
            floor_rtt_ns: connect_rtt_ns,
            rejected_streak: 0,
            best_pending: None,
        }
    }

    /// The current baseline the guard compares batches against (log/debug surface).
    pub fn floor_rtt_ns(&self) -> u64 {
        self.floor_rtt_ns
    }

    /// Judge a completed batch. The caller applies the offset on [`ResyncAdmit::Fresh`] (this
    /// batch's) or [`ResyncAdmit::BestOfStreak`] (the carried one) and keeps the old offset on
    /// [`ResyncAdmit::Rejected`].
    pub fn admit(&mut self, offset_ns: i64, rtt_ns: u64) -> ResyncAdmit {
        // Judge against the floor as evidenced BEFORE this batch, then fold this batch in —
        // comparing a batch against a floor that already includes it would accept everything.
        let fresh = accept_resync(rtt_ns, self.floor_rtt_ns);
        self.floor_rtt_ns = self.floor_rtt_ns.min(rtt_ns);
        if fresh {
            self.rejected_streak = 0;
            self.best_pending = None;
            return ResyncAdmit::Fresh;
        }
        let best = match self.best_pending {
            Some((o, r)) if r <= rtt_ns => (o, r),
            _ => (offset_ns, rtt_ns),
        };
        self.best_pending = Some(best);
        self.rejected_streak += 1;
        if self.rejected_streak >= Self::MAX_REJECTED_STREAK {
            self.rejected_streak = 0;
            self.best_pending = None;
            return ResyncAdmit::BestOfStreak {
                offset_ns: best.0,
                rtt_ns: best.1,
            };
        }
        ResyncAdmit::Rejected {
            streak: self.rejected_streak,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn clock_offset_picks_min_rtt_and_recovers_offset() {
        // Host clock is +1_000_000 ns ahead of the client. Construct samples where a symmetric
        // round-trip recovers exactly that offset, and a noisy (asymmetric, high-RTT) sample is
        // present but must be ignored by the min-RTT selection.
        const OFF: i64 = 1_000_000;
        // Clean sample: client t1=0, one-way=200µs each way → t2 = t1 + 200_000 + OFF (host clock),
        // t3 = t2 + 50_000 (host processing), t4 = t3 - OFF + 200_000 (back in client clock).
        let t1 = 0u64;
        let t2 = (t1 as i64 + 200_000 + OFF) as u64;
        let t3 = t2 + 50_000;
        let t4 = (t3 as i64 - OFF + 200_000) as u64;
        // Noisy sample: same offset but a fat, asymmetric RTT (slow return path) — higher RTT.
        let n1 = 1_000_000u64;
        let n2 = (n1 as i64 + 200_000 + OFF) as u64;
        let n3 = n2 + 50_000;
        let n4 = (n3 as i64 - OFF + 5_000_000) as u64; // 5 ms return → big RTT
        let (offset, rtt) =
            clock_offset_ns(&[(n1, n2, n3, n4), (t1, t2, t3, t4)]).expect("non-empty");
        // The min-RTT sample recovers the offset exactly; its RTT is 2x200us, and the noisy
        // (asymmetric, 5 ms return) sample is ignored by the min-RTT selection.
        assert_eq!(offset, OFF);
        assert_eq!(rtt, 400_000);
        assert!(clock_offset_ns(&[]).is_none());
    }

    /// The mid-stream re-sync state machine: 8 rounds collected via matched echoes, stale
    /// echoes ignored, a restarted batch abandons the old one, and the batch result is the
    /// min-RTT estimate — the exact behavior the connect-time `clock_sync` loop has.
    #[test]
    fn clock_resync_collects_rounds_and_ignores_stale_echoes() {
        // Host clock +1 ms ahead; symmetric 100 µs one-way paths except one congested round.
        const OFF: i64 = 1_000_000;
        let echo_for = |t1: u64, one_way: u64| ClockEcho {
            t1_ns: t1,
            t2_ns: (t1 as i64 + one_way as i64 + OFF) as u64,
            t3_ns: (t1 as i64 + one_way as i64 + OFF) as u64 + 10_000,
        };
        let t4_for = |e: &ClockEcho, one_way: u64| (e.t3_ns as i64 - OFF + one_way as i64) as u64;

        let mut rs = ClockResync::new();
        // An unsolicited echo before any batch is ignored.
        assert_eq!(
            rs.on_echo(&echo_for(42, 100_000), 500_000),
            ResyncStep::Idle
        );

        let mut probe = rs.begin(1_000_000);
        // A stale echo (wrong t1: the abandoned pre-begin probe) is ignored mid-batch.
        assert_eq!(
            rs.on_echo(&echo_for(42, 100_000), 500_000),
            ResyncStep::Idle
        );
        for round in 0..ClockResync::ROUNDS {
            // Round 3 is congested (5 ms one-way) — it must lose the min-RTT selection.
            let one_way = if round == 3 { 5_000_000 } else { 100_000 };
            let echo = echo_for(probe.t1_ns, one_way);
            let t4 = t4_for(&echo, one_way);
            match rs.on_echo(&echo, t4) {
                ResyncStep::MoreRounds => {
                    assert!(round < ClockResync::ROUNDS - 1, "batch overran its rounds");
                    // A duplicate of the just-consumed echo must not double-record: no probe
                    // is in flight until the driver arms the next round.
                    assert_eq!(rs.on_echo(&echo, t4), ResyncStep::Idle);
                    // The driver stamps the next probe at SEND time (after its inter-round
                    // spacing), so the spacing never lands in the measured RTT.
                    probe = rs.next_probe(t4 + 7_000_000);
                }
                ResyncStep::Done { offset_ns, rtt_ns } => {
                    assert_eq!(round, ClockResync::ROUNDS - 1, "batch ended early");
                    assert_eq!(offset_ns, OFF, "min-RTT round recovers the offset exactly");
                    assert_eq!(rtt_ns, 200_000); // 2×100 µs; host processing (t3−t2) excluded
                }
                ResyncStep::Idle => panic!("matched echo must advance the batch"),
            }
        }
        // The batch is done: even a matching-t1 replay no longer advances anything.
        assert_eq!(
            rs.on_echo(&echo_for(probe.t1_ns, 100_000), probe.t1_ns + 300_000),
            ResyncStep::Idle
        );

        // begin() mid-batch abandons the in-flight batch: its echo is stale afterwards.
        let old = rs.begin(2_000_000);
        let fresh = rs.begin(3_000_000);
        assert_eq!(
            rs.on_echo(&echo_for(old.t1_ns, 100_000), 2_300_000),
            ResyncStep::Idle
        );
        assert_eq!(
            rs.on_echo(&echo_for(fresh.t1_ns, 100_000), 3_300_000),
            ResyncStep::MoreRounds
        );
    }

    /// The guard's two field-report fixes: the baseline tracks the SESSION floor (a batch
    /// better than the stale connect figure re-anchors it), and a rejection streak is bounded
    /// — the best batch of the streak is applied rather than letting the offset go stale
    /// while the wall clocks drift apart.
    #[test]
    fn resync_guard_floor_tracking_and_bounded_streak() {
        // Connect measured 400 µs idle; the 2 ms floor of accept_resync governs early on.
        let mut g = ResyncGuard::new(400_000);
        assert_eq!(g.admit(10, 1_500_000), ResyncAdmit::Fresh);
        // A better batch lowers the floor evidence.
        assert_eq!(g.admit(11, 300_000), ResyncAdmit::Fresh);
        assert_eq!(g.floor_rtt_ns(), 300_000);

        // Loaded stretch: batches at 4–6 ms all exceed max(2 ms, 1.5 × 300 µs).
        assert_eq!(g.admit(100, 6_000_000), ResyncAdmit::Rejected { streak: 1 });
        // The best (min-RTT) batch of the streak is remembered…
        assert_eq!(g.admit(200, 4_000_000), ResyncAdmit::Rejected { streak: 2 });
        // …and applied when the streak hits the cap — offset 200 (the 4 ms batch), not 300.
        assert_eq!(
            g.admit(300, 5_000_000),
            ResyncAdmit::BestOfStreak {
                offset_ns: 200,
                rtt_ns: 4_000_000
            }
        );
        // The streak reset: the next congested batch starts a new one.
        assert_eq!(g.admit(400, 5_000_000), ResyncAdmit::Rejected { streak: 1 });
        // A quiet batch clears it and applies normally.
        assert_eq!(g.admit(500, 350_000), ResyncAdmit::Fresh);

        // A batch that IS the new floor is always fresh (compared against the pre-batch floor).
        let mut g2 = ResyncGuard::new(10_000_000);
        assert_eq!(g2.admit(1, 8_000_000), ResyncAdmit::Fresh);
        assert_eq!(g2.floor_rtt_ns(), 8_000_000);
    }

    /// The acceptance guard: a batch measured through a congested window (fat RTT) must not
    /// replace the offset — its queueing delay biases the estimate exactly when frames
    /// already read late. Floor of 2 ms so a near-zero connect RTT (same-host/LAN) doesn't
    /// reject every later batch over normal jitter.
    #[test]
    fn clock_resync_acceptance_guard() {
        // Generous connect RTT (10 ms): accept up to 1.5×.
        assert!(accept_resync(14_000_000, 10_000_000));
        assert!(!accept_resync(16_000_000, 10_000_000));
        // Tiny connect RTT (200 µs, wired LAN): the 2 ms floor governs.
        assert!(accept_resync(1_900_000, 200_000));
        assert!(!accept_resync(2_100_000, 200_000));
        // Boundary: exactly at the bound is accepted.
        assert!(accept_resync(2_000_000, 0));
        assert!(accept_resync(15_000_000, 10_000_000));
    }
}
