//! The host's answer to a `SetBitrate` ask: the rate the encoder is sent and the ack's reason.
//!
//! In order: the PyroWave pin answers first, and a pinned answer is final. Then a climb is
//! held at the live rate while encode is behind cadence, then the path share binds (PyroWave's
//! floor under it), then the encoder's learned ceiling. The ceiling comes last, so it is asked
//! about the rate the encoder will run and spends its re-test on nothing else.
//!
//! [`resolve`] reads [`RateFacts`], moves the pin, and asks the ceiling at most once; it logs
//! but does no I/O. The table in `tests` pins the order, its rows named after the fixes.

use crate::native::bitrate::resolve_bitrate_kbps;
use punktfunk_core::quic::AckReason;

/// What the session knows when an ask lands.
pub(super) struct RateFacts {
    /// The stream encodes PyroWave, so its pin answers first.
    pub(super) pyrowave: bool,
    /// The bring-up ramp's window; an explicit pin takes its verdict only while it is open.
    pub(super) ramp_open: bool,
    /// The encoder-applied rate; `0` before the first apply.
    pub(super) live_kbps: u32,
    pub(super) cadence_degraded: bool,
    /// Named on the hold's log line: without it an ABR floor and the network look alike.
    pub(super) behind_score: u32,
    /// This session's share of a path it is not alone on; `0` when it is.
    pub(super) share_kbps: u32,
}

/// The rate to send the encoder for an ask of `ask`, and the reason its ack carries.
/// `ceiling` is the encoder ceiling's own `resolve`, asked once, last.
pub(super) fn resolve(
    ask: u32,
    facts: &RateFacts,
    pin: &mut PyroWavePin,
    ceiling: impl FnOnce(u32) -> (u32, AckReason),
) -> (u32, AckReason) {
    let was_pin = pin.kbps;
    let (mut want, mut why) = if facts.pyrowave {
        pin.resolve(ask, facts.ramp_open)
    } else {
        (resolve_bitrate_kbps(ask), AckReason::Granted)
    };
    if why == AckReason::Pinned {
        if want < was_pin {
            tracing::info!(
                pin_kbps = was_pin,
                fit_kbps = want,
                "PyroWave pin lowered to what the bring-up ramp measured"
            );
        } else {
            tracing::info!(
                requested_kbps = ask,
                pinned_kbps = want,
                "PyroWave session: mid-stream bitrate retarget refused (pinned)"
            );
        }
        return (want, why);
    }
    // Past the compute knee more bits deepen the miss, and on a fat LAN nothing else stops
    // the climb. Descents pass.
    let live = facts.live_kbps;
    if facts.cadence_degraded && live != 0 && want > live {
        tracing::info!(
            requested_kbps = ask,
            held_kbps = live,
            behind_score = facts.behind_score,
            "bitrate climb refused — encode is behind cadence"
        );
        want = live;
        why = AckReason::Cadence;
    }
    // Whatever else allows, a session may not take a sibling's half of the path.
    if facts.share_kbps > 0 && want > facts.share_kbps {
        want = facts.share_kbps.max(pin.floor);
        why = AckReason::Governor;
    }
    // The ack is the client's climb base: never past a ceiling the encoder taught, unless
    // this ask is its re-test. A ceiling under the rate so far names the ack instead.
    let (kbps, ceiling_why) = ceiling(want);
    if kbps < want {
        why = ceiling_why;
    }
    (kbps, why)
}

/// A PyroWave session's pin against `SetBitrate` asks.
///
/// Automatic: any ask lands inside `[floor, pin]`, at any time. Explicit: every
/// ask is refused with the pin — except one: the bring-up ramp ends in a verdict
/// ask, and while the ramp window is still open a lower one becomes the pin.
/// Never a raise, never twice, and a refused ask does not spend the verdict.
pub(super) struct PyroWavePin {
    /// The pin as it stands — what an explicit session's refusals ack, and what
    /// an Automatic one adapts under.
    pub(super) kbps: u32,
    /// `BPP_FLOOR` at the delivered mode, never over the pin; `0` off PyroWave.
    pub(super) floor: u32,
    /// The client asked Automatic.
    pub(super) automatic: bool,
    /// An explicit session's verdict ask already landed.
    pub(super) fit_taken: bool,
}

