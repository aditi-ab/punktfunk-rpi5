//! What a forwarded pad is told, over plain values. [`rumble_verdict`] decides a wire rumble
//! command, [`hid_plan`] a host HID output. The worker looks up the slot, asks, and performs
//! the answer on its SDL handle.
//!
//! No SDL type, so the table tests below run on the Mac and pin every row.

use super::ds5::Ds5Feedback;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::quic::HidOutput;

/// `audio_caps` bit0: the pad renders the host's haptics audio through its coils.
const CAP_HAPTICS: u8 = 0x01;

/// A DualSense or its Edge: the pads that take raw effect packets for LEDs and triggers.
pub(super) fn is_dualsense(pref: GamepadPref) -> bool {
    matches!(pref, GamepadPref::DualSense | GamepadPref::DualSenseEdge)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RumbleVerdict {
    /// Rumble is off, or the host's raw writes own the motors.
    Drop,
    /// The haptics stream carries this pad's feedback.
    Suppress,
    /// Play it. `mute_coils`: SDL's rumble sets `ucEnableBits1` `0x01|0x02`, which mutes the
    /// audio-haptics coils until they are armed again.
    Play { mute_coils: bool },
}

/// One wire rumble command for a slot. A title that renders no haptics audio sends no frames
/// (`haptics_live` stays false), so it keeps its rumble.
pub(super) fn rumble_verdict(
    rumble_on: bool,
    raw_rumble: bool,
    audio_caps: u8,
    haptics_live: bool,
) -> RumbleVerdict {
    if !rumble_on || raw_rumble {
        return RumbleVerdict::Drop;
    }
    let haptics = audio_caps & CAP_HAPTICS != 0;
    if haptics && haptics_live {
        RumbleVerdict::Suppress
    } else {
        RumbleVerdict::Play {
            mute_coils: haptics,
        }
    }
}

/// What the slot's pad is, as far as a HID output cares.
#[derive(Clone, Copy)]
pub(super) struct HidTarget {
    pub(super) dualsense: bool,
    /// Nonzero only for a tier-A pad with a live pad-audio renderer.
    pub(super) audio_caps: u8,
    /// A raw Steam Controller capture is open.
    pub(super) sc2: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HidPlan {
    /// A DualSense effects packet for SDL's raw-effect call.
    Effect([u8; 47]),
    /// SDL's lightbar call, for a pad that is not a DualSense.
    Led(u8, u8, u8),
    /// The player-indicator bits through SDL's player index, for a pad that is not a DualSense.
    PlayerLeds(u8),
    /// A raw report for the Steam Controller capture.
    Raw { kind: u8, data: Vec<u8> },
    /// This pad has nowhere to put it.
    Ignore,
}

/// One host HID output for a pad. Audio controls reach only a pad with a live tier-A
/// renderer: replaying volumes at a silent pad would mute or blast the next session's start.
pub(super) fn hid_plan(hid: HidOutput, t: HidTarget) -> HidPlan {
    let ds = t.dualsense;
    match hid {
        HidOutput::Led { r, g, b, .. } if ds => {
            HidPlan::Effect(Ds5Feedback::lightbar_packet(r, g, b))
        }
        HidOutput::Led { r, g, b, .. } => HidPlan::Led(r, g, b),
        HidOutput::PlayerLeds { bits, .. } if ds => {
            HidPlan::Effect(Ds5Feedback::player_packet(bits))
        }
        HidOutput::PlayerLeds { bits, .. } => HidPlan::PlayerLeds(bits),
        HidOutput::MicLed { mode, .. } if ds => HidPlan::Effect(Ds5Feedback::mic_led_packet(mode)),
        HidOutput::Trigger { which, effect, .. } if ds => {
            HidPlan::Effect(Ds5Feedback::trigger_packet(which, &effect))
        }
        HidOutput::AudioCtl { flags, raw, .. } if ds && t.audio_caps != 0 => {
            HidPlan::Effect(Ds5Feedback::audio_ctl_packet(flags, &raw))
        }
        HidOutput::HidRaw { kind, data, .. } if t.sc2 => HidPlan::Raw { kind, data },
        HidOutput::HidRaw { .. }
        | HidOutput::Trigger { .. }
        | HidOutput::TrackpadHaptic { .. }
        | HidOutput::AudioCtl { .. }
        | HidOutput::MicLed { .. } => HidPlan::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HidPlan::*;
    use RumbleVerdict::*;

    const DS: HidTarget = HidTarget {
        dualsense: true,
        audio_caps: 0,
        sc2: false,
    };
    const XBOX: HidTarget = HidTarget {
        dualsense: false,
        audio_caps: 0,
        sc2: false,
    };

    #[test]
    fn the_dualsense_family_takes_effect_packets() {
        use GamepadPref as P;
        assert!(is_dualsense(P::DualSense) && is_dualsense(P::DualSenseEdge));
        for other in [
            P::Xbox360,
            P::DualShock4,
            P::SteamDeck,
            P::SteamController,
            P::Auto,
        ] {
            assert!(!is_dualsense(other), "{other:?}");
        }
    }

    /// (rumble on, raw writes, `audio_caps`, haptics arriving) → what happens to the command.
    #[test]
    fn rumble_gives_way_to_live_haptics_and_to_raw_writes() {
        let rows = [
            ((true, false, 0, false), Play { mute_coils: false }),
            // Without the haptics cap the live flag means nothing.
            ((true, false, 0, true), Play { mute_coils: false }),
            // A title that sends no haptics frames keeps its rumble, and the coils are muted.
            ((true, false, 1, false), Play { mute_coils: true }),
            ((true, false, 1, true), Suppress),
            ((true, false, 3, true), Suppress),
            // Speaker alone leaves the coils to rumble.
            ((true, false, 2, true), Play { mute_coils: false }),
            ((false, false, 0, false), Drop),
            ((false, false, 1, true), Drop),
            ((true, true, 0, false), Drop),
            ((true, true, 1, true), Drop),
        ];
        for ((on, raw, caps, live), want) in rows {
            assert_eq!(
                rumble_verdict(on, raw, caps, live),
                want,
                "{on} {raw} {caps} {live}"
            );
        }
    }

    #[test]
    fn a_dualsense_takes_leds_and_triggers_as_effect_packets() {
        let led = HidOutput::Led {
            pad: 0,
            r: 1,
            g: 2,
            b: 3,
        };
        assert_eq!(
            hid_plan(led, DS),
            Effect(Ds5Feedback::lightbar_packet(1, 2, 3))
        );
        let player = HidOutput::PlayerLeds { pad: 0, bits: 0x15 };
        assert_eq!(
            hid_plan(player, DS),
            Effect(Ds5Feedback::player_packet(0x15))
        );
        let mic = HidOutput::MicLed { pad: 0, mode: 2 };
        assert_eq!(hid_plan(mic, DS), Effect(Ds5Feedback::mic_led_packet(2)));
        let trigger = HidOutput::Trigger {
            pad: 0,
            which: 1,
            effect: vec![2, 7, 9],
        };
        assert_eq!(
            hid_plan(trigger, DS),
            Effect(Ds5Feedback::trigger_packet(1, &[2, 7, 9]))
        );
    }

    #[test]
    fn any_other_pad_takes_leds_through_sdl_and_drops_the_rest() {
        let led = HidOutput::Led {
            pad: 0,
            r: 1,
            g: 2,
            b: 3,
        };
        assert_eq!(hid_plan(led, XBOX), Led(1, 2, 3));
        let player = HidOutput::PlayerLeds { pad: 0, bits: 0x15 };
        assert_eq!(hid_plan(player, XBOX), PlayerLeds(0x15));
        let mic = HidOutput::MicLed { pad: 0, mode: 2 };
        assert_eq!(hid_plan(mic, XBOX), Ignore);
        let trigger = HidOutput::Trigger {
            pad: 0,
            which: 0,
            effect: vec![1],
        };
        assert_eq!(hid_plan(trigger, XBOX), Ignore);
    }

    /// Volumes replay only at a pad with a live tier-A renderer.
    #[test]
    fn audio_controls_need_a_dualsense_with_a_renderer() {
        let ctl = || HidOutput::AudioCtl {
            pad: 0,
            flags: 0b101,
            raw: [1, 2, 3, 4, 5, 6],
        };
        let tier_a = HidTarget {
            audio_caps: 1,
            ..DS
        };
        assert_eq!(
            hid_plan(ctl(), tier_a),
            Effect(Ds5Feedback::audio_ctl_packet(0b101, &[1, 2, 3, 4, 5, 6]))
        );
        assert_eq!(hid_plan(ctl(), DS), Ignore);
        assert_eq!(
            hid_plan(
                ctl(),
                HidTarget {
                    audio_caps: 1,
                    ..XBOX
                }
            ),
            Ignore
        );
    }

    #[test]
    fn raw_reports_need_a_steam_controller_capture() {
        let raw = || HidOutput::HidRaw {
            pad: 0,
            kind: 1,
            data: vec![0x80, 7],
        };
        let sc2 = HidTarget { sc2: true, ..XBOX };
        assert_eq!(
            hid_plan(raw(), sc2),
            Raw {
                kind: 1,
                data: vec![0x80, 7]
            }
        );
        assert_eq!(
            hid_plan(raw(), HidTarget { sc2: true, ..DS }),
            Raw {
                kind: 1,
                data: vec![0x80, 7]
            }
        );
        assert_eq!(hid_plan(raw(), XBOX), Ignore);
    }

    /// The tag is allocated on the wire, and no client renders it.
    #[test]
    fn a_trackpad_pulse_goes_nowhere() {
        let pulse = || HidOutput::TrackpadHaptic {
            pad: 0,
            side: 0,
            amplitude: 1,
            period: 2,
            count: 3,
        };
        let sc2 = HidTarget { sc2: true, ..DS };
        for t in [DS, XBOX, sc2] {
            assert_eq!(hid_plan(pulse(), t), Ignore);
        }
    }
}
