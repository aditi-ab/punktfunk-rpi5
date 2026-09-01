//! Shared packet scheduling for native and GameStream video sends.
//!
//! Both planes send an initial microburst, then pace overflow with a bitrate-derived budget
//! while retaining their own sockets and framing. Native adapts chunk size to preserve useful
//! sleep intervals; GameStream bounds the number of sleep steps on its non-realtime thread.
//! Deterministic tests pin both policies.
//!
//! The shared `PUNKTFUNK_VIDEO_DROP` test hook and percentile helper live here as well.

use std::time::{Duration, Instant};

/// One paced send's outcome: how long the frame's packets took to leave (`spread_us`) and
/// whether any were paced (vs the whole frame fitting the microburst and going out
/// immediately). The native plane feeds it to the PUNKTFUNK_PERF histogram so the pacing tail
/// is visible per-frame.
pub(crate) struct PaceStat {
    pub(crate) spread_us: u32,
    pub(crate) paced: bool,
}

/// How a frame's packets split into send chunks.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ChunkPolicy {
    /// Fixed chunk size; the step count scales with the frame (native: 16).
    Fixed(usize),
    /// Rate-adaptive chunk size (native, plan Phase 1.2): `base` packets until the per-chunk
    /// interval (`budget / steps`) would drop under the sleep floor, then the smallest chunk
    /// that keeps the interval ≥ floor, capped at `max` (the 64-segment GSO super-buffer
    /// limit). Zero budget (no slack — the frame blasts anyway) takes `max`: fewest syscalls
    /// for the same immediate send. Decouples the syscall batch from the pace step so high
    /// rates keep REAL sleeps between chunks instead of skipping every sub-floor wait.
    Adaptive { base: usize, max: usize },
    /// Bounded step count: `chunk = max(min_chunk, ceil(n / max_steps))` (GameStream: 16 / 12).
    /// Keeps per-frame sleep overshoot independent of bitrate — see `spawn_sender`'s history.
    Bounded { min_chunk: usize, max_steps: usize },
}

/// The time the paced (post-burst) packets spread across.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PaceBudget {
    /// `min((deadline − now-after-burst) × fraction, cap)`, collapsing to 0 with no slack
    /// (native: fraction 0.9). `cap` bounds the spread to the time the overflow actually needs
    /// at a rate the link is proven to carry (latency plan T1.2): the deadline term alone
    /// smears a large frame across the whole remaining interval even when the link could drain
    /// it in a fraction of that — `Duration::MAX` = uncapped (the legacy smoothness-only
    /// schedule).
    UntilDeadline {
        deadline: Instant,
        fraction: f32,
        cap: Duration,
    },
    /// A precomputed fixed budget (GameStream: ¾ of the frame interval; native: the rate-cap
    /// spread from [`native_budget`]).
    Fixed(Duration),
}

/// Absolute ceiling on one frame's paced spread (native plane): a pathological frame must not
/// park the send thread for longer than this, whatever the rate math says. At the ceiling the
/// tail is late but delivered whole — still strictly better than the blast-loss → freeze →
/// recovery-IDR round trip it replaces.
pub(crate) const MAX_PACE_SPREAD: Duration = Duration::from_millis(100);

/// The native plane's pace budget for one frame (pure — unit-tested): with the T1.2 rate cap
/// active, the paced overflow spreads across exactly the time it needs at the pace rate
/// (`cap`, bounded by [`MAX_PACE_SPREAD`]) and is NEVER under-cut by the frame deadline.
///
/// The old schedule took `min(0.9 × time-to-deadline, cap)`. For a steady-state frame the cap
/// is the smaller term and nothing changes. But for an OVERSIZED frame — a stall-resume scene
/// delta after seconds of frozen composition, a cold IDR — the overflow needs SEVERAL frame
/// intervals at the pace rate, and the deadline term clamped that into the remainder of ONE:
/// an instantaneous many-×-stream-rate blast that overruns the socket tx-buffer and loses the
/// very frame that would have ended the freeze (field fingerprint: WSAENOBUFS 10055 +
/// `loss_ppm` spikes at capture-stall edges, then a recovery-IDR round trip per retry). The
/// pace rate is ~3× a rate the link demonstrably carries, so holding it past the deadline is
/// safe by the same argument that introduced the cap — the deadline stays a *target*, not a
/// license to blast.
///
/// `pace_rate_bps == 0` (PUNKTFUNK_PACE_FACTOR=0) or an overflow-free frame keeps the legacy
/// deadline-only spread.
///
/// `max_spread` (ABR overhaul RFC §2.2) bounds one frame's spread to what the encode|send
/// `sync_channel(3)` can absorb — the caller passes ~2 frame intervals. A spread past that
/// backs the channel up into `cadence_degraded`, and the host then refuses every climb; a
/// shorter budget over the same overflow IS the lifted effective rate the RFC asks for.
/// Pass [`MAX_PACE_SPREAD`] when the interval is unknown.
pub(crate) fn native_budget(
    deadline: Instant,
    pace_rate_bps: u64,
    overflow_bytes: u64,
    max_spread: Duration,
) -> PaceBudget {
    if pace_rate_bps > 0 && overflow_bytes > 0 {
        let cap = Duration::from_nanos(
            (overflow_bytes * 8).saturating_mul(1_000_000_000) / pace_rate_bps,
        );
        PaceBudget::Fixed(cap.min(max_spread).min(MAX_PACE_SPREAD))
    } else {
        PaceBudget::UntilDeadline {
            deadline,
            fraction: 0.9,
            cap: Duration::MAX,
        }
    }
}

