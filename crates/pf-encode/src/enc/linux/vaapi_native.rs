//! The VAAPI encoder behind [`Encoder`]: `pf_libva`'s session, and the only one.
//!
//! A loss is answered by predicting from a slot the client still has
//! (`invalidate_ref_frames`), or by an intra refresh wave when none survives
//! (`design/vulkan-intra-refresh.md` §10), an ABR step retargets in place
//! (`reconfigure_bitrate`), and HEVC Main 10 carries the HDR10 SEI.
//! Synchronous: `submit` encodes and `poll` hands the AU straight back.
//!
//! H.264 and HEVC on AMD and Intel. AV1 there is Vulkan Video's.

use std::os::fd::AsRawFd as _;

use anyhow::{anyhow, bail, ensure, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use pf_libva::encode::{CodecParams, Encoder as Session, Stripe};
use pf_libva::{Display, DmabufSource, Libva};
use pf_vaapi::drm::ExportedPlane;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::hevc::{HdrStatic, COLOUR_BT2020_PQ, COLOUR_BT709};
use pf_vaapi::vpp;

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use pf_encode_win::rfi::{self, plan_slot_recovery, Wave, WaveMark};

/// Slots a session keeps: how far back a recovery anchor may reach. A report
/// names frames the client missed two frames ago and spends a round trip
/// arriving, so the ring must still hold the picture before the loss — eight is
/// 80 ms at 100 fps, past a Wi-Fi report. The level's DPB may allow fewer.
const SLOTS: u8 = 8;

pub struct NativeVaapiEncoder {
    /// `None` between a [`Encoder::reset`] and the submit that reopens.
    session: Option<Session>,
    params: SessionParams,
    codec: CodecParams,
    hdr: Option<HdrStatic>,
    force_kf: bool,
    /// A loss plan's anchor, consumed by the next submit.
    anchor: Option<usize>,
    /// Intra refresh wave in flight: the rung a loss with no anchor takes instead of the
    /// IDR. An anchor or a forced IDR abandons it; a loss reported while it runs spoils it
    /// and queues a fresh one behind it, never a restart: iHD tracks each reference's
    /// refreshed rows itself and a stripe that jumps back to the top never heals there.
    wave: Option<Wave>,
    wave_spoiled: bool,
    wave_queued: bool,
    /// The host's wire index minus the session's own picture count.
    wire_offset: i64,
    frames: u64,
    pending: Option<EncodedFrame>,
    /// 24-bit CPU frames are repacked to 32 here.
    repack: Vec<u8>,
    /// The part of each picture this session encodes ([`Encoder::set_input_crop`]).
    crop: Option<[u32; 4]>,
    /// `PUNKTFUNK_VAAPI_DUMP=<file>`: every access unit, appended, for a decoder to look at.
    dump: Option<std::fs::File>,
}

impl NativeVaapiEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
        // BT.2020 PQ vs BT.709. Independent of depth: 10-bit SDR is Main10 under BT.709.
        hdr: bool,
    ) -> Result<Self> {
        ensure!(!chroma.is_444(), "the native VAAPI encoder is 4:2:0 only");
        let ten_bit = bit_depth == 10;
        let codec = match codec {
            Codec::H264 => {
                ensure!(
                    !ten_bit,
                    "ten-bit H.264 is not a path here; HEVC Main 10 is"
                );
                CodecParams::H264
            }
            Codec::H265 => CodecParams::Hevc {
                ten_bit,
                colour: if hdr { COLOUR_BT2020_PQ } else { COLOUR_BT709 },
            },
            Codec::Av1 | Codec::PyroWave => {
                bail!("the native VAAPI encoder does not encode {codec:?}")
            }
        };
        let mut params = SessionParams {
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            bitrate_bps: bitrate_bps.min(u64::from(u32::MAX)) as u32,
            slots: SLOTS,
            max_num_reorder_frames: 0,
            initial_qp: 26,
            vbv_frames: super::vbv_frames_env() as f32,
        };
        params.slots = SLOTS.min(match codec {
            CodecParams::H264 => params.h264_max_slots(),
            CodecParams::Hevc { .. } => params.hevc_max_slots(),
        });
        let mut this = Self {
            session: None,
            params,
            codec,
            hdr: None,
            force_kf: true,
            anchor: None,
            wave: None,
            wave_spoiled: false,
            wave_queued: false,
            wire_offset: 0,
            frames: 0,
            pending: None,
            repack: Vec::new(),
            crop: None,
            dump: std::env::var("PUNKTFUNK_VAAPI_DUMP")
                .ok()
                .and_then(|p| std::fs::File::create(&p).ok()),
        };
        this.open_session()?;
        tracing::info!(
            ?codec,
            width,
            height,
            fps,
            bitrate_bps,
            slots = params.slots,
            "native VAAPI encode session open"
        );
        Ok(this)
    }

    /// Open on the GPU the host chose, never the first node that initialises.
    fn open_session(&mut self) -> Result<()> {
        let va = Libva::load().context("libva")?;
        let node = pf_gpu::linux_render_node();
        let display = Display::open_path(va, &node.to_string_lossy())
            .with_context(|| format!("VAAPI display on {}", node.display()))?;
        let mut session =
            Session::new(display, self.params, self.codec).map_err(|e| anyhow!("{e:#}"))?;
        session.set_hdr(self.hdr);
        session.set_source_crop(self.crop);
        self.session = Some(session);
        self.force_kf = true;
        Ok(())
    }
}

