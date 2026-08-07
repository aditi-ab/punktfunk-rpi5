//! The stream's per-frame colour signalling (`ColorDesc`) + the Y′CbCr→RGB CSC matrix (`csc_rows`).

/// The stream's colour signaling, read PER-FRAME out of the bitstream's own VUI /
/// sequence header (pf-bitstream, on every rung — the libavcodec `AVFrame` CICP read this
/// used to have went with M10's rungs). The Windows host switches an HDR desktop to
/// Main10 BT.2020 PQ **in-band** (the Welcome still says SDR — clients are expected to
/// follow the VUI, as the Windows/Apple/Android clients do), so rendering must follow the
/// frames, not the handshake — else PQ content drawn as BT.709 comes out washed out and
/// desaturated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorDesc {
    /// H.273 code points as signaled (2 = unspecified → the renderer picks the SDR default).
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorDesc {
    /// PQ (SMPTE ST.2084) transfer — the HDR10 signal.
    pub fn is_pq(&self) -> bool {
        self.transfer == 16
    }
}

/// The Y′CbCr→RGB conversion as three vec4 rows for a shader constant buffer / push-constant
/// block: `rgb[i] = dot(r[i].xyz, yuv) + r[i].w` — bit-depth exact. The ONE coefficient
/// implementation every presenter derives its CSC from (Vulkan push constants, the Windows
/// client's D3D11 constant buffer), so a stream's signaled matrix/range is honored identically
/// everywhere; the Apple client ports this function (and its tests) to Swift.
///
/// `depth` picks the limited-range code points (8-bit: 16/235/240 over 255; 10-bit:
/// 64/940/960 over 1023 — NOT the same normalized values, the difference is ~half a
/// code). `msb_packed` folds in the P010/X6 packing factor: 10 significant bits live in
/// the MSBs of 16, so a UNORM16 sample reads `code·64/65535` — multiplying by
/// `65535/65472` recovers exact `code/1023`.
pub fn csc_rows(desc: ColorDesc, depth: u8, msb_packed: bool) -> [[f32; 4]; 3] {
    // BT.601 (5/6), BT.2020 (9/10); everything else — incl. unspecified — is the host's
    // BT.709 SDR default. Since M8 this is the ONLY coefficient choice in the client:
    // the software rung's swscale (which defaulted to BT.601 and needed correcting) is
    // gone, and its planes come through this function like every hardware lane's.
    let (kr, kb) = match desc.matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let max = f64::from((1u32 << depth) - 1); // 255 / 1023
    let step = f64::from(1u32 << (depth - 8)); // code points per 8-bit step: 1 / 4
    let pack = if msb_packed { 65535.0 / 65472.0 } else { 1.0 };
    let (sy, oy, sc) = if desc.full_range {
        (pack, 0.0f64, pack)
    } else {
        (
            pack * max / (219.0 * step),
            -(16.0 * step) / max,
            pack * max / (224.0 * step),
        )
    };
    // rgb = M * (yuv + off) = M*yuv + M*off — rows of M with the offset dot folded into
    // w. `yuv` is the SAMPLED (packed) value, so the offsets divide by the packing
    // factor to land on the same scale.
    let off = [oy / pack, -0.5 / pack, -0.5 / pack];
    let m = [
        [sy, 0.0, 2.0 * (1.0 - kr) * sc],
        [
            sy,
            -2.0 * (1.0 - kb) * kb / kg * sc,
            -2.0 * (1.0 - kr) * kr / kg * sc,
        ],
        [sy, 2.0 * (1.0 - kb) * sc, 0.0],
    ];
    core::array::from_fn(|r| {
        let w: f64 = (0..3).map(|c| m[r][c] * off[c]).sum();
        [m[r][0] as f32, m[r][1] as f32, m[r][2] as f32, w as f32]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(matrix: u8, full_range: bool) -> ColorDesc {
        ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix,
            full_range,
        }
    }

    fn apply(rows: &[[f32; 4]; 3], yuv: [f32; 3]) -> [f32; 3] {
        core::array::from_fn(|r| {
            rows[r][0] * yuv[0] + rows[r][1] * yuv[1] + rows[r][2] * yuv[2] + rows[r][3]
        })
    }

    /// 10-bit limited MSB-packed (P010/X6): reference white Y=940, black Y=64, neutral
    /// chroma 512 — sampled as UNORM16 of `code << 6`.
    #[test]
    fn bt2020_10bit_limited_white_black() {
        let rows = csc_rows(desc(9, false), 10, true);
        let s = |code: u32| ((code << 6) as f32) / 65535.0;
        let white = apply(&rows, [s(940), s(512), s(512)]);
        let black = apply(&rows, [s(64), s(512), s(512)]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.002, "white {white:?}");
            assert!(b.abs() < 0.002, "black {black:?}");
        }
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0
    /// — the GL presenter's test, in row form.
    #[test]
    fn bt709_limited_white_black() {
        let rows = csc_rows(desc(1, false), 8, false);
        let white = apply(&rows, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let black = apply(&rows, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.005, "white {white:?}");
            assert!(b.abs() < 0.005, "black {black:?}");
        }
    }

    /// Full-range identity points + the 601-vs-709 red excursion (guards the
    /// matrix-code dispatch), same as the GL presenter's test.
    #[test]
    fn full_range_and_red_excursion() {
        let rows = csc_rows(desc(5, true), 8, false);
        let white = apply(&rows, [1.0, 0.5, 0.5]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let red = apply(&rows, [0.0, 0.5, 1.0]);
        assert!((red[0] - 2.0 * (1.0 - 0.299) * 0.5).abs() < 1e-4, "{red:?}");
        let rows709 = csc_rows(desc(1, true), 8, false);
        let red709 = apply(&rows709, [0.0, 0.5, 1.0]);
        assert!(
            (red709[0] - 2.0 * (1.0 - 0.2126) * 0.5).abs() < 1e-4,
            "{red709:?}"
        );
        assert!((red[0] - red709[0]).abs() > 0.05);
    }

    /// The row form must agree with the GL presenter's column-major `yuv_to_rgb` on a
    /// grid of inputs — same math, different packing.
    #[test]
    fn rows_match_the_gl_matrix_form() {
        for (matrix, full) in [(1u8, false), (1, true), (5, false), (9, false), (9, true)] {
            let d = desc(matrix, full);
            let rows = csc_rows(d, 8, false);
            // Reimplementation of video_gl::yuv_to_rgb's application for comparison.
            let (kr, kb) = match matrix {
                5 | 6 => (0.299f32, 0.114f32),
                9 | 10 => (0.2627, 0.0593),
                _ => (0.2126, 0.0722),
            };
            let kg = 1.0 - kr - kb;
            let (sy, oy, sc) = if full {
                (1.0f32, 0.0f32, 1.0f32)
            } else {
                (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
            };
            let mat = [
                sy,
                sy,
                sy,
                0.0,
                -2.0 * (1.0 - kb) * kb / kg * sc,
                2.0 * (1.0 - kb) * sc,
                2.0 * (1.0 - kr) * sc,
                -2.0 * (1.0 - kr) * kr / kg * sc,
                0.0,
            ];
            let off = [oy, -0.5, -0.5];
            for yuv in [
                [0.1f32, 0.3, 0.7],
                [0.9, 0.5, 0.5],
                [0.5, 0.2, 0.8],
                [16.0 / 255.0, 0.5, 0.5],
            ] {
                let v = [yuv[0] + off[0], yuv[1] + off[1], yuv[2] + off[2]];
                let gl: [f32; 3] =
                    core::array::from_fn(|r| (0..3).map(|c| mat[c * 3 + r] * v[c]).sum());
                let ours = apply(&rows, yuv);
                for (a, b) in gl.iter().zip(ours) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "{matrix}/{full}: gl {gl:?} rows {ours:?}"
                    );
                }
            }
        }
    }
}
