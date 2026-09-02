//! Client → host input events, plus the GameStream decoded-frame vocabulary injectors share.
//!
//! Input rides the same datagram plane as video, tagged [`INPUT_MAGIC`] so a session
//! demultiplexes by the first byte. Every native event is a fixed [`INPUT_WIRE_LEN`]-byte
//! little-endian [`InputEvent`] (`#[repr(C)]` as `PunktfunkInputEvent`). Variant field
//! packing lives on [`InputKind`]. Capability-gated tags (`GamepadState`/`Remove`/`Arrival`,
//! `TextInput`) are ignored by hosts that never advertised the matching `HOST_CAP_*`.
//!
//! Motion units and the rest-pose accel are pinned by `pf-inject`'s `motion_contract` test
//! against [`gamepad::MOTION_GYRO_LSB_PER_DEG_S`] / [`gamepad::MOTION_NEUTRAL_ACCEL`].

/// Wire tag: input datagram vs video packet.
pub const INPUT_MAGIC: u8 = 0xC8;

/// Serialized [`InputEvent`] size (tag + fields). The C struct is larger (`_pad`).
pub const INPUT_WIRE_LEN: usize = 1 + 1 + 4 + 4 + 4 + 4;

/// `#[repr(u8)]` so the C ABI sees a byte tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    KeyDown = 0,
    KeyUp = 1,
    /// Relative motion: `x`/`y` carry `dx`/`dy`.
    MouseMove = 2,
    /// Absolute: `x`/`y` pixels, `flags` = `(width << 16) | height` (same as
    /// [`TouchDown`](Self::TouchDown)). Injectors drop the event when flags is 0.
    MouseMoveAbs = 3,
    MouseButtonDown = 4,
    MouseButtonUp = 5,
    /// `x` carries the (signed) scroll delta.
    MouseScroll = 6,
    /// `code` = button bit ([`gamepad`] `BTN_*`), `x` ≠ 0 = pressed, `flags` = pad index.
    GamepadButton = 7,
    /// `code` = [`gamepad`] `AXIS_*`, `x` = value, `flags` = pad. Sticks i16 with **+y =
    /// up** (unlike mouse); triggers 0..255.
    GamepadAxis = 8,
    /// `code` = touch id (reusable after [`TouchUp`](Self::TouchUp)), `x`/`y` pixels,
    /// `flags` = `(width << 16) | height` — same absolute mapping as [`MouseMoveAbs`](Self::MouseMoveAbs).
    TouchDown = 9,
    /// Same field meaning as [`TouchDown`](Self::TouchDown).
    TouchMove = 10,
    /// Only `code` (the touch id) is used.
    TouchUp = 11,
    /// Full pad in one event ([`GamepadSnapshot`]). A dropped transition corrupts
    /// accumulated host state until the next change; a snapshot heals on the next send
    /// and `seq` drops reorders. Sent only when the host advertised
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE); older hosts keep
    /// the per-transition events.
    GamepadState = 12,
    /// Pad unplugged. `flags` = [`encode_gamepad_remove`] (`seq << 24 | pad`, shared
    /// seq space with [`GamepadSnapshot`]) so a snapshot reordered past the removal
    /// cannot re-create the pad. Sent only to a
    /// [`HOST_CAP_GAMEPAD_STATE`](crate::quic::HOST_CAP_GAMEPAD_STATE) host; older
    /// hosts ignore the tag and the pad lingers until session end.
    GamepadRemove = 13,
    /// Kind this pad presents (`code` = [`GamepadPref`](crate::config::GamepadPref)
    /// wire byte) so a session can mix types. `flags` = pad in the low byte; bits 8/9
    /// are [`ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`]/[`ARRIVAL_FLAG_PAD_AUDIO_SPEAKER`] and
    /// ride only toward a [`HOST_CAP_PAD_AUDIO`](crate::quic::HOST_CAP_PAD_AUDIO) host
    /// (an older host reads the whole word as the index). Decode with
    /// [`decode_gamepad_arrival`]. Idempotent, no seq. A pad that never arrives
    /// uses the handshake default; older hosts ignore the unknown tag.
    GamepadArrival = 14,
    /// One Unicode scalar of committed text (`code` = the scalar; other fields 0).
    /// Layout-independent VK events cannot express IME commits, so a capable client
    /// sends characters verbatim. Sent only when the host advertised
    /// [`HOST_CAP_TEXT_INPUT`](crate::quic::HOST_CAP_TEXT_INPUT); older hosts ignore
    /// the tag and clients keep best-effort VK synthesis.
    TextInput = 15,
}

