//! Input events flowing client → host (and the host-side receive callback).
//!
//! Input rides the same transport as video but on its own wire tag
//! ([`INPUT_MAGIC`]), so a session can demultiplex video from input by the first byte.

/// Wire tag distinguishing an input datagram from a video packet.
pub const INPUT_MAGIC: u8 = 0xC8;

/// Fixed serialized size of an [`InputEvent`] on the wire (tag + fields).
pub const INPUT_WIRE_LEN: usize = 1 + 1 + 4 + 4 + 4 + 4; // = 18

/// Kinds of input event. `#[repr(u8)]` so it crosses the C ABI as a byte tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    KeyDown = 0,
    KeyUp = 1,
    /// Relative motion: `x`/`y` carry `dx`/`dy`.
    MouseMove = 2,
    /// Absolute motion: `x`/`y` carry pixel coordinates and `flags` packs the client's
    /// coordinate-space size as `(width << 16) | height` (the same contract as
    /// [`TouchDown`](Self::TouchDown)) — injectors normalize against it before mapping
    /// into the output region and **drop the event when it is zero**.
    MouseMoveAbs = 3,
    MouseButtonDown = 4,
    MouseButtonUp = 5,
    /// `x` carries the (signed) scroll delta.
    MouseScroll = 6,
    /// `code` = button bit ([`gamepad`] `BTN_*`), `x` ≠ 0 = pressed, `flags` = pad index.
    GamepadButton = 7,
    /// `code` = axis id ([`gamepad`] `AXIS_*`), `x` = axis value, `flags` = pad index.
    /// Sticks are i16 range (−32768..32767) in the XInput/Moonlight convention — **+y =
    /// up** (unlike mouse coordinates); triggers 0..255.
    GamepadAxis = 8,
    /// Touch begins. `code` = touch id (which finger; reusable after `TouchUp`), `x`/`y` =
    /// pixel coordinates and `flags` = `(width << 16) | height` of the client's touch surface
    /// — the same absolute mapping as [`MouseMoveAbs`](Self::MouseMoveAbs).
    TouchDown = 9,
    /// Touch moves. Same field meaning as [`TouchDown`](Self::TouchDown).
    TouchMove = 10,
    /// Touch ends. Only `code` (the touch id) is used.
    TouchUp = 11,
    /// Full gamepad state in one event ([`GamepadSnapshot`]) — idempotent, sequence-numbered.
    ///
    /// The per-transition [`GamepadButton`](Self::GamepadButton)/[`GamepadAxis`](Self::GamepadAxis)
    /// events are fragile on the unreliable datagram plane: a dropped or reordered event corrupts
    /// the host's accumulated pad state until the *next* change (a held trigger stays wrong
    /// indefinitely). A snapshot carries the whole pad, so loss heals on the next send and the
    /// sequence number lets the host drop stale reorders — the same idempotent-state discipline
    /// as the host→client rumble refresh. Sent only when the host advertised
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE); older hosts keep
    /// receiving the per-transition events.
    GamepadState = 12,
    /// A pad was unplugged client-side (the native plane's answer to GameStream's
    /// `activeGamepadMask`, which the per-transition/snapshot planes otherwise lack — see
    /// [`encode_gamepad_remove`]). `flags` packs `seq << 24 | pad`: the low byte is the pad
    /// index, the high byte a per-pad wrapping seq sharing the [`GamepadSnapshot`] sequence
    /// space. The host clears the pad's `active_mask` bit so its virtual device is torn down,
    /// seq-gated against snapshots so one the network reordered past the removal can't resurrect
    /// the pad, and the shared seq space keeps the same index reusable by a later re-plug. Sent
    /// only to a host that advertised [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE);
    /// an older host ignores the unknown tag (the pad then lingers until session end — the
    /// pre-existing behaviour).
    GamepadRemove = 13,
    /// Declares which controller KIND a pad presents so a session can MIX types (pad 0 a
    /// DualSense, pad 1 an Xbox pad). `code` = the [`GamepadPref`](crate::config::GamepadPref)
    /// wire byte, `flags` = pad index. Sent when the client opens a pad slot — before that pad's
    /// first input — and re-sent a few times against datagram loss (like [`GamepadRemove`]). The
    /// host resolves the kind to a buildable backend and routes that pad's virtual device to it; a
    /// pad the client never declares (an older client, or a fully-lost declaration) falls back to
    /// the session-default kind from the handshake. Idempotent (no seq): re-declaring the same kind
    /// is a no-op. Meaningful only to a host that advertised
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE); an older host ignores the
    /// unknown tag (every pad then uses the session-default kind — the pre-existing behaviour).
    GamepadArrival = 14,
}

