//! Borrowed-device wrap: the presenter's live Vulkan handles loaded into ash
//! function tables, plus the queue-lock contract every queue submission runs under.
//!
//! Ownership: everything in [`DeviceHandles`] is BORROWED. This crate never creates
//! and never destroys the instance/device — [`DecodeDevice`]'s ash wrappers are
//! function tables over foreign handles, and dropping them destroys nothing. The
//! objects this crate does create (sessions, images, buffers, pools) are destroyed
//! by their owning structs' `Drop` impls, all of which must run before the borrowed
//! device dies — the same liveness contract FFmpeg's decoder had over the identical
//! handle bundle (`pf-client-core`'s `VulkanDecodeDevice`), now written down.

use ash::vk;
use ash::vk::Handle;

/// The borrowed handles of the presenter's decode-capable device, as raw integers so
/// the type stays FFI-plain (mirrors `pf-client-core`'s `VulkanDecodeDevice`, which
/// adapts into this in WP-C — pf-vkdecode deliberately does not depend on it).
///
/// Caller contract (checked where cheap, otherwise trusted):
/// - All four handles are live, and stay live for the lifetime of every object this
///   crate builds from them (the presenter outlives every session pump).
/// - The instance/device were created with the Vulkan Video decode stack enabled:
///   `VK_KHR_video_queue`, `VK_KHR_video_decode_queue`, `VK_KHR_video_decode_h264`,
///   plus the `synchronization2` and `timelineSemaphore` features (the presenter's
///   device meets all of this when it advertises `video_decode`).
/// - `decode_qf`/`decode_queue_index` name a queue with `VIDEO_DECODE_KHR` ops whose
///   family advertises H.264 decode; `graphics_qf` is the family the presenter
///   samples on (image sharing crosses the two when they differ).
#[derive(Debug, Clone)]
pub struct DeviceHandles {
    /// `PFN_vkGetInstanceProcAddr` from the loader; everything else is resolved
    /// through it.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    /// The video-decode queue family.
    pub decode_qf: u32,
    /// Queue index within `decode_qf` this decoder submits on.
    pub decode_queue_index: u32,
    /// The presenter's graphics+present family (the other side of image sharing).
    pub graphics_qf: u32,
}

/// External synchronization for `vkQueueSubmit`: the caller supplies the lock that
/// serializes EVERY submit on the shared device — in WP-C that is pf-client-core's
/// `QueueLock`, the same object the presenter holds around its own submits/presents
/// (the 2026-07-09 `VK_ERROR_DEVICE_LOST` race is why this is a first-class contract
/// and not an afterthought). Tests use [`NoopQueueLock`].
///
/// `lock` blocks until the queue is free and takes it; `unlock` releases it. Use
/// [`QueueSubmitGuard`] rather than calling the pair by hand.
pub trait QueueLock {
    fn lock(&self);
    fn unlock(&self);
}

/// A [`QueueLock`] that guards nothing — for tests and for callers whose decode
/// queue is provably not shared with any other submitter.
#[derive(Debug, Default)]
pub struct NoopQueueLock;

impl QueueLock for NoopQueueLock {
    fn lock(&self) {}
    fn unlock(&self) {}
}

/// RAII scope over a [`QueueLock`]: acquired for exactly the duration of a queue
/// submission, released on drop (including unwinds — though this crate's own paths
/// never panic while holding it).
pub struct QueueSubmitGuard<'a> {
    lock: &'a dyn QueueLock,
}

impl<'a> QueueSubmitGuard<'a> {
    /// Take the queue (blocking until free).
    pub fn acquire(lock: &'a dyn QueueLock) -> Self {
        lock.lock();
        Self { lock }
    }
}

impl Drop for QueueSubmitGuard<'_> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

/// A [`DeviceHandles`] bundle that cannot be wrapped. Every variant is a caller
/// bug (a half-filled bundle), not a runtime condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    /// One of the four raw handles is zero/null.
    NullHandle(&'static str),
}

