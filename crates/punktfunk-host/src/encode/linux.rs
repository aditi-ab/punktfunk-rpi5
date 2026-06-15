//! NVENC encoder via `ffmpeg-next` (binds the system FFmpeg — `ffmpeg-sys-next` auto-detects the
//! installed version, so this builds against FFmpeg 7.x/libavcodec 61 *or* 8.x/libavcodec 62;
//! validated live on Ubuntu 26.04 (FFmpeg 8) and Bazzite F43 (FFmpeg 7.1)).
//!
//! Input is a packed RGB/BGR CPU frame; `*_nvenc` accepts `rgb0`/`bgr0`/`rgba`/`bgra`
//! directly and does the RGB→YUV conversion on the GPU, so the host stays off the
//! colour-conversion path. The portal commonly negotiates packed 24-bit `RGB`, which NVENC
//! does *not* accept — we expand it to `rgb0` (one padding byte/pixel, no colour math).
//! The encoder is opened *without* a global header so VPS/SPS/PPS are emitted in-band on
//! every IDR — the output is both a playable raw Annex-B stream and self-contained AUs.

use super::{Codec, EncodedFrame, Encoder};
use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video as VideoFrame;
use ffmpeg::{codec, encoder, Dictionary, Packet, Rational};
use ffmpeg_next as ffmpeg;
use std::os::raw::c_int;

use ffmpeg::ffi; // = ffmpeg_sys_next

/// `AVCUDADeviceContext` (libavutil/hwcontext_cuda.h) — not in the ffmpeg-sys bindings (the
/// crate doesn't allowlist that header), so mirror its stable 3-pointer layout. We set the
/// first field to *our* `CUcontext` so NVENC shares the context the EGL importer maps into.
#[repr(C)]
struct AVCUDADeviceContext {
    cuda_ctx: *mut std::ffi::c_void, // CUcontext
    stream: *mut std::ffi::c_void,   // CUstream (null = default)
    internal: *mut std::ffi::c_void, // filled by ctx_init
}

/// CUDA hardware-frame contexts that wrap our shared `CUcontext`, so `hevc_nvenc` reads the
/// imported device buffer directly. Owns two `AVBufferRef`s, unref'd on drop.
struct CudaHw {
    device_ref: *mut ffi::AVBufferRef,
    frames_ref: *mut ffi::AVBufferRef,
}

impl CudaHw {
    /// Build a CUDA hwdevice wrapping `cu_ctx` and a frames pool (`sw_format` = `pixel`).
    unsafe fn new(cu_ctx: *mut std::ffi::c_void, sw_format: Pixel, w: u32, h: u32) -> Result<Self> {
        let mut device_ref = ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
        if device_ref.is_null() {
            bail!("av_hwdevice_ctx_alloc(CUDA) failed");
        }
        let dev_ctx = (*device_ref).data as *mut ffi::AVHWDeviceContext;
        let cu = (*dev_ctx).hwctx as *mut AVCUDADeviceContext;
        (*cu).cuda_ctx = cu_ctx; // share the importer's context
        let r = ffi::av_hwdevice_ctx_init(device_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwdevice_ctx_init failed ({r})");
        }

        let mut frames_ref = ffi::av_hwframe_ctx_alloc(device_ref);
        if frames_ref.is_null() {
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_alloc failed");
        }
        let fc = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*fc).format = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*fc).sw_format = pixel_to_av(sw_format);
        (*fc).width = w as c_int;
        (*fc).height = h as c_int;
        (*fc).initial_pool_size = 0; // we supply the device pointers
        let r = ffi::av_hwframe_ctx_init(frames_ref);
        if r < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            ffi::av_buffer_unref(&mut device_ref);
            bail!("av_hwframe_ctx_init failed ({r})");
        }
        Ok(CudaHw {
            device_ref,
            frames_ref,
        })
    }
}

impl Drop for CudaHw {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.frames_ref);
            ffi::av_buffer_unref(&mut self.device_ref);
        }
    }
}

