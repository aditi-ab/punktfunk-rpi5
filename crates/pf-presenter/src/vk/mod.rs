//! The Vulkan presenter: swapchain + two frame paths into one device-local RGBA video
//! image, then a letterboxed `vkCmdBlitImage` composite.
//!
//! * **Software** (`FrameInput::Cpu`): staging upload + `copy_buffer_to_image` (row
//!   stride via `buffer_row_length`) — transfer-only, runs on every GPU.
//! * **Hardware** (`FrameInput::Dmabuf`): the decoder's NV12 dmabuf imported per-plane
//!   (`dmabuf.rs`) and converted by the CSC render pass (`csc.rs`) — zero-copy, gated on
//!   the four import extensions at device creation; boxes without them (NVIDIA
//!   proprietary by design) report `supports_dmabuf() == false` and the caller keeps the
//!   decoder on software.
//!
//! Pacing: one frame in flight (the submit fence is waited before each record), MAILBOX
//! when available, FIFO otherwise (`PUNKTFUNK_PRESENT_MODE=fifo|mailbox|immediate`
//! overrides — see `pick_present_mode` for why an arrival-paced presenter must not
//! block in FIFO's present queue). Present is arrival-paced by the caller: a frame
//! input on each decoded frame, `FrameInput::Redraw` re-blits the retained video image
//! (expose/resize redraws).

use crate::csc::CscPass;
#[cfg(target_os = "linux")]
use crate::dmabuf::HwFrame;
use crate::overlay::SharedDevice;
use ash::vk;
#[cfg(target_os = "linux")]
use pf_client_core::video::DmabufFrame;
use pf_client_core::video::{CpuFrame, VkVideoFrame};

mod gpu;
mod overlay_pipe;
mod present;
mod present_timing;
mod reconfig;
mod resources;
mod setup;

pub use setup::list_adapters;

/// One presenter iteration's video input.
pub enum FrameInput<'a> {
    /// No new frame — re-composite the retained video image (expose/resize).
    Redraw,
    Cpu(&'a CpuFrame),
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
    /// FFmpeg Vulkan Video output — a VkImage already on THIS device (zero copy).
    VkFrame(VkVideoFrame),
    /// D3D11VA hand-off — a shareable NT-handle texture to import (`d3d11.rs`).
    #[cfg(windows)]
    D3d11(pf_client_core::video::D3d11Frame),
    /// PyroWave planar output — three R8 plane views already on THIS device, decode
    /// fence-complete, GENERAL layout (`pf_client_core::video_pyrowave`).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(pf_client_core::video_pyrowave::PyroWavePlanarFrame),
}

/// The dmabuf/CSC machinery, present only when the device carries the import extensions.
#[cfg(target_os = "linux")]
struct HwCtx {
    ext_mem_fd: ash::khr::external_memory_fd::Device,
}

/// The D3D11 shared-texture import machinery, present only when the device carries
/// `VK_KHR_external_memory_win32` + `VK_KHR_win32_keyed_mutex`.
#[cfg(windows)]
struct HwCtxWin {
    ext_mem_win32: ash::khr::external_memory_win32::Device,
}

/// A submitted hardware frame parked until the in-flight fence proves the GPU reads
/// done: imported dmabuf planes, or a Vulkan-Video frame (FFmpeg's image — we own only
/// the plane views; dropping the frame's guard releases the AVFrame back to the pool).
enum Retired {
    #[cfg(target_os = "linux")]
    Dmabuf(HwFrame),
    #[cfg(windows)]
    D3d11(crate::d3d11::HwFrame),
    Vk {
        frame: VkVideoFrame,
        views: [vk::ImageView; 2],
    },
}

/// The overlay composite: one premultiplied-alpha quad blended over the swapchain image
/// after the video blit (the §6.1 contract's presenter half). Always built — it has no
/// Skia dependency and costs nothing while no overlay frame arrives (the render pass
/// isn't even recorded).
struct OverlayPipe {
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_pool: vk::DescriptorPool,
    desc_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    /// Per-swapchain-image render targets, rebuilt with the swapchain.
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
}

/// The one video image (device-local RGBA the size of the decoded stream) + its staging.
/// `view`/`framebuffer` exist only on hw-capable devices (the CSC pass renders into it).
struct VideoImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
}

struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    capacity: usize,
}