/// Pack a [`InputKind::GamepadRemove`] `flags` word (`seq << 24 | pad`) — the same low-byte-pad /
/// high-byte-seq layout as [`GamepadSnapshot::to_event`], so a removal seq-gates against snapshots.
pub fn encode_gamepad_remove(pad: u8, seq: u8) -> u32 {
    ((seq as u32) << 24) | (pad as u32)
}

/// Unpack a [`InputKind::GamepadRemove`] `flags` word into `(pad, seq)`.
pub fn decode_gamepad_remove(flags: u32) -> (u8, u8) {
    (flags as u8, (flags >> 24) as u8)
}

/// The gamepad wire contract for [`InputKind::GamepadButton`]/[`InputKind::GamepadAxis`].
///
/// Everything follows the GameStream/XInput conventions end to end: buttons reuse
/// GameStream's `buttonFlags` bit positions, sticks are −32768..32767 with **+y = up**,
/// triggers 0..255 (what Moonlight sends and what the host's virtual xpad already
/// consumes). One event carries one transition: `code` = the bit below, `x` = 1 pressed /
/// 0 released. Axes are sent individually; the host accumulates per-pad state and emits
/// one evdev SYN per event.
pub mod gamepad {
    pub const BTN_DPAD_UP: u32 = 0x0001;
    pub const BTN_DPAD_DOWN: u32 = 0x0002;
    pub const BTN_DPAD_LEFT: u32 = 0x0004;
    pub const BTN_DPAD_RIGHT: u32 = 0x0008;
    pub const BTN_START: u32 = 0x0010;
    pub const BTN_BACK: u32 = 0x0020;
    pub const BTN_LS_CLICK: u32 = 0x0040;
    pub const BTN_RS_CLICK: u32 = 0x0080;
    pub const BTN_LB: u32 = 0x0100;
    pub const BTN_RB: u32 = 0x0200;
    pub const BTN_GUIDE: u32 = 0x0400;
    pub const BTN_A: u32 = 0x1000;
    pub const BTN_B: u32 = 0x2000;
    pub const BTN_X: u32 = 0x4000;
    pub const BTN_Y: u32 = 0x8000;
    // Extended buttons in Moonlight's `buttonFlags2 << 16` namespace (see `gamestream/gamepad.rs`),
    // so the GameStream paddle path and the native path share one host injector map. The four Steam
    // Deck back grips (L4/L5/R4/R5) reuse the four GameStream/Xbox-Elite paddle slots — a semantic
    // 1:1 for binding (the device identity carries the glyph distinction).
    /// Back grip R4 — SDL `RightPaddle1` / GameStream `PADDLE1`.
    pub const BTN_PADDLE1: u32 = 0x0001_0000;
    /// Back grip L4 — SDL `LeftPaddle1` / GameStream `PADDLE2`.
    pub const BTN_PADDLE2: u32 = 0x0002_0000;
    /// Back grip R5 — SDL `RightPaddle2` / GameStream `PADDLE3`.
    pub const BTN_PADDLE3: u32 = 0x0004_0000;
    /// Back grip L5 — SDL `LeftPaddle2` / GameStream `PADDLE4`.
    pub const BTN_PADDLE4: u32 = 0x0008_0000;
    /// DualSense touchpad click. Moonlight's extended-button position (`buttonFlags2`
    /// merges in at `<< 16`, see `gamestream/gamepad.rs`), so GameStream clients land on
    /// the same bit. Only the DualSense backend renders it; the xpad has no such button.
    pub const BTN_TOUCHPAD: u32 = 0x10_0000;
    /// Misc / capture button — the Deck `…`/quick-access, Share/Capture / GameStream `MISC`.
    pub const BTN_MISC1: u32 = 0x0020_0000;

