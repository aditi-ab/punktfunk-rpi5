//! Shared Vulkan Video bring-up for the `#[ignore]`d GPU legs.
//!
//! `tests/gpu_smoke.rs` and `tests/gpu_parity.rs` each drive THREE codecs, and the
//! path from "a Vulkan loader exists" to "a [`DeviceHandles`] a decoder can be
//! constructed on" is the same ~150 unsafe lines every time: loader → instance →
//! pick a physical device whose queue families carry the codec's decode ops →
//! logical device with the decode extensions plus `timelineSemaphore` and
//! `synchronization2`. Six copies of that would be six places for a
//! fleet-only failure to hide, so it lives here once, parameterised by the one
//! thing that genuinely differs between the callers ([`Graphics`]: the parity
//! legs read back on a graphics queue and so REQUIRE one, while the smoke legs
//! accept a decode-only device and fall back to the decode family — which also
//! decides whether pool images end up EXCLUSIVE or CONCURRENT, so it is not
//! cosmetic). [`Request::report_families`] exists so a caller CAN suppress the
//! per-family table, but every leg currently asks for it: it is the first
//! thing a fleet failure report needs, and it is a physical-device property
//! query, never a recorded RESULT_STATUS query, so it cannot trip the RADV VCN
//! hang.
//!
//! Cargo does not treat `tests/common/mod.rs` as a test target of its own (it
//! auto-discovers `tests/*.rs` and `tests/*/main.rs` only), so this file is
//! compiled purely as a `mod common;` of each test binary — and therefore under
//! each one's `#![deny(clippy::undocumented_unsafe_blocks)]`.
//!
//! Environment knobs, honoured exactly as they were before this module existed:
//! - `PF_VKD_SMOKE_VENDOR` (hex `0x1002`/`0x10de`, or decimal): pin a PCI vendor
//!   on a multi-GPU box, so a run is attributable to one driver instead of
//!   whichever device enumerated first.
//! - RADV additionally needs `RADV_PERFTEST=video_decode` in the environment;
//!   without it no device advertises the decode extensions and [`bring_up`]
//!   panics with "no physical device with VK_KHR_video_decode_*", which is the
//!   correct report rather than a confusing later failure.

// Each of the two test binaries drives a different subset of this module (the
// smoke legs never touch `Setup::pd`, the parity legs never pass
// `Graphics::DecodeFamilyIsFine`), and a test binary gets no `pub` exemption
// from dead-code analysis.
#![allow(dead_code)]

use std::io::Cursor;

use ash::vk;
use ash::vk::Handle;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DecodedVkFrame;
use pf_vkdecode::DeviceHandles;
use pf_vkdecode::VkDecodeError;

/// The vendored H.264 vector both GPU legs decode: 250 AUs of real encoder
/// output, 320x240 — the same file pf-bitstream's WP-A tests plan, at the same
/// relative path. It is **two slice NALUs per picture** (500 slice NALs over 250
/// AUs, with 4 IDRs), which is why the splitter's `first_mb_in_slice == 0` branch
/// is load-bearing here rather than decorative — and, per libavcodec's own DXVA
/// slice-control descriptors captured on hardware, why its slice-control buffer
/// is two records wide where the HEVC vector's is one.
pub const TEST_25FPS_H264: &[u8] = include_bytes!(
    "../../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
);

/// The vendored H.265 twin: 250 AUs, 320x240 Main 8-bit 4:2:0, one IDR_N_LP then
/// 249 TRAIL pictures (verified by pf-bitstream's
/// `the_full_25fps_vector_plans_every_picture_and_every_pic_id_reaches_output`).
pub const TEST_25FPS_H265: &[u8] = include_bytes!(
    "../../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
);

/// The vendored AV1 twin: 320x240 Main 4:2:0 8-bit, no film grain, **250 temporal
/// units carrying 274 coded frames** — 24 units carry two frames each, and those 24
/// extras are HIDDEN (decoded, referenced, never shown; the vector contains no
/// `show_existing_frame` at all). 250 frames are displayed, which is what the rung
/// delivers and what `data/test-25fps-av1.nv12.sha256` hashes.
///
/// Note the file name: it is `test-25fps.ivf.av1`, not `test-25fps.av1.ivf` — the
/// directory holds BOTH, byte-identical, and only the former has the `.md5`/`.crc`
/// reference hashes beside it. pf-bitstream's planner tests include this one.
pub const TEST_25FPS_AV1: &[u8] = include_bytes!(
    "../../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
);

