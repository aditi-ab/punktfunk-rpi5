//! VAAPI encoder via `ffmpeg-next` — AMD (Mesa `radeonsi`) and Intel (`iHD`/`i965`) over one
//! libavcodec backend (`h264_vaapi`/`hevc_vaapi`/`av1_vaapi`). The kernel driver differs per
//! vendor; the libva userspace API is identical, so a single encoder covers both. This is the
//! sibling of [`super::linux`] (NVENC/CUDA) behind the shared [`Encoder`] trait — selected in
//! [`super::open_video`] (NVIDIA → NVENC, AMD/Intel → here).
//!
//! Two input paths, chosen lazily from the FIRST frame's payload (so `open_video`'s signature
//! is unchanged and the encoder self-configures for whatever the capturer produces):
//! * **CPU upload** ([`CpuInner`]): the portal hands packed RGB/BGR CPU frames; we swscale to
//!   BT.709-limited NV12 and `av_hwframe_transfer_data` it into a pooled VA surface. Works on any
//!   VAAPI GPU with no capture changes (the capturer falls back to CPU frames on non-NVIDIA).
//! * **Zero-copy dmabuf** ([`DmabufInner`], `PUNKTFUNK_ZEROCOPY=1`): the capturer hands a packed-RGB
//!   dmabuf. We wrap it as an `AV_PIX_FMT_DRM_PRIME` frame and push it through a tiny filter graph
//!   `buffer(drm_prime) → hwmap=derive_device=vaapi → scale_vaapi=format=nv12 → buffersink`, so
//!   the import AND the RGB→NV12 colour conversion run on the GPU's video engine — no host CSC, no
//!   upload. The encoder takes the NV12 surfaces straight from the filter sink.
//!
//! Raw FFI: `ffmpeg-next` has no hwcontext/filter wrappers for what we need, so the
//! hwdevice/hwframes/buffersrc/buffersink calls go through `ffmpeg::ffi` (= `ffmpeg_sys_next`),
//! as the CUDA encode path and the clients' decode paths already do. The encoder is opened
//! *without* a global header, so VPS/SPS/PPS are in-band on every IDR.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::{codec, encoder, Dictionary, Packet, Rational};
use ffmpeg_next as ffmpeg;
use std::ffi::{CStr, CString};
use std::os::fd::AsRawFd;
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

