//! Native Vulkan Video H.264 decode for the clients — M2 of the native-decode program
//! (design/client-native-decode.md §3.2).
//!
//! This crate sits between [`pf_bitstream`]'s per-AU planning and Vulkan Video
//! submission.
//!
//! WP-A — the CPU-testable half, everything runs without a GPU:
//!
//! - [`params`]: the vendored parser's `Sps`/`Pps` converted into the
//!   `StdVideoH264*ParameterSet` structs session parameters are created from, behind
//!   owning wrappers ([`OwnedStdSps`]/[`OwnedStdPps`]) because the Std structs embed
//!   raw pointers.
//! - [`slots`]: [`SlotMap`], the hardware DPB slot ledger keyed by
//!   [`pf_bitstream::h264::PicId`]. pf-bitstream's DPB decides what lives and dies;
//!   this map only translates ids to slot indices and refuses to guess.
//! - [`pic`]: [`plan_to_vk`], one [`pf_bitstream::h264::AuPlan`] converted into the
//!   `StdVideoDecodeH264PictureInfo`/`StdVideoDecodeH264ReferenceInfo` set plus slice
//!   offsets and slot bindings a `vkCmdDecodeVideoKHR` call wants.
//!
//! WP-B — the GPU half, built ON the borrowed presenter device (this crate never
//! creates or destroys a VkDevice) with the decision logic split out pure so it
//! stays CPU-testable:
//!
//! - [`device`]: [`DeviceHandles`] (the borrowed handle bundle + its liveness
//!   contract), the [`QueueLock`] trait every submit runs under, [`DecodeDevice`].
//! - [`caps`]: one thin driver query + [`derive_caps`], the pure
//!   coincide/distinct/layered decision table ([`DecodeCaps`]).
//! - [`session`]: `VkVideoSessionKHR` + versioned session parameters (pure ledger
//!   decides Add-vs-Recreate; extent/DPB renegotiation rebuilds the session).
//! - [`images`]: the picture pool DECOUPLED from DPB slots (the zero-copy FFmpeg
//!   pool model — a re-activated slot binds a fresh free image, so delivered
//!   pictures are never decode targets), per-image timeline semaphores with the
//!   presenter `value+1` write-back, per-plane views, [`HOLD_HEADROOM`] sizing.
//! - [`ring`]: the host-visible bitstream upload ring (pure alignment/growth
//!   math) — SLICE NALUs only (feeding a whole AU hangs VCN firmware).
//! - [`decoder`]: [`VkH264Decoder`] — plan → convert → upload → record → submit,
//!   with a per-op `RESULT_STATUS_ONLY` query ([`VkH264Decoder::poll_status`]) so
//!   driver-reported corruption is finally observable (the Ally X class) —
//!   caps-gated per queue family: where `queryResultStatusSupport` is absent
//!   (RADV), verdicts degrade to timeline completion, FFmpeg parity.
//!
//! M3 (HEVC) — the CPU half, over [`pf_bitstream::h265`]'s WP-1 planner:
//!
//! - [`params_h265`]: VPS/SPS/PPS into the `StdVideoH265*ParameterSet` structs
//!   behind owning wrappers ([`OwnedStdH265Vps`]/[`OwnedStdH265Sps`]/
//!   [`OwnedStdH265Pps`]) — Main/Main10/4:4:4 RExt fidelity carried through, the
//!   rest of the envelope rejected typed.
//! - [`pic_h265`]: [`plan_to_vk_h265`], one [`pf_bitstream::h265::AuPlan`] into
//!   `StdVideoDecodeH265PictureInfo`/`StdVideoDecodeH265ReferenceInfo` plus the
//!   RPS index arrays, slice offsets and slot bindings — over the SAME
//!   [`SlotMap`] (H.265's DPB ceiling is H.264's: 16 references + 1 setup).
//!
//! M3 (HEVC) — the GPU half, sharing every codec-agnostic piece with H.264
//! (picture pool, bitstream ring, op ring, DPB settling, frame delivery) rather
//! than re-implementing them:
//!
//! - [`caps_h265`]: [`H265ProfileKey`] (the stream's profile idc, chroma format
//!   and bit depths, which Vulkan wants stated on every object) and
//!   [`derive_caps_h265`] — Main → NV12, Main 10 → P010, RExt 4:4:4 → the
//!   two-plane 4:4:4 formats, with a device that cannot host the combination
//!   refused BEFORE a session exists ([`CapsError::NoFormat`]).
//! - [`session_h265`]: the H.265 session and its THREE-array parameters ledger —
//!   VPS included, with [`fallback_vps_from_sps`] standing in (and deduping
//!   correctly) for streams joined after their VPS NALU.
//! - [`decoder_h265`]: [`VkH265Decoder`], mirroring [`VkH264Decoder`]'s public
//!   surface method-for-method. Codec DISPATCH is the client wiring's job.
//!
//! M7 (AV1) — the CPU half, over [`pf_bitstream::av1`]'s planner:
//!
//! - [`params_av1`]: the sequence header into `StdVideoAV1SequenceHeader` behind an
//!   owning wrapper ([`OwnedStdAv1SequenceHeader`]) — the ONE parameter set AV1 has.
//! - [`pic_av1`]: [`plan_to_vk_av1`], one [`pf_bitstream::av1::AuPlan`] into
//!   `StdVideoDecodeAV1PictureInfo` and its eight per-frame sub-blocks, plus the
//!   per-reference-NAME DPB SLOT table, the tile-group ranges and the slot bindings
//!   — over the SAME [`SlotMap`] (AV1's ceiling is eight references + one setup).
//!
//! M7 (AV1) — the GPU half, sharing every codec-agnostic piece with the other two
//! (picture pool, bitstream ring, op ring, frame delivery, DPB settling) rather
//! than re-implementing them:
//!
//! - [`caps_av1`]: [`Av1ProfileKey`] — Std profile, sampling, bit depth AND the
//!   sequence's film-grain flag, because `filmGrainSupport` is part of the Vulkan
//!   decode PROFILE — and [`derive_caps_av1`]: 4:2:0 8-bit → NV12, 10-bit → P010,
//!   4:4:4 → the two-plane 4:4:4 pair, with a device that cannot host the
//!   combination (film grain very much included) refused BEFORE a session exists.
//! - [`session_av1`]: the AV1 session and its ONE-set parameters ledger — no PPS,
//!   no VPS, no update path at all, so a changed sequence header RECREATES.
//! - [`decoder_av1`]: [`VkAv1Decoder`], mirroring [`VkH265Decoder`]'s public
//!   surface method-for-method, over temporal units that may carry several frames.
//!
//! M4 (status and telemetry) — three pure modules turning the signals above into
//! something a session, a user and a support engineer can act on:
//!
//! - [`recovery`]: [`RecoveryWatch`], the recovery point SEI folded into a
//!   per-picture "the stream healed HERE" mark ([`RecoveryMark`], carried on
//!   [`DecodedVkFrame::recovery`]). The only clean point an intra-refresh session
//!   has — its wave emits no IDR — so without it a client freezes for its full
//!   backstop and then forces the very IDR the wave exists to avoid.
//! - [`integrity`]: [`is_integrity_warning`] / [`is_integrity_warning_h265`] /
//!   [`is_integrity_warning_av1`], the one list of warnings that mean the PICTURE
//!   is damaged. Here rather than in the client so the fault harness asserts
//!   against the predicate production conceals on.
//! - [`fault`]: [`AuFault`], deliberate decoder-input corruption
//!   (`PUNKTFUNK_AU_FAULT`), inert unless armed. A detector nobody can fire is
//!   exactly as trustworthy as no detector at all.
//!
//! Plus [`VkH264Decoder::status_queries`] / [`VkH265Decoder::status_queries`]: does
//! this device answer per-op `RESULT_STATUS` at all? Without that fact a clean
//! integrity report cannot be told apart from an unmeasured one — which is the
//! precise failure the program exists to end.
//!
//! Unsafe posture: unlike pf-bitstream (which forbids unsafe outright), this crate
//! cannot — the `ash::vk::native` bindgen structs are zero-initialized the way the
//! encode side does it (`pf-encode/src/enc/linux/vk_build.rs`), and the GPU half is
//! Vulkan FFI. Every unsafe block therefore carries a written `// SAFETY:` proof,
//! enforced (and unlike the encoder there is NO file-level
//! `unsafe_op_in_unsafe_fn` exemption — every operation is individually fenced):
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod caps;
pub mod caps_av1;
pub mod caps_h265;
pub mod decoder;
pub mod decoder_av1;
pub mod decoder_h265;
pub mod device;
pub mod fault;
pub mod images;
pub mod integrity;
pub mod params;
pub mod params_av1;
pub mod params_h265;
pub mod pic;
pub mod pic_av1;
pub mod pic_h265;
pub mod recovery;
pub mod ring;
pub mod session;
pub mod session_av1;
pub mod session_h265;
pub mod slots;

