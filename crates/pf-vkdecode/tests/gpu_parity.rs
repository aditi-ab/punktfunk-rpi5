//! GPU frame-hash parity tests (WP-D) — the two decode legs are `#[ignore]`d
//! because they need real Vulkan Video hardware; the coherence guards at the
//! bottom of this file are not, and run in ordinary CI.
//!
//! Run on a Vulkan-Video box with:
//!
//! ```text
//! cargo test -p pf-vkdecode --test gpu_parity -- --ignored --nocapture
//! ```
//!
//! (RADV boxes additionally need `RADV_PERFTEST=video_decode`; multi-GPU boxes
//! pin the vendor with `PF_VKD_SMOKE_VENDOR=0x1002` / `0x10de`, same knob as
//! the smoke tests. Device bring-up lives in `tests/common/mod.rs`.)
//!
//! What they prove: H.264 and H.265 decoding are both exactly specified — every
//! conformant decoder must produce bit-identical output — so the vendored 25fps
//! vector of each codec is decoded through [`VkH264Decoder`] / [`VkH265Decoder`],
//! every output frame's NV12 planes are read back (`vkCmdCopyImageToBuffer` on the
//! graphics queue — GPU→CPU is fine in a test; the pool grows TRANSFER_SRC via the
//! decoders' `PF_VKD_TEST_READBACK` hook), cropped to the display region,
//! SHA-256-hashed in DISPLAY order and compared against goldens from libavcodec's
//! SOFTWARE decoder (the reference implementation — provenance in
//! `data/test-25fps.nv12.sha256` and `data/test-25fps-h265.nv12.sha256`). ALL
//! frames are collected, including the tail `flush` delivers, and the frame count
//! must match libavcodec's too.
//!
//! Both legs run ONE body ([`collect_hashes`]) over `common::TestDecoder`, so the
//! H.265 leg cannot quietly test something weaker than the H.264 one. A box that
//! decodes only one codec runs that leg and reports the other as "no physical
//! device with VK_KHR_video_decode_…", which is a fact about the box.
//!
//! The readback follows the presenter's exact frame contract: wait the frame's
//! timeline `value`, round-trip the layout, signal `value + 1` in the SAME
//! submission, then `release_frame(frame, true)` — and every submission is
//! host-waited before the next decode, so nothing here races the decode queue.
//!
//! Reading a failure: frame 0 is intra-only — if it already mismatches, suspect
//! the readback geometry (row pitch / crop) or intra decode; mismatches that
//! only appear on later frames point at inter prediction / DPB management.

#![deny(clippy::undocumented_unsafe_blocks)]

mod common;

use ash::vk;
use common::TestDecoder;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DecodedVkFrame;
use pf_vkdecode::NoopQueueLock;
use pf_vkdecode::VkH264Decoder;
use pf_vkdecode::VkH265Decoder;
use sha2::Digest;

/// Golden SHA-256 per display-order frame of the H.264 vector, from libavcodec
/// software decode (generation command + ffmpeg version in the file's header).
const GOLDENS_H264: &str = include_str!("data/test-25fps.nv12.sha256");

/// The H.265 twin, cross-checked between two independent FFmpeg builds (header).
const GOLDENS_H265: &str = include_str!("data/test-25fps-h265.nv12.sha256");

/// The H.264 vector's display (conformance-window) region; the goldens hash
/// exactly this as tightly packed NV12.
const DISPLAY_H264: (u32, u32) = (320, 240);

/// The H.265 vector's display region. Its SPS carries NO conformance window at
/// all, so this is also its coded size (golden header) — the two vectors merely
/// HAPPEN to share dimensions, which is why [`Readback`] takes the size as a
/// parameter instead of reading one global pair.
const DISPLAY_H265: (u32, u32) = (320, 240);

/// Both vectors' picture format: 8-bit 4:2:0. H.264 is NV12 by envelope
/// (`derive_caps` wants nothing else), H.265 Main resolves to it from the SPS —
/// and [`DecodedVkFrame::format`] exists precisely so a pool misconfigured to
/// P010 fails loudly instead of hashing differently.
const EXPECTED_FORMAT: vk::Format = pf_vkdecode::NV12;

/// Every vendored 25fps vector, in both codecs, is 250 display frames.
const FRAME_COUNT: usize = 250;

