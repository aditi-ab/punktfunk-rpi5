//! The compose-cadence histogram the drain worker stamps after `FinishedProcessingFrame`, in
//! both pool modes, and reports to the driver log ([`Cadence`]). [`Acquired`] is one frame as
//! the acquire returned it. Pure arithmetic over QPC stamps, apart from the clock frequency
//! read at creation and the report line.

use super::thread::qpc_frequency;

/// One acquired frame, as the drain worker saw it arrive.
pub struct Acquired {
    /// `PresentDisplayQPCTime`: the OS's display time for the frame.
    pub display_qpc: u64,
    /// When the acquire returned it.
    pub at_qpc: u64,
    /// `PresentationFrameNumber`.
    pub number: u32,
    /// It was already waiting: no empty acquire came before it.
    pub queued: bool,
}

/// `PresentDisplayQPCTime` deltas in eighth frame periods: 24 buckets, the last one everything
/// at or over 2.875 periods, plus the run's worst gap. Reported every [`Cadence::REPORT_MS`] to
/// the driver log. This is the only instrument for gate §5-4 ("deltas never exceed 2 frame
/// periods"), so it is unconditional — a spike feature must not be what a shipping gate reads.
/// Bucket 8 opens at exactly one period and bucket 16 at two, so both bars are counts, not
/// interpolations.
///
/// After the histogram come the acquire's own counts, which tell a late driver from a head that
/// composed nothing: `missed` frame numbers never seen, `same` frames whose number did not
/// move, `queued` frames already waiting when the acquire came back, `lead_us` how far ahead of
/// its display time a frame arrived (negative is after it), `hold_us` the bypass wait.
pub struct Cadence {
    hz: u64,
    last: u64,
    last_number: Option<u32>,
    since: u64,
    n: u64,
    max_us: u64,
    buckets: [u32; Self::BUCKETS],
    missed: u64,
    same: u32,
    queued: u32,
    leads: u32,
    lead_sum_us: i64,
    lead_min_us: i64,
    holds: u32,
    hold_sum_us: u64,
    hold_max_us: u64,
}

impl Cadence {
    const REPORT_MS: u64 = 10_000;
    const BUCKETS: usize = 24;

    /// A histogram on the QPC clock.
    pub fn new() -> Self {
        Self::with_hz(qpc_frequency())
    }

    /// A histogram on a clock of `hz` ticks a second.
    fn with_hz(hz: u64) -> Self {
        Self {
            hz,
            last: 0,
            last_number: None,
            since: 0,
            n: 0,
            max_us: 0,
            buckets: [0; Self::BUCKETS],
            missed: 0,
            same: 0,
            queued: 0,
            leads: 0,
            lead_sum_us: 0,
            lead_min_us: i64::MAX,
            holds: 0,
            hold_sum_us: 0,
            hold_max_us: 0,
        }
    }

    /// One bypass hold, in QPC ticks.
    pub fn note_hold(&mut self, ticks: u64) {
        let us = ticks * 1_000_000 / self.hz;
        self.holds += 1;
        self.hold_sum_us += us;
        self.hold_max_us = self.hold_max_us.max(us);
    }

    /// The acquire's own counts. Every frame counts, whatever its stamp.
    fn note_acquire(&mut self, f: &Acquired) {
        if let Some(last) = self.last_number.replace(f.number) {
            match f.number.wrapping_sub(last) {
                0 => self.same += 1,
                1 => {}
                // A step backwards is a counter that started over, not a gap.
                gap if gap < u32::MAX / 2 => self.missed += u64::from(gap - 1),
                _ => {}
            }
        }
        self.queued += u32::from(f.queued);
        if f.display_qpc != 0 {
            let lead_us = (f.display_qpc as i64 - f.at_qpc as i64) * 1_000_000 / self.hz as i64;
            self.leads += 1;
            self.lead_sum_us += lead_us;
            self.lead_min_us = self.lead_min_us.min(lead_us);
        }
    }

