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
//! Unsafe posture: unlike pf-bitstream (which forbids unsafe outright), this crate
//! cannot — the `ash::vk::native` bindgen structs are zero-initialized the way the
//! encode side does it (`pf-encode/src/enc/linux/vk_build.rs`), and the GPU half is
//! Vulkan FFI. Every unsafe block therefore carries a written `// SAFETY:` proof,
//! enforced (and unlike the encoder there is NO file-level
//! `unsafe_op_in_unsafe_fn` exemption — every operation is individually fenced):
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod caps;
pub mod decoder;
pub mod device;
pub mod images;
pub mod params;
pub mod pic;
pub mod ring;
pub mod session;
pub mod slots;

/// Re-exported for the integration layer (WP-C): [`DecodedVkFrame`]'s handle fields are
/// ash types, and the consumer (pf-client-core, whose own `ash` is optional/feature-gated)
/// flattens them to raw `u64`s through `ash::vk::Handle` — via THIS instance of ash, so
/// the versions can never skew.
pub use ash;
// The pf-bitstream types a [`DecodedVkFrame`] consumer names, re-exported so it
// doesn't grow a pf-bitstream dependency of its own:
/// [`DecodedVkFrame::colour`]'s type.
pub use pf_bitstream::h264::ColourDescription;
/// [`DecodedVkFrame::crop`]'s type.
pub use pf_bitstream::h264::DisplayCrop;
/// [`VkH264Decoder::take_warnings`]'s warning type.
pub use pf_bitstream::h264::PlanWarning;

pub use caps::derive_caps;
pub use caps::CapsError;
pub use caps::DecodeCaps;
pub use caps::RawH264Caps;
pub use caps::VideoFormat;
pub use decoder::DecodeStatus;
pub use decoder::DecodedVkFrame;
pub use decoder::VkDecodeError;
pub use decoder::VkH264Decoder;
pub use device::DecodeDevice;
pub use device::DeviceHandles;
pub use device::NoopQueueLock;
pub use device::QueueLock;
pub use device::QueueSubmitGuard;
pub use images::plan_pools;
pub use images::PoolPlan;
pub use images::HOLD_HEADROOM;
pub use params::pps_to_std;
pub use params::sps_to_std;
pub use params::OwnedStdPps;
pub use params::OwnedStdSps;
pub use params::ParamsError;
pub use pic::plan_to_vk;
pub use pic::DecodePlanVk;
pub use pic::PlanToVkError;
pub use pic::VkRef;
pub use ring::RingLayout;
pub use session::ParamsAction;
pub use session::SessionConfig;
pub use slots::SlotError;
pub use slots::SlotMap;
