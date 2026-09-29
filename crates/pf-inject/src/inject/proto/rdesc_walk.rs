//! Test-only HID report-descriptor walk: the payload length one report declares, which is what
//! hidclass sizes its buffers from and what SDL reads back.

/// Main-item tags (`prefix & 0xFC`).
pub const INPUT: u8 = 0x80;
pub const OUTPUT: u8 = 0x90;
pub const FEATURE: u8 = 0xB0;

/// Bytes after the id that report `id` of kind `main` declares.
pub fn payload_len(desc: &[u8], main: u8, id: u8) -> usize {
    let (mut size, mut count, mut cur, mut bits) = (0u32, 0u32, 0u8, 0u32);
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        let n = [0, 1, 2, 4][(prefix & 3) as usize];
        let mut v = 0u32;
        for (k, b) in desc[i + 1..i + 1 + n].iter().enumerate() {
            v |= (*b as u32) << (8 * k);
        }
        match prefix & 0xFC {
            0x74 => size = v,
            0x94 => count = v,
            0x84 => cur = v as u8,
            tag if tag == main && cur == id => bits += size * count,
            _ => {}
        }
        i += 1 + n;
    }
    assert_eq!(bits % 8, 0, "report {id:#04x} is not whole bytes");
    (bits / 8) as usize
}
