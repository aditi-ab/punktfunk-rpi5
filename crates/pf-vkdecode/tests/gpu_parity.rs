//! Vulkan Video pixel-parity for H.264, H.265, and AV1.
//!
//! Ignored GPU legs decode vendored and host-produced streams, read back each
//! display-region frame, and require SHA-256 plus display-order frame count
//! (including flush) to match the checked-in libavcodec goldens. H.264/H.265
//! also prove three- and four-byte Annex-B start codes produce identical pixels;
//! AV1 OBUs are length-delimited, so there is no prefix-width leg. CPU guards
//! (not ignored) keep fixture, AU split, golden count, and stream shape coherent.
//! File fixtures do not cover packetisation, reassembly, or loss.
//!
//! Needs a Vulkan Video device for the codec/profile under test. RADV also needs
//! `RADV_PERFTEST=video_decode`. Select a GPU with `PF_VKD_SMOKE_VENDOR=0x1002`
//! or `0x10de`.
//!
//! ```text
//! cargo test -p pf-vkdecode --test gpu_parity -- --ignored --nocapture
//! ```
//!
//! Inputs live under `tests/data`. The legs assert; they write no evidence files.

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

/// One SHA-256 per display-order frame. Provenance is in the file header.
const GOLDENS_H264: &str = include_str!("data/test-25fps.nv12.sha256");

/// H.265 goldens. Provenance is in the file header.
const GOLDENS_H265: &str = include_str!("data/test-25fps-h265.nv12.sha256");

/// 250 displayed frames of a 274-coded-frame vector. Provenance is in the file
/// header.
const GOLDENS_AV1: &str = include_str!("data/test-25fps-av1.nv12.sha256");

/// Frame 0 of the AV1 vector: 320×240 tightly packed NV12, 115200 bytes, hashed
/// by [`GOLDENS_AV1`]'s first line. Intra-only, so a mismatch is this picture.
const AV1_FRAME0: &[u8] = include_bytes!("data/test-25fps-av1.frame0.nv12");

/// Ten-bit HEVC vector and P010 goldens. The CPU guard is here so every platform
/// runs it.
const TEST_MAIN10_H265: &[u8] = include_bytes!("data/test-main10.h265");
const GOLDENS_MAIN10: &str = include_str!("data/test-main10.p010.sha256");

/// Host-emitted low-delay H.264. `max_num_ref_frames = 3` equals DPB depth with
/// `max_num_reorder_frames = 0`, so unmark and C.4.5.3 eviction share an AU and
/// `pSetupReferenceSlot` can name the same slot as a reference.
const LOWDELAY_H264: &[u8] = include_bytes!("data/lowdelay-640x480.h264");
const GOLDENS_LOWDELAY: &str = include_str!("data/lowdelay-640x480.nv12.sha256");

/// 120 display frames at 640×480. Both dimensions are macroblock-aligned, so coded
/// and display sizes agree (no conformance window).
const LOWDELAY_FRAME_COUNT: usize = 120;
const DISPLAY_LOWDELAY: (u32, u32) = (640, 480);

/// Host-emitted low-delay HEVC. Snapshot is after `decode_rps`, so an RPS-dropped
/// picture is never in the `RefPicList` set. Five-picture DPB, four marked, no
/// reorder. Vulkan binds `plan.rps`; the CPU guard pins `removed ∩ dpb_refs == 0`.
const LOWDELAY_H265: &[u8] = include_bytes!("data/lowdelay-640x480.h265");
const GOLDENS_LOWDELAY_H265: &str = include_str!("data/lowdelay-640x480-h265.nv12.sha256");

/// Host-emitted AV1. The only host stream with more than one tile: 3840×2160
/// yields `tile_cols = 1, tile_rows = 2` in one Tile Group OBU. Lower resolutions
/// stay 1×1. Covers multi-tile decode, not fragmentation, reassembly, or loss.
const LOWDELAY_AV1: &[u8] = include_bytes!("data/lowdelay-3840x2160.ivf.av1");
const GOLDENS_LOWDELAY_AV1: &str = include_str!("data/lowdelay-3840x2160-av1.nv12.sha256");

/// Temporal units, displayed frames, and render region. Units and frames are two
/// constants, both 60: deriving one from the other would assert AV1 accounting
/// instead of measuring it. The vendored vector is 250 / 250 / 274.
const LOWDELAY_AV1_UNIT_COUNT: usize = 60;
const LOWDELAY_AV1_FRAME_COUNT: usize = 60;
const DISPLAY_LOWDELAY_AV1: (u32, u32) = (3840, 2160);

/// Own frame count and display region. Not shared with [`LOWDELAY_FRAME_COUNT`]:
/// a size change must fail this leg. Same reason [`DISPLAY_H264`] and
/// [`DISPLAY_H265`] are two 320×240 constants.
const LOWDELAY_H265_FRAME_COUNT: usize = 120;
const DISPLAY_LOWDELAY_H265: (u32, u32) = (640, 480);

const MAIN10_FRAME_COUNT: usize = 50;

/// H.264 display (conformance-window) region. Goldens hash this as packed NV12.
const DISPLAY_H264: (u32, u32) = (320, 240);

/// H.265 display region. The SPS has no conformance window, so this is also the
/// coded size. Sharing 320×240 with H.264 is coincidence; [`Readback`] takes size
/// as a parameter.
const DISPLAY_H265: (u32, u32) = (320, 240);

/// AV1 `render_width` × `render_height` — what [`DecodedVkFrame::crop`] carries.
/// Equal to coded size for this vector; the CPU guard pins that so a shrunken
/// render region cannot crop bytes the goldens never hashed.
const DISPLAY_AV1: (u32, u32) = (320, 240);

/// 8-bit 4:2:0. H.264 is NV12 by envelope; H.265 Main and AV1 Main resolve to it
/// from SPS / sequence header. [`DecodedVkFrame::format`] fails a P010 pool
/// instead of hashing a different layout.
const EXPECTED_FORMAT: vk::Format = pf_vkdecode::NV12;

/// Displayed frames in every 25fps vector. H.264/H.265: one per AU. AV1: 250
/// temporal units carry [`AV1_CODED_FRAME_COUNT`] coded frames, 24 of them hidden
/// (decoded, referenced, never shown; this vector has no `show_existing_frame`).
/// One delivered frame per `dpb.outputs` id, so 250 is the golden count.
const FRAME_COUNT: usize = 250;

/// Coded frames in the AV1 vector — 24 more than [`FRAME_COUNT`]. The CPU guard
/// asserts the gap so a re-sync that drops hidden-frame coverage cannot still
/// match every hash.
const AV1_CODED_FRAME_COUNT: usize = 274;