/// `fourcc(a,b,c,d)` — DRM FourCC packing (`a | b<<8 | c<<16 | d<<24`).
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// The render node a VAAPI/DRM device should open. `PUNKTFUNK_RENDER_NODE` pins it on a multi-GPU
/// box; the default is correct on a single-GPU host.
fn render_node() -> CString {
    let p = std::env::var("PUNKTFUNK_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    CString::new(p).unwrap_or_else(|_| CString::new("/dev/dri/renderD128").unwrap())
}

/// The swscale *source* pixel format for a captured CPU layout (packed RGB/BGR only).
fn vaapi_sws_src(format: PixelFormat) -> Result<Pixel> {
    Ok(match format {
        PixelFormat::Bgrx => Pixel::BGRZ, // bgr0
        PixelFormat::Rgbx => Pixel::RGBZ, // rgb0
        PixelFormat::Bgra => Pixel::BGRA,
        PixelFormat::Rgba => Pixel::RGBA,
        PixelFormat::Rgb => Pixel::RGB24,
        PixelFormat::Bgr => Pixel::BGR24,
        PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Rgb10a2 => {
            bail!("VAAPI CPU-input path supports packed RGB/BGR only; got {format:?}")
        }
    })
}

/// Build the FFmpeg encoder context (shared by both inner paths): name, mode, low-latency RC,
/// infinite GOP, BT.709-limited VUI, `pix_fmt=VAAPI`, and the given hw device + frames contexts.
/// Returns the opened encoder. `device_ref`/`frames_ref` are borrowed (ref'd into the context).
unsafe fn open_vaapi_encoder(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
) -> Result<encoder::video::Encoder> {
    let name = codec.vaapi_name();
    let av_codec = encoder::find_by_name(name).ok_or_else(|| {
        anyhow!("{name} not built into libavcodec (no VAAPI encoder for {codec:?})")
    })?;
    let mut video = codec::context::Context::new_with_codec(av_codec)
        .encoder()
        .video()
        .context("alloc video encoder")?;
    video.set_width(width);
    video.set_height(height);
    video.set_format(Pixel::NV12); // sw view; pix_fmt overridden to VAAPI below
    video.set_time_base(Rational(1, fps as i32));
    video.set_frame_rate(Some(Rational(fps as i32, 1)));
    video.set_bit_rate(bitrate_bps as usize);
    video.set_max_bit_rate(bitrate_bps as usize); // == target → vaapi_encode picks CBR when supported
    let vbv_frames = std::env::var("PUNKTFUNK_VBV_FRAMES")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(1.0);
    let vbv_bits =
        ((bitrate_bps as f64 / fps.max(1) as f64) * vbv_frames as f64).clamp(1.0, i32::MAX as f64);
    video.set_max_b_frames(0);
    let raw = video.as_mut_ptr();
    (*raw).rc_buffer_size = vbv_bits as i32;
    (*raw).gop_size = i32::MAX; // no periodic IDR (forced-IDR via pict_type=I on RFI)
                                // We hand the encoder BT.709 *limited* NV12 (swscale CSC, or scale_vaapi which preserves the
                                // input range we tag), so signal that VUI — else the client decoder washes the picture out.
    (*raw).colorspace = ffi::AVColorSpace::AVCOL_SPC_BT709;
    (*raw).color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
    (*raw).color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT709;
    (*raw).color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
    (*raw).hw_device_ctx = ffi::av_buffer_ref(device_ref);
    (*raw).hw_frames_ctx = ffi::av_buffer_ref(frames_ref);

    let mut opts = Dictionary::new();
    opts.set("async_depth", "1"); // one-in/one-out — minimal encode-pipeline latency
    video
        .open_with(opts)
        .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))
}

/// Probe whether THIS GPU can VAAPI-encode `codec`, by opening a tiny encoder: the driver rejects
/// codecs its video engine can't do (e.g. AV1 on pre-RDNA3 AMD / pre-Arc Intel). Used to build the
/// GameStream codec advertisement so a client never negotiates a codec the GPU can't encode. The
/// device + encoder are torn down immediately (RAII).
pub fn probe_can_encode(codec: Codec) -> bool {
    if ffmpeg::init().is_err() {
        return false;
    }
    unsafe {
        // A missing VA device (non-VAAPI host, GPU-less CI) is an expected probe outcome — quiet
        // ffmpeg's "No VA display found" error for the probe, then restore the level.
        let prev = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_FATAL);
        let ok = match VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, 640, 480, 2) {
            Ok(hw) => {
                open_vaapi_encoder(codec, 640, 480, 30, 2_000_000, hw.device_ref, hw.frames_ref)
                    .is_ok()
            }
            Err(_) => false,
        };
        ffi::av_log_set_level(prev);
        ok
    }
}

