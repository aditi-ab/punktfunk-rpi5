//! Software H.264 encoder (openh264) — the GPU-less encode path for the Windows host (and a
//! fallback when NVENC is unavailable). Low-latency screen-content config: single-reference,
//! no B-frames (Baseline), bitrate rate-control, in-band SPS/PPS each IDR.
//! Synchronous: `submit` encodes immediately and stashes the AU for `poll` (no internal queue).
//!
//! The RGB→YUV conversion is OURS, BT.709 limited range, and the SPS VUI says so
//! ([`VuiConfig::bt709`], applied in `open`). The crate's own `YUVBuffer` converter is BT.601
//! (0.2578/0.5039/0.0977 + 16), which decoded-as-709 is a constant hue error; that's why it is
//! NOT used here.
//!
//! Signalling is not optional. This used to leave the VUI unwritten and lean on decoders
//! defaulting to BT.709 limited — true of every punktfunk client (`csc_rows` falls back to 709 on
//! "unspecified"), but NOT of vendor TV decoders, which guess colorimetry from RESOLUTION: an LG
//! webOS panel reads a 4K SDR stream as BT.2020 and renders it visibly washed out.

use super::{EncodedFrame, Encoder};
use anyhow::{bail, ensure, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder as Oh264, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    Profile, RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;

pub struct OpenH264Encoder {
    enc: Oh264,
    width: u32,
    height: u32,
    fps: u32,
    src_format: PixelFormat,
    /// The converted I420 planes (our BT.709-limited CSC — see the module doc), reused across
    /// frames: full-res luma + quarter-res Cb/Cr, tightly packed (stride = width, width/2).
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    frame_idx: i64,
    force_kf: bool,
    /// One AU per submit (no lookahead), handed back FIFO by `poll`. A queue, not an `Option`:
    /// the session loop pipelines up to `capturer.pipeline_depth()` submits before polling, and a
    /// single-slot pending would silently overwrite (lose) the older AUs — including the opening
    /// IDR — and permanently skew the loop's FIFO pts pairing.
    pending: VecDeque<EncodedFrame>,
}

// openh264's Encoder holds a raw C handle (not auto-Send); it lives on the single encode thread.
// SAFETY: `OpenH264Encoder` wraps `Oh264` (openh264's `Encoder`), which holds a raw C handle to the
// openh264 `ISVCEncoder` and is not auto-`Send`; the other fields (the plane `Vec`s, scalars,
// `Option<EncodedFrame>`) are plain owned data. The session creates the encoder, calls
// `submit`/`poll`/`flush`, and drops it all on one dedicated encode thread, never sharing it by
// reference across threads, so the C handle is only ever touched from a single thread. Moving the
// whole value to that thread is therefore sound — there is no concurrent access to the handle.
unsafe impl Send for OpenH264Encoder {}

/// openh264's own ceiling: level 5.2, so 3840x2160 landscape or 2160x3840 portrait.
///
/// The long edge may reach 3840 and the short edge 2160 — the rule is orientation-aware, not a
/// per-axis `w <= 3840 && h <= 2160`, so a portrait 2160x3840 session is legal.
const OPENH264_MAX_LONG_EDGE: u32 = 3840;
const OPENH264_MAX_SHORT_EDGE: u32 = 2160;

/// Whether the bundled openh264 can encode this resolution at all.
///
/// Mirrors the check inside the crate we ship (openh264 0.9.3, `encoder.rs` `reinit`). That check
/// runs on the FIRST ENCODE, not at encoder construction — so without this gate a too-large mode
/// opens perfectly and then fails *every* submit, and the session connects and never delivers a
/// frame. `Codec::max_dimension` does not cover it: it is keyed on the codec, and H.264 legitimately
/// reaches 4096 on every hardware backend — this ceiling belongs to the software backend alone.
fn openh264_supports_dimensions(width: u32, height: u32) -> bool {
    width.max(height) <= OPENH264_MAX_LONG_EDGE && width.min(height) <= OPENH264_MAX_SHORT_EDGE
}

impl OpenH264Encoder {
    pub fn open(
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        // validate_dimensions() ran in open_video: even, non-zero, <= 4096. That leaves modes this
        // encoder cannot serve (e.g. a legal 4096-wide H.264 mode), so refuse them here — at the
        // open, where the caller still gets a real error — rather than at every submit.
        ensure!(
            openh264_supports_dimensions(width, height),
            "openh264 cannot encode {width}x{height}: the software encoder tops out at \
             {OPENH264_MAX_LONG_EDGE}x{OPENH264_MAX_SHORT_EDGE} (or \
             {OPENH264_MAX_SHORT_EDGE}x{OPENH264_MAX_LONG_EDGE} portrait) — lower the client \
             resolution, or use a host with a hardware encoder"
        );
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
            .profile(Profile::Baseline) // no B-frames
            // video_signal_type + colour_description in the SPS VUI: BT.709 primaries/transfer/
            // matrix, video_full_range_flag = 0 — exactly what `convert_bt709` below produces.
            .vui(VuiConfig::bt709());
        let api = OpenH264API::from_source(); // statically-bundled build (default `source` feature)
        let enc = Oh264::with_api_config(api, cfg).context("openh264 Encoder::with_api_config")?;
        let (w, h) = (width as usize, height as usize);
        tracing::info!(
            "openh264 software encoder: {width}x{height}@{fps} {} Mbps (Baseline, screen-content)",
            bps / 1_000_000
        );
        Ok(Self {
            enc,
            width,
            height,
            fps,
            src_format: format,
            y_plane: vec![0; w * h],
            u_plane: vec![0; (w / 2) * (h / 2)],
            v_plane: vec![0; (w / 2) * (h / 2)],
            frame_idx: 0,
            force_kf: false,
            pending: VecDeque::new(),
        })
    }

    /// Convert one packed full-range RGB frame into the I420 planes, BT.709 limited range.
    /// `bpp` is the source pixel stride; `ri`/`gi`/`bi` the channel byte offsets within a pixel.
    /// Luma per pixel; Cb/Cr from the 2×2 block's averaged RGB (the same box filter the crate's
    /// converter used, so only the matrix changed).
    fn convert_bt709(&mut self, src: &[u8], bpp: usize, ri: usize, gi: usize, bi: usize) {
        let w = self.width as usize;
        let h = self.height as usize;
        let cw = w / 2;
        for by in 0..h / 2 {
            for bx in 0..cw {
                let mut sum = (0f32, 0f32, 0f32);
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let (px, py) = (bx * 2 + dx, by * 2 + dy);
                    let s = &src[(py * w + px) * bpp..];
                    let (r, g, b) = (f32::from(s[ri]), f32::from(s[gi]), f32::from(s[bi]));
                    self.y_plane[py * w + px] = luma709(r, g, b);
                    sum = (sum.0 + r, sum.1 + g, sum.2 + b);
                }
                let (cb, cr) = chroma709(sum.0 / 4.0, sum.1 / 4.0, sum.2 / 4.0);
                self.u_plane[by * cw + bx] = cb;
                self.v_plane[by * cw + bx] = cr;
            }
        }
    }
}

