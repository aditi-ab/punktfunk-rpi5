//! GPU frame-hash parity test (WP-D) — `#[ignore]`d because it needs real
//! Vulkan Video hardware.
//!
//! Run on a Vulkan-Video box with:
//!
//! ```text
//! cargo test -p pf-vkdecode --test gpu_parity -- --ignored --nocapture
//! ```
//!
//! (RADV boxes additionally need `RADV_PERFTEST=video_decode`; multi-GPU boxes
//! pin the vendor with `PF_VKD_SMOKE_VENDOR=0x1002` / `0x10de`, same knob as
//! the smoke test.)
//!
//! What it proves: H.264 decoding is exactly specified — every conformant
//! decoder must produce bit-identical output — so the vendored 25fps vector is
//! decoded through [`VkH264Decoder`], every output frame's NV12 planes are read
//! back (`vkCmdCopyImageToBuffer` on the graphics queue — GPU→CPU is fine in a
//! test; the pool grows TRANSFER_SRC via the decoder's `PF_VKD_TEST_READBACK`
//! hook), cropped to the display region, SHA-256-hashed in DISPLAY order and
//! compared against goldens from libavcodec's SOFTWARE decoder (the reference
//! implementation — provenance in `data/test-25fps.nv12.sha256`). ALL frames
//! are collected, including the tail [`VkH264Decoder::flush`] delivers, and the
//! frame count must match libavcodec's too.
//!
//! The readback follows the presenter's exact frame contract: wait the frame's
//! timeline `value`, round-trip the layout, signal `value + 1` in the SAME
//! submission, then `release_frame(frame, true)` — and every submission is
//! host-waited before the next decode, so nothing here races the decode queue.
//!
//! Reading a failure: frame 0 is IDR-only — if it already mismatches, suspect
//! the readback geometry (row pitch / crop) or intra decode; mismatches that
//! only appear on later frames point at inter prediction / DPB management.

#![deny(clippy::undocumented_unsafe_blocks)]

use std::io::Cursor;

use ash::vk;
use ash::vk::Handle;
use cros_codecs::codec::h264::parser::Nalu;
use cros_codecs::codec::h264::parser::NaluType;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DecodedVkFrame;
use pf_vkdecode::DeviceHandles;
use pf_vkdecode::NoopQueueLock;
use pf_vkdecode::VkH264Decoder;
use sha2::Digest;

// The same vendored vector the WP-A tests convert, same relative path.
const TEST_25FPS: &[u8] = include_bytes!(
    "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);

/// Golden SHA-256 per display-order frame, from libavcodec software decode
/// (generation command + ffmpeg version in the file's header).
const GOLDENS: &str = include_str!("data/test-25fps.nv12.sha256");

/// The vector's display (conformance-window) size; the goldens hash exactly
/// this region as tightly packed NV12.
const DISPLAY_W: u32 = 320;
const DISPLAY_H: u32 = 240;
const FRAME_BYTES: usize = (DISPLAY_W * DISPLAY_H * 3 / 2) as usize;

/// Test-only AU splitter, mirroring pf-bitstream's (`#[cfg(test)]`-private there).
fn split_into_aus(stream: &[u8]) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let nalu_offset = cursor.position() as usize;
        let start = nalu_offset - nalu.offset;
        let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
        let first_mb_zero = is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_mb_zero) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// The golden file's hash lines (comments and blanks skipped).
fn golden_hashes() -> Vec<&'static str> {
    GOLDENS
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    sha2::Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Test-only GPU→CPU readback: one persistently mapped staging buffer plus one
/// command buffer on the GRAPHICS queue. Each read follows the presenter's
/// frame contract — wait the frame's timeline `value`, transition the image out
/// of its video layout, copy, restore the layout, signal `value + 1` in the
/// same submission — and is host-waited (fence) before returning, so the test
/// stays fully serialized against the decode queue.
struct Readback {
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *const u8,
}

