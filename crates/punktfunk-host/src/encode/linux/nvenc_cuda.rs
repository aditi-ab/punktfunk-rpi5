//! Direct-SDK NVENC encoder (Linux, CUDA input) — the raw `nvEncodeAPI` port that gives the Linux
//! NVIDIA host **real reference-frame invalidation + the recovery-anchor tag + a `reset()` stall
//! lever + HDR/Main10 plumbing** that libavcodec `hevc_nvenc` (`super::NvencEncoder`) structurally
//! cannot express (no avcodec option maps to `nvEncInvalidateRefFrames`). Design:
//! `design/linux-direct-nvenc.md`; the recovery semantics it delivers: `encoder-recovery-hardening.md`.
//!
//! This is the CUDA sibling of `encode/windows/nvenc.rs`. It drives the same runtime-loaded entry
//! table (`nvidia_video_codec_sdk::sys::nvEncodeAPI` `sys` types) but:
//!   * loads `libnvidia-encode.so.1` via `dlopen` (the Linux analogue of the Windows System32 DLL
//!     load, and of this crate's `zerocopy::cuda` libcuda loader) — never a link-time import, so
//!     one binary still starts on AMD/Intel Linux boxes (no NVIDIA driver) and falls through to
//!     VAAPI/software;
//!   * opens the encode session on `NV_ENC_DEVICE_TYPE_CUDA` bound to the **shared process-wide
//!     `CUcontext`** (`zerocopy::cuda::context()`) the capture/import path already uses;
//!   * feeds NVENC from an **encoder-owned ring of registered CUDA input surfaces**
//!     ([`zerocopy::cuda::InputSurface`]): each captured `FramePayload::Cuda` `DeviceBuffer` is
//!     device→device copied into the current ring slot (via the existing `copy_*_to_device`
//!     helpers) before `encode_picture`. This mirrors the libav path's recycled-hwframe-pool copy
//!     (NVENC rejects a null-`buf[0]` frame and its CUDADEVICEPTR registration cache is bounded +
//!     pointer-keyed, so registering a fresh pool pointer each frame would thrash it) — so it is
//!     zero regression versus today; true zero-copy input registration is a follow-up.
//!
//! **Sync-only.** NVENC async mode (`enableEncodeAsync` + Win32 completion events) is Windows-only,
//! so the whole two-thread async-retrieve subsystem of the Windows backend is absent here: `poll`
//! does the blocking `lock_bitstream`, exactly like the libav path.
//!
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise); compiles GPU-less and
//! starts driver-less (the `.so` resolves at runtime — on an AMD/Intel box [`try_api`] fails cleanly
//! and the VAAPI/software backends carry the session).

// Every `unsafe` block / impl in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::capture::{CapturedFrame, FramePayload};
use crate::zerocopy::cuda::{self, InputSurface};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

// ---------------------------------------------------------------------------------------------
// Runtime-loaded NVENC entry table (Linux). Same shape as the Windows backend's `EncodeApi`, minus
// the async-event entry points (Windows-only). Resolved once from `libnvidia-encode.so.1` — the two
// real exports (`NvEncodeAPIGetMaxSupportedVersion`, `NvEncodeAPICreateInstance`) by name, the rest
// through `NvEncodeAPICreateInstance`. NEVER a link-time import: the shipped binary compiles the
// `nvenc` feature in unconditionally and a load-time `.so` dependency would refuse to start the
// process on every AMD/Intel-only Linux box (the Linux analogue of the Windows nvEncodeAPI64.dll
// problem, and of this crate's dlopen'd libcuda).
// ---------------------------------------------------------------------------------------------

/// The `NV_ENCODE_API_FUNCTION_LIST` entries this encoder uses. Field names mirror the sdk crate's
/// list; the crate's safe `ENCODE_API` must NOT be referenced (its statically-declared externs put
/// a load-time `.so` import on the all-vendor binary — the exact thing the runtime load avoids).
struct EncodeApi {
    open_encode_session_ex: unsafe extern "C" fn(
        *mut nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
        *mut *mut c_void,
    ) -> nv::NVENCSTATUS,
    initialize_encoder:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_INITIALIZE_PARAMS) -> nv::NVENCSTATUS,
    reconfigure_encoder:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_RECONFIGURE_PARAMS) -> nv::NVENCSTATUS,
    destroy_encoder: unsafe extern "C" fn(*mut c_void) -> nv::NVENCSTATUS,
    get_encode_caps: unsafe extern "C" fn(
        *mut c_void,
        nv::GUID,
        *mut nv::NV_ENC_CAPS_PARAM,
        *mut core::ffi::c_int,
    ) -> nv::NVENCSTATUS,
    get_encode_preset_config_ex: unsafe extern "C" fn(
        *mut c_void,
        nv::GUID,
        nv::GUID,
        nv::NV_ENC_TUNING_INFO,
        *mut nv::NV_ENC_PRESET_CONFIG,
    ) -> nv::NVENCSTATUS,
    create_bitstream_buffer: unsafe extern "C" fn(
        *mut c_void,
        *mut nv::NV_ENC_CREATE_BITSTREAM_BUFFER,
    ) -> nv::NVENCSTATUS,
    destroy_bitstream_buffer:
        unsafe extern "C" fn(*mut c_void, nv::NV_ENC_OUTPUT_PTR) -> nv::NVENCSTATUS,
    lock_bitstream:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_LOCK_BITSTREAM) -> nv::NVENCSTATUS,
    unlock_bitstream: unsafe extern "C" fn(*mut c_void, nv::NV_ENC_OUTPUT_PTR) -> nv::NVENCSTATUS,
    register_resource:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_REGISTER_RESOURCE) -> nv::NVENCSTATUS,
    unregister_resource:
        unsafe extern "C" fn(*mut c_void, nv::NV_ENC_REGISTERED_PTR) -> nv::NVENCSTATUS,
    map_input_resource:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_MAP_INPUT_RESOURCE) -> nv::NVENCSTATUS,
    unmap_input_resource:
        unsafe extern "C" fn(*mut c_void, nv::NV_ENC_INPUT_PTR) -> nv::NVENCSTATUS,
    encode_picture:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_PIC_PARAMS) -> nv::NVENCSTATUS,
    invalidate_ref_frames: unsafe extern "C" fn(*mut c_void, u64) -> nv::NVENCSTATUS,
}

/// Local `NVENCSTATUS` → `Result` (the sdk's own helper lives in the `safe` module this file must
/// not pull in). The raw status's Debug repr is the error payload.
trait NvStatusExt {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS>;
}
impl NvStatusExt for nv::NVENCSTATUS {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS> {
        match self {
            nv::NVENCSTATUS::NV_ENC_SUCCESS => Ok(()),
            err => Err(err),
        }
    }
}

/// Resolve the table once per process. `Err` = NVENC genuinely unavailable (no NVIDIA driver/.so,
/// or a driver older than our headers) — [`NvencCudaEncoder::open`] gates on it and the VAAPI/
/// software backends carry on.
fn try_api() -> std::result::Result<&'static EncodeApi, &'static str> {
    static TABLE: std::sync::OnceLock<std::result::Result<EncodeApi, String>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let table = load_api();
            if let Err(e) = &table {
                tracing::warn!("NVENC (Linux direct) API unavailable: {e}");
            }
            table
        })
        .as_ref()
        .map_err(|e| e.as_str())
}