impl PyroWavePin {
    /// The rate to send the encoder, and the ack's reason: an Automatic ask
    /// held inside the bounds (zero asks for the pin), else the explicit pin —
    /// the asked rate the one time the window admits it. `ramp_open` is the
    /// bring-up window — it closes before the first frame.
    fn resolve(&mut self, asked_kbps: u32, ramp_open: bool) -> (u32, AckReason) {
        if self.automatic {
            let kbps = match asked_kbps {
                0 => self.kbps,
                k => k.max(self.floor).min(self.kbps),
            };
            return (kbps, AckReason::Granted);
        }
        if !self.fit_taken && ramp_open && asked_kbps > 0 && asked_kbps < self.kbps {
            self.fit_taken = true;
            self.kbps = asked_kbps;
        }
        (self.kbps, AckReason::Pinned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An 800 Mbit/s PyroWave pin over a 250 Mbit/s floor.
    fn pyrowave_pin(automatic: bool) -> PyroWavePin {
        PyroWavePin {
            kbps: 800_000,
            floor: 250_000,
            automatic,
            fit_taken: false,
        }
    }

    /// An Automatic session's ask lands inside its floor and pin, granted, with
    /// the ramp window long closed; zero asks for the pin.
    #[test]
    fn an_automatic_ask_lands_inside_the_floor_and_pin() {
        let mut pin = pyrowave_pin(true);
        assert_eq!(pin.resolve(400_000, false), (400_000, AckReason::Granted));
        assert_eq!(pin.resolve(100_000, false), (250_000, AckReason::Granted));
        assert_eq!(pin.resolve(900_000, false), (800_000, AckReason::Granted));
        assert_eq!(pin.resolve(0, false), (800_000, AckReason::Granted));
        assert_eq!(pin.kbps, 800_000, "an ask never moves the pin");
    }

    /// The ramp's verdict ask — lower than the pin, inside the bring-up
    /// window — becomes an explicit session's pin, and the ack carries it.
    #[test]
    fn a_ramp_verdict_under_the_pin_lowers_it() {
        let mut pin = pyrowave_pin(false);
        assert_eq!(pin.resolve(340_000, true), (340_000, AckReason::Pinned));
        assert_eq!(pin.kbps, 340_000, "the pin moved to the measured rate");
    }

    /// Once the verdict landed the session is pinned again: a further ask —
    /// even a lower one — is refused with the pin as it now stands.
    #[test]
    fn the_verdict_ask_comes_once() {
        let mut pin = pyrowave_pin(false);
        pin.resolve(340_000, true);
        assert_eq!(
            pin.resolve(200_000, true).0,
            340_000,
            "a second lower ask is refused at the lowered pin"
        );
        assert_eq!(
            pin.resolve(900_000, true).0,
            340_000,
            "and a raise over the old pin is refused the same"
        );
    }

    /// A raise inside the window is refused and does not spend the verdict:
    /// the ramp's own ask, landing behind it, still lowers the pin.
    #[test]
    fn the_pin_never_raises_and_a_refusal_spends_nothing() {
        let mut pin = pyrowave_pin(false);
        assert_eq!(pin.resolve(900_000, true).0, 800_000, "a raise is refused");
        assert_eq!(
            pin.resolve(800_000, true).0,
            800_000,
            "as is the pin itself"
        );
        assert_eq!(
            pin.resolve(340_000, true).0,
            340_000,
            "the verdict still fits after them"
        );
    }

    /// The window closes with the bring-up: a verdict that lands after it is
    /// an ordinary ask, refused like every other, and a zero ask on the wire
    /// is no verdict.
    #[test]
    fn a_late_verdict_finds_the_window_closed() {
        let mut pin = pyrowave_pin(false);
        assert_eq!(
            pin.resolve(340_000, false),
            (800_000, AckReason::Pinned),
            "ramp_open closed: the pin stands"
        );
        assert!(!pin.fit_taken);
        assert_eq!(
            pin.resolve(0, true).0,
            800_000,
            "a zero ask is not a measurement"
        );
    }

    /// The encoder ceiling a row starts from.
    #[derive(Clone, Copy)]
    enum Ceiling {
        None,
        /// Learned at this rate, inside its wait.
        Standing(u32),
        /// Learned at this rate, then a dip under it the encoder took whole.
        Dipped(u32),
        /// Learned at this rate, its wait run out: an ask over it is the re-test and passes.
        /// A stand-in, as `EncoderCeiling` reaches it only after twelve seconds.
        Due(u32),
    }

    struct Row {
        name: &'static str,
        ask: u32,
        facts: RateFacts,
        pin: PyroWavePin,
        ceiling: Ceiling,
        /// The rate sent and the reason acked.
        acks: (u32, AckReason),
        /// The rate the ceiling is asked about; `None` when it is not asked.
        ceiling_sees: Option<u32>,
    }

    /// Alone on the path, encode on cadence, 20 Mbit/s live, not PyroWave.
    const CALM: RateFacts = RateFacts {
        pyrowave: false,
        ramp_open: false,
        live_kbps: 20_000,
        cadence_degraded: false,
        behind_score: 0,
        share_kbps: 0,
    };
    /// Off PyroWave the pin is never asked and its floor is `0`.
    const NO_PIN: PyroWavePin = PyroWavePin {
        kbps: 20_000,
        floor: 0,
        automatic: false,
        fit_taken: false,
    };
    const BEHIND: RateFacts = RateFacts {
        cadence_degraded: true,
        ..CALM
    };
    const PYRO: RateFacts = RateFacts {
        pyrowave: true,
        live_kbps: 400_000,
        ..CALM
    };

    #[rustfmt::skip]
    fn rows() -> Vec<Row> {
        use AckReason::{Cadence, EncoderLimit, Governor, Granted, Pinned};
        let row = |name, ask, facts, pin, ceiling, acks, ceiling_sees| Row {
            name,
            ask,
            facts,
            pin,
            ceiling,
            acks,
            ceiling_sees,
        };
        let shared = |share_kbps| RateFacts { share_kbps, ..CALM };
        let max = crate::native::bitrate::MAX_BITRATE_KBPS;
        vec![
            row("an ask is granted as asked", 30_000, CALM, NO_PIN, Ceiling::None,
                (30_000, Granted), Some(30_000)),
            row("zero asks for the host default", 0, CALM, NO_PIN, Ceiling::None,
                (20_000, Granted), Some(20_000)),
            row("an ask past the host's range is clamped", 9_000_000, CALM, NO_PIN, Ceiling::None,
                (max, Granted), Some(max)),
            row("cadence hold: a climb waits at the live rate", 30_000, BEHIND, NO_PIN,
                Ceiling::None, (20_000, Cadence), Some(20_000)),
            row("cadence hold: a descent passes", 15_000, BEHIND, NO_PIN, Ceiling::None,
                (15_000, Granted), Some(15_000)),
            row("cadence hold: nothing to hold before the first apply", 30_000,
                RateFacts { live_kbps: 0, ..BEHIND }, NO_PIN, Ceiling::None,
                (30_000, Granted), Some(30_000)),
            row("share: a climb past it binds", 30_000, shared(12_000), NO_PIN, Ceiling::None,
                (12_000, Governor), Some(12_000)),
            row("share: an ask under it passes", 10_000, shared(12_000), NO_PIN, Ceiling::None,
                (10_000, Granted), Some(10_000)),
            row("share: PyroWave's floor binds under it", 400_000,
                RateFacts { share_kbps: 100_000, ..PYRO }, pyrowave_pin(true), Ceiling::None,
                (250_000, Governor), Some(250_000)),
            row("ceiling: an ask past it binds", 30_000, CALM, NO_PIN, Ceiling::Standing(25_000),
                (25_000, EncoderLimit), Some(30_000)),
            row("ceiling: under the share it names the ack", 30_000, shared(28_000), NO_PIN,
                Ceiling::Standing(25_000), (25_000, EncoderLimit), Some(28_000)),
            row("ceiling: under the hold it names the ack", 30_000,
                RateFacts { live_kbps: 28_000, ..BEHIND }, NO_PIN, Ceiling::Standing(25_000),
                (25_000, EncoderLimit), Some(28_000)),
            row("ceiling re-test: past its wait the ask reaches the encoder", 30_000, CALM,
                NO_PIN, Ceiling::Due(25_000), (30_000, Granted), Some(30_000)),
            row("S3-3: a climb the share holds spends no re-test", 30_000, shared(20_000),
                NO_PIN, Ceiling::Due(25_000), (20_000, Governor), Some(20_000)),
            row("S3-3: a climb the cadence hold holds spends no re-test", 30_000, BEHIND, NO_PIN,
                Ceiling::Due(25_000), (20_000, Cadence), Some(20_000)),
            row("S2-2: a dip the encoder took whole keeps the ceiling", 30_000, CALM, NO_PIN,
                Ceiling::Dipped(25_000), (25_000, EncoderLimit), Some(30_000)),
            row("PyroWave pin: a retarget is refused, nothing after the pin asked", 400_000,
                RateFacts { cadence_degraded: true, share_kbps: 100_000, ..PYRO },
                pyrowave_pin(false), Ceiling::Standing(200_000), (800_000, Pinned), None),
            row("PyroWave pin: the ramp's verdict lowers it", 340_000,
                RateFacts { ramp_open: true, ..PYRO }, pyrowave_pin(false), Ceiling::None,
                (340_000, Pinned), None),
            row("PyroWave Automatic: an ask under the floor lands on it", 100_000, PYRO,
                pyrowave_pin(true), Ceiling::None, (250_000, Granted), Some(250_000)),
            row("PyroWave Automatic: an ask over the pin lands on it", 900_000, PYRO,
                pyrowave_pin(true), Ceiling::None, (800_000, Granted), Some(800_000)),
            row("PyroWave Automatic: a climb is held like any other", 600_000,
                RateFacts { cadence_degraded: true, ..PYRO }, pyrowave_pin(true), Ceiling::None,
                (400_000, Cadence), Some(400_000)),
        ]
    }

    /// The pin answers first and a pinned answer is final; then the hold, the share and the
    /// ceiling last, so the ceiling is asked about the rate the encoder runs.
    #[test]
    fn a_rate_ask_meets_pin_hold_share_then_ceiling() {
        for r in rows() {
            let mut ceiling = crate::native::bitrate::EncoderCeiling::new();
            if let Ceiling::Standing(cap) | Ceiling::Dipped(cap) = r.ceiling {
                ceiling.note_applied(cap * 2, cap);
            }
            if let Ceiling::Dipped(cap) = r.ceiling {
                ceiling.note_applied(cap / 2, cap / 2);
            }
            let mut pin = r.pin;
            let mut sees = None;
            let acks = resolve(r.ask, &r.facts, &mut pin, |want| {
                sees = Some(want);
                match r.ceiling {
                    Ceiling::Due(_) => (want, AckReason::Granted),
                    _ => ceiling.resolve(want),
                }
            });
            assert_eq!((acks, sees), (r.acks, r.ceiling_sees), "{}", r.name);
            // A due ceiling spends its re-test on an ask over the cap: only one the encoder runs.
            if let Ceiling::Due(cap) = r.ceiling {
                let spent = sees.is_some_and(|w| w > cap);
                assert_eq!(spent, acks.0 > cap, "{}: re-test on another rate", r.name);
            }
        }
    }
}
