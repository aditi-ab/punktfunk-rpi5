//! Video decode: reassembled HEVC access units → frames for the D3D11 presenter.
//!
//! The dev box has no working GPU, so this ships the **software** backend first: libavcodec
//! on the CPU + swscale to RGBA, uploaded into a D3D11 texture by the presenter. It runs
//! `AV_CODEC_FLAG_LOW_DELAY` with slice threading only — the host encodes zero-reorder
//! streams (no B-frames, in-band parameter sets on every IDR), so decode is strictly
//! one-in/one-out and frame threading would only add latency.
//!
//! `DecodedFrame` is an enum so the real-GPU **D3D11VA** path (decode → `NV12`/`P010`
//! `ID3D11Texture2D`, zero-copy into the swapchain) can be added as a second variant without
//! touching the session pump or the presenter's frame contract.

use anyhow::{anyhow, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;

pub enum DecodedFrame {
    Cpu(CpuFrame),
}

/// Packed 4-byte-per-pixel frame for a D3D11 texture upload (which takes a row pitch). The bytes
/// are `R8G8B8A8` for SDR and `X2BGR10` (== DXGI `R10G10B10A2`, R in the low 10 bits) for HDR.
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub pixels: Vec<u8>,
    /// BT.2020 PQ HDR10 frame: `pixels` is `X2BGR10` and the presenter switches to a 10-bit
    /// R10G10B10A2 + ST.2084 swapchain. `false` = ordinary 8-bit BT.709 SDR.
    pub hdr: bool,
}

pub struct Decoder {
    inner: SoftwareDecoder,
}

impl Decoder {
    pub fn new() -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        Ok(Decoder {
            inner: SoftwareDecoder::new()?,
        })
    }

    /// Feed one access unit; returns the decoded frame (the host's streams are
    /// one-in/one-out). A decode error after packet loss is survivable — log upstream and
    /// keep feeding; the host's IDR/RFI recovery resynchronizes on the next keyframe.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedFrame>> {
        Ok(self.inner.decode(au)?.map(DecodedFrame::Cpu))
    }
}

struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size **or output format** changes (mid-stream
    /// `Reconfigure`, or an SDR↔HDR flip): `(ctx, src_fmt, w, h, dst_fmt)`.
    sws: Option<(scaling::Context, Pixel, u32, u32, Pixel)>,
}

impl SoftwareDecoder {
    fn new() -> Result<SoftwareDecoder> {
        let codec =
            ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC).ok_or(anyhow!("no HEVC decoder"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open HEVC decoder")?;
        Ok(SoftwareDecoder { decoder, sws: None })
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<CpuFrame>> {
        let packet = ffmpeg::Packet::copy(au);
        self.decoder
            .send_packet(&packet)
            .map_err(|e| anyhow!("send_packet: {e}"))?;
        let mut frame = AvFrame::empty();
        let mut out = None;
        while self.decoder.receive_frame(&mut frame).is_ok() {
            out = Some(self.convert(&frame)?);
        }
        Ok(out)
    }

    /// Convert the decoded YUV frame to a packed 4-byte format the presenter uploads directly:
    /// SDR → `RGBA` (BT.709), HDR (SMPTE ST.2084 / PQ transfer) → `X2BGR10` (10-bit, == DXGI
    /// R10G10B10A2) using the BT.2020 matrix. For HDR the PQ-encoded values pass through unchanged
    /// (swscale only applies the YUV→RGB matrix + range, never the transfer) — exactly what an
    /// HDR10/ST.2084 swapchain wants.
    fn convert(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        use ffmpeg::color::TransferCharacteristic;
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        let hdr = frame.color_transfer_characteristic() == TransferCharacteristic::SMPTE2084;
        let dst = if hdr { Pixel::X2BGR10LE } else { Pixel::RGBA };
        let rebuild = !matches!(&self.sws, Some((_, f, sw, sh, d)) if *f == fmt && *sw == w && *sh == h && *d == dst);
        if rebuild {
            let mut ctx = scaling::Context::get(fmt, w, h, dst, w, h, scaling::Flags::POINT)
                .context("swscale context")?;
            if hdr {
                // BT.2020 non-constant-luminance YUV (limited range) → full-range RGB. swscale
                // applies only the matrix + range here, so the samples stay PQ-encoded.
                unsafe {
                    let coef = ffmpeg::ffi::sws_getCoefficients(ffmpeg::ffi::SWS_CS_BT2020);
                    ffmpeg::ffi::sws_setColorspaceDetails(
                        ctx.as_mut_ptr(),
                        coef,
                        0, // src range: limited (video)
                        coef,
                        1, // dst range: full
                        0,
                        1 << 16,
                        1 << 16, // brightness / contrast / saturation defaults (16.16)
                    );
                }
            }
            self.sws = Some((ctx, fmt, w, h, dst));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        let mut conv = AvFrame::empty();
        sws.run(frame, &mut conv).map_err(|e| anyhow!("sws: {e}"))?;
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: conv.stride(0),
            pixels: conv.data(0).to_vec(),
            hdr,
        })
    }
}
