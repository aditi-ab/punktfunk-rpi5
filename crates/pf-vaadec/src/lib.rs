//! Native VAAPI decode for the Linux clients — M6 (H.264/HEVC) and M7 (AV1) of the
//! native-decode program, and the VAAPI counterpart of [`pf_vkdecode`] and
//! `pf-dxvadec`.
//!
//! Like `pf-dxvadec`, this crate is the **CPU-testable half**: everything between
//! pf-bitstream's per-AU plan and the buffers a `vaRenderPicture` call delivers. It
//! links no libva, names no `VA*` handle type, and compiles on macOS and in the Linux
//! container — which is the point. The VAAPI rung itself is
//! `cfg(target_os = "linux")` code that only a box can build, so anything left inside
//! that boundary is verified by a remote `cargo check` and nothing more.
//!
//! - [`va`] / [`va_h265`] / [`va_av1`]: the libva decode buffer layouts,
//!   **hand-declared**, with every size and offset measured off the real headers and
//!   pinned as compile-time assertions.
//! - [`config`]: profile, render-target format and surface-count decisions.
//! - [`pic`] / [`pic_h265`] / [`pic_av1`]: one `AuPlan` into picture parameters, IQ
//!   matrices and slice records (AV1: tile records, and no IQ matrix at all).
//!
//! # Status
//!
//! **All three codecs converted, and the rung is wired.** `pf-client-core`'s
//! `video_vaapi_native` dlopens libva and drives these buffers; this crate holds
//! everything decidable without a device — including [`drm`], the export
//! descriptor the driver writes back and the plane walk that reads it.
//!
//! ⚠ **Nothing here has decoded a frame.** The rung is pin-only
//! (`PUNKTFUNK_DECODER=native-vaapi`) and no VAAPI hardware has been reachable
//! during M7, so everything below is a CPU-side conversion checked against
//! libavcodec and against measured layouts, not against a picture.
//!
//! Five things this crate settled that a reader would otherwise have to re-derive:
//!
//! * **`slice_data_bit_offset` costs no new parsing.** VAAPI is the only one of the
//!   three backends that wants a bit position — DXVA takes a byte offset, Vulkan
//!   takes none — and the vendored parser already records exactly it as
//!   `SliceHeader::header_bit_size`, because cros-codecs' own production backend is
//!   VAAPI. Its definition matches field for field: computed as
//!   `(nalu.size - emulation_prevention_bytes) * 8 - bits_left`, it counts from and
//!   including the NAL header byte with emulation-prevention bytes removed, which is
//!   what `VASliceParameterBufferH264` documents.
//! * **The slice data buffer starts at the NAL header byte**, so the start code is
//!   skipped — `SlicePlan::data` is start-code-inclusive, and the prefix is three
//!   OR four bytes (the real host emits four on 100% of access units), so it is
//!   measured per slice rather than assumed.
//! * **`VAPictureParameterBufferH264::reference_frames` is the MARKED DPB**, not the
//!   access unit's own lists — the same statement DXVA's `RefFrameList` makes, so it
//!   is filled from pf-bitstream's per-AU `dpb_refs` snapshot. Vulkan's
//!   `pReferenceSlots` is the opposite and takes the AU's own set; all three
//!   conventions now have a written home.
//! * **Unlike DXVA's short-format slice control, VAAPI wants the per-slice reference
//!   lists themselves** (`RefPicList0`/`RefPicList1`, 32 entries each, in 8.2.4.2
//!   order) and the prediction weight tables. One wrinkle handled in [`pic`]: the
//!   vendored `PredWeightTable` stores `luma_offset_l0` as `[i8; 32]` but
//!   `luma_offset_l1` as `[i16; 32]`, an upstream inconsistency, while libva wants
//!   `i16` for both.
//! * **AV1's reference plumbing is a FIFTH convention**, and libva's AV1 buffers
//!   break three of this rung's other habits: the "slice" parameter buffer is a TILE
//!   parameter buffer, several of its records share ONE data buffer (the only place
//!   `vaCreateBuffer`'s `num_elements` is not 1), and there is no IQ matrix buffer at
//!   all. [`va_av1`] states the convention and what it was established from;
//!   [`pic_av1`] is where it is applied.
//!
//! # Why the slot ledger is borrowed
//!
//! [`SlotMap`] comes from [`pf_vkdecode`] for the reason `pf-dxvadec`'s docs give at
//! length: it is not a Vulkan object but a ledger from
//! [`pf_bitstream::h264::PicId`] to hardware DPB slot indices, and it is as
//! API-agnostic as it is codec-agnostic. VAAPI's own indirection is one step longer —
//! a slot indexes the caller's surface table, because `VAPictureH264::picture_id` is
//! a `VASurfaceID` rather than an index — so the conversion will take that table as
//! a parameter and stay pure.

