//! Native D3D11VA (DXVA) H.264/HEVC decode for the Windows clients — M5 of the
//! native-decode program, and the DXVA counterpart of [`pf_vkdecode`].
//!
//! This crate is the CPU-testable half: everything between pf-bitstream's per-AU
//! plan and the bytes an `ID3D11VideoContext::SubmitDecoderBuffers` call
//! delivers. It never touches D3D11, COM, or any Windows type — which is the
//! point. The D3D11VA rung is `cfg(windows)` code that neither the macOS
//! development host nor the Linux container can compile, let alone run, so
//! anything left inside that boundary is verified by a `cargo check` on a remote
//! box and nothing more. Everything that can be a pure decision or a pure
//! conversion lives here instead, where the ordinary gates run it on every leg.
//!
//! - [`dxva`]: the `dxva.h` buffer layouts, **hand-declared** — windows-rs does
//!   not generate them (see the module docs for the verification and for the
//!   compile-time size/offset proofs that stand in for a header).
//! - [`config`]: decoder-creation decisions — profile GUID per codec/shape,
//!   `D3D11_VIDEO_DECODER_CONFIG` selection (short-format slice control, whose
//!   `ConfigBitstreamRaw` value differs between the two codecs), surface
//!   alignment and pool sizing.
//! - [`pack`]: the bitstream buffer's contents — start-code normalisation and
//!   the 128-byte tail padding rule.
//! - [`pic`] / [`pic_h265`]: one [`pf_bitstream`] `AuPlan` into
//!   `DXVA_PicParams_*`, `DXVA_Qmatrix_*` and the slice-control records, with the
//!   reference lists resolved through a DPB slot map.
//! - [`descriptors`]: which buffers one `SubmitDecoderBuffers` call carries and
//!   the four `D3D11_VIDEO_DECODER_BUFFER_DESC` fields that are a decision —
//!   where two of review 13's three structural defects lived, and the reason
//!   they are now a CPU test rather than a Windows-only code path.
//!
//! # Why the slot map comes from pf-vkdecode
//!
//! [`SlotMap`] is re-exported from [`pf_vkdecode`] rather than reimplemented.
//! It is not a Vulkan object: it is a ledger mapping
//! [`pf_bitstream::h264::PicId`]s to hardware DPB slot indices, codec-agnostic
//! (H.264 and H.265 share it there) and, as it turns out, API-agnostic too —
//! DXVA's `DXVA_PicEntry::Index7Bits` is a decode-surface index with exactly the
//! lifetime the map already models. It lives in pf-vkdecode because M2 is where
//! it was written and where hardware has already proven it over a 92-minute
//! soak; duplicating a ledger of that provenance to avoid one crate edge would
//! buy nothing and cost a divergence.
//!
//! # Unsafe posture
//!
//! Almost none. Every DXVA structure is built field by field from a `const fn
//! zeroed()`, never `mem::zeroed`, so construction is entirely safe code. The
//! only unsafe in the crate is the byte view the submission needs
//! ([`dxva::as_bytes`] / [`dxva::slice_bytes`]), fenced behind a sealed trait
//! that only this crate's `#[repr(C)]` PODs implement, and carrying a written
//! proof — enforced:
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod config;
pub mod descriptors;
pub mod dxva;
pub mod dxva_av1;
pub mod pack;
pub mod pic;
pub mod pic_av1;
pub mod pic_h265;

/// The DPB slot ledger — see the crate docs for why it is borrowed rather than
/// redefined. Re-exported so this crate's callers name it through `pf_dxvadec`.
pub use pf_vkdecode::SlotError;
pub use pf_vkdecode::SlotMap;

// Unlike pf-vkdecode, this crate exposes the PLANNER rather than wrapping it: DXVA
// submission is synchronous and stateless (`DecoderBeginFrame` … `DecoderEndFrame`
// with no fences, no timeline, no picture pool to decouple), so there is no
// per-decode state worth an owning decoder type here. The Windows layer drives the
// planner itself — and names every type it touches through this crate, so it needs
// no pf-bitstream dependency of its own.
/// The H.264 planner and the plan it produces.
pub use pf_bitstream::h264::AuPlan;
pub use pf_bitstream::h264::ColourDescription;
pub use pf_bitstream::h264::DisplayCrop;
pub use pf_bitstream::h264::H264Planner;
pub use pf_bitstream::h264::PlanError;
pub use pf_bitstream::h264::PlanWarning;
/// The H.265 planner and its plan — separate types with the same job, exactly as
/// pf-bitstream defines them (their warning enums genuinely differ, and a consumer
/// dispatching per codec must be able to name both).
pub use pf_bitstream::h265::AuPlan as AuPlanH265;
pub use pf_bitstream::h265::H265Planner;
pub use pf_bitstream::h265::PlanError as PlanErrorH265;
pub use pf_bitstream::h265::PlanWarning as PlanWarningH265;
/// Which warnings mean the PICTURE is damaged — pf-vkdecode's one list, reused so
/// both native rungs conceal on exactly the same predicate.
pub use pf_vkdecode::is_integrity_warning;
pub use pf_vkdecode::is_integrity_warning_h265;

pub use config::align_surface;
pub use config::pick_config;
pub use config::pool_size;
pub use config::profile_for;
pub use config::short_slice_config;
pub use config::surface_alignment;
pub use config::Codec;
pub use config::ConfigFacts;
pub use config::DxvaProfile;
pub use config::DXGI_FORMAT_NV12;
pub use config::DXGI_FORMAT_P010;
pub use config::H264_VLD_NOFGT;
pub use config::HEVC_VLD_MAIN;
pub use config::HEVC_VLD_MAIN10;
pub use descriptors::descriptors_h264;
pub use descriptors::descriptors_h265;
pub use descriptors::BufferDescriptor;
pub use descriptors::BUFFER_BITSTREAM;
pub use descriptors::BUFFER_INVERSE_QUANTIZATION_MATRIX;
pub use descriptors::BUFFER_PICTURE_PARAMETERS;
pub use descriptors::BUFFER_SLICE_CONTROL;
pub use dxva::as_bytes;
pub use dxva::slice_bytes;
pub use dxva::PicParamsH264;
pub use dxva::PicParamsHevc;
pub use dxva::QmatrixH264;
pub use dxva::QmatrixHevc;
pub use dxva::SliceH264Short;
pub use dxva::SliceHevcShort;
pub use dxva::BITSTREAM_ALIGN;
pub use pack::pack;
pub use pack::packed_size;
pub use pack::PackError;
pub use pack::Packed;
pub use pack::SliceRecord;
pub use pic::plan_to_dxva;
pub use pic::slice_control;
pub use pic::DecodePlanDxva;
pub use pic::DxvaRef;
pub use pic::PlanToDxvaError;
pub use pic_h265::plan_to_dxva_h265;
pub use pic_h265::slice_control_h265;
pub use pic_h265::DecodePlanDxvaH265;
pub use pic_h265::DxvaRefH265;
pub use pic_h265::PlanToDxvaH265Error;
