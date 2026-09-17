//! Adaptive bitrate: the controller behind the Automatic bitrate setting.
//!
//! Runs in [`crate::client`]'s data-plane pump on the 750 ms cadence shared
//! with [`crate::quic::LossReport`]. FEC absorbs short random loss; the
//! controller asks the host for a different encoder rate via
//! [`crate::quic::SetBitrate`] when congestion persists.
//!
//! One module per concern: [`sample`] is one closed report window,
//! [`verdict`] scores it, [`controller`] holds the state it moves. `sim/`
//! drives the real controller
//! against modelled links and pins every decision in a checked-in baseline.

/// The link simulator and the checked-in baseline (`abr/sim/`).
#[cfg(test)]
mod sim;

mod controller;
#[cfg(test)]
mod harness;
mod sample;
mod verdict;

pub(crate) use controller::BitrateController;
pub(crate) use sample::{WindowActivity, WindowSample};

/// Upper bound on bitrate this stream's shape could use, in kbps.
///
/// The probe-measured ceiling is pure link capacity (`delivered × 0.7`) with
/// no term for pixels. A CBR encoder fills whatever target it is handed, so
/// utilization never supplies one. Deliberately generous: a bound on the
/// absurd, not a quality opinion. Explicit-bitrate and PyroWave sessions
/// never reach here.
pub(crate) fn stream_ceiling_kbps(
    width: u32,
    height: u32,
    refresh_hz: u32,
    codec: u8,
    bit_depth: u8,
    chroma_format: u8,
) -> u32 {
    let pixel_rate = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(refresh_hz.max(1) as u64);
    if pixel_rate == 0 {
        return u32::MAX;
    }
    // Milli-bits per pixel so the arithmetic stays integer. H.264 is the
    // least efficient of the three and is allowed correspondingly more.
    let milli_bpp: u64 = match codec {
        crate::quic::CODEC_H264 => 1_000,
        _ => 750,
    };
    // 10-bit is 25 % more sample depth; 4:4:4 is twice the chroma of 4:2:0
    // → half again as many samples overall.
    let milli_bpp = if bit_depth >= 10 {
        milli_bpp * 5 / 4
    } else {
        milli_bpp
    };
    let milli_bpp = if chroma_format == crate::quic::CHROMA_IDC_444 {
        milli_bpp * 3 / 2
    } else {
        milli_bpp
    };
    // bits/s = pixel_rate × bpp; kbps = that / 1000. The milli- factor and
    // the kbps divisor cancel: pixel_rate × milli_bpp / 1_000_000.
    u32::try_from(pixel_rate.saturating_mul(milli_bpp) / 1_000_000).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bound cuts an absurd probe ceiling and must not trim a session anyone runs.
    #[test]
    fn the_stream_bound_cuts_the_absurd_and_spares_the_ordinary() {
        use crate::quic::{CHROMA_IDC_420, CHROMA_IDC_444, CODEC_H264, CODEC_HEVC};

        // 1440p120 HEVC Main10 4:2:0: bound must sit under the ~460 Mbps decode knee.
        let field = stream_ceiling_kbps(2560, 1440, 120, CODEC_HEVC, 10, CHROMA_IDC_420);
        assert!(
            field < 657_000,
            "the bound must actually bind on the field case, got {field}"
        );
        assert!(
            field < 460_000,
            "and land under the decode knee this session found, got {field}"
        );

        // 1080p60 HEVC 8-bit: 80–100 Mbps sessions must keep headroom.
        let ordinary = stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_420);
        assert!(
            ordinary >= 90_000,
            "an ordinary 1080p60 session must keep its headroom, got {ordinary}"
        );

        // H.264, 10-bit, and 4:4:4 are each allowed more.
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_H264, 8, CHROMA_IDC_420) > ordinary,
            "H.264 is allowed more than HEVC"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 10, CHROMA_IDC_420) > ordinary,
            "10-bit is allowed more than 8-bit"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_444) > ordinary,
            "4:4:4 is allowed more than 4:2:0"
        );
        // Degenerate mode must not bound at zero.
        assert_eq!(
            stream_ceiling_kbps(0, 0, 0, CODEC_HEVC, 8, CHROMA_IDC_420),
            u32::MAX
        );
    }
}
