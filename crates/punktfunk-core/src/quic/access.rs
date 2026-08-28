//! Per-client access grants — the shared vocabulary of `design/per-client-access.md` §3.
//!
//! Trust used to be binary: a paired device got *everything*, forever. Grants split that into
//! six capabilities a device may hold (a guest pad that can't type over the owner's desktop,
//! a TV that can't read the clipboard), carried as a `u32` bitmask that is the SAME value on
//! the wire ([`Welcome`](super::Welcome) advert, [`AccessUpdate`](super::AccessUpdate)) and in
//! the host's trust store — no translation layer to drift. Reserved bits must be zero; the
//! management API rejects masks with unknown bits set, and hosts never emit them.
//!
//! The host is the only enforcer: nothing here appears in any client→host message, so nothing
//! a client sends can widen its grants. Client-side use of the mask (capture gating, the
//! "Controller only" chip) is courtesy UX over the same vocabulary.
//!
//! [`classify`] is the default-deny mechanism for the input plane: an exhaustive, non-wildcard
//! match from [`InputKind`] to [`GrantClass`], shared by the host's datagram filter and the
//! clients' capture gates. A future `InputKind` that nobody classified does not compile —
//! the compiler, not a code review, keeps a new event kind from slipping past the filter.

use crate::input::InputKind;

/// Controller input: gamepad button/axis/snapshot/remove/arrival events, plus everything that
/// rides with a pad — rich DualSense input (0xCC motion/touchpad), pad-audio, rumble return,
/// and virtual-pad creation itself (deny-at-setup: no bit, no uinput node).
pub const GRANT_GAMEPAD: u32 = 1 << 0;
/// Pointing input: mouse rel/abs + buttons, scroll, touch, and the pen plane.
pub const GRANT_POINTER: u32 = 1 << 1;
/// Key input: key down/up and IME-committed text.
pub const GRANT_KEYBOARD: u32 = 1 << 2;
/// Shared clipboard — ANDed into the operator clipboard policy, never overriding it.
pub const GRANT_CLIPBOARD: u32 = 1 << 3;
/// Mic injection: the mic datagram plane + the per-session mic-service attach.
pub const GRANT_MIC: u32 = 1 << 4;
/// Library launch: `Hello.launch` resolution (and any future in-session launch/end verbs).
pub const GRANT_LAUNCH: u32 = 1 << 5;
/// Host power: invoking the `power.*` host actions (sleep/reboot/shutdown) over the mgmt cert
/// lane (`design/host-actions.md` §4). Route-gated like `CLIPBOARD`/`MIC`/`LAUNCH` — no
/// datagram ever carries it, so [`classify`] is untouched. Machine power ONLY: future
/// plugin/custom actions get their own class, never this bit.
pub const GRANT_POWER: u32 = 1 << 6;

/// Every defined grant. Also the value an *absent* mask means — a record from before grants
/// existed (or an old host's Welcome that omits the field) is full control, so existing
/// pairings keep today's behavior.
pub const GRANT_ALL: u32 = GRANT_GAMEPAD
    | GRANT_POINTER
    | GRANT_KEYBOARD
    | GRANT_CLIPBOARD
    | GRANT_MIC
    | GRANT_LAUNCH
    | GRANT_POWER;

/// [`GRANT_ALL`] as it was before [`GRANT_POWER`] existed (hosts ≤ 0.32.x) — the mask an
/// explicitly saved "Full control" wrote back then. See [`normalize_legacy_full`].
pub const GRANT_ALL_PRE_POWER: u32 =
    GRANT_GAMEPAD | GRANT_POINTER | GRANT_KEYBOARD | GRANT_CLIPBOARD | GRANT_MIC | GRANT_LAUNCH;

