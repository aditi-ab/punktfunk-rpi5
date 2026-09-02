//! AMD **AMF** hardware encoder (Windows, D3D11 input). Direct-SDK analogue of [`super::nvenc`].
//!
//! Drives the AMF **C vtable ABI** (GPUOpen headers; FFmpeg's `amfenc.c` uses the same surface,
//! not the C++ classes). FFI is header **v1.4.36**; load accepts runtimes down to **v1.4.34**
//! ([`sys::AMF_MIN_VERSION`]). Newer encoder features are string-keyed properties that degrade
//! per driver, not vtable changes. Loads `amfrt64.dll` at runtime — no build feature. Missing or
//! old runtime fails [`AmfEncoder::open`] and the session.
//!
//! Input is a same-device D3D11 NV12/P010 texture ring: `CopySubresourceRegion` then
//! `CreateSurfaceFromDX11Native`. No readback: Bgra/Rgb10a2 or CPU frames fail open/submit.
//! VCN does not encode 4:4:4. Evidence: `design/native-amf-encoder.md`.

// `unsafe_op_in_unsafe_fn` is off here: the body is raw AMF vtable calls. Clearing it means
// deleting markers that carry no caller contract, not wrapping each call in `unsafe {}`.
#![allow(unsafe_op_in_unsafe_fn)]

use super::policy::{intra_refresh_period, intra_refresh_requested, ltr_test_force_at};
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

// FFI vtable mirror in `amf_sys.rs` (no policy). `#[path]` keeps the `sys::` name at call sites.
#[path = "amf_sys.rs"]
mod sys;

use sys::{result_name, AmfVariant};

fn amf_ok(r: sys::AmfResult, what: &str) -> Result<()> {
    if r == sys::AMF_OK {
        Ok(())
    } else {
        Err(anyhow!("{what}: {} ({r})", result_name(r)))
    }
}

/// `AMF_FULL_VERSION` as `major.minor.patch` — the build nibble is unused here.
fn amf_version_str(v: u64) -> String {
    format!(
        "{}.{}.{}",
        (v >> 48) & 0xffff,
        (v >> 32) & 0xffff,
        (v >> 16) & 0xffff
    )
}

/// Path + file-version of the loaded `amfrt64.dll`. File-version is the driver build, not the AMF
/// runtime version — a stale System32 copy can lag the display driver. Diagnostics only.
///
/// # Safety
/// `module` must be a live handle the caller owns (the never-unloaded `amfrt64.dll`).
unsafe fn loaded_dll_identity(module: HMODULE) -> (Option<String>, Option<String>) {
    let mut buf = [0u16; 512];
    let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
    // n == 0 failed; n >= len truncated (no guaranteed NUL). Otherwise `buf[n]` is the terminator.
    if n == 0 || n >= buf.len() {
        return (None, None);
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    (Some(path), dll_file_version(PCWSTR(buf.as_ptr())))
}

/// `VS_FIXEDFILEINFO` file version as `a.b.c.d`. `None` if the resource is missing.
///
/// # Safety
/// `path` is a valid NUL-terminated wide string to a readable file.
unsafe fn dll_file_version(path: PCWSTR) -> Option<String> {
    let size = GetFileVersionInfoSizeW(path, None);
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    GetFileVersionInfoW(path, None, size, block.as_mut_ptr() as *mut c_void).ok()?;
    let mut value: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;
    let ok = VerQueryValueW(
        block.as_ptr() as *const c_void,
        w!("\\"),
        &mut value,
        &mut len,
    );
    if !ok.as_bool() || value.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: on success `VerQueryValueW` points `value` at a `VS_FIXEDFILEINFO` living inside
    // `block` and valid for `len` bytes (checked >= its size); `block` outlives this read.
    let ffi = &*(value as *const VS_FIXEDFILEINFO);
    let (ms, ls) = (ffi.dwFileVersionMS, ffi.dwFileVersionLS);
    Some(format!(
        "{}.{}.{}.{}",
        ms >> 16,
        ms & 0xffff,
        ls >> 16,
        ls & 0xffff
    ))
}

// Runtime loader: resolve `amfrt64.dll` once, gate on the ABI floor, keep the factory forever.

struct AmfLib {
    factory: *mut sys::AmfFactory,
    version: u64,
}
// SAFETY: `factory` is the process-global AMFInit singleton; AMF documents factory creation as
// thread-safe, the DLL is never unloaded, and this is only handed out as `&'static` from a
// `OnceLock` — no interior mutation on the Rust side.
unsafe impl Send for AmfLib {}
// SAFETY: shared refs only read the two plain fields; mutation is inside the thread-safe runtime.
unsafe impl Sync for AmfLib {}

/// Resolve the AMF runtime once per process. `Err` = no `amfrt64.dll` or older than
/// [`sys::AMF_MIN_VERSION`] — callers fail open with "update the AMD driver".
fn try_factory() -> std::result::Result<&'static AmfLib, &'static str> {
    static LIB: std::sync::OnceLock<std::result::Result<AmfLib, String>> =
        std::sync::OnceLock::new();
    LIB.get_or_init(|| {
        let lib = load_factory();
        if let Err(e) = &lib {
            tracing::warn!(error = %e, "native AMF runtime unavailable");
        }
        lib
    })
    .as_ref()
    .map_err(|e| e.as_str())
}

fn load_factory() -> std::result::Result<AmfLib, String> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names; SYSTEM32-only
    // search keeps a planted DLL out of the SYSTEM-service process. Transmutes match
    // `AMFQueryVersion_Fn`/`AMFInit_Fn` (core/Factory.h). `AMFQueryVersion` writes one u64;
    // `AMFInit` is passed min(header, runtime) and fills `factory` only on AMF_OK (null-checked).
    // The module is never freed, so factory and entry points live for the process.
    unsafe {
        let module = LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| {
                format!("amfrt64.dll not loadable (install/update the AMD driver): {e}")
            })?;
        let query_version = GetProcAddress(module, s!("AMFQueryVersion"))
            .ok_or("amfrt64.dll exports no AMFQueryVersion")?;
        let init = GetProcAddress(module, s!("AMFInit")).ok_or("amfrt64.dll exports no AMFInit")?;
        let query_version: sys::AmfQueryVersionFn = std::mem::transmute(query_version);
        let init: sys::AmfInitFn = std::mem::transmute(init);

        let mut version = 0u64;
        let r = query_version(&mut version);
        if r != sys::AMF_OK {
            return Err(format!("AMFQueryVersion failed: {} ({r})", result_name(r)));
        }
        // Path + file version of the System32 DLL actually loaded — a stale copy can lag the driver.
        let (dll_path, dll_file_ver) = loaded_dll_identity(module);
        let dll_desc = format!(
            "{}{}",
            dll_path.as_deref().unwrap_or("amfrt64.dll"),
            dll_file_ver
                .as_deref()
                .map(|v| format!(" (file version {v})"))
                .unwrap_or_default(),
        );
        // Below AMF_MIN_VERSION the mirrored vtable is not guaranteed — decline, never UB. Newer
        // encoder features are string properties (`set_prop(required=false)`), not vtable slots.
        if version < sys::AMF_MIN_VERSION {
            return Err(format!(
                "AMF runtime {amf} (loaded from {dll_desc}) is older than the minimum supported \
                 1.4.34 — update the AMD driver (Adrenalin 24.6.1+; 25.1.1+ for the \
                 fully-validated feature set). If the display driver already reports a newer \
                 version, this amfrt64.dll did not update — reboot, then DDU + reinstall so \
                 System32's copy is refreshed.",
                amf = amf_version_str(version),
            ));
        }
        // Never pass a version newer than the runtime: AMFInit can reject an otherwise-usable driver.
        let init_version = sys::AMF_HEADER_VERSION.min(version);
        let mut factory: *mut sys::AmfFactory = ptr::null_mut();
        let r = init(init_version, &mut factory);
        if r != sys::AMF_OK {
            return Err(format!("AMFInit failed: {} ({r})", result_name(r)));
        }
        if factory.is_null() {
            return Err("AMFInit returned a null factory".into());
        }
        if version >= sys::AMF_HEADER_VERSION {
            tracing::info!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime loaded (meets the validated 1.4.36 baseline)"
            );
        } else {
            tracing::warn!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime is older than the validated 1.4.36 baseline — accepted (the core \
                 encode ABI is stable), but advanced features (LTR / intra-refresh recovery, AV1 \
                 coded-size alignment, in-band HDR metadata) validated on 1.4.36 may be \
                 unavailable on this driver and will degrade individually (see the per-property \
                 logs below). Update to AMD Adrenalin 25.1.1+ for the fully-validated path."
            );
        }
        Ok(AmfLib { factory, version })
    }
}

// Per-codec property names (v1.4.36 headers). Unknown names use `set_prop(required=false)`.
// Enum VALUES differ: CBR is 1 on AVC, 3 on HEVC/AV1; SPEED is 1 / 10 / 100; AV1 swaps
// ULTRA_LOW_LATENCY/LOW_LATENCY relative to AVC/HEVC.

/// `AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED`.
const HEVC_HEADER_IDR_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED`.
const AV1_HEADER_KEY_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10`.
const HEVC_PROFILE_MAIN_10: i64 = 2;
/// `AMF_COLOR_BIT_DEPTH_10` (components/ColorSpace.h).
const COLOR_BIT_DEPTH_10: i64 = 10;
/// `AMF_VIDEO_ENCODER_AV1_ALIGNMENT_MODE_NO_RESTRICTIONS` / `_64X16_1080P_CODED_1082`.
/// Driver default `64X16_ONLY` rejects heights that are not multiples of 16 (1080p).
const AV1_ALIGNMENT_NO_RESTRICTIONS: i64 = 3;
const AV1_ALIGNMENT_1080P_CODED_1082: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_ENCODING_LATENCY_MODE_LOWEST_LATENCY`.
const AV1_LATENCY_LOWEST: i64 = 3;
// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM` (components/ColorSpace.h): studio-range 709 / 2020.
const COLOR_PROFILE_709: i64 = 1;
const COLOR_PROFILE_2020: i64 = 2;
// `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM` / `AMF_COLOR_PRIMARIES_ENUM` (CICP code points).
const TRANSFER_BT709: i64 = 1;
const TRANSFER_SMPTE2084: i64 = 16;
const PRIMARIES_BT709: i64 = 1;
const PRIMARIES_BT2020: i64 = 9;

