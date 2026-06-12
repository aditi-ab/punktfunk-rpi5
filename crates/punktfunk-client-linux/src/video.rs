//! Video decode: reassembled HEVC access units → RGBA frames for the GTK presenter.
//!
//! Stage 1 is libavcodec software decode + swscale to RGBA (`GdkMemoryTexture` upload on
//! the UI side). The host encodes zero-reorder streams (no B-frames, in-band parameter
//! sets on every IDR), so with `AV_CODEC_FLAG_LOW_DELAY` the decoder is strictly
//! one-in/one-out with no hidden queue. Slice threading only — frame threading would add
//! a frame of latency per extra thread.
//!
//! Stage 1.5 (Intel/AMD boxes): VAAPI hwaccel → DRM-PRIME dmabuf → `GdkDmabufTexture`,
//! slotting in behind the same `decode()` signature. Stage 2 (NVIDIA): Vulkan Video in
//! the bespoke presenter (see the design notes in docs-site).

use anyhow::{anyhow, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;

/// One decoded frame, tightly enough packed for `GdkMemoryTexture` (which takes a stride).
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub rgba: Vec<u8>,
}

pub struct Decoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size changes (mid-stream `Reconfigure`).
    sws: Option<(scaling::Context, Pixel, u32, u32)>,
}

impl Decoder {
    pub fn new() -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
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
        Ok(Decoder { decoder, sws: None })
    }

    /// Feed one access unit; returns the decoded frame (the host's streams are
    /// one-in/one-out). A decode error after packet loss is survivable — log upstream and
    /// keep feeding; the host's RFI/IDR recovery resynchronizes the reference chain.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedFrame>> {
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

    fn convert_rgba(&mut self, frame: &AvFrame) -> Result<DecodedFrame> {
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
        Ok(DecodedFrame {
            width: w,
            height: h,
            stride: rgba.stride(0),
            rgba: rgba.data(0).to_vec(),
        })
    }
}
