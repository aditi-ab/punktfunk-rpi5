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
    /// Absolute motion: `x`/`y` carry pixel coordinates.
    MouseMoveAbs = 3,
    MouseButtonDown = 4,
    MouseButtonUp = 5,
    /// `x` carries the (signed) scroll delta.
    MouseScroll = 6,
    GamepadButton = 7,
    /// `code` = axis id, `x` = axis value.
    GamepadAxis = 8,
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
            _ => return None,
        })
    }
}

/// A single input event. `#[repr(C)]` — shared verbatim with the C ABI as
/// `LumenInputEvent`.
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
}