/// The native plane's automatic microburst allowance (pure — unit-tested): the bytes that may
/// leave unpaced before the spread starts, sized in TIME at the pace rate (ABR overhaul RFC
/// §2.1) — "what the link drains in ≤10 ms" — instead of the old absolute
/// `max(128 KiB, wire/4)`, which was sized for gigabit LAN: at Wi-Fi bitrates every frame sat
/// under it, so the whole packet train went out back-to-back and the first motion frame after
/// a static stretch was lost to the burst (the 2026-08-26 field case; `PACE_BURST_KB=16` was
/// its discriminator, and the 16 KiB clamp floor below is exactly that value).
///
/// One constant lines up both known-good ends: 5 Mbps stream (~15 Mbps pace) → ~19 KiB ≈ the
/// field discriminator; 30 Mbps LAN (~90 Mbps pace) → ~112 KiB ≈ the old 128 KiB floor, so
/// LAN latency does not regress; ≥205 Mbps pace clamps at 256 KiB and the rest rides the
/// 3×-rate spread. `pace_rate_bps == 0` (PUNKTFUNK_PACE_FACTOR=0 — pacing off) keeps the
/// legacy fraction-of-frame burst.
pub(crate) fn auto_burst_bytes(pace_rate_bps: u64, wire_bytes: usize) -> usize {
    const BURST_MS: u64 = 10;
    const BURST_MIN: usize = 16 * 1024;
    const BURST_MAX: usize = 256 * 1024;
    if pace_rate_bps == 0 {
        return (wire_bytes / 4).max(128 * 1024);
    }
    usize::try_from(pace_rate_bps * BURST_MS / 8000)
        .unwrap_or(BURST_MAX)
        .clamp(BURST_MIN, BURST_MAX)
}

/// Per-plane pacing parameters. See the module doc for the two canonical values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PaceCfg {
    /// Bytes that leave immediately as one absorbed microburst before pacing starts; `None` =
    /// no burst stage at all (GameStream). `Some(0)` still bursts the first packet — the split
    /// is "the packet that crosses the cap goes with the burst", exactly the native semantics.
    pub(crate) burst_bytes: Option<usize>,
    pub(crate) chunk: ChunkPolicy,
    /// Sleeps shorter than this are skipped (scheduler-jitter floor; both planes: 500 µs).
    pub(crate) sleep_floor: Duration,
}

/// A frame's send schedule, computed up front as pure data (what the deterministic tests pin):
/// packets `[0..burst_len)` go immediately in `chunk`-sized bursts; the rest go in `steps`
/// chunks of `chunk`, chunk `j` (0-based) sleeping toward `budget × (j+1)/steps`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaceSchedule {
    pub(crate) burst_len: usize,
    pub(crate) chunk: usize,
    pub(crate) steps: usize,
}

/// Compute the schedule for one frame's wire packets under `cfg`. `pace_budget` is the time
/// the paced overflow will spread across (resolved by the caller); only
/// [`ChunkPolicy::Adaptive`] reads it — the `Fixed`/`Bounded` schedules are budget-independent
/// (the pinned legacy planes).
pub(crate) fn schedule<T: AsRef<[u8]>>(
    packets: &[T],
    cfg: &PaceCfg,
    pace_budget: Duration,
) -> PaceSchedule {
    let burst_len = match cfg.burst_bytes {
        None => 0,
        Some(cap) => {
            // The packet that crosses the cap still bursts (`split = k + 1`) — the whole frame
            // bursts when it never crosses it.
            let mut cum = 0usize;
            let mut split = packets.len();
            for (k, p) in packets.iter().enumerate() {
                cum += p.as_ref().len();
                if cum >= cap {
                    split = k + 1;
                    break;
                }
            }
            split
        }
    };
    let overflow = packets.len() - burst_len;
    let (chunk, steps) = match cfg.chunk {
        ChunkPolicy::Fixed(c) => (c, overflow.div_ceil(c).max(1)),
        ChunkPolicy::Adaptive { base, max } => {
            let c = if overflow == 0 {
                base
            } else if pace_budget.is_zero() {
                max
            } else {
                // interval = budget/steps ≈ budget·c/overflow ≥ sleep_floor ⇔
                // c ≥ overflow·floor/budget — the smallest such c, clamped to [base, max].
                let c_min = (overflow as u128 * cfg.sleep_floor.as_nanos())
                    .div_ceil(pace_budget.as_nanos());
                c_min.clamp(base as u128, max as u128) as usize
            };
            (c, overflow.div_ceil(c).max(1))
        }
        ChunkPolicy::Bounded {
            min_chunk,
            max_steps,
        } => {
            let c = min_chunk.max(overflow.div_ceil(max_steps));
            (c, overflow.div_ceil(c).max(1))
        }
    };
    PaceSchedule {
        burst_len,
        chunk,
        steps,
    }
}

