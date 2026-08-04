//! Per-pad dedup for the rich HID-output feedback plane (0xCD), carved out of `dualsense_proto`
//! (plan §W4 — it is device-agnostic, shared by the DualSense/DS4/Deck managers via
//! [`crate::uhid_manager`], not DualSense-specific). A game bundles rumble + lightbar +
//! LEDs + adaptive triggers into one output report, so a merely-rumbling pad re-sends unchanged
//! rich state every report; this forwards only genuine changes (one-shot pulses always fire).

use punktfunk_core::quic::HidOutput;
use std::time::{Duration, Instant};

/// How often the latched rich state is re-emitted even though nothing changed.
///
/// The 0xCD plane is deduped AND rides unreliable datagrams, which is a bad pairing: a change is
/// forwarded exactly once, so if that datagram is dropped the game will never produce it again —
/// it keeps re-sending the same value and the dedup swallows every copy. The pad is then left
/// holding the PREVIOUS value: the last weapon's trigger effect, the last lightbar colour, for as
/// long as the game keeps that setting. For a trigger effect that can be the rest of a level.
///
/// Slow on purpose. This is a repair mechanism, not a transport — at one second a lost update
/// costs a noticeable but bounded wrong-feel window, while the steady-state cost is at most four
/// small datagrams per second per pad, against a rumble plane that already resends at ~120 ms.
const RENEW_EVERY: Duration = Duration::from_millis(1000);

/// Per-pad dedup for the DualSense HID-output feedback plane (0xCD). A game's DualSense output report
/// bundles rumble + lightbar + player-LEDs + adaptive-triggers into one report, so a pad that is
/// merely *rumbling* re-sends its (unchanged) lightbar / LED / trigger state on every output report.
/// The managers already dedup rumble; this does the same for the rich [`HidOutput`] feedback so the
/// 0xCD plane carries only genuine changes. State (`Led` / `PlayerLeds` / `Trigger`) is deduped by
/// value; a one-shot `TrackpadHaptic` pulse is always forwarded (each pulse must fire).
#[derive(Clone, Default)]
pub struct HidoutDedup {
    led: Option<(u8, u8, u8)>,
    player_leds: Option<u8>,
    /// Last-forwarded adaptive-trigger effect per side: `[0]` = L2, `[1]` = R2.
    trigger: [Option<Vec<u8>>; 2],
    /// When anything was last put on the wire for this pad. `None` = nothing latched yet, so
    /// there is nothing to renew. See [`RENEW_EVERY`].
    last_sent: Option<Instant>,
}

impl HidoutDedup {
    /// Forget all remembered state — call when a pad is created or unplugged so the first feedback
    /// after a (re)connect is always forwarded.
    pub fn clear(&mut self) {
        *self = HidoutDedup::default();
    }

    /// Whether `h` should be forwarded: `true` for a genuine change (remembering the new value) or a
    /// one-shot pulse; `false` if it repeats the last-forwarded value for its kind.
    ///
    /// `now` only stamps the renewal clock ([`Self::renewals`]) — forwarding a change resets it, so
    /// a plane the game is actively changing never pays for a renewal it does not need.
    pub fn should_forward(&mut self, h: &HidOutput, now: Instant) -> bool {
        let fwd = self.decide(h);
        if fwd {
            self.last_sent = Some(now);
        }
        fwd
    }

    /// Re-emit the latched rich state, so one lost datagram cannot strand the pad on the previous
    /// value. Returns the reports to send (empty until [`RENEW_EVERY`] has passed since anything
    /// last went out); every one is idempotent, so a client that DID receive the original simply
    /// re-applies it.
    ///
    /// One-shots are deliberately absent: replaying a `TrackpadHaptic` pulse would be a *new*
    /// pulse, not a repair, and `HidRaw` is already re-sent verbatim by the device's own refresh
    /// cadence (see the note in [`Self::decide`]).
    pub fn renewals(&mut self, pad: u8, now: Instant) -> Vec<HidOutput> {
        if self
            .last_sent
            .is_none_or(|t| now.duration_since(t) < RENEW_EVERY)
        {
            return Vec::new();
        }
        self.last_sent = Some(now);
        let mut out = Vec::new();
        if let Some((r, g, b)) = self.led {
            out.push(HidOutput::Led { pad, r, g, b });
        }
        if let Some(bits) = self.player_leds {
            out.push(HidOutput::PlayerLeds { pad, bits });
        }
        for (which, effect) in self.trigger.iter().enumerate() {
            if let Some(effect) = effect {
                out.push(HidOutput::Trigger {
                    pad,
                    which: which as u8,
                    effect: effect.clone(),
                });
            }
        }
        out
    }

