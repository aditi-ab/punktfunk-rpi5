//! The NV12→RGBA color-space-conversion pass: coefficient rows from the stream's CICP
//! signaling (the GL presenter's `yuv_to_rgb`, folded into row form for the shader's
//! push constants) and the graphics pipeline that renders the two imported planes into
//! the presenter's video image.
//!
//! Deliberately NOT `VK_KHR_sampler_ycbcr_conversion`: the ext can't express the PQ path
//! coming with the HDR phase and its range handling varies by driver — these are the
//! proven, unit-tested coefficients (plan §5.2).

use anyhow::{Context as _, Result};
use ash::vk;

// The coefficient math lives in pf-client-core next to `ColorDesc` (one tested
// implementation shared with the Windows client's D3D11 constant buffer and mirrored by the
// Apple client's Swift port); re-exported here so presenter callers keep their import path.
pub use pf_client_core::video::csc_rows;

/// The pass objects (everything except the per-video-size framebuffer, which lives with
/// the video image). Destroyed explicitly via [`CscPass::destroy`] from the presenter's
/// `Drop` — no device handle is stored here.
pub struct CscPass {
    pub render_pass: vk::RenderPass,
    pub set_layout: vk::DescriptorSetLayout,
    pub pipeline_layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    pub desc_pool: vk::DescriptorPool,
    pub desc_set: vk::DescriptorSet,
    pub sampler: vk::Sampler,
}

impl CscPass {
    /// `attachment_format` = the video image's format: R8G8B8A8 for SDR, a 10-bit
    /// format when the pass writes PQ (8 bits would band the PQ curve visibly).
    pub fn new(device: &ash::Device, attachment_format: vk::Format) -> Result<CscPass> {
        Self::build(
            device,
            attachment_format,
            2,
            include_bytes!("../shaders/nv12_csc.frag.spv"),
        )
    }

    /// The planar 3-plane variant (separate Cb/Cr R8 planes — the PyroWave decode
    /// output, design/pyrowave-codec-plan.md §4.5). Same push-constant contract.
    #[cfg(feature = "pyrowave")]
    pub fn new_planar(device: &ash::Device, attachment_format: vk::Format) -> Result<CscPass> {
        Self::build(
            device,
            attachment_format,
            3,
            include_bytes!("../shaders/planar_csc.frag.spv"),
        )
    }

    fn build(
        device: &ash::Device,
        attachment_format: vk::Format,
        plane_bindings: u32,
        frag_spv: &[u8],
    ) -> Result<CscPass> {
        // One color attachment: the presenter's video image. Content is fully
        // overwritten (DONT_CARE load), and the pass ends in TRANSFER_SRC so the
        // existing letterbox blit consumes it with no extra barrier.
        let attachment = [vk::AttachmentDescription::default()
            .format(attachment_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_ref)];
        // Conservative scopes, matching the presenter's per-frame barrier granularity.
        let deps = [
            vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
        ];
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let render_pass = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpass)
                    .dependencies(&deps),
                None,
            )
        }
        .context("CSC render pass")?;

        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let sampler = unsafe {
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

        let samplers = [sampler];
        let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..plane_bindings)
            .map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                    .immutable_samplers(&samplers)
            })
            .collect();
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let set_layouts = [set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .size(64)]; // three vec4 rows + a params vec4 (mode, tonemap peak)
                        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
                        // type and live for the call, and every builder struct is a local that outlives it.
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }?;

        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(plane_bindings)];
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        let desc_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&set_layouts),
            )
        }?[0];

        let pipeline = build_fullscreen_pipeline(
            device,
            render_pass,
            pipeline_layout,
            frag_spv,
            false, // opaque — the CSC output IS the video
        )?;

        Ok(CscPass {
            render_pass,
            set_layout,
            pipeline_layout,
            pipeline,
            desc_pool,
            desc_set,
            sampler,
        })
    }

    /// Point the descriptor set at this frame's plane views. Only safe while no
    /// submitted command buffer references the set — the presenter's single in-flight
    /// fence is waited before every record, which covers it.
    pub fn bind_planes(&self, device: &ash::Device, luma: vk::ImageView, chroma: vk::ImageView) {
        let infos = [luma, chroma].map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        });
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[0]),
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[1]),
        ];
        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// Planar variant of [`bind_planes`](Self::bind_planes): three single-component
    /// plane views in GENERAL layout (the pyrowave decode leaves them there; same
    /// fence-wait safety contract).
    #[cfg(feature = "pyrowave")]
    pub fn bind_planes_planar(&self, device: &ash::Device, planes: [vk::ImageView; 3]) {
        let infos = planes.map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::GENERAL)]
        });
        let writes = [0u32, 1, 2].map(|b| {
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(b)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[b as usize])
        });
        // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns
        // and has begun, referencing handles it also owns; nothing is submitted until the
        // recording is ended.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    pub fn destroy(&self, device: &ash::Device) {
        // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and the
        // GPU is known idle for them (the fence/queue-wait on the path here, or the swapchain
        // being retired), which is the obligation that makes a destroy sound rather than the
        // handle merely being non-null.
        unsafe {
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.desc_pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
            device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// A bufferless fullscreen-triangle pipeline over `fullscreen.vert` + the given
/// fragment SPIR-V, dynamic viewport/scissor. `blend` = premultiplied-alpha over the
/// destination (the overlay composite); `false` = opaque write (the CSC pass). Shared
/// by both passes — the geometry and states are identical.
pub(crate) fn build_fullscreen_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    frag_spv: &[u8],
    blend: bool,
) -> Result<vk::Pipeline> {
    // Committed SPIR-V (shaders/build.sh) — include_bytes! alignment is unspecified, so
    // read_spv copies into aligned Vec<u32>s.
    let vert = ash::util::read_spv(&mut std::io::Cursor::new(
        &include_bytes!("../shaders/fullscreen.vert.spv")[..],
    ))?;
    let frag = ash::util::read_spv(&mut std::io::Cursor::new(frag_spv))?;
    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this type
    // and live for the call, and every builder struct is a local that outlives it.
    let vert_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert), None)
    }?;
    // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this type
    // and live for the call, and every builder struct is a local that outlives it.
    let frag_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag), None)
    };
    let frag_mod = match frag_mod {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and
            // the GPU is known idle for them (the fence/queue-wait on the path here, or the
            // swapchain being retired), which is the obligation that makes a destroy sound rather
            // than the handle merely being non-null.
            unsafe { device.destroy_shader_module(vert_mod, None) };
            return Err(e).context("fragment shader module");
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_mod)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_mod)
            .name(entry),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default(); // bufferless
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    // Dynamic viewport/scissor: the video size changes with the stream mode, the
    // pipeline must not bake one in.
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [if blend {
        // Premultiplied alpha over the destination (Skia surfaces are premultiplied).
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
    } else {
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    }];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .render_pass(render_pass);
    let pipeline =
        // SAFETY: per the Vulkan contract above - a create/allocate call on the live device, over
        // builder structs that are locals outliving the call; the handle it returns is owned by
        // the value being built here.
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
            .map_err(|(_, e)| e)
            .context("CSC pipeline");
    // SAFETY: per the Vulkan contract above - this destroys objects this type owns, and the GPU is
    // known idle for them (the fence/queue-wait on the path here, or the swapchain being retired),
    // which is the obligation that makes a destroy sound rather than the handle merely being non-
    // null.
    unsafe {
        device.destroy_shader_module(vert_mod, None);
        device.destroy_shader_module(frag_mod, None);
    }
    Ok(pipeline?[0])
}
