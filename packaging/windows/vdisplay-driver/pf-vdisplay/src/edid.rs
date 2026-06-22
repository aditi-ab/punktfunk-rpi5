use std::{array::TryFromSliceError, ops::Deref};

use bytemuck::{Pod, Zeroable};

// A clean, self-contained 128-byte EDID carrying punktfunk's own identity — manufacturer ID "PNK"
// (bytes 8-9) and product name "punktfunk" (the 0xFC display-descriptor). Derived from the
// virtual-display-rs base block (a standard, widely-deployed virtual EDID); it deliberately carries NO
// other driver's bytes or branding. The serial-number field (offset 0x0C) encodes the per-monitor
// index, so `parse_monitor_description` can map an EDID the OS hands back to its monitor;
// `generate_with` patches that serial and `gen_checksum` recomputes byte 127 before the EDID reaches
// IddCx. The detailed-timing / range-limit descriptors are placeholders: the modes we actually
// advertise come from the monitor's stored mode list (`monitor.rs` / `callbacks.rs`), not from parsing
// this EDID.
const _EDID: [u8; 128] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x41, 0xCB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xFF, 0x21, 0x01, 0x03, 0x80, 0x32, 0x1F, 0x78, 0x07, 0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26,
    0x0F, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C,
    0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E, 0x00, 0x00, 0x00, 0xFD, 0x00, 0x17, 0xF0, 0x0F,
    0xFF, 0x0F, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x70,
    0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B, 0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const EDID_LEN: usize = _EDID.len();

static EDID: AlignedEdid<EDID_LEN> = AlignedEdid {
    data: _EDID,
    _align: [],
};

#[repr(C)]
struct AlignedEdid<const N: usize> {
    data: [u8; N],
    // required to make this type aligned to Edid
    _align: [Edid; 0],
}

impl<const N: usize> AlignedEdid<N> {
    fn new(data: &[u8]) -> Result<Self, TryFromSliceError> {
        let data: [u8; N] = data.try_into()?;
        Ok(Self { data, _align: [] })
    }
}

impl<const N: usize> Deref for AlignedEdid<N> {
    type Target = Edid;

    fn deref(&self) -> &Self::Target {
        let header = &self.data[..EDID_SIZE];
        bytemuck::from_bytes(header)
    }
}

const EDID_SIZE: usize = std::mem::size_of::<Edid>();

#[repr(C)]
#[derive(Debug, Copy, Clone, Pod, Zeroable)]
pub struct Edid {
    header: [u8; 8],
    manufacturer_id: [u8; 2],
    product_code: u16,
    serial_number: u32,
    manufacture_week: u8,
    manufacture_year: u8,
    version: u8,
    revision: u8,
}

impl Edid {
    pub fn generate_with(serial: u32) -> Vec<u8> {
        // change serial number in the header
        let mut header = *EDID;
        header.serial_number = serial;

        header.generate()
    }

    pub fn get_serial(edid: &[u8]) -> Result<u32, TryFromSliceError> {
        let edid = AlignedEdid::<EDID_LEN>::new(edid)?;
        Ok(edid.serial_number)
    }

    fn generate(&self) -> Vec<u8> {
        let header = bytemuck::bytes_of(self);

        // slice of monitor edid minus header
        let data = &EDID.data[EDID_SIZE..];

        // splice together header and the rest of the EDID
        let mut edid: Vec<u8> = header.iter().chain(data).copied().collect();
        // regenerate checksum
        Self::gen_checksum(&mut edid);

        edid
    }

    fn gen_checksum(data: &mut [u8]) {
        // important, this is the bare minimum length
        assert!(data.len() >= 128);

        // slice to the entire data minus the last checksum byte
        let edid_data = &data[..=126];

        // do checksum calculation
        let sum: u32 = edid_data.iter().copied().map(u32::from).sum();
        // this wont ever truncate
        #[allow(clippy::cast_possible_truncation)]
        let checksum = (256 - (sum % 256)) as u8;

        // update last byte with new checksum
        data[127] = checksum;
    }
}