/// Split the AV1 vector into access units — one IVF packet per TEMPORAL UNIT.
///
/// Unlike the two Annex-B splitters below there is nothing to re-derive here: AV1
/// carries no start codes, so an access unit is not something a scan recovers from
/// the elementary stream — it is the container's framing, and the vector's container
/// is IVF. `IvfIterator` is the vendored parser's own reader (the same one
/// pf-bitstream's AV1 planner tests walk), so this is a rename for symmetry with
/// [`split_h264_aus`]/[`split_h265_aus`] rather than a second implementation that
/// could disagree with production.
///
/// The consequence for the parity legs is worth stating: AV1 has **no prefix-width
/// leg** and needs none. The four-byte-start-code hazard that made HEVC unplayable
/// on every driver (see [`h265_four_byte_start_codes`]) simply has no AV1
/// counterpart — OBUs are length-delimited, the host hands whole temporal units
/// across, and there is no prefix for a driver to mis-skip. Its absence here is
/// deliberate, not an omission.
pub fn split_av1_aus(stream: &[u8]) -> Vec<&[u8]> {
    cros_codecs::bitstream_utils::IvfIterator::new(stream).collect()
}

/// Test-only H.264 AU splitter, mirroring pf-bitstream's
/// (`#[cfg(test)]`-private there): a new AU starts at a non-VCL NALU following
/// slices, or at a slice whose `first_mb_in_slice` is 0 (the first bit of the byte
/// after the 1-byte NAL header) when the current AU already has slices.
pub fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h264::parser::Nalu;
    use cros_codecs::codec::h264::parser::NaluType;

    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let nalu_offset = cursor.position() as usize;
        let start = nalu_offset - nalu.offset;
        let is_slice = matches!(nalu.header.type_, NaluType::Slice | NaluType::SliceIdr);
        let first_mb_zero = is_slice && stream.get(nalu_offset + 1).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_mb_zero) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// Test-only H.265 AU splitter, a verbatim copy of pf-bitstream's
/// (`#[cfg(test)]`-private in `h265.rs`, and the same one `pic_h265`'s and
/// `fault_detection`'s tests carry).
///
/// The two differences from [`split_h264_aus`] are the whole point and are why
/// this is copied rather than re-derived: HEVC's NAL header is TWO bytes, so
/// `first_slice_segment_in_pic_flag` is the top bit of `stream[header_start + 2]`
/// (H.264 reads `+ 1`), and "is a slice" is the numeric range `nal_unit_type < 32`
/// rather than an enum pair. Getting either wrong silently merges or splits AUs,
/// which shows up as a frame-count mismatch a long way from its cause.
pub fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
    use cros_codecs::codec::h265::parser::Nalu;

    let mut aus = Vec::new();
    let mut cursor = Cursor::new(stream);
    let mut au_start = 0usize;
    let mut au_has_slice = false;

    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let header_start = cursor.position() as usize;
        let start = header_start - nalu.offset;
        let is_slice = (nalu.header.type_ as u32) < 32;
        let first_slice_flag =
            is_slice && stream.get(header_start + 2).is_some_and(|b| b & 0x80 != 0);

        if au_has_slice && (!is_slice || first_slice_flag) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// Rewrite every Annex-B start code in `stream` to the FOUR-byte form