/// The loaded table, for call sites past a [`try_api`] gate — a live session implies the load
/// succeeded, and the table lives for the process lifetime.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

fn load_api() -> std::result::Result<EncodeApi, String> {
    // SAFETY: `Library::new` runs `libnvidia-encode.so.1`'s initializers — the trusted NVIDIA driver
    // library, so loading has no unexpected effects; `map_err` handles its absence (AMD/Intel/no
    // driver). Each `lib.get::<T>(name)` asserts the symbol's ABI equals `T`, the documented
    // `nvEncodeAPI.h` prototype. `NvEncodeAPIGetMaxSupportedVersion` writes one u32 through a live
    // pointer; `NvEncodeAPICreateInstance` fills `list` (a `#[repr(C)]` function list with `version`
    // set) during the call only. Each extracted fn pointer is deref-copied out of its borrowing
    // `Symbol` before `forget(lib)` leaks the mapping, so every address stays valid for the process
    // lifetime. Runs once under the `OnceLock` init — no aliasing.
    unsafe {
        let lib = libloading::Library::new("libnvidia-encode.so.1")
            .or_else(|_| libloading::Library::new("libnvidia-encode.so"))
            .map_err(|e| format!("libnvidia-encode.so.1 not loadable (no NVIDIA driver?): {e}"))?;
        let get_version: libloading::Symbol<unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS> =
            lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0")
                .map_err(|e| {
                    format!("libnvidia-encode exports no NvEncodeAPIGetMaxSupportedVersion: {e}")
                })?;
        let create_instance: libloading::Symbol<
            unsafe extern "C" fn(*mut nv::NV_ENCODE_API_FUNCTION_LIST) -> nv::NVENCSTATUS,
        > = lib
            .get(b"NvEncodeAPICreateInstance\0")
            .map_err(|e| format!("libnvidia-encode exports no NvEncodeAPICreateInstance: {e}"))?;
        let get_version = *get_version;
        let create_instance = *create_instance;

        let mut version = 0u32;
        get_version(&mut version)
            .nv_ok()
            .map_err(|e| format!("NvEncodeAPIGetMaxSupportedVersion: {e:?}"))?;
        // The sdk's version assert, minus the panic: an older driver is a clean Err.
        let (major, minor) = (version >> 4, version & 0xf);
        if (major, minor) < (nv::NVENCAPI_MAJOR_VERSION, nv::NVENCAPI_MINOR_VERSION) {
            return Err(format!(
                "driver NVENC API {major}.{minor} is older than the host's headers {}.{} — \
                 update the NVIDIA driver",
                nv::NVENCAPI_MAJOR_VERSION,
                nv::NVENCAPI_MINOR_VERSION
            ));
        }

        let mut list = nv::NV_ENCODE_API_FUNCTION_LIST {
            version: nv::NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        create_instance(&mut list)
            .nv_ok()
            .map_err(|e| format!("NvEncodeAPICreateInstance: {e:?}"))?;
        const MISSING: &str = "NvEncodeAPICreateInstance left an entry point unfilled";
        let api = EncodeApi {
            open_encode_session_ex: list.nvEncOpenEncodeSessionEx.ok_or(MISSING)?,
            initialize_encoder: list.nvEncInitializeEncoder.ok_or(MISSING)?,
            reconfigure_encoder: list.nvEncReconfigureEncoder.ok_or(MISSING)?,
            destroy_encoder: list.nvEncDestroyEncoder.ok_or(MISSING)?,
            get_encode_caps: list.nvEncGetEncodeCaps.ok_or(MISSING)?,
            get_encode_preset_config_ex: list.nvEncGetEncodePresetConfigEx.ok_or(MISSING)?,
            create_bitstream_buffer: list.nvEncCreateBitstreamBuffer.ok_or(MISSING)?,
            destroy_bitstream_buffer: list.nvEncDestroyBitstreamBuffer.ok_or(MISSING)?,
            lock_bitstream: list.nvEncLockBitstream.ok_or(MISSING)?,
            unlock_bitstream: list.nvEncUnlockBitstream.ok_or(MISSING)?,
            register_resource: list.nvEncRegisterResource.ok_or(MISSING)?,
            unregister_resource: list.nvEncUnregisterResource.ok_or(MISSING)?,
            map_input_resource: list.nvEncMapInputResource.ok_or(MISSING)?,
            unmap_input_resource: list.nvEncUnmapInputResource.ok_or(MISSING)?,
            encode_picture: list.nvEncEncodePicture.ok_or(MISSING)?,
            invalidate_ref_frames: list.nvEncInvalidateRefFrames.ok_or(MISSING)?,
        };
        std::mem::forget(lib); // keep the .so mapped for the fn pointers' lifetime (process)
        Ok(api)
    }
}

/// Output bitstream buffers = max in-flight encodes; equals the input-surface ring depth. The host
/// loop deep-pipelines (submits several frames before locking the oldest) so this must be ≥ the
/// helper's `PUNKTFUNK_ENCODE_DEPTH` (default 4, clamped ≤ 6).
const POOL: usize = 8;

/// Reference-frame DPB depth when RFI is supported (Apollo uses 5). A deeper DPB lets an invalidated
/// reference fall back to an older still-valid frame instead of a full IDR; `numRefL0 = 1` keeps each
/// P-frame single-reference for low latency.
const RFI_DPB: u32 = 5;

fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
        // Guarded by the open_video dispatch: a PyroWave session never reaches NVENC.
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }
}

/// The NVENC input buffer format for a captured `DeviceBuffer`'s layout. NV12/YUV444 are the zero-
/// copy worker's convert outputs; packed RGB (`ABGR`) is the fallback where NVENC does the internal
/// CSC. 10-bit is never produced on Linux today (Phase 5.1), so everything is 8-bit.
fn buffer_format(buf: &cuda::DeviceBuffer) -> nv::NV_ENC_BUFFER_FORMAT {
    if buf.yuv444 {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
    } else if buf.is_nv12() {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
    } else {
        // Packed 4-byte BGRA-order (the `copy_device_to_device` fallback path); NVENC's `ARGB`
        // ingests this layout + does the internal CSC, matching the proven Windows RGB-input path.
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
    }
}

/// One encoder-owned CUDA input surface + its NVENC registration. The surface is copied into each
/// use (device→device) and the registration is created once at session init, unregistered at teardown.
struct RingSlot {
    surface: InputSurface,
    reg: nv::NV_ENC_REGISTERED_PTR,
}

