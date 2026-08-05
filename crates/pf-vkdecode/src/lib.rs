//! Native Vulkan Video H.264 decode for the clients — M2 of the native-decode program
//! (design/client-native-decode.md §3.2).
//!
//! This crate sits between [`pf_bitstream`]'s per-AU planning and Vulkan Video
//! submission. WP-A (this round) is the CPU-testable half, and everything in it runs
//! without a GPU:
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
//! WP-B adds the other half: VkVideoSessionKHR/session-parameters objects, DPB image
//! memory, command recording and result queries. Nothing here touches a VkDevice.
//!
//! Unsafe posture: unlike pf-bitstream (which forbids unsafe outright), this crate
//! cannot — the `ash::vk::native` bindgen structs are zero-initialized the way the
//! encode side does it (`pf-encode/src/enc/linux/vk_build.rs`), and WP-B brings Vulkan
//! FFI. Every unsafe block therefore carries a written `// SAFETY:` proof, enforced:
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod params;
pub mod pic;
pub mod slots;

pub use params::pps_to_std;
pub use params::sps_to_std;
pub use params::OwnedStdPps;
pub use params::OwnedStdSps;
pub use params::ParamsError;
pub use pic::plan_to_vk;
pub use pic::DecodePlanVk;
pub use pic::PlanToVkError;
pub use pic::VkRef;
pub use slots::SlotError;
pub use slots::SlotMap;
