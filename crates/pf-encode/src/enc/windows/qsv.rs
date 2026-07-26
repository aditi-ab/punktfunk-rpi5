//! Intel **QSV** hardware encoder (Windows, D3D11 input) — the native-VPL replacement for the
//! libavcodec `*_qsv` path (design/native-qsv-encoder.md), the Intel analogue of [`super::amf`]
//! and [`super::nvenc`].
//!
//! Why not libavcodec: the ffmpeg QSV arm defaults to a **CPU readback** per frame (the exact
//! GPU→CPU→GPU roundtrip the video path bans), stubs every capability probe (`can_encode_10bit`
//! hardcoded `false` — 10-bit/HDR is dead on Intel today), and can express none of RFI, in-place
//! bitrate retarget, or in-band HDR metadata. The VPL runtime returns typed `mfxStatus` codes, so
//! a driver wedge surfaces on the frame it happens instead of as forever-EAGAIN.
//!
//! Drives the **statically linked MIT VPL dispatcher** (`libvpl-sys`, vendored — no new runtime
//! DLL): `MFXLoad` → hardware-implementation filter → `MFXCreateSession` resolves the
//! driver-store GPU runtime (`libmfx64-gen.dll` on Gen12+/Arc/MTL, legacy `libmfxhw64.dll` on
//! BDW→ICL). A box without an Intel driver fails session creation and the open falls through —
//! the same degrade contract as the NVENC/AMF runtime loaders. Behind the `qsv` cargo feature
//! (build needs cmake+libclang, both already in the closure via pyrowave-sys).
//!
//! Input is zero-copy by construction (design §3.4): the session is bound to the **capturer's
//! device** (`SetHandle` before Init — same-adapter requirement enforced via LUID match against
//! the dispatcher's implementation list), input surfaces come from the runtime's own video-memory
//! pool (`MFXMemory_GetSurfaceForEncode`, production API 2.0), and the captured NV12/P010 texture
//! is `CopySubresourceRegion`'d GPU-side into the surface's native texture
//! (`FrameInterface->GetNativeHandle`). No readback path exists: a capturer that fell back to
//! Bgra/Rgb10a2 or CPU frames is rejected at open/submit, mirroring the AMF contract. The
//! experimental import API (`mfxSurfaceD3D11Tex2D`, still `ONEVPL_EXPERIMENTAL` at 2.17) is a
//! later opt-in, not the shipping path.
//!
//! Scope (design §6): Phase 1 — AVC + HEVC (NV12/P010), bounded sync-point poll, in-place
//! `reset()`, no-IDR `reconfigure_bitrate` (`MFXVideoENCODE_Reset` + `StartNewSequence=OFF`,
//! legal because HRD conformance is off); Phase 2 — AV1 (DG2/Arc + MTL+; probed, never assumed),
//! 10-bit (HEVC Main10 / AV1, P010 `Shift=1`), in-band HDR mastering/CLL
//! (`mfxExtMasteringDisplayColourVolume`/`mfxExtContentLightLevelInfo` → HEVC prefix SEI / AV1
//! metadata OBU at IDR) + BT.2020/PQ `mfxExtVideoSignalInfo`; Phase 3 — LTR-based RFI via
//! `mfxExtRefListCtrl` (the [`super::amf`] slot policy verbatim: mark cadence into 2 user-LTR
//! slots, force-reference the newest pre-loss slot, `recovery_anchor` on the forced frame),
//! Query-gated per codec — the AV1 arm is honored by the open runtime source but spec-silent, so
//! it stays behind the same gate and falls back to IDR wherever the driver declines. 4:4:4 stays
//! `false` until probed on real hardware (design §8.6).

// Every `unsafe` block / impl in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use libvpl_sys as vpl;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ptr;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

/// Human-readable name for the `mfxStatus` codes this module branches on or logs; everything
/// else is reported numerically (identifiable against mfxdefs.h).
fn sts_name(s: vpl::mfxStatus) -> &'static str {
    match s {
        vpl::MFX_ERR_NONE => "MFX_ERR_NONE",
        vpl::MFX_ERR_UNSUPPORTED => "MFX_ERR_UNSUPPORTED",
        vpl::MFX_ERR_MORE_DATA => "MFX_ERR_MORE_DATA",
        vpl::MFX_ERR_NOT_FOUND => "MFX_ERR_NOT_FOUND",
        vpl::MFX_ERR_DEVICE_LOST => "MFX_ERR_DEVICE_LOST",
        vpl::MFX_ERR_DEVICE_FAILED => "MFX_ERR_DEVICE_FAILED",
        vpl::MFX_ERR_GPU_HANG => "MFX_ERR_GPU_HANG",
        vpl::MFX_ERR_NOT_ENOUGH_BUFFER => "MFX_ERR_NOT_ENOUGH_BUFFER",
        vpl::MFX_ERR_MEMORY_ALLOC => "MFX_ERR_MEMORY_ALLOC",
        vpl::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM",
        vpl::MFX_ERR_INVALID_VIDEO_PARAM => "MFX_ERR_INVALID_VIDEO_PARAM",
        vpl::MFX_ERR_UNDEFINED_BEHAVIOR => "MFX_ERR_UNDEFINED_BEHAVIOR",
        vpl::MFX_ERR_NOT_INITIALIZED => "MFX_ERR_NOT_INITIALIZED",
        vpl::MFX_WRN_DEVICE_BUSY => "MFX_WRN_DEVICE_BUSY",
        vpl::MFX_WRN_IN_EXECUTION => "MFX_WRN_IN_EXECUTION",
        vpl::MFX_WRN_INCOMPATIBLE_VIDEO_PARAM => "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM",
        vpl::MFX_WRN_PARTIAL_ACCELERATION => "MFX_WRN_PARTIAL_ACCELERATION",
        _ => "mfxStatus",
    }
}

fn vpl_ok(s: vpl::mfxStatus, what: &str) -> Result<()> {
    // Warnings (> 0) are success-with-note in the VPL model (e.g. params corrected); only
    // negative codes are failures.
    if s < vpl::MFX_ERR_NONE {
        bail!("{what} failed: {} ({s})", sts_name(s));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Bindgen anonymous-union accessors. The VPL C API nests anonymous structs/unions
// (`mfxVideoParam.{mfx}` → encode-options struct → per-RC unions); these helpers pin the
// generated `__bindgen_anon_*` paths in ONE place so the rest of the module reads like the C.
// ---------------------------------------------------------------------------------------------

/// The encode-options view of `mfxInfoMFX` (`TargetUsage`…`EncodedOrder`).
type EncOpts = vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1;

fn mfx_of(par: &mut vpl::mfxVideoParam) -> &mut vpl::mfxInfoMFX {
    // SAFETY: `mfxVideoParam`'s anonymous union is `{ mfx, vpp }`; an encoder parameter block
    // only ever uses the `mfx` view, and all-zero bytes are a valid `mfxInfoMFX`.
    unsafe { &mut par.__bindgen_anon_1.mfx }
}

fn enc_of(mfx: &mut vpl::mfxInfoMFX) -> &mut EncOpts {
    // SAFETY: `mfxInfoMFX`'s anonymous union overlays encode/decode/JPEG option structs; an
    // encoder only ever uses the encode view, and all-zero bytes are valid for it.
    unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 }
}

fn set_target_kbps(e: &mut EncOpts, kbps: u16) {
    e.__bindgen_anon_2 =
        vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2 { TargetKbps: kbps };
}

fn set_max_kbps(e: &mut EncOpts, kbps: u16) {
    e.__bindgen_anon_3 =
        vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_3 { MaxKbps: kbps };
}

fn frame_wh(info: &mut vpl::mfxFrameInfo) -> &mut vpl::mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
    // SAFETY: `mfxFrameInfo`'s anonymous union overlays the frame Width/Height/Crop view with
    // the buffer-size view; a video frame description uses the former, valid at all-zero.
    unsafe { &mut info.__bindgen_anon_1.__bindgen_anon_1 }
}

/// The VPL rate ceiling is `mfxU16` kbps × `BRCParamMultiplier`. Split `bps` into the smallest
/// multiplier that fits, so high-rate sessions (the >65 Mbps streaming range) don't saturate.
fn split_rate(bps: u64) -> (u16, u16) {
    let kbps = (bps / 1000).max(1);
    let mult = (kbps / (u16::MAX as u64) + 1) as u16;
    ((kbps / mult as u64) as u16, mult)
}

const fn align16(v: u32) -> u16 {
    (v.div_ceil(16) * 16) as u16
}

// ---------------------------------------------------------------------------------------------
// LTR-RFI policy knobs — the [`super::amf`] slot machinery verbatim (design §5): two user-LTR
// slots refreshed on a cadence; loss recovery force-references the newest pre-loss slot.
// ---------------------------------------------------------------------------------------------

const NUM_LTR_SLOTS: usize = 2;

/// `PUNKTFUNK_NO_QSV_LTR` — defeat switch for the LTR-RFI path (parity with
/// `PUNKTFUNK_NO_AMF_LTR`); loss recovery then always falls back to IDR.
fn ltr_disabled() -> bool {
    // Same accepted spellings as AMF's `ltr_disabled` — this had dropped the `trim()` and the
    // `yes`/`on` forms, so a value with stray whitespace (easy to produce with `set VAR=1 `)
    // silently left LTR enabled on Intel while the identical value worked on AMD.
    std::env::var("PUNKTFUNK_NO_QSV_LTR")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Frames between LTR marks (`PUNKTFUNK_LTR_INTERVAL_FRAMES`, shared with AMF); default ~1/4 s
/// so a loss usually finds a slot only a few frames old.
fn ltr_mark_interval(fps: u32) -> i64 {
    std::env::var("PUNKTFUNK_LTR_INTERVAL_FRAMES")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or_else(|| (fps as i64 / 4).max(1))
}

/// Spike-only validation hook (`PUNKTFUNK_LTR_FORCE_AT=N`, shared with AMF): self-trigger the
/// real `invalidate_ref_frames` path at frame N so a headless run exercises mark → force →
/// recovery-anchor without a live client.
fn ltr_test_force_at() -> Option<i64> {
    std::env::var("PUNKTFUNK_LTR_FORCE_AT")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
}

/// Mirrors [`super::amf`]'s `PUNKTFUNK_INTRA_REFRESH` opt-in: request the intra-refresh wave
/// instead of LTR (mutually exclusive — the wave sweeps the whole picture, LTR pins references).
fn intra_refresh_requested() -> bool {
    // Spelling parity with AMF (see `ltr_disabled` above).
    std::env::var("PUNKTFUNK_INTRA_REFRESH")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// The wave period in frames (~0.5 s), `PUNKTFUNK_IR_PERIOD_FRAMES` overrides — the same knob and
/// default as AMF / Linux NVENC. (This claimed parity while ignoring the env var entirely, so the
/// knob silently did nothing on Intel; the clamp is kept because `mfxU16` bounds the field.)
fn intra_refresh_period(fps: u32) -> u16 {
    std::env::var("PUNKTFUNK_IR_PERIOD_FRAMES")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|v| *v >= 2)
        .unwrap_or(fps / 2)
        .clamp(8, 240) as u16
}

// ---------------------------------------------------------------------------------------------
// Dispatcher plumbing: loader/session guards + Intel-implementation enumeration.
// ---------------------------------------------------------------------------------------------

/// Owned `mfxLoader` (dispatcher instance) — `MFXUnload` on drop.
struct Loader(vpl::mfxLoader);
impl Drop for Loader {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `MFXLoad` and is dropped exactly once.
            unsafe { vpl::MFXUnload(self.0) };
        }
    }
}

