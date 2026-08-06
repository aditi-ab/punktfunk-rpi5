//! GPU frame-hash parity tests (WP-D) — the decode legs are `#[ignore]`d
//! because they need real Vulkan Video hardware; the coherence guards at the
//! bottom of this file are not, and run in ordinary CI.
//!
//! Run on a Vulkan-Video box with:
//!
//! ```text
//! cargo test -p pf-vkdecode --test gpu_parity -- --ignored --nocapture
//! ```
//!
//! (RADV boxes additionally need `RADV_PERFTEST=video_decode` — for AV1 as much as
//! for the other two, and without it `bring_up` reports "no physical device with
//! VK_KHR_video_decode_av1", which reads like missing silicon; multi-GPU boxes
//! pin the vendor with `PF_VKD_SMOKE_VENDOR=0x1002` / `0x10de`, same knob as
//! the smoke tests. Device bring-up lives in `tests/common/mod.rs`.)
//!
//! What they prove: H.264, H.265 and AV1 decoding are all exactly specified — every
//! conformant decoder must produce bit-identical output — so the vendored 25fps
//! vector of each codec is decoded through [`VkH264Decoder`] / [`VkH265Decoder`] /
//! [`VkAv1Decoder`],
//! every output frame's NV12 planes are read back (`vkCmdCopyImageToBuffer` on the
//! graphics queue — GPU→CPU is fine in a test; the pool grows TRANSFER_SRC via the
//! decoders' `PF_VKD_TEST_READBACK` hook), cropped to the display region,
//! SHA-256-hashed in DISPLAY order and compared against goldens from libavcodec's
//! SOFTWARE decoder (the reference implementation — provenance in
//! `data/test-25fps.nv12.sha256`, `data/test-25fps-h265.nv12.sha256` and
//! `data/test-25fps-av1.nv12.sha256`). ALL
//! frames are collected, including the tail `flush` delivers, and the frame count
//! must match libavcodec's too.
//!
//! Every leg runs ONE body ([`collect_hashes`]) over `common::TestDecoder`, so the
//! H.265 and AV1 legs cannot quietly test something weaker than the H.264 one. A box
//! that decodes only some of the three runs those legs and reports the rest as "no
//! physical device with VK_KHR_video_decode_…", which is a fact about the box —
//! and on today's fleet AV1 is the one most likely to say so.
//!
//! The two Annex-B codecs run that body TWICE: once over the vendored vector as it
//! sits, and once over the same vector rewritten to FOUR-byte start codes, which is
//! what the real host emits on 100% of access units in both codecs (1514/1514
//! H.264 and 1133/1133 HEVC, measured off the M0 NVENC corpus). Prefix width
//! carries no information, so both runs must reproduce the same goldens —
//! and submitting the four-byte form to the driver unchanged is precisely the
//! defect that made HEVC unplayable on every driver tested. Until these legs
//! existed no parity vector exercised the form that actually ships. **AV1 has no
//! such twin and needs none**: OBUs are length-delimited, so there is no start-code
//! prefix for a driver to mis-skip and no second framing to test (see
//! `common::split_av1_aus`). Its absence is deliberate.
//!
//! # Why the AV1 leg exists at all
//!
//! Because until it did, the AV1 rung had no pixel evidence whatsoever. An
//! adversarial review of the conversion found four defects — per-frame flags left
//! unset on all 274 frames, a units error in `LoopRestorationSize`, per-reference
//! info describing the wrong picture, and zeroed film-grain fields — and every one
//! of them would have shown as a hash mismatch on frame 0 or shortly after, while
//! NONE of them failed clippy or the crate's unit tests. Type-checking a struct
//! conversion cannot tell you the struct describes the right picture; only the
//! pixels can.
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
use pf_vkdecode::VkAv1Decoder;
use pf_vkdecode::VkH264Decoder;
use pf_vkdecode::VkH265Decoder;
use sha2::Digest;

/// Golden SHA-256 per display-order frame of the H.264 vector, from libavcodec
/// software decode (generation command + ffmpeg version in the file's header).
const GOLDENS_H264: &str = include_str!("data/test-25fps.nv12.sha256");

