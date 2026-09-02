//! Per-client access grants: `u32` bitmask shared by the wire and the host
//! trust store (`design/per-client-access.md`).
//!
//! [`Welcome`](super::Welcome) and [`AccessUpdate`](super::AccessUpdate) carry
//! the same value as the registry — no translation. Reserved bits must be
//! zero: the management API rejects unknown bits; hosts never emit them.
//! An omitted mask is [`GRANT_ALL`].
//!
//! [`classify`] is the input-plane table: exhaustive [`InputKind`] →
//! [`GrantClass`], no wildcard. Clipboard, mic, launch, and power are
//! plane and route gates, not `0xC8` events.
//!
//! Tests in this file pin the bit layout, presets, legacy-full read, and
//! the classifier table.

use crate::input::InputKind;

/// DualSense `0xCC`, pad-audio, rumble, and virtual-pad creation (no bit, no uinput node).
pub const GRANT_GAMEPAD: u32 = 1 << 0;
/// Mouse, scroll, touch, and the pen plane.
pub const GRANT_POINTER: u32 = 1 << 1;
/// Key down/up and IME-committed text.
pub const GRANT_KEYBOARD: u32 = 1 << 2;
/// Clipboard coordinator. ANDed with the operator clipboard policy; never overrides it.
pub const GRANT_CLIPBOARD: u32 = 1 << 3;
/// Mic datagram plane and the per-session mic-service attach.
pub const GRANT_MIC: u32 = 1 << 4;
/// `Hello.launch` resolution.
pub const GRANT_LAUNCH: u32 = 1 << 5;
/// `power.*` (sleep/reboot/shutdown) on the mgmt cert lane (`design/host-actions.md`).
/// Not a datagram; [`classify`] is untouched. Machine power only — never plugin actions.
pub const GRANT_POWER: u32 = 1 << 6;

/// An omitted Welcome or registry mask reads as this.
pub const GRANT_ALL: u32 = GRANT_GAMEPAD
    | GRANT_POINTER
    | GRANT_KEYBOARD
    | GRANT_CLIPBOARD
    | GRANT_MIC
    | GRANT_LAUNCH
    | GRANT_POWER;

/// Stored "Full control" before [`GRANT_POWER`]. [`normalize_legacy_full`] lifts it.
pub const GRANT_ALL_PRE_POWER: u32 =
    GRANT_GAMEPAD | GRANT_POINTER | GRANT_KEYBOARD | GRANT_CLIPBOARD | GRANT_MIC | GRANT_LAUNCH;

/// Exact [`GRANT_ALL_PRE_POWER`] → [`GRANT_ALL`]. Other masks pass through.
/// That stored Full already has `KEYBOARD`+`POINTER` (desktop power menu), so
/// "everything except Power" is not an expressible stored mask.
pub fn normalize_legacy_full(mask: u32) -> u32 {
    if mask == GRANT_ALL_PRE_POWER {
        GRANT_ALL
    } else {
        mask
    }
}

/// The management API rejects these; it never silently clears unknown bits
/// (that would grant less than the caller asked).
pub const GRANT_RESERVED: u32 = !GRANT_ALL;

/// UI preset "Full control".
pub const GRANT_PRESET_FULL: u32 = GRANT_ALL;
/// UI preset "Controller only". No `LAUNCH` — the owner picks what runs.
pub const GRANT_PRESET_CONTROLLER_ONLY: u32 = GRANT_GAMEPAD;
/// UI preset "View only" — the spectator sends nothing.
pub const GRANT_PRESET_VIEW_ONLY: u32 = 0;

/// [`classify`] covers `0xC8` events; Clipboard/Mic/Launch/Power name the
/// plane and route gates so drop counters share this vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantClass {
    Gamepad,
    Pointer,
    Keyboard,
    Clipboard,
    Mic,
    Launch,
    /// `power.*` on the mgmt cert lane — never an input event.
    Power,
}

impl GrantClass {
    pub fn bit(self) -> u32 {
        match self {
            Self::Gamepad => GRANT_GAMEPAD,
            Self::Pointer => GRANT_POINTER,
            Self::Keyboard => GRANT_KEYBOARD,
            Self::Clipboard => GRANT_CLIPBOARD,
            Self::Mic => GRANT_MIC,
            Self::Launch => GRANT_LAUNCH,
            Self::Power => GRANT_POWER,
        }
    }
}

