//! FFmpeg Vulkan Video decode over the presenter's own VkDevice (zero-copy VkImage).
#![allow(clippy::unnecessary_cast)]

use crate::video::{
    averr, frame_is_keyframe, DrmFrameGuard, QueueLock, VkVideoFrame, VulkanDecodeDevice,
    AVERROR_EAGAIN,
};
use crate::video_color::ColorDesc;
use crate::video_libav::AvBuffer;
use anyhow::{bail, Context, Result};
use ffmpeg_next as ffmpeg;
use std::ptr;

// --- Vulkan Video backend -------------------------------------------------------------

/// FFmpeg's Vulkan Video decoder over the PRESENTER's device: the hwdevice context is
/// built from [`VulkanDecodeDevice`]'s handles (not `av_hwdevice_ctx_create`, which
/// would make FFmpeg create its own device the presenter can't sample from). Output
/// frames are `AVVkFrame`s whose VkImage the presenter feeds straight to its CSC pass.
pub(crate) struct VulkanDecoder {
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    /// The Vulkan hwdevice, owned. Nothing reads this field after construction — the codec context
    /// took its own ref via `av_buffer_ref` — it exists so the device outlives the decoder and is
    /// unref'd exactly once when it drops. Declared after `ctx` so it still releases AFTER the
    /// `Drop` below frees packet/frame/context, which is the order the hand-written unref had.
    /// `dead_code` is answered here rather than by removing the field (that would free the device
    /// early) or by an underscore name (that would hide what it is).
    #[allow(dead_code)]
    hw_device: AvBuffer,
    packet: *mut ffmpeg::ffi::AVPacket,
    frame: *mut ffmpeg::ffi::AVFrame,
    /// `vkWaitSemaphores` on the shared device — the decode-complete measurement
    /// (resolved through the same get_proc_addr chain FFmpeg uses).
    wait_semaphores: pf_ffvk::PFN_vkWaitSemaphores,
    vk_device: pf_ffvk::VkDevice,
    /// Storage `AVVulkanDeviceContext` points into (extension string arrays + the
    /// feature chain) — FFmpeg reads the extension lists past init (frames-context
    /// setup keys code paths off them), so this lives exactly as long as `hw_device`.
    _ctx_storage: Box<VkCtxStorage>,
}

// SAFETY: `ctx`/`packet`/`frame` are allocations this decoder owns from its constructor to `Drop`,
// `hw_device` is an owning `AvBuffer` (atomic refcount), and `_ctx_storage` is a `Box` that merely
// has to outlive them. `Send` only moves that ownership between threads, which libav permits for a
// codec context used serially — `&mut self` on every method provides that. The presenter reaches
// decoded images through the `AVFrame` guard's own references and the shared `QueueLock`, not
// through this struct. Deliberately NOT `Sync`.
unsafe impl Send for VulkanDecoder {}

struct VkCtxStorage {
    _inst: Vec<std::ffi::CString>,
    inst_ptrs: Vec<*const std::os::raw::c_char>,
    _dev: Vec<std::ffi::CString>,
    dev_ptrs: Vec<*const std::os::raw::c_char>,
    f11: pf_ffvk::VkPhysicalDeviceVulkan11Features,
    f12: pf_ffvk::VkPhysicalDeviceVulkan12Features,
    f13: pf_ffvk::VkPhysicalDeviceVulkan13Features,
    /// Keeps the shared queue lock alive for `AVHWDeviceContext.user_opaque` — the
    /// `lock_queue`/`unlock_queue` trampolines below dereference it for as long as the
    /// hw device context can fire them.
    _queue_lock: std::sync::Arc<QueueLock>,
}

/// FFmpeg `AVVulkanDeviceContext.lock_queue` trampoline: take the device's shared
/// [`QueueLock`] (stashed in `AVHWDeviceContext.user_opaque`; owned by
/// [`VkCtxStorage`], which outlives the context). Replaces FFmpeg's internal default,
/// which only serializes FFmpeg against itself — the presenter submits to the same
/// graphics queue from another thread and holds this same lock around its calls.
///
/// # Safety
/// FFmpeg calls this with the `AVHWDeviceContext` it owns, whose `user_opaque` we set to a
/// `*const QueueLock` before handing the context over.
unsafe extern "C" fn ffvk_lock_queue(
    ctx: *mut pf_ffvk::AVHWDeviceContext,
    _queue_family: u32,
    _index: u32,
) {
    // SAFETY: `ctx` is the live context FFmpeg passes to its own callback, and the two
    // `AVHWDeviceContext` declarations (pf_ffvk's and ffmpeg-sys's) describe the same C struct, so
    // the cast reads the same `user_opaque` field. That field holds the pointer we stored, which
    // borrows `VkCtxStorage::_queue_lock` — an `Arc<QueueLock>` the storage keeps alive for as long
    // as the hw device context can fire this trampoline (see its field doc), so the lock outlives
    // every call FFmpeg can make.
    unsafe {
        let dev = ctx as *mut ffmpeg::ffi::AVHWDeviceContext;
        let lock = (*dev).user_opaque as *const QueueLock;
        (*lock).lock();
    }
}

