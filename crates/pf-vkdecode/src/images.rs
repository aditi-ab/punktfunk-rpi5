//! Decode image pools — the FFmpeg pool model, zero-copy:
//!
//! The PICTURE POOL is decoupled from DPB slots. Images outnumber slots by
//! [`HOLD_HEADROOM`], and a DPB slot binds an image at ACTIVATION time — a
//! re-activated slot may bind a DIFFERENT free image (spec-legal with
//! `SEPARATE_REFERENCE_IMAGES`, which the caps derivation requires for coincide
//! mode). A picture the consumer still holds is therefore NEVER a decode target:
//! its image simply stays off the free list until the release token returns.
//! This is the exact contract the presenter already speaks on the AVVkFrame path,
//! re-implemented without FFmpeg in the middle.
//!
//! - **coincide** (RADV): pool images are DPB + decode output + sampled surface
//!   in one (`DPB|DST|SAMPLED`, per-slot images).
//! - **distinct** (NVIDIA): a separate reference-only DPB array (layered or
//!   per-slot — never delivered, so its slot↔layer mapping stays fixed) plus the
//!   pool as decode outputs (`DST|SAMPLED`).
//!
//! Every pool image carries its OWN timeline semaphore (the AVVkFrame contract):
//! the decoder signals `value+1` when it writes the image; the presenter waits
//! that value, samples, restores the layout, and signals `value+1` again in the
//! same submission — the decoder's ledger learns of that write-back at
//! `release_frame` and waits it before the image's next use.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::DecodeCaps;
use crate::caps::H264ProfileChain;
use crate::caps::COINCIDE_USAGE;
use crate::caps::DPB_USAGE;
use crate::caps::OUTPUT_USAGE;
use crate::device::find_memory_type;
use crate::device::AllocError;
use crate::device::DecodeDevice;

/// Picture-pool headroom on top of the stream's DPB needs: how many decoded
/// pictures the CONSUMER may hold (delivered, unreleased) before the decoder
/// reports backpressure. The real client pipeline holds ~4-7 frames at steady
/// state (two bounded(2) channels, the FrameStore's 1..=3 preroll, the in-flight
/// present and the retired-frame slot), so 8 gives it a frame of slack; a
/// consumer holding MORE than this earns the `NoFreeSlot` error, which then
/// means exactly what it says.
///
/// (The 2026-08 .25 field failure taught the sizing lesson the hard way: any
/// FIXED pool ignoring the stream's DPB depth starves on a clean stream — the
/// vendored 25fps vector alone keeps `max_dpb_frames + 1 = 8` pictures resident.
/// Pool size is always `required_slots + HOLD_HEADROOM`.)
pub const HOLD_HEADROOM: u32 = 8;

/// The pure pool shape for one (caps, required-slots) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPlan {
    /// Distinct-mode reference-only DPB array; 0 images in coincide mode (the
    /// picture pool IS the DPB backing there).
    pub dpb_image_count: u32,
    pub dpb_layers_per_image: u32,
    pub dpb_usage: vk::ImageUsageFlags,
    /// The decoupled picture pool: decode outputs + (coincide) DPB bindings.
    pub picture_count: u32,
    pub picture_usage: vk::ImageUsageFlags,
    pub picture_flags: vk::ImageCreateFlags,
}

/// Decide the pool shape. Pure — unit-tested below.
///
/// `required_slots` is the stream's `max_dpb_frames + 1`; the picture pool adds
/// [`HOLD_HEADROOM`] on top so consumer-held pictures never displace decode
/// targets. Layered-coincide never reaches here (the caps derivation rejects it).
pub fn plan_pools(caps: &DecodeCaps, required_slots: u32) -> PoolPlan {
    let picture_count = required_slots + HOLD_HEADROOM;
    let picture_flags = vk::ImageCreateFlags::MUTABLE_FORMAT;
    if caps.coincide {
        PoolPlan {
            dpb_image_count: 0,
            dpb_layers_per_image: 0,
            dpb_usage: vk::ImageUsageFlags::empty(),
            picture_count,
            picture_usage: COINCIDE_USAGE,
            picture_flags,
        }
    } else {
        let (dpb_image_count, dpb_layers_per_image) = if caps.layered_dpb {
            (1, required_slots)
        } else {
            (required_slots, 1)
        };
        PoolPlan {
            dpb_image_count,
            dpb_layers_per_image,
            dpb_usage: DPB_USAGE,
            picture_count,
            picture_usage: OUTPUT_USAGE,
            picture_flags,
        }
    }
}

