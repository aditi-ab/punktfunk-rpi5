//! Video decode: reassembled HEVC access units → frames for the GTK presenter.
//!
//! Two backends, picked at session start (override: `PUNKTFUNK_DECODER=software|vaapi`):
//!
//! * **VAAPI** (Intel/AMD): libavcodec hwaccel decodes on the GPU; each frame is mapped
//!   to a DRM-PRIME dmabuf (`av_hwframe_map`, zero copy) and handed to the UI as fds +
//!   plane layout for `GdkDmabufTextureBuilder` — inside `GtkGraphicsOffload` that is the
//!   decoder-to-subsurface path, direct-scanout eligible when fullscreen. NVIDIA boxes
//!   have no usable VAAPI (nvidia-vaapi-driver is broken for this — Moonlight blacklists
//!   it); device creation fails there and the software path takes over. A mid-session
//!   VAAPI error also falls back — the host's IDR/RFI recovery resynchronizes.
//! * **Software**: libavcodec on the CPU + swscale to RGBA (`GdkMemoryTexture` upload).
//!   Slice threading only — frame threading would add a frame of latency per thread.
//!
//! Both run `AV_CODEC_FLAG_LOW_DELAY`; the host encodes zero-reorder streams (no
//! B-frames, in-band parameter sets on every IDR), so decode is strictly one-in/one-out.

use anyhow::{anyhow, bail, Context as _, Result};
use ffmpeg::format::Pixel;
use ffmpeg::software::scaling;
use ffmpeg::util::frame::Video as AvFrame;
use ffmpeg_next as ffmpeg;
use std::os::fd::RawFd;
use std::ptr;

/// One decoded frame headed for the presenter, carrying the host capture timestamp so the
/// UI can measure capture→displayed latency at the moment it presents.
pub struct DecodedFrame {
    /// Host-clock capture pts (ns) of the AU this image decoded from — compare against
    /// the local wall clock + `clock_offset_ns` at paintable-set time.
    pub pts_ns: u64,
    /// Local wall clock (ns) when the decoder emitted this image — the `decoded`
    /// measurement point (design/stats-unification.md); the presenter subtracts it from
    /// its paintable-set stamp for the client-local `display` stage.
    pub decoded_ns: u64,
    pub image: DecodedImage,
}

pub enum DecodedImage {
    Cpu(CpuFrame),
    Dmabuf(DmabufFrame),
}

/// The stream's colour signaling, read PER-FRAME from the decoder (HEVC VUI → the
/// `AVFrame` CICP fields). The Windows host switches an HDR desktop to Main10 BT.2020 PQ
/// **in-band** (the Welcome still says SDR — clients are expected to follow the VUI, as
/// the Windows/Apple/Android clients do), so rendering must follow the frames, not the
/// handshake — else PQ content drawn as BT.709 comes out washed out and desaturated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorDesc {
    /// H.273 code points as signaled (2 = unspecified → the renderer picks the SDR default).
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorDesc {
    /// Read the CICP fields off a raw decoded frame.
    ///
    /// # Safety
    /// `frame` must point to a valid `AVFrame` (alive for the duration of the call).
    unsafe fn from_raw(frame: *const ffmpeg::ffi::AVFrame) -> ColorDesc {
        // SAFETY: caller guarantees a live AVFrame; these are plain enum field reads.
        unsafe {
            ColorDesc {
                primaries: (*frame).color_primaries as u32 as u8,
                transfer: (*frame).color_trc as u32 as u8,
                matrix: (*frame).colorspace as u32 as u8,
                full_range: (*frame).color_range == ffmpeg::ffi::AVColorRange::AVCOL_RANGE_JPEG,
            }
        }
    }

    /// PQ (SMPTE ST.2084) transfer — the HDR10 signal.
    pub fn is_pq(&self) -> bool {
        self.transfer == 16
    }
}

