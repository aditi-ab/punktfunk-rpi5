//! Hung-decoder detection for the MediaCodec decode loop.

use std::time::{Duration, Instant};

/// How long access units may wait while the codec frees no input slot before the decoder counts
/// as hung. A working codec frees a slot per decoded frame: 1 s is 60 missed slots at 60 Hz.
pub(crate) const INPUT_STALL_PATIENCE: Duration = Duration::from_secs(1);

/// Trips once access units have waited [`INPUT_STALL_PATIENCE`] with no input slot offered.
///
/// A hung hardware decoder raises no error. It stops taking input, so no keyframe can reach it,
/// and the loop's keyframe requests change nothing. A codec that takes no input shows no new
/// picture either, so the screen is already frozen whenever this trips.
#[derive(Default)]
pub(crate) struct InputStall {
    since: Option<Instant>,
}

impl InputStall {
    /// One loop pass. `offered`: the codec freed an input slot this pass. `waiting`: access units
    /// are still parked after feeding.
    pub(crate) fn poll(&mut self, offered: bool, waiting: bool, now: Instant) -> bool {
        if offered || !waiting {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= INPUT_STALL_PATIENCE
    }
}

/// How long the decoder may owe output before the loop asks for a re-anchor keyframe. A
/// silence window, not the gate's per-AU streak: MediaCodec does not pair inputs with outputs.
/// Well past any decoder's first-frame latency.
pub(crate) const NO_OUTPUT_PATIENCE: Duration = Duration::from_millis(500);

/// Trips once the decoder has owed output for [`NO_OUTPUT_PATIENCE`]: it never got the opening
/// IDR, or its reference chain is gone. A hardware decoder then emits nothing, and under
/// infinite GOP nothing re-anchors it unless the client asks.
pub(crate) struct NoOutput {
    /// The latest pass that produced a frame or owed none.
    owed_since: Instant,
    handled_at_output: u64,
}

impl NoOutput {
    pub(crate) fn new(now: Instant) -> NoOutput {
        NoOutput {
            owed_since: now,
            handled_at_output: 0,
        }
    }

    /// One pass. `handled`: AUs fed to the codec or withheld from it. A withheld AU owes output
    /// too, so a stretch whose anchor was lost still trips. At most once per window.
    pub(crate) fn poll(&mut self, handled: u64, had_output: bool, now: Instant) -> bool {
        // Owing nothing restarts the window, so the first AU after a still stretch gets the full
        // patience instead of tripping on the pause before it.
        if had_output || handled == self.handled_at_output {
            self.owed_since = now;
            self.handled_at_output = handled;
            return false;
        }
        if now.duration_since(self.owed_since) < NO_OUTPUT_PATIENCE {
            return false;
        }
        log::warn!(
            "decode: no output for {} ms with {} AU(s) fed or withheld — requesting a re-anchor keyframe",
            now.duration_since(self.owed_since).as_millis(),
            handled - self.handled_at_output
        );
        self.owed_since = now;
        self.handled_at_output = handled;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{InputStall, NoOutput, INPUT_STALL_PATIENCE, NO_OUTPUT_PATIENCE};
    use std::time::{Duration, Instant};

    /// A still host sends one frame after seconds of nothing. That frame must reach the screen:
    /// tripping on it arms the freeze and holds the very picture that carries the change.
    #[test]
    fn an_au_after_a_still_host_gets_the_full_patience() {
        let t0 = Instant::now();
        let mut b = NoOutput::new(t0);
        assert!(!b.poll(1, true, t0));
        assert!(!b.poll(1, false, t0 + Duration::from_secs(2)));
        let fed_at = t0 + Duration::from_millis(2005);
        assert!(!b.poll(2, false, fed_at));
        assert!(!b.poll(2, true, fed_at + Duration::from_millis(8)));
        // A decoder that really goes silent still trips, once, after the patience.
        let silent = fed_at + Duration::from_millis(20);
        assert!(!b.poll(3, false, silent));
        assert!(b.poll(4, false, silent + NO_OUTPUT_PATIENCE));
        assert!(!b.poll(
            5,
            false,
            silent + NO_OUTPUT_PATIENCE + Duration::from_millis(5)
        ));
    }

    /// After a loss the codec may get nothing: every AU is withheld until the anchor. If the
    /// anchor is lost the window must still trip, which only counting withheld AUs does.
    #[test]
    fn withheld_aus_still_trip_the_window() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let fed = 10;
        let mut counted = NoOutput::new(t0);
        let mut fed_only = NoOutput::new(t0);
        assert!(!counted.poll(fed, true, t0));
        assert!(!fed_only.poll(fed, true, t0));
        let (mut trips, mut fed_only_trips) = (0, 0);
        for withheld in 1..=40u64 {
            let now = t0 + ms(16 * withheld);
            trips += u32::from(counted.poll(fed + withheld, false, now));
            fed_only_trips += u32::from(fed_only.poll(fed, false, now));
        }
        assert_eq!(
            trips, 1,
            "640 ms of withheld AUs trips the 500 ms window once"
        );
        assert_eq!(fed_only_trips, 0);
    }

    #[test]
    fn only_a_codec_that_stops_taking_input_trips() {
        let mut s = InputStall::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // A working codec frees a slot each pass: never trips, however long the backlog.
        for i in 0..300 {
            assert!(!s.poll(true, true, t0 + ms(i * 10)));
        }
        // A still host parks nothing, so an idle stretch never counts against the codec.
        assert!(!s.poll(false, false, t0 + ms(8000)));
        // The first AU after the pause starts the clock instead of tripping on the pause.
        let t1 = t0 + ms(10_000);
        assert!(!s.poll(false, true, t1));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE - ms(1)));
        assert!(s.poll(false, true, t1 + INPUT_STALL_PATIENCE));
        // One freed slot restarts the wait.
        assert!(!s.poll(true, true, t1 + INPUT_STALL_PATIENCE + ms(5)));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE + ms(10)));
    }
}
