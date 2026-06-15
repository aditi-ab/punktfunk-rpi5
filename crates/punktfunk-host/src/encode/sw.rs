//! Software H.264 encoder (openh264) — the GPU-less encode path for the Windows host (and a
//! fallback when NVENC is unavailable). Low-latency screen-content config: single-reference,
//! no B-frames (Baseline), bitrate rate-control, in-band SPS/PPS each IDR, BT.709 limited range.
//! Synchronous: `submit` encodes immediately and stashes the AU for `poll` (no internal queue).

use super::{EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{bail, ensure, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder as Oh264, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    Profile, RateControlMode, SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, RgbSliceU8, YUVBuffer};
use openh264::OpenH264API;

pub struct OpenH264Encoder {
    enc: Oh264,
    yuv: YUVBuffer,
    width: u32,
    height: u32,
    fps: u32,
    src_format: PixelFormat,
    /// BGRA scratch for the 3-bpp (Bgr) and R/B-swapped (Rgba/Rgbx) formats openh264 can't wrap
    /// directly. Reused across frames.
    scratch: Vec<u8>,
    frame_idx: i64,
    force_kf: bool,
    /// At most one AU per submit (no lookahead), handed back by the next `poll`.
    pending: Option<EncodedFrame>,
}

// openh264's Encoder holds a raw C handle (not auto-Send); it lives on the single encode thread.
unsafe impl Send for OpenH264Encoder {}

impl OpenH264Encoder {
    pub fn open(
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        // validate_dimensions() ran in open_video: even, non-zero, <= 4096.
        let bps: u32 = bitrate_bps.try_into().unwrap_or(u32::MAX);
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(intra_period_frames(fps)))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId) // SPS/PPS in-band on every IDR
            .num_threads(num_threads())
            .scene_change_detect(false) // no surprise IDRs (bitrate spikes / freeze)
            .adaptive_quantization(true)
            .complexity(Complexity::Low) // latency over BD-rate
            .profile(Profile::Baseline); // no B-frames; BT.709 limited is the crate default VUI
        let api = OpenH264API::from_source(); // statically-bundled build (default `source` feature)
        let enc = Oh264::with_api_config(api, cfg).context("openh264 Encoder::with_api_config")?;
        let yuv = YUVBuffer::new(width as usize, height as usize);
        tracing::info!(
            "openh264 software encoder: {width}x{height}@{fps} {} Mbps (Baseline, screen-content)",
            bps / 1_000_000
        );
        Ok(Self {
            enc,
            yuv,
            width,
            height,
            fps,
            src_format: format,
            scratch: Vec::new(),
            frame_idx: 0,
            force_kf: false,
            pending: None,
        })
    }

    /// Normalize a packed source buffer into the reused BGRA `scratch` ([B,G,R,A]). `rgb_order`
    /// = source is R,G,B (swap into B,G,R); otherwise source is already B,G,R.
    fn normalize_to_bgra(&mut self, src: &[u8], src_bpp: usize, rgb_order: bool) {
        let w = self.width as usize;
        let h = self.height as usize;
        self.scratch.resize(w * h * 4, 0);
        for px in 0..(w * h) {
            let s = &src[px * src_bpp..px * src_bpp + 3];
            let d = &mut self.scratch[px * 4..px * 4 + 4];
            if rgb_order {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
            } else {
                d[0] = s[0];
                d[1] = s[1];
                d[2] = s[2];
            }
            d[3] = 0xff;
        }
    }
}

impl Encoder for OpenH264Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        ensure!(
            captured.format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            captured.format,
            self.src_format
        );
        // Refutable once the capture backend adds `FramePayload::D3d11`; today `Cpu` is the only
        // non-Linux variant, so the pattern is (temporarily) irrefutable.
        #[allow(irrefutable_let_patterns)]
        let FramePayload::Cpu(bytes) = &captured.payload
        else {
            bail!("openh264 backend requires a CPU frame payload");
        };
        let w = self.width as usize;
        let h = self.height as usize;
        ensure!(
            bytes.len() >= w * h * self.src_format.bytes_per_pixel(),
            "captured buffer {} bytes too small for {w}x{h} {:?}",
            bytes.len(),
            self.src_format
        );

        match self.src_format {
            PixelFormat::Rgb => self
                .yuv
                .read_rgb(RgbSliceU8::new(&bytes[..w * h * 3], (w, h))),
            PixelFormat::Bgra | PixelFormat::Bgrx => self
                .yuv
                .read_rgb(BgraSliceU8::new(&bytes[..w * h * 4], (w, h))),
            PixelFormat::Rgba | PixelFormat::Rgbx => {
                self.normalize_to_bgra(bytes, 4, true);
                self.yuv.read_rgb(BgraSliceU8::new(&self.scratch, (w, h)));
            }
            PixelFormat::Bgr => {
                self.normalize_to_bgra(bytes, 3, false);
                self.yuv.read_rgb(BgraSliceU8::new(&self.scratch, (w, h)));
            }
        }

        if self.force_kf {
            self.enc.force_intra_frame();
            self.force_kf = false;
        }
        let bs = self.enc.encode(&self.yuv).context("openh264 encode")?;
        let mut data = Vec::new();
        bs.write_vec(&mut data); // AnnexB start codes; SPS/PPS prepended on IDR
        if !data.is_empty() {
            let keyframe = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            let pts_ns = self.frame_idx as u64 * 1_000_000_000 / self.fps.max(1) as u64;
            self.pending = Some(EncodedFrame {
                data,
                pts_ns,
                keyframe,
            });
        }
        self.frame_idx += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.take())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // synchronous: nothing buffered
    }
}

/// Approximate infinite-GOP: insert IDRs rarely (recovery is via `request_keyframe`/RFI). Env
/// `PUNKTFUNK_OH264_GOP` overrides (0 = encoder-auto).
fn intra_period_frames(fps: u32) -> u32 {
    if let Ok(v) = std::env::var("PUNKTFUNK_OH264_GOP") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return n;
        }
    }
    fps.max(1).saturating_mul(600) // ~10 min between automatic IDRs
}

/// Encode threads. Env `PUNKTFUNK_OH264_THREADS` overrides; default 2 (latency over throughput).
fn num_threads() -> u16 {
    std::env::var("PUNKTFUNK_OH264_THREADS")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
        .unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CapturedFrame, FramePayload, PixelFormat};

    #[test]
    fn encodes_synthetic_frame_to_annexb_idr() {
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        let mut enc =
            OpenH264Encoder::open(PixelFormat::Bgrx, w, h, fps, 8_000_000).expect("open openh264");
        // A flat gray BGRx frame.
        let frame = CapturedFrame {
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0x80u8; (w * h * 4) as usize]),
        };
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("an AU");
        assert!(au.keyframe, "first frame must be an IDR");
        // AnnexB start code + an SPS NAL (type 7) somewhere in the first frame.
        assert!(
            au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
            "expected AnnexB start code"
        );
        let has_sps = au
            .data
            .windows(5)
            .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1 && (w[4] & 0x1f) == 7);
        assert!(has_sps, "IDR must carry an SPS NAL (type 7)");
    }
}