pub mod config;
pub mod drm;
pub mod pic;
pub mod pic_av1;
pub mod pic_h265;
pub mod va;
pub mod va_av1;
pub mod va_h265;

/// The DPB slot ledger — borrowed, not redefined (crate docs).
pub use pf_vkdecode::SlotError;
pub use pf_vkdecode::SlotMap;

/// The AV1 planner and its plan. ⚠ Its `plan_au` returns a **`Vec`**: an AV1 access
/// unit is a TEMPORAL UNIT and may carry several frames, of which at most one
/// displays.
pub use pf_bitstream::av1::AuPlan as AuPlanAv1;
pub use pf_bitstream::av1::Av1Planner;
pub use pf_bitstream::av1::DpbUpdate as DpbUpdateAv1;
pub use pf_bitstream::av1::FrameType as FrameTypeAv1;
pub use pf_bitstream::av1::ParsedFrameHeader as ParsedFrameHeaderAv1;
pub use pf_bitstream::av1::ParsedSequenceHeader as ParsedSequenceHeaderAv1;
pub use pf_bitstream::av1::PicId as PicIdAv1;
pub use pf_bitstream::av1::PicturePlan as PicturePlanAv1;
pub use pf_bitstream::av1::PlanError as PlanErrorAv1;
pub use pf_bitstream::av1::PlanWarning as PlanWarningAv1;
pub use pf_bitstream::av1::NUM_REF_SLOTS;
/// The planners and plans this crate converts, re-exported so the Linux layer names
/// every type it touches through `pf_vaadec` — the same courtesy `pf-dxvadec` does
/// for the Windows layer.
pub use pf_bitstream::h264::AuPlan;
pub use pf_bitstream::h264::ColourDescription;
pub use pf_bitstream::h264::DisplayCrop;
pub use pf_bitstream::h264::H264Planner;
pub use pf_bitstream::h264::PlanError;
pub use pf_bitstream::h264::PlanWarning;
pub use pf_bitstream::h265::AuPlan as AuPlanH265;
pub use pf_bitstream::h265::H265Planner;
pub use pf_bitstream::h265::PlanError as PlanErrorH265;
pub use pf_bitstream::h265::PlanWarning as PlanWarningH265;
/// Which warnings mean the PICTURE is damaged — pf-vkdecode's one list, so all three
/// native rungs conceal on exactly the same predicate.
pub use pf_vkdecode::is_integrity_warning;
pub use pf_vkdecode::is_integrity_warning_av1;
pub use pf_vkdecode::is_integrity_warning_h265;

pub use drm::flatten;
pub use drm::ExportError;
pub use drm::ExportedPlane;
pub use drm::ExportedSurface;
pub use drm::VaDrmPrimeSurfaceDescriptor;
pub use drm::VA_EXPORT_SURFACE_READ_ONLY;
pub use drm::VA_EXPORT_SURFACE_SEPARATE_LAYERS;
pub use drm::VA_FOURCC_NV12;
pub use drm::VA_FOURCC_P010;

pub use config::profile_for;
pub use config::rt_format;
pub use config::surface_count;
pub use config::Codec;
pub use config::ConfigError;
pub use config::VaProfile;
pub use config::AV1_MAX_DPB_FRAMES;
pub use config::VA_ENTRYPOINT_VLD;
pub use pic::plan_to_va;
pub use pic::DecodePlanVa;
pub use pic::PlanToVaError;
pub use pic_av1::plan_to_va_av1;
pub use pic_av1::DecodePlanVaAv1;
pub use pic_av1::PlanToVaAv1Error;
pub use pic_av1::TileGroupVa;
pub use pic_h265::plan_to_va_h265;
pub use pic_h265::DecodePlanVaH265;
pub use pic_h265::PlanToVaH265Error;
pub use va::PicFieldsH264;
pub use va::SeqFieldsH264;
pub use va::VaIqMatrixBufferH264;
pub use va::VaPictureH264;
pub use va::VaPictureParameterBufferH264;
pub use va::VaSliceParameterBufferH264;