/// The legacy-full read rule (`design/host-actions.md` §4.3): a mask that is EXACTLY the
/// pre-power [`GRANT_ALL_PRE_POWER`] was written by "Full control" before the Power bit existed
/// — read it as the current [`GRANT_ALL`], so an old explicit-Full record neither renders as
/// "Custom" nor silently lacks Power while looking Full. Not an escalation: that mask holds
/// `KEYBOARD`+`POINTER`, which already reach the streamed desktop's power menu (§4.2). The
/// deliberate consequence: "everything except Power" is not an expressible stored mask — by
/// design, because it would be a lock painted on an open door. Any other mask passes through.
pub fn normalize_legacy_full(mask: u32) -> u32 {
    if mask == GRANT_ALL_PRE_POWER {
        GRANT_ALL
    } else {
        mask
    }
}

/// The reserved-must-be-zero region: a mask with any of these bits set is invalid today and is
/// rejected at the management API (never silently cleared — the caller meant *something* this
/// host doesn't understand, and clearing would grant less than they asked for without saying so).
pub const GRANT_RESERVED: u32 = !GRANT_ALL;

/// Preset: **Full control** — all bits; today's behavior and the default for absent grants.
pub const GRANT_PRESET_FULL: u32 = GRANT_ALL;
/// Preset: **Controller only** — the guest/co-play preset. Deliberately excludes `LAUNCH`
/// (design §11 D2: in co-play the owner drives what runs).
pub const GRANT_PRESET_CONTROLLER_ONLY: u32 = GRANT_GAMEPAD;
/// Preset: **View only** — spectator; sees and hears the stream, sends nothing.
pub const GRANT_PRESET_VIEW_ONLY: u32 = 0;

/// The grant a piece of traffic needs — one variant per [`GRANT_GAMEPAD`]-family bit.
/// [`classify`] maps every input event onto the first three; the last three name the
/// plane/message gates (clipboard coordinator, mic attach, `Hello.launch`) so their
/// drop counters and log lines share this vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantClass {
    Gamepad,
    Pointer,
    Keyboard,
    Clipboard,
    Mic,
    Launch,
    /// The `power.*` host actions (route-gated on the mgmt cert lane; never an input event).
    Power,
}

impl GrantClass {
    /// The grant bit that authorizes this class — the mask test is
    /// `grants & class.bit() != 0`.
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

/// Which grant an input event needs before it may reach the injector.
///
/// Exhaustive and wildcard-free ON PURPOSE — this match IS the default-deny mechanism
/// (design §5.3): adding an [`InputKind`] without deciding its grant class is a compile
/// error here, not a filter hole in the field. Do not "fix" a build break by adding a
/// `_ =>` arm; classify the new kind.
///
/// Only the `0xC8` event vocabulary routes through here. The mic (`0xCA`), rich-input
/// (`0xCC`) and pen planes are gated by their *plane* tag before per-event decode — their
/// classes are [`GrantClass::Mic`], [`GrantClass::Gamepad`] and [`GrantClass::Pointer`]
/// by construction.
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
        // Full = everything (Power included — host-actions §4.2); Controller-only = pad bit
        // ONLY (no LAUNCH — §11 D2, and certainly no POWER); View = nothing.
        assert_eq!(GRANT_PRESET_FULL, GRANT_ALL);
        assert_eq!(GRANT_PRESET_FULL & GRANT_POWER, GRANT_POWER);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY, GRANT_GAMEPAD);
        assert_eq!(GRANT_PRESET_CONTROLLER_ONLY & GRANT_LAUNCH, 0);
        assert_eq!(GRANT_PRESET_VIEW_ONLY, 0);
    }

    #[test]
    fn legacy_full_reads_as_the_current_full() {
        // Exactly the pre-power full mask (an explicitly saved "Full control" from ≤ 0.32.x)
        // normalizes to today's GRANT_ALL — anything else, limited or already-current, passes
        // through untouched (host-actions §4.3).
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
        // Walk the whole wire vocabulary via from_u8, so a new kind added to the enum AND the
        // decoder shows up here too (the classify match itself already breaks the build).
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
