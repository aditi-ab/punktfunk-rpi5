//! Pure HDR static-metadata helpers shared by the capture (source mastering metadata) and encode
//! (in-band SEI) paths — kept platform-independent and unit-tested here so the byte-level logic is
//! verified on every target, even though the only *callers* of the SEI builders are the Windows
//! NVENC path (`encode/nvenc.rs`) and of the display conversion the Windows DXGI/WGC capturers.
//!
//! Units follow the HDR10 standards so the values pass straight through:
//! - chromaticities in 1/50000 increments (SMPTE ST.2086 / DXGI `DXGI_HDR_METADATA_HDR10`),
//! - mastering luminance in 0.0001 cd/m²,
//! - content light level (MaxCLL/MaxFALL) in cd/m² (nits).

use punktfunk_core::quic::HdrMeta;

/// HEVC/H.264 SEI payload type for `mastering_display_colour_volume` (SMPTE ST.2086). Same code
/// point in AVC and HEVC.
pub const SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME: u32 = 137;
/// HEVC/H.264 SEI payload type for `content_light_level_info` (CEA-861.3 MaxCLL/MaxFALL).
pub const SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO: u32 = 144;

/// Quantize a CIE xy chromaticity coordinate (0.0..=1.0) to ST.2086 1/50000 units.
fn xy_to_2086(v: f32) -> u16 {
    (v * 50000.0).round().clamp(0.0, 65535.0) as u16
}

/// Build an [`HdrMeta`] from a source display's measured colour volume — the chromaticities (CIE xy)
/// and luminances (cd/m²) reported by e.g. Windows `IDXGIOutput6::GetDesc1`. `max_cll`/`max_fall`
/// are content light levels in nits; pass `0` when unknown (GetDesc1 doesn't expose them — Apollo
/// zeroes them too, and a `0` lets the display fall back to the mastering luminance).
#[allow(clippy::too_many_arguments)]
pub fn hdr_meta_from_display(
    red: (f32, f32),
    green: (f32, f32),
    blue: (f32, f32),
    white: (f32, f32),
    max_mastering_nits: f32,
    min_mastering_nits: f32,
    max_cll: u16,
    max_fall: u16,
) -> HdrMeta {
    HdrMeta {
        // ST.2086 stores primaries in G, B, R order.
        display_primaries: [
            [xy_to_2086(green.0), xy_to_2086(green.1)],
            [xy_to_2086(blue.0), xy_to_2086(blue.1)],
            [xy_to_2086(red.0), xy_to_2086(red.1)],
        ],
        white_point: [xy_to_2086(white.0), xy_to_2086(white.1)],
        max_display_mastering_luminance: (max_mastering_nits.max(0.0) * 10_000.0).round() as u32,
        min_display_mastering_luminance: (min_mastering_nits.max(0.0) * 10_000.0).round() as u32,
        max_cll,
        max_fall,
    }
}

/// Convert an [`HdrMeta`] display volume into the pf-vdisplay `AddRequest` luminance fields —
/// `(max nits, max frame-average nits, min MILLI-nits)` — which the driver codes into the virtual
/// monitor's EDID CTA-861.3 HDR block. Pure unit conversion: mastering luminance is 0.0001 cd/m²
/// (so nits = /10 000, milli-nits = /10); MaxFALL is already nits and doubles as the display's
/// frame-average ceiling.
pub fn vdisplay_luminance_fields(m: &HdrMeta) -> (u32, u32, u32) {
    (
        m.max_display_mastering_luminance / 10_000,
        m.max_fall as u32,
        m.min_display_mastering_luminance / 10,
    )
}

/// A generic HDR10 default (BT.2020 primaries, D65 white, 1000-nit mastering, MaxCLL 1000 /
/// MaxFALL 400) — the baseline a host sends until it reads the source display's real mastering
/// metadata, and the values clients used to hardcode.
pub fn generic_hdr10() -> HdrMeta {
    HdrMeta {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]], // BT.2020 G, B, R
        white_point: [15635, 16450],                                      // D65
        max_display_mastering_luminance: 10_000_000,                      // 1000 nits
        min_display_mastering_luminance: 1,                               // 0.0001 nits
        max_cll: 1000,
        max_fall: 400,
    }
}

