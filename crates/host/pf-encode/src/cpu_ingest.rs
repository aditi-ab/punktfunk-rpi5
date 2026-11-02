//! The one stage a CPU frame (`FramePayload::Cpu`) passes before a Linux backend copies it to
//! the device: 4-byte pixels in the channel order the backend asks for, rows `width * 4` apart.
//!
//! Pure: no `ash`, no CUDA, so the table below runs on every target. NVENC's `upload_cpu`,
//! VAAPI's packed upload, Vulkan Video's `stage_cpu` and PyroWave's `ensure_cpu_rgb` each hold
//! a [`CpuIngest`] and keep only their device copy. [`CpuIngest::stage`] borrows the payload
//! when it already fits and otherwise writes into a scratch buffer reused across frames, so a
//! CPU frame allocates only when it outgrows the last one. 24-bit pixels gain an opaque pad
//! byte; formats the stage does not lay out (10-bit packed, YUV) pass through for the backend
//! to take or refuse. A packed payload short of its frame is refused before any device copy.
//!
//! Each row of `stage_table` names the layout it pins. Design: `design/pf-encode-overhaul.md`
//! §4.5.

use anyhow::{bail, ensure, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// The channel order a backend stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The captured order; only 24-bit pixels change, to 32.
    AsCaptured,
    /// B, G, R, pad: an encode source whose picture format is BGRA.
    BFirst,
}

/// A CPU frame laid out for the device copy.
#[derive(Debug)]
pub(crate) struct Staging<'a> {
    pub(crate) fmt: PixelFormat,
    pub(crate) bytes: &'a [u8],
    /// Bytes from one row to the next.
    pub(crate) row: usize,
}

/// The stage's scratch, kept by a backend across frames.
#[derive(Debug, Default)]
pub(crate) struct CpuIngest {
    scratch: Vec<u8>,
}

impl CpuIngest {
    /// `frame`'s pixels as `target` wants them, borrowed from the payload or from the scratch.
    pub(crate) fn stage<'a>(
        &'a mut self,
        frame: &'a CapturedFrame,
        target: Target,
    ) -> Result<Staging<'a>> {
        let FramePayload::Cpu(bytes) = &frame.payload else {
            bail!("stage a CPU frame: the payload holds no CPU pixels");
        };
        // A short payload would leave the last frame's tail in a backend's staging buffer.
        if !matches!(
            frame.format,
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Yuv444
        ) {
            let px = u64::from(frame.width) * u64::from(frame.height);
            let need = px.saturating_mul(frame.format.bytes_per_pixel() as u64);
            ensure!(
                bytes.len() as u64 >= need,
                "CPU frame {}x{} {:?} needs {need} bytes, got {}",
                frame.width,
                frame.height,
                frame.format,
                bytes.len()
            );
        }
        let bgra = target == Target::BFirst;
        let (fmt, bytes) = normalize_cpu_rgb(frame.format, bytes, &mut self.scratch, bgra);
        Ok(Staging {
            fmt,
            bytes,
            row: frame.width as usize * 4,
        })
    }
}

/// Rec.709 Kr / Kb; Kg = 1 − Kr − Kb.
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;
const KG: f32 = 1.0 - KR - KB;

/// Full-range RGB (0..=255) → BT.709 luma, full range or studio (16..=235). Lockstep with
/// `rgb2yuv.comp` and `pf-client-video::video_color::csc_rows`.
pub(crate) fn luma709(r: f32, g: f32, b: f32, full_range: bool) -> u8 {
    let y = KR * r + KG * g + KB * b;
    let y = if full_range {
        y
    } else {
        16.0 + y * (219.0 / 255.0)
    };
    (y + 0.5) as u8 // `as` saturates — no manual clamp needed
}

/// Averaged full-range RGB → BT.709 Cb/Cr (neutral 128), full range or studio (16..=240).
pub(crate) fn chroma709(r: f32, g: f32, b: f32, full_range: bool) -> (u8, u8) {
    let y = KR * r + KG * g + KB * b;
    let swing = if full_range { 1.0 } else { 224.0 / 255.0 };
    let cb = 128.0 + (b - y) * swing / (2.0 * (1.0 - KB));
    let cr = 128.0 + (r - y) * swing / (2.0 * (1.0 - KR));
    ((cb + 0.5) as u8, (cr + 0.5) as u8)
}

