//! Session/frame **construction** for the Vulkan Video encoder — the unsafe builders
//! (`make_frame*`, `make_video_image`, `probe_rgb_direct`) and the parameter-set bitstream
//! writers (`build_parameters_h265`/`_av1`, the AV1 sequence-header OBU). Split from
//! `vulkan_video.rs` (WP7.5) the way `amf_sys.rs` was split from `amf.rs`: a `#[path]` child
//! module, so this file sees the parent's private items (`Frame` and friends) with zero
//! visibility churn, and ~800 lines of construction `unsafe` get their own review surface.
//! Steady-state encode logic stays in the parent.

// The parent's whole item namespace (Frame, the consts, sibling helpers) — the point of the
// child-module shape. External imports are this file's own; `vk_util` is a crate-root sibling,
// so the path is `crate::`, not the parent-relative `super::` the parent uses.
use super::*;
use crate::vk_util::{find_mem, make_plain_image, make_view};
use anyhow::{bail, Result};
use ash::vk;
use std::ffi::c_void;

pub(super) fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Probe for the RGB-direct encode source (design/vulkan-rgb-direct-encode.md): can this device
/// take the captured RGB dmabuf directly, with the VCN EFC front-end doing the 709-narrow CSC,
/// via `VK_VALVE_video_encode_rgb_conversion` (RADV since Mesa 26.0, gated on EFC hardware)?
/// `Ok((x_offset, y_offset))` carries the chroma-siting bits a session must be created with
/// (the preferred available bit per axis); `Err` is the first missing requirement, logged as
/// the open-time verdict.
pub(super) unsafe fn probe_rgb_direct(
    instance: &ash::Instance,
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    codec_op: vk::VideoCodecOperationFlagsKHR,
    av1: bool,
) -> Result<(u32, u32), &'static str> {
    use crate::vk_av1_encode as av1b;
    use crate::vk_valve_rgb as vrgb;
    // 1. The device extension must exist (Mesa >= 26.0 AND the VCN has an EFC block).
    let Ok(exts) = instance.enumerate_device_extension_properties(pd) else {
        return Err("probe-failed(ext-enum)");
    };
    if !exts
        .iter()
        .any(|e| std::ffi::CStr::from_ptr(e.extension_name.as_ptr()) == vrgb::EXTENSION_NAME)
    {
        return Err("no-ext(mesa<26.0-or-no-efc)");
    }
    // 2. Feature bit.
    let mut feat = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
        s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
        p_next: std::ptr::null_mut(),
        video_encode_rgb_conversion: vk::FALSE,
    };
    let mut f2 = vk::PhysicalDeviceFeatures2 {
        p_next: &mut feat as *mut _ as *mut c_void,
        ..Default::default()
    };
    instance.get_physical_device_features2(pd, &mut f2);
    if feat.video_encode_rgb_conversion == vk::FALSE {
        return Err("no-feature");
    }
    // 3. Capabilities under the rgb-chained profile — the conversion must cover the compute
    //    CSC's colour math (rgb2yuv.comp: BT.709, narrow range; chroma siting is looser, see
    //    below). The profile chain is the same one every rgb-direct consumer presents.
    let mut ps = RgbProfileStack::new(codec_op);
    let profile = *ps.wire(av1);
    let mut rgb_caps = vrgb::VideoEncodeRgbConversionCapabilitiesVALVE {
        s_type: vrgb::stype(vrgb::ST_CAPABILITIES),
        p_next: std::ptr::null_mut(),
        rgb_models: 0,
        rgb_ranges: 0,
        x_chroma_offsets: 0,
        y_chroma_offsets: 0,
    };
    let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut av1_caps: av1b::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
    av1_caps.s_type = av1b::stype(av1b::ST_CAPABILITIES);
    let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    if av1 {
        av1_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
    } else {
        h265_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut h265_caps as *mut _ as *mut c_void;
    }
    caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
    let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
    if r != vk::Result::SUCCESS {
        return Err("no-rgb-profile(caps)");
    }
    // Colour model + range must match the shader exactly (709 narrow). Chroma siting is looser
    // BY ON-GLASS FINDING (RADV 26.0.4 / 780M): the VCN EFC advertises x=COSITED_EVEN only —
    // the canonical H.26x left-cosited siting — while our 2x2-average shader is midpoint. The
    // difference is a half-pel chroma-x phase, imperceptible (and EFC's is arguably the more
    // correct one since nothing in our bitstream signals siting). Accept either bit per axis
    // and choose the closest to the shader's math: midpoint if offered, else cosited-even.
    let pick = |offered: u32| -> Option<u32> {
        if offered & vrgb::CHROMA_OFFSET_MIDPOINT != 0 {
            Some(vrgb::CHROMA_OFFSET_MIDPOINT)
        } else if offered & vrgb::CHROMA_OFFSET_COSITED_EVEN != 0 {
            Some(vrgb::CHROMA_OFFSET_COSITED_EVEN)
        } else {
            None
        }
    };
    if rgb_caps.rgb_models & vrgb::MODEL_YCBCR_709 == 0
        || rgb_caps.rgb_ranges & vrgb::RANGE_NARROW == 0
    {
        return Err("no-709-narrow");
    }
    let (Some(x_offset), Some(y_offset)) = (
        pick(rgb_caps.x_chroma_offsets),
        pick(rgb_caps.y_chroma_offsets),
    ) else {
        return Err("no-chroma-siting");
    };
    // 4. The encode-src format set under this profile must offer BGRA with DRM-modifier tiling —
    //    the capture hands LINEAR BGRx dmabufs (fourcc XR24), which import as B8G8R8A8_UNORM.
    let profile_arr = [profile];
    let plist = vk::VideoProfileListInfoKHR::default().profiles(&profile_arr);
    let mut fmt_info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR);
    fmt_info.p_next = &plist as *const _ as *const c_void;
    let get_fmt = vq_inst.fp().get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    let r = get_fmt(pd, &fmt_info, &mut count, std::ptr::null_mut());
    if r != vk::Result::SUCCESS || count == 0 {
        return Err("no-rgb-format");
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let r = get_fmt(pd, &fmt_info, &mut count, props.as_mut_ptr());
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return Err("no-rgb-format");
    }
    if !props[..count as usize].iter().any(|p| {
        p.format == vk::Format::B8G8R8A8_UNORM
            && p.image_tiling == vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT
    }) {
        return Err("no-bgra-modifier-tiling");
    }
    Ok((x_offset, y_offset))
}

