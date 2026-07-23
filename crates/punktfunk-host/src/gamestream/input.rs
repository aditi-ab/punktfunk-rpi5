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
const MAGIC_SS_TOUCH: u32 = 0x5500_0002;
const MAGIC_SS_PEN: u32 = 0x5500_0003;

/// `code` value marking a [`InputKind::MouseScroll`] as horizontal (vs `0` = vertical).
pub const SCROLL_HORIZONTAL: u32 = 1;

/// Decode one decrypted control plaintext into zero or more input events. Non-input control
/// messages (keepalives, QoS) and unhandled input kinds (gamepad/pen/touch) yield nothing.
pub fn decode(plaintext: &[u8]) -> Vec<InputEvent> {
    if plaintext.len() < 4 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return Vec::new();
    }
    let p = &plaintext[4..];
    // UTF-8 text (Moonlight's client-side keyboard commit) expands to one `TextInput` event per
    // Unicode scalar — the only magic yielding more than one event, so it's handled before the
    // single-event dispatch. Injected the same way as the native plane's IME text.
    if p.len() >= 8 && u32::from_le_bytes([p[4], p[5], p[6], p[7]]) == MAGIC_UTF8 {
        // NV_INPUT_HEADER.size (BE, excludes itself) counts magic + body.
        let size = u32::from_be_bytes([p[0], p[1], p[2], p[3]]) as usize;
        let body_len = size.saturating_sub(4).min(p.len() - 8);
        return match std::str::from_utf8(&p[8..8 + body_len]) {
            Ok(s) => s
                .chars()
                .filter(|c| !c.is_control())
                .map(|c| ev(InputKind::TextInput, c as u32, 0, 0, 0))
                .collect(),
            Err(_) => Vec::new(),
        };
    }
    decode_input_packet(p).into_iter().collect()
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
            // Moonlight VKs are LAYOUT-SEMANTIC (the client's layout already resolved them) —
            // tag them so the Windows injector maps them under the receiving app's layout
            // instead of the fixed US-positional table the first-party clients use.
            let key_code = (u16::from_le_bytes([*b.get(1)?, *b.get(2)?]) & 0x00FF) as u32;
            let modifiers = *b.get(3)? as u32;
            let kind = if magic == MAGIC_KEY_DOWN {
                InputKind::KeyDown
            } else {
                InputKind::KeyUp
            };
            ev(
                kind,
                key_code,
                0,
                0,
                modifiers | crate::inject::KEY_FLAG_SEMANTIC_VK,
            )
        }
        // Gamepad, pen, touch, haptics — not yet injected. (UTF-8 text is handled in `decode`.)
        _ => return None,
    })
}

/// One decoded `SS_PEN_PACKET` body (moonlight-common-c `Input.h`; all fields little-endian,
/// coordinates/pressure as normalized floats). Semantics — `pressure_or_distance` is pressure
/// (0..1, 0 = unknown) for contact events and hover distance (1 = farthest) for hover;
/// `rotation` is the tilt azimuth (0..360, `0xFFFF` unknown); `tilt` is degrees from the
/// surface normal (0..90, `0xFF` unknown) — are interpreted in [`super::pen`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsPen {
    pub event_type: u8,
    pub tool: u8,
    pub buttons: u8,
    pub x: f32,
    pub y: f32,
    pub pressure_or_distance: f32,
    pub rotation: u16,
    pub tilt: u8,
}

/// One decoded `SS_TOUCH_PACKET` body (same conventions as [`SsPen`]; contact-area fields are
/// not carried — the wire touch kinds have nowhere to put them yet).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SsTouch {
    pub event_type: u8,
    pub rotation: u16,
    pub pointer_id: u32,
    pub x: f32,
    pub y: f32,
    pub pressure_or_distance: f32,
}

/// A Sunshine-extension pointer event (sent only after we advertise
/// `SS_FF_PEN_TOUCH_EVENTS` — see [`super::rtsp`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SsPointer {
    Pen(SsPen),
    Touch(SsTouch),
}

/// Whether this control plaintext carries a pointer magic ([`decode_pointer`]'s domain) —
/// lets the caller tell "malformed pointer packet" (worth a warn) apart from "some other
/// message" when `decode_pointer` returns `None`.
pub fn is_pointer_magic(plaintext: &[u8]) -> bool {
    plaintext.len() >= 12
        && u16::from_le_bytes([plaintext[0], plaintext[1]]) == INPUT_DATA_TYPE
        && matches!(
            u32::from_le_bytes([plaintext[8], plaintext[9], plaintext[10], plaintext[11]]),
            MAGIC_SS_TOUCH | MAGIC_SS_PEN
        )
}