/// Pack [`InputKind::GamepadRemove`] `flags` (`seq << 24 | pad`) — same layout as
/// [`GamepadSnapshot::to_event`], so a removal seq-gates against snapshots.
pub fn encode_gamepad_remove(pad: u8, seq: u8) -> u32 {
    ((seq as u32) << 24) | (pad as u32)
}

/// Unpack [`InputKind::GamepadRemove`] `flags` into `(pad, seq)`.
pub fn decode_gamepad_remove(flags: u32) -> (u8, u8) {
    (flags as u8, (flags >> 24) as u8)
}

/// [`InputKind::GamepadArrival`] `flags` bit 8: this pad renders haptics
/// ([`PAD_AUDIO_KIND_HAPTICS`](crate::quic::PAD_AUDIO_KIND_HAPTICS)). Sent only toward
/// a [`HOST_CAP_PAD_AUDIO`](crate::quic::HOST_CAP_PAD_AUDIO) host — an older host
/// reads the whole `flags` word as the index, so a high bit would drop the declaration.
pub const ARRIVAL_FLAG_PAD_AUDIO_HAPTICS: u32 = 1 << 8;
/// [`InputKind::GamepadArrival`] `flags` bit 9: this pad renders speaker
/// ([`PAD_AUDIO_KIND_SPEAKER`](crate::quic::PAD_AUDIO_KIND_SPEAKER)). Same wire
/// discipline as [`ARRIVAL_FLAG_PAD_AUDIO_HAPTICS`].
pub const ARRIVAL_FLAG_PAD_AUDIO_SPEAKER: u32 = 1 << 9;

/// Pack [`InputKind::GamepadArrival`] `flags`: pad in the low byte, `audio_caps`
/// (bit0 = haptics, bit1 = speaker) as bits 8/9. `audio_caps = 0` is byte-identical
/// to the pre-pad-audio wire.
pub fn encode_gamepad_arrival(pad: u8, audio_caps: u8) -> u32 {
    (pad as u32) | (((audio_caps & 0x03) as u32) << 8)
}

/// Unpack [`InputKind::GamepadArrival`] `flags` into `(pad, audio_caps)`. Mask the
/// index (`flags & 0xFF`); taking the whole word turns a capability bit into a
/// phantom pad. `audio_caps` is bits 8/9. An old-format word yields `audio_caps = 0`.
pub fn decode_gamepad_arrival(flags: u32) -> (u8, u8) {
    (flags as u8, ((flags >> 8) & 0x03) as u8)
}

/// Gamepad wire contract for [`InputKind::GamepadButton`]/[`InputKind::GamepadAxis`].
///
/// GameStream/XInput end to end: buttons reuse GameStream `buttonFlags` bit positions,
/// sticks −32768..32767 with **+y = up**, triggers 0..255.
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
    // Moonlight `buttonFlags2 << 16` (see `gamestream/gamepad.rs`) so both planes share
    // one host injector map. Steam Deck L4/L5/R4/R5 reuse the four Elite paddle slots.
    /// Back grip R4 — SDL `RightPaddle1` / GameStream `PADDLE1`.
    pub const BTN_PADDLE1: u32 = 0x0001_0000;
    /// Back grip L4 — SDL `LeftPaddle1` / GameStream `PADDLE2`.
    pub const BTN_PADDLE2: u32 = 0x0002_0000;
    /// Back grip R5 — SDL `RightPaddle2` / GameStream `PADDLE3`.
    pub const BTN_PADDLE3: u32 = 0x0004_0000;
    /// Back grip L5 — SDL `LeftPaddle2` / GameStream `PADDLE4`.
    pub const BTN_PADDLE4: u32 = 0x0008_0000;
    /// DualSense touchpad click. Moonlight `buttonFlags2 << 16` so GameStream clients
    /// land on the same bit. Only the DualSense backend has this button.
    pub const BTN_TOUCHPAD: u32 = 0x10_0000;
    /// Misc / capture — Deck `…`/quick-access, Share/Capture / GameStream `MISC`.
    pub const BTN_MISC1: u32 = 0x0020_0000;

    pub const AXIS_LS_X: u32 = 0;
    pub const AXIS_LS_Y: u32 = 1;
    pub const AXIS_RS_X: u32 = 2;
    pub const AXIS_RS_Y: u32 = 3;
    /// Triggers: value range 0..255.
    pub const AXIS_LT: u32 = 4;
    pub const AXIS_RT: u32 = 5;

    /// Gyro scale: DualSense raw `i16` LSBs per °/s, carried by `RichInput::Motion`.
    /// Saturates at `i16::MAX / 20` ≈ ±1638 °/s (a real DualSense is ±2000). Every
    /// capture path scales *into* these units and every host backend *from* them;
    /// `pf-inject`'s `motion_contract` pins the calibration blobs against this number.
    /// Lifting the clip is a wire-v2 change, not a quiet re-tune.
    pub const MOTION_GYRO_LSB_PER_DEG_S: i32 = 20;
    /// Accel scale: DualSense raw `i16` LSBs per g. Saturates at ±3.28 g (device ±4 g).
    /// Same pin as [`MOTION_GYRO_LSB_PER_DEG_S`].
    pub const MOTION_ACCEL_LSB_PER_G: i32 = 10_000;

    /// Rest pose: 1 g along up (index 1), zeros on the other two. `[0, 0, 0]` is
    /// free-fall, not "no sample". Backends that use different units rescale this
    /// like any other sample (`steam_remap::motion_wire_to_deck`).
    pub const MOTION_NEUTRAL_ACCEL: [i16; 3] = [0, MOTION_ACCEL_LSB_PER_G as i16, 0];
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
            15 => TextInput,
            _ => return None,
        })
    }
}