/// The matching `unlock_queue` trampoline — see [`ffvk_lock_queue`].
///
/// # Safety
/// As [`ffvk_lock_queue`]; additionally, FFmpeg only calls this after a matching `lock_queue`, so
/// the lock it releases is one this pair took.
unsafe extern "C" fn ffvk_unlock_queue(
    ctx: *mut pf_ffvk::AVHWDeviceContext,
    _queue_family: u32,
    _index: u32,
) {
    // SAFETY: as `ffvk_lock_queue` — same live context from FFmpeg, same `user_opaque` pointer into
    // the `Arc<QueueLock>` that `VkCtxStorage` keeps alive for the context's whole lifetime.
    unsafe {
        let dev = ctx as *mut ffmpeg::ffi::AVHWDeviceContext;
        let lock = (*dev).user_opaque as *const QueueLock;
        (*lock).unlock();
    }
}

impl VulkanDecoder {
    pub(crate) fn new(
        codec_id: ffmpeg::codec::Id,
        vk: &VulkanDecodeDevice,
    ) -> Result<VulkanDecoder> {
        use ffmpeg::ffi;
        // SAFETY: a self-contained builder — every allocation is made here and null-checked before
        // use, the `AVVulkanDeviceContext` fields are filled from `vk`'s live handles and from
        // `_ctx_storage`, which the decoder keeps alive alongside the context, and what survives is
        // moved into the returned `VulkanDecoder`, which frees each exactly once in `Drop`.
        unsafe {
            let mut hw_device =
                ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN);
            if hw_device.is_null() {
                bail!("av_hwdevice_ctx_alloc(VULKAN) failed (FFmpeg built without Vulkan?)");
            }
            let devctx = (*hw_device).data as *mut ffi::AVHWDeviceContext;
            let hwctx = (*devctx).hwctx as *mut pf_ffvk::AVVulkanDeviceContext;

            // Pinned storage for everything the context points into.
            let mut store = Box::new(VkCtxStorage {
                _inst: vk.instance_extensions.clone(),
                inst_ptrs: Vec::new(),
                _dev: vk.device_extensions.clone(),
                dev_ptrs: Vec::new(),
                f11: std::mem::zeroed(),
                f12: std::mem::zeroed(),
                f13: std::mem::zeroed(),
                _queue_lock: vk.queue_lock.clone(),
            });
            store.inst_ptrs = store._inst.iter().map(|c| c.as_ptr()).collect();
            store.dev_ptrs = store._dev.iter().map(|c| c.as_ptr()).collect();
            // The features enabled at device creation, as the 1.1/1.2/1.3 chain FFmpeg
            // walks to learn what it may use (sType values are vulkan.h constants).
            store.f11.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_1_FEATURES;
            store.f11.samplerYcbcrConversion = vk.f_sampler_ycbcr as u32;
            store.f12.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES;
            store.f12.timelineSemaphore = vk.f_timeline_semaphore as u32;
            store.f13.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES;
            store.f13.synchronization2 = vk.f_synchronization2 as u32;
            store.f11.pNext = &mut store.f12 as *mut _ as *mut std::ffi::c_void;
            store.f12.pNext = &mut store.f13 as *mut _ as *mut std::ffi::c_void;

            (*hwctx).get_proc_addr = std::mem::transmute::<usize, pf_ffvk::PFN_vkGetInstanceProcAddr>(
                vk.get_instance_proc_addr,
            );
            (*hwctx).inst = vk.instance as pf_ffvk::VkInstance;
            (*hwctx).phys_dev = vk.physical_device as pf_ffvk::VkPhysicalDevice;
            (*hwctx).act_dev = vk.device as pf_ffvk::VkDevice;
            (*hwctx).device_features.sType =
                pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
            (*hwctx).device_features.pNext = &mut store.f11 as *mut _ as *mut std::ffi::c_void;
            (*hwctx).enabled_inst_extensions = store.inst_ptrs.as_ptr();
            (*hwctx).nb_enabled_inst_extensions = store.inst_ptrs.len() as i32;
            (*hwctx).enabled_dev_extensions = store.dev_ptrs.as_ptr();
            (*hwctx).nb_enabled_dev_extensions = store.dev_ptrs.len() as i32;

            // Queue map: the deprecated per-role indices (tx/comp are "Required") plus
            // the qf[] list, which per the header must also carry every family named
            // above. One merged entry when decode shares the graphics family.
            let g = vk.graphics_qf as i32;
            let d = vk.decode_qf as i32;
            (*hwctx).queue_family_index = g;
            (*hwctx).nb_graphics_queues = 1;
            (*hwctx).queue_family_tx_index = g;
            (*hwctx).nb_tx_queues = 1;
            (*hwctx).queue_family_comp_index = g;
            (*hwctx).nb_comp_queues = 1;
            (*hwctx).queue_family_encode_index = -1;
            (*hwctx).nb_encode_queues = 0;
            (*hwctx).queue_family_decode_index = d;
            (*hwctx).nb_decode_queues = 1;
            const VIDEO_DECODE_BIT: u32 = 0x20; // VK_QUEUE_VIDEO_DECODE_BIT_KHR
                                                // `flags`/`video_caps` are bindgen enum types: i32 under MSVC, u32 under
                                                // Linux clang — the `as _` casts absorb the difference.
            if g == d {
                (*hwctx).qf[0] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: g,
                    num: 1,
                    flags: (vk.graphics_queue_flags | VIDEO_DECODE_BIT) as _,
                    video_caps: vk.decode_video_caps as _,
                };
                (*hwctx).nb_qf = 1;
            } else {
                (*hwctx).qf[0] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: g,
                    num: 1,
                    flags: vk.graphics_queue_flags as _,
                    video_caps: 0,
                };
                (*hwctx).qf[1] = pf_ffvk::AVVulkanDeviceQueueFamily {
                    idx: d,
                    num: 1,
                    flags: VIDEO_DECODE_BIT as _,
                    video_caps: vk.decode_video_caps as _,
                };
                (*hwctx).nb_qf = 2;
            }

            // Shared-queue external sync (see [`QueueLock`]): FFmpeg must take the
            // same lock the presenter holds around its own submits/presents — set
            // BEFORE init so FFmpeg never installs its internal defaults (which only
            // serialize FFmpeg against itself; the cross-thread race with the
            // presenter's queue was an intermittent VK_ERROR_DEVICE_LOST).
            (*devctx).user_opaque =
                std::sync::Arc::as_ptr(&store._queue_lock) as *mut std::ffi::c_void;
            (*hwctx).lock_queue = Some(ffvk_lock_queue);
            (*hwctx).unlock_queue = Some(ffvk_unlock_queue);

            let r = ffi::av_hwdevice_ctx_init(hw_device);
            if r < 0 {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(averr("av_hwdevice_ctx_init(VULKAN)", r));
            }
            // Owned from here: every failure path below drops it instead of unref'ing by hand.
            let hw_device = AvBuffer::from_raw(hw_device)
                .context("av_hwdevice_ctx_alloc(VULKAN) gave no device")?;

            // vkWaitSemaphores for the pump's decode-complete stat: loader →
            // vkGetDeviceProcAddr → device fn (core 1.2, guaranteed by our gate).
            let gipa = (*hwctx)
                .get_proc_addr
                .expect("get_proc_addr was just set above");
            let gdpa: pf_ffvk::PFN_vkGetDeviceProcAddr =
                std::mem::transmute(gipa((*hwctx).inst, c"vkGetDeviceProcAddr".as_ptr()));
            let wait_semaphores: pf_ffvk::PFN_vkWaitSemaphores = std::mem::transmute(gdpa
                .expect("vkGetDeviceProcAddr resolvable")(
                (*hwctx).act_dev,
                c"vkWaitSemaphores".as_ptr(),
            ));
            if wait_semaphores.is_none() {
                bail!("vkWaitSemaphores unresolvable on this device");
            }
            let vk_device = (*hwctx).act_dev;

            let codec = ffi::avcodec_find_decoder(codec_id.into());
            if codec.is_null() {
                bail!("no {codec_id:?} decoder");
            }
            let ctx = ffi::avcodec_alloc_context3(codec);
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device.as_ptr());
            (*ctx).get_format = Some(pick_vulkan);
            (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1; // hwaccel: threads only add latency
                                     // Same pool headroom rationale as VAAPI: the presenter pins the on-screen
                                     // frame + the newest in flight past receive_frame.
            (*ctx).extra_hw_frames = 4;
            let r = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if r < 0 {
                let mut ctx = ctx;
                ffi::avcodec_free_context(&mut ctx);
                return Err(averr("avcodec_open2 (vulkan)", r));
            }
            Ok(VulkanDecoder {
                ctx,
                hw_device,
                packet: ffi::av_packet_alloc(),
                frame: ffi::av_frame_alloc(),
                wait_semaphores,
                vk_device,
                _ctx_storage: store,
            })
        }
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<VkVideoFrame>> {
        use ffmpeg::ffi;
        // SAFETY: `packet`/`frame`/`ctx` are this decoder's own allocations, live for its whole
        // lifetime; `au` outlives the synchronous `send_packet` that copies out of it, and every
        // libav return is checked before the result is used.
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
                out = Some(self.extract()?); // newest wins; older guards drop here
                ffi::av_frame_unref(self.frame);
            }
            Ok(out)
        }
    }

    /// Block until the timeline semaphore reaches `value` (GPU decode complete) or the
    /// timeout passes. Pure measurement — the presenter's own GPU wait is what gates
    /// sampling, so a timeout here only degrades the stat, never the picture.
    pub(crate) fn wait_timeline(&self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        let sems = [sem as pf_ffvk::VkSemaphore];
        let values = [value];
        let info = pf_ffvk::VkSemaphoreWaitInfo {
            sType: pf_ffvk::VkStructureType_VK_STRUCTURE_TYPE_SEMAPHORE_WAIT_INFO,
            pNext: std::ptr::null(),
            flags: 0,
            semaphoreCount: 1,
            pSemaphores: sems.as_ptr(),
            pValues: values.as_ptr(),
        };
        // SAFETY: resolved from this device at init; handles outlive the decoder.
        let r = unsafe {
            self.wait_semaphores.expect("checked at init")(self.vk_device, &info, timeout_ns)
        };
        r == 0 // VK_SUCCESS (VK_TIMEOUT = 2)
    }

    /// Lift the decoded `AVVkFrame` into a [`VkVideoFrame`]: clone the AVFrame (the
    /// guard — keeps the image + frames context alive through present) and ship the
    /// POINTERS; the presenter reads the live sync state under the frames-context lock
    /// at its own submit time.
    fn extract(&mut self) -> Result<VkVideoFrame> {
        use ffmpeg::ffi;
        // SAFETY: `self.frame` is this decoder's own `AVFrame`; the format check below is what
        // proves it carries an `AVVkFrame` before anything reads the Vulkan image out of it, and
        // the clone handed onward keeps the image + frames context alive through present.
        unsafe {
            if (*self.frame).format != ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32 {
                bail!("decoder returned a non-Vulkan frame");
            }
            let hwfc_ref = (*self.frame).hw_frames_ctx;
            if hwfc_ref.is_null() {
                bail!("Vulkan frame without a hardware frames context");
            }
            let fc = (*hwfc_ref).data as *mut ffi::AVHWFramesContext;
            let sw = (*fc).sw_format;
            // The 2-plane layouts the presenter's CSC can sample: 4:2:0 (NV12/P010) and
            // full-chroma 4:4:4 (NV24/P410 — HEVC RExt decode, semi-planar like all
            // NVDEC output). The presenter's `vkframe_plane_formats` table is the final
            // authority; anything else bails here so the session demotes cleanly.
            if sw != ffi::AVPixelFormat::AV_PIX_FMT_NV12
                && sw != ffi::AVPixelFormat::AV_PIX_FMT_P010LE
                && sw != ffi::AVPixelFormat::AV_PIX_FMT_NV24
                && sw != ffi::AVPixelFormat::AV_PIX_FMT_P410LE
            {
                bail!("Vulkan decode output {sw:?} unsupported (NV12/P010/NV24/P410 only)");
            }
            let vkfc = (*fc).hwctx as *const pf_ffvk::AVVulkanFramesContext;
            let vk_format = (*vkfc).format[0] as i32;
            let lock_frame = (*vkfc).lock_frame.map_or(0, |f| f as usize);
            let unlock_frame = (*vkfc).unlock_frame.map_or(0, |f| f as usize);
            if lock_frame == 0 || unlock_frame == 0 {
                bail!("Vulkan frames context without lock functions");
            }

            let clone = ffi::av_frame_clone(self.frame);
            if clone.is_null() {
                bail!("av_frame_clone failed");
            }
            let vkf = (*clone).data[0] as *mut pf_ffvk::AVVkFrame;
            // v1 handles the (default) single multiplanar image; a disjoint/multi-image
            // pool would need per-plane images — bail so the session demotes cleanly.
            if !(*vkf).img[1].is_null() {
                let mut clone = clone;
                ffi::av_frame_free(&mut clone);
                bail!("multi-image Vulkan frames unsupported (disjoint pool)");
            }
            // Safe without the frames lock: the handle is creation-constant and
            // sem_value was last written by the decode submission on THIS thread.
            let timeline_sem = (*vkf).sem[0] as u64;
            let decode_done_value = (*vkf).sem_value[0];
            Ok(VkVideoFrame {
                vkframe: vkf as usize,
                frames_ctx: fc as usize,
                lock_frame,
                unlock_frame,
                vk_format,
                timeline_sem,
                decode_done_value,
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
                color: ColorDesc::from_raw(self.frame),
                keyframe: frame_is_keyframe(self.frame),
                guard: DrmFrameGuard(clone),
            })
        }
    }
}