/// (`00 00 00 01`), leaving each NAL's payload bytes untouched.
///
/// # Why the suite needs this
///
/// Both vendored vectors use THREE-byte start codes throughout, while the real
/// host emits FOUR-byte ones on **100% of access units, both codecs** — measured
/// off the M0 NVENC corpus through the capture hook's `.idx` offsets: 1514/1514
/// H.264 AUs and 1133/1133 HEVC. So without this, every parity verdict this
/// program has recorded was taken on a prefix form that never ships.
///
/// That gap was not theoretical. Submitting four-byte start codes to
/// `vkCmdDecodeVideoKHR` is exactly what made HEVC unplayable on every driver
/// tested: drivers are validated on the three-byte form, and a fixed `+3 + 2`
/// skip into a four-byte-prefixed slice lands a byte early and reads a nonsense
/// `pps_id`. The cure lives in `ring::pack_slices`, which trims the leading zero
/// byte and derives the slice offsets from the trimmed lengths in one call.
/// H.264 was safe here only by its vendored encoder's convention, never by
/// structure — which is why the normalisation is shared and so is this helper.
///
/// # What it preserves
///
/// The payload copied is `nalu.data[nalu.offset..]`: exactly the `nal_size`
/// bytes the parser itself hands the planner. `Nalu::next` already discards
/// `trailing_zero_8bits` before the following start code, so the NALs in the
/// output are the NALs the production parser sees in the input, and the ONLY
/// difference between the two streams is the width of every prefix.
///
/// Generic over the NAL header because both codecs share one `Nalu` type. The
/// AU splitters above cannot be shared for the opposite reason: their AU
/// boundary rules genuinely differ.
fn four_byte_start_codes<H>(stream: &[u8]) -> Vec<u8>
where
    H: cros_codecs::codec::h264::nalu::Header + std::fmt::Debug,
{
    use cros_codecs::codec::h264::nalu::Nalu;

    // A lower bound, not the answer: the output gains a byte per three-byte
    // prefix and loses any trailing zeroes.
    let mut out = Vec::with_capacity(stream.len());
    let mut cursor = Cursor::new(stream);
    while let Ok(nalu) = Nalu::<H>::next(&mut cursor) {
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&nalu.data[nalu.offset..]);
    }
    out
}

/// [`four_byte_start_codes`] bound to H.264's NAL header.
pub fn h264_four_byte_start_codes(stream: &[u8]) -> Vec<u8> {
    four_byte_start_codes::<cros_codecs::codec::h264::parser::NaluHeader>(stream)
}

/// [`four_byte_start_codes`] bound to H.265's NAL header.
pub fn h265_four_byte_start_codes(stream: &[u8]) -> Vec<u8> {
    four_byte_start_codes::<cros_codecs::codec::h265::parser::NaluHeader>(stream)
}

/// The slice of a decoder's surface the GPU legs drive.
///
/// `VkH264Decoder`, `VkH265Decoder` and `VkAv1Decoder` expose it method-for-method
/// (the crate docs say so deliberately) but share no trait — codec DISPATCH is the
/// client wiring's job, not this crate's. Binding it here lets each GPU leg run ONE
/// body against all three codecs, which is the only way "the AV1 leg proves the same
/// thing the H.264 leg does" can be a fact instead of a claim about three hand-copied
/// functions.
pub trait TestDecoder {
    fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError>;
    fn take_ready(&mut self) -> Option<DecodedVkFrame>;
    fn wait_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus;
    fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presenter_signaled: bool,
    ) -> Result<(), VkDecodeError>;
    fn flush(&mut self);
    fn status_queries(&self) -> bool;
    fn debug_snapshot(&self) -> String;
}

/// Forwarding impl — one macro so the three decoders can never drift into being
/// driven differently by accident.
macro_rules! impl_test_decoder {
    ($ty:ty) => {
        impl TestDecoder for $ty {
            fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
                <$ty>::decode(self, au)
            }
            fn take_ready(&mut self) -> Option<DecodedVkFrame> {
                <$ty>::take_ready(self)
            }
            fn wait_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
                <$ty>::wait_status(self, frame)
            }
            fn release_frame(
                &mut self,
                frame: &DecodedVkFrame,
                presenter_signaled: bool,
            ) -> Result<(), VkDecodeError> {
                <$ty>::release_frame(self, frame, presenter_signaled)
            }
            fn flush(&mut self) {
                <$ty>::flush(self);
            }
            fn status_queries(&self) -> bool {
                <$ty>::status_queries(self)
            }
            fn debug_snapshot(&self) -> String {
                <$ty>::debug_snapshot(self)
            }
        }
    };
}

impl_test_decoder!(pf_vkdecode::VkH264Decoder);
impl_test_decoder!(pf_vkdecode::VkH265Decoder);
impl_test_decoder!(pf_vkdecode::VkAv1Decoder);