/// A device allocation that cannot proceed. Wraps the raw Vulkan failure OR the
/// memory-type miss that used to be silently papered over with index 0 — a wrong
/// type index is at best an immediate validation error and at worst a mapping of
/// the wrong heap, so a miss is an ERROR here, never a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocError {
    Vk(vk::Result),
    /// No memory type satisfies (`type_bits`, `flags`) on this device.
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl From<vk::Result> for AllocError {
    fn from(r: vk::Result) -> Self {
        AllocError::Vk(r)
    }
}

/// First memory type matching `bits` and `want` — an [`AllocError::NoMemoryType`]
/// when none does (the encoder's `find_mem` falls back to 0 there; here the miss
/// surfaces).
pub(crate) fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Result<u32, AllocError> {
    for i in 0..props.memory_type_count {
        if (bits & (1 << i)) != 0 && props.memory_types[i as usize].property_flags.contains(want) {
            return Ok(i);
        }
    }
    Err(AllocError::NoMemoryType {
        type_bits: bits,
        flags: want,
    })
}

/// First memory type matching `bits` that also carries `prefer`; when none does,
/// the first type matching `bits` at all. A driver constrains `memoryTypeBits` to
/// where the allocation can legally live — NVIDIA (610.88) reports some video-
/// session bindings host-visible-ONLY, which is spec-legal, so a hard `prefer`
/// requirement there is unsatisfiable by construction. Still an
/// [`AllocError::NoMemoryType`] when `bits` selects nothing whatsoever (that
/// miss-is-error contract stays; only the property preference softens). Mapped
/// staging paths (the bitstream ring) must NOT use this: they require
/// `HOST_VISIBLE|HOST_COHERENT` as a hard property, not a preference.
pub(crate) fn find_memory_type_preferring(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    prefer: vk::MemoryPropertyFlags,
) -> Result<u32, AllocError> {
    match find_memory_type(props, bits, prefer) {
        Ok(index) => Ok(index),
        Err(_) => find_memory_type(props, bits, vk::MemoryPropertyFlags::empty()),
    }
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::NullHandle(which) => {
                write!(f, "DeviceHandles.{which} is null — a half-filled bundle")
            }
        }
    }
}

impl std::error::Error for DeviceError {}

/// The borrowed device with ash function tables loaded: the object every other
/// module in this crate makes its Vulkan calls through.
///
/// Clone is cheap-ish (ash tables are plain structs of function pointers) and safe:
/// clones share the same borrowed handles under the same liveness contract.
#[derive(Clone)]
pub struct DecodeDevice {
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    video_queue_instance: ash::khr::video_queue::Instance,
    video_queue: ash::khr::video_queue::Device,
    video_decode_queue: ash::khr::video_decode_queue::Device,
    decode_queue: vk::Queue,
    decode_qf: u32,
    graphics_qf: u32,
    /// The decode family advertises `queryResultStatusSupport`: per-op
    /// RESULT_STATUS queries are legal in its video coding scopes. FALSE on RADV
    /// (2026-08, .25: recording one anyway hangs the VCN ring) — the decoder
    /// must skip queries entirely there and fall back to timeline-completion
    /// verdicts.
    result_status_queries: bool,
}

