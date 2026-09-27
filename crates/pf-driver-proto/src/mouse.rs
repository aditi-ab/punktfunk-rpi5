//! Virtual-pointer shared-memory layout (host ↔ UMDF HID-mouse minidriver `pf_mouse`).
//!
//! With no pointing device, win32k reports the cursor absent (`SM_MOUSEPRESENT` = 0) and DWM
//! never composites a cursor into the pf-vdisplay frame — `SendInput` still moves it, but the
//! stream shows no pointer. A resident HID mouse devnode makes Windows consider a pointer
//! present. Injection stays `SendInput`; the report path is the higher-fidelity route.
//!
//! Same sealed-pad handshake as [`gamepad`](crate::gamepad) (`design/gamepad-channel-sealing.md`):
//! [`gamepad::PadBootstrap`](crate::gamepad::PadBootstrap), [`mouse_boot_name`], mouse DATA
//! magic, `pad_index` 0. Reusing the handshake means `pf-umdf-util`'s
//! `ChannelClient`/`PadChannel` serve the mouse unchanged.

use alloc::string::String;
use bytemuck::{Pod, Zeroable};

/// Mouse DATA-section magic ("PFMO" LE) — distinct from the pad magics so a cross-wire fails.
pub const MOUSE_MAGIC: u32 = 0x4F4D_4650;

/// `Global\pfmouse-boot-<index>` — mouse bootstrap mailbox ([`crate::gamepad::PadBootstrap`]).
pub fn mouse_boot_name(index: u8) -> String {
    alloc::format!("Global\\pfmouse-boot-{index}")
}

/// HID identity ("PF" / "MO") — obviously virtual; no software matches on it, unlike the
/// pads' cloned Sony/Valve ids.
pub const MOUSE_VID: u16 = 0x5046;
pub const MOUSE_PID: u16 = 0x4D4F;
pub const MOUSE_VER: u16 = 0x0100;

/// Input report id `0x01`: `[id, buttons(5 bits), x_lo, x_hi, y_lo, y_hi, wheel, pan]` —
/// absolute X/Y over `0..=`[`MOUSE_ABS_MAX`], relative wheel/pan.
pub const MOUSE_REPORT_ID: u8 = 0x01;
pub const MOUSE_REPORT_LEN: usize = 8;
/// Logical maximum of the absolute X/Y axes (15-bit, HID-descriptor convention).
pub const MOUSE_ABS_MAX: u16 = 0x7FFF;

/// Build the 8-byte input report. Pure so the layout is unit-tested here (the driver
/// workspace is `panic = "abort"`); the driver only ferries these bytes.
#[must_use]
pub fn input_report(buttons: u8, x: u16, y: u16, wheel: i8, pan: i8) -> [u8; MOUSE_REPORT_LEN] {
    let x = x.min(MOUSE_ABS_MAX);
    let y = y.min(MOUSE_ABS_MAX);
    [
        MOUSE_REPORT_ID,
        buttons & 0x1F,
        (x & 0xFF) as u8,
        (x >> 8) as u8,
        (y & 0xFF) as u8,
        (y >> 8) as u8,
        wheel as u8,
        pan as u8,
    ]
}

/// Virtual-mouse shared section (64 B). Host writes a report then bumps `in_seq` (Release);
/// the driver's timer Acquire-loads it and completes a pended `READ_REPORT`. Idle generates
/// no HID traffic — a constant report stream would read as user activity to the OS.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct MouseShm {
    pub magic: u32,
    /// Bumped AFTER `report` is in place (Release). `0` = nothing published yet.
    pub in_seq: u32,
    pub report: [u8; MOUSE_REPORT_LEN],
    /// [`crate::gamepad::GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver.
    pub driver_proto: u32,
    /// Bumped each timer tick — advances whether or not input flows.
    pub driver_heartbeat: u32,
    /// Device index (host-stamped before the magic); driver checks it against the devnode Location.
    pub pad_index: u32,
    pub _reserved: [u8; 36],
}

// Offsets are the cross-process wire contract — pin every one.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<MouseShm>() == 64);
    assert!(offset_of!(MouseShm, magic) == 0);
    assert!(offset_of!(MouseShm, in_seq) == 4);
    assert!(offset_of!(MouseShm, report) == 8);
    assert!(offset_of!(MouseShm, driver_proto) == 16);
    assert!(offset_of!(MouseShm, driver_heartbeat) == 20);
    assert!(offset_of!(MouseShm, pad_index) == 24);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad;

    #[test]
    fn mouse_report_and_names_are_stable() {
        assert_eq!(mouse_boot_name(0), "Global\\pfmouse-boot-0");
        // "PFMO" LE, and never colliding with a pad magic.
        assert_eq!(MOUSE_MAGIC.to_le_bytes(), *b"PFMO");
        assert_ne!(MOUSE_MAGIC, gamepad::XUSB_MAGIC);
        assert_ne!(MOUSE_MAGIC, gamepad::PAD_MAGIC);
        let r = input_report(0b0000_0101, 0x1234, 0x7FFF, -3, 7);
        assert_eq!(r, [0x01, 0x05, 0x34, 0x12, 0xFF, 0x7F, 0xFD, 0x07]);
        // Axes clamp to the 15-bit logical max; buttons to the declared 5.
        let r = input_report(0xFF, 0xFFFF, 0, 0, 0);
        assert_eq!((r[1], r[2], r[3]), (0x1F, 0xFF, 0x7F));
        // A zeroed section reads as nothing published (`in_seq` 0).
        let shm = MouseShm::zeroed();
        assert_eq!(shm.in_seq, 0);
        assert_eq!(bytemuck::bytes_of(&shm).len(), 64);
    }
}