/// RGBA pixels for `GdkMemoryTexture` (which takes a stride).
pub struct CpuFrame {
    pub width: u32,
    pub height: u32,
    /// RGBA row stride in bytes (≥ width*4 — swscale pads rows for SIMD).
    pub stride: usize,
    pub rgba: Vec<u8>,
    /// Signaling of the source frame. swscale already undid the YUV matrix + range (the
    /// pixels are full-range RGB), but a PQ/BT.2020 stream keeps its transfer + primaries
    /// baked in — the presenter tags the texture so GTK tone-maps it.
    pub color: ColorDesc,
}

/// A decoded frame still on the GPU: dmabuf fds + plane layout for
/// `GdkDmabufTextureBuilder`. The fds belong to `guard`'s mapped DRM frame — they stay
/// valid until the guard drops (the texture's release func).
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    /// Combined DRM fourcc of the whole surface (NV12 for 8-bit VAAPI output), derived
    /// from the decoder's software format — NOT the per-plane component formats.
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Signaling of the source frame — drives the `GdkDmabufTexture` color state (BT.709
    /// narrow for SDR, BT.2020 PQ for an HDR stream).
    pub color: ColorDesc,
    pub guard: DrmFrameGuard,
}

pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Owns the mapped DRM-PRIME `AVFrame` (which in turn references the VAAPI surface).
/// Dropping it releases the surface back to the decoder pool and closes the fds.
pub struct DrmFrameGuard(*mut ffmpeg::ffi::AVFrame);
// An AVFrame is plain refcounted data; freeing it from the GTK main thread is fine.
unsafe impl Send for DrmFrameGuard {}

impl Drop for DrmFrameGuard {
    fn drop(&mut self) {
        unsafe { ffmpeg::ffi::av_frame_free(&mut self.0) };
    }
}

enum Backend {
    Vaapi(VaapiDecoder),
    Software(SoftwareDecoder),
}

pub struct Decoder {
    backend: Backend,
    /// The negotiated codec (from the host's Welcome), so a mid-session VAAPI→software demotion
    /// rebuilds the software decoder for the SAME codec.
    codec_id: ffmpeg::codec::Id,
    /// Consecutive VAAPI decode errors — a single transient failure (e.g. a reference-missing
    /// frame after packet loss) shouldn't cost the whole session its hardware decoder.
    vaapi_fails: u32,
    /// Set when the decoder needs a fresh IDR to resynchronize (after an error or a demotion).
    /// The pump drains it and asks the host — under the infinite GOP there is no periodic
    /// keyframe, so a rebuilt/erroring decoder would otherwise stay gray/frozen forever.
    want_keyframe: bool,
}

/// Demote VAAPI→software only after this many consecutive hardware decode errors; a lone
/// transient error just re-requests an IDR and keeps the hardware decoder.
const VAAPI_DEMOTE_AFTER: u32 = 3;

/// Map a negotiated `quic` codec bit to the FFmpeg decoder id the client opens.
pub fn ffmpeg_codec_id(wire: u8) -> ffmpeg::codec::Id {
    match wire {
        punktfunk_core::quic::CODEC_H264 => ffmpeg::codec::Id::H264,
        punktfunk_core::quic::CODEC_AV1 => ffmpeg::codec::Id::AV1,
        _ => ffmpeg::codec::Id::HEVC,
    }
}

/// The `quic` codec bitfield this client can decode — whatever FFmpeg has a decoder for (HEVC/H.264
/// always; AV1 when built in). Advertised to the host so it never emits a codec we can't decode.
pub fn decodable_codecs() -> u8 {
    let _ = ffmpeg::init();
    let mut bits = 0u8;
    for (id, bit) in [
        (ffmpeg::codec::Id::HEVC, punktfunk_core::quic::CODEC_HEVC),
        (ffmpeg::codec::Id::H264, punktfunk_core::quic::CODEC_H264),
        (ffmpeg::codec::Id::AV1, punktfunk_core::quic::CODEC_AV1),
    ] {
        if ffmpeg::decoder::find(id).is_some() {
            bits |= bit;
        }
    }
    bits
}

