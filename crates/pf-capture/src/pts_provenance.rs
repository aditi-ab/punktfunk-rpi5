//! Where the wire's presentation timestamp actually comes from
//! (design/host-source-stutter-fixes.md, WP-A3 and WP-B3).
//!
//! Every Linux capture publish stamps `pts_ns` with `SystemTime::now()` inside OUR PipeWire
//! process callback — the instant the buffer was DELIVERED to us, not the instant the compositor
//! produced it. On a host whose screencast delivery is jittery, that difference IS the jitter, and
//! it is baked into the timestamps the client eventually plays back from. (The 2026-08-15 Skynet
//! log: 41 phase-lock disengage cycles in 24 minutes, arrival offsets up to a full 120 Hz period,
//! on a session whose transport was provably clean.) The compositor's own `spa_meta_header.pts` is
//! stamped upstream of that delivery, so it *might* be clean — but "might" is the entire question,
//! and the client-side cure (source-timestamp playout) faithfully REPRODUCES whatever jitter the
//! timestamps carry rather than absorbing it. So: measure both clocks in the same window, against
//! each other, before trusting either.
//!
//! Time is passed IN rather than read here, which keeps this pure and lets its tests run on every
//! platform — the same reason `capture_policy` in the host crate is split out.

/// Frames sampled per reporting window. ~34 s at 120 Hz, so a 30 s window is covered without the
/// callback ever reallocating: the vectors are built once at their cap and only ever pushed into
/// while short of it. Allocation on the PipeWire loop thread is what this avoids.
const MAX_SAMPLES: usize = 4096;

/// A rebased compositor stamp further than this from the delivery instant is not a timestamp for
/// this frame: the wrong clock domain, a stale header, or a producer that never fills it in. Fall
/// back to the delivery stamp for that frame, and count it — silently trusting a garbage stamp
/// would put the whole stream's timing on a fiction (risk R3).
const PLAUSIBLE_NS: i64 = 50_000_000;

/// One frame's stamp, and which clock produced it.
pub(crate) struct WirePts {
    pub(crate) pts_ns: u64,
    /// False when this frame fell back to the delivery stamp — the honest per-frame answer, and
    /// the number that says whether B3 is actually doing anything on this host.
    pub(crate) from_header: bool,
}

/// The wire stamp for one frame.
///
/// `hdr_pts_ns` is the compositor's `spa_meta_header.pts`, which PipeWire defines in the graph's
/// clock domain (`CLOCK_MONOTONIC`); `delivery_ns` is realtime-since-epoch, which is the domain
/// the wire and the client's plausibility gate both speak. `rt_minus_mono_ns` carries one into the
/// other. A missing or implausible header stamp yields the delivery stamp — today's behaviour —
/// so a producer that fills in no header is unaffected.
pub(crate) fn wire_pts(
    hdr_pts_ns: Option<i64>,
    delivery_ns: u64,
    rt_minus_mono_ns: i64,
) -> WirePts {
    let fallback = WirePts {
        pts_ns: delivery_ns,
        from_header: false,
    };
    // Producers that have no timestamp write 0 (or leave it negative); neither is a stamp.
    let Some(hdr) = hdr_pts_ns.filter(|&p| p > 0) else {
        return fallback;
    };
    let rebased = hdr.saturating_add(rt_minus_mono_ns);
    if rebased <= 0 || (rebased - delivery_ns as i64).abs() >= PLAUSIBLE_NS {
        return fallback;
    }
    WirePts {
        pts_ns: rebased as u64,
        from_header: true,
    }
}

/// One reporting window of "which clock is cleaner", plus the domain sanity check.
#[derive(Default)]
pub(crate) struct PtsProvenance {
    frames: u64,
    with_hdr: u64,
    /// Intervals between consecutive stamps, ns — one series per clock. THE pair the whole WP
    /// exists to compare: if the compositor's is materially tighter than ours, its stamp is worth
    /// adopting; if both are equally ragged, the compositor is composing irregularly and no
    /// choice of stamp can fix it (risk R7).
    hdr_intervals: Vec<i64>,
    delivery_intervals: Vec<i64>,
    /// `hdr − delivery` per frame. Expected to be huge and roughly CONSTANT (two clock origins);
    /// its variance is the signal, and a wildly varying one means the header is not a per-frame
    /// stamp at all.
    offsets: Vec<i64>,
    prev_hdr: Option<i64>,
    prev_delivery: Option<u64>,
    /// Frames that asked for the header stamp and were refused by the plausibility gate.
    pub(crate) implausible: u64,
}

