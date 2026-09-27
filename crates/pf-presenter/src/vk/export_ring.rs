//! A Vulkan Video picture copied into an exportable dma-buf for the native Wayland lane.
//!
//! Drivers decode only into their own optimal tiling, which no compositor imports. The ring
//! holds a few NV12 or P010 images on a DRM modifier the compositor listed, each exported as
//! one dma-buf, and one copy submit per frame moves both planes across. The copy waits the
//! picture's timeline value, restores its layout and signals `value + 1`: the write-back the
//! decoder waits before it reuses a sampled picture. The fence is waited on the CPU before
//! the commit, so the compositor never reads a half-written buffer.

use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pf_client_core::video::{NativeVkFrame, NativeVkLayout, QueueLock, RawVkFormat};
use std::cell::Cell;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::rc::Rc;

pub(crate) const DRM_FORMAT_NV12: u32 = 0x3231_564e;
pub(crate) const DRM_FORMAT_P010: u32 = 0x3031_3050;
/// One buffer on screen, one queued in the compositor, one being written.
const SLOTS: usize = 3;
/// Bit 63 marks a ring key in the lane, never a VAAPI pool key.
const KEY_RING: u64 = 1 << 63;
/// A copy of a 4K picture takes well under a millisecond; this bounds a wedged queue.
const COPY_WAIT_NS: u64 = 100_000_000;

/// DRM fourcc and Vulkan format of a picture the ring can copy.
pub(crate) fn fourcc_for(format: RawVkFormat) -> Option<(u32, vk::Format)> {
    let f = vk::Format::from_raw(format.0);
    match f {
        vk::Format::G8_B8R8_2PLANE_420_UNORM => Some((DRM_FORMAT_NV12, f)),
        vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 => Some((DRM_FORMAT_P010, f)),
        _ => None,
    }
}

/// Kept by the lane while the compositor holds a slot's buffer; dropping it frees the slot.
pub(crate) struct RingHold(Rc<Cell<bool>>);

impl Drop for RingHold {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

struct Slot {
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    /// (offset, pitch) per memory plane.
    planes: Vec<(u32, u32)>,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Submitted, fence not yet waited.
    in_flight: bool,
    /// The compositor holds the buffer.
    busy: Rc<Cell<bool>>,
}

pub(crate) struct ExportRing {
    device: ash::Device,
    pool: vk::CommandPool,
    /// The family the pool and the copy submits live on.
    qfi: u32,
    slots: Vec<Slot>,
    pub(crate) fourcc: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) modifier: u64,
    /// The lane's feedback generation the modifier was chosen under.
    pub(crate) feedback_gen: u64,
    /// Separates this ring's keys from an earlier ring's.
    generation: u64,
}

/// `(modifier, memory planes)` the driver can create `format` with as a copy target.
///
/// # Safety
///
/// `instance` and `pdev` are live and paired.
unsafe fn driver_modifiers(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<(u64, u32)> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: fn contract; `list`/`fp2` outlive the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut fp2) };
    let mut props = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = props.as_mut_ptr();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: fn contract; `props` holds exactly the reported count and outlives the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut fp2) };
    props.truncate(list.drm_format_modifier_count as usize);
    props
        .iter()
        .filter(|p| {
            p.drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::TRANSFER_DST)
        })
        .map(|p| (p.drm_format_modifier, p.drm_format_modifier_plane_count))
        .collect()
}

/// Whether `format` on `modifier` can be created as a TRANSFER_DST image and exported as a
/// dma-buf.
///
/// # Safety
///
/// `instance` and `pdev` are live and paired.
unsafe fn exportable(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
) -> bool {
    let mut mi = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::TRANSFER_DST)
        .push_next(&mut mi)
        .push_next(&mut ext);
    let mut ep = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ep);
    // SAFETY: fn contract; the chains root locals that outlive the call.
    unsafe {
        instance
            .get_physical_device_image_format_properties2(pdev, &info, &mut props)
            .is_ok()
            && ep
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    }
}

