//! Per-pad motion inter-arrival statistics — the measurement a "gyro feels floaty" report needs.
//!
//! Gyro aim integrates angular velocity over time, so what a player feels as floaty or jumpy is
//! usually not the samples' values but their spacing: a 250 Hz feed arriving in 40 ms clumps
//! integrates the same total rotation in visibly worse steps. This is the one number that
//! distinguishes "the client stopped sending" from "the link is clumping them" from "we are fine
//! and the problem is elsewhere", and it is cheap enough to keep on always.
//!
//! Two things were wrong with the version this replaces. It kept ONE global accumulator, so two
//! motion-capable pads in a session interleaved into each other's inter-arrival gaps and produced
//! a number that described neither. And it lived at `debug` behind a `tracing::enabled!` check, so
//! a field report arrived with nothing in it and the only way to get the measurement was to ask
//! the user to reproduce with debug logging on.
//!
//! Cost, since it now runs unconditionally: one `Instant` subtraction and one array increment per
//! motion sample. Percentiles come out of a fixed log2 histogram rather than a growing sorted Vec
//! — no allocation, no per-window sort, and no way for a client that streams motion as fast as the
//! link allows to make the instrument expensive. The price is resolution: a reported percentile is
//! the upper bound of its bucket (hence the `_le` suffixes), which is a factor-of-two answer to a
//! question — 4 ms or 40 ms? — whose answers are orders of magnitude apart.

use punktfunk_core::input::MAX_PADS;
use std::time::{Duration, Instant};

/// Log2 buckets over the inter-arrival gap in microseconds: bucket `k` holds gaps in
/// `[2^(k-1), 2^k)` µs, with bucket 0 holding a gap of 0. 22 buckets reach ~2.1 s, past which a
/// gap says "the feed stopped", not "the feed is uneven", and the top bucket saturates.
const BUCKETS: usize = 22;

/// Gaps at or above this are not cadence, they are an interruption — a client that backgrounded,
/// a link that stalled, a session that idled. Counting them would drag every percentile toward a
/// number that describes the interruption instead of the stream, so they are tallied separately.
const STALL_GAP: Duration = Duration::from_millis(500);

/// One pad's cadence accumulator.
#[derive(Clone, Copy, Default)]
struct PadCadence {
    last: Option<Instant>,
    hist: [u32; BUCKETS],
    /// Gaps folded into `hist` (so `samples = gaps + 1` when the pad sent anything at all).
    gaps: u64,
    /// Largest gap below [`STALL_GAP`], exactly — the histogram's top bucket is too coarse to
    /// answer "how bad was the worst one".
    max_us: u32,
    /// Gaps at or beyond [`STALL_GAP`]: how many times this pad's feed simply stopped and resumed.
    stalls: u32,
}

impl PadCadence {
    fn record(&mut self, now: Instant) {
        let Some(prev) = self.last.replace(now) else {
            return; // first sample — no gap yet
        };
        let gap = now.saturating_duration_since(prev);
        if gap >= STALL_GAP {
            self.stalls = self.stalls.saturating_add(1);
            return;
        }
        let us = gap.as_micros() as u32;
        self.max_us = self.max_us.max(us);
        self.gaps = self.gaps.saturating_add(1);
        self.hist[bucket(us)] += 1;
    }