/// Send one frame's packets under the plane's pacing policy: the burst stage leaves
/// immediately, then each paced chunk is sent and slept toward its slice of the budget
/// (sub-`sleep_floor` waits are skipped). A `send` error aborts the frame and propagates —
/// the native plane bails the session, GameStream stops the stream.
pub(crate) fn pace_frame<T: AsRef<[u8]>, E>(
    packets: &[T],
    budget: PaceBudget,
    cfg: &PaceCfg,
    mut send: impl FnMut(&[T]) -> Result<(), E>,
) -> Result<PaceStat, E> {
    let start = Instant::now();
    // Resolve the pace budget up front: adaptive chunk sizing needs it before the burst
    // leaves. The paced loop below still re-anchors at `pace_start` (after the burst), so the
    // sleep targets are exactly the legacy math; this entry-time estimate only sizes chunks
    // (it overshoots the post-burst budget by the burst's few µs — harmless, sub-floor sleeps
    // are skipped anyway).
    let budget_est = match budget {
        PaceBudget::UntilDeadline {
            deadline,
            fraction,
            cap,
        } => deadline
            .checked_duration_since(start)
            .unwrap_or_default()
            .mul_f32(fraction)
            .min(cap),
        PaceBudget::Fixed(d) => d,
    };
    let sched = schedule(packets, cfg, budget_est);
    for chunk in packets[..sched.burst_len].chunks(sched.chunk) {
        send(chunk)?;
    }
    let paced = sched.burst_len < packets.len();
    if paced {
        let pace_start = Instant::now();
        let budget = match budget {
            PaceBudget::UntilDeadline {
                deadline,
                fraction,
                cap,
            } => deadline
                .checked_duration_since(pace_start)
                .unwrap_or_default()
                .mul_f32(fraction)
                .min(cap),
            PaceBudget::Fixed(d) => d,
        };
        for (j, chunk) in packets[sched.burst_len..].chunks(sched.chunk).enumerate() {
            send(chunk)?;
            // Sleep toward this chunk's slice of the budget; skip sub-floor waits (jitter).
            let target = pace_start + budget.mul_f64((j + 1) as f64 / sched.steps as f64);
            if let Some(ahead) = target.checked_duration_since(Instant::now()) {
                if ahead >= cfg.sleep_floor {
                    std::thread::sleep(ahead);
                }
            }
        }
    }
    Ok(PaceStat {
        spread_us: start.elapsed().as_micros() as u32,
        paced,
    })
}

/// T1.1 frame-driven encode trigger (latency plan): `PUNKTFUNK_FRAME_DRIVEN=0` restores the
/// legacy fixed-cadence tick everywhere (backends without an arrival wait keep it regardless —
/// see [`pf_capture::Capturer::supports_arrival_wait`]). Shared by both video planes: the
/// native loop and the GameStream loop key their arrival-driven capture on the same knob.
pub(crate) fn frame_driven_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PUNKTFUNK_FRAME_DRIVEN").as_deref() != Ok("0"))
}

/// Wire-rate credit bucket for the frame-driven (arrival-wait) capture trigger, shared by both
/// video planes (moved here from the native stream loop when the GameStream loop adopted T1.1).
///
/// The floor alone (0.9 × interval between grabs) lets a source that ALWAYS has a frame pending
/// — a display overdriven by `PUNKTFUNK_VDISPLAY_HZ_MULT`, uncapped content, a mirrored panel
/// running faster than the negotiated rate — settle at 0.9-interval spacing: 1.11× the
/// negotiated rate on the wire (field report: 132 fps on a 120 fps session, frames the client's
/// 120 Hz panel can only drop). Credit accrues at one frame per interval of real elapsed time
/// (capped at [`Self::CAP`]) and every submitted frame spends one; a grab may run early only
/// against banked credit, so per-gap jitter still passes while the average cannot exceed the
/// pacing rate. A source at or below the pacing rate banks credit faster than it spends and is
/// never delayed.
pub(crate) struct CaptureCredit {
    /// Banked frames, in `[-1.0, CAP]`. Transiently dips below 0 when a grab spent credit it had
    /// only partly banked; the owed fraction is repaid before the next grab.
    credit: f32,
    /// When credit last accrued (the previous [`Self::earliest`] call).
    last: Instant,
}

