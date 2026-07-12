//! Raw **Vulkan Video** HEVC encoder (`VK_KHR_video_encode_h265`) with true reference-frame
//! invalidation — the open-stack AMD/Intel-Linux twin of the direct-NVENC RFI path. The app owns
//! the DPB, so loss recovery is a clean P-frame that re-references a known-good older slot (no IDR).
//!
//! Capture delivers packed RGB (dmabuf/CPU); this backend imports it, runs an on-GPU RGB→NV12
//! BT.709 compute CSC, then encodes. Proven end-to-end in `punktfunk-planning/design/vkenc-probe-harness`.
//! Opt-in via `PUNKTFUNK_VULKAN_ENCODE`; gated to HEVC + a device that advertises h265 encode.
#![allow(clippy::too_many_arguments)]

use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
use crate::encode::{Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::{AsRawFd, IntoRawFd};

const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// Max resident dmabuf imports (comfortably above any PipeWire pool depth; imports alias existing
/// buffers so this holds handles, not new allocations).
const IMPORT_CACHE_CAP: usize = 16;
// Prebuilt SPIR-V for the RGB→NV12 BT.709 compute CSC. Source is `rgb2yuv.comp` beside this file;
// regenerate with `glslangValidator -V rgb2yuv.comp -o rgb2yuv.spv` after editing the shader.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// DPB ring depth (well under the RADV `maxDpbSlots=17`); also the RFI recovery window.
const DPB_SLOTS: u32 = 8;
/// In-flight frame ring: how many captures may have GPU work outstanding at once. 2 overlaps a
/// frame's CSC+encode with the next capture (the throughput win) at the lowest possible added
/// latency — on-glass validated as rock-solid at 1080p@240, so it is the real-time default;
/// backpressure kicks in at the 2nd unread frame. Distinct from `DPB_SLOTS` (reference pool).
const RING_DEFAULT: usize = 2;

/// Resolve the in-flight ring depth: `PUNKTFUNK_VULKAN_INFLIGHT` (clamped 2..=6), else `RING_DEFAULT`.
fn ring_depth() -> usize {
    std::env::var("PUNKTFUNK_VULKAN_INFLIGHT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(2, 6))
        .unwrap_or(RING_DEFAULT)
}

/// Newest resident DPB slot whose wire index is strictly older than the loss — the clean anchor.
fn pick_recovery_slot(slot_wire: &[i64], loss_first: i64) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_wire = -1i64;
    for (i, &w) in slot_wire.iter().enumerate() {
        if w >= 0 && w < loss_first && w > best_wire {
            best = Some(i);
            best_wire = w;
        }
    }
    best
}

/// One in-flight frame's private GPU resources. The encoder keeps a small ring of these so a
/// frame's GPU work (CSC + encode) overlaps the CPU capturing and submitting the next one:
/// `submit()` records into a free slot and returns without blocking; `poll()` reads back the
/// oldest slot once its `fence` signals. Everything here is written by one frame and read by the
/// next-but-K, so it cannot be shared while a submission is outstanding.
struct Frame {
    compute_cmd: vk::CommandBuffer, // CSC (compute+transfer)
    cmd: vk::CommandBuffer,         // encode queue
    csc_sem: vk::Semaphore,         // compute -> encode ordering (this frame only)
    fence: vk::Fence,               // signaled when this frame's encode completes
    query_pool: vk::QueryPool,      // bitstream offset/bytes feedback
    bs_buf: vk::Buffer,
    bs_mem: vk::DeviceMemory,
    csc_set: vk::DescriptorSet, // Y/UV bindings fixed; binding 0 (RGB) rewritten each use
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    nv12_src: vk::Image,
    nv12_mem: vk::DeviceMemory,
    nv12_view: vk::ImageView,
    // CPU-input staging (lazily sized; only the software-capture / smoke-test path uses it).
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
    // Frame metadata, set at submit and read back at poll (valid only while this slot is in flight).
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
}

pub struct VulkanVideoEncoder {
    // --- vulkan core (owned) ---
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    vq_dev: ash::khr::video_queue::Device,
    venc_dev: ash::khr::video_encode_queue::Device,
    encode_queue: vk::Queue,
    compute_queue: vk::Queue,
    compute_family: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,

    // --- video session ---
    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    header: Vec<u8>, // VPS/SPS/PPS bytes, prepended to each keyframe

    // --- DPB ---
    dpb_image: vk::Image,
    dpb_mem: vk::DeviceMemory,
    dpb_views: Vec<vk::ImageView>,
    slot_wire: Vec<i64>, // wire index held per slot (-1 = empty) — RFI/loss domain
    slot_poc: Vec<i32>,  // HEVC POC held per slot — reference-delta domain
    prev_slot: usize,

    // --- CSC (RGB -> NV12), shared across the frame ring ---
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    // Per-buffer dmabuf-import cache, keyed by (st_dev, st_ino) — PipeWire cycles a small fixed pool,
    // so each underlying buffer is imported ONCE and reused (no per-frame VkImage create/import/destroy).
    // Imports are read-only per frame, so the ring shares them (concurrent frames read distinct buffers).
    import_cache: Vec<(u64, u64, vk::Image, vk::DeviceMemory, vk::ImageView)>,

    // --- in-flight frame ring (pipelining) ---
    frames: Vec<Frame>,         // per-slot private resources
    ring: usize,                // next slot to record into (round-robin over `frames`)
    in_flight: VecDeque<usize>, // slots submitted but not yet read back, oldest first
    bs_size: u64,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,

    // --- rate control (CBR), rebuilt-safe ---
    bitrate: u64,
    fps: u32,

    // --- state ---
    width: u32,
    height: u32,
    poc: i32,                  // monotonic HEVC picture-order-count
    enc_count: u64,            // total frames encoded — drives the DPB ring cursor
    auto_wire: i64,            // fallback wire index when submit() (not submit_indexed) is used
    first_frame: bool,         // needs RESET + DPB layout transition + CBR install + IDR
    force_kf: bool,            // request_keyframe / non-recoverable loss -> next frame is IDR
    pending_loss: Option<i64>, // invalidate_ref_frames(first) -> recover on next frame
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: the encoder is used only from the single encode thread; all Vulkan handles are owned and
// never shared. Matches `NvencCudaEncoder`'s `unsafe impl Send`.
unsafe impl Send for VulkanVideoEncoder {}

impl VulkanVideoEncoder {
    /// Signature mirrors the other Linux backends' `open` (see `nvenc_cuda::NvencCudaEncoder::open`).
    pub fn open(codec: Codec, width: u32, height: u32, fps: u32, bitrate_bps: u64) -> Result<Self> {
        if codec != Codec::H265 {
            bail!("vulkan-encode backend is HEVC-only (got {codec:?})");
        }
        // align coded extent to the encode granularity (64x16 on RADV); a conformance window
        // crops the aligned padding back to (width,height) on the decoder.
        let w = (width + 63) & !63;
        let h = (height + 15) & !15;
        // SAFETY: `open_inner` only issues Vulkan calls whose preconditions it establishes itself
        // (valid instance/device, correctly-chained create-infos); all handles are freshly created
        // here and owned by the returned `Self`. No aliasing or outside invariants are involved.
        unsafe { Self::open_inner(w, h, width, height, fps.max(1), bitrate_bps.max(1_000_000)) }
    }

    unsafe fn open_inner(w: u32, h: u32, rw: u32, rh: u32, fps: u32, bitrate: u64) -> Result<Self> {
        let entry = ash::Entry::load().context("load vulkan loader")?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .context("create instance")?;

        let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);

        // pick the physical device + encode queue family (skip llvmpipe)
        let (pd, encode_family) = {
            let mut found = None;
            for pd in instance.enumerate_physical_devices()? {
                let qf_len = instance.get_physical_device_queue_family_properties2_len(pd);
                let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); qf_len];
                let mut qf = vec![vk::QueueFamilyProperties2::default(); qf_len];
                for i in 0..qf_len {
                    qf[i].p_next = &mut video[i] as *mut _ as *mut c_void;
                }
                instance.get_physical_device_queue_family_properties2(pd, &mut qf);
                for i in 0..qf_len {
                    if qf[i]
                        .queue_family_properties
                        .queue_flags
                        .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                        && video[i]
                            .video_codec_operations
                            .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
                    {
                        found = Some((pd, i as u32));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            found.context("no VK_KHR_video_encode_h265 queue on any device")?
        };
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // a compute queue family for the CSC (usually family 0)
        let compute_family = {
            let qf = instance.get_physical_device_queue_family_properties(pd);
            qf.iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .context("no compute queue")? as u32
        };

        // the H265 Main encode profile
        let mut h265_profile = vk::VideoEncodeH265ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
        let mut usage = vk::VideoEncodeUsageInfoKHR::default()
            .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
            .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
            .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
        let profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H265)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
            .push_next(&mut h265_profile)
            .push_next(&mut usage);

        // capabilities (chain required for encode) -> std header version
        let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
        let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default()
            .push_next(&mut enc_caps)
            .push_next(&mut h265_caps);
        let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
        if r != vk::Result::SUCCESS {
            bail!("get_physical_device_video_capabilities: {r:?}");
        }
        let std_hdr = caps.std_header_version;

        // logical device: encode + compute queues + video extensions
        let dev_exts = [
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_encode_queue::NAME.as_ptr(),
            ash::khr::video_encode_h265::NAME.as_ptr(),
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
        ];
        let prio = [1.0f32];
        let mut qcis = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(encode_family)
            .queue_priorities(&prio)];
        if compute_family != encode_family {
            qcis.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(compute_family)
                    .queue_priorities(&prio),
            );
        }
        let mut sync2 =
            vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
        let device = instance
            .create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qcis)
                    .enabled_extension_names(&dev_exts)
                    .push_next(&mut sync2),
                None,
            )
            .context("create device")?;
        let encode_queue = device.get_device_queue(encode_family, 0);
        let compute_queue = device.get_device_queue(compute_family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let vq_dev = ash::khr::video_queue::Device::new(&instance, &device);
        let venc_dev = ash::khr::video_encode_queue::Device::new(&instance, &device);

        // ---- video session ----
        let session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(encode_family)
            .video_profile(&profile)
            .picture_format(NV12)
            .max_coded_extent(vk::Extent2D {
                width: w,
                height: h,
            })
            .reference_picture_format(NV12)
            .max_dpb_slots(DPB_SLOTS + 1)
            .max_active_reference_pictures(1)
            .std_header_version(&std_hdr);
        let mut session = vk::VideoSessionKHR::null();
        let r = (vq_dev.fp().create_video_session_khr)(
            device.handle(),
            &session_ci,
            std::ptr::null(),
            &mut session,
        );
        if r != vk::Result::SUCCESS {
            bail!("create_video_session: {r:?}");
        }
        // bind session memory
        let get_mem = vq_dev.fp().get_video_session_memory_requirements_khr;
        let mut n = 0u32;
        let _ = get_mem(device.handle(), session, &mut n, std::ptr::null_mut());
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        let _ = get_mem(device.handle(), session, &mut n, reqs.as_mut_ptr());
        let mut session_mem = Vec::new();
        let mut binds = Vec::new();
        for rq in &reqs {
            let mr = rq.memory_requirements;
            let ti = find_mem(
                &mem_props,
                mr.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            );
            let m = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(mr.size)
                    .memory_type_index(ti),
                None,
            )?;
            session_mem.push(m);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(rq.memory_bind_index)
                    .memory(m)
                    .memory_offset(0)
                    .memory_size(mr.size),
            );
        }
        let r = (vq_dev.fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            binds.len() as u32,
            binds.as_ptr(),
        );
        if r != vk::Result::SUCCESS {
            bail!("bind_video_session_memory: {r:?}");
        }

        // ---- session parameters (VPS/SPS/PPS) + retrieve header ----
        let (params, header) =
            build_parameters(&device, &vq_dev, &venc_dev, session, w, h, rw, rh)?;

        // ---- DPB image (NV12 OPTIMAL, ring of slots) — encode queue only ----
        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(&profile));
        let (dpb_image, dpb_mem) = make_video_image(
            &device,
            &mem_props,
            NV12,
            w,
            h,
            DPB_SLOTS,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            &mut profile_list,
            &[],
        )?;
        let dpb_views: Vec<vk::ImageView> = (0..DPB_SLOTS)
            .map(|slot| make_view(&device, dpb_image, NV12, slot))
            .collect::<Result<_>>()?;

        // NV12 encode-src, CSC scratch (Y/UV), bitstream, query and command buffers are all per
        // in-flight frame (built in `make_frame` below); only the queue-family list is shared here.
        let fams = if compute_family == encode_family {
            vec![]
        } else {
            vec![compute_family, encode_family]
        };

        // ---- CSC compute pipeline (shared across the frame ring) ----
        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(CSC_SPV))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
        let sb = |b: u32, t: vk::DescriptorType| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            sb(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
            sb(1, vk::DescriptorType::STORAGE_IMAGE),
            sb(2, vk::DescriptorType::STORAGE_IMAGE),
        ];
        let csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [csc_dsl];
        let csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default().set_layouts(&dsls),
            None,
        )?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let csc_pipe = device
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .layout(csc_layout)
                    .stage(stage)],
                None,
            )
            .map_err(|(_, e)| e)?[0];
        device.destroy_shader_module(shader, None);
        // One CSC descriptor set + its own Y/UV/NV12/bitstream per in-flight frame.
        let nframes = ring_depth();
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(nframes as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * nframes as u32),
        ];
        let csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(nframes as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;

        // ---- bitstream size (shared) + shared command pools ----
        let bs_size = align_up(
            3 * w as u64 * h as u64 + (1 << 16),
            caps.min_bitstream_buffer_size_alignment.max(1),
        );
        let cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(encode_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        let compute_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(compute_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;

        // ---- build the in-flight frame ring ----
        let mut frames = Vec::with_capacity(nframes);
        for _ in 0..nframes {
            frames.push(make_frame(
                &device,
                &mem_props,
                w,
                h,
                &fams,
                &profile,
                &mut profile_list,
                csc_dsl,
                csc_pool,
                cmd_pool,
                compute_pool,
                bs_size,
            )?);
        }

        Ok(Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            vq_dev,
            venc_dev,
            encode_queue,
            compute_queue,
            compute_family,
            mem_props,
            session,
            session_mem,
            params,
            header,
            dpb_image,
            dpb_mem,
            dpb_views,
            slot_wire: vec![-1; DPB_SLOTS as usize],
            slot_poc: vec![-1; DPB_SLOTS as usize],
            prev_slot: 0,
            csc_pipe,
            csc_layout,
            csc_dsl,
            csc_pool,
            sampler,
            import_cache: Vec::new(),
            frames,
            ring: 0,
            in_flight: VecDeque::new(),
            bs_size,
            cmd_pool,
            compute_pool,
            bitrate,
            fps,
            width: w,
            height: h,
            poc: 0,
            enc_count: 0,
            auto_wire: 0,
            first_frame: true,
            force_kf: false,
            pending_loss: None,
            pending: VecDeque::new(),
        })
    }
}

