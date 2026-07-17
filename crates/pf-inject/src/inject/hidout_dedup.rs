//! Per-pad dedup for the rich HID-output feedback plane (0xCD), carved out of `dualsense_proto`
//! (plan §W4 — it is device-agnostic, shared by the DualSense/DS4/Deck managers via
//! [`crate::uhid_manager`], not DualSense-specific). A game bundles rumble + lightbar +
//! LEDs + adaptive triggers into one output report, so a merely-rumbling pad re-sends unchanged
//! rich state every report; this forwards only genuine changes (one-shot pulses always fire).

use punktfunk_core::quic::HidOutput;

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
}

impl HidoutDedup {
    /// Forget all remembered state — call when a pad is created or unplugged so the first feedback
    /// after a (re)connect is always forwarded.
    pub fn clear(&mut self) {
        *self = HidoutDedup::default();
    }

    /// Whether `h` should be forwarded: `true` for a genuine change (remembering the new value) or a
    /// one-shot pulse; `false` if it repeats the last-forwarded value for its kind.
    pub fn should_forward(&mut self, h: &HidOutput) -> bool {
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
        let mut d = HidoutDedup::default();
        let led = |r| HidOutput::Led {
            pad: 0,
            r,
            g: 0,
            b: 0,
        };
        // First value forwards; an exact repeat is dropped; a change forwards again.
        assert!(d.should_forward(&led(10)));
        assert!(!d.should_forward(&led(10)));
        assert!(d.should_forward(&led(20)));

        // Player LEDs dedup on their own field, independent of the lightbar.
        let pl = |bits| HidOutput::PlayerLeds { pad: 0, bits };
        assert!(d.should_forward(&pl(0b101)));
        assert!(!d.should_forward(&pl(0b101)));
        assert!(!d.should_forward(&led(20))); // lightbar still unchanged

        // The two adaptive triggers (L2=0, R2=1) are tracked separately.
        let trig = |which, byte| HidOutput::Trigger {
            pad: 0,
            which,
            effect: vec![byte, 0, 0],
        };
        assert!(d.should_forward(&trig(0, 1)));
        assert!(d.should_forward(&trig(1, 1))); // same bytes, other side → still forwards
        assert!(!d.should_forward(&trig(0, 1)));
        assert!(d.should_forward(&trig(0, 2))); // L2 effect changed

        // One-shot haptic pulses are never deduped.
        let haptic = HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        assert!(d.should_forward(&haptic));
        assert!(d.should_forward(&haptic));

        // `clear` re-arms every kind.
        d.clear();
        assert!(d.should_forward(&led(20)));
        assert!(d.should_forward(&pl(0b101)));
        assert!(d.should_forward(&trig(0, 2)));
    }
}