fn golden_hashes(file: &'static str) -> Vec<&'static str> {
    file.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Refuse a golden set that would make a parity verdict vacuous.
///
/// - Empty or short: [`assert_bit_identical`] pairwise-compares and checks
///   lengths, so a wiped file would agree with a decoder that delivered nothing.
/// - Not a 64-hex digest: a truncated line never matches, but blank-looking
///   lines become a comparison of nothing.
/// - All entries identical: a decoder that froze on one frame would pass.
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

/// Set `PF_VKD_TEST_READBACK` so pool images grow TRANSFER_SRC for the copy.
///
/// The GPU lock is taken by reference: `env::set_var` is unsound from a live
/// multithreaded process, so a borrow states "the caller holds the lock" in the
/// type. This is the file's only `set_var`.
fn arm_test_readback(_gpu: &std::sync::MutexGuard<'static, ()>) {
    // SAFETY: `_gpu` is the binary-wide GPU lock (`common::gpu_lock`). The parity
    // legs are this variable's only writers and readers, and they run one at a time.
    unsafe { std::env::set_var("PF_VKD_TEST_READBACK", "1") };
}

/// GPU→CPU readback: one mapped staging buffer and one command buffer on the
/// graphics queue. Each read waits the frame's timeline `value`, copies, restores
/// layout, signals `value + 1` in the same submit, then host-waits the fence.
///
/// Display size is a constructor argument: it sizes the staging buffer and is the
/// crop every read asserts.
struct Readback {
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *const u8,
    display: (u32, u32),
    /// Pool format. Held here so buffer sizing and the per-frame assert share one
    /// source — an 8-bit buffer that accepted a 10-bit frame would hash half a picture.
    format: vk::Format,
    /// 1 for NV12, 2 for the `3PACK16` ten-bit family (10 bits in the high end of
    /// each 16-bit word — P010's layout).
    bytes_per_sample: u32,
    /// Tightly packed size: `w * h * 3 / 2 * bytes_per_sample`.
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
        // The two-plane copy halves both dimensions for chroma; an odd region
        // would silently drop a chroma row/column.
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

    /// Copy `frame`'s cropped planes into the staging buffer, tightly packed
    /// (Y `w*h`, then interleaved UV `w*h/2`) — ffmpeg `-f rawvideo -pix_fmt nv12`.
    /// `bufferRowLength = 0` packs rows at the copy extent, so pitch/crop padding
    /// cannot leak into the hash.
    ///
    /// # Safety
    ///
    /// `frame` was delivered by a decoder on this device and is not yet
    /// released; its image carries TRANSFER_SRC (`PF_VKD_TEST_READBACK`); no other
    /// work uses the graphics queue or this image concurrently (test is serialized).
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
        // SAFETY: RESET_COMMAND_BUFFER pool (begin implicitly resets); previous
        // submit was fence-waited.
        unsafe { self.device.begin_command_buffer(self.cmd, &begin) }
            .expect("begin the readback command buffer");

        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: frame.layer,
            layer_count: 1,
        };
        // Timeline wait at submit carries decode visibility; no src access here.
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

        // Crop at the source; plane-1 offsets/extents are in the chroma plane's
        // half-resolution coordinates. Rows pack at the copy extent.
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
                // Byte offset (extents above are texels): luma is `w * h * bytes_per_sample`.
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

        // Presenter contract: restore the delivered layout; HOST_READ the copy.
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

        // Wait `value`, signal `value + 1` — the `DecodedVkFrame` sync contract.
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
        // SAFETY: live queue/fence; semaphore is the frame's timeline (fn
        // contract); fence was reset after its last use.
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

        // SAFETY: `mapped` is `frame_bytes` of HOST_COHERENT memory; the fence
        // wait plus HOST_READ barrier ordered the device writes before this read.
        unsafe { std::slice::from_raw_parts(self.mapped, self.frame_bytes) }.to_vec()
    }

    /// # Safety
    ///
    /// No submission in flight (every `read_nv12` fence-waited) and nothing else
    /// references these handles.
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

/// Wait, read, release with the presenter write-back the readback enqueued.
fn consume_frame(
    decoder: &mut impl TestDecoder,
    readback: &Readback,
    frame: &DecodedVkFrame,
    index: usize,
) -> Vec<u8> {
    assert_eq!(
        decoder.wait_status(frame),
        DecodeStatus::Ok,
        "frame {index}: decode op not COMPLETE\n  state: {}",
        decoder.debug_snapshot()
    );
    // Wrong pool format decodes, then hashes a different layout; refuse it here.
    assert_eq!(
        frame.format, readback.format,
        "frame {index}: the vector must decode into the pool format the readback \
         was built for"
    );
    // SAFETY: delivered and unreleased on this device; pool has TRANSFER_SRC
    // (`PF_VKD_TEST_READBACK` set before the first decode); test is serialized.
    let nv12 = unsafe { readback.read_nv12(frame) };
    decoder
        .release_frame(frame, true)
        .unwrap_or_else(|e| panic!("frame {index}: release failed: {e}"));
    nv12
}

/// Decode every AU and hash every delivered frame in display order, including
/// the `flush` tail. One body for all three codecs.
///
/// H.264/H.265 `flush` can release a reorder tail; AV1's planner has no flush — a
/// shown frame is output by the unit that decodes it — so `VkAv1Decoder::flush`
/// frees hidden pictures and returns nothing. The shared body catches a stranded
/// shown frame via the frame-count assert.
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
            let planes = consume_frame(decoder, readback, &frame, hashes.len());
            hashes.push(sha256_hex(&planes));
            next = decoder.take_ready();
        }
    }
    decoder.flush();
    while let Some(frame) = decoder.take_ready() {
        let planes = consume_frame(decoder, readback, &frame, hashes.len());
        hashes.push(sha256_hex(&planes));
    }
    eprintln!(
        "final state: {} status_queries={}",
        decoder.debug_snapshot(),
        decoder.status_queries()
    );
    hashes
}

/// Verdict after teardown so a mismatch panic cannot leave the device alive.
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
    // First divergence localises the defect; later mismatches are often DPB
    // downstream of that one frame.
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
/// AUs are a parameter so the three-byte and four-byte host-prefix legs share this
/// body. Prefix width carries no information; both must match the same goldens.
fn h264_parity_run(aus: &[&[u8]], label: &str) {
    h264_parity_run_against(aus, label, GOLDENS_H264, FRAME_COUNT, DISPLAY_H264);
}

fn h264_parity_run_against(
    aus: &[&[u8]],
    label: &str,
    goldens: &'static str,
    frame_count: usize,
    display: (u32, u32),
) {
    // One codec at a time; `set_var` only under this lock (`common::gpu_lock`).
    let _gpu = common::gpu_lock();

    arm_test_readback(&_gpu);

    let goldens = golden_hashes(goldens);
    assert_eq!(
        goldens.len(),
        frame_count,
        "the golden file carries one hash per libavcodec frame"
    );

    let setup = common::bring_up(&common::Request {
        codec: common::H264,
        // Readback records on graphics; a device without that family is skipped.
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        // SAFETY: `setup` outlives this block; created with H.264 decode
        // extensions + timeline/sync2; queue fields name the families it created.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block after its last read.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                display,
                EXPECTED_FORMAT,
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: decoder Drop drained the queue and destroyed session/pools;
    // readback handles are gone; nothing else references the setup.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
    h264_parity_run(&common::split_h264_aus(common::TEST_25FPS_H264), "H.264");
}

/// Same 250 frames, four-byte start codes as the host emits.
///
/// A failure here where the three-byte leg passes means the extra prefix byte is
/// reaching the driver.
#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_four_byte_start_codes_decode_bit_identically() {
    let stream = common::h264_four_byte_start_codes(common::TEST_25FPS_H264);
    h264_parity_run(
        &common::split_h264_aus(&stream),
        "H.264 (4-byte start codes)",
    );
}

/// Host low-delay H.264. The conformance vector never aliases setup and a
/// reference onto one DPB slot; this stream does ([`LOWDELAY_H264`]). DISTINCT
/// hands the aliased reference the setup's array layer; COINCIDE drops it from
/// `pReferenceSlots`.
#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn low_delay_host_h264_every_frame_hashes_bit_identical_to_libavcodec() {
    h264_parity_run_against(
        &common::split_h264_aus(LOWDELAY_H264),
        "H.264 (low-delay host stream)",
        GOLDENS_LOWDELAY,
        LOWDELAY_FRAME_COUNT,
        DISPLAY_LOWDELAY,
    );
}