    /// Axis ids for `InputKind::GamepadAxis`.
    pub const AXIS_LS_X: u32 = 0;
    pub const AXIS_LS_Y: u32 = 1;
    pub const AXIS_RS_X: u32 = 2;
    pub const AXIS_RS_Y: u32 = 3;
    /// Triggers: value range 0..255.
    pub const AXIS_LT: u32 = 4;
    pub const AXIS_RT: u32 = 5;
}

impl InputKind {
    pub fn from_u8(v: u8) -> Option<InputKind> {
        use InputKind::*;
        Some(match v {
            0 => KeyDown,
            1 => KeyUp,
            2 => MouseMove,
            3 => MouseMoveAbs,
            4 => MouseButtonDown,
            5 => MouseButtonUp,
            6 => MouseScroll,
            7 => GamepadButton,
            8 => GamepadAxis,
            9 => TouchDown,
            10 => TouchMove,
            11 => TouchUp,
            12 => GamepadState,
            13 => GamepadRemove,
            14 => GamepadArrival,
            _ => return None,
        })
    }
}

/// The number of gamepads addressable on the wire (`flags` pad index 0..15). Shared by the
/// client's snapshot fold and the host's per-pad accumulators.
pub const MAX_PADS: usize = 16;

/// One pad's complete state, packed into a single [`InputKind::GamepadState`] event — the
/// whole 18-byte wire layout is reused, nothing is appended:
///
/// - `code`  = `buttons` (the [`gamepad`] `BTN_*` bitmask, extended bits included)
/// - `x`     = `ls_x << 16 | ls_y` (two i16 halves, wire stick convention: **+y = up**)
/// - `y`     = `rs_x << 16 | rs_y`
/// - `flags` = `seq << 24 | left_trigger << 16 | right_trigger << 8 | pad`
///
/// `seq` is a per-pad wrapping u8, bumped on every send (changes *and* refreshes); the host
/// applies a snapshot only when `seq` is newer than the last applied one (wrapping i8
/// compare), so a datagram the network reordered can't roll held state backwards. The wrap
/// window (128 sends) dwarfs any real reorder window, and the client's periodic refresh
/// heals the pathological case anyway.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadSnapshot {
    /// Pad index 0..[`MAX_PADS`].
    pub pad: u8,
    /// Wrapping send counter (see the type docs for the reorder gate).
    pub seq: u8,
    /// [`gamepad`] `BTN_*` bitmask.
    pub buttons: u32,
    /// Triggers 0..255 (the [`gamepad::AXIS_LT`]/[`gamepad::AXIS_RT`] convention).
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks −32768..32767, **+y = up** (the wire convention).
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

impl GamepadSnapshot {
    /// Pack into the fixed [`InputEvent`] layout (kind = [`InputKind::GamepadState`]).
    pub fn to_event(&self) -> InputEvent {
        InputEvent {
            kind: InputKind::GamepadState,
            _pad: [0; 3],
            code: self.buttons,
            x: ((self.ls_x as u16 as i32) << 16) | (self.ls_y as u16 as i32),
            y: ((self.rs_x as u16 as i32) << 16) | (self.rs_y as u16 as i32),
            flags: ((self.seq as u32) << 24)
                | ((self.left_trigger as u32) << 16)
                | ((self.right_trigger as u32) << 8)
                | (self.pad as u32),
        }
    }

    /// Unpack from a [`InputKind::GamepadState`] event; `None` for any other kind.
    pub fn from_event(ev: &InputEvent) -> Option<GamepadSnapshot> {
        if ev.kind != InputKind::GamepadState {
            return None;
        }
        Some(GamepadSnapshot {
            pad: ev.flags as u8,
            seq: (ev.flags >> 24) as u8,
            buttons: ev.code,
            left_trigger: (ev.flags >> 16) as u8,
            right_trigger: (ev.flags >> 8) as u8,
            ls_x: (ev.x >> 16) as i16,
            ls_y: ev.x as i16,
            rs_x: (ev.y >> 16) as i16,
            rs_y: ev.y as i16,
        })
    }