/// Owned `mfxSession` — `MFXClose` on drop (closes the encoder too if still open).
struct Session(vpl::mfxSession);
impl Drop for Session {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `MFXCreateSession` and is dropped exactly
            // once; MFXClose tears down any component still initialised on it.
            unsafe { vpl::MFXClose(self.0) };
        }
    }
}

/// One dispatcher-enumerated hardware implementation: its index (for `MFXCreateSession`) and
/// the adapter LUID it lives on (`mfxExtendedDeviceId`, zero when the runtime couldn't say).
struct VplImpl {
    index: u32,
    luid: [u8; 8],
    luid_valid: bool,
}

/// Create a loader filtered to **Intel hardware** implementations and enumerate them. The
/// filter properties go through per-property `mfxConfig` objects (the dispatcher contract:
/// one property per config handle).
fn intel_loader() -> Result<(Loader, Vec<VplImpl>)> {
    // SAFETY: plain dispatcher C calls on handles owned by this function. Each config handle is
    // owned by the loader (released by MFXUnload); the `mfxVariant`s are by-value. The
    // enumeration loop releases every description handle it obtained via
    // `MFXDispReleaseImplDescription` before returning.
    unsafe {
        let loader = Loader(vpl::MFXLoad());
        if loader.0.is_null() {
            bail!("MFXLoad returned null (dispatcher out of memory?)");
        }
        for (name, value) in [
            (
                b"mfxImplDescription.Impl\0".as_slice(),
                vpl::MFX_IMPL_TYPE_HARDWARE as u32,
            ),
            (b"mfxImplDescription.VendorID\0".as_slice(), 0x8086u32),
        ] {
            let cfg = vpl::MFXCreateConfig(loader.0);
            if cfg.is_null() {
                bail!("MFXCreateConfig returned null");
            }
            let mut var: vpl::mfxVariant = std::mem::zeroed();
            var.Type = vpl::MFX_VARIANT_TYPE_U32;
            var.Data = vpl::mfxVariant_data { U32: value };
            vpl_ok(
                vpl::MFXSetConfigFilterProperty(cfg, name.as_ptr(), var),
                "MFXSetConfigFilterProperty",
            )?;
        }
        let mut impls = Vec::new();
        for i in 0u32.. {
            let mut hdl: vpl::mfxHDL = ptr::null_mut();
            let sts = vpl::MFXEnumImplementations(
                loader.0,
                i,
                vpl::MFX_IMPLCAPS_DEVICE_ID_EXTENDED,
                &mut hdl,
            );
            if sts != vpl::MFX_ERR_NONE || hdl.is_null() {
                break; // MFX_ERR_NOT_FOUND past the last implementation
            }
            let dev = &*(hdl as *const vpl::mfxExtendedDeviceId);
            impls.push(VplImpl {
                index: i,
                luid: dev.DeviceLUID,
                luid_valid: dev.LUIDValid != 0,
            });
            let _ = vpl::MFXDispReleaseImplDescription(loader.0, hdl);
        }
        Ok((loader, impls))
    }
}

/// The DXGI adapter LUID of a live D3D11 device, as the little-endian byte layout
/// `mfxExtendedDeviceId::DeviceLUID` uses (a straight memcpy of the Windows `LUID`).
fn device_luid(device: &ID3D11Device) -> Option<[u8; 8]> {
    // SAFETY: standard COM navigation on a live device; every interface is an owned
    // windows-rs wrapper released on drop, and `GetDesc` fills a plain out-struct.
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let desc = dxgi.GetAdapter().ok()?.GetDesc().ok()?;
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
        b[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
        Some(b)
    }
}

/// Create a session on the implementation living on `luid` (the capture device's adapter —
/// the same-device requirement every backend enforces). `None` LUID (or no match) picks the
/// first Intel implementation, which is only correct on the boxes where there is exactly one.
fn create_session(target_luid: Option<[u8; 8]>) -> Result<(Loader, Session, (u16, u16))> {
    let (loader, impls) = intel_loader()?;
    if impls.is_empty() {
        bail!("no Intel hardware VPL implementation (no Intel GPU/driver on this box?)");
    }
    let chosen = target_luid
        .and_then(|want| impls.iter().find(|i| i.luid_valid && i.luid == want))
        .unwrap_or(&impls[0]);
    if let Some(want) = target_luid {
        if !(chosen.luid_valid && chosen.luid == want) {
            // Static configuration mismatch — the capture adapter has no Intel VPL
            // implementation, so every rebuild re-hits this same wall. The terminal marker
            // makes the stream loop end the session at once instead of spending its reset
            // budget (5 futile rebuilds, then an opaque loop as the client reconnects).
            return Err(anyhow::Error::new(super::TerminalEncoderError).context(
                "capture device's adapter is not an Intel VPL implementation (hybrid box? \
                     point PUNKTFUNK_RENDER_ADAPTER / the web-console GPU preference at the \
                     Intel adapter for a QSV session)",
            ));
        }
    }
    // SAFETY: `loader.0` is live; `MFXCreateSession` fills `session` only on success and the
    // guard closes it exactly once. `MFXQueryVersion` fills a plain out-struct on the live
    // session.
    unsafe {
        let mut session: vpl::mfxSession = ptr::null_mut();
        vpl_ok(
            vpl::MFXCreateSession(loader.0, chosen.index, &mut session),
            "MFXCreateSession",
        )?;
        let session = Session(session);
        let mut ver: vpl::mfxVersion = std::mem::zeroed();
        let _ = vpl::MFXQueryVersion(session.0, &mut ver);
        let api = (ver.__bindgen_anon_1.Major, ver.__bindgen_anon_1.Minor);
        Ok((loader, session, api))
    }
}

// ---------------------------------------------------------------------------------------------
// Parameter construction. The ext-buffer chain is owned by `ParamSet` so every pointer the
// `mfxVideoParam` hands to the runtime stays alive for the duration of the Init/Query/Reset
// call it is used in.
// ---------------------------------------------------------------------------------------------

/// `NumRefFrame`: 2 short-term + the 2 LTR slots.
const NUM_REF_FRAMES: u16 = 4;

/// A fully-built `mfxVideoParam` + the ext-buffer storage its `ExtParam` points into.
struct ParamSet {
    par: vpl::mfxVideoParam,
    co: Box<vpl::mfxExtCodingOption>,
    co2: Option<Box<vpl::mfxExtCodingOption2>>,
    vsi: Option<Box<vpl::mfxExtVideoSignalInfo>>,
    mastering: Option<Box<vpl::mfxExtMasteringDisplayColourVolume>>,
    cll: Option<Box<vpl::mfxExtContentLightLevelInfo>>,
    ptrs: Vec<*mut vpl::mfxExtBuffer>,
}

impl ParamSet {
    /// (Re)collect the ext-buffer pointer array and wire it into `par`. Must be called after
    /// any change to which buffers exist; the boxes give each buffer a stable address.
    fn seal(&mut self) {
        self.ptrs.clear();
        self.ptrs
            .push(&mut self.co.Header as *mut vpl::mfxExtBuffer);
        if let Some(b) = self.co2.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.vsi.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.mastering.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.cll.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        self.par.ExtParam = self.ptrs.as_mut_ptr();
        self.par.NumExtParam = self.ptrs.len() as u16;
    }
}

struct EncodeConfig {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// Attach the intra-refresh wave (CO2) instead of running LTR.
    intra_refresh: bool,
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
}

fn codec_id(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => vpl::MFX_CODEC_AVC as u32,
        Codec::H265 => vpl::MFX_CODEC_HEVC as u32,
        Codec::Av1 => vpl::MFX_CODEC_AV1 as u32,
        Codec::PyroWave => unreachable!("PyroWave never opens the QSV backend"),
    }
}