/// A window's worth, in the units a log line wants.
pub(crate) struct PtsReport {
    pub(crate) frames: u64,
    pub(crate) with_hdr: u64,
    /// Intervals the deviations were computed from. A MAD over eight samples and one over three
    /// thousand deserve different amounts of belief, and the log line should not hide which it is.
    pub(crate) samples: u64,
    /// Median interval between deliveries — the empirical period. Derived rather than taken from
    /// the negotiated refresh on purpose: a median is immune both to a wrong nominal and to the
    /// occasional skipped tick, and a skipped tick is exactly what a fixed nominal would
    /// mis-score as jitter.
    pub(crate) period_us: i64,
    /// Median absolute deviation of each clock's intervals about ITS OWN median — "how ragged is
    /// this clock's cadence", and nothing else. Judging both against one shared centre sounds
    /// tidier and is worse: it folds the period-estimation error, and any genuine rate difference
    /// between two clock sources, into a number that is supposed to be about jitter. A perfectly
    /// regular producer would then report several µs of dispersion it does not have.
    pub(crate) hdr_mad_us: i64,
    pub(crate) delivery_mad_us: i64,
    pub(crate) offset_p50_ms: i64,
    pub(crate) implausible: u64,
}

impl PtsProvenance {
    pub(crate) fn new() -> PtsProvenance {
        PtsProvenance {
            hdr_intervals: Vec::with_capacity(MAX_SAMPLES),
            delivery_intervals: Vec::with_capacity(MAX_SAMPLES),
            offsets: Vec::with_capacity(MAX_SAMPLES),
            ..Default::default()
        }
    }

    /// Fold one frame in. `hdr_pts_ns` is `None` when the buffer carried no usable header, which
    /// also breaks the header interval chain — an interval spanning a frame we could not stamp
    /// would read as one clean long gap rather than the missing measurement it is.
    pub(crate) fn observe(&mut self, hdr_pts_ns: Option<i64>, delivery_ns: u64) {
        self.frames += 1;
        if let Some(prev) = self.prev_delivery {
            push_capped(
                &mut self.delivery_intervals,
                delivery_ns as i64 - prev as i64,
            );
        }
        self.prev_delivery = Some(delivery_ns);

        let Some(hdr) = hdr_pts_ns.filter(|&p| p > 0) else {
            self.prev_hdr = None;
            return;
        };
        self.with_hdr += 1;
        push_capped(&mut self.offsets, hdr - delivery_ns as i64);
        if let Some(prev) = self.prev_hdr {
            push_capped(&mut self.hdr_intervals, hdr - prev);
        }
        self.prev_hdr = Some(hdr);
    }

    /// The window's answer, or `None` if too little arrived to say anything.
    pub(crate) fn report(&mut self) -> Option<PtsReport> {
        if self.delivery_intervals.len() < 8 {
            return None;
        }
        Some(PtsReport {
            frames: self.frames,
            with_hdr: self.with_hdr,
            samples: self.delivery_intervals.len() as u64,
            period_us: median(&mut self.delivery_intervals) / 1_000,
            hdr_mad_us: mad(&mut self.hdr_intervals) / 1_000,
            delivery_mad_us: mad(&mut self.delivery_intervals) / 1_000,
            offset_p50_ms: median(&mut self.offsets) / 1_000_000,
            implausible: self.implausible,
        })
    }

    /// Start a fresh window. The previous stamps survive so the first interval of the new window
    /// is a real measurement rather than a hole.
    pub(crate) fn reset_window(&mut self) {
        self.frames = 0;
        self.with_hdr = 0;
        self.implausible = 0;
        self.hdr_intervals.clear();
        self.delivery_intervals.clear();
        self.offsets.clear();
    }
}