/// `ffmpeg::format::Pixel` → raw `AVPixelFormat`.
fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    // `Pixel` is `#[repr(i32)]`-compatible with `AVPixelFormat` (the bindgen enum) via this
    // documented conversion in ffmpeg-next.
    ffi::AVPixelFormat::from(p)
}

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
        // 10-bit HDR (R10G10B10A2) is produced only by the Windows DXGI HDR capture path; the Linux
        // capturer never emits it. Map to BGRA so the match is exhaustive — unreachable here.
        PixelFormat::Rgb10a2 => (Pixel::BGRA, false),
    }
}

pub struct NvencEncoder {
    enc: encoder::video::Encoder,
    /// Reusable 4-bpp CPU input frame (CPU path only; `None` for the zero-copy/CUDA path).
    /// Mutating it in place across frames is sound only because the encoder is opened with
    /// `delay=0`/`bf=0`/`max_b_frames=0` and the caller drains `poll()` after each `submit`,
    /// so libavcodec holds no reference to the previous frame's buffer when we overwrite it.
    frame: Option<VideoFrame>,
    /// Zero-copy path: CUDA hwdevice/hwframes contexts (the encoder takes `AV_PIX_FMT_CUDA`).
    cuda: Option<CudaHw>,
    src_format: PixelFormat,
    expand: bool,
    width: u32,
    height: u32,
    fps: u32,
    /// Monotonic presentation index, in `1/fps` time-base units.
    frame_idx: i64,
    /// Force the next submitted frame to be an IDR (set by [`request_keyframe`]).
    force_kf: bool,
}