/// The golden file's hash lines (comments and blanks skipped).
fn golden_hashes(file: &'static str) -> Vec<&'static str> {
    file.lines()
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
///
/// The display size is a CONSTRUCTION parameter, not a module constant: it sizes
/// the staging buffer and is the crop every read asserts against, and the two
/// vectors sharing 320x240 today is a coincidence that must not become the next
/// vector's silent corruption.
struct Readback {
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *const u8,
    /// The display region every read copies, and the crop it requires.
    display: (u32, u32),
    /// `w * h * 3 / 2` — the tightly packed NV12 frame this buffer holds.
    frame_bytes: usize,
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
        display: (u32, u32),
    ) -> Self {
        let (width, height) = display;
        // The two-plane copy below halves both dimensions for the R8G8 plane, so
        // an odd display region would silently drop a chroma row/column.
        assert_eq!(
            (width % 2, height % 2),
            (0, 0),
            "the display region must be chroma-aligned"
        );
        let frame_bytes = (width * height * 3 / 2) as usize;

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
            .size(frame_bytes as u64)
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
            display,
            frame_bytes,
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
    /// released; its image carries TRANSFER_SRC usage (the decoders'
    /// `PF_VKD_TEST_READBACK` hook); no other work uses the graphics queue or
    /// this frame's image concurrently (the test is fully serialized).
    unsafe fn read_nv12(&self, frame: &DecodedVkFrame) -> Vec<u8> {
        let (width, height) = self.display;
        assert_eq!(
            (frame.crop.width, frame.crop.height),
            self.display,
            "the vector's display size this readback was built for (the goldens \
             hash exactly this region)"
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
                    width,
                    height,
                    depth: 1,
                },
            },
            vk::BufferImageCopy {
                buffer_offset: u64::from(width * height),
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(vk::ImageAspectFlags::PLANE_1),
                image_offset: vk::Offset3D {
                    x: (frame.crop.x / 2) as i32,
                    y: (frame.crop.y / 2) as i32,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width: width / 2,
                    height: height / 2,
                    depth: 1,
                },
            },
        ];
        // SAFETY: the image is in TRANSFER_SRC_OPTIMAL via the barrier above and
        // carries TRANSFER_SRC usage (fn contract); the buffer's `frame_bytes`
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

        // SAFETY: `mapped` points at `frame_bytes` host-coherent bytes (the
        // buffer was created at that size); the fence wait (plus the HOST_READ
        // barrier) ordered the device writes before this host read.
        unsafe { std::slice::from_raw_parts(self.mapped, self.frame_bytes) }.to_vec()
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
    decoder: &mut impl TestDecoder,
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
    // A pool built for the wrong picture format would decode correctly and then
    // hash differently for a reason no mismatch report could explain
    // (`DecodedVkFrame::format` docs) — refuse it here instead.
    assert_eq!(
        frame.format, EXPECTED_FORMAT,
        "frame {index}: 8-bit 4:2:0 vector must decode into an NV12 pool"
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

/// Decode every AU, hash every delivered frame in display order, including the
/// tail `flush` hands back. One body for both codecs.
fn collect_hashes(
    decoder: &mut impl TestDecoder,
    readback: &Readback,
    aus: &[&[u8]],
) -> Vec<String> {
    let mut hashes: Vec<String> = Vec::new();
    for (au_index, au) in aus.iter().enumerate() {
        let mut next = decoder.decode(au).unwrap_or_else(|e| {
            panic!(
                "AU {au_index}: decode failed: {e}\n  state: {}",
                decoder.debug_snapshot()
            )
        });
        while let Some(frame) = next {
            let hash = consume_frame(decoder, readback, &frame, hashes.len());
            hashes.push(hash);
            next = decoder.take_ready();
        }
    }
    // The decoders emit in bumping (display) order and a stream may hold frames —
    // the flush tail belongs in the comparison too.
    decoder.flush();
    while let Some(frame) = decoder.take_ready() {
        let hash = consume_frame(decoder, readback, &frame, hashes.len());
        hashes.push(hash);
    }
    eprintln!(
        "final state: {} status_queries={}",
        decoder.debug_snapshot(),
        decoder.status_queries()
    );
    hashes
}

/// The verdict, run AFTER teardown so a mismatch panic cannot leave the device
/// alive.
fn assert_bit_identical(hashes: &[String], goldens: &[&str], codec: &str) {
    assert_eq!(
        hashes.len(),
        goldens.len(),
        "{codec}: frame count diverges from libavcodec ({} decoded vs {} golden)",
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
        "{codec}: {mismatches}/{} frames diverge from libavcodec (first 10 printed \
         above; frame 0 is intra-only — if IT mismatches, suspect readback \
         geometry (pitch/crop) or intra decode; later-only mismatches point at \
         inter prediction / DPB management)",
        hashes.len()
    );
    eprintln!(
        "{codec}: {} frames bit-identical to libavcodec software decode",
        hashes.len()
    );
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
    // One codec at a time on the device, and the `set_var` below happens only
    // under this lock (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    // The decoder reads this at session creation (first decode call): pool
    // images grow TRANSFER_SRC so vkCmdCopyImageToBuffer is legal.
    std::env::set_var("PF_VKD_TEST_READBACK", "1");

    let goldens = golden_hashes(GOLDENS_H264);
    assert_eq!(
        goldens.len(),
        FRAME_COUNT,
        "the golden file carries one hash per libavcodec frame"
    );

    let setup = common::bring_up(&common::Request {
        codec: common::H264,
        // Unlike the smoke legs this one NEEDS a graphics queue (the readback
        // records there), so a device without one is skipped, not defaulted.
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        // SAFETY: `setup` outlives this block (destroyed below, after the decoder
        // and readback drop at the block's end), it was created with the H.264
        // decode extensions + timeline/sync2 features, and its queue fields name
        // the families/queues it created.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // SAFETY: live instance/device; queue 0 of `graphics_qf` was created by
        // the bring-up; destroyed at the end of this block after its last read.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                DISPLAY_H264,
            )
        };
        let hashes = collect_hashes(
            &mut decoder,
            &readback,
            &common::split_h264_aus(common::TEST_25FPS_H264),
        );
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: the decoder is gone (its Drop drained the queue and destroyed its
    // session/pools), the readback's handles are destroyed, and nothing else
    // references the setup's handles.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, "H.264");
}

#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
    // As the H.264 leg: one codec at a time, `set_var` under the lock.
    let _gpu = common::gpu_lock();

    std::env::set_var("PF_VKD_TEST_READBACK", "1");

    let goldens = golden_hashes(GOLDENS_H265);
    assert_eq!(
        goldens.len(),
        FRAME_COUNT,
        "the golden file carries one hash per libavcodec frame"
    );

    let setup = common::bring_up(&common::Request {
        codec: common::H265,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        // SAFETY: as the H.264 leg — `setup` outlives this block and was created
        // with the H.265 decode extensions + timeline/sync2 features.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // The construction-time shape gate the client's ladder relies on, on the
        // vector's own facts (Main, 4:2:0, 8-bit → NV12): a device that cannot
        // host the combination refuses here with a caps reason instead of failing
        // mid-stream.
        decoder
            .probe_stream_support(1, 0)
            .expect("the box must host H.265 Main 8-bit 4:2:0 (the vector's shape)");
        // SAFETY: as the H.264 leg — live instance/device, queue 0 of
        // `graphics_qf` exists; destroyed at the end of this block.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                DISPLAY_H265,
            )
        };
        let hashes = collect_hashes(
            &mut decoder,
            &readback,
            &common::split_h265_aus(common::TEST_25FPS_H265),
        );
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: as the H.264 leg — decoder and readback are gone.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, "H.265");
}

// ---------------------------------------------------------------------------
// CPU coherence guards — NOT `#[ignore]`d.
//
// The legs above only run on the fleet, so without these nothing in ordinary CI
// notices that a re-synced vendored vector, a golden regeneration or an edit to
// `common`'s AU splitters has made the two disagree. They would then fail on the
// fleet as a frame-count mismatch, which reads like a decoder defect and costs a
// hardware round trip to disprove.
//
// Each guard pins the whole chain the parity verdict rests on: the AU split, the
// planner's output count, and the golden line count — with NO GPU involved.
// ---------------------------------------------------------------------------

#[test]
fn h265_goldens_and_au_split_agree_with_the_planner() {
    use pf_bitstream::h265::H265Planner;

    let goldens = golden_hashes(GOLDENS_H265);
    assert_eq!(
        goldens.len(),
        FRAME_COUNT,
        "data/test-25fps-h265.nv12.sha256 must carry one hash per display frame"
    );
    assert!(
        goldens
            .iter()
            .all(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit())),
        "every golden line is a bare lowercase SHA-256 hex digest"
    );

    // The AU split the parity leg feeds the decoder. `common::split_h265_aus` is
    // the copy of pf-bitstream's private splitter, and it keys on HEVC's 2-byte
    // NAL header — a `+ 1` there (H.264's offset) silently merges or splits AUs.
    let aus = common::split_h265_aus(common::TEST_25FPS_H265);
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "the vendored H.265 vector is {FRAME_COUNT} access units \
         (pf-bitstream's own planner test pins the same number)"
    );

    // Walk the CPU planner over the same AUs: it is the authority on how many
    // frames the GPU leg can possibly deliver, because the decoder builds exactly
    // one delivered frame per `dpb.outputs` id (plus the flush tail).
    let mut planner = H265Planner::new();
    let mut outputs = 0usize;
    let mut iraps = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!(
                "AU {index}: the clean vector must plan without errors, got {e:?} \
                 — if this is RaslSkipped the vector has gained CRA/RASL pictures \
                 and the parity legs' expected frame count needs rederiving"
            );
        });
        outputs += plan.dpb.outputs.len();
        iraps += usize::from(plan.picture.is_irap);
        // Pin the picture shape the H.265 legs hard-code. They call
        // `probe_stream_support(1, 0)` (4:2:0, 8-bit) and assert the NV12 output
        // format; a re-synced Main-10 or 4:4:4 vector would make both of those
        // silently probe and expect the WRONG profile on the fleet, which is a
        // confusing hardware-only failure. Fail here, on CPU, with the reason.
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8
            ),
            (1, 0),
            "AU {index}: the vendored H.265 vector must stay Main 4:2:0 8-bit — \
             the parity and smoke legs hard-code probe_stream_support(1, 0) and \
             an NV12 output format, so a re-synced vector of another shape needs \
             both legs updated, not just the goldens"
        );
        if index == 0 {
            assert!(plan.picture.is_idr, "the vector opens with an IDR");
            assert_eq!(
                (plan.picture.coded_width, plan.picture.coded_height),
                DISPLAY_H265,
                "the vector is 320x240"
            );
            assert_eq!(
                (
                    plan.picture.display_crop.x,
                    plan.picture.display_crop.y,
                    plan.picture.display_crop.width,
                    plan.picture.display_crop.height,
                ),
                (0, 0, DISPLAY_H265.0, DISPLAY_H265.1),
                "the vector carries NO conformance window — coded size IS display \
                 size (the golden header's claim, and what `Readback` asserts)"
            );
        }
    }
    outputs += planner.flush().outputs.len();

    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {} hashes — \
         the parity leg's frame-count assertion would fail on hardware for a \
         reason that has nothing to do with the GPU",
        goldens.len()
    );
    // No CRA/BLA anywhere means `PlanError::RaslSkipped` — the Ok-skip that
    // returns `Ok(None)` rather than an error (h265 module docs, and
    // `VkH265Decoder::decode`'s RASL arm) — is UNREACHABLE on this vector, so the
    // count above cannot be perturbed by it. If a re-synced vector ever opens with
    // a CRA, this assertion fires first and says where to look.
    assert_eq!(
        iraps, 1,
        "the vector holds exactly one IRAP (the opening IDR); a CRA/BLA would make \
         RASL skips reachable and the expected frame count needs rederiving"
    );
}

#[test]
fn h264_goldens_and_au_split_agree_with_the_planner() {
    use pf_bitstream::h264::H264Planner;

    let goldens = golden_hashes(GOLDENS_H264);
    assert_eq!(
        goldens.len(),
        FRAME_COUNT,
        "data/test-25fps.nv12.sha256 must carry one hash per display frame"
    );
    assert!(
        goldens
            .iter()
            .all(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit())),
        "every golden line is a bare lowercase SHA-256 hex digest"
    );

    let aus = common::split_h264_aus(common::TEST_25FPS_H264);
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "the vendored H.264 vector is {FRAME_COUNT} access units"
    );

    let mut planner = H264Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {} hashes",
        goldens.len()
    );
}