    /// Fold one per-transition [`GamepadButton`](InputKind::GamepadButton) /
    /// [`GamepadAxis`](InputKind::GamepadAxis) event into this snapshot (`seq`/`pad` untouched).
    /// `false` = not a foldable event / unknown axis id (snapshot unchanged).
    pub fn fold(&mut self, ev: &InputEvent) -> bool {
        match ev.kind {
            InputKind::GamepadButton => {
                if ev.x != 0 {
                    self.buttons |= ev.code;
                } else {
                    self.buttons &= !ev.code;
                }
                true
            }
            InputKind::GamepadAxis => {
                let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                let trigger = ev.x.clamp(0, 255) as u8;
                match ev.code {
                    gamepad::AXIS_LS_X => self.ls_x = stick,
                    gamepad::AXIS_LS_Y => self.ls_y = stick,
                    gamepad::AXIS_RS_X => self.rs_x = stick,
                    gamepad::AXIS_RS_Y => self.rs_y = stick,
                    gamepad::AXIS_LT => self.left_trigger = trigger,
                    gamepad::AXIS_RT => self.right_trigger = trigger,
                    _ => return false,
                }
                true
            }
            _ => false,
        }
    }

    /// True when `seq` supersedes `last` (wrapping u8 distance, forward window of 127) — the
    /// host's reorder gate. `None` (nothing applied yet) always accepts.
    pub fn seq_newer(seq: u8, last: Option<u8>) -> bool {
        match last {
            None => true,
            Some(l) => (seq.wrapping_sub(l) as i8) > 0,
        }
    }
}

/// A single input event. `#[repr(C)]` — shared verbatim with the C ABI as
/// `PunktfunkInputEvent`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub _pad: [u8; 3],
    /// keycode / button id / axis id, depending on `kind`.
    pub code: u32,
    /// x / dx / abs-x / axis-value / scroll-delta, depending on `kind`.
    pub x: i32,
    /// y / dy / abs-y, depending on `kind`.
    pub y: i32,
    /// modifier bitmask or gamepad index.
    pub flags: u32,
}

impl InputEvent {
    /// Serialize to the fixed wire layout (`INPUT_MAGIC` + little-endian fields).
    pub fn encode(&self) -> [u8; INPUT_WIRE_LEN] {
        let mut b = [0u8; INPUT_WIRE_LEN];
        b[0] = INPUT_MAGIC;
        b[1] = self.kind as u8;
        b[2..6].copy_from_slice(&self.code.to_le_bytes());
        b[6..10].copy_from_slice(&self.x.to_le_bytes());
        b[10..14].copy_from_slice(&self.y.to_le_bytes());
        b[14..18].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    /// Parse from the wire layout. Returns `None` on bad tag/length/kind.
    pub fn decode(buf: &[u8]) -> Option<InputEvent> {
        if buf.len() < INPUT_WIRE_LEN || buf[0] != INPUT_MAGIC {
            return None;
        }
        let kind = InputKind::from_u8(buf[1])?;
        Some(InputEvent {
            kind,
            _pad: [0; 3],
            code: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
            x: i32::from_le_bytes(buf[6..10].try_into().unwrap()),
            y: i32::from_le_bytes(buf[10..14].try_into().unwrap()),
            flags: u32::from_le_bytes(buf[14..18].try_into().unwrap()),
        })
    }
}

/// One decoded GameStream (Moonlight-plane) controller event. Shared vocabulary: the host's
/// GameStream/Moonlight decode path produces these, and the platform-neutral input injectors
/// (`pf-inject`) consume them — so the type lives in `core::input`, below both, rather than in
/// either plane. The `buttons` bitmask uses the same [`gamepad`] `BTN_*` layout as the native
/// [`GamepadSnapshot`] (GameStream's `buttonFlags | buttonFlags2 << 16` is bit-identical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadEvent {
    /// Full state of one controller + the set of attached controllers.
    State(GamepadFrame),
    /// Sunshine arrival metadata (precedes the first State for that pad).
    Arrival {
        index: u8,
        /// 0 unknown, 1 xbox, 2 ps, 3 nintendo.
        kind: u8,
        /// LI_CCAP_* bits (0x02 = rumble).
        capabilities: u16,
    },
}

