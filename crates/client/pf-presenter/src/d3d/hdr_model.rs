//! The D3D11 backend's swapchain format rule (planning `windows-d3d11-presenter.md` D15),
//! pure so the table runs on every platform: the buffers and colour space a frame presents
//! through on the window's output, and whether the shader tonemaps it. `d3d::swapchain`
//! builds the format; `d3d::Presenter` follows each frame with it.

/// A swapchain's buffer format with its colour space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    /// BGRA8, sRGB (`G22_NONE_P709`).
    Sdr8,
    /// RGB10A2, sRGB (`G22_NONE_P709`).
    Sdr10,
    /// RGB10A2, PQ BT.2020 (`G2084_NONE_P2020`).
    Hdr10,
}

impl Format {
    /// A PQ picture on an SDR swapchain is tonemapped by the shader; HDR10 takes it as is.
    pub(crate) fn tonemaps(self, pq: bool) -> bool {
        pq && self != Format::Hdr10
    }
}

/// A PQ stream takes HDR10 where `hdr10` (the output is in HDR mode and the `hdr10` knob
/// allows it), else 8-bit sRGB. A 10-bit SDR stream takes 10-bit sRGB unless `sdr_8bit`,
/// which governs SDR only, as on the Vulkan backend.
pub(crate) fn pick_format(pq: bool, ten_bit: bool, hdr10: bool, sdr_8bit: bool) -> Format {
    match (pq, ten_bit) {
        (true, _) if hdr10 => Format::Hdr10,
        (false, true) if !sdr_8bit => Format::Sdr10,
        _ => Format::Sdr8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_swapchain_follows_the_stream_and_the_output() {
        use Format::*;
        // (name, pq, ten_bit, hdr10, sdr_8bit) → format, tonemapped
        #[rustfmt::skip]
        let rows = [
            ("8-bit SDR is BGRA8", false, false, true, false, Sdr8, false),
            ("10-bit SDR is RGB10A2 sRGB", false, true, false, false, Sdr10, false),
            ("sdr_8bit keeps 10-bit SDR on BGRA8", false, true, true, true, Sdr8, false),
            ("PQ on an HDR output is HDR10", true, true, true, false, Hdr10, false),
            ("sdr_8bit leaves HDR10 alone", true, true, true, true, Hdr10, false),
            ("PQ without HDR10 tonemaps on BGRA8", true, true, false, false, Sdr8, true),
            ("8-bit PQ without HDR10 tonemaps", true, false, false, true, Sdr8, true),
        ];
        for (name, pq, ten_bit, hdr10, sdr_8bit, format, tonemaps) in rows {
            let got = pick_format(pq, ten_bit, hdr10, sdr_8bit);
            assert_eq!(got, format, "{name}");
            assert_eq!(got.tonemaps(pq), tonemaps, "{name}");
        }
    }
}
