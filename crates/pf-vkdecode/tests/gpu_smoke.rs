//! GPU smoke tests — `#[ignore]`d because they need real Vulkan Video hardware.
//!
//! Run on a Vulkan-Video box with:
//!
//! ```text
//! cargo test -p pf-vkdecode -- --ignored
//! ```
//!
//! Environment expectations (the fleet's RADV boxes .21/.25, the NVIDIA .173, or
//! any machine like them):
//! - a Vulkan 1.3 loader on the library path (`libvulkan.so.1` / `vulkan-1.dll`);
//! - a physical device advertising `VK_KHR_video_queue`,
//!   `VK_KHR_video_decode_queue` and the leg's codec extension
//!   (`VK_KHR_video_decode_h264` / `VK_KHR_video_decode_h265` /
//!   `VK_KHR_video_decode_av1`), with a queue
//!   family carrying `VIDEO_DECODE_KHR` ops for that codec;
//! - `timelineSemaphore` + `synchronization2` feature support (Vulkan 1.3 core);
//! - on RADV, `RADV_PERFTEST=video_decode` in the environment — for AV1 exactly as
//!   for the other two, and without it the AV1 leg reports missing silicon it has.
//!
//! One leg per codec, running the SAME body ([`smoke`]) over the vendored 25fps
//! vector of that codec — a box that decodes only some of the three runs those legs
//! and reports the rest as "no physical device with VK_KHR_video_decode_…", which is
//! a fact about the box rather than a failure (AV1 is the one most likely to say so
//! on today's fleet). Device bring-up lives in `tests/common/mod.rs`.
//!
//! What they prove: device wrap → caps query/derivation on REAL caps → session +
//! parameters creation → the decoupled picture pool → 48 AUs of the vendored
//! 25fps vector decoded through `vkCmdDecodeVideoKHR` — well past DPB-full, so
//! slot re-activation binds fresh pool images repeatedly — while the consumer
//! HOLDS FOUR delivered frames unreleased at steady state (the real client's
//! pipeline shape: bounded channels, FrameStore preroll, in-flight present).
//! Every frame's RESULT_STATUS_ONLY query must read COMPLETE before its
//! release. This is the regression test for the .25 field failure class: any
//! pool sizing that ignores the stream's DPB depth or the client's hold depth
//! starves exactly here. What they deliberately do NOT prove (that is
//! `gpu_parity`'s and WP-D on-glass's ground): pixel correctness vs the ffmpeg
//! rung, presenter interop (the `value + 1` signal-back — no presenter runs here,
//! so releases pass `false`), soak, and both vendors' DPB arrangements at once
//! (each box exercises only its own).

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

/// AUs fed: far past every vector's DPB depth (`max_dpb_frames = 7` for the
/// H.264 clip, eight reference slots for AV1), so DPB slots re-activate onto fresh
/// pool images repeatedly.
const AUS: usize = 48;
/// The REAL client's consumption shape: the consumer holds four delivered frames
/// and releases only the oldest beyond that (its channels + preroll + in-flight
/// present hold ~4-7).
const CLIENT_HOLD: usize = 4;
/// 48 AUs may legitimately leave a few pictures buffered for reorder; anything
/// below this is a delivery failure, not reordering.
const MIN_DELIVERED: usize = 40;

/// The geometry one leg's vector must deliver.
struct Geometry {
    /// The vector's display (conformance-window) region.
    display: (u32, u32),
    /// The ALLOCATED extent, when the leg knows it for a fact.
    ///
    /// `pictureAccessGranularity` rounds the coded size up, so this is a
    /// per-vector AND per-driver fact, not a property of the bitstream. The H.264
    /// leg has asserted `(320, 240)` on the fleet since WP-B and keeps asserting
    /// it; the H.265 leg has NO hardware evidence yet, so it asserts only the
    /// invariant that always holds (allocated >= display) and PRINTS what it got
    /// — which is exactly what a first fleet run needs in order to pin it later.
    exact_coded: Option<(u32, u32)>,
}