/// Snapshot of one controller's inputs (Moonlight conventions: sticks −32768..32767 with +Y
/// up, triggers 0..255, buttons = `buttonFlags | buttonFlags2 << 16`). The decoded-frame twin of
/// [`GamepadSnapshot`] on the GameStream/Moonlight plane; see [`GamepadEvent`] for why it lives here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadFrame {
    pub index: i16,
    /// Bit n set = controller n attached; a clear bit for an allocated pad means unplug.
    pub active_mask: u16,
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_wire_roundtrip() {
        let e = InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: -12,
            y: 34,
            flags: 0xABCD,
        };
        assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        assert!(InputEvent::decode(&[0u8; INPUT_WIRE_LEN]).is_none()); // bad magic
    }

    #[test]
    fn touch_kinds_roundtrip() {
        for kind in [
            InputKind::TouchDown,
            InputKind::TouchMove,
            InputKind::TouchUp,
        ] {
            assert_eq!(InputKind::from_u8(kind as u8), Some(kind));
            let e = InputEvent {
                kind,
                _pad: [0; 3],
                code: 2, // touch id
                x: 640,
                y: 360,
                flags: (1280u32 << 16) | 720, // client surface w/h
            };
            assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        }
        // GamepadRemove + GamepadArrival are valid kinds; 15 (one past them) is not.
        assert_eq!(InputKind::from_u8(13), Some(InputKind::GamepadRemove));
        assert_eq!(InputKind::from_u8(14), Some(InputKind::GamepadArrival));
        assert_eq!(InputKind::from_u8(15), None);
    }

    #[test]
    fn gamepad_remove_flags_roundtrip() {
        for (pad, seq) in [(0u8, 0u8), (3, 200), (15, 255), (7, 1)] {
            let flags = encode_gamepad_remove(pad, seq);
            assert_eq!(decode_gamepad_remove(flags), (pad, seq));
        }
        // Layout matches the snapshot's pad/seq packing (low byte pad, high byte seq).
        let snap = GamepadSnapshot {
            pad: 9,
            seq: 123,
            ..Default::default()
        };
        let (pad, seq) = decode_gamepad_remove(snap.to_event().flags);
        assert_eq!((pad, seq), (9, 123));
    }

    #[test]
    fn gamepad_snapshot_roundtrip() {
        let s = GamepadSnapshot {
            pad: 3,
            seq: 200,
            buttons: gamepad::BTN_A | gamepad::BTN_PADDLE4 | gamepad::BTN_MISC1,
            left_trigger: 255,
            right_trigger: 1,
            ls_x: -32768,
            ls_y: 32767,
            rs_x: -1,
            rs_y: 12345,
        };
        let ev = s.to_event();
        assert_eq!(ev.kind, InputKind::GamepadState);
        // Survives the wire encode/decode unchanged.
        let dec = InputEvent::decode(&ev.encode()).unwrap();
        assert_eq!(GamepadSnapshot::from_event(&dec), Some(s));
        // Non-snapshot kinds unpack to None.
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 255,
            y: 0,
            flags: 0,
        };
        assert_eq!(GamepadSnapshot::from_event(&axis), None);
    }

    #[test]
    fn gamepad_snapshot_fold() {
        let mut s = GamepadSnapshot::default();
        let ev = |kind: InputKind, code: u32, x: i32| InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: 0,
        };
        // Button down/up sets and clears its bit.
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 1)));
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_RB, 1)));
        assert_eq!(s.buttons, gamepad::BTN_A | gamepad::BTN_RB);
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 0)));
        assert_eq!(s.buttons, gamepad::BTN_RB);
        // Axes land in their slots; triggers clamp to 0..255, sticks to i16.
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LT, 300)));
        assert_eq!(s.left_trigger, 255);
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LS_Y, -40000)));
        assert_eq!(s.ls_y, i16::MIN);
        // Unknown axis / unrelated kind leave the snapshot untouched.
        assert!(!s.fold(&ev(InputKind::GamepadAxis, 99, 1)));
        assert!(!s.fold(&ev(InputKind::KeyDown, 30, 1)));
    }

    #[test]
    fn gamepad_snapshot_seq_gate() {
        // First snapshot always applies.
        assert!(GamepadSnapshot::seq_newer(0, None));
        // Strictly newer within the forward window applies; equal/older doesn't.
        assert!(GamepadSnapshot::seq_newer(6, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(5, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(4, Some(5)));
        // Wraps: 2 supersedes 250 (forward distance 8), not the reverse.
        assert!(GamepadSnapshot::seq_newer(2, Some(250)));
        assert!(!GamepadSnapshot::seq_newer(250, Some(2)));
        // Exactly half the window away is treated as stale (i8 > 0 excludes -128).
        assert!(!GamepadSnapshot::seq_newer(133, Some(5)));
    }
}