/// Wire pad index 0..15. Shared by the client's snapshot fold and the host's per-pad
/// accumulators.
pub const MAX_PADS: usize = 16;

/// One pad's complete state packed into a single [`InputKind::GamepadState`] event
/// (the 18-byte layout, nothing appended):
///
/// - `code`  = `buttons` ([`gamepad`] `BTN_*` bitmask, extended bits included)
/// - `x`     = `ls_x << 16 | ls_y` (two i16 halves, **+y = up**)
/// - `y`     = `rs_x << 16 | rs_y`
/// - `flags` = `seq << 24 | left_trigger << 16 | right_trigger << 8 | pad`
///
/// `seq` is a per-pad wrapping u8. The host applies a snapshot only when `seq` is
/// newer (wrapping i8 compare). The wrap window (128 sends) dwarfs any real reorder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamepadSnapshot {
    /// Pad index 0..[`MAX_PADS`].
    pub pad: u8,
    /// Wrapping send counter; host applies only if [`Self::seq_newer`].
    pub seq: u8,
    pub buttons: u32,
    /// Triggers 0..255 (the [`gamepad::AXIS_LT`]/[`gamepad::AXIS_RT`] convention).
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// Sticks −32768..32767, **+y = up**.
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

impl GamepadSnapshot {
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

    /// Fold one [`GamepadButton`](InputKind::GamepadButton) /
    /// [`GamepadAxis`](InputKind::GamepadAxis) into this snapshot (`seq`/`pad` untouched).
    /// `false` = not foldable / unknown axis (snapshot unchanged).
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

    /// True when `seq` supersedes `last` (wrapping u8, forward window of 127).
    /// `None` (nothing applied yet) always accepts.
    pub fn seq_newer(seq: u8, last: Option<u8>) -> bool {
        match last {
            None => true,
            Some(l) => (seq.wrapping_sub(l) as i8) > 0,
        }
    }
}

/// `#[repr(C)]` as `PunktfunkInputEvent`.
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
    /// Serialize: [`INPUT_MAGIC`] + little-endian fields.
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

/// One decoded GameStream (Moonlight-plane) controller event. The host decode path
/// produces these; `pf-inject` consumes them — so the type lives here, below both
/// planes. `buttons` uses the same [`gamepad`] `BTN_*` layout as [`GamepadSnapshot`]
/// (GameStream `buttonFlags | buttonFlags2 << 16` is bit-identical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamepadEvent {
    /// Full state of one controller plus the attached-controller mask.
    State(GamepadFrame),
    /// Sunshine arrival metadata; precedes the first [`Self::State`] for that pad.
    Arrival {
        index: u8,
        /// 0 unknown, 1 xbox, 2 ps, 3 nintendo.
        kind: u8,
        /// LI_CCAP_* bits (0x02 = rumble).
        capabilities: u16,
        /// Pad-audio render caps from a native-plane arrival's `flags` bits 8/9
        /// (see [`decode_gamepad_arrival`]). Not a GameStream LI_CCAP bit — that
        /// lives in `capabilities`. GameStream always sets 0.
        audio_caps: u8,
    },
}