/// Build the low-latency parameter block (design §3.3): `AsyncDepth=1`, no B-frames, VDEnc,
/// CBR with HRD conformance off (the no-IDR bitrate-reset prerequisite), effectively-infinite
/// GOP — IDRs happen on demand via `mfxEncodeCtrl` only.
fn build_params(cfg: &EncodeConfig) -> ParamSet {
    // SAFETY: all-zero is the documented initial state for every VPL parameter struct; fields
    // are then set through the typed accessors.
    let mut par: vpl::mfxVideoParam = unsafe { std::mem::zeroed() };
    par.AsyncDepth = 1;
    par.IOPattern = vpl::MFX_IOPATTERN_IN_VIDEO_MEMORY as u16;
    let mfx = mfx_of(&mut par);
    mfx.CodecId = codec_id(cfg.codec);
    mfx.LowPower = vpl::MFX_CODINGOPTION_ON as u16;
    mfx.CodecProfile = match (cfg.codec, cfg.ten_bit) {
        (Codec::H264, _) => vpl::MFX_PROFILE_AVC_HIGH as u16,
        (Codec::H265, false) => vpl::MFX_PROFILE_HEVC_MAIN as u16,
        (Codec::H265, true) => vpl::MFX_PROFILE_HEVC_MAIN10 as u16,
        (Codec::Av1, _) => vpl::MFX_PROFILE_AV1_MAIN as u16,
        (Codec::PyroWave, _) => unreachable!("PyroWave never opens the QSV backend"),
    };
    let (kbps, mult) = split_rate(cfg.bitrate_bps);
    mfx.BRCParamMultiplier = mult;
    {
        let e = enc_of(mfx);
        e.TargetUsage = vpl::MFX_TARGETUSAGE_BEST_SPEED as u16;
        e.GopPicSize = u16::MAX; // effectively infinite — IDR on demand only
        e.GopRefDist = 1; // no B-frames (latency + FIFO pairing contract)
        e.IdrInterval = 0;
        e.RateControlMethod = vpl::MFX_RATECONTROL_CBR as u16;
        e.NumRefFrame = NUM_REF_FRAMES;
        set_target_kbps(e, kbps);
        set_max_kbps(e, kbps);
    }
    let info = &mut mfx.FrameInfo;
    info.FourCC = if cfg.ten_bit {
        vpl::MFX_FOURCC_P010 as u32
    } else {
        vpl::MFX_FOURCC_NV12 as u32
    };
    if cfg.ten_bit {
        info.BitDepthLuma = 10;
        info.BitDepthChroma = 10;
        info.Shift = 1; // P010 is MSB-aligned
    }
    info.ChromaFormat = vpl::MFX_CHROMAFORMAT_YUV420 as u16;
    info.PicStruct = vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
    info.FrameRateExtN = cfg.fps.max(1);
    info.FrameRateExtD = 1;
    {
        let wh = frame_wh(info);
        wh.Width = align16(cfg.width);
        wh.Height = align16(cfg.height);
        wh.CropW = cfg.width as u16;
        wh.CropH = cfg.height as u16;
    }

    // mfxExtCodingOption: HRD conformance OFF — the spec's prerequisite for a bitrate Reset
    // that "will not result in the generation of a new keyframe or sequence header".
    // SAFETY: all-zero is valid for every ext buffer; the header is then stamped.
    let mut co: Box<vpl::mfxExtCodingOption> = Box::new(unsafe { std::mem::zeroed() });
    co.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION as u32;
    co.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption>() as u32;
    co.NalHrdConformance = vpl::MFX_CODINGOPTION_OFF as u16;
    co.VuiNalHrdParameters = vpl::MFX_CODINGOPTION_OFF as u16;
    co.MaxDecFrameBuffering = NUM_REF_FRAMES;

    // Intra-refresh wave (opt-in; AVC/HEVC only — the AV1 runtime has no IntRefType at all).
    let co2 = (cfg.intra_refresh && matches!(cfg.codec, Codec::H264 | Codec::H265)).then(|| {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtCodingOption2> = Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION2 as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption2>() as u32;
        b.IntRefType = 1; // vertical wave
        b.IntRefCycleSize = intra_refresh_period(cfg.fps);
        b
    });

    // Colour signalling, written UNCONDITIONALLY (mirrors nvenc_core.rs): the input is already
    // CSC'd to a specific matrix — BT.709 limited for SDR (the capture-side VideoConverter),
    // BT.2020 PQ for HDR (HdrP010Converter) — so the stream must say so. An SDR stream without a
    // colour description leaves the choice to the decoder's "unspecified" default, and
    // Moonlight/third-party/Android-vendor decoders default to 601 at sub-HD → mis-rendered
    // colours. (10-bit sessions are the HDR path on Windows — same coupling as NVENC.)
    let hdr = cfg.ten_bit && cfg.codec != Codec::H264;
    let vsi = {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtVideoSignalInfo> = Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_VIDEO_SIGNAL_INFO as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtVideoSignalInfo>() as u32;
        b.VideoFormat = 5; // unspecified
        b.VideoFullRange = 0;
        b.ColourDescriptionPresent = 1;
        if hdr {
            b.ColourPrimaries = 9; // BT.2020
            b.TransferCharacteristics = 16; // SMPTE ST 2084 (PQ)
            b.MatrixCoefficients = 9; // BT.2020 non-constant
        } else {
            b.ColourPrimaries = 1; // BT.709
            b.TransferCharacteristics = 1; // BT.709
            b.MatrixCoefficients = 1; // BT.709
        }
        Some(b)
    };
    let mastering = cfg.hdr_meta.filter(|_| hdr).map(|m| {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtMasteringDisplayColourVolume> =
            Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_MASTERING_DISPLAY_COLOUR_VOLUME as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtMasteringDisplayColourVolume>() as u32;
        b.InsertPayloadToggle = vpl::MFX_PAYLOAD_IDR as u16;
        // HdrMeta carries the ST.2086 G,B,R primary order in 1/50000 units — the same order and
        // units as the SEI fields VPL mirrors, so the chromaticities copy straight through.
        for (i, p) in m.display_primaries.iter().enumerate() {
            b.DisplayPrimariesX[i] = p[0];
            b.DisplayPrimariesY[i] = p[1];
        }
        b.WhitePointX = m.white_point[0];
        b.WhitePointY = m.white_point[1];
        // BOTH luminance fields are 0.0001 cd/m² here — do NOT scale the max.
        //
        // The `/10_000` this used to carry followed the VPL header's *video-processing* unit
        // (whole cd/m², which is right for VPP), but on the ENCODE path the runtime passes these
        // straight into the ITU-T H.265 Annex D mastering-display SEI, whose unit is 0.0001 cd/m².
        // Dividing therefore under-reported peak brightness by 10,000x. Confirmed in a real
        // bitstream on Intel UHD 750 (2026-07-25): SEI 137 carried
        // max_display_mastering_luminance = 1000, i.e. **0.1 nits** for a 1000-nit display, while
        // the undivided min = 500 (0.05 nits) was correct — the asymmetry was the tell. A client
        // tone-mapping against 0.1 nits crushes the image.
        //
        // Unscaled also matches every sibling: `MinDisplayMasteringLuminance` right below,
        // `pf_frame::hdr::hevc_mastering_display_sei`, and the AMF backend.
        b.MaxDisplayMasteringLuminance = m.max_display_mastering_luminance;
        b.MinDisplayMasteringLuminance = m.min_display_mastering_luminance;
        b
    });
    let cll = cfg
        .hdr_meta
        .filter(|_| hdr)
        .filter(|m| m.max_cll != 0 || m.max_fall != 0)
        .map(|m| {
            // SAFETY: all-zero is valid; header stamped below.
            let mut b: Box<vpl::mfxExtContentLightLevelInfo> =
                Box::new(unsafe { std::mem::zeroed() });
            b.Header.BufferId = vpl::MFX_EXTBUFF_CONTENT_LIGHT_LEVEL_INFO as u32;
            b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtContentLightLevelInfo>() as u32;
            b.InsertPayloadToggle = vpl::MFX_PAYLOAD_IDR as u16;
            b.MaxContentLightLevel = m.max_cll;
            b.MaxPicAverageLightLevel = m.max_fall;
            b
        });

    let mut set = ParamSet {
        par,
        co,
        co2,
        vsi,
        mastering,
        cll,
        ptrs: Vec::new(),
    };
    set.seal();
    set
}

/// A zeroed `mfxExtRefListCtrl` with every list entry marked unused
/// (`FrameOrder = MFX_FRAMEORDER_UNKNOWN`) — the required idle state.
fn empty_reflist() -> vpl::mfxExtRefListCtrl {
    // SAFETY: all-zero is a valid `mfxExtRefListCtrl`; the header + sentinel FrameOrders are
    // stamped before use.
    let mut r: vpl::mfxExtRefListCtrl = unsafe { std::mem::zeroed() };
    r.Header.BufferId = vpl::MFX_EXTBUFF_UNIVERSAL_REFLIST_CTRL as u32;
    r.Header.BufferSz = std::mem::size_of::<vpl::mfxExtRefListCtrl>() as u32;
    let unknown = vpl::MFX_FRAMEORDER_UNKNOWN as u32;
    for e in r.PreferredRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    for e in r.RejectedRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    for e in r.LongTermRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    r
}

/// Per-frame encode control: the `mfxEncodeCtrl` plus the ext buffers it points at. Boxed and
/// kept alive in the in-flight FIFO until the frame's sync point completes — EncodeFrameAsync
/// copies the ctrl itself but NOT the attached ext buffers.
struct FrameCtrl {
    ctrl: vpl::mfxEncodeCtrl,
    reflist: vpl::mfxExtRefListCtrl,
    ptrs: [*mut vpl::mfxExtBuffer; 1],
}

impl FrameCtrl {
    fn new() -> Box<Self> {
        // SAFETY: all-zero is valid for `mfxEncodeCtrl` (no ext buffers attached, no forced
        // type); the reflist starts as the sentinel idle state and the pointer array is wired
        // only when the reflist is actually used.
        let ctrl: vpl::mfxEncodeCtrl = unsafe { std::mem::zeroed() };
        let mut b = Box::new(FrameCtrl {
            ctrl,
            reflist: empty_reflist(),
            ptrs: [ptr::null_mut()],
        });
        b.ptrs[0] = &mut b.reflist.Header as *mut vpl::mfxExtBuffer;
        b
    }

    fn attach_reflist(&mut self) {
        self.ctrl.ExtParam = self.ptrs.as_mut_ptr();
        self.ctrl.NumExtParam = 1;
    }
}

/// One in-flight frame: its sync point, the bitstream buffer the AU lands in, the FIFO
/// metadata, and the ctrl keeping per-frame ext buffers alive until synced.
struct Pending {
    syncp: vpl::mfxSyncPoint,
    bs: Box<BsBuf>,
    pts_ns: u64,
    forced: bool,
    recovery_anchor: bool,
    _ctrl: Option<Box<FrameCtrl>>,
}

/// An output bitstream buffer: the backing bytes + the `mfxBitstream` describing them. Boxed so
/// the `Data` pointer stays stable while the runtime writes into it asynchronously.
struct BsBuf {
    buf: Vec<u8>,
    mfx: vpl::mfxBitstream,
}

impl BsBuf {
    fn new(capacity: usize) -> Box<Self> {
        // SAFETY: all-zero is a valid `mfxBitstream`; Data/MaxLength are wired below.
        let mfx: vpl::mfxBitstream = unsafe { std::mem::zeroed() };
        let mut b = Box::new(BsBuf {
            buf: vec![0u8; capacity],
            mfx,
        });
        b.mfx.Data = b.buf.as_mut_ptr();
        b.mfx.MaxLength = capacity as u32;
        b
    }

    fn recycle(&mut self) {
        self.mfx.DataOffset = 0;
        self.mfx.DataLength = 0;
    }
}

/// How many frames may be in flight before `submit` drains output to make room — with
/// `AsyncDepth=1` the steady state is 1, this only bounds a falling-behind encoder.
const IN_FLIGHT_MAX: usize = 4;

/// How long `submit` waits out `MFX_WRN_DEVICE_BUSY` / a full in-flight window before declaring
/// a wedge — same shape as the AMF drain budget: generous vs a frame's encode time, far under
/// the session watchdog's ~2 s floor.
const BUSY_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

struct Inner {
    /// Drop order matters: the session (with its open encoder) must close before the loader
    /// unloads the runtime — struct fields drop in declaration order.
    session: Session,
    _loader: Loader,
    /// The capturer's device this session is bound to (SetHandle at bring-up).
    _device: ID3D11Device,
    /// That device's immediate context, for the GPU-side copy into the runtime's surface.
    dctx: ID3D11DeviceContext,
    /// In-flight frames, FIFO — GopRefDist=1 means AUs complete in submit order.
    pending: VecDeque<Pending>,
    /// AUs already synced by `submit`'s backpressure drain, waiting for `poll`.
    ready: VecDeque<EncodedFrame>,
    /// Recycled bitstream buffers. Boxed although the Vec is heap-backed: the `mfxBitstream`
    /// address must stay stable while a buffer is in flight (`Pending` moves the SAME box out
    /// of/into this pool, and the runtime holds `&mut mfx` across the async encode).
    #[allow(clippy::vec_box)]
    bs_pool: Vec<Box<BsBuf>>,
    /// Per-AU output buffer size (from `GetVideoParam` post-Init).
    bs_bytes: usize,
    frames_submitted: u64,
    first_au_logged: bool,
    /// Warn once if the runtime hands out array textures (subresource-0 copy would be wrong).
    array_warned: bool,
}

impl Inner {
    fn note_first_au(&mut self, au: &EncodedFrame) {
        if !self.first_au_logged {
            self.first_au_logged = true;
            tracing::info!(
                bytes = au.data.len(),
                keyframe = au.keyframe,
                "QSV produced its first AU on this session"
            );
        }
    }

    fn take_bs(&mut self) -> Box<BsBuf> {
        // Pooled buffers were sized by whatever `bs_bytes` was when they were allocated. A bitrate
        // retarget raises the driver's worst-case AU size, so a recycled buffer can be SMALLER than
        // the current requirement — hand those back to the allocator instead of letting the runtime
        // write an AU into a short buffer.
        while let Some(mut b) = self.bs_pool.pop() {
            if b.mfx.MaxLength as usize >= self.bs_bytes {
                b.recycle();
                return b;
            }
        }
        BsBuf::new(self.bs_bytes)
    }
}