/// AU boundaries of a `PUNKTFUNK_DUMP_VIDEO` capture: `.idx` sidecar when present
/// (`offset len flags complete` per line), H.265 splitter otherwise. Skip
/// `complete == 0` — the client is only ever fed complete AUs.
fn field_aus(stream: &[u8], idx_path: &std::path::Path) -> Vec<std::ops::Range<usize>> {
    let Ok(idx) = std::fs::read_to_string(idx_path) else {
        return common::split_h265_aus(stream)
            .iter()
            .map(|au| {
                let start = au.as_ptr() as usize - stream.as_ptr() as usize;
                start..start + au.len()
            })
            .collect();
    };
    idx.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let offset: usize = parts.next()?.parse().ok()?;
            let len: usize = parts.next()?.parse().ok()?;
            let _flags = parts.next()?;
            let complete = parts.next()? != "0";
            (complete && offset + len <= stream.len()).then_some(offset..offset + len)
        })
        .collect()
}

/// Decode a `PUNKTFUNK_DUMP_VIDEO` H.265 capture and write one SHA-256 per
/// display-order frame to `<stream>.pfhash`. `scripts/vkdecode-field-parity.sh`
/// diffs that against ffmpeg and names the first divergent frame.
///
/// - `PF_VKD_FIELD_STREAM=/path/au-*.h265` — the capture (required).
/// - `PF_VKD_FIELD_YUV=12,13` — write those frames' planes to
///   `<stream>.frame<N>.yuv` (optional, after the script names the divergence).
///
/// Per-AU errors (truncated tail, renegotiation): print, count, resume at the
/// next IRAP. A mid-capture resolution change ends the comparable stretch;
/// off-size crops are released unshown.
#[test]
#[ignore = "field triage: set PF_VKD_FIELD_STREAM=/path/capture.h265 (needs a Vulkan Video H.265 decode device; RADV additionally RADV_PERFTEST=video_decode)"]
fn field_h265_stream_writes_frame_hashes_for_ffmpeg_diff() {
    let stream_path = std::env::var("PF_VKD_FIELD_STREAM").expect(
        "PF_VKD_FIELD_STREAM must point at a PUNKTFUNK_DUMP_VIDEO .h265 capture \
         (see this test's docs)",
    );
    let stream = std::fs::read(&stream_path).expect("read the capture");
    let idx_path = std::path::PathBuf::from(format!("{stream_path}.idx"));
    let aus = field_aus(&stream, &idx_path);
    assert!(!aus.is_empty(), "no AUs in the capture");

    // Join at the first AU that plans: pre-IRAP AUs of a mid-session capture fail
    // with AwaitingIdr-shaped errors.
    let mut planner = pf_bitstream::h265::H265Planner::new();
    let mut first_planned = None;
    for (index, range) in aus.iter().enumerate() {
        if let Ok(plan) = planner.plan_au(&stream[range.clone()]) {
            first_planned = Some((index, plan.picture));
            break;
        }
    }
    let (start_index, picture) = first_planned.expect("no AU in the capture plans");
    let (format, label) = match picture.bit_depth_luma_minus8 {
        0 => (pf_vkdecode::NV12, "H.265 (field capture, 8-bit)"),
        2 => (pf_vkdecode::P010, "H.265 (field capture, 10-bit)"),
        other => panic!("unsupported field bit depth: {}", other + 8),
    };
    assert_eq!(
        picture.chroma_format_idc, 1,
        "the readback speaks NV12/P010 — a 4:4:4 capture needs its own leg"
    );
    let display = (picture.display_crop.width, picture.display_crop.height);
    let yuv_wanted: std::collections::BTreeSet<usize> = std::env::var("PF_VKD_FIELD_YUV")
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();

    // One codec at a time; `set_var` under the lock.
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let setup = common::bring_up(&common::Request {
        codec: common::H265,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let mut au_errors = 0usize;
    let mut off_size = 0usize;
    let mut concealed_aus: Vec<usize> = Vec::new();
    let hashes = {
        // SAFETY: `setup` outlives this block; created with H.265 decode
        // extensions + timeline/sync2.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(picture.chroma_format_idc, picture.bit_depth_luma_minus8)
            .unwrap_or_else(|e| panic!("{label}: the box must host this shape — {e:?}"));
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block.
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

        let mut hashes: Vec<String> = Vec::new();
        let sink = |decoder: &mut VkH265Decoder,
                    frame: DecodedVkFrame,
                    hashes: &mut Vec<String>,
                    off_size: &mut usize| {
            if (frame.crop.width, frame.crop.height) != display {
                // Renegotiated stretch — not comparable against this readback.
                *off_size += 1;
                decoder
                    .release_frame(&frame, false)
                    .expect("release an off-size frame unshown");
                return;
            }
            let index = hashes.len();
            let planes = consume_frame(decoder, &readback, &frame, index);
            if yuv_wanted.contains(&index) {
                let path = format!("{stream_path}.frame{index}.yuv");
                std::fs::write(&path, &planes).expect("write the requested .yuv");
                eprintln!("frame {index}: planes written to {path}");
            }
            hashes.push(sha256_hex(&planes));
        };
        for (au_index, range) in aus.iter().enumerate().skip(start_index) {
            match decoder.decode(&stream[range.clone()]) {
                Ok(mut next) => {
                    // Lossy captures hold concealed AUs; ffmpeg conceals them
                    // differently, so a divergence there is not this decoder.
                    let warnings = decoder.take_warnings();
                    if warnings.iter().any(pf_vkdecode::is_integrity_warning_h265) {
                        if concealed_aus.len() < 10 {
                            eprintln!("AU {au_index}: planned with concealment {warnings:?}");
                        }
                        concealed_aus.push(au_index);
                    }
                    while let Some(frame) = next {
                        sink(&mut decoder, frame, &mut hashes, &mut off_size);
                        next = decoder.take_ready();
                    }
                }
                // Print, count, continue — recovery latch resumes at the next IRAP.
                Err(e) => {
                    au_errors += 1;
                    eprintln!("AU {au_index}: decode failed ({e}) — continuing");
                }
            }
        }
        decoder.flush();
        while let Some(frame) = decoder.take_ready() {
            sink(&mut decoder, frame, &mut hashes, &mut off_size);
        }
        eprintln!(
            "final state: {} status_queries={}",
            decoder.debug_snapshot(),
            decoder.status_queries()
        );
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: decoder Drop drained the queue and destroyed session/pools;
    // readback handles are gone; nothing else references the setup.
    unsafe { setup.destroy() };

    let out = format!("{stream_path}.pfhash");
    std::fs::write(&out, hashes.join("\n") + "\n").expect("write the hash file");
    eprintln!(
        "{label}: {} frames hashed → {out} (skipped {start_index} pre-join AUs, \
         {au_errors} AU errors, {off_size} off-size frames); diff with \
         scripts/vkdecode-field-parity.sh",
        hashes.len()
    );
    // No concealed AU: every divergence the diff finds is this decoder's.
    if concealed_aus.is_empty() {
        eprintln!("integrity: no AU needed concealment — any divergence is OURS");
    } else {
        eprintln!(
            "integrity: {} of {} AUs planned with concealment (first: {:?}) — a \
             divergence AT or AFTER the first is likely ffmpeg concealing differently, \
             not this decoder",
            concealed_aus.len(),
            aus.len() - start_index,
            &concealed_aus[..concealed_aus.len().min(10)]
        );
    }
}

/// H.265 twin of [`h264_parity_run`]; AUs are a parameter for the same reason.
fn h265_parity_run(
    aus: &[&[u8]],
    goldens_file: &'static str,
    expected_frames: usize,
    bit_depth_luma_minus8: u8,
    format: vk::Format,
    display: (u32, u32),
    label: &str,
) {
    // One codec at a time; `set_var` under the lock.
    let _gpu = common::gpu_lock();

    arm_test_readback(&_gpu);

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
        // SAFETY: `setup` outlives this block; created with H.265 decode
        // extensions + timeline/sync2.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // Construction-time shape gate: refuse with a caps reason, not mid-stream.
        decoder
            .probe_stream_support(1, bit_depth_luma_minus8)
            .unwrap_or_else(|e| {
                panic!("{label}: the box must host this H.265 shape — {e:?}");
            });
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block.
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

    // SAFETY: decoder and readback are gone.
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

/// Ten-bit path — the only non-8-bit leg here. Goldens are P010; the Vulkan pool
/// is `G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16` (10 bits in the high end of each
/// 16-bit word), the same layout, so one golden file serves both rungs.
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

/// Host low-delay HEVC. Five-picture DPB, four marked references, no reorder: the
/// `SlotMap` retires and reissues a slot on the same AU. The vendored vector
/// reorders and never does that. The CPU guard pins the planner property.
#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn low_delay_host_h265_every_frame_hashes_bit_identical_to_libavcodec() {
    h265_parity_run(
        &common::split_h265_aus(LOWDELAY_H265),
        GOLDENS_LOWDELAY_H265,
        LOWDELAY_H265_FRAME_COUNT,
        0,
        EXPECTED_FORMAT,
        DISPLAY_LOWDELAY_H265,
        "H.265 (low-delay host stream)",
    );
}

/// See [`h264_four_byte_start_codes_decode_bit_identically`].
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

/// AV1 twin of [`h265_parity_run`]. Hard-coded to the vendored shape (Main 4:2:0
/// 8-bit, no film grain, 320×240). [`av1_goldens_and_the_ivf_split_agree_with_the_planner`]
/// re-derives those facts on CPU so a re-synced vector of another shape fails in CI.
fn av1_parity_run(aus: &[&[u8]], label: &str) {
    av1_parity_run_against(
        aus,
        label,
        GOLDENS_AV1,
        "data/test-25fps-av1.nv12.sha256",
        FRAME_COUNT,
        FRAME_COUNT,
        DISPLAY_AV1,
    );
}

/// [`av1_parity_run`] with the stream's own goldens and geometry.
///
/// `units` and `frames` stay separate. Equal for the low-delay host stream (one
/// shown frame per unit); the vendored vector is 250 units / 274 coded / 250 shown.
fn av1_parity_run_against(
    aus: &[&[u8]],
    label: &str,
    goldens_file: &'static str,
    goldens_path: &str,
    units: usize,
    frames: usize,
    display: (u32, u32),
) {
    // One codec at a time; `set_var` only under this lock (`common::gpu_lock`).
    let _gpu = common::gpu_lock();

    arm_test_readback(&_gpu);

    let goldens = golden_hashes(goldens_file);
    assert_goldens_are_a_real_set(&goldens, frames, goldens_path);
    // Empty IVF packets would deliver no frames and look like a decoder defect.
    assert_eq!(
        aus.len(),
        units,
        "{label}: the stream must split into {units} temporal units"
    );

    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        // Readback records on graphics; a device without that family is skipped.
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        // SAFETY: `setup` outlives this block; created with the AV1 decode
        // extension + timeline/sync2; queue fields name the families it created.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // Grain synthesis is part of the Vulkan decode profile: refuse a
        // grain-only (or grain-disabled-only) box here, not at the first unit.
        decoder
            .probe_stream_support(1, 8, false)
            .unwrap_or_else(|e| {
                panic!("{label}: the box must host AV1 Main 4:2:0 8-bit, no film grain — {e:?}");
            });
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                display,
                EXPECTED_FORMAT,
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing else
        // references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: decoder Drop drained the queue and destroyed session/pools;
    // readback handles are gone.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

/// 250 temporal units in, 250 displayed frames out (24 hidden frames are decoded
/// and referenced, never shown). Tightly packed NV12 over the render region.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_every_frame_hashes_bit_identical_to_libavcodec() {
    av1_parity_run(&common::split_av1_aus(common::TEST_25FPS_AV1), "AV1");
}

/// Host AV1 at the only resolution that emits more than one tile.
///
/// The vendored vector is `tile_cols = tile_rows = 1`. This stream is
/// `tile_rows = 2`, `height_in_sbs_minus_1 = [16, 16]`, both tiles in one Tile
/// Group OBU. Readback is 12,441,600 bytes/frame, not 115,200.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn low_delay_host_av1_every_frame_hashes_bit_identical_to_libavcodec() {
    av1_parity_run_against(
        &common::split_av1_aus(LOWDELAY_AV1),
        "AV1 (low-delay host stream, 4K two-tile)",
        GOLDENS_LOWDELAY_AV1,
        "data/lowdelay-3840x2160-av1.nv12.sha256",
        LOWDELAY_AV1_UNIT_COUNT,
        LOWDELAY_AV1_FRAME_COUNT,
        DISPLAY_LOWDELAY_AV1,
    );
}

/// Frame 0 pixels vs libavcodec, byte for byte. The hash leg names no cause;
/// these signatures do:
///
/// - luma identical, chroma differs: `PLANE_1` copy region / chroma-plane origin.
/// - both differ, a shift matches: crop origin or copy extent from the pool;
///   printed `dy`/`dx` is the error.
/// - both differ, deltas ≤ ~8: in-loop filter (CDEF / restoration / deblock).
/// - both differ, large structured deltas: quantisation or tile payloads.
/// - ours constant: nothing was decoded into the image.
///
/// Equality is last so a failure prints the report above the panic.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_frame0_pixels_say_which_plane_and_how_badly() {
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let aus = common::split_av1_aus(common::TEST_25FPS_AV1);
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "the vector must split into 250 units"
    );
    let ours = av1_first_frame(&aus);

    report_nv12_divergence(&ours, AV1_FRAME0, DISPLAY_AV1);
    assert_eq!(
        sha256_hex(&ours),
        golden_hashes(GOLDENS_AV1)[0],
        "AV1 frame 0 is not libavcodec's — read the report above for the class"
    );
    eprintln!("AV1 frame 0 is byte-identical to libavcodec");
}

