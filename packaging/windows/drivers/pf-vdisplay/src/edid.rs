//! The 256-byte EDID the pf-vdisplay driver hands IddCx for each virtual monitor: a 128-byte EDID 1.4
//! base block + a **CTA-861.3 extension** that advertises HDR — a BT.2020 Colorimetry Data Block and an
//! HDR Static Metadata Data Block declaring the SMPTE ST 2084 (PQ) EOTF. Windows reads a display's HDR
//! capability from this CTA HDR block; without it the monitor is treated as SDR-only regardless of the
//! IddCx adapter's `CAN_PROCESS_FP16` / `HIGH_COLOR_SPACE` / 10-bit mode caps (the missing piece that
//! made "Use HDR" never appear for the virtual display). The base block declares EDID 1.4 + 10-bit
//! digital so the panel's bit depth is unambiguous.
//!
//! Identity: manufacturer "PNK" (bytes 8-9), product name "Punktfunk" (the 0xFC display descriptor —
//! this is what Windows shows as `Generic Monitor (Punktfunk)`; byte 127's checksum is recomputed in
//! [`Edid::generate_with`], so editing the name here needs no hand-patched checksum). The
//! serial-number field (base offset 0x0C, little-endian) encodes the per-monitor index so
//! `parse_monitor_description` can map an EDID the OS hands back to its monitor; [`Edid::generate_with`]
//! patches that serial and recomputes BOTH block checksums (base byte 127 + extension byte 255). The
//! detailed-timing / range-limit descriptors are placeholders — the modes we actually advertise come
//! from the monitor's stored mode list (`monitor.rs` / `callbacks.rs`), not from parsing this EDID.

use std::array::TryFromSliceError;

/// Per-monitor serial number, base-block offset 0x0C, little-endian u32.
const SERIAL_OFFSET: usize = 0x0C;

/// EDID 1.4 base block (128 bytes). Differs from a plain SDR virtual EDID only by: revision 1.4 (byte
/// 19 = 0x04), 10-bit digital video input (byte 20 = 0xB0), and one extension present (byte 126 = 0x01).
/// Byte 127 (checksum) and the serial (0x0C) are filled/patched in [`Edid::generate_with`].
#[rustfmt::skip]
const BASE: [u8; 128] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, // fixed header
    0x41, 0xCB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mfr "PNK", product, serial (patched)
    0xFF, 0x21, 0x01, 0x04, 0xB0, 0x32, 0x1F, 0x78, // week/year, EDID 1.4, 10-bit digital, size, gamma
    0x03, 0x78, 0xB1, 0xB5, 0x4A, 0x2B, 0xCC, 0x21, // feature (sRGB-default CLEARED), BT.2020 primaries...
    0x0B, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, // ...BT.2020 primaries, established timings, std timings
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3A, // std timings, DTD 1 (placeholder preferred timing)
    0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C,
    0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
    0x00, 0x00, 0x00, 0xFD, 0x00, 0x17, 0xF0, 0x0F, // display range-limits descriptor
    0xFF, 0x0F, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20,
    0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x50, // name descriptor "Punktfunk"
    0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B,
    0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00, // empty 4th descriptor...
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, // ...byte 126 = 1 extension, byte 127 = checksum
];

/// CTA-861.3 extension block (128 bytes), block 1. Header + a Data Block Collection holding the
/// Colorimetry and HDR Static Metadata data blocks; the rest is padding up to the checksum (byte 255).
/// `D` (byte 130) marks where DTDs would start (= end of the data blocks); we carry none.
#[rustfmt::skip]
const CTA_HEADER: [u8; 4] = [
    0x02, // CTA Extension tag
    0x03, // revision 3 (CTA-861.3 — required for the extended-tag data blocks below)
    0x0F, // D = 15: the (empty) DTD region starts at block byte 15, i.e. data blocks occupy bytes 4..15
    0x00, // 0 native DTDs; no basic audio; no YCbCr 4:4:4/4:2:2 (RGB-only, matching the wire format)
];

/// Colorimetry Data Block (CTA extended tag 0x05): declare BT.2020 RGB (bit 7). YCbCr variants are left
/// clear — the IddCx wire format is RGB-only — and the gamut-metadata flags are 0.
#[rustfmt::skip]
const COLORIMETRY_DB: [u8; 4] = [
    0xE3, // tag 0b111 (use-extended-tag) | length 3
    0x05, // extended tag: Colorimetry
    0x80, // BT2020RGB (bit 7); xvYCC/sYCC/opRGB/BT2020 YCC/cYCC all clear
    0x00, // gamut metadata profiles MD0..MD3: none
];