/// Decode a control plaintext into a pen/touch pointer event, or `None` for every other
/// message (the caller then falls through to [`decode`]). Bounds- and sanity-checked like the
/// rest of the plane: short bodies and non-finite floats (a forged NaN must never reach the
/// injectors' scaling) drop the packet whole.
pub fn decode_pointer(plaintext: &[u8]) -> Option<SsPointer> {
    if plaintext.len() < 12 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return None;
    }
    let p = &plaintext[4..];
    let magic = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let b = &p[8..];
    let f32at = |o: usize| -> Option<f32> {
        let v = f32::from_le_bytes([*b.get(o)?, *b.get(o + 1)?, *b.get(o + 2)?, *b.get(o + 3)?]);
        v.is_finite().then_some(v)
    };
    match magic {
        // eventType, zero[1], rotation u16, pointerId u32, x, y, pressureOrDistance, areas.
        MAGIC_SS_TOUCH => Some(SsPointer::Touch(SsTouch {
            event_type: *b.first()?,
            rotation: u16::from_le_bytes([*b.get(2)?, *b.get(3)?]),
            pointer_id: u32::from_le_bytes([*b.get(4)?, *b.get(5)?, *b.get(6)?, *b.get(7)?]),
            x: f32at(8)?,
            y: f32at(12)?,
            pressure_or_distance: f32at(16)?,
        })),
        // eventType, toolType, penButtons, zero[1], x, y, pressureOrDistance, rotation u16,
        // tilt, zero2[1], areas.
        MAGIC_SS_PEN => Some(SsPointer::Pen(SsPen {
            event_type: *b.first()?,
            tool: *b.get(1)?,
            buttons: *b.get(2)?,
            x: f32at(4)?,
            y: f32at(8)?,
            pressure_or_distance: f32at(12)?,
            rotation: u16::from_le_bytes([*b.get(16)?, *b.get(17)?]),
            tilt: *b.get(18)?,
        })),
        _ => None,
    }
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
        // keyCode 0x80A4 (LE a4 80) → VK 0xA4 (VK_LMENU); modifiers 0x04 (Alt). GameStream keys
        // are additionally tagged layout-semantic (Moonlight resolved the VK under its layout).
        let pt = wrap(MAGIC_KEY_DOWN, &[0x00, 0xa4, 0x80, 0x04, 0x00, 0x00]);
        let ev = decode(&pt);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, InputKind::KeyDown);
        assert_eq!(ev[0].code, 0xA4);
        assert_eq!(ev[0].flags, 0x04 | crate::inject::KEY_FLAG_SEMANTIC_VK);
    }

    #[test]
    fn decodes_utf8_text_per_scalar() {
        // "aß😀" — ASCII, Latin-1, and an astral scalar; one TextInput event per scalar.
        let pt = wrap(MAGIC_UTF8, "aß😀".as_bytes());
        let ev = decode(&pt);
        assert_eq!(ev.len(), 3);
        assert!(ev.iter().all(|e| e.kind == InputKind::TextInput));
        assert_eq!(ev[0].code, 'a' as u32);
        assert_eq!(ev[1].code, 'ß' as u32);
        assert_eq!(ev[2].code, 0x1F600);
        // Truncated / invalid UTF-8 decodes to nothing rather than mojibake.
        let bad = wrap(MAGIC_UTF8, &[0xff, 0xfe]);
        assert!(decode(&bad).is_empty());
    }

    #[test]
    fn decodes_ss_pen_and_touch_golden_bytes() {
        // SS_PEN body per Input.h: DOWN, pen tool, primary button, x=0.5 y=0.25,
        // pressure=0.75, rotation=180, tilt=45, then contact areas (present but ignored).
        let mut body = vec![0x01, 0x01, 0x01, 0x00];
        for f in [0.5f32, 0.25, 0.75] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        body.extend_from_slice(&180u16.to_le_bytes());
        body.extend_from_slice(&[45, 0x00]);
        for f in [0.0f32, 0.0] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        let pt = wrap(0x5500_0003, &body);
        assert_eq!(
            decode_pointer(&pt),
            Some(SsPointer::Pen(SsPen {
                event_type: 0x01,
                tool: 0x01,
                buttons: 0x01,
                x: 0.5,
                y: 0.25,
                pressure_or_distance: 0.75,
                rotation: 180,
                tilt: 45,
            }))
        );
        // A pen packet is invisible to the classic decoder (no misparse as mouse/key).
        assert!(decode(&pt).is_empty());

        // SS_TOUCH body: MOVE, rotation unknown, pointerId 42, x=1.0 y=0.0, pressure 1.0.
        let mut body = vec![0x03, 0x00];
        body.extend_from_slice(&0xFFFFu16.to_le_bytes());
        body.extend_from_slice(&42u32.to_le_bytes());
        for f in [1.0f32, 0.0, 1.0, 0.0, 0.0] {
            body.extend_from_slice(&f.to_le_bytes());
        }
        let pt = wrap(0x5500_0002, &body);
        assert_eq!(
            decode_pointer(&pt),
            Some(SsPointer::Touch(SsTouch {
                event_type: 0x03,
                rotation: 0xFFFF,
                pointer_id: 42,
                x: 1.0,
                y: 0.0,
                pressure_or_distance: 1.0,
            }))
        );

        // Truncated bodies and forged NaN coordinates drop the packet whole.
        assert_eq!(decode_pointer(&pt[..pt.len() - 18]), None);
        let mut nan = body.clone();
        nan[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(decode_pointer(&wrap(0x5500_0002, &nan)), None);
        // Non-pointer magics fall through to the classic decoder.
        assert_eq!(
            decode_pointer(&wrap(MAGIC_MOUSE_REL_GEN5, &[0, 0, 0, 0])),
            None
        );
    }

    #[test]
    fn ignores_non_input_type() {
        let mut pt = vec![0x00, 0x02]; // type 0x0200 (keepalive)
        pt.extend_from_slice(&[0x08, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0]);
        assert!(decode(&pt).is_empty());
    }
}