    /// The upper bound (µs) of the bucket the `q`-quantile falls in, or `None` if this pad never
    /// produced a gap.
    fn percentile_us_le(&self, q: f64) -> Option<u32> {
        if self.gaps == 0 {
            return None;
        }
        // The rank of the quantile, 1-based: q=0.5 over 4 gaps is the 2nd.
        let want = ((q * self.gaps as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (k, n) in self.hist.iter().enumerate() {
            seen += *n as u64;
            if seen >= want {
                return Some(bucket_upper_us(k));
            }
        }
        Some(bucket_upper_us(BUCKETS - 1))
    }
}

/// The bucket a gap of `us` microseconds falls in — `0` for 0, else `floor(log2(us)) + 1`, capped.
fn bucket(us: u32) -> usize {
    if us == 0 {
        return 0;
    }
    ((32 - us.leading_zeros()) as usize).min(BUCKETS - 1)
}

/// The exclusive upper bound of bucket `k`, in microseconds (bucket 0 is exactly 0).
fn bucket_upper_us(k: usize) -> u32 {
    if k == 0 {
        return 0;
    }
    1u32.checked_shl(k as u32).unwrap_or(u32::MAX)
}

/// Every pad's cadence, keyed by wire index.
pub(super) struct MotionCadence {
    pads: [PadCadence; MAX_PADS],
}

impl MotionCadence {
    pub(super) fn new() -> MotionCadence {
        MotionCadence {
            pads: [PadCadence::default(); MAX_PADS],
        }
    }

    /// Note one `RichInput::Motion` for `pad`, arriving now.
    pub(super) fn record(&mut self, pad: u8, now: Instant) {
        if let Some(p) = self.pads.get_mut(pad as usize) {
            p.record(now);
        }
    }

    /// Log one `info` line per pad that carried motion this session. Called once, when the session
    /// ends — the point at which a field report is being written and the numbers still exist.
    pub(super) fn log_summary(&self) {
        for (i, p) in self.pads.iter().enumerate() {
            if p.gaps == 0 && p.stalls == 0 {
                continue;
            }
            tracing::info!(
                pad = i,
                samples = p.gaps + 1,
                // 0 = no gap was recorded at all (a pad that sent one sample and then stalled).
                gap_p50_us_le = p.percentile_us_le(0.5).unwrap_or(0),
                gap_p95_us_le = p.percentile_us_le(0.95).unwrap_or(0),
                gap_max_us = p.max_us,
                stalls = p.stalls,
                "motion cadence for the session (client gyro inter-arrival; percentiles are \
                 log2-bucket upper bounds, stalls are gaps ≥ 500 ms)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// The bug this module exists to fix: two motion pads used to share one accumulator, so each
    /// one's gaps were measured against the OTHER's arrivals. Here pad 0 arrives every 4 ms and
    /// pad 1 every 40 ms, interleaved — and each must report its own cadence, not the ~2 ms the
    /// merged stream would show.
    #[test]
    fn two_pads_do_not_corrupt_each_others_cadence() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        for i in 0..100u64 {
            c.record(0, at(t0, i * 4));
            if i % 10 == 0 {
                c.record(1, at(t0, i * 4));
            }
        }
        // 4 ms lands in the (2048, 4096] µs bucket; 40 ms in (32768, 65536].
        assert_eq!(c.pads[0].percentile_us_le(0.5), Some(4096));
        assert_eq!(c.pads[1].percentile_us_le(0.5), Some(65536));
        assert_eq!(c.pads[0].gaps, 99);
        assert_eq!(c.pads[1].gaps, 9);
    }

    /// A pad that never sent motion contributes nothing — no line, no gaps.
    #[test]
    fn a_silent_pad_records_nothing() {
        let c = MotionCadence::new();
        assert_eq!(c.pads[3].gaps, 0);
        assert_eq!(c.pads[3].percentile_us_le(0.5), None);
        // One sample is not a gap.
        let mut c = MotionCadence::new();
        c.record(3, Instant::now());
        assert_eq!(c.pads[3].gaps, 0);
    }

    /// An interruption is not cadence: a backgrounded client's multi-second silence must be
    /// counted as a stall rather than dragged through the percentiles, which would otherwise
    /// report a healthy 250 Hz feed as a terrible one.
    #[test]
    fn a_stall_is_counted_separately_from_the_cadence() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        for i in 0..50u64 {
            c.record(0, at(t0, i * 4));
        }
        c.record(0, at(t0, 5_000)); // the client came back after five seconds
        for i in 0..50u64 {
            c.record(0, at(t0, 5_000 + i * 4));
        }
        assert_eq!(c.pads[0].stalls, 1);
        assert_eq!(
            c.pads[0].percentile_us_le(0.95),
            Some(4096),
            "the stall leaked in"
        );
        assert!(c.pads[0].max_us < STALL_GAP.as_micros() as u32);
    }

    /// Percentiles track the tail, which is the half that matters: a feed that is mostly 4 ms but
    /// clumps every tenth sample is exactly the "floaty" report, and p50 alone would hide it.
    #[test]
    fn percentiles_separate_the_body_from_the_tail() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        let mut t = 0u64;
        for i in 0..100u64 {
            t += if i % 10 == 9 { 40 } else { 4 };
            c.record(0, at(t0, t));
        }
        // 90 gaps of 4 ms, 10 of 40 ms: the body is healthy and the tail is not.
        assert_eq!(c.pads[0].percentile_us_le(0.5), Some(4096));
        assert_eq!(c.pads[0].percentile_us_le(0.95), Some(65536));
        assert!((39_000..=41_000).contains(&c.pads[0].max_us));
    }

    /// Bucket `k` holds gaps in `[2^(k-1), 2^k)` µs and reports `2^k`.
    #[test]
    fn bucket_edges() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1); // [1, 2)
        assert_eq!(bucket(2), 2); // [2, 4)
        assert_eq!(bucket(3), 2);
        assert_eq!(bucket(4), 3); // [4, 8)
        assert_eq!(bucket_upper_us(0), 0);
        assert_eq!(bucket_upper_us(1), 2);
        assert_eq!(bucket_upper_us(12), 4096); // 4 ms lands here
        assert_eq!(bucket(u32::MAX), BUCKETS - 1); // saturates rather than wrapping
    }
}
