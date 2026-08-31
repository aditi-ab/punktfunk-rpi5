//! Native Vulkan Video decoding for H.264, H.265, and AV1, between
//! [`pf_bitstream`] access-unit planning and GPU submission.
//!
//! The crate borrows the presenter's Vulkan device and never creates or destroys
//! it; every queue submission is serialized through the caller's [`QueueLock`].
//! Parameter wrappers own all storage referenced by embedded StdVideo pointers.
//! [`SlotMap`] only translates planner-owned DPB identities into hardware slots;
//! it must not infer reference lifetime or conceal missing references.
//! Picture images are independent of DPB slots: delivered images remain live and
//! are never rebound as decode targets; timeline hand-off uses presenter value+1.
//! Upload rings contain only codec decode payloads, with Vulkan alignment and
//! lifetime maintained until the corresponding timeline operation completes.
//! Session parameters and pools are rebuilt when profile, extent, or DPB changes.
//! Result-status queries are capability-gated; unsupported means unmeasured, not
//! clean. Recovery marks and fault injection remain independent optional signals.
//! Vulkan FFI is unsafe; each unsafe operation requires a local `SAFETY` proof,
//! with no file-level `unsafe_op_in_unsafe_fn` exemption.

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
pub mod probe;
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