impl Drop for VulkanDecoder {
    fn drop(&mut self) {
        use ffmpeg::ffi;
        // SAFETY: each pointer is this decoder's own allocation and nothing else holds it; `Drop`
        // runs exactly once, and each free nulls the pointer through its `&mut`, so none can be
        // released twice. Freed packet-then-frame-then-context, the order libav documents.
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::av_frame_free(&mut self.frame);
            ffi::avcodec_free_context(&mut self.ctx);
            // `hw_device` is an `AvBuffer` and unrefs itself when the field drops, right after this.
        }
    }
}

/// libavcodec offers the formats it can decode into; pick the Vulkan hw surface and
/// hand the decoder OUR frames context — the default one lacks
/// `VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT`, without which the presenter can't create the
/// per-plane views its CSC pass samples. Returning NONE (over the software entry) keeps
/// failures loud: the session demotes explicitly instead of silently CPU-decoding.
unsafe extern "C" fn pick_vulkan(
    ctx: *mut ffmpeg::ffi::AVCodecContext,
    mut list: *const ffmpeg::ffi::AVPixelFormat,
) -> ffmpeg::ffi::AVPixelFormat {
    use ffmpeg::ffi;
    // SAFETY: libav calls this `get_format` callback with a list it owns, terminated by
    // `AV_PIX_FMT_NONE` — the walk stops at that terminator, so it stays inside the array, and it
    // only reads.
    unsafe {
        let mut offered = false;
        while *list != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *list == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN {
                offered = true;
                break;
            }
            list = list.add(1);
        }
        if !offered {
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        let mut fr: *mut ffi::AVBufferRef = ptr::null_mut();
        let r = ffi::avcodec_get_hw_frames_parameters(
            ctx,
            (*ctx).hw_device_ctx,
            ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
            &mut fr,
        );
        if r < 0 || fr.is_null() {
            tracing::warn!(code = r, "avcodec_get_hw_frames_parameters(VULKAN) failed");
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        // Owned until the codec takes it at the bottom: the init-failure path below just returns
        // and the drop releases it.
        let Some(fr) = AvBuffer::from_raw(fr) else {
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        };
        let fc = (*fr.as_ptr()).data as *mut ffi::AVHWFramesContext;
        let vkfc = (*fc).hwctx as *mut pf_ffvk::AVVulkanFramesContext;
        // MUTABLE_FORMAT: per-plane views (spec requirement); ALIAS is FFmpeg's default.
        // (`as _`: the FlagBits constants are i32 under MSVC, the img_flags field u32.)
        (*vkfc).img_flags = (pf_ffvk::VkImageCreateFlagBits_VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT
            | pf_ffvk::VkImageCreateFlagBits_VK_IMAGE_CREATE_ALIAS_BIT)
            as _;
        let r = ffi::av_hwframe_ctx_init(fr.as_ptr());
        if r < 0 {
            tracing::warn!(code = r, "av_hwframe_ctx_init(VULKAN) failed");
            return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
        }
        if !(*ctx).hw_frames_ctx.is_null() {
            ffi::av_buffer_unref(&mut (*ctx).hw_frames_ctx);
        }
        // Ownership TRANSFERS to the codec here, so hand over the raw pointer and forget the
        // wrapper — dropping it as well would be the double-unref `AvBuffer` exists to prevent.
        (*ctx).hw_frames_ctx = fr.into_raw();
        ffi::AVPixelFormat::AV_PIX_FMT_VULKAN
    }
}
