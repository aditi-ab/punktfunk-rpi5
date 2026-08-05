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
//! - [`images`]: DPB + output pools for BOTH DPB arrangements (pure [`plan_pools`]
//!   decides the shape), per-plane views, one timeline semaphore per output slot,
//!   crop carried on the frame.
//! - [`ring`]: the host-visible bitstream upload ring (pure alignment/growth math).
//! - [`decoder`]: [`VkH264Decoder`] — plan → convert → upload → record → submit,
//!   with a per-op `RESULT_STATUS_ONLY` query ([`VkH264Decoder::poll_status`]) so
//!   driver-reported corruption is finally observable (the Ally X class).
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