    fn decide(&mut self, h: &HidOutput) -> bool {
        match h {
            HidOutput::Led { r, g, b, .. } => {
                let v = Some((*r, *g, *b));
                if self.led == v {
                    false
                } else {
                    self.led = v;
                    true
                }
            }
            HidOutput::PlayerLeds { bits, .. } => {
                let v = Some(*bits);
                if self.player_leds == v {
                    false
                } else {
                    self.player_leds = v;
                    true
                }
            }
            HidOutput::Trigger { which, effect, .. } => {
                let slot = (*which as usize).min(1);
                if self.trigger[slot].as_deref() == Some(effect.as_slice()) {
                    false
                } else {
                    self.trigger[slot] = Some(effect.clone());
                    true
                }
            }
            // One-shot haptic pulse (Steam voice-coil) — state-less, always fires.
            HidOutput::TrackpadHaptic { .. } => true,
            // Raw as-is passthrough reports must NEVER dedup: the physical device's firmware
            // watchdogs RELY on identical periodic refreshes (Triton rumble re-sent every ~40 ms
            // against a ~50 ms safety timeout, lizard-off every ~3 s) — dropping a repeat would
            // silence the motors / re-enable lizard mode on the real controller.
            HidOutput::HidRaw { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HidoutDedup` forwards a value once, drops exact repeats, re-forwards a change, tracks the two
    /// trigger sides independently, never dedups one-shot haptic pulses, and re-arms after `clear`.
    #[test]
    fn hidout_dedup_forwards_only_changes() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        // First value forwards; an exact repeat is dropped; a change forwards again.
        assert!(d.should_forward(&led(10), t));
        assert!(!d.should_forward(&led(10), t));
        assert!(d.should_forward(&led(20), t));

        // Player LEDs dedup on their own field, independent of the lightbar.
        let pl = |bits| HidOutput::PlayerLeds { pad: 0, bits };
        assert!(d.should_forward(&pl(0b101), t));
        assert!(!d.should_forward(&pl(0b101), t));
        assert!(!d.should_forward(&led(20), t)); // lightbar still unchanged

        // The two adaptive triggers (L2=0, R2=1) are tracked separately.
        let trig = |which, byte| HidOutput::Trigger {
            pad: 0,
            which,
            effect: vec![byte, 0, 0],
        };
        assert!(d.should_forward(&trig(0, 1), t));
        assert!(d.should_forward(&trig(1, 1), t)); // same bytes, other side → still forwards
        assert!(!d.should_forward(&trig(0, 1), t));
        assert!(d.should_forward(&trig(0, 2), t)); // L2 effect changed

        // One-shot haptic pulses are never deduped.
        let haptic = HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        assert!(d.should_forward(&haptic, t));
        assert!(d.should_forward(&haptic, t));

        // `clear` re-arms every kind.
        d.clear();
        assert!(d.should_forward(&led(20), t));
        assert!(d.should_forward(&pl(0b101), t));
        assert!(d.should_forward(&trig(0, 2), t));
    }

    /// A change is forwarded once and then deduped — so if that one datagram is lost, nothing else
    /// would ever carry it. The renewal is what repairs that.
    #[test]
    fn latched_state_is_renewed_so_a_lost_datagram_is_not_permanent() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        let trig = HidOutput::Trigger {
            pad: 3,
            which: 1,
            effect: vec![0x02, 0x90, 0xA0],
        };
        assert!(d.should_forward(&trig, t));
        assert!(
            !d.should_forward(&trig, t),
            "the game re-sends it; the dedup swallows it"
        );

        // Nothing due yet.
        assert!(d.renewals(3, t + Duration::from_millis(999)).is_empty());

        // Past the window: the latched state goes out again, addressed to the right pad.
        let out = d.renewals(3, t + Duration::from_millis(1000));
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            HidOutput::Trigger { pad: 3, which: 1, effect } if effect == &vec![0x02, 0x90, 0xA0]
        ));

        // And it keeps repairing on the same cadence, not just once.
        assert!(d.renewals(3, t + Duration::from_millis(1500)).is_empty());
        assert_eq!(d.renewals(3, t + Duration::from_millis(2000)).len(), 1);
    }

    /// Every latched plane is renewed together, and a plane the game is actively driving does not
    /// pay for renewals it does not need (a forward resets the clock).
    #[test]
    fn renewal_covers_every_latched_plane_and_an_active_plane_defers_it() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        assert!(d.should_forward(
            &HidOutput::Led {
                pad: 0,
                r: 9,
                g: 8,
                b: 7
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b100
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::Trigger {
                pad: 0,
                which: 0,
                effect: vec![1]
            },
            t
        ));
        assert!(d.should_forward(
            &HidOutput::Trigger {
                pad: 0,
                which: 1,
                effect: vec![2]
            },
            t
        ));

        let out = d.renewals(0, t + Duration::from_millis(1000));
        assert_eq!(
            out.len(),
            4,
            "lightbar + player LEDs + both triggers, got {out:?}"
        );

        // A genuine change re-stamps the clock, so the next renewal is a full window away.
        let later = t + Duration::from_millis(1500);
        assert!(d.should_forward(
            &HidOutput::Led {
                pad: 0,
                r: 1,
                g: 2,
                b: 3
            },
            later
        ));
        assert!(d.renewals(0, later + Duration::from_millis(999)).is_empty());
        assert!(!d
            .renewals(0, later + Duration::from_millis(1000))
            .is_empty());
    }

    /// Nothing latched = nothing to renew; a one-shot pulse must never be replayed as a "repair".
    #[test]
    fn renewal_is_silent_with_nothing_latched_and_never_replays_a_pulse() {
        let t = Instant::now();
        let mut d = HidoutDedup::default();
        assert!(d.renewals(0, t + Duration::from_secs(60)).is_empty());

        let pulse = HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        assert!(d.should_forward(&pulse, t));
        // The pulse stamped the clock but latched no state, so the renewal has nothing to repeat.
        assert!(d.renewals(0, t + Duration::from_millis(1000)).is_empty());
    }
}
