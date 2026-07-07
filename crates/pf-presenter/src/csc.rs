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
use pf_client_core::video::ColorDesc;

/// The push-constant block's matrix half: three vec4 rows,
/// `rgb[i] = dot(r[i].xyz, yuv) + r[i].w` — bit-depth exact.
///
/// `depth` picks the limited-range code points (8-bit: 16/235/240 over 255; 10-bit:
/// 64/940/960 over 1023 — NOT the same normalized values, the difference is ~half a
/// code). `msb_packed` folds in the P010/X6 packing factor: 10 significant bits live in
/// the MSBs of 16, so a UNORM16 sample reads `code·64/65535` — multiplying by
/// `65535/65472` recovers exact `code/1023`.
pub fn csc_rows(desc: ColorDesc, depth: u8, msb_packed: bool) -> [[f32; 4]; 3] {
    // BT.601 (5/6), BT.2020 (9/10); everything else — incl. unspecified — is the host's
    // BT.709 SDR default (mirrors the software path's swscale coefficient choice).
    let (kr, kb) = match desc.matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let max = f64::from((1u32 << depth) - 1); // 255 / 1023
    let step = f64::from(1u32 << (depth - 8)); // code points per 8-bit step: 1 / 4
    let pack = if msb_packed { 65535.0 / 65472.0 } else { 1.0 };
    let (sy, oy, sc) = if desc.full_range {
        (pack, 0.0f64, pack)
    } else {
        (
            pack * max / (219.0 * step),
            -(16.0 * step) / max,
            pack * max / (224.0 * step),
        )
    };
    // rgb = M * (yuv + off) = M*yuv + M*off — rows of M with the offset dot folded into
    // w. `yuv` is the SAMPLED (packed) value, so the offsets divide by the packing
    // factor to land on the same scale.
    let off = [oy / pack, -0.5 / pack, -0.5 / pack];
    let m = [
        [sy, 0.0, 2.0 * (1.0 - kr) * sc],
        [
            sy,
            -2.0 * (1.0 - kb) * kb / kg * sc,
            -2.0 * (1.0 - kr) * kr / kg * sc,
        ],
        [sy, 2.0 * (1.0 - kb) * sc, 0.0],
    ];
    core::array::from_fn(|r| {
        let w: f64 = (0..3).map(|c| m[r][c] * off[c]).sum();
        [m[r][0] as f32, m[r][1] as f32, m[r][2] as f32, w as f32]
    })
}

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
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&samplers),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&samplers),
        ];
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
            .descriptor_count(2)];
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
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
            include_bytes!("../shaders/nv12_csc.frag.spv"),
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
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    pub fn destroy(&self, device: &ash::Device) {
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
    let vert_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert), None)
    }?;
    let frag_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag), None)
    };
    let frag_mod = match frag_mod {
        Ok(m) => m,
        Err(e) => {
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
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
            .map_err(|(_, e)| e)
            .context("CSC pipeline");
    unsafe {
        device.destroy_shader_module(vert_mod, None);
        device.destroy_shader_module(frag_mod, None);
    }
    Ok(pipeline?[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(matrix: u8, full_range: bool) -> ColorDesc {
        ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix,
            full_range,
        }
    }

    fn apply(rows: &[[f32; 4]; 3], yuv: [f32; 3]) -> [f32; 3] {
        core::array::from_fn(|r| {
            rows[r][0] * yuv[0] + rows[r][1] * yuv[1] + rows[r][2] * yuv[2] + rows[r][3]
        })
    }

    /// 10-bit limited MSB-packed (P010/X6): reference white Y=940, black Y=64, neutral
    /// chroma 512 — sampled as UNORM16 of `code << 6`.
    #[test]
    fn bt2020_10bit_limited_white_black() {
        let rows = csc_rows(desc(9, false), 10, true);
        let s = |code: u32| ((code << 6) as f32) / 65535.0;
        let white = apply(&rows, [s(940), s(512), s(512)]);
        let black = apply(&rows, [s(64), s(512), s(512)]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.002, "white {white:?}");
            assert!(b.abs() < 0.002, "black {black:?}");
        }
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0
    /// — the GL presenter's test, in row form.
    #[test]
    fn bt709_limited_white_black() {
        let rows = csc_rows(desc(1, false), 8, false);
        let white = apply(&rows, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let black = apply(&rows, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 0.005, "white {white:?}");
            assert!(b.abs() < 0.005, "black {black:?}");
        }
    }

    /// Full-range identity points + the 601-vs-709 red excursion (guards the
    /// matrix-code dispatch), same as the GL presenter's test.
    #[test]
    fn full_range_and_red_excursion() {
        let rows = csc_rows(desc(5, true), 8, false);
        let white = apply(&rows, [1.0, 0.5, 0.5]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let red = apply(&rows, [0.0, 0.5, 1.0]);
        assert!((red[0] - 2.0 * (1.0 - 0.299) * 0.5).abs() < 1e-4, "{red:?}");
        let rows709 = csc_rows(desc(1, true), 8, false);
        let red709 = apply(&rows709, [0.0, 0.5, 1.0]);
        assert!(
            (red709[0] - 2.0 * (1.0 - 0.2126) * 0.5).abs() < 1e-4,
            "{red709:?}"
        );
        assert!((red[0] - red709[0]).abs() > 0.05);
    }

    /// The row form must agree with the GL presenter's column-major `yuv_to_rgb` on a
    /// grid of inputs — same math, different packing.
    #[test]
    fn rows_match_the_gl_matrix_form() {
        for (matrix, full) in [(1u8, false), (1, true), (5, false), (9, false), (9, true)] {
            let d = desc(matrix, full);
            let rows = csc_rows(d, 8, false);
            // Reimplementation of video_gl::yuv_to_rgb's application for comparison.
            let (kr, kb) = match matrix {
                5 | 6 => (0.299f32, 0.114f32),
                9 | 10 => (0.2627, 0.0593),
                _ => (0.2126, 0.0722),
            };
            let kg = 1.0 - kr - kb;
            let (sy, oy, sc) = if full {
                (1.0f32, 0.0f32, 1.0f32)
            } else {
                (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
            };
            let mat = [
                sy,
                sy,
                sy,
                0.0,
                -2.0 * (1.0 - kb) * kb / kg * sc,
                2.0 * (1.0 - kb) * sc,
                2.0 * (1.0 - kr) * sc,
                -2.0 * (1.0 - kr) * kr / kg * sc,
                0.0,
            ];
            let off = [oy, -0.5, -0.5];
            for yuv in [
                [0.1f32, 0.3, 0.7],
                [0.9, 0.5, 0.5],
                [0.5, 0.2, 0.8],
                [16.0 / 255.0, 0.5, 0.5],
            ] {
                let v = [yuv[0] + off[0], yuv[1] + off[1], yuv[2] + off[2]];
                let gl: [f32; 3] =
                    core::array::from_fn(|r| (0..3).map(|c| mat[c * 3 + r] * v[c]).sum());
                let ours = apply(&rows, yuv);
                for (a, b) in gl.iter().zip(ours) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "{matrix}/{full}: gl {gl:?} rows {ours:?}"
                    );
                }
            }
        }
    }
}