pub struct Presenter {
    // Field order = drop order documentation only; teardown is explicit in `Drop`.
    entry: ash::Entry,
    instance: ash::Instance,
    surface_i: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    device: ash::Device,
    swap_d: ash::khr::swapchain::Device,
    queue: vk::Queue,
    qfi: u32,
    /// Dmabuf import — `None` when the device lacks the import extensions (the CSC
    /// pass itself is unconditional: Vulkan-Video frames need it everywhere).
    #[cfg(target_os = "linux")]
    hw: Option<HwCtx>,
    /// D3D11 shared-texture import — `None` when the device lacks the win32 external
    /// memory / keyed-mutex extensions.
    #[cfg(windows)]
    hw_win: Option<HwCtxWin>,
    csc: CscPass,
    /// The planar (3-plane) CSC variant for PyroWave frames; built only when the device
    /// passed the pyrowave probe.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    csc_planar: Option<CscPass>,
    /// FFmpeg Vulkan Video decode handles — `None` when the stack can't do it.
    video_export: Option<pf_client_core::video::VulkanDecodeDevice>,
    /// The console-UI composite quad (§6.1's presenter half).
    overlay_pipe: OverlayPipe,
    /// The submitted hardware frame (dmabuf plane images + guard, or a Vulkan-Video
    /// frame + our plane views): its GPU reads end with the in-flight fence, so it's
    /// destroyed right after the next fence wait.
    retired_hw: Option<Retired>,
    /// External-sync lock over this device's queues, shared with FFmpeg (via
    /// [`pf_client_core::video::VulkanDecodeDevice::queue_lock`] → its
    /// `lock_queue`/`unlock_queue` callbacks) and the Skia overlay: FFmpeg preps on the
    /// SAME graphics queue from the pump thread, so every `vkQueueSubmit`/
    /// `vkQueuePresentKHR`/`vkQueueWaitIdle`/`vkDeviceWaitIdle` here must hold it —
    /// the unsynchronized overlap was an intermittent `VK_ERROR_DEVICE_LOST`.
    queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    format: vk::SurfaceFormatKHR,
    /// The surface's HDR10/ST.2084 pairing, when the stack offers one.
    hdr10_format: Option<vk::SurfaceFormatKHR>,
    /// PQ frames are on screen and the swapchain is in HDR10 mode.
    hdr_active: bool,
    /// One-shot latch: a PQ frame arrived but the surface offers no HDR10 colorspace, so the
    /// CSC pass silently tone-maps to SDR. Warned once — the single most useful signal for
    /// diagnosing "HDR isn't advertised" (e.g. gamescope's WSI layer invisible in a flatpak
    /// sandbox) vs. the host simply not sending PQ.
    hdr_downgrade_warned: bool,
    /// `VK_EXT_hdr_metadata` device fns when the driver offers them (gamescope/KDE do).
    hdr_metadata_d: Option<ash::ext::hdr_metadata::Device>,
    /// The host's latest ST.2086/CLL metadata (the 0xCE plane) — pushed to the
    /// swapchain whenever HDR10 mode is live; `None` until the first datagram lands
    /// (a generic HDR10 baseline is pushed meanwhile).
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// The video image / CSC attachment format for the current mode.
    video_format: vk::Format,
    present_mode: vk::PresentModeKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    extent: vk::Extent2D,
    /// Per-swapchain-image render-finished semaphores (present consumes them on the
    /// image's schedule — one shared semaphore could be re-submitted while a previous
    /// present still holds it).
    render_sems: Vec<vk::Semaphore>,
    acquire_sem: vk::Semaphore,
    fence: vk::Fence,
    cmd_pool: vk::CommandPool,
    cmd_buf: vk::CommandBuffer,
    staging: Option<Staging>,
    video: Option<VideoImage>,
    /// The submit fence has a submission pending (wait before recording again — also
    /// what makes the single staging buffer safe to overwrite).
    submitted: bool,
    /// `VK_KHR_present_wait` on-glass timing (latency plan T0.2) — `None` when the
    /// device lacks the present-id/present-wait pair; the run loop then keeps its
    /// submit-time display stamp.
    present_timer: Option<present_timing::PresentTimer>,
    /// Monotonic present id (global counter — strictly increasing per swapchain, which
    /// is all the spec asks). 0 = nothing presented with an id yet.
    next_present_id: u64,
    /// The last successful id-carrying present, awaiting its [`Presenter::note_presented`]
    /// claim from the run loop (which owns the frame's pts/decode stamps).
    last_presented: Option<(vk::SwapchainKHR, u64)>,
}