/// libavcodec logs reference-frame recovery to the process stderr very verbosely
/// (`First slice in a frame missing`, `Could not find ref with POC …`, `Error
/// constructing the frame RPS`) — normal chatter while the decoder waits for a keyframe
/// after loss, but a raw flood in the user's terminal (it bypasses our tracing). Default
/// it to fatal-only; `PUNKTFUNK_FFMPEG_LOG=<quiet|error|warning|info|debug>` restores it
/// for decode debugging. Process-global; set once per decoder build (idempotent).
fn quiet_ffmpeg_log() {
    use ffmpeg::util::log::Level;
    let level = match std::env::var("PUNKTFUNK_FFMPEG_LOG").ok().as_deref() {
        Some("quiet") => Level::Quiet,
        Some("error") => Level::Error,
        Some("warning") => Level::Warning,
        Some("info") => Level::Info,
        Some("debug" | "trace") => Level::Debug,
        _ => Level::Fatal,
    };
    ffmpeg::util::log::set_level(level);
}

impl Decoder {
    /// `codec_id` is the codec the host resolved in the Welcome (never assume HEVC).
    /// `pref` is the Settings "Video decoder" value (`auto`/`vaapi`/`software`).
    /// Precedence: the `PUNKTFUNK_DECODER` env override wins (support/debug escape
    /// hatch, and the documented knob), then the setting; both default to auto
    /// (VAAPI → software).
    pub fn new(codec_id: ffmpeg::codec::Id, pref: &str) -> Result<Decoder> {
        ffmpeg::init().context("ffmpeg init")?;
        quiet_ffmpeg_log();
        let choice = std::env::var("PUNKTFUNK_DECODER")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| pref.to_string());
        // Deck note: `auto` means VAAPI here too. GTK's tiled-NV12 dmabuf import is broken on
        // the Deck (Mesa ≥ 25.1 exports VCN surfaces TILED; artifacts/gray/washed-out), but the
        // presenter routes Deck frames through the in-process GL converter (`video_gl`) instead
        // of GdkDmabufTexture — and if THAT can't initialize, it demotes this decoder to
        // software mid-session via [`Decoder::force_software`]. The broken direct path is never
        // the fallback.
        if choice != "software" {
            match VaapiDecoder::new(codec_id) {
                Ok(v) => {
                    tracing::info!(?codec_id, "VAAPI hardware decode active (zero-copy dmabuf)");
                    return Ok(Decoder {
                        backend: Backend::Vaapi(v),
                        codec_id,
                        vaapi_fails: 0,
                        want_keyframe: false,
                    });
                }
                Err(e) => {
                    if choice == "vaapi" {
                        return Err(e.context("PUNKTFUNK_DECODER=vaapi but VAAPI failed"));
                    }
                    tracing::info!(reason = %e, "VAAPI unavailable — software decode");
                }
            }
        }
        Ok(Decoder {
            backend: Backend::Software(SoftwareDecoder::new(codec_id)?),
            codec_id,
            vaapi_fails: 0,
            want_keyframe: false,
        })
    }

    /// Drain the "please ask the host for an IDR" flag — the pump calls this each iteration
    /// (throttled) so a demoted/erroring decoder can resynchronize under the infinite GOP.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Demote to software decode on the PRESENTER's verdict (dmabuf presentation impossible:
    /// GL converter init failed, texture import rejected). Decode itself succeeds in that
    /// state, so the error-streak demotion never fires — without this the stream would stay
    /// black forever. No-op when already software.
    pub fn force_software(&mut self) -> Result<()> {
        if matches!(self.backend, Backend::Software(_)) {
            return Ok(());
        }
        tracing::warn!("presenter can't display hardware frames — demoting to software decode");
        self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
        self.vaapi_fails = 0;
        self.want_keyframe = true;
        Ok(())
    }

    /// Feed one access unit; returns the decoded frame (the host's streams are
    /// one-in/one-out). A software decode error after packet loss is survivable — log
    /// upstream and keep feeding. A VAAPI error re-requests an IDR and retries the hardware
    /// decoder; only a persistent streak of failures (a genuinely broken driver, e.g.
    /// nvidia-vaapi-driver) demotes to software. Either way `want_keyframe` is set so the
    /// pump asks the host for a fresh IDR — under the infinite GOP nothing else resyncs a
    /// rebuilt/erroring decoder, so skipping this leaves the picture gray/frozen for good.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        match &mut self.backend {
            Backend::Vaapi(v) => match v.decode(au) {
                Ok(f) => {
                    self.vaapi_fails = 0;
                    Ok(f.map(DecodedImage::Dmabuf))
                }
                Err(e) => {
                    self.vaapi_fails += 1;
                    self.want_keyframe = true;
                    if self.vaapi_fails >= VAAPI_DEMOTE_AFTER {
                        tracing::warn!(error = %e, fails = self.vaapi_fails,
                            "VAAPI decode failing repeatedly — demoting to software");
                        self.backend = Backend::Software(SoftwareDecoder::new(self.codec_id)?);
                        self.vaapi_fails = 0;
                    } else {
                        tracing::warn!(error = %e,
                            "VAAPI decode error — requesting keyframe, keeping hardware decode");
                    }
                    Ok(None)
                }
            },
            Backend::Software(s) => Ok(s.decode(au)?.map(DecodedImage::Cpu)),
        }
    }
}

