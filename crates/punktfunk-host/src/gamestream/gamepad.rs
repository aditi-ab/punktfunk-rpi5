//! Decode GameStream controller packets (carried on the same encrypted control stream as
//! mouse/keyboard — see [`super::input`]) into [`GamepadFrame`]s for the uinput virtual pads.
//!
//! Layouts mirror moonlight-common-c `Input.h` (all `#pragma pack(1)`; the `size` header field
//! is big-endian, everything else little-endian). We implement the Gen5+ `MULTI_CONTROLLER`
//! event (magic `0x0C`) — the only controller event Sunshine-class hosts receive — plus the
//! Sunshine-extension `CONTROLLER_ARRIVAL` (`0x55000004`). Because our serverinfo advertises a
//! Sunshine appversion (4th component negative), clients also send `buttonFlags2` (paddles /
//! touchpad-click / Share) inside the MC packet.

/// Inner control-message type for input (same as [`super::input`]).
const INPUT_DATA_TYPE: u16 = 0x0206;

/// `NV_INPUT_HEADER.magic` for the Gen5+ multi-controller event.
const MAGIC_MULTI_CONTROLLER: u32 = 0x0C;
/// Sunshine extension: controller arrival metadata (type/capabilities).
const MAGIC_CONTROLLER_ARRIVAL: u32 = 0x5500_0004;

// The decoded controller types ([`GamepadEvent`]/[`GamepadFrame`]) and the pad count
// ([`punktfunk_core::input::MAX_PADS`]) are shared vocabulary between this Moonlight decode path and
// the platform-neutral injectors, so they live in `core::input` (below both) rather than here.
use punktfunk_core::input::{GamepadEvent, GamepadFrame};

// The `BTN_*` button bitmask and axis ids live in `punktfunk_core::input::gamepad` — GameStream's
// `buttonFlags | buttonFlags2 << 16` layout (Limelight.h) is bit-identical to punktfunk's native
// gamepad wire, so the injector button map + hat math source them straight from core (the single
// point of truth). `decode` below merges the two 16-bit halves into `buttons` raw; the exact wire
// values are pinned by `native.rs::gamepad_wire_bits_are_pinned`.

/// Decode one decrypted control plaintext into a controller event, if it is one. Mouse,
/// keyboard, keepalives etc. yield `None` (they're handled by [`super::input::decode`]).
pub fn decode(plaintext: &[u8]) -> Option<GamepadEvent> {
    if plaintext.len() < 4 || u16::from_le_bytes([plaintext[0], plaintext[1]]) != INPUT_DATA_TYPE {
        return None;
    }
    let p = &plaintext[4..];
    if p.len() < 8 {
        return None;
    }
    let magic = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    let b = &p[8..]; // body after NV_INPUT_HEADER
    let le16 = |o: usize| -> Option<i16> { Some(i16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?])) };

    match magic {
        MAGIC_MULTI_CONTROLLER => {
            // Body: headerB@0, controllerNumber@2, activeGamepadMask@4, midB@6, buttonFlags@8,
            // LT@10, RT@11, lsX@12, lsY@14, rsX@16, rsY@18, tailA@20, buttonFlags2@22, tailB@24.
            // The constants (headerB/midB/tail*) are never validated, mirroring Sunshine.
            let buttons_lo = le16(8)? as u16 as u32;
            // buttonFlags2 is absent on pre-extension clients (shorter packet) — treat as 0.
            let buttons_hi = le16(22).map(|v| v as u16 as u32).unwrap_or(0);
            Some(GamepadEvent::State(GamepadFrame {
                index: le16(2)?,
                active_mask: le16(4)? as u16,
                buttons: buttons_lo | (buttons_hi << 16),
                left_trigger: *b.get(10)?,
                right_trigger: *b.get(11)?,
                ls_x: le16(12)?,
                ls_y: le16(14)?,
                rs_x: le16(16)?,
                rs_y: le16(18)?,
            }))
        }
        MAGIC_CONTROLLER_ARRIVAL => Some(GamepadEvent::Arrival {
            index: *b.first()?,
            kind: *b.get(1)?,
            capabilities: le16(2)? as u16,
        }),
        _ => None,
    }
}