/// Serializes the GPU legs within one test binary. Hold it for the whole leg.
///
/// Cargo runs a binary's tests on PARALLEL threads by default. While each GPU test
/// file held exactly one test that never mattered; with one leg per codec it
/// matters twice over:
///
/// - two decoders would contend for the same decode queue and double peak video
///   memory on a device that may not have it, turning an attribution run into a
///   race and any failure into something nobody can pin on a codec;
/// - the parity legs set `PF_VKD_TEST_READBACK` through `std::env::set_var`, which
///   is not thread-safe and would be racing a second leg's reads of it.
///
/// Poisoning is deliberately ignored: if the first leg panics, the second must
/// still run and report its own codec's verdict rather than fail as a casualty.
pub fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
    static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GPU.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Which codec a bring-up must find silicon (and an extension) for.
#[derive(Clone, Copy)]
pub struct Codec {
    /// The video codec operation the chosen decode queue family must advertise.
    /// Checked per FAMILY, not per device: a device can advertise the extension
    /// while only one of its families carries the op.
    pub op: vk::VideoCodecOperationFlagsKHR,
    /// The codec's device extension — required on the physical device AND enabled
    /// on the logical one, per [`DeviceHandles`]' contract (a decoder whose
    /// extension was not enabled reaches `vkCreateVideoSessionKHR` on an
    /// unenabled codec).
    pub extension: &'static std::ffi::CStr,
}

/// H.264 decode (`VkH264Decoder`).
pub const H264: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_H264,
    extension: ash::khr::video_decode_h264::NAME,
};

/// H.265 decode (`VkH265Decoder`).
pub const H265: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_H265,
    extension: ash::khr::video_decode_h265::NAME,
};

/// AV1 decode (`VkAv1Decoder`).
///
/// The narrowest of the three on the fleet: `VK_KHR_video_decode_av1` only reached
/// core drivers in 2024, so a box that decodes both H.26x codecs may still report
/// "no physical device with VK_KHR_video_decode_av1", which is a fact about the box.
/// On RADV the extension is additionally behind `RADV_PERFTEST=video_decode`, the
/// same knob the other two need.
pub const AV1: Codec = Codec {
    op: vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
    extension: ash::khr::video_decode_av1::NAME,
};

/// What a caller needs from the GRAPHICS queue family — the one behavioural
/// difference between the smoke and parity bring-ups, an explicit parameter so
/// it cannot drift back into being an accident of two copied loops.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Graphics {
    /// A device with no graphics family is still usable: `graphics_qf` falls back
    /// to the decode family. The smoke legs want this — they submit nothing
    /// outside the decoder. The fallback is not cosmetic: `decode_qf ==
    /// graphics_qf` makes the picture pool's images EXCLUSIVE rather than
    /// CONCURRENT (`DecodeDevice::sharing_families`), which is exactly the
    /// arrangement a decode-only device has to run.
    DecodeFamilyIsFine,
    /// A device without a graphics family is SKIPPED, not defaulted. The parity
    /// legs want this — their readback records `vkCmdCopyImageToBuffer` on the
    /// graphics queue.
    Required,
}

/// One bring-up request. A struct rather than positional arguments because the
/// two fields below are precisely where the callers disagree, and a bare
/// `bring_up(H265, true, false)` at a call site is how that disagreement becomes
/// invisible again.
pub struct Request {
    /// The codec whose decode ops and extension are required.
    pub codec: Codec,
    /// Whether a graphics queue family is required or merely preferred.
    pub graphics: Graphics,
    /// Print each candidate device's per-family `flags / video_ops /
    /// query_result_status` table. It is the first thing a fleet failure report
    /// needs: which families exist, which of them decode this codec, and whether
    /// per-op status verdicts exist on this box at all (RADV: they do not, and
    /// recording one hangs the VCN — the 2026-08 .25 lesson).
    pub report_families: bool,
}

/// A live instance + logical device a decoder can be constructed on.
///
/// Torn down explicitly through [`Setup::destroy`] rather than `Drop`, so the
/// ordering against the decoder — which must be gone FIRST — stays visible in the
/// test body, exactly as it was when each test carried its own teardown.
pub struct Setup {
    /// Kept because [`Setup::handles`] hands the loader's
    /// `vkGetInstanceProcAddr` to the decoder, which resolves everything through
    /// it.
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub pd: vk::PhysicalDevice,
    pub device: ash::Device,
    pub decode_qf: u32,
    /// The graphics family, or `decode_qf` under
    /// [`Graphics::DecodeFamilyIsFine`] when the device has none.
    pub graphics_qf: u32,
}