// --- software backend ---------------------------------------------------------------

struct SoftwareDecoder {
    decoder: ffmpeg::decoder::Video,
    /// Rebuilt whenever the decoded format/size — or the colour signaling (a mid-stream
    /// SDR↔HDR flip) — changes.
    sws: Option<(scaling::Context, Pixel, u32, u32, ColorDesc)>,
}

impl SoftwareDecoder {
    fn new(codec_id: ffmpeg::codec::Id) -> Result<SoftwareDecoder> {
        let codec = ffmpeg::decoder::find(codec_id)
            .ok_or_else(|| anyhow!("no {codec_id:?} decoder in libavcodec"))?;
        let mut ctx = ffmpeg::codec::Context::new_with_codec(codec);
        unsafe {
            let raw = ctx.as_mut_ptr();
            (*raw).flags |= ffmpeg::ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Slice threading adds no frame delay (frame threading adds thread_count-1).
            (*raw).thread_type = ffmpeg::ffi::FF_THREAD_SLICE;
            (*raw).thread_count = 0; // auto
        }
        let decoder = ctx.decoder().video().context("open video decoder")?;
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
        // SAFETY: `frame.as_ptr()` is the decoder-owned live AVFrame for this call.
        let color = unsafe { ColorDesc::from_raw(frame.as_ptr()) };
        let rebuild = !matches!(&self.sws,
            Some((_, f, sw, sh, c)) if *f == fmt && *sw == w && *sh == h && *c == color);
        if rebuild {
            let mut ctx =
                scaling::Context::get(fmt, w, h, Pixel::RGBA, w, h, scaling::Flags::POINT)
                    .context("swscale context")?;
            // swscale defaults to BT.601 coefficients — set them from the FRAME's signaling
            // (unspecified → BT.709 limited, the host's SDR default; a Windows HDR desktop
            // streams BT.2020 in-band). Without this, YUV→RGB decodes with the wrong matrix
            // and colours shift. Destination = full-range RGB; the transfer function stays
            // baked in (the presenter tags PQ textures so GTK applies the EOTF).
            const SWS_CS_ITU709: i32 = 1;
            const SWS_CS_ITU601: i32 = 5;
            const SWS_CS_BT2020: i32 = 9;
            let cs = match color.matrix {
                9 | 10 => SWS_CS_BT2020,
                5 | 6 => SWS_CS_ITU601,
                _ => SWS_CS_ITU709,
            };
            unsafe {
                let coeffs = ffmpeg::ffi::sws_getCoefficients(cs);
                ffmpeg::ffi::sws_setColorspaceDetails(
                    ctx.as_mut_ptr(),
                    coeffs, // inv_table: source (YUV) coefficients per the VUI
                    color.full_range as i32, // srcRange: 0 = limited/studio (MPEG)
                    coeffs, // table: destination coefficients (ignored for RGB output)
                    1,      // dstRange: 1 = full-range RGB
                    0,
                    1 << 16,
                    1 << 16, // brightness, contrast, saturation (defaults)
                );
            }
            self.sws = Some((ctx, fmt, w, h, color));
        }
        let (sws, ..) = self.sws.as_mut().unwrap();
        // Single-pass conversion: swscale writes straight into the Vec the texture will
        // wrap. (The old path scaled into a scratch AVFrame and then copied `data(0)` out
        // — a second full-frame pass per frame.) 64-byte row alignment keeps swscale on
        // aligned SIMD stores; `GdkMemoryTexture` takes the resulting stride explicitly.
        const ALIGN: i32 = 64;
        use ffmpeg::ffi;
        let dst_fmt = ffi::AVPixelFormat::AV_PIX_FMT_RGBA;
        // SAFETY: pure size computation from format/dimensions; no pointers involved.
        let size = unsafe { ffi::av_image_get_buffer_size(dst_fmt, w as i32, h as i32, ALIGN) };
        if size < 0 {
            return Err(averr("av_image_get_buffer_size", size));
        }
        let rgba = vec![0u8; size as usize];
        let mut dst_data: [*mut u8; 4] = [ptr::null_mut(); 4];
        let mut dst_linesize: [i32; 4] = [0; 4];
        // SAFETY: fill_arrays only derives plane pointers/strides into `rgba` (sized by
        // av_image_get_buffer_size above, same format/align) — no allocation, no
        // ownership transfer; `rgba` outlives the scale below.
        let r = unsafe {
            ffi::av_image_fill_arrays(
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
                rgba.as_ptr(),
                dst_fmt,
                w as i32,
                h as i32,
                ALIGN,
            )
        };
        if r < 0 {
            return Err(averr("av_image_fill_arrays", r));
        }
        // SAFETY: src pointers/strides belong to the decoder-owned `frame` (alive for the
        // call); dst pointers were just filled over `rgba`, and sws_scale writes rows
        // [0, h) only — exactly the buffer fill_arrays sized.
        let r = unsafe {
            ffi::sws_scale(
                sws.as_mut_ptr(),
                (*frame.as_ptr()).data.as_ptr() as *const *const u8,
                (*frame.as_ptr()).linesize.as_ptr(),
                0,
                h as i32,
                dst_data.as_ptr(),
                dst_linesize.as_ptr(),
            )
        };
        if r < 0 {
            return Err(averr("sws_scale", r));
        }
        Ok(CpuFrame {
            width: w,
            height: h,
            stride: dst_linesize[0] as usize,
            rgba,
            color,
        })
    }
}