/// Grant class for one `0xC8` input event.
///
/// Exhaustive, no wildcard: a new [`InputKind`] is a compile error until
/// classified. Do not add `_ =>`. Mic (`0xCA`), DualSense (`0xCC`), and pen
/// are plane-gated before decode (Mic / Gamepad / Pointer by construction).
pub fn classify(kind: InputKind) -> GrantClass {
    match kind {
        InputKind::KeyDown | InputKind::KeyUp | InputKind::TextInput => GrantClass::Keyboard,
        InputKind::MouseMove
        | InputKind::MouseMoveAbs
        | InputKind::MouseButtonDown
        | InputKind::MouseButtonUp
        | InputKind::MouseScroll
        | InputKind::TouchDown
        | InputKind::TouchMove
        | InputKind::TouchUp => GrantClass::Pointer,
        InputKind::GamepadButton
        | InputKind::GamepadAxis
        | InputKind::GamepadState
        | InputKind::GamepadRemove
        | InputKind::GamepadArrival => GrantClass::Gamepad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_disjoint_and_all_covers_exactly_them() {
        let bits = [
            GRANT_GAMEPAD,
            GRANT_POINTER,
            GRANT_KEYBOARD,
            GRANT_CLIPBOARD,
            GRANT_MIC,
            GRANT_LAUNCH,
            GRANT_POWER,
        ];
        let mut acc = 0u32;
        for b in bits {
            assert_eq!(b.count_ones(), 1);
            assert_eq!(acc & b, 0, "overlapping grant bits");
            acc |= b;
        }
        assert_eq!(acc, GRANT_ALL);
        assert_eq!(GRANT_ALL & GRANT_RESERVED, 0);
        assert_eq!(GRANT_ALL | GRANT_RESERVED, u32::MAX);
    }

    #[test]
    fn presets_match_the_design() {
        assert_eq!(GRANT_PRESET_FULL, GRANT_ALL);
        assert_eq!(GRANT_PRESET_FULL & GRANT_POWER, GRANT_POWER);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY, GRANT_GAMEPAD);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY & GRANT_LAUNCH, 0);
        assert_eq!(GRANT_PRESET_VIEW_ONLY, 0);
    }

    #[test]
    fn legacy_full_reads_as_the_current_full() {
        assert_eq!(GRANT_ALL_PRE_POWER, 0x3F);
        assert_eq!(normalize_legacy_full(GRANT_ALL_PRE_POWER), GRANT_ALL);
        assert_eq!(normalize_legacy_full(GRANT_ALL), GRANT_ALL);
        assert_eq!(normalize_legacy_full(GRANT_GAMEPAD), GRANT_GAMEPAD);
        assert_eq!(normalize_legacy_full(0), 0);
        let limited = GRANT_ALL_PRE_POWER & !GRANT_KEYBOARD;
        assert_eq!(normalize_legacy_full(limited), limited);
    }

    #[test]
    fn every_input_kind_classifies_per_the_design_table() {
        use GrantClass::*;
        // from_u8, not the enum: a kind in the decoder but not classify still fails here.
        let mut seen = 0;
        for v in 0..=u8::MAX {
            let Some(kind) = InputKind::from_u8(v) else {
                continue;
            };
            seen += 1;
            let want = match kind {
                InputKind::KeyDown | InputKind::KeyUp | InputKind::TextInput => Keyboard,
                InputKind::GamepadButton
                | InputKind::GamepadAxis
                | InputKind::GamepadState
                | InputKind::GamepadRemove
                | InputKind::GamepadArrival => Gamepad,
                _ => Pointer,
            };
            assert_eq!(classify(kind), want, "kind {kind:?}");
        }
        assert_eq!(
            seen, 16,
            "InputKind wire vocabulary grew — classify the new kind"
        );
    }

    #[test]
    fn class_bits_round_onto_the_grant_consts() {
        assert_eq!(GrantClass::Gamepad.bit(), GRANT_GAMEPAD);
        assert_eq!(GrantClass::Pointer.bit(), GRANT_POINTER);
        assert_eq!(GrantClass::Keyboard.bit(), GRANT_KEYBOARD);
        assert_eq!(GrantClass::Clipboard.bit(), GRANT_CLIPBOARD);
        assert_eq!(GrantClass::Mic.bit(), GRANT_MIC);
        assert_eq!(GrantClass::Launch.bit(), GRANT_LAUNCH);
        assert_eq!(GrantClass::Power.bit(), GRANT_POWER);
    }
}