impl Setup {
    /// The borrowed-handle bundle both decoders are constructed from. Valid only
    /// while `self` is alive and un-destroyed ([`DeviceHandles`]' contract).
    pub fn handles(&self) -> DeviceHandles {
        DeviceHandles {
            get_instance_proc_addr: self.entry.static_fn().get_instance_proc_addr as usize,
            instance: self.instance.handle().as_raw() as usize,
            physical_device: self.pd.as_raw() as usize,
            device: self.device.handle().as_raw() as usize,
            decode_qf: self.decode_qf,
            decode_queue_index: 0,
            graphics_qf: self.graphics_qf,
        }
    }

    /// # Safety
    ///
    /// Every object created from this device — the decoder's session/pools, any
    /// readback handles — is already destroyed, and no [`DeviceHandles`] taken
    /// from [`Setup::handles`] is still in use.
    pub unsafe fn destroy(self) {
        // SAFETY: fn contract — nothing derived from these handles survives, so
        // the device can be destroyed and then the instance it came from.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        // Deliberately LEAK the loader. `ash::Entry` owns an `Arc<Library>`, so
        // dropping it `dlclose`/`FreeLibrary`s the Vulkan loader together with
        // every ICD and implicit layer. That was harmless while each test binary
        // held a single GPU leg (the unload was immediately followed by process
        // exit), but each binary now holds two legs serialized by `gpu_lock`, so
        // the second leg would re-`dlopen` a loader the first just tore down —
        // a documented way to fault inside `Entry::load` or to take a signal at
        // exit AFTER both legs reported ok, which reads as a decoder defect.
        // The process is about to end regardless, so leaking is free.
        std::mem::forget(self.entry);
    }
}

/// The optional `PF_VKD_SMOKE_VENDOR` pin: multi-GPU boxes enumerate several
/// decode-capable devices and first-match hides all but one — the pin makes a run
/// attributable to a specific vendor's driver.
fn vendor_pin() -> Option<u32> {
    std::env::var("PF_VKD_SMOKE_VENDOR").ok().map(|raw| {
        let trimmed = raw.trim();
        trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .map_or_else(|| trimmed.parse(), |hex| u32::from_str_radix(hex, 16))
            .unwrap_or_else(|_| panic!("PF_VKD_SMOKE_VENDOR is not a PCI vendor id: {raw:?}"))
    })
}

