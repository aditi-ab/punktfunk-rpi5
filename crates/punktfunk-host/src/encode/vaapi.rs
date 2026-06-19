//! VAAPI encoder via `ffmpeg-next` — AMD (Mesa `radeonsi`) and Intel (`iHD`/`i965`) over one
//! libavcodec backend (`h264_vaapi`/`hevc_vaapi`/`av1_vaapi`). The kernel driver differs per
//! vendor; the libva userspace API is identical, so a single encoder covers both. This is the
//! sibling of [`super::linux`] (NVENC/CUDA) behind the shared [`Encoder`] trait — selected in
//! [`super::open_video`] (NVIDIA → NVENC, AMD/Intel → here).
//!
//! Two input paths:
//! * **CPU (this file today).** The portal negotiates packed RGB/BGR; we swscale it to BT.709
//!   limited-range NV12, upload that into a pooled VA surface (`av_hwframe_transfer_data`), and
//!   encode in place. Robust on any VAAPI GPU with no capture-side changes — the capturer already
//!   falls back to CPU frames on a non-NVIDIA box (its EGL→CUDA importer needs `libcuda`).
//! * **Zero-copy dmabuf (deferred to Phase 2).** Import the capture dmabuf straight into a VA
//!   surface (`av_hwframe_map` of an `AV_PIX_FMT_DRM_PRIME` frame) — no EGL/Vulkan/CUDA detour,
//!   no host CSC. This is the inverse of the Linux client's VAAPI *decode* path.
//!
//! Raw FFI: `ffmpeg-next` has no hwcontext wrappers, so the hwdevice/hwframes/transfer calls go
//! through `ffmpeg::ffi` (= `ffmpeg_sys_next`), exactly as the CUDA encode path and the clients'
//! decode paths already do. The encoder is opened *without* a global header, so VPS/SPS/PPS are
//! in-band on every IDR.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary, Packet, Rational};
use ffmpeg_next as ffmpeg;
use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::ptr;

use ffmpeg::ffi; // = ffmpeg_sys_next

// libswscale scaler-flag + colour-space constants (not exported as Rust consts by the bindings;
// these are the stable `<libswscale/swscale.h>` #defines). No-rescale → POINT is cheapest.
const SWS_POINT: c_int = 0x10;
const SWS_CS_ITU709: c_int = 1;

/// `ffmpeg::format::Pixel` → raw `AVPixelFormat` (the documented ffmpeg-next conversion).
fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    ffi::AVPixelFormat::from(p)
}

/// The swscale *source* pixel format for a captured CPU layout. The portal fixates packed
/// 24/32-bit RGB/BGR; swscale converts any of these → NV12 directly (it even takes 3-bpp RGB24
/// with no host-side 3→4 expand, unlike NVENC). NV12/P010/HDR only arrive on Windows or the
/// deferred 10-bit path, so reject them here with a clear message.
fn vaapi_sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 => bail!(
            "VAAPI CPU-input path supports packed RGB/BGR only; got {format:?} \
             (NV12/P010/HDR arrive only on the Windows or deferred 10-bit paths)"
        ),
    })
}

/// VAAPI hardware contexts: a device created on a DRM render node and a frames pool the encoder
/// draws input surfaces from. Owns two `AVBufferRef`s, unref'd on drop (refcounted, so the copies
/// we hand the encoder outlive this).
struct VaapiHw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl VaapiHw {
    /// Create a VAAPI device (`node` = e.g. `/dev/dri/renderD128`, or `None` for libva's default
    /// — correct on a single-GPU box) and an `AV_PIX_FMT_VAAPI` frames pool with `sw_format`.
    unsafe fn new(
        node: Option<&CStr>,
        sw_format: ffi::AVPixelFormat,
        w: u32,
        h: u32,
        pool: c_int,
    ) -> Result<Self> {
        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        let node_ptr = node.map_or(ptr::null(), |c| c.as_ptr());
        let r = ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node_ptr,
            ptr::null_mut(),
            0,
        );
        if r < 0 {
            let where_ = node
                .and_then(|c| c.to_str().ok())
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            bail!("no VAAPI device{where_}: {}", ffmpeg::Error::from(r));
        }

        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_alloc(VAAPI) failed");
        }
        let fc = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*fc).sw_format = sw_format;
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = pool;
        let r = ffi::av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_init(VAAPI) failed ({r})");
        }
        Ok(VaapiHw {
            device_ref,
            frames_ref,
        })
    }
}

