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
//! # The three legs that decode OUR OWN streams
//!
//! [`LOWDELAY_H264`], [`LOWDELAY_H265`] and [`LOWDELAY_AV1`] are not conformance
//! vectors — they are `punktfunk-host spike` output, vendored because a conformance
//! vector proves conformance to itself and the encoder we ship behind is a different
//! stream. Each is here for its own reason:
//!
//! * **H.264** caught a defect the vector is structurally blind to — 117 of its 120
//!   access units named one surface as both the decode target and a reference.
//! * **H.265** is EXEMPT from that defect for a structural reason, and an exemption
//!   with no stream behind it is how the H.264 defect survived two milestones.
//! * **AV1** is neither: the vendored AV1 vector already aliases on 268 of its 274
//!   frames, so that class was covered. It is here because the vector is ONE TILE on
//!   every frame while our encoder splits 4K into two tile rows, so every tile array
//!   the conversions fill had only ever been exercised at index 0.
//!
//! All three are backed by a non-ignored CPU guard asserting the stream still has the
//! property it was vendored for. ⚠ And all three are FILES. A file fixture says
//! nothing about packetisation, reassembly or loss — which for AV1 is not a
//! hypothetical caveat but a recorded failure: this suite reported 250/250 throughout
//! the period the host was shipping only the first tile of every 4K frame.
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

/// The AV1 vector's FIRST frame, as libavcodec decodes it: the 320x240 render
/// region, tightly packed NV12, 115200 bytes — the same bytes
/// [`GOLDENS_AV1`]'s first line hashes.
///
/// Hashes tell you a frame is wrong; only pixels tell you HOW. This exists for
/// [`av1_frame0_pixels_say_which_plane_and_how_badly`], whose whole job is to turn
/// "FIRST DIVERGENT FRAME = 0" into a class: luma or chroma, a shift or a
/// difference, a filter's worth of error or a structural one. Frame 0 earns the
/// 113 KiB because it is intra-only — nothing upstream of it can be blamed — and
/// because on this rung it is where a divergence appears first.
///
/// It cannot drift from the golden set it was cut out of:
/// [`the_av1_frame0_reference_is_the_first_golden`] re-derives its SHA-256 and
/// compares, in ordinary CI, with no GPU.
const AV1_FRAME0: &[u8] = include_bytes!("data/test-25fps-av1.frame0.nv12");

/// The ten-bit vector and its goldens. No hardware leg in this file consumes them
/// yet — the D3D11VA rung is where the ten-bit parity leg currently runs — but the
/// files live here, beside the other goldens, so the guard that keeps them honest
/// belongs here too and runs on every platform rather than only on Windows.
const TEST_MAIN10_H265: &[u8] = include_bytes!("data/test-main10.h265");
const GOLDENS_MAIN10: &str = include_str!("data/test-main10.p010.sha256");

/// **Our own host's H.264**, and the only stream here that is not a conformance
/// vector — vendored 2026-08-07 because the conformance vector is BLIND to the one
/// defect this rung had.
///
/// `test-25fps.h264` reorders and carries a 7-frame DPB against 2 reference frames,
/// so a picture the sliding window unmarks is never evicted in the same access unit.
/// A punktfunk host emits low-delay IPPP with `max_num_reorder_frames = 0` and — this
/// is the part that matters — NVENC writes `max_num_ref_frames = 3` alongside
/// `max_dec_frame_buffering = 3`, a DPB exactly as deep as its reference count. 8.2.5's
/// window then unmarks the oldest reference in the very access unit whose C.4.5.3 bump
/// evicts it, and the conversion used to release that picture's slot before assigning
/// the setup one — so `pSetupReferenceSlot` and a reference named the same slot on
/// **117 of these 120 access units**.
///
/// The vector passed 250/250 throughout. This is the stream that could not.
const LOWDELAY_H264: &[u8] = include_bytes!("data/lowdelay-640x480.h264");
const GOLDENS_LOWDELAY: &str = include_str!("data/lowdelay-640x480.nv12.sha256");

/// The low-delay stream is 120 display frames at 640x480 (no conformance window —
/// both dimensions are macroblock-aligned, so the coded and display sizes agree).
const LOWDELAY_FRAME_COUNT: usize = 120;
const DISPLAY_LOWDELAY: (u32, u32) = (640, 480);