/// Decode [`AUS`] access units while holding [`CLIENT_HOLD`] frames, asserting the
/// decode verdict of every frame before its release.
///
/// One body for all three codecs (over `common::TestDecoder`) so "the AV1 leg proves
/// what the H.264 leg proves" is structural rather than a claim about three copies.
fn smoke(decoder: &mut impl TestDecoder, aus: &[&[u8]], geometry: &Geometry) {
    // The smoke legs exist to prove the PRODUCTION pool arrangement survives 48
    // AUs at the client's hold depth. `PF_VKD_TEST_READBACK` adds TRANSFER_SRC to
    // the picture pool for whoever sets it, so a shell that exported it while
    // iterating on the parity legs would quietly test a pool production never
    // builds — and the leg would still pass. Refuse rather than mislead.
    assert!(
        std::env::var_os("PF_VKD_TEST_READBACK").is_none(),
        "PF_VKD_TEST_READBACK is set in the environment: it grows the picture pool \
         a usage flag production never carries, so this leg would no longer be \
         testing the production pool arrangement. Unset it for the smoke legs \
         (the parity legs set it themselves, under the same GPU lock)."
    );
    // Status is read (COMPLETE required, the program's whole point) as each frame
    // retires; `take_ready` is drained every AU so nothing is stranded. No
    // presenter runs here, so releases report `presenter_signaled = false` (no
    // `value + 1` write-back).
    let mut held: std::collections::VecDeque<DecodedVkFrame> = std::collections::VecDeque::new();
    let mut delivered = 0usize;
    let mut geometry_checked = false;
    for (index, au) in aus.iter().enumerate().take(AUS) {
        let mut next = decoder.decode(au).unwrap_or_else(|e| {
            panic!(
                "AU {index}: decode failed: {e}\n  state: {}",
                decoder.debug_snapshot()
            )
        });
        while let Some(frame) = next {
            if !geometry_checked {
                assert_eq!(
                    (frame.crop.width, frame.crop.height),
                    geometry.display,
                    "the vector's display region"
                );
                assert!(
                    frame.coded_width >= frame.crop.width
                        && frame.coded_height >= frame.crop.height,
                    "the ALLOCATED extent ({}x{}) must cover the display region ({}x{})",
                    frame.coded_width,
                    frame.coded_height,
                    frame.crop.width,
                    frame.crop.height,
                );
                if let Some(exact) = geometry.exact_coded {
                    assert_eq!(
                        (frame.coded_width, frame.coded_height),
                        exact,
                        "ALLOCATED extent (this vector needs no granularity padding here)"
                    );
                }
                // A pool built for the wrong picture format decodes and then
                // renders with the wrong maths (`DecodedVkFrame::format` docs);
                // all three vectors are 8-bit 4:2:0, so all three must land on NV12.
                assert_eq!(
                    frame.format,
                    pf_vkdecode::NV12,
                    "8-bit 4:2:0 vector must decode into an NV12 pool"
                );
                assert_ne!(frame.image, vk::Image::null());
                assert_ne!(frame.semaphore, vk::Semaphore::null());
                assert!(frame.value > 0);
                eprintln!(
                    "geometry: allocated {}x{} display {}x{} format {:?} layout {:?}",
                    frame.coded_width,
                    frame.coded_height,
                    frame.crop.width,
                    frame.crop.height,
                    frame.format,
                    frame.layout,
                );
                geometry_checked = true;
            }
            held.push_back(frame);
            delivered += 1;
            // Steady state: keep CLIENT_HOLD frames in hand, retire beyond.
            while held.len() > CLIENT_HOLD {
                let oldest = held.pop_front().expect("nonempty");
                assert_eq!(
                    decoder.wait_status(&oldest),
                    DecodeStatus::Ok,
                    "AU {index}: decode op not COMPLETE\n  state: {}",
                    decoder.debug_snapshot()
                );
                decoder
                    .release_frame(&oldest, false)
                    .unwrap_or_else(|e| panic!("AU {index}: release failed: {e}"));
            }
            next = decoder.take_ready();
        }
    }
    // Retire the tail the consumer still holds.
    for frame in held.drain(..) {
        assert_eq!(decoder.wait_status(&frame), DecodeStatus::Ok);
        decoder
            .release_frame(&frame, false)
            .expect("tail frames release");
    }
    assert!(
        delivered >= MIN_DELIVERED,
        "expected at least {MIN_DELIVERED} delivered frames from {AUS} AUs, got {delivered}"
    );
    // The DPB mode the caps derivation chose, and whether this box answers per-op
    // status at all — a passing run should say so too (failure paths already carry
    // the snapshot).
    eprintln!(
        "final state: {} status_queries={}",
        decoder.debug_snapshot(),
        decoder.status_queries()
    );
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::H264,
        // The smoke legs submit nothing outside the decoder, so a decode-only
        // device is usable (and its EXCLUSIVE pool sharing is worth exercising).
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: true,
    });
    let handles = setup.handles();
    {
        // SAFETY: `setup` outlives this block (destroyed below, after the decoder
        // drops at the block's end), it was created with the H.264 decode
        // extensions + timeline/sync2 features, and its queue fields name the
        // families/queues it created.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        smoke(
            &mut decoder,
            &common::split_h264_aus(common::TEST_25FPS_H264),
            &Geometry {
                display: (320, 240),
                exact_coded: Some((320, 240)),
            },
        );
    }
    // SAFETY: the decoder is gone (its Drop drained the queue and destroyed its
    // session/pools), and nothing else references the setup's handles.
    unsafe { setup.destroy() };
}

