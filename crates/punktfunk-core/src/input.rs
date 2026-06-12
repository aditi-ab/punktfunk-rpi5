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
    /// DualSense touchpad click. Moonlight's extended-button position (`buttonFlags2`
    /// merges in at `<< 16`, see `gamestream/gamepad.rs`), so GameStream clients land on
    /// the same bit. Only the DualSense backend renders it; the xpad has no such button.
    pub const BTN_TOUCHPAD: u32 = 0x10_0000;

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
            _ => return None,
        })
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
        // 12 (one past TouchUp) is not a valid kind.
        assert_eq!(InputKind::from_u8(12), None);
    }
}
