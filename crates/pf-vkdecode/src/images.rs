//! DPB + decode-output image pools, caps-driven for BOTH DPB arrangements:
//!
//! - **coincide** (`DPB_AND_OUTPUT_COINCIDE`, RADV's shape): the decode output IS
//!   the DPB picture — one pool, every slot usable both as setup/reference and as
//!   the frame handed to the presenter.
//! - **distinct** (NVIDIA's shape): a reference-only DPB pool plus a small ring of
//!   output images the decoder writes `dst` into.
//!
//! Within either mode the DPB is **layered** (one image, one array layer per slot —
//! mandatory when the driver lacks `SEPARATE_REFERENCE_IMAGES`) or **per-slot**
//! (one image each). [`plan_pools`] is the pure decision table; [`ImagePool`] is
//! the thin Vulkan half.
//!
//! Presenter-facing surfaces (outputs) carry `MUTABLE_FORMAT` (advertised by the
//! driver — [`crate::caps::derive_caps`] refuses otherwise; nothing here aliases,
//! so no `ALIAS`) so per-plane `R8`/`R8G8` views exist for the presenter's
//! sampling path, one TIMELINE semaphore per output slot signals decode
//! completion, and the conformance-window crop rides the frame struct — the
//! 1088-row smear class dies by construction because the consumer is TOLD the
//! crop instead of guessing from the pool shape. Images are allocated at the
//! `pictureAccessGranularity`-rounded extent; the stream's coded extent rides
//! separately for per-picture resources.

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

/// Distinct-mode output ring depth: decode-ahead is one-in/one-out under the
/// punktfunk envelope, so a small ring covers pipelining plus a frame in the
/// presenter's hands.
pub const OUTPUT_RING: u32 = 4;

/// The pure pool shape for one (caps, slot-count) pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolPlan {
    pub dpb_image_count: u32,
    pub dpb_layers_per_image: u32,
    pub dpb_usage: vk::ImageUsageFlags,
    pub dpb_flags: vk::ImageCreateFlags,
    /// 0 in coincide mode — outputs ARE the DPB slots.
    pub output_image_count: u32,
    pub output_usage: vk::ImageUsageFlags,
    pub output_flags: vk::ImageCreateFlags,
    /// Semaphore/query/command ring size: DPB slots when coincide, the output
    /// ring otherwise.
    pub output_slots: u32,
}

/// Decide the pool shape. Pure — the four caps combinations are unit-tested below.
///
/// The usages are exactly the ones the caps derivation validated against the
/// driver's advertised envelope ([`DPB_USAGE`]/[`OUTPUT_USAGE`]/[`COINCIDE_USAGE`]);
/// presenter-facing images add only `MUTABLE_FORMAT` (advertised — derive_caps
/// gates on it; no `ALIAS`: nothing aliases these images).
pub fn plan_pools(caps: &DecodeCaps, dpb_slots: u32, output_ring: u32) -> PoolPlan {
    let (dpb_image_count, dpb_layers_per_image) = if caps.layered_dpb {
        (1, dpb_slots)
    } else {
        (dpb_slots, 1)
    };
    let presented_flags = vk::ImageCreateFlags::MUTABLE_FORMAT;
    if caps.coincide {
        PoolPlan {
            dpb_image_count,
            dpb_layers_per_image,
            dpb_usage: COINCIDE_USAGE,
            dpb_flags: presented_flags,
            output_image_count: 0,
            output_usage: vk::ImageUsageFlags::empty(),
            output_flags: vk::ImageCreateFlags::empty(),
            output_slots: dpb_slots,
        }
    } else {
        PoolPlan {
            dpb_image_count,
            dpb_layers_per_image,
            dpb_usage: DPB_USAGE,
            dpb_flags: vk::ImageCreateFlags::empty(),
            output_image_count: output_ring,
            output_usage: OUTPUT_USAGE,
            output_flags: presented_flags,
            output_slots: output_ring,
        }
    }
}

/// One presenter-facing output slot: the image (a DPB slot's in coincide mode, a
/// ring image otherwise), its full + per-plane views, and the timeline semaphore
/// each decode into this slot signals.
pub(crate) struct OutputSlot {
    pub image: vk::Image,
    /// The image's array layer this slot occupies (barriers target it; the VIEWS
    /// already select it, so picture resources use `base_array_layer` 0).
    pub layer: u32,
    /// Full-picture view in the pool format (what decode binds as `dst`).
    pub view: vk::ImageView,
    /// `R8_UNORM` / `R8G8_UNORM` plane views for the presenter's sampler path.
    pub plane_views: [vk::ImageView; 2],
    pub semaphore: vk::Semaphore,
    /// Last timeline value signalled on `semaphore` (0 = never used).
    pub value: u64,
}