/// HDR Static Metadata Data Block (CTA extended tag 0x06): EOTFs = Traditional SDR (ET_0) + SMPTE ST
/// 2084 / PQ (ET_2); Static Metadata Type 1 (SM_0). Plus the desired-content luminance hints
/// (~993 nit max, ~400 nit max-frame-average, ~0.05 nit min) — the BUILT-IN defaults, used when the
/// host reported no client volume; [`Edid::generate_with`] overwrites bytes 4..7 with the CLIENT
/// display's coded volume otherwise, so host apps tone-map to the panel the stream lands on.
#[rustfmt::skip]
const HDR_STATIC_METADATA_DB: [u8; 7] = [
    0xE6, // tag 0b111 (use-extended-tag) | length 6
    0x06, // extended tag: HDR Static Metadata
    0x05, // Supported EOTFs: ET_0 (traditional SDR) | ET_2 (SMPTE ST 2084 / PQ)
    0x01, // Supported Static Metadata Descriptors: SM_0 (Static Metadata Type 1)
    0x8A, // Desired Content Max Luminance      (code 138 ≈ 993 nits)
    0x60, // Desired Content Max Frame-avg Lum. (code  96 = 400 nits)
    0x12, // Desired Content Min Luminance      (code  18 ≈ 0.05 nits)
];

/// The client display's luminance volume for the CTA HDR block (the [`AddRequest`]
/// (pf_driver_proto::control::AddRequest) luminance tail, same units). `max_nits == 0` = unknown
/// (an SDR client, or an un-upgraded host whose short ADD zero-fills the tail) → the built-in
/// defaults stay.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientLuminance {
    /// Peak luminance, nits. `0` = unknown → keep the built-in default block.
    pub max_nits: u32,
    /// Max frame-average luminance, nits. `0` = unknown ("no data" on the wire).
    pub max_frame_avg_nits: u32,
    /// Min luminance, milli-nits. `0` = unknown/true black ("no data" on the wire).
    pub min_millinits: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Edid;

impl Edid {
    /// Build the full 256-byte EDID for monitor `serial`, with both block checksums recomputed.
    /// `lum` is the CLIENT display's luminance volume — coded into the HDR static-metadata block's
    /// desired-content bytes (CTA-861.3, via the shared+unit-tested `pf_driver_proto::edid`
    /// coders) so the OS/apps tone-map to the client's real panel; all-zero keeps the built-in
    /// ~993-nit defaults.
    pub fn generate_with(serial: u32, lum: ClientLuminance) -> Vec<u8> {
        let mut edid = [0u8; 256];
        // Block 0: base.
        edid[..128].copy_from_slice(&BASE);
        edid[SERIAL_OFFSET..SERIAL_OFFSET + 4].copy_from_slice(&serial.to_le_bytes());
        // Block 1: CTA-861.3 extension (header + colorimetry + HDR static metadata; rest stays 0).
        edid[128..132].copy_from_slice(&CTA_HEADER);
        edid[132..136].copy_from_slice(&COLORIMETRY_DB);
        let mut hdr_db = HDR_STATIC_METADATA_DB;
        if lum.max_nits > 0 {
            let max_code = pf_driver_proto::edid::cta_max_luminance_code(lum.max_nits);
            hdr_db[4] = max_code;
            hdr_db[5] = if lum.max_frame_avg_nits > 0 {
                pf_driver_proto::edid::cta_max_luminance_code(lum.max_frame_avg_nits)
            } else {
                0 // "no data" — valid per CTA-861.3
            };
            hdr_db[6] = pf_driver_proto::edid::cta_min_luminance_code(lum.min_millinits, max_code);
        }
        edid[136..143].copy_from_slice(&hdr_db);
        // Each 128-byte block ends in a checksum byte that makes the block sum ≡ 0 (mod 256).
        Self::fix_block_checksum(&mut edid, 0);
        Self::fix_block_checksum(&mut edid, 128);
        edid.to_vec()
    }

    /// Read the per-monitor serial (base offset 0x0C, little-endian) from an EDID the OS handed back.
    /// Works for the full 256-byte EDID or just the 128-byte base block. Errors (rather than panics) on
    /// a too-short buffer so the caller can reject a malformed descriptor.
    pub fn get_serial(edid: &[u8]) -> Result<u32, TryFromSliceError> {
        let bytes: [u8; 4] = edid
            .get(SERIAL_OFFSET..SERIAL_OFFSET + 4)
            .unwrap_or(&[])
            .try_into()?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Set the trailing byte of the 128-byte block at `start` so the block's bytes sum to 0 (mod 256) —
    /// the standard EDID block checksum.
    fn fix_block_checksum(edid: &mut [u8], start: usize) {
        let sum = edid[start..start + 127]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        edid[start + 127] = 0u8.wrapping_sub(sum);
    }
}