impl Readback {
    /// # Safety
    ///
    /// `instance`/`pd`/`device` are live; `graphics_qf` names a queue family a
    /// queue was created on (index 0) whose family supports TRANSFER (GRAPHICS
    /// implies it).
    unsafe fn new(
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
        device: &ash::Device,
        graphics_qf: u32,
    ) -> Self {
        // SAFETY: fn contract — live device, queue 0 of this family exists.
        let queue = unsafe { device.get_device_queue(graphics_qf, 0) };
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(graphics_qf);
        // SAFETY: live device; destroyed in `destroy`.
        let cmd_pool = unsafe { device.create_command_pool(&pool_ci, None) }
            .expect("create the readback command pool");
        let alloc_ci = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the pool was just created on this device.
        let cmd = unsafe { device.allocate_command_buffers(&alloc_ci) }
            .expect("allocate the readback command buffer")[0];
        // SAFETY: live device; destroyed in `destroy`.
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .expect("create the readback fence");

        let buffer_ci = vk::BufferCreateInfo::default()
            .size(FRAME_BYTES as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: live device; destroyed in `destroy`.
        let buffer =
            unsafe { device.create_buffer(&buffer_ci, None) }.expect("create the staging buffer");
        // SAFETY: the buffer was just created on this device.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        // SAFETY: live instance + physical device (fn contract).
        let props = unsafe { instance.get_physical_device_memory_properties(pd) };
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let type_index = (0..props.memory_type_count)
            .find(|&i| {
                req.memory_type_bits & (1u32 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(wanted)
            })
            .expect("a HOST_VISIBLE|HOST_COHERENT memory type for the staging buffer");
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index);
        // SAFETY: live device, size from the requirements just queried; freed in
        // `destroy`.
        let memory =
            unsafe { device.allocate_memory(&alloc, None) }.expect("allocate staging memory");
        // SAFETY: fresh buffer bound to fresh memory of the required size.
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }.expect("bind staging memory");
        // SAFETY: the memory is HOST_VISIBLE and not yet mapped; the mapping
        // lives until `destroy` frees the memory (implicit unmap).
        let mapped =
            unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                .expect("map the staging buffer")
                .cast_const()
                .cast::<u8>();

        Self {
            device: device.clone(),
            queue,
            cmd_pool,
            cmd,
            fence,
            buffer,
            memory,
            mapped,
        }
    }

