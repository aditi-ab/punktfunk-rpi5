//! Low-level GPU helpers: memory allocation, image barriers, AVVkFrame sync, geometry.

use super::Presenter;
use anyhow::{Context as _, Result};
use ash::vk;

impl Presenter {
    /// Wait the in-flight fence: OUR command buffers are done (staging, video image,
    /// old-swapchain images). Deliberately NOT `vkDeviceWaitIdle` — the pump thread
    /// submits FFmpeg's Vulkan decode work concurrently, and wait-idle's external-sync
    /// rule over every device queue would race it (observed as a resize crash).
    pub(super) fn quiesce_own(&mut self) -> Result<()> {
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
        }
        Ok(())
    }
    pub(super) fn allocate(
        &self,
        reqs: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory> {
        let type_index = (0..self.mem_props.memory_type_count)
            .find(|&i| {
                reqs.memory_type_bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .with_context(|| format!("no memory type for {flags:?}"))?;
        unsafe {
            self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("vkAllocateMemory")
    }
}

/// The Contain-fit letterbox: video (vw×vh) into the swapchain extent, centered.
pub(super) fn letterbox(extent: vk::Extent2D, vw: u32, vh: u32) -> (vk::Offset3D, vk::Offset3D) {
    let (ew, eh) = (f64::from(extent.width), f64::from(extent.height));
    let scale = (ew / f64::from(vw.max(1))).min(eh / f64::from(vh.max(1)));
    let dw = (f64::from(vw) * scale).round();
    let dh = (f64::from(vh) * scale).round();
    let ox = ((ew - dw) / 2.0).floor() as i32;
    let oy = ((eh - dh) / 2.0).floor() as i32;
    (
        vk::Offset3D { x: ox, y: oy, z: 0 },
        vk::Offset3D {
            x: (ox + dw as i32).min(extent.width as i32),
            y: (oy + dh as i32).min(extent.height as i32),
            z: 1,
        },
    )
}

pub(super) fn subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

pub(super) fn subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// Acquire a Vulkan-Video frame's image from the decode queue family (EXCLUSIVE
/// sharing) and transition it for sampling. `src_qf == dst_qf` (or IGNORED/CONCURRENT)
/// degrades to a plain layout transition. The matching decode-side acquire happens in
/// FFmpeg, keyed off the queue_family we write back after submission.
///
/// `srcStage` is FRAGMENT_SHADER — NOT TOP_OF_PIPE — deliberately: the submit waits the
/// frame's decode-complete timeline semaphore with `wait_dst_stage_mask =
/// FRAGMENT_SHADER`, and a semaphore wait only orders operations whose first sync scope
/// INTERSECTS that mask (the dependency-chain rule). With TOP_OF_PIPE the barrier's
/// layout transition (VIDEO_DECODE_DST/DPB → SHADER_READ_ONLY) formed no chain with the
/// wait and could execute while the decode queue was still writing the image. On RADV
/// that transition physically touches the image (metadata/decompression), so the race
/// showed as green/yellow block corruption exactly at freshly-decoded (damaged) regions
/// — the Steam Deck cursor-trail artifact. NVIDIA treats the transition as a no-op,
/// which is why the same code looked clean there.
pub(super) fn vkframe_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    src_qf: u32,
    dst_qf: u32,
) {
    let (src, dst) = if src_qf == dst_qf || src_qf == vk::QUEUE_FAMILY_IGNORED {
        (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
    } else {
        (src_qf, dst_qf)
    };
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(src)
        .dst_queue_family_index(dst)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// Acquire an imported D3D11 texture from the EXTERNAL queue family as a copy source.
/// The keyed mutex on the submit is the actual cross-API ordering; per the
/// external-memory rules an UNDEFINED-old-layout transition on externally-bound memory
/// preserves the contents (unlike ordinary images), so this is purely the
/// layout/ownership hop.
#[cfg(windows)]
pub(super) fn external_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
        .dst_queue_family_index(qfi)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// Acquire a dmabuf plane image from its foreign owner (the VAAPI decoder): queue-family
/// transfer FOREIGN → ours, UNDEFINED → SHADER_READ_ONLY (content is preserved across
/// the transfer regardless of the UNDEFINED old-layout, per the external-memory rules).
#[cfg(target_os = "linux")]
pub(super) fn foreign_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        .dst_queue_family_index(qfi)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// A full-subresource layout transition with the conservative ALL_COMMANDS/TRANSFER
/// scopes this transfer-only pipeline needs (per-frame granularity, not per-stage).
pub(super) fn barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(subresource_range());
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_pillarboxes_a_wide_window() {
        // 16:10 video in a 21:9-ish window: full height, centered horizontally.
        let (a, b) = letterbox(
            vk::Extent2D {
                width: 3440,
                height: 1440,
            },
            1280,
            800,
        );
        assert_eq!((a.y, b.y), (0, 1440));
        assert_eq!(b.x - a.x, 2304); // 1280 * (1440/800)
        assert_eq!(a.x, (3440 - 2304) / 2);
    }

    #[test]
    fn letterbox_matches_exact_fit() {
        let (a, b) = letterbox(
            vk::Extent2D {
                width: 1280,
                height: 800,
            },
            1280,
            800,
        );
        assert_eq!((a.x, a.y), (0, 0));
        assert_eq!((b.x, b.y), (1280, 800));
    }
}
