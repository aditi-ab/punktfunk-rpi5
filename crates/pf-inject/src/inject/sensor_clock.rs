//! The `sensor_timestamp` a virtual Sony pad stamps into every input report.
//!
//! Real hardware fills this field from its IMU's own clock, and it is the **only** time basis a
//! consumer has for the motion samples in the same report: `hid-playstation` forwards it as
//! `MSC_TIMESTAMP`, SDL reads it straight out of the report on Windows, and anything doing gyro aim
//! integrates angular velocity against the `dt` it implies. A clock in the wrong units doesn't look
//! broken — it looks like a controller whose sensitivity is off by that factor.
//!
//! Our virtual pads used to advance the field by a fixed amount per report: the DualSense by +1 raw
//! unit (0.33 µs, a clock running ~12000× slow — effectively frozen), the DualShock 4 by +188
//! (~1 ms) regardless of the real 4–8 ms publish cadence. Both now stamp real elapsed time.
//!
//! The value is computed from the pad's first report rather than accumulated per report, so a
//! bursty or throttled publish loop cannot make the clock drift; the caller truncates it to the
//! field's width, which reproduces the wrap real hardware does (and which every consumer's
//! `prev > current` delta check already handles).

use std::time::Instant;

/// A monotonic sensor clock in one pad's tick units. Construct per pad — the epoch is that pad's
/// first report, so the field starts at 0 like a freshly enumerated device.
pub struct SensorClock {
    epoch: Option<Instant>,
    /// Ticks per microsecond as an exact fraction, `ticks_num / ticks_den`.
    ticks_num: u64,
    ticks_den: u64,
}

impl SensorClock {
    /// DualSense: the u32 `sensor_timestamp` counts **1/3 µs** ticks — `hid-playstation` converts a
    /// delta with `DIV_ROUND_CLOSEST(delta, 3)`. Wraps every ~23.9 minutes.
    pub fn dualsense() -> SensorClock {
        SensorClock::new(3, 1)
    }

    /// DualShock 4: the u16 `sensor_timestamp` counts **16/3 µs** (≈5.33 µs) ticks —
    /// `DIV_ROUND_CLOSEST(delta * 16, 3)`. Wraps every ~349 ms, which is normal and expected.
    pub fn dualshock4() -> SensorClock {
        SensorClock::new(3, 16)
    }

    fn new(ticks_num: u64, ticks_den: u64) -> SensorClock {
        SensorClock {
            epoch: None,
            ticks_num,
            ticks_den,
        }
    }

    /// Ticks elapsed since this pad's first report. `now` is a parameter rather than an internal
    /// `Instant::now()` so the unit tests below can drive the clock.
    pub fn ticks(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        let micros = now.saturating_duration_since(epoch).as_micros() as u64;
        micros * self.ticks_num / self.ticks_den
    }

    /// [`ticks`](Self::ticks) truncated to the DualSense's u32 field.
    pub fn ds_ticks(&mut self, now: Instant) -> u32 {
        self.ticks(now) as u32
    }

    /// [`ticks`](Self::ticks) truncated to the DualShock 4's u16 field.
    pub fn ds4_ticks(&mut self, now: Instant) -> u16 {
        self.ticks(now) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// One second of elapsed time must read as one second in each pad's units — the property the
    /// old fixed-increment clocks got wrong by 12000× (DualSense) and ~5× (DualShock 4).
    #[test]
    fn a_second_reads_as_a_second() {
        let t0 = Instant::now();
        let after = t0 + Duration::from_secs(1);

        // DualSense: 1 s = 3_000_000 ticks of 1/3 µs.
        let mut ds = SensorClock::dualsense();
        assert_eq!(ds.ticks(t0), 0, "the first report is the epoch");
        assert_eq!(ds.ticks(after), 3_000_000);

        // DualShock 4: 1 s = 187_500 ticks of 16/3 µs.
        let mut ds4 = SensorClock::dualshock4();
        assert_eq!(ds4.ticks(t0), 0);
        assert_eq!(ds4.ticks(after), 187_500);
    }

    /// A realistic 4 ms publish interval, which is what the DS4's old `+188` claimed to be (it was
    /// ~1 ms) and what the DualSense's old `+1` was off by four orders of magnitude from.
    #[test]
    fn one_publish_interval() {
        let t0 = Instant::now();
        let mut ds = SensorClock::dualsense();
        let mut ds4 = SensorClock::dualshock4();
        ds.ticks(t0);
        ds4.ticks(t0);
        let after = t0 + Duration::from_millis(4);
        assert_eq!(ds.ticks(after), 12_000); // 4000 µs × 3
        assert_eq!(ds4.ticks(after), 750); // 4000 µs × 3 / 16
    }

    /// The value is anchored to the epoch, not accumulated — an irregular cadence stays honest.
    #[test]
    fn jitter_does_not_drift() {
        let t0 = Instant::now();
        let mut ds4 = SensorClock::dualshock4();
        ds4.ticks(t0); // the pad's first report — this, not `t0` itself, is the epoch
        let mut t = t0;
        for step in [1u64, 17, 3, 40, 9, 2] {
            t += Duration::from_millis(step);
            ds4.ticks(t);
        }
        // 72 ms since that first report, regardless of how it was walked.
        assert_eq!(ds4.ticks(t), 72_000 * 3 / 16);
    }

    /// Both fields wrap, exactly as the hardware's do; consumers handle it with a `prev > current`
    /// check, so truncation is the correct way to fill them.
    #[test]
    fn fields_wrap_like_hardware() {
        let t0 = Instant::now();

        // The DS4's u16 holds 65536 ticks × 16/3 µs = 349_525.33 µs, so 349_525 µs is still the
        // last representable tick and the next microsecond rolls over.
        let mut ds4 = SensorClock::dualshock4();
        ds4.ticks(t0);
        assert_eq!(ds4.ds4_ticks(t0 + Duration::from_micros(349_525)), 65_535);
        assert_eq!(ds4.ds4_ticks(t0 + Duration::from_micros(349_526)), 0);

        // The DualSense's u32 takes ~23.9 minutes to get there; 3 ticks per µs means the first
        // microsecond past the roll lands on 2.
        let mut ds = SensorClock::dualsense();
        ds.ticks(t0);
        let past_wrap = Duration::from_micros(u32::MAX as u64 / 3 + 1);
        assert_eq!(ds.ds_ticks(t0 + past_wrap), 2);
    }
}