impl CaptureCredit {
    /// Burst allowance: at most this many frames may follow a stall back-to-back before the
    /// bucket re-gates. One frame of instant catch-up plus the floor's own headroom.
    pub(crate) const CAP: f32 = 1.25;

    pub(crate) fn new(now: Instant) -> CaptureCredit {
        CaptureCredit {
            credit: Self::CAP,
            last: now,
        }
    }

    /// Accrue the elapsed credit and return the earliest instant the next grab may run: `now`
    /// once a full frame is banked, else the missing fraction of an interval out.
    pub(crate) fn earliest(&mut self, now: Instant, interval: Duration) -> Instant {
        let secs = interval.as_secs_f32();
        if secs > 0.0 {
            let accrued = now.duration_since(self.last).as_secs_f32() / secs;
            self.credit = (self.credit + accrued).min(Self::CAP);
        }
        self.last = now;
        now + interval.mul_f32((1.0 - self.credit).max(0.0))
    }

    /// One frame submitted — spend its credit.
    pub(crate) fn charge(&mut self) {
        self.credit -= 1.0;
    }
}

/// Parsed-once `PUNKTFUNK_VIDEO_DROP` percentage (1..=90, anything else = off): discard N % of
/// the sealed wire packets before send — controlled loss injection with no netem/root, honored
/// by BOTH video planes. Warned once on activation.
pub(crate) fn video_drop_pct() -> u32 {
    static PCT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *PCT.get_or_init(|| {
        let pct = std::env::var("PUNKTFUNK_VIDEO_DROP")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| (1..=90).contains(p))
            .unwrap_or(0);
        if pct > 0 {
            tracing::warn!(
                pct,
                "PUNKTFUNK_VIDEO_DROP: injecting wire-packet loss (FEC test)"
            );
        }
        pct
    })
}

/// Apply the [`video_drop_pct`] loss injection to one frame's wire packets, returning how many
/// were discarded (0 when the knob is off — the normal path is untouched).
pub(crate) fn inject_video_drop<T>(packets: &mut Vec<T>) -> u64 {
    let pct = video_drop_pct();
    if pct == 0 {
        return 0;
    }
    use rand::Rng;
    let mut rng = rand::rng();
    let before = packets.len();
    packets.retain(|_| rng.random_range(0..100) >= pct);
    (before - packets.len()) as u64
}