/// One picture-pool image with its sync + occupancy ledger.
pub(crate) struct Picture {
    pub image: vk::Image,
    /// Full-picture NV12 view (decode dst / DPB binding).
    pub view: vk::ImageView,
    /// `R8`/`R8G8` plane views for the presenter's sampler path.
    pub plane_views: [vk::ImageView; 2],
    /// The image's own timeline semaphore (AVVkFrame contract).
    pub semaphore: vk::Semaphore,
    /// Latest timeline value known signalled-or-enqueued: the decoder's write
    /// signal, bumped to the presenter's write-back (`frame.value + 1`) when a
    /// release token reports the frame was sampled.
    pub value: u64,
    /// A DPB slot currently binds this image (coincide mode).
    pub bound: bool,
    /// A decoded picture awaiting its output verdict lives here.
    pub pending: bool,
    /// Frames over this image not yet released (ready queue + consumer-held).
    pub held: u32,
}

impl Picture {
    /// Free for a new decode target: no slot binds it, no pending picture lives
    /// in it, no unreleased frame reads it.
    pub(crate) fn is_free(&self) -> bool {
        !self.bound && !self.pending && self.held == 0
    }
}

/// The decoupled picture pool. Destroys everything it created on drop
/// (null-safe); a pool with consumer-held images is retired to the decoder's
/// graveyard instead of dropped, and dies when its last release token arrives.
pub(crate) struct PicturePool {
    device: ash::Device,
    memory: Vec<vk::DeviceMemory>,
    pub(crate) pictures: Vec<Picture>,
}

impl PicturePool {
    /// Create `plan.picture_count` single-layer images at `extent` (the
    /// granularity-ALIGNED allocation extent).
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        extent: vk::Extent2D,
        std_profile_idc: hh::StdVideoH264ProfileIdc,
    ) -> Result<Self, AllocError> {
        let mut pool = Self {
            device: dev.ash().clone(),
            memory: Vec::new(),
            pictures: Vec::new(),
        };
        let families = dev.sharing_families();
        for _ in 0..plan.picture_count {
            // SAFETY: fn contract (live device); every created handle is parked
            // in `pool` so a mid-build failure unwinds through Drop.
            let (image, memory) = unsafe {
                create_video_image(
                    dev,
                    caps.output_format,
                    extent,
                    1,
                    plan.picture_usage,
                    plan.picture_flags,
                    &families,
                    std_profile_idc,
                )?
            };
            pool.memory.push(memory);
            // The picture is parked with null handles IMMEDIATELY (Drop ignores
            // nulls), then each view/semaphore is filled as it is created — a
            // failure anywhere unwinds everything created so far.
            pool.pictures.push(Picture {
                image,
                view: vk::ImageView::null(),
                plane_views: [vk::ImageView::null(); 2],
                semaphore: vk::Semaphore::null(),
                value: 0,
                bound: false,
                pending: false,
                held: 0,
            });
            let picture = pool.pictures.len() - 1;
            // SAFETY: `image` was just created with layer 0 in range (holds for
            // all three creates in this block); plane formats are
            // NV12-compatible under MUTABLE_FORMAT (caps-gated by derive_caps).
            unsafe {
                pool.pictures[picture].view = create_view(
                    &pool.device,
                    image,
                    caps.output_format,
                    vk::ImageAspectFlags::COLOR,
                    0,
                )?;
                pool.pictures[picture].plane_views[0] = create_view(
                    &pool.device,
                    image,
                    vk::Format::R8_UNORM,
                    vk::ImageAspectFlags::PLANE_0,
                    0,
                )?;
                pool.pictures[picture].plane_views[1] = create_view(
                    &pool.device,
                    image,
                    vk::Format::R8G8_UNORM,
                    vk::ImageAspectFlags::PLANE_1,
                    0,
                )?;
            }
            let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let sem_ci = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
            // SAFETY: live device; timelineSemaphore enabled per the handles
            // contract.
            pool.pictures[picture].semaphore =
                unsafe { pool.device.create_semaphore(&sem_ci, None)? };
        }
        Ok(pool)
    }

    /// Index of the first free image, if any.
    pub(crate) fn free_index(&self) -> Option<usize> {
        self.pictures.iter().position(Picture::is_free)
    }

    /// Total frames not yet released across the pool (graveyard retirement key).
    pub(crate) fn held_total(&self) -> u32 {
        self.pictures.iter().map(|p| p.held).sum()
    }
}