/// The H.265 twin, cross-checked between two independent FFmpeg builds (header).
const GOLDENS_H265: &str = include_str!("data/test-25fps-h265.nv12.sha256");

/// The AV1 twin: 250 DISPLAYED frames of a 274-coded-frame vector, cross-checked
/// between two independent FFmpeg builds on two architectures AND against the
/// per-frame MD5s cros-codecs vendored beside the vector (full provenance in the
/// file's header — it is the only golden here with a third-party corroboration).
const GOLDENS_AV1: &str = include_str!("data/test-25fps-av1.nv12.sha256");

/// The ten-bit vector and its goldens. No hardware leg in this file consumes them
/// yet — the D3D11VA rung is where the ten-bit parity leg currently runs — but the
/// files live here, beside the other goldens, so the guard that keeps them honest
/// belongs here too and runs on every platform rather than only on Windows.
const TEST_MAIN10_H265: &[u8] = include_bytes!("data/test-main10.h265");
const GOLDENS_MAIN10: &str = include_str!("data/test-main10.p010.sha256");

/// The Main 10 vector is 50 display frames.
const MAIN10_FRAME_COUNT: usize = 50;

/// The H.264 vector's display (conformance-window) region; the goldens hash
/// exactly this as tightly packed NV12.
const DISPLAY_H264: (u32, u32) = (320, 240);

/// The H.265 vector's display region. Its SPS carries NO conformance window at
/// all, so this is also its coded size (golden header) — the two vectors merely
/// HAPPEN to share dimensions, which is why [`Readback`] takes the size as a
/// parameter instead of reading one global pair.
const DISPLAY_H265: (u32, u32) = (320, 240);

/// The AV1 vector's display region — its `render_width` x `render_height`, AV1's
/// answer to a conformance window, and what [`DecodedVkFrame::crop`] carries on this
/// rung. Equal to the coded (post-superres) size for this vector, which
/// [`av1_goldens_and_the_ivf_split_agree_with_the_planner`] pins rather than assumes:
/// a re-synced vector whose render region shrank would make the readback crop a
/// region the goldens never hashed.
const DISPLAY_AV1: (u32, u32) = (320, 240);

/// All three vectors' picture format: 8-bit 4:2:0. H.264 is NV12 by envelope
/// (`derive_caps` wants nothing else), H.265 Main resolves to it from the SPS and
/// AV1 Main (`seq_profile = 0`, `high_bitdepth = 0`) from the sequence header —
/// and [`DecodedVkFrame::format`] exists precisely so a pool misconfigured to
/// P010 fails loudly instead of hashing differently.
const EXPECTED_FORMAT: vk::Format = pf_vkdecode::NV12;

/// Every vendored 25fps vector, in all three codecs, is 250 DISPLAY frames.
///
/// For H.264 and H.265 that is also one per access unit. For AV1 it is emphatically
/// not: its 250 temporal units carry [`AV1_CODED_FRAME_COUNT`] coded frames, 24 of
/// which are HIDDEN — decoded, referenced by later frames, never shown (the vector
/// uses no `show_existing_frame`, so they are displayed by no route at all). The
/// rung delivers one frame per `dpb.outputs` id, so 250 is the number the goldens
/// carry and the number the parity leg must compare.
const FRAME_COUNT: usize = 250;

/// The AV1 vector's CODED frame count — 24 more than [`FRAME_COUNT`].
///
/// Asserted by the CPU guard so the display/coded distinction stays a measured fact
/// rather than a comment: if a re-sync ever made these two numbers equal, the vector
/// would have lost its hidden-frame coverage (the exact thing that makes AV1's
/// multi-frame temporal units worth testing) while every hash still matched.
const AV1_CODED_FRAME_COUNT: usize = 274;

