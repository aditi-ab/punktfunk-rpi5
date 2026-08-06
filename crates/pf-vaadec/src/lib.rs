//! Native VAAPI decode for the Linux clients — M6 of the native-decode program, and
//! the VAAPI counterpart of [`pf_vkdecode`] and `pf-dxvadec`.
//!
//! Like `pf-dxvadec`, this crate is the **CPU-testable half**: everything between
//! pf-bitstream's per-AU plan and the buffers a `vaRenderPicture` call delivers. It
//! links no libva, names no `VA*` handle type, and compiles on macOS and in the Linux
//! container — which is the point. The VAAPI rung itself is
//! `cfg(target_os = "linux")` code that only a box can build, so anything left inside
//! that boundary is verified by a remote `cargo check` and nothing more.
//!
//! - [`va`]: the libva decode buffer layouts, **hand-declared**, with every size and
//!   offset measured off the real headers and pinned as compile-time assertions.
//! - [`config`]: profile, render-target format and surface-count decisions.
//! - [`pic`]: one `AuPlan` into picture parameters, IQ matrices and slice records.
//!
//! # Status
//!
//! **H.264 conversion complete; no libva calls yet.** What remains for the rung is
//! the Linux-only plumbing (config/context/surface creation, `vaRenderPicture`,
//! sync, dmabuf export) and the H.265 twin of [`pic`].
//!
//! Four things this crate settled that a reader would otherwise have to re-derive:
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
pub mod pic;
pub mod pic_h265;
pub mod va;
pub mod va_h265;

/// The DPB slot ledger — borrowed, not redefined (crate docs).
pub use pf_vkdecode::SlotError;
pub use pf_vkdecode::SlotMap;

/// The planners and plans this crate converts, re-exported so the Linux layer names
/// every type it touches through `pf_vaadec` — the same courtesy `pf-dxvadec` does
/// for the Windows layer.
pub use pf_bitstream::h264::AuPlan;
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
pub use pf_vkdecode::is_integrity_warning_h265;

pub use config::profile_for;
pub use config::rt_format;
pub use config::surface_count;
pub use config::Codec;
pub use config::ConfigError;
pub use config::VaProfile;
pub use config::VA_ENTRYPOINT_VLD;
pub use pic::plan_to_va;
pub use pic::DecodePlanVa;
pub use pic::PlanToVaError;
pub use pic_h265::plan_to_va_h265;
pub use pic_h265::DecodePlanVaH265;
pub use pic_h265::PlanToVaH265Error;
pub use va::PicFieldsH264;
pub use va::SeqFieldsH264;
pub use va::VaIqMatrixBufferH264;
pub use va::VaPictureH264;
pub use va::VaPictureParameterBufferH264;
pub use va::VaSliceParameterBufferH264;
