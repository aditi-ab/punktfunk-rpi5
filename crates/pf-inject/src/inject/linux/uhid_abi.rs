//! The `/dev/uhid` event ABI (`linux/uhid.h`), in one place.
//!
//! Every UHID gamepad backend — DualSense, DualShock 4, Switch Pro, Steam Controller and Steam
//! Controller 2 — speaks the same kernel protocol, and each carried its own verbatim copy of these
//! constants plus its own `put_cstr`. Five copies of one kernel ABI is five chances to drift from
//! it, and they already had: `switch_pro` was missing the SET_REPORT pair entirely, and one backend
//! read a fixed-size SET_REPORT payload instead of the length the kernel gave it (see
//! [`set_report_data`]).
//!
//! `struct uhid_event` is `__packed__`: a `u32` `type` followed by a union whose largest member is
//! `uhid_create2_req` (name 128 + phys 64 + uniq 64 + rd_size 2 + bus 2 + 4×u32 + rd_data 4096 =
//! 4372 bytes). Nothing here allocates or parses a whole event — the backends still drive their own
//! read/write loops; this module owns the numbers and the two field accessors that are easy to get
//! subtly wrong.

/// The character device every backend opens.
pub const UHID_PATH: &str = "/dev/uhid";

// Event types (`enum uhid_event_type`). Only the ones the backends actually use.
pub const UHID_DESTROY: u32 = 1;
pub const UHID_OUTPUT: u32 = 6;
pub const UHID_GET_REPORT: u32 = 9;
pub const UHID_GET_REPORT_REPLY: u32 = 10;
pub const UHID_CREATE2: u32 = 11;
pub const UHID_INPUT2: u32 = 12;
pub const UHID_SET_REPORT: u32 = 13;
pub const UHID_SET_REPORT_REPLY: u32 = 14;

/// `HID_MAX_DESCRIPTOR_SIZE` — also the cap on a report payload we will copy out of an event.
pub const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
/// `size_of::<uhid_event>()`: the `u32` type tag plus the create2 union.
pub const UHID_EVENT_SIZE: usize = 4 + 4372;
/// `BUS_USB` from `linux/input.h`.
pub const BUS_USB: u16 = 0x03;

/// Offset of the `id` field shared by the GET_REPORT / SET_REPORT request and reply structs.
const OFF_ID: usize = 4;
/// Offset of `uhid_set_report_req::size` (after `id: u32`, `rnum: u8`, `rtype: u8`).
const OFF_SET_REPORT_SIZE: usize = 10;
/// Offset of the payload in a SET_REPORT request — and of `data` in the reply structs.
const OFF_DATA: usize = 12;
/// Offset of `uhid_output_req::size` (the payload follows `data[4096]`).
const OFF_OUTPUT_SIZE: usize = 4 + HID_MAX_DESCRIPTOR_SIZE;

/// Copy a NUL-padded C string field into the event buffer. The buffer is zeroed by the caller, so
/// truncation still leaves a NUL terminator.
pub fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]); // rest already zero (NUL-terminated)
}

/// The request id of a GET_REPORT / SET_REPORT event — what the matching reply must echo.
pub fn request_id(ev: &[u8]) -> u32 {
    u32::from_ne_bytes([ev[OFF_ID], ev[OFF_ID + 1], ev[OFF_ID + 2], ev[OFF_ID + 3]])
}

/// The payload of a `UHID_SET_REPORT` event: exactly the bytes the kernel says are there.
///
/// Read the length from the event's own `size` field. Assuming a fixed window instead is wrong in
/// both directions — a longer report is silently truncated, and a shorter one is parsed together
/// with whatever stale bytes the reused event buffer still holds past its end, which for a rumble
/// report means acting on numbers the game never wrote.
pub fn set_report_data(ev: &[u8]) -> &[u8] {
    let size = u16::from_ne_bytes([ev[OFF_SET_REPORT_SIZE], ev[OFF_SET_REPORT_SIZE + 1]]) as usize;
    let end = (OFF_DATA + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(ev.len());
    &ev[OFF_DATA.min(end)..end]
}

/// The payload of a `UHID_OUTPUT` event (`uhid_output_req`: `data[4096]` then `size`).
pub fn output_data(ev: &[u8]) -> &[u8] {
    let size = u16::from_ne_bytes([ev[OFF_OUTPUT_SIZE], ev[OFF_OUTPUT_SIZE + 1]]) as usize;
    let end = (4 + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(ev.len());
    &ev[4.min(end)..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank() -> Vec<u8> {
        vec![0u8; UHID_EVENT_SIZE]
    }

    #[test]
    fn set_report_data_honours_the_events_own_size() {
        let mut ev = blank();
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&5u16.to_ne_bytes());
        for (i, b) in [1u8, 2, 3, 4, 5].iter().enumerate() {
            ev[OFF_DATA + i] = *b;
        }
        // Stale bytes past the payload — a fixed-window read would hand these to the parser.
        ev[OFF_DATA + 5] = 0xAA;
        ev[OFF_DATA + 15] = 0xBB;
        assert_eq!(set_report_data(&ev), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn set_report_data_is_not_truncated_at_sixteen() {
        let mut ev = blank();
        let n = 40usize;
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&(n as u16).to_ne_bytes());
        for i in 0..n {
            ev[OFF_DATA + i] = i as u8;
        }
        let d = set_report_data(&ev);
        assert_eq!(
            d.len(),
            n,
            "a report longer than 16 bytes must survive whole"
        );
        assert_eq!(d[39], 39);
    }

    #[test]
    fn oversized_and_empty_sizes_stay_in_bounds() {
        let mut ev = blank();
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&u16::MAX.to_ne_bytes());
        assert!(set_report_data(&ev).len() <= HID_MAX_DESCRIPTOR_SIZE);
        assert!(OFF_DATA + set_report_data(&ev).len() <= UHID_EVENT_SIZE);

        let ev0 = blank(); // size = 0
        assert!(set_report_data(&ev0).is_empty());
        assert!(output_data(&ev0).is_empty());
    }

    #[test]
    fn output_data_reads_its_trailing_size_field() {
        let mut ev = blank();
        ev[OFF_OUTPUT_SIZE..OFF_OUTPUT_SIZE + 2].copy_from_slice(&3u16.to_ne_bytes());
        ev[4] = 0x02;
        ev[5] = 0x11;
        ev[6] = 0x22;
        ev[7] = 0x33; // past the declared size
        assert_eq!(output_data(&ev), &[0x02, 0x11, 0x22]);
    }

    #[test]
    fn request_id_round_trips() {
        let mut ev = blank();
        ev[OFF_ID..OFF_ID + 4].copy_from_slice(&0xDEAD_BEEFu32.to_ne_bytes());
        assert_eq!(request_id(&ev), 0xDEAD_BEEF);
    }
}