/// One controller's inputs on the GameStream/Moonlight plane (sticks −32768..32767
/// with +Y up, triggers 0..255, buttons = `buttonFlags | buttonFlags2 << 16`).
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
        assert!(InputEvent::decode(&[0u8; INPUT_WIRE_LEN]).is_none());
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
                code: 2,
                x: 640,
                y: 360,
                flags: (1280u32 << 16) | 720,
            };
            assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        }
        // 16 is one past the last valid kind.
        assert_eq!(InputKind::from_u8(13), Some(InputKind::GamepadRemove));
        assert_eq!(InputKind::from_u8(14), Some(InputKind::GamepadArrival));
        assert_eq!(InputKind::from_u8(15), Some(InputKind::TextInput));
        assert_eq!(InputKind::from_u8(16), None);
    }

    #[test]
    fn text_input_roundtrip() {
        for cp in ['a' as u32, 'ß' as u32, '語' as u32, 0x1F600] {
            let e = InputEvent {
                kind: InputKind::TextInput,
                _pad: [0; 3],
                code: cp,
                x: 0,
                y: 0,
                flags: 0,
            };
            assert_eq!(InputEvent::decode(&e.encode()), Some(e));
        }
    }

    #[test]
    fn gamepad_remove_flags_roundtrip() {
        for (pad, seq) in [(0u8, 0u8), (3, 200), (15, 255), (7, 1)] {
            let flags = encode_gamepad_remove(pad, seq);
            assert_eq!(decode_gamepad_remove(flags), (pad, seq));
        }
        // Snapshot pack uses the same low-byte pad / high-byte seq as a removal.
        let snap = GamepadSnapshot {
            pad: 9,
            seq: 123,
            ..Default::default()
        };
        let (pad, seq) = decode_gamepad_remove(snap.to_event().flags);
        assert_eq!((pad, seq), (9, 123));
    }

    #[test]
    fn gamepad_arrival_flags_roundtrip() {
        for (pad, caps) in [(0u8, 0u8), (3, 0b01), (15, 0b10), (7, 0b11)] {
            let flags = encode_gamepad_arrival(pad, caps);
            assert_eq!(decode_gamepad_arrival(flags), (pad, caps));
            assert_eq!(flags & 0xFF, pad as u32);
        }
        assert_eq!(
            encode_gamepad_arrival(2, 0b11),
            2 | ARRIVAL_FLAG_PAD_AUDIO_HAPTICS | ARRIVAL_FLAG_PAD_AUDIO_SPEAKER
        );
        // `audio_caps = 0` is byte-identical to the plain index.
        assert_eq!(encode_gamepad_arrival(5, 0), 5);
        assert_eq!(decode_gamepad_arrival(5), (5, 0));
        // Undefined high bits (a future extension) do not leak into index or caps.
        assert_eq!(
            decode_gamepad_arrival(0xFFFF_0000 | (0b01 << 8) | 9),
            (9, 1)
        );
        // encode masks unknown caps bits so they cannot land in the index space.
        assert_eq!(encode_gamepad_arrival(1, 0xFF), 1 | (0b11 << 8));
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
        let dec = InputEvent::decode(&ev.encode()).unwrap();
        assert_eq!(GamepadSnapshot::from_event(&dec), Some(s));
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
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 1)));
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_RB, 1)));
        assert_eq!(s.buttons, gamepad::BTN_A | gamepad::BTN_RB);
        assert!(s.fold(&ev(InputKind::GamepadButton, gamepad::BTN_A, 0)));
        assert_eq!(s.buttons, gamepad::BTN_RB);
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LT, 300)));
        assert_eq!(s.left_trigger, 255);
        assert!(s.fold(&ev(InputKind::GamepadAxis, gamepad::AXIS_LS_Y, -40000)));
        assert_eq!(s.ls_y, i16::MIN);
        assert!(!s.fold(&ev(InputKind::GamepadAxis, 99, 1)));
        assert!(!s.fold(&ev(InputKind::KeyDown, 30, 1)));
    }

    #[test]
    fn gamepad_snapshot_seq_gate() {
        assert!(GamepadSnapshot::seq_newer(0, None));
        assert!(GamepadSnapshot::seq_newer(6, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(5, Some(5)));
        assert!(!GamepadSnapshot::seq_newer(4, Some(5)));
        assert!(GamepadSnapshot::seq_newer(2, Some(250)));
        assert!(!GamepadSnapshot::seq_newer(250, Some(2)));
        // Distance 128 is stale: wrapping i8 of 128 is -128, and `> 0` excludes it.
        assert!(!GamepadSnapshot::seq_newer(133, Some(5)));
    }
}