struct CodecProps {
    /// `factory->CreateComponent` id.
    component: PCWSTR,
    usage: PCWSTR,
    rc_method: PCWSTR,
    /// `RATE_CONTROL_METHOD_CBR` — 1 on AVC, **3** on HEVC and AV1.
    rc_cbr: i64,
    target_bitrate: PCWSTR,
    peak_bitrate: PCWSTR,
    vbv_size: PCWSTR,
    enforce_hrd: PCWSTR,
    filler_data: PCWSTR,
    quality_preset: PCWSTR,
    /// `QUALITY_PRESET_SPEED` — 1 on AVC, **10** on HEVC, **100** on AV1.
    quality_speed: i64,
    /// AVC/HEVC: `L"LowLatencyInternal"` (bool). AV1: `Av1EncodingLatencyMode` (enum).
    lowlatency: PCWSTR,
    /// Bool `true` (AVC/HEVC) or the AV1 latency-mode enum value.
    lowlatency_value: AmfVariantKind,
    framerate: PCWSTR,
    /// AVC `IDRPeriod`, HEVC `HevcGOPSize`, AV1 `Av1GOPSize`. Value is `i32::MAX` (infinite GOP)
    /// except AV1, whose header defines **0** as "key frame at first frame only".
    idr_period: PCWSTR,
    idr_period_value: i64,
    /// Per-surface forced-keyframe: 2 = PICTURE_TYPE_IDR (AVC/HEVC), **1** = KEY (AV1).
    force_picture_type: PCWSTR,
    force_idr_value: i64,
    /// Output `*_OUTPUT_DATA_TYPE_*` / `Av1OutputFrameType`. Type ≤ `output_key_max` is a
    /// keyframe. AV1 INTRA_ONLY=1 does not reset references — not a join point.
    output_data_type: PCWSTR,
    output_key_max: i64,
    out_color_profile: PCWSTR,
    out_transfer: PCWSTR,
    out_primaries: PCWSTR,
    /// `*InHDRMetadata` (`AMFBuffer` of [`sys::AmfHdrMetadata`]). `None` on AVC — no HDR on the wire.
    hdr_metadata: Option<PCWSTR>,
    /// Intra-refresh: (units-per-slot, block edge px). AVC 16-px MBs, HEVC 64-px CTBs. `None` on
    /// AV1 (mode enum only, no slot-size control).
    intra_refresh: Option<(PCWSTR, u32)>,
    /// LTR-RFI property names. `None` on AV1 — this path does not drive its frame-marking OBU.
    ltr: Option<LtrProps>,
}

/// AMF LTR property names, codec-prefixed (AVC bare, HEVC `Hevc*`). Two static at open, two
/// per-frame on the input surface.
struct LtrProps {
    /// `MaxOfLTRFrames` — user LTR slots (we request [`NUM_LTR_SLOTS`]).
    max_ltr_frames: PCWSTR,
    /// `MaxNumRefFrames` — reference-picture budget; must exceed 1 for LTR to engage.
    max_num_ref_frames: PCWSTR,
    /// `MarkCurrentWithLTRIndex` — tag this frame as long-term reference slot N.
    mark_ltr_index: PCWSTR,
    /// `ForceLTRReferenceBitfield` — reference only LTR slots in the bitfield (`1<<N`).
    force_ltr_bitfield: PCWSTR,
}

enum AmfVariantKind {
    Bool(bool),
    I64(i64),
}

impl AmfVariantKind {
    fn to_variant(&self) -> AmfVariant {
        match self {
            AmfVariantKind::Bool(b) => AmfVariant::from_bool(*b),
            AmfVariantKind::I64(v) => AmfVariant::from_i64(*v),
        }
    }
}