// --- VAAPI backend --------------------------------------------------------------------
//
// Raw FFI: ffmpeg-next has no hwaccel wrappers. All pointers are owned here and freed in
// Drop; decoded surfaces transfer out through DrmFrameGuard.

const AVERROR_EAGAIN: i32 = -11; // -EAGAIN; Linux-only crate

fn averr(what: &str, code: i32) -> anyhow::Error {
    anyhow!("{what}: {}", ffmpeg::Error::from(code))
}

/// libavcodec offers the formats it can decode into; pick the VAAPI hw surface. Falling
/// back to the first (software) entry would silently decode on the CPU *and* break our
/// dmabuf mapping — return NONE instead so the error surfaces and the session demotes
/// to the software backend explicitly.
unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    unsafe {
        while *list != ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            list = list.add(1);
        }
    }
    ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

struct VaapiDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    hw_device: *mut ffmpeg::ffi::AVBufferRef,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
}

// Single-owner pointers, only touched from the session pump thread.
unsafe impl Send for VaapiDecoder {}

impl VaapiDecoder {
    fn new(codec_id: ffmpeg::codec::Id) -> Result<VaapiDecoder> {
        use ffmpeg::ffi;
        unsafe {
            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let r = ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if r < 0 {
                bail!("no VAAPI device ({})", ffmpeg::Error::from(r));
            }
            // The negotiated codec's decoder id (av_codec_id maps 1:1 from ffmpeg::codec::Id).
            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(pick_vaapi);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // hwaccel: threads only add latency

            // The presenter holds mapped surfaces PAST receive_frame (the paintable's
            // current texture + the newest frame in flight each pin one until GDK's
            // release func) — surfaces libavcodec doesn't know are missing from its
            // fixed-size VAAPI pool. Without headroom the decoder can recycle a surface
            // the renderer is still sampling (intermittent block corruption) or fail
            // allocation under scheduling jitter.
            (*ctx).extra_hw_frames = 4;
            let r = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                ffi::avcodec_free_context(&mut ctx);
                let mut hw_device = hw_device;
                ffi::av_buffer_unref(&mut hw_device);
                bail!("avcodec_open2: {}", ffmpeg::Error::from(r));
            }
            Ok(VaapiDecoder {
                ctx,
                hw_device,
                packet: ffi::av_packet_alloc(),
                frame: ffi::av_frame_alloc(),
            })
        }
    }

    fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        use ffmpeg::ffi;
        unsafe {
            let r = ffi::av_new_packet(self.packet, au.len() as i32);
            if r < 0 {
                return Err(averr("av_new_packet", r));
            }
            ptr::copy_nonoverlapping(au.as_ptr(), (*self.packet).data, au.len());
            let r = ffi::avcodec_send_packet(self.ctx, self.packet);
            ffi::av_packet_unref(self.packet);
            if r < 0 {
                return Err(averr("send_packet", r));
            }
            let mut out = None;
            loop {
                let r = ffi::avcodec_receive_frame(self.ctx, self.frame);
                if r == AVERROR_EAGAIN {
                    break;
                }
                if r < 0 {
                    return Err(averr("receive_frame", r));
                }
                out = Some(self.map_dmabuf()?); // newest wins; older guards drop here
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// Map the VAAPI surface to DRM PRIME (zero copy) and lift the descriptor into a
    /// `DmabufFrame`. The mapped frame keeps the surface alive via its buffer refs.
    ///
    /// FFmpeg's VAAPI export uses `VA_EXPORT_SURFACE_SEPARATE_LAYERS`, so an NV12 surface
    /// comes back as TWO layers (`R8` luma + `GR88` chroma), each one plane — NOT a single
    /// `NV12` layer. The previous code took `layers[0]` only: GTK then saw an `R8`
    /// single-plane texture with the chroma dropped, painting the screen green. The fix:
    /// derive the COMBINED fourcc from the decoder's software pixel format (NV12 →
    /// `DRM_FORMAT_NV12`) and flatten every plane across every layer in order (Y then UV).
    unsafe fn map_dmabuf(&mut self) -> Result<DmabufFrame> {
        use ffmpeg::ffi;
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                bail!("decoder returned a software frame (no VAAPI surface)");
            }
            // The real pixel layout lives on the hardware frames context, not the
            // DRM-PRIME layer formats (those are the per-plane R8/GR88 component formats).
            let sw_format = {
                let hwfc = (*self.frame).hw_frames_ctx;
                if hwfc.is_null() {
                    bail!("VAAPI frame without a hardware frames context");
                }
                (*((*hwfc).data as *const ffi::AVHWFramesContext)).sw_format
            };
            let fourcc = drm_fourcc_for(sw_format)
                .ok_or_else(|| anyhow!("unsupported VAAPI output format {sw_format:?}"))?;

            let drm = ffi::av_frame_alloc();
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = ffi::av_hwframe_map(drm, self.frame, ffi::AV_HWFRAME_MAP_READ as i32);
            if r < 0 {
                let mut drm = drm;
                ffi::av_frame_free(&mut drm);
                return Err(averr("av_hwframe_map", r));
            }
            let desc = (*drm).data[0] as *const ffi::AVDRMFrameDescriptor;
            let guard = DrmFrameGuard(drm);
            let d = &*desc;
            if d.nb_layers < 1 || d.nb_objects < 1 {
                bail!("DRM descriptor without layers/objects");
            }

            // Flatten planes across ALL layers, in declared order — the combined fourcc's
            // plane order (Y, then UV for NV12) matches the layer order FFmpeg emits.
            let mut planes = Vec::new();
            for layer in &d.layers[..d.nb_layers as usize] {
                for p in &layer.planes[..layer.nb_planes as usize] {
                    let obj = &d.objects[p.object_index as usize];
                    planes.push(DmabufPlane {
                        fd: obj.fd,
                        offset: p.offset as u32,
                        stride: p.pitch as u32,
                    });
                }
            }

            // The whole surface shares one tiling modifier (one BO on radeonsi); GTK takes
            // a single modifier for the texture.
            let modifier = d.objects[0].format_modifier;

            log_descriptor_once(d, sw_format, fourcc, modifier);

            Ok(DmabufFrame {
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                fourcc,
                modifier,
                planes,
                // SAFETY: `self.frame` is the live decoded AVFrame (unref'd only after
                // this returns); plain CICP field reads.
                color: ColorDesc::from_raw(self.frame),
                guard,
            })
        }
    }
}