/// Build the host→client rumble plaintext (type `0x010B`): `[type][len=10][u32 filler]
/// [controllerNumber][lowFreqMotor][highFreqMotor]` (all LE; motors 0..0xFFFF). The caller
/// seals it with the host-direction GCM scheme and sends it on the ENet control peer.
pub fn rumble_plaintext(index: u16, low: u16, high: u16) -> Vec<u8> {
    let mut pt = Vec::with_capacity(14);
    pt.extend_from_slice(&0x010Bu16.to_le_bytes());
    pt.extend_from_slice(&10u16.to_le_bytes());
    pt.extend_from_slice(&0x00C0_FFEEu32.to_le_bytes()); // filler — present but ignored
    pt.extend_from_slice(&index.to_le_bytes());
    pt.extend_from_slice(&low.to_le_bytes());
    pt.extend_from_slice(&high.to_le_bytes());
    pt
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::gamepad::{BTN_A, BTN_RB};

    fn wrap(magic: u32, body: &[u8]) -> Vec<u8> {
        let mut inp = Vec::new();
        inp.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        inp.extend_from_slice(&magic.to_le_bytes());
        inp.extend_from_slice(body);
        let mut pt = Vec::new();
        pt.extend_from_slice(&INPUT_DATA_TYPE.to_le_bytes());
        pt.extend_from_slice(&(inp.len() as u16).to_le_bytes());
        pt.extend_from_slice(&inp);
        pt
    }

    #[test]
    fn decodes_multi_controller() {
        // Pad 1 attached (mask 0b10), A+RB held, LT=10 RT=200, LS=(1000,-2000), RS=(-1,32767),
        // paddle1 via buttonFlags2.
        let mut body = Vec::new();
        body.extend_from_slice(&0x001Ai16.to_le_bytes()); // headerB
        body.extend_from_slice(&1i16.to_le_bytes()); // controllerNumber
        body.extend_from_slice(&0b10i16.to_le_bytes()); // activeGamepadMask
        body.extend_from_slice(&0x0014i16.to_le_bytes()); // midB
        body.extend_from_slice(&((BTN_A | BTN_RB) as u16).to_le_bytes());
        body.push(10); // LT
        body.push(200); // RT
        body.extend_from_slice(&1000i16.to_le_bytes());
        body.extend_from_slice(&(-2000i16).to_le_bytes());
        body.extend_from_slice(&(-1i16).to_le_bytes());
        body.extend_from_slice(&32767i16.to_le_bytes());
        body.extend_from_slice(&0x009Ci16.to_le_bytes()); // tailA
        body.extend_from_slice(&0x0001u16.to_le_bytes()); // buttonFlags2 (paddle1)
        body.extend_from_slice(&0x0055i16.to_le_bytes()); // tailB

        let Some(GamepadEvent::State(f)) = decode(&wrap(MAGIC_MULTI_CONTROLLER, &body)) else {
            panic!("expected State");
        };
        assert_eq!(f.index, 1);
        assert_eq!(f.active_mask, 0b10);
        assert_eq!(f.buttons, BTN_A | BTN_RB | 0x0001_0000);
        assert_eq!((f.left_trigger, f.right_trigger), (10, 200));
        assert_eq!((f.ls_x, f.ls_y, f.rs_x, f.rs_y), (1000, -2000, -1, 32767));
    }

    #[test]
    fn decodes_arrival() {
        let body = [0u8, 1, 0x02, 0x00, 0xFF, 0xFF, 0x0F, 0x00]; // pad 0, xbox, rumble cap
        let Some(GamepadEvent::Arrival {
            index,
            kind,
            capabilities,
        }) = decode(&wrap(MAGIC_CONTROLLER_ARRIVAL, &body))
        else {
            panic!("expected Arrival");
        };
        assert_eq!((index, kind, capabilities), (0, 1, 0x0002));
    }

    #[test]
    fn ignores_mouse_and_short_packets() {
        assert!(decode(&wrap(0x07, &[0, 1, 0, 2])).is_none()); // relative mouse
        assert!(decode(&[0u8; 3]).is_none());
    }

    #[test]
    fn rumble_layout() {
        let pt = rumble_plaintext(2, 0x1234, 0xBEEF);
        assert_eq!(pt.len(), 14);
        assert_eq!(u16::from_le_bytes([pt[0], pt[1]]), 0x010B);
        assert_eq!(u16::from_le_bytes([pt[2], pt[3]]), 10);
        assert_eq!(u16::from_le_bytes([pt[8], pt[9]]), 2);
        assert_eq!(u16::from_le_bytes([pt[10], pt[11]]), 0x1234);
        assert_eq!(u16::from_le_bytes([pt[12], pt[13]]), 0xBEEF);
    }
}