/// The golden file's hash lines (comments and blanks skipped).
fn golden_hashes(file: &'static str) -> Vec<&'static str> {
    file.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Refuse a golden set that could make a parity verdict vacuous.
///
/// Three ways a comparison can "pass" while proving nothing, all closed here:
///
/// - **an empty or short set** — [`assert_bit_identical`] compares `hashes` against
///   `goldens` pairwise and asserts the lengths match, so a file that lost its
///   entries to a bad regeneration would agree with a decoder that delivered
///   nothing. Pinning the count against a constant the CPU guards also re-derive
///   from the planner closes that.
/// - **junk that is not a digest** — a truncated or re-formatted line can never
///   equal a real hash, but a file of blank-looking lines could quietly become a
///   comparison of nothing.
/// - **all entries identical** — the one that matters most on a video codec. If
///   every golden were the same digest, a decoder emitting one frozen frame 250
///   times would pass, which is precisely the failure mode a broken reference
///   conversion produces. All four golden sets here are fully distinct (250/250
///   H.264, 250/250 H.265, 250/250 AV1, 50/50 Main 10), so requiring full
///   distinctness is not a weak bound.
fn assert_goldens_are_a_real_set(goldens: &[&str], expected: usize, path: &str) {
    assert_eq!(
        goldens.len(),
        expected,
        "{path} must carry one hash per display frame"
    );
    assert!(
        goldens
            .iter()
            .all(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit())),
        "{path}: every golden line is a bare lowercase SHA-256 hex digest"
    );
    let distinct = goldens
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len();
    assert_eq!(
        distinct,
        goldens.len(),
        "{path}: {distinct} of {} goldens are distinct — a set with repeats (and \
         above all a set that is ALL one digest) would let a decoder that froze on \
         a single frame pass parity",
        goldens.len()
    );
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
    /// The picture format the pool must carry. Held here rather than read from a
    /// module constant so the sizing below and the per-frame assertion come from
    /// ONE source — a readback sized for eight bits that then accepted a ten-bit
    /// frame would hash half a picture and blame the decoder.
    format: vk::Format,
    /// 1 for NV12, 2 for the `3PACK16` ten-bit family (its samples are 16-bit
    /// words with the ten bits in the high end — the same layout P010 has, which
    /// is why one golden file serves both this rung and the D3D11VA one).
    bytes_per_sample: u32,
    /// `w * h * 3 / 2 * bytes_per_sample` — the tightly packed frame this buffer
    /// holds.
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
        format: vk::Format,
    ) -> Self {
        let (width, height) = display;
        // The two-plane copy below halves both dimensions for the chroma plane, so
        // an odd display region would silently drop a chroma row/column.
        assert_eq!(
            (width % 2, height % 2),
            (0, 0),
            "the display region must be chroma-aligned"
        );
        let bytes_per_sample = match format {
            f if f == pf_vkdecode::NV12 => 1,
            f if f == pf_vkdecode::P010 => 2,
            other => panic!("readback has no sample size for {other:?}"),
        };
        let frame_bytes = (width * height * 3 / 2 * bytes_per_sample) as usize;

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
            format,
            bytes_per_sample,
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
                // A BYTE offset, unlike the extents above, which are texels: the
                // luma plane occupies `w * h * bytes_per_sample` bytes.
                buffer_offset: u64::from(width * height * self.bytes_per_sample),
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
        frame.format, readback.format,
        "frame {index}: the vector must decode into the pool format the readback \
         was built for"
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
/// tail `flush` hands back. One body for all three codecs.
///
/// The flush tail is where the codecs legitimately differ and the body deliberately
/// does not: H.264 and H.265 can hold pictures back for reorder, so their planners'
/// `flush` releases a tail. AV1's planner has no `flush` at all — a shown frame is
/// output by the very temporal unit that decodes it — so `VkAv1Decoder::flush` frees
/// the hidden pictures' images and hands back nothing. Draining afterwards is
/// therefore a no-op for AV1 rather than a special case, and running the identical
/// body means an AV1 rung that ever DID strand a shown frame would be caught by the
/// frame-count assertion instead of hidden by a codec-specific shortcut.
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
    let mut first_divergence: Option<usize> = None;
    for (index, (got, want)) in hashes.iter().zip(goldens.iter()).enumerate() {
        if got.as_str() != *want {
            if mismatches < 10 {
                eprintln!("frame {index}: MISMATCH\n  ours:   {got}\n  golden: {want}");
            }
            first_divergence.get_or_insert(index);
            mismatches += 1;
        }
    }
    // The FIRST divergent index is the whole diagnostic: everything after it may be
    // downstream of that one frame through prediction and the DPB, so a report that
    // only counted mismatches would bury the one number that localises the defect.
    assert!(
        first_divergence.is_none(),
        "{codec}: FIRST DIVERGENT FRAME = {} ({mismatches}/{} frames diverge from \
         libavcodec; up to 10 printed above). Frame 0 is intra-only — if IT is the \
         first, suspect readback geometry (pitch/crop), the picture format, or intra \
         decode / the per-frame parameter conversion; a first divergence LATER points \
         at inter prediction, per-reference info or DPB management, and the frames \
         after it are probably just downstream of it.",
        first_divergence.unwrap_or_default(),
        hashes.len()
    );
    eprintln!(
        "{codec}: {} frames bit-identical to libavcodec software decode",
        hashes.len()
    );
}

/// One H.264 parity run over a caller-supplied AU list.
///
/// The AUs are a parameter rather than a constant because two legs share this
/// body: the vendored vector as it sits (three-byte start codes) and the same
/// vector rewritten to the four-byte ones the real host actually emits. Both
/// must reproduce the SAME goldens, because prefix width carries no
/// information — and running one body twice is what makes that an equality
/// rather than two similar-looking assertions that could drift apart.
fn h264_parity_run(aus: &[&[u8]], label: &str) {
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
                EXPECTED_FORMAT,
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: the decoder is gone (its Drop drained the queue and destroyed its
    // session/pools), the readback's handles are destroyed, and nothing else
    // references the setup's handles.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
    h264_parity_run(&common::split_h264_aus(common::TEST_25FPS_H264), "H.264");
}

/// The same 250 frames, submitted the way the real host submits them.
///
/// A failure here where the leg above passes means the four-byte prefix is
/// reaching the driver — `ring::pack_slices` stopped trimming the leading zero
/// byte, or stopped deriving the slice offsets from the trimmed lengths — which
/// is the defect that made HEVC unplayable on every driver tested.
#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_four_byte_start_codes_decode_bit_identically() {
    let stream = common::h264_four_byte_start_codes(common::TEST_25FPS_H264);
    h264_parity_run(
        &common::split_h264_aus(&stream),
        "H.264 (4-byte start codes)",
    );
}

/// The H.265 twin of [`h264_parity_run`]; see its docs for why the AUs are a
/// parameter.
fn h265_parity_run(
    aus: &[&[u8]],
    goldens_file: &'static str,
    expected_frames: usize,
    bit_depth_luma_minus8: u8,
    format: vk::Format,
    display: (u32, u32),
    label: &str,
) {
    // As the H.264 leg: one codec at a time, `set_var` under the lock.
    let _gpu = common::gpu_lock();

    std::env::set_var("PF_VKD_TEST_READBACK", "1");

    let goldens = golden_hashes(goldens_file);
    assert_eq!(
        goldens.len(),
        expected_frames,
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
            .probe_stream_support(1, bit_depth_luma_minus8)
            .unwrap_or_else(|e| {
                panic!("{label}: the box must host this H.265 shape — {e:?}");
            });
        // SAFETY: as the H.264 leg — live instance/device, queue 0 of
        // `graphics_qf` exists; destroyed at the end of this block.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                display,
                format,
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: as the H.264 leg — decoder and readback are gone.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
    h265_parity_run(
        &common::split_h265_aus(common::TEST_25FPS_H265),
        GOLDENS_H265,
        FRAME_COUNT,
        0,
        EXPECTED_FORMAT,
        DISPLAY_H265,
        "H.265",
    );
}

/// The ten-bit path — the only leg in this file that is not eight-bit.
///
/// Every other golden set in this program is NV12, so no rung had pixel evidence
/// for its ten-bit path: the HDR legs proved a Main10 session BUILDS and streams
/// clean, which a stream decoding to garbage would also do. The goldens are P010
/// and the Vulkan pool is `G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16`, whose
/// samples are 16-bit words with the ten bits in the high end — the same layout,
/// which is why one golden file serves this rung and the D3D11VA one.
#[test]
#[ignore = "needs a Vulkan Video H.265 Main 10 decode device (fleet boxes; see module docs)"]
fn main10_every_frame_hashes_bit_identical_to_libavcodec() {
    h265_parity_run(
        &common::split_h265_aus(TEST_MAIN10_H265),
        GOLDENS_MAIN10,
        MAIN10_FRAME_COUNT,
        2,
        pf_vkdecode::P010,
        (320, 240),
        "HEVC Main 10",
    );
}

/// The HEVC leg of the production prefix form — the one that would have caught
/// the shipped defect. See [`h264_four_byte_start_codes_decode_bit_identically`].
#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_four_byte_start_codes_decode_bit_identically() {
    let stream = common::h265_four_byte_start_codes(common::TEST_25FPS_H265);
    h265_parity_run(
        &common::split_h265_aus(&stream),
        GOLDENS_H265,
        FRAME_COUNT,
        0,
        EXPECTED_FORMAT,
        DISPLAY_H265,
        "H.265 (4-byte start codes)",
    );
}

/// The AV1 twin of [`h265_parity_run`].
///
/// Concrete where the H.265 one is parameterised, because AV1 has exactly one
/// vendored vector and one shape (Main 4:2:0 8-bit, no film grain, 320x240); the
/// facts it hard-codes are re-derived from the planner, without a GPU, by
/// [`av1_goldens_and_the_ivf_split_agree_with_the_planner`], so a re-synced vector
/// of another shape fails in ordinary CI with the reason rather than on the fleet as
/// a confusing probe refusal. A second AV1 vector (Main 10, or one that uses
/// `show_existing_frame`) is the point at which this should grow the same parameters
/// the H.265 body carries — not before.
fn av1_parity_run(aus: &[&[u8]], label: &str) {
    // As the other legs: one codec at a time on the device, and the `set_var` below
    // happens only under this lock (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    std::env::set_var("PF_VKD_TEST_READBACK", "1");

    let goldens = golden_hashes(GOLDENS_AV1);
    // Non-vacuity, before any hardware is touched: the right number of entries, all
    // real digests, all distinct (see the helper's docs — a frozen-frame decoder
    // must not be able to pass this leg).
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-av1.nv12.sha256");
    // …and the leg must actually be fed something. An IVF whose packets failed to
    // parse would hand `collect_hashes` an empty AU list, which delivers no frames
    // and would then fail as a frame-count mismatch that reads like a decoder defect.
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "{label}: the vector must split into {FRAME_COUNT} temporal units"
    );

    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        // As the other parity legs: the readback records on the graphics queue, so a
        // device without a graphics family is skipped rather than defaulted.
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        // SAFETY: as the H.264/H.265 legs — `setup` outlives this block (destroyed
        // below, after the decoder and readback drop at the block's end), it was
        // created with the AV1 decode extension + timeline/sync2 features, and its
        // queue fields name the families/queues it created.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // The construction-time shape gate, on the vector's own facts: 4:2:0, 8-bit,
        // and NO film grain. The third argument is the load-bearing one — grain
        // synthesis is part of the Vulkan decode PROFILE, so a box that offers only
        // the grain-enabled profile (or only the disabled one) refuses HERE with a
        // caps reason instead of failing at the first temporal unit.
        decoder
            .probe_stream_support(1, 8, false)
            .unwrap_or_else(|e| {
                panic!("{label}: the box must host AV1 Main 4:2:0 8-bit, no film grain — {e:?}");
            });
        // SAFETY: as the other legs — live instance/device, queue 0 of `graphics_qf`
        // was created by the bring-up; destroyed at the end of this block.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                DISPLAY_AV1,
                EXPECTED_FORMAT,
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing else
        // references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: as the other legs — the decoder is gone (its Drop drained the queue and
    // destroyed its session/pools) and the readback's handles are destroyed.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

/// The AV1 rung's first pixel evidence.
///
/// 250 temporal units in, 250 DISPLAYED frames out (the 24 hidden frames the vector
/// also codes are decoded, referenced and never shown — module docs), each read back
/// as tightly packed NV12 over its `render_width` x `render_height` region and
/// compared against libavcodec's software decode.
///
/// What a failure looks like, and where to point it:
/// - **frame 0** — the sequence header or the per-frame parameter conversion:
///   `StdVideoAV1SequenceHeader`, the eight per-frame sub-blocks (tile info,
///   quantisation, segmentation, loop filter, CDEF, loop restoration, global motion,
///   film grain), the tile-group ranges, or the readback geometry. AV1 puts in the
///   frame header what H.26x puts in parameter sets, so a single wrong field here
///   damages every frame.
/// - **frame 1** — the first frame with a reference. Per-reference info, the
///   reference-NAME → DPB-slot table, or `ref_frame_idx` ordering.
/// - **later, then everything after** — DPB slot management, `refresh_frame_flags`,
///   or the hidden frames: a run that is clean until roughly the first multi-frame
///   temporal unit and wrong thereafter is the signature of the hidden ALTREF being
///   stored wrong or not at all.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_every_frame_hashes_bit_identical_to_libavcodec() {
    av1_parity_run(&common::split_av1_aus(common::TEST_25FPS_AV1), "AV1");
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
// planner's output count, the vector's shape and the golden set — with NO GPU
// involved. And they are what make the verdicts non-vacuous: a comparison of zero
// frames, or of 250 copies of one digest, would otherwise "pass" on any hardware
// (see [`assert_goldens_are_a_real_set`]).
// ---------------------------------------------------------------------------

#[test]
fn h265_goldens_and_au_split_agree_with_the_planner() {
    use pf_bitstream::h265::H265Planner;

    let goldens = golden_hashes(GOLDENS_H265);
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-h265.nv12.sha256");

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
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps.nv12.sha256");

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

#[test]
fn the_main10_vector_is_ten_bit_and_agrees_with_its_goldens() {
    use pf_bitstream::h265::H265Planner;

    let goldens = golden_hashes(GOLDENS_MAIN10);
    assert_goldens_are_a_real_set(&goldens, MAIN10_FRAME_COUNT, "data/test-main10.p010.sha256");

    let aus = common::split_h265_aus(TEST_MAIN10_H265);
    assert_eq!(
        aus.len(),
        MAIN10_FRAME_COUNT,
        "the Main 10 vector is {MAIN10_FRAME_COUNT} access units"
    );

    let mut planner = H265Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the Main 10 vector must plan without errors, got {e:?}")
        });
        // The whole reason this vector exists. Every other golden set in this
        // program is eight-bit; a regenerated vector that came out eight-bit would
        // turn the ten-bit parity leg into a second run of the eight-bit path, and
        // it would PASS, because its goldens would have been regenerated with it.
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8,
                plan.picture.bit_depth_chroma_minus8,
            ),
            (1, 2, 2),
            "AU {index}: the Main 10 vector must stay 4:2:0 at ten bits"
        );
        if index == 0 {
            assert!(plan.picture.is_idr, "the vector opens with an IDR");
            assert_eq!(
                (plan.picture.coded_width, plan.picture.coded_height),
                (320, 240),
                "the goldens hash a 320x240 picture"
            );
        }
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {}",
        goldens.len()
    );
}