/// **Our own host's HEVC**, the twin of [`LOWDELAY_H264`] — and the one vendored to
/// keep an exemption honest rather than to catch a defect.
///
/// H.264 and AV1 both had to defer their slot releases past the decode op because
/// their planners snapshot the marked DPB BEFORE the marking that retires a picture.
/// `H265Planner` snapshots AFTER `decode_rps`, so an RPS-dropped picture is never in
/// the set `RefPicList`/`pReferenceSlots` is built from, and the HEVC conversions
/// still release inline. That argument is correct — and it was, until this stream,
/// backed by `test-25fps.h265` (which REORDERS, so it cannot reach the shape at all)
/// plus one throwaway measurement.
///
/// This stream reaches the shape. `sps_max_dec_pic_buffering_minus1 = 4` against four
/// pictures marked in steady state, `sps_max_num_reorder_pics = 0`: 115 of its 120
/// access units retire exactly one picture, and **all 115 of them would alias** if
/// the snapshot moved above `decode_rps`. Measured `removed ∩ dpb_refs` is 0 of 120,
/// so the exemption is a measurement on our own encoder's output rather than a
/// re-derivable argument.
///
/// ⚠ On THIS rung that counterfactual is about the planner, not about
/// [`pf_vkdecode::plan_to_vk_h265`]: Vulkan's `pReferenceSlots` is spec-defined as the
/// slots the decode operation uses, so the conversion binds `plan.rps` — the three
/// current sets, which `decode_rps` itself derives — and never reads `dpb_refs` at
/// all. The DXVA rung is the one that binds the whole marked DPB (`RefPicList` is
/// spec-defined that way, and an RFI long-term anchor must survive in it), so it is
/// the rung a moved snapshot would actually alias on;
/// `pf_dxvadec::pic_h265`'s tests drive that counterfactual through the conversion.
/// What the leg below adds on this rung is the thing no HEVC leg here had: PIXELS
/// from our own encoder, under a DPB that evicts and reuses a slot on 115 of 120
/// access units instead of a vector whose reordering keeps eviction slack.
///
/// Provenance, the `punktfunk-host spike` command and the two-build ffmpeg
/// cross-check are in the golden file's header, as for the H.264 sibling.
const LOWDELAY_H265: &[u8] = include_bytes!("data/lowdelay-640x480.h265");
const GOLDENS_LOWDELAY_H265: &str = include_str!("data/lowdelay-640x480-h265.nv12.sha256");

/// **Our own host's AV1**, and the only stream here with more than ONE TILE.
///
/// The vendored AV1 vector already exercises the reference-slot aliasing shape (268 of
/// its 274 frames), so unlike the H.264 and H.265 siblings this is not vendored to
/// close that. It closes a different gap: no host-generated AV1 stream was tested at
/// pixel level anywhere, and our encoder's AV1 is structurally unlike the vector —
/// `RFI_DPB = 5` references, reference-frame invalidation, and at 4K a split encode
/// that puts **two tile rows in one frame**.
///
/// 4K is not a size choice, it is the only shape that has the property. Measured on
/// .21, same command at four resolutions: 1280x720, 1920x1080 and 2560x1440 all give
/// `tile_cols = tile_rows = 1`; 3840x2160 gives `tile_cols = 1, tile_rows = 2` with
/// both tiles in ONE Tile Group OBU. It is paid for with 60 frames instead of 120,
/// which lands at 261 KB — under both other low-delay fixtures.
///
/// ⚠ It is a FILE, and a file is not the wire path. "250/250 bit-identical to
/// libavcodec" was true for AV1 throughout the period the host was shipping only the
/// first tile of every 4K frame: that number came from a vendored file while the
/// truncation lived in packetisation. This fixture gives the multi-tile shape pixel
/// coverage on the DECODE rungs and says nothing whatever about fragmentation,
/// reassembly or loss. The golden file's header says the same, at length.
const LOWDELAY_AV1: &[u8] = include_bytes!("data/lowdelay-3840x2160.ivf.av1");
const GOLDENS_LOWDELAY_AV1: &str = include_str!("data/lowdelay-3840x2160-av1.nv12.sha256");