impl ExportRing {
    /// A ring of `SLOTS` images on the first of `wanted` (compositor order) the driver can
    /// create as an exportable copy target with no auxiliary planes.
    ///
    /// # Safety
    ///
    /// Handles are live and paired; `device` enabled external_memory_fd,
    /// external_memory_dma_buf, image_drm_format_modifier and queue_family_foreign.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn new(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        device: &ash::Device,
        ext_mem_fd: &ash::khr::external_memory_fd::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        qfi: u32,
        (fourcc, format): (u32, vk::Format),
        (width, height): (u32, u32),
        wanted: &[u64],
        feedback_gen: u64,
        generation: u64,
    ) -> Result<Self> {
        // SAFETY: fn contract.
        let driver = unsafe { driver_modifiers(instance, pdev, format) };
        let candidates: Vec<u64> = wanted
            .iter()
            .copied()
            .filter(|m| driver.iter().any(|&(dm, planes)| dm == *m && planes == 2))
            // SAFETY: fn contract.
            .filter(|&m| unsafe { exportable(instance, pdev, format, m) })
            .collect();
        let Some(&modifier) = candidates.first() else {
            bail!(
                "no modifier the compositor lists for {fourcc:#010x} is an exportable copy \
                 target here (compositor {wanted:x?}, driver {driver:x?})"
            );
        };
        let pci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(qfi)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: fn contract (live device); `pci` outlives the call.
        let pool =
            unsafe { device.create_command_pool(&pci, None) }.context("vkCreateCommandPool")?;
        let mut ring = Self {
            device: device.clone(),
            pool,
            qfi,
            slots: Vec::new(),
            fourcc,
            width,
            height,
            modifier,
            feedback_gen,
            generation,
        };
        let image_mod = ash::ext::image_drm_format_modifier::Device::new(instance, device);
        for _ in 0..SLOTS {
            // SAFETY: fn contract; each handle lands in `ring` as it is made, so a failure
            // unwinds through Drop.
            unsafe {
                ring.add_slot(
                    ext_mem_fd,
                    &image_mod,
                    mem_props,
                    format,
                    (width, height),
                    modifier,
                )?
            };
        }
        Ok(ring)
    }

    /// # Safety
    ///
    /// As [`Self::new`].
    unsafe fn add_slot(
        &mut self,
        ext_mem_fd: &ash::khr::external_memory_fd::Device,
        image_mod: &ash::ext::image_drm_format_modifier::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        format: vk::Format,
        (width, height): (u32, u32),
        modifier: u64,
    ) -> Result<()> {
        let d = &self.device;
        let mods = [modifier];
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list)
            .push_next(&mut ext);
        // SAFETY: live device; `ci` roots locals that outlive the call.
        let image = unsafe { d.create_image(&ci, None) }.context("vkCreateImage (export)")?;
        // SAFETY: `image` was just created on this device.
        let req = unsafe { d.get_image_memory_requirements(image) };
        let type_index = (0..mem_props.memory_type_count).find(|&i| {
            req.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        });
        let Some(type_index) = type_index else {
            // SAFETY: the never-bound image created above.
            unsafe { d.destroy_image(image, None) };
            bail!("no device-local memory type for the export image");
        };
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        // SAFETY: live device; the chain roots locals that outlive the call.
        let memory = match unsafe { d.allocate_memory(&ai, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: the never-bound image created above.
                unsafe { d.destroy_image(image, None) };
                return Err(e).context("vkAllocateMemory (exportable)");
            }
        };
        let gi = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: fresh image and memory of the required size; the export names live
        // memory allocated exportable.
        let raw = unsafe {
            d.bind_image_memory(image, memory, 0)
                .and_then(|()| ext_mem_fd.get_memory_fd(&gi))
        };
        let raw = match raw {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: unwinding the two objects created above; nothing else uses them.
                unsafe {
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                }
                return Err(e).context("bind + vkGetMemoryFdKHR");
            }
        };
        // SAFETY: the driver hands us a fresh fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: `image` is live and was created with modifier tiling.
        let chosen =
            unsafe { image_mod.get_image_drm_format_modifier_properties(image, &mut props) }
                .map(|()| props.drm_format_modifier);
        let planes: Vec<(u32, u32)> = [
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        ]
        .iter()
        .map(|&aspect| {
            let sub = vk::ImageSubresource {
                aspect_mask: aspect,
                mip_level: 0,
                array_layer: 0,
            };
            // SAFETY: `image` is live; memory-plane aspects are legal on a modifier image.
            let l = unsafe { d.get_image_subresource_layout(image, sub) };
            (l.offset as u32, l.row_pitch as u32)
        })
        .collect();
        let cbi = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: live pool on this device.
        let cmd = unsafe { d.allocate_command_buffers(&cbi) }.map(|v| v[0]);
        // SAFETY: live device.
        let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) };
        // Park before judging, so Drop unwinds whatever did get made.
        self.slots.push(Slot {
            image,
            memory,
            fd,
            planes,
            cmd: *cmd.as_ref().unwrap_or(&vk::CommandBuffer::null()),
            fence: *fence.as_ref().unwrap_or(&vk::Fence::null()),
            in_flight: false,
            busy: Rc::new(Cell::new(false)),
        });
        cmd.context("vkAllocateCommandBuffers")?;
        fence.context("vkCreateFence")?;
        if chosen.context("vkGetImageDrmFormatModifierPropertiesEXT")? != modifier {
            bail!("the driver created the export image on another modifier");
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn key(&self, slot: usize) -> u64 {
        KEY_RING | (self.generation << 8) | slot as u64
    }

    /// `(fd, offset, pitch)` per memory plane of `slot`'s dma-buf, for the lane's import.
    pub(crate) fn planes(&self, slot: usize) -> Vec<(BorrowedFd<'_>, u32, u32)> {
        let s = &self.slots[slot];
        s.planes
            .iter()
            .map(|&(offset, pitch)| (s.fd.as_fd(), offset, pitch))
            .collect()
    }

    /// A slot the compositor does not hold and the lane reports usable.
    pub(crate) fn free_slot(&self, usable: impl Fn(u64) -> bool) -> Option<usize> {
        (0..self.slots.len()).find(|&i| !self.slots[i].busy.get() && usable(self.key(i)))
    }

    /// Mark `slot` held; the lane keeps the returned hold until the compositor releases it.
    pub(crate) fn hold(&self, slot: usize) -> RingHold {
        let busy = &self.slots[slot].busy;
        busy.set(true);
        RingHold(busy.clone())
    }

    /// Copy `frame`'s visible picture into `slot` and wait for the copy. `Err` means nothing
    /// was submitted. `Ok(true)`: the buffer is complete. `Ok(false)`: submitted, but the copy
    /// did not finish in time; the slot stays in flight and the frame is not shown. On any
    /// `Ok` the submit carries the frame's `value + 1` signal.
    ///
    /// # Safety
    ///
    /// `frame`'s handles are live on this device and its guard is held for the call; `queue`
    /// is this device's queue of the ring's family, externally synchronised by `lock`.
    pub(crate) unsafe fn copy(
        &mut self,
        slot: usize,
        frame: &NativeVkFrame,
        queue: vk::Queue,
        lock: &QueueLock,
    ) -> Result<bool> {
        let d = &self.device;
        let own = self.qfi;
        let s = &mut self.slots[slot];
        if s.in_flight {
            // SAFETY: the fence belongs to this slot's last submit.
            unsafe { d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS) }
                .context("an earlier copy never finished")?;
            s.in_flight = false;
        }
        // SAFETY: the fence is unsignalled-or-idle: its submit, if any, completed above.
        unsafe { d.reset_fences(&[s.fence]) }.context("vkResetFences")?;
        let src = vk::Image::from_raw(frame.image);
        let decode_layout = match frame.layout {
            NativeVkLayout::DecodeDst => vk::ImageLayout::VIDEO_DECODE_DST_KHR,
            NativeVkLayout::DecodeDpb => vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        };
        let src_range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .base_array_layer(frame.layer)
            .layer_count(1);
        let dst_range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let barrier = |image, range, from, to, src_q, dst_q, src_a, dst_a| {
            vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(from)
                .new_layout(to)
                .src_queue_family_index(src_q)
                .dst_queue_family_index(dst_q)
                .src_access_mask(src_a)
                .dst_access_mask(dst_a)
        };
        let qfi_ignored = vk::QUEUE_FAMILY_IGNORED;
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        let before = [
            // Picture: decode layout to copy source, after the timeline wait at TRANSFER.
            barrier(
                src,
                src_range,
                decode_layout,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                qfi_ignored,
                qfi_ignored,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_READ,
            ),
            // Export image back from the compositor; its old contents are overwritten.
            barrier(
                s.image,
                dst_range,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                foreign,
                own,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            ),
        ];
        let after = [
            barrier(
                src,
                src_range,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                decode_layout,
                qfi_ignored,
                qfi_ignored,
                vk::AccessFlags::empty(),
                vk::AccessFlags::empty(),
            ),
            // Hand the buffer to the compositor.
            barrier(
                s.image,
                dst_range,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::GENERAL,
                own,
                foreign,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::empty(),
            ),
        ];
        let layers = |aspect, layer| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: layer,
            layer_count: 1,
        };
        let regions = [
            vk::ImageCopy {
                src_subresource: layers(vk::ImageAspectFlags::PLANE_0, frame.layer),
                src_offset: vk::Offset3D {
                    x: frame.crop_x as i32,
                    y: frame.crop_y as i32,
                    z: 0,
                },
                dst_subresource: layers(vk::ImageAspectFlags::PLANE_0, 0),
                dst_offset: vk::Offset3D::default(),
                extent: vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                },
            },
            vk::ImageCopy {
                src_subresource: layers(vk::ImageAspectFlags::PLANE_1, frame.layer),
                src_offset: vk::Offset3D {
                    x: (frame.crop_x / 2) as i32,
                    y: (frame.crop_y / 2) as i32,
                    z: 0,
                },
                dst_subresource: layers(vk::ImageAspectFlags::PLANE_1, 0),
                dst_offset: vk::Offset3D::default(),
                extent: vk::Extent3D {
                    width: self.width / 2,
                    height: self.height / 2,
                    depth: 1,
                },
            },
        ];
        let sem = vk::Semaphore::from_raw(frame.semaphore);
        let wait_values = [frame.semaphore_value];
        let signal_values = [frame.semaphore_value + 1];
        let sems = [sem];
        let stages = [vk::PipelineStageFlags::TRANSFER];
        let cmds = [s.cmd];
        // SAFETY: the slot's command buffer is idle (fence waited above); every handle it
        // names is live per the fn contract; builders are locals outliving each call.
        unsafe {
            d.begin_command_buffer(
                s.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            d.cmd_pipeline_barrier(
                s.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &before,
            );
            d.cmd_copy_image(
                s.cmd,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                s.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
            d.cmd_pipeline_barrier(
                s.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &after,
            );
            d.end_command_buffer(s.cmd)?;
        }
        let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&sems)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&sems)
            .push_next(&mut timeline);
        {
            let _q = lock.guard();
            // SAFETY: fn contract (queue external sync held by `_q`); the submit's arrays
            // are locals that outlive the call.
            unsafe { d.queue_submit(queue, &[submit], s.fence) }.context("vkQueueSubmit (copy)")?;
        }
        s.in_flight = true;
        // SAFETY: the fence belongs to the submit above.
        match unsafe { d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS) } {
            Ok(()) => {
                s.in_flight = false;
                Ok(true)
            }
            Err(vk::Result::TIMEOUT) => Ok(false),
            Err(e) => Err(e).context("vkWaitForFences (copy)"),
        }
    }
}