/// BT.709 luma coefficients (Kg = 1 − Kr − Kb).
const KR: f32 = 0.2126;
const KB: f32 = 0.0722;
const KG: f32 = 1.0 - KR - KB;

/// One full-range RGB pixel (0..=255 channels) → the BT.709 limited-range 8-bit luma code
/// (16..=235). Kept in lockstep with the client-side inverse (`pf-client-core::video::csc_rows`).
fn luma709(r: f32, g: f32, b: f32) -> u8 {
    let y = KR * r + KG * g + KB * b; // full-scale luma, 0..=255
    (16.0 + y * (219.0 / 255.0) + 0.5) as u8 // `as` saturates — no manual clamp needed
}

/// (Averaged) full-range RGB → the BT.709 limited-range Cb/Cr codes (16..=240, neutral 128).
fn chroma709(r: f32, g: f32, b: f32) -> (u8, u8) {
    let y = KR * r + KG * g + KB * b;
    let cb = 128.0 + (b - y) * (224.0 / 255.0) / (2.0 * (1.0 - KB));
    let cr = 128.0 + (r - y) * (224.0 / 255.0) / (2.0 * (1.0 - KR));
    ((cb + 0.5) as u8, (cr + 0.5) as u8)
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

        // Source pixel stride + R/G/B byte offsets within a pixel — one converter for every
        // packed-RGB layout the capturers emit (no BGRA normalization pass needed).
        let (bpp, ri, gi, bi) = match self.src_format {
            PixelFormat::Rgb => (3, 0, 1, 2),
            PixelFormat::Bgr => (3, 2, 1, 0),
            PixelFormat::Rgba | PixelFormat::Rgbx => (4, 0, 1, 2),
            PixelFormat::Bgra | PixelFormat::Bgrx => (4, 2, 1, 0),
            // 10-bit comes only from the GPU paths; the software 8-bit H.264 encoder can't
            // represent it (and never receives it — neither HDR nor 10-bit SDR is negotiated on
            // a software host).
            PixelFormat::Rgb10a2
            | PixelFormat::Rgb10a2Sdr
            | PixelFormat::X2Rgb10
            | PixelFormat::X2Bgr10 => {
                anyhow::bail!(
                    "software H.264 encoder cannot encode 10-bit ({:?})",
                    self.src_format
                )
            }
            // NV12/P010 are GPU-resident video-processor outputs for the NVENC path; the software
            // encoder never receives them (it only gets CPU RGB frames).
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Yuv444 => {
                anyhow::bail!(
                    "software encoder cannot encode YUV GPU frames (NV12/P010/YUV444 → NVENC only)"
                )
            }
        };
        self.convert_bt709(bytes, bpp, ri, gi, bi);

        if self.force_kf {
            self.enc.force_intra_frame();
            self.force_kf = false;
        }
        let slices = YUVSlices::new(
            (&self.y_plane, &self.u_plane, &self.v_plane),
            (w, h),
            (w, w / 2, w / 2),
        );
        let bs = self.enc.encode(&slices).context("openh264 encode")?;
        let mut data = Vec::new();
        bs.write_vec(&mut data); // AnnexB start codes; SPS/PPS prepended on IDR
        if !data.is_empty() {
            let keyframe = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            let pts_ns = self.frame_idx as u64 * 1_000_000_000 / self.fps.max(1) as u64;
            self.pending.push_back(EncodedFrame {
                data,
                pts_ns,
                keyframe,
                recovery_anchor: false,
                chunk_aligned: false,
            });
        }
        self.frame_idx += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.pop_front())
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
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

    /// The BT.709 limited-range anchor points: reference white → (235,128,128), black →
    /// (16,128,128), pure red's Cr must hit the positive extreme 240 (it does exactly:
    /// 255(1−Kr)·(224/255)/(2(1−Kr)) = 112). ±1 code for float rounding.
    #[test]
    fn bt709_conversion_anchor_points() {
        assert_eq!(luma709(255.0, 255.0, 255.0), 235);
        assert_eq!(luma709(0.0, 0.0, 0.0), 16);
        assert_eq!(chroma709(255.0, 255.0, 255.0), (128, 128));
        assert_eq!(chroma709(0.0, 0.0, 0.0), (128, 128));
        let (cb, cr) = chroma709(255.0, 0.0, 0.0);
        assert_eq!(cr, 240, "pure red must reach the Cr extreme");
        assert!((101..=103).contains(&cb), "red Cb ~102, got {cb}");
        let (cb, _) = chroma709(0.0, 0.0, 255.0);
        assert_eq!(cb, 240, "pure blue must reach the Cb extreme");
    }

    /// The 601-vs-709 luma split on pure green (Kg 0.587 vs 0.7152) — guards against anyone
    /// "simplifying" the coefficients back to the crate's BT.601 converter (the hue-shift bug
    /// this module's own conversion exists to prevent).
    #[test]
    fn bt709_is_not_bt601() {
        // BT.601 green luma: 16 + 219·0.587 = 144.5; BT.709: 16 + 219·0.7152 = 172.6.
        let y = luma709(0.0, 255.0, 0.0);
        assert!((172..=174).contains(&y), "709 green luma ~173, got {y}");
    }

    /// A flat gray frame converts to neutral chroma and mid luma across every plane byte
    /// (exercises the block loop + plane sizing, not just the per-pixel math).
    #[test]
    fn converts_flat_gray_to_neutral_planes() {
        let (w, h) = (16u32, 8u32);
        let mut enc =
            OpenH264Encoder::open(PixelFormat::Bgrx, w, h, 60, 1_000_000).expect("open openh264");
        let bytes = vec![0x80u8; (w * h * 4) as usize];
        enc.convert_bt709(&bytes, 4, 2, 1, 0);
        // 16 + 128·(219/255) = 125.9 → 126.
        assert!(
            enc.y_plane.iter().all(|&y| y == 126),
            "{:?}",
            &enc.y_plane[..4]
        );
        assert!(enc.u_plane.iter().all(|&u| u == 128));
        assert!(enc.v_plane.iter().all(|&v| v == 128));
    }

    #[test]
    fn encodes_synthetic_frame_to_annexb_idr() {
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        let mut enc =
            OpenH264Encoder::open(PixelFormat::Bgrx, w, h, fps, 8_000_000).expect("open openh264");
        // A flat gray BGRx frame.
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0x80u8; (w * h * 4) as usize]),
            cursor: None,
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

    /// Strip Annex-B framing + emulation-prevention bytes from the first SPS NAL in `au`.
    fn sps_rbsp(au: &[u8]) -> Vec<u8> {
        let start = au
            .windows(5)
            .position(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 7)
            .map(|p| p + 5)
            .expect("an SPS NAL");
        let end = au[start..]
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] || w == [0, 0, 0, 1])
            .map_or(au.len(), |p| start + p);
        let mut rbsp = Vec::new();
        let nal = &au[start..end];
        let mut i = 0;
        while i < nal.len() {
            // 00 00 03 -> the 03 is an emulation-prevention byte, not payload.
            if i + 2 < nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
                rbsp.extend_from_slice(&[0, 0]);
                i += 3;
            } else {
                rbsp.push(nal[i]);
                i += 1;
            }
        }
        rbsp
    }

    /// The colour signalling the SPS actually carries, walked per ITU-T H.264 §7.3.2.1.1: returns
    /// `(video_full_range_flag, colour_primaries, transfer_characteristics, matrix_coefficients)`.
    /// `None` when the stream is unsignalled — which is what this module used to emit.
    fn sps_colour(rbsp: &[u8]) -> Option<(u8, u8, u8, u8)> {
        // Exp-Golomb ue(v): count leading zeros, then read that many trailing bits.
        fn ue(u: &mut dyn FnMut(u32) -> u32) -> u32 {
            let mut lz = 0;
            while u(1) == 0 {
                lz += 1;
                assert!(lz < 32, "malformed Exp-Golomb");
            }
            if lz == 0 {
                0
            } else {
                (1 << lz) - 1 + u(lz)
            }
        }
        let mut pos = 0usize;
        let mut u = |bits: u32| -> u32 {
            let mut v = 0;
            for _ in 0..bits {
                v = (v << 1) | u32::from((rbsp[pos / 8] >> (7 - (pos % 8))) & 1);
                pos += 1;
            }
            v
        };
        let profile_idc = u(8);
        u(8); // constraint_set flags + reserved
        u(8); // level_idc
        ue(&mut u); // seq_parameter_set_id
        assert_eq!(
            profile_idc, 66,
            "this encoder is pinned to Baseline — a profile change adds the chroma_format_idc \
             block this walk deliberately omits"
        );
        ue(&mut u); // log2_max_frame_num_minus4
        let poc_type = ue(&mut u);
        match poc_type {
            0 => {
                ue(&mut u);
            } // log2_max_pic_order_cnt_lsb_minus4
            1 => panic!("pic_order_cnt_type 1 unhandled — openh264 emits 0 or 2"),
            _ => {}
        }
        ue(&mut u); // max_num_ref_frames
        u(1); // gaps_in_frame_num_value_allowed_flag
        ue(&mut u); // pic_width_in_mbs_minus1
        ue(&mut u); // pic_height_in_map_units_minus1
        if u(1) == 0 {
            u(1); // mb_adaptive_frame_field_flag
        }
        u(1); // direct_8x8_inference_flag
        if u(1) == 1 {
            for _ in 0..4 {
                ue(&mut u); // frame_crop_*_offset
            }
        }
        if u(1) == 0 {
            return None; // vui_parameters_present_flag
        }
        if u(1) == 1 {
            // aspect_ratio_info_present_flag
            if u(8) == 255 {
                u(16);
                u(16);
            }
        }
        if u(1) == 1 {
            u(1); // overscan_info_present_flag -> overscan_appropriate_flag
        }
        if u(1) == 0 {
            return None; // video_signal_type_present_flag
        }
        u(3); // video_format
        let full_range = u(1) as u8;
        if u(1) == 0 {
            return None; // colour_description_present_flag
        }
        Some((full_range, u(8) as u8, u(8) as u8, u(8) as u8))
    }

    /// The SPS must SIGNAL BT.709 limited, not merely be encoded that way. `VuiConfig::bt709()`
    /// is a request to a C library; this asserts it lands in the emitted bitstream.
    ///
    /// Unsignalled was the old behaviour and it looks fine on every punktfunk client (`csc_rows`
    /// defaults to BT.709 on "unspecified"), so nothing in our own stack catches a regression
    /// here — but vendor TV decoders guess colorimetry from RESOLUTION, and an LG webOS panel
    /// reads a 4K SDR stream as BT.2020 and renders it visibly washed out.
    #[test]
    fn sps_signals_bt709_limited() {
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        let mut enc =
            OpenH264Encoder::open(PixelFormat::Bgrx, w, h, fps, 8_000_000).expect("open openh264");
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0x80u8; (w * h * 4) as usize]),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("an AU");
        let colour = sps_colour(&sps_rbsp(&au.data)).expect(
            "the SPS must carry video_signal_type + colour_description — \
             see EncoderConfig::vui in `open`",
        );
        // (video_full_range_flag, colour_primaries, transfer, matrix) — 0 = limited, 1 = BT.709.
        assert_eq!(colour, (0, 1, 1, 1), "expected BT.709 limited signalling");
    }

    /// The modes the software encoder can actually serve — including the portrait orientation,
    /// which a naive per-axis `w <= 3840 && h <= 2160` would wrongly reject.
    #[test]
    fn openh264_accepts_up_to_4k_in_either_orientation() {
        assert!(openh264_supports_dimensions(1920, 1080));
        assert!(openh264_supports_dimensions(3840, 2160));
        assert!(openh264_supports_dimensions(2160, 3840));
        assert!(openh264_supports_dimensions(1080, 1920));
    }

    /// Modes `validate_dimensions` lets through (H.264 legitimately reaches 4096 on hardware) but
    /// openh264 rejects on the first encode. Catching them at open is the whole point of the gate:
    /// otherwise the session opens and then fails every single submit.
    #[test]
    fn openh264_rejects_modes_that_would_fail_on_first_submit() {
        // 4096-wide is legal H.264 and passes `Codec::max_dimension`, but exceeds the long edge.
        assert!(!openh264_supports_dimensions(4096, 2160));
        assert!(!openh264_supports_dimensions(2160, 4096));
        // Long edge is fine, short edge is not (e.g. an ultrawide-tall composite desktop).
        assert!(!openh264_supports_dimensions(3840, 2400));
        assert!(!openh264_supports_dimensions(2400, 3840));
    }

    /// A too-large mode must fail at `open`, not silently at every `submit`.
    #[test]
    fn open_refuses_a_mode_openh264_cannot_encode() {
        // Matched rather than `expect_err`: `OpenH264Encoder` is not `Debug` (it wraps a raw C
        // handle), so `expect_err` would not compile.
        let err = match OpenH264Encoder::open(PixelFormat::Bgra, 4096, 2160, 60, 20_000_000) {
            Ok(_) => panic!("4096x2160 exceeds openh264's long-edge ceiling and must be refused"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("openh264 cannot encode 4096x2160"), "{msg}");
    }
}