/// `fourcc(a,b,c,d)` — the DRM FourCC packing (little-endian, `a | b<<8 | c<<16 | d<<24`).
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// The combined DRM FourCC for a decoder software pixel format. The host streams 8-bit
/// 4:2:0 (NV12); P010 is here for the eventual 10-bit/HDR path.
fn drm_fourcc_for(sw: ffmpeg_next::ffi::AVPixelFormat) -> Option<u32> {
    use ffmpeg_next::ffi::AVPixelFormat::*;
    Some(match sw {
        AV_PIX_FMT_NV12 => fourcc(b'N', b'V', b'1', b'2'),
        AV_PIX_FMT_P010LE => fourcc(b'P', b'0', b'1', b'0'),
        _ => return None,
    })
}

/// One-time dump of the DRM descriptor layout (objects, layers, planes, modifier) — so a
/// new client/driver combination's real layout is visible in the logs without a debugger.
fn log_descriptor_once(
    d: &ffmpeg_next::ffi::AVDRMFrameDescriptor,
    sw: ffmpeg_next::ffi::AVPixelFormat,
    fourcc: u32,
    modifier: u64,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if !ONCE.swap(false, Ordering::Relaxed) {
        return;
    }
    let layers: Vec<(u32, i32)> = d.layers[..d.nb_layers.max(0) as usize]
        .iter()
        .map(|l| (l.format, l.nb_planes))
        .collect();
    tracing::info!(
        sw_format = ?sw,
        chosen_fourcc = format_args!("{:#010x}", fourcc),
        nb_objects = d.nb_objects,
        nb_layers = d.nb_layers,
        ?layers,
        modifier = format_args!("{:#018x}", modifier),
        "VAAPI dmabuf descriptor layout (first frame)"
    );
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock the DRM FourCC magic numbers against typos — these are the exact values
    /// `<drm_fourcc.h>` defines, and a wrong one is what painted the Steam Deck green.
    #[test]
    fn drm_fourcc_constants() {
        assert_eq!(fourcc(b'N', b'V', b'1', b'2'), 0x3231_564e);
        assert_eq!(fourcc(b'P', b'0', b'1', b'0'), 0x3031_3050);
        assert_eq!(
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12),
            Some(0x3231_564e)
        );
        assert_eq!(
            drm_fourcc_for(ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_RGBA),
            None
        );
    }

    /// The wire → `ColorDesc` plumbing: an HDR10 stream's VUI (BT.2020 primaries, PQ
    /// transfer, BT.2020-NCL matrix, limited range) must arrive on the decoded frame —
    /// this is what the Windows host emits in-band for an HDR desktop, and mis-rendering
    /// it as BT.709 is the washed-out-colors bug. Fixture: one 64×64 Main10 IDR
    /// (`tests/pq-frame.h265`, x265 with explicit VUI).
    #[test]
    fn software_decode_carries_pq_signaling() {
        let au = include_bytes!("../tests/pq-frame.h265");
        let mut dec = SoftwareDecoder::new(ffmpeg::codec::Id::HEVC).expect("hevc decoder");
        let mut got = dec.decode(au).expect("decode");
        if got.is_none() {
            // Low-delay decoders may still hold the frame until a flush — send EOF.
            dec.decoder.send_eof().ok();
            let mut frame = AvFrame::empty();
            if dec.decoder.receive_frame(&mut frame).is_ok() {
                got = Some(dec.convert_rgba(&frame).expect("convert"));
            }
        }
        let f = got.expect("no frame decoded from the PQ fixture");
        assert_eq!(
            f.color,
            ColorDesc {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false
            }
        );
        assert!(f.color.is_pq());
        assert_eq!((f.width, f.height), (64, 64));
    }
}