/// Drain the encoder for one packet (shared poll logic).
fn poll_encoder(enc: &mut encoder::video::Encoder, fps: u32) -> Result<Option<EncodedFrame>> {
    let mut pkt = Packet::empty();
    match enc.receive_packet(&mut pkt) {
        Ok(()) => {
            let data = pkt.data().map(|d| d.to_vec()).unwrap_or_default();
            let pts = pkt.pts().unwrap_or(0).max(0) as u64;
            Ok(Some(EncodedFrame {
                data,
                pts_ns: pts * 1_000_000_000 / fps as u64,
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

// ---------------------------------------------------------------------------------------------
// CPU upload path (Phase 1): swscale RGB→NV12 → upload into a pooled VA surface → encode.
// ---------------------------------------------------------------------------------------------

/// VAAPI device + NV12 frames pool (the encoder's input surfaces for the CPU path).
struct VaapiHw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl VaapiHw {
    unsafe fn new(sw_format: ffi::AVPixelFormat, w: u32, h: u32, pool: c_int) -> Result<Self> {
        let mut device_ref: *mut ffi::AVBufferRef = ptr::null_mut();
        let node = render_node();
        let r = ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            node.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if r < 0 {
            bail!("no VAAPI device ({:?}): {}", node, ffmpeg::Error::from(r));
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

struct CpuInner {
    enc: encoder::video::Encoder,
    hw: VaapiHw,
    sws: *mut ffi::SwsContext,
    nv12: *mut ffi::AVFrame, // reusable software NV12 staging frame (swscale dst → upload src)
    src_format: PixelFormat,
    width: u32,
    height: u32,
}

impl CpuInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let src_pixel = vaapi_sws_src(format)?;
        const POOL: c_int = 16;
        let hw = unsafe { VaapiHw::new(ffi::AVPixelFormat::AV_PIX_FMT_NV12, width, height, POOL)? };
        let enc = unsafe {
            open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                hw.device_ref,
                hw.frames_ref,
            )?
        };
        // swscale RGB→NV12, BT.709 limited (matches the VUI), no rescale.
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
            let cs709 = ffi::sws_getCoefficients(SWS_CS_ITU709);
            ffi::sws_setColorspaceDetails(sws, cs709, 1, cs709, 0, 0, 1 << 16, 1 << 16);
        }
        let nv12 = unsafe {
            let f = ffi::av_frame_alloc();
            if f.is_null() {
                ffi::sws_freeContext(sws);
                bail!("av_frame_alloc(NV12) failed");
            }
            (*f).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int;
            (*f).width = width as c_int;
            (*f).height = height as c_int;
            if ffi::av_frame_get_buffer(f, 0) < 0 {
                let mut f = f;
                ffi::av_frame_free(&mut f);
                ffi::sws_freeContext(sws);
                bail!("av_frame_get_buffer(NV12) failed");
            }
            f
        };
        tracing::info!(
            encoder = codec.vaapi_name(),
            "VAAPI encode active ({width}x{height}@{fps}, CPU→NV12 upload path)"
        );
        Ok(CpuInner {
            enc,
            hw,
            sws,
            nv12,
            src_format: format,
            width,
            height,
        })
    }

    fn submit(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {format:?} != encoder source {:?}",
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_row = w * self.src_format.bytes_per_pixel();
        anyhow::ensure!(bytes.len() >= src_row * h, "captured buffer too small");
        unsafe {
            let src_data: [*const u8; 4] = [bytes.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_stride: [c_int; 4] = [src_row as c_int, 0, 0, 0];
            if ffi::sws_scale(
                self.sws,
                src_data.as_ptr(),
                src_stride.as_ptr(),
                0,
                h as c_int,
                (*self.nv12).data.as_ptr(),
                (*self.nv12).linesize.as_ptr(),
            ) < 0
            {
                bail!("sws_scale RGB→NV12 failed");
            }
            let mut hwf = ffi::av_frame_alloc();
            if hwf.is_null() {
                bail!("av_frame_alloc(hw) failed");
            }
            if ffi::av_hwframe_get_buffer(self.hw.frames_ref, hwf, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_get_buffer(VAAPI) failed");
            }
            if ffi::av_hwframe_transfer_data(hwf, self.nv12, 0) < 0 {
                ffi::av_frame_free(&mut hwf);
                bail!("av_hwframe_transfer_data(→VAAPI) failed");
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

impl Drop for CpuInner {
    fn drop(&mut self) {
        unsafe {
            if !self.nv12.is_null() {
                ffi::av_frame_free(&mut self.nv12);
            }
            if !self.sws.is_null() {
                ffi::sws_freeContext(self.sws);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Zero-copy dmabuf path: DRM-PRIME → hwmap(vaapi) → scale_vaapi(nv12) filter graph → encode.
// ---------------------------------------------------------------------------------------------

struct DmabufInner {
    enc: encoder::video::Encoder,
    /// DRM device the source dmabuf frames reference (the buffersrc's `hw_frames_ctx` device).
    drm_device: *mut ffi::AVBufferRef,
    /// VAAPI device driving `hwmap`/`scale_vaapi`/the encoder.
    vaapi_device: *mut ffi::AVBufferRef,
    /// DRM-PRIME frames context for the imported dmabufs (buffersrc input).
    drm_frames: *mut ffi::AVBufferRef,
    graph: *mut ffi::AVFilterGraph,
    src: *mut ffi::AVFilterContext,
    sink: *mut ffi::AVFilterContext,
    width: u32,
    height: u32,
    fourcc: u32,
}

impl DmabufInner {
    fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
    ) -> Result<Self> {
        let drm_fourcc = crate::zerocopy::drm_fourcc(format)
            .ok_or_else(|| anyhow!("no DRM fourcc for {format:?} (VAAPI zero-copy)"))?;
        let node = render_node();
        unsafe {
            // DRM device (source dmabuf frames) + a VAAPI device derived from it (same GPU) for
            // hwmap/scale_vaapi/the encoder.
            let mut drm_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut drm_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
                node.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!(
                    "av_hwdevice_ctx_create(DRM {:?}): {}",
                    node,
                    ffmpeg::Error::from(r)
                );
            }
            let mut vaapi_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create_derived(
                &mut vaapi_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                drm_device,
                0,
            );
            if r < 0 {
                ffi::av_buffer_unref(&mut drm_device);
                bail!("derive VAAPI from DRM: {}", ffmpeg::Error::from(r));
            }

            // DRM-PRIME frames context for the imported dmabufs.
            let mut drm_frames = ffi::av_hwframe_ctx_alloc(drm_device);
            if drm_frames.is_null() {
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_hwframe_ctx_alloc(DRM) failed");
            }
            let fc = (*drm_frames).data as *mut ffi::AVHWFramesContext;
            (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
            (*fc).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0; // packed XR24 RGB plane
            (*fc).width = width as c_int;
            (*fc).height = height as c_int;
            if ffi::av_hwframe_ctx_init(drm_frames) < 0 {
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_hwframe_ctx_init(DRM) failed");
            }

            // Filter graph: buffer(drm_prime) → hwmap=derive_device=vaapi:mode=read →
            // scale_vaapi=format=nv12 → buffersink.
            let mut graph = ffi::avfilter_graph_alloc();
            if graph.is_null() {
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_graph_alloc failed");
            }

            let mk = |name: &CStr, inst: &CStr| -> *mut ffi::AVFilterContext {
                let f = ffi::avfilter_get_by_name(name.as_ptr());
                if f.is_null() {
                    return ptr::null_mut();
                }
                ffi::avfilter_graph_alloc_filter(graph, f, inst.as_ptr())
            };
            let src = mk(c"buffer", c"in");
            let hwmap = mk(c"hwmap", c"map");
            let scale = mk(c"scale_vaapi", c"csc");
            let sink = mk(c"buffersink", c"out");
            if src.is_null() || hwmap.is_null() || scale.is_null() || sink.is_null() {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("a VAAPI filter (buffer/hwmap/scale_vaapi/buffersink) is missing");
            }
            // hwmap maps the DRM-PRIME input onto THIS vaapi device; scale_vaapi runs the CSC on
            // it. Giving both our device (rather than `hwmap=derive_device`) keeps every surface —
            // and the sink's output frames ctx the encoder adopts — on one VADisplay.
            (*hwmap).hw_device_ctx = ffi::av_buffer_ref(vaapi_device);
            (*scale).hw_device_ctx = ffi::av_buffer_ref(vaapi_device);

            // buffersrc params: DRM-PRIME frames, the drm_frames ctx.
            let par = ffi::av_buffersrc_parameters_alloc();
            (*par).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*par).width = width as c_int;
            (*par).height = height as c_int;
            (*par).time_base = ffi::AVRational {
                num: 1,
                den: fps as c_int,
            };
            (*par).hw_frames_ctx = ffi::av_buffer_ref(drm_frames);
            let r = ffi::av_buffersrc_parameters_set(src, par);
            ffi::av_free(par as *mut _);
            if r < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("av_buffersrc_parameters_set failed ({r})");
            }
            macro_rules! init {
                ($ctx:expr, $args:expr, $what:literal) => {{
                    let r = ffi::avfilter_init_str($ctx, $args);
                    if r < 0 {
                        ffi::avfilter_graph_free(&mut graph);
                        ffi::av_buffer_unref(&mut drm_frames);
                        ffi::av_buffer_unref(&mut vaapi_device);
                        ffi::av_buffer_unref(&mut drm_device);
                        bail!(concat!("init ", $what, " failed ({})"), r);
                    }
                }};
            }
            init!(src, ptr::null(), "buffer");
            init!(hwmap, c"mode=read".as_ptr(), "hwmap");
            init!(scale, c"format=nv12".as_ptr(), "scale_vaapi");
            init!(sink, ptr::null(), "buffersink");

            let link = |a: *mut ffi::AVFilterContext, b: *mut ffi::AVFilterContext| -> c_int {
                ffi::avfilter_link(a, 0, b, 0)
            };
            if link(src, hwmap) < 0 || link(hwmap, scale) < 0 || link(scale, sink) < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_link failed");
            }
            let r = ffi::avfilter_graph_config(graph, ptr::null_mut());
            if r < 0 {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("avfilter_graph_config failed ({r})");
            }

            // The encoder takes NV12 surfaces from the sink's output frames context.
            let nv12_ctx = ffi::av_buffersink_get_hw_frames_ctx(sink);
            if nv12_ctx.is_null() {
                ffi::avfilter_graph_free(&mut graph);
                ffi::av_buffer_unref(&mut drm_frames);
                ffi::av_buffer_unref(&mut vaapi_device);
                ffi::av_buffer_unref(&mut drm_device);
                bail!("filter sink has no VAAPI frames context");
            }
            let enc = open_vaapi_encoder(
                codec,
                width,
                height,
                fps,
                bitrate_bps,
                vaapi_device,
                nv12_ctx,
            )?;

            tracing::info!(
                encoder = codec.vaapi_name(),
                "VAAPI encode active ({width}x{height}@{fps}, zero-copy dmabuf → GPU NV12)"
            );
            Ok(DmabufInner {
                enc,
                drm_device,
                vaapi_device,
                drm_frames,
                graph,
                src,
                sink,
                width,
                height,
                fourcc: drm_fourcc,
            })
        }
    }

    fn submit(&mut self, dmabuf: &DmabufFrame, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            dmabuf.fourcc == self.fourcc,
            "dmabuf fourcc {:#x} != encoder {:#x}",
            dmabuf.fourcc,
            self.fourcc
        );
        unsafe {
            // Build a DRM-PRIME AVFrame describing the dmabuf (one object/fd, one layer/plane).
            let mut desc: Box<ffi::AVDRMFrameDescriptor> = Box::new(std::mem::zeroed());
            desc.nb_objects = 1;
            desc.objects[0].fd = dmabuf.fd.as_raw_fd();
            desc.objects[0].size = 0;
            desc.objects[0].format_modifier = dmabuf.modifier;
            desc.nb_layers = 1;
            desc.layers[0].format = self.fourcc;
            desc.layers[0].nb_planes = 1;
            desc.layers[0].planes[0].object_index = 0;
            desc.layers[0].planes[0].offset = dmabuf.offset as isize;
            desc.layers[0].planes[0].pitch = dmabuf.stride as isize;

            let mut drm = ffi::av_frame_alloc();
            if drm.is_null() {
                bail!("av_frame_alloc(drm) failed");
            }
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as c_int;
            (*drm).width = self.width as c_int;
            (*drm).height = self.height as c_int;
            (*drm).hw_frames_ctx = ffi::av_buffer_ref(self.drm_frames);
            (*drm).data[0] = Box::into_raw(desc) as *mut u8;
            // Own the descriptor so it frees with the frame (the fd is owned by the DmabufFrame,
            // which outlives this call — the graph reads the surface before submit returns).
            extern "C" fn free_desc(_opaque: *mut std::ffi::c_void, data: *mut u8) {
                unsafe { drop(Box::from_raw(data as *mut ffi::AVDRMFrameDescriptor)) };
            }
            (*drm).buf[0] = ffi::av_buffer_create(
                (*drm).data[0],
                std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
                Some(free_desc),
                ptr::null_mut(),
                0,
            );

            // Push through hwmap → scale_vaapi; pull the NV12 surface back out.
            let r = ffi::av_buffersrc_add_frame_flags(
                self.src,
                drm,
                ffi::AV_BUFFERSRC_FLAG_KEEP_REF as c_int,
            );
            ffi::av_frame_free(&mut drm);
            if r < 0 {
                bail!("av_buffersrc_add_frame failed ({r})");
            }
            let mut nv12 = ffi::av_frame_alloc();
            if nv12.is_null() {
                bail!("av_frame_alloc(nv12) failed");
            }
            let r = ffi::av_buffersink_get_frame(self.sink, nv12);
            if r < 0 {
                ffi::av_frame_free(&mut nv12);
                bail!("av_buffersink_get_frame failed ({r})");
            }
            (*nv12).pts = pts;
            (*nv12).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), nv12);
            ffi::av_frame_free(&mut nv12);
            if r < 0 {
                bail!("avcodec_send_frame(VAAPI) failed ({r})");
            }
        }
        Ok(())
    }
}

impl Drop for DmabufInner {
    fn drop(&mut self) {
        unsafe {
            ffi::avfilter_graph_free(&mut self.graph);
            ffi::av_buffer_unref(&mut self.drm_frames);
            ffi::av_buffer_unref(&mut self.vaapi_device);
            ffi::av_buffer_unref(&mut self.drm_device);
        }
    }
}

// ---------------------------------------------------------------------------------------------

enum Inner {
    Cpu(CpuInner),
    Dmabuf(DmabufInner),
}

pub struct VaapiEncoder {
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    /// Built lazily from the first frame's payload (CPU upload vs zero-copy dmabuf).
    inner: Option<Inner>,
    frame_idx: i64,
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
        if bit_depth != 8 {
            tracing::warn!(bit_depth, "VAAPI 10-bit not yet wired — encoding 8-bit");
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            unsafe { ffi::av_log_set_level(48) };
        }
        // Validate the codec/format up front so a bad request fails at open, not on the first frame.
        let _ = vaapi_sws_src(format)?;
        Ok(VaapiEncoder {
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            inner: None,
            frame_idx: 0,
            force_kf: false,
        })
    }

    fn ensure_inner(&mut self, want_dmabuf: bool) -> Result<&mut Inner> {
        if self.inner.is_none() {
            let inner = if want_dmabuf {
                Inner::Dmabuf(DmabufInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            } else {
                Inner::Cpu(CpuInner::open(
                    self.codec,
                    self.format,
                    self.width,
                    self.height,
                    self.fps,
                    self.bitrate_bps,
                )?)
            };
            self.inner = Some(inner);
        }
        Ok(self.inner.as_mut().unwrap())
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
            FramePayload::Cpu(bytes) => match self.ensure_inner(false)? {
                Inner::Cpu(c) => c.submit(bytes, captured.format, pts, idr),
                Inner::Dmabuf(_) => bail!("VAAPI encoder built for dmabuf got a CPU frame"),
            },
            FramePayload::Dmabuf(d) => match self.ensure_inner(true)? {
                Inner::Dmabuf(dm) => dm.submit(d, pts, idr),
                Inner::Cpu(_) => bail!("VAAPI encoder built for CPU got a dmabuf frame"),
            },
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
        match &mut self.inner {
            Some(Inner::Cpu(c)) => poll_encoder(&mut c.enc, self.fps),
            Some(Inner::Dmabuf(d)) => poll_encoder(&mut d.enc, self.fps),
            None => Ok(None),
        }
    }

    fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            Some(Inner::Cpu(c)) => c.enc.send_eof().context("send_eof")?,
            Some(Inner::Dmabuf(d)) => d.enc.send_eof().context("send_eof")?,
            None => {}
        }
        Ok(())
    }
}