impl Drop for VaapiHw {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

pub struct VaapiEncoder {
    enc: encoder::video::Encoder,
    hw: VaapiHw,
    /// swscale context: packed RGB/BGR → NV12 (BT.709 limited). CPU-input path only.
    sws: *mut ffi::SwsContext,
    /// Reusable software NV12 staging frame (swscale dst → `av_hwframe_transfer_data` src).
    /// Overwriting it across frames is sound: the upload copies into a fresh pooled VA surface and
    /// the caller drains `poll()` after each `submit`, so nothing holds a reference to it.
    nv12: *mut ffi::AVFrame,
    src_format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic presentation index, in `1/fps` time-base units.
    frame_idx: i64,
    /// Force the next submitted frame to be an IDR (set by [`request_keyframe`]).
    force_kf: bool,
}

// Raw FFI pointers; the encoder lives on a single thread (same contract as `NvencEncoder`).
unsafe impl Send for VaapiEncoder {}

impl VaapiEncoder {
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
    ) -> Result<Self> {
        // 10-bit/HDR (P010 sw_format) is a follow-up — VAAPI supports it cleanly via Main10, but
        // it needs the capture/negotiation 10-bit plumbing that the Linux host doesn't have yet.
        if bit_depth != 8 {
            tracing::warn!(bit_depth, "VAAPI 10-bit not yet wired — encoding 8-bit");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            unsafe { ffi::av_log_set_level(48) }; // AV_LOG_DEBUG — surface VAAPI open/upload rejects
        }
        let name = codec.vaapi_name();
        let av_codec = encoder::find_by_name(name).ok_or_else(|| {
            anyhow!("{name} not built into libavcodec (no VAAPI encoder for {codec:?})")
        })?;
        let src_pixel = vaapi_sws_src(format)?;

        // VAAPI device + NV12 frames pool. `PUNKTFUNK_RENDER_NODE` pins the GPU on a multi-GPU box;
        // unset = libva's default render node (right on a single-GPU host).
        let node = std::env::var("PUNKTFUNK_RENDER_NODE").ok();
        let node_c = node
            .as_deref()
            .map(CString::new)
            .transpose()
            .context("PUNKTFUNK_RENDER_NODE contained a NUL")?;
        const POOL: c_int = 16;
        let hw = unsafe {
            VaapiHw::new(
                node_c.as_deref(),
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                width,
                height,
                POOL,
            )?
        };

        let mut video = codec::context::Context::new_with_codec(av_codec)
            .encoder()
            .video()
            .context("alloc video encoder")?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(Pixel::NV12); // sw_format; pix_fmt is overridden to VAAPI below
        video.set_time_base(Rational(1, fps as i32));
        video.set_frame_rate(Some(Rational(fps as i32, 1)));
        video.set_bit_rate(bitrate_bps as usize);
        // max == target so vaapi_encode selects CBR when the driver's RC entrypoint supports it
        // (modern AMD/Intel), and gracefully degrades to VBR otherwise — without failing to open.
        video.set_max_bit_rate(bitrate_bps as usize);
        // VBV/HRD ~1 frame of bits — same rationale as NVENC: keep per-frame size roughly constant
        // so a high-motion P-frame can't balloon past the bounded send queue. PUNKTFUNK_VBV_FRAMES
        // tunes it (shared knob with NVENC).
        let vbv_frames = std::env::var("PUNKTFUNK_VBV_FRAMES")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(1.0);
        let vbv_bits = ((bitrate_bps as f64 / fps.max(1) as f64) * vbv_frames as f64)
            .clamp(1.0, i32::MAX as f64);
        video.set_max_b_frames(0);
        unsafe {
            let raw = video.as_mut_ptr();
            (*raw).rc_buffer_size = vbv_bits as i32;
            // Infinite GOP — no periodic IDR (the "freeze" fix). VAAPI has no NVENC `gop_size=-1`,
            // so use a huge GOP and drive keyframes on demand via forced IDR (pict_type=I), the
            // same Moonlight/Sunshine low-latency model.
            (*raw).gop_size = i32::MAX;
            // We CSC RGB→NV12 as BT.709 *limited* range in swscale (below), so signal that VUI —
            // otherwise the client decoder assumes a default and the picture is washed-out / wrong
            // contrast. Matches the NVENC NV12 path's signalling.
            (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
            (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG; // limited/studio
            (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
            (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            // Take VAAPI hw surfaces: derive the device from the frames pool, set both before open.
            (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*raw).hw_device_ctx = ffi::av_buffer_ref(hw.device_ref);
            (*raw).hw_frames_ctx = ffi::av_buffer_ref(hw.frames_ref);
        }

        let mut opts = Dictionary::new();
        opts.set("async_depth", "1"); // one-in/one-out — minimal encode-pipeline latency

        let enc = video
            .open_with(opts)
            .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))?;

        // swscale: packed RGB/BGR → NV12, no rescale (POINT). Force BT.709 limited so the bytes
        // match the VUI we signalled.
        let src_av = pixel_to_av(src_pixel);
        let sws = unsafe {
            ffi::sws_getContext(
                width as c_int,
                height as c_int,
                src_av,
                width as c_int,
                height as c_int,
                ffi::AVPixelFormat::AV_PIX_FMT_NV12,
                SWS_POINT,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if sws.is_null() {
            bail!("sws_getContext(RGB→NV12) failed");
        }
        unsafe {
            // src RGB = full range (1), dst YUV = limited/studio (0); BT.709 coefficients both sides.
            let cs709 = ffi::sws_getCoefficients(SWS_CS_ITU709);
            ffi::sws_setColorspaceDetails(sws, cs709, 1, cs709, 0, 0, 1 << 16, 1 << 16);
        }

        // Reusable software NV12 staging frame.
        let nv12 = unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                ffi::sws_freeContext(sws);
                bail!("av_frame_alloc(NV12) failed");
            }
            (*f).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*f).width = width as c_int;
            (*f).height = height as c_int;
            let r = ffi::av_frame_get_buffer(f, 0);
            if r < 0 {
                let mut f = f;
                ffi::av_frame_free(&mut f);
                ffi::sws_freeContext(sws);
                bail!("av_frame_get_buffer(NV12) failed ({r})");
            }
            f
        };

        tracing::info!(
            encoder = name,
            render_node = node.as_deref().unwrap_or("default"),
            "VAAPI encode active ({width}x{height}@{fps}, CPU→NV12 upload path)"
        );
        Ok(VaapiEncoder {
            enc,
            hw,
            sws,
            nv12,
            src_format: format,
            width,
            height,
            fps,
            frame_idx: 0,
            force_kf: false,
        })
    }

    /// CPU path: swscale the packed RGB/BGR bytes into the reusable NV12 frame, upload that into a
    /// pooled VA surface, and encode in place.
    fn submit_cpu(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            format,
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_row = w * self.src_format.bytes_per_pixel();
        anyhow::ensure!(
            bytes.len() >= src_row * h,
            "captured buffer {} bytes < required {}",
            bytes.len(),
            src_row * h
        );
        unsafe {
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            let r = ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.nv12).data.as_ptr(),
                (*self.nv12).linesize.as_ptr(),
            );
            if r < 0 {
                bail!("sws_scale RGB→NV12 failed ({r})");
            }

            // Pooled VA surface ← NV12 upload, then encode in place. Free the frame after send;
            // avcodec_send_frame takes its own ref to the surface.
            let mut hwf = ffi::av_frame_alloc();
            if hwf.is_null() {
                bail!("av_frame_alloc(hw) failed");
            }
            let r = ffi::av_hwframe_get_buffer(self.hw.frames_ref, hwf, 0);
            if r < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_get_buffer(VAAPI) failed ({r})");
            }
            let r = ffi::av_hwframe_transfer_data(hwf, self.nv12, 0);
            if r < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_transfer_data(→VAAPI) failed ({r})");
            }
            (*hwf).pts = pts;
            (*hwf).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), hwf);
            ffi::av_frame_free(&mut hwf);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        Ok(())
    }
}