fn codec_props(codec: Codec) -> CodecProps {
    match codec {
        Codec::H264 => CodecProps {
            component: w!("AMFVideoEncoderVCE_AVC"),
            usage: w!("Usage"),
            rc_method: w!("RateControlMethod"),
            rc_cbr: 1,
            target_bitrate: w!("TargetBitrate"),
            peak_bitrate: w!("PeakBitrate"),
            vbv_size: w!("VBVBufferSize"),
            enforce_hrd: w!("EnforceHRD"),
            filler_data: w!("FillerDataEnable"),
            quality_preset: w!("QualityPreset"),
            quality_speed: 1,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("FrameRate"),
            idr_period: w!("IDRPeriod"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("ForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("OutputDataType"),
            output_key_max: 1,
            out_color_profile: w!("OutColorProfile"),
            out_transfer: w!("OutColorTransferChar"),
            out_primaries: w!("OutColorPrimaries"),
            hdr_metadata: None,
            intra_refresh: Some((w!("IntraRefreshMBsNumberPerSlot"), 16)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("MaxOfLTRFrames"),
                max_num_ref_frames: w!("MaxNumRefFrames"),
                mark_ltr_index: w!("MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("ForceLTRReferenceBitfield"),
            }),
        },
        Codec::H265 => CodecProps {
            component: w!("AMFVideoEncoderHW_HEVC"),
            usage: w!("HevcUsage"),
            rc_method: w!("HevcRateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("HevcTargetBitrate"),
            peak_bitrate: w!("HevcPeakBitrate"),
            vbv_size: w!("HevcVBVBufferSize"),
            enforce_hrd: w!("HevcEnforceHRD"),
            filler_data: w!("HevcFillerDataEnable"),
            quality_preset: w!("HevcQualityPreset"),
            quality_speed: 10,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("HevcFrameRate"),
            idr_period: w!("HevcGOPSize"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("HevcForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("HevcOutputDataType"),
            output_key_max: 1,
            out_color_profile: w!("HevcOutColorProfile"),
            out_transfer: w!("HevcOutColorTransferChar"),
            out_primaries: w!("HevcOutColorPrimaries"),
            hdr_metadata: Some(w!("HevcInHDRMetadata")),
            intra_refresh: Some((w!("HevcIntraRefreshCTBsNumberPerSlot"), 64)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("HevcMaxOfLTRFrames"),
                max_num_ref_frames: w!("HevcMaxNumRefFrames"),
                mark_ltr_index: w!("HevcMarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("HevcForceLTRReferenceBitfield"),
            }),
        },
        Codec::Av1 => CodecProps {
            component: w!("AMFVideoEncoderHW_AV1"),
            usage: w!("Av1Usage"),
            rc_method: w!("Av1RateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("Av1TargetBitrate"),
            peak_bitrate: w!("Av1PeakBitrate"),
            vbv_size: w!("Av1VBVBufferSize"),
            enforce_hrd: w!("Av1EnforceHRD"),
            filler_data: w!("Av1FillerData"),
            quality_preset: w!("Av1QualityPreset"),
            quality_speed: 100,
            lowlatency: w!("Av1EncodingLatencyMode"),
            lowlatency_value: AmfVariantKind::I64(AV1_LATENCY_LOWEST),
            framerate: w!("Av1FrameRate"),
            idr_period: w!("Av1GOPSize"),
            idr_period_value: 0,
            force_picture_type: w!("Av1ForceFrameType"),
            force_idr_value: 1,
            output_data_type: w!("Av1OutputFrameType"),
            output_key_max: 0,
            out_color_profile: w!("Av1OutputColorProfile"),
            out_transfer: w!("Av1OutputColorTransferChar"),
            out_primaries: w!("Av1OutputColorPrimaries"),
            hdr_metadata: Some(w!("Av1InHDRMetadata")),
            intra_refresh: None,
            ltr: None,
        },
        Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
    }
}

/// `PUNKTFUNK_AMF_USAGE` → `*_USAGE_ENUM`. AVC/HEVC share numbering; **AV1 swaps
/// ULTRA_LOW_LATENCY (2) and LOW_LATENCY (1)** (VideoEncoderAV1.h). Unknown → ultralowlatency.
fn usage_from_env(codec: Codec) -> i64 {
    let av1 = codec == Codec::Av1;
    let ull = if av1 { 2 } else { 1 };
    let v = std::env::var("PUNKTFUNK_AMF_USAGE").unwrap_or_else(|_| "ultralowlatency".into());
    match v.as_str() {
        "ultralowlatency" => ull,
        "lowlatency" => {
            if av1 {
                1
            } else {
                2
            }
        }
        "lowlatency_high_quality" => 5,
        "transcoding" => 0,
        "highquality" | "high_quality" => 4,
        other => {
            tracing::warn!(
                usage = other,
                "unknown PUNKTFUNK_AMF_USAGE — using ultralowlatency"
            );
            ull
        }
    }
}

/// User LTR slots. AMD exposes 2; rotating them keeps a pair so a loss can re-reference the newest
/// mark *before* the loss point.
const NUM_LTR_SLOTS: usize = 2;

/// LTR loss recovery is on unless `PUNKTFUNK_NO_AMF_LTR=1`. AMF intra-refresh has no
/// constrained-intra property and is mutually exclusive with LTR, so LTR wins.
fn ltr_disabled() -> bool {
    super::policy::env_flag("PUNKTFUNK_NO_AMF_LTR")
}

/// Frames between LTR marks. Default `fps/2` (~0.5 s); [`NUM_LTR_SLOTS`] then covers ~1 s of
/// recent references. `PUNKTFUNK_LTR_INTERVAL_FRAMES` overrides.
fn ltr_mark_interval(fps: u32) -> i64 {
    super::policy::ltr_interval_env().unwrap_or_else(|| (fps.max(2) / 2).max(1) as i64)
}

// Owned-pointer guards: Terminate before Release (amfenc.c teardown order).

/// Owned `AMFComponent*` — `Terminate` + `Release` on drop.
struct Component(*mut sys::AmfComponent);
impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null `CreateComponent` pointer this guard uniquely owns;
        // vtable calls run on the owning thread. Flush-then-Terminate-then-Release; drop once.
        unsafe {
            // Flush before Terminate: an unflushed session can occupy AMD's limited VCN slots so
            // the next Init returns AMF_OK but never emits an AU. Best-effort on a wedge.
            ((*(*self.0).vtbl).flush)(self.0);
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF component Terminate returned non-OK on drop"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFContext*` — `Terminate` + `Release` on drop.
struct Ctx(*mut sys::AmfContext);
impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null `CreateContext` pointer this guard uniquely owns
        // (`Inner` declares `comp` before `ctx`, so components drop first). Drop once, owning thread.
        unsafe {
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF context Terminate returned non-OK on drop (D3D11 device unbind)"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFData*` (surface or buffer viewed through the `AMFData` prefix) — `Release` on drop.
struct OwnedData(*mut sys::AmfData);
impl Drop for OwnedData {
    fn drop(&mut self) {
        // SAFETY: one owned/AddRef'd reference from CreateSurface / QueryOutput / QueryInterface.
        // `release` is slot 2 of every AMF vtable. Drop once.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Set one component property. Required: abort the open. Optional: log and continue (VCN/driver
/// variance). Returns whether it applied, so callers gate advertised caps on the driver's answer.
unsafe fn set_prop(
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    value: AmfVariant,
    required: bool,
) -> Result<bool> {
    let r = ((*(*comp).vtbl).set_property)(comp, name.0, value);
    if r == sys::AMF_OK {
        return Ok(true);
    }
    let name = String::from_utf16_lossy(name.as_wide());
    if required {
        Err(anyhow!(
            "AMF SetProperty({name}) failed: {} ({r})",
            result_name(r)
        ))
    } else {
        // INFO not debug: rejected optional props are the per-box capability matrix.
        tracing::info!(
            property = %name,
            result = result_name(r),
            amf_code = r,
            "optional AMF encoder property rejected (VCN generation/driver) — continuing"
        );
        Ok(false)
    }
}

/// `GetProperty` INT64 after any internal clamp. `None` on decline or non-INT64 — never treat as 0.
unsafe fn get_prop_i64(comp: *mut sys::AmfComponent, name: PCWSTR) -> Option<i64> {
    let mut v = AmfVariant::zeroed();
    let r = ((*(*comp).vtbl).get_property)(comp, name.0, &mut v);
    if r != sys::AMF_OK {
        return None;
    }
    v.as_i64()
}

/// Input texture ring depth. AMF keeps reading a slot until its AU is retrieved, so at most
/// `RING - 1` frames may be in flight. `submit` drains before reuse. Shallow enough that
/// back-pressure starts after a few frames, not after AMF's 16-deep input queue.
const RING: usize = 6;

/// Process-wide count of successful `Init`s. A climbing number with no following first-AU log
/// ([`Inner::note_first_au`]) is a silent VCN-session wedge.
static AMF_CONTEXTS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Live AMF session. Field order: `comp` drops (Flush+Terminate+Release) before `ctx`.
struct Inner {
    comp: Component,
    ctx: Ctx,
    /// Capturer device — kept alive for the ring textures.
    _device: ID3D11Device,
    /// Immediate context for the ring copy (this encode thread only).
    dctx: ID3D11DeviceContext,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    /// (pts_ns, forced-IDR, recovery-anchor) FIFO. AMF emits in submit order (no B-frames).
    /// Length is surfaces AMF still holds; `submit` keeps it below [`RING`].
    pending: VecDeque<(u64, bool, bool)>,
    /// AUs `submit` already drained for back-pressure, older than anything in `pending`.
    ready: VecDeque<EncodedFrame>,
    /// Last `*InHDRMetadata` pushed to this component — re-push on change or rebuild.
    hdr_pushed: Option<punktfunk_core::quic::HdrMeta>,
    /// Gates the one-shot first-AU log. Absence after a context-created line is a VCN wedge.
    first_au_logged: bool,
}

impl Inner {
    /// One-shot first-AU log. Pairs a context-created line with proof VCN actually encodes.
    fn note_first_au(&mut self, au: &EncodedFrame) {
        if !self.first_au_logged {
            self.first_au_logged = true;
            tracing::info!(
                bytes = au.data.len(),
                keyframe = au.keyframe,
                "AMF produced its first AU on this context"
            );
        }
    }
}

pub struct AmfEncoder {
    codec: Codec,
    props: CodecProps,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// Lazy from the first frame's device; rebuilt on capturer-device change.
    inner: Option<Inner>,
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    /// Static HDR mastering metadata; pushed as `*InHDRMetadata` when it changes.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Driver accepted intra-refresh — gates [`EncoderCaps::intra_refresh`].
    ir_active: bool,
    /// Driver accepted LTR at open. Mutually exclusive with intra-refresh; LTR wins.
    ltr_active: bool,
    /// Wire `frame_idx` in each LTR slot (`None` = never marked). Newest pre-loss slot is forced.
    ltr_slots: [Option<i64>; NUM_LTR_SLOTS],
    /// Next LTR mark slot (round-robin).
    next_ltr_slot: usize,
    ltr_mark_interval: i64,
    /// LTR slot the next submit must force-reference. Consumed on that submit.
    pending_force: Option<usize>,
    /// `PUNKTFUNK_LTR_FORCE_AT=N`: self-trigger [`Encoder::invalidate_ref_frames`] at that index.
    ltr_test_force_at: Option<i64>,
    /// Resets with no AU since (cleared in `poll`). At 2, escalate past in-place re-Init: that
    /// reuses the same context and cannot clear a dead VCN session. Drop `inner` instead.
    resets_without_output: u32,
}

// SAFETY: raw AMF pointers and D3D11 COM handles are not auto-`Send`. The session moves the
// encoder onto one encode thread and drives it there; the immediate context is never shared.
unsafe impl Send for AmfEncoder {}

impl AmfEncoder {
    /// Open the native AMF encoder. Fails the session when the runtime is missing/too old or the
    /// capture format is not NV12/P010. AV1 is probed up front (RDNA3+; same [`probe_can_encode`]
    /// as the advertisement).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
    ) -> Result<Self> {
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        tracing::debug!(
            version = %amf_version_str(lib.version),
            "opening AMF encoder"
        );
        let props = codec_props(codec);
        // AV1 is RDNA3+ — probe here so a pre-RDNA3 box fails at open, not at lazy Init.
        if codec == Codec::Av1 && !probe_can_encode(Codec::Av1) {
            bail!("this GPU/driver declined AV1 encode (RDNA3+ required) — native AMF probe");
        }
        // Depth follows delivered pixels, not negotiated depth ([`crate::ten_bit_input`]).
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        // Ring is NV12/P010 only. Any other capture format has no native input path.
        let expected = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if format != expected {
            bail!(
                "native AMF needs the video-processor {expected:?} capture path; capturer \
                 delivered {format:?} (no readback path since Phase 3 — see the AMFVideoConverter \
                 note in §3.2)"
            );
        }
        if ten_bit && codec == Codec::H264 {
            bail!("native AMF: 10-bit is HEVC-only (H.264 High10 is not a VCN mode)");
        }
        // VCN does not encode 4:4:4. `can_encode_444` is already false; degrade, don't fail.
        if chroma.is_444() {
            tracing::warn!("AMF cannot encode 4:4:4 (VCN hardware limit) — encoding 4:2:0");
        }
        Ok(AmfEncoder {
            codec,
            props,
            width,
            height,
            fps,
            bitrate_bps,
            ten_bit,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            hdr_meta: None,
            ir_active: false,
            ltr_active: false,
            ltr_slots: [None; NUM_LTR_SLOTS],
            next_ltr_slot: 0,
            ltr_mark_interval: ltr_mark_interval(fps),
            pending_force: None,
            ltr_test_force_at: ltr_test_force_at(),
            resets_without_output: 0,
        })
    }

    /// Attempt LTR-RFI: AVC/HEVC only, unless `PUNKTFUNK_NO_AMF_LTR`. Driver accept is `ltr_active`.
    fn ltr_wanted(&self) -> bool {
        !ltr_disabled() && matches!(self.codec, Codec::H264 | Codec::H265)
    }

    /// VBV/HRD buffer (bits) at `bps`: ~1 frame interval, `PUNKTFUNK_VBV_FRAMES`-scaled.
    fn vbv_bits(&self, bps: u64) -> i64 {
        ((bps as f64 / self.fps.max(1) as f64) * crate::vbv_frames_env())
            .clamp(1.0, i32::MAX as f64) as i64
    }

    /// Static encoder config, before `Init` and again on `reset()` re-`Init` (Terminate does not
    /// keep properties on every driver). Returns `(ir_active, ltr_active)` as requested AND
    /// accepted. Mutually exclusive — LTR wins.
    unsafe fn apply_static_props(&self, comp: *mut sys::AmfComponent) -> Result<(bool, bool)> {
        let p = &self.props;
        // Usage first: it fully configures the parameter set; everything after is an override.
        set_prop(
            comp,
            p.usage,
            AmfVariant::from_i64(usage_from_env(self.codec)),
            true,
        )?;
        set_prop(comp, p.rc_method, AmfVariant::from_i64(p.rc_cbr), true)?;
        let bps = self.bitrate_bps.min(i64::MAX as u64) as i64;
        set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(
            comp,
            p.framerate,
            AmfVariant::from_rate(self.fps.max(1), 1),
            true,
        )?;
        set_prop(
            comp,
            p.vbv_size,
            AmfVariant::from_i64(self.vbv_bits(self.bitrate_bps)),
            false,
        )?;
        set_prop(comp, p.enforce_hrd, AmfVariant::from_bool(true), false)?;
        set_prop(comp, p.filler_data, AmfVariant::from_bool(false), false)?;
        // Latency-first quality; low-latency submit (optional on older VCN).
        set_prop(
            comp,
            p.quality_preset,
            AmfVariant::from_i64(p.quality_speed),
            false,
        )?;
        set_prop(comp, p.lowlatency, p.lowlatency_value.to_variant(), false)?;
        // No periodic IDR (`i32::MAX` AVC/HEVC; 0 on AV1 = first frame only). Forced type supplies IDRs.
        set_prop(
            comp,
            p.idr_period,
            AmfVariant::from_i64(p.idr_period_value),
            false,
        )?;
        // Intra-refresh: per-slot units = ceil(total blocks / period). Optional; gates `caps()`.
        let mut ir_active = false;
        let mut ltr_active = false;
        if let Some(ltr) = p.ltr.as_ref().filter(|_| self.ltr_wanted()) {
            // LTR needs >1 ref frames and is mutually exclusive with intra-refresh.
            let ref_ok = set_prop(
                comp,
                ltr.max_num_ref_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            let ltr_ok = set_prop(
                comp,
                ltr.max_ltr_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            ltr_active = ref_ok && ltr_ok;
            if ltr_active {
                tracing::info!(
                    slots = NUM_LTR_SLOTS,
                    mark_interval = self.ltr_mark_interval,
                    "AMF LTR-RFI recovery enabled (loss recovery re-references a known-good LTR, not a full IDR)"
                );
            } else {
                tracing::warn!(
                    ref_ok,
                    ltr_ok,
                    "this VCN/driver rejected an LTR property — loss recovery stays full-IDR"
                );
            }
        } else if let Some((name, block)) = p.intra_refresh {
            if intra_refresh_requested() {
                let period = intra_refresh_period(self.fps);
                let blocks = self.width.div_ceil(block) * self.height.div_ceil(block);
                let per_slot = blocks.div_ceil(period).max(1);
                ir_active = set_prop(comp, name, AmfVariant::from_i64(per_slot as i64), false)?;
                if ir_active {
                    tracing::info!(
                        period_frames = period,
                        units_per_slot = per_slot,
                        "AMF intra-refresh wave enabled (keyframe requests will be rate-limited)"
                    );
                } else {
                    tracing::warn!(
                        "PUNKTFUNK_INTRA_REFRESH requested but this VCN/driver rejected the \
                         intra-refresh property — loss recovery stays full-IDR"
                    );
                }
            }
        }
        match self.codec {
            Codec::H264 => {
                // Never B-frames: a full frame of latency each (RDNA3+ defaults > 0).
                set_prop(comp, w!("BPicturesPattern"), AmfVariant::from_i64(0), false)?;
                // Limited-range YUV (matches the video processor's NV12).
                set_prop(
                    comp,
                    w!("FullRangeColor"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
            }
            Codec::H265 => {
                // In-band VPS/SPS/PPS on every IDR. Forced-IDR surfaces also set `HevcInsertHeader`.
                set_prop(
                    comp,
                    w!("HevcHeaderInsertionMode"),
                    AmfVariant::from_i64(HEVC_HEADER_IDR_ALIGNED),
                    false,
                )?;
                // Studio range, matching NV12/P010 video-processor output.
                set_prop(comp, w!("HevcNominalRange"), AmfVariant::from_i64(0), false)?;
                if self.ten_bit {
                    // Main10 + 10-bit surfaces: required — silent 8-bit HDR is worse than failing open.
                    set_prop(
                        comp,
                        w!("HevcProfile"),
                        AmfVariant::from_i64(HEVC_PROFILE_MAIN_10),
                        true,
                    )?;
                    set_prop(
                        comp,
                        w!("HevcColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::Av1 => {
                // Never B-frames: VCN5 can grow them (H.264 already did on RDNA3+). A B-frame
                // adds a frame of latency and breaks FIFO on the codec with no LTR/IR. Pre-VCN5
                // rejects the names (no-op). HEVC has no B-frame property at all.
                set_prop(
                    comp,
                    w!("Av1BPicturesPattern"),
                    AmfVariant::from_i64(0),
                    false,
                )?;
                set_prop(
                    comp,
                    w!("Av1MaxConsecutiveBPictures"),
                    AmfVariant::from_i64(0),
                    false,
                )?;
                set_prop(
                    comp,
                    w!("Av1AdaptiveMiniGop"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
                // Sequence header OBU on every key frame (self-contained join points).
                set_prop(
                    comp,
                    w!("Av1HeaderInsertionMode"),
                    AmfVariant::from_i64(AV1_HEADER_KEY_ALIGNED),
                    false,
                )?;
                // Default `64X16_ONLY` rejects non-16-multiple heights (1080p). Prefer unrestricted;
                // fall back to 1080p-coded-1082. If neither applies, Init fails.
                let unrestricted = set_prop(
                    comp,
                    w!("Av1AlignmentMode"),
                    AmfVariant::from_i64(AV1_ALIGNMENT_NO_RESTRICTIONS),
                    false,
                )?;
                if !unrestricted && self.height % 16 != 0 {
                    set_prop(
                        comp,
                        w!("Av1AlignmentMode"),
                        AmfVariant::from_i64(AV1_ALIGNMENT_1080P_CODED_1082),
                        false,
                    )?;
                }
                if self.ten_bit {
                    // 10-bit is AV1 Main — only the surface depth needs forcing.
                    set_prop(
                        comp,
                        w!("Av1ColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
        }
        // BT.709 limited (SDR) or BT.2020 PQ (HDR). Required for HDR — missing PQ washes out.
        let (profile, transfer, primaries) = if self.ten_bit {
            (COLOR_PROFILE_2020, TRANSFER_SMPTE2084, PRIMARIES_BT2020)
        } else {
            (COLOR_PROFILE_709, TRANSFER_BT709, PRIMARIES_BT709)
        };
        set_prop(
            comp,
            p.out_color_profile,
            AmfVariant::from_i64(profile),
            self.ten_bit,
        )?;
        set_prop(
            comp,
            p.out_transfer,
            AmfVariant::from_i64(transfer),
            self.ten_bit,
        )?;
        set_prop(
            comp,
            p.out_primaries,
            AmfVariant::from_i64(primaries),
            self.ten_bit,
        )?;
        Ok((ir_active, ltr_active))
    }

    /// Build or rebuild the AMF context + component on the capturer's device, plus the input ring.
    fn ensure_inner(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        // SAFETY: `lib.factory` is live (gated above). CreateContext/CreateComponent fill
        // out-pointers only on AMF_OK (null-checked); each object moves into a guard so early
        // `?` releases once. `InitDX11` borrows a live `ID3D11Device`; AMF AddRefs until Terminate.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                "AMF CreateContext",
            )?;
            if ctx.is_null() {
                bail!("AMF CreateContext returned null");
            }
            let ctx = Ctx(ctx);
            amf_ok(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                "AMF InitDX11 (capturer device)",
            )?;
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    self.props.component.0,
                    &mut comp,
                ),
                "AMF CreateComponent",
            )?;
            if comp.is_null() {
                bail!("AMF CreateComponent returned null");
            }
            let comp = Component(comp);
            let (ir_active, ltr_active) = self.apply_static_props(comp.0)?;
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            amf_ok(
                ((*(*comp.0).vtbl).init)(comp.0, fmt, self.width as i32, self.height as i32),
                "AMF encoder Init",
            )?;
            self.ir_active = ir_active;
            // Rebuilt component has no reference history; drop prior LTR marks.
            self.ltr_active = ltr_active;
            if ltr_active {
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: if self.ten_bit {
                    DXGI_FORMAT_P010
                } else {
                    DXGI_FORMAT_NV12
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let mut t: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D (AMF input ring)")?;
                ring.push(t.context("AMF input ring texture")?);
            }
            let dctx = device
                .GetImmediateContext()
                .context("ID3D11Device immediate context")?;
            // Bump after successful Init so a failed bring-up never counts.
            let context_no =
                AMF_CONTEXTS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::info!(
                codec = ?self.codec,
                context = context_no,
                device = %format_args!("{:#x}", device.as_raw() as usize),
                width = self.width,
                height = self.height,
                fps = self.fps,
                ring = if self.ten_bit { "P010" } else { "NV12" },
                ltr = ltr_active,
                intra_refresh = ir_active,
                runtime = %format_args!(
                    "{}.{}.{}",
                    (lib.version >> 48) & 0xffff,
                    (lib.version >> 32) & 0xffff,
                    (lib.version >> 16) & 0xffff
                ),
                "native AMF encode active (zero-copy D3D11)"
            );
            self.inner = Some(Inner {
                comp,
                ctx,
                _device: device.clone(),
                dctx,
                ring,
                next: 0,
                pending: VecDeque::new(),
                ready: VecDeque::new(),
                hdr_pushed: None,
                first_au_logged: false,
            });
            Ok(())
        }
    }
}

/// Push HDR mastering metadata as `*InHDRMetadata` (dynamic). Units match [`HdrMeta`]; primary
/// order is the trap: ST.2086 wire is G,B,R → labeled R/G/B fields.
///
/// # Safety
/// `ctx` and `comp` are the live pair owned by the calling encoder, encode thread only.
unsafe fn push_hdr_metadata(
    ctx: *mut sys::AmfContext,
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    meta: &punktfunk_core::quic::HdrMeta,
) -> Result<()> {
    let mut buf: *mut sys::AmfBuffer = ptr::null_mut();
    amf_ok(
        ((*(*ctx).vtbl).alloc_buffer)(
            ctx,
            sys::AMF_MEMORY_HOST,
            std::mem::size_of::<sys::AmfHdrMetadata>(),
            &mut buf,
        ),
        "AMF AllocBuffer(HDR metadata)",
    )?;
    if buf.is_null() {
        bail!("AMF AllocBuffer(HDR metadata) returned null");
    }
    // AMFData-prefix guard (slot 2 is Release). SetProperty AddRefs; our drop leaves the property.
    let guard = OwnedData(buf as *mut sys::AmfData);
    let native = ((*(*buf).vtbl).get_native)(buf) as *mut sys::AmfHdrMetadata;
    if native.is_null() {
        bail!("AMF HDR metadata buffer has no host pointer");
    }
    // Host AMFBuffer heap alignment is unknown — write unaligned.
    native.write_unaligned(sys::AmfHdrMetadata {
        red_primary: meta.display_primaries[2],
        green_primary: meta.display_primaries[0],
        blue_primary: meta.display_primaries[1],
        white_point: meta.white_point,
        max_mastering_luminance: meta.max_display_mastering_luminance,
        min_mastering_luminance: meta.min_display_mastering_luminance,
        max_content_light_level: meta.max_cll,
        max_frame_average_light_level: meta.max_fall,
    });
    let r = ((*(*comp).vtbl).set_property)(
        comp,
        name.0,
        AmfVariant::from_interface(guard.0 as *mut c_void),
    );
    amf_ok(r, "AMF SetProperty(InHDRMetadata)")
}

/// Can this GPU's AMF runtime `Init` a `codec` encoder on the selected render adapter?
/// Tears down before return. `false` on any failure, including no runtime.
pub fn probe_can_encode(codec: Codec) -> bool {
    let Some(device) = selected_adapter_device() else {
        return false;
    };
    probe_can_encode_on(&device, codec)
}

/// [`probe_can_encode`] on an explicit device (live tests pin the AMD adapter on a hybrid box).
fn probe_can_encode_on(device: &ID3D11Device, codec: Codec) -> bool {
    probe_open_on(device, codec, false)
}

/// Can this GPU `Init` `codec` at 10-bit (Main10 / `*ColorBitDepth` 10, P010)? H.264 is always
/// false (High10 is not a VCN mode).
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    let Some(device) = selected_adapter_device() else {
        return false;
    };
    probe_open_on(&device, codec, true)
}

/// Probe body: context + component + usage + optional 10-bit props + tiny `Init`. `false` on fail.
fn probe_open_on(device: &ID3D11Device, codec: Codec, ten_bit: bool) -> bool {
    if try_factory().is_err() {
        return false;
    }
    let props = codec_props(codec);
    // SAFETY: factory is live; each created object moves into a guard so early return releases
    // once. `InitDX11` borrows `device`; AMF AddRefs until Terminate. Usage must be set before
    // `Init` (header default is N/A).
    unsafe {
        let Ok(lib) = try_factory() else { return false };
        let mut ctx: *mut sys::AmfContext = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx) != sys::AMF_OK
            || ctx.is_null()
        {
            return false;
        }
        let ctx = Ctx(ctx);
        if ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1) != sys::AMF_OK {
            return false;
        }
        let mut comp: *mut sys::AmfComponent = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_component)(
            lib.factory,
            ctx.0,
            props.component.0,
            &mut comp,
        ) != sys::AMF_OK
            || comp.is_null()
        {
            return false;
        }
        let comp = Component(comp);
        if ((*(*comp.0).vtbl).set_property)(
            comp.0,
            props.usage.0,
            AmfVariant::from_i64(usage_from_env(codec)),
        ) != sys::AMF_OK
        {
            return false;
        }
        if ten_bit {
            // Same required 10-bit props as a real session — reject here is the probe's answer.
            let depth_props: &[(PCWSTR, i64)] = match codec {
                Codec::H265 => &[
                    (w!("HevcProfile"), HEVC_PROFILE_MAIN_10),
                    (w!("HevcColorBitDepth"), COLOR_BIT_DEPTH_10),
                ],
                Codec::Av1 => &[(w!("Av1ColorBitDepth"), COLOR_BIT_DEPTH_10)],
                Codec::H264 | Codec::PyroWave => return false,
            };
            for (name, value) in depth_props {
                if ((*(*comp.0).vtbl).set_property)(comp.0, name.0, AmfVariant::from_i64(*value))
                    != sys::AMF_OK
                {
                    return false;
                }
            }
        }
        let surface = if ten_bit {
            sys::AMF_SURFACE_P010
        } else {
            sys::AMF_SURFACE_NV12
        };
        ((*(*comp.0).vtbl).init)(comp.0, surface, 640, 480) == sys::AMF_OK
    }
}

/// D3D11 device on the selected render adapter; OS default hardware adapter if unresolved.
fn selected_adapter_device() -> Option<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    // SAFETY: probe owns every handle. Factory/adapter COM objects or err → default fallback.
    // `D3D11CreateDevice` fills `device` only on success. Everything drops with its COM wrapper.
    unsafe {
        let adapter: Option<IDXGIAdapter1> =
            pf_gpu::resolve_render_adapter_luid().and_then(|luid| {
                let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
                factory.EnumAdapterByLuid(luid).ok()
            });
        let mut device: Option<ID3D11Device> = None;
        let created = match &adapter {
            Some(a) => D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
            None => D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
        };
        if created.is_err() {
            return None;
        }
        device
    }
}

enum DrainOutcome {
    /// Finished AU, FIFO-paired with its `pending` entry.
    Frame(EncodedFrame),
    /// No output yet (AMF_OK / AMF_REPEAT / AMF_NEED_MORE_INPUT with null data).
    NotReady,
    /// End of stream after `Drain`/`Flush` (AMF_EOF).
    Eof,
}

/// One `QueryOutput`, FIFO-paired with the oldest `pending` (no B-frames). Free fn so `submit`
/// can call it while already holding `&mut Inner`.
///
/// # Safety
/// `comp` is live, `pending` is its FIFO, encode thread, no other AMF call in flight.
unsafe fn drain_one_output(
    comp: *mut sys::AmfComponent,
    pending: &mut VecDeque<(u64, bool, bool)>,
    output_data_type: PCWSTR,
    output_key_max: i64,
) -> Result<DrainOutcome> {
    // SAFETY: `QueryOutput` fills `data` with an owned ref only when it returns one; non-null
    // moves into `OwnedData`. `QueryInterface(IID_AMFBuffer)` AddRefs (slot-2 release). Host
    // memory is valid until buffer release: copy to `Vec` before the guards drop.
    let mut data: *mut sys::AmfData = ptr::null_mut();
    let r = ((*(*comp).vtbl).query_output)(comp, &mut data);
    if data.is_null() {
        return match r {
            sys::AMF_EOF => Ok(DrainOutcome::Eof),
            sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_NEED_MORE_INPUT => Ok(DrainOutcome::NotReady),
            // Typed failure on this frame (device-lost, …) — caller resets in place.
            other => bail!("AMF QueryOutput failed: {} ({other})", result_name(other)),
        };
    }
    let data = OwnedData(data);
    // Keyframe from output type, OR the forced flag so a driver that skips the property still flags.
    let mut var = AmfVariant::zeroed();
    let key_prop = ((*(*data.0).vtbl).get_property)(data.0, output_data_type.0, &mut var)
        == sys::AMF_OK
        && var.as_i64().is_some_and(|t| t <= output_key_max);
    let mut buf: *mut c_void = ptr::null_mut();
    amf_ok(
        ((*(*data.0).vtbl).query_interface)(data.0, &sys::IID_AMF_BUFFER, &mut buf),
        "AMF QueryInterface(AMFBuffer)",
    )?;
    if buf.is_null() {
        bail!("AMF output is not an AMFBuffer");
    }
    // AMFData-prefix guard: slot 2 is Release on every vtable.
    let buf_guard = OwnedData(buf as *mut sys::AmfData);
    let buf = buf_guard.0 as *mut sys::AmfBuffer;
    let size = ((*(*buf).vtbl).get_size)(buf);
    let native = ((*(*buf).vtbl).get_native)(buf);
    if native.is_null() || size == 0 {
        bail!("AMF output buffer is empty");
    }
    let au = std::slice::from_raw_parts(native as *const u8, size).to_vec();
    let (pts_ns, forced, recovery_anchor) = pending.pop_front().unwrap_or((0, false, false));
    Ok(DrainOutcome::Frame(EncodedFrame {
        data: au,
        pts_ns,
        keyframe: key_prop || forced,
        recovery_anchor,
        chunk_aligned: false,
    }))
}

/// How long `submit` drains for a free input slot before declaring a wedge. Above one frame's
/// encode time, far under the session watchdog's ~2 s floor.
const INPUT_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

impl Encoder for AmfEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!("native AMF is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        // Mid-session format fallback: CopySubresourceRegion across format groups is UB. No readback.
        let expected = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        anyhow::ensure!(
            captured.format == expected,
            "captured format {:?} != AMF input ring {:?} (capturer video-processor fallback \
             mid-session — native AMF has no readback path)",
            captured.format,
            expected
        );
        self.ensure_inner(&frame.device)?;
        let cur_idx = self.frame_idx;
        // First submit on a component must be a forced IDR. Use the ring counter, not
        // `frame_idx == 0`: `submit_indexed` pins wire indexes that are non-zero after a rebuild.
        let opening = self.inner.as_ref().is_none_or(|i| i.next == 0);
        let forced = std::mem::take(&mut self.force_kf) || opening;
        let pts_100ns = self.frame_idx * 10_000_000 / self.fps.max(1) as i64;
        self.frame_idx += 1;
        // LTR decisions before borrowing `inner`: the test hook re-enters `&mut self`, and
        // PCWSTR copies let the surface block set props without re-borrowing `self.props`.
        let ltr_names = self
            .props
            .ltr
            .as_ref()
            .map(|l| (l.mark_ltr_index, l.force_ltr_bitfield));
        let mut mark_slot: Option<usize> = None;
        let mut force_slot: Option<usize> = None;
        let mut recovery_anchor = false;
        if self.ltr_active {
            if forced {
                // IDR resets decoder refs — drop stale LTR slots and any force queued against them.
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            } else if self.ltr_test_force_at == Some(cur_idx) {
                // Spike hook: self-trigger the real invalidate path without a live client.
                let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
                tracing::info!(
                    frame = cur_idx,
                    triggered,
                    "AMF LTR test hook fired invalidate_ref_frames"
                );
            }
            // Apply a queued force to this frame. Skip if the taint sweep emptied the slot: the
            // hardware still holds the tainted mark, so forcing it would re-reference the loss.
            if let Some(slot) = self.pending_force.take() {
                if self.ltr_slots[slot].is_some() {
                    force_slot = Some(slot);
                    recovery_anchor = true;
                }
            }
            // Mark on IDR and every interval, never on the recovery frame (would overwrite the force).
            if force_slot.is_none() && (forced || cur_idx % self.ltr_mark_interval == 0) {
                let slot = self.next_ltr_slot;
                self.ltr_slots[slot] = Some(cur_idx);
                self.next_ltr_slot = (self.next_ltr_slot + 1) % NUM_LTR_SLOTS;
                mark_slot = Some(slot);
            }
        }
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Re-push HDR metadata on change or rebuild. Best-effort: reject leaves the 0xCE datagram.
        if let Some(name) = self.props.hdr_metadata {
            if self.ten_bit && inner.hdr_pushed != self.hdr_meta {
                if let Some(m) = self.hdr_meta {
                    // SAFETY: live context/component pair, encode thread (`push_hdr_metadata`).
                    match unsafe { push_hdr_metadata(inner.ctx.0, inner.comp.0, name, &m) } {
                        Ok(()) => tracing::debug!(
                            "AMF HDR mastering metadata attached (in-band on keyframes)"
                        ),
                        Err(e) => tracing::warn!(
                            error = %format!("{e:#}"),
                            "AMF rejected the HDR mastering metadata — no in-band SEI/OBU"
                        ),
                    }
                }
                inner.hdr_pushed = self.hdr_meta;
            }
        }
        // Bound in-flight below RING before reuse: AMF keeps reading a slot until its AU is
        // retrieved. Drain finished AUs into `ready` rather than overwrite or treat INPUT_FULL as
        // a wedge. No progress for the whole budget is a genuine wedge.
        if inner.pending.len() >= RING {
            let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
            while inner.pending.len() >= RING {
                // SAFETY: live component + its FIFO, encode thread, no other AMF call in flight.
                match unsafe {
                    drain_one_output(
                        inner.comp.0,
                        &mut inner.pending,
                        self.props.output_data_type,
                        self.props.output_key_max,
                    )
                }? {
                    DrainOutcome::Frame(f) => inner.ready.push_back(f),
                    DrainOutcome::Eof => break,
                    DrainOutcome::NotReady => {
                        if std::time::Instant::now() >= deadline {
                            self.force_kf = true;
                            bail!(
                                "AMF produced no output for {} ms with {} frame(s) in flight — \
                                 wedged (escalating to reset)",
                                INPUT_DRAIN_BUDGET.as_millis(),
                                inner.pending.len()
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_micros(250));
                    }
                }
            }
        }
        let slot = inner.next % RING;
        inner.next += 1;
        // SAFETY: `src`/`dst` are same-format, same-size, same-device (ring rebuilt on device
        // change). `CopySubresourceRegion` on this thread's immediate context is a valid GPU copy.
        // `CreateSurfaceFromDX11Native` wraps without owning (null observer); the surface moves
        // into `OwnedData`. AMF AddRefs what it keeps, so our release does not free a buffer in flight.
        unsafe {
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = inner.ring[slot].cast().context("ring -> resource")?;
            inner
                .dctx
                .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None);

            let mut surf: *mut sys::AmfData = ptr::null_mut();
            amf_ok(
                ((*(*inner.ctx.0).vtbl).create_surface_from_dx11_native)(
                    inner.ctx.0,
                    inner.ring[slot].as_raw(),
                    &mut surf,
                    ptr::null_mut(),
                ),
                "AMF CreateSurfaceFromDX11Native",
            )?;
            if surf.is_null() {
                bail!("AMF CreateSurfaceFromDX11Native returned null");
            }
            let surf = OwnedData(surf);
            ((*(*surf.0).vtbl).set_pts)(surf.0, pts_100ns);
            if forced {
                // Forced IDR/KEY + in-band headers. Log-and-continue: reject still encodes.
                let r = ((*(*surf.0).vtbl).set_property)(
                    surf.0,
                    self.props.force_picture_type.0,
                    AmfVariant::from_i64(self.props.force_idr_value),
                );
                if r != sys::AMF_OK {
                    tracing::warn!(
                        result = result_name(r),
                        amf_code = r,
                        "AMF forced-keyframe picture type rejected"
                    );
                }
                match self.codec {
                    Codec::H264 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertSPS").0,
                            AmfVariant::from_bool(true),
                        );
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertPPS").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    Codec::H265 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("HevcInsertHeader").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    // KEY_FRAME_ALIGNED already puts a sequence header OBU on every key frame.
                    Codec::Av1 => {}
                    Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
                }
            }
            // LTR mark/force decided above. Best-effort: reject leaves the client on IDR fallback.
            if let Some((mark_name, force_name)) = ltr_names {
                if let Some(slot) = mark_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        mark_name.0,
                        AmfVariant::from_i64(slot as i64),
                    );
                    if r != sys::AMF_OK {
                        tracing::warn!(
                            slot,
                            result = result_name(r),
                            amf_code = r,
                            "AMF LTR mark rejected"
                        );
                    }
                }
                if let Some(slot) = force_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        force_name.0,
                        AmfVariant::from_i64(1_i64 << slot),
                    );
                    if r == sys::AMF_OK {
                        tracing::info!(
                            slot,
                            frame = cur_idx,
                            "AMF LTR-RFI: re-referencing known-good LTR (clean recovery, no IDR)"
                        );
                    } else {
                        tracing::warn!(
                            slot,
                            result = result_name(r),
                amf_code = r,
                            "AMF LTR force-reference rejected — client stays frozen until its IDR fallback"
                        );
                    }
                }
            }
            let mut r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
            // AMF_INPUT_FULL is "busy, drain and retry", not a wedge. Re-submit the same surface.
            if r == sys::AMF_INPUT_FULL {
                let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
                loop {
                    match drain_one_output(
                        inner.comp.0,
                        &mut inner.pending,
                        self.props.output_data_type,
                        self.props.output_key_max,
                    )? {
                        DrainOutcome::Frame(f) => inner.ready.push_back(f),
                        DrainOutcome::Eof => break,
                        DrainOutcome::NotReady => {
                            std::thread::sleep(std::time::Duration::from_micros(250))
                        }
                    }
                    r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
                    if r != sys::AMF_INPUT_FULL || std::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
            match r {
                // NEED_MORE_INPUT = accepted; no AU owed for this submit alone.
                sys::AMF_OK | sys::AMF_NEED_MORE_INPUT => {}
                sys::AMF_INPUT_FULL => {
                    self.force_kf = true; // retried frame stays an IDR candidate
                    bail!("AMF SubmitInput stayed AMF_INPUT_FULL past the drain budget — wedged");
                }
                other => {
                    self.force_kf = true;
                    bail!("AMF SubmitInput failed: {} ({other})", result_name(other));
                }
            }
        }
        inner
            .pending
            .push_back((captured.pts_ns, forced, recovery_anchor));
        Ok(())
    }

    /// Pin `frame_idx` to the wire index so LTR slots compare against client frame numbers across
    /// rebuilds. An internal counter desyncs on the first bitrate rebuild and can force an LTR
    /// marked inside the lost range.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.hdr_meta = meta;
    }

    /// Force the next submit to re-reference the newest LTR marked before `[first, last]`.
    /// `true` = usable pre-loss LTR (caller must not also IDR); `false` = fall back to keyframe.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live LTR (driver declined, or AV1) or a nonsense range → caller IDRs.
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        // Policy is `rfi::plan_slot_recovery`; mechanism is clear the mirror slot. Slots store
        // wire indexes (`submit_indexed`) so they compare against client `first` across rebuilds.
        let view: Vec<(usize, i64)> = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter_map(|(s, m)| m.map(|w| (s, w)))
            .collect();
        let plan = super::rfi::plan_slot_recovery(&view, first);
        for (slot, marked) in self.ltr_slots.iter_mut().enumerate() {
            if plan.tainted & (1 << slot) != 0 {
                *marked = None;
            }
        }
        match plan.anchor {
            Some((slot, ltr_frame)) => {
                // Next submit force-references this slot and ships `recovery_anchor`.
                self.pending_force = Some(slot);
                tracing::info!(
                    first,
                    last,
                    slot,
                    ltr_frame,
                    "AMF LTR-RFI: forcing the next frame to re-reference a known-good LTR (no IDR)"
                );
                true
            }
            None => {
                // Sweep may have emptied a queued force's slot — don't force a tainted hardware slot.
                self.pending_force = None;
                tracing::info!(
                    first,
                    last,
                    "AMF LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    /// Clear every LTR mirror slot and any queued force (would otherwise re-reference the taint).
    fn distrust_references(&mut self) {
        let live = self.ltr_slots.iter().filter(|m| m.is_some()).count();
        if live == 0 && self.pending_force.is_none() {
            return;
        }
        self.ltr_slots = [None; NUM_LTR_SLOTS];
        self.pending_force = None;
        tracing::debug!(
            live,
            "AMF LTR-RFI: client reported unrepaired damage — withdrawing anchor trust from every \
             live LTR (the marking cadence re-marks a clean frame within ~1/4 s)"
        );
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            blends_cursor: false,
            supports_rfi: self.ltr_active,
            chroma_444: false,
            intra_refresh: self.ir_active,
            // AMF emits no recovery-point SEI; host keeps the IDR path.
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    /// Bounded-blocking poll: spin `QueryOutput` with ~250 µs sleeps up to
    /// `min(3/4 frame interval, 12 ms)`. Expiry is `Ok(None)` — watchdog arbitrates a real wedge.
    /// Hands out `submit`'s buffered AUs first.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let odt = self.props.output_data_type;
        let okm = self.props.output_key_max;
        // Scope the inner borrow so a produced AU can clear `resets_without_output` on `self`.
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            if let Some(au) = inner.ready.pop_front() {
                inner.note_first_au(&au);
                Some(au)
            } else {
                let budget = std::time::Duration::from_micros(750_000 / self.fps.max(1) as u64)
                    .min(std::time::Duration::from_millis(12));
                let deadline = std::time::Instant::now() + budget;
                let mut out = None;
                loop {
                    // SAFETY: live component + FIFO, encode thread, no other AMF call in flight.
                    match unsafe { drain_one_output(inner.comp.0, &mut inner.pending, odt, okm) }? {
                        DrainOutcome::Frame(au) => {
                            inner.note_first_au(&au);
                            out = Some(au);
                            break;
                        }
                        DrainOutcome::Eof => {
                            inner.pending.clear();
                            break;
                        }
                        DrainOutcome::NotReady => {}
                    }
                    // Wait only while a frame is owed; ~250 µs between checks.
                    if inner.pending.is_empty() || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
                out
            }
        };
        // Any AU proves this context encodes — reset the no-output streak.
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// Stall recovery: Flush + Terminate + re-Init on the same context. Fail → drop `inner` so
    /// the next submit rebuilds lazily. Owed AUs forfeited; next frame is a forced IDR.
    /// In-place re-Init cannot clear a dead VCN session; at 2 no-output resets, tear the context down.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        if self.inner.is_none() {
            return true; // next submit rebuilds lazily
        }
        // Second no-output reset: the fault is the context. Drop `inner` before borrowing it.
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "AMF stall persisted across in-place re-Init — full context teardown, reopening a \
                 fresh context (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner is Some — checked above and not cleared since");
        inner.pending.clear();
        inner.ready.clear(); // owed AUs forfeited; rebuilt stream restarts at IDR
        inner.hdr_pushed = None; // re-Init'd component needs HDR metadata again
                                 // SAFETY: live component, encode thread, no AMF call in flight. Flush/Terminate are
                                 // legal on a wedge (results ignored); apply_static_props + init rebuild it.
        let rebuilt = unsafe {
            let comp = inner.comp.0;
            ((*(*comp).vtbl).flush)(comp);
            ((*(*comp).vtbl).terminate)(comp);
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            match self.apply_static_props(comp) {
                Ok((ir, ltr)) => {
                    self.ir_active = ir;
                    // Re-Init voids reference history; drop prior LTR marks.
                    self.ltr_active = ltr;
                    self.ltr_slots = [None; NUM_LTR_SLOTS];
                    self.next_ltr_slot = 0;
                    self.pending_force = None;
                    ((*(*comp).vtbl).init)(comp, fmt, self.width as i32, self.height as i32)
                        == sys::AMF_OK
                }
                Err(_) => false,
            }
        };
        if rebuilt {
            tracing::info!(
                "AMF encoder rebuilt in place (Terminate + re-Init on the same context)"
            );
        } else {
            self.ir_active = false;
            self.ltr_active = false;
            tracing::warn!("AMF in-place re-Init failed — full context teardown, reopening lazily");
            self.inner = None;
            self.bound_device = 0;
        }
        true
    }

    /// `TargetBitrate` via `GetProperty`. `None` before lazy open or on decline — caller keeps
    /// the requested rate. Without this, ABR never learns `encoder_ceiling_kbps` on AMD.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        let inner = self.inner.as_ref()?;
        // SAFETY: live component, session thread, no AMF call in flight; out-param is a local.
        unsafe { get_prop_i64(inner.comp.0, self.props.target_bitrate) }
            .filter(|&b| b > 0)
            .map(|b| b as u64)
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps_i = bps.min(i64::MAX as u64) as i64;
        let vbv = self.vbv_bits(bps);
        let Some(inner) = self.inner.as_ref() else {
            // Lazy open applies the new rate via `apply_static_props`.
            self.bitrate_bps = bps;
            return true;
        };
        // Target/Peak/VBV are dynamic: SetProperty retargets without Terminate (no IDR).
        // SAFETY: live component, encode thread, no AMF call in flight.
        let applied = unsafe {
            let p = &self.props;
            let comp = inner.comp.0;
            let ok = set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps_i), false)
                .unwrap_or(false)
                && set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps_i), false)
                    .unwrap_or(false);
            if ok {
                // Optional VBV rescale; decline keeps the old buffer (HRD absorbs the mismatch).
                let _ = set_prop(comp, p.vbv_size, AmfVariant::from_i64(vbv), false);
            }
            ok
        };
        if !applied {
            // Half-applied pair is fine: the rebuild fallback re-authors from scratch.
            tracing::warn!(
                mbps = bps / 1_000_000,
                "AMF declined the dynamic bitrate retarget — falling back to a rebuild"
            );
            return false;
        }
        self.bitrate_bps = bps; // reset()/re-Init re-apply the new rate
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // SAFETY: live component, owning thread. Drain = EOS; remaining AUs surface until AMF_EOF.
        let r = unsafe { ((*(*inner.comp.0).vtbl).drain)(inner.comp.0) };
        if r != sys::AMF_OK {
            tracing::debug!(
                result = result_name(r),
                amf_code = r,
                "AMF Drain returned non-OK at flush"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Layout of the FFI mirrors lives as `const _: ()` in `amf_sys.rs` (every build). This
    // checks little-endian union payload packing, which a size/align assert cannot express.
    #[test]
    fn variant_payload_packing_matches_c() {
        let v = AmfVariant::from_rate(60, 1);
        assert_eq!(v.payload[0], 60u64 | (1u64 << 32));
        assert_eq!(AmfVariant::from_i64(-1).payload[0], u64::MAX);
    }

    /// HDR10 grade for live tests: BT.2020, 1000-nit, ST.2086 wire order (primaries G, B, R).
    fn sample_hdr_meta() -> punktfunk_core::quic::HdrMeta {
        punktfunk_core::quic::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 1000 * 10000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        }
    }

    /// D3D11 device on the AMD adapter. `None` = no AMD GPU — caller skips.
    fn amd_d3d11_device() -> Option<ID3D11Device> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
        const VENDOR_AMD: u32 = 0x1002;
        // SAFETY: probe owns every handle. Factory/adapter COM or err; CreateDevice fills
        // `device` only on success. Everything drops with its COM wrapper.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            for i in 0.. {
                let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
                let desc = adapter.GetDesc1().ok()?;
                if desc.VendorId != VENDOR_AMD {
                    continue;
                }
                let mut device: Option<ID3D11Device> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    Default::default(),
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .ok()?;
                return device;
            }
            None
        }
    }

    /// DEFAULT-usage NV12 texture (uninit GPU memory; content is irrelevant).
    fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("NV12 texture");
        tex.expect("NV12 texture")
    }

    /// `p`-quantile of `samples` (µs), sorting in place. `0` when empty. Gated like its only
    /// caller so a `--features nvenc,qsv` build does not trip `dead_code`.
    #[cfg(feature = "amf-qsv")]
    fn percentile(samples: &mut [u128], p: f64) -> u128 {
        if samples.is_empty() {
            return 0;
        }
        samples.sort_unstable();
        let idx = (((samples.len() - 1) as f64) * p).round() as usize;
        samples[idx]
    }

    /// Pace at `1/fps` and return each frame's submit→AU wall-clock (µs), FIFO-paired. Unflushed
    /// trailing frames are left unmeasured so every sample is a genuine paced submit→AU.
    #[cfg(feature = "amf-qsv")]
    #[allow(clippy::too_many_arguments)]
    fn drive_and_measure(
        enc: &mut dyn Encoder,
        device: &ID3D11Device,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
        fps: u32,
        fmt: PixelFormat,
        frames: usize,
    ) -> Vec<u128> {
        use std::time::{Duration, Instant};
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let mut pending: VecDeque<Instant> = VecDeque::new();
        let mut samples: Vec<u128> = Vec::new();
        let mut next = Instant::now();
        for i in 0..frames {
            if let Some(d) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            next += interval;
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: fmt,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            let t = Instant::now();
            enc.submit(&frame).expect("bench submit");
            pending.push_back(t);
            while let Some(_au) = enc.poll().expect("bench poll") {
                let ts = pending.pop_front().expect("FIFO pairing");
                samples.push(ts.elapsed().as_micros());
            }
        }
        samples
    }

    /// Native vs libavcodec-AMF submit→AU A/B on the same paced NV12 input. Opt-in
    /// (`PUNKTFUNK_AMF_BENCH=1`); gated on `amf-qsv`. Skips without the AMD runtime/GPU.
    #[cfg(feature = "amf-qsv")]
    #[test]
    fn amf_latency_ab_bench() {
        if std::env::var("PUNKTFUNK_AMF_BENCH").as_deref() != Ok("1") {
            eprintln!(
                "skipping: set PUNKTFUNK_AMF_BENCH=1 to run the native-vs-ffmpeg latency A/B"
            );
            return;
        }
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (1920u32, 1080u32, 60u32);
        let bitrate = 20_000_000u64;
        let frames = 180usize;
        let tex = nv12_texture(&device, w, h);

        let mut native = AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            bitrate,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("native AMF open");
        let mut native_us = drive_and_measure(
            &mut native,
            &device,
            &tex,
            w,
            h,
            fps,
            PixelFormat::Nv12,
            frames,
        );
        drop(native);

        let mut ffmpeg = crate::ffmpeg_win::FfmpegWinEncoder::open(
            crate::ffmpeg_win::WinVendor::Amf,
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            bitrate,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("libavcodec AMF open");
        let mut ffmpeg_us = drive_and_measure(
            &mut ffmpeg,
            &device,
            &tex,
            w,
            h,
            fps,
            PixelFormat::Nv12,
            frames,
        );
        drop(ffmpeg);

        let iv = 1_000_000u128 / fps as u128;
        let (n50, n99, nc) = (
            percentile(&mut native_us, 0.50),
            percentile(&mut native_us, 0.99),
            native_us.len(),
        );
        let (f50, f99, fc) = (
            percentile(&mut ffmpeg_us, 0.50),
            percentile(&mut ffmpeg_us, 0.99),
            ffmpeg_us.len(),
        );
        eprintln!("=== native AMF vs libavcodec-AMF  encode_us A/B ===");
        eprintln!("mode: {w}x{h}@{fps} HEVC, {frames} paced frames, frame period {iv} us");
        eprintln!(
            "native (direct SDK) : p50={n50} us  p99={n99} us  ({nc} AUs)  = {:.2} frame periods",
            n50 as f64 / iv as f64
        );
        eprintln!(
            "ffmpeg (libavcodec) : p50={f50} us  p99={f99} us  ({fc} AUs)  = {:.2} frame periods",
            f50 as f64 / iv as f64
        );
        if n50 > 0 {
            eprintln!(
                "native p50 is {:.1}x lower than ffmpeg",
                f50 as f64 / n50 as f64
            );
        }
        assert!(
            n50 < f50,
            "native encode_us p50 ({n50}) must beat the libavcodec hold ({f50})"
        );
        assert!(
            n50 < iv,
            "native encode_us p50 ({n50} us) should collapse below one frame period ({iv} us)"
        );
    }

    /// Live [`Encoder`] smoke per codec: submit/poll, native `reset()`, second batch, flush-drain.
    /// Asserts Annex-B (or AV1 OBU), IDR at start and after reset, FIFO pts. Skips without AMD.
    #[test]
    fn amf_encode_live_smoke() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);

        for codec in [Codec::H265, Codec::H264, Codec::Av1] {
            // AV1 is RDNA3+: probe THIS device (`open` may pick a different GPU on a hybrid box).
            if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
                eprintln!("skipping Av1: this AMD GPU's native probe declined it (pre-RDNA3?)");
                continue;
            }
            let mut enc = match AmfEncoder::open(
                codec,
                PixelFormat::Nv12,
                w,
                h,
                fps,
                2_000_000,
                8,
                ChromaFormat::Yuv420,
            ) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("skipping {codec:?}: native AMF open declined ({e:#})");
                    continue;
                }
            };
            let batch = |enc: &mut AmfEncoder, base: u64, n: usize| -> Vec<EncodedFrame> {
                let mut aus = Vec::new();
                for i in 0..n {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: w,
                        height: h,
                        pts_ns: base + i as u64,
                        format: PixelFormat::Nv12,
                        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                            texture: tex.clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit(&frame).expect("submit");
                    if let Some(au) = enc.poll().expect("poll") {
                        aus.push(au);
                    }
                }
                aus
            };
            let first_run = batch(&mut enc, 1, 6);
            assert!(enc.reset(), "native reset must report rebuilt");
            let mut second_run = batch(&mut enc, 100, 6);
            enc.flush().expect("flush");
            for _ in 0..50 {
                match enc.poll().expect("drain poll") {
                    Some(au) => second_run.push(au),
                    None => break,
                }
            }
            assert!(
                first_run.len() >= 3 && second_run.len() >= 3,
                "{codec:?}: expected most AUs out (got {} + {})",
                first_run.len(),
                second_run.len()
            );
            for run in [&first_run, &second_run] {
                let first = &run[0];
                assert!(
                    first.keyframe,
                    "{codec:?}: stream/reset start must be an IDR"
                );
                if codec == Codec::Av1 {
                    // AV1 is OBU, not Annex-B.
                    assert!(!first.data.is_empty(), "Av1: empty key AU");
                } else {
                    assert!(
                        first.data.starts_with(&[0, 0, 0, 1]) || first.data.starts_with(&[0, 0, 1]),
                        "{codec:?}: AU must be Annex-B (got {:02x?})",
                        &first.data[..first.data.len().min(8)]
                    );
                }
            }
            assert_eq!(first_run[0].pts_ns, 1, "FIFO pts pairing");
            // Bitstream FIFO: a declined B-frame pin would reorder AUs. Don't trust set_prop.
            for run in [&first_run, &second_run] {
                for pair in run.windows(2) {
                    assert!(
                        pair[1].pts_ns > pair[0].pts_ns,
                        "{codec:?}: AUs must leave in submit order (reordering ⇒ B-frames), \
                         got {} then {}",
                        pair[0].pts_ns,
                        pair[1].pts_ns
                    );
                }
            }
            assert_eq!(second_run[0].pts_ns, 100, "post-reset FIFO pts pairing");
            eprintln!(
                "live AMF {codec:?} encode: {} + {} AUs across a native reset, first IDR {} bytes",
                first_run.len(),
                second_run.len(),
                first_run[0].data.len()
            );
        }
    }

    /// Live `applied_bitrate_bps`: None before lazy open, open rate after submit, new rate after
    /// retarget. Skips without AMD.
    #[test]
    fn amf_applied_bitrate_readback_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("native AMF open");
        assert_eq!(
            enc.applied_bitrate_bps(),
            None,
            "no readback before the lazy open — the caller must keep the requested rate"
        );
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let opened = enc.applied_bitrate_bps();
        assert_eq!(
            opened,
            Some(2_000_000),
            "post-open readback must be the accepted open rate"
        );
        assert!(
            enc.reconfigure_bitrate(8_000_000),
            "dynamic retarget declined on live hardware"
        );
        let retargeted = enc.applied_bitrate_bps();
        assert_eq!(
            retargeted,
            Some(8_000_000),
            "post-retarget readback must be the accepted NEW rate"
        );
        eprintln!("live AMF applied-bitrate readback: open {opened:?} -> retarget {retargeted:?}");
    }

    /// Live probe: AVC and HEVC must be true on any VCN; AV1 is hardware truth (RDNA3+).
    #[test]
    fn amf_native_probe_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let h264 = probe_can_encode_on(&device, Codec::H264);
        let h265 = probe_can_encode_on(&device, Codec::H265);
        let av1 = probe_can_encode_on(&device, Codec::Av1);
        eprintln!("native AMF probe: h264={h264} h265={h265} av1={av1}");
        assert!(h264 && h265, "every VCN generation encodes AVC + HEVC");
    }

    /// Live HDR: P010 HEVC Main10 must encode. Mastering/CLL prefix SEI (payload 137/144) is
    /// soft-reported — VCN generations differ.
    #[test]
    fn amf_hdr_encode_live_smoke() {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
        let tex = tex.expect("P010 texture");
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            w,
            h,
            fps,
            4_000_000,
            10,
            ChromaFormat::Yuv420,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF 10-bit open declined ({e:#})");
                return;
            }
        };
        enc.set_hdr_meta(Some(sample_hdr_meta()));
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..6 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit(&frame).expect("submit (P010)");
            if let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        assert!(!aus.is_empty(), "10-bit HDR encode produced no AUs");
        let idr = &aus[0];
        assert!(idr.keyframe, "first AU must be an IDR");
        // HEVC prefix-SEI (NUH 0x4E 0x01): payload 137 mastering / 144 CLL.
        let mut mastering = false;
        let mut cll = false;
        for i in 0..idr.data.len().saturating_sub(5) {
            let d = &idr.data[i..];
            let nal = if d.starts_with(&[0, 0, 1]) {
                &d[3..]
            } else if d.starts_with(&[0, 0, 0, 1]) {
                &d[4..]
            } else {
                continue;
            };
            if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 {
                match nal[2] {
                    137 => mastering = true,
                    144 => cll = true,
                    _ => {}
                }
            }
        }
        eprintln!(
            "live AMF HEVC Main10 HDR: {} AUs, IDR {} bytes, mastering SEI={mastering}, CLL SEI={cll}",
            aus.len(),
            idr.data.len()
        );
        if !mastering {
            eprintln!("note: no mastering-display SEI found on this VCN/driver — client falls back to the 0xCE datagram");
        }
    }

    /// Live intra-refresh property on a scratch component (does not mutate process env).
    #[test]
    fn amf_intra_refresh_property_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let Ok(lib) = try_factory() else { return };
        // SAFETY: guards own every created object; `set_prop` on this thread.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            assert_eq!(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                sys::AMF_OK
            );
            let ctx = Ctx(ctx);
            assert_eq!(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                sys::AMF_OK
            );
            for codec in [Codec::H264, Codec::H265] {
                let props = codec_props(codec);
                let mut comp: *mut sys::AmfComponent = ptr::null_mut();
                if ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    props.component.0,
                    &mut comp,
                ) != sys::AMF_OK
                    || comp.is_null()
                {
                    eprintln!("skipping {codec:?}: component unavailable");
                    continue;
                }
                let comp = Component(comp);
                let _ = set_prop(
                    comp.0,
                    props.usage,
                    AmfVariant::from_i64(usage_from_env(codec)),
                    true,
                );
                let (name, block) = props.intra_refresh.expect("AVC/HEVC define intra-refresh");
                let blocks = 640u32.div_ceil(block) * 480u32.div_ceil(block);
                let per_slot = blocks.div_ceil(30).max(1);
                let applied = set_prop(comp.0, name, AmfVariant::from_i64(per_slot as i64), false)
                    .expect("optional set_prop never errors");
                eprintln!(
                    "intra-refresh {codec:?}: {per_slot} units/slot accepted={applied} on this VCN"
                );
            }
        }
    }

    /// Burst faster than the encoder drains (no poll between submits). `submit` must drain into
    /// `ready` instead of erroring. Asserts IDR-first FIFO across the ready→pending boundary.
    #[test]
    fn amf_backpressure_burst_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        const BURST: u64 = 48; // >> RING, faster than the ASIC drains
        for i in 1..=BURST {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i,
                format: PixelFormat::Nv12,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            // No poll between submits: the in-flight bound must drain, never error.
            enc.submit(&frame)
                .expect("burst submit must ride back-pressure, not error");
        }
        enc.flush().expect("flush");
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for _ in 0..(BURST as usize + 100) {
            match enc.poll().expect("drain poll") {
                Some(au) => aus.push(au),
                None => break,
            }
        }
        assert!(
            aus.len() as u64 >= BURST - 2,
            "most AUs must survive the burst without a reset (got {} of {BURST})",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be the IDR");
        for pair in aus.windows(2) {
            assert!(
                pair[1].pts_ns > pair[0].pts_ns,
                "AUs must stay FIFO-monotonic across the ready→pending boundary: {} then {}",
                pair[0].pts_ns,
                pair[1].pts_ns
            );
        }
        eprintln!(
            "back-pressure burst: {} AUs, FIFO-monotonic, IDR-first — ring bound held, no reset",
            aus.len()
        );
    }

    /// FFI smoke: load, version-gate, CreateContext + HEVC CreateComponent. A layout error in
    /// the mirror crashes; pass/skip is the assertion.
    #[test]
    fn amf_factory_probe_smoke() {
        let lib = match try_factory() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: AMF runtime unavailable ({e})");
                return;
            }
        };
        assert!(lib.version >= sys::AMF_MIN_VERSION);
        // SAFETY: CreateContext fills `ctx` only on AMF_OK; InitDX11(null) is AMF's own device
        // (fail → skip). Guards release every created object once.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx);
            assert_eq!(r, sys::AMF_OK, "CreateContext: {}", result_name(r));
            assert!(!ctx.is_null());
            let ctx = Ctx(ctx);
            let r = ((*(*ctx.0).vtbl).init_dx11)(ctx.0, ptr::null_mut(), sys::AMF_DX11_1);
            if r != sys::AMF_OK {
                eprintln!(
                    "skipping: InitDX11(default device) failed ({})",
                    result_name(r)
                );
                return;
            }
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_component)(
                lib.factory,
                ctx.0,
                w!("AMFVideoEncoderHW_HEVC").0,
                &mut comp,
            );
            if r != sys::AMF_OK || comp.is_null() {
                // Probe answer (no HEVC VCN), not a mirror failure.
                eprintln!("note: CreateComponent(HEVC) declined ({})", result_name(r));
                return;
            }
            let _comp = Component(comp);
        }
    }
}