/// `loop_filter_level[2]` and `[3]` (U/V deblock) as bit offsets from the first AU.
///
/// Derived from syntax: temporal delimiter (2) + sequence header (2+11) +
/// `OBU_FRAME` (1 + 2-byte leb128) put the uncompressed header at byte 18.
/// `loop_filter_level[0]` starts at bit 35; four `f(6)` levels (5.9.11) put U at
/// bit 47 and V at bit 53. The GPU probe re-parses before decode.
const AV1_FRAME0_FILTER_LEVEL_U_BIT: usize = 18 * 8 + 47;
const AV1_FRAME0_FILTER_LEVEL_V_BIT: usize = 18 * 8 + 53;

/// Strongest AV1 deblock (`f(6)`). The probe rewrites both chroma levels to this.
const MAX_LOOP_FILTER_LEVEL: u8 = 63;

/// Driver reads chroma deblock levels.
///
/// Decode frame 0 twice — coded `[8, 12]` vs rewritten `[63, 63]` — and require
/// chroma to differ, luma identical. Identical chroma: audit the lifetime of every
/// block the submit points at (`pColorConfig` / Std sequence header) first. A
/// freed `pColorConfig` reads as `mono_chrome = 1`, and a monochrome frame skips
/// those levels (AV1 7.14).
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_frame0_probes_whether_the_driver_reads_the_chroma_deblocking_levels() {
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let aus = common::split_av1_aus(common::TEST_25FPS_AV1);
    assert_eq!(aus.len(), FRAME_COUNT);

    let mutated_au = av1_frame0_with_max_chroma_deblocking(aus[0]);
    let mut units: Vec<&[u8]> = aus.clone();
    units[0] = &mutated_au;

    let coded = av1_first_frame(&aus);
    let maxed = av1_first_frame(&units);

    let luma = (DISPLAY_AV1.0 * DISPLAY_AV1.1) as usize;
    eprintln!("  coded chroma levels [8, 12]  {}", sha256_hex(&coded));
    eprintln!("  chroma levels [63, 63]       {}", sha256_hex(&maxed));
    eprintln!("  libavcodec's frame 0         {}", sha256_hex(AV1_FRAME0));
    assert_eq!(
        coded[..luma],
        maxed[..luma],
        "the chroma deblocking levels must not move a luma sample — if they did, \
         the mutation desynchronised the frame header and the chroma comparison \
         below means nothing"
    );
    assert_ne!(
        coded[luma..],
        maxed[luma..],
        "the driver produced the SAME chroma from loop_filter_level[2..3] = [8, 12] \
         and from [63, 63]. This happened once before and the driver was INNOCENT: \
         a monochrome-looking sequence header makes it skip both levels, and ours \
         looked monochrome because its `pColorConfig` block had been freed and \
         reused before the decode op was recorded (see this test's docs). So audit \
         the LIFETIME of everything the submission points at — the Std sequence \
         header behind the parameters object first — before blaming the vendor"
    );
    eprintln!("the driver reads the chroma deblocking levels — the two decodes differ");
}

