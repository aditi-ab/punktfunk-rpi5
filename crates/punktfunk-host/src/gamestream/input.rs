//! Decode the GameStream input wire format (carried AES-GCM-encrypted on the ENet control
//! stream — see [`super::control`]) into platform-agnostic
//! [`punktfunk_core::input::InputEvent`]s for injection.
//!
//! A decrypted control message is `[u16 type LE][u16 length LE][NV_INPUT packet]`. We only
//! handle the input type (`0x0206`); the packet is an 8-byte `NV_INPUT_HEADER` (`size` BE,
//! `magic` LE) followed by a magic-specific body. Multi-byte body fields are big-endian
//! (network order) except `magic` and the keyboard `keyCode` (little-endian). Struct layouts
//! mirror moonlight-common-c `Input.h`; the magic dispatch matches Sunshine `input.cpp`
//! (Gen5+, where scroll is `0x0A` and controllers are `0x0C`, so there's no ambiguity).

use punktfunk_core::input::{InputEvent, InputKind};

/// Inner control-message type for input (moonlight `packetTypesGen7[IDX_INPUT_DATA]`).
const INPUT_DATA_TYPE: u16 = 0x0206;

// NV_INPUT_HEADER.magic values (Input.h), with the Gen5+ variants where they differ.
const MAGIC_KEY_DOWN: u32 = 0x03;
const MAGIC_KEY_UP: u32 = 0x04;
const MAGIC_MOUSE_ABS: u32 = 0x05;
const MAGIC_MOUSE_REL: u32 = 0x06;
const MAGIC_MOUSE_REL_GEN5: u32 = 0x07;
const MAGIC_MOUSE_BTN_DOWN: u32 = 0x08;
const MAGIC_MOUSE_BTN_UP: u32 = 0x09;
const MAGIC_SCROLL_GEN5: u32 = 0x0A;
const MAGIC_UTF8: u32 = 0x17;
const MAGIC_HSCROLL: u32 = 0x5500_0001;

/// `code` value marking a [`InputKind::MouseScroll`] as horizontal (vs `0` = vertical).
pub const SCROLL_HORIZONTAL: u32 = 1;

/// Decode one decrypted control plaintext into zero or more input events. Non-input control
/// messages (keepalives, QoS) and unhandled input kinds (gamepad/pen/touch) yield nothing.
pub fn decode(plaintext: &[u8]) -> Vec<InputEvent> {
    if plaintext.len() < 4 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return Vec::new();
    }
    decode_input_packet(&plaintext[4..]).into_iter().collect()
}

fn decode_input_packet(p: &[u8]) -> Option<InputEvent> {
    if p.len() < 8 {
        return None;
    }
    // NV_INPUT_HEADER: size (BE u32, excludes itself) + magic (LE u32). Body follows.
    let magic = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let b = &p[8..];
    let be16 = |o: usize| -> Option<i16> { Some(i16::from_be_bytes([*b.get(o)?, *b.get(o + 1)?])) };

    Some(match magic {
        MAGIC_MOUSE_REL | MAGIC_MOUSE_REL_GEN5 => {
            ev(InputKind::MouseMove, 0, be16(0)? as i32, be16(2)? as i32, 0)
        }
        MAGIC_MOUSE_ABS => {
            // short x, y, unused, width, height (all BE). Carry the client's reference extent
            // (width<<16 | height) in `flags` so the injector can scale to its output.
            let (x, y) = (be16(0)? as i32, be16(2)? as i32);
            let flags = ((be16(6)? as u16 as u32) << 16) | (be16(8)? as u16 as u32);
            ev(InputKind::MouseMoveAbs, 0, x, y, flags)
        }
        MAGIC_MOUSE_BTN_DOWN => ev(InputKind::MouseButtonDown, *b.first()? as u32, 0, 0, 0),
        MAGIC_MOUSE_BTN_UP => ev(InputKind::MouseButtonUp, *b.first()? as u32, 0, 0, 0),
        MAGIC_SCROLL_GEN5 => ev(InputKind::MouseScroll, 0, be16(0)? as i32, 0, 0),
        MAGIC_HSCROLL => ev(
            InputKind::MouseScroll,
            SCROLL_HORIZONTAL,
            be16(0)? as i32,
            0,
            0,
        ),
        MAGIC_KEY_DOWN | MAGIC_KEY_UP => {
            // char flags, short keyCode (LE), char modifiers, short zero2. The client stuffs a
            // 0x80 high byte on key-down; Sunshine masks to the low-byte VK (`& 0xFF`).
            let key_code = (u16::from_le_bytes([*b.get(1)?, *b.get(2)?]) & 0x00FF) as u32;
            let modifiers = *b.get(3)? as u32;
            let kind = if magic == MAGIC_KEY_DOWN {
                InputKind::KeyDown
            } else {
                InputKind::KeyUp
            };
            ev(kind, key_code, 0, 0, modifiers)
        }
        // UTF-8 text, gamepad, pen, touch, haptics — not yet injected.
        _ => return None,
    })
}

fn ev(kind: InputKind, code: u32, x: i32, y: i32, flags: u32) -> InputEvent {
    InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a control plaintext: inner header + NV_INPUT_HEADER + body.
    fn wrap(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut inp = Vec::new();
        inp.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes()); // size (excl. itself)
        inp.extend_from_slice(&magic.to_le_bytes());
        inp.extend_from_slice(body);
        let mut pt = Vec::new();
        pt.extend_from_slice(&INPUT_DATA_TYPE.to_le_bytes());
        pt.extend_from_slice(&(inp.len() as u16).to_le_bytes());
        pt.extend_from_slice(&inp);
        pt
    }

    #[test]
    fn decodes_relative_mouse() {
        // deltaX = -1 (ffff BE), deltaY = +2 (0002 BE) — matches a real captured packet.
        let pt = wrap(MAGIC_MOUSE_REL_GEN5, &[0xff, 0xff, 0x00, 0x02]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::MouseMove);
        assert_eq!((ev[0].x, ev[0].y), (-1, 2));
    }

    #[test]
    fn decodes_key_down_masking_high_byte() {
        // keyCode 0x80A4 (LE a4 80) → VK 0xA4 (VK_LMENU); modifiers 0x04 (Alt).
        let pt = wrap(MAGIC_KEY_DOWN, &[0x00, 0xa4, 0x80, 0x04, 0x00, 0x00]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::KeyDown);
        assert_eq!(ev[0].code, 0xA4);
        assert_eq!(ev[0].flags, 0x04);
    }

    #[test]
    fn ignores_non_input_type() {
        let mut pt = vec![0x00, 0x02]; // type 0x0200 (keepalive)
        pt.extend_from_slice(&[0x08, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode(&pt).is_empty());
    }
}