pub struct QsvEncoder {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// Built lazily from the first frame's device; rebuilt when the capturer's device changes
    /// — the same lifecycle as the NVENC/AMF backends.
    inner: Option<Inner>,
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// The HDR metadata baked into the live encoder's Init params, so a post-Init change can
    /// trigger the (IDR-carrying) re-Init that refreshes the in-band SEI/OBU.
    hdr_applied: Option<punktfunk_core::quic::HdrMeta>,
    /// The driver accepted intra-refresh at Query — gates `EncoderCaps::intra_refresh`.
    ir_active: bool,
    // --- LTR-RFI recovery (the mfxExtRefListCtrl port of the AMF slot policy, design §5) ---
    /// `mfxExtRefListCtrl` passed the per-codec Query gate at bring-up — gates
    /// `EncoderCaps::supports_rfi` and all per-frame marking/forcing below.
    ltr_active: bool,
    /// The wire frame index stored in each LTR slot (`None` = never marked).
    ///
    /// This mirrors the HARDWARE DPB, so an entry must not be cleared merely because we distrust
    /// it: nulling issues no VPL call, and the encoder keeps the frame marked long-term until that
    /// `LongTermIdx` is re-marked or an IDR flushes it. Distrust is recorded in `ltr_tainted`
    /// instead, so the rejection list can still NAME the entry the hardware is holding.
    ltr_slots: [Option<i64>; NUM_LTR_SLOTS],
    /// Per-slot taint from `invalidate_ref_frames`' sweep: the mark is still live in the hardware
    /// DPB but was encoded inside the client's corrupt window, so it may not anchor a recovery —
    /// it must be REJECTED instead. Cleared wherever the slot is re-marked or the DPB is flushed.
    ltr_tainted: [bool; NUM_LTR_SLOTS],
    next_ltr_slot: usize,
    ltr_mark_interval: i64,
    /// Set by `invalidate_ref_frames`: the slot the next submitted frame force-references.
    pending_force: Option<usize>,
    ltr_test_force_at: Option<i64>,
    /// Consecutive `reset()`s without a subsequent AU — escalates the in-place Close+Init to a
    /// full session teardown, the same ladder as the AMF reconnect-wedge recovery.
    resets_without_output: u32,
}

// SAFETY: `QsvEncoder` owns raw VPL handles (loader/session/sync points) and windows-rs COM
// handles that are not auto-`Send`. The session glue creates the encoder, drives
// `submit`/`poll`/`flush`/`reset`, and drops it all on one dedicated encode thread; it is never
// shared by reference across threads, and the D3D11 immediate context is only ever touched from
// that thread — the same contract the NVENC/AMF/ffmpeg backends rely on.
unsafe impl Send for QsvEncoder {}

impl QsvEncoder {
    /// Open the native QSV encoder. Fails cleanly when: no Intel hardware VPL implementation
    /// exists (no Intel GPU/driver), the codec fails its probe (AV1 pre-DG2/MTL), or the
    /// capture format is not the zero-copy NV12/P010 path.
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
        if codec == Codec::PyroWave {
            bail!("PyroWave never opens the QSV backend");
        }
        // AV1 is DG2/Arc + MTL and later — probe at open (never assume) so an older box fails
        // HERE with a clear reason. The codec advertisement gates on the same probe.
        if codec == Codec::Av1 && !probe_can_encode(Codec::Av1) {
            bail!("this GPU/driver declined AV1 encode (DG2/Arc or MTL+ required) — QSV probe");
        }
        // Depth follows the delivered pixels, not the negotiated depth ([`crate::ten_bit_input`]).
        // With the old `bit_depth >= 10 || …` shape a 10-bit-negotiated session over an 8-bit
        // capture derived `expected = P010` and hit the bail below, ending the session at open —
        // and taking the ffmpeg fallback with it, since that path had the same defect in a worse
        // form (it accepted the open and then failed every frame).
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        if ten_bit && codec == Codec::H264 {
            bail!("native QSV: 10-bit is HEVC/AV1-only (H.264 High10 is not negotiated)");
        }
        let expected = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if format != expected {
            bail!(
                "native QSV needs the video-processor {expected:?} capture path; capturer \
                 delivered {format:?} (no readback path by design — zero-copy invariant)"
            );
        }
        if chroma.is_444() {
            tracing::warn!("QSV 4:4:4 is not probed/wired yet — encoding 4:2:0");
        }
        Ok(QsvEncoder {
            codec,
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
            hdr_applied: None,
            ir_active: false,
            ltr_active: false,
            ltr_slots: [None; NUM_LTR_SLOTS],
            ltr_tainted: [false; NUM_LTR_SLOTS],
            next_ltr_slot: 0,
            ltr_mark_interval: ltr_mark_interval(fps),
            pending_force: None,
            ltr_test_force_at: ltr_test_force_at(),
            resets_without_output: 0,
        })
    }

    fn ltr_wanted(&self) -> bool {
        !ltr_disabled() && !intra_refresh_requested()
    }

    fn encode_config(&self) -> EncodeConfig {
        EncodeConfig {
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_bps: self.bitrate_bps,
            ten_bit: self.ten_bit,
            intra_refresh: intra_refresh_requested(),
            hdr_meta: self.hdr_meta,
        }
    }

    /// Initialise (or re-initialise) the encoder component on a live session: Query-gate the
    /// optional features (LTR reflist, intra-refresh), Init, then size the bitstream pool from
    /// the driver's own `BufferSizeInKB` answer. Returns `(ltr_active, ir_active, bs_bytes)`.
    fn init_encode(&self, session: vpl::mfxSession) -> Result<(bool, bool, usize)> {
        let cfg = self.encode_config();
        let mut set = build_params(&cfg);
        // Query-gate mfxExtRefListCtrl per codec (design §5): attach an idle reflist to a Query
        // copy; a driver/codec that can't honor it clears or errors it. AVC/HEVC are spec'd;
        // AV1 is runtime-implemented but spec-silent — exactly why this is a gate, not an
        // assumption. (Query mutates nothing on the live encoder.)
        let ltr_active = self.ltr_wanted() && {
            let mut q_in = build_params(&cfg);
            let mut q_out = build_params(&cfg);
            let mut rl_in = Box::new(empty_reflist());
            let mut rl_out = Box::new(empty_reflist());
            q_in.ptrs.push(&mut rl_in.Header as *mut vpl::mfxExtBuffer);
            q_in.par.ExtParam = q_in.ptrs.as_mut_ptr();
            q_in.par.NumExtParam = q_in.ptrs.len() as u16;
            q_out
                .ptrs
                .push(&mut rl_out.Header as *mut vpl::mfxExtBuffer);
            q_out.par.ExtParam = q_out.ptrs.as_mut_ptr();
            q_out.par.NumExtParam = q_out.ptrs.len() as u16;
            // SAFETY: `session` is live; both param blocks and their ext chains outlive the
            // synchronous call (owned locals above).
            let sts = unsafe { vpl::MFXVideoENCODE_Query(session, &mut q_in.par, &mut q_out.par) };
            let ok = sts >= vpl::MFX_ERR_NONE;
            if !ok {
                tracing::info!(
                    codec = ?self.codec,
                    status = sts_name(sts),
                    "QSV declined mfxExtRefListCtrl — loss recovery falls back to IDR"
                );
            }
            ok
        };
        // Init validates the whole block; warnings (corrected params, partial acceleration)
        // are logged but accepted.
        // SAFETY: `session` is live; `set` (params + ext chain) outlives the synchronous call.
        let sts = unsafe { vpl::MFXVideoENCODE_Init(session, &mut set.par) };
        vpl_ok(sts, "MFXVideoENCODE_Init")?;
        if sts > vpl::MFX_ERR_NONE {
            tracing::debug!(status = sts_name(sts), "QSV Init returned a warning");
        }
        // Whether we ASKED for intra-refresh. Not the same as getting it — confirmed below.
        let ir_requested = cfg.intra_refresh && set.co2.is_some();
        // The driver's own answer for the worst-case AU size.
        // SAFETY: `session` is live; `got` and its (empty) ext chain outlive the call.
        let bs_bytes = unsafe {
            let mut got: vpl::mfxVideoParam = std::mem::zeroed();
            vpl_ok(
                vpl::MFXVideoENCODE_GetVideoParam(session, &mut got),
                "MFXVideoENCODE_GetVideoParam",
            )?;
            let m = &mut got.__bindgen_anon_1.mfx;
            let mult = m.BRCParamMultiplier.max(1) as usize;
            let kb = enc_of(m).BufferSizeInKB as usize;
            (kb * mult * 1000).max(256 * 1024)
        };
        // Intra-refresh HONESTY: ask the driver what it actually installed instead of trusting that
        // it took what we sent. Both `Query` and `Init` can return MFX_WRN_INCOMPATIBLE_VIDEO_PARAM
        // — a WARNING, which the code above accepts — while silently dropping the wave. Confirmed
        // on Intel UHD 750 (2026-07-25): H.264 reports exactly that, `GetVideoParam` comes back with
        // `IntRefType = 0`, and `ir_active` still claimed true. H.265 on the same GPU is genuinely
        // active, so this is per-codec and cannot be decided statically.
        //
        // That lie is not cosmetic: `ir_active` feeds `EncoderCaps::intra_refresh`, so the session
        // advertises gradual refresh to the client and then never emits it — the client stops
        // requesting the IDRs it would otherwise have asked for on loss, and a lost frame is
        // concealed instead of repaired.
        //
        // Deliberately a SEPARATE, best-effort query rather than a buffer chained onto the
        // `BufferSizeInKB` call above (which is what the audit finding suggested): that value is
        // load-bearing for every bitstream allocation, and a runtime that dislikes the chained
        // buffer would take it down with the readback. A failure here costs only the verdict, and
        // is resolved conservatively.
        let ir_active = if ir_requested {
            // SAFETY: `session` is live on this thread; `got` and `co2_out` (with `ptrs` holding the
            // only reference to it) all outlive the synchronous call, and the runtime writes back
            // only into the buffer whose header we stamped.
            let confirmed = unsafe {
                let mut got: vpl::mfxVideoParam = std::mem::zeroed();
                let mut co2_out: vpl::mfxExtCodingOption2 = std::mem::zeroed();
                co2_out.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION2 as u32;
                co2_out.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption2>() as u32;
                let mut ptrs: [*mut vpl::mfxExtBuffer; 1] = [&mut co2_out.Header as *mut _];
                got.ExtParam = ptrs.as_mut_ptr();
                got.NumExtParam = 1;
                let sts = vpl::MFXVideoENCODE_GetVideoParam(session, &mut got);
                if sts < vpl::MFX_ERR_NONE {
                    tracing::debug!(
                        status = sts_name(sts),
                        "QSV: could not read back CodingOption2 — trusting the intra-refresh request"
                    );
                    true
                } else {
                    co2_out.IntRefType != 0
                }
            };
            if !confirmed {
                tracing::warn!(
                    codec = ?cfg.codec,
                    "QSV silently dropped intra-refresh (GetVideoParam reports IntRefType=0) — \
                     advertising it OFF so the client keeps asking for IDRs on loss"
                );
            }
            confirmed
        } else {
            false
        };
        Ok((ltr_active, ir_active, bs_bytes))
    }

    /// Lazy bring-up on the capturer's device (rebuilt on device change): dispatcher session on
    /// the device's own adapter, `SetHandle`, multithread-protect the D3D11 context, Init.
    fn ensure_inner(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let luid = device_luid(device);
        let (loader, session, api) = create_session(luid)?;
        // SAFETY: `session.0` is live; `device.as_raw()` is a borrowed live COM pointer for the
        // synchronous SetHandle (the runtime AddRefs what it keeps). The multithread-protect QI
        // is standard COM on the owned immediate context.
        let dctx = unsafe {
            let dctx = device
                .GetImmediateContext()
                .context("ID3D11Device immediate context")?;
            // The runtime touches the device from its own threads — multithread protection
            // must be ON **before** SetHandle or the runtime rejects the device with
            // MFX_ERR_UNDEFINED_BEHAVIOR (-16, seen on-glass on Arc).
            if let Ok(mt) = dctx.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            vpl_ok(
                vpl::MFXVideoCORE_SetHandle(
                    session.0,
                    vpl::MFX_HANDLE_D3D11_DEVICE,
                    device.as_raw(),
                ),
                "MFXVideoCORE_SetHandle(D3D11)",
            )?;
            dctx
        };
        let (ltr_active, ir_active, bs_bytes) = self.init_encode(session.0)?;
        self.ltr_active = ltr_active;
        self.ir_active = ir_active;
        self.ltr_slots = [None; NUM_LTR_SLOTS];
        self.ltr_tainted = [false; NUM_LTR_SLOTS];
        self.next_ltr_slot = 0;
        self.pending_force = None;
        self.hdr_applied = self.hdr_meta;
        tracing::info!(
            codec = ?self.codec,
            width = self.width,
            height = self.height,
            fps = self.fps,
            ten_bit = self.ten_bit,
            ltr = ltr_active,
            intra_refresh = ir_active,
            api = %format_args!("{}.{}", api.0, api.1),
            device = %format_args!("{:#x}", dev_raw as usize),
            "native QSV encode active (VPL, zero-copy D3D11)"
        );
        self.inner = Some(Inner {
            session,
            _loader: loader,
            _device: device.clone(),
            dctx,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            bs_pool: Vec::new(),
            bs_bytes,
            frames_submitted: 0,
            first_au_logged: false,
            array_warned: false,
        });
        Ok(())
    }
}

