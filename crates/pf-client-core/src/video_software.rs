//! CPU/libavcodec software decode backend (swscale → RGBA).

use crate::video::{averr, CpuFrame};
use crate::video_color::ColorDesc;
use anyhow::{anyhow, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;
use std::ptr;

// --- software backend ---------------------------------------------------------------

pub(crate) struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size — or the colour signaling (a mid-stream
    /// SDR↔HDR flip) — changes.
    sws: Option<(scaling::Context, Pixel, u32, u32, ColorDesc)>,
}

impl SoftwareDecoder {
    pub(crate) fn new(codec_id: ffmpeg::codec::Id) -> Result<SoftwareDecoder> {
        let codec = ffmpeg::decoder::find(codec_id)
            .ok_or_else(|| anyhow!("no {codec_id:?} decoder in libavcodec"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        // SAFETY: `as_mut_ptr` yields the `AVCodecContext` behind the `ctx` allocated on the line
        // above, which outlives these writes; each store is an in-bounds scalar field write on that
        // live context, made before the decoder is opened and reads them.
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open video decoder")?;
        Ok(SoftwareDecoder { decoder, sws: None })
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<CpuFrame>> {
        let packet = ffmpeg::Packet::copy(au);
        self.decoder
            .send_packet(&packet)
            .map_err(|e| anyhow!("send_packet: {e}"))?;
        let mut frame = AvFrame::empty();
        let mut out = None;
        while self.decoder.receive_frame(&mut frame).is_ok() {
            out = Some(self.convert_rgba(&frame)?);
        }
        Ok(out)
    }

    fn convert_rgba(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        // SAFETY: `frame.as_ptr()` is the decoder-owned live AVFrame for this call.
        let color = unsafe { ColorDesc::from_raw(frame.as_ptr()) };
        let rebuild = !matches!(&self.sws,
            Some((_, f, sw, sh, c)) if *f == fmt && *sw == w && *sh == h && *c == color);
        if rebuild {
            let mut ctx =
                scaling::Context::get(fmt, w, h, Pixel::RGBA, w, h, scaling::Flags::POINT)
                    .context("swscale context")?;
            // swscale defaults to BT.601 coefficients — set them from the FRAME's signaling
            // (unspecified → BT.709 limited, the host's SDR default; a Windows HDR desktop
            // streams BT.2020 in-band). Without this, YUV→RGB decodes with the wrong matrix
            // and colours shift. Destination = full-range RGB; the transfer function stays
            // baked in (the presenter tags PQ textures so GTK applies the EOTF).
            const SWS_CS_ITU709: i32 = 1;
            const SWS_CS_ITU601: i32 = 5;
            const SWS_CS_BT2020: i32 = 9;
            let cs = match color.matrix {
                9 | 10 => SWS_CS_BT2020,
                5 | 6 => SWS_CS_ITU601,
                _ => SWS_CS_ITU709,
            };
            // SAFETY: `sws_getCoefficients` returns a pointer into libav's own static coefficient
            // tables — valid for the process, read-only — and `sws_setColorspaceDetails` takes it
            // plus the live `SwsContext` behind `ctx` and plain scalars.
            unsafe {
                let coeffs = ffmpeg::ffi::sws_getCoefficients(cs);
                ffmpeg::ffi::sws_setColorspaceDetails(
                    ctx.as_mut_ptr(),
                    coeffs, // inv_table: source (YUV) coefficients per the VUI
                    color.full_range as i32, // srcRange: 0 = limited/studio (MPEG)
                    coeffs, // table: destination coefficients (ignored for RGB output)
                    1,      // dstRange: 1 = full-range RGB
                    0,
                    1 << 16,
                    1 << 16, // brightness, contrast, saturation (defaults)
                );
            }
            self.sws = Some((ctx, fmt, w, h, color));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        // Single-pass conversion: swscale writes straight into the Vec the texture will
        // wrap. (The old path scaled into a scratch AVFrame and then copied `data(0)` out
        // — a second full-frame pass per frame.) 64-byte row alignment keeps swscale on
        // aligned SIMD stores; `GdkMemoryTexture` takes the resulting stride explicitly.
        const ALIGN: i32 = 64;
        use ffmpeg::ffi;
        let dst_fmt = ffi::AVPixelFormat::AV_PIX_FMT_RGBA;
        // SAFETY: pure size computation from format/dimensions; no pointers involved.
        let size = unsafe { ffi::av_image_get_buffer_size(dst_fmt, w as i32, h as i32, ALIGN) };
        if size < 0 {
            return Err(averr("av_image_get_buffer_size", size));
        }
        let rgba = vec![0u8; size as usize];
        let mut dst_data: [*mut u8; 4] = [ptr::null_mut(); 4];
        let mut dst_linesize: [i32; 4] = [0; 4];
        // SAFETY: fill_arrays only derives plane pointers/strides into `rgba` (sized by
        // av_image_get_buffer_size above, same format/align) — no allocation, no
        // ownership transfer; `rgba` outlives the scale below.
        let r = unsafe {
            ffi::av_image_fill_arrays(
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
                rgba.as_ptr(),
                dst_fmt,
                w as i32,
                h as i32,
                ALIGN,
            )
        };
        if r < 0 {
            return Err(averr("av_image_fill_arrays", r));
        }
        // SAFETY: src pointers/strides belong to the decoder-owned `frame` (alive for the
        // call); dst pointers were just filled over `rgba`, and sws_scale writes rows
        // [0, h) only — exactly the buffer fill_arrays sized.
        let r = unsafe {
            ffi::sws_scale(
                sws.as_mut_ptr(),
                (*frame.as_ptr()).data.as_ptr() as *const *const u8,
                (*frame.as_ptr()).linesize.as_ptr(),
                0,
                h as i32,
                dst_data.as_ptr(),
                dst_linesize.as_ptr(),
            )
        };
        if r < 0 {
            return Err(averr("sws_scale", r));
        }
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: dst_linesize[0] as usize,
            rgba,
            color,
            // `is_key()` reads the same intra flag `frame_is_keyframe` derives from pict_type
            // for the hardware paths; ffmpeg-next handles the FFmpeg-version binding split.
            keyframe: frame.is_key(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire → `ColorDesc` plumbing: an HDR10 stream's VUI (BT.2020 primaries, PQ
    /// transfer, BT.2020-NCL matrix, limited range) must arrive on the decoded frame —
    /// this is what the Windows host emits in-band for an HDR desktop, and mis-rendering
    /// it as BT.709 is the washed-out-colors bug. Fixture: one 64×64 Main10 IDR
    /// (`tests/pq-frame.h265`, x265 with explicit VUI).
    #[test]
    fn software_decode_carries_pq_signaling() {
        let au = include_bytes!("../tests/pq-frame.h265");
        let mut dec = SoftwareDecoder::new(ffmpeg::codec::Id::HEVC).expect("hevc decoder");
        let mut got = dec.decode(au).expect("decode");
        if got.is_none() {
            // Low-delay decoders may still hold the frame until a flush — send EOF.
            dec.decoder.send_eof().ok();
            let mut frame = AvFrame::empty();
            if dec.decoder.receive_frame(&mut frame).is_ok() {
                got = Some(dec.convert_rgba(&frame).expect("convert"));
            }
        }
        let f = got.expect("no frame decoded from the PQ fixture");
        assert_eq!(
            f.color,
            ColorDesc {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false
            }
        );
        assert!(f.color.is_pq());
        assert_eq!((f.width, f.height), (64, 64));
    }

    /// Golden colour fixtures: one 256×64 LOSSLESS x265 IDR of 8 fully-saturated colour bars per
    /// signaling variant (generated offline with ffmpeg/libx265; the RGB→YUV conversion matched
    /// to the VUI each fixture declares, so the original RGB is recoverable ±1 code). Decoding
    /// through the real CPU path (`SoftwareDecoder` → per-frame `ColorDesc` → swscale with the
    /// signaled matrix/range) must reproduce the bars — the end-to-end guard for the
    /// signaling-driven CSC across BT.601/709 × limited/full. A hardcoded-709 regression fails
    /// the 601 fixture by tens of code points; a range mix-up fails the full-range one.
    #[test]
    fn software_decode_reproduces_golden_bars() {
        const BARS: [(u8, u8, u8); 8] = [
            (255, 255, 255),
            (255, 255, 0),
            (0, 255, 255),
            (0, 255, 0),
            (255, 0, 255),
            (255, 0, 0),
            (0, 0, 255),
            (0, 0, 0),
        ];
        let fixtures: [(&str, &[u8], ColorDesc); 3] = [
            (
                "601-limited",
                include_bytes!("../tests/bars-601-limited.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 5, // BT.470BG — what a Linux host's RGB-input NVENC signals
                    full_range: false,
                },
            ),
            (
                "709-limited",
                include_bytes!("../tests/bars-709-limited.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: false,
                },
            ),
            (
                "709-full",
                include_bytes!("../tests/bars-709-full.h265"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: true, // the PUNKTFUNK_444_FULLRANGE experiment's signaling
                },
            ),
        ];
        for (name, au, want_color) in fixtures {
            let mut dec = SoftwareDecoder::new(ffmpeg::codec::Id::HEVC).expect("hevc decoder");
            let mut got = dec.decode(au).expect("decode");
            if got.is_none() {
                dec.decoder.send_eof().ok();
                let mut frame = AvFrame::empty();
                if dec.decoder.receive_frame(&mut frame).is_ok() {
                    got = Some(dec.convert_rgba(&frame).expect("convert"));
                }
            }
            let f = got.unwrap_or_else(|| panic!("{name}: no frame decoded"));
            assert_eq!(f.color, want_color, "{name}: signaling");
            assert_eq!((f.width, f.height), (256, 64), "{name}: dims");
            for (i, (r, g, b)) in BARS.iter().enumerate() {
                let (cx, cy) = (i * 32 + 16, 32usize);
                let o = cy * f.stride + cx * 4;
                let px = &f.rgba[o..o + 3];
                for (got, want) in px.iter().zip([r, g, b]) {
                    assert!(
                        got.abs_diff(*want) <= 3,
                        "{name} bar {i}: got {px:?}, want ({r},{g},{b})"
                    );
                }
            }
        }
    }
}