impl VulkanVideoEncoder {
    /// Point a slot's CSC descriptor binding 0 at the current frame's RGB image view.
    unsafe fn bind_rgb(&self, csc_set: vk::DescriptorSet, rgb_view: vk::ImageView) {
        let ii0 = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii0)],
            &[],
        );
    }

    /// Import a packed-RGB dmabuf as a SAMPLED VkImage (explicit DRM modifier). Caller destroys.
    unsafe fn import_dmabuf(
        &self,
        d: &crate::capture::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
        let fmt = fourcc_to_vk(d.fourcc)
            .with_context(|| format!("unsupported dmabuf fourcc {:#x}", d.fourcc))?;
        let plane = [vk::SubresourceLayout::default()
            .offset(d.offset as u64)
            .row_pitch(d.stride as u64)];
        let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(d.modifier)
            .plane_layouts(&plane);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let img = self.device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(fmt)
                .extent(vk::Extent3D {
                    width: cw,
                    height: ch,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut ext)
                .push_next(&mut drm),
            None,
        )?;
        // dup the fd; Vulkan takes ownership of the dup on a successful import.
        let dup = d.fd.try_clone().context("dup dmabuf fd")?.into_raw_fd();
        let fd_props = {
            let mut p = vk::MemoryFdPropertiesKHR::default();
            let _ = (self.ext_fd.fp().get_memory_fd_properties_khr)(
                self.device.handle(),
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                dup,
                &mut p,
            );
            p.memory_type_bits
        };
        let req = self.device.get_image_memory_requirements(img);
        let bits = req.memory_type_bits & fd_props;
        let ti = find_mem(
            &self.mem_props,
            if bits != 0 {
                bits
            } else {
                req.memory_type_bits
            },
            vk::MemoryPropertyFlags::empty(),
        );
        let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(img);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup);
        let mem = self.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti)
                .push_next(&mut ded)
                .push_next(&mut import),
            None,
        )?;
        self.device.bind_image_memory(img, mem, 0)?;
        let view = self.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(img)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(fmt)
                .subresource_range(color_range(0)),
            None,
        )?;
        Ok((img, mem, view))
    }

    /// Import a dmabuf, reusing a cached per-buffer import when the same underlying buffer recurs
    /// (PipeWire cycles a small fixed pool). Keyed by `(st_dev, st_ino)` because each `DmabufFrame`
    /// owns a fresh *dup* — a new fd number, same inode. Returns `(image, view, fresh)`; `fresh` is
    /// true only on a first import (caller uses UNDEFINED old-layout to preserve modifier-tiled data).
    unsafe fn import_cached(
        &mut self,
        d: &crate::capture::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let mut st: libc::stat = std::mem::zeroed();
        let key = if libc::fstat(d.fd.as_raw_fd(), &mut st) == 0 {
            (st.st_dev as u64, st.st_ino as u64)
        } else {
            // fstat failed → uncacheable; a per-frame-unique sentinel key never matches, so this
            // frame imports fresh (as before) but is still owned by the cache and freed on evict/Drop.
            (u64::MAX, self.enc_count)
        };
        if let Some(&(_, _, img, _, view)) = self.import_cache.iter().find(|e| (e.0, e.1) == key) {
            return Ok((img, view, false));
        }
        let (img, mem, view) = self.import_dmabuf(d, cw, ch)?;
        // Bound the cache; evict oldest (FIFO). A stable PipeWire pool never trips this in steady state
        // (all imports resident); it only cycles across a pool change (which also rebuilds the session).
        while self.import_cache.len() >= IMPORT_CACHE_CAP {
            let (_, _, oi, om, ov) = self.import_cache.remove(0);
            self.device.destroy_image_view(ov, None);
            self.device.destroy_image(oi, None);
            self.device.free_memory(om, None);
        }
        self.import_cache.push((key.0, key.1, img, mem, view));
        // Fires once per distinct pool buffer then goes quiet in steady state — the signal the cache
        // is hitting (a per-frame log here would mean inode keying failed and we're re-importing).
        tracing::debug!(
            resident = self.import_cache.len(),
            "vulkan-encode: imported a new dmabuf buffer"
        );
        Ok((img, view, true))
    }

    /// Reusable RGB image + staging buffer for software (CPU) capture, private to one frame slot;
    /// (re)created on format change. Only the software-capture / smoke-test path exercises this.
    unsafe fn ensure_cpu_rgb(
        &mut self,
        slot: usize,
        fmt: vk::Format,
        bytes: &[u8],
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        let need = (w * h * 4) as u64;
        if self.frames[slot].cpu_img.map(|(_, _, _, f)| f) != Some(fmt) {
            if let Some((i, m, v, _)) = self.frames[slot].cpu_img.take() {
                dev.destroy_image_view(v, None);
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            let (i, m, v) = make_plain_image(
                &dev,
                &self.mem_props,
                fmt,
                w,
                h,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            self.frames[slot].cpu_img = Some((i, m, v, fmt));
        }
        if self.frames[slot]
            .cpu_stage
            .map(|(_, _, s)| s < need)
            .unwrap_or(true)
        {
            if let Some((b, m, _)) = self.frames[slot].cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let buf = dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(need)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )?;
            let req = dev.get_buffer_memory_requirements(buf);
            let mem = dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(find_mem(
                        &self.mem_props,
                        req.memory_type_bits,
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )),
                None,
            )?;
            dev.bind_buffer_memory(buf, mem, 0)?;
            self.frames[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.frames[slot].cpu_stage.unwrap();
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        let n = bytes.len().min(need as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        dev.unmap_memory(m);
        Ok(self.frames[slot].cpu_img.unwrap().2)
    }

    /// Record one frame's CSC + HEVC encode (+ RFI) into ring `slot` and submit it WITHOUT waiting.
    /// The slot's fence is polled later (`read_slot`) so this frame's GPU work overlaps the next
    /// capture. `slot` must be free (its prior submission already read back).
    unsafe fn record_submit(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        wire: i64,
    ) -> Result<()> {
        use ash::vk::native as h;
        let (w, h_px) = (self.width, self.height);
        // Copy this slot's Vulkan handles into locals (all `vk::*` handles are Copy) so the rest of
        // the function can still borrow `&mut self` for the import/CSC helpers without aliasing
        // `self.frames`. Shared objects (csc_pipe/layout, dpb, session) stay on `self`.
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let csc_set = self.frames[slot].csc_set;
        let y_img = self.frames[slot].y_img;
        let uv_img = self.frames[slot].uv_img;
        let nv12_src = self.frames[slot].nv12_src;
        let nv12_view = self.frames[slot].nv12_view;

        // ---- 1. decide frame type + reference (RFI) ----
        let mut is_idr = self.first_frame || self.force_kf;
        let mut ref_slot = self.prev_slot;
        let mut recovery = false;
        if let Some(lf) = self.pending_loss.take() {
            if !is_idr {
                match pick_recovery_slot(&self.slot_wire, lf) {
                    Some(s) => {
                        ref_slot = s;
                        recovery = true;
                        tracing::debug!(
                            loss_first = lf,
                            anchor_slot = s,
                            anchor_wire = self.slot_wire[s],
                            "vulkan-encode: emitting clean recovery-anchor P-frame (references a known-good frame older than the loss, no IDR)"
                        );
                    }
                    None => {
                        is_idr = true;
                        tracing::debug!(loss_first = lf, "vulkan-encode: no resident reference older than the loss — forcing IDR");
                    }
                }
            }
        }
        let poc: i32 = if is_idr { 0 } else { self.poc };
        let mut setup_idx = (self.enc_count % DPB_SLOTS as u64) as usize;
        if !is_idr && setup_idx == ref_slot {
            setup_idx = (setup_idx + 1) % DPB_SLOTS as usize;
        }
        let ref_poc = if is_idr { 0 } else { self.slot_poc[ref_slot] };
        let ref_delta = (poc - ref_poc).max(1);

        // ---- 2. RGB source -> compute_cmd: prep barriers + CSC + copy into nv12_src ----
        let cw = frame.width.min(w);
        let ch = frame.height.min(h_px);
        let dev = self.device.clone(); // cheap handle clone -> lets us also call &mut self helpers
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        let rgb_view = match &frame.payload {
            FramePayload::Dmabuf(d) => {
                // Reuse the per-buffer import (PipeWire cycles a small pool) — no per-frame VkImage
                // create/import/destroy. The producer wrote new content out-of-band, so still acquire
                // from FOREIGN each frame; a fresh import starts UNDEFINED (preserves modifier-tiled
                // data), a cached one is already SHADER_READ_ONLY_OPTIMAL.
                let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                // First import: acquire from the foreign producer (UNDEFINED preserves the modifier-tiled
                // bytes). Cached re-read: we still own it, so no queue-family transfer — just a visibility
                // barrier so the shader read sees the content the producer wrote out-of-band this frame
                // (single-GPU coherent; the capture layer guarantees the buffer is ready at hand-off).
                let (old, src_qf, dst_qf) = if fresh {
                    (
                        vk::ImageLayout::UNDEFINED,
                        vk::QUEUE_FAMILY_FOREIGN_EXT,
                        self.compute_family,
                    )
                } else {
                    (
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::QUEUE_FAMILY_IGNORED,
                        vk::QUEUE_FAMILY_IGNORED,
                    )
                };
                let acq = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                    .old_layout(old)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(src_qf)
                    .dst_queue_family_index(dst_qf)
                    .image(img)
                    .subresource_range(color_range(0));
                dev.cmd_pipeline_barrier2(
                    compute_cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                );
                view
            }
            FramePayload::Cpu(bytes) => {
                let fmt = pixel_to_vk(frame.format).context("unsupported CPU pixel format")?;
                let view = self.ensure_cpu_rgb(slot, fmt, bytes)?;
                let (img, ..) = self.frames[slot].cpu_img.unwrap();
                let (stage, ..) = self.frames[slot].cpu_stage.unwrap();
                let to_dst = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(color_range(0));
                dev.cmd_pipeline_barrier2(
                    compute_cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                );
                dev.cmd_copy_buffer_to_image(
                    compute_cmd,
                    stage,
                    img,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: self.width,
                            height: self.height,
                            depth: 1,
                        })],
                );
                let to_read = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(color_range(0));
                dev.cmd_pipeline_barrier2(
                    compute_cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                );
                view
            }
            _ => bail!("vulkan-encode: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
        };
        let _ = (cw, ch);
        self.bind_rgb(csc_set, rgb_view);

        // y/uv -> GENERAL (shader write); nv12_src -> GENERAL (transfer dst, discard prior)
        let to_general = |img, dst_stage, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(dst_stage)
                .dst_access_mask(dst_access)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        let pre = [
            to_general(
                y_img,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            ),
            to_general(
                uv_img,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            ),
            to_general(
                nv12_src,
                vk::PipelineStageFlags2::ALL_TRANSFER,
                vk::AccessFlags2::TRANSFER_WRITE,
            ),
        ];
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre),
        );

        dev.cmd_bind_pipeline(compute_cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
        dev.cmd_bind_descriptor_sets(
            compute_cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.csc_layout,
            0,
            &[csc_set],
            &[],
        );
        dev.cmd_dispatch(compute_cmd, (w / 2).div_ceil(8), (h_px / 2).div_ceil(8), 1);

        // y/uv shader-write -> transfer-read (stay GENERAL); then copy into nv12 planes
        let yuv_rd = |img| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[yuv_rd(y_img), yuv_rd(uv_img)]),
        );
        let plane_copy = |src_aspect, dst_aspect, ew, eh| {
            vk::ImageCopy::default()
                .src_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(src_aspect)
                        .layer_count(1),
                )
                .dst_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(dst_aspect)
                        .layer_count(1),
                )
                .extent(vk::Extent3D {
                    width: ew,
                    height: eh,
                    depth: 1,
                })
        };
        dev.cmd_copy_image(
            compute_cmd,
            y_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_0,
                w,
                h_px,
            )],
        );
        dev.cmd_copy_image(
            compute_cmd,
            uv_img,
            vk::ImageLayout::GENERAL,
            nv12_src,
            vk::ImageLayout::GENERAL,
            &[plane_copy(
                vk::ImageAspectFlags::COLOR,
                vk::ImageAspectFlags::PLANE_1,
                w / 2,
                h_px / 2,
            )],
        );
        dev.end_command_buffer(compute_cmd)?;

        // ---- 3. author HEVC Std structs + record encode into `cmd` ----
        let mut pic_flags: h::StdVideoEncodeH265PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_is_reference(1);
        if is_idr {
            pic_flags.set_IrapPicFlag(1);
        }
        pic_flags.set_pic_output_flag(1);
        let mut std_pic: h::StdVideoEncodeH265PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.pic_type = if is_idr {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        std_pic.PicOrderCntVal = poc;
        let mut rps: h::StdVideoH265ShortTermRefPicSet = std::mem::zeroed();
        rps.num_negative_pics = 1;
        rps.delta_poc_s0_minus1[0] = (ref_delta - 1) as u16;
        rps.used_by_curr_pic_s0_flag = 1;
        let mut ref_lists: h::StdVideoEncodeH265ReferenceListsInfo = std::mem::zeroed();
        ref_lists.RefPicList0 = [0xff; 15];
        ref_lists.RefPicList1 = [0xff; 15];
        ref_lists.RefPicList0[0] = ref_slot as u8;
        if !is_idr {
            std_pic.pShortTermRefPicSet = &rps;
            std_pic.pRefLists = &ref_lists;
        }
        let mut sh_flags: h::StdVideoEncodeH265SliceSegmentHeaderFlags = std::mem::zeroed();
        sh_flags.set_first_slice_segment_in_pic_flag(1);
        sh_flags.set_slice_loop_filter_across_slices_enabled_flag(1);
        let mut std_sh: h::StdVideoEncodeH265SliceSegmentHeader = std::mem::zeroed();
        std_sh.flags = sh_flags;
        std_sh.slice_type = if is_idr {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
        } else {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
        };
        std_sh.MaxNumMergeCand = 5;
        let slice = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
            .constant_qp(0)
            .std_slice_segment_header(&std_sh);
        let slices = [slice];
        let mut h265_pic = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(&slices)
            .std_picture_info(&std_pic);

        // setup slot (reconstruct into) + reference slot (read from)
        let ext2d = vk::Extent2D {
            width: w,
            height: h_px,
        };
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        setup_std.pic_type = std_pic.pic_type;
        setup_std.PicOrderCntVal = poc;
        let mut setup_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let mut setup_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_a);
        let begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_b);

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        ref_std.pic_type = if ref_poc == 0 {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        ref_std.PicOrderCntVal = ref_poc;
        let mut ref_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let mut ref_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_a);
        let ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_b);
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        // CBR rate control (chained manually; push_next would clobber rc.p_next)
        let rc_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate)
            .max_bitrate(self.bitrate)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1)];
        let h265_rc = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP)
            .gop_frame_count(u32::MAX)
            .idr_period(u32::MAX)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(1000)
            .initial_virtual_buffer_size_in_ms(500);
        rc.p_next = &h265_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        dev.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        dev.cmd_reset_query_pool(cmd, query_pool, 0, 1);
        // nv12_src GENERAL -> VIDEO_ENCODE_SRC (semaphore already ordered the CSC copy before this)
        let mut pre_enc = vec![vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(nv12_src)
            .subresource_range(color_range(0))];
        if self.first_frame {
            pre_enc.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                    .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.dpb_image)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: DPB_SLOTS,
                    }),
            );
        } else {
            // Pipelining hazard: the previous frame's encode reconstruct-writes its DPB setup slot
            // while this one may already be recording. Order that write before this frame's
            // reference-read/write of the DPB. Barrier first scope covers all prior-submitted encode
            // work on this queue (submission order), so it spans the two separate command buffers.
            pre_enc.push(
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                    .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                    .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                    .dst_access_mask(
                        vk::AccessFlags2::VIDEO_ENCODE_READ_KHR
                            | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                    )
                    .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(self.dpb_image)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: DPB_SLOTS,
                    }),
            );
        }
        dev.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre_enc),
        );

        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        if !self.first_frame {
            begin.p_next = rc_ptr;
        } // CBR is current state after frame 0's control
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL,
            );
            ctrl.p_next = rc_ptr;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(nv12_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(&setup_slot)
            .push_next(&mut h265_pic);
        if !is_idr {
            enc = enc.reference_slots(&enc_refs);
        }
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        dev.end_command_buffer(cmd)?;

        // ---- 4. submit compute (signal csc_sem) then encode (wait csc_sem, signal fence).
        //         Non-blocking: `fence` is polled later so this frame's CSC+encode overlaps the next
        //         capture/submit. Per-slot cmd/sem/fence make ring frames independent; the DPB
        //         barrier above orders slot N's reconstruct-write before N+1's reference-read. ----
        dev.reset_fences(&[fence])?;
        let ccmds = [compute_cmd];
        let sems = [csc_sem];
        dev.queue_submit(
            self.compute_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ccmds)
                .signal_semaphores(&sems)],
            vk::Fence::null(),
        )?;
        let ecmds = [cmd];
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        dev.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ecmds)
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&wait_stages)],
            fence,
        )?;
        // Stash the metadata `read_slot` needs once `fence` signals.
        self.frames[slot].pts_ns = frame.pts_ns;
        self.frames[slot].keyframe = is_idr;
        self.frames[slot].recovery_anchor = recovery;

        // ---- 5. advance DPB bookkeeping (in submission order, before returning) ----
        if is_idr {
            self.slot_wire.iter_mut().for_each(|s| *s = -1);
            self.slot_poc.iter_mut().for_each(|s| *s = -1);
        }
        self.slot_wire[setup_idx] = wire;
        self.slot_poc[setup_idx] = poc;
        self.prev_slot = setup_idx;
        self.poc = poc + 1;
        self.enc_count += 1;
        self.first_frame = false;
        self.force_kf = false;
        Ok(())
    }

    /// Read one completed slot's bitstream into an `EncodedFrame` (VPS/SPS/PPS prepended on
    /// keyframes). Caller must have confirmed the slot's fence is signaled (blocking wait or probe).
    unsafe fn read_slot(&mut self, slot: usize) -> Result<EncodedFrame> {
        let dev = self.device.clone();
        let f = &self.frames[slot];
        let mut fb = [[0u32; 2]; 1];
        dev.get_query_pool_results(f.query_pool, 0, &mut fb, vk::QueryResultFlags::WAIT)?;
        let (off, len) = (fb[0][0] as usize, fb[0][1] as usize);
        let p =
            dev.map_memory(f.bs_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *const u8;
        let mut data = Vec::with_capacity(self.header.len() + len);
        if f.keyframe {
            data.extend_from_slice(&self.header);
        } // keyframes carry VPS/SPS/PPS
        data.extend_from_slice(std::slice::from_raw_parts(p.add(off), len));
        dev.unmap_memory(f.bs_mem);
        Ok(EncodedFrame {
            data,
            pts_ns: f.pts_ns,
            keyframe: f.keyframe,
            recovery_anchor: f.recovery_anchor,
        })
    }

    /// Acquire a free ring slot (blocking-draining the oldest if the ring is full), record+submit
    /// this frame into it without waiting, and track it as in-flight (FIFO).
    unsafe fn enqueue(&mut self, frame: &CapturedFrame, wire: i64) -> Result<()> {
        // Backpressure: if every slot is outstanding, block on the oldest, read it into `pending`,
        // and free it — that oldest slot is exactly the round-robin `ring` cursor we reuse next.
        while self.in_flight.len() >= self.frames.len() {
            let slot = self.in_flight.pop_front().unwrap();
            self.device
                .wait_for_fences(&[self.frames[slot].fence], true, u64::MAX)?;
            let done = self.read_slot(slot)?;
            self.pending.push_back(done);
        }
        let slot = self.ring;
        self.ring = (self.ring + 1) % self.frames.len();
        self.record_submit(slot, frame, wire)?;
        self.in_flight.push_back(slot);
        Ok(())
    }
}