pub(super) unsafe fn make_video_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    layers: u32,
    usage: vk::ImageUsageFlags,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    concurrent: &[u32],
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let mut ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(fmt)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(profile_list);
    if concurrent.len() >= 2 {
        ci = ci
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(concurrent);
    } else {
        ci = ci.sharing_mode(vk::SharingMode::EXCLUSIVE);
    }
    let img = device.create_image(&ci, None)?;
    let req = device.get_image_memory_requirements(img);
    // Unwind on failure: callers (the open path) only ever see the completed pair.
    let mem = match device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )),
        None,
    ) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_image(img, None);
            return Err(e.into());
        }
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        device.destroy_image(img, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    Ok((img, mem))
}

/// Build one in-flight frame's private resources: NV12 encode-src, Y/UV CSC scratch, its CSC
/// descriptor set (Y/UV bound now, RGB per use), the bitstream buffer + feedback query, and the
/// per-frame command buffers + sync. `profile_list`/`profile` are borrowed only during creation.
///
/// Builds in place into `f` — a [`Frame::default`] the caller has already parked in its
/// [`VkTeardown`] guard — so every handle is owned by the unwind the moment it exists and a
/// mid-build failure leaks nothing.
pub(super) unsafe fn make_frame(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fams: &[u32],
    profile: &vk::VideoProfileInfoKHR,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,
    bs_size: u64,
    sampler: vk::Sampler,
    with_ts: bool,
    csc: bool,
    pad_fmt: Option<vk::Format>,
    f: &mut Frame,
) -> Result<()> {
    // "no cursor uploaded yet" sentinel — a real serial may be 0 (see `prep_cursor`).
    f.cursor_serial = u64::MAX;
    // Padded-copy staging (unaligned-mode RGB-direct or native NV12): an aligned encode-src in
    // the session's picture format, filled by a transfer blit each frame — concurrent compute
    // (copy) + encode (source read). TRANSFER_SRC because the width-padding pass self-copies the
    // staging image's own last visible column (see `record_pad_blit`).
    if let Some(fmt) = pad_fmt {
        (f.pad_img, f.pad_mem) = make_video_image(
            device,
            mem_props,
            fmt,
            w,
            h,
            1,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC,
            profile_list,
            fams,
        )?;
        f.pad_view = make_view(device, f.pad_img, fmt, 0)?;
    }
    // RGB-direct sessions never touch the CSC pipeline: no NV12 encode-src, no Y/UV scratch, no
    // cursor overlay, no descriptor set — the encode source is the imported RGB itself (or the
    // CPU staging image, built lazily). Their Frame keeps the null handles (teardown-safe).
    if csc {
        make_frame_csc(
            device,
            mem_props,
            w,
            h,
            fams,
            profile_list,
            csc_dsl,
            csc_pool,
            sampler,
            f,
        )?;
    }
    make_frame_common(
        device,
        mem_props,
        profile,
        profile_list,
        cmd_pool,
        compute_pool,
        bs_size,
        with_ts,
        f,
    )
}