/// First AU with both chroma deblock levels rewritten to [`MAX_LOOP_FILTER_LEVEL`].
/// Offsets are syntax-derived; neighbours are fixed-width, so a miss probes the
/// wrong parameter without desynchronising. The CPU test checks the mutation.
fn av1_frame0_with_max_chroma_deblocking(au: &[u8]) -> Vec<u8> {
    let mut mutated = au.to_vec();
    for bit in [AV1_FRAME0_FILTER_LEVEL_U_BIT, AV1_FRAME0_FILTER_LEVEL_V_BIT] {
        set_bits(&mut mutated, bit, 6, MAX_LOOP_FILTER_LEVEL);
    }

    let before = av1_first_header(au);
    let after = av1_first_header(&mutated);
    assert_eq!(
        before.loop_filter_params.loop_filter_level,
        [1, 7, 8, 12],
        "the vendored vector's frame 0 codes these levels, and the whole probe is \
         built around the last two of them"
    );
    assert_eq!(
        after.loop_filter_params.loop_filter_level,
        [1, 7, MAX_LOOP_FILTER_LEVEL, MAX_LOOP_FILTER_LEVEL],
        "the rewrite must land on the two CHROMA levels and leave the luma pair \
         alone — a luma change would make the probe's control meaningless"
    );
    // A shifted rewrite would corrupt a neighbouring block before it showed in pixels.
    assert_eq!(
        after.cdef_params, before.cdef_params,
        "the CDEF block follows the loop filter block and is what a shifted rewrite \
         would corrupt first"
    );
    assert_eq!(after.quantization_params, before.quantization_params);
    assert_eq!(after.tile_info, before.tile_info);
    assert_eq!(
        after.loop_restoration_params,
        before.loop_restoration_params
    );
    assert_eq!(after.segmentation_params, before.segmentation_params);
    assert_eq!(
        (
            after.loop_filter_params.loop_filter_sharpness,
            after.loop_filter_params.loop_filter_ref_deltas,
            after.loop_filter_params.loop_filter_mode_deltas,
        ),
        (
            before.loop_filter_params.loop_filter_sharpness,
            before.loop_filter_params.loop_filter_ref_deltas,
            before.loop_filter_params.loop_filter_mode_deltas,
        ),
        "the rest of the loop filter block rides after the levels and must survive"
    );
    assert_ne!(mutated, au, "the rewrite must actually change bytes");
    mutated
}

#[test]
fn the_av1_chroma_deblocking_mutation_changes_only_those_two_levels() {
    let aus = common::split_av1_aus(common::TEST_25FPS_AV1);
    let mutated = av1_frame0_with_max_chroma_deblocking(aus[0]);
    // U ends mid-byte, so two or three bytes change; a whole-unit diff means
    // `set_bits` walked off its field.
    let changed = aus[0].iter().zip(&mutated).filter(|(a, b)| a != b).count();
    assert!(
        (1..=3).contains(&changed),
        "twelve bits spanning at most three bytes, and {changed} bytes changed"
    );
}

/// Overwrite the `bits`-wide big-endian bitfield at `bit`.
///
/// AV1 `f(n)` is MSB-first from the OBU payload: same width, same position, so
/// nothing after the field shifts.
fn set_bits(data: &mut [u8], bit: usize, bits: usize, value: u8) {
    for i in 0..bits {
        let at = bit + i;
        let mask = 1u8 << (7 - (at % 8));
        let set = (value >> (bits - 1 - i)) & 1 == 1;
        if set {
            data[at / 8] |= mask;
        } else {
            data[at / 8] &= !mask;
        }
    }
}

fn av1_first_header(au: &[u8]) -> pf_bitstream::av1::ParsedFrameHeader {
    let mut planner = pf_bitstream::av1::Av1Planner::new();
    let plans = planner.plan_au(au).expect("the unit plans");
    let plan = plans.first().expect("the unit carries a frame");
    (*plan.header).clone()
}

/// Decode only as far as the first delivered frame; tightly packed NV12. Owns its
/// device so a test may call it twice; GPU lock and readback hook are the caller's.
fn av1_first_frame(aus: &[&[u8]]) -> Vec<u8> {
    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let ours = {
        // SAFETY: `setup` outlives this block; created with the AV1 decode
        // extension + timeline/sync2.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(1, 8, false)
            .expect("the box must host AV1 Main 4:2:0 8-bit, no film grain");
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists.
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

        // First delivered frame only: the first unit is a shown key frame.
        let mut first: Option<Vec<u8>> = None;
        for (index, au) in aus.iter().enumerate() {
            let frame = decoder
                .decode(au)
                .unwrap_or_else(|e| panic!("AU {index}: decode failed: {e}"));
            if let Some(frame) = frame {
                assert_eq!(
                    decoder.wait_status(&frame),
                    DecodeStatus::Ok,
                    "frame 0: decode op not COMPLETE\n  state: {}",
                    decoder.debug_snapshot()
                );
                assert_eq!(frame.format, EXPECTED_FORMAT, "frame 0: pool format");
                // SAFETY: delivered and unreleased; pool has TRANSFER_SRC; serialized.
                first = Some(unsafe { readback.read_nv12(&frame) });
                decoder
                    .release_frame(&frame, true)
                    .expect("frame 0: release");
                // A unit may carry more than one frame; this leg stops at the first.
                // Spare frames go back with `false` — nothing signalled their timeline.
                while let Some(spare) = decoder.take_ready() {
                    decoder
                        .release_frame(&spare, false)
                        .expect("release an unread frame of the same temporal unit");
                }
                break;
            }
        }
        // SAFETY: every readback was fence-waited inside `read_nv12`.
        unsafe { readback.destroy() };
        first.expect("the vector's first temporal unit shows a frame")
    };

    // SAFETY: decoder and readback are gone.
    unsafe { setup.destroy() };
    ours
}