impl Encoder for VulkanVideoEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        let wire = self.auto_wire;
        self.auto_wire += 1;
        // SAFETY: `enqueue` records/submits into a free ring slot owned by this encoder without
        // blocking on GPU completion (poll() does); `&mut self` guarantees exclusive access.
        unsafe { self.enqueue(frame, wire) }
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.auto_wire = wire_index as i64 + 1;
        // SAFETY: see `submit` — exclusive `&mut self`, all Vulkan work confined to owned objects.
        unsafe { self.enqueue(frame, wire_index as i64) }
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        // Can we anchor a clean P-frame to a resident slot strictly older than the loss?
        match pick_recovery_slot(&self.slot_wire, first_frame) {
            Some(_) => {
                self.pending_loss = Some(first_frame);
                true
            }
            None => {
                self.force_kf = true;
                tracing::debug!(first_frame, last_frame, "vulkan-encode RFI declined: no resident reference older than the loss — caller will keyframe");
                false
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Backpressure-drained frames (already read, oldest) come out first, then the oldest slot
        // still in flight once its fence signals — both in submission order. Non-blocking: an
        // unfinished frame returns None so the caller keeps the pipeline moving.
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let Some(&slot) = self.in_flight.front() else {
            return Ok(None);
        };
        // SAFETY: probing a fence + reading back this slot's own owned objects under `&mut self`.
        let ready = unsafe { self.device.get_fence_status(self.frames[slot].fence)? };
        if !ready {
            return Ok(None);
        }
        self.in_flight.pop_front();
        // SAFETY: fence signaled ⟹ this slot's CSC+encode is complete; read its bitstream.
        Ok(Some(unsafe { self.read_slot(slot)? }))
    }

    fn reset(&mut self) -> bool {
        // Abandon everything in flight: wait the GPU idle, discard unread slots + queued output, and
        // restart GOP/DPB state so the next frame is a fresh IDR.
        // SAFETY: `device_wait_idle` guarantees no slot's fence is still pending before we drop them.
        unsafe {
            let _ = self.device.device_wait_idle();
        }
        self.in_flight.clear();
        self.pending.clear();
        self.ring = 0;
        self.first_frame = true;
        self.force_kf = false;
        self.pending_loss = None;
        self.poc = 0;
        self.slot_wire.iter_mut().for_each(|s| *s = -1);
        self.slot_poc.iter_mut().for_each(|s| *s = -1);
        true
    }

    fn flush(&mut self) -> Result<()> {
        // Drain every outstanding slot in order into `pending` so a following poll-loop returns them.
        while let Some(slot) = self.in_flight.pop_front() {
            // SAFETY: wait this slot's fence, then read back its own owned bitstream objects.
            unsafe {
                self.device
                    .wait_for_fences(&[self.frames[slot].fence], true, u64::MAX)?;
                let done = self.read_slot(slot)?;
                self.pending.push_back(done);
            }
        }
        Ok(())
    }
}