#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::H265,
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: true,
    });
    let handles = setup.handles();
    {
        // SAFETY: as the H.264 leg — `setup` outlives this block and was created
        // with the H.265 decode extensions + timeline/sync2 features.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // The construction-time shape gate the client's ladder relies on, on the
        // vector's own facts (Main, 4:2:0, 8-bit → NV12). Called here rather than
        // left to the first AU so a device that cannot host the combination says
        // so as a refusal with a caps reason, not as a mid-stream decode failure —
        // and so this path has hardware evidence at all.
        decoder
            .probe_stream_support(1, 0)
            .expect("the box must host H.265 Main 8-bit 4:2:0 (the vector's shape)");
        smoke(
            &mut decoder,
            &common::split_h265_aus(common::TEST_25FPS_H265),
            &Geometry {
                display: (320, 240),
                // No hardware evidence for HEVC's `pictureAccessGranularity` on
                // any fleet box yet; the leg prints what it allocates instead of
                // asserting a number nobody has observed.
                exact_coded: None,
            },
        );
    }
    // SAFETY: as the H.264 leg — the decoder is gone and nothing else references
    // the setup's handles.
    unsafe { setup.destroy() };
}

/// The AV1 leg — the rung's first hardware evidence of ANY kind.
///
/// The same 48 access units at the same client hold depth, but AV1 loads the pool
/// harder than either H.26x leg does and that is the point of running it: the first
/// 48 temporal units carry 53 coded frames to show 48 (the number
/// [`the_delivery_floor_is_under_what_the_planners_emit_from_the_first_48_aus`]
/// prints), each hidden frame keeps a pool image resident as a reference while
/// nothing displays it, and eight reference slots re-activate against that. A pool
/// sized as if one access unit meant one picture starves exactly here — which is this
/// leg's whole job, since `gpu_parity`'s AV1 leg would report the same starvation as a
/// decode failure with a less obvious cause.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: true,
    });
    let handles = setup.handles();
    {
        // SAFETY: as the H.264 leg — `setup` outlives this block and was created
        // with the AV1 decode extension + timeline/sync2 features.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // The construction-time shape gate on the vector's own facts (Main, 4:2:0,
        // 8-bit, NO film grain → NV12). The film-grain argument is the one that has
        // no H.26x counterpart: grain synthesis is part of the Vulkan decode PROFILE,
        // so a box offering only the grain-enabled profile refuses HERE with a caps
        // reason rather than at the first temporal unit.
        decoder.probe_stream_support(1, 8, false).expect(
            "the box must host AV1 Main 4:2:0 8-bit without film grain (the vector's shape)",
        );
        smoke(
            &mut decoder,
            &common::split_av1_aus(common::TEST_25FPS_AV1),
            &Geometry {
                display: (320, 240),
                // As HEVC: no hardware evidence for AV1's `pictureAccessGranularity`
                // on any fleet box yet, so the leg prints what it allocates rather
                // than asserting a number nobody has observed. AV1's decode extent is
                // the POST-superres width, which is another reason not to guess.
                exact_coded: None,
            },
        );
    }
    // SAFETY: as the H.264 leg — the decoder is gone and nothing else references
    // the setup's handles.
    unsafe { setup.destroy() };
}