/// Per-plane stats of `ours` vs `want`, printed not asserted. A hash cannot say
/// which plane, shift vs value, or how big. See
/// [`av1_frame0_pixels_say_which_plane_and_how_badly`].
fn report_nv12_divergence(ours: &[u8], want: &[u8], display: (u32, u32)) {
    let (width, height) = (display.0 as usize, display.1 as usize);
    let luma = width * height;
    assert_eq!(ours.len(), want.len(), "both frames are the same layout");
    assert_eq!(ours.len(), luma * 3 / 2, "tightly packed NV12");

    eprintln!(
        "--- AV1 frame 0: {width}x{height} NV12, {} bytes ---",
        ours.len()
    );
    eprintln!("  ours   {}", sha256_hex(ours));
    eprintln!("  golden {}", sha256_hex(want));

    // A constant plane means nothing was decoded, not that it was decoded wrongly.
    let flat = |plane: &[u8]| plane.iter().all(|b| *b == plane[0]);
    if flat(&ours[..luma]) {
        eprintln!(
            "  ⚠ our LUMA is constant ({}) — nothing decoded here",
            ours[0]
        );
    }
    if flat(&ours[luma..]) {
        eprintln!(
            "  ⚠ our CHROMA is constant ({}) — nothing decoded here",
            ours[luma]
        );
    }

    for (name, ours, want) in [
        ("luma  ", &ours[..luma], &want[..luma]),
        ("chroma", &ours[luma..], &want[luma..]),
    ] {
        if ours == want {
            eprintln!("  {name}: IDENTICAL ({} bytes)", ours.len());
            continue;
        }
        let mut differing = 0usize;
        let mut max_delta = 0u32;
        let mut total_delta = 0u64;
        let mut buckets = [0usize; 7];
        let mut first: Vec<(usize, u8, u8)> = Vec::new();
        for (i, (a, b)) in ours.iter().zip(want.iter()).enumerate() {
            if a == b {
                continue;
            }
            let delta = u32::from(a.abs_diff(*b));
            differing += 1;
            max_delta = max_delta.max(delta);
            total_delta += u64::from(delta);
            let bucket = match delta {
                1 => 0,
                2 => 1,
                3..=4 => 2,
                5..=8 => 3,
                9..=16 => 4,
                17..=64 => 5,
                _ => 6,
            };
            buckets[bucket] += 1;
            if first.len() < 8 {
                first.push((i, *a, *b));
            }
        }
        let percent = 100.0 * differing as f64 / ours.len() as f64;
        eprintln!(
            "  {name}: {differing}/{} bytes differ ({percent:.2}%), max |delta| {max_delta}, \
             mean |delta| over the differing bytes {:.2}",
            ours.len(),
            total_delta as f64 / differing as f64
        );
        eprintln!(
            "    |delta| histogram  1:{} 2:{} 3-4:{} 5-8:{} 9-16:{} 17-64:{} 65+:{}",
            buckets[0], buckets[1], buckets[2], buckets[3], buckets[4], buckets[5], buckets[6]
        );
        // One row is `width` bytes in both planes (luma samples; chroma is
        // `width/2` samples × 2 bytes). Printed chroma coords are in chroma units.
        let positions: Vec<String> = first
            .iter()
            .map(|(i, a, b)| format!("(x{},y{}) {a}≠{b}", i % width, i / width))
            .collect();
        eprintln!("    first differing: {}", positions.join("  "));
    }

    // Displacement vs difference: a matching shift is a wrong crop origin or a
    // copy extent taken from the pool rather than the render region.
    if ours[..luma] != want[..luma] {
        let mut best: Option<(i32, i32, f64)> = None;
        for dy in -4i32..=4 {
            for dx in -8i32..=8 {
                if (dy, dx) == (0, 0) {
                    continue;
                }
                let (mut hit, mut seen) = (0usize, 0usize);
                for y in 8..height - 8 {
                    for x in 8..width - 8 {
                        let sy = (y as i32 + dy) as usize;
                        let sx = (x as i32 + dx) as usize;
                        seen += 1;
                        if ours[y * width + x] == want[sy * width + sx] {
                            hit += 1;
                        }
                    }
                }
                let score = hit as f64 / seen as f64;
                if best.is_none_or(|(_, _, b)| score > b) {
                    best = Some((dy, dx, score));
                }
            }
        }
        // Identity score for scale: a slightly wrong decode still matches in place,
        // so a shift only means something when it beats staying put.
        let (mut hit, mut seen) = (0usize, 0usize);
        for y in 8..height - 8 {
            for x in 8..width - 8 {
                seen += 1;
                if ours[y * width + x] == want[y * width + x] {
                    hit += 1;
                }
            }
        }
        let identity = hit as f64 / seen as f64;
        if let Some((dy, dx, score)) = best {
            eprintln!(
                "  luma shift probe: in place {:.3} · best shift dy{dy:+} dx{dx:+} {score:.3}{}",
                identity,
                if score > identity + 0.05 {
                    "  ⚠ A SHIFT FITS BETTER — this is readback geometry, not decode"
                } else {
                    "  (no shift fits better: the pixels are in the right place and \
                     carry the wrong values)"
                }
            );
        }
    }
}

// CPU coherence guards — not `#[ignore]`d. GPU legs run only on the fleet; these
// pin AU split, planner output count, vector shape, and golden set so a re-sync
// fails in CI instead of as a fleet frame-count mismatch. They also close the
// vacuous-pass (zero frames, or 250 copies of one digest).

#[test]
fn h265_goldens_and_au_split_agree_with_the_planner() {
    use pf_bitstream::h265::H265Planner;

    let goldens = golden_hashes(GOLDENS_H265);
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-h265.nv12.sha256");

    // `split_h265_aus` keys on HEVC's 2-byte NAL header; H.264's `+ 1` offset
    // would silently merge or split AUs.
    let aus = common::split_h265_aus(common::TEST_25FPS_H265);
    assert_eq!(
        aus.len(),
        FRAME_COUNT,
        "the vendored H.265 vector is {FRAME_COUNT} access units \
         (pf-bitstream's own planner test pins the same number)"
    );

    // Planner output count is the GPU delivery ceiling: one frame per `dpb.outputs`
    // id, plus the flush tail.
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
        // H.265 legs hard-code `probe_stream_support(1, 0)` and NV12. Fail here
        // on CPU rather than as a fleet probe of the wrong profile.
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
    // No CRA/BLA: `RaslSkipped` (`Ok(None)`) is unreachable, so the count cannot
    // be perturbed by it. A re-synced CRA opening fires here first.
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

/// Low-delay H.264 CPU guard. The stream must still reach the aliasing
/// precondition (a picture removed by the AU whose `dpb_refs` still names it) on
/// nearly every AU. Without that the GPU leg is a duplicate of the conformance one.
#[test]
fn the_low_delay_stream_agrees_with_its_goldens_and_still_exercises_the_aliasing_shape() {
    use pf_bitstream::h264::H264Planner;

    let goldens = golden_hashes(GOLDENS_LOWDELAY);
    assert_goldens_are_a_real_set(
        &goldens,
        LOWDELAY_FRAME_COUNT,
        "data/lowdelay-640x480.nv12.sha256",
    );

    let aus = common::split_h264_aus(LOWDELAY_H264);
    assert_eq!(aus.len(), LOWDELAY_FRAME_COUNT);

    let mut planner = H264Planner::new();
    let mut outputs = 0usize;
    let mut both = 0usize;
    let mut first_sps = None;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {index}: the low-delay stream must plan, got {e:?}"));
        outputs += plan.dpb.outputs.len();
        both += plan
            .dpb
            .removed
            .iter()
            .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
            .count();
        first_sps.get_or_insert((
            plan.sps.max_num_ref_frames,
            plan.picture.max_dpb_frames,
            plan.sps.vui_parameters.max_num_reorder_frames,
        ));
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {} hashes",
        goldens.len()
    );

    // SPS facts that make the shape reachable; a different encoder must not
    // quietly stop being low-delay.
    assert_eq!(
        first_sps,
        Some((3, 3, 0)),
        "max_num_ref_frames, DPB depth and max_num_reorder_frames — a DPB exactly as \
         deep as the reference count, with no reordering, is what puts the unmarking \
         and the eviction in one access unit"
    );
    assert_eq!(
        both, 117,
        "the stream must still remove pictures its own reference lists name — that is \
         the ONLY reason it is vendored, and without it the GPU leg is a duplicate of \
         the conformance one"
    );
}