impl DecodeDevice {
    /// Load ash function tables over the borrowed handles.
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract: live handles (outliving `self`
    /// and everything created through it), the video-decode extensions/features
    /// enabled at creation, and truthful queue-family fields. Null handles are
    /// rejected here; everything else cannot be checked and is trusted.
    pub unsafe fn wrap(handles: &DeviceHandles) -> Result<Self, DeviceError> {
        if handles.get_instance_proc_addr == 0 {
            return Err(DeviceError::NullHandle("get_instance_proc_addr"));
        }
        if handles.instance == 0 {
            return Err(DeviceError::NullHandle("instance"));
        }
        if handles.physical_device == 0 {
            return Err(DeviceError::NullHandle("physical_device"));
        }
        if handles.device == 0 {
            return Err(DeviceError::NullHandle("device"));
        }

        // SAFETY: the usize is non-zero (checked above) and the caller contract says
        // it is the loader's PFN_vkGetInstanceProcAddr; fn pointers and usize share
        // size/ABI on every supported target.
        let gipa: vk::PFN_vkGetInstanceProcAddr = unsafe {
            std::mem::transmute::<usize, vk::PFN_vkGetInstanceProcAddr>(
                handles.get_instance_proc_addr,
            )
        };
        // SAFETY: `gipa` is a valid Vulkan-1.0-conformant loader entry point per the
        // caller contract, valid for the returned Entry's lifetime (handle liveness).
        let entry = unsafe {
            ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr: gipa,
            })
        };
        // SAFETY: `handles.instance` is a live VkInstance created through this very
        // loader (caller contract), so loading instance-level functions against it
        // is exactly the ash::Instance::load contract.
        let instance = unsafe {
            ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(handles.instance as u64),
            )
        };
        // SAFETY: `handles.device` is a live VkDevice of that instance (caller
        // contract) — the ash::Device::load contract.
        let device = unsafe {
            ash::Device::load(
                instance.fp_v1_0(),
                vk::Device::from_raw(handles.device as u64),
            )
        };
        let video_queue_instance = ash::khr::video_queue::Instance::new(&entry, &instance);
        let video_queue = ash::khr::video_queue::Device::new(&instance, &device);
        let video_decode_queue = ash::khr::video_decode_queue::Device::new(&instance, &device);
        // SAFETY: the caller contract guarantees `decode_qf`/`decode_queue_index`
        // name a queue the device was created with.
        let decode_queue =
            unsafe { device.get_device_queue(handles.decode_qf, handles.decode_queue_index) };

        // Whether the decode family supports RESULT_STATUS queries (per-family
        // cap; struct field docs).
        let physical_device = vk::PhysicalDevice::from_raw(handles.physical_device as u64);
        // SAFETY: live physical device (caller contract); the two-call form fills
        // the chained per-family status-support structs.
        let family_count =
            unsafe { instance.get_physical_device_queue_family_properties2_len(physical_device) };
        let result_status_queries = if (handles.decode_qf as usize) < family_count {
            let mut status_props =
                vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); family_count];
            let mut families: Vec<vk::QueueFamilyProperties2<'_>> = status_props
                .iter_mut()
                .map(|s| vk::QueueFamilyProperties2::default().push_next(s))
                .collect();
            // SAFETY: as above, arrays sized to the reported count.
            unsafe {
                instance
                    .get_physical_device_queue_family_properties2(physical_device, &mut families)
            };
            drop(families);
            status_props[handles.decode_qf as usize].query_result_status_support != vk::FALSE
        } else {
            false
        };

        // `entry` is only the ladder the tables above were loaded through; nothing
        // needs it afterwards (ash tables own their function pointers).
        drop(entry);

        Ok(Self {
            instance,
            device,
            physical_device,
            video_queue_instance,
            video_queue,
            video_decode_queue,
            decode_queue,
            decode_qf: handles.decode_qf,
            graphics_qf: handles.graphics_qf,
            result_status_queries,
        })
    }

    pub(crate) fn ash(&self) -> &ash::Device {
        &self.device
    }

    /// Whether the decode family supports per-op RESULT_STATUS queries (struct
    /// field docs — FALSE on RADV, where recording one hangs the VCN).
    pub(crate) fn result_status_queries(&self) -> bool {
        self.result_status_queries
    }

    pub(crate) fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    pub(crate) fn video_queue_instance(&self) -> &ash::khr::video_queue::Instance {
        &self.video_queue_instance
    }

    pub(crate) fn video_queue(&self) -> &ash::khr::video_queue::Device {
        &self.video_queue
    }

    pub(crate) fn video_decode_queue(&self) -> &ash::khr::video_decode_queue::Device {
        &self.video_decode_queue
    }

    pub(crate) fn decode_queue(&self) -> vk::Queue {
        self.decode_queue
    }

    pub(crate) fn decode_qf(&self) -> u32 {
        self.decode_qf
    }

    /// The queue families image sharing spans: empty (EXCLUSIVE) when decode and
    /// graphics are one family, both otherwise (CONCURRENT — the presenter samples
    /// decode output on its own family and per-frame ownership transfers would buy
    /// latency for nothing at punktfunk's frame rates).
    pub(crate) fn sharing_families(&self) -> Vec<u32> {
        if self.decode_qf == self.graphics_qf {
            Vec::new()
        } else {
            vec![self.decode_qf, self.graphics_qf]
        }
    }

    /// The device's memory properties (queried fresh; cheap and stateless).
    pub(crate) fn memory_properties(&self) -> vk::PhysicalDeviceMemoryProperties {
        // SAFETY: `physical_device` is live per the DeviceHandles contract; the call
        // fills a plain struct and touches nothing else.
        unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_filled_bundle_is_rejected_before_any_ffi() {
        let mut handles = DeviceHandles {
            get_instance_proc_addr: 0,
            instance: 1,
            physical_device: 1,
            device: 1,
            decode_qf: 0,
            decode_queue_index: 0,
            graphics_qf: 0,
        };
        // SAFETY: wrap rejects the null handle before making any Vulkan call, so no
        // part of the liveness contract is exercised. (`Err` matched by hand: the
        // Ok side holds ash tables, which carry no Debug for unwrap_err.)
        let result = unsafe { DecodeDevice::wrap(&handles) };
        let Err(err) = result else {
            panic!("a null gipa must be rejected")
        };
        assert_eq!(err, DeviceError::NullHandle("get_instance_proc_addr"));

        handles.get_instance_proc_addr = 1;
        handles.device = 0;
        // SAFETY: as above — the null device handle is rejected before any FFI.
        let result = unsafe { DecodeDevice::wrap(&handles) };
        let Err(err) = result else {
            panic!("a null device must be rejected")
        };
        assert_eq!(err, DeviceError::NullHandle("device"));
    }

    #[test]
    fn a_memory_type_miss_is_an_error_never_a_fallback_to_index_zero() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 2,
            ..Default::default()
        };
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

        // A hit resolves to the matching index, not the first.
        assert_eq!(
            find_memory_type(
                &props,
                0b11,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            ),
            Ok(1)
        );
        // A type excluded by the requirement bits does not count as a hit.
        assert_eq!(
            find_memory_type(&props, 0b01, vk::MemoryPropertyFlags::HOST_VISIBLE),
            Err(AllocError::NoMemoryType {
                type_bits: 0b01,
                flags: vk::MemoryPropertyFlags::HOST_VISIBLE
            })
        );
        // Flags nothing advertises: an error carrying the miss, never index 0.
        assert_eq!(
            find_memory_type(&props, 0b11, vk::MemoryPropertyFlags::PROTECTED),
            Err(AllocError::NoMemoryType {
                type_bits: 0b11,
                flags: vk::MemoryPropertyFlags::PROTECTED
            })
        );
    }

    #[test]
    fn preferring_picks_the_preferred_type_and_falls_back_inside_the_bits() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 4,
            ..Default::default()
        };
        props.memory_types[0].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[3].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

        // The preferred property wins over a lower-indexed non-preferred type.
        assert_eq!(
            find_memory_type_preferring(&props, 0b0011, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Ok(1)
        );
        // The NVIDIA session-binding shape: `memoryTypeBits` names only a
        // host-visible type — honor the bits instead of erroring.
        assert_eq!(
            find_memory_type_preferring(&props, 0b1000, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Ok(3)
        );
        // Bits selecting nothing remain a hard miss, never index 0.
        assert_eq!(
            find_memory_type_preferring(&props, 0b0000, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Err(AllocError::NoMemoryType {
                type_bits: 0b0000,
                flags: vk::MemoryPropertyFlags::empty()
            })
        );
    }

    #[test]
    fn the_queue_submit_guard_brackets_the_lock() {
        use std::sync::atomic::AtomicI32;
        use std::sync::atomic::Ordering;

        #[derive(Default)]
        struct CountingLock {
            depth: AtomicI32,
            peak: AtomicI32,
        }
        impl QueueLock for CountingLock {
            fn lock(&self) {
                let d = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(d, Ordering::SeqCst);
            }
            fn unlock(&self) {
                self.depth.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let lock = CountingLock::default();
        {
            let _guard = QueueSubmitGuard::acquire(&lock);
            assert_eq!(lock.depth.load(Ordering::SeqCst), 1);
        }
        assert_eq!(lock.depth.load(Ordering::SeqCst), 0, "released on drop");
        assert_eq!(lock.peak.load(Ordering::SeqCst), 1);
    }
}