/// The AV1 leg's whole chain, with no GPU: the golden set, the IVF split, the shape
/// the leg hard-codes, and — the one that matters — that **250 goldens is the
/// DISPLAY count of a 274-frame vector**, re-derived from the planner rather than
/// asserted from a comment.
///
/// Every number here is a way the fleet run could otherwise fail for a reason that
/// is not the decoder:
///
/// - a golden file regenerated per CODED frame would carry 274 hashes and the leg
///   would report a frame-count mismatch that reads exactly like dropped frames;
/// - an IVF reader that lost packets would feed a short AU list and the leg would
///   report the same thing;
/// - a re-synced vector at another bit depth, sampling, or with film grain would
///   make `probe_stream_support(1, 8, false)` probe the WRONG Vulkan profile and the
///   readback expect the wrong format, which on hardware surfaces as a caps refusal
///   or half a hashed picture;
/// - and if the 24 hidden frames ever disappeared, the leg would still pass while
///   having quietly stopped exercising multi-frame temporal units at all — the one
///   thing AV1 has that neither H.26x vector does.
#[test]
fn av1_goldens_and_the_ivf_split_agree_with_the_planner() {
    use pf_bitstream::av1::Av1Planner;

    let goldens = golden_hashes(GOLDENS_AV1);
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-av1.nv12.sha256");

    // The AU split the parity leg feeds the decoder: one IVF packet per temporal
    // unit. AV1 carries no start codes, so this is the container's framing rather
    // than something a scan could get subtly wrong — but a truncated or re-muxed
    // vector would still shorten it silently.
    let aus = common::split_av1_aus(common::TEST_25FPS_AV1);
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "the vendored AV1 vector is {FRAME_COUNT} temporal units"
    );
    assert!(
        aus.iter().all(|au| !au.is_empty()),
        "no temporal unit is empty — an IVF reader that returned empty packets would \
         make the parity leg decode nothing and blame the decoder"
    );

    // Walk the CPU planner over the same temporal units. It is the authority on how
    // many frames the GPU leg can possibly deliver: the decoder builds exactly one
    // delivered frame per `dpb.outputs` id, and AV1's planner has no `flush` tail.
    let mut planner = Av1Planner::new();
    let mut outputs = 0usize;
    let mut coded_frames = 0usize;
    let mut multi_frame_units = 0usize;
    let mut show_existing = 0usize;
    let mut warnings = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plans = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("temporal unit {index}: the clean vector must plan without errors, got {e:?}")
        });
        if plans.len() > 1 {
            multi_frame_units += 1;
        }
        for plan in &plans {
            coded_frames += 1;
            outputs += plan.dpb.outputs.len();
            warnings += plan.warnings.len();
            // A `show_existing_frame` decodes nothing and stores nothing.
            if plan.dpb.stored.is_none() {
                show_existing += 1;
            }
            // Pin the picture shape both AV1 legs hard-code. They call
            // `probe_stream_support(1, 8, false)` and assert an NV12 output format;
            // a re-synced Main-10, 4:4:4 or film-grain vector would make both
            // silently probe and expect the WRONG Vulkan decode profile on the
            // fleet — grain synthesis is part of the PROFILE, not a per-frame
            // toggle, so a grain-bearing vector is a different device requirement,
            // not merely different pixels.
            assert_eq!(
                (
                    plan.picture.chroma_format_idc,
                    plan.picture.bit_depth,
                    plan.sequence.film_grain_params_present,
                ),
                (1, 8, false),
                "frame {coded_frames} (temporal unit {index}): the vendored AV1 vector \
                 must stay Main 4:2:0 8-bit with no film grain"
            );
            if coded_frames == 1 {
                assert!(plan.picture.is_key, "the vector opens on a key frame");
                // What `Readback` crops to, and what the goldens hash.
                assert_eq!(
                    (plan.picture.render_width, plan.picture.render_height),
                    DISPLAY_AV1,
                    "the display (render) region the readback asserts against"
                );
                // …and what the pool allocates. Equal to the render region here, so
                // the vector needs no AV1 conformance-window equivalent — the golden
                // header's claim.
                assert_eq!(
                    (plan.picture.upscaled_width, plan.picture.frame_height),
                    DISPLAY_AV1,
                    "the decoded (post-superres) picture IS the display region for \
                     this vector — coded size and render size coincide"
                );
            }
        }
    }

    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {} hashes — the \
         parity leg's frame-count assertion would fail on hardware for a reason that \
         has nothing to do with the GPU",
        goldens.len()
    );
    assert_eq!(
        coded_frames,
        AV1_CODED_FRAME_COUNT,
        "the vendored AV1 vector codes {AV1_CODED_FRAME_COUNT} frames; {} of them are \
         hidden, which is why the goldens are {FRAME_COUNT} and not {coded_frames}",
        AV1_CODED_FRAME_COUNT - FRAME_COUNT
    );
    assert_eq!(
        multi_frame_units, 24,
        "24 temporal units carry two frames each — the hidden ALTREFs, and the only \
         reason AV1's `plan_au` returns a vector at all. If this reaches 0 the parity \
         leg has stopped exercising multi-frame temporal units while still passing"
    );
    assert_eq!(
        show_existing, 0,
        "this vector uses no `show_existing_frame`; if that ever changes, frames start \
         being displayed by a route the decoder handles differently and the display \
         order the goldens assume needs rederiving"
    );
    assert_eq!(
        warnings, 0,
        "a clean conformance vector must plan without concealment — any warning here \
         means the parity leg would be hashing concealed pixels against a clean \
         reference"
    );
}

