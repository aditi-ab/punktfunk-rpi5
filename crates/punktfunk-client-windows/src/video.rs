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

/// RGBA pixels for a D3D11 `R8G8B8A8_UNORM` texture upload (which takes a row pitch).
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub rgba: Vec<u8>,
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
    /// Rebuilt whenever the decoded format/size changes (mid-stream `Reconfigure`).
    sws: Option<(scaling::Context, Pixel, u32, u32)>,
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
            out = Some(self.convert_rgba(&frame)?);
        }
        Ok(out)
    }

    fn convert_rgba(&mut self, frame: &AvFrame) -> Result<CpuFrame> {
        let (fmt, w, h) = (frame.format(), frame.width(), frame.height());
        let rebuild =
            !matches!(&self.sws, Some((_, f, sw, sh)) if *f == fmt && *sw == w && *sh == h);
        if rebuild {
            let ctx = scaling::Context::get(fmt, w, h, Pixel::RGBA, w, h, scaling::Flags::POINT)
                .context("swscale context")?;
            self.sws = Some((ctx, fmt, w, h));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        let mut rgba = AvFrame::empty();
        sws.run(frame, &mut rgba).map_err(|e| anyhow!("sws: {e}"))?;
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: rgba.stride(0),
            rgba: rgba.data(0).to_vec(),
        })
    }
}