/// Whether the host's render node offers an encode entrypoint for `codec`
/// at this depth — what a native open needs. AV1 is not a native path.
pub fn probe_can_encode(codec: Codec, ten_bit: bool) -> bool {
    use pf_vaapi::config::{VA_PROFILE_H264_HIGH, VA_PROFILE_HEVC_MAIN, VA_PROFILE_HEVC_MAIN10};
    use pf_vaapi::enc_h264::{VA_ENTRYPOINT_ENC_SLICE, VA_ENTRYPOINT_ENC_SLICE_LP};
    let profile = match (codec, ten_bit) {
        (Codec::H264, false) => VA_PROFILE_H264_HIGH,
        (Codec::H265, false) => VA_PROFILE_HEVC_MAIN,
        (Codec::H265, true) => VA_PROFILE_HEVC_MAIN10,
        _ => return false,
    };
    let node = pf_gpu::linux_render_node();
    let display = match Libva::load().and_then(|va| Display::open_path(va, &node.to_string_lossy()))
    {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(error = %format!("{e:#}"), "no VAAPI display to probe");
            return false;
        }
    };
    display
        .entrypoints(profile)
        .map(|e| {
            e.iter()
                .any(|&p| p == VA_ENTRYPOINT_ENC_SLICE || p == VA_ENTRYPOINT_ENC_SLICE_LP)
        })
        .unwrap_or(false)
}