/// Count Annex-B start codes in `stream` as `(total, three_byte)`.
///
/// Emulation prevention guarantees `00 00 01` cannot occur inside a NAL payload,
/// so every hit is a real prefix; a hit not preceded by a zero byte is a
/// three-byte one.
fn annexb_prefixes(stream: &[u8]) -> (usize, usize) {
    let mut total = 0;
    let mut three_byte = 0;
    for i in 0..stream.len().saturating_sub(2) {
        if stream[i..i + 3] == [0x00, 0x00, 0x01] {
            total += 1;
            if i == 0 || stream[i - 1] != 0x00 {
                three_byte += 1;
            }
        }
    }
    (total, three_byte)
}

// The two guards below are what stop the four-byte hardware legs from passing
// vacuously. Those legs assert that a rewritten vector decodes to the SAME
// goldens as the original — which is trivially true if the rewrite quietly
// returned its input, or dropped NALs the planner never missed. Nothing on the
// fleet would notice; these notice in ordinary CI, with the reason.

#[test]
fn the_h264_four_byte_rewrite_changes_prefixes_and_nothing_else() {
    use pf_bitstream::h264::H264Planner;

    let original = common::TEST_25FPS_H264;
    let rewritten = common::h264_four_byte_start_codes(original);

    let (original_total, original_three) = annexb_prefixes(original);
    let (rewritten_total, rewritten_three) = annexb_prefixes(&rewritten);

    assert!(
        original_three > 0,
        "the vendored H.264 vector is supposed to carry THREE-byte start codes; \
         if it no longer does, `h264_four_byte_start_codes_decode_bit_identically` \
         is feeding the hardware the same bytes as the leg above it and proves \
         nothing"
    );
    assert_eq!(
        rewritten_three, 0,
        "every start code in the rewritten stream must be four-byte — {rewritten_three} \
         of {rewritten_total} are not"
    );
    assert_eq!(
        rewritten_total, original_total,
        "the rewrite must preserve the NAL count exactly ({original_total}), not \
         drop or invent units"
    );
    assert!(
        rewritten.len() > original.len(),
        "widening every prefix cannot shrink the stream"
    );

    // Same access units, same planner verdict: the rewrite changed the framing
    // and nothing the decoder acts on.
    let aus = common::split_h264_aus(&rewritten);
    assert_eq!(
        aus.len(),
        common::split_h264_aus(original).len(),
        "the rewritten stream must split into the same access units"
    );
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "…and there are {FRAME_COUNT} of them"
    );

    let mut planner = H264Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the four-byte rewrite must plan as the original does, got {e:?}")
        });
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs, FRAME_COUNT,
        "the rewritten vector must still output {FRAME_COUNT} pictures"
    );
}