fn push_capped(v: &mut Vec<i64>, x: i64) {
    if v.len() < MAX_SAMPLES {
        v.push(x);
    }
}

/// Median, in place. Empty reports 0 so a log line stays parseable (the caller has already
/// declined to report on a window this thin).
fn median(v: &mut [i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Median absolute deviation about the series' own median — robust to the occasional skipped
/// tick, which a mean would let dominate and a fixed nominal would mis-score as jitter.
fn mad(v: &mut [i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    let centre = median(v);
    for x in v.iter_mut() {
        *x = (*x - centre).abs();
    }
    median(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monotonic clock a few days into an uptime, against a realtime clock in 2026 — the domain
    /// gap the rebase exists to close, and the shape the plausibility gate must not mistake for a
    /// bad stamp.
    const MONO_BASE: i64 = 300_000 * 1_000_000_000;
    const RT_BASE: u64 = 1_786_000_000 * 1_000_000_000;
    const RT_MINUS_MONO: i64 = RT_BASE as i64 - MONO_BASE;

    #[test]
    fn a_rebased_compositor_stamp_is_used_and_a_stale_one_is_not() {
        // In domain and on time: adopted.
        let w = wire_pts(
            Some(MONO_BASE + 1_000_000),
            RT_BASE + 1_200_000,
            RT_MINUS_MONO,
        );
        assert!(w.from_header);
        assert_eq!(
            w.pts_ns,
            RT_BASE + 1_000_000,
            "the compositor's own instant"
        );

        // Half a second stale — a header nobody refreshed. Fall back rather than put the stream's
        // timing on a fiction.
        let w = wire_pts(Some(MONO_BASE - 500_000_000), RT_BASE, RT_MINUS_MONO);
        assert!(!w.from_header);
        assert_eq!(w.pts_ns, RT_BASE);

        // Never stamped at all (0), and the no-header case: today's behaviour, unchanged.
        assert!(!wire_pts(Some(0), RT_BASE, RT_MINUS_MONO).from_header);
        assert_eq!(wire_pts(None, RT_BASE, RT_MINUS_MONO).pts_ns, RT_BASE);
    }

    /// The wrong clock domain is the failure mode risk R3 names, and it must be *loud in the
    /// numbers and silent in the stream*: every frame falls back, nothing is corrupted.
    #[test]
    fn a_raw_monotonic_stamp_never_reaches_the_wire() {
        let w = wire_pts(Some(MONO_BASE), RT_BASE, 0); // rebase forgotten
        assert!(!w.from_header);
        assert_eq!(w.pts_ns, RT_BASE);
    }

    const PERIOD: i64 = 8_333_333; // 120 Hz

    /// Deterministic LCG in ±spread around zero (no OS randomness in tests). Zero-mean noise, not
    /// a short repeating cycle: a cycle's interval series has only a handful of distinct values
    /// and its median lands on one of the jitter peaks rather than on the period — which is a
    /// property of that harness, not of the statistic.
    struct Lcg(u64);
    impl Lcg {
        fn noise(&mut self, spread_ns: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as i64 % (2 * spread_ns)) - spread_ns
        }
    }

    /// The measurement the branch decision rests on: a compositor stamping a clean 120 Hz grid
    /// whose buffers reach us raggedly must show a materially tighter MAD than our delivery
    /// stamps. This is the field shape — arrival offsets wandering up to a full period.
    #[test]
    fn a_clean_producer_behind_a_jittery_delivery_is_visible() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(7);
        for i in 0..600i64 {
            let hdr = MONO_BASE + i * PERIOD; // the producer's own stamp is exact
            let delivery = (RT_BASE as i64 + i * PERIOD + rng.noise(3_000_000)) as u64;
            p.observe(Some(hdr), delivery);
        }
        let r = p.report().expect("600 frames is plenty");
        assert_eq!(r.frames, 600);
        assert_eq!(r.with_hdr, 600);
        assert!(
            (r.period_us - PERIOD / 1_000).abs() <= 100,
            "the empirical period must find the grid, got {} us",
            r.period_us
        );
        assert_eq!(r.hdr_mad_us, 0, "an exact producer has no deviation");
        assert!(
            r.delivery_mad_us > 1_000,
            "delivery jitter of milliseconds must show as milliseconds, got {} us",
            r.delivery_mad_us
        );
        // The domain check: a roughly CONSTANT offset is what "two clock origins" looks like — a
        // varying one would mean the header is not a per-frame stamp at all. (The tolerance is
        // the delivery jitter itself, which the offset carries by construction.)
        let origins_ms = (MONO_BASE - RT_BASE as i64) / 1_000_000;
        assert!(
            (r.offset_p50_ms - origins_ms).abs() <= 5,
            "offset p50 {} vs clock origins {origins_ms}",
            r.offset_p50_ms
        );
    }

    /// Risk R7 asserted: if the compositor composes irregularly rather than merely delivering
    /// late, both clocks are equally ragged and the numbers say so — no stamp swap can help, and
    /// the report must not flatter the header into looking like a cure.
    #[test]
    fn an_irregular_producer_is_not_flattered() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(11);
        for i in 0..600i64 {
            // One wobble, carried faithfully by both clocks: the compositor really did compose
            // at that instant, and really did deliver it straight away.
            let wobble = rng.noise(3_000_000);
            p.observe(
                Some(MONO_BASE + i * PERIOD + wobble),
                (RT_BASE as i64 + i * PERIOD + wobble) as u64,
            );
        }
        let r = p.report().unwrap();
        assert_eq!(
            r.hdr_mad_us, r.delivery_mad_us,
            "an irregular producer must look exactly as bad through either clock"
        );
        assert!(
            r.hdr_mad_us > 1_000,
            "…and both must show the wobble, got {} us",
            r.hdr_mad_us
        );
    }

    /// A producer that fills in no header at all still gets its delivery clock measured, and the
    /// header series must not invent intervals across the frames it could not stamp.
    #[test]
    fn a_producer_without_headers_still_reports_its_delivery_clock() {
        let mut p = PtsProvenance::new();
        for i in 0..40i64 {
            p.observe(None, (RT_BASE as i64 + i * 8_333_333) as u64);
        }
        let r = p.report().unwrap();
        assert_eq!(r.with_hdr, 0);
        assert_eq!(r.hdr_mad_us, 0, "no samples, not a clean clock");
        assert!((r.period_us - 8_333).abs() <= 1);
    }

    /// A new window starts clean but NOT blind: the previous stamps survive the reset, so the
    /// first interval after a report is a real measurement rather than a hole. Getting this wrong
    /// silently drops one frame's interval every 30 s — invisible, and exactly the kind of slow
    /// bias that makes two clocks look more alike than they are.
    #[test]
    fn a_reset_window_keeps_measuring_across_the_boundary() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(13);
        for i in 0..40i64 {
            p.observe(
                Some(MONO_BASE + i * PERIOD),
                (RT_BASE as i64 + i * PERIOD) as u64,
            );
        }
        p.implausible = 3;
        let first = p.report().unwrap();
        assert_eq!(
            first.implausible, 3,
            "the window's fallbacks must be reported"
        );

        p.reset_window();
        for i in 40..80i64 {
            let delivery = (RT_BASE as i64 + i * PERIOD + rng.noise(2_000_000)) as u64;
            p.observe(Some(MONO_BASE + i * PERIOD), delivery);
        }
        let second = p.report().unwrap();
        assert_eq!(second.frames, 40, "counts start over");
        assert_eq!(second.implausible, 0, "…and so do the fallbacks");
        assert_eq!(
            second.samples, 40,
            "40 frames across a boundary yield 40 intervals, not 39 — the chain survived"
        );
        assert_eq!(second.hdr_mad_us, 0, "the exact producer is still exact");
    }

    /// A window too thin to mean anything says nothing rather than reporting noise as a verdict.
    #[test]
    fn a_thin_window_reports_nothing() {
        let mut p = PtsProvenance::new();
        for i in 0..4i64 {
            p.observe(Some(MONO_BASE + i), RT_BASE + i as u64);
        }
        assert!(p.report().is_none());
    }
}