impl Encoder for NativeVaapiEncoder {
    /// A mirrored head or a crop arrives larger and is scaled on ingest; the shape must
    /// match within the even-floor's two pixels. Smaller, another shape, or a crop past
    /// the picture is a host size fault: fail here, not with a garbage picture.
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        let [cx, cy, cw, ch] = self
            .crop
            .unwrap_or([0, 0, frame.width, frame.height])
            .map(u64::from);
        let (fw, fh) = (cw, ch);
        let (ew, eh) = (u64::from(self.params.width), u64::from(self.params.height));
        ensure!(
            cx + cw <= u64::from(frame.width)
                && cy + ch <= u64::from(frame.height)
                && fw >= ew
                && fh >= eh
                && (fw * eh).abs_diff(fh * ew) < 2 * fw.max(fh),
            "captured frame {}x{} (encoding {cw}x{ch} at {cx},{cy}) does not fit encoder {}x{}",
            frame.width,
            frame.height,
            self.params.width,
            self.params.height
        );
        if self.session.is_none() {
            self.open_session()?;
        }
        let session = self.session.as_mut().expect("opened above");
        match &frame.payload {
            FramePayload::Cpu(bytes) => {
                let (fourcc, bytes) = packed_rgb(frame.format, bytes, &mut self.repack)?;
                session.submit_packed(
                    bytes,
                    fourcc,
                    frame.width,
                    frame.height,
                    frame.width as usize * 4,
                )?;
            }
            FramePayload::Dmabuf(d) => {
                let fd = d.fd.as_raw_fd();
                let mut planes = vec![ExportedPlane {
                    fd,
                    offset: d.offset,
                    stride: d.stride,
                }];
                // NV12/P010 chroma: named by the producer, or contiguous below the luma
                // rows. Both are two-plane; a single-plane import is refused by the driver.
                if let Some((offset, stride)) = d.plane1 {
                    planes.push(ExportedPlane { fd, offset, stride });
                } else if d.fourcc == vpp::DRM_FORMAT_NV12 || d.fourcc == vpp::DRM_FORMAT_P010 {
                    planes.push(ExportedPlane {
                        fd,
                        offset: d.offset + d.stride * frame.height,
                        stride: d.stride,
                    });
                }
                session.submit_dmabuf(&DmabufSource {
                    width: frame.width,
                    height: frame.height,
                    drm_fourcc: d.fourcc,
                    modifier: d.modifier,
                    planes: &planes,
                })?;
            }
            FramePayload::Cuda(_) => bail!(
                "a CUDA frame reached the VAAPI encoder — that payload is NVENC-only; unset \
                 PUNKTFUNK_ZEROCOPY or do not pin PUNKTFUNK_ENCODER=vaapi-native on an NVIDIA host"
            ),
        }
        if self.force_kf || self.anchor.is_some() {
            self.wave = None;
            self.wave_spoiled = false;
            self.wave_queued = false;
        }
        let wave = self.wave;
        let mark = wave.map_or(WaveMark::None, |w| w.mark(self.wave_spoiled));
        let pic = match (self.anchor.take(), wave) {
            (Some(slot), _) => session.encode_anchored(slot)?,
            (None, Some(w)) => {
                let (first_row, rows) = w.stripe(session.wave_rows());
                let stripe = Stripe {
                    first_row: first_row as u16,
                    rows: rows as u16,
                };
                // A spoiled close still leans on the lost frame: never an anchor.
                session.encode_wave(stripe, !w.closes() || self.wave_spoiled)?
            }
            (None, None) => session.encode(self.force_kf)?,
        };
        self.force_kf = false;
        if let Some(w) = wave {
            self.wave = w.next();
            if self.wave.is_none() {
                self.wave_spoiled = false;
                if std::mem::take(&mut self.wave_queued) {
                    self.wave = Some(Wave::start(w.cycle));
                }
            }
        }
        if let Some(f) = &mut self.dump {
            use std::io::Write as _;
            let _ = f.write_all(&pic.bytes);
        }
        let pts_ns = self.frames * 1_000_000_000 / u64::from(self.params.fps_num.max(1));
        self.frames += 1;
        self.pending = Some(EncodedFrame {
            data: pic.bytes,
            pts_ns,
            keyframe: pic.is_idr,
            recovery_anchor: pic.recovery_anchor,
            recovery_point: mark.point() && !pic.is_idr,
            recovery_close: mark.close() && !pic.is_idr,
            chunk_aligned: false,
        });
        Ok(())
    }

    /// The session numbers pictures from zero; the host's index is that plus an
    /// offset, re-learnt on every submit so a rebuild cannot desync the two.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        if let Some(s) = &self.session {
            self.wire_offset = i64::from(wire_index) - s.next_wire();
        }
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            downscales_input: true,
            crops_input: true,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.hdr = meta.map(|m| HdrStatic {
            display_primaries: m.display_primaries,
            white_point: m.white_point,
            max_display_mastering_luminance: m.max_display_mastering_luminance,
            min_display_mastering_luminance: m.min_display_mastering_luminance,
            max_cll: m.max_cll,
            max_fall: m.max_fall,
        });
        if let Some(s) = &mut self.session {
            s.set_hdr(self.hdr);
        }
    }

    /// The slot plan on the session's trusted slots, in the host's wire domain:
    /// taint what the loss touched, anchor the next picture on the newest older
    /// one. `false` when nothing older survives — the caller keyframes.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if first < 0 || last < first {
            return false;
        }
        let Some(session) = &mut self.session else {
            return false;
        };
        let refs: Vec<(usize, i64)> = session
            .slots()
            .into_iter()
            .map(|(slot, wire)| (slot, wire + self.wire_offset))
            .collect();
        let plan = plan_slot_recovery(&refs, first);
        session.distrust(plan.tainted);
        self.anchor = plan.anchor.map(|(slot, _)| slot);
        if self.anchor.is_some() {
            return true;
        }
        if rfi::wave_enabled() && session.next_wire() > 0 {
            if self.wave.is_some() {
                // The client re-armed at this loss: this wave runs out unmarked and a
                // fresh one, whose start and close it counts, starts behind it.
                self.wave_spoiled = true;
                self.wave_queued = true;
                tracing::debug!(first, last, "vaapi-native RFI: loss mid-wave — wave queued");
                return true;
            }
            // No anchor, but a wave heals without one.
            let cycle = rfi::wave_cycle(
                session.wave_rows(),
                (self.params.fps_num / self.params.fps_den.max(1)).max(1),
                u32::MAX,
                rfi::pinned_cycle(),
            );
            self.wave = Some(Wave::start(cycle));
            tracing::debug!(
                first,
                last,
                cycle,
                "vaapi-native RFI: no reference older than the loss — starting an intra \
                 refresh wave instead of an IDR"
            );
            return true;
        }
        tracing::debug!(
            first,
            last,
            slots = refs.len(),
            "vaapi-native RFI declined: the ring holds no reference older than the loss — \
             caller falls back to its (coalesced) keyframe path"
        );
        false
    }

    fn distrust_references(&mut self) {
        if let Some(s) = &mut self.session {
            s.distrust_all();
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.take())
    }

    /// Drop the session; the next submit reopens it with an IDR.
    fn reset(&mut self) -> bool {
        self.session = None;
        self.pending = None;
        self.anchor = None;
        self.wave = None;
        self.wave_spoiled = false;
        self.wave_queued = false;
        self.force_kf = true;
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps = bps.min(u64::from(u32::MAX)) as u32;
        self.params.bitrate_bps = bps;
        if let Some(s) = &mut self.session {
            s.set_bitrate(bps);
        }
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        Some(u64::from(self.params.bitrate_bps))
    }

    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        self.crop = Some(rect);
        if let Some(session) = self.session.as_mut() {
            session.set_source_crop(self.crop);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The packed-RGB fourcc a CPU frame uploads as, repacking 24-bit to 32 on the
/// way. `Bgrx` uploads as `BGRA`: same bytes, and iHD allocates no `BGRX`.
fn packed_rgb<'a>(
    format: PixelFormat,
    bytes: &'a [u8],
    repack: &'a mut Vec<u8>,
) -> Result<(u32, &'a [u8])> {
    Ok(match format {
        PixelFormat::Bgrx | PixelFormat::Bgra => (vpp::VA_FOURCC_BGRA, bytes),
        PixelFormat::Rgbx | PixelFormat::Rgba => (vpp::VA_FOURCC_RGBA, bytes),
        PixelFormat::X2Rgb10 => (vpp::VA_FOURCC_X2R10G10B10, bytes),
        PixelFormat::X2Bgr10 => (vpp::VA_FOURCC_X2B10G10R10, bytes),
        PixelFormat::Bgr | PixelFormat::Rgb => {
            repack.clear();
            repack.reserve(bytes.len() / 3 * 4);
            for px in bytes.chunks_exact(3) {
                repack.extend_from_slice(px);
                repack.push(255);
            }
            let fourcc = if format == PixelFormat::Bgr {
                vpp::VA_FOURCC_BGRA
            } else {
                vpp::VA_FOURCC_RGBA
            };
            (fourcc, repack.as_slice())
        }
        other => bail!("no native VAAPI ingest for CPU {other:?} frames"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 24-bit CPU frames become 32-bit with an opaque alpha; 32-bit ones upload
    /// as they are.
    #[test]
    fn packed_rgb_repacks_24_bit_only() {
        let mut scratch = Vec::new();
        let (fourcc, out) =
            packed_rgb(PixelFormat::Bgr, &[1, 2, 3, 4, 5, 6], &mut scratch).unwrap();
        assert_eq!(fourcc, vpp::VA_FOURCC_BGRA);
        assert_eq!(out, &[1, 2, 3, 255, 4, 5, 6, 255]);
        let bytes = [9u8; 8];
        let (fourcc, out) = packed_rgb(PixelFormat::Bgrx, &bytes, &mut scratch).unwrap();
        assert_eq!(fourcc, vpp::VA_FOURCC_BGRA);
        assert_eq!(out.as_ptr(), bytes.as_ptr(), "no copy");
        assert!(packed_rgb(PixelFormat::Nv12, &bytes, &mut scratch).is_err());
    }

    /// The trait contract on real silicon: an IDR first, P after, a loss answered
    /// by a recovery anchor and not an IDR, and a bitrate step accepted in place.
    ///
    /// `cargo test -p pf-encode native_vaapi_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_smoke() {
        let (w, h) = (320u32, 240u32);
        let mut enc = NativeVaapiEncoder::open(
            Codec::H264,
            w,
            h,
            60,
            4_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
        )
        .expect("open");
        assert!(enc.caps().supports_rfi);
        let frame = |i: u32| {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&[(i * 8) as u8, 0x40, 0xC0, 0xFF]);
            }
            CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: u64::from(i) * 16_666_666,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(buf),
                cursor: None,
            }
        };
        let mut aus = Vec::new();
        for i in 0..10 {
            enc.submit_indexed(&frame(i), 100 + i).expect("submit");
            aus.push(enc.poll().expect("poll").expect("an AU per submit"));
        }
        assert!(aus[0].keyframe);
        assert!(aus[1..]
            .iter()
            .all(|au| !au.keyframe && !au.recovery_anchor));
        // Wire 108 and 109 were lost; the anchor must be 107.
        assert!(
            enc.invalidate_ref_frames(108, 109),
            "a slot older than the loss survives"
        );
        assert!(enc.reconfigure_bitrate(2_000_000));
        assert_eq!(enc.applied_bitrate_bps(), Some(2_000_000));
        enc.submit_indexed(&frame(10), 110).expect("submit");
        let recovery = enc.poll().expect("poll").expect("an AU");
        assert!(recovery.recovery_anchor && !recovery.keyframe);
        // Everything is tainted: no anchor, so a wave answers instead of the IDR.
        assert!(enc.invalidate_ref_frames(100, 110));
        assert!(enc.wave.is_some(), "the wave starts where the IDR used to");
        assert!(enc.reset());
        enc.submit(&frame(11)).expect("submit after reset");
        assert!(
            enc.poll().unwrap().unwrap().keyframe,
            "a rebuild starts with an IDR"
        );
    }
    /// BGRX frame of horizontal bands scrolled down by `shift` rows, with a diagonal so no
    /// two rows are alike: the encoder must reach for rows above to predict it.
    fn scroll_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        let buf = pf_encode_win::smoke_pattern::scroll_pattern(w as usize, h as usize, i as usize);
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: u64::from(i) * 16_666_666,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// The wave replaces the IDR: an RFI with every reference tainted starts one; its start
    /// and close AU carry the mark, nothing in between does, no IDR follows frame 0, and a
    /// later loss of the plain P after the wave re-anchors on the close (a fully swept
    /// picture is trusted). `PF_WAVE_DUMP=<dir>` writes the full stream and the client's
    /// view with the pre-wave P frames lost: decoded side by side, the close must match
    /// the full decode.
    ///
    /// `cargo test -p pf-encode native_vaapi_wave -- --ignored --nocapture`
    fn run_wave_smoke(codec: Codec, ext: &str) {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            NativeVaapiEncoder::open(codec, w, h, 60, 4_000_000, 8, ChromaFormat::Yuv420, false)
                .expect("open");
        const WAVE_START: usize = 3;
        // `PF_WAVE_SPOIL=1`: two frames into the wave a frame inside its sweep is lost, so
        // it closes unmarked and the wave queued behind it carries the start and close.
        let restart = std::env::var("PF_WAVE_SPOIL").is_ok_and(|v| v == "1");
        let mut start2 = WAVE_START;
        let mut aus = Vec::new();
        let mut cycle = 0usize;
        let mut i = 0usize;
        loop {
            if i == WAVE_START {
                assert!(
                    enc.invalidate_ref_frames(0, WAVE_START as i64 - 1),
                    "a wave-capable encoder answers an RFI with no anchor"
                );
                let w = enc.wave.expect("the wave is armed");
                assert_eq!(w.index, 0);
                cycle = w.cycle as usize;
                if restart {
                    start2 = WAVE_START + cycle;
                }
                eprintln!(
                    "run_wave_smoke: {} rows, cycle {cycle}, low_power={}",
                    enc.session.as_ref().unwrap().wave_rows(),
                    enc.session.as_ref().unwrap().low_power()
                );
            }
            if restart && i == WAVE_START + 2 {
                let lost = (i - 1) as i64;
                assert!(
                    enc.invalidate_ref_frames(lost, lost),
                    "a loss inside the sweep"
                );
                assert!(enc.wave_spoiled && enc.wave_queued, "spoiled, one queued");
                assert_eq!(enc.wave.map(|w| w.index), Some(2), "the sweep runs on");
            }
            let after_wave = start2 + cycle; // the plain P after the close
            let anchor_p = after_wave + 1;
            if cycle > 0 && i == anchor_p {
                assert!(
                    enc.invalidate_ref_frames(after_wave as i64, after_wave as i64),
                    "the wave close is a trusted anchor"
                );
            }
            enc.submit_indexed(&scroll_frame(w, h, i as u32), i as u32)
                .expect("submit");
            aus.push(enc.poll().expect("poll").expect("an AU per submit"));
            if cycle > 0 && i == anchor_p {
                break;
            }
            i += 1;
        }
        let close = start2 + cycle - 1;
        let after_wave = close + 1;
        let anchor_p = after_wave + 1;
        assert!(enc.wave.is_none(), "the wave closed");
        assert!(aus[0].keyframe, "frame 0 is the IDR");
        for (i, au) in aus.iter().enumerate().skip(1) {
            assert!(!au.data.is_empty(), "AU {i} empty");
            assert!(
                !au.keyframe,
                "AU {i}: no IDR after frame 0 — the wave replaced it"
            );
            let start = i == WAVE_START || i == start2;
            assert_eq!(
                au.recovery_point,
                start || i == close,
                "AU {i}: recovery_point marks every start and the close"
            );
            assert_eq!(au.recovery_close, i == close, "AU {i}: the close bit");
            assert_eq!(
                au.recovery_anchor,
                i == anchor_p,
                "AU {i}: the only anchor P answers the post-wave loss"
            );
        }
        if let Ok(dir) = std::env::var("PF_WAVE_DUMP") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p = format!("{dir}/vaenc-wave-smoke.{ext}");
            std::fs::write(&p, &full).unwrap_or_else(|e| panic!("write {p}: {e}"));
            // The client's view: the pre-wave P frames lost, and the restart's frame too.
            let dropped: Vec<u8> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    *i == 0 || (*i >= WAVE_START && !(restart && *i == WAVE_START + 1))
                })
                .flat_map(|(_, a)| a.data.iter().copied())
                .collect();
            let p2 = format!("{dir}/vaenc-wave-smoke-dropped.{ext}");
            std::fs::write(&p2, &dropped).unwrap_or_else(|e| panic!("write {p2}: {e}"));
            eprintln!(
                "run_wave_smoke: wrote {p} ({} bytes, {} AUs) and {p2} (frames 1..{} dropped; \
                 the close at {close} must decode identical to the full stream)",
                full.len(),
                aus.len(),
                WAVE_START,
            );
        }
    }

    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_wave_h264() {
        run_wave_smoke(Codec::H264, "h264");
    }

    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_wave_hevc() {
        run_wave_smoke(Codec::H265, "h265");
    }

    /// The probe agrees with an open: H.264 and both HEVC depths yes, AV1 and
    /// ten-bit H.264 no.
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_probe_matches_open() {
        assert!(probe_can_encode(Codec::H264, false));
        assert!(probe_can_encode(Codec::H265, false));
        assert!(probe_can_encode(Codec::H265, true));
        assert!(!probe_can_encode(Codec::Av1, false));
        assert!(!probe_can_encode(Codec::H264, true));
    }
}