// `CudaHw` holds raw `AVBufferRef`s; the encoder lives on a single thread. The CPU encoder is
// already `Send` via ffmpeg-next; assert it for the CUDA fields too.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        cuda: bool,
        bit_depth: u8,
    ) -> Result<Self> {
        // TODO(hdr): Linux 10-bit parity. Unlike the Windows raw-SDK path (which upconverts 8-bit
        // ARGB → Main10 via pixelBitDepthMinus8), libavcodec hevc_nvenc needs a 10-bit input pixel
        // format (p010) for Main10, so it's a bigger change; deferred until a Linux GPU box is
        // available to validate. The Linux host stays 8-bit for now.
        if bit_depth != 8 {
            tracing::warn!(
                bit_depth,
                "Linux NVENC 10-bit not yet wired — encoding 8-bit"
            );
        }
        ffmpeg::init().context("ffmpeg init")?;
        if std::env::var_os("PUNKTFUNK_FFMPEG_DEBUG").is_some() {
            unsafe { ffi::av_log_set_level(48) }; // AV_LOG_DEBUG — surface NVENC hw-frame rejects
        }
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
        // VBV/HRD buffer — bound the SIZE of any single frame. Under CBR with no buffer set, NVENC
        // uses a loose default VBV, so a high-motion P-frame is allowed to balloon to many times the
        // average; those extra packets overflow the bounded send queue + kernel socket buffer and
        // get dropped, which the client sees as framedrops/jitter (and, on the infinite-GOP path, as
        // old/stale frames flashing until the next RFI). A tight ~1-frame buffer makes the encoder
        // hold frame size roughly constant and absorb motion as a momentary QP (quality) dip instead
        // — the trade we want. Default = 1 frame of bits (bitrate/fps); PUNKTFUNK_VBV_FRAMES tunes it
        // (larger = better motion quality but bigger per-frame bursts).
        let vbv_frames = std::env::var("PUNKTFUNK_VBV_FRAMES")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(1.0);
        let vbv_bits = ((bitrate_bps as f64 / fps.max(1) as f64) * vbv_frames as f64)
            .clamp(1.0, i32::MAX as f64);
        unsafe {
            (*video.as_mut_ptr()).rc_buffer_size = vbv_bits as i32;
        }
        video.set_max_b_frames(0);
        // Infinite GOP — NO periodic IDR. A keyframe at 5120x1440 is ~20-40x a P-frame, so a
        // periodic IDR is a recurring multi-millisecond encode+packetize+send spike — the ~2s
        // "freeze". NVENC emits one IDR at stream start, then P-frames only; `forced-idr` (below)
        // turns a client recovery request (RFI, via `request_keyframe`) into an IDR on demand.
        // This is the Moonlight/Sunshine low-latency model.
        unsafe {
            (*video.as_mut_ptr()).gop_size = -1;
        }

        // For the zero-copy path, take CUDA surfaces: wrap the shared CUcontext in CUDA
        // hwdevice/hwframes contexts and set `pix_fmt = CUDA` on the raw encoder context
        // *before* open (NVENC derives the device from `hw_frames_ctx`).
        let cuda_hw = if cuda {
            let cu_ctx = crate::zerocopy::cuda::context().context("shared CUDA context")?;
            let hw = unsafe { CudaHw::new(cu_ctx, nvenc_pixel, width, height)? };
            unsafe {
                let raw = video.as_mut_ptr();
                (*raw).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_CUDA;
                (*raw).hw_device_ctx = ffi::av_buffer_ref(hw.device_ref);
                (*raw).hw_frames_ctx = ffi::av_buffer_ref(hw.frames_ref);
            }
            Some(hw)
        } else {
            None
        };

        // Low-latency NVENC tuning (plan §7 / linux-setup doc).
        let mut opts = Dictionary::new();
        opts.set("preset", "p1"); // fastest
        opts.set("tune", "ull"); // ultra-low-latency
        opts.set("rc", "cbr");
        opts.set("bf", "0");
        opts.set("delay", "0");
        opts.set("forced-idr", "1"); // RFI/request_keyframe → real IDR under the infinite GOP

        // Split-frame encode across both NVENC engines (GB203 has 2) when the pixel rate exceeds
        // a single engine's HEVC capacity (~1 Gpix/s); e.g. 5120x1440@240 = 1.77 Gpix/s needs it,
        // @120 = 0.88 Gpix/s does not. HEVC/AV1 only (not H.264). AUTO won't engage below ~2112px
        // height, so we force `2`; below the threshold we leave it AUTO (split costs ~2% BD-rate).
        // Output is standard HEVC — transparent to the client. Override with PUNKTFUNK_SPLIT_ENCODE.
        let pix_rate = width as u64 * height as u64 * fps as u64;
        let split = std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok();
        match split.as_deref() {
            Some(mode) => opts.set("split_encode_mode", mode),
            None if matches!(codec, Codec::H265 | Codec::Av1) && pix_rate > 1_000_000_000 => {
                opts.set("split_encode_mode", "2");
                tracing::info!(
                    pix_rate,
                    "NVENC: forcing 2-way split encode (high pixel rate)"
                );
            }
            None => {}
        }

        let enc = video
            .open_with(opts)
            .with_context(|| format!("open {name} ({width}x{height}@{fps}, {bitrate_bps} bps)"))?;

        let frame = if cuda {
            None
        } else {
            Some(VideoFrame::new(nvenc_pixel, width, height))
        };
        Ok(NvencEncoder {
            enc,
            frame,
            cuda: cuda_hw,
            src_format: format,
            expand,
            width,
            height,
            fps,
            frame_idx: 0,
            force_kf: false,
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
        let pts = self.frame_idx;
        self.frame_idx += 1;
        // Force an IDR when requested (client RFI); otherwise let NVENC pick (GOP/P-frame).
        let idr = self.force_kf;
        self.force_kf = false;
        match &captured.payload {
            FramePayload::Cuda(buf) => self.submit_cuda(buf, pts, idr),
            FramePayload::Cpu(bytes) => self.submit_cpu(bytes, captured.format, pts, idr),
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

impl NvencEncoder {
    /// CPU path: expand/copy the packed RGB/BGR bytes into the reusable 4-bpp frame, then send.
    fn submit_cpu(&mut self, bytes: &[u8], format: PixelFormat, pts: i64, idr: bool) -> Result<()> {
        anyhow::ensure!(
            format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            format,
            self.src_format
        );
        let w = self.width as usize;
        let h = self.height as usize;
        let src_bpp = self.src_format.bytes_per_pixel();
        let src_row = w * src_bpp;
        anyhow::ensure!(
            bytes.len() >= src_row * h,
            "captured buffer {} bytes < required {}",
            bytes.len(),
            src_row * h
        );
        let frame = self
            .frame
            .as_mut()
            .context("CPU frame missing (encoder opened in CUDA mode)")?;
        let stride = frame.stride(0); // dst is 4-bpp, aligned
        let dst = frame.data_mut(0);
        if self.expand {
            // packed 3-bpp RGB/BGR → 4-bpp *0 (copy 3 bytes, zero the pad byte)
            for y in 0..h {
                let s = &bytes[y * src_row..y * src_row + src_row];
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
                    .copy_from_slice(&bytes[y * src_row..y * src_row + src_row]);
            }
        }
        frame.set_pts(Some(pts));
        frame.set_kind(if idr {
            ffmpeg::picture::Type::I
        } else {
            ffmpeg::picture::Type::None
        });
        self.enc.send_frame(frame).context("send_frame")?;
        Ok(())
    }

    /// Zero-copy path: hand the imported CUDA device buffer to NVENC with no CPU touch.
    ///
    /// We take a *pooled* surface from the CUDA hwframes context (`av_hwframe_get_buffer`) and
    /// device→device-copy our imported buffer into it, rather than wrapping our own pointer in a
    /// bare frame. Two reasons: (1) NVENC's `nvenc_send_frame` ignores frames whose `buf[0]` is
    /// null and the generic encode path's `av_frame_ref` needs a refcounted buffer — a bare
    /// frame is rejected with `EINVAL`; (2) NVENC caches CUDA-resource *registrations* keyed by
    /// device pointer with a bounded table, so a fresh pointer every frame would thrash/overflow
    /// it — the pool recycles a small set of pointers. The extra copy is device-local (~8 MB at
    /// 1080p, sub-millisecond on the GPU) and keeps the host fully off the pixel path.
    fn submit_cuda(
        &mut self,
        buf: &crate::zerocopy::DeviceBuffer,
        pts: i64,
        idr: bool,
    ) -> Result<()> {
        let frames_ref = self
            .cuda
            .as_ref()
            .context("CUDA hw context missing (encoder opened in CPU mode)")?
            .frames_ref;
        // The device→device copy below uses our shared context directly; make it current on the
        // encode thread (ffmpeg pushes its own around the pool alloc, so order is fine).
        crate::zerocopy::cuda::make_current().context("CUDA context current (encode thread)")?;
        unsafe {
            let mut f = ffi::av_frame_alloc();
            if f.is_null() {
                bail!("av_frame_alloc failed");
            }
            // Pooled CUDA surface: sets format, width/height, data[0]/linesize[0], buf[0] and
            // hw_frames_ctx. Reused across frames (the pool recycles), keeping NVENC's
            // registration cache warm.
            let r = ffi::av_hwframe_get_buffer(frames_ref, f, 0);
            if r < 0 {
                ffi::av_frame_free(&mut f);
                bail!("av_hwframe_get_buffer(CUDA) failed ({r})");
            }
            let dst_ptr = (*f).data[0] as crate::zerocopy::cuda::CUdeviceptr;
            let dst_pitch = (*f).linesize[0] as usize;
            if let Err(e) = crate::zerocopy::cuda::copy_device_to_device(buf, dst_ptr, dst_pitch) {
                ffi::av_frame_free(&mut f);
                return Err(e).context("copy imported buffer into NVENC surface");
            }
            (*f).pts = pts;
            (*f).pict_type = if idr {
                ffi::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ffi::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let r = ffi::avcodec_send_frame(self.enc.as_mut_ptr(), f);
            ffi::av_frame_free(&mut f);
            if r < 0 {
                bail!("avcodec_send_frame(CUDA) failed ({r})");
            }
        }
        Ok(())
    }
}
