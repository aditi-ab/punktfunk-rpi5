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
    /// Send this next-round probe and keep feeding echoes.
    Probe(ClockProbe),
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
        if self.samples.len() < Self::ROUNDS {
            self.pending_t1 = Some(now_ns);
            return ResyncStep::Probe(ClockProbe { t1_ns: now_ns });
        }
        self.pending_t1 = None;
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

/// Acceptance guard for a re-sync batch: apply the new offset only when its min RTT is
/// comparable to the connect-time RTT — `≤ max(2 ms, 1.5 × connect RTT)`. A congested window
/// biases the offset by its queueing delay, and frames already read late exactly then; better
/// to keep the old estimate and let the next batch try again.
pub fn accept_resync(batch_rtt_ns: u64, connect_rtt_ns: u64) -> bool {
    batch_rtt_ns <= (connect_rtt_ns + connect_rtt_ns / 2).max(2_000_000)
}