/// The CSC-only half of [`make_frame`]: NV12 encode-src + Y/UV scratch + cursor + descriptors.
#[allow(clippy::too_many_arguments)]
unsafe fn make_frame_csc(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fams: &[u32],
    profile_list: &mut vk::VideoProfileListInfoKHR,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    f: &mut Frame,
) -> Result<()> {
    // NV12 encode-src (filled by the CSC copy) — concurrent compute+encode.
    (f.nv12_src, f.nv12_mem) = make_video_image(
        device,
        mem_props,
        NV12,
        w,
        h,
        1,
        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
        profile_list,
        fams,
    )?;
    f.nv12_view = make_view(device, f.nv12_src, NV12, 0)?;
    // CSC scratch (Y R8 full-res, UV RG8 half-res).
    (f.y_img, f.y_mem, f.y_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8_UNORM,
        w,
        h,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    (f.uv_img, f.uv_mem, f.uv_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8G8_UNORM,
        w / 2,
        h / 2,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    // Cursor overlay: fixed CURSOR_MAX² RGBA8 sampled image + host staging (cursor-as-metadata). The
    // view/descriptor is static (bound at binding 3 below); only the image *content* changes, and
    // only when the pointer bitmap does — see `prep_cursor`.
    (f.cursor_img, f.cursor_mem, f.cursor_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8G8B8A8_UNORM,
        CURSOR_MAX,
        CURSOR_MAX,
        vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
    )?;
    f.cursor_stage = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size((CURSOR_MAX * CURSOR_MAX * 4) as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC),
        None,
    )?;
    let cs_req = device.get_buffer_memory_requirements(f.cursor_stage);
    f.cursor_stage_mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(cs_req.size)
            .memory_type_index(find_mem(
                mem_props,
                cs_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    )?;
    device.bind_buffer_memory(f.cursor_stage, f.cursor_stage_mem, 0)?;
    // Descriptor set — Y/UV storage bindings fixed; binding 0 (RGB) rewritten per use; binding 3
    // (cursor) points at the static cursor image (its layout is SHADER_READ_ONLY once prepped).
    let dsls = [csc_dsl];
    f.csc_set = device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(csc_pool)
            .set_layouts(&dsls),
    )?[0];
    let y_info = [vk::DescriptorImageInfo::default()
        .image_view(f.y_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let uv_info = [vk::DescriptorImageInfo::default()
        .image_view(f.uv_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let cur_info = [vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(f.cursor_view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    device.update_descriptor_sets(
        &[
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&uv_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&cur_info),
        ],
        &[],
    );
    Ok(())
}

/// The mode-independent half of [`make_frame`]: bitstream buffer (+ persistent map), feedback
/// query, optional timestamp pool, command buffers and sync objects.
#[allow(clippy::too_many_arguments)]
unsafe fn make_frame_common(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    profile: &vk::VideoProfileInfoKHR,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,
    bs_size: u64,
    with_ts: bool,
    f: &mut Frame,
) -> Result<()> {
    // Bitstream buffer + feedback query.
    f.bs_buf = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(bs_size)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .push_next(profile_list),
        None,
    )?;
    let bs_req = device.get_buffer_memory_requirements(f.bs_buf);
    f.bs_mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(bs_req.size)
            .memory_type_index(find_mem(
                mem_props,
                bs_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    )?;
    device.bind_buffer_memory(f.bs_buf, f.bs_mem, 0)?;
    // Map once for the slot's lifetime — read_slot copies AUs straight out of this (coherent
    // memory, no per-frame map/unmap); vkFreeMemory implicitly unmaps at teardown.
    f.bs_ptr = BsPtr(
        device.map_memory(f.bs_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *const u8,
    );
    // PUNKTFUNK_PERF: a 2-slot timestamp pool bracketing this slot's compute batch (CSC split).
    if with_ts {
        f.ts_pool = device.create_query_pool(
            &vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2),
            None,
        )?;
    }
    let mut fb_ci = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
        vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
            | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
    );
    fb_ci.p_next = profile as *const _ as *const c_void;
    let mut query_ci = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
        .query_count(1);
    query_ci.p_next = &fb_ci as *const _ as *const c_void;
    f.query_pool = device.create_query_pool(&query_ci, None)?;
    // Command buffers + per-frame sync.
    f.cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(1),
    )?[0];
    f.compute_cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(compute_pool)
            .command_buffer_count(1),
    )?[0];
    f.csc_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
    f.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
    Ok(())
}

/// Author VPS/SPS/PPS (Main, level 4.0, low-latency, conformance-window crop) and return the
/// session-parameters object + the encoded header bytes (VPS+SPS+PPS NALs) for keyframes.
pub(super) unsafe fn build_parameters_h265(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    venc_dev: &ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
    quality_level: u32,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>)> {
    use ash::vk::native as hh;
    let mut ptl: hh::StdVideoH265ProfileTierLevel = std::mem::zeroed();
    ptl.flags.set_general_progressive_source_flag(1);
    ptl.flags.set_general_frame_only_constraint_flag(1);
    ptl.general_profile_idc = hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN;
    ptl.general_level_idc = hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0;

    let mut dpbm: hh::StdVideoH265DecPicBufMgr = std::mem::zeroed();
    dpbm.max_dec_pic_buffering_minus1[0] = (DPB_SLOTS - 1) as u8;
    dpbm.max_num_reorder_pics[0] = 0;
    dpbm.max_latency_increase_plus1[0] = 0;

    let mut vps: hh::StdVideoH265VideoParameterSet = std::mem::zeroed();
    vps.flags.set_vps_temporal_id_nesting_flag(1);
    vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
    vps.pDecPicBufMgr = &dpbm;
    vps.pProfileTierLevel = &ptl;

    let mut sps: hh::StdVideoH265SequenceParameterSet = std::mem::zeroed();
    sps.flags.set_sps_temporal_id_nesting_flag(1);
    sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
    sps.chroma_format_idc = hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
    sps.pic_width_in_luma_samples = w;
    sps.pic_height_in_luma_samples = h;
    sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
    sps.log2_diff_max_min_luma_coding_block_size = 3;
    sps.log2_diff_max_min_luma_transform_block_size = 3;
    sps.max_transform_hierarchy_depth_inter = 4;
    sps.max_transform_hierarchy_depth_intra = 4;
    sps.pProfileTierLevel = &ptl;
    sps.pDecPicBufMgr = &dpbm;
    if w != rw || h != rh {
        sps.flags.set_conformance_window_flag(1);
        sps.conf_win_right_offset = (w - rw) / 2; // 4:2:0 SubWidthC = 2
        sps.conf_win_bottom_offset = (h - rh) / 2; // 4:2:0 SubHeightC = 2
    }

    // Colour signalling. This backend's CSC (`rgb2yuv.comp`) is BT.709 LIMITED 8-bit and nothing
    // else — `open_amd_intel` routes every HDR session to VAAPI precisely because this path
    // hardcodes it — so the SPS can state it as a constant. Without the VUI the stream is
    // "unspecified" and each decoder applies its own default: the punktfunk clients fall back to
    // BT.709 (`pf_client_core::video_color::csc_rows`), but vendor TV decoders guess from
    // RESOLUTION — an LG webOS panel reads a 4K SDR stream as BT.2020 and renders it visibly
    // washed out. Every sibling backend (NVENC `nvenc_core.rs`, VAAPI, QSV, the Windows libav
    // path) already signals this triplet; this one was the hole.
    //
    // `vui` must outlive `create_video_session_parameters_khr` below — it does, `sps_arr` only
    // copies the pointer and both live to the end of this function.
    let mut vui: hh::StdVideoH265SequenceParameterSetVui = std::mem::zeroed();
    vui.flags.set_video_signal_type_present_flag(1);
    vui.flags.set_video_full_range_flag(0); // limited/studio swing (16-235 luma)
    vui.flags.set_colour_description_present_flag(1);
    vui.video_format = 5; // unspecified — the CICP triplet below is what matters
    vui.colour_primaries = 1; // BT.709
    vui.transfer_characteristics = 1; // BT.709
    vui.matrix_coeffs = 1; // BT.709
    sps.flags.set_vui_parameters_present_flag(1);
    sps.pSequenceParameterSetVui = &vui;

    let mut pps: hh::StdVideoH265PictureParameterSet = std::mem::zeroed();
    pps.flags.set_cu_qp_delta_enabled_flag(1);
    pps.flags.set_pps_loop_filter_across_slices_enabled_flag(1);

    let vps_arr = [vps];
    let sps_arr = [sps];
    let pps_arr = [pps];
    let add = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
        .std_vp_ss(&vps_arr)
        .std_sp_ss(&sps_arr)
        .std_pp_ss(&pps_arr);
    let mut h265_ci = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
        .max_std_vps_count(1)
        .max_std_sps_count(1)
        .max_std_pps_count(1)
        .parameters_add_info(&add);
    // Bake the session's quality level into the parameters object — the spec requires it to match
    // the level the first frame's ENCODE_QUALITY_LEVEL control installs.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    let ci = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut h265_ci)
        .push_next(&mut q_info);
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters: {r:?}");
    }

    let mut get_h265 = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
        .write_std_vps(true)
        .write_std_sps(true)
        .write_std_pps(true)
        .std_vps_id(0)
        .std_sps_id(0)
        .std_pps_id(0);
    let get = vk::VideoEncodeSessionParametersGetInfoKHR::default()
        .video_session_parameters(params)
        .push_next(&mut get_h265);
    let get_fn = venc_dev.fp().get_encoded_video_session_parameters_khr;
    let mut fb = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default();
    let mut size: usize = 0;
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        std::ptr::null_mut(),
    );
    if r != vk::Result::SUCCESS {
        // `params` is live but not yet the caller's guard's to unwind — destroy before bailing.
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header size: {r:?}");
    }
    let mut buf = vec![0u8; size];
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != vk::Result::SUCCESS {
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header bytes: {r:?}");
    }
    buf.truncate(size);
    Ok((params, buf))
}

/// AV1 low-overhead OBU bit-writer (MSB-first), used to hand-pack the sequence-header OBU that
/// Vulkan AV1 encode (unlike H26x) never emits itself.
struct Av1BitWriter {
    buf: Vec<u8>,
    cur: u8,
    fill: u8,
}
impl Av1BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            fill: 0,
        }
    }
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.fill += 1;
        if self.fill == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.fill = 0;
        }
    }
    fn put(&mut self, val: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.bit((val >> i) & 1);
        }
    }
    /// Flush, zero-padding the final partial byte (OBU size field delimits the payload).
    fn finish(mut self) -> Vec<u8> {
        if self.fill > 0 {
            self.cur <<= 8 - self.fill;
            self.buf.push(self.cur);
        }
        self.buf
    }
}