impl Presenter {
    /// Whether the hardware (dmabuf) path exists on this device — callers keep the
    /// decoder on software when it doesn't.
    #[cfg(target_os = "linux")]
    pub fn supports_dmabuf(&self) -> bool {
        self.hw.is_some()
    }

    /// Whether the D3D11 shared-texture path exists on this device — callers keep the
    /// decoder on software when it doesn't.
    #[cfg(windows)]
    pub fn supports_d3d11(&self) -> bool {
        self.hw_win.is_some()
    }

    /// The FFmpeg Vulkan Video decode handle bundle — `None` when this stack can't
    /// (device < 1.3, missing video extensions/queue/features). The decoder chain
    /// falls back to VAAPI/software then.
    pub fn vulkan_decode(&self) -> Option<pf_client_core::video::VulkanDecodeDevice> {
        self.video_export.clone()
    }

    /// Full device idle — TEARDOWN ONLY, and only after the session pump thread has
    /// been joined (it submits FFmpeg decode work; wait-idle's external-sync rule
    /// covers every queue on the device). Mid-session code uses the fence quiesce.
    /// The queue lock is held as cheap insurance against a straggling submitter.
    pub fn wait_idle(&self) {
        let _q = self.queue_lock.guard();
        unsafe { self.device.device_wait_idle() }.ok();
    }

    /// True when `VK_KHR_present_wait` drives the display stamp — the run loop then
    /// defers its e2e/display windows to [`Presenter::take_presented_samples`] instead
    /// of stamping at `present()` return.
    pub(crate) fn present_timing_active(&self) -> bool {
        self.present_timer.is_some()
    }

    /// Claim the just-submitted present for on-glass timing. Call right after a
    /// `present()` that returned `true`, with that frame's capture + decode stamps
    /// (the presenter itself never sees them). No-op when timing is inactive.
    pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
        if let (Some(t), Some((sc, id))) = (&self.present_timer, self.last_presented.take()) {
            t.enqueue(sc, id, pts_ns, decoded_ns);
        }
    }

    /// Take the window's completed on-glass samples (empty when timing is inactive).
    pub(crate) fn take_presented_samples(&self) -> Vec<present_timing::PresentedSample> {
        self.present_timer
            .as_ref()
            .map(|t| t.take_samples())
            .unwrap_or_default()
    }

    /// The device handles the console-UI overlay renders on (§6.1). Valid for the
    /// presenter's lifetime; the run loop drops the overlay first.
    pub fn shared_device(&self) -> SharedDevice {
        SharedDevice {
            entry: self.entry.clone(),
            instance: self.instance.clone(),
            physical_device: self.pdev,
            device: self.device.clone(),
            queue: self.queue,
            queue_family_index: self.qfi,
            queue_lock: self.queue_lock.clone(),
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // The present-wait waiter references the swapchain — stop it (its Drop joins
        // after in-flight waits complete, bounded by their 250 ms cap) BEFORE the
        // swapchain teardown below.
        self.present_timer.take();
        unsafe {
            {
                // Insurance against a straggling submitter (the run loop joins the
                // pump before dropping us, so this is normally uncontended).
                let _q = self.queue_lock.guard();
                self.device.device_wait_idle().ok();
            }
            if let Some(f) = self.retired_hw.take() {
                f.destroy(&self.device); // idle above — the GPU reads are done
            }
            if let Some(s) = self.staging.take() {
                self.device.unmap_memory(s.memory);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            if let Some(v) = self.video.take() {
                if v.framebuffer != vk::Framebuffer::null() {
                    self.device.destroy_framebuffer(v.framebuffer, None);
                }
                if v.view != vk::ImageView::null() {
                    self.device.destroy_image_view(v.view, None);
                }
                self.device.destroy_image(v.image, None);
                self.device.free_memory(v.memory, None);
            }
            #[cfg(target_os = "linux")]
            self.hw.take();
            self.csc.destroy(&self.device);
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            if let Some(p) = &self.csc_planar {
                p.destroy(&self.device);
            }
            self.overlay_pipe.destroy(&self.device);
            for s in self.render_sems.drain(..) {
                self.device.destroy_semaphore(s, None);
            }
            self.device.destroy_semaphore(self.acquire_sem, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(self.swapchain, None);
            }
            self.device.destroy_device(None);
            self.surface_i.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
        // `entry` (the libvulkan handle) drops last, after every vk call is done.
        let _ = &self.entry;
    }
}
