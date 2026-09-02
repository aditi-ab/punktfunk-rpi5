//! HDR10 static-metadata helpers shared by capture (source mastering) and encode
//! (in-band SEI). Platform-independent so the byte-level logic is unit-tested on
//! every target.
//!
//! Units follow HDR10 so values pass through:
//! - chromaticities in 1/50000 (SMPTE ST.2086 / `DXGI_HDR_METADATA_HDR10`),
//! - mastering luminance in 0.0001 cd/m²,
//! - MaxCLL/MaxFALL in cd/m² (nits).
//!
//! SEI builders are consumed by the Windows NVENC path; display conversion by
//! the Windows DXGI/WGC capturers.

use punktfunk_core::quic::HdrMeta;

/// HEVC/H.264 SEI payload type `mastering_display_colour_volume` (ST.2086). Same
/// code point in AVC and HEVC.
pub const SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME: u32 = 137;
/// HEVC/H.264 SEI payload type `content_light_level_info` (CEA-861.3).
pub const SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO: u32 = 144;

fn xy_to_2086(v: f32) -> u16 {
    (v * 50000.0).round().clamp(0.0, 65535.0) as u16
}

/// Build [`HdrMeta`] from a source display's measured colour volume (CIE xy,
/// cd/m²). `max_cll`/`max_fall` are nits; pass `0` when unknown — GetDesc1 does
/// not expose them, and `0` lets the display fall back to mastering luminance.
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

/// [`HdrMeta`] volume → pf-vdisplay `AddRequest` luminance:
/// `(max nits, max frame-average nits, min milli-nits)`.
///
/// Mastering luminance is 0.0001 cd/m² (nits = /10_000, milli-nits = /10).
/// MaxFALL is already nits and is the display's frame-average ceiling.
pub fn vdisplay_luminance_fields(m: &HdrMeta) -> (u32, u32, u32) {
    (
        m.max_display_mastering_luminance / 10_000,
        m.max_fall as u32,
        m.min_display_mastering_luminance / 10,
    )
}

/// BT.2020 / D65 / 1000-nit HDR10 default until the source display is read.
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

/// `mastering_display_colour_volume` SEI payload: 24 bytes, big-endian, G/B/R
/// per ST.2086. Pass the raw bytes to NVENC `NV_ENC_SEI_PAYLOAD`.
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

/// `content_light_level_info` SEI payload: 4 bytes, big-endian, MaxCLL then MaxFALL.
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
        // First field = green.x (ST.2086 G/B/R), big-endian.
        assert_eq!(&p[0..2], &8500u16.to_be_bytes());
        assert_eq!(&p[2..4], &39850u16.to_be_bytes());
        assert_eq!(&p[4..6], &6550u16.to_be_bytes());
        assert_eq!(&p[12..14], &15635u16.to_be_bytes());
        assert_eq!(&p[16..20], &10_000_000u32.to_be_bytes());
        assert_eq!(&p[20..24], &1u32.to_be_bytes());
    }

    #[test]
    fn cll_sei_is_4_bytes_big_endian() {
        let m = generic_hdr10();
        let p = hevc_content_light_level_sei(&m);
        assert_eq!(p, [0x03, 0xE8, 0x01, 0x90]); // 1000, 400 big-endian
    }

    #[test]
    fn vdisplay_luminance_fields_convert_units() {
        // 800 nits / 0.05 nits / 400 MaxFALL → (nits, nits, milli-nits).
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
        // Unknown (all-zero) volume stays all-zero; the driver keeps EDID defaults.
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
