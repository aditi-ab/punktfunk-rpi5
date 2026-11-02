//! Premultiplied-alpha overlay quad over the swapchain after the video blit.
//!
//! Views and framebuffers are per swapchain image ([`OverlayTargets`]): they belong to the
//! swapchain's `Targets`, built and destroyed with it.

use super::gpu::subresource_range;
use super::OverlayPipe;
use crate::csc::{build_fullscreen_pipeline, color_pass, fill_or_destroy};
use anyhow::{Context as _, Result};
use ash::vk;

impl OverlayPipe {
    /// `pq`: the target is an HDR10 swapchain, so the sRGB UI is re-encoded as PQ. A failed
    /// step destroys what was made.
    pub(super) fn new(device: &ash::Device, format: vk::Format, pq: bool) -> Result<OverlayPipe> {
        let fill = |p: &mut OverlayPipe| p.fill(device, format, pq);
        fill_or_destroy(device, fill, Self::destroy)
    }

    fn fill(&mut self, device: &ash::Device, format: vk::Format, pq: bool) -> Result<()> {
        // This pass owns the last layout transition on overlay frames (LOAD, end PRESENT-ready).
        self.pass.render_pass = color_pass(
            device,
            format,
            vk::AttachmentLoadOp::LOAD,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::PRESENT_SRC_KHR,
        )
        .context("overlay render pass")?;

        // SAFETY: `device` is live; CreateInfo is a local that outlives the call.
        self.pass.sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }?;
        let samplers = [self.pass.sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        // SAFETY: `device` is live; `bindings` and `samplers` outlive the call.
        self.pass.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let set_layouts = [self.pass.set_layout];
        // SAFETY: `device` is live; `set_layouts` outlives the call.
        self.pass.pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                None,
            )
        }?;
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        // SAFETY: `device` is live; `pool_sizes` outlives the call.
        self.pass.desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        // SAFETY: `device` and `desc_pool` are live; `set_layouts` outlives the call.
        self.desc_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pass.desc_pool)
                    .set_layouts(&set_layouts),
            )
        }?[0];
        self.pass.pipeline = build_fullscreen_pipeline(
            device,
            self.pass.render_pass,
            self.pass.pipeline_layout,
            if pq {
                pf_client_core::video_csc_spv::OVERLAY_PQ_FRAG
            } else {
                pf_client_core::video_csc_spv::OVERLAY_FRAG
            },
            true, // both overlay shaders write premultiplied alpha
        )?;
        Ok(())
    }

    /// [`Pass::destroy`](super::gpu::Pass::destroy): the set goes with the pool.
    pub(super) fn destroy(&mut self, device: &ash::Device) {
        self.pass.destroy(device);
    }
}

/// One view and framebuffer per swapchain image, on the overlay's render pass. The scale and
/// direct passes draw through them too: `csc::color_pass` keeps those passes compatible.
#[derive(Default)]
pub(super) struct OverlayTargets {
    pub(super) views: Vec<vk::ImageView>,
    pub(super) framebuffers: Vec<vk::Framebuffer>,
}

impl OverlayTargets {
    /// A view and a framebuffer on `pipe`'s render pass for each of `images`. A failure
    /// leaves what was made here, for [`OverlayTargets::destroy`].
    pub(super) fn fill(
        &mut self,
        device: &ash::Device,
        pipe: &OverlayPipe,
        images: &[vk::Image],
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Result<()> {
        for &image in images {
            // SAFETY: `image` is a live swapchain image; CreateInfo outlives the call.
            let view = unsafe {
                device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .subresource_range(subresource_range()),
                    None,
                )
            }?;
            self.views.push(view);
            let attachments = [view];
            // SAFETY: `view` and `pipe.pass.render_pass` are live; CreateInfo outlives the call.
            let fb = unsafe {
                device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(pipe.pass.render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }?;
            self.framebuffers.push(fb);
        }
        Ok(())
    }

    /// Only once the GPU is done with every command buffer that drew through them.
    pub(super) fn destroy(&mut self, device: &ash::Device) {
        // SAFETY: these views and framebuffers are owned here; the GPU is idle for them
        // (the caller's fence or queue wait, or the swapchain already retired).
        unsafe {
            for fb in self.framebuffers.drain(..) {
                device.destroy_framebuffer(fb, None);
            }
            for v in self.views.drain(..) {
                device.destroy_image_view(v, None);
            }
        }
    }
}