/// The `mastering_display_colour_volume` SEI payload (HEVC/H.264 type
/// [`SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME`]) — 24 bytes, big-endian (SEI RBSP order), in G/B/R
/// primary order per ST.2086. Pass this raw payload to NVENC's `NV_ENC_SEI_PAYLOAD` (NVENC wraps it
/// in the SEI NAL).
pub fn hevc_mastering_display_sei(m: &HdrMeta) -> [u8; 24] {
    let mut b = [0u8; 24];
    let mut o = 0;
    let mut put16 = |v: u16| {
        b[o..o + 2].copy_from_slice(&v.to_be_bytes());
        o += 2;
    };
    for p in m.display_primaries.iter() {
        put16(p[0]);
        put16(p[1]);
    }
    put16(m.white_point[0]);
    put16(m.white_point[1]);
    let mut put32 = |v: u32| {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
        o += 4;
    };
    put32(m.max_display_mastering_luminance);
    put32(m.min_display_mastering_luminance);
    debug_assert_eq!(o, 24);
    b
}

/// The `content_light_level_info` SEI payload (HEVC/H.264 type
/// [`SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO`]) — 4 bytes, big-endian: MaxCLL then MaxFALL.
pub fn hevc_content_light_level_sei(m: &HdrMeta) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[0..2].copy_from_slice(&m.max_cll.to_be_bytes());
    b[2..4].copy_from_slice(&m.max_fall.to_be_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_conversion_bt2020_1000nit() {
        // BT.2020 primaries + D65 white, a 1000-nit / 0.0001-nit mastering display.
        let m = hdr_meta_from_display(
            (0.708, 0.292),   // red
            (0.170, 0.797),   // green
            (0.131, 0.046),   // blue
            (0.3127, 0.3290), // D65
            1000.0,
            0.0001,
            0,
            0,
        );
        // ST.2086 G, B, R order, 1/50000 units.
        assert_eq!(
            m.display_primaries,
            [[8500, 39850], [6550, 2300], [35400, 14600]]
        );
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!(m.max_display_mastering_luminance, 10_000_000); // 1000 * 10000
        assert_eq!(m.min_display_mastering_luminance, 1); // 0.0001 * 10000
        assert_eq!((m.max_cll, m.max_fall), (0, 0));
    }

    #[test]
    fn mastering_sei_is_24_bytes_big_endian_gbr() {
        let m = generic_hdr10();
        let p = hevc_mastering_display_sei(&m);
        assert_eq!(p.len(), 24);
        // First field = green.x = 8500 = 0x2134, big-endian.
        assert_eq!(&p[0..2], &8500u16.to_be_bytes());
        assert_eq!(&p[2..4], &39850u16.to_be_bytes()); // green.y
        assert_eq!(&p[4..6], &6550u16.to_be_bytes()); // blue.x
        assert_eq!(&p[12..14], &15635u16.to_be_bytes()); // white.x
        assert_eq!(&p[16..20], &10_000_000u32.to_be_bytes()); // max lum
        assert_eq!(&p[20..24], &1u32.to_be_bytes()); // min lum
    }

    #[test]
    fn cll_sei_is_4_bytes_big_endian() {
        let m = generic_hdr10();
        let p = hevc_content_light_level_sei(&m);
        assert_eq!(p, [0x03, 0xE8, 0x01, 0x90]); // 1000, 400 big-endian
    }

    #[test]
    fn vdisplay_luminance_fields_convert_units() {
        // An 800-nit / 0.05-nit panel with a 400-nit frame-average ceiling: the AddRequest fields
        // come out as whole nits / nits / MILLI-nits.
        let m = hdr_meta_from_display(
            (0.680, 0.320),
            (0.265, 0.690),
            (0.150, 0.060),
            (0.3127, 0.3290),
            800.0,
            0.05,
            0,
            400,
        );
        assert_eq!(vdisplay_luminance_fields(&m), (800, 400, 50));
        // The all-zero (unknown) volume stays all-zero — the driver keeps its EDID defaults.
        assert_eq!(vdisplay_luminance_fields(&HdrMeta::default()), (0, 0, 0));
    }

    #[test]
    fn clamps_out_of_range() {
        let m = hdr_meta_from_display(
            (2.0, 2.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.5, 0.5),
            -5.0,
            0.0,
            0,
            0,
        );
        assert_eq!(m.display_primaries[2], [65535, 65535]); // red clamped
        assert_eq!(m.max_display_mastering_luminance, 0); // negative → 0
    }
}