impl Drop for PicturePool {
    fn drop(&mut self) {
        // SAFETY: every handle is this pool's own on the (contract-live) device;
        // the owning decoder drains decode work before dropping/retiring, and a
        // retired pool is only dropped once its last release token returned (the
        // presenter's fence wait). Destroys ignore NULL (half-built unwinding).
        unsafe {
            for p in self.pictures.drain(..) {
                self.device.destroy_image_view(p.view, None);
                self.device.destroy_image_view(p.plane_views[0], None);
                self.device.destroy_image_view(p.plane_views[1], None);
                self.device.destroy_semaphore(p.semaphore, None);
                self.device.destroy_image(p.image, None);
            }
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// Distinct-mode reference-only DPB backing (fixed slot↔layer mapping — these
/// images are never delivered, so nothing consumer-side ever pins them).
pub(crate) struct DpbPool {
    device: ash::Device,
    images: Vec<vk::Image>,
    memory: Vec<vk::DeviceMemory>,
    dpb_views: Vec<vk::ImageView>,
    dpb_location: Vec<(usize, u32)>,
}

impl DpbPool {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        extent: vk::Extent2D,
        std_profile_idc: hh::StdVideoH264ProfileIdc,
    ) -> Result<Self, AllocError> {
        let mut pool = Self {
            device: dev.ash().clone(),
            images: Vec::new(),
            memory: Vec::new(),
            dpb_views: Vec::new(),
            dpb_location: Vec::new(),
        };
        let families = dev.sharing_families();
        for _ in 0..plan.dpb_image_count {
            // SAFETY: fn contract (live device); parked in `pool` for unwinding.
            let (image, memory) = unsafe {
                create_video_image(
                    dev,
                    caps.dpb_format,
                    extent,
                    plan.dpb_layers_per_image,
                    plan.dpb_usage,
                    vk::ImageCreateFlags::empty(),
                    &families,
                    std_profile_idc,
                )?
            };
            pool.images.push(image);
            pool.memory.push(memory);
        }
        let slots = plan.dpb_image_count * plan.dpb_layers_per_image;
        for slot in 0..slots {
            let (image_index, layer) = if plan.dpb_image_count == 1 {
                (0usize, slot)
            } else {
                (slot as usize, 0u32)
            };
            // SAFETY: the image was created above with `layer` in range.
            let view = unsafe {
                create_view(
                    &pool.device,
                    pool.images[image_index],
                    caps.dpb_format,
                    vk::ImageAspectFlags::COLOR,
                    layer,
                )?
            };
            pool.dpb_views.push(view);
            pool.dpb_location.push((image_index, layer));
        }
        Ok(pool)
    }

    /// The DPB binding view of `slot`.
    pub(crate) fn dpb_view(&self, slot: u8) -> vk::ImageView {
        self.dpb_views[usize::from(slot)]
    }

    /// The image + array layer behind DPB `slot` (barrier targeting).
    pub(crate) fn dpb_target(&self, slot: u8) -> (vk::Image, u32) {
        let (image_index, layer) = self.dpb_location[usize::from(slot)];
        (self.images[image_index], layer)
    }
}

impl Drop for DpbPool {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device; the owning decoder
        // drains decode work before dropping state (nothing consumer-side ever
        // references these). Destroys ignore NULL.
        unsafe {
            for view in self.dpb_views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            for image in self.images.drain(..) {
                self.device.destroy_image(image, None);
            }
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// One OPTIMAL-tiling video image bound to fresh DEVICE_LOCAL memory, profile-listed
/// (mirrors the encoder's `make_video_image`, minus its `&mut` profile-list plumbing).
///
/// # Safety
///
/// `dev` wraps live handles.
#[allow(clippy::too_many_arguments)]
unsafe fn create_video_image(
    dev: &DecodeDevice,
    format: vk::Format,
    extent: vk::Extent2D,
    layers: u32,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
    families: &[u32],
    std_profile_idc: hh::StdVideoH264ProfileIdc,
) -> Result<(vk::Image, vk::DeviceMemory), AllocError> {
    let mut chain = H264ProfileChain::new(std_profile_idc);
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let mut ci = vk::ImageCreateInfo::default()
        .flags(flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width: extent.width,
            height: extent.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut profile_list);
    ci = if families.len() >= 2 {
        ci.sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(families)
    } else {
        ci.sharing_mode(vk::SharingMode::EXCLUSIVE)
    };
    // SAFETY: live device; `ci` roots a chain of locals outliving the call.
    let image = unsafe { dev.ash().create_image(&ci, None)? };
    // SAFETY: `image` was just created on this device.
    let req = unsafe { dev.ash().get_image_memory_requirements(image) };
    let props = dev.memory_properties();
    let type_index = match find_memory_type(
        &props,
        req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    ) {
        Ok(index) => index,
        Err(e) => {
            // SAFETY: destroying the just-created, never-bound image.
            unsafe { dev.ash().destroy_image(image, None) };
            return Err(e);
        }
    };
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(type_index);
    // SAFETY: live device; unwind destroys the unbound image so the error path
    // leaks nothing.
    let memory = match unsafe { dev.ash().allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: destroying the just-created, never-bound image.
            unsafe { dev.ash().destroy_image(image, None) };
            return Err(e.into());
        }
    };
    // SAFETY: fresh image + fresh memory of the required size.
    if let Err(e) = unsafe { dev.ash().bind_image_memory(image, memory, 0) } {
        // SAFETY: unwinding the two objects created above.
        unsafe {
            dev.ash().destroy_image(image, None);
            dev.ash().free_memory(memory, None);
        }
        return Err(e.into());
    }
    Ok((image, memory))
}

/// One single-layer 2D view (`base_array_layer = layer`, identity swizzle).
///
/// # Safety
///
/// `image` is live on `device` with `layer` in range; `format`/`aspect` are
/// compatible with the image's creation (same format for COLOR, plane-compatible
/// under MUTABLE_FORMAT for the plane aspects).
unsafe fn create_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    aspect: vk::ImageAspectFlags,
    layer: u32,
) -> Result<vk::ImageView, vk::Result> {
    let ci = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: aspect,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: layer,
            layer_count: 1,
        });
    // SAFETY: the fn-level contract restates exactly what create_image_view needs.
    unsafe { device.create_image_view(&ci, None) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::derive_caps;
    use crate::caps::RawH264Caps;
    use crate::caps::VideoFormat;
    use crate::caps::NV12;

    fn caps(coincide: bool, layered: bool) -> DecodeCaps {
        // Every entry advertises its role's full usage plus MUTABLE_FORMAT — the
        // derivation gates on those; this module's decision table is downstream.
        let entry = |usage: vk::ImageUsageFlags| VideoFormat {
            format: NV12,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT,
        };
        let raw = RawH264Caps {
            capability_flags: if layered {
                vk::VideoCapabilityFlagsKHR::empty()
            } else {
                vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES
            },
            decode_flags: if coincide {
                vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE
            } else {
                vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT
            },
            dpb_formats: vec![entry(DPB_USAGE)],
            output_formats: vec![entry(OUTPUT_USAGE)],
            coincide_formats: vec![entry(COINCIDE_USAGE)],
            ..Default::default()
        };
        derive_caps(&raw).unwrap()
    }

    #[test]
    fn coincide_pools_are_headroomed_dual_use_pictures_with_no_dpb_array() {
        let plan = plan_pools(&caps(true, false), 8);
        assert_eq!(
            plan.dpb_image_count, 0,
            "the picture pool IS the DPB backing"
        );
        assert_eq!(
            plan.picture_count,
            8 + HOLD_HEADROOM,
            "the stream's DPB depth PLUS the consumer-hold headroom — a pool \
             sized to either alone starves (.25 field failure)"
        );
        assert_eq!(
            plan.picture_usage, COINCIDE_USAGE,
            "pool pictures are DPB + decode dst + sampled surface in one"
        );
        assert_eq!(plan.picture_flags, vk::ImageCreateFlags::MUTABLE_FORMAT);
    }

    #[test]
    fn distinct_keeps_a_fixed_dpb_array_and_headrooms_the_output_pool() {
        let plan = plan_pools(&caps(false, true), 17);
        assert_eq!(
            (plan.dpb_image_count, plan.dpb_layers_per_image),
            (1, 17),
            "layered: one array, one layer per slot"
        );
        assert_eq!(plan.dpb_usage, DPB_USAGE);
        assert_eq!(plan.picture_count, 17 + HOLD_HEADROOM);
        assert_eq!(plan.picture_usage, OUTPUT_USAGE);

        let plan = plan_pools(&caps(false, false), 3);
        assert_eq!(
            (plan.dpb_image_count, plan.dpb_layers_per_image),
            (3, 1),
            "separate reference images: one image per slot"
        );
        assert_eq!(plan.picture_count, 3 + HOLD_HEADROOM);
    }

    #[test]
    fn picture_occupancy_frees_only_when_unbound_unpending_and_released() {
        let mut p = Picture {
            image: vk::Image::null(),
            view: vk::ImageView::null(),
            plane_views: [vk::ImageView::null(); 2],
            semaphore: vk::Semaphore::null(),
            value: 0,
            bound: true,
            pending: true,
            held: 2,
        };
        assert!(!p.is_free());
        p.bound = false;
        assert!(!p.is_free(), "pending pictures are not decode targets");
        p.pending = false;
        assert!(!p.is_free(), "held frames are not decode targets");
        p.held = 1;
        assert!(!p.is_free());
        p.held = 0;
        assert!(p.is_free());
    }
}