/// Re-exported for the integration layer (WP-C): [`DecodedVkFrame`]'s handle fields are
/// ash types, and the consumer (pf-client-core, whose own `ash` is optional/feature-gated)
/// flattens them to raw `u64`s through `ash::vk::Handle` — via THIS instance of ash, so
/// the versions can never skew.
pub use ash;
// The pf-bitstream types a [`DecodedVkFrame`] consumer names, re-exported so it
// doesn't grow a pf-bitstream dependency of its own:
/// [`VkAv1Decoder::take_warnings`]'s warning type — the AV1 twin of
/// [`PlanWarning`], renamed for the same reason [`H265PlanWarning`] is: the three
/// enums are genuinely different (AV1 has `MissingShowExisting`, and its
/// `MissingReference` needs no interpretation because no AV1 process empties a
/// reference slot behind the stream's back) and a consumer dispatching per codec
/// must be able to name all three.
pub use pf_bitstream::av1::PlanWarning as Av1PlanWarning;
/// [`DecodedVkFrame::colour`]'s type.
pub use pf_bitstream::h264::ColourDescription;
/// [`DecodedVkFrame::crop`]'s type.
pub use pf_bitstream::h264::DisplayCrop;
/// [`VkH264Decoder::take_warnings`]'s warning type.
pub use pf_bitstream::h264::PlanWarning;
/// [`VkH265Decoder::take_warnings`]'s warning type — the H.265 twin of
/// [`PlanWarning`], renamed rather than shadowed because the two enums are
/// genuinely different (H.264 has `FrameNumGap`/`Mmco5Rebase`, H.265 has
/// `NonZeroReorder`) and a consumer dispatching per codec must be able to name
/// BOTH. Without it the client could only render warnings as strings — and it has
/// to BRANCH on them: `NonZeroReorder` and `Mmco5Rebase` are spec-legal facts the
/// planner planned in full, not concealment, and dropping their frames would cost
/// a visible hitch at every SPS activation.
pub use pf_bitstream::h265::PlanWarning as H265PlanWarning;