/// AV1 leb128 (little-endian base-128) encoding of an OBU size.
fn leb128(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
    out
}

/// Bit-pack a `sequence_header_obu` (AV1 spec §5.5) into a size-delimited OBU. The field values here
/// MUST mirror the `StdVideoAV1SequenceHeader` handed to the driver in `build_parameters_av1` so the
/// driver-emitted frame OBUs parse against this header. Single operating point, 8-bit 4:2:0,
/// order-hint on, CDEF+restoration+filter-intra allowed, everything exotic (compound/warp/superres)
/// disabled — the profile our single-reference P-frame encoder actually uses.
fn av1_sequence_header_obu(
    sb128: bool,
    fwb: u32,
    fhb: u32,
    max_w_m1: u32,
    max_h_m1: u32,
    order_hint_bits_minus_1: u32,
    seq_level_idx: u32,
) -> Vec<u8> {
    let mut w = Av1BitWriter::new();
    w.put(0, 3); // seq_profile = MAIN
    w.bit(0); // still_picture
    w.bit(0); // reduced_still_picture_header
    w.bit(0); // timing_info_present_flag
    w.bit(0); // initial_display_delay_present_flag
    w.put(0, 5); // operating_points_cnt_minus_1 = 0
    w.put(0, 12); // operating_point_idc[0]
    w.put(seq_level_idx, 5); // seq_level_idx[0]
    if seq_level_idx > 7 {
        w.bit(0); // seq_tier[0] = 0
    }
    w.put(fwb, 4); // frame_width_bits_minus_1
    w.put(fhb, 4); // frame_height_bits_minus_1
    w.put(max_w_m1, fwb + 1); // max_frame_width_minus_1
    w.put(max_h_m1, fhb + 1); // max_frame_height_minus_1
    w.bit(0); // frame_id_numbers_present_flag
    w.bit(sb128 as u32); // use_128x128_superblock
    w.bit(0); // enable_filter_intra
    w.bit(0); // enable_intra_edge_filter
    w.bit(0); // enable_interintra_compound
    w.bit(0); // enable_masked_compound
    w.bit(0); // enable_warped_motion
    w.bit(0); // enable_dual_filter
    w.bit(1); // enable_order_hint
    w.bit(0); // enable_jnt_comp
    w.bit(0); // enable_ref_frame_mvs
    w.bit(1); // seq_choose_screen_content_tools -> seq_force_screen_content_tools = SELECT
    w.bit(1); // seq_choose_integer_mv -> seq_force_integer_mv = SELECT
    w.put(order_hint_bits_minus_1, 3); // order_hint_bits_minus_1
    w.bit(0); // enable_superres
    w.bit(0); // enable_cdef
    w.bit(0); // enable_restoration
              // color_config() (AV1 spec §5.5.2): 8-bit 4:2:0, BT.709 limited — the CSC this
              // backend's `rgb2yuv.comp` actually performs. AV1 has no VUI, so the CICP triplet
              // lives here; omitting it (color_description_present_flag = 0) left the stream
              // "unspecified" and vendor TV decoders guess colorimetry from resolution.
              // CP_BT_709/TC_BT_709/MC_BT_709 avoids the spec's sRGB special case (which would
              // force color_range = 1 and drop the explicit range bit), so the field order below
              // is the same as the unspecified form plus the three CICP bytes.
    w.bit(0); // high_bitdepth
    w.bit(0); // mono_chrome
    w.bit(1); // color_description_present_flag
    w.put(1, 8); // color_primaries = CP_BT_709
    w.put(1, 8); // transfer_characteristics = TC_BT_709
    w.put(1, 8); // matrix_coefficients = MC_BT_709
    w.bit(0); // color_range (studio/limited)
    w.put(0, 2); // chroma_sample_position = CSP_UNKNOWN (subsampling_x==subsampling_y==1 for profile 0)
    w.bit(0); // separate_uv_delta_q
    w.bit(0); // film_grain_params_present

    // trailing_bits(): a stop `1` bit then zero-pad to a byte (the size field delimits the OBU, but
    // the parser still requires the trailing_one_bit — dav1d/cbs reject a plain zero pad).
    w.bit(1);
    let payload = w.finish();
    let mut obu = vec![0x0au8]; // obu_header: type=OBU_SEQUENCE_HEADER(1), has_size_field=1
    obu.extend_from_slice(&leb128(payload.len() as u64));
    obu.extend_from_slice(&payload);
    obu
}