pub struct NvencCudaEncoder {
    encoder: *mut c_void,
    /// The shared process-wide `CUcontext` the session is bound to (from `zerocopy::cuda::context`).
    cu_ctx: *mut c_void,
    codec: Codec,
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    /// Encoded bit depth (8 on Linux until Phase 5.1 lands a P010 capture path). Kept for parity with
    /// the Windows Main10 config, which is ported but inert until a 10-bit input exists.
    bit_depth: u8,
    /// Full-chroma 4:4:4 (HEVC Range Extensions) — set when the capturer delivers a planar-YUV444
    /// `DeviceBuffer` on an HEVC session and the GPU supports YUV444 encode.
    chroma_444: bool,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` — whether this GPU can 4:4:4 encode at all.
    yuv444_supported: bool,
    /// HDR (BT.2020 PQ 10-bit). Always `false` on Linux today (no 10-bit input); the VUI/SEI/Main10
    /// plumbing is ported for Phase 5.1 readiness.
    hdr: bool,
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// The encoder-owned input-surface ring (allocated + registered at session init, round-robin per
    /// submit). Empty until the session is initialized.
    ring: Vec<RingSlot>,
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// (bitstream, mapped input resource to unmap after retrieval, pts_ns, recovery-anchor) per
    /// in-flight encode. The fourth field tags the first frame encoded after a successful
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) — the clean re-anchor P-frame the
    /// client lifts its post-loss freeze on.
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64, bool)>,
    /// The frame number of the NEXT submission (also its `inputTimeStamp`). Pinned per frame by
    /// [`Encoder::submit_indexed`] to the WIRE frame index the AU will carry, so the DPB timestamps
    /// `invalidate_ref_frames` compares client frame numbers against stay 1:1 with the wire across
    /// encoder rebuilds/resets. Self-increments as a fallback for un-indexed callers (tests).
    frame_idx: i64,
    force_kf: bool,
    /// A successful [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) arms this; the next
    /// `submit` consumes it into `pending` so that AU ships as the recovery anchor (NVENC applies the
    /// invalidation at the next `encode_picture`, so that frame is by construction the first coded
    /// against only-valid references — the F2 fix, identical to the Windows backend).
    pending_anchor: bool,
    inited: bool,
    /// GPU capabilities probed once via `nvEncGetEncodeCaps` before configuring.
    rfi_supported: bool,
    custom_vbv: bool,
    /// The split-encode mode the live session was initialized with — `reconfigure_bitrate` must
    /// present the SAME init params as the open (only the config's rate fields may move).
    /// Meaningless while `inited` is false.
    split_mode: u32,
    /// The last reference-frame range we invalidated — dedupes repeated RFI requests for one loss.
    last_rfi_range: Option<(i64, i64)>,
}

// SAFETY: the `!Send` fields are the raw NVENC session handle (`encoder`), the shared `CUcontext`
// (`cu_ctx`, a process-global handle valid from any thread once `cuCtxSetCurrent` is issued), and the
// raw NVENC bitstream/registered/mapped pointers in `bitstreams`/`ring`/`pending`. The encoder is
// owned by exactly one thread: it is moved onto the host encode thread once at construction, and every
// method (`submit`/`poll`/`invalidate_ref_frames`/`Drop`) runs there. There is no secondary thread
// (unlike the Windows async retrieve) — this backend is sync-only. Moving the encoder across its one
// ownership-transfer boundary is sound because no NVENC/CUDA call is in flight during the move, so
// `Send` introduces no data race on the non-`Send` fields.
unsafe impl Send for NvencCudaEncoder {}

impl NvencCudaEncoder {
    /// Signature mirrors `super::NvencEncoder::open` so the Linux dispatcher fork is a one-line swap.
    /// `format`/`cuda` are advisory: the session's real input format is derived from the first
    /// captured `DeviceBuffer`'s layout (lazy init in `submit`), and this backend only accepts CUDA
    /// frames (a CPU/dmabuf payload `bail`s). `bit_depth` is pinned to 8 on Linux (Phase 5.1 will
    /// lift it once P010 capture exists).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        _format: crate::capture::PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        _cuda: bool,
        bit_depth: u8,
        chroma: ChromaFormat,
    ) -> Result<Self> {
        // The runtime `.so` load is the real "is NVENC possible here" gate: fail the open with a
        // clear reason instead of an opaque session error on the first frame.
        try_api().map_err(|e| anyhow!("NVENC (Linux direct) unavailable: {e}"))?;
        if bit_depth >= 10 {
            tracing::warn!(
                "Linux direct-NVENC: 10-bit requested but no P010 capture path exists yet \
                 (Phase 5.1) — encoding 8-bit SDR"
            );
        }
        Ok(Self {
            encoder: ptr::null_mut(),
            cu_ctx: ptr::null_mut(),
            codec,
            codec_guid: codec_guid(codec),
            width,
            height,
            fps,
            bitrate_bps,
            buffer_fmt: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bit_depth: 8,
            // 4:4:4 is HEVC-only; confirmed against the frame layout + GPU support at init.
            chroma_444: chroma.is_444() && codec == Codec::H265,
            yuv444_supported: false,
            hdr: false,
            hdr_meta: None,
            ring: Vec::new(),
            next: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            pending_anchor: false,
            inited: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32,
            last_rfi_range: None,
        })
    }

    /// Tear down the encode session + pooled resources. Reused on a size change and at Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Unmap any in-flight inputs, unregister every ring surface, destroy the bitstreams.
        for (_, map, _, _) in &self.pending {
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, *map);
            }
        }
        for slot in &self.ring {
            let _ = (api().unregister_resource)(self.encoder, slot.reg);
        }
        for &bs in &self.bitstreams {
            let _ = (api().destroy_bitstream_buffer)(self.encoder, bs);
        }
        let _ = (api().destroy_encoder)(self.encoder);
        self.ring.clear(); // drops the InputSurfaces, freeing their CUDA allocations
        self.bitstreams.clear();
        self.pending.clear();
        self.encoder = ptr::null_mut();
        self.inited = false;
        self.next = 0;
        // The new session starts with an empty DPB (its first frame is an IDR), so any prior
        // invalidation range is meaningless and a pending anchor from a pre-teardown RFI is stale.
        self.last_rfi_range = None;
        self.pending_anchor = false;
    }

    /// Query one `NV_ENC_CAPS` value for this codec; 0 on any error (treat unqueryable as unsupported).
    unsafe fn get_cap(&self, enc: *mut c_void, which: nv::NV_ENC_CAPS) -> i32 {
        let mut param = nv::NV_ENC_CAPS_PARAM {
            version: nv::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: which,
            reserved: [0; 62],
        };
        let mut val: i32 = 0;
        match (api().get_encode_caps)(enc, self.codec_guid, &mut param, &mut val).nv_ok() {
            Ok(()) => val,
            Err(_) => 0,
        }
    }

    /// Probe this GPU's capabilities once (max dims / 4:4:4 / ref-pic-invalidation / custom-VBV) on a
    /// throwaway CUDA session before configuring, so the config is gated on what the card supports and
    /// an out-of-range mode fails with a clear error rather than an opaque `InvalidParam`.
    unsafe fn query_caps(&mut self) -> Result<()> {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device: self.cu_ctx,
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        (api().open_encode_session_ex)(&mut params, &mut enc)
            .nv_ok()
            .map_err(|e| {
                anyhow!("NVENC open_encode_session_ex (caps probe): {e:?} (no NVIDIA GPU?)")
            })?;
        let wmax = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX);
        let hmax = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX);
        let yuv444 = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE);
        let rfi = self.get_cap(
            enc,
            nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
        );
        let custom_vbv = self.get_cap(
            enc,
            nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE,
        );
        let _ = (api().destroy_encoder)(enc);

        if wmax > 0 && hmax > 0 && (self.width as i32 > wmax || self.height as i32 > hmax) {
            bail!(
                "this GPU's NVENC max encode size for {:?} is {wmax}x{hmax}; client requested \
                 {}x{} (lower the client resolution or use a codec/GPU that supports it)",
                self.codec,
                self.width,
                self.height
            );
        }
        self.yuv444_supported = yuv444 != 0;
        if self.chroma_444 && !self.yuv444_supported {
            tracing::warn!("NVENC (Linux): this GPU can't 4:4:4 encode — falling back to 4:2:0");
            self.chroma_444 = false;
        }
        self.rfi_supported = rfi != 0;
        self.custom_vbv = custom_vbv != 0;
        tracing::info!(
            rfi = self.rfi_supported,
            custom_vbv = self.custom_vbv,
            yuv444 = self.yuv444_supported,
            max = %format!("{wmax}x{hmax}"),
            "NVENC (Linux direct) capabilities probed"
        );
        Ok(())
    }

    /// Author the session's `NV_ENC_CONFIG` at `bitrate` (bps): the P1/ULL preset (queried on
    /// `enc`) seeded with the RC/tier/chroma/VUI/DPB shape this backend always runs. ONE builder
    /// shared by [`try_open_session`] and [`Encoder::reconfigure_bitrate`], so an in-place rate
    /// retarget re-authors the exact same config with only the bitrate + derived VBV moved.
    unsafe fn build_config(&self, enc: *mut c_void, bitrate: u64) -> Result<nv::NV_ENC_CONFIG> {
        // Seed the P1 + ultra-low-latency preset config.
        let mut preset = nv::NV_ENC_PRESET_CONFIG {
            version: nv::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        (api().get_encode_preset_config_ex)(
            enc,
            self.codec_guid,
            nv::NV_ENC_PRESET_P1_GUID,
            nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            &mut preset,
        )
        .nv_ok()
        .map_err(|e| anyhow!("get_encode_preset_config_ex: {e:?}"))?;
        let mut cfg = preset.presetCfg;

        // CBR, infinite GOP, P-only, ~1-frame VBV (mirror the Windows/Linux-libav RC config).
        cfg.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
        cfg.frameIntervalP = 1;
        cfg.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        let bps = bitrate.min(u32::MAX as u64) as u32;
        cfg.rcParams.averageBitRate = bps;
        cfg.rcParams.maxBitRate = bps;
        if self.custom_vbv {
            // ~1-frame VBV by default; PUNKTFUNK_VBV_FRAMES scales it (parity with AMF/VAAPI/QSV).
            let vbv = ((bitrate as f64 / self.fps.max(1) as f64) * crate::encode::vbv_frames_env())
                .clamp(1.0, u32::MAX as f64) as u32;
            cfg.rcParams.vbvBufferSize = vbv;
            cfg.rcParams.vbvInitialDelay = vbv;
        }

        // Tier + autoselect level, PER CODEC (HEVC HIGH tier for the higher bitrate ceiling; AV1
        // Main tier only — tier=1 fails AV1 init; H.264 has no tier). Level 0 = autoselect.
        match self.codec {
            Codec::H265 => {
                cfg.encodeCodecConfig.hevcConfig.tier = 1;
                cfg.encodeCodecConfig.hevcConfig.level = 0;
            }
            Codec::Av1 => {}
            Codec::H264 => {}
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }

        // Chroma + bit depth. 4:4:4 (HEVC Range Extensions, chromaFormatIDC=3) engages on a YUV444
        // input; 10-bit Main10 is ported but never engages on Linux yet (input is 8-bit).
        let yuv444_input = matches!(
            self.buffer_fmt,
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
        );
        if self.chroma_444 && yuv444_input {
            cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_FREXT_GUID;
            cfg.encodeCodecConfig.hevcConfig.set_chromaFormatIDC(3);
            if self.bit_depth == 10 {
                cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2);
            }
        } else if self.bit_depth == 10 {
            match self.codec {
                Codec::H265 => {
                    cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID;
                    cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2);
                }
                Codec::Av1 => {
                    cfg.encodeCodecConfig.av1Config.set_pixelBitDepthMinus8(2);
                    cfg.encodeCodecConfig
                        .av1Config
                        .set_inputPixelBitDepthMinus8(0);
                }
                Codec::H264 => {}
                Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
            }
        }

        // Colour signaling, written UNCONDITIONALLY: the CUDA input is already CSC'd to a specific
        // matrix (NV12/YUV444 8-bit BT.709 limited from the convert worker, or the packed-RGB path
        // where NVENC's internal CSC follows this VUI), so the stream must say BT.709 or a decoder
        // whose "unspecified" default is 601 will mis-render.
        {
            let (prim, trc, mat) = if self.hdr {
                (
                    nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020,
                    nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084,
                    nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL,
                )
            } else {
                (
                    nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709,
                    nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709,
                    nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709,
                )
            };
            match self.codec {
                Codec::H265 => {
                    let vui = &mut cfg.encodeCodecConfig.hevcConfig.hevcVUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = prim;
                    vui.transferCharacteristics = trc;
                    vui.colourMatrix = mat;
                }
                Codec::H264 => {
                    let vui = &mut cfg.encodeCodecConfig.h264Config.h264VUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = prim;
                    vui.transferCharacteristics = trc;
                    vui.colourMatrix = mat;
                }
                Codec::Av1 => {
                    let av1 = &mut cfg.encodeCodecConfig.av1Config;
                    av1.colorPrimaries = prim;
                    av1.transferCharacteristics = trc;
                    av1.matrixCoefficients = mat;
                    av1.colorRange = 0;
                }
                Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
            }
        }

        // Reference-frame invalidation: a deeper DPB so an invalidated reference can fall back to an
        // older still-valid frame instead of a full IDR; `numRefL0 = 1` keeps each P-frame
        // single-reference for low latency. Only when this GPU supports RFI.
        if self.rfi_supported {
            let one = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
            match self.codec {
                Codec::H264 => {
                    cfg.encodeCodecConfig.h264Config.maxNumRefFrames = RFI_DPB;
                    cfg.encodeCodecConfig.h264Config.numRefL0 = one;
                }
                Codec::H265 => {
                    cfg.encodeCodecConfig.hevcConfig.maxNumRefFramesInDPB = RFI_DPB;
                    cfg.encodeCodecConfig.hevcConfig.numRefL0 = one;
                }
                Codec::Av1 => {
                    cfg.encodeCodecConfig.av1Config.maxNumRefFramesInDPB = RFI_DPB;
                }
                Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
            }
        }
        Ok(cfg)
    }

    /// Author the `NV_ENC_INITIALIZE_PARAMS` pointing at `cfg`. Shared by [`try_open_session`]
    /// and [`Encoder::reconfigure_bitrate`] — a reconfigure must present the SAME init params as
    /// the open. The returned struct borrows `cfg` raw; the caller keeps `cfg` alive across the
    /// NVENC call it feeds this into.
    fn build_init_params(
        &self,
        cfg: &mut nv::NV_ENC_CONFIG,
        split_mode: u32,
    ) -> nv::NV_ENC_INITIALIZE_PARAMS {
        let mut init = nv::NV_ENC_INITIALIZE_PARAMS {
            version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: self.codec_guid,
            presetGUID: nv::NV_ENC_PRESET_P1_GUID,
            tuningInfo: nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            encodeWidth: self.width,
            encodeHeight: self.height,
            darWidth: self.width,
            darHeight: self.height,
            frameRateNum: self.fps,
            frameRateDen: 1,
            enablePTD: 1,
            encodeConfig: cfg,
            ..Default::default()
        };
        init.set_splitEncodeMode(split_mode);
        init
    }

    /// Open + configure + initialize ONE NVENC CUDA session at `bitrate` (bps) and `split_mode`.
    /// Returns the session handle, or destroys it and returns the error.
    unsafe fn try_open_session(&self, bitrate: u64, split_mode: u32) -> Result<*mut c_void> {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device: self.cu_ctx,
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        (api().open_encode_session_ex)(&mut params, &mut enc)
            .nv_ok()
            .map_err(|e| anyhow!("NVENC open_encode_session_ex: {e:?} (no NVIDIA GPU?)"))?;

        let mut cfg = match self.build_config(enc, bitrate) {
            Ok(cfg) => cfg,
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                return Err(e);
            }
        };
        let mut init = self.build_init_params(&mut cfg, split_mode);

        match (api().initialize_encoder)(enc, &mut init).nv_ok() {
            Ok(()) => Ok(enc),
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                Err(anyhow!("initialize_encoder: {e:?}"))
            }
        }
    }

    /// Lazily create the session + input-surface ring on the first frame's format.
    fn init_session(&mut self) -> Result<()> {
        // SAFETY: every NVENC call goes through a function pointer resolved once from the runtime
        // table (`api()`, gated in `open`). `try_open_session`/`query_caps` return either a valid
        // open NVENC session handle or `Err`; `destroy_encoder` is only called on a handle
        // `try_open_session` just returned (and `best` only when non-null). `create_bitstream_buffer`
        // and `register_resource` take `enc`, the chosen live session, and `&mut` locals whose
        // `version` is set and which outlive the synchronous call. `InputSurface::alloc_*` returns a
        // live pitched CUDA allocation on the shared context. No handle escapes the encode thread.
        unsafe {
            // Bind to the shared CUDA context; make it current on this (encode) thread for both the
            // session open and every subsequent device→device input copy.
            self.cu_ctx = cuda::context().context("shared CUDA context (Linux direct NVENC)")?;
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;

            self.query_caps()?;
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // 2-way NVENC split-frame encoding (Ada dual-NVENC) above ~1 Gpix/s; env override
            // PUNKTFUNK_SPLIT_ENCODE = 0/disable | 1/auto | 2 | 3. HEVC/AV1 only.
            let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
            let mut split_mode: u32 = match std::env::var("PUNKTFUNK_SPLIT_ENCODE").ok().as_deref()
            {
                Some("0") | Some("disable") => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32
                }
                Some("1") | Some("auto") => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
                }
                Some("3") => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                Some("2") => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
                _ if self.bit_depth >= 10 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32
                }
                _ if pixel_rate > 1_000_000_000 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
                }
                _ => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_MODE as u32,
            };
            const CLAMP_TOL_BPS: u64 = 20_000_000;

            let mut probe = self.try_open_session(requested_bps, split_mode);
            // Disambiguate a forced-split rejection from a bitrate-cap rejection.
            let split_on =
                split_mode != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
            if probe.is_err() && split_on {
                let no_split = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                if let Ok(e) = self.try_open_session(requested_bps, no_split) {
                    tracing::warn!(
                        "NVENC (Linux): split-encode rejected by codec/config — disabled"
                    );
                    split_mode = no_split;
                    probe = Ok(e);
                }
            }

            let enc = match probe {
                Ok(enc) => {
                    self.bitrate_bps = requested_bps;
                    enc
                }
                Err(_) => {
                    // Requested bitrate exceeds the codec-level ceiling — binary-search the max accepted.
                    let mut lo = FLOOR_BPS;
                    let mut hi = requested_bps;
                    let mut best: *mut c_void = ptr::null_mut();
                    let mut best_bps = 0u64;
                    while hi > lo + CLAMP_TOL_BPS {
                        let mid = lo + (hi - lo) / 2;
                        match self.try_open_session(mid, split_mode) {
                            Ok(e) => {
                                if !best.is_null() {
                                    let _ = (api().destroy_encoder)(best);
                                }
                                best = e;
                                best_bps = mid;
                                lo = mid;
                            }
                            Err(_) => hi = mid,
                        }
                    }
                    if best.is_null() {
                        let no_split =
                            nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                        best = self
                            .try_open_session(FLOOR_BPS, split_mode)
                            .or_else(|_| self.try_open_session(FLOOR_BPS, no_split))
                            .context(
                                "NVENC initialize_encoder rejected even at the floor bitrate",
                            )?;
                        best_bps = FLOOR_BPS;
                    }
                    tracing::warn!(
                        requested_mbps = requested_bps / 1_000_000,
                        clamped_mbps = best_bps / 1_000_000,
                        "NVENC (Linux): requested bitrate above the GPU codec-level ceiling — clamped"
                    );
                    self.bitrate_bps = best_bps;
                    best
                }
            };
            self.encoder = enc;
            // (Best effort: the floor fallback above may have succeeded split-disabled without
            // updating `split_mode` — a later reconfigure then presents the forced mode, NVENC
            // rejects it, and the caller's rebuild fallback covers the mismatch.)
            self.split_mode = split_mode;

            // Output bitstream pool.
            for _ in 0..POOL {
                let mut cb = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                (api().create_bitstream_buffer)(enc, &mut cb)
                    .nv_ok()
                    .map_err(|e| anyhow!("create_bitstream_buffer: {e:?}"))?;
                self.bitstreams.push(cb.bitstreamBuffer);
            }

            // Encoder-owned input-surface ring: allocate + register POOL surfaces in the negotiated
            // format. Registered once here, mapped per submit, unregistered at teardown.
            for _ in 0..POOL {
                let surface = match self.buffer_fmt {
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => {
                        InputSurface::alloc_yuv444(self.width, self.height)
                    }
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                        InputSurface::alloc_nv12(self.width, self.height)
                    }
                    _ => InputSurface::alloc_rgb(self.width, self.height),
                }
                .context("alloc NVENC input surface")?;
                let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType:
                        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                    width: self.width,
                    height: self.height,
                    pitch: surface.pitch as u32,
                    resourceToRegister: surface.ptr as *mut c_void,
                    bufferFormat: self.buffer_fmt,
                    bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                (api().register_resource)(self.encoder, &mut rr)
                    .nv_ok()
                    .map_err(|e| anyhow!("register_resource (CUDADEVICEPTR): {e:?}"))?;
                self.ring.push(RingSlot {
                    surface,
                    reg: rr.registeredResource,
                });
            }

            self.inited = true;
            tracing::info!(
                "NVENC CUDA session: {}x{}@{} {}-bit {} Mbps {:?} fmt={:?}",
                self.width,
                self.height,
                self.fps,
                self.bit_depth,
                self.bitrate_bps / 1_000_000,
                self.codec_guid,
                self.buffer_fmt,
            );
            Ok(())
        }
    }

    /// Copy the captured `DeviceBuffer` into the ring slot's registered input surface (device→device
    /// on the shared context, synchronized by the copy helpers).
    fn copy_into_slot(&self, buf: &cuda::DeviceBuffer, slot: usize) -> Result<()> {
        let s = &self.ring[slot].surface;
        let base = s.ptr;
        let pitch = s.pitch;
        let hh = s.height as u64;
        match self.buffer_fmt {
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => {
                if !buf.yuv444 {
                    bail!("4:4:4 session but the captured buffer is not planar YUV444");
                }
                let planes = [
                    (base, pitch),
                    (base + pitch as u64 * hh, pitch),
                    (base + 2 * pitch as u64 * hh, pitch),
                ];
                cuda::copy_yuv444_to_device(buf, planes)
            }
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                if !buf.is_nv12() {
                    bail!("NV12 session but the captured buffer has no chroma plane");
                }
                // Contiguous NV12: UV follows Y at base + pitch*height, same pitch.
                cuda::copy_nv12_to_device(buf, base, pitch, base + pitch as u64 * hh, pitch)
            }
            _ => cuda::copy_device_to_device(buf, base, pitch),
        }
    }
}

impl Encoder for NvencCudaEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let buf = match &captured.payload {
            FramePayload::Cuda(b) => b,
            _ => bail!(
                "Linux direct-NVENC needs a CUDA frame (FramePayload::Cuda); got a CPU/dmabuf frame"
            ),
        };
        // Re-init on a size change (the capturer can return at a different resolution after a mode
        // switch). Format changes (NV12↔YUV444) likewise re-init.
        let new_fmt = buffer_format(buf);
        let size_changed =
            self.inited && (self.width != captured.width || self.height != captured.height);
        let fmt_changed = self.inited && self.buffer_fmt != new_fmt;
        if self.inited && (size_changed || fmt_changed) {
            tracing::info!(
                size_changed,
                fmt_changed,
                new = format!("{}x{}", captured.width, captured.height),
                "NVENC (Linux): capture size/format changed — re-initializing session"
            );
            // SAFETY: `teardown` requires the encode thread with no NVENC call in flight and a session
            // whose cached ring/bitstreams/pending all belong to `self.encoder` — all hold: this is
            // the synchronous encode thread, `self.inited` so `self.encoder` is live, and the previous
            // frame was already polled (synchronous submit→poll), so nothing is mid-encode.
            unsafe { self.teardown() };
        }
        if !self.inited {
            self.width = captured.width;
            self.height = captured.height;
            self.buffer_fmt = new_fmt;
            // 4:4:4 honesty: engage FREXT only on a genuine YUV444 input; a subsampled NV12/RGB input
            // can't reconstruct full chroma, so clear the flag so `caps().chroma_444` is truthful.
            self.chroma_444 = self.chroma_444 && buf.yuv444;
            self.init_session()?;
        } else {
            // Steady state: the copy helpers need the shared context current on this thread.
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;
        }

        // The session's opening frame is an IDR regardless of pic flags. Detected via the still-empty
        // output slot counter (`teardown` zeroes it), NOT `pts`: `submit_indexed` pins pts to the
        // wire frame index, non-zero on a mid-session rebuild's first frame.
        let opening = self.next == 0;
        let slot = self.next % POOL;
        self.next += 1;

        // Copy the captured buffer into this slot's input surface before encoding it.
        self.copy_into_slot(buf, slot)?;

        // SAFETY: every NVENC call goes through a function pointer from the runtime table and takes
        // `self.encoder`, the live session `init_session` established (non-null here). `mp`
        // (`NV_ENC_MAP_INPUT_RESOURCE`, version set) maps the ring slot's registration (created in
        // `init_session`) and is recorded in `pending` to be unmapped exactly once in `poll`/teardown.
        // `pic` (`NV_ENC_PIC_PARAMS`, version set) points `inputBuffer` at `mp.mappedResource` and
        // `outputBitstream` at the live pool bitstream `bitstreams[slot]`; the optional SEI scratch is
        // stack-local and outlives the synchronous `encode_picture`. The input surface for `slot` was
        // just filled by the (synchronized) device→device copy and is not overwritten until this slot
        // is reused POOL submits later, by which time this encode was polled (POOL ≥ in-flight depth).
        unsafe {
            let reg = self.ring[slot].reg;
            let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
                version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: reg,
                ..Default::default()
            };
            (api().map_input_resource)(self.encoder, &mut mp)
                .nv_ok()
                .map_err(|e| anyhow!("map_input_resource: {e:?}"))?;

            let pts = self.frame_idx as u64;
            self.frame_idx += 1;
            let flags = if std::mem::take(&mut self.force_kf) {
                nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            };
            // Recovery anchor (armed by a successful invalidate_ref_frames): THIS frame is the first
            // encoded after the invalidation. A simultaneous forced IDR is itself the re-anchor, so
            // the tag is dropped in that case.
            let anchor = std::mem::take(&mut self.pending_anchor) && flags == 0;
            let mut pic = nv::NV_ENC_PIC_PARAMS {
                version: nv::NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: self.ring[slot].surface.pitch as u32,
                inputBuffer: mp.mappedResource,
                bufferFmt: mp.mappedBufferFmt,
                outputBitstream: self.bitstreams[slot],
                pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: pts,
                encodePicFlags: flags,
                ..Default::default()
            };

            // In-band HDR10 SEI on every IDR when in HDR mode (inert on Linux until Phase 5.1 lands a
            // 10-bit input, but ported so the wiring is complete). HEVC/H.264 carry SEI; AV1 = OBUs.
            let is_idr = flags != 0 || opening;
            let mastering_sei = self
                .hdr_meta
                .map(|m| crate::hdr::hevc_mastering_display_sei(&m));
            let cll_sei = self
                .hdr_meta
                .map(|m| crate::hdr::hevc_content_light_level_sei(&m));
            let mut sei: Vec<nv::NV_ENC_SEI_PAYLOAD> = Vec::new();
            if is_idr && self.hdr {
                if let Some(p) = mastering_sei.as_ref() {
                    sei.push(nv::NV_ENC_SEI_PAYLOAD {
                        payloadSize: p.len() as u32,
                        payloadType: crate::hdr::SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME,
                        payload: p.as_ptr() as *mut u8,
                    });
                }
                if let Some(p) = cll_sei.as_ref() {
                    sei.push(nv::NV_ENC_SEI_PAYLOAD {
                        payloadSize: p.len() as u32,
                        payloadType: crate::hdr::SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO,
                        payload: p.as_ptr() as *mut u8,
                    });
                }
            }
            if !sei.is_empty() {
                match self.codec {
                    Codec::H265 => {
                        pic.codecPicParams.hevcPicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.hevcPicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    Codec::H264 => {
                        pic.codecPicParams.h264PicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.h264PicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    Codec::Av1 => {}
                    Codec::PyroWave => {
                        unreachable!("PyroWave never opens the direct-NVENC backend")
                    }
                }
            }
            (api().encode_picture)(self.encoder, &mut pic)
                .nv_ok()
                .map_err(|e| anyhow!("encode_picture: {e:?}"))?;
            self.pending.push_back((
                self.bitstreams[slot],
                mp.mappedResource,
                captured.pts_ns,
                anchor,
            ));
        }
        Ok(())
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: self.rfi_supported,
            supports_hdr_metadata: self.hdr,
            chroma_444: self.chroma_444,
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.hdr_meta = meta;
    }

    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if self.encoder.is_null() || !self.rfi_supported || first < 0 || first > last {
            return false;
        }
        // Already invalidated a covering range for this loss event — re-arm the anchor (the previous
        // anchor AU may itself have been lost) but skip the driver calls.
        if let Some((pf, pl)) = self.last_rfi_range {
            if first >= pf && last <= pl {
                self.pending_anchor = true;
                return true;
            }
        }
        // The DPB holds `[frame_idx - RFI_DPB, frame_idx - 1]`; a lost frame older than that can't be
        // invalidated, so the only correct recovery is an IDR.
        let oldest_in_dpb = self.frame_idx - RFI_DPB as i64;
        if first < oldest_in_dpb {
            return false;
        }
        let last = last.min(self.frame_idx - 1);
        if first > last {
            return false;
        }
        // Each input's `inputTimeStamp` is the WIRE frame index (pinned by `submit_indexed`), so the
        // client's lost-frame range maps 1:1 onto the timestamps NVENC invalidates here.
        // SAFETY: `invalidate_ref_frames` is a function pointer from the runtime table; `self.encoder`
        // was checked non-null and is the live session; this runs on the encode thread (no concurrent
        // NVENC use). Each `ts` is clamped to `[oldest_in_dpb, frame_idx - 1]`, naming a frame still
        // in the DPB; the call passes only that `u64` (no struct).
        unsafe {
            for ts in first..=last {
                if (api().invalidate_ref_frames)(self.encoder, ts as u64)
                    .nv_ok()
                    .is_err()
                {
                    return false;
                }
            }
        }
        self.last_rfi_range = Some((first, last));
        // The next submitted frame is the clean re-anchor — arm the tag so its AU ships with
        // `recovery_anchor` and the client lifts its post-loss freeze on it.
        self.pending_anchor = true;
        true
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some((bs, map, pts_ns, anchor)) = self.pending.pop_front() else {
            return Ok(None);
        };
        // SAFETY: a non-empty `pending` implies `submit` ran, so `self.encoder` is the live session
        // (`teardown` clears `pending` whenever it nulls the handle); all calls use function pointers
        // from the runtime table on the encode thread. `lock_bitstream` (version set) locks `bs`, a
        // pool bitstream a prior `encode_picture` targeted, and blocks until that encode finishes, so
        // `lock.bitstreamBufferPtr` points at `bitstreamSizeInBytes` bytes of CPU-readable output
        // valid until `unlock_bitstream`; the slice is copied (`to_vec`) BEFORE the unlock on the same
        // buffer. `map` (paired with `bs` in `pending`) is unmapped here, after the encode completed,
        // exactly once.
        unsafe {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: bs,
                ..Default::default()
            };
            (api().lock_bitstream)(self.encoder, &mut lock)
                .nv_ok()
                .map_err(|e| anyhow!("lock_bitstream: {e:?}"))?;
            let data = std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
            .to_vec();
            let keyframe = matches!(
                lock.pictureType,
                nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            (api().unlock_bitstream)(self.encoder, bs)
                .nv_ok()
                .map_err(|e| anyhow!("unlock_bitstream: {e:?}"))?;
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, map);
            }
            Ok(Some(EncodedFrame {
                data,
                pts_ns,
                keyframe,
                recovery_anchor: anchor,
                chunk_aligned: false,
            }))
        }
    }

    fn reset(&mut self) -> bool {
        // SAFETY: `teardown` requires the encode thread with no NVENC call in flight and a session
        // whose cached resources belong to `self.encoder` — all hold here (reset is called from the
        // session loop between submit/poll), and it early-returns on an already-null session.
        unsafe { self.teardown() };
        self.force_kf = true;
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        if !self.inited {
            // No live session yet — the lazy init simply opens at the new rate.
            self.bitrate_bps = bps;
            return true;
        }
        // SAFETY: `inited` ⟹ `self.encoder` is the live session and every call here runs on the
        // encode thread with no NVENC call in flight (the session loop calls this between
        // submit/poll). `build_config` only queries the preset on that session; `cfg` outlives
        // the synchronous reconfigure call whose `reInitEncodeParams.encodeConfig` points at it.
        unsafe {
            let mut cfg = match self.build_config(self.encoder, bps) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "NVENC reconfigure: config re-author failed — falling back to a rebuild");
                    return false;
                }
            };
            let mut params = nv::NV_ENC_RECONFIGURE_PARAMS {
                version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
                reInitEncodeParams: self.build_init_params(&mut cfg, self.split_mode),
                ..Default::default()
            };
            // Keep the encoder's RC state and reference chain: no reset, no IDR — the in-flight
            // frames and the caller's wire-index prediction survive the retarget.
            params.set_resetEncoder(0);
            params.set_forceIDR(0);
            match (api().reconfigure_encoder)(self.encoder, &mut params).nv_ok() {
                Ok(()) => {
                    self.bitrate_bps = bps;
                    true
                }
                Err(e) => {
                    // E.g. the new rate is above the codec-level ceiling — the caller's rebuild
                    // fallback owns the clamp search.
                    tracing::warn!(status = ?e, mbps = bps / 1_000_000,
                        "nvEncReconfigureEncoder rejected — falling back to a rebuild");
                    false
                }
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + frameIntervalP=1: each submit yields its AU; no internal queue to drain.
    }
}

impl Drop for NvencCudaEncoder {
    fn drop(&mut self) {
        // SAFETY: at Drop this encoder is owned exclusively, runs on the encode thread it was confined
        // to, and `teardown` early-returns on a null session; otherwise every cached ring/bitstream/
        // pending was created against that live session. Runs exactly once (here).
        unsafe { self.teardown() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{CapturedFrame, FramePayload, PixelFormat};
    use crate::zerocopy::cuda::DeviceBuffer;

    fn nv12_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        // Content is uninitialized device memory — NVENC encodes it fine; this smoke test asserts the
        // session/registration/encode/RFI machinery, not picture fidelity (that's the on-glass A/B).
        let buf = DeviceBuffer::alloc_nv12(w, h).expect("alloc NV12 device buffer");
        CapturedFrame {
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
        }
    }

    /// ON-HARDWARE (RTX box `.21`): drives the full direct-SDK CUDA path end to end — open the
    /// session on the shared `CUcontext`, allocate + register the CUDA input-surface ring, encode
    /// synthetic NV12 frames, then perform a **real** `invalidate_ref_frames` over an in-DPB range
    /// and assert the next AU carries the recovery-anchor tag (the F2 fix) and that `caps()`
    /// advertises RFI. Needs an NVIDIA GPU + driver. Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_cuda_smoke --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_smoke_rfi_anchor() {
        const W: u32 = 1280;
        const H: u32 = 720;
        crate::zerocopy::cuda::make_current().expect("shared CUDA context current");

        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("open NVENC CUDA session");

        // Warm up: submit 8 frames pinning wire indices 0..8; the sync poll returns one AU per frame.
        let mut aus = 0usize;
        let mut first_key = false;
        for i in 0..8u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                if aus == 0 {
                    first_key = au.keyframe;
                }
                aus += 1;
            }
        }
        assert!(aus > 0, "no AUs produced");
        assert!(
            first_key,
            "first AU must be a keyframe (session opening IDR)"
        );
        assert!(enc.caps().supports_rfi, "RTX NVENC must advertise RFI");

        // Invalidate a recent, still-in-DPB range (frames 5..=6; DPB depth is RFI_DPB=5, so 3..=7 are
        // live). Must perform a real reference invalidation, not fall back to IDR.
        assert!(
            enc.invalidate_ref_frames(5, 6),
            "invalidate_ref_frames should succeed for an in-DPB range"
        );

        // The next submitted frame is the clean re-anchor — its AU must be tagged recovery_anchor and
        // must NOT be a forced IDR (RFI recovers via a P-frame, not a keyframe).
        let frame = nv12_frame(W, H, 8);
        enc.submit_indexed(&frame, 8).expect("submit post-RFI");
        let mut saw_anchor = false;
        let mut anchor_was_keyframe = false;
        while let Some(au) = enc.poll().expect("poll") {
            if au.recovery_anchor {
                saw_anchor = true;
                anchor_was_keyframe = au.keyframe;
            }
        }
        assert!(
            saw_anchor,
            "the post-RFI AU must carry recovery_anchor (the F2 fix)"
        );
        assert!(
            !anchor_was_keyframe,
            "RFI re-anchor must be a P-frame, not an IDR"
        );
        enc.flush().ok();
        println!(
            "nvenc_cuda smoke: {aus} AUs, RFI succeeded, recovery-anchor tagged on the P-frame"
        );
    }

    /// ON-HARDWARE (RTX box `.21`): the 4:4:4 path — a planar-YUV444 `DeviceBuffer` through an HEVC
    /// FREXT (chromaFormatIDC=3) session, exercising the stacked-plane input surface + copy that NV12
    /// doesn't. Asserts AUs come out and `caps().chroma_444` reports true (the GPU supports it). Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_cuda_yuv444 --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_yuv444() {
        const W: u32 = 1280;
        const H: u32 = 720;
        crate::zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Yuv444,
            W,
            H,
            60,
            40_000_000,
            true,
            8,
            ChromaFormat::Yuv444,
        )
        .expect("open NVENC CUDA 4:4:4 session");

        let mut aus = 0usize;
        for i in 0..6u32 {
            let buf = DeviceBuffer::alloc_yuv444(W, H).expect("alloc YUV444 device buffer");
            let frame = CapturedFrame {
                width: W,
                height: H,
                pts_ns: i as u64 * 16_666_667,
                format: PixelFormat::Yuv444,
                payload: FramePayload::Cuda(buf),
            };
            enc.submit_indexed(&frame, i).expect("submit 444");
            while let Some(_au) = enc.poll().expect("poll") {
                aus += 1;
            }
        }
        assert!(aus > 0, "no 4:4:4 AUs produced");
        assert!(enc.caps().chroma_444, "RTX NVENC HEVC must report 4:4:4");
        println!("nvenc_cuda 4:4:4 smoke: {aus} AUs, caps.chroma_444=true");
    }

    /// ON-HARDWARE (RTX box `.21`): the Phase 3.2 in-place rate retarget — encode a few frames,
    /// `reconfigure_bitrate` mid-stream (up AND down), keep encoding, and assert every
    /// post-reconfigure AU is a P-frame: `nvEncReconfigureEncoder` with `resetEncoder=0` /
    /// `forceIDR=0` must NOT restart the stream (the whole point vs. the rebuild path). Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_cuda_reconfigure --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_reconfigure_no_idr() {
        const W: u32 = 1280;
        const H: u32 = 720;
        crate::zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let mut keyframes = 0usize;
            let mut aus = 0usize;
            for i in range {
                let frame = nv12_frame(W, H, i);
                enc.submit_indexed(&frame, i).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    aus += 1;
                    keyframes += au.keyframe as usize;
                }
            }
            (aus, keyframes)
        };

        let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
        assert!(aus > 0, "no AUs before the reconfigure");
        assert_eq!(kfs, 1, "exactly the opening IDR before the reconfigure");

        assert!(
            enc.reconfigure_bitrate(60_000_000),
            "in-place reconfigure to 60 Mbps must succeed on RTX NVENC"
        );
        let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
        assert!(aus > 0, "no AUs after the up-reconfigure");
        assert_eq!(kfs, 0, "an in-place rate retarget must not emit an IDR");

        assert!(
            enc.reconfigure_bitrate(10_000_000),
            "in-place reconfigure down to 10 Mbps must succeed"
        );
        let (aus, kfs) = submit_and_poll(&mut enc, 8..12);
        assert!(aus > 0, "no AUs after the down-reconfigure");
        assert_eq!(kfs, 0, "an in-place rate retarget must not emit an IDR");

        enc.flush().ok();
        println!("nvenc_cuda reconfigure smoke: 20→60→10 Mbps in place, zero IDRs");
    }

    /// A pre-session RFI request and nonsense ranges all correctly decline (→ caller forces IDR).
    /// Needs no GPU session (it short-circuits on the null encoder / range checks), so it runs in the
    /// normal suite — but `open` gates on the NVENC `.so`, so it skips gracefully where the NVIDIA
    /// driver is absent (driverless CI) instead of failing.
    #[test]
    fn rfi_declines_impossible_ranges() {
        let Ok(mut enc) = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            1920,
            1080,
            60,
            20_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
        ) else {
            eprintln!(
                "skipping rfi_declines_impossible_ranges: NVENC unavailable (no NVIDIA driver)"
            );
            return;
        };
        // No live session yet (lazy init on first frame) → every RFI request declines.
        assert!(!enc.invalidate_ref_frames(0, 0), "no session → decline");
        assert!(!enc.invalidate_ref_frames(10, 5), "first > last → decline");
        assert!(
            !enc.invalidate_ref_frames(-1, 3),
            "negative first → decline"
        );
    }
}