/// Sync the OLDEST in-flight frame with `wait_ms`, FIFO-pairing bitstream and metadata.
/// `Ok(Some)` = an AU; `Ok(None)` = not ready yet; `Err` = typed failure (caller resets).
fn sync_one(inner: &mut Inner, wait_ms: u32) -> Result<Option<EncodedFrame>> {
    let Some(front) = inner.pending.front() else {
        return Ok(None);
    };
    // SAFETY: `session` is live and `syncp` belongs to an operation submitted on it that has
    // not been synced yet (entries leave `pending` exactly once, below).
    let sts = unsafe { vpl::MFXVideoCORE_SyncOperation(inner.session.0, front.syncp, wait_ms) };
    match sts {
        vpl::MFX_WRN_IN_EXECUTION => Ok(None),
        s if s < vpl::MFX_ERR_NONE => {
            bail!("MFXVideoCORE_SyncOperation failed: {} ({s})", sts_name(s))
        }
        _ => {
            let done = inner.pending.pop_front().expect("front checked above");
            let bs = &done.bs.mfx;
            let off = bs.DataOffset as usize;
            let len = bs.DataLength as usize;
            if off + len > done.bs.buf.len() {
                bail!(
                    "QSV bitstream out of bounds: offset {off} + length {len} > buffer {}",
                    done.bs.buf.len()
                );
            }
            let data = done.bs.buf[off..off + len].to_vec();
            let key_flag =
                bs.FrameType & (vpl::MFX_FRAMETYPE_IDR as u16 | vpl::MFX_FRAMETYPE_I as u16) != 0;
            let au = EncodedFrame {
                data,
                pts_ns: done.pts_ns,
                keyframe: key_flag || done.forced,
                recovery_anchor: done.recovery_anchor,
                chunk_aligned: false,
            };
            let mut bs_box = done.bs;
            bs_box.recycle();
            inner.bs_pool.push(bs_box);
            Ok(Some(au))
        }
    }
}

impl Encoder for QsvEncoder {
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
                bail!("native QSV is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        let expected = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        anyhow::ensure!(
            captured.format == expected,
            "captured format {:?} != QSV input {:?} (capturer video-processor fallback \
             mid-session — native QSV has no readback path)",
            captured.format,
            expected
        );
        self.ensure_inner(&frame.device)?;
        // A mid-stream HDR regrade re-inits the encoder so the new mastering SEI/OBU rides the
        // (unavoidable anyway) fresh IDR. Rare — the grade is static per source.
        if self.ten_bit && self.hdr_meta != self.hdr_applied && self.hdr_meta.is_some() {
            tracing::info!("QSV HDR metadata changed — re-initialising the encoder");
            self.inner = None;
            self.bound_device = 0;
            self.ensure_inner(&frame.device)?;
        }
        let cur_idx = self.frame_idx;
        let opening = self.inner.as_ref().is_none_or(|i| i.frames_submitted == 0);
        let forced = std::mem::take(&mut self.force_kf) || opening;
        self.frame_idx += 1;
        // --- LTR-RFI per-frame decisions (the AMF policy verbatim; see that module's doc) ---
        let mut mark_slot: Option<usize> = None;
        let mut force_ltr: Option<(usize, i64)> = None;
        let mut recovery_anchor = false;
        if self.ltr_active {
            if forced {
                // An IDR voids the decoder's reference buffers — drop stale slots and any
                // queued force; the mark cadence below re-anchors on the IDR itself.
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.ltr_tainted = [false; NUM_LTR_SLOTS]; // the IDR flushed the DPB with them
                self.next_ltr_slot = 0;
                self.pending_force = None;
            } else if self.ltr_test_force_at == Some(cur_idx) {
                let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
                tracing::info!(
                    frame = cur_idx,
                    triggered,
                    "QSV LTR test hook fired invalidate_ref_frames"
                );
            }
            if let Some(slot) = self.pending_force.take() {
                // Resolve the anchor NOW: a taint sweep in `invalidate_ref_frames` may have
                // emptied the slot since the force was queued. An empty slot means there is
                // nothing clean to re-reference — the frame must ship as a plain P WITHOUT the
                // `recovery_anchor` tag (the client lifts its post-loss freeze on that tag).
                // The slot is no longer emptied by the sweep, so test the taint flag too — a
                // tainted slot is exactly the "nothing clean to re-reference" case.
                if let Some(idx) = self.ltr_slots[slot].filter(|_| !self.ltr_tainted[slot]) {
                    force_ltr = Some((slot, idx));
                    recovery_anchor = true;
                }
            }
            if force_ltr.is_none() && (forced || cur_idx % self.ltr_mark_interval == 0) {
                let slot = self.next_ltr_slot;
                self.ltr_slots[slot] = Some(cur_idx);
                // Re-marking replaces the hardware's LongTermIdx: the tainted frame is gone from
                // the DPB and this slot is clean again.
                self.ltr_tainted[slot] = false;
                self.next_ltr_slot = (self.next_ltr_slot + 1) % NUM_LTR_SLOTS;
                mark_slot = Some(slot);
            }
        }
        let ltr_slots = self.ltr_slots;
        let reject_ok = self.codec != Codec::Av1;
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Bound the in-flight window BEFORE submitting: drain finished AUs (buffered for
        // `poll`) instead of letting the queue grow under overload.
        if inner.pending.len() >= IN_FLIGHT_MAX {
            let deadline = std::time::Instant::now() + BUSY_BUDGET;
            while inner.pending.len() >= IN_FLIGHT_MAX {
                match sync_one(inner, 1)? {
                    Some(au) => inner.ready.push_back(au),
                    None => {
                        if std::time::Instant::now() >= deadline {
                            self.force_kf = true;
                            bail!(
                                "QSV produced no output for {} ms with {} frame(s) in flight — \
                                 wedged (escalating to reset)",
                                BUSY_BUDGET.as_millis(),
                                inner.pending.len()
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_micros(250));
                    }
                }
            }
        }
        // SAFETY: the whole block runs on the single encode thread against the live session.
        // `MFXMemory_GetSurfaceForEncode` returns a runtime-owned surface we must Release
        // exactly once (every exit path below does). `GetNativeHandle` returns a borrowed
        // (non-AddRef'd) D3D11 texture the runtime keeps alive at least until the surface's
        // Release — the `CopySubresourceRegion` happens strictly before that. The manually
        // re-wrapped `ID3D11Texture2D::from_raw_borrowed` reference is never released by us.
        // `EncodeFrameAsync` copies `ctrl` internally; the attached ext buffers live on in the
        // `Pending` entry until the sync point completes, per the API contract.
        unsafe {
            let mut surf: *mut vpl::mfxFrameSurface1 = ptr::null_mut();
            vpl_ok(
                vpl::MFXMemory_GetSurfaceForEncode(inner.session.0, &mut surf),
                "MFXMemory_GetSurfaceForEncode",
            )?;
            if surf.is_null() {
                bail!("MFXMemory_GetSurfaceForEncode returned null");
            }
            let iface = (*surf).__bindgen_anon_1.FrameInterface;
            let release = (!iface.is_null())
                .then(|| (*iface).Release)
                .flatten()
                .ok_or_else(|| anyhow!("QSV surface has no FrameInterface.Release"))?;
            // From here on, every failure path must release the surface.
            let submit_result: Result<vpl::mfxSyncPoint> = (|| {
                let get_native = (*iface)
                    .GetNativeHandle
                    .ok_or_else(|| anyhow!("QSV surface has no GetNativeHandle"))?;
                let mut res: vpl::mfxHDL = ptr::null_mut();
                let mut res_type: vpl::mfxResourceType = 0;
                vpl_ok(
                    get_native(surf, &mut res, &mut res_type),
                    "FrameInterface.GetNativeHandle",
                )?;
                if res_type != vpl::MFX_RESOURCE_DX11_TEXTURE || res.is_null() {
                    bail!("QSV surface native handle is not a D3D11 texture (type {res_type})");
                }
                let dst = ID3D11Texture2D::from_raw_borrowed(&res)
                    .ok_or_else(|| anyhow!("QSV native handle is not ID3D11Texture2D"))?;
                if !inner.array_warned {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    dst.GetDesc(&mut desc);
                    if desc.ArraySize > 1 {
                        inner.array_warned = true;
                        tracing::warn!(
                            array_size = desc.ArraySize,
                            "QSV runtime handed out an ARRAY texture — subresource-0 copy may \
                             target the wrong slice (needs the on-glass check, design §3.4)"
                        );
                    }
                }
                let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
                let dst_res: ID3D11Resource = dst.cast().context("qsv texture -> resource")?;
                inner
                    .dctx
                    .CopySubresourceRegion(&dst_res, 0, 0, 0, 0, &src, 0, None);
                // Wire-index pinning: mfxExtRefListCtrl addresses frames by FrameOrder, so the
                // wire index IS the RFI domain — `submit_indexed` keeps them equal.
                (*surf).Data.FrameOrder = cur_idx as u32;
                (*surf).Data.TimeStamp = captured.pts_ns.wrapping_mul(9) / 100_000; // 90 kHz
                                                                                    // Per-frame control: forced IDR and/or the LTR reflist.
                let mut ctrl: Option<Box<FrameCtrl>> = None;
                if forced || mark_slot.is_some() || force_ltr.is_some() {
                    let mut c = FrameCtrl::new();
                    if forced {
                        c.ctrl.FrameType = (vpl::MFX_FRAMETYPE_IDR
                            | vpl::MFX_FRAMETYPE_I
                            | vpl::MFX_FRAMETYPE_REF)
                            as u16;
                    }
                    let mut use_reflist = false;
                    if let Some(slot) = mark_slot {
                        // Mark THIS frame as long-term reference `slot` (explicit index).
                        c.reflist.LongTermRefList[0].FrameOrder = cur_idx as u32;
                        c.reflist.LongTermRefList[0].PicStruct =
                            vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                        c.reflist.LongTermRefList[0].LongTermIdx = slot as u16;
                        c.reflist.ApplyLongTermIdx = 1;
                        use_reflist = true;
                    }
                    if let Some((slot, ltr_frame)) = force_ltr {
                        // Force THIS frame to predict only from the known-good LTR — the
                        // clean re-anchor. LongTermIdx stays 0 inside PreferredRefList
                        // (the AV1 runtime rejects nonzero there; AVC/HEVC key on
                        // FrameOrder).
                        c.reflist.PreferredRefList[0].FrameOrder = ltr_frame as u32;
                        c.reflist.PreferredRefList[0].PicStruct =
                            vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                        // A preference alone is a reorder HINT (VPL spec) — the encoder may
                        // still predict from the tainted short-term refs alongside it. AMF's
                        // ForceLTRReferenceBitfield and NVENC's invalidation are hard
                        // exclusions; emulate that here by rejecting every other DPB
                        // candidate — the short-term sliding window (the 2 most recent
                        // frames) and the other LTR slot — and capping L0 at one active
                        // entry. Without this the "clean recovery" frame can carry the
                        // corruption forward, which the client cannot detect (the field
                        // failure: permanent macroblock soup under sustained loss).
                        // AVC/HEVC only: the AV1 runtime's universal-reflist rejection path
                        // is unvalidated, and an unhonored hint there still converges via
                        // the host's IDR escalation.
                        if reject_ok {
                            let mut rej = 0;
                            let mut reject = |idx: i64| {
                                if idx >= 0 && idx != ltr_frame {
                                    c.reflist.RejectedRefList[rej].FrameOrder = idx as u32;
                                    c.reflist.RejectedRefList[rej].PicStruct =
                                        vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                                    rej += 1;
                                }
                            };
                            reject(cur_idx - 1);
                            reject(cur_idx - 2);
                            for (s, marked) in ltr_slots.iter().enumerate() {
                                if s != slot {
                                    if let Some(idx) = *marked {
                                        reject(idx);
                                    }
                                }
                            }
                            c.reflist.NumRefIdxL0Active = 1;
                        }
                        use_reflist = true;
                        tracing::info!(
                            slot,
                            ltr_frame,
                            frame = cur_idx,
                            "QSV LTR-RFI: re-referencing known-good LTR (clean recovery, \
                             no IDR)"
                        );
                    }
                    if use_reflist {
                        c.attach_reflist();
                    }
                    ctrl = Some(c);
                }
                let mut bs = inner.take_bs();
                let mut syncp: vpl::mfxSyncPoint = ptr::null_mut();
                let ctrl_ptr = ctrl
                    .as_mut()
                    .map(|c| &mut c.ctrl as *mut vpl::mfxEncodeCtrl)
                    .unwrap_or(ptr::null_mut());
                let deadline = std::time::Instant::now() + BUSY_BUDGET;
                let sts = loop {
                    let sts = vpl::MFXVideoENCODE_EncodeFrameAsync(
                        inner.session.0,
                        ctrl_ptr,
                        surf,
                        &mut bs.mfx,
                        &mut syncp,
                    );
                    if sts != vpl::MFX_WRN_DEVICE_BUSY {
                        break sts;
                    }
                    // Busy = drain and retry, bounded — not a wedge unless it never clears.
                    if let Some(au) = sync_one(inner, 1)? {
                        inner.ready.push_back(au);
                    }
                    if std::time::Instant::now() >= deadline {
                        break sts;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(250));
                };
                match sts {
                    s if s == vpl::MFX_WRN_DEVICE_BUSY => {
                        self.force_kf = true;
                        bail!("QSV EncodeFrameAsync stayed DEVICE_BUSY past the drain budget");
                    }
                    // With GopRefDist=1 the encoder owes one AU per submission; MORE_DATA
                    // (frame cached, no sync point) would break the FIFO pairing — surface it
                    // loudly rather than desync.
                    vpl::MFX_ERR_MORE_DATA => {
                        self.force_kf = true;
                        bail!("QSV EncodeFrameAsync returned MORE_DATA with GopRefDist=1");
                    }
                    s if s < vpl::MFX_ERR_NONE => {
                        self.force_kf = true;
                        bail!("QSV EncodeFrameAsync failed: {} ({s})", sts_name(s));
                    }
                    _ => {}
                }
                if syncp.is_null() {
                    self.force_kf = true;
                    bail!("QSV EncodeFrameAsync returned no sync point");
                }
                inner.pending.push_back(Pending {
                    syncp,
                    bs,
                    pts_ns: captured.pts_ns,
                    forced,
                    recovery_anchor,
                    _ctrl: ctrl,
                });
                inner.frames_submitted += 1;
                Ok(syncp)
            })();
            // The runtime holds its own reference for the encode in flight; ours drops now.
            let _ = release(surf);
            submit_result?;
        }
        Ok(())
    }

    /// Pin this submission's frame number to the wire frame index (see the trait doc): the LTR
    /// slots and `mfxFrameData::FrameOrder` then live in the wire domain, so
    /// `invalidate_ref_frames`'s pre-loss check stays correct across every rebuild.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        // Stored; baked into the Init ext chain (mastering/CLL at every IDR). A post-Init
        // change re-inits on the next submit — see `submit`.
        self.hdr_meta = meta;
    }