// ---------------------------------------------------------------------------
// CPU coherence guards — NOT `#[ignore]`d.
//
// The legs above only run on the fleet, so [`MIN_DELIVERED`] would otherwise be a
// number copied from the H.264 leg and never checked against the H.265 or AV1
// vector's own reorder depth. It is the CPU planner that decides how many of the
// first [`AUS`] pictures can possibly be delivered — the decoder builds exactly one
// frame per `dpb.outputs` id — so the floor is checkable here, without a GPU, and
// a re-synced vector that reorders more deeply fails HERE instead of looking like
// a pool-starvation bug on hardware.
// ---------------------------------------------------------------------------

#[test]
fn the_delivery_floor_is_under_what_the_planners_emit_from_the_first_48_aus() {
    let h264 = {
        let mut planner = pf_bitstream::h264::H264Planner::new();
        common::split_h264_aus(common::TEST_25FPS_H264)
            .iter()
            .take(AUS)
            .enumerate()
            .map(|(index, au)| {
                planner
                    .plan_au(au)
                    .unwrap_or_else(|e| panic!("H.264 AU {index} must plan, got {e:?}"))
                    .dpb
                    .outputs
                    .len()
            })
            .sum::<usize>()
    };
    let h265 = {
        let mut planner = pf_bitstream::h265::H265Planner::new();
        common::split_h265_aus(common::TEST_25FPS_H265)
            .iter()
            .take(AUS)
            .enumerate()
            .map(|(index, au)| {
                planner
                    .plan_au(au)
                    .unwrap_or_else(|e| panic!("H.265 AU {index} must plan, got {e:?}"))
                    .dpb
                    .outputs
                    .len()
            })
            .sum::<usize>()
    };
    // AV1 needs the extra fold: one temporal unit can plan SEVERAL frames, so the
    // outputs of a unit are the outputs of all of its plans — and counting one plan
    // per unit is exactly how a reader would under-count here.
    let (av1, av1_frames) = {
        let mut planner = pf_bitstream::av1::Av1Planner::new();
        let mut outputs = 0usize;
        let mut frames = 0usize;
        for (index, au) in common::split_av1_aus(common::TEST_25FPS_AV1)
            .iter()
            .take(AUS)
            .enumerate()
        {
            let plans = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AV1 temporal unit {index} must plan, got {e:?}"));
            frames += plans.len();
            outputs += plans.iter().map(|p| p.dpb.outputs.len()).sum::<usize>();
        }
        (outputs, frames)
    };
    eprintln!(
        "outputs from the first {AUS} AUs: h264={h264} h265={h265} av1={av1} \
         (av1 decoded {av1_frames} frames to show {av1} — the hidden ones)"
    );
    // No `flush` here on purpose: the smoke legs do not flush either, so the
    // planner's un-flushed output count is exactly the frame budget they have.
    assert!(
        h264 >= MIN_DELIVERED,
        "the H.264 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {h264} pictures from the first {AUS} AUs"
    );
    assert!(
        h265 >= MIN_DELIVERED,
        "the H.265 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {h265} pictures from the first {AUS} AUs"
    );
    assert!(
        av1 >= MIN_DELIVERED,
        "the AV1 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {av1} pictures from the first {AUS} temporal units"
    );
    // The AV1 leg's load is not the same as the other two's, and the assertion above
    // cannot see the difference: the pool must hold the hidden frames as well as the
    // shown ones. If these ever became equal, the leg would have stopped exercising
    // multi-frame temporal units — the one pool pressure AV1 has that H.26x has not —
    // while still passing everything above.
    assert!(
        av1_frames > av1,
        "the first {AUS} AV1 temporal units must decode MORE frames ({av1_frames}) \
         than they show ({av1}); equal counts mean the hidden-frame coverage is gone"
    );
}