/// AV1 session parameters + header framing. Vulkan AV1 encode emits only the per-frame OBU, so we
/// return the app-owned prefixes: a temporal-delimiter OBU that opens every temporal unit
/// (`frame_prefix`), and TD + the bit-packed sequence-header OBU for keyframes (`header`).
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn build_parameters_av1(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    _rw: u32,
    _rh: u32,
    max_level: ash::vk::native::StdVideoAV1Level,
    sb128: bool,
    quality_level: u32,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>, Vec<u8>)> {
    use crate::vk_av1_encode as av1;
    use ash::vk::native as hh;

    let fwb = 31 - w.leading_zeros(); // av_log2(w): enough bits for max_frame_width_minus_1 = w-1
    let fhb = 31 - h.leading_zeros();
    let order_hint_bits_minus_1: u32 = 7; // OrderHintBits = 8
    let seq_level_idx = max_level; // StdVideoAV1Level's numeric value IS the AV1 seq_level_idx

    // ---- Std sequence header (must match the OBU packed below) ----
    // BT.709 limited, mirroring the `color_config()` bits `av1_sequence_header_obu` packs — the two
    // MUST stay identical or the driver's frame OBUs parse against a header we didn't write.
    // `color_range` stays 0 (studio swing); only the description flag + CICP triplet change.
    let mut cc_flags: hh::StdVideoAV1ColorConfigFlags = std::mem::zeroed();
    cc_flags.set_color_description_present_flag(1);
    let mut cc: hh::StdVideoAV1ColorConfig = std::mem::zeroed();
    cc.flags = cc_flags;
    cc.BitDepth = 8;
    cc.subsampling_x = 1;
    cc.subsampling_y = 1;
    cc.color_primaries = hh::StdVideoAV1ColorPrimaries_STD_VIDEO_AV1_COLOR_PRIMARIES_BT_709;
    cc.transfer_characteristics =
        hh::StdVideoAV1TransferCharacteristics_STD_VIDEO_AV1_TRANSFER_CHARACTERISTICS_BT_709;
    cc.matrix_coefficients =
        hh::StdVideoAV1MatrixCoefficients_STD_VIDEO_AV1_MATRIX_COEFFICIENTS_BT_709;
    cc.chroma_sample_position =
        hh::StdVideoAV1ChromaSamplePosition_STD_VIDEO_AV1_CHROMA_SAMPLE_POSITION_UNKNOWN;

    // Match FFmpeg's Vulkan AV1 encoder (proven on this RADV/VCN path): the ONLY coding tools
    // enabled are order-hint and (per caps) 128x128 superblocks. CDEF, loop restoration, filter-
    // intra, warped/compound motion, superres all OFF — enabling them made VCN emit frame-header
    // sections whose bit layout our sequence header didn't match, desyncing every inter frame.
    let mut sh_flags: hh::StdVideoAV1SequenceHeaderFlags = std::mem::zeroed();
    if sb128 {
        sh_flags.set_use_128x128_superblock(1);
    }
    sh_flags.set_enable_order_hint(1);
    let mut sh: hh::StdVideoAV1SequenceHeader = std::mem::zeroed();
    sh.flags = sh_flags;
    sh.seq_profile = hh::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN;
    sh.frame_width_bits_minus_1 = fwb as u8;
    sh.frame_height_bits_minus_1 = fhb as u8;
    sh.max_frame_width_minus_1 = (w - 1) as u16;
    sh.max_frame_height_minus_1 = (h - 1) as u16;
    sh.order_hint_bits_minus_1 = order_hint_bits_minus_1 as u8;
    sh.seq_force_integer_mv = 2; // SELECT
    sh.seq_force_screen_content_tools = 2; // SELECT
    sh.pColorConfig = &cc;

    // ---- single operating point conveying the level/tier the driver targets ----
    let op = av1::StdVideoEncodeAV1OperatingPointInfo {
        flags: std::mem::zeroed(),
        operating_point_idc: 0,
        seq_level_idx: seq_level_idx as u8,
        seq_tier: 0,
        decoder_buffer_delay: 0,
        encoder_buffer_delay: 0,
        initial_display_delay_minus_1: 0,
    };
    let ops = [op];
    let av1_spci = av1::VideoEncodeAV1SessionParametersCreateInfoKHR {
        s_type: av1::stype(av1::ST_SESSION_PARAMETERS_CREATE_INFO),
        p_next: std::ptr::null(),
        p_std_sequence_header: &sh,
        p_std_decoder_model_info: std::ptr::null(),
        std_operating_point_count: 1,
        p_std_operating_points: ops.as_ptr() as *const c_void,
    };
    // Bake the session's quality level into the parameters object (must match the level the first
    // frame's ENCODE_QUALITY_LEVEL control installs); chained raw ahead of the vendored AV1 struct.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    q_info.p_next = &av1_spci as *const _ as *const c_void;
    let mut ci = vk::VideoSessionParametersCreateInfoKHR::default().video_session(session);
    ci.p_next = &q_info as *const _ as *const c_void;
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters (av1): {r:?}");
    }

    // ---- header framing: TD every temporal unit; TD + seq-header OBU on keyframes ----
    let td = vec![0x12u8, 0x00]; // temporal_delimiter OBU (type=2, size=0)
    let seq_obu = av1_sequence_header_obu(
        sb128,
        fwb,
        fhb,
        w - 1,
        h - 1,
        order_hint_bits_minus_1,
        seq_level_idx,
    );
    let mut keyframe_prefix = td.clone();
    keyframe_prefix.extend_from_slice(&seq_obu);
    Ok((params, keyframe_prefix, td))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks a bit-packed AV1 sequence header field-by-field (spec §5.5.1 order, for the fixed
    /// configuration `av1_sequence_header_obu` emits) and returns the `color_config()` values.
    /// Deliberately an INDEPENDENT walk rather than a mirror of the writer: it is the only thing
    /// that catches a field width or ordering change upstream of `color_config`, which would leave
    /// the colour bits parsing at the wrong offset — the exact desync the module doc warns about.
    fn read_color_config(
        obu: &[u8],
        fwb: u32,
        fhb: u32,
        seq_level_idx: u32,
    ) -> (u8, u8, u8, u8, u8) {
        // obu_header (1 byte) + leb128 size — the payload starts after both.
        assert_eq!(
            obu[0], 0x0a,
            "obu_header: OBU_SEQUENCE_HEADER + has_size_field"
        );
        let mut i = 1;
        while obu[i] & 0x80 != 0 {
            i += 1;
        }
        let payload = &obu[i + 1..];

        let mut pos = 0usize;
        let mut take = |bits: u32| -> u32 {
            let mut v = 0u32;
            for _ in 0..bits {
                let byte = payload[pos / 8];
                v = (v << 1) | u32::from((byte >> (7 - (pos % 8))) & 1);
                pos += 1;
            }
            v
        };

        assert_eq!(take(3), 0, "seq_profile = MAIN");
        take(1); // still_picture
        assert_eq!(take(1), 0, "reduced_still_picture_header");
        assert_eq!(take(1), 0, "timing_info_present_flag");
        assert_eq!(take(1), 0, "initial_display_delay_present_flag");
        assert_eq!(take(5), 0, "operating_points_cnt_minus_1");
        take(12); // operating_point_idc[0]
        assert_eq!(take(5), seq_level_idx, "seq_level_idx[0]");
        if seq_level_idx > 7 {
            take(1); // seq_tier[0]
        }
        assert_eq!(take(4), fwb, "frame_width_bits_minus_1");
        assert_eq!(take(4), fhb, "frame_height_bits_minus_1");
        take(fwb + 1); // max_frame_width_minus_1
        take(fhb + 1); // max_frame_height_minus_1
        take(1); // frame_id_numbers_present_flag
        take(1); // use_128x128_superblock
        take(1); // enable_filter_intra
        take(1); // enable_intra_edge_filter
        take(1); // enable_interintra_compound
        take(1); // enable_masked_compound
        take(1); // enable_warped_motion
        take(1); // enable_dual_filter
        let order_hint = take(1); // enable_order_hint
        assert_eq!(
            order_hint, 1,
            "enable_order_hint (our single-ref P-frame config)"
        );
        take(1); // enable_jnt_comp
        take(1); // enable_ref_frame_mvs
        assert_eq!(take(1), 1, "seq_choose_screen_content_tools = SELECT");
        // seq_force_screen_content_tools = SELECT (> 0), so seq_choose_integer_mv is present.
        assert_eq!(take(1), 1, "seq_choose_integer_mv = SELECT");
        take(3); // order_hint_bits_minus_1
        take(1); // enable_superres
        take(1); // enable_cdef
        take(1); // enable_restoration

        // color_config()
        assert_eq!(take(1), 0, "high_bitdepth (8-bit)");
        assert_eq!(take(1), 0, "mono_chrome");
        let described = take(1) as u8;
        let (cp, tc, mc) = if described == 1 {
            (take(8) as u8, take(8) as u8, take(8) as u8)
        } else {
            (2, 2, 2) // CICP "unspecified"
        };
        let range = take(1) as u8;
        take(2); // chroma_sample_position
        assert_eq!(take(1), 0, "separate_uv_delta_q");
        assert_eq!(take(1), 0, "film_grain_params_present");
        assert_eq!(take(1), 1, "trailing_one_bit");
        (described, cp, tc, mc, range)
    }

    /// The sequence header must SIGNAL BT.709 limited — the CSC `rgb2yuv.comp` actually performs.
    /// An unsignalled ("unspecified") AV1 stream makes vendor TV decoders guess colorimetry from
    /// resolution: an LG webOS panel reads 4K SDR as BT.2020 and renders it washed out.
    ///
    /// The values here must equal the `StdVideoAV1ColorConfig` in `build_parameters_av1` — the
    /// driver packs its frame OBUs against that struct while clients parse this header, so a
    /// mismatch desyncs every inter frame.
    #[test]
    fn av1_sequence_header_signals_bt709_limited() {
        // 1920x1080: av_log2 gives 10/10 frame-size bits; level 4.0 (seq_level_idx 8) exercises
        // the seq_tier branch, and sb128 both ways since it sits above color_config.
        for (sb128, level) in [(false, 8u32), (true, 5u32)] {
            let obu = av1_sequence_header_obu(sb128, 10, 10, 1919, 1079, 7, level);
            let (described, cp, tc, mc, range) = read_color_config(&obu, 10, 10, level);
            assert_eq!(
                described, 1,
                "color_description_present_flag (sb128={sb128})"
            );
            assert_eq!(
                (cp, tc, mc),
                (1, 1, 1),
                "CICP BT.709 primaries/transfer/matrix"
            );
            assert_eq!(range, 0, "color_range = studio/limited swing");
        }
    }
}