/// HEVC low-delay CPU guard. Pinning `both == 0` alone is vacuous (a stream that
/// never removes, or that reorders, also reports zero). Three numbers instead:
///
/// - 115 AUs remove a picture — DPB pressure exists;
/// - 0 of them intersect `dpb_refs` — the exemption, measured;
/// - 115 would intersect a snapshot taken before `decode_rps`.
///
/// `pre_rps_marked(N)` is exact: between AU N-1's snapshot and AU N's `decode_rps`
/// only `finish_picture(N-1)` stores a short-term mark, so the set RPS sees is
/// `dpb_refs(N-1) ∪ {stored(N-1)}`.
#[test]
fn the_low_delay_h265_stream_agrees_with_its_goldens_and_keeps_the_exemption_falsifiable() {
    use pf_bitstream::h265::H265Planner;

    let goldens = golden_hashes(GOLDENS_LOWDELAY_H265);
    assert_goldens_are_a_real_set(
        &goldens,
        LOWDELAY_H265_FRAME_COUNT,
        "data/lowdelay-640x480-h265.nv12.sha256",
    );

    let aus = common::split_h265_aus(LOWDELAY_H265);
    assert_eq!(aus.len(), LOWDELAY_H265_FRAME_COUNT);

    let mut planner = H265Planner::new();
    let mut outputs = 0usize;
    let mut iraps = 0usize;
    let mut with_removals = 0usize;
    let mut both = 0usize;
    let mut would_alias = 0usize;
    let mut first_sps = None;
    // Marked DPB as AU N's `decode_rps` finds it: N-1 snapshot plus N-1 stored.
    let mut pre_rps_marked: Vec<u64> = Vec::new();
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the low-delay HEVC stream must plan, got {e:?}")
        });
        outputs += plan.dpb.outputs.len();
        iraps += usize::from(plan.picture.is_irap);
        if !plan.dpb.removed.is_empty() {
            with_removals += 1;
        }
        both += plan
            .dpb
            .removed
            .iter()
            .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
            .count();
        would_alias += plan
            .dpb
            .removed
            .iter()
            .filter(|id| pre_rps_marked.contains(id))
            .count();

        // Both HEVC legs hard-code `probe_stream_support(1, 0)` and NV12.
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8
            ),
            (1, 0),
            "AU {index}: the low-delay HEVC stream must stay Main 4:2:0 8-bit"
        );
        if index == 0 {
            assert!(plan.picture.is_idr, "the stream opens with an IDR");
            assert_eq!(
                (plan.picture.coded_width, plan.picture.coded_height),
                DISPLAY_LOWDELAY_H265,
                "the stream is 640x480"
            );
            assert_eq!(
                (
                    plan.picture.display_crop.x,
                    plan.picture.display_crop.y,
                    plan.picture.display_crop.width,
                    plan.picture.display_crop.height,
                ),
                (0, 0, DISPLAY_LOWDELAY_H265.0, DISPLAY_LOWDELAY_H265.1),
                "640 and 480 are both multiples of MinCbSizeY, so there is no \
                 conformance window and the coded size IS what the goldens hashed"
            );
        }
        first_sps.get_or_insert((
            plan.picture.max_dpb_frames,
            plan.sps.max_num_reorder_pics[usize::from(plan.sps.max_sub_layers_minus1)],
        ));

        pre_rps_marked = plan.dpb_refs.iter().map(|r| r.id).collect();
        if let Some(id) = plan.dpb.stored {
            assert!(
                plan.picture.is_reference,
                "AU {index}: every picture of this stream is a reference — a \
                 sub-layer non-reference picture would break the pre-RPS \
                 reconstruction below"
            );
            pre_rps_marked.push(id);
        }
    }
    outputs += planner.flush().outputs.len();

    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {} hashes",
        goldens.len()
    );
    assert_eq!(
        iraps, 1,
        "the stream holds exactly one IRAP (the opening IDR); a CRA/BLA would make \
         RASL skips reachable and the expected frame count needs rederiving"
    );
    assert_eq!(
        first_sps,
        Some((5, 0)),
        "DPB depth and sps_max_num_reorder_pics — a five-picture DPB against the four \
         pictures 8.3.2 keeps marked, with no reordering, is what puts an RPS drop and \
         the eviction it causes in one access unit"
    );

    assert_eq!(
        with_removals, 115,
        "the stream must still retire a picture on nearly every access unit; without \
         that the two numbers below are both trivially zero"
    );
    assert_eq!(
        both, 0,
        "{both} picture(s) are in an access unit's own marked DPB AND removed by it. \
         That is the H.264/AV1 aliasing precondition, and HEVC is supposed to be \
         structurally incapable of it — `H265Planner`'s snapshot has moved ahead of \
         `decode_rps`. Restore the ordering, or give the HEVC conversions the \
         `release_after_decode` deferral the other two carry; do NOT relax this number"
    );
    assert_eq!(
        would_alias, 115,
        "the fixture must stay CAPABLE of exposing the defect it is here to rule out. \
         A regenerated stream that reordered, or that carried a DPB deeper than its \
         reference count, would report 0 here — and the zero above would then prove \
         nothing at all, exactly as `test-25fps.h264` proved nothing for two milestones"
    );
}

