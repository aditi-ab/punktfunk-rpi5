//! The Vulkan handoff types a decode lane shares with whatever owns the device: the
//! `VkDevice` as plain integers, plus the lock that serializes the one queue both sides
//! submit to.
//!
//! Split out of [`crate::video`] — which re-exports them, so desktop call sites are
//! unchanged — because Android reaches the PyroWave decoder without the `desktop` half of
//! this crate: there the device belongs to the JNI client's own present path, not to a
//! session presenter. std only; ash stays on the callers' side of the seam.

/// Mutex serializing `vkQueueSubmit` / `vkQueuePresentKHR` / `vkQueueWaitIdle`
/// on the queue the presenter shares with the decode lane.
///
/// The presenter has one graphics-family queue; the pump submits decode/CSC
/// to it from another thread. Unsynchronized `vkQueueSubmit` is intermittent
/// `VK_ERROR_DEVICE_LOST`. Lock/unlock stay for callbacks; [`QueueLock::guard`] is RAII.
pub struct QueueLock {
    locked: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl QueueLock {
    #[allow(clippy::new_without_default)]
    pub fn new() -> QueueLock {
        QueueLock {
            locked: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Block until the queue is free, then take it. Pair with [`QueueLock::unlock`], or use [`QueueLock::guard`].
    pub fn lock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *g {
            g = self
                .cv
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *g = true;
    }

    pub fn unlock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = false;
        drop(g);
        self.cv.notify_one();
    }

    /// RAII form for Rust call sites (presenter submits/presents, Skia flushes).
    pub fn guard(&self) -> QueueLockGuard<'_> {
        self.lock();
        QueueLockGuard(self)
    }
}

/// Releases the [`QueueLock`] on drop.
pub struct QueueLockGuard<'a>(&'a QueueLock);

impl Drop for QueueLockGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}

/// Selected presenter-device facts plus shared handles: the device the frame is
/// presented from, so decode runs where the pixels are sampled — the VkImage is
/// composited in place. Desktop fills this from the session presenter, Android
/// from the JNI client's own swapchain.
///
/// The bundle exists with or without Vulkan Video: `video_decode` gates that
/// rung while vendor and import facts keep answering either way. Plain integers:
/// this crate has no ash. Handles stay valid for the owner's lifetime, which
/// outlives every session pump.
#[derive(Clone)]
pub struct VulkanDecodeDevice {
    /// `PFN_vkGetInstanceProcAddr` from the loader. Decode lanes resolve everything else through it.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    /// PCI vendor of the presenter's physical device (0x10DE NVIDIA, 0x1002 AMD,
    /// 0x8086 Intel) — drives [`Self::prefer_vulkan_first`].
    pub vendor_id: u32,
    /// Driver device-name string (logged on admission refusal).
    pub device_name: String,
    /// The presenter's graphics+present family.
    pub graphics_qf: u32,
    /// Video-decode family. May equal `graphics_qf`; the native rung must detect that (`submit_queues_collide`).
    pub decode_qf: u32,
    /// Raw `VkVideoCodecOperationFlagsKHR` the decode family advertises.
    pub decode_video_caps: u32,
    /// Extensions enabled at instance/device creation. Pyrowave replays these
    /// verbatim into pinned create-info, so they must match reality.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<std::ffi::CString>,
    /// Features enabled at device creation (reported via `device_features`).
    pub f_sampler_ycbcr: bool,
    pub f_timeline_semaphore: bool,
    pub f_synchronization2: bool,
    /// Vulkan Video decode is usable (queue + extensions + features). The bundle
    /// exists without it; gate the Vulkan rung on this, not on `Some`.
    pub video_decode: bool,
    /// Real present timing (`VK_KHR_present_wait`). Gates `CLIENT_CAP_PHASE_LOCK`:
    /// without a latch stamp the desktop must not claim the cap.
    pub present_timing: bool,
    /// PyroWave decode is usable (Vulkan 1.3 + `shaderInt16` / 8-bit storage /
    /// subgroup size control). Gates the `CODEC_PYROWAVE` advertisement.
    pub pyrowave_decode: bool,
    /// Feature facts the pyrowave pinned create-info reconstruction mirrors
    /// so it can share this `VkDevice`.
    pub f_shader_int16: bool,
    pub f_storage_buffer8: bool,
    pub f_subgroup_size_control: bool,
    pub f_compute_full_subgroups: bool,
    pub f_shader_float16: bool,
    /// `VkPhysicalDeviceProperties::apiVersion` of the presenter's device.
    pub api_version: u32,
    /// Queue families the device was created with (one queue each, priority 1.0). Mirrored by reconstruction.
    pub queue_families: Vec<u32>,
    /// Presenter enabled win32 external-memory + keyed mutex. Always `false` off Windows.
    pub d3d11_import: bool,
    /// Presenter enabled Linux dma-buf import. Always `false` off Linux.
    pub dmabuf_import: bool,
    /// Presenter can import RGB10A2 and offers an HDR10 swapchain, so D3D11VA
    /// emits PQ pass-through instead of tonemapping to sRGB. Always `false` off Windows.
    pub d3d11_hdr10: bool,
    /// Presenter imports two-plane NV12 / P010 D3D11 textures for sampling and the vendor
    /// survives it, so D3D11VA copies into planar slots instead of running the video
    /// processor. Always `false` off Windows.
    pub d3d11_nv12: bool,
    pub d3d11_p010: bool,
    /// Adapter LUID when the driver reports one. D3D11VA builds on the same
    /// adapter so shared textures never cross GPUs. `None` off Windows or when unreported.
    pub adapter_luid: Option<[u8; 8]>,
    /// Shared queue lock. Presenter and decode lanes both take it around their submits.
    pub queue_lock: std::sync::Arc<QueueLock>,
}

/// PCI vendor ids `vendor_id` reports.
pub(crate) const VENDOR_NVIDIA: u32 = 0x10DE;
pub(crate) const VENDOR_AMD: u32 = 0x1002;

impl VulkanDecodeDevice {
    /// Should `auto` try Vulkan Video before VAAPI / D3D11VA on this device?
    ///
    /// NVIDIA and AMD: yes. NVIDIA has no usable VAAPI; VanGogh VAAPI chroma-fringes.
    /// This orders attempts only: later admission may skip the platform rung
    /// (NVIDIA VAAPI is barred from auto). Intel/unknown try the platform rung first.
    pub fn prefer_vulkan_first(&self) -> bool {
        self.vendor_id == VENDOR_NVIDIA || self.vendor_id == VENDOR_AMD
    }
}