pub use caps::derive_caps;
pub use caps::plane_formats;
pub use caps::CapsError;
pub use caps::DecodeCaps;
pub use caps::MaxLevelIdc;
pub use caps::RawH264Caps;
pub use caps::VideoFormat;
pub use caps::NV12;
pub use caps::OUTPUT_FORMATS;
pub use caps::P010;
pub use caps::YUV444_10;
pub use caps::YUV444_8;
pub use caps_av1::derive_caps_av1;
pub use caps_av1::Av1ProfileKey;
pub use caps_av1::RawAv1Caps;
pub use caps_h265::derive_caps_h265;
pub use caps_h265::output_format_for;
pub use caps_h265::H265ProfileKey;
pub use caps_h265::RawH265Caps;
pub use decoder::DecodeStatus;
pub use decoder::DecodedVkFrame;
pub use decoder::VkDecodeError;
pub use decoder::VkH264Decoder;
pub use decoder_av1::plan_bitstream;
pub use decoder_av1::Av1Bitstream;
pub use decoder_av1::Av1TileError;
pub use decoder_av1::VkAv1Decoder;
pub use decoder_h265::VkH265Decoder;
pub use device::DecodeDevice;
pub use device::DeviceHandles;
pub use device::NoopQueueLock;
pub use device::QueueLock;
pub use device::QueueSubmitGuard;
pub use fault::AuFault;
pub use fault::FaultAction;
pub use fault::FaultMode;
pub use fault::DEFAULT_FAULT_PERIOD;
pub use images::plan_pools;
pub use images::PoolPlan;
pub use images::HOLD_HEADROOM;
pub use integrity::is_integrity_warning;
pub use integrity::is_integrity_warning_av1;
pub use integrity::is_integrity_warning_h265;
pub use params::pps_to_std;
pub use params::sps_to_std;
pub use params::OwnedStdPps;
pub use params::OwnedStdSps;
pub use params::ParamsError;
pub use params_av1::sequence_to_std;
pub use params_av1::OwnedStdAv1SequenceHeader;
pub use params_av1::ParamsAv1Error;
pub use params_h265::fallback_vps_from_sps;
pub use params_h265::pps_to_std_h265;
pub use params_h265::sps_to_std_h265;
pub use params_h265::vps_to_std_h265;
pub use params_h265::H265ParamsError;
pub use params_h265::OwnedStdH265Pps;
pub use params_h265::OwnedStdH265Sps;
pub use params_h265::OwnedStdH265Vps;
pub use pic::plan_to_vk;
pub use pic::DecodePlanVk;
pub use pic::PlanToVkError;
pub use pic::VkRef;
pub use pic_av1::plan_to_vk_av1;
pub use pic_av1::DecodePlanVkAv1;
pub use pic_av1::OwnedStdAv1PictureInfo;
pub use pic_av1::PlanToVkAv1Error;
pub use pic_av1::VkRefAv1;
pub use pic_av1::REFERENCE_NAME_UNUSED;
pub use pic_h265::plan_to_vk_h265;
pub use pic_h265::DecodePlanVkH265;
pub use pic_h265::PlanToVkH265Error;
pub use pic_h265::VkRefH265;
pub use pic_h265::H265_RPS_LIST_SIZE;
pub use recovery::RecoveryMark;
pub use recovery::RecoveryWatch;
pub use ring::RingLayout;
pub use session::ParamsAction;
pub use session::SessionConfig;
pub use session_av1::ParamsActionAv1;
pub use session_av1::SessionConfigAv1;
pub use session_h265::ParamsActionH265;
pub use session_h265::SessionConfigH265;
pub use slots::SlotError;
pub use slots::SlotMap;