/// The low-delay AV1 stream's temporal units, DISPLAYED frames and render region.
///
/// Units and frames are two constants holding 60 rather than one, and that is
/// deliberate: for the vendored vector they are 250 and 250 while the CODED count is
/// 274, and a leg that derived one from the other would be asserting AV1's frame
/// accounting instead of measuring it.
const LOWDELAY_AV1_UNIT_COUNT: usize = 60;
const LOWDELAY_AV1_FRAME_COUNT: usize = 60;
const DISPLAY_LOWDELAY_AV1: (u32, u32) = (3840, 2160);

/// The HEVC low-delay stream's own frame count and display region.
///
/// Deliberately NOT shared with [`LOWDELAY_FRAME_COUNT`]/[`DISPLAY_LOWDELAY`] even
/// though the two fixtures agree today: they are separate files from separate
/// encoder configurations, and one regenerated at another size must fail on its own
/// leg rather than silently redefine the other's geometry. Same reason
/// [`DISPLAY_H264`] and [`DISPLAY_H265`] are two constants holding 320x240.
const LOWDELAY_H265_FRAME_COUNT: usize = 120;
const DISPLAY_LOWDELAY_H265: (u32, u32) = (640, 480);

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

/// Arm the decoders' readback hook: they read `PF_VKD_TEST_READBACK` at session creation, and it
/// is what makes their pool images grow TRANSFER_SRC so `vkCmdCopyImageToBuffer` is legal.
///
/// The GPU lock guard is taken BY REFERENCE rather than the precondition being restated in prose
/// at every leg. `env::set_var` is safe to call and unsound from a live multithreaded process, so
/// "the caller holds the lock" IS the whole safety argument — and a borrow states it in the type
/// system, where it cannot drift out of date. It also keeps this the file's ONLY `set_var`:
/// `scripts/ci/check-unsafe-hygiene.sh` gate C counts them per file, and six near-identical
/// copies of one argument is the shape that ratchet exists to discourage.
fn arm_test_readback(_gpu: &std::sync::MutexGuard<'static, ()>) {
    // SAFETY: the caller holds the binary-wide GPU lock (`common::gpu_lock`), proved by `_gpu`;
    // the parity legs are this variable's only writers and readers, and they run one at a time
    // under that lock.
    unsafe { std::env::set_var("PF_VKD_TEST_READBACK", "1") };
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
) -> Vec<u8> {
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
    nv12
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
            let planes = consume_frame(decoder, readback, &frame, hashes.len());
            hashes.push(sha256_hex(&planes));
            next = decoder.take_ready();
        }
    }
    // The decoders emit in bumping (display) order and a stream may hold frames —
    // the flush tail belongs in the comparison too.
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
    h264_parity_run_against(aus, label, GOLDENS_H264, FRAME_COUNT, DISPLAY_H264);
}