    /// LTR-RFI recovery — the `mfxExtRefListCtrl` port of the AMF slot policy: answer a loss of
    /// client frames `[first, last]` by force-referencing the newest LTR marked *before* the
    /// loss. `false` = no usable pre-loss LTR (or the driver declined the buffer at bring-up) —
    /// the caller falls back to `request_keyframe`.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        // The taint-sweep + anchor-pick POLICY lives in `rfi::plan_slot_recovery` (one decision
        // shared with AMF and Vulkan Video). This backend's mechanism: distrust is a SEPARATE
        // `ltr_tainted` flag, never a cleared mirror slot — `ltr_slots` mirrors the HARDWARE DPB,
        // and nulling an entry issues no VPL call, so the frame stays marked long-term in the
        // encoder. Clearing it made the rejection list below (which iterates the mirror and only
        // names `Some` slots) silently SKIP the one entry the sweep exists to distrust, so the
        // recovery frame could still predict from it. With two slots the "exactly one swept" case
        // is the modal one, and it was the broken one. The `!ltr_tainted` view filter below is
        // what persists that distrust across loss events (this call's taints are excluded from
        // this call's anchor by the policy itself).
        let view: Vec<(usize, i64)> = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter(|&(slot, _)| !self.ltr_tainted[slot])
            .filter_map(|(s, m)| m.map(|w| (s, w)))
            .collect();
        let plan = super::rfi::plan_slot_recovery(&view, first);
        for (slot, tainted) in self.ltr_tainted.iter_mut().enumerate() {
            if plan.tainted & (1 << slot) != 0 {
                *tainted = true;
            }
        }
        match plan.anchor {
            Some((slot, ltr_frame)) => {
                self.pending_force = Some(slot);
                tracing::info!(
                    first,
                    last,
                    slot,
                    ltr_frame,
                    "QSV LTR-RFI: forcing the next frame to re-reference a known-good LTR (no \
                     IDR)"
                );
                true
            }
            None => {
                // The sweep may have emptied the slot an earlier (un-consumed) force pointed
                // at — clear it so the next submit can't half-apply a stale recovery.
                self.pending_force = None;
                tracing::info!(
                    first,
                    last,
                    "QSV LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            // As Windows NVENC: the capturer composites; this backend never reads `frame.cursor`.
            blends_cursor: false,
            supports_rfi: self.ltr_active,
            chroma_444: false,
            intra_refresh: self.ir_active,
            // Unvalidated on-glass — the host keeps the IDR recovery path until then.
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    /// Bounded-blocking poll ([`super::amf`]'s model): give the oldest owed AU up to
    /// `min(3/4 frame interval, 12 ms)` inside `SyncOperation`, so it ships the same tick the
    /// hardware finishes. On expiry return `Ok(None)` — the session loop keeps the frame in
    /// flight and the encode-stall watchdog arbitrates a real wedge.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            if let Some(au) = inner.ready.pop_front() {
                inner.note_first_au(&au);
                Some(au)
            } else {
                let budget_ms = (750 / self.fps.max(1)).clamp(1, 12);
                match sync_one(inner, budget_ms)? {
                    Some(au) => {
                        inner.note_first_au(&au);
                        Some(au)
                    }
                    None => None,
                }
            }
        };
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// Encode-stall recovery: forfeit the in-flight frames, `Close` the encoder and re-`Init`
    /// it on the same session. A second reset without any produced AU escalates to a full
    /// session teardown (fresh loader/session on the next submit) — the same ladder as AMF's
    /// reconnect-wedge recovery.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        if self.inner.is_none() {
            return true;
        }
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "QSV stall persisted across in-place re-Init — full session teardown, reopening \
                 lazily (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        let rebuilt = {
            let inner = self.inner.as_mut().expect("checked above");
            // Best-effort settle of in-flight operations (Close aborts them anyway).
            while sync_one(inner, 5).ok().flatten().is_some() {}
            // Close BEFORE dropping `pending`. Each `Pending` owns the `Box<BsBuf>` the runtime
            // is writing into asynchronously (and a `Box<FrameCtrl>` it reads), so clearing first
            // frees that heap while the operation is still live — a use-after-free by the VPL
            // runtime. The drain above is best-effort and bails on the first `Err`, which is
            // exactly the wedged-encoder case that triggers this reset, so it cannot be relied on
            // to have retired everything. Close aborts the operations; only then is the drop safe.
            // SAFETY: the session is live on this thread; Close on a wedged encoder is legal
            // (result deliberately ignored) and re-Init happens through `init_encode`.
            unsafe {
                let _ = vpl::MFXVideoENCODE_Close(inner.session.0);
            }
            inner.pending.clear();
            inner.ready.clear();
            inner.frames_submitted = 0;
            inner.first_au_logged = false;
            inner.session.0
        };
        match self.init_encode(rebuilt) {
            Ok((ltr, ir, bs_bytes)) => {
                self.ltr_active = ltr;
                self.ir_active = ir;
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.ltr_tainted = [false; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
                if let Some(inner) = self.inner.as_mut() {
                    inner.bs_bytes = bs_bytes;
                    inner.bs_pool.clear(); // sizes may have changed
                }
                tracing::info!("QSV encoder rebuilt in place (Close + re-Init on the session)");
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "QSV in-place re-Init failed — full session teardown, reopening lazily"
                );
                self.inner = None;
                self.bound_device = 0;
                self.ir_active = false;
                self.ltr_active = false;
            }
        }
        true
    }

    /// In-place ABR retarget: `MFXVideoENCODE_Reset` with the new rate and
    /// `mfxExtEncoderResetOption{StartNewSequence=OFF}` — no IDR, no in-flight forfeit, legal
    /// in CBR because HRD conformance is off (Appendix C). The one wrinkle vs AMF: Reset
    /// requires completed sync operations, so the in-flight window is drained (into `ready`)
    /// first; a drain that can't finish inside the budget falls back to the caller's rebuild.
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Drain phase in its own scope so the `inner` borrow ends before the param rebuild
        // below reads `self` again (the raw session pointer stays valid — `self.inner` is not
        // touched in between).
        let session = {
            let Some(inner) = self.inner.as_mut() else {
                self.bitrate_bps = bps;
                return true;
            };
            let deadline = std::time::Instant::now() + BUSY_BUDGET;
            while !inner.pending.is_empty() {
                match sync_one(inner, 5) {
                    Ok(Some(au)) => inner.ready.push_back(au),
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            tracing::warn!(
                                "QSV bitrate retarget: in-flight frames didn't settle — falling \
                                 back to a rebuild"
                            );
                            return false;
                        }
                        std::thread::sleep(std::time::Duration::from_micros(250));
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "QSV retarget drain failed");
                        return false;
                    }
                }
            }
            inner.session.0
        };
        let old = self.bitrate_bps;
        self.bitrate_bps = bps;
        let cfg = self.encode_config();
        let mut set = build_params(&cfg);
        // SAFETY: all-zero valid; header stamped below; outlives the synchronous Reset call.
        let mut reset_opt: Box<vpl::mfxExtEncoderResetOption> =
            Box::new(unsafe { std::mem::zeroed() });
        reset_opt.Header.BufferId = vpl::MFX_EXTBUFF_ENCODER_RESET_OPTION as u32;
        reset_opt.Header.BufferSz = std::mem::size_of::<vpl::mfxExtEncoderResetOption>() as u32;
        reset_opt.StartNewSequence = vpl::MFX_CODINGOPTION_OFF as u16;
        set.ptrs
            .push(&mut reset_opt.Header as *mut vpl::mfxExtBuffer);
        set.par.ExtParam = set.ptrs.as_mut_ptr();
        set.par.NumExtParam = set.ptrs.len() as u16;
        // SAFETY: session live on this thread, no operation in flight (drained above); the
        // param block + ext chain outlive the synchronous call.
        let sts = unsafe { vpl::MFXVideoENCODE_Reset(session, &mut set.par) };
        if sts < vpl::MFX_ERR_NONE {
            tracing::warn!(
                mbps = bps / 1_000_000,
                status = sts_name(sts),
                "QSV declined the no-IDR bitrate retarget — falling back to a rebuild"
            );
            self.bitrate_bps = old;
            return false;
        }
        // Re-read the driver's worst-case AU size. `Reset` re-derives `BufferSizeInKB` from the new
        // rate, and a step-up can raise it well above what `init_encode` computed — leaving
        // `bs_bytes` (and every buffer already in the pool) sized for the OLD, lower bitrate. This
        // mirrors `init_encode`, which asks the same question post-Init; `take_bs` drops any pooled
        // buffer that is now too small.
        // SAFETY: `session` is live on this thread and drained above; `got` and its (empty) ext
        // chain outlive the synchronous call.
        let refreshed = unsafe {
            let mut got: vpl::mfxVideoParam = std::mem::zeroed();
            let sts = vpl::MFXVideoENCODE_GetVideoParam(session, &mut got);
            (sts >= vpl::MFX_ERR_NONE).then(|| {
                let m = &mut got.__bindgen_anon_1.mfx;
                let mult = m.BRCParamMultiplier.max(1) as usize;
                let kb = enc_of(m).BufferSizeInKB as usize;
                (kb * mult * 1000).max(256 * 1024)
            })
        };
        match (refreshed, self.inner.as_mut()) {
            (Some(bytes), Some(inner)) if bytes > inner.bs_bytes => {
                tracing::debug!(
                    old_bytes = inner.bs_bytes,
                    new_bytes = bytes,
                    mbps = bps / 1_000_000,
                    "QSV retarget raised the worst-case AU size — resizing the bitstream pool"
                );
                inner.bs_bytes = bytes;
            }
            (None, _) => tracing::warn!(
                "QSV retarget: GetVideoParam failed, keeping the previous AU buffer size"
            ),
            _ => {}
        }
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // Signal end-of-stream: null-surface EncodeFrameAsync drains the (with AsyncDepth=1,
        // empty) internal queue; owed AUs then surface through `poll`.
        // SAFETY: session live on this thread; a null surface is the documented EOS marker;
        // each drained AU gets its own pooled bitstream + sync point, queued like a submit.
        unsafe {
            loop {
                let mut bs = inner.take_bs();
                let mut syncp: vpl::mfxSyncPoint = ptr::null_mut();
                let sts = vpl::MFXVideoENCODE_EncodeFrameAsync(
                    inner.session.0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut bs.mfx,
                    &mut syncp,
                );
                if sts < vpl::MFX_ERR_NONE || syncp.is_null() {
                    break; // MFX_ERR_MORE_DATA = fully drained
                }
                inner.pending.push_back(Pending {
                    syncp,
                    bs,
                    pts_ns: 0,
                    forced: false,
                    recovery_anchor: false,
                    _ctrl: None,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Capability probes (design §4): honest per-GPU Query answers, feeding `can_encode_10bit` /
// `windows_codec_support` so negotiation matches what the session's encoder will really open.
// ---------------------------------------------------------------------------------------------

/// Can the selected Intel GPU encode `codec` at all? Session + `MFXVideoENCODE_Query` on a
/// tiny parameter block (no device handle — the runtime probes on its own device).
pub fn probe_can_encode(codec: Codec) -> bool {
    probe_query(codec, false)
}

/// Can it encode `codec` at 10-bit (HEVC Main10 / AV1 10-bit, P010 input)? H.264 is always
/// `false` (High10 is never negotiated).
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    probe_query(codec, true)
}

fn probe_query(codec: Codec, ten_bit: bool) -> bool {
    if codec == Codec::PyroWave {
        return false;
    }
    let selected = pf_gpu::resolve_render_adapter_luid().map(|l| {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&l.LowPart.to_le_bytes());
        b[4..].copy_from_slice(&l.HighPart.to_le_bytes());
        b
    });
    // Prefer the implementation on the selected adapter; on a hybrid box whose selected GPU is
    // not Intel, fall back to the first Intel implementation — the probe answers "can the
    // Intel silicon on this box do it", the session open then enforces same-adapter.
    let opened = match selected {
        Some(want) => create_session(Some(want)).or_else(|_| create_session(None)),
        None => create_session(None),
    };
    let Ok((_loader, session, _api)) = opened else {
        return false;
    };
    let cfg = EncodeConfig {
        codec,
        width: 640,
        height: 480,
        fps: 30,
        bitrate_bps: 4_000_000,
        ten_bit,
        intra_refresh: false,
        hdr_meta: None,
    };
    let mut q_in = build_params(&cfg);
    let mut q_out = build_params(&cfg);
    // SAFETY: session is live; both param blocks + ext chains outlive the synchronous call.
    let sts = unsafe { vpl::MFXVideoENCODE_Query(session.0, &mut q_in.par, &mut q_out.par) };
    sts >= vpl::MFX_ERR_NONE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dispatcher + enumeration smoke: must not crash on any box; on a box without Intel
    /// hardware it returns an empty list or errors cleanly.
    #[test]
    fn intel_enumeration_smoke() {
        match intel_loader() {
            Ok((_l, impls)) => {
                tracing::debug!(count = impls.len(), "Intel VPL implementations");
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "no Intel VPL loader (expected on non-Intel boxes)");
            }
        }
    }

    /// Probe smoke: answers must be stable booleans (no panic) whether or not Intel hardware
    /// is present. On the CI box (no Intel GPU) everything is `false`.
    #[test]
    fn probe_smoke() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let can = probe_can_encode(codec);
            let can10 = probe_can_encode_10bit(codec);
            tracing::debug!(?codec, can, can10, "QSV probe");
            if can10 {
                assert!(can, "10-bit implies base codec support");
            }
        }
    }

    fn init_tracing() {
        // Mirror the NVENC tests: visible encoder logs under --nocapture (the LTR accept/
        // reject and array-texture warnings are the on-glass diagnostics).
        let _ = tracing_subscriber::fmt()
            .with_env_filter("pf_encode=debug")
            .with_test_writer()
            .try_init();
    }

    /// Per-AU facts the live matrix asserts on.
    struct AuMeta {
        keyframe: bool,
        recovery_anchor: bool,
        annexb_start: bool,
        len: usize,
    }

    fn test_hdr_meta() -> punktfunk_core::quic::HdrMeta {
        punktfunk_core::quic::HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], // G, B, R
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000, // 1000 nits in 0.0001 cd/m²
            min_display_mastering_luminance: 500,        // 0.05 nits
            max_cll: 1000,
            max_fall: 400,
        }
    }

    /// Drive a live encode on the box's Intel implementation. `None` = no usable hardware for
    /// this (codec, bit depth) — the caller's test skips cleanly. `on_frame` runs before each
    /// submit (the seam for RFI/retarget mid-stream actions).
    fn drive_live(
        codec: Codec,
        ten_bit: bool,
        frames: u32,
        mut on_frame: impl FnMut(&mut QsvEncoder, u32),
    ) -> Option<Vec<AuMeta>> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_SDK_VERSION, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

        init_tracing();
        let Ok((_l, impls)) = intel_loader() else {
            eprintln!("skipping: no VPL loader");
            return None;
        };
        let Some(imp) = impls.iter().find(|i| i.luid_valid) else {
            eprintln!("skipping: no Intel VPL implementation on this box");
            return None;
        };
        if !probe_can_encode(codec) {
            eprintln!("skipping: this GPU declines {codec:?} encode");
            return None;
        }
        if ten_bit && !probe_can_encode_10bit(codec) {
            eprintln!("skipping: this GPU declines 10-bit {codec:?}");
            return None;
        }
        // A device on the Intel adapter the implementation reported.
        // SAFETY: self-contained harness owning every COM handle it creates; `EnumAdapterByLuid`
        // gets the LUID the runtime itself reported; `D3D11CreateDevice` fills `device` only
        // on success; the NV12/P010 texture is created and used on that one device/thread.
        let (device, tex) = unsafe {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: u32::from_le_bytes(imp.luid[..4].try_into().unwrap()),
                HighPart: i32::from_le_bytes(imp.luid[4..].try_into().unwrap()),
            };
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("dxgi factory");
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).expect("intel adapter");
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device on intel adapter");
            let device: ID3D11Device = device.expect("device");
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 640,
                Height: 480,
                MipLevels: 1,
                ArraySize: 1,
                Format: if ten_bit {
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
            let mut t: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .expect("input texture");
            (device.clone(), t.expect("texture"))
        };
        let format = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let mut enc = QsvEncoder::open(
            codec,
            format,
            640,
            480,
            30,
            2_000_000,
            if ten_bit { 10 } else { 8 },
            ChromaFormat::Yuv420,
        )
        .expect("open");
        if ten_bit {
            enc.set_hdr_meta(Some(test_hdr_meta()));
        }
        let mut aus = Vec::new();
        let mut push = |au: EncodedFrame| {
            aus.push(AuMeta {
                keyframe: au.keyframe,
                recovery_anchor: au.recovery_anchor,
                annexb_start: au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
                len: au.data.len(),
            });
        };
        for i in 0..frames {
            on_frame(&mut enc, i);
            let frame = CapturedFrame {
                width: 640,
                height: 480,
                pts_ns: i as u64 * 33_333_333,
                format,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit");
            if let Some(au) = enc.poll().expect("poll") {
                push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            push(au);
        }
        Some(aus)
    }

    fn assert_stream_shape(aus: &[AuMeta], frames: u32, annexb: bool) {
        assert!(
            aus.len() >= frames as usize - 5,
            "expected ~{frames} AUs, got {}",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be a keyframe");
        assert!(aus[0].len > 0);
        if annexb {
            assert!(aus[0].annexb_start, "first AU is not Annex-B");
        }
    }

    /// H.264 8-bit — the Phase-0 spike (design §6).
    #[test]
    fn qsv_encode_live_smoke() {
        let Some(aus) = drive_live(Codec::H264, false, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    /// HEVC 8-bit.
    #[test]
    fn qsv_live_hevc() {
        let Some(aus) = drive_live(Codec::H265, false, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    /// HEVC Main10 (P010) with the HDR mastering/CLL grade attached — Phase 2 on-glass.
    #[test]
    fn qsv_live_hevc10_hdr() {
        let Some(aus) = drive_live(Codec::H265, true, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    /// AV1 10-bit (DG2/Arc + MTL+ only; skips elsewhere) — Phase 2 on-glass. AV1 output is an
    /// OBU stream, not Annex-B.
    #[test]
    fn qsv_live_av1_10bit() {
        let Some(aus) = drive_live(Codec::Av1, true, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, false);
    }

    /// LTR-RFI end-to-end — Phase 3 on-glass: invalidate mid-stream and expect the clean
    /// re-anchor P-frame (recovery_anchor, NOT a keyframe) instead of an IDR.
    #[test]
    fn qsv_live_ltr_rfi() {
        let mut rfi_answered = false;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 && enc.caps().supports_rfi {
                rfi_answered = enc.invalidate_ref_frames(28, 29);
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        if !rfi_answered {
            eprintln!("note: driver declined LTR (supports_rfi=false or no usable slot) — IDR fallback path");
            return;
        }
        let anchor = aus.iter().position(|a| a.recovery_anchor);
        assert!(
            anchor.is_some(),
            "RFI was answered but no recovery_anchor AU was emitted"
        );
        let a = &aus[anchor.unwrap()];
        assert!(
            !a.keyframe,
            "the recovery anchor must be a clean P-frame, not an IDR"
        );
        assert!(
            !aus[1..].iter().any(|x| x.keyframe),
            "an IDR appeared despite successful LTR-RFI recovery"
        );
    }

    /// Taint sweep: a loss that predates every live LTR mark leaves NO clean anchor — every
    /// slot was marked inside the client's corrupt window. The invalidate must decline (the
    /// caller then serves the IDR) and no recovery_anchor AU may ship; before the sweep this
    /// force-referenced a tainted mark and shipped corruption tagged as a clean recovery.
    #[test]
    fn qsv_live_ltr_rfi_taint_sweep_declines() {
        let mut rfi_answered = None;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 && enc.caps().supports_rfi {
                // Frame 0 lost: the IDR itself — every mark (0, 15, ...) is at-or-after it.
                rfi_answered = Some(enc.invalidate_ref_frames(0, 2));
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        let Some(answered) = rfi_answered else {
            eprintln!("note: driver declined LTR (supports_rfi=false) — sweep not exercised");
            return;
        };
        assert!(
            !answered,
            "a loss covering every live LTR mark must fall back to IDR recovery"
        );
        assert!(
            !aus.iter().any(|a| a.recovery_anchor),
            "no recovery_anchor AU may ship when the sweep left no clean LTR"
        );
    }

    /// No-IDR bitrate retarget — Phase 3 on-glass: `reconfigure_bitrate` mid-stream must be
    /// accepted (HRD off + StartNewSequence=OFF) and must not emit a keyframe.
    #[test]
    fn qsv_live_bitrate_retarget() {
        let mut accepted = false;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 {
                accepted = enc.reconfigure_bitrate(6_000_000);
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        assert!(accepted, "the no-IDR bitrate retarget was declined");
        assert!(
            !aus[1..].iter().any(|x| x.keyframe),
            "the bitrate retarget emitted a keyframe (StartNewSequence leak)"
        );
    }

    /// FULL-CHAIN colour check at the field capture size: a known P010 colour-bar source at
    /// 1920x1080 — whose height is NOT 16-aligned, so the ingest `CopySubresourceRegion` copies
    /// into a 1920x1088 runtime pool surface whose chroma plane sits at a DIFFERENT row offset
    /// than the source's (the seam no 640x480 test exercises) — encoded to Main10 HEVC and
    /// dumped to `%TEMP%\pf_qsv_1080_bars.h265` for off-box decode verification against the
    /// same codes. On-box this asserts stream shape; the pixel verdict needs a decoder.
    #[test]
    fn qsv_live_p010_1080_colorbars_dump() {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

        // (Y, Cb, Cr) 10-bit limited codes for the 8 sRGB bars white/yellow/cyan/green/magenta/
        // red/blue/black at 80-nit SDR white under PQ/BT.2020 — the same math as pf-capture's
        // `p010_reference` (and the bars_pq2020 client fixture). Stored MSB-aligned (`<<6`).
        const BARS: [(u16, u16, u16); 8] = [
            (490, 512, 512),
            (478, 423, 518),
            (464, 525, 473),
            (450, 432, 476),
            (350, 584, 585),
            (325, 448, 598),
            (226, 650, 535),
            (64, 512, 512),
        ];
        const W: u32 = 1920;
        const H: u32 = 1080;

        init_tracing();
        let Ok((_l, impls)) = intel_loader() else {
            eprintln!("skipping: no VPL loader");
            return;
        };
        let Some(imp) = impls.iter().find(|i| i.luid_valid) else {
            eprintln!("skipping: no Intel VPL implementation on this box");
            return;
        };
        if !probe_can_encode_10bit(Codec::H265) {
            eprintln!("skipping: this GPU declines 10-bit HEVC");
            return;
        }

        // P010 initial data: plane 0 = H rows of W u16 luma; plane 1 = H/2 rows of W u16
        // (interleaved Cb,Cr pairs), same pitch. Bars are vertical: bar index = x / (W/8).
        let bar_w = (W / 8) as usize;
        let mut init = vec![0u16; (W as usize) * (H as usize + H as usize / 2)];
        for y in 0..H as usize {
            for x in 0..W as usize {
                init[y * W as usize + x] = BARS[(x / bar_w).min(7)].0 << 6;
            }
        }
        let chroma_base = (W as usize) * (H as usize);
        for cy in 0..(H as usize / 2) {
            for cx in 0..(W as usize / 2) {
                let (_, cb, cr) = BARS[((cx * 2) / bar_w).min(7)];
                init[chroma_base + cy * W as usize + cx * 2] = cb << 6;
                init[chroma_base + cy * W as usize + cx * 2 + 1] = cr << 6;
            }
        }

        // SAFETY: self-contained harness on one thread/device (same contract as `drive_live`);
        // the initial-data pointer outlives the synchronous CreateTexture2D that reads it.
        let (device, tex) = unsafe {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: u32::from_le_bytes(imp.luid[..4].try_into().unwrap()),
                HighPart: i32::from_le_bytes(imp.luid[4..].try_into().unwrap()),
            };
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("dxgi factory");
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).expect("intel adapter");
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device on intel adapter");
            let device: ID3D11Device = device.expect("device");
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_P010,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: init.as_ptr() as *const std::ffi::c_void,
                SysMemPitch: W * 2,
                SysMemSlicePitch: 0,
            };
            let mut t: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut t))
                .expect("bar texture");
            (device.clone(), t.expect("texture"))
        };

        let mut enc = QsvEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            W,
            H,
            30,
            10_000_000,
            10,
            ChromaFormat::Yuv420,
        )
        .expect("open");
        enc.set_hdr_meta(Some(test_hdr_meta()));
        let mut stream = Vec::new();
        let mut aus = 0usize;
        let mut keyframes = 0usize;
        for i in 0..12u32 {
            let frame = CapturedFrame {
                width: W,
                height: H,
                pts_ns: i as u64 * 33_333_333,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit");
            if let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus += 1;
            keyframes += au.keyframe as usize;
            stream.extend_from_slice(&au.data);
        }
        assert!(aus >= 10, "expected ≥10 AUs, got {aus}");
        assert!(keyframes >= 1, "expected an IDR in the dump");
        let path = std::env::temp_dir().join("pf_qsv_1080_bars.h265");
        std::fs::write(&path, &stream).expect("write dump");
        println!(
            "wrote {} AUs ({} bytes, {keyframes} keyframes) to {}",
            aus,
            stream.len(),
            path.display()
        );
    }

    /// The PRODUCTION host chain minus the IDD ring: the REAL `HdrP010Converter` renders the 8
    /// sRGB bars into a ring-profile P010 texture (`BIND_RENDER_TARGET` only — RTV-written, not
    /// CPU-uploaded) on the VPL implementation's own adapter, and THAT texture goes through the
    /// unaligned-height ingest copy into a Main10 encode. Dumped to
    /// `%TEMP%\pf_qsv_conv_1080_bars.h265`; expected decode codes = the bars_pq2020 fixture set
    /// (see `hdr_p010_convert_bars_on_luid`).
    #[test]
    fn qsv_live_hdr_converter_e2e_1080_dump() {
        const W: u32 = 1920;
        const H: u32 = 1080;

        init_tracing();
        let Ok((_l, impls)) = intel_loader() else {
            eprintln!("skipping: no VPL loader");
            return;
        };
        let Some(imp) = impls.iter().find(|i| i.luid_valid) else {
            eprintln!("skipping: no Intel VPL implementation on this box");
            return;
        };
        if !probe_can_encode_10bit(Codec::H265) {
            eprintln!("skipping: this GPU declines 10-bit HEVC");
            return;
        }
        let (device, tex) = pf_capture::dxgi::hdr_p010_convert_bars_on_luid(imp.luid, W, H)
            .expect("converter bars");

        let mut enc = QsvEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            W,
            H,
            30,
            10_000_000,
            10,
            ChromaFormat::Yuv420,
        )
        .expect("open");
        enc.set_hdr_meta(Some(test_hdr_meta()));
        let mut stream = Vec::new();
        let mut aus = 0usize;
        for i in 0..12u32 {
            let frame = CapturedFrame {
                width: W,
                height: H,
                pts_ns: i as u64 * 33_333_333,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit");
            if let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus += 1;
            stream.extend_from_slice(&au.data);
        }
        assert!(aus >= 10, "expected ≥10 AUs, got {aus}");
        let path = std::env::temp_dir().join("pf_qsv_conv_1080_bars.h265");
        std::fs::write(&path, &stream).expect("write dump");
        println!(
            "wrote {aus} AUs ({} bytes) to {}",
            stream.len(),
            path.display()
        );
    }
}