    /// Copy `frame`'s cropped NV12 planes into the staging buffer and return
    /// them tightly packed (Y `w*h` bytes, then interleaved UV `w*h/2` bytes) —
    /// exactly the layout ffmpeg's `-f rawvideo -pix_fmt nv12` writes, so the
    /// hashes compare 1:1 and row pitch/crop padding can never leak in
    /// (`bufferRowLength = 0` packs rows at the copy extent).
    ///
    /// # Safety
    ///
    /// `frame` was delivered by a decoder on this device and is not yet
    /// released; its image carries TRANSFER_SRC usage (the decoder's
    /// `PF_VKD_TEST_READBACK` hook); no other work uses the graphics queue or
    /// this frame's image concurrently (the test is fully serialized).
    unsafe fn read_nv12(&self, frame: &DecodedVkFrame) -> Vec<u8> {
        assert_eq!(
            (frame.crop.width, frame.crop.height),
            (DISPLAY_W, DISPLAY_H),
            "the vector's display size (goldens hash exactly this region)"
        );
        assert_eq!(
            (frame.crop.x % 2, frame.crop.y % 2),
            (0, 0),
            "chroma-aligned crop origin"
        );

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer came from a RESET_COMMAND_BUFFER pool (begin
        // implicitly resets) and its previous submission was fence-waited.
        unsafe { self.device.begin_command_buffer(self.cmd, &begin) }
            .expect("begin the readback command buffer");

        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: frame.layer,
            layer_count: 1,
        };
        // Into TRANSFER_SRC: execution/memory dependencies against the decode
        // are carried by the timeline wait at submit (visibility included), so
        // no src access is needed here.
        let to_transfer = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(frame.layout)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.image)
            .subresource_range(subresource);
        let dep =
            vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&to_transfer));
        // SAFETY: recording state; the image is live until release (fn contract).
        unsafe { self.device.cmd_pipeline_barrier2(self.cmd, &dep) };

        // Two plane copies, crop applied at the source (plane-1 offsets/extents
        // are in the R8G8 plane's own half-resolution coordinates), rows packed
        // into the buffer at the copy extent.
        let layers = |aspect| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: frame.layer,
            layer_count: 1,
        };
        let regions = [
            vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(vk::ImageAspectFlags::PLANE_0),
                image_offset: vk::Offset3D {
                    x: frame.crop.x as i32,
                    y: frame.crop.y as i32,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width: DISPLAY_W,
                    height: DISPLAY_H,
                    depth: 1,
                },
            },
            vk::BufferImageCopy {
                buffer_offset: u64::from(DISPLAY_W * DISPLAY_H),
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(vk::ImageAspectFlags::PLANE_1),
                image_offset: vk::Offset3D {
                    x: (frame.crop.x / 2) as i32,
                    y: (frame.crop.y / 2) as i32,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width: DISPLAY_W / 2,
                    height: DISPLAY_H / 2,
                    depth: 1,
                },
            },
        ];
        // SAFETY: the image is in TRANSFER_SRC_OPTIMAL via the barrier above and
        // carries TRANSFER_SRC usage (fn contract); the buffer's FRAME_BYTES
        // exactly spans the two packed regions.
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                frame.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &regions,
            );
        }

        // Restore the video layout (the presenter contract: the consumer puts
        // the image back exactly as delivered) and make the copy host-readable.
        let restore = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(frame.layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.image)
            .subresource_range(subresource);
        let host_read = vk::BufferMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        let dep = vk::DependencyInfo::default()
            .image_memory_barriers(std::slice::from_ref(&restore))
            .buffer_memory_barriers(std::slice::from_ref(&host_read));
        // SAFETY: recording state; own buffer, live image.
        unsafe { self.device.cmd_pipeline_barrier2(self.cmd, &dep) };
        // SAFETY: recording above is complete and valid.
        unsafe { self.device.end_command_buffer(self.cmd) }.expect("end the readback commands");

        // Wait `value`, signal `value + 1` — the DecodedVkFrame sync contract.
        let wait = vk::SemaphoreSubmitInfo::default()
            .semaphore(frame.semaphore)
            .value(frame.value)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let signal = vk::SemaphoreSubmitInfo::default()
            .semaphore(frame.semaphore)
            .value(frame.value + 1)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let cmd_info = vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd);
        let submit = vk::SubmitInfo2::default()
            .wait_semaphore_infos(std::slice::from_ref(&wait))
            .command_buffer_infos(std::slice::from_ref(&cmd_info))
            .signal_semaphore_infos(std::slice::from_ref(&signal));
        // SAFETY: live queue/fence; the semaphore is the frame's timeline
        // semaphore (fn contract), the fence was reset after its last use.
        unsafe {
            self.device
                .queue_submit2(self.queue, std::slice::from_ref(&submit), self.fence)
        }
        .expect("submit the readback");
        // SAFETY: the fence was just submitted.
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, 10_000_000_000)
        }
        .expect("readback completes within 10s");
        // SAFETY: the fence was observed signalled above.
        unsafe { self.device.reset_fences(&[self.fence]) }.expect("reset the readback fence");

        // SAFETY: `mapped` points at FRAME_BYTES host-coherent bytes; the fence
        // wait (plus the HOST_READ barrier) ordered the device writes before
        // this host read.
        unsafe { std::slice::from_raw_parts(self.mapped, FRAME_BYTES) }.to_vec()
    }

    /// # Safety
    ///
    /// No submission in flight (every `read_nv12` fence-waited before
    /// returning) and nothing else references these handles.
    unsafe fn destroy(&self) {
        // SAFETY: own handles on the live device, idle per the fn contract;
        // freeing the memory implicitly unmaps it.
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
        }
    }
}

