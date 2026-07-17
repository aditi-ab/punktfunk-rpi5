//! Pure fallback-remap policy for the Steam Controller / Steam Deck rich inputs when the resolved
//! host backend is **not** the virtual `hid-steam` device (DualSense / DualShock 4 / Xbox), so a
//! client's Steam-only inputs aren't silently dropped — plus the cross-device motion rescale the
//! Deck backend itself needs.
//!
//! Driven by the host's `PUNKTFUNK_STEAM_REMAP` env (`key=value`, `,`/`;`-separated, e.g.
//! `paddles=stickclicks`). No I/O beyond [`RemapConfig::from_env`]; everything else is pure +
//! unit-testable. The uinput Xbox pad already exposes the back grips as Elite paddles
//! (`BTN_TRIGGER_HAPPY5-8`), so only the slot-less DualSense / DS4 backends fold them.

use punktfunk_core::input::gamepad as gs;

/// Where the four Steam back grips go on a backend with no native back-button HID slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PaddleFallback {
    /// Drop them — the back buttons are simply absent on this pad. The honest default: don't fire
    /// buttons the user didn't ask for. Set the env to map them instead.
    #[default]
    Drop,
    /// L4/L5 → left-stick click, R4/R5 → right-stick click.
    StickClicks,
    /// L4/L5 → left bumper, R4/R5 → right bumper.
    Shoulders,
}

/// Fallback-remap knobs parsed from `PUNKTFUNK_STEAM_REMAP`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemapConfig {
    pub paddles: PaddleFallback,
}

impl RemapConfig {
    /// Parse the host's `PUNKTFUNK_STEAM_REMAP` env (absent / unrecognized → defaults).
    pub fn from_env() -> RemapConfig {
        std::env::var("PUNKTFUNK_STEAM_REMAP")
            .map(|s| RemapConfig::parse(&s))
            .unwrap_or_default()
    }

    /// Pure parse of the `key=value[,key=value…]` string (the testable core of [`from_env`]).
    pub fn parse(s: &str) -> RemapConfig {
        let mut cfg = RemapConfig::default();
        for kv in s.split([',', ';']) {
            let mut it = kv.splitn(2, '=');
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if k.trim().eq_ignore_ascii_case("paddles") {
                    cfg.paddles = match v.trim().to_ascii_lowercase().as_str() {
                        "stickclicks" | "l3r3" | "sticks" => PaddleFallback::StickClicks,
                        "shoulders" | "lbrb" | "bumpers" => PaddleFallback::Shoulders,
                        _ => PaddleFallback::Drop,
                    };
                }
            }
        }
        cfg
    }
}

/// Fold the wire back-grip bits (`BTN_PADDLE1..4`) into standard buttons per `policy` for a pad with
/// no native back-button slot, clearing the paddle bits. Pure. PADDLE1/2/3/4 = R4/L4/R5/L5.
pub fn fold_paddles(mut buttons: u32, policy: PaddleFallback) -> u32 {
    let left = buttons & (gs::BTN_PADDLE2 | gs::BTN_PADDLE4) != 0; // L4 | L5
    let right = buttons & (gs::BTN_PADDLE1 | gs::BTN_PADDLE3) != 0; // R4 | R5
    buttons &= !(gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4);
    let (lbit, rbit) = match policy {
        PaddleFallback::Drop => return buttons,
        PaddleFallback::StickClicks => (gs::BTN_LS_CLICK, gs::BTN_RS_CLICK),
        PaddleFallback::Shoulders => (gs::BTN_LB, gs::BTN_RB),
    };
    if left {
        buttons |= lbit;
    }
    if right {
        buttons |= rbit;
    }
    buttons
}

// Motion rescale. The wire uses the DualSense convention (20 LSB/°·s gyro, 10000 LSB/g accel — the
// scale every client capture applies). The Steam Deck's `hid-steam` report wants 16 LSB/°·s and
// 16384 LSB/g, so the Deck backend rescales; the DualSense / DS4 backends consume the wire 1:1.
const GYRO_NUM: i32 = 16;
const GYRO_DEN: i32 = 20;
const ACCEL_NUM: i32 = 16384;
const ACCEL_DEN: i32 = 10000;

fn scale(v: i16, num: i32, den: i32) -> i16 {
    ((v as i32 * num) / den).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Rescale a wire (DualSense-convention) motion sample into the Steam Deck's `hid-steam` units.
pub fn motion_wire_to_deck(gyro: [i16; 3], accel: [i16; 3]) -> ([i16; 3], [i16; 3]) {
    (
        gyro.map(|g| scale(g, GYRO_NUM, GYRO_DEN)),
        accel.map(|a| scale(a, ACCEL_NUM, ACCEL_DEN)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paddle_policy() {
        assert_eq!(RemapConfig::parse("").paddles, PaddleFallback::Drop);
        assert_eq!(
            RemapConfig::parse("paddles=stickclicks").paddles,
            PaddleFallback::StickClicks
        );
        assert_eq!(
            RemapConfig::parse("foo=bar; paddles = Shoulders").paddles,
            PaddleFallback::Shoulders
        );
        assert_eq!(
            RemapConfig::parse("paddles=nonsense").paddles,
            PaddleFallback::Drop
        );
    }

    #[test]
    fn fold_paddles_maps_and_clears() {
        // All four grips set + a real A button.
        let b = gs::BTN_A | gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4;
        // Drop: paddle bits cleared, A preserved, nothing added.
        assert_eq!(fold_paddles(b, PaddleFallback::Drop), gs::BTN_A);
        // StickClicks: left grips → L3, right grips → R3.
        assert_eq!(
            fold_paddles(b, PaddleFallback::StickClicks),
            gs::BTN_A | gs::BTN_LS_CLICK | gs::BTN_RS_CLICK
        );
        // Only a left grip (L4 = PADDLE2) → only the left bumper under Shoulders.
        assert_eq!(
            fold_paddles(gs::BTN_PADDLE2, PaddleFallback::Shoulders),
            gs::BTN_LB
        );
    }

    #[test]
    fn motion_rescale_to_deck_units() {
        // gyro × 16/20 = 0.8; accel × 16384/10000 = 1.6384.
        let (g, a) = motion_wire_to_deck([1000, -2000, 0], [10000, -5000, 0]);
        assert_eq!(g, [800, -1600, 0]);
        assert_eq!(a, [16384, -8192, 0]);
        // Saturates rather than wraps.
        let (_, a) = motion_wire_to_deck([0; 3], [32767, i16::MIN, 0]);
        assert_eq!(a[0], i16::MAX);
        assert_eq!(a[1], i16::MIN);
    }
}
