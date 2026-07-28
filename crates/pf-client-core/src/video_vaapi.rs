//! VAAPI (libavcodec hwaccel) decode backend → DRM-PRIME dmabuf for the presenter. Linux-only.

use crate::video::{
    averr, drm_fourcc_for, frame_is_keyframe, DmabufFrame, DmabufPlane, DrmFrameGuard,
    AVERROR_EAGAIN,
};
use crate::video_color::ColorDesc;
use crate::video_libav::AvBuffer;
use anyhow::{anyhow, bail, Context, Result};
use ffmpeg_next as ffmpeg;
use std::ptr;

/// libavcodec offers the formats it can decode into; pick the VAAPI hw surface. Falling
/// back to the first (software) entry would silently decode on the CPU *and* break our
/// dmabuf mapping — return NONE instead so the error surfaces and the session demotes
/// to the software backend explicitly.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub(crate) struct VaapiDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    /// The VAAPI hwdevice, owned. Nothing reads this field after construction — the codec context
    /// took its own ref via `av_buffer_ref` — it exists so the device outlives the decoder and is
    /// unref'd exactly once when it drops. Declared after `ctx` so it still releases AFTER the
    /// `Drop` below frees packet/frame/context, which is the order the hand-written unref had.
    /// `dead_code` is answered here rather than by removing the field (that would free the device
    /// early) or by an underscore name (that would hide what it is).
    #[allow(dead_code)]
    hw_device: AvBuffer,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
}

// Single-owner pointers, only touched from the session pump thread.
#[cfg(target_os = "linux")]
unsafe impl Send for VaapiDecoder {}

#[cfg(target_os = "linux")]
impl VaapiDecoder {
    pub(crate) fn new(codec_id: ffmpeg::codec::Id) -> Result<VaapiDecoder> {
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
            // Owned from here: every `bail!` below drops it, so none of them unref by hand.
            let hw_device = AvBuffer::from_raw(hw_device)
                .context("av_hwdevice_ctx_create(VAAPI) gave no device")?;
            // The negotiated codec's decoder id (av_codec_id maps 1:1 from ffmpeg::codec::Id).
            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device.as_ptr());
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

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
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
    fn map_dmabuf(&mut self) -> Result<DmabufFrame> {
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
                keyframe: frame_is_keyframe(self.frame),
                guard,
            })
        }
    }
}

/// One-time dump of the DRM descriptor layout (objects, layers, planes, modifier) — so a
/// new client/driver combination's real layout is visible in the logs without a debugger.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            // `hw_device` is an `AvBuffer` and unrefs itself when the field drops, right after this.
        }
    }
}