    /// One acquired frame, logging the report line when it closes a window ([`Self::tally`]).
    pub fn note(&mut self, f: &Acquired, fps: u32, bypass: bool) {
        if let Some(line) = self.tally(f, fps, bypass) {
            crate::log::log(&line);
        }
    }

    /// One acquired frame. A zero or backwards present stamp only re-anchors, and so does an
    /// unknown refresh (`fps` 0: no session): the run's buckets stay in one unit. A frame that
    /// closes a [`Self::REPORT_MS`] window returns its report line and starts the counts over.
    fn tally(&mut self, f: &Acquired, fps: u32, bypass: bool) -> Option<String> {
        self.note_acquire(f);
        let qpc = f.display_qpc;
        let (last, since) = (self.last, self.since);
        self.last = qpc;
        if fps == 0 || qpc == 0 || last == 0 || qpc <= last {
            self.since = qpc;
            return None;
        }
        if since == 0 {
            self.since = last;
        }
        let period_us = 1_000_000 / u64::from(fps);
        let delta_us = (qpc - last) * 1_000_000 / self.hz;
        self.n += 1;
        self.max_us = self.max_us.max(delta_us);
        let bucket = (delta_us * 8 / period_us.max(1)).min(Self::BUCKETS as u64 - 1) as usize;
        self.buckets[bucket] += 1;
        let window_ms = (qpc - self.since) * 1_000 / self.hz;
        if window_ms < Self::REPORT_MS {
            return None;
        }
        let h = self.buckets.map(|b| b.to_string()).join("/");
        let line = format!(
            "[pf-vd] cadence: mode={} fps={fps} win_ms={window_ms} n={} max_us={} h={h} missed={} same={} queued={} lead_us mean={} min={} hold_us mean={} max={}",
            if bypass { "bypass" } else { "pool" },
            self.n,
            self.max_us,
            self.missed,
            self.same,
            self.queued,
            self.lead_sum_us / i64::from(self.leads.max(1)),
            if self.leads == 0 { 0 } else { self.lead_min_us },
            self.hold_sum_us / u64::from(self.holds.max(1)),
            self.hold_max_us
        );
        // The window's counts start over; the two anchors and the clock carry across it.
        *self = Self {
            last: self.last,
            last_number: self.last_number,
            since: qpc,
            ..Self::with_hz(self.hz)
        };
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One tick a microsecond at 125 Hz: a period is 8,000 ticks, an eighth 1,000.
    const HZ: u64 = 1_000_000;
    const FPS: u32 = 125;
    const EIGHTH: u64 = 1_000;
    const PERIOD: u64 = 8 * EIGHTH;

    fn frame(display_qpc: u64, number: u32) -> Acquired {
        Acquired {
            display_qpc,
            at_qpc: display_qpc,
            number,
            queued: false,
        }
    }

    /// The bucket one gap of `delta` ticks lands in.
    fn bucket_of(delta: u64) -> usize {
        let mut c = Cadence::with_hz(HZ);
        c.tally(&frame(PERIOD, 1), FPS, false);
        c.tally(&frame(PERIOD + delta, 2), FPS, false);
        assert_eq!(c.n, 1, "a gap of {delta}");
        c.buckets.iter().position(|&b| b == 1).unwrap()
    }

    #[test]
    fn a_gap_of_n_eighth_periods_lands_in_bucket_n() {
        assert_eq!(bucket_of(1), 0);
        for n in 1..Cadence::BUCKETS as u64 {
            assert_eq!(bucket_of(n * EIGHTH), n as usize, "{n} eighths");
            assert_eq!(
                bucket_of(n * EIGHTH - 1),
                n as usize - 1,
                "under {n} eighths"
            );
        }
        assert_eq!(bucket_of(100 * PERIOD), Cadence::BUCKETS - 1);
    }

    #[test]
    fn a_zero_same_or_backwards_stamp_only_re_anchors() {
        for stamp in [0, 2 * PERIOD, PERIOD + EIGHTH] {
            let mut c = Cadence::with_hz(HZ);
            c.tally(&frame(PERIOD, 1), FPS, false);
            c.tally(&frame(2 * PERIOD, 2), FPS, false);
            let counted = c.buckets;
            c.tally(&frame(stamp, 3), FPS, false);
            assert_eq!((c.n, c.buckets), (1, counted), "stamp {stamp}");
            assert_eq!((c.last, c.since), (stamp, stamp), "stamp {stamp}");
        }
        let mut c = Cadence::with_hz(HZ);
        for (qpc, number) in [(PERIOD, 1), (2 * PERIOD, 2), (PERIOD + EIGHTH, 3)] {
            c.tally(&frame(qpc, number), FPS, false);
        }
        c.tally(&frame(2 * PERIOD + EIGHTH, 4), FPS, false);
        assert_eq!(
            (c.n, c.buckets[8]),
            (2, 2),
            "the next gap runs from the new anchor"
        );
    }

    #[test]
    fn an_unknown_refresh_only_re_anchors() {
        let mut c = Cadence::with_hz(HZ);
        c.tally(&frame(PERIOD, 1), 0, false);
        c.tally(&frame(2 * PERIOD, 2), 0, false);
        assert_eq!((c.n, c.buckets), (0, [0; Cadence::BUCKETS]));
        assert_eq!((c.last, c.since), (2 * PERIOD, 2 * PERIOD));
        assert!(
            c.tally(&frame(20 * HZ, 3), 0, false).is_none(),
            "no window without a session"
        );
        c.tally(&frame(20 * HZ + PERIOD, 4), FPS, false);
        assert_eq!((c.n, c.buckets[8]), (1, 1));
    }

    #[test]
    fn the_report_window_resets_the_counts_and_carries_the_anchors() {
        let mut c = Cadence::with_hz(HZ);
        let mut lines = Vec::new();
        // 1,250 periods of 8 ms: the last frame closes the 10 s window.
        for number in 1..=1_251u32 {
            let f = Acquired {
                queued: true,
                ..frame(u64::from(number) * PERIOD, number)
            };
            lines.extend(c.tally(&f, FPS, false));
        }
        let [line] = &lines[..] else {
            panic!("one report line, got {lines:?}");
        };
        let head = "[pf-vd] cadence: mode=pool fps=125 win_ms=10000 n=1250 max_us=8000 h=0/0/0/0/0/0/0/0/1250/0/";
        assert!(line.starts_with(head), "{line}");
        assert!(line.contains(" missed=0 same=0 queued=1251 "), "{line}");
        let closing = 1_251 * PERIOD;
        assert_eq!((c.n, c.max_us, c.buckets), (0, 0, [0; Cadence::BUCKETS]));
        assert_eq!((c.queued, c.hz), (0, HZ));
        assert_eq!(
            (c.last, c.since, c.last_number),
            (closing, closing, Some(1_251))
        );
        assert!(
            c.tally(&frame(closing + PERIOD, 1_252), FPS, false)
                .is_none()
        );
        assert_eq!((c.n, c.buckets[8]), (1, 1));
    }

    #[test]
    fn the_acquire_counts_read_a_step_back_as_a_restart() {
        let mut c = Cadence::with_hz(HZ);
        // 11 twice is one `same`; 14 after 11 misses two; 3 after 14 and `u32::MAX` after 4
        // started over; 0 after `u32::MAX` is the next frame.
        for number in [10, 11, 11, 14, 3, 4, u32::MAX, 0] {
            let f = Acquired {
                queued: number == 4,
                ..frame(0, number)
            };
            c.tally(&f, FPS, false);
        }
        assert_eq!((c.missed, c.same, c.queued), (2, 1, 1));
    }
}