/// AV1 low-delay CPU guard. The fixture exists for more than one tile. Tile shape
/// is asserted per frame. Frame accounting is pinned, not derived: 60/60/0/60
/// against the vendored vector's 250/274/24/250.
#[test]
fn the_low_delay_av1_stream_agrees_with_its_goldens_and_still_carries_two_tiles() {
    use pf_bitstream::av1::Av1Planner;

    let goldens = golden_hashes(GOLDENS_LOWDELAY_AV1);
    assert_goldens_are_a_real_set(
        &goldens,
        LOWDELAY_AV1_FRAME_COUNT,
        "data/lowdelay-3840x2160-av1.nv12.sha256",
    );

    let aus = common::split_av1_aus(LOWDELAY_AV1);
    assert_eq!(
        aus.len(),
        LOWDELAY_AV1_UNIT_COUNT,
        "the low-delay AV1 stream is {LOWDELAY_AV1_UNIT_COUNT} temporal units"
    );
    assert!(
        aus.iter().all(|au| !au.is_empty()),
        "no temporal unit is empty — an IVF reader returning empty packets would make \
         the parity leg decode nothing and blame the decoder"
    );

    let mut planner = Av1Planner::new();
    let mut outputs = 0usize;
    let mut coded_frames = 0usize;
    let mut multi_frame_units = 0usize;
    let mut hidden = 0usize;
    let mut show_existing = 0usize;
    let mut keys = 0usize;
    let mut with_removals = 0usize;
    let mut aliasing_shape = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plans = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("temporal unit {index}: the low-delay stream must plan, got {e:?}")
        });
        if plans.len() > 1 {
            multi_frame_units += 1;
        }
        for plan in &plans {
            coded_frames += 1;
            outputs += plan.dpb.outputs.len();
            keys += usize::from(plan.picture.is_key);
            hidden += usize::from(!plan.picture.show_frame);
            if plan.dpb.stored.is_none() {
                show_existing += 1;
            }
            assert!(
                plan.warnings.is_empty(),
                "temporal unit {index}: a clean stream plans without warnings, got {:?}",
                plan.warnings
            );

            // Two tile rows, one column, both in one Tile Group OBU — every frame.
            let tile = &plan.header.tile_info;
            assert_eq!(
                (tile.tile_cols, tile.tile_rows),
                (1, 2),
                "frame {coded_frames} (unit {index}): this fixture exists because our \
                 encoder emits TWO TILE ROWS at 4K. A single-tile stream here means it \
                 was regenerated at a lower resolution (1440p and below measured \
                 single-tile) or the encoder stopped splitting — either way the GPU leg \
                 below is now a duplicate of the vendored vector's and this fixture's \
                 260 KB buys nothing. Regenerate at 3840x2160; do NOT relax this"
            );
            assert_eq!(
                (
                    tile.width_in_sbs_minus_1[0],
                    tile.height_in_sbs_minus_1[0],
                    tile.height_in_sbs_minus_1[1],
                ),
                (59, 16, 16),
                "frame {coded_frames}: the per-tile superblock sizing the conversions \
                 copy into their tile arrays"
            );
            assert_eq!(
                plan.tiles.len(),
                1,
                "frame {coded_frames}: both tiles ride in ONE Tile Group OBU"
            );
            assert_eq!(
                (plan.tiles[0].tg_start, plan.tiles[0].tg_end),
                (0, 1),
                "frame {coded_frames}: the single tile group covers tiles 0..=1 — a \
                 range of 0..=0 is the truncation shape the host once shipped"
            );

            // Both AV1 legs hard-code `probe_stream_support(1, 8, false)` and NV12.
            // Grain is part of the Vulkan decode profile, not a per-frame toggle.
            assert_eq!(
                (
                    plan.picture.chroma_format_idc,
                    plan.picture.bit_depth,
                    plan.sequence.film_grain_params_present,
                ),
                (1, 8, false),
                "frame {coded_frames}: Main 4:2:0 8-bit, no film grain"
            );
            if coded_frames == 1 {
                assert!(plan.picture.is_key, "the stream opens on a key frame");
                assert_eq!(
                    (plan.picture.render_width, plan.picture.render_height),
                    DISPLAY_LOWDELAY_AV1,
                    "the render region the readback crops to and the goldens hash"
                );
                assert_eq!(
                    (plan.picture.upscaled_width, plan.picture.frame_height),
                    DISPLAY_LOWDELAY_AV1,
                    "no superres and no AV1 conformance-window equivalent — the coded \
                     picture IS the render region"
                );
            }

            if !plan.dpb.removed.is_empty() {
                with_removals += 1;
            }
            aliasing_shape += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
        }
    }

    // One shown frame per unit — the simple shape. The vendored vector is not;
    // deriving from either silently mis-counts the other.
    assert_eq!(
        (
            coded_frames,
            outputs,
            multi_frame_units,
            hidden,
            show_existing,
            keys
        ),
        (
            LOWDELAY_AV1_FRAME_COUNT,
            LOWDELAY_AV1_FRAME_COUNT,
            0,
            0,
            0,
            1
        ),
        "coded / displayed / multi-frame units / hidden / show_existing / key frames — \
         our host emits one shown frame per temporal unit and one key frame at the \
         head, against the vendored vector's 274 / 250 / 24 / 24 / 0 / 1"
    );
    assert_eq!(
        outputs,
        goldens.len(),
        "the planner outputs {outputs} pictures but the goldens carry {}",
        goldens.len()
    );

    // A regen must not drop below this aliasing coverage.
    assert_eq!(
        (with_removals, aliasing_shape),
        (55, 55),
        "55 of the 60 frames displace a reference they still name, which is the \
         precondition `release_after_decode` exists for"
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
        // Every other golden set is 8-bit. An 8-bit regen would turn this leg
        // into a second 8-bit run and still pass.
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

/// AV1 chain on CPU: goldens, IVF split, hard-coded shape, and that 250 goldens
/// is the display count of a 274-frame vector, re-derived from the planner.
///
/// Coded-frame goldens (274 hashes) look like dropped frames. A short IVF split
/// looks the same. Another bit depth / sampling / grain probes the wrong Vulkan
/// profile. If the 24 hidden frames disappear, the leg still passes while no
/// longer exercising multi-frame temporal units.
#[test]
fn av1_goldens_and_the_ivf_split_agree_with_the_planner() {
    use pf_bitstream::av1::Av1Planner;

    let goldens = golden_hashes(GOLDENS_AV1);
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-av1.nv12.sha256");

    // One IVF packet per temporal unit. No start codes; a truncated remux shortens
    // the split silently.
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

    // One delivered frame per `dpb.outputs` id; AV1's planner has no flush tail.
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
            // `show_existing_frame` decodes nothing and stores nothing.
            if plan.dpb.stored.is_none() {
                show_existing += 1;
            }
            // Both AV1 legs hard-code `probe_stream_support(1, 8, false)` and NV12.
            // Grain is part of the Vulkan decode profile, not a per-frame toggle.
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
                assert_eq!(
                    (plan.picture.render_width, plan.picture.render_height),
                    DISPLAY_AV1,
                    "the display (render) region the readback asserts against"
                );
                // Pool allocation. Equal to the render region: no AV1 window equivalent.
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

/// [`AV1_FRAME0`] pixels must hash to `GOLDENS_AV1`'s first line. A stale blob
/// after a golden regen names the wrong cause. Also pins layout: 320×240 packed
/// NV12 is 115200 bytes, luma first.
#[test]
fn the_av1_frame0_reference_is_the_first_golden() {
    let (width, height) = (DISPLAY_AV1.0 as usize, DISPLAY_AV1.1 as usize);
    assert_eq!(
        AV1_FRAME0.len(),
        width * height * 3 / 2,
        "data/test-25fps-av1.frame0.nv12 must be one tightly packed NV12 frame of \
         the vector's render region"
    );
    let goldens = golden_hashes(GOLDENS_AV1);
    assert_goldens_are_a_real_set(&goldens, FRAME_COUNT, "data/test-25fps-av1.nv12.sha256");
    assert_eq!(
        sha256_hex(AV1_FRAME0),
        goldens[0],
        "the vendored frame-0 pixels must hash to the AV1 golden set's FIRST entry — \
         if they no longer do, the blob is from a different decode than the goldens \
         and `av1_frame0_pixels_say_which_plane_and_how_badly` would attribute a \
         divergence to the wrong cause. Regenerate it alongside the goldens: decode \
         the vector with `-f rawvideo -pix_fmt nv12 -fps_mode passthrough` and take \
         the first 115200 bytes (the golden file's header carries the full command)"
    );
    // A repeated-byte frame would pass a length check and make per-plane stats vacuous.
    let luma = &AV1_FRAME0[..width * height];
    let chroma = &AV1_FRAME0[width * height..];
    assert!(
        luma.iter().any(|b| *b != luma[0]) && chroma.iter().any(|b| *b != chroma[0]),
        "both planes must carry real picture content"
    );
}

/// Annex-B start codes as `(total, three_byte)`. Emulation prevention means
/// `00 00 01` cannot occur inside a NAL; a hit not preceded by a zero is three-byte.
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

// Four-byte GPU legs assert the rewrite matches the original goldens — true if
// the rewrite returned its input or dropped NALs. These catch that in CI.

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

    // Same AUs, same planner verdict: framing changed, nothing the decoder acts on.
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