impl Drop for VulkanVideoEncoder {
    fn drop(&mut self) {
        // SAFETY: `device_wait_idle` first guarantees no GPU work still references any object, so
        // every handle destroyed below is idle and owned solely by `self`; each is freed exactly once
        // (the drains prevent a double free) and in dependency order (views before images before
        // memory, per-frame objects before their shared pools, session params before session).
        unsafe {
            let _ = self.device.device_wait_idle();
            for (_, _, img, mem, view) in std::mem::take(&mut self.import_cache) {
                self.device.destroy_image_view(view, None);
                self.device.destroy_image(img, None);
                self.device.free_memory(mem, None);
            }
            // Per-frame ring resources (command buffers, descriptor sets freed with their pools).
            for f in std::mem::take(&mut self.frames) {
                self.device.destroy_semaphore(f.csc_sem, None);
                self.device.destroy_fence(f.fence, None);
                self.device.destroy_query_pool(f.query_pool, None);
                self.device.destroy_buffer(f.bs_buf, None);
                self.device.free_memory(f.bs_mem, None);
                for (img, mem, view) in [
                    (f.y_img, f.y_mem, f.y_view),
                    (f.uv_img, f.uv_mem, f.uv_view),
                    (f.nv12_src, f.nv12_mem, f.nv12_view),
                ] {
                    self.device.destroy_image_view(view, None);
                    self.device.destroy_image(img, None);
                    self.device.free_memory(mem, None);
                }
                if let Some((i, m, v, _)) = f.cpu_img {
                    self.device.destroy_image_view(v, None);
                    self.device.destroy_image(i, None);
                    self.device.free_memory(m, None);
                }
                if let Some((b, m, _)) = f.cpu_stage {
                    self.device.destroy_buffer(b, None);
                    self.device.free_memory(m, None);
                }
            }
            self.device.destroy_command_pool(self.compute_pool, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_pipeline(self.csc_pipe, None);
            self.device.destroy_pipeline_layout(self.csc_layout, None);
            self.device.destroy_descriptor_pool(self.csc_pool, None);
            self.device
                .destroy_descriptor_set_layout(self.csc_dsl, None);
            self.device.destroy_sampler(self.sampler, None);
            for &v in &self.dpb_views {
                self.device.destroy_image_view(v, None);
            }
            self.device.destroy_image(self.dpb_image, None);
            self.device.free_memory(self.dpb_mem, None);
            (self.vq_dev.fp().destroy_video_session_parameters_khr)(
                self.device.handle(),
                self.params,
                std::ptr::null(),
            );
            (self.vq_dev.fp().destroy_video_session_khr)(
                self.device.handle(),
                self.session,
                std::ptr::null(),
            );
            for &m in &self.session_mem {
                self.device.free_memory(m, None);
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

// ---------- free helpers ----------

fn color_range(layer: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: layer,
        layer_count: 1,
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

unsafe fn find_mem(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> u32 {
    for i in 0..mp.memory_type_count {
        if (bits & (1 << i)) != 0 && mp.memory_types[i as usize].property_flags.contains(want) {
            return i;
        }
    }
    0
}

/// DRM fourcc -> the VkFormat whose *color* components match (Vulkan handles the byte swizzle).
fn fourcc_to_vk(fourcc: u32) -> Option<vk::Format> {
    // fourcc_code(a,b,c,d) = a | b<<8 | c<<16 | d<<24
    const XR24: u32 = 0x3432_5258; // XRGB8888
    const AR24: u32 = 0x3432_5241; // ARGB8888
    const XB24: u32 = 0x3432_4258; // XBGR8888
    const AB24: u32 = 0x3432_4241; // ABGR8888
    match fourcc {
        XR24 | AR24 => Some(vk::Format::B8G8R8A8_UNORM),
        XB24 | AB24 => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

fn pixel_to_vk(fmt: PixelFormat) -> Option<vk::Format> {
    match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => Some(vk::Format::B8G8R8A8_UNORM),
        PixelFormat::Rgbx | PixelFormat::Rgba => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

unsafe fn make_view(
    device: &ash::Device,
    image: vk::Image,
    fmt: vk::Format,
    layer: u32,
) -> Result<vk::ImageView> {
    Ok(device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(color_range(layer)),
        None,
    )?)
}

unsafe fn make_plain_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let img = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(fmt)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
    )?;
    let req = device.get_image_memory_requirements(img);
    let mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )),
        None,
    )?;
    device.bind_image_memory(img, mem, 0)?;
    let view = make_view(device, img, fmt, 0)?;
    Ok((img, mem, view))
}

unsafe fn make_video_image(
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
    let mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(find_mem(
                mp,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )),
        None,
    )?;
    device.bind_image_memory(img, mem, 0)?;
    Ok((img, mem))
}

/// Build one in-flight frame's private resources: NV12 encode-src, Y/UV CSC scratch, its CSC
/// descriptor set (Y/UV bound now, RGB per use), the bitstream buffer + feedback query, and the
/// per-frame command buffers + sync. `profile_list`/`profile` are borrowed only during creation.
unsafe fn make_frame(
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
) -> Result<Frame> {
    // NV12 encode-src (filled by the CSC copy) — concurrent compute+encode.
    let (nv12_src, nv12_mem) = make_video_image(
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
    let nv12_view = make_view(device, nv12_src, NV12, 0)?;
    // CSC scratch (Y R8 full-res, UV RG8 half-res).
    let (y_img, y_mem, y_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8_UNORM,
        w,
        h,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    let (uv_img, uv_mem, uv_view) = make_plain_image(
        device,
        mem_props,
        vk::Format::R8G8_UNORM,
        w / 2,
        h / 2,
        vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
    )?;
    // Descriptor set — Y/UV storage bindings fixed; binding 0 (RGB) rewritten per use.
    let dsls = [csc_dsl];
    let csc_set = device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(csc_pool)
            .set_layouts(&dsls),
    )?[0];
    let y_info = [vk::DescriptorImageInfo::default()
        .image_view(y_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let uv_info = [vk::DescriptorImageInfo::default()
        .image_view(uv_view)
        .image_layout(vk::ImageLayout::GENERAL)];
    device.update_descriptor_sets(
        &[
            vk::WriteDescriptorSet::default()
                .dst_set(csc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(csc_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&uv_info),
        ],
        &[],
    );
    // Bitstream buffer + feedback query.
    let bs_buf = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(bs_size)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .push_next(profile_list),
        None,
    )?;
    let bs_req = device.get_buffer_memory_requirements(bs_buf);
    let bs_mem = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(bs_req.size)
            .memory_type_index(find_mem(
                mem_props,
                bs_req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )),
        None,
    )?;
    device.bind_buffer_memory(bs_buf, bs_mem, 0)?;
    let mut fb_ci = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
        vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
            | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
    );
    fb_ci.p_next = profile as *const _ as *const c_void;
    let mut query_ci = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
        .query_count(1);
    query_ci.p_next = &fb_ci as *const _ as *const c_void;
    let query_pool = device.create_query_pool(&query_ci, None)?;
    // Command buffers + per-frame sync.
    let cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(1),
    )?[0];
    let compute_cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(compute_pool)
            .command_buffer_count(1),
    )?[0];
    let csc_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
    let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
    Ok(Frame {
        compute_cmd,
        cmd,
        csc_sem,
        fence,
        query_pool,
        bs_buf,
        bs_mem,
        csc_set,
        y_img,
        y_mem,
        y_view,
        uv_img,
        uv_mem,
        uv_view,
        nv12_src,
        nv12_mem,
        nv12_view,
        cpu_img: None,
        cpu_stage: None,
        pts_ns: 0,
        keyframe: false,
        recovery_anchor: false,
    })
}

/// Author VPS/SPS/PPS (Main, level 4.0, low-latency, conformance-window crop) and return the
/// session-parameters object + the encoded header bytes (VPS+SPS+PPS NALs) for keyframes.
unsafe fn build_parameters(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    venc_dev: &ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
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
    let ci = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut h265_ci);
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
        bail!("get header bytes: {r:?}");
    }
    buf.truncate(size);
    Ok((params, buf))
}

