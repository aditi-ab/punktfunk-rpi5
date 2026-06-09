//! NVENC encoder via `ffmpeg-next` (binds the system FFmpeg 7.x / libavcodec 61).
//!
//! Input is a packed RGB/BGR CPU frame; `*_nvenc` accepts `rgb0`/`bgr0`/`rgba`/`bgra`
//! directly and does the RGB→YUV conversion on the GPU, so the host stays off the
//! colour-conversion path. The portal commonly negotiates packed 24-bit `RGB`, which NVENC
//! does *not* accept — we expand it to `rgb0` (one padding byte/pixel, no colour math).
//! The encoder is opened *without* a global header so VPS/SPS/PPS are emitted in-band on
//! every IDR — the output is both a playable raw Annex-B stream and self-contained AUs.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, PixelFormat};
use anyhow::{anyhow, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video as VideoFrame;
use ffmpeg::{codec, encoder, Dictionary, Packet, Rational};
use ffmpeg_next as ffmpeg;

/// Map a captured layout to the NVENC input pixel format, and whether a 3→4 byte expand is
/// needed (packed RGB/BGR have no padding byte; the NVENC `*0` formats do).
fn nvenc_input(format: PixelFormat) -> (Pixel, bool) {
    match format {
        PixelFormat::Bgrx => (Pixel::BGRZ, false), // bgr0
        PixelFormat::Rgbx => (Pixel::RGBZ, false), // rgb0
        PixelFormat::Bgra => (Pixel::BGRA, false),
        PixelFormat::Rgba => (Pixel::RGBA, false),
        PixelFormat::Rgb => (Pixel::RGBZ, true), // RGB -> rgb0
        PixelFormat::Bgr => (Pixel::BGRZ, true), // BGR -> bgr0
    }
}

pub struct NvencEncoder {
    enc: encoder::video::Encoder,
    /// Reusable 4-bpp input frame in `nvenc_pixel` (its plane stride may exceed width*4).
    /// Mutating it in place across frames is sound only because the encoder is opened with
    /// `delay=0`/`bf=0`/`max_b_frames=0` and the caller drains `poll()` after each `submit`,
    /// so libavcodec holds no reference to the previous frame's buffer when we overwrite it.
    frame: VideoFrame,
    src_format: PixelFormat,
    expand: bool,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic presentation index, in `1/fps` time-base units.
    frame_idx: i64,
}

impl NvencEncoder {
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        ffmpeg::init().context("ffmpeg init")?;
        let name = codec.nvenc_name();
        let av_codec = encoder::find_by_name(name)
            .ok_or_else(|| anyhow!("{name} not built into libavcodec"))?;
        let (nvenc_pixel, expand) = nvenc_input(format);

        let mut video = codec::context::Context::new_with_codec(av_codec)
            .encoder()
            .video()
            .context("alloc video encoder")?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(nvenc_pixel); // NVENC converts RGB→YUV internally
        video.set_time_base(Rational(1, fps as i32));
        video.set_frame_rate(Some(Rational(fps as i32, 1)));
        video.set_bit_rate(bitrate_bps as usize);
        video.set_max_bit_rate(bitrate_bps as usize);
        video.set_gop(fps.saturating_mul(2).max(1)); // ~2s keyframe interval
        video.set_max_b_frames(0);

        // Low-latency NVENC tuning (plan §7 / linux-setup doc).
        let mut opts = Dictionary::new();
        opts.set("preset", "p1"); // fastest
        opts.set("tune", "ull"); // ultra-low-latency
        opts.set("rc", "cbr");
        opts.set("bf", "0");
        opts.set("delay", "0");

        let enc = video
            .open_with(opts)
            .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))?;

        let frame = VideoFrame::new(nvenc_pixel, width, height);
        Ok(NvencEncoder {
            enc,
            frame,
            src_format: format,
            expand,
            width,
            height,
            fps,
            frame_idx: 0,
        })
    }
}

impl Encoder for NvencEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        anyhow::ensure!(
            captured.format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            captured.format,
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_bpp = self.src_format.bytes_per_pixel();
        let src_row = w * src_bpp;
        anyhow::ensure!(
            captured.cpu_bytes.len() >= src_row * h,
            "captured buffer {} bytes < required {}",
            captured.cpu_bytes.len(),
            src_row * h
        );

        let stride = self.frame.stride(0); // dst is 4-bpp, aligned
        let dst = self.frame.data_mut(0);
        if self.expand {
            // packed 3-bpp RGB/BGR → 4-bpp *0 (copy 3 bytes, zero the pad byte)
            for y in 0..h {
                let s = &captured.cpu_bytes[y * src_row..y * src_row + src_row];
                let drow = &mut dst[y * stride..y * stride + w * 4];
                for x in 0..w {
                    drow[x * 4..x * 4 + 3].copy_from_slice(&s[x * 3..x * 3 + 3]);
                    drow[x * 4 + 3] = 0;
                }
            }
        } else {
            // 4-bpp → 4-bpp, honoring the (possibly larger) dst stride
            for y in 0..h {
                dst[y * stride..y * stride + src_row]
                    .copy_from_slice(&captured.cpu_bytes[y * src_row..y * src_row + src_row]);
            }
        }
        self.frame.set_pts(Some(self.frame_idx));
        self.frame_idx += 1;
        self.enc.send_frame(&self.frame).context("send_frame")?;
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let mut pkt = Packet::empty();
        match self.enc.receive_packet(&mut pkt) {
            Ok(()) => {
                let data = pkt.data().map(|d| d.to_vec()).unwrap_or_default();
                let pts = pkt.pts().unwrap_or(0).max(0) as u64;
                let pts_ns = pts * 1_000_000_000 / self.fps as u64;
                Ok(Some(EncodedFrame {
                    data,
                    pts_ns,
                    keyframe: pkt.is_key(),
                }))
            }
            // No packet ready yet (need another input frame).
            Err(ffmpeg::Error::Other { errno })
                if errno == ffmpeg::util::error::EAGAIN
                    || errno == ffmpeg::util::error::EWOULDBLOCK =>
            {
                Ok(None)
            }
            // Fully drained after flush().
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(e) => Err(e).context("receive_packet"),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.enc.send_eof().context("send_eof")?;
        Ok(())
    }
}