#[test]
fn the_h265_four_byte_rewrite_changes_prefixes_and_nothing_else() {
    use pf_bitstream::h265::H265Planner;

    let original = common::TEST_25FPS_H265;
    let rewritten = common::h265_four_byte_start_codes(original);

    let (original_total, original_three) = annexb_prefixes(original);
    let (rewritten_total, rewritten_three) = annexb_prefixes(&rewritten);

    assert!(
        original_three > 0,
        "the vendored H.265 vector is supposed to carry THREE-byte start codes; \
         if it no longer does, `h265_four_byte_start_codes_decode_bit_identically` \
         proves nothing"
    );
    assert_eq!(
        rewritten_three, 0,
        "every start code in the rewritten stream must be four-byte — {rewritten_three} \
         of {rewritten_total} are not"
    );
    assert_eq!(
        rewritten_total, original_total,
        "the rewrite must preserve the NAL count exactly ({original_total})"
    );
    assert!(
        rewritten.len() > original.len(),
        "widening every prefix cannot shrink the stream"
    );

    let aus = common::split_h265_aus(&rewritten);
    assert_eq!(
        aus.len(),
        common::split_h265_aus(original).len(),
        "the rewritten stream must split into the same access units"
    );
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "…and there are {FRAME_COUNT} of them"
    );

    let mut planner = H265Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the four-byte rewrite must plan as the original does, got {e:?}")
        });
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs, FRAME_COUNT,
        "the rewritten vector must still output {FRAME_COUNT} pictures"
    );
}