/// Loader → instance → a physical device that can decode `request.codec` →
/// logical device with the decode extensions and `timelineSemaphore` +
/// `synchronization2`.
///
/// Panics (the test harness's only failure channel) with the reason when the box
/// cannot host the request; the message names the codec's OWN extension, so a box
/// with H.264 silicon but no H.265 says exactly that rather than "no Vulkan
/// Video".
pub fn bring_up(request: &Request) -> Setup {
    // ---- instance ----
    // SAFETY: loads the system Vulkan loader; no Vulkan objects exist yet.
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader on this box");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 3, 0));
    let instance_ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info rooted in locals; the instance is destroyed by
    // `Setup::destroy` after everything created from it.
    let instance =
        unsafe { entry.create_instance(&instance_ci, None) }.expect("create a Vulkan 1.3 instance");

    let vendor_filter = vendor_pin();

    // ---- physical device with a decode queue family for this codec ----
    // SAFETY: live instance.
    let physical_devices =
        unsafe { instance.enumerate_physical_devices() }.expect("enumerate physical devices");
    let mut picked: Option<(vk::PhysicalDevice, u32, u32)> = None;
    for pd in physical_devices {
        // SAFETY: `pd` was just enumerated from this instance.
        let props = unsafe { instance.get_physical_device_properties(pd) };
        if vendor_filter.is_some_and(|vendor| props.vendor_id != vendor) {
            continue;
        }
        // SAFETY: `pd` was just enumerated from this instance.
        let ext_props =
            unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
        let has = |name: &std::ffi::CStr| {
            ext_props.iter().any(|e| {
                e.extension_name_as_c_str()
                    .is_ok_and(|extension| extension == name)
            })
        };
        if !(has(ash::khr::video_queue::NAME)
            && has(ash::khr::video_decode_queue::NAME)
            && has(request.codec.extension))
        {
            continue;
        }
        // SAFETY: live physical device; the two-call form fills the chained video
        // properties for each family.
        let family_count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
        let mut families: Vec<vk::QueueFamilyProperties2<'_>> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        // SAFETY: as above, arrays sized to the reported count.
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families) };
        let flags_per_family: Vec<vk::QueueFlags> = families
            .iter()
            .map(|f| f.queue_family_properties.queue_flags)
            .collect();
        drop(families); // release the &mut borrows so video_props is readable

        // Each family's video ops + RESULT_STATUS query support (see
        // `Request::report_families`) — printed per CANDIDATE device, so a
        // multi-GPU box reports every device it considered.
        if request.report_families {
            let mut status_props =
                vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); family_count];
            let mut families2: Vec<vk::QueueFamilyProperties2<'_>> = status_props
                .iter_mut()
                .map(|s| vk::QueueFamilyProperties2::default().push_next(s))
                .collect();
            // SAFETY: as the query above.
            unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut families2) };
            drop(families2);
            for (i, s) in status_props.iter().enumerate() {
                eprintln!(
                    "family {i}: flags={:?} video_ops={:?} query_result_status={}",
                    flags_per_family[i],
                    video_props[i].video_codec_operations,
                    s.query_result_status_support != vk::FALSE,
                );
            }
        }

        let mut decode_qf = None;
        let mut graphics_qf = None;
        for (index, flags) in flags_per_family.iter().enumerate() {
            if flags.contains(vk::QueueFlags::GRAPHICS) && graphics_qf.is_none() {
                graphics_qf = Some(index as u32);
            }
            if flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR)
                && video_props[index]
                    .video_codec_operations
                    .contains(request.codec.op)
                && decode_qf.is_none()
            {
                decode_qf = Some(index as u32);
            }
        }
        match (request.graphics, decode_qf, graphics_qf) {
            // A graphics queue is required and present.
            (Graphics::Required, Some(decode), Some(graphics)) => {
                picked = Some((pd, decode, graphics));
                break;
            }
            // Not required: fall back to the decode family (see the variant docs).
            (Graphics::DecodeFamilyIsFine, Some(decode), graphics) => {
                picked = Some((pd, decode, graphics.unwrap_or(decode)));
                break;
            }
            // No decode family for this codec, or none of the graphics kind
            // required — keep looking.
            (Graphics::Required, _, None) | (_, None, _) => {}
        }
    }
    let (pd, decode_qf, graphics_qf) = picked.unwrap_or_else(|| {
        panic!(
            "no physical device with {} and a decode queue{}{}",
            request.codec.extension.to_string_lossy(),
            match request.graphics {
                Graphics::Required => " and a graphics queue",
                Graphics::DecodeFamilyIsFine => "",
            },
            match vendor_filter {
                Some(vendor) => format!(" (PF_VKD_SMOKE_VENDOR pinned vendor 0x{vendor:04x})"),
                None => String::new(),
            },
        )
    });

    // Attribution header: which device (and driver) this run actually exercised.
    {
        let mut driver_props = vk::PhysicalDeviceDriverProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver_props);
        // SAFETY: live physical device; the chain fills the Vulkan 1.2 core
        // driver-identity struct.
        unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
        let props = props2.properties;
        eprintln!(
            "picked: {:?} vendor=0x{:04x} driver={:?} info={:?}",
            props.device_name_as_c_str().unwrap_or(c"?"),
            props.vendor_id,
            driver_props.driver_name_as_c_str().unwrap_or(c"?"),
            driver_props.driver_info_as_c_str().unwrap_or(c"?"),
        );
    }

    // ---- logical device: decode (+ graphics) queues, video + sync features ----
    let priorities = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(decode_qf)
        .queue_priorities(&priorities)];
    if graphics_qf != decode_qf {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(graphics_qf)
                .queue_priorities(&priorities),
        );
    }
    let extensions = [
        ash::khr::video_queue::NAME.as_ptr(),
        ash::khr::video_decode_queue::NAME.as_ptr(),
        request.codec.extension.as_ptr(),
    ];
    let mut features12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let mut features13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
    let device_ci = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_infos)
        .enabled_extension_names(&extensions)
        .push_next(&mut features12)
        .push_next(&mut features13);
    // SAFETY: live physical device, valid create info rooted in locals; destroyed
    // by `Setup::destroy` after the decoder drops.
    let device =
        unsafe { instance.create_device(pd, &device_ci, None) }.expect("create the decode device");

    Setup {
        entry,
        instance,
        pd,
        device,
        decode_qf,
        graphics_qf,
    }
}