/// Percentile of a slice (sorts it in place first). `q` in `0.0..=1.0`. Used for the
/// PUNKTFUNK_PERF histograms and the web-console stats sample's per-stage p50/p99.
pub(crate) fn percentile(v: &mut [u32], q: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * q) as usize).min(v.len() - 1);
    v[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native plane's canonical parameters (mirrors `native::paced_submit`).
    fn native_cfg(burst_cap: usize) -> PaceCfg {
        PaceCfg {
            burst_bytes: Some(burst_cap),
            chunk: ChunkPolicy::Fixed(16),
            sleep_floor: Duration::from_micros(500),
        }
    }

    /// The GameStream plane's canonical parameters (mirrors `gamestream::stream::spawn_sender`,
    /// post-WP1.2): an auto-sized microburst plus BOUNDED overflow chunking.
    fn gs_cfg(burst_cap: usize) -> PaceCfg {
        PaceCfg {
            burst_bytes: Some(burst_cap),
            chunk: ChunkPolicy::Bounded {
                min_chunk: 16,
                max_steps: 12,
            },
            sleep_floor: Duration::from_micros(500),
        }
    }

    fn packets(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n).map(|_| vec![0u8; len]).collect()
    }

    /// Deterministic-schedule pin, native plane: burst split + chunking + step count must
    /// reproduce the legacy `paced_submit` math exactly — `split = first k with cum ≥ cap + 1`
    /// (whole frame if never crossed), fixed 16-packet chunks, `m = ceil(overflow/16).max(1)`.
    #[test]
    fn native_schedule_matches_legacy_paced_submit() {
        let legacy = |sizes: &[usize], burst_cap: usize| -> (usize, usize) {
            // Verbatim transcription of the pre-dedup split + step-count computation.
            let mut cum = 0usize;
            let mut split = sizes.len();
            for (k, len) in sizes.iter().enumerate() {
                cum += len;
                if cum >= burst_cap {
                    split = k + 1;
                    break;
                }
            }
            let m = (sizes.len() - split).div_ceil(16).max(1);
            (split, m)
        };
        for (n, len, cap) in [
            (1usize, 1200usize, 128 * 1024usize), // tiny frame ≪ cap → all burst
            (109, 1200, 128 * 1024),              // exactly at the cap boundary region
            (110, 1200, 128 * 1024),              // one past
            (600, 1200, 128 * 1024),              // 4K P-frame: burst + paced overflow
            (3300, 1200, 128 * 1024),             // multi-MB IDR
            (600, 1200, 0),                       // cap 0: first packet still bursts
            (0, 1200, 128 * 1024),                // empty (post-drop-injection) frame
        ] {
            let pkts = packets(n, len);
            let sizes: Vec<usize> = pkts.iter().map(|p| p.len()).collect();
            let (split, m) = legacy(&sizes, cap);
            // Two very different budgets: Fixed schedules must not read the budget at all.
            for budget in [Duration::ZERO, Duration::from_millis(7)] {
                let s = schedule(&pkts, &native_cfg(cap), budget);
                assert_eq!(s.burst_len, split, "n={n} cap={cap}: burst split");
                assert_eq!(s.chunk, 16, "n={n} cap={cap}: chunk size");
                assert_eq!(s.steps, m, "n={n} cap={cap}: paced step count");
            }
        }
    }

    /// Deterministic-schedule pin, GameStream plane (post-WP1.2): the burst split follows the
    /// native rule (the packet crossing the cap still bursts; a frame under the cap bursts
    /// whole — the ~0-latency fast path for normal frames), and the OVERFLOW keeps the bounded
    /// legacy layout (chunk = max(16, ceil(overflow/12)), ≤ 12 steps).
    #[test]
    fn gamestream_schedule_bursts_then_bounds_the_overflow() {
        // The canonical stream: 20 Mbps × 3 pace → auto burst 75 000 B (10 ms at the rate).
        let cap = auto_burst_bytes(60_000_000, 0);
        assert_eq!(cap, 75_000);
        // A steady-state 60 fps frame at 20 Mbps (~42 KB, ~35 packets): entirely under the
        // burst cap → the whole frame leaves immediately, nothing is paced.
        let pkts = packets(35, 1200);
        for budget in [Duration::ZERO, Duration::from_millis(7)] {
            let s = schedule(&pkts, &gs_cfg(cap), budget);
            assert_eq!(s.burst_len, 35, "steady-state frame bursts whole");
        }
        // An IDR (~600 KB, 500 packets): 63 packets burst (cum crosses 75 000 at #63), the
        // 437-packet overflow spreads in the bounded layout.
        let pkts = packets(500, 1200);
        for budget in [Duration::ZERO, Duration::from_millis(7)] {
            let s = schedule(&pkts, &gs_cfg(cap), budget);
            assert_eq!(s.burst_len, 63, "burst split at the cap crossing");
            let overflow: usize = 500 - 63;
            let chunk = 16usize.max(overflow.div_ceil(12));
            assert_eq!(s.chunk, chunk, "overflow keeps the bounded chunking");
            assert_eq!(s.steps, overflow.div_ceil(chunk));
            assert!(s.steps <= 12, "step count stays bounded");
            assert!(s.chunk >= 16, "chunk floor");
        }
        // The bounded layout invariants across sizes (the old test's historical bounds).
        for &n in &[64usize, 146, 610, 5000, 50_000] {
            let pkts = packets(n, 1200);
            let s = schedule(&pkts, &gs_cfg(cap), Duration::ZERO);
            let overflow = n - s.burst_len;
            assert!(s.steps <= 12, "n={n}: step count bounded");
            assert!(s.chunk >= 16, "n={n}: chunk floor");
            assert!(
                s.chunk * s.steps >= overflow,
                "n={n}: layout covers the overflow"
            );
        }
    }

    /// The native plane's Phase-1.2 policy (plan `throughput-beyond-1gbps.md`): 16-packet
    /// chunks at today's rates, coarsening only when the per-chunk interval would drop under
    /// the 500 µs sleep floor, capped at the 64-segment GSO super-buffer limit; zero budget
    /// (blast) takes the cap.
    #[test]
    fn adaptive_chunk_coarsens_with_rate() {
        let cfg = PaceCfg {
            burst_bytes: Some(12_000),
            chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
            sleep_floor: Duration::from_micros(500),
        };
        // 210 × 1200 B: packets 0..=9 burst (cum hits 12 000 at #10), 200 overflow.
        let pkts = packets(210, 1200);
        // Ample budget (100 ms): a 16-packet interval is ≫ floor → base, legacy-identical.
        let s = schedule(&pkts, &cfg, Duration::from_millis(100));
        assert_eq!((s.burst_len, s.chunk, s.steps), (10, 16, 13));
        // 2.5 ms budget: c ≥ 200 × 500 µs / 2.5 ms = 40 → exactly 40, 5 steps × 500 µs each.
        let s = schedule(&pkts, &cfg, Duration::from_micros(2_500));
        assert_eq!((s.chunk, s.steps), (40, 5));
        // 1 ms budget: c ≥ 100 → capped at 64 (the GSO segment limit).
        let s = schedule(&pkts, &cfg, Duration::from_millis(1));
        assert_eq!((s.chunk, s.steps), (64, 4));
        // Zero budget (no slack — the frame blasts): max chunk = fewest syscalls.
        let s = schedule(&pkts, &cfg, Duration::ZERO);
        assert_eq!((s.chunk, s.steps), (64, 4));
        // Whole frame under the cap: no overflow → base chunk for the burst sends.
        let s = schedule(&packets(5, 1200), &cfg, Duration::ZERO);
        assert_eq!((s.burst_len, s.chunk, s.steps), (5, 16, 1));
    }

    /// The executed chunk sequence follows the schedule exactly, on both parameterizations —
    /// zero budget, so the test never sleeps.
    #[test]
    fn pace_frame_sends_the_scheduled_chunk_sequence() {
        // Native, 40 × 1 KB with a 10 KB cap: packets 0..=9 burst (cum hits 10 KB at #10),
        // then 30 overflow → chunks of 16: [10..26), [26..40).
        let pkts = packets(40, 1024);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &native_cfg(10 * 1024),
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 16, 14]);
        assert!(stat.paced);

        // Native, frame under the cap: one immediate burst (chunked at 16), nothing paced.
        let pkts = packets(20, 100);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &native_cfg(128 * 1024),
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![16, 4]);
        assert!(!stat.paced);

        // Native adaptive, zero budget: the burst leaves in one ≤64-packet chunk, the overflow
        // in 64-packet super-chunks (the blast path takes the coarsest syscall batching).
        let pkts = packets(210, 1200);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &PaceCfg {
                burst_bytes: Some(12_000),
                chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
                sleep_floor: Duration::from_micros(500),
            },
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 64, 64, 64, 8]);
        assert!(stat.paced);

        // GameStream, 146 × 1 KB with a 16 KB burst cap: packets 0..=15 burst (cum hits 16 KB
        // at #16, sent as one 16-packet chunk), then 130 overflow → bounded chunks of
        // max(16, ceil(130/12)=11) = 16: 9 chunks (8 × 16 + 2).
        let pkts = packets(146, 1024);
        let mut seen: Vec<usize> = Vec::new();
        pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &gs_cfg(16 * 1024),
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 10);
        assert_eq!(seen.iter().sum::<usize>(), 146);
        assert!(seen[..9].iter().all(|&c| c == 16));
        assert_eq!(*seen.last().unwrap(), 2);

        // A send error aborts the frame and propagates.
        let pkts = packets(64, 1024);
        let mut calls = 0;
        let r = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &gs_cfg(16 * 1024),
            |_chunk| {
                calls += 1;
                if calls == 2 {
                    Err(std::io::Error::other("client gone"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(r.is_err());
        assert_eq!(calls, 2, "no sends after the failing chunk");
    }

    /// The sleep targets are each paced chunk's fraction of the budget — pinned against the
    /// legacy formulas of both planes (native: `budget×(j+1)/m` directly; GameStream:
    /// `(budget×1/steps)×(i+1)`, which agrees to sub-step-count nanoseconds).
    #[test]
    fn sleep_targets_match_legacy_formulas() {
        let budget = Duration::from_micros(12_500); // GS: ¾ of a 60 Hz frame interval
        for steps in [1usize, 2, 10, 12] {
            for j in 0..steps {
                let unified = budget.mul_f64((j + 1) as f64 / steps as f64);
                // Native legacy: one fused fraction — identical expression.
                assert_eq!(unified, budget.mul_f64((j + 1) as f64 / steps as f64));
                // GameStream legacy: per_step rounds to ns first; ≤ steps/2 ns apart.
                let gs_legacy = budget.mul_f64(1.0 / steps as f64).mul_f64((j + 1) as f64);
                let diff = unified.abs_diff(gs_legacy);
                assert!(
                    diff <= Duration::from_nanos(steps as u64),
                    "steps={steps} j={j}: {diff:?} off legacy"
                );
            }
        }
    }

    /// The T1.2 rate cap bounds an `UntilDeadline` budget from above: with ample deadline
    /// slack the cap decides the spread (and therefore the adaptive chunk sizing); a
    /// `Duration::MAX` cap reproduces the legacy deadline-only schedule exactly.
    #[test]
    fn until_deadline_cap_bounds_the_budget() {
        let cfg = PaceCfg {
            burst_bytes: Some(12_000),
            chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
            sleep_floor: Duration::from_micros(500),
        };
        // 210 × 1200 B: 10 burst, 200 overflow (the adaptive test's canonical frame).
        let pkts = packets(210, 1200);

        // Zero cap + far deadline: the budget collapses to 0 → blast schedule (max chunks,
        // no sleeps) even though the deadline alone would have spread ~90 ms.
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now() + Duration::from_millis(100),
                fraction: 0.9,
                cap: Duration::ZERO,
            },
            &cfg,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 64, 64, 64, 8], "zero cap = blast schedule");
        assert!(stat.paced);
        assert!(
            stat.spread_us < 50_000,
            "zero cap must not sleep toward the deadline"
        );

        // A 2.5 ms cap under a ~90 ms deadline budget: the cap sizes the chunks
        // (c ≥ 200 × 500 µs / 2.5 ms = 40) and the frame drains in ~2.5 ms, not ~90.
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now() + Duration::from_millis(100),
                fraction: 0.9,
                cap: Duration::from_micros(2_500),
            },
            &cfg,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![10, 40, 40, 40, 40, 40],
            "cap drives chunk sizing"
        );
        assert!(
            stat.spread_us < 50_000,
            "capped spread must be ~2.5 ms, nowhere near the 90 ms deadline budget"
        );

        // MAX cap = legacy: no-slack deadline still collapses to the blast path.
        let mut seen: Vec<usize> = Vec::new();
        pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now(),
                fraction: 0.9,
                cap: Duration::MAX,
            },
            &cfg,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![10, 64, 64, 64, 8],
            "MAX cap = legacy no-slack blast"
        );
    }

    /// [`native_budget`]: with the rate cap active the budget is the overflow's wire time at
    /// the pace rate — a FIXED spread the deadline can no longer under-cut — bounded by
    /// [`MAX_PACE_SPREAD`]; rate 0 / no overflow keep the legacy deadline-only schedule.
    #[test]
    fn native_budget_is_rate_bound_never_deadline_cut() {
        // The stall-resume case the fix exists for: a 3 MB overflow at 3×240 Mbps needs
        // ~33 ms — an IMMINENT deadline (the old min() made this a blast) must not shrink it.
        let deadline = Instant::now() + Duration::from_millis(4); // 240 fps interval
        let b = native_budget(deadline, 720_000_000, 3_000_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_nanos(33_333_333)));

        // A steady-state frame: overflow 90 KB at 3×240 Mbps = 1 ms — identical to what the
        // old min(slack, cap) chose (cap was the smaller term), so nothing regresses.
        let b = native_budget(deadline, 720_000_000, 90_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_micros(1_000)));

        // A crater-rate resume (ABR backed off to 20 Mbps, pace 60 Mbps): the raw rate math
        // says 400 ms for 3 MB — the absolute ceiling bounds the send thread's stall.
        let b = native_budget(deadline, 60_000_000, 3_000_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(MAX_PACE_SPREAD));

        // Rate cap off (PUNKTFUNK_PACE_FACTOR=0): the legacy deadline-only spread, uncapped.
        let b = native_budget(deadline, 0, 3_000_000, MAX_PACE_SPREAD);
        assert!(matches!(
            b,
            PaceBudget::UntilDeadline {
                fraction,
                cap: Duration::MAX,
                ..
            } if fraction == 0.9
        ));

        // No overflow (the whole frame bursts): budget is never consulted — legacy shape.
        let b = native_budget(deadline, 720_000_000, 0, MAX_PACE_SPREAD);
        assert!(matches!(b, PaceBudget::UntilDeadline { .. }));
    }

    /// [`native_budget`] with the RFC §2.2 spread cap: a paced IDR must not park the send
    /// thread past ~2 frame intervals (encode|send `sync_channel(3)` backpressure →
    /// `cadence_degraded` → the host refuses every climb) — the shorter budget over the same
    /// overflow IS the lifted effective rate.
    #[test]
    fn native_budget_spread_capped_to_frame_intervals() {
        let deadline = Instant::now() + Duration::from_millis(8);
        // A 3 MB IDR at 60 Mbps pace wants 400 ms; two 120 Hz intervals (16.6 ms) win.
        let two_intervals = Duration::from_nanos(2 * 8_333_333);
        let b = native_budget(deadline, 60_000_000, 3_000_000, two_intervals);
        assert_eq!(b, PaceBudget::Fixed(two_intervals));

        // A steady-state frame under the cap is untouched by it.
        let b = native_budget(deadline, 720_000_000, 90_000, two_intervals);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_micros(1_000)));

        // The absolute ceiling still binds when the interval cap is the larger one
        // (a 5 fps virtual mode must not re-license a 400 ms stall).
        let b = native_budget(deadline, 60_000_000, 3_000_000, Duration::from_millis(400));
        assert_eq!(b, PaceBudget::Fixed(MAX_PACE_SPREAD));
    }

    /// [`auto_burst_bytes`] (RFC §2.1): the unpaced allowance is 10 ms at the pace rate,
    /// clamped to [16 KiB, 256 KiB] — the constants that line up the Wi-Fi field
    /// discriminator on one end and the old LAN behavior on the other.
    #[test]
    fn auto_burst_is_time_at_pace_rate() {
        // 5 Mbps stream × 3 = 15 Mbps pace → 18.75 KiB: the Wi-Fi field case, where the old
        // 128 KiB floor swallowed every frame whole and the motion-onset burst was lost.
        assert_eq!(auto_burst_bytes(15_000_000, 40_000), 18_750);
        // 30 Mbps LAN × 3 = 90 Mbps → ~112 KiB ≈ the old 128 KiB floor: no LAN regression.
        assert_eq!(auto_burst_bytes(90_000_000, 250_000), 112_500);
        // Below ~13 Mbps pace the floor is the field-proven 16 KiB.
        assert_eq!(auto_burst_bytes(3_000_000, 10_000), 16 * 1024);
        // Gigabit-class pace clamps at 256 KiB — the rest rides the 3×-rate spread.
        assert_eq!(auto_burst_bytes(3_000_000_000, 4_000_000), 256 * 1024);
        // PUNKTFUNK_PACE_FACTOR=0 (pacing off): the legacy fraction-of-frame burst.
        assert_eq!(auto_burst_bytes(0, 4_000_000), 1_000_000);
        assert_eq!(auto_burst_bytes(0, 40_000), 128 * 1024);
    }

    /// `inject_video_drop` is a no-op when the knob is off (the default test env).
    #[test]
    fn drop_injection_off_by_default() {
        let mut pkts = packets(100, 64);
        assert_eq!(inject_video_drop(&mut pkts), 0);
        assert_eq!(pkts.len(), 100);
    }

    /// Drive [`CaptureCredit`] against a source that ALWAYS has a frame pending (the overdriven
    /// display + uncapped content case): each cycle grabs the instant the gate opens, the next
    /// `earliest` call runs right after (encode folded into the wait, like the loop). Returns the
    /// grab instants.
    fn grab_saturated(
        b: &mut CaptureCredit,
        start: Instant,
        interval: Duration,
        n: usize,
    ) -> Vec<Instant> {
        let mut now = start;
        let mut grabs = Vec::with_capacity(n);
        for _ in 0..n {
            let gate = b.earliest(now, interval);
            let grab = gate.max(now);
            b.charge();
            grabs.push(grab);
            now = grab;
        }
        grabs
    }

    #[test]
    fn capture_credit_pins_a_saturated_source_at_the_interval() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        let grabs = grab_saturated(&mut b, t0, interval, 120);
        // Whatever the initial credit bought, the total may exceed the on-rate schedule by at
        // most the burst cap — 120 grabs span no less than (120 - 1 - CAP) intervals.
        let span = grabs[119].duration_since(grabs[0]);
        assert!(
            span >= interval.mul_f32(120.0 - 1.0 - CaptureCredit::CAP),
            "span {span:?} admits more than CAP frames of overshoot"
        );
        // And the steady state is EXACTLY the interval: past the warmup, consecutive grabs are
        // one interval apart (not 0.9 — the 132-fps bug).
        for w in grabs[20..].windows(2) {
            let gap = w[1].duration_since(w[0]);
            assert!(
                gap >= interval.mul_f32(0.999) && gap <= interval.mul_f32(1.001),
                "steady-state gap {gap:?} != interval {interval:?}"
            );
        }
    }

    #[test]
    fn capture_credit_never_delays_an_on_rate_or_slow_source() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        // A source at half the pacing rate (a 60 fps game on a 120 fps session): every arrival
        // banks two frames of credit and spends one — the gate is always already open.
        let mut now = t0;
        for _ in 0..50 {
            now += interval * 2;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "slow source must not be gated"
            );
            b.charge();
        }
        // Exactly on-rate: still never gated (credit hovers at the cap, never below 1).
        let mut b = CaptureCredit::new(t0);
        let mut now = t0;
        for _ in 0..50 {
            now += interval;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "on-rate source must not be gated"
            );
            b.charge();
        }
    }

    #[test]
    fn capture_credit_burst_after_a_stall_is_capped() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        // Settle into the gated steady state, then stall the source for 10 intervals.
        let grabs = grab_saturated(&mut b, t0, interval, 20);
        let stall_end = grabs[19] + interval * 10;
        // However long the stall, the recovery may run ahead of the on-rate schedule by at most
        // CAP frames: the second post-stall grab is already re-gated.
        let after = grab_saturated(&mut b, stall_end, interval, 3);
        assert_eq!(after[0], stall_end, "first post-stall grab is immediate");
        assert!(
            after[1].duration_since(after[0]) >= interval.mul_f32(2.0 - CaptureCredit::CAP),
            "second post-stall grab spent more than the burst cap"
        );
        assert!(
            after[2].duration_since(after[1]) >= interval.mul_f32(0.999),
            "third post-stall grab must be back on the interval grid"
        );
    }

    #[test]
    fn percentile_picks_expected_ranks() {
        let mut v = vec![90, 10, 50, 70, 30];
        assert_eq!(percentile(&mut v, 0.0), 10);
        assert_eq!(percentile(&mut v, 0.5), 50);
        assert_eq!(percentile(&mut v, 0.99), 90);
        assert_eq!(percentile(&mut [], 0.5), 0);
    }
}