impl Drop for ExportRing {
    fn drop(&mut self) {
        let d = &self.device;
        // SAFETY: each in-flight fence is waited before its image and command buffer go;
        // the compositor keeps its own reference to a dma-buf it still shows.
        unsafe {
            for s in &self.slots {
                if s.in_flight && s.fence != vk::Fence::null() {
                    let _ = d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS);
                }
            }
            for s in self.slots.drain(..) {
                if s.fence != vk::Fence::null() {
                    d.destroy_fence(s.fence, None);
                }
                d.destroy_image(s.image, None);
                d.free_memory(s.memory, None);
            }
            d.destroy_command_pool(self.pool, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the two 4:2:0 layouts the compositor can take map to a fourcc.
    #[test]
    fn the_ring_copies_nv12_and_p010_only() {
        let nv12 = RawVkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw());
        let p010 = RawVkFormat(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16.as_raw());
        let yuv444 = RawVkFormat(vk::Format::G8_B8_R8_3PLANE_444_UNORM.as_raw());
        assert_eq!(fourcc_for(nv12).map(|f| f.0), Some(DRM_FORMAT_NV12));
        assert_eq!(fourcc_for(p010).map(|f| f.0), Some(DRM_FORMAT_P010));
        assert_eq!(fourcc_for(yuv444), None);
    }

    /// Ring keys never collide with a VAAPI pool key (high half = pool generation) and
    /// stay distinct across ring generations.
    #[test]
    fn ring_keys_live_in_their_own_namespace() {
        let key = |generation: u64, slot: u64| KEY_RING | (generation << 8) | slot;
        assert_ne!(key(1, 0), key(2, 0));
        assert_ne!(key(1, 0), key(1, 1));
        assert!(key(1, 2) & KEY_RING != 0);
        assert_eq!(
            (7u64 << 32 | 3) & KEY_RING,
            0,
            "a VAAPI pool key never sets bit 63"
        );
    }
}