impl Encoder for VaapiEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let pts = self.frame_idx;
        self.frame_idx += 1;
        let idr = self.force_kf;
        self.force_kf = false;
        match &captured.payload {
            FramePayload::Cpu(bytes) => self.submit_cpu(bytes, captured.format, pts, idr),
            // CUDA frames are produced only by the NVIDIA zero-copy importer, which never runs on a
            // VAAPI host. Reaching here means a misconfiguration (e.g. forced PUNKTFUNK_ENCODER=vaapi
            // on an NVIDIA box with zero-copy on).
            FramePayload::Cuda(_) => bail!(
                "VAAPI encoder received a CUDA frame — that payload is NVENC-only; \
                 unset PUNKTFUNK_ZEROCOPY or don't force PUNKTFUNK_ENCODER=vaapi on an NVIDIA host"
            ),
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
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
            Err(ffmpeg::Error::Other { errno })
                if errno == ffmpeg::util::error::EAGAIN
                    || errno == ffmpeg::util::error::EWOULDBLOCK =>
            {
                Ok(None)
            }
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(e) => Err(e).context("receive_packet"),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.enc.send_eof().context("send_eof")?;
        Ok(())
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.nv12.is_null() {
                ffi::av_frame_free(&mut self.nv12);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
        }
        // `enc` (frees the codec ctx, unref'ing its hw-context copies) and `hw` (unref'ing the
        // originals) drop via their own impls — refcounting makes the order irrelevant.
    }
}
