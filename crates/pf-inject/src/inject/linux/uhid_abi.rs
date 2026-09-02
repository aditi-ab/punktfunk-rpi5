//! Shared `/dev/uhid` event ABI (`linux/uhid.h`).
//!
//! Backends open [`UHID_PATH`] and drive their own read/write loops. This
//! file owns the packed numbers, not a parser.
//!
//! `struct uhid_event` is `__packed__`: a `u32` type then a union whose
//! largest member is `uhid_create2_req` (name 128 + phys 64 + uniq 64 +
//! rd_size 2 + bus 2 + 4×u32 + rd_data 4096 = 4372). [`UHID_EVENT_SIZE`] is
//! that plus the type tag.
//!
//! [`set_report_data`] and [`output_data`] honour the kernel's `size` field.
//! A fixed window truncates a long report or parses stale bytes past a short
//! one in a reused event buffer.

pub const UHID_PATH: &str = "/dev/uhid";

// `enum uhid_event_type`; only the ones backends write.
pub const UHID_DESTROY: u32 = 1;
pub const UHID_OUTPUT: u32 = 6;
pub const UHID_GET_REPORT: u32 = 9;
pub const UHID_GET_REPORT_REPLY: u32 = 10;
pub const UHID_CREATE2: u32 = 11;
pub const UHID_INPUT2: u32 = 12;
pub const UHID_SET_REPORT: u32 = 13;
pub const UHID_SET_REPORT_REPLY: u32 = 14;

/// Cap on a report payload copied out of an event.
pub const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
/// `u32` type tag plus the create2 union (`sizeof(struct uhid_event)`).
pub const UHID_EVENT_SIZE: usize = 4 + 4372;
/// From `linux/input.h`.
pub const BUS_USB: u16 = 0x03;

/// Shared by GET_REPORT / SET_REPORT request and reply.
const OFF_ID: usize = 4;
/// After `id: u32`, `rnum: u8`, `rtype: u8`.
const OFF_SET_REPORT_SIZE: usize = 10;
/// SET_REPORT payload, and `data` in the reply structs.
const OFF_DATA: usize = 12;
/// After `data[4096]` — unlike SET_REPORT, `size` is trailing.
const OFF_OUTPUT_SIZE: usize = 4 + HID_MAX_DESCRIPTOR_SIZE;

/// Truncation still leaves a NUL: the caller zeros the buffer first.
pub fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
}

/// What the matching GET_REPORT / SET_REPORT reply must echo.
pub fn request_id(ev: &[u8]) -> u32 {
    u32::from_ne_bytes([ev[OFF_ID], ev[OFF_ID + 1], ev[OFF_ID + 2], ev[OFF_ID + 3]])
}

/// Honour the event's own `size`. A fixed window truncates a long report
/// or parses stale bytes past a short one in a reused buffer.
pub fn set_report_data(ev: &[u8]) -> &[u8] {
    let size = u16::from_ne_bytes([ev[OFF_SET_REPORT_SIZE], ev[OFF_SET_REPORT_SIZE + 1]]) as usize;
    let end = (OFF_DATA + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(ev.len());
    &ev[OFF_DATA.min(end)..end]
}

/// `uhid_output_req`: `data[4096]` then trailing `size` (unlike SET_REPORT).
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

        let ev0 = blank();
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
