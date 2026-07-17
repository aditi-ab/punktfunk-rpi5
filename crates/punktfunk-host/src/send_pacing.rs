//! Shared microburst pacing POLICY for the two video send planes (networking-audit deferred
//! plan §5): the native plane (`native::paced_submit`, GSO via the core `Session`) and the
//! GameStream compat plane (`gamestream::stream::spawn_sender`, `sendmmsg` over its own RTP
//! socket). Both spread a frame's packets across a time budget in chunked bursts so a real link
//! doesn't drop the frame as one line-rate burst; the syscall layers stay deliberately separate
//! (different sockets, framing, and error contracts) — this module shares the schedule, not the
//! plumbing.
//!
//! The two planes keep their historical parameterizations exactly (pinned by the
//! deterministic-schedule tests below):
//!
//! * **native** — the first `burst_bytes` leave immediately (one absorbed microburst), only the
//!   overflow is paced across `min(90 % of the time left to the frame deadline, the time the
//!   overflow needs at ~3× the live stream bitrate)` in ADAPTIVE chunks: 16 packets at today's
//!   rates, coarsening just enough that the per-chunk interval clears the sleep floor (≤ 64,
//!   the GSO-segment cap) once the rate would otherwise skip every sleep — so ≥1 Gbps frames
//!   still pace instead of blasting (no slack ⇒ budget 0 ⇒ never slower than unpaced). The
//!   rate cap (latency plan T1.2) front-loads the spread: the link demonstrably carries 1× the
//!   stream rate sustained, so a bounded 3× excursion is safe and a large frame's tail stops
//!   waiting out the whole interval;
//! * **GameStream** — no burst stage; the whole frame spreads across a fixed ¾-frame-interval
//!   budget in a BOUNDED number of steps (≤ 12, chunk ≥ 16), because on that non-RT send thread
//!   every step ends in a `thread::sleep` whose overshoot must stay independent of bitrate
//!   (Moonlight clients are tested against this timing).
//!
//! `PUNKTFUNK_VIDEO_DROP` (the FEC-recovery test knob both planes honor) and the stats
//! `percentile` helper live here too — they were duplicated alongside the pacing.

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
#[derive(Clone, Copy, Debug)]
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
    /// A precomputed fixed budget (GameStream: ¾ of the frame interval).
    Fixed(Duration),
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
    let mut rng = rand::thread_rng();
    let before = packets.len();
    packets.retain(|_| rng.gen_range(0..100) >= pct);
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

    /// The GameStream plane's canonical parameters (mirrors `gamestream::stream::spawn_sender`).
    fn gs_cfg() -> PaceCfg {
        PaceCfg {
            burst_bytes: None,
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

    /// Deterministic-schedule pin, GameStream plane: no burst stage, and the chunk/step layout
    /// must reproduce the legacy `pace_layout` exactly (chunk = max(16, ceil(n/12)), ≤ 12
    /// steps) — including the historical bounds its old unit test asserted.
    #[test]
    fn gamestream_schedule_matches_legacy_pace_layout() {
        let legacy_pace_layout = |n: usize| -> (usize, usize) {
            let chunk_sz = 16usize.max(n.div_ceil(12));
            (chunk_sz, n.div_ceil(chunk_sz))
        };
        for &n in &[1usize, 16, 17, 146, 192, 193, 610, 1024, 5000, 50_000] {
            let pkts = packets(n, 1024);
            let (chunk, steps) = legacy_pace_layout(n);
            // Two very different budgets: Bounded schedules must not read the budget at all.
            for budget in [Duration::ZERO, Duration::from_millis(7)] {
                let s = schedule(&pkts, &gs_cfg(), budget);
                assert_eq!(s.burst_len, 0, "n={n}: GameStream has no burst stage");
                assert_eq!(s.chunk, chunk, "n={n}: chunk size");
                assert_eq!(s.steps, steps, "n={n}: step count");
                assert!(s.steps <= 12, "n={n}: step count bounded");
                assert!(s.chunk >= 16, "n={n}: chunk floor");
                assert!(s.chunk * s.steps >= n, "n={n}: layout covers all packets");
            }
        }
        // The legacy test's exact anchors.
        let s = schedule(&packets(1, 1024), &gs_cfg(), Duration::ZERO);
        assert_eq!((s.chunk, s.steps), (16, 1));
        let s = schedule(&packets(16, 1024), &gs_cfg(), Duration::ZERO);
        assert_eq!((s.chunk, s.steps), (16, 1));
        assert!(schedule(&packets(610, 1024), &gs_cfg(), Duration::ZERO).steps <= 12);
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

        // GameStream, 146 packets: chunk = max(16, ceil(146/12)=13) = 16 → 10 paced chunks.
        let pkts = packets(146, 1024);
        let mut seen: Vec<usize> = Vec::new();
        pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &gs_cfg(),
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
            &gs_cfg(),
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

    /// `inject_video_drop` is a no-op when the knob is off (the default test env).
    #[test]
    fn drop_injection_off_by_default() {
        let mut pkts = packets(100, 64);
        assert_eq!(inject_video_drop(&mut pkts), 0);
        assert_eq!(pkts.len(), 100);
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