/// The Vulkan half: images, memory, views, semaphores. Destroys everything it
/// created on drop (null-safe, so a half-built pool from a failed create unwinds
/// cleanly).
pub(crate) struct ImagePool {
    device: ash::Device,
    pub(crate) coincide: bool,
    /// The STREAM's coded extent — what per-picture resources report.
    pub(crate) coded_extent: vk::Extent2D,
    /// The allocation extent: `coded_extent` rounded up to the device's
    /// `pictureAccessGranularity` (images only; never leaks into picture params).
    image_extent: vk::Extent2D,
    images: Vec<vk::Image>,
    memory: Vec<vk::DeviceMemory>,
    /// Per-DPB-slot full view (setup/reference binding).
    dpb_views: Vec<vk::ImageView>,
    /// Per-DPB-slot (image index, array layer) for barrier targeting.
    dpb_location: Vec<(usize, u32)>,
    pub(crate) outputs: Vec<OutputSlot>,
}

impl ImagePool {
    /// Create the pools for `plan`: images at the granularity-rounded
    /// `image_extent`, picture metadata at the stream's `coded_extent`.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        coded_extent: vk::Extent2D,
        std_profile_idc: hh::StdVideoH264ProfileIdc,
    ) -> Result<Self, AllocError> {
        let mut pool = Self {
            device: dev.ash().clone(),
            coincide: caps.coincide,
            coded_extent,
            image_extent: caps.aligned_extent(coded_extent),
            images: Vec::new(),
            memory: Vec::new(),
            dpb_views: Vec::new(),
            dpb_location: Vec::new(),
            outputs: Vec::new(),
        };
        // SAFETY: caller's contract; on error `pool` drops and unwinds whatever
        // half was built (Drop is null-safe and destroys only owned objects).
        unsafe { pool.build(dev, caps, plan, std_profile_idc)? };
        Ok(pool)
    }

    /// # Safety
    ///
    /// As [`Self::create`].
    unsafe fn build(
        &mut self,
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        plan: &PoolPlan,
        std_profile_idc: hh::StdVideoH264ProfileIdc,
    ) -> Result<(), AllocError> {
        let families = dev.sharing_families();

        // DPB images + per-slot views.
        for _ in 0..plan.dpb_image_count {
            // SAFETY: fn contract (live device).
            let (image, memory) = unsafe {
                create_video_image(
                    dev,
                    caps.dpb_format,
                    self.image_extent,
                    plan.dpb_layers_per_image,
                    plan.dpb_usage,
                    plan.dpb_flags,
                    &families,
                    std_profile_idc,
                )?
            };
            self.images.push(image);
            self.memory.push(memory);
        }
        let dpb_slots = plan.dpb_image_count * plan.dpb_layers_per_image;
        for slot in 0..dpb_slots {
            let (image_index, layer) = if plan.dpb_image_count == 1 {
                (0usize, slot)
            } else {
                (slot as usize, 0u32)
            };
            // SAFETY: `image` was created above with at least `layer + 1` layers.
            let view = unsafe {
                create_view(
                    &self.device,
                    self.images[image_index],
                    caps.dpb_format,
                    vk::ImageAspectFlags::COLOR,
                    layer,
                )?
            };
            self.dpb_views.push(view);
            self.dpb_location.push((image_index, layer));
        }

        // Output slots: over the DPB slots (coincide) or over a fresh ring.
        let output_targets: Vec<(vk::Image, u32)> = if caps.coincide {
            self.dpb_location
                .iter()
                .map(|&(image_index, layer)| (self.images[image_index], layer))
                .collect()
        } else {
            let mut targets = Vec::new();
            for _ in 0..plan.output_image_count {
                // SAFETY: fn contract (live device).
                let (image, memory) = unsafe {
                    create_video_image(
                        dev,
                        caps.output_format,
                        self.image_extent,
                        1,
                        plan.output_usage,
                        plan.output_flags,
                        &families,
                        std_profile_idc,
                    )?
                };
                self.images.push(image);
                self.memory.push(memory);
                targets.push((image, 0));
            }
            targets
        };

        for (image, layer) in output_targets {
            // SAFETY: `image` exists with `layer` in range (holds for all four
            // creates below); the formats are plane-compatible with the pool
            // format (NV12: R8 + R8G8) and the image carries MUTABLE_FORMAT for
            // the reinterpreting views.
            let view = unsafe {
                create_view(
                    &self.device,
                    image,
                    caps.output_format,
                    vk::ImageAspectFlags::COLOR,
                    layer,
                )?
            };
            // SAFETY: as above.
            let plane_y = unsafe {
                create_view(
                    &self.device,
                    image,
                    vk::Format::R8_UNORM,
                    vk::ImageAspectFlags::PLANE_0,
                    layer,
                )?
            };
            // SAFETY: as above.
            let plane_uv = unsafe {
                create_view(
                    &self.device,
                    image,
                    vk::Format::R8G8_UNORM,
                    vk::ImageAspectFlags::PLANE_1,
                    layer,
                )?
            };
            let mut type_info = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let sem_ci = vk::SemaphoreCreateInfo::default().push_next(&mut type_info);
            // SAFETY: live device; timelineSemaphore is enabled per the
            // DeviceHandles feature contract.
            let semaphore = unsafe { self.device.create_semaphore(&sem_ci, None)? };
            self.outputs.push(OutputSlot {
                image,
                layer,
                view,
                plane_views: [plane_y, plane_uv],
                semaphore,
                value: 0,
            });
        }
        Ok(())
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

impl Drop for ImagePool {
    fn drop(&mut self) {
        // SAFETY: every handle below was created by this pool on this (still-live,
        // per the DeviceHandles contract) device; the owning decoder drains GPU
        // work before dropping state. vkDestroy*/vkFree ignore NULL handles.
        unsafe {
            for view in self.dpb_views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            for out in self.outputs.drain(..) {
                self.device.destroy_image_view(out.view, None);
                self.device.destroy_image_view(out.plane_views[0], None);
                self.device.destroy_image_view(out.plane_views[1], None);
                self.device.destroy_semaphore(out.semaphore, None);
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
    fn coincide_layered_is_one_dual_use_array_with_no_output_ring() {
        let plan = plan_pools(&caps(true, true), 5, OUTPUT_RING);
        assert_eq!((plan.dpb_image_count, plan.dpb_layers_per_image), (1, 5));
        assert_eq!(
            plan.dpb_usage, COINCIDE_USAGE,
            "coincide: the DPB image is the decode output AND the sampled surface"
        );
        assert_eq!(
            plan.dpb_flags,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
            "plane views need MUTABLE_FORMAT; nothing aliases, so no ALIAS"
        );
        assert_eq!(plan.output_image_count, 0);
        assert_eq!(plan.output_slots, 5, "one output slot per DPB slot");
    }

    #[test]
    fn coincide_separate_is_one_dual_use_image_per_slot() {
        let plan = plan_pools(&caps(true, false), 5, OUTPUT_RING);
        assert_eq!((plan.dpb_image_count, plan.dpb_layers_per_image), (5, 1));
        assert_eq!(plan.output_image_count, 0);
        assert_eq!(plan.output_slots, 5);
    }

    #[test]
    fn distinct_layered_is_a_reference_only_array_plus_an_output_ring() {
        let plan = plan_pools(&caps(false, true), 17, OUTPUT_RING);
        assert_eq!((plan.dpb_image_count, plan.dpb_layers_per_image), (1, 17));
        assert_eq!(
            plan.dpb_usage, DPB_USAGE,
            "distinct: the DPB is never sampled and never a decode dst"
        );
        assert_eq!(plan.dpb_flags, vk::ImageCreateFlags::empty());
        assert_eq!(plan.output_image_count, OUTPUT_RING);
        assert_eq!(plan.output_usage, OUTPUT_USAGE);
        assert_eq!(
            plan.output_flags,
            vk::ImageCreateFlags::MUTABLE_FORMAT,
            "plane views need MUTABLE_FORMAT; nothing aliases, so no ALIAS"
        );
        assert_eq!(plan.output_slots, OUTPUT_RING);
    }

    #[test]
    fn distinct_separate_is_per_slot_reference_images_plus_the_ring() {
        let plan = plan_pools(&caps(false, false), 3, 2);
        assert_eq!((plan.dpb_image_count, plan.dpb_layers_per_image), (3, 1));
        assert_eq!(plan.output_image_count, 2);
        assert_eq!(plan.output_slots, 2);
    }
}