/// Wait the frame's decode verdict, read + hash its pixels, release it (with
/// the presenter write-back the readback enqueued). `index` is the display
/// index the hash will land at.
fn consume_frame(
    decoder: &mut VkH264Decoder,
    readback: &Readback,
    frame: &DecodedVkFrame,
    index: usize,
) -> String {
    assert_eq!(
        decoder.wait_status(frame),
        DecodeStatus::Ok,
        "frame {index}: decode op not COMPLETE\n  state: {}",
        decoder.debug_snapshot()
    );
    // SAFETY: the frame is delivered and unreleased on the readback's device;
    // the pool carries TRANSFER_SRC (PF_VKD_TEST_READBACK was set before the
    // decoder's first decode); the test is fully serialized, so nothing else
    // touches the graphics queue or this image.
    let nv12 = unsafe { readback.read_nv12(frame) };
    decoder
        .release_frame(frame, true)
        .unwrap_or_else(|e| panic!("frame {index}: release failed: {e}"));
    sha256_hex(&nv12)
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn every_frame_hashes_bit_identical_to_libavcodec() {
    // The decoder reads this at session creation (first decode call): pool
    // images grow TRANSFER_SRC so vkCmdCopyImageToBuffer is legal.
    std::env::set_var("PF_VKD_TEST_READBACK", "1");

    let goldens = golden_hashes();
    assert_eq!(
        goldens.len(),
        250,
        "the golden file carries one hash per libavcodec frame"
    );

    // ---- instance ----
    // SAFETY: loads the system Vulkan loader; no Vulkan objects exist yet.
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader on this box");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let instance_ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info rooted in locals; the instance is destroyed at the
    // end of this test after everything created from it.
    let instance =
        unsafe { entry.create_instance(&instance_ci, None) }.expect("create a Vulkan 1.3 instance");

    // Optional vendor pin (`PF_VKD_SMOKE_VENDOR`, hex `0x1002` or decimal) — the
    // smoke test's knob, same semantics: makes multi-GPU runs attributable.
    let vendor_filter: Option<u32> = std::env::var("PF_VKD_SMOKE_VENDOR").ok().map(|raw| {
        let trimmed = raw.trim();
        trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .map_or_else(|| trimmed.parse(), |hex| u32::from_str_radix(hex, 16))
            .unwrap_or_else(|_| panic!("PF_VKD_SMOKE_VENDOR is not a PCI vendor id: {raw:?}"))
    });

    // ---- physical device with an H.264 decode queue family ----
    // SAFETY: live instance.
    let physical_devices =
        unsafe { instance.enumerate_physical_devices() }.expect("enumerate physical devices");
    let mut picked: Option<(vk::PhysicalDevice, u32, u32)> = None;
    for pd in physical_devices {
        // SAFETY: `pd` was just enumerated from this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        if vendor_filter.is_some_and(|vendor| props.vendor_id != vendor) {
            continue;
        }
        // SAFETY: `pd` was just enumerated from this instance.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
        let has = |name: &std::ffi::CStr| {
            ext_props.iter().any(|e| {
                e.extension_name_as_c_str()
                    .is_ok_and(|extension| extension == name)
            })
        };
        if !(has(ash::khr::video_queue::NAME)
            && has(ash::khr::video_decode_queue::NAME)
            && has(ash::khr::video_decode_h264::NAME))
        {
            continue;
        }
        // SAFETY: live physical device; the two-call form fills the chained video
        // properties for each family.
        let family_count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
        let mut families: Vec<vk::QueueFamilyProperties2<'_>> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: as above, arrays sized to the reported count.
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families) };
        let flags_per_family: Vec<vk::QueueFlags> = families
            .iter()
            .map(|f| f.queue_family_properties.queue_flags)
            .collect();
        drop(families); // release the &mut borrows so video_props is readable

        let mut decode_qf = None;
        let mut graphics_qf = None;
        for (index, flags) in flags_per_family.iter().enumerate() {
            if flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                graphics_qf = Some(index as u32);
            }
            if flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                && video_props[index]
                    .video_codec_operations
                    .contains(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
                && decode_qf.is_none()
            {
                decode_qf = Some(index as u32);
            }
        }
        // Unlike the smoke test this one NEEDS a graphics queue (the readback
        // runs there), so a device without one is skipped, not defaulted.
        if let (Some(decode), Some(graphics)) = (decode_qf, graphics_qf) {
            picked = Some((pd, decode, graphics));
            break;
        }
    }
    let (pd, decode_qf, graphics_qf) = picked.expect(
        "a physical device with VK_KHR_video_decode_h264, a decode queue and a graphics queue",
    );

    // Attribution header: which device (and driver) this run actually exercised.
    {
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        // SAFETY: live physical device; the chain fills the Vulkan 1.2 core
        // driver-identity struct.
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
        let props = props2.properties;
        eprintln!(
            "picked: {:?} vendor=0x{:04x} driver={:?} info={:?}",
            props.device_name_as_c_str().unwrap_or(c"?"),
            props.vendor_id,
            driver_props.driver_name_as_c_str().unwrap_or(c"?"),
            driver_props.driver_info_as_c_str().unwrap_or(c"?"),
        );
    }

    // ---- logical device: decode (+ graphics) queues, video + sync features ----
    let priorities = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(decode_qf)
        .queue_priorities(&priorities)];
    if graphics_qf != decode_qf {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_qf)
                .queue_priorities(&priorities),
        );
    }
    let extensions = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_decode_queue::NAME.as_ptr(),
        ash::khr::video_decode_h264::NAME.as_ptr(),
    ];
    let mut features12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut features13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let device_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&extensions)
        .push_next(&mut features12)
        .push_next(&mut features13);
    // SAFETY: live physical device, valid create info rooted in locals; destroyed
    // at the end of this test after the decoder drops.
    let device =
        unsafe { instance.create_device(pd, &device_ci, None) }.expect("create the decode device");

    // ---- decode the WHOLE vector, hash every display-order frame ----
    let handles = DeviceHandles {
        get_instance_proc_addr: entry.static_fn().get_instance_proc_addr as usize,
        instance: instance.handle().as_raw() as usize,
        physical_device: pd.as_raw() as usize,
        device: device.handle().as_raw() as usize,
        decode_qf,
        decode_queue_index: 0,
        graphics_qf,
    };
    let mut hashes: Vec<String> = Vec::new();
    {
        // SAFETY: the handles above are live for this whole block (the decoder
        // drops at its end, before the device/instance destroys below), the device
        // was created with the decode extensions + timeline/sync2 features, and
        // the queue fields name the families/queues created above.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // SAFETY: live instance/device; queue 0 of `graphics_qf` was created
        // above; destroyed at the end of this block after its last read.
        let readback = unsafe { Readback::new(&instance, pd, &device, graphics_qf) };

        let aus = split_into_aus(TEST_25FPS);
        for (au_index, au) in aus.iter().enumerate() {
            let mut next = decoder.decode(au).unwrap_or_else(|e| {
                panic!(
                    "AU {au_index}: decode failed: {e}\n  state: {}",
                    decoder.debug_snapshot()
                )
            });
            while let Some(frame) = next {
                let hash = consume_frame(&mut decoder, &readback, &frame, hashes.len());
                hashes.push(hash);
                next = decoder.take_ready();
            }
        }
        // The decoder emits in bumping (display) order and the stream may hold
        // frames — the flush tail belongs in the comparison too.
        decoder.flush();
        while let Some(frame) = decoder.take_ready() {
            let hash = consume_frame(&mut decoder, &readback, &frame, hashes.len());
            hashes.push(hash);
        }
        eprintln!("final state: {}", decoder.debug_snapshot());
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
    }

    // ---- teardown (decoder is gone; its Drop drained the queue) ----
    // SAFETY: every object created from the device (the decoder's pools/session,
    // the readback's buffer/pool/fence) was destroyed above.
    unsafe {
        device.destroy_device(None);
        instance.destroy_instance(None);
    }

    // ---- the verdict ----
    assert_eq!(
        hashes.len(),
        goldens.len(),
        "frame count diverges from libavcodec ({} decoded vs {} golden)",
        hashes.len(),
        goldens.len()
    );
    let mut mismatches = 0usize;
    for (index, (got, want)) in hashes.iter().zip(goldens.iter()).enumerate() {
        if got.as_str() != *want {
            if mismatches < 10 {
                eprintln!("frame {index}: MISMATCH\n  ours:   {got}\n  golden: {want}");
            }
            mismatches += 1;
        }
    }
    assert_eq!(
        mismatches,
        0,
        "{mismatches}/{} frames diverge from libavcodec (first 10 printed above; \
         frame 0 is IDR-only — if IT mismatches, suspect readback geometry \
         (pitch/crop) or intra decode; later-only mismatches point at inter \
         prediction / DPB management)",
        hashes.len()
    );
    eprintln!(
        "{} frames bit-identical to libavcodec software decode",
        hashes.len()
    );
}
