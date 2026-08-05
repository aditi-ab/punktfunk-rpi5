//! SEI payload parsing — the piece the vendored parser layer lacks: upstream classifies
//! SEI NALUs (`NaluType::Sei`) but never reads a payload. punktfunk needs exactly one:
//! the recovery point SEI (payload type 6, spec D.1.8 syntax / D.2.8 semantics), which
//! hosts emit on RFI recovery so the client knows where a decode-from-here point lands.
//! Every other payload type is skipped by its declared size.

/// Recovery point SEI (D.2.8).
///
/// `recovery_frame_cnt` counts in `frame_num` increments from the AU carrying the SEI to
/// the picture at which output is exact (`exact_match`) or approximate. `broken_link` set
/// means pictures before the recovery point may be visually broken and must not be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPoint {
    pub recovery_frame_cnt: u32,
    pub exact_match: bool,
    pub broken_link: bool,
}

/// Parse the first recovery point SEI message out of a SEI NALU.
///
/// `sei_payload` are the bytes of the NALU after its one-byte NAL header, emulation
/// prevention bytes still in place (they are removed here — 7.4.1 RBSP extraction).
/// `Ok(None)` means the NALU parsed cleanly but carries no recovery point.
pub fn parse_recovery_point(sei_payload: &[u8]) -> Result<Option<RecoveryPoint>, String> {
    let rbsp = strip_emulation_prevention(sei_payload);

    let mut i = 0usize;
    while i < rbsp.len() && !is_rbsp_trailing(&rbsp, i) {
        // D.1: payload type and size are ff-coded — 0xFF bytes each add 255 until a
        // non-0xFF byte terminates the value. The run length is unbounded, so the type
        // accumulates saturating: an adversarial ~16M-byte 0xFF run must not overflow
        // (a saturated type simply never matches 6). The size accumulator is a usize
        // whose use is bounds-checked below.
        let mut payload_type = 0u32;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            payload_type = payload_type.saturating_add(255);
            i += 1;
        }
        if i >= rbsp.len() {
            return Err("truncated SEI payload type".into());
        }
        payload_type = payload_type.saturating_add(u32::from(rbsp[i]));
        i += 1;

        let mut payload_size = 0usize;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            payload_size += 255;
            i += 1;
        }
        if i >= rbsp.len() {
            return Err("truncated SEI payload size".into());
        }
        payload_size += usize::from(rbsp[i]);
        i += 1;

        let end = i
            .checked_add(payload_size)
            .filter(|&end| end <= rbsp.len())
            .ok_or_else(|| "SEI payload overruns the NALU".to_string())?;

        if payload_type == 6 {
            let mut r = BitCursor::new(&rbsp[i..end]);
            let recovery_frame_cnt = r.read_ue()?;
            let exact_match = r.read_bit()? != 0;
            let broken_link = r.read_bit()? != 0;
            // changing_slice_group_idc u(2): parsed to keep the reader honest, unused —
            // slice groups are outside every profile punktfunk hosts emit.
            let _changing_slice_group_idc = r.read_bits(2)?;
            return Ok(Some(RecoveryPoint {
                recovery_frame_cnt,
                exact_match,
                broken_link,
            }));
        }

        i = end;
    }

    Ok(None)
}

/// 7.4.1: within the RBSP, `00 00 03` encodes two zero bytes; the `03` is the emulation
/// prevention byte and is dropped.
fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// `more_rbsp_data()` at a byte-aligned message boundary: the remainder is trailing bits
/// iff it is the stop bit (0x80) followed by nothing but zero bytes.
fn is_rbsp_trailing(rbsp: &[u8], i: usize) -> bool {
    rbsp[i] == 0x80 && rbsp[i + 1..].iter().all(|&b| b == 0)
}

/// Minimal MSB-first bit reader over an already-unescaped RBSP slice. The vendored
/// `BitReader` is `pub(crate)` to the vendored crate, so this crate carries its own.
struct BitCursor<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bit(&mut self) -> Result<u32, String> {
        let byte = *self
            .data
            .get(self.pos / 8)
            .ok_or("SEI payload out of bits")?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, String> {
        debug_assert!(count <= 31);
        let mut out = 0u32;
        for _ in 0..count {
            out = (out << 1) | self.read_bit()?;
        }
        Ok(out)
    }

    /// ue(v), spec 9.1.
    fn read_ue(&mut self) -> Result<u32, String> {
        let mut leading_zeros = 0usize;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err("invalid exp-Golomb code in SEI payload".into());
            }
        }
        let suffix = self.read_bits(leading_zeros)?;
        ((1u32 << leading_zeros) - 1)
            .checked_add(suffix)
            .ok_or_else(|| "exp-Golomb value overflows u32".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_recovery_point_message_parses_to_its_field_values() {
        // Message: type 6, size 1. Payload bits: ue(0)='1', exact=0, broken=0, csg=00,
        // then payload alignment '1' + zeros -> 0b1000_0100. NALU trailing 0x80.
        let sei = [0x06, 0x01, 0x84, 0x80];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );
    }

    #[test]
    fn recovery_frame_cnt_and_both_flags_round_trip_through_the_bit_reader() {
        // ue(5)='00110', exact=1, broken=1, csg=00, alignment -> 0b0011_0110 0b0100_0000.
        let sei = [0x06, 0x02, 0x36, 0x40, 0x80];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 5,
                exact_match: true,
                broken_link: true
            })
        );
    }

    #[test]
    fn earlier_messages_and_ff_coded_types_are_skipped_to_reach_the_recovery_point() {
        // First message: ff-coded payload type 255 (0xFF 0x00), size 1, payload 0x55.
        // Second message: type 5 (user data), size 3. Third: the recovery point.
        let sei = [
            0xFF, 0x00, 0x01, 0x55, // type 255
            0x05, 0x03, 0xAA, 0xBB, 0xCC, // type 5
            0x06, 0x01, 0x84, // recovery point
            0x80,
        ];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );
    }

    #[test]
    fn emulation_prevention_bytes_inside_the_payload_are_removed_before_reading() {
        // Unescaped payload (7 bytes): ue with a 22-zero prefix => recovery_frame_cnt
        // 2^22-1 = 4194303, exact=1, broken=0, csg=00, alignment. Its first bytes are
        // 00 00 02, which the escaper must have written as 00 00 03 02 on the wire.
        let sei = [
            0x06, 0x07, 0x00, 0x00, 0x03, 0x02, 0x00, 0x00, 0x04, 0x40, 0x80,
        ];
        assert!(sei.windows(3).any(|w| w == [0x00, 0x00, 0x03]));
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 4194303,
                exact_match: true,
                broken_link: false
            })
        );
    }

    #[test]
    fn a_sei_nalu_without_a_recovery_point_yields_none_not_an_error() {
        let sei = [0x05, 0x01, 0x00, 0x80];
        assert_eq!(parse_recovery_point(&sei).unwrap(), None);
    }

    #[test]
    fn a_payload_size_overrunning_the_nalu_is_a_parse_error() {
        let sei = [0x06, 0x0A, 0x00];
        assert!(parse_recovery_point(&sei).is_err());
    }
}