/// Expand packed 24-bpp RGB into `scratch` as 4-bpp with pad 0xFF: no 24-bpp `VkFormat` is
/// reliably sampleable and no backend takes 24-bit input.
///
/// `bgra_target = false` keeps the channel order; `true` writes B,G,R,pad, replacing a 4-bpp
/// source's alpha. A trailing partial pixel is dropped.
fn normalize_cpu_rgb<'a>(
    fmt: PixelFormat,
    bytes: &'a [u8],
    scratch: &'a mut Vec<u8>,
    bgra_target: bool,
) -> (PixelFormat, &'a [u8]) {
    let (bpp, r, g, b) = match fmt {
        PixelFormat::Rgb => (3usize, 0usize, 1usize, 2usize),
        PixelFormat::Bgr => (3, 2, 1, 0),
        PixelFormat::Rgbx | PixelFormat::Rgba => (4, 0, 1, 2),
        PixelFormat::Bgrx | PixelFormat::Bgra => (4, 2, 1, 0),
        _ => return (fmt, bytes),
    };
    if bpp == 4 && (!bgra_target || b == 0) {
        return (fmt, bytes); // 4-bpp already in the target order: borrow
    }
    let px = bytes.len() / bpp;
    scratch.clear();
    scratch.resize(px * 4, 0xFF);
    let (dr, dg, db) = if bgra_target { (2, 1, 0) } else { (r, g, b) };
    for (dst, src) in scratch.chunks_exact_mut(4).zip(bytes.chunks_exact(bpp)) {
        dst[dr] = src[r];
        dst[dg] = src[g];
        dst[db] = src[b];
    }
    let out_fmt = if bgra_target || b == 0 {
        PixelFormat::Bgrx
    } else {
        PixelFormat::Rgbx
    };
    (out_fmt, scratch.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use PixelFormat as P;

    fn frame(format: PixelFormat, (width, height): (u32, u32), bytes: &[u8]) -> CapturedFrame {
        CapturedFrame {
            width,
            height,
            pts_ns: 0,
            format,
            payload: FramePayload::Cpu(bytes.to_vec()),
            cursor: None,
            provenance: Default::default(),
        }
    }

    /// What a row stages: the format, the bytes, the row pitch, and whether they are the
    /// payload's own.
    type Staged = (PixelFormat, Vec<u8>, usize, bool);

    #[test]
    fn stage_table() {
        const FF: u8 = 0xFF;
        let rows: Vec<(&str, CapturedFrame, Target, Option<Staged>)> = vec![
            (
                "expand 24-bit RGB in its own order",
                frame(P::Rgb, (2, 1), &[1, 2, 3, 4, 5, 6]),
                Target::AsCaptured,
                Some((P::Rgbx, vec![1, 2, 3, FF, 4, 5, 6, FF], 8, false)),
            ),
            (
                "expand 24-bit BGR B-first",
                frame(P::Bgr, (1, 1), &[9, 8, 7]),
                Target::BFirst,
                Some((P::Bgrx, vec![9, 8, 7, FF], 4, false)),
            ),
            (
                "swap 24-bit RGB B-first",
                frame(P::Rgb, (1, 1), &[1, 2, 3]),
                Target::BFirst,
                Some((P::Bgrx, vec![3, 2, 1, FF], 4, false)),
            ),
            (
                "borrow 32-bit R-first in its own order",
                frame(P::Rgbx, (1, 1), &[1, 2, 3, 4]),
                Target::AsCaptured,
                Some((P::Rgbx, vec![1, 2, 3, 4], 4, true)),
            ),
            (
                "swap 32-bit R-first B-first behind an opaque pad",
                frame(P::Rgba, (1, 1), &[1, 2, 3, 4]),
                Target::BFirst,
                Some((P::Bgrx, vec![3, 2, 1, FF], 4, false)),
            ),
            (
                "borrow 32-bit B-first",
                frame(P::Bgra, (1, 1), &[10, 20, 30, 40]),
                Target::BFirst,
                Some((P::Bgra, vec![10, 20, 30, 40], 4, true)),
            ),
            (
                "pass 10-bit packed RGB through",
                frame(P::X2Rgb10, (1, 1), &[1, 2, 3, 4]),
                Target::BFirst,
                Some((P::X2Rgb10, vec![1, 2, 3, 4], 4, true)),
            ),
            (
                "an odd width keeps tight rows",
                frame(P::Bgr, (3, 1), &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
                Target::AsCaptured,
                Some((
                    P::Bgrx,
                    vec![1, 2, 3, FF, 4, 5, 6, FF, 7, 8, 9, FF],
                    12,
                    false,
                )),
            ),
            (
                "refuse a short 24-bit payload",
                frame(P::Rgb, (2, 1), &[1, 2, 3, 4, 5]),
                Target::AsCaptured,
                None,
            ),
            (
                "refuse a short 32-bit payload, even one it would borrow",
                frame(P::Bgrx, (2, 2), &[0; 12]),
                Target::BFirst,
                None,
            ),
            (
                "leave a YUV payload's size to the backend",
                frame(P::Nv12, (2, 2), &[0; 6]),
                Target::BFirst,
                Some((P::Nv12, vec![0; 6], 8, true)),
            ),
            (
                "a frame without CPU pixels is refused",
                CapturedFrame {
                    payload: FramePayload::Pixelless,
                    ..frame(P::Bgrx, (1, 1), &[])
                },
                Target::AsCaptured,
                None,
            ),
        ];
        let mut ingest = CpuIngest::default();
        for (name, frame, target, want) in rows {
            let got = ingest.stage(&frame, target).ok().map(|s| {
                let borrowed = match &frame.payload {
                    FramePayload::Cpu(p) => std::ptr::eq(s.bytes.as_ptr(), p.as_ptr()),
                    _ => false,
                };
                (s.fmt, s.bytes.to_vec(), s.row, borrowed)
            });
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn stage_reuses_its_scratch() {
        let mut ingest = CpuIngest::default();
        let f = frame(P::Rgb, (2, 2), &[7; 12]);
        let first = ingest.stage(&f, Target::BFirst).unwrap().bytes.as_ptr();
        let again = ingest.stage(&f, Target::BFirst).unwrap().bytes.as_ptr();
        assert_eq!(
            first, again,
            "a second frame of the same size allocates nothing"
        );
    }

    #[test]
    fn normalize_cpu_rgb_expands_24bpp_and_borrows_4bpp() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5, 6], &mut scratch, false);
        assert_eq!(f, PixelFormat::Rgbx);
        assert_eq!(b, &[1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // 5 bytes = one pixel + a 2-byte remainder that must be dropped.
        let mut scratch = Vec::new();
        let (_, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5], &mut scratch, false);
        assert_eq!(b, &[1, 2, 3, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgrx, &src, &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());
    }

    #[test]
    fn normalize_cpu_rgb_forces_bgra_for_the_encode_source() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // R-first 4-bpp: swap; source alpha replaced by the 0xFF pad.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgbx, &[1, 2, 3, 4], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgra, &src, &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgra);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());
    }
}