/// [`h264_parity_run`] with its stream's own goldens and geometry, for the legs that
/// do not decode the vendored vector.
fn h264_parity_run_against(
    aus: &[&[u8]],
    label: &str,
    goldens: &'static str,
    frame_count: usize,
    display: (u32, u32),
) {
    // One codec at a time on the device, and the `set_var` below happens only
    // under this lock (see `common::gpu_lock`).
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

/// The leg the vendored vector cannot be: **our own host's low-delay H.264**.
///
/// The vector above passed 250/250 on every driver in the fleet while this rung
/// named one DPB slot as both `pSetupReferenceSlot` and a reference on 117 of the
/// 120 access units below — the shape it simply never produces (see
/// [`LOWDELAY_H264`]). Both of this rung's DPB modes take it badly and neither
/// loudly: DISTINCT hands the aliased reference the same array layer the setup
/// writes; COINCIDE finds no bound image for it, drops it from `pReferenceSlots` and
/// `trace!`s. So a leg that decodes what we actually ship is not redundant with the
/// conformance leg, it is the only one that can see this class at all.
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

/// AU boundaries of a `PUNKTFUNK_DUMP_VIDEO` capture: the `.idx` sidecar when it
/// exists (`offset len flags complete` per line, `au_dump.rs`), the H.265 AU
/// splitter otherwise. `complete == 0` lines are skipped — the client's native
/// lanes are only ever fed complete AUs, so replaying a partial would test a path
/// the field never took.
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

/// Field-stream triage leg: decode a `PUNKTFUNK_DUMP_VIDEO` H.265 capture on the
/// real GPU and write one SHA-256 per delivered frame (display order) to
/// `<stream>.pfhash` — `scripts/vkdecode-field-parity.sh` diffs that against
/// ffmpeg's software decode of the same bytes and names the FIRST divergent
/// frame, which localises a silent-corruption defect the way the golden legs
/// cannot (they only ever decode streams our hosts do not emit).
///
/// Environment:
/// - `PF_VKD_FIELD_STREAM=/path/au-*.h265` — the capture (required).
/// - `PF_VKD_FIELD_YUV=12,13` — display indices whose raw planes are written to
///   `<stream>.frame<N>.yuv` for visual inspection (optional; second run,
///   once the script has named the divergence).
///
/// Unlike the golden legs this one TOLERATES per-AU errors (a field capture can
/// hold a truncated tail or a renegotiation) — errors are printed and counted,
/// the recovery latch resumes at the next IRAP, exactly the client's behaviour.
/// A mid-capture resolution change ends the comparable stretch: frames whose
/// crop differs from the first planned picture are released unshown and counted.
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

    // The join point + stream facts, exactly as the client found them: plan AUs
    // until one succeeds (pre-IRAP AUs of a mid-session capture fail with
    // AwaitingIdr-shaped errors, as they did in the field).
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

    // As the golden legs: one codec at a time, `set_var` under the lock.
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
        // SAFETY: as the golden legs — `setup` outlives this block and was created
        // with the H.265 decode extensions + timeline/sync2 features.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(picture.chroma_format_idc, picture.bit_depth_luma_minus8)
            .unwrap_or_else(|e| panic!("{label}: the box must host this shape — {e:?}"));
        // SAFETY: as the golden legs — live instance/device, queue 0 of
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

        let mut hashes: Vec<String> = Vec::new();
        let sink = |decoder: &mut VkH265Decoder,
                    frame: DecodedVkFrame,
                    hashes: &mut Vec<String>,
                    off_size: &mut usize| {
            if (frame.crop.width, frame.crop.height) != display {
                // A renegotiated stretch — not comparable against this readback.
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
                    // A capture taken over a lossy link holds AUs the planner had to
                    // conceal, and ffmpeg conceals them DIFFERENTLY — so a divergence
                    // at such a frame says nothing about this decoder. Recording the
                    // AUs is what separates "our bug" from "the field lost packets".
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
                // Field behaviour: print, count, continue — the recovery latch
                // resumes at the next IRAP exactly as the client did.
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

    // SAFETY: the decoder is gone (its Drop drained the queue and destroyed its
    // session/pools), the readback's handles are destroyed, and nothing else
    // references the setup's handles.
    unsafe { setup.destroy() };

    let out = format!("{stream_path}.pfhash");
    std::fs::write(&out, hashes.join("\n") + "\n").expect("write the hash file");
    eprintln!(
        "{label}: {} frames hashed → {out} (skipped {start_index} pre-join AUs, \
         {au_errors} AU errors, {off_size} off-size frames); diff with \
         scripts/vkdecode-field-parity.sh",
        hashes.len()
    );
    // The verdict's precondition, printed last so it is the thing a reader keeps:
    // on a capture with NO concealed AU, every divergence the diff finds is this
    // decoder's own — there is no second explanation left.
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

/// The HEVC twin of [`low_delay_host_h264_every_frame_hashes_bit_identical_to_libavcodec`]:
/// **our own host's HEVC**, in the shape the vendored vector cannot produce.
///
/// Its job is the opposite of the H.264 leg's. That one exists because the rung was
/// broken and only this stream shape could show it. This one exists because until now
/// no HEVC leg anywhere had decoded a single frame our own encoder produced: both
/// existing legs run vendored vectors, and the H.264 sibling is the standing proof
/// that a vector's silence about a stream shape is not evidence.
///
/// What it exercises that `h265_every_frame_hashes_bit_identical_to_libavcodec` does
/// not: a five-picture DPB with four references marked and no reordering, so the
/// `SlotMap` retires and reissues a slot on 115 of the 120 access units, back to back,
/// with the decode target taking the slot freed in the same access unit. The vector
/// reorders, which keeps that eviction slack and never puts the two together.
///
/// It is NOT the leg that would catch a moved `dpb_snapshot()` — see [`LOWDELAY_H265`]
/// for why that lands on the DXVA rung instead, and
/// [`the_low_delay_h265_stream_agrees_with_its_goldens_and_keeps_the_exemption_falsifiable`]
/// for the guard that keeps the planner property itself pinned, on CPU, in ordinary CI.
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

/// [`av1_parity_run`] with its stream's own goldens and geometry, for the leg that
/// does not decode the vendored vector.
///
/// `units` and `frames` are SEPARATE parameters and must stay so. They are equal for
/// the low-delay host stream (one shown frame per temporal unit) and unequal for the
/// vendored vector only in the sense that its 250 units carry 274 coded frames of
/// which 250 are shown — deriving either from the other is exactly the assumption
/// AV1 punishes.
fn av1_parity_run_against(
    aus: &[&[u8]],
    label: &str,
    goldens_file: &'static str,
    goldens_path: &str,
    units: usize,
    frames: usize,
    display: (u32, u32),
) {
    // As the other legs: one codec at a time on the device, and the `set_var` below
    // happens only under this lock (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    arm_test_readback(&_gpu);

    let goldens = golden_hashes(goldens_file);
    // Non-vacuity, before any hardware is touched: the right number of entries, all
    // real digests, all distinct (see the helper's docs — a frozen-frame decoder
    // must not be able to pass this leg).
    assert_goldens_are_a_real_set(&goldens, frames, goldens_path);
    // …and the leg must actually be fed something. An IVF whose packets failed to
    // parse would hand `collect_hashes` an empty AU list, which delivers no frames
    // and would then fail as a frame-count mismatch that reads like a decoder defect.
    assert_eq!(
        aus.len(),
        units,
        "{label}: the stream must split into {units} temporal units"
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

/// **Our own host's AV1, at the only resolution where it emits more than one tile.**
///
/// The leg above proves the conversion against a vector with `tile_cols = tile_rows
/// = 1` on every one of its 274 frames, so every tile-info field it exercises is the
/// degenerate case: one `width_in_sbs_minus_1`, one `height_in_sbs_minus_1`, one
/// `context_update_tile_id`, `TileCols = TileRows = 1`. This stream carries
/// `tile_rows = 2` with `height_in_sbs_minus_1 = [16, 16]` on all 60 frames, and both
/// tiles arrive in a single Tile Group OBU — so a conversion that got the tile arrays,
/// the per-tile sizing or the tile-group range wrong would decode the vector perfectly
/// and this stream visibly (see [`LOWDELAY_AV1`]).
///
/// It is also 4K, which no other parity leg in this program is: the readback moves
/// 12,441,600 bytes per frame instead of 115,200.
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

/// Frame 0's pixels against libavcodec's, byte for byte — the diagnostic leg.
///
/// [`av1_every_frame_hashes_bit_identical_to_libavcodec`] is the verdict; this is
/// the microscope, and it decodes only as far as the first delivered frame. A hash
/// mismatch names no cause, and the four causes the parity leg's own message ranks
/// for a frame-0 divergence produce *completely different* pixel signatures:
///
/// | printed here | what it means |
/// |---|---|
/// | `luma IDENTICAL`, chroma differs | the chroma plane's layout — `PLANE_1`'s copy region, or a pool whose chroma plane starts somewhere other than where the readback reads it. NOT a decode problem |
/// | both differ, and a **shift** matches | readback geometry: the crop origin, or a copy extent taken from the pool rather than the render region. The printed `dy`/`dx` IS the error |
/// | both differ, deltas ≤ ~8 over most of the plane | an in-loop filter parameter — CDEF, loop restoration, the deblocking levels. Small and everywhere is what a filter does, and it is why the whole 250 frames go with it: CDEF runs before the frame is stored as a reference |
/// | both differ, deltas large and structured | quantisation, tile geometry, or the tile payloads themselves — the frame was reconstructed from the wrong data rather than filtered wrongly |
/// | ours is CONSTANT | nothing was decoded into the image the readback read |
///
/// It asserts equality last, so a failure prints the whole report above the panic.
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

/// `loop_filter_level[2]` and `[3]` — the U and V deblocking levels — as BIT
/// offsets from the start of the vector's FIRST access unit.
///
/// Derived rather than found: the IVF packet holds a temporal-delimiter OBU (2
/// bytes), a sequence-header OBU (2 + 11) and an `OBU_FRAME` header (1 + a 2-byte
/// leb128 size), so the uncompressed frame header starts at byte 18. Inside it
/// `loop_filter_level[0]` begins at bit 35 and the four levels are `f(6)` back to
/// back (5.9.11), which puts U at bit 47 and V at bit 53.
///
/// [`av1_frame0_probes_whether_the_driver_reads_the_chroma_deblocking_levels`]
/// re-parses the mutated unit before it decodes anything, so a re-synced vector
/// makes this fail loudly instead of poking an unrelated field.
const AV1_FRAME0_FILTER_LEVEL_U_BIT: usize = 18 * 8 + 47;
const AV1_FRAME0_FILTER_LEVEL_V_BIT: usize = 18 * 8 + 53;

/// The strongest deblocking level AV1 can code (`f(6)`), and the value the probe
/// rewrites both chroma levels to.
const MAX_LOOP_FILTER_LEVEL: u8 = 63;

/// The driver DOES read the chroma deblocking levels — and this is the test that
/// says so, after a pass of this program's history said the opposite.
///
/// ⚠⚠ **The claim "NVIDIA ignores `StdVideoAV1LoopFilter::loop_filter_level[2..3]`"
/// is refuted. Do not reintroduce it.** It was an honest reading of a real
/// measurement: the AV1 frame-0 parity leg came back `luma IDENTICAL, chroma
/// 319/38400 bytes differ, max |delta| 4`, software re-decode with both chroma
/// levels forced to zero reproduced that signature byte for byte, and this very
/// probe then came back IDENTICAL for `[8, 12]` and `[63, 63]`. Every step was
/// sound; the inference was not. The levels were reaching the driver intact — what
/// was NOT intact was the sequence header, whose `pColorConfig` block this crate
/// freed the instant `vkCreateVideoSessionParametersKHR` returned while the driver
/// went on dereferencing it at every decode. The recycled bytes read as
/// `mono_chrome = 1`, and a monochrome frame skips exactly `loop_filter_level[2..3]`
/// (AV1 7.14) — which is why rewriting them changed nothing, and why the
/// fingerprint was a perfect match for levels that were never applied.
/// `pf-vkdecode`'s `session_av1` module docs carry the capture and the fix.
///
/// So the probe survives its own refutation, with its verdict inverted: it now
/// PASSES, and it is the cheapest guard there is against that whole class coming
/// back. It decodes frame 0 twice — once from the vector as it sits, once from the
/// same unit with both chroma levels rewritten to the strongest AV1 can code — and
/// requires the pixels to differ. In software that rewrite moves 793 chroma bytes
/// with `max |delta| 29`, which no readback or crop error could hide, and it leaves
/// luma bit-identical, which is the control: a mutation that changed luma would
/// have desynchronised the header rather than changed the field.
///
/// If the two decodes are ever IDENTICAL again, the message below is the one to
/// act on — and the FIRST thing to check is not the driver but whether some block
/// the decode op points at is being freed before the op is recorded. That is what
/// it was last time, on a bug this test could not see.
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

/// The vector's first access unit with both CHROMA deblocking levels rewritten to
/// [`MAX_LOOP_FILTER_LEVEL`] — and the proof, through the real parser, that this is
/// the only thing it changed.
///
/// The proof is not decoration. The offsets are derived from the spec's syntax
/// order rather than searched for, and a rewrite landing one field over would
/// desynchronise nothing (both neighbours are fixed-width) while silently probing
/// the wrong parameter. So every block the conversion reads is compared before and
/// after, and [`the_av1_chroma_deblocking_mutation_changes_only_those_two_levels`]
/// runs this on CPU in ordinary CI — the hardware run cannot be spent discovering
/// that the mutation was wrong.
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
    // Everything else the conversion reads, unchanged: a rewrite that shifted the
    // header would show up in one of these long before it showed up in pixels.
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

/// [`av1_frame0_with_max_chroma_deblocking`] on CPU, so the GPU probe's mutation is
/// known-good before any device time is spent on it.
#[test]
fn the_av1_chroma_deblocking_mutation_changes_only_those_two_levels() {
    let aus = common::split_av1_aus(common::TEST_25FPS_AV1);
    let mutated = av1_frame0_with_max_chroma_deblocking(aus[0]);
    // One byte may carry bits of both fields (U ends mid-byte), so the rewrite
    // touches two or three bytes and no more — a whole-unit difference would mean
    // `set_bits` walked off its field.
    let changed = aus[0].iter().zip(&mutated).filter(|(a, b)| a != b).count();
    assert!(
        (1..=3).contains(&changed),
        "twelve bits spanning at most three bytes, and {changed} bytes changed"
    );
}

/// Overwrite the `bits`-wide big-endian bitfield at `bit` in `data`.
///
/// AV1's `f(n)` is MSB-first from the start of the OBU payload, which is what the
/// probe above needs to rewrite a syntax element in place: same width, same
/// position, so nothing after it shifts.
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

/// The parsed frame header of the FIRST frame in one access unit.
fn av1_first_header(au: &[u8]) -> pf_bitstream::av1::ParsedFrameHeader {
    let mut planner = pf_bitstream::av1::Av1Planner::new();
    let plans = planner.plan_au(au).expect("the unit plans");
    let plan = plans.first().expect("the unit carries a frame");
    (*plan.header).clone()
}

/// Decode `aus` only as far as the FIRST delivered frame, and read it back as
/// tightly packed NV12 — the device half of both frame-0 legs.
///
/// Brings its own device up and tears it down, so one test may call it more than
/// once; the GPU lock and the readback hook are the caller's.
fn av1_first_frame(aus: &[&[u8]]) -> Vec<u8> {
    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let ours = {
        // SAFETY: as the parity legs — `setup` outlives this block and was created
        // with the AV1 decode extension + timeline/sync2 features.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(1, 8, false)
            .expect("the box must host AV1 Main 4:2:0 8-bit, no film grain");
        // SAFETY: as the parity legs — live instance/device, queue 0 of `graphics_qf`.
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

        // The FIRST delivered frame and no further: the first temporal unit is a
        // key frame that shows, so this is one decode.
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
                // SAFETY: the frame is delivered and unreleased on the readback's
                // device, the pool carries TRANSFER_SRC, and the test is serialized.
                first = Some(unsafe { readback.read_nv12(&frame) });
                decoder
                    .release_frame(&frame, true)
                    .expect("frame 0: release");
                // A temporal unit may carry more than one frame, and this leg stops
                // at the first. Anything else the unit made ready is handed straight
                // back — with `false`, because no presenter signalled its timeline
                // (nothing read it) — rather than left held while the decoder drops.
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

    // SAFETY: as the parity legs — the decoder and readback are gone.
    unsafe { setup.destroy() };
    ours
}

/// Per-plane statistics of `ours` against `want`, printed rather than asserted.
///
/// Everything here answers a question a hash cannot: WHICH plane, whether the
/// difference is a displacement or a value error, and how big. See
/// [`av1_frame0_pixels_say_which_plane_and_how_badly`] for how to read it.
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

    // A plane that never varies means nothing was decoded into the image at all,
    // which is a different failure from decoding it wrongly.
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
        // |delta| buckets: 1, 2, 3-4, 5-8, 9-16, 17-64, 65+.
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
        // One ROW is `width` bytes in both planes — luma because it is `width`
        // samples wide, interleaved chroma because it is `width / 2` samples wide
        // and two bytes per sample. So one formula serves both, and the chroma
        // coordinates it prints are in chroma units.
        let positions: Vec<String> = first
            .iter()
            .map(|(i, a, b)| format!("(x{},y{}) {a}≠{b}", i % width, i / width))
            .collect();
        eprintln!("    first differing: {}", positions.join("  "));
    }

    // A displacement, not a difference: does our luma equal the reference read a
    // few rows or columns over? That is what a wrong crop origin or a copy extent
    // taken from the pool rather than the render region looks like, and the shift
    // that matches IS the error.
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
        // The identity's own score, for scale: a decode that is merely slightly
        // wrong still matches most bytes in place, so a shift only means something
        // when it beats staying put.
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

/// The low-delay stream's own CPU guard, plus the property that makes it worth
/// vendoring at all.
///
/// The goldens/AU/output agreement is the same three-way check
/// [`h264_goldens_and_au_split_agree_with_the_planner`] does. What is extra here is
/// the last assertion: this stream must actually REACH the aliasing precondition —
/// a picture removed by the same access unit whose `dpb_refs` still names it — on
/// nearly every access unit. If a re-generation ever produced a stream that did not,
/// the GPU leg above would still pass 120/120 while proving nothing the vendored
/// vector does not already prove, and nothing else would say so.
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

    // The three SPS facts that make the shape reachable, pinned so a regenerated
    // stream from a different encoder cannot quietly stop being low-delay.
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

/// The HEVC low-delay stream's CPU guard — the twin of the H.264 one above, with the
/// extra assertion HEVC needs and H.264 does not.
///
/// H.264's guard pins that the stream still ALIASES (117 of 120), because its GPU leg
/// exists to catch a defect. HEVC's leg exists to keep an exemption from rotting, so
/// pinning `both == 0` alone would be exactly the vacuous check `fd6241a2` called out:
/// zero is also what a stream that never removes anything reports, and what a stream
/// that reorders reports. So this pins three numbers instead:
///
/// - **115 access units remove a picture** — the stream reaches the DPB pressure at all;
/// - **0 of them intersect `dpb_refs`** — the exemption, measured;
/// - **115 of them WOULD intersect** a snapshot taken before `decode_rps`.
///
/// The third is what makes the second worth having. `pre_rps_marked(N)` is exact rather
/// than approximate: `begin_picture` runs `decode_rps` → `update_dpb_before_decoding` →
/// `dpb_snapshot`, and the only thing that happens between AU N-1's snapshot and AU N's
/// `decode_rps` is `finish_picture(N-1)` storing its picture marked short-term. So the
/// marked set AU N's RPS sees is exactly `dpb_refs(N-1) ∪ {stored(N-1)}`, which is what
/// `dpb_snapshot()` would have returned from the other side of that call.
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
    // The marked DPB as AU N's `decode_rps` finds it: AU N-1's snapshot plus the
    // picture AU N-1 stored. See the doc comment for why this is exact.
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

        // The picture shape both HEVC legs hard-code: `probe_stream_support(1, 0)`
        // and an NV12 pool. Fail here, on CPU, rather than as a confusing
        // hardware-only refusal.
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

/// The AV1 low-delay stream's CPU guard, and the property it was vendored for: **more
/// than one tile**.
///
/// A regenerated fixture could lose that in two silent ways — a re-run at a lower
/// resolution (1440p and below are single-tile on this encoder) or a driver/encoder
/// change that stopped splitting — and in both cases the GPU leg would go on passing
/// 60/60 while duplicating what the vendored vector already covers. So the tile shape
/// is asserted per frame, not sampled.
///
/// It also pins AV1's frame accounting explicitly rather than by derivation. The
/// vendored vector is 250 units / 274 coded / 24 hidden / 250 shown; this stream is
/// 60 / 60 / 0 / 60. Neither is the general case, and a leg that assumed either would
/// break on the other for reasons that look like a decoder defect.
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

            // THE PROPERTY. Two tile ROWS, one tile COLUMN, both tiles in a single
            // Tile Group OBU — the 4K split-encode shape, on every frame including
            // the key frame.
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

            // The picture shape both AV1 legs hard-code (`probe_stream_support(1, 8,
            // false)` plus an NV12 pool). Film grain especially: it is part of the
            // Vulkan decode PROFILE, so a grain-bearing stream is a different device
            // requirement, not merely different pixels.
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

    // AV1's frame accounting, pinned rather than derived. This stream is the SIMPLE
    // shape — one shown frame per temporal unit — which is exactly why it must be
    // stated: the vendored vector is not, and a leg that learned its habits from one
    // of them silently mis-counts the other.
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

    // Not the reason this fixture exists — the vendored vector already aliases on 268
    // of its 274 frames — but recorded so a regeneration cannot quietly drop below the
    // vector's coverage while claiming to be the host-shaped stream.
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

/// The vendored frame-0 pixels ARE the first golden — not a second opinion about it.
///
/// [`AV1_FRAME0`] is the one place in this file where reference PIXELS live rather
/// than hashes, and pixels are exactly the kind of file that rots: regenerate the
/// goldens from a re-synced vector and this blob keeps describing the old one, while
/// the diagnostic leg that reads it goes on confidently naming the wrong cause. So
/// its digest is re-derived here and compared against `GOLDENS_AV1`'s first line —
/// the trusted, three-way cross-checked set — on every platform, with no GPU.
///
/// It also pins the layout the diagnostic's arithmetic assumes: 320x240 tightly
/// packed NV12 is 115200 bytes, luma first.
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
    // Not a flat blob: a frame of one repeated byte would satisfy a length check and
    // make every per-plane statistic in the diagnostic meaningless.
    let luma = &AV1_FRAME0[..width * height];
    let chroma = &AV1_FRAME0[width * height..];
    assert!(
        luma.iter().any(|b| *b != luma[0]) && chroma.iter().any(|b| *b != chroma[0]),
        "both planes must carry real picture content"
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