#[cfg(test)]
mod tests {
    use super::VulkanVideoEncoder;
    use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
    use crate::encode::{Codec, Encoder};

    fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        CapturedFrame {
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
        }
    }

    /// Full `open` → IDR → P-frames → RFI-recovery path through the real [`VulkanVideoEncoder`] on
    /// whatever Vulkan device is present (RADV on the test bed). Exercises the CPU→NV12 compute CSC,
    /// the NV12 plane copy, the DPB ring and the reference-slot RFI end-to-end, and dumps the
    /// elementary stream to `$HOME/vkenc-host-smoke.h265` for an out-of-band `ffmpeg` decode check.
    /// `#[ignore]`d so it only runs where a real `VK_KHR_video_encode_h265` driver exists — build in
    /// the distrobox, run on the host:
    ///   cargo test -p punktfunk-host --features vulkan-encode --no-run
    ///   <host> target/debug/deps/punktfunk_host-<hash> --ignored --nocapture vulkan_smoke
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke() {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc = VulkanVideoEncoder::open(Codec::H265, w, h, 60, 10_000_000).expect("open");
        assert!(enc.caps().supports_rfi, "must advertise RFI");

        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [200, 200, 40, 255],
            [40, 200, 200, 255],
            [200, 40, 200, 255],
        ];
        let mut aus: Vec<crate::encode::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            if i == 4 {
                // simulate loss of wire frame 3 → expect a clean recovery anchor referencing frame 2
                assert!(
                    enc.invalidate_ref_frames(3, 3),
                    "RFI should find an older-than-loss slot"
                );
            }
            enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            // The encoder is pipelined now: submit() no longer blocks, so drain whatever completed
            // (FIFO = submission order) and finish the tail via flush below.
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");

        let (mut keyframes, mut anchors) = (0usize, 0usize);
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.data.is_empty(), "AU {i} empty");
            keyframes += au.keyframe as usize;
            anchors += au.recovery_anchor as usize;
            if i == 0 {
                assert!(au.keyframe, "frame 0 must be IDR");
            }
            if i == 4 {
                assert!(
                    au.recovery_anchor && !au.keyframe,
                    "frame 4 must be a clean recovery P-frame, not IDR"
                );
            }
        }
        assert_eq!(keyframes, 1, "exactly one IDR (frame 0)");
        assert_eq!(anchors, 1, "exactly one recovery anchor (frame 4)");

        if let Ok(home) = std::env::var("HOME") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p1 = format!("{home}/vkenc-host-smoke.h265");
            let _ = std::fs::write(&p1, &full);
            eprintln!(
                "vulkan_smoke: wrote {p1} ({} bytes, {} AUs)",
                full.len(),
                aus.len()
            );
            // Drop the "lost" AU (wire frame 3). Because the recovery frame re-anchored to frame 2,
            // this must still decode cleanly — the on-host proof that the ported RFI heals real loss
            // without an IDR (a non-RFI encoder's frame 4 would reference frame 3 → decoder corrupts).
            let dropped: Vec<u8> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != 3)
                .flat_map(|(_, a)| a.data.iter().copied())
                .collect();
            let p2 = format!("{home}/vkenc-host-smoke-dropped.h265");
            let _ = std::fs::write(&p2, &dropped);
            eprintln!("vulkan_smoke: wrote {p2} (frame 3 dropped; recovery@4 anchors to frame 2)");
        }
    }
}
