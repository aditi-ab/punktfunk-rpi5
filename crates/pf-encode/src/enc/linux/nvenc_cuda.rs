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
//!     (NVENC rejects a null-`buf[0]` frame; the captured buffer is worker-owned CUDA-IPC memory
//!     recycled on drop, so registering it directly needs a contiguous worker-pool layout + a
//!     registration↔IPC-mapping lifetime tie — the true zero-copy follow-up, plan §7 LN2 v2).
//!     **Stream-ordered submit** (default, `PUNKTFUNK_NVENC_STREAM_ORDERED=0` reverts): the
//!     session's IO streams are bound to the encode thread's copy stream
//!     (`NvEncSetIOCudaStreams`), so in sync-retrieve depth-1 use the copy + cursor blend enqueue
//!     with NO per-frame `cuStreamSynchronize` and the encode orders after them on the stream —
//!     the submit path's CPU stalls are gone even though the copy itself remains.
//!
//! **Two-thread retrieve** (`PUNKTFUNK_NVENC_ASYNC`: `1` = always, `0` = never, unset =
//! **adaptive** — engaged by the session loop's contention escalation via
//! [`Encoder::set_pipelined`] when depth-1 can't hold cadence; at depth-1 it costs ~one loop
//! tick of latency, which is why it is not simply on. gpu-contention plan §5.B, latency plan
//! T2.2/§7 LN3): NVENC *async mode*
//! (`enableEncodeAsync` + completion events) is Windows-only, so the session here stays SYNC —
//! but the NVENC guide's threading model still applies: the main thread should only *submit*
//! while a secondary thread does the (blocking) `nvEncLockBitstream`. With the flag set, an
//! internal retrieve thread owns exactly that blocking lock (+ copy + unlock); `submit` returns
//! after `encode_picture` and `poll` drains finished AUs without blocking, so under a
//! GPU-saturating game completed frames queue instead of serializing capture on the scheduler
//! wait. All input-resource calls (register/map/unmap) and every other session call stay on the
//! encode thread. Backpressure: `submit` blocks on the oldest completion at
//! `PUNKTFUNK_NVENC_ASYNC_DEPTH` (default 4) in-flight encodes. Without the flag, `poll` does
//! the blocking `lock_bitstream` on the encode thread, exactly like the libav path (unchanged
//! default). Caveat shared with the sync path: a driver wedge that hangs `lock_bitstream` hangs
//! the retrieve thread the same way it would hang the encode thread today (Linux has no
//! event-timeout escape) — no regression, just no new watchdog either.
//!
//! **Sub-frame chunked poll** (latency plan §7 LN1 — **default-on since Phase 3**: 4 slices +
//! sub-frame readback on every session whose GPU advertises `SUBFRAME_READBACK`; escapes are
//! `PUNKTFUNK_NVENC_SLICES=1` and `PUNKTFUNK_NVENC_SUBFRAME=0`): on a sync depth-1 session,
//! [`Encoder::poll_chunk`] hands the in-flight AU out as slice-boundary chunks read through
//! `doNotWait` sub-frame locks while the tail is still encoding; one final blocking lock closes
//! the AU (the completion authority — `numSlices` alone is not trusted across driver branches).
//! Mutually exclusive with the pipelined retrieve (the escalated rebuild drops it); composes
//! with stream-ordered submit (both are sync depth-1 features).
//!
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise); compiles GPU-less and
//! starts driver-less (the `.so` resolves at runtime — on an AMD/Intel box [`try_api`] fails cleanly
//! and the VAAPI/software backends carry the session).

// UNSAFE-LINT EXEMPTION (rationale + exit criteria: `unsafe_op_in_unsafe_fn` in the workspace
// Cargo.toml). This body is raw CUDA driver + `nvEncodeAPI` entry-table calls almost line for line;
// narrowing it would add one `unsafe {}` plus one SAFETY comment per call that could only restate
// the signature. Clearing this file means DELETING the markers that carry no caller contract, not
// wrapping the calls — until then the lint is off HERE and enforced everywhere else.
#![allow(unsafe_op_in_unsafe_fn)]

use super::nvenc_core::{
    apply_low_latency_config, build_init_params, cached_ceiling, cached_split_verdict, codec_guid,
    plan_range_recovery, resolve_slices, resolve_split_subframe, resolve_subframe, store_ceiling,
    store_split_verdict, subframe_env_forced, ArbAction, CeilingKey, LowLatencyConfig, NvStatusExt,
    RangePlan, SplitArbiter, SplitKey,
};
use super::nvenc_status;
use super::{max_forced_split_mode, resolve_split_mode};
use super::{AuChunk, ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pf_zerocopy::cuda::{self, InputSurface};
use pf_zerocopy::vkslot::{SlotFormat, VkSlotBlend, VkSlotRef};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;

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
    // The two entry points behind [`probe_support`] — the driver's own list of encode GUIDs
    // this chip exposes. Mandatory like every other entry: both have existed since NVENC 1.0, so a
    // driver missing them is broken in ways the rest of this table would not survive either.
    get_encode_guid_count: unsafe extern "C" fn(*mut c_void, *mut u32) -> nv::NVENCSTATUS,
    get_encode_guids:
        unsafe extern "C" fn(*mut c_void, *mut nv::GUID, u32, *mut u32) -> nv::NVENCSTATUS,
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
    /// `NvEncSetIOCudaStreams` — binds the session's input/output ordering to a CUDA stream so the
    /// input copy + cursor blend can enqueue without a CPU sync (stream-ordered submit). The two
    /// `NV_ENC_CUSTREAM_PTR` args are pointers TO `CUstream` values.
    set_io_cuda_streams: unsafe extern "C" fn(
        *mut c_void,
        nv::NV_ENC_CUSTREAM_PTR,
        nv::NV_ENC_CUSTREAM_PTR,
    ) -> nv::NVENCSTATUS,
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
                tracing::warn!(error = %e, "NVENC (Linux direct) API unavailable");
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

/// Everything the host advertisement asks of this GPU's NVENC, answered by the driver itself on
/// ONE throwaway session: the encode-GUID list (which codecs exist at all) and the HEVC 4:4:4 cap.
#[derive(Clone, Copy)]
pub(crate) struct ProbedSupport {
    /// Which codecs this chip's NVENC encodes (`nvEncGetEncodeGUIDs`). All-`false` = the probe
    /// could not answer — [`crate::CodecSupport::wire_mask`] turns that into `None` so the caller
    /// keeps the static superset (fail open).
    pub codecs: crate::CodecSupport,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` for the HEVC GUID — whether this chip can encode
    /// full-chroma 4:4:4 HEVC. `false` when unanswered (fail CLOSED, unlike `codecs`: the honest
    /// downgrade is a 4:2:0 session, not a dead one).
    pub hevc_444: bool,
    /// `NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` per listed codec — HEVC Main10 / AV1 10-bit. Rides this
    /// same session for exactly the reason 4:4:4 does: the alternative, `linux::probe_can_encode_10bit`,
    /// answers by opening an ffmpeg `hevc_nvenc`, and one ffmpeg NVENC open in a direct-SDK process
    /// is the LOG-3 field bug that wedges every later open process-wide with
    /// `NV_ENC_ERR_INVALID_VERSION`. That probe was left on ffmpeg when the 4:4:4 one was moved off
    /// it, on the reading that "Linux HDR rides the libav P010 path" — no longer true for a CUDA
    /// capture, which `open_video` sends to the direct backend (`bit_depth` passes straight through,
    /// and `is_ten_bit_input` accepts packed 10-bit RGB). `false` when unanswered — fail CLOSED, an
    /// 8-bit session beats a wedged one.
    pub ten_bit: crate::CodecSupport,
}

/// The cached [`probe_support_uncached`] answer — one throwaway session per process lifetime.
pub(crate) fn probe_support() -> ProbedSupport {
    static CACHE: std::sync::OnceLock<ProbedSupport> = std::sync::OnceLock::new();
    *CACHE.get_or_init(probe_support_uncached)
}

/// Which codecs **this GPU's** NVENC can actually encode — and whether HEVC can go 4:4:4 — asked
/// of the driver itself (`nvEncGetEncodeGUIDs` + `nvEncGetEncodeCaps`) instead of assumed from the
/// SDK version.
///
/// Why this exists: the host used to advertise a static `H.264 | HEVC | AV1` superset for every
/// NVIDIA box, so a chip without HEVC NVENC (1st-gen Maxwell, e.g. GTX 960M — HEVC needs 2nd-gen
/// Maxwell+, AV1 needs Ada+) still offered HEVC. A client reasonably negotiated H265 and got a dead
/// session: `hevc_nvenc` "No capable devices found", eight pipeline retries, ~15 s of blank video,
/// then a disconnect. The GUID list is a property of the chip+driver, so it is equally right for
/// the direct-SDK backend and the libav `*_nvenc` one.
///
/// ⚠️ Deliberately NOT the VAAPI probe's shape (open a tiny libav encoder per codec). That would run
/// ffmpeg's NVENC client, and mixing it with this direct-SDK client in one process is the prime
/// suspect for the open bug where one `probe_can_encode_444` open wedges NVENC **process-wide**
/// (`NV_ENC_ERR_INVALID_VERSION` on every later session until a host restart — LOG-3, Droff,
/// 0.19.2). This asks the SAME client, on the SAME shared CUDA context, that real sessions use —
/// one extra session open of a kind the encoder already performs per open (`query_caps`), cached
/// once per process by [`probe_support`]. The 4:4:4 cap rides the same session for the same
/// reason: it used to be its own libav `hevc_nvenc` FREXT open — the exact open LOG-3 caught
/// wedging NVENC — and the direct backend re-checks the same cap at session open anyway
/// (`query_caps` → `yuv444_supported`), so the caps bit is the answer the live session will obey.
///
/// Every failure path returns "nothing probed" (see the [`ProbedSupport`] field docs for the
/// per-field fail direction).
fn probe_support_uncached() -> ProbedSupport {
    let unknown = ProbedSupport {
        codecs: crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        },
        hevc_444: false,
        ten_bit: crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        },
    };
    let Ok(api) = try_api() else {
        return unknown;
    };
    let cu_ctx = match cuda::context() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "NVENC codec probe: no CUDA context");
            return unknown;
        }
    };
    // SAFETY: `try_api()` returned Ok, so every fn pointer below is a live entry point from the
    // driver's own function list. `params`/`enc`/`count`/`written` are live locals that outlive
    // their synchronous calls; `device` is the process-shared CUDA context (`cuda::context()`
    // returned Ok), the same handle `query_caps` passes. `guids` is sized to the count the driver
    // just reported and its pointer is valid for that many `GUID`s, matching the
    // `guidArraySize` argument. The session is destroyed on every path out — including the failed
    // open, which the NVENC docs still require (the driver may have taken the slot before
    // erroring; skipping it leaks toward the concurrent-session cap).
    unsafe {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device: cu_ctx,
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        if let Err(e) = (api.open_encode_session_ex)(&mut params, &mut enc).nv_ok() {
            if !enc.is_null() {
                let _ = (api.destroy_encoder)(enc);
            }
            tracing::warn!(
                error = %format!("{:#}", nvenc_status::call_err("open_encode_session_ex (codec probe)", e)),
                "NVENC codec probe failed — keeping the static codec advertisement"
            );
            return unknown;
        }
        // The handshake with the kernel module succeeded (same latch `query_caps` sets).
        nvenc_status::note_session_opened();
        let mut count = 0u32;
        let counted = (api.get_encode_guid_count)(enc, &mut count).nv_ok().is_ok();
        let mut guids = vec![nv::GUID::default(); count as usize];
        let mut written = 0u32;
        let listed = counted
            && count > 0
            && (api.get_encode_guids)(enc, guids.as_mut_ptr(), count, &mut written)
                .nv_ok()
                .is_ok();
        guids.truncate(written as usize);
        // The 4:4:4 cap needs the session that is still open — query it before the destroy. Only
        // meaningful against a listed HEVC GUID (a cap query for an absent codec is undefined).
        let mut hevc_444 = false;
        if listed && guids.contains(&nv::NV_ENC_CODEC_HEVC_GUID) {
            let mut param = nv::NV_ENC_CAPS_PARAM {
                version: nv::NV_ENC_CAPS_PARAM_VER,
                capsToQuery: nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                reserved: [0; 62],
            };
            let mut val: core::ffi::c_int = 0;
            hevc_444 = (api.get_encode_caps)(enc, nv::NV_ENC_CODEC_HEVC_GUID, &mut param, &mut val)
                .nv_ok()
                .is_ok()
                && val != 0;
        }
        // The 10-bit cap, per codec, on the SAME still-open session — same reason 4:4:4 rides it.
        // Only queried against a listed GUID (a cap query for an absent codec is undefined).
        let mut ten_bit = crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        if listed {
            for (guid, slot) in [
                (nv::NV_ENC_CODEC_HEVC_GUID, &mut ten_bit.h265),
                (nv::NV_ENC_CODEC_AV1_GUID, &mut ten_bit.av1),
            ] {
                if !guids.contains(&guid) {
                    continue;
                }
                let mut param = nv::NV_ENC_CAPS_PARAM {
                    version: nv::NV_ENC_CAPS_PARAM_VER,
                    capsToQuery: nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
                    reserved: [0; 62],
                };
                let mut val: core::ffi::c_int = 0;
                *slot = (api.get_encode_caps)(enc, guid, &mut param, &mut val)
                    .nv_ok()
                    .is_ok()
                    && val != 0;
            }
        }
        let _ = (api.destroy_encoder)(enc);
        if !listed {
            tracing::warn!(
                "NVENC codec probe: driver listed no encode GUIDs — keeping the static advertisement"
            );
            return unknown;
        }
        ProbedSupport {
            codecs: crate::CodecSupport {
                h264: guids.contains(&nv::NV_ENC_CODEC_H264_GUID),
                h265: guids.contains(&nv::NV_ENC_CODEC_HEVC_GUID),
                av1: guids.contains(&nv::NV_ENC_CODEC_AV1_GUID),
            },
            hevc_444,
            ten_bit,
        }
    }
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
            get_encode_guid_count: list.nvEncGetEncodeGUIDCount.ok_or(MISSING)?,
            get_encode_guids: list.nvEncGetEncodeGUIDs.ok_or(MISSING)?,
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
            set_io_cuda_streams: list.nvEncSetIOCudaStreams.ok_or(MISSING)?,
        };
        std::mem::forget(lib); // keep the .so mapped for the fn pointers' lifetime (process)
        Ok(api)
    }
}

/// Output bitstream buffers = max in-flight encodes; equals the input-surface ring depth. Must
/// stay ≥ the two-thread retrieve's in-flight cap ([`async_inflight_cap`], ≤ `POOL - 1`) so a
/// bitstream/ring slot is never reused mid-encode.
const POOL: usize = 8;

/// The operator's `PUNKTFUNK_NVENC_ASYNC` intent (the SAME knob as the Windows backend):
/// `Some(true)` = force the two-thread retrieve from session open — note that at the Linux
/// default pipeline depth of 1 this adds ~one loop tick of latency (the non-blocking poll's AU
/// rides the next tick), so it only pays under GPU contention; `Some(false)` = never (also
/// vetoes the session loop's contention escalation via [`Encoder::set_pipelined`]); `None`
/// (unset) = adaptive — off until the session loop escalates on sustained cadence overrun.
/// Unlike Windows this changes NO session parameter (Linux stays sync mode; only the blocking
/// lock moves off the encode thread), so there is no async-rejecting config to fail the open.
fn async_retrieve_env() -> Option<bool> {
    match std::env::var("PUNKTFUNK_NVENC_ASYNC") {
        Ok(v) if matches!(v.trim(), "1" | "true" | "yes" | "on") => Some(true),
        Ok(v) if matches!(v.trim(), "0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}

/// Operator forced the two-thread retrieve on from session open (see [`async_retrieve_env`]).
fn async_retrieve_requested() -> bool {
    async_retrieve_env() == Some(true)
}

/// Max encodes in flight in two-thread mode (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4, clamped
/// `2..=POOL-1` — a bitstream must never be reused mid-encode, and the input ring is the same
/// depth). Mirrors the Windows knob exactly, memoization included: this is the backpressure
/// **loop condition** in `submit`, so an engaged two-thread session re-read the environment once
/// per spin. The default session never pays it (the condition short-circuits on `async_rt`), which
/// is why the audit's severity ranking for this site was inverted — but an escalated one did.
fn async_inflight_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("PUNKTFUNK_NVENC_ASYNC_DEPTH")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(2, POOL - 1)
    })
}

/// Stream-ordered submit (`PUNKTFUNK_NVENC_STREAM_ORDERED`, default ON; `0` = the pre-existing
/// blocking copies). With the session's IO streams bound to the encode thread's copy stream
/// (`NvEncSetIOCudaStreams`), the input copy + cursor blend enqueue with NO CPU sync and
/// `encode_picture` orders after them on the stream — deleting the 1–3 per-frame
/// `cuStreamSynchronize` stalls from the submit path (latency plan §7 LN2). Sync-retrieve mode
/// only, and only while nothing is in flight (see the gate in [`Encoder::submit`]).
fn stream_ordered_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_STREAM_ORDERED")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// One in-flight encode handed to the retrieve thread: the output bitstream to (blocking-)lock.
/// Raw pointer travels as `usize` (a process-global driver handle; the thread is joined before
/// the session it belongs to is destroyed).
struct RetrieveJob {
    bs: usize,
}

/// A finished retrieve: the locked-and-copied AU (or the retrieve-side error) for the oldest
/// in-flight bitstream. `bs` lets the encode thread cross-check FIFO pairing with `pending`.
struct RetrieveDone {
    bs: usize,
    result: std::result::Result<(Vec<u8>, bool), String>,
}

/// The two-thread-retrieve runtime: the job channel feeding the retrieve thread, the completion
/// channel back, the thread handle (joined in `teardown` BEFORE the session is destroyed), and
/// AUs already absorbed by backpressure that `poll` hands out first.
struct AsyncRetrieve {
    work_tx: Option<mpsc::SyncSender<RetrieveJob>>,
    done_rx: mpsc::Receiver<RetrieveDone>,
    join: Option<std::thread::JoinHandle<()>>,
    ready: VecDeque<EncodedFrame>,
}

impl AsyncRetrieve {
    fn spawn(enc: usize) -> Self {
        let (work_tx, work_rx) = mpsc::sync_channel::<RetrieveJob>(POOL);
        let (done_tx, done_rx) = mpsc::channel::<RetrieveDone>();
        let join = std::thread::Builder::new()
            .name("pf-nvenc-out".into())
            .spawn(move || retrieve_loop(enc, work_rx, done_tx))
            .expect("spawn pf-nvenc-out");
        AsyncRetrieve {
            work_tx: Some(work_tx),
            done_rx,
            join: Some(join),
            ready: VecDeque::new(),
        }
    }
}

/// The retrieve-thread body (latency plan T2.2, the Linux half of gpu-contention §5.B): for each
/// submitted frame, BLOCKING-lock the bitstream (sync-mode `nvEncLockBitstream` returns when the
/// encode completes — the guide's sanctioned secondary-thread surface), copy the AU out, unlock,
/// and send it back. Exits when the job channel closes (teardown drops the sender and joins
/// BEFORE destroying the session, so `enc` and every `bs` outlive their uses here).
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // The session is bound to the shared process-wide CUDA context; make it current here the
    // same way the encode thread does before its own NVENC calls.
    if let Err(e) = cuda::make_current() {
        tracing::warn!(error = %format!("{e:#}"), "pf-nvenc-out: cuCtxSetCurrent failed");
    }
    let mut jobs: u64 = 0;
    while let Ok(job) = work_rx.recv() {
        // In two-thread mode the host loop's `wait_us` wraps a non-blocking poll, so the real
        // encode wait (scheduling + ASIC) is measured by NO timer there — sample it here instead
        // (same PUNKTFUNK_PERF cadence as the submit split).
        let sample = pf_host_config::config().perf && jobs % 120 == 0;
        jobs += 1;
        let t0 = std::time::Instant::now();
        // SAFETY: `job.bs` is one of the session's pool bitstreams a prior `encode_picture`
        // targeted; both it and the session stay valid until `teardown`, which joins this thread
        // first. `lock_bitstream` (version set, struct a live stack local for the synchronous
        // call) BLOCKS until that encode finishes, then yields a CPU-readable
        // `bitstreamBufferPtr`/`bitstreamSizeInBytes` valid until `unlock_bitstream`; the slice
        // is copied (`to_vec`) before the unlock on the same buffer. Lock/unlock from a
        // secondary thread while the encode thread submits is the NVENC guide's documented
        // threading model.
        let result = unsafe {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: job.bs as *mut c_void,
                ..Default::default()
            };
            match (api().lock_bitstream)(enc as *mut c_void, &mut lock).nv_ok() {
                Ok(()) => {
                    let data = std::slice::from_raw_parts(
                        lock.bitstreamBufferPtr as *const u8,
                        lock.bitstreamSizeInBytes as usize,
                    )
                    .to_vec();
                    let keyframe = matches!(
                        lock.pictureType,
                        nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
                            | nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
                    );
                    let _ = (api().unlock_bitstream)(
                        enc as *mut c_void,
                        job.bs as nv::NV_ENC_OUTPUT_PTR,
                    );
                    Ok((data, keyframe))
                }
                Err(e) => Err(format!(
                    "lock_bitstream (retrieve thread): {e:?} — {}",
                    nvenc_status::explain(e)
                )),
            }
        };
        if sample {
            if let Ok((data, _)) = &result {
                tracing::info!(
                    lock_us = t0.elapsed().as_micros() as u64,
                    au_kib = (data.len() / 1024) as u64,
                    "NVENC retrieve lock (sampled): blocking lock_bitstream + AU copy on \
                     pf-nvenc-out (the async-mode encode wait)"
                );
            }
        }
        if done_tx.send(RetrieveDone { bs: job.bs, result }).is_err() {
            break; // encoder side gone (teardown drains us via join)
        }
    }
}

/// The NVENC input buffer format for a captured frame. NV12/YUV444 are the zero-copy worker's
/// convert outputs and are recognised from the `DeviceBuffer`'s layout; the packed formats are 4
/// bytes per pixel either way, so their DEPTH and channel order can only come from the capture
/// format — which is why `fmt` is a parameter and not something derived from `buf`.
///
/// Packed RGB lets NVENC do the CSC internally, which is exactly what an HDR gamescope session
/// wants: the frame is already PQ-encoded BT.2020 RGB, and NVENC's internal conversion follows the
/// configured VUI matrix (BT.2020 NCL for HDR — see `apply_low_latency_config`), so there is no
/// host-side CSC pass and no depth loss anywhere on the path.
fn buffer_format(buf: &cuda::DeviceBuffer, fmt: pf_frame::PixelFormat) -> nv::NV_ENC_BUFFER_FORMAT {
    if buf.yuv444 {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
    } else if buf.is_nv12() {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
    } else {
        match fmt {
            // `x:R:G:B` 2:10:10:10 LE — NVENC's `ARGB10` is the same word layout (B in the low
            // 10 bits, R in bits 20-29).
            pf_frame::PixelFormat::X2Rgb10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
            // `x:B:G:R` 2:10:10:10 LE — NVENC's `ABGR10` (R in the low 10 bits).
            pf_frame::PixelFormat::X2Bgr10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10,
            // Packed 4-byte BGRA-order (the `copy_device_to_device` fallback path); NVENC's `ARGB`
            // ingests this layout + does the internal CSC, matching the proven Windows RGB-input
            // path.
            _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        }
    }
}

/// Is `fmt` one of NVENC's packed 10-bit RGB inputs? Decides the session's effective bit depth and
/// HDR flag — the input format is the only honest source for both (a 10-bit-negotiated session
/// whose capture came back 8-bit must encode, and label, 8-bit).
fn is_ten_bit_input(fmt: nv::NV_ENC_BUFFER_FORMAT) -> bool {
    matches!(
        fmt,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10
            | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
    )
}

/// One encoder-owned input surface + its NVENC registration. The surface is copied into each
/// use (device→device) and the registration is created once at session init, unregistered at teardown.
struct RingSlot {
    surface: SlotSurface,
    reg: nv::NV_ENC_REGISTERED_PTR,
}

/// The ring slot's backing allocation: Vulkan external memory CUDA-imported (the normal case —
/// blendable by the SPIR-V cursor pass, see `vkslot.rs`) or a plain pitched CUDA allocation (the
/// fallback when Vulkan bring-up fails: sessions still encode, composite mode just has no
/// cursor). Both present the same `(ptr, pitch, height)` NVENC-registration vocabulary.
enum SlotSurface {
    Cuda(InputSurface),
    /// Backing objects live in the encoder's [`VkSlotBlend`] (freed by its `free_slots`); the
    /// ref itself is Copy and carries the registered geometry.
    Vk(VkSlotRef),
}

/// The [`SlotFormat`] for an NVENC buffer format (the ring-build + blend vocabulary).
fn slot_fmt_of(fmt: nv::NV_ENC_BUFFER_FORMAT) -> SlotFormat {
    match fmt {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => SlotFormat::Yuv444,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => SlotFormat::Nv12,
        // Still 4 bytes per pixel, so the slot GEOMETRY matches `Argb` — but the cursor blend
        // must unpack 10-bit channels instead of bytes, hence a separate mode per channel order.
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10 => SlotFormat::X2Rgb10,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10 => SlotFormat::X2Bgr10,
        _ => SlotFormat::Argb,
    }
}

impl SlotSurface {
    fn ptr(&self) -> pf_zerocopy::cuda::CUdeviceptr {
        match self {
            SlotSurface::Cuda(s) => s.ptr,
            SlotSurface::Vk(r) => r.ptr,
        }
    }
    fn pitch(&self) -> usize {
        match self {
            SlotSurface::Cuda(s) => s.pitch,
            SlotSurface::Vk(r) => r.pitch,
        }
    }
    fn height(&self) -> u32 {
        match self {
            SlotSurface::Cuda(s) => s.height,
            SlotSurface::Vk(r) => r.height,
        }
    }
}

/// `doNotWait` sampling cadence inside [`Encoder::poll_chunk`] — the probe measured ~200 µs
/// between slice completions on the 5070 Ti, so 50 µs keeps the added per-chunk delivery delay
/// well under one slice time without hammering the driver.
const CHUNK_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_micros(50);

/// Progress of a sub-frame chunked readback (§7 LN1 Phase 1) for the FRONT in-flight AU: how
/// much of the bitstream has already been handed out as chunks. `Some` from an AU's first
/// emitted chunk until its `last` — [`Encoder::poll`] refuses to run while it exists (a plain
/// poll would re-emit the already-shipped prefix).
struct ChunkState {
    /// Bytes emitted so far — also the next chunk's start offset (always a slice boundary).
    emitted: usize,
    /// Completed slices already covered by emitted chunks.
    slices_out: u32,
    /// The AU-opening chunk (`AuChunk::first`) has been handed out.
    opened: bool,
    /// Debug-build shadow of every emitted byte, cross-checked against the finishing blocking
    /// lock's full AU — a mis-cut chunk fails loudly in the on-hw tests instead of silently
    /// corrupting the wire. Compiled out of release builds.
    #[cfg(debug_assertions)]
    shadow: Vec<u8>,
}

impl ChunkState {
    fn new() -> Self {
        ChunkState {
            emitted: 0,
            slices_out: 0,
            opened: false,
            #[cfg(debug_assertions)]
            shadow: Vec::new(),
        }
    }
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
    /// Encoded bit depth. **10 is live on Linux** — this used to say "8 until Phase 5.1 lands a P010
    /// capture path", which the code has since outrun: the gamescope HDR capture patches offer
    /// 10-bit BT.2020/PQ formats, `nvenc_fmt` maps `X2Rgb10`/`X2Bgr10` to `ARGB10`/`ABGR10`, and
    /// [`is_ten_bit_input`] flips this field and `hdr` from the negotiated input. A 10-bit frame
    /// deliberately takes NEITHER the NV12 nor the YUV444 convert (both compute CSCs write 8-bit
    /// planes) and rides packed RGB to the encoder, which does its own BT.2020 CSC —
    /// `pf-capture/src/linux/pipewire.rs` owns that gate. So Main10 needed no P010 path to arrive.
    /// P010 remains worth having later purely as a perf win (pre-convert so NVENC skips its internal
    /// RGB→YUV CSC, as NV12 already does for SDR) — it is not what makes 10-bit work.
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
    /// Frames submitted over the encoder's lifetime (never reset, unlike `next`) — drives the
    /// sampled `PUNKTFUNK_PERF` submit-split log cadence, mirroring the VAAPI backend's counter.
    frames: u64,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// (bitstream, mapped input resource to unmap after retrieval, pts_ns, recovery-anchor,
    /// IDR-predicted) per in-flight encode. The fourth field tags the first frame encoded after a
    /// successful [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) — the clean re-anchor
    /// P-frame the client lifts its post-loss freeze on. The fifth is the submit-time keyframe
    /// prediction (forced/opening IDR) that chunked poll stamps on chunks emitted before the
    /// driver reports the real picture type — exact under P-only + infinite GOP (the driver only
    /// emits IDRs we asked for); the finishing blocking lock cross-checks it.
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64, bool, bool)>,
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
    /// Cursor-as-metadata GPU blend: the Vulkan device + SPIR-V compute pass the ring's
    /// external-memory slots are allocated through (`vkslot.rs`) — the driver-portable
    /// replacement for the retired PTX kernels. Brought up once at session init (`cursor_tried`
    /// stops re-attempts); `None` = bring-up failed, the ring fell back to plain CUDA
    /// allocations and composite mode degrades to no cursor. `cursor_serial` tracks the
    /// uploaded bitmap.
    vk_blend: Option<VkSlotBlend>,
    /// The session may hand this encoder cursor overlays (`SessionPlan.cursor_blend` — only
    /// cursor-channel sessions since Phase B). Off = skip the Vulkan bring-up entirely and ring
    /// on plain CUDA surfaces: embedded-pointer sessions never carry an overlay, so they pay
    /// zero blend cost, per-session or per-frame.
    blend_wanted: bool,
    cursor_tried: bool,
    cursor_serial: u64,
    /// Suppress-until-success latch for the per-frame blend warn: a persistent failure sits in
    /// the submit() hot path, so warn once per failure streak (reset on success) rather than on
    /// every cursor-bearing frame, which would evict the log ring.
    cursor_blend_warned: bool,
    /// One-shot latch for [`diagnose_failed_open`](Self::diagnose_failed_open) so a rebuild-retry
    /// burst (the session loop's bounded encoder resets) logs the diagnosis once, not per attempt.
    diagnosed: bool,
    /// The two-thread retrieve runtime (`PUNKTFUNK_NVENC_ASYNC`) — `None` in the default
    /// single-thread mode and between sessions. Exists only `init_session`→`teardown`.
    async_rt: Option<AsyncRetrieve>,
    /// The session loop escalated into pipelined retrieve ([`Encoder::set_pipelined`], the
    /// contention analog of the capturer depth escalation). Sticky across session rebuilds
    /// (escalate-and-hold, like the depth escalation); the switch itself happens at the next
    /// safe point via [`maybe_engage_async`](Self::maybe_engage_async).
    want_async: bool,
    /// A de-escalation request ([`Encoder::set_pipelined(false)`]) waiting for its safe point:
    /// the next drained moment tears the session down and lazily re-inits SYNC (IO-stream
    /// binding and sub-frame chunking re-arm at that re-init). Distinct from `!want_async` —
    /// an operator-forced async session (`PUNKTFUNK_NVENC_ASYNC=1`) also has `want_async`
    /// false, and a de-escalation must never tear THAT down.
    want_sync: bool,
    /// Boxed `CUstream` the session's IO-stream binding points at (`NvEncSetIOCudaStreams` takes
    /// POINTERS to `CUstream`, and this struct moves — the pointee needs a stable heap address for
    /// the session's lifetime). Null when stream-ordering is off; freed in `teardown` AFTER the
    /// session is destroyed.
    io_stream: *mut *mut c_void,
    /// Stream-ordered submit armed for the live session (sync-retrieve mode only; see
    /// [`stream_ordered_requested`]). The per-frame gate additionally requires `pending` empty.
    stream_ordered: bool,
    /// Slice count the live session was configured with ([`resolve_slices`] — env override,
    /// else the Linux direct-NVENC default of 4 since Phase 3 clamped to
    /// [`max_slices`](Self::max_slices); 1 = the preset's single slice).
    /// Chunked poll needs ≥ 2 to have boundaries to cut at. Latched at init, consumed by
    /// `build_config` (so an in-place reconfigure presents the same slicing).
    slices: u32,
    /// Ceiling on the per-frame slice count the session's CLIENT decoder accepts (from
    /// negotiation: `VIDEO_CAP_MULTI_SLICE`, or GameStream's `videoEncoderSlicesPerFrame`).
    /// 1 = single-slice only — the safe shape toward decoders that never asked (Amlogic TV
    /// SoCs wedge on multi-slice AUs). Clamps the Phase-3 default; the explicit
    /// `PUNKTFUNK_NVENC_SLICES` env override still wins in both directions.
    max_slices: u32,
    /// `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK` from the caps probe — gates the DEFAULT-on
    /// sub-frame arming (an unsupported GPU must not have `enableSubFrameWrite` forced into its
    /// init params, which could fail the session open). `PUNKTFUNK_NVENC_SUBFRAME=1` overrides.
    subframe_cap: bool,
    /// Sub-frame readback resolved for the live session ([`resolve_subframe`] over
    /// [`subframe_cap`](Self::subframe_cap)); consumed by every `build_init_params` call so the
    /// open and the in-place reconfigure present identical init params.
    subframe_on: bool,
    /// Whether `PUNKTFUNK_NVENC_SUBFRAME=1` was EXPLICITLY forced — latched with `subframe_on`
    /// (same invariant: open and reconfigure must present identical init params, so no env
    /// re-reads after `query_caps`). Only consumed by [`resolve_split_subframe`]'s log severity.
    subframe_forced: bool,
    /// Sub-frame chunked poll armed for the live session (§7 LN1 Phase 1): multi-slice +
    /// sub-frame readback configured AND sync retrieve at init. See [`Encoder::poll_chunk`].
    subframe_chunks: bool,
    /// `NV_ENC_CAPS_NUM_ENCODER_ENGINES` — how many NVENC engines this GPU has, probed in
    /// [`query_caps`]. `0` = not probed / unreadable. The split-encode ceiling: the driver accepts
    /// a split wider than the hardware and silently encodes narrower, so this is the only honest
    /// source for how wide we may go (see `codec::max_forced_split_mode`).
    encoder_engines: u32,
    /// Submit stamp for the split arbiter's per-frame cost (sync depth-1 path only).
    last_submit_at: Option<std::time::Instant>,
    /// Whole-AU paced-send time (µs) the host last reported, via
    /// [`Encoder::set_send_spread_us`]. `0` = never reported, which keeps the arbiter out of the
    /// sub-frame trade entirely (it cannot price what it cannot see).
    send_spread_us: u32,
    /// Sub-frame state the session was OPENED able to run — what `resolve_subframe` decided from
    /// the caps probe and the env. `subframe_on` moves as the arbiter flips arms; this does not,
    /// so a return to a non-forced split can restore sub-frame without re-deriving it (and
    /// without ever turning it on for a session that never had it).
    subframe_opened_with: bool,
    /// The live split-mode experiment, when one is running. `None` = not arbitrating (gated off,
    /// already decided this process, or the config is one we refuse to arbitrate).
    arbiter: Option<SplitArbiter>,
    /// In-progress chunked readback of the front in-flight AU. See [`ChunkState`].
    chunk: Option<ChunkState>,
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
    /// captured frame (lazy init in `submit`), and this backend only accepts CUDA frames (a
    /// CPU/dmabuf payload `bail`s). The effective `bit_depth`/`hdr` are derived from that same
    /// input format rather than trusted from the negotiation — a 10-bit session whose capture came
    /// back 8-bit must encode 8-bit AND say so, never mislabel.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        _format: pf_frame::PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        _cuda: bool,
        bit_depth: u8,
        chroma: ChromaFormat,
        cursor_blend: bool,
        max_slices: u32,
    ) -> Result<Self> {
        // The runtime `.so` load is the real "is NVENC possible here" gate: fail the open with a
        // clear reason instead of an opaque session error on the first frame.
        try_api().map_err(|e| anyhow!("NVENC (Linux direct) unavailable: {e}"))?;

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
            // Provisional until the first frame names the real input format (see `submit`'s init
            // block, which sets both from `buffer_fmt`).
            bit_depth,
            // 4:4:4 is HEVC-only; confirmed against the frame layout + GPU support at init.
            chroma_444: chroma.is_444() && codec == Codec::H265,
            yuv444_supported: false,
            hdr: false,
            hdr_meta: None,
            ring: Vec::new(),
            next: 0,
            frames: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            pending_anchor: false,
            vk_blend: None,
            blend_wanted: cursor_blend,
            cursor_tried: false,
            cursor_serial: u64::MAX,
            cursor_blend_warned: false,
            diagnosed: false,
            inited: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32,
            last_rfi_range: None,
            async_rt: None,
            want_async: false,
            want_sync: false,
            io_stream: ptr::null_mut(),
            stream_ordered: false,
            slices: 1,
            // A zero from a misbehaving caller must not zero the resolver's default arithmetic.
            max_slices: max_slices.max(1),
            subframe_cap: false,
            subframe_on: false,
            subframe_forced: false,
            subframe_chunks: false,
            encoder_engines: 0,
            last_submit_at: None,
            send_spread_us: 0,
            subframe_opened_with: false,
            arbiter: None,
            chunk: None,
        })
    }

    /// Engage the escalated pipelined retrieve at a safe point: nothing in flight, and — because
    /// a live session has its IO streams bound for stream-ordered submit, whose output-stream
    /// semantics would make every later stream op wait on the previous encode and so serialize a
    /// pipelined session — via a clean session rebuild WITHOUT the binding (the re-open's first
    /// frame is the standard session-opening IDR). No-op until [`want_async`](Self::want_async)
    /// is set and `pending` drains.
    fn maybe_engage_async(&mut self) {
        if !self.want_async || self.async_rt.is_some() || !self.pending.is_empty() {
            return;
        }
        if self.inited {
            // SAFETY: encode thread, `pending` empty ⇒ no encode in flight; `teardown` handles
            // exactly this live-session state (and a torn-down encoder lazily re-inits on the
            // next submit, which spawns the retrieve thread and skips the IO-stream arming).
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve escalation: rebuilding the session without the \
                 IO-stream binding (stream-ordered submit and two-thread retrieve are mutually \
                 exclusive); next frame opens with an IDR"
            );
        }
    }

    /// [`maybe_engage_async`](Self::maybe_engage_async)'s inverse — wind the escalated pipelined
    /// retrieve back at a safe point: nothing in flight, then a clean session rebuild whose lazy
    /// SYNC re-init restores the IO-stream binding and re-arms sub-frame chunking (the two
    /// latency features the escalation traded away). No-op until
    /// [`want_sync`](Self::want_sync) is set and `pending` drains.
    fn maybe_disengage_async(&mut self) {
        if !self.want_sync || self.async_rt.is_none() || !self.pending.is_empty() {
            return;
        }
        self.want_sync = false;
        if self.inited {
            // SAFETY: encode thread, `pending` empty ⇒ no encode in flight (and nothing queued
            // to the retrieve thread); `teardown` joins the retrieve thread and handles exactly
            // this live-session state — the next submit lazily re-inits sync.
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve de-escalation: rebuilding the session with the sync \
                 retrieve (IO-stream binding and sub-frame chunking restored); next frame opens \
                 with an IDR"
            );
        }
    }

    /// Tear down the encode session + pooled resources. Reused on a size change and at Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Stop the retrieve thread FIRST: close its job channel and join. Any in-flight blocking
        // lock returns once its encode completes (≤ a frame time on a live driver), so the join
        // is bounded; after it no other thread can touch the session the code below destroys.
        if let Some(mut rt) = self.async_rt.take() {
            rt.work_tx.take();
            if let Some(j) = rt.join.take() {
                let _ = j.join();
            }
        }
        // Unmap any in-flight inputs, unregister every ring surface, destroy the bitstreams.
        for (_, map, _, _, _) in &self.pending {
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
        // A destroy failure means the driver may still hold this session's slot (the concurrent-
        // session cap is per process and only a restart clears a leak) — make it visible instead
        // of silently discarding the status.
        if let Err(e) = (api().destroy_encoder)(self.encoder).nv_ok() {
            tracing::warn!(
                status = ?e,
                "NVENC destroy_encoder failed at teardown — the driver may have leaked this \
                 session's slot toward the concurrent-session cap"
            );
        }
        // The boxed CUstream the IO-stream binding pointed at — freed only now, AFTER the session
        // that referenced it is destroyed (created by `Box::into_raw` in `init_session`, freed
        // exactly once here; `io_stream` is nulled so a re-init can't double-free).
        if !self.io_stream.is_null() {
            drop(Box::from_raw(self.io_stream));
            self.io_stream = ptr::null_mut();
        }
        self.stream_ordered = false;
        // Chunked-poll state is per session: a half-chunked AU dies with its in-flight frame
        // (the forfeit contract), and the next session re-latches the arming at init.
        self.subframe_chunks = false;
        self.chunk = None;
        self.ring.clear(); // drops the CUDA InputSurfaces; Vk slots are freed just below
        if let Some(vk) = &mut self.vk_blend {
            // The Vulkan-backed slots' memory (and its CUDA mapping) — the device itself stays
            // up for the next session's ring (`cursor_tried` keeps bring-up one-shot).
            vk.free_slots();
        }
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
        if let Err(e) = (api().open_encode_session_ex)(&mut params, &mut enc).nv_ok() {
            // The NVENC docs require NvEncDestroyEncoder even after a FAILED open (the driver may
            // have allocated the session slot before erroring) — without it, every failed open in
            // a retry loop leaks a slot toward the concurrent-session cap, turning a transient
            // failure into permanent exhaustion that only a host restart clears.
            if !enc.is_null() {
                let _ = (api().destroy_encoder)(enc);
            }
            return Err(nvenc_status::call_err(
                "open_encode_session_ex (caps probe)",
                e,
            ));
        }
        // The handshake with the kernel module just succeeded — from here on, an
        // `NV_ENC_ERR_INVALID_VERSION` in this process cannot be a driver version skew.
        nvenc_status::note_session_opened();
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
        // Sub-frame-output prerequisites (latency plan §7 LN1): logged for fleet visibility now,
        // consumed when slice-level readback lands. Not stored — LN1 re-probes when it configures.
        let subframe = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK);
        let dyn_slice = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE);
        // How many NVENC engines this GPU has — the split-encode ceiling. Must be probed rather
        // than inferred from a rejection: the driver ACCEPTS a split wider than the hardware and
        // silently encodes narrower (measured on `.21`, see `max_forced_split_mode`).
        let engines = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES);
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
        self.subframe_cap = subframe != 0;
        self.encoder_engines = engines.max(0) as u32;
        // Phase-3 default-on (nvenc-subframe-slice-output.md): 4 slices + sub-frame readback on
        // every Linux direct-NVENC session, resolved HERE (before the session opens) so the
        // config author, the init params and the chunked-poll latch all agree; the caps probe
        // gates the sub-frame default so a GPU without SUBFRAME_READBACK never has it forced
        // into its init params. The Phase-3 default is CLAMPED to the session's negotiated
        // `max_slices` — the client-decoder ceiling (`VIDEO_CAP_MULTI_SLICE`, or GameStream's
        // `videoEncoderSlicesPerFrame`): a client that never asked for multi-slice AUs gets
        // single-slice frames, because TV-SoC decoders (Amlogic — Chromecast with Google TV)
        // wedge the whole device on frames carrying several slice NALs (the 0.17.0 field
        // regression). PUNKTFUNK_NVENC_SLICES / PUNKTFUNK_NVENC_SUBFRAME stay the explicit
        // operator overrides in both directions.
        self.slices = resolve_slices(self.codec, 4.min(self.max_slices));
        self.subframe_on = resolve_subframe(self.subframe_cap);
        self.subframe_forced = subframe_env_forced();
        tracing::info!(
            rfi = self.rfi_supported,
            custom_vbv = self.custom_vbv,
            yuv444 = self.yuv444_supported,
            subframe_readback = subframe != 0,
            dynamic_slice = dyn_slice != 0,
            slices = self.slices,
            max_slices = self.max_slices,
            max = %format!("{wmax}x{hmax}"),
            "NVENC (Linux direct) capabilities probed"
        );
        Ok(())
    }

    /// One-shot self-diagnosis for a failed session open (2026-07 field report: after a codec
    /// switch every open returned `NV_ENC_ERR_INVALID_VERSION` until the HOST PROCESS was
    /// restarted — so the poisoned state is per-process, not the driver install). Retries the raw
    /// open on a FRESH dedicated CUDA context to split the candidate causes apart in the log:
    ///   * fresh context WORKS  → the shared process context (or its NVENC association) is in a
    ///     bad state — a host bug to report;
    ///   * fresh context fails the SAME way → driver-level: userspace/kernel version skew,
    ///     concurrent-session-cap exhaustion (leaked sessions), or a lost/reset GPU;
    ///   * no fresh context AT ALL → CUDA itself is unhealthy in this process.
    ///
    /// Log-only (the caller still fails the open); latched per encoder so a reset burst logs once.
    fn diagnose_failed_open(&mut self) {
        if self.diagnosed {
            return;
        }
        self.diagnosed = true;
        let fresh = cuda::with_fresh_context(|ctx| {
            let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device: ctx,
                apiVersion: nv::NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc: *mut c_void = ptr::null_mut();
            // SAFETY: `params`/`enc` are live stack locals across the synchronous call; `ctx` is
            // the live diagnostic context `with_fresh_context` just created. Any session the probe
            // opened (even on a failed status, per the NVENC docs) is destroyed exactly once here.
            unsafe {
                let st = (api().open_encode_session_ex)(&mut params, &mut enc);
                if !enc.is_null() {
                    let _ = (api().destroy_encoder)(enc);
                }
                st
            }
        });
        match fresh {
            Ok(nv::NVENCSTATUS::NV_ENC_SUCCESS) => tracing::error!(
                "NVENC self-diagnosis: the session opens FINE on a fresh CUDA context — the \
                 host's shared CUDA context is in a bad state (host bug; please report this log)"
            ),
            Ok(st) => tracing::error!(
                fresh_ctx_status = ?st,
                "NVENC self-diagnosis: the open fails on a fresh CUDA context too — driver-level \
                 cause: {}",
                nvenc_status::explain(st)
            ),
            Err(e) => tracing::error!(
                error = %format!("{e:#}"),
                "NVENC self-diagnosis: could not create a fresh CUDA context — CUDA itself is \
                 unhealthy in this process (GPU reset/fell off the bus, or a poisoned driver \
                 state); a host restart should clear it"
            ),
        }
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
        .map_err(|e| nvenc_status::call_err("get_encode_preset_config_ex", e))?;
        let mut cfg = preset.presetCfg;

        // Steps 3-7 (RC/VBV, tier+level, chroma+bit-depth, colour VUI, RFI DPB) are the shared
        // low-latency contract. On Linux the full-chroma input is a YUV444 surface; AV1's
        // input-depth follows the surface format (10-bit for a packed PQ/BT.2020 HDR capture).
        let yuv444_input = matches!(
            self.buffer_fmt,
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
        );
        apply_low_latency_config(
            &mut cfg,
            LowLatencyConfig {
                codec: self.codec,
                bitrate,
                fps: self.fps,
                custom_vbv: self.custom_vbv,
                chroma_444: self.chroma_444,
                full_chroma_input: yuv444_input,
                bit_depth: self.bit_depth,
                av1_input_depth_minus8: if is_ten_bit_input(self.buffer_fmt) {
                    2
                } else {
                    0
                },
                hdr: self.hdr,
                rfi_supported: self.rfi_supported,
                slices: self.slices,
            },
        );
        Ok(cfg)
    }

    /// This session config's identity in the process-lifetime bitrate-ceiling cache
    /// (`nvenc_core::{cached_ceiling, store_ceiling}`). GPU identity is the process-global shared
    /// `CUcontext` pointer — one context per process, stable for its lifetime; only valid once
    /// `cu_ctx` is bound (`init_session` start), which every caller is downstream of.
    fn ceiling_key(&self, split_mode: u32) -> CeilingKey {
        CeilingKey {
            gpu: self.cu_ctx as u64,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
            split_mode,
        }
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
        if let Err(e) = (api().open_encode_session_ex)(&mut params, &mut enc).nv_ok() {
            // Destroy-on-failed-open, as in `query_caps`: a failed open may still hold a session
            // slot that must be released.
            if !enc.is_null() {
                let _ = (api().destroy_encoder)(enc);
            }
            return Err(nvenc_status::call_err("open_encode_session_ex", e));
        }
        nvenc_status::note_session_opened();

        let mut cfg = match self.build_config(enc, bitrate) {
            Ok(cfg) => cfg,
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                return Err(e);
            }
        };
        let mut init = build_init_params(
            self.codec_guid,
            self.width,
            self.height,
            self.fps,
            &mut cfg,
            split_mode,
            false,
            self.subframe_on,
        );

        match (api().initialize_encoder)(enc, &mut init).nv_ok() {
            Ok(()) => Ok(enc),
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                Err(nvenc_status::call_err("initialize_encoder", e))
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
        // live pitched CUDA allocation on the shared context. `set_io_cuda_streams` takes `enc` plus
        // two pointers to the boxed live `CUstream` (`Box::into_raw`), which outlives the session —
        // freed exactly once: in `teardown` after `destroy_encoder` when armed, or via
        // `Box::from_raw` right here on the rejection path (where `io_stream` is never set). No
        // handle escapes the encode thread.
        unsafe {
            // Bind to the shared CUDA context; make it current on this (encode) thread for both the
            // session open and every subsequent device→device input copy.
            self.cu_ctx = cuda::context().context("shared CUDA context (Linux direct NVENC)")?;
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;

            if let Err(e) = self.query_caps() {
                // The one place every session-open failure funnels through (the probe is the first
                // open of any session) — run the one-shot self-diagnosis before propagating.
                self.diagnose_failed_open();
                return Err(e);
            }
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // 2-way NVENC split-frame encoding (Ada dual-NVENC) — shared selector, see
            // [`resolve_split_mode`] for the precedence (env override / 10-bit / pixel rate).
            let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
            let mut split_mode: u32 =
                resolve_split_mode(self.codec, self.bit_depth, pixel_rate, self.encoder_engines);
            // A verdict this process already measured for this exact config wins over the static
            // rule — that is the whole point of arbitrating, and it lets later sessions skip the
            // ~1 s experiment. An operator pin still beats both (checked inside `resolve_split_mode`,
            // so only consult the cache when the knob is unset).
            if std::env::var_os("PUNKTFUNK_SPLIT_ENCODE").is_none() {
                if let Some(known) = cached_split_verdict(&self.split_key()) {
                    if known != split_mode {
                        tracing::info!(
                            from = split_mode,
                            to = known,
                            "NVENC: using the split mode a previous arbitration measured as \
                             fastest for this config"
                        );
                    }
                    split_mode = known;
                }
            }
            // Split × sub-frame arbitration (Phase 8) BEFORE the ladder, the ceiling key and the
            // chunked-poll latch — all three must see the post-arbitration truth (a drop inside
            // build_init_params would leave poll_chunk busy-polling its whole budget per AU).
            let (split_mode, subframe_on) = resolve_split_subframe(
                self.codec,
                split_mode,
                self.subframe_on,
                self.subframe_forced,
            );
            self.subframe_on = subframe_on;
            self.subframe_opened_with = subframe_on;
            const CLAMP_TOL_BPS: u64 = 20_000_000;

            // Ceiling cache (process lifetime, `nvenc_core`): a prior clamp search already found
            // this config's max accepted rate — open straight AT the ceiling instead of paying
            // the ~6-open binary search (and its session churn) on every ABR overshoot.
            let mut target_bps = requested_bps;
            if let Some(ceiling) = cached_ceiling(&self.ceiling_key(split_mode)) {
                if requested_bps > ceiling {
                    tracing::info!(
                        requested_mbps = requested_bps / 1_000_000,
                        ceiling_mbps = ceiling / 1_000_000,
                        "NVENC (Linux): requested bitrate above the cached codec-level ceiling — \
                         opening at the ceiling"
                    );
                    target_bps = ceiling;
                }
            }

            let mut probe = self.try_open_session(target_bps, split_mode);
            // The cache is advisory: a stale entry (driver change, identity collision) must not
            // wedge the open — retry the requested rate and let the search below rediscover.
            if probe.is_err() && target_bps < requested_bps {
                target_bps = requested_bps;
                probe = self.try_open_session(requested_bps, split_mode);
            }
            // Disambiguate a forced-split rejection from a bitrate-cap rejection. `used_split`
            // tracks the mode sessions ACTUALLY open with from here on — it feeds
            // `self.split_mode` (a reconfigure must re-present it) and the ceiling-cache key.
            let mut used_split = split_mode;
            let split_on =
                split_mode != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
            if probe.is_err() && split_on {
                let no_split = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                if let Ok(e) = self.try_open_session(target_bps, no_split) {
                    tracing::warn!(
                        "NVENC (Linux): split-encode rejected by codec/config — disabled"
                    );
                    used_split = no_split;
                    probe = Ok(e);
                }
            }

            let enc = match probe {
                Ok(enc) => {
                    self.bitrate_bps = target_bps;
                    enc
                }
                // Only a parameter/caps rejection means "the bitrate is above the codec-level
                // ceiling". A transient failure (busy engine, session limit, OOM, device loss,
                // version skew) must propagate — a search steered by it would discover, and
                // cache, a bogus ceiling.
                Err(e) if !nvenc_status::is_param_rejection(&e) => return Err(e),
                Err(_) => {
                    // Requested bitrate exceeds the codec-level ceiling — binary-search the max accepted.
                    let mut lo = FLOOR_BPS;
                    let mut hi = target_bps;
                    let mut best: *mut c_void = ptr::null_mut();
                    let mut best_bps = 0u64;
                    while hi > lo + CLAMP_TOL_BPS {
                        let mid = lo + (hi - lo) / 2;
                        match self.try_open_session(mid, used_split) {
                            Ok(e) => {
                                if !best.is_null() {
                                    let _ = (api().destroy_encoder)(best);
                                }
                                best = e;
                                best_bps = mid;
                                lo = mid;
                            }
                            Err(e) if nvenc_status::is_param_rejection(&e) => hi = mid,
                            Err(e) => {
                                // Environmental mid-search failure: don't let it shrink the
                                // search — release the partial result and propagate.
                                if !best.is_null() {
                                    let _ = (api().destroy_encoder)(best);
                                }
                                return Err(e);
                            }
                        }
                    }
                    if best.is_null() {
                        let no_split =
                            nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                        best = match self.try_open_session(FLOOR_BPS, used_split) {
                            Ok(e) => e,
                            Err(_) => {
                                let e = self.try_open_session(FLOOR_BPS, no_split).context(
                                    "NVENC initialize_encoder rejected even at the floor bitrate",
                                )?;
                                used_split = no_split;
                                e
                            }
                        };
                        best_bps = FLOOR_BPS;
                    }
                    tracing::warn!(
                        requested_mbps = requested_bps / 1_000_000,
                        clamped_mbps = best_bps / 1_000_000,
                        "NVENC (Linux): requested bitrate above the GPU codec-level ceiling — clamped"
                    );
                    store_ceiling(self.ceiling_key(used_split), best_bps);
                    self.bitrate_bps = best_bps;
                    best
                }
            };
            self.encoder = enc;
            self.split_mode = used_split;

            // Output bitstream pool.
            for _ in 0..POOL {
                let mut cb = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                    ..Default::default()
                };
                (api().create_bitstream_buffer)(enc, &mut cb)
                    .nv_ok()
                    .map_err(|e| nvenc_status::call_err("create_bitstream_buffer", e))?;
                self.bitstreams.push(cb.bitstreamBuffer);
            }

            // Encoder-owned input-surface ring: allocate + register POOL surfaces in the negotiated
            // format. Registered once here, mapped per submit, unregistered at teardown.
            // Preferred backing = Vulkan external memory CUDA-imported (`vkslot.rs`), so the
            // SPIR-V cursor blend can composite into the very bytes NVENC encodes; any bring-up
            // or per-slot failure falls back to plain pitched CUDA allocations (sessions always
            // encode — composite mode just loses the cursor, warned below).
            if !self.cursor_tried && self.blend_wanted {
                self.cursor_tried = true;
                match VkSlotBlend::new() {
                    Ok(v) => self.vk_blend = Some(v),
                    Err(e) => tracing::warn!(
                        error = %format!("{e:#}"),
                        "NVENC (Linux): Vulkan slot-blend bring-up failed — plain CUDA input \
                         surfaces, cursor compositing unavailable"
                    ),
                }
            }
            let slot_fmt = slot_fmt_of(self.buffer_fmt);
            // Two attempts: the full ring on Vulkan slots, else (any failure) the full ring on
            // plain CUDA — never a mixed ring (it would blend on some slots only: a flickering
            // cursor) and never a short one.
            'ring: for use_vk in [self.vk_blend.is_some(), false] {
                if !use_vk && self.vk_blend.is_some() {
                    // Second attempt: retire the Vulkan side wholesale first.
                    for s in self.ring.drain(..) {
                        let _ = (api().unregister_resource)(self.encoder, s.reg);
                    }
                    if let Some(vk) = &mut self.vk_blend {
                        vk.free_slots();
                    }
                    self.vk_blend = None;
                }
                for _ in 0..POOL {
                    let surface = if use_vk {
                        let vk = self.vk_blend.as_mut().expect("use_vk implies Some");
                        match vk.alloc_slot(slot_fmt, self.width, self.height) {
                            Ok(r) => SlotSurface::Vk(r),
                            Err(e) => {
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    "NVENC (Linux): Vulkan slot alloc failed — rebuilding the \
                                     ring on plain CUDA surfaces (cursor compositing \
                                     unavailable)"
                                );
                                continue 'ring;
                            }
                        }
                    } else {
                        SlotSurface::Cuda(
                            match self.buffer_fmt {
                                nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => {
                                    InputSurface::alloc_yuv444(self.width, self.height)
                                }
                                nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                                    InputSurface::alloc_nv12(self.width, self.height)
                                }
                                _ => InputSurface::alloc_rgb(self.width, self.height),
                            }
                            .context("alloc NVENC input surface")?,
                        )
                    };
                    let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                        version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                        resourceType:
                            nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                        width: self.width,
                        height: self.height,
                        pitch: surface.pitch() as u32,
                        resourceToRegister: surface.ptr() as *mut c_void,
                        bufferFormat: self.buffer_fmt,
                        bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                        ..Default::default()
                    };
                    match (api().register_resource)(self.encoder, &mut rr).nv_ok() {
                        Ok(()) => {}
                        Err(e) if use_vk => {
                            // NVENC refusing the imported pointer is a Vulkan-side condition
                            // too — same wholesale fallback.
                            tracing::warn!(
                                error = ?e,
                                "NVENC (Linux): registering a Vulkan-imported slot failed — \
                                 rebuilding the ring on plain CUDA surfaces"
                            );
                            continue 'ring;
                        }
                        Err(e) => {
                            return Err(nvenc_status::call_err(
                                "register_resource (CUDADEVICEPTR)",
                                e,
                            ));
                        }
                    }
                    self.ring.push(RingSlot {
                        surface,
                        reg: rr.registeredResource,
                    });
                }
                break 'ring; // full ring built
            }

            self.inited = true;
            // Two-thread retrieve (T2.2): spawn the lock thread against the live session. No
            // session parameter differs — teardown/rebuild always stops it before destroy.
            if async_retrieve_requested() || self.want_async {
                self.async_rt = Some(AsyncRetrieve::spawn(self.encoder as usize));
                tracing::info!(
                    depth = async_inflight_cap(),
                    escalated = self.want_async,
                    "NVENC two-thread retrieve enabled (submit thread + blocking-lock thread)"
                );
            }
            // Stream-ordered submit (latency plan §7 LN2): bind the session's IO streams to this
            // thread's copy stream so the input copy + cursor blend enqueue with no CPU sync and
            // `encode_picture` orders after them. Same stream both ways: input-stream semantics
            // start the encode only after our enqueued copies, output-stream semantics insert the
            // encode's completion INTO the stream — so later stream work (the next frame's copy
            // into a reused ring slot) also waits for it. Sync-retrieve mode only: in two-thread
            // mode the captured buffer may be recycled after `submit` returns while the stream
            // still holds its copy (the blocking copies are the lifetime guarantee there).
            if self.async_rt.is_none() && stream_ordered_requested() {
                let stream = cuda::copy_stream_handle();
                if !stream.is_null() {
                    // The pointee must outlive the session (the driver takes CUstream POINTERS) —
                    // box it; `teardown` frees it after `destroy_encoder`.
                    let holder = Box::into_raw(Box::new(stream));
                    match (api().set_io_cuda_streams)(
                        enc,
                        holder as nv::NV_ENC_CUSTREAM_PTR,
                        holder as nv::NV_ENC_CUSTREAM_PTR,
                    )
                    .nv_ok()
                    {
                        Ok(()) => {
                            self.io_stream = holder;
                            self.stream_ordered = true;
                            tracing::info!(
                                "NVENC stream-ordered submit armed (IO streams bound — no CPU \
                                 sync in the submit path)"
                            );
                        }
                        Err(e) => {
                            drop(Box::from_raw(holder));
                            tracing::debug!(
                                status = ?e,
                                "NvEncSetIOCudaStreams rejected — keeping blocking copies"
                            );
                        }
                    }
                }
            }
            // Sub-frame chunked poll (§7 LN1 Phase 1; default-on since Phase 3): armed iff this
            // session was CONFIGURED multi-slice + sub-frame readback (`self.slices` /
            // `self.subframe_on` were resolved once in `query_caps` and consumed by
            // `build_config` / `build_init_params`) and the retrieve is sync — chunked poll is a
            // depth-1 sync feature; a pipelined session's non-blocking poll owns the bitstream
            // from the retrieve thread instead (sub-frame write is not armed there at all —
            // `build_init_params` gates it on `!enable_async`).
            //
            // ⚠ THIS LATCH CAN DISAGREE WITH THE WRITER, and an earlier revision of this comment
            // claimed it could not ("so the latch can't disagree with the session config"). It
            // can: `build_init_params` arms `enableSubFrameWrite` from `subframe_on` ALONE,
            // while this line additionally demands `slices >= 2`. AV1 resolves to 1 slice by
            // construction, so it armed the writer with nothing to read the chunks and every
            // frame reached the wire truncated to its first tile. `resolve_split_subframe` now
            // disarms sub-frame for AV1 so the two agree; that function's docs carry the
            // measurement.
            self.subframe_chunks = self.slices >= 2 && self.subframe_on && self.async_rt.is_none();
            if self.subframe_chunks {
                tracing::info!(
                    slices = self.slices,
                    "NVENC sub-frame chunked poll armed (poll_chunk emits slice-boundary AU chunks)"
                );
            }
            tracing::info!(
                mode = %format_args!("{}x{}@{}", self.width, self.height, self.fps),
                bit_depth = self.bit_depth,
                mbps = self.bitrate_bps / 1_000_000,
                codec = ?self.codec_guid,
                fmt = ?self.buffer_fmt,
                // The FINAL split mode (post any rejection fallback) at INFO — journals run
                // INFO+, and "did 4K120 actually split across engines?" was undiagnosable from
                // a user log without it (Windows only had a debug! at selection time).
                split_mode = self.split_mode,
                // …and how many engines the GPU HAS, so `split_mode` can be read against the
                // ceiling it was chosen from. Without it a log showing split_mode=2 is ambiguous
                // between "used both engines" and "left a third engine idle", and the driver
                // silently honours an over-wide request, so the mode alone cannot be trusted.
                engines = self.encoder_engines,
                subframe = self.subframe_on,
                "NVENC CUDA session ready"
            );
            self.arm_split_arbiter();
            Ok(())
        }
    }

    /// Decide whether this session may run a live split experiment, and arm it if so.
    ///
    /// Opt-in (`PUNKTFUNK_NVENC_SPLIT_ARBITRATE=1`) while it earns trust. Every other gate is a
    /// correctness condition, not a preference:
    ///
    /// - **Operator pin wins.** `PUNKTFUNK_SPLIT_ENCODE` set ⇒ never arbitrate; a pinned mode is an
    ///   instruction, and an A/B that overrides it would make the knob useless for exactly the
    ///   debugging it exists for.
    /// - **Already decided.** A cached verdict for this config was applied at open; re-running the
    ///   experiment every session would pay its cost forever.
    /// - **Sync depth-1 only** (`async_rt.is_none()`), the same gate chunked poll uses: the
    ///   per-frame cost is measured as submit → AU, which is only the encode on this path. Under
    ///   pipelined retrieve that span includes queue depth and the comparison would be noise.
    /// - **Needs a second engine**, and split must be applicable at all (never H.264).
    /// - ⚠ **No sub-frame trade.** For HEVC, forcing split gives up sub-frame readback, which costs
    ///   send/encode overlap the ENCODER CANNOT SEE — it measures encode time only, so it would
    ///   reliably prefer split and silently make end-to-end latency worse. So we arbitrate only
    ///   where nothing is traded: sub-frame already off, or AV1 (where both features are legal).
    ///   Pricing that trade needs the host's send cost and is the next work package.
    fn arm_split_arbiter(&mut self) {
        if !matches!(
            std::env::var("PUNKTFUNK_NVENC_SPLIT_ARBITRATE").as_deref(),
            Ok("1")
        ) {
            return;
        }
        if std::env::var_os("PUNKTFUNK_SPLIT_ENCODE").is_some()
            || cached_split_verdict(&self.split_key()).is_some()
            || self.async_rt.is_some()
            || self.encoder_engines < 2
            || self.codec == Codec::H264
        {
            return;
        }
        // Losing sub-frame costs the send/encode overlap: without it the AU's last byte waits for
        // the WHOLE send instead of just the final slice, so the challenger owes roughly
        // `spread × (slices−1)/slices`. Priced here because only the encoder knows `slices`; the
        // host reports the raw spread.
        let handicap_us = if self.subframe_on && self.codec != Codec::Av1 {
            if self.send_spread_us == 0 || self.slices < 2 {
                tracing::debug!(
                    "NVENC split arbitration skipped: engaging split would cost sub-frame readback \
                     and no send-spread has been reported, so the trade cannot be priced — an \
                     encode-only comparison would take the arm that looks fastest and lose \
                     end-to-end"
                );
                return;
            }
            let slices = self.slices as u64;
            self.send_spread_us as u64 * (slices - 1) / slices
        } else {
            0
        };
        // Pick the challenger that tests the question worth asking: "are we leaving engines idle?"
        // So anything that is not already the widest forced split is challenged BY the widest, and
        // only a session already there is challenged by single-engine ("is splitting even helping
        // here?").
        //
        // ⚠ Not "whatever we are not": with the fallthrough `AUTO` incumbent that a 4K60 session
        // gets, the naive version challenged with DISABLE and spent the experiment re-proving that
        // splitting beats not-splitting — while parking the session on the slow arm to do it.
        let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let widest = max_forced_split_mode(self.encoder_engines);
        let challenger = if self.split_mode == widest {
            disable
        } else {
            widest
        };
        if challenger == self.split_mode {
            return;
        }
        tracing::info!(
            incumbent = self.split_mode,
            challenger,
            handicap_us,
            send_spread_us = self.send_spread_us,
            "NVENC split arbitration armed — measuring both arms on the live session (no IDR)"
        );
        self.arbiter = Some(SplitArbiter::with_handicap(
            self.split_mode,
            challenger,
            handicap_us,
        ));
    }

    /// The config identity this session's split verdict is cached under.
    fn split_key(&self) -> SplitKey {
        SplitKey {
            gpu: self.cu_ctx as u64,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
        }
    }

    /// Move the LIVE session to `mode` without an IDR — spike S1 proved `nvEncReconfigureEncoder`
    /// takes a changed `splitEncodeMode` with `resetEncoder=0`, emits no keyframe, and actually
    /// applies it. Reuses the bitrate reconfigure path at the CURRENT rate, so only the split mode
    /// moves. Returns whether the driver accepted it; on refusal the field is restored so the
    /// encoder's idea of its own session stays truthful.
    fn apply_split_mode(&mut self, mode: u32) -> bool {
        let (prev_mode, prev_sub, prev_chunks) =
            (self.split_mode, self.subframe_on, self.subframe_chunks);
        // Sub-frame rides along: HEVC cannot hold both, so a forced split must drop it and a
        // return to non-forced may take it back (only up to what the session was opened able to
        // do — `subframe_cap`/`resolve_subframe` decided that once, at open).
        let (mode, subframe) = resolve_split_subframe(
            self.codec,
            mode,
            self.subframe_opened_with,
            self.subframe_forced,
        );
        self.split_mode = mode;
        self.subframe_on = subframe;
        // ⚠ The latch `reconfigure_bitrate` does NOT recompute (spike S1c): leave it stale and
        // `supports_chunked_poll` keeps saying yes while `numSlices` never advances, so
        // `poll_chunk` busy-polls its entire budget every AU.
        self.subframe_chunks = self.slices >= 2 && subframe && self.async_rt.is_none();
        if self.reconfigure_bitrate(self.bitrate_bps) {
            true
        } else {
            tracing::warn!(
                from = prev_mode,
                to = mode,
                "NVENC split arbitration: driver refused the in-place split change — staying put"
            );
            self.split_mode = prev_mode;
            self.subframe_on = prev_sub;
            self.subframe_chunks = prev_chunks;
            false
        }
    }

    /// Feed one frame's encode cost to the split arbiter and act on its verdict.
    fn feed_split_arbiter(&mut self, encode_us: u64) {
        let Some(arb) = self.arbiter.as_mut() else {
            return;
        };
        let action = arb.on_frame(encode_us);
        let done = arb.is_done();
        match action {
            Some(ArbAction::SwitchTo(mode)) => {
                if !self.apply_split_mode(mode) {
                    // The experiment cannot proceed if the session will not move — abandon it
                    // rather than compare two measurements of the same arm.
                    self.arbiter = None;
                    return;
                }
            }
            Some(ArbAction::Settled(mode)) => {
                store_split_verdict(self.split_key(), mode);
            }
            None => {}
        }
        if done {
            // A "switch back to the incumbent" verdict settles on the mode now live.
            store_split_verdict(self.split_key(), self.split_mode);
            self.arbiter = None;
        }
    }

    /// Copy the captured `DeviceBuffer` into the ring slot's registered input surface (device→device
    /// on the shared context). `sync` blocks until the copy completes (the pre-existing behavior);
    /// `!sync` enqueues on the encode thread's copy stream and leaves ordering to the session's
    /// IO-stream binding (stream-ordered submit — see the gate in [`Encoder::submit`]).
    fn copy_into_slot(&self, buf: &cuda::DeviceBuffer, slot: usize, sync: bool) -> Result<()> {
        let s = &self.ring[slot].surface;
        let base = s.ptr();
        let pitch = s.pitch();
        let hh = s.height() as u64;
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
                cuda::copy_yuv444_to_device(buf, planes, sync)
            }
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                if !buf.is_nv12() {
                    bail!("NV12 session but the captured buffer has no chroma plane");
                }
                // Contiguous NV12: UV follows Y at base + pitch*height, same pitch.
                cuda::copy_nv12_to_device(buf, base, pitch, base + pitch as u64 * hh, pitch, sync)
            }
            _ => cuda::copy_device_to_device(buf, base, pitch, sync),
        }
    }

    /// Fold one retrieve-thread completion into `ready` (two-thread mode only): pop the oldest
    /// in-flight entry, cross-check FIFO pairing, unmap its input HERE (the encode thread — the
    /// retrieve thread never touches input resources), and queue the finished AU.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some((bs, map, pts_ns, anchor, _)) = self.pending.pop_front() else {
            bail!("NVENC retrieve: completion with no in-flight frame (pairing bug)");
        };
        if bs as usize != done.bs {
            bail!("NVENC retrieve: completion out of order (pairing bug)");
        }
        // SAFETY: `map` is the mapped input `submit` recorded for exactly this now-completed
        // encode; the session is live (`async_rt` exists only between `init_session` and
        // `teardown`) and this runs on the encode thread — the single unmap here mirrors the
        // sync path's poll-side unmap, exactly once per mapping.
        unsafe {
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, map);
            }
        }
        let (data, keyframe) = done.result.map_err(|e| anyhow!("{e}"))?;
        self.async_rt
            .as_mut()
            .expect("absorb_done is only reachable in two-thread mode")
            .ready
            .push_back(EncodedFrame {
                data,
                pts_ns,
                keyframe,
                recovery_anchor: anchor,
                chunk_aligned: false,
            });
        Ok(())
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
        // A pending pipelined-retrieve escalation — or de-escalation — engages here, at the
        // submit-side safe point (nothing in flight after the previous poll drained).
        self.maybe_engage_async();
        self.maybe_disengage_async();
        // Re-init on a size change (the capturer can return at a different resolution after a mode
        // switch). Format changes (NV12↔YUV444) likewise re-init.
        let new_fmt = buffer_format(buf, captured.format);
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
            // Depth + HDR follow the INPUT, like the Windows backend: a packed 10-bit PQ/BT.2020
            // capture (an HDR gamescope output) selects Main10 / AV1 10-bit and the BT.2020 PQ
            // colour signalling; anything else is 8-bit SDR. Deriving it here rather than
            // trusting the negotiated depth is what keeps the label and the bitstream in step
            // when capture and negotiation disagree.
            let ten_bit_in = is_ten_bit_input(new_fmt);
            if self.bit_depth >= 10 && !ten_bit_in {
                tracing::warn!(
                    format = ?captured.format,
                    "Linux direct-NVENC: 10-bit negotiated but the capture delivered an 8-bit \
                     format — encoding 8-bit SDR (the stream is labelled to match)"
                );
            }
            self.bit_depth = if ten_bit_in { 10 } else { 8 };
            self.hdr = ten_bit_in;
            // 4:4:4 honesty: engage FREXT only on a genuine YUV444 input; a subsampled NV12/RGB input
            // can't reconstruct full chroma, so clear the flag so `caps().chroma_444` is truthful.
            self.chroma_444 = self.chroma_444 && buf.yuv444;
            // `init_session` publishes `self.encoder` before its remaining fallible steps (bitstream
            // buffers, input-surface alloc, `register_resource`), so a failure there leaves a live
            // session with `inited == false`. Every guard on the re-init path keys off `inited`, so
            // without this the next submit would skip teardown and overwrite `self.encoder`, leaking
            // the session and its registered input surfaces permanently. `teardown` keys off
            // `encoder.is_null()`, not `inited`, so it cleans up exactly this half-built state.
            if let Err(e) = self.init_session() {
                // SAFETY: the encode thread owns the session and a failed init leaves nothing
                // mid-encode to race with.
                unsafe { self.teardown() };
                return Err(e);
            }
        } else {
            // Steady state: the copy helpers need the shared context current on this thread.
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;
        }

        // The session's opening frame is an IDR regardless of pic flags. Detected via the still-empty
        // output slot counter (`teardown` zeroes it), NOT `pts`: `submit_indexed` pins pts to the
        // wire frame index, non-zero on a mid-session rebuild's first frame.
        let opening = self.next == 0;
        // Two-thread backpressure: never more than the cap in flight — block on the OLDEST
        // completion first, absorbing its AU into `ready` for `poll`. Bounds the added latency
        // exactly like the sync path's blocking poll, just `cap` deep instead of 1, and keeps
        // this slot's bitstream/input surface free before they're reused below.
        while self.async_rt.is_some() && self.pending.len() >= async_inflight_cap() {
            let done = {
                let rt = self.async_rt.as_mut().expect("checked in loop condition");
                rt.done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(|_| anyhow!("NVENC retrieve stalled (5s) — encoder wedged?"))?
            };
            self.absorb_done(done)?;
        }
        let slot = self.next % POOL;
        self.next += 1;

        // Sampled breakdown of the submit hot path under PUNKTFUNK_PERF (~1 line per 2 s at
        // 60 fps, the VAAPI submit-split convention): copy = the per-frame device→device input
        // copy (the zero-copy-registration target), blend = cursor overlay kernel (0 without a
        // cursor), map/pic = the NVENC map_input_resource / encode_picture launches. The host
        // loop's `submit_us` folds all four together; this is what splits them apart.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;

        // Stream-ordered fast path (§7 LN2): enqueue the copy + blend with no CPU sync and let the
        // IO-stream binding order `encode_picture` after them — but ONLY while nothing is in
        // flight (true depth-1 usage). The gate is what makes this sound: with `pending` empty,
        // every prior encode was drained by a blocking `poll`, so (a) the ring slot being reused
        // was fully read, and (b) the caller still holds this frame's payload across the matching
        // `poll` (both host loops do — see `Encoder::submit`'s doc), which blocks until the encode
        // (and therefore the enqueued copy) completed. A pipelined caller (pending non-empty)
        // falls back to the blocking copy so an early-recycled source can never be read late.
        // `async_rt` must be absent too: in two-thread mode the frame may be recycled right after
        // submit returns while the stream still holds its copy (belt-and-braces — an escalated
        // session was rebuilt without the binding, so `stream_ordered` is false there anyway).
        let base_ordered =
            self.stream_ordered && self.async_rt.is_none() && self.pending.is_empty();
        // Cursor-bearing frames stay on the fast path when the blend itself can be stream-
        // ordered: the Vulkan dispatch waits/advances a timeline semaphore CUDA also holds, so
        // copy→blend→encode orders entirely on-device (`VkSlotBlend::blend_ref_ordered`). Where
        // that isn't available (no timeline export, or the ring fell back to plain CUDA slots)
        // a cursor forces the CPU-synced path: the blend's cross-API ordering is then fence/CPU-
        // established, sitting between the copy and the encode. That slow path is why cursor
        // frames USED to be gated out entirely — under gamescope the compositor re-attaches the
        // live pointer to EVERY frame, and the per-frame CPU syncs (exposed to the running
        // game's GPU load) capped a 120 fps session near 80 (submit p50 ~10 ms).
        let cursor_ordered = base_ordered
            && captured.cursor.is_some()
            && matches!(self.ring[slot].surface, SlotSurface::Vk(_))
            && self.vk_blend.as_ref().is_some_and(|vk| vk.ordered_ready());
        let ordered = base_ordered && (captured.cursor.is_none() || cursor_ordered);
        let t0 = std::time::Instant::now();

        // Copy the captured buffer into this slot's input surface before encoding it.
        self.copy_into_slot(buf, slot, !ordered)?;
        let t_copy = t0.elapsed();

        // Cursor-as-metadata: blend the overlay into this slot's OWNED input surface via the
        // SPIR-V compute pass (a dispatch over the cursor's rect — never the compositor's
        // dmabuf). On the `cursor_ordered` path the enqueued copy, the dispatch, and the encode
        // are ordered on-device through the timeline semaphore (no CPU sync — see the gate
        // above). Otherwise `ordered` is false: the CUDA copy completed before the Vulkan
        // dispatch and the fence-waited dispatch completes before the encode below — the
        // cross-API ordering is CPU-established. Any failure degrades to no cursor, never a
        // dropped frame (a failed ordered blend leaves the copy→encode stream ordering intact).
        if let Some(ov) = &captured.cursor {
            if let (Some(vk), SlotSurface::Vk(vref)) =
                (self.vk_blend.as_mut(), &self.ring[slot].surface)
            {
                if self.cursor_serial != ov.serial {
                    // Quiesces any in-flight ordered blend internally before touching the
                    // staging buffer (bitmap changes are rare — shape flips).
                    vk.upload_cursor(ov.rgba.as_slice(), ov.w, ov.h);
                    self.cursor_serial = ov.serial;
                }
                // surfW = content width; the blend derives plane strides from the slot's luma
                // height. Cursor pixels past the content land in cropped padding rows — harmless.
                let r = if cursor_ordered {
                    vk.blend_ref_ordered(
                        vref,
                        slot_fmt_of(self.buffer_fmt),
                        self.width,
                        ov.w,
                        ov.h,
                        ov.x,
                        ov.y,
                    )
                } else {
                    vk.blend_ref(
                        vref,
                        slot_fmt_of(self.buffer_fmt),
                        self.width,
                        ov.w,
                        ov.h,
                        ov.x,
                        ov.y,
                    )
                };
                if let Err(e) = r {
                    if !self.cursor_blend_warned {
                        self.cursor_blend_warned = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "NVENC (Linux): cursor blend dispatch failed — cursor not composited"
                        );
                    }
                } else {
                    self.cursor_blend_warned = false;
                }
            } else if !self.cursor_blend_warned {
                self.cursor_blend_warned = true;
                tracing::warn!(
                    blend_wanted = self.blend_wanted,
                    "NVENC (Linux): cursor overlay present but no Vulkan blend (bring-up failed, \
                     or a non-blend session unexpectedly carried an overlay) — cursor not \
                     composited"
                );
            }
        }

        let t_blend = t0.elapsed() - t_copy;
        let t_map: std::time::Duration;
        let t_pic: std::time::Duration;
        // SAFETY: every NVENC call goes through a function pointer from the runtime table and takes
        // `self.encoder`, the live session `init_session` established (non-null here). `mp`
        // (`NV_ENC_MAP_INPUT_RESOURCE`, version set) maps the ring slot's registration (created in
        // `init_session`) and is recorded in `pending` to be unmapped exactly once in `poll`/teardown.
        // `pic` (`NV_ENC_PIC_PARAMS`, version set) points `inputBuffer` at `mp.mappedResource` and
        // `outputBitstream` at the live pool bitstream `bitstreams[slot]`; the optional SEI scratch is
        // stack-local and outlives the synchronous `encode_picture`. The input surface for `slot` was
        // just filled by the device→device copy — either synchronized (blocking mode) or ordered
        // before this encode by the session's IO-stream binding (`ordered` — same stream, see the
        // gate above; on the `cursor_ordered` path the blend's writes are likewise ordered before
        // the encode, via the timeline-semaphore wait `blend_ref_ordered` enqueued on that same
        // stream) — and is not overwritten until this slot is reused POOL submits later, by
        // which time this encode was polled (POOL ≥ in-flight depth; in ordered mode the poll's
        // blocking lock additionally proves the enqueued copy completed).
        unsafe {
            let reg = self.ring[slot].reg;
            let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
                version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: reg,
                ..Default::default()
            };
            let tm = std::time::Instant::now();
            (api().map_input_resource)(self.encoder, &mut mp)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("map_input_resource", e))?;
            t_map = tm.elapsed();

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
                inputPitch: self.ring[slot].surface.pitch() as u32,
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
                .map(|m| pf_frame::hdr::hevc_mastering_display_sei(&m));
            let cll_sei = self
                .hdr_meta
                .map(|m| pf_frame::hdr::hevc_content_light_level_sei(&m));
            let mut sei: Vec<nv::NV_ENC_SEI_PAYLOAD> = Vec::new();
            if is_idr && self.hdr {
                if let Some(p) = mastering_sei.as_ref() {
                    sei.push(nv::NV_ENC_SEI_PAYLOAD {
                        payloadSize: p.len() as u32,
                        payloadType: pf_frame::hdr::SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME,
                        payload: p.as_ptr() as *mut u8,
                    });
                }
                if let Some(p) = cll_sei.as_ref() {
                    sei.push(nv::NV_ENC_SEI_PAYLOAD {
                        payloadSize: p.len() as u32,
                        payloadType: pf_frame::hdr::SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO,
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
            let tp = std::time::Instant::now();
            (api().encode_picture)(self.encoder, &mut pic)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("encode_picture", e))?;
            t_pic = tp.elapsed();
            self.pending.push_back((
                self.bitstreams[slot],
                mp.mappedResource,
                captured.pts_ns,
                anchor,
                // The chunked-poll keyframe prediction: exactly the SEI gate's is_idr (forced
                // flags or the session-opening frame) — under P-only + infinite GOP the driver
                // never emits an IDR on its own, so this matches the eventual pictureType.
                is_idr,
            ));
            // Stamp for the split arbiter's per-frame cost. Deliberately a single field rather
            // than a sixth `pending` element: the arbiter only runs on the sync depth-1 path
            // (`async_rt.is_none()`), where at most one encode is outstanding.
            self.last_submit_at = Some(std::time::Instant::now());
        }
        if sample {
            tracing::info!(
                copy_us = t_copy.as_micros() as u64,
                blend_us = t_blend.as_micros() as u64,
                map_us = t_map.as_micros() as u64,
                pic_us = t_pic.as_micros() as u64,
                "NVENC submit split (sampled): copy=input D2D copy blend=cursor map=map_input \
                 pic=encode_picture launch"
            );
        }
        // Two-thread mode: hand the blocking lock for this bitstream to the retrieve thread.
        // The sync_channel(POOL) can never fill (in-flight is capped < POOL above).
        if let Some(rt) = &self.async_rt {
            if let Some(tx) = &rt.work_tx {
                let _ = tx.send(RetrieveJob {
                    bs: self.bitstreams[slot] as usize,
                });
            }
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

    fn set_pipelined(&mut self, on: bool) -> bool {
        if !on {
            // De-escalation (the v2 of escalate-and-hold): latch the wind-back intent; the
            // switch itself happens at the next drained safe point
            // ([`maybe_disengage_async`](Self::maybe_disengage_async)) — the caller polls
            // this same method until it reports inactive.
            if async_retrieve_env() == Some(true) {
                // Operator pinned async on — de-escalation must not undo an explicit choice.
                return self.want_async || self.async_rt.is_some();
            }
            if self.want_async || self.async_rt.is_some() {
                self.want_async = false;
                self.want_sync = true;
                self.maybe_disengage_async();
            }
            return self.want_async || self.async_rt.is_some();
        }
        if async_retrieve_env() == Some(false) {
            return false; // operator veto: PUNKTFUNK_NVENC_ASYNC=0 means NEVER
        }
        self.want_sync = false; // latest intent wins — cancel a pending wind-back
        if !self.want_async && self.async_rt.is_none() {
            self.want_async = true;
            self.maybe_engage_async();
        }
        true
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            // Composites `frame.cursor` via the SPIR-V blend over the Vulkan-allocated input slot.
            blends_cursor: true,
            supports_rfi: self.rfi_supported,
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
        // Range validity, covering-range dedup, DPB window and clamp all live in
        // `nvenc_core::plan_range_recovery` — one policy for both direct-NVENC backends; only the
        // session gate and the driver loop are this backend's.
        if self.encoder.is_null() || !self.rfi_supported {
            return false;
        }
        match plan_range_recovery(first, last, self.frame_idx, self.last_rfi_range) {
            // Already invalidated a covering range for this loss event — re-arm the anchor (the
            // previous anchor AU may itself have been lost) but skip the driver calls.
            RangePlan::Covered => {
                self.pending_anchor = true;
                true
            }
            RangePlan::Decline => false,
            RangePlan::Invalidate { first, last } => {
                // Each input's `inputTimeStamp` is the WIRE frame index (pinned by
                // `submit_indexed`), so the client's lost-frame range maps 1:1 onto the timestamps
                // NVENC invalidates here.
                // SAFETY: `invalidate_ref_frames` is a function pointer from the runtime table;
                // `self.encoder` was checked non-null and is the live session; this runs on the
                // encode thread (no concurrent NVENC use). The plan clamped each `ts` to
                // `[oldest_in_dpb, frame_idx - 1]`, naming a frame still in the DPB; the call
                // passes only that `u64` (no struct).
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
                // The next submitted frame is the clean re-anchor — arm the tag so its AU ships
                // with `recovery_anchor` and the client lifts its post-loss freeze on it.
                self.pending_anchor = true;
                true
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // A partially-chunked AU must be finished through `poll_chunk`: its emitted prefix is
        // already with the caller, so a whole-AU poll here would double-emit those bytes.
        if self.chunk.is_some() {
            bail!("NVENC poll() called mid-chunked-AU — drain it via poll_chunk (caller bug)");
        }
        // Two-thread mode: drain whatever the retrieve thread has finished (non-blocking) and
        // hand out the oldest ready AU. `None` = nothing completed yet — the session loop keeps
        // the frame in flight and re-polls next tick; capture never blocks on the encode wait.
        if self.async_rt.is_some() {
            while let Ok(done) = self
                .async_rt
                .as_mut()
                .expect("checked just above")
                .done_rx
                .try_recv()
            {
                self.absorb_done(done)?;
            }
            return Ok(self
                .async_rt
                .as_mut()
                .expect("checked just above")
                .ready
                .pop_front());
        }
        let Some((bs, map, pts_ns, anchor, _)) = self.pending.pop_front() else {
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
                .map_err(|e| nvenc_status::call_err("lock_bitstream", e))?;
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
                .map_err(|e| nvenc_status::call_err("unlock_bitstream", e))?;
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, map);
            }
            // One frame's encode cost, submit → AU complete. Only meaningful on this sync,
            // depth-1 path (the arbiter is gated to it), where `lock_bitstream` above blocked
            // until the ASIC finished, so the span is the encode rather than a queue wait.
            let encode_us = self
                .last_submit_at
                .take()
                .map(|t| t.elapsed().as_micros() as u64);
            if let Some(us) = encode_us {
                self.feed_split_arbiter(us);
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

    fn supports_chunked_poll(&self) -> bool {
        // Dynamic on purpose: a pipelined-retrieve escalation rebuilds the session with
        // `async_rt` present (and `teardown` drops the latch), so a caller re-querying per AU
        // sees the mode fall away instead of chunk-polling a session that can't serve it.
        self.subframe_chunks && self.async_rt.is_none()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // Not a chunked session (knobs off, AV1, escalated to pipelined retrieve): degrade to a
        // single whole-AU chunk so a chunk-driven caller works against every session shape. The
        // `chunk.is_none()` arm is defensive — a mid-AU state must always finish below.
        if !self.supports_chunked_poll() && self.chunk.is_none() {
            return Ok(self.poll()?.map(AuChunk::whole));
        }
        let Some(&(bs, _, pts_ns, anchor, idr_hint)) = self.pending.front() else {
            return Ok(None);
        };
        // Sampling budget: if this driver branch never publishes intermediate slices, stop
        // burning CPU after ~2 frame intervals and finish through the blocking lock — worst
        // case poll_chunk behaves like sync `poll` plus a few failed doNotWait attempts.
        let budget = std::time::Duration::from_micros(2_000_000 / self.fps.max(1) as u64);
        let t0 = std::time::Instant::now();
        let mut offsets = [0u32; 32];
        loop {
            let emitted = self.chunk.as_ref().map_or(0, |c| c.emitted);
            let slices_out = self.chunk.as_ref().map_or(0, |c| c.slices_out);
            // SAFETY: `bs` is the front `pending` entry's pool bitstream (a prior
            // `encode_picture` targeted it, and `teardown` clears `pending` whenever it nulls
            // the session), the session is live, and this runs on the encode thread. `lock`
            // (version set, doNotWait) and `offsets` are live stack locals across the
            // synchronous call; `reportSliceOffsets` was armed at init so the driver may write
            // up to `numSlices` ≤ 32 offsets (`sliceModeData` is clamped 2..=32 by
            // `resolve_slices`). On a successful sub-frame lock `bitstreamBufferPtr` holds
            // `bitstreamSizeInBytes` readable bytes of COMPLETED slices (enableSubFrameWrite
            // publishes them mid-encode; proven by the on-hw probe) valid until the matching
            // unlock; the emitted range is copied out BEFORE the unlock. Every successful lock
            // is unlocked exactly once on all paths through the body.
            unsafe {
                let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                    version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                    outputBitstream: bs,
                    sliceOffsets: offsets.as_mut_ptr(),
                    ..Default::default()
                };
                lock.set_doNotWait(1);
                if (api().lock_bitstream)(self.encoder, &mut lock)
                    .nv_ok()
                    .is_ok()
                {
                    let n = lock.numSlices;
                    let bytes = lock.bitstreamSizeInBytes as usize;
                    if n >= self.slices {
                        // Every slice is readable — fall through to the finishing blocking
                        // lock (the completion authority; `numSlices` alone is not trusted
                        // across driver branches).
                        let _ = (api().unlock_bitstream)(self.encoder, bs);
                        break;
                    }
                    if n > slices_out && bytes > emitted {
                        // New completed slice(s): cut `[emitted..bytes)`. `bytes` with `n`
                        // reported slices is the end of slice n (slices are contiguous
                        // Annex-B), so the cut lands on a NAL boundary.
                        let data =
                            std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, bytes)
                                [emitted..]
                                .to_vec();
                        (api().unlock_bitstream)(self.encoder, bs)
                            .nv_ok()
                            .map_err(|e| nvenc_status::call_err("unlock_bitstream (chunk)", e))?;
                        let cs = self.chunk.get_or_insert_with(ChunkState::new);
                        #[cfg(debug_assertions)]
                        cs.shadow.extend_from_slice(&data);
                        let first = !cs.opened;
                        cs.opened = true;
                        cs.emitted = bytes;
                        cs.slices_out = n;
                        return Ok(Some(AuChunk {
                            data,
                            pts_ns,
                            keyframe: idr_hint,
                            recovery_anchor: anchor,
                            chunk_aligned: false,
                            first,
                            last: false,
                        }));
                    }
                    let _ = (api().unlock_bitstream)(self.encoder, bs);
                }
                // Non-SUCCESS (LOCK_BUSY on other branches) = not ready — never an error here;
                // the finishing blocking lock below owns real failures.
            }
            if t0.elapsed() > budget {
                break;
            }
            std::thread::sleep(CHUNK_SAMPLE_INTERVAL);
        }

        // Finish: ONE blocking lock — the completion authority and the wedge-watchdog hook,
        // exactly like sync `poll` (so the final chunk blocks and the AU tail never rides a
        // +1 tick — the depth-1 pump contract). Emits whatever the sampler hadn't handed out.
        let (bs, map, pts_ns, anchor, idr_hint) =
            self.pending.pop_front().expect("front() checked above");
        // SAFETY: same contract as `poll`'s blocking lock: `bs` is the popped in-flight pool
        // bitstream on the live session (encode thread); the blocking `lock_bitstream` (version
        // set) returns when the encode finished, yielding `bitstreamSizeInBytes` CPU-readable
        // bytes at `bitstreamBufferPtr` valid until `unlock_bitstream` — every read (tail copy
        // + debug prefix check) happens BEFORE the unlock. `map` (paired with `bs` in `pending`)
        // is unmapped here, after completion, exactly once.
        unsafe {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: bs,
                ..Default::default()
            };
            (api().lock_bitstream)(self.encoder, &mut lock)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("lock_bitstream (chunk finish)", e))?;
            let total = lock.bitstreamSizeInBytes as usize;
            let full = std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, total);
            let cs = self.chunk.take().unwrap_or_else(ChunkState::new);
            if cs.emitted > total {
                let _ = (api().unlock_bitstream)(self.encoder, bs);
                bail!(
                    "NVENC chunked poll: {} bytes already emitted but the finished AU is only \
                     {} — sub-frame readback reported bytes the final lock disowns",
                    cs.emitted,
                    total
                );
            }
            #[cfg(debug_assertions)]
            if cs.shadow.as_slice() != &full[..cs.emitted] {
                let _ = (api().unlock_bitstream)(self.encoder, bs);
                bail!("NVENC chunked poll: emitted chunks diverge from the finished AU prefix");
            }
            let data = full[cs.emitted..].to_vec();
            let keyframe = matches!(
                lock.pictureType,
                nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | nv::NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            (api().unlock_bitstream)(self.encoder, bs)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("unlock_bitstream (chunk finish)", e))?;
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, map);
            }
            if cs.opened && keyframe != idr_hint {
                // Can't happen under P-only + infinite GOP; if a driver branch ever proves
                // otherwise, the earlier chunks carried the wrong flag — make it visible.
                tracing::warn!(
                    predicted = idr_hint,
                    actual = keyframe,
                    "NVENC chunked poll: picture type diverged from the submit-time prediction"
                );
            }
            // The AU is complete here too — the chunked path is how a sub-frame session finishes,
            // so the arbiter has to be fed from BOTH completion points or it would never see a
            // frame on the incumbent arm of an HEVC sub-frame experiment (that arm is chunked;
            // only the challenger, with sub-frame dropped, comes through `poll`).
            let encode_us = self
                .last_submit_at
                .take()
                .map(|t| t.elapsed().as_micros() as u64);
            if let Some(us) = encode_us {
                self.feed_split_arbiter(us);
            }
            Ok(Some(AuChunk {
                data,
                pts_ns,
                keyframe,
                recovery_anchor: anchor,
                chunk_aligned: false,
                first: !cs.opened,
                last: true,
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
        // Cached codec-level ceiling: clamp the target BEFORE the driver call, so a known
        // overshoot retargets to the ceiling IN PLACE instead of bouncing off the driver into
        // the caller's full-rebuild fallback (an IDR plus ~half a second of session churn per
        // ABR overshoot on the pre-cache path). The caller reads the clamp back through
        // [`Encoder::applied_bitrate_bps`].
        let bps = match cached_ceiling(&self.ceiling_key(self.split_mode)) {
            Some(ceiling) => bps.min(ceiling),
            None => bps,
        };
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
                reInitEncodeParams: build_init_params(
                    self.codec_guid,
                    self.width,
                    self.height,
                    self.fps,
                    &mut cfg,
                    self.split_mode,
                    false,
                    self.subframe_on,
                ),
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

    fn set_send_spread_us(&mut self, us: u32) {
        self.send_spread_us = us;
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // `bitrate_bps` is the post-clamp truth: the open path's ceiling search and the
        // reconfigure path's cache clamp both write what the session ACTUALLY targets.
        Some(self.bitrate_bps)
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
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
    use pf_zerocopy::cuda::DeviceBuffer;

    /// Env knob for the `#[ignore]`d hardware spikes, which every caller's doc says to run ALONE
    /// with `--test-threads=1` (they mutate process env and own the GPU).
    fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: only reached from the manually-run `--test-threads=1` hardware tests, so no
        // other thread exists in this process to read or write the environment concurrently.
        unsafe { std::env::set_var(key, val) };
    }

    /// [`set_env`]'s companion; the same single-threaded-run contract.
    fn remove_env(key: &str) {
        // SAFETY: as `set_env` — single-threaded manual test run, no concurrent env access.
        unsafe { std::env::remove_var(key) };
    }

    /// The 10-bit input mapping is load-bearing in a way a smoke test can't reach: pick the wrong
    /// NVENC format for a packed 2:10:10:10 capture and the encoder reads the words as 8-bit
    /// `ARGB` — a picture that decodes, looks *almost* right, and is silently 8-bit with the
    /// channels shifted. These are the two tables that decide it.
    #[test]
    fn ten_bit_rgb_maps_to_the_matching_nvenc_format_and_blend_mode() {
        use nv::NV_ENC_BUFFER_FORMAT as F;
        // `x:R:G:B` (B in the low bits) is NVENC's ARGB10; `x:B:G:R` is ABGR10.
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB10));
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ABGR10));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_NV12));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_YUV444));
        // …and each gets the cursor-blend mode that unpacks ITS channel order. Swapping these
        // would tint the pointer (R and B exchanged) with nothing else out of place.
        assert_eq!(
            slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB10),
            SlotFormat::X2Rgb10
        );
        assert_eq!(
            slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ABGR10),
            SlotFormat::X2Bgr10
        );
        assert_eq!(slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB), SlotFormat::Argb);
    }

    /// The `encoder_engines` field `query_caps` latched — read through a helper so the intent
    /// ("what the resolver will actually see") is explicit at the call site.
    fn self_engines(enc: &NvencCudaEncoder) -> u32 {
        enc.encoder_engines
    }

    /// An NV12 frame filled with **real high-entropy content**, not the zeroed VRAM every other
    /// helper here hands the encoder.
    ///
    /// This matters more than it looks. Under CBR the rate controller spends its quota only if
    /// there is something to code; against uninitialised (driver-zeroed) buffers it emits ~300 B/AU
    /// where the configured rate wants ~833 KB, so every timing taken that way measures the
    /// PIXEL-proportional cost and is blind to the bits/frame regime — the regime the 4K60 HDR
    /// field report actually came from. A cheap xorshift per pixel plus a per-frame seed gives both
    /// spatial detail (so intra costs real bits) and inter-frame change (so P-frames cannot
    /// skip-code), which is what drives the entropy coder.
    /// `block` sets the spatial detail: 1 = per-pixel noise (incompressible — rate control
    /// OVERSHOOTS any low target), larger = blockier and cheaper to code. Sweeping it is how the
    /// bench reaches the LOW bits/frame end at all; pure noise cannot get there.
    fn noise_nv12_frame(w: u32, h: u32, i: u32, block: usize) -> CapturedFrame {
        let buf = DeviceBuffer::alloc_nv12(w, h).expect("alloc NV12 device buffer");
        let (uv_ptr, uv_pitch) = buf.uv.expect("NV12 buffer has a UV plane");
        let mut st = 0x2545_F491_4F6C_DD1Du64 ^ ((i as u64 + 1) << 32);
        let mut next = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        let b = block.max(1);
        let mut plane = |pw: usize, ph: usize| -> Vec<u8> {
            let bw = pw.div_ceil(b);
            let cells: Vec<u8> = (0..(bw * ph.div_ceil(b)))
                .map(|_| (next() >> 24) as u8)
                .collect();
            let mut out = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                let row = y / b * bw;
                for x in 0..pw {
                    out.push(cells[row + x / b]);
                }
            }
            out
        };
        let y = plane(w as usize, h as usize);
        let uv = plane(w as usize, h as usize / 2);
        pf_zerocopy::cuda::write_plane_from_host(buf.ptr, buf.pitch, &y, w as usize, h as usize)
            .expect("upload Y plane");
        pf_zerocopy::cuda::write_plane_from_host(uv_ptr, uv_pitch, &uv, w as usize, h as usize / 2)
            .expect("upload UV plane");
        CapturedFrame {
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

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
            cursor: None,
        }
    }

    /// ON-HARDWARE (RTX box `.21`): drives the full direct-SDK CUDA path end to end — open the
    /// session on the shared `CUcontext`, allocate + register the CUDA input-surface ring, encode
    /// synthetic NV12 frames, then perform a **real** `invalidate_ref_frames` over an in-DPB range
    /// and assert the next AU carries the recovery-anchor tag (the F2 fix) and that `caps()`
    /// advertises RFI. Needs an NVIDIA GPU + driver. Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_cuda_smoke --nocapture
    /// ON-HARDWARE: the codec/4:4:4 advertisement probe against the real driver. Asserts the two
    /// invariants that matter for what the host advertises — every NVENC-capable GPU ever made can
    /// encode H.264, so a probe that comes back with `h264 = false` while NVENC is otherwise
    /// working means the enumeration itself is broken (and would silently narrow the host's
    /// advertisement); and the answer must be stable across calls (asserted on the UNCACHED fn —
    /// the cached [`probe_support`] would make it vacuous), since one cached answer drives every
    /// negotiation. Prints the mask so a run on an OLD card (Maxwell GM107 = h264 only, no 4:4:4 —
    /// the GPU this probe exists for) is self-documenting. Run:
    ///   cargo test -p pf-encode --features nvenc -- --ignored nvenc_codec_probe --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on an NVIDIA box"]
    fn nvenc_codec_probe_reports_real_gpu_support() {
        let probed = probe_support_uncached();
        let caps = probed.codecs;
        eprintln!(
            "NVENC probe: h264={} h265={} av1={} hevc_444={}",
            caps.h264, caps.h265, caps.av1, probed.hevc_444
        );
        assert!(
            caps.h264,
            "every NVENC generation encodes H.264 — a false here means the GUID enumeration \
             failed, which would narrow the host's codec advertisement"
        );
        assert!(
            !probed.hevc_444 || caps.h265,
            "a 4:4:4-capable HEVC that is not in the GUID list is contradictory"
        );
        let again = probe_support_uncached();
        assert_eq!(
            (caps.h264, caps.h265, caps.av1, probed.hevc_444),
            (
                again.codecs.h264,
                again.codecs.h265,
                again.codecs.av1,
                again.hevc_444
            ),
            "the probe must be stable — it is cached once and drives every later negotiation"
        );
    }

    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_smoke_rfi_anchor() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

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
            false,
            4,
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

    /// A packed 2:10:10:10 (`X2Rgb10`) CUDA frame — the layout an HDR gamescope capture imports
    /// through the Vulkan bridge, and what NVENC ingests as `ARGB10` with no host CSC at all.
    /// The device memory is uninitialised: this smoke asserts the session/registration/encode
    /// machinery, not picture fidelity (that is the AMD round-trip's job — the CSC here is
    /// NVENC's own ASIC, not our shader).
    fn rgb10_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        let buf = DeviceBuffer::alloc(w, h).expect("alloc packed RGB device buffer");
        CapturedFrame {
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    /// ON-HARDWARE: the HDR path — a packed 10-bit PQ/BT.2020 CUDA payload straight into NVENC as
    /// `ARGB10`, which is what makes an NVIDIA HDR session zero-copy AND host-CSC-free: NVENC does
    /// the BT.2020 conversion in the ASIC, following the VUI this session configures.
    ///
    /// The load-bearing assertions are the ones that would catch a mislabelled stream: the encoder
    /// must have DERIVED 10-bit from the input format (not merely been asked for it), and it must
    /// report HDR — that pair is what selects Main10 / AV1-10 and the BT.2020 PQ signalling.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
    fn nvenc_cuda_hdr10_packed_rgb() {
        for codec in [Codec::H265, Codec::Av1] {
            const W: u32 = 1280;
            const H: u32 = 720;
            pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
            let mut enc = NvencCudaEncoder::open(
                codec,
                PixelFormat::X2Rgb10,
                W,
                H,
                60,
                20_000_000,
                true,
                10,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");

            let mut aus = 0usize;
            let mut first_key = false;
            let mut stream: Vec<u8> = Vec::new();
            for i in 0..4u32 {
                enc.submit_indexed(&rgb10_frame(W, H, i), i)
                    .expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    if aus == 0 {
                        first_key = au.keyframe;
                    }
                    assert!(!au.data.is_empty(), "empty AU");
                    stream.extend_from_slice(&au.data);
                    aus += 1;
                }
            }
            enc.flush().ok();
            // Dumped for the out-of-band ffprobe check. In-tree we can assert the encoder's OWN
            // view of the config; only a decoder confirms the BITSTREAM says Main10 / BT.2020 /
            // PQ, which is what a client actually reads.
            if let Ok(home) = std::env::var("HOME") {
                let ext = if codec == Codec::Av1 { "obu" } else { "h265" };
                let path = format!("{home}/nvenc-hdr10.{ext}");
                if std::fs::write(&path, &stream).is_ok() {
                    println!(
                        "nvenc_cuda HDR10 {codec:?}: wrote {path} ({} bytes)",
                        stream.len()
                    );
                }
            }
            assert!(aus > 0, "{codec:?}: no AUs produced");
            assert!(first_key, "{codec:?}: first AU must be the session IDR");
            // The whole point: depth + HDR came from the INPUT format, so the bitstream's profile
            // and colour signalling describe what was actually encoded.
            assert_eq!(enc.bit_depth, 10, "{codec:?}: must have derived 10-bit");
            assert!(
                enc.hdr,
                "{codec:?}: must have derived HDR from the PQ format"
            );
            assert_eq!(
                enc.buffer_fmt,
                nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
                "{codec:?}: X2Rgb10 must ingest as ARGB10"
            );
            println!("nvenc_cuda HDR10 {codec:?}: {aus} AUs, ARGB10 in, 10-bit derived");
        }
    }

    /// ON-HARDWARE: the cursor blended into a **10-bit** input surface — `cursor_blend.comp`'s
    /// MODE 3/4, which unpack 2:10:10:10 channels instead of bytes. New shader code, and the only
    /// way a gamescope pointer reaches an HDR NVIDIA stream: the packed-RGB slot is what NVENC
    /// ingests, so the blend has to happen in that layout rather than in a YUV plane.
    ///
    /// Asserts the machinery — AUs come out, and the blend targets the 10-bit slot layout rather
    /// than silently falling back to the 8-bit one, which would tint the pointer and shift its
    /// channels. Blend CORRECTNESS is display-referred by design (see the shader), so it is
    /// judged by eye on a dump, not here.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
    fn nvenc_cuda_hdr10_cursor_blend() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::X2Rgb10,
            W,
            H,
            60,
            8_000_000,
            true,
            10,
            ChromaFormat::Yuv420,
            true, // cursor_blend: bring up the Vulkan slot ring + the 10-bit blend
            4,
        )
        .expect("open NVENC CUDA session");
        let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
            x,
            y,
            w: 32,
            h: 32,
            rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
            serial,
            hot_x: 0,
            hot_y: 0,
            visible: true,
        };
        let mut aus = 0usize;
        for i in 0..6u32 {
            let mut frame = rgb10_frame(W, H, i);
            // Bitmap serial flips at frame 3 (upload quiesce over in-flight ordered blends); the
            // position moves every frame (push-constant path) — same shape as the 8-bit twin.
            frame.cursor = Some(cursor(
                if i < 3 { 1 } else { 2 },
                40 + i as i32 * 9,
                60 + i as i32 * 5,
            ));
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                assert!(!au.data.is_empty(), "empty AU");
                aus += 1;
            }
        }
        enc.flush().ok();
        assert!(aus > 0, "no AUs produced");
        assert_eq!(enc.bit_depth, 10, "must be a 10-bit session");
        assert_eq!(
            slot_fmt_of(enc.buffer_fmt),
            SlotFormat::X2Rgb10,
            "the blend must target the 10-bit packed slot layout, not the 8-bit one"
        );
        assert!(
            enc.caps().blends_cursor,
            "the direct-SDK path must still report a cursor blend at 10-bit"
        );
        println!("nvenc_cuda HDR10 cursor blend: {aus} AUs, slot fmt X2Rgb10");
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
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
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
            false,
            4,
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
                cursor: None,
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
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
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
            false,
            4,
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

    /// ON-HARDWARE — **spike S1** (`design/nvenc-split-encode-engagement-implementation-plan.md`):
    /// can `splitEncodeMode` change via `nvEncReconfigureEncoder` with `resetEncoder=0`, WITHOUT
    /// emitting an IDR?
    ///
    /// This is the gate on the whole split-engagement program. `splitEncodeMode` lives in
    /// `NV_ENC_INITIALIZE_PARAMS`, and our own invariant says a reconfigure "must present the SAME
    /// init params as the open" (`windows/nvenc.rs:620`) — but that is OUR rule, never tested
    /// against the driver. A forced mid-stream IDR is not acceptable (user), so:
    /// - **driver rejects the change** → the constraint is real; the split decision is
    ///   once-per-session and must be predicted at open.
    /// - **accepts it AND the next AU is not a keyframe** → mid-stream adaptation is free, and the
    ///   engagement rule can simply be re-resolved whenever ABR moves.
    /// - **accepts it but emits an IDR anyway** → same as a rejection for our purposes. This is the
    ///   case a naive "did it return Ok?" check would get wrong, which is why the keyframe count
    ///   below is the real assertion.
    ///
    /// Sub-frame is pinned OFF for the whole test: HEVC forced-split and sub-frame readback are
    /// mutually unsupported (`resolve_split_subframe`), so leaving it on would have the driver
    /// reject the reconfigure for the WRONG reason and read as a false negative.
    ///
    /// Reports rather than asserts the verdict — S1 is a measurement, and BOTH outcomes are
    /// legitimate findings. It only asserts the things that would invalidate the measurement
    /// itself (session came up, engines ≥ 2, the arms actually differ). Run ALONE (it sets env):
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_split_reconfigure_in_place --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_reconfigure_in_place() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Isolate the split variable: sub-frame off, and open explicitly split-DISABLED so the
        // switch below is a real change rather than a no-op.
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        set_env("PUNKTFUNK_SPLIT_ENCODE", "0");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let (mut aus, mut keyframes) = (0usize, 0usize);
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

        // Frames first: the session is lazily created on the first submit, and
        // `reconfigure_bitrate` short-circuits to `true` while `!inited` (no session to reconfigure
        // yet), which would make the whole spike vacuous.
        let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
        assert!(aus > 0, "no AUs before the reconfigure");
        assert_eq!(kfs, 1, "exactly the opening IDR before the reconfigure");
        assert!(
            enc.inited,
            "session must be live for the spike to mean anything"
        );
        assert_eq!(
            enc.split_mode, disable,
            "the spike needs to OPEN split-disabled so the switch is a real change"
        );

        // Engine count (WP1.1's probe, borrowed): forced-2 on a 1-NVENC GPU would be rejected for a
        // reason that has nothing to do with reconfigure, so the verdict is only interpretable
        // when the card actually has a second engine.
        // SAFETY: `enc.encoder` is the live session (`inited` asserted above); `get_cap` only reads
        // a cap through it and returns 0 on any driver error.
        let engines = unsafe {
            enc.get_cap(
                enc.encoder,
                nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
            )
        };
        println!(
            "S1: NV_ENC_CAPS_NUM_ENCODER_ENGINES = {engines} (query_caps latched \
             encoder_engines={})",
            self_engines(&enc)
        );
        // The cap is only useful if `query_caps` actually stored it — that latched field is what
        // `resolve_split_mode` reads to pick the split width, so a silent 0 there would quietly
        // fall back to "assume two engines" on every GPU.
        assert_eq!(
            self_engines(&enc),
            engines.max(0) as u32,
            "query_caps must latch NUM_ENCODER_ENGINES — resolve_split_mode reads that field, \
             not the live cap"
        );
        assert!(
            engines >= 2,
            "this GPU reports {engines} NVENC engine(s) — S1 is not interpretable here, run it on \
             a 2-engine card"
        );

        // THE SPIKE: change ONLY splitEncodeMode (same bitrate, same everything else) and ask the
        // driver to take it in place.
        enc.split_mode = two;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1: reconfigure DISABLE→TWO_FORCED accepted = {accepted}");

        let verdict = if !accepted {
            // Restore the field so the encoder's idea of its own session stays truthful for the
            // rest of the test (the live session is still split-disabled).
            enc.split_mode = disable;
            "FAIL — driver REJECTED the in-place splitEncodeMode change"
        } else {
            let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
            assert!(aus > 0, "no AUs after the accepted reconfigure");
            if kfs == 0 {
                "PASS — accepted with NO IDR: mid-stream split adaptation is free"
            } else {
                "FAIL — accepted but forced an IDR (silently), which is the same as a rejection"
            }
        };
        println!("S1 VERDICT: {verdict}");

        // The reverse direction only means something if the forward one worked.
        if accepted {
            enc.split_mode = disable;
            let back = enc.reconfigure_bitrate(BPS);
            let kfs = if back {
                submit_and_poll(&mut enc, 8..12).1
            } else {
                usize::MAX
            };
            println!("S1: reverse TWO_FORCED→DISABLE accepted = {back}, keyframes after = {kfs}");
        }

        enc.flush().ok();
        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// ON-HARDWARE — **spike S1b**, the other half of S1: an in-place `splitEncodeMode` change that
    /// the driver ACCEPTS without an IDR is worthless if the driver then quietly ignores it, and
    /// "accepted, no IDR" looks identical in both cases. So measure whether it took effect.
    ///
    /// Three legs at 4K (where split has something to bite on), same bitrate throughout:
    ///   A. fresh session, split DISABLED
    ///   B. fresh session, split TWO_FORCED
    ///   C. session opened DISABLED, then reconfigured in place to TWO_FORCED
    /// If C ≈ B and both differ from A, the reconfigure is real. If C ≈ A, the driver accepted the
    /// parameter and dropped it on the floor.
    ///
    /// ⚠ **Reads out bytes/AU as well as timing, and that column is load-bearing**: these frames
    /// are uninitialised device memory, so under CBR rate control can run out of things to code
    /// and every leg collapses to the same trivially-cheap encode — which would make the A/B/C
    /// comparison meaningless rather than negative. Tiny or identical byte counts ⇒ the run says
    /// nothing, and the real answer needs the content path WP0 route (b) uses. Run ALONE:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_split_reconfigure_takes_effect --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_reconfigure_takes_effect() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;
        /// Frames to discard AFTER an in-place switch before measuring. Split-encode does not
        /// reach steady state on the first frame — even a FRESH `TWO_FORCED` session shows it
        /// (early-half 3280 µs vs late-half 1996 in one run) — and without this the switched leg
        /// lands midway between the two arms and the verdict flips run to run. Measured: at 16
        /// the switched leg reaches the fresh-split steady state; at 0 it did so only sometimes.
        const SETTLE: u32 = 16;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");

        // Separate buffers rotated per frame, so identical content can't let the encoder
        // skip-code everything and erase the difference we are trying to measure.
        // ⚠ MEASURED 2026-08-06: this does NOT work — the driver hands back zeroed VRAM, so all
        // four are identical anyway and the legs come out at ~427 B/AU against an 833 KB CBR
        // quota. What survives is the PIXEL-proportional half of the cost (motion estimation over
        // 8.29 Mpix); the bits/frame half is untested by this harness. Read the printout's
        // INCONCLUSIVE-on-content line before drawing any bitrate conclusion from it.
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // Returns (early-half p50 µs, late-half p50 µs, median bytes/AU).
        let run_leg = |open_split: &str, switch_to: Option<u32>| -> (u128, u128, usize) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", open_split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");

            // Every leg is measured over the SAME number of frames; a switched leg just starts its
            // window `SETTLE` frames later, so the arms stay comparable.
            let measure_from = if switch_to.is_some() {
                WARMUP + SETTLE
            } else {
                WARMUP
            };
            let (mut times, mut sizes) = (Vec::new(), Vec::new());
            for i in 0..(measure_from + MEASURED) {
                // Flip to the target mode exactly once, after warmup, in place.
                if i == WARMUP {
                    if let Some(target) = switch_to {
                        enc.split_mode = target;
                        assert!(
                            enc.reconfigure_bitrate(BPS),
                            "in-place split switch must be accepted (S1a proved it is)"
                        );
                        continue;
                    }
                }
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                let mut got = 0usize;
                while let Some(au) = enc.poll().expect("poll") {
                    got = au.data.len();
                }
                let dt = t0.elapsed().as_micros();
                if i >= measure_from {
                    times.push(dt);
                    sizes.push(got);
                }
            }
            enc.flush().ok();
            // Split the window in half. A single median over the whole post-switch run is
            // ACTIVELY MISLEADING here: leg C's median landed midway between the two arms and the
            // nearest-neighbour verdict flipped run to run. Early-vs-late says whether the switch
            // SETTLES — which a median cannot.
            let half = times.len() / 2;
            let med = |s: &[u128]| {
                let mut v = s.to_vec();
                v.sort_unstable();
                v[v.len() / 2]
            };
            let (early, late) = (med(&times[..half]), med(&times[half..]));
            sizes.sort_unstable();
            (early, late, sizes[sizes.len() / 2])
        };

        let (a_early, a_late, a_bytes) = run_leg("0", None);
        let (b_early, b_late, b_bytes) = run_leg("2", None);
        let (c_early, c_late, c_bytes) = run_leg("0", Some(two));
        let (a_us, b_us, c_us) = (a_late, b_late, c_late);

        println!("S1b @ {W}x{H}@60 HEVC 8-bit, {} Mbps CBR:", BPS / 1_000_000);
        println!("  (early = first half of the measured window, late = second half)");
        println!(
            "  A fresh DISABLE      : early {a_early:>6} late {a_late:>6} us/frame, {a_bytes:>8} B/AU"
        );
        println!(
            "  B fresh TWO_FORCED   : early {b_early:>6} late {b_late:>6} us/frame, {b_bytes:>8} B/AU"
        );
        println!(
            "  C DISABLE→TWO in situ: early {c_early:>6} late {c_late:>6} us/frame, {c_bytes:>8} B/AU"
        );
        if c_early > c_late + c_late / 8 {
            println!(
                "  ⇒ leg C SETTLES ({c_early} → {c_late} us): the in-place switch is not \
                 instantaneous, so a whole-window median understates it."
            );
        }

        let want_bytes = (BPS / 60 / 8) as usize;
        if a_bytes * 4 < want_bytes {
            println!(
                "  ⚠ INCONCLUSIVE on content: {a_bytes} B/AU is far below the {want_bytes} B/AU \
                 CBR quota — rate control ran out of things to code, so these legs are not the \
                 high-bits/frame regime the field case is in."
            );
        }
        let (near_b, near_a) = (c_us.abs_diff(b_us), c_us.abs_diff(a_us));
        println!(
            "  ⇒ C is nearer {} (|C-B|={near_b} vs |C-A|={near_a}) — {}",
            if near_b < near_a { "B" } else { "A" },
            if near_b < near_a {
                "the in-place split switch TOOK EFFECT"
            } else {
                "the driver appears to have IGNORED the in-place split change"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        let _ = (a_bytes, b_bytes, c_bytes);
    }

    /// ON-HARDWARE — **spike S1c**, the leg S1a/S1b deliberately excluded. Both pinned sub-frame
    /// OFF to isolate the split variable, but a real HEVC arbitration cannot: split and sub-frame
    /// readback are mutually unsupported there (`resolve_split_subframe`), so engaging split means
    /// flipping `enableSubFrameWrite` in the same breath — a SECOND init param, and the one the
    /// reconfigure path deliberately pins today (`windows/nvenc.rs:624-628`).
    ///
    /// So: can the PAIR move in place? `(DISABLE, sub-frame on)` → `(TWO_FORCED, sub-frame off)`,
    /// `resetEncoder=0`, and back. Accepted? IDR-free?
    ///
    /// ⚠ Also pins the invariant that makes this safe to build on: `subframe_chunks` is latched
    /// ONLY in the init path (line ~1625) and is NOT recomputed by `reconfigure_bitrate`, so a
    /// caller flipping sub-frame in place MUST clear it too — otherwise `supports_chunked_poll`
    /// keeps reporting true and `poll_chunk` busy-polls its whole budget every AU against a
    /// `numSlices` that never advances. That is the exact failure the Phase 8 comment warns about
    /// for an in-params drop; here the test performs the correct sequence and asserts the state
    /// stays coherent, so WP3 has a worked example to copy. Run ALONE:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_split_subframe_pair_reconfigure --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_subframe_pair_reconfigure() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Open split-DISABLED, and leave sub-frame at its Linux default (ON where the GPU
        // advertises SUBFRAME_READBACK) — that is the fleet shape the arbitration starts from.
        set_env("PUNKTFUNK_SPLIT_ENCODE", "0");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let (mut aus, mut keyframes) = (0usize, 0usize);
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
        assert!(aus > 0 && kfs == 1, "opening IDR then steady P-frames");
        println!(
            "S1c: opened split={} subframe_on={} subframe_chunks={} chunked_poll={}",
            enc.split_mode,
            enc.subframe_on,
            enc.subframe_chunks,
            enc.supports_chunked_poll()
        );
        if !enc.subframe_on {
            println!(
                "S1c SKIPPED: sub-frame is off at open on this GPU/driver, so there is no pair to \
                 flip — the arbitration reduces to S1a's plain split switch here."
            );
            remove_env("PUNKTFUNK_SPLIT_ENCODE");
            return;
        }

        // THE PAIR FLIP, in the order WP3 must use: clear the chunked-poll latch alongside the
        // sub-frame flag, or `poll_chunk` outlives the feature it depends on.
        enc.split_mode = two;
        enc.subframe_on = false;
        enc.subframe_chunks = false;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1c: (DISABLE,sub-frame on) → (TWO_FORCED,sub-frame off) accepted = {accepted}");

        if accepted {
            let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
            assert!(aus > 0, "no AUs after the pair flip");
            assert!(
                !enc.supports_chunked_poll(),
                "chunked poll must be disarmed once sub-frame is off — a stale latch makes \
                 poll_chunk busy-poll its whole budget every AU"
            );
            println!(
                "S1c VERDICT: {}",
                if kfs == 0 {
                    "PASS — the split×sub-frame PAIR moves in place with NO IDR"
                } else {
                    "FAIL — pair flip forced an IDR"
                }
            );

            // …and back, which is what a de-escalation would do.
            enc.split_mode = disable;
            enc.subframe_on = true;
            enc.subframe_chunks = enc.slices >= 2 && enc.async_rt.is_none();
            let back = enc.reconfigure_bitrate(BPS);
            let kfs_back = if back {
                submit_and_poll(&mut enc, 8..12).1
            } else {
                usize::MAX
            };
            println!("S1c: reverse pair flip accepted = {back}, keyframes after = {kfs_back}");
        } else {
            println!(
                "S1c VERDICT: FAIL — driver REJECTED the pair flip. Split can still move alone \
                 (S1a), so a WP3 arbitration would have to keep sub-frame fixed for the session \
                 and only arbitrate split within that."
            );
            enc.split_mode = disable;
            enc.subframe_on = true;
        }

        enc.flush().ok();
        remove_env("PUNKTFUNK_SPLIT_ENCODE");
    }

    /// ON-HARDWARE — **the D5 confirm** (design §2 defect D5), the one claim in that list that was
    /// only ever *inferred*: plain `AUTO` + default-on sub-frame is believed to resolve to
    /// no-split, because HEVC split is unsupported *with* sub-frame — which would make the
    /// resolver's `AUTO` fallthrough read as "let the driver decide" while actually meaning "never
    /// split", on both platforms.
    ///
    /// The driver reports no "mode I actually chose", so this settles it the same way S1b settled
    /// its question: by timing. At 4K the split/no-split gap is unmissable (~2×), so
    ///   AUTO+sub-frame ≈ DISABLE ⇒ the driver did NOT split ⇒ D5 CONFIRMED
    ///   AUTO+sub-frame ≈ TWO_FORCED ⇒ it did ⇒ D5 REFUTED and the `AUTO` arm is fine as-is
    ///
    /// Content is trivial here for the reason `nvenc_cuda_split_reconfigure_takes_effect`
    /// documents (zeroed VRAM), so this compares the PIXEL-proportional cost — which is exactly
    /// the term split halves, so the discriminator holds. Run ALONE:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_auto_split_with_subframe --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_auto_split_with_subframe() {
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // (split env, sub-frame env) → p50 µs, plus the resolved sub-frame state for the printout.
        // `split: None` means UNSET, which is the only way to reach the resolver's plain-`AUTO`
        // fallthrough: the env knob cannot express it (`0` is DISABLE, `1` is AUTO_**FORCED**),
        // and AUTO_FORCED counts as forced in `resolve_split_subframe`, so passing `1` here would
        // silently disarm sub-frame and test a completely different configuration. That mistake
        // produced a spurious "D5 REFUTED" on the first run of this test.
        let run = |split: Option<&str>, subframe: Option<&str>| -> (u128, bool) {
            match split {
                Some(v) => set_env("PUNKTFUNK_SPLIT_ENCODE", v),
                None => remove_env("PUNKTFUNK_SPLIT_ENCODE"),
            }
            match subframe {
                Some(v) => set_env("PUNKTFUNK_NVENC_SUBFRAME", v),
                None => remove_env("PUNKTFUNK_NVENC_SUBFRAME"),
            }
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let mut times = Vec::new();
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                while enc.poll().expect("poll").is_some() {}
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                }
            }
            let sub = enc.subframe_on;
            enc.flush().ok();
            times.sort_unstable();
            (times[times.len() / 2], sub)
        };

        // THE FLEET CASE: env unset ⇒ 4K60 8-bit is below SPLIT_FORCE_PIXEL_RATE (497.7 vs 950
        // Mpix/s) and not 10-bit, so the resolver falls through to plain AUTO, and sub-frame
        // stays at its caps-gated default. This leg must report sub-frame TRUE or it is not
        // testing D5.
        let (auto_us, auto_sub) = run(None, None);
        let (dis_us, dis_sub) = run(Some("0"), None);
        let (two_us, two_sub) = run(Some("2"), Some("0"));
        // The leg that decides whether the `AUTO` arm can simply be RETIRED: D5 proves AUTO does
        // not split while sub-frame is on, but retiring it would also change sub-frame-OFF
        // sessions, where AUTO is free to split and might. Measure before removing.
        let (auto_nosub_us, auto_nosub_sub) = run(None, Some("0"));

        println!("D5 confirm @ {W}x{H}@60 HEVC 8-bit:");
        println!("  AUTO (unset) + sub-frame({auto_sub}) : {auto_us:>6} us/frame");
        println!("  DISABLE      + sub-frame({dis_sub}) : {dis_us:>6} us/frame");
        println!("  TWO_FORCED,   no sub-frame({two_sub}): {two_us:>6} us/frame");
        println!("  AUTO (unset), no sub-frame({auto_nosub_sub}): {auto_nosub_us:>6} us/frame");
        println!(
            "  ⇒ with sub-frame OFF, AUTO is nearer {} — retiring the AUTO arm {}",
            if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
                "TWO_FORCED (it DOES split)"
            } else {
                "DISABLE (it does not split either way)"
            },
            if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
                "would LOSE a real split on sub-frame-off sessions"
            } else {
                "is behaviour-neutral"
            }
        );
        assert!(
            auto_sub,
            "the AUTO leg resolved sub-frame OFF — it is not testing D5's fleet shape"
        );
        let (near_dis, near_two) = (auto_us.abs_diff(dis_us), auto_us.abs_diff(two_us));
        println!(
            "  ⇒ AUTO sits nearer {} (|A-D|={near_dis} vs |A-T|={near_two}) — D5 {}",
            if near_dis < near_two {
                "DISABLE"
            } else {
                "TWO"
            },
            if near_dis < near_two {
                "CONFIRMED: AUTO + sub-frame does NOT split; the resolver's AUTO arm is dead"
            } else {
                "REFUTED: AUTO does engage the second engine even with sub-frame on"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// ON-HARDWARE — **what is the real split ceiling on this GPU?** Feeds WP1.1: we want to use
    /// every engine the card has, not a hard-coded 2.
    ///
    /// `NV_ENC_SPLIT_ENCODE_MODE` tops out at `THREE_FORCED` in SDK 0.4.0 / NVENCAPI 12.1 (values
    /// 4..14 are unallocated, so a future API could add more), and `AUTO_FORCED` means "split, you
    /// pick how many" — the only way to name a count we have no enum for.
    ///
    /// For each candidate this reports what the session ACTUALLY opened with, which is the honest
    /// signal: the backend's rejection fallback silently retries split-disabled, so a mode the
    /// driver refuses shows up as `split_mode == DISABLE` afterwards rather than as an error. And
    /// the timing says whether an ACCEPTED mode did anything — a card that takes `THREE_FORCED`
    /// but only has two engines would otherwise look like a win. Run ALONE:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_split_hardware_max --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_hardware_max() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // → (requested mode, mode actually opened, p50 µs, engines the driver reports)
        let run = |split: &str| -> (u32, u128, i32) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let mut times = Vec::new();
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                while enc.poll().expect("poll").is_some() {}
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                }
            }
            // SAFETY: the session is live (frames encoded above); `get_cap` only reads a cap and
            // returns 0 on any driver error.
            let engines = unsafe {
                enc.get_cap(
                    enc.encoder,
                    nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
                )
            };
            let opened = enc.split_mode;
            enc.flush().ok();
            times.sort_unstable();
            (opened, times[times.len() / 2], engines)
        };

        println!("split ceiling probe @ {W}x{H}@60 HEVC 8-bit:");
        let mut baseline = None;
        // The env value is NOT the enum value for DISABLE (`0` selects `NV_ENC_SPLIT_DISABLE_MODE`,
        // which is 15), so compare against the enum each arm actually asks for.
        for (label, env, want) in [
            ("DISABLE     ", "0", M::NV_ENC_SPLIT_DISABLE_MODE as u32),
            ("AUTO_FORCED ", "1", M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32),
            ("TWO_FORCED  ", "2", M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32),
            (
                "THREE_FORCED",
                "3",
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            ),
        ] {
            let (opened, us, engines) = run(env);
            let honoured = opened == want;
            let vs = match baseline {
                None => {
                    baseline = Some(us);
                    String::new()
                }
                Some(b) => format!("  ({:.2}× vs DISABLE)", b as f64 / us as f64),
            };
            println!(
                "  req {label} → opened_mode={opened:<2} {} {us:>6} us/frame{vs}  [engines={engines}]",
                if honoured { "HONOURED" } else { "FELL BACK" }
            );
        }
        println!(
            "  note: opened_mode 15 = DISABLE (the backend's rejection fallback); a mode that is \
             HONOURED but no faster than DISABLE was accepted and did nothing."
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// ON-HARDWARE — the live split arbitration end to end (WP3). Opens a 4K session that the
    /// static rule leaves single-engine, lets the arbiter run, and asserts it converges to the
    /// faster arm **without emitting a single IDR** and records a verdict other sessions can reuse.
    ///
    /// Sub-frame is pinned off so the arbiter's own no-trade gate lets it arm (see
    /// `arm_split_arbiter`); this is the shape the first increment supports.
    ///
    /// Asserts behaviour, not timing: that it settles, that it lands on the arm the ~2× split
    /// advantage implies, and — the load-bearing one — **zero keyframes after the opening IDR**,
    /// which is the whole reason this design is allowed to exist. Run ALONE:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_split_arbitration_converges --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_arbitration_converges() {
        const W: u32 = 3840;
        const H: u32 = 2160;
        let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;

        set_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE", "1");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        remove_env("PUNKTFUNK_SPLIT_ENCODE");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            400_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut keyframes = 0usize;
        let mut aus = 0usize;
        // Enough frames for measure + settle + measure with room to spare.
        for i in 0..140u32 {
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
            }
        }
        let final_mode = enc.split_mode;
        let still_arbitrating = enc.arbiter.is_some();
        let verdict = cached_split_verdict(&enc.split_key());
        let enc_engines = enc.encoder_engines;
        enc.flush().ok();

        println!(
            "arbitration: {aus} AUs, {keyframes} keyframes, final split_mode={final_mode}, \
             cached verdict={verdict:?}, still running={still_arbitrating}"
        );
        assert!(aus > 100, "not enough AUs to complete an arbitration");
        assert!(
            !still_arbitrating,
            "arbitration did not finish in 140 frames"
        );
        assert_eq!(
            keyframes, 1,
            "THE POINT OF THIS DESIGN: arbitration must cost ZERO extra IDRs — only the session's \
             opening one"
        );
        assert_eq!(
            verdict,
            Some(final_mode),
            "the winning arm must be cached so later sessions skip the experiment"
        );
        assert_ne!(
            final_mode, disable,
            "at 4K with two engines a splitting arm is ~2x faster, so single-engine must not win"
        );
        // The static rule leaves 4K60 on the fallthrough AUTO (497.7 Mpix/s is under
        // SPLIT_FORCE_PIXEL_RATE), so the experiment is AUTO vs the widest forced split — the
        // "are we leaving engines idle?" question. Either outcome is legitimate; what must NOT
        // happen is landing on single-engine.
        println!(
            "  (incumbent was the static rule's choice; challenger was mode {})",
            max_forced_split_mode(enc_engines)
        );

        remove_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        // The verdict cache is process-global: leaving this session's result in it would steer
        // every later test that opens the same config with the split env unset (the D5 legs do
        // exactly that).
        super::super::nvenc_core::clear_split_verdicts();
    }

    /// ON-HARDWARE — **THE ADA MAIN10 QUESTION**, the one this whole programme has been deferring.
    ///
    /// The 10-bit split veto rests on a single datapoint: at 5120×1440@240 Main10 on Ada, forced-2
    /// took 7.6 ms/frame against 2.8 ms single-engine — split was **2.7× SLOWER**. That number
    /// vetoed splitting for every HDR session on every GPU, and `resolve_split_mode` has now
    /// stopped short-circuiting on it, which means a Main10 session above the pixel-rate bar WILL
    /// split. If the datapoint generalises, that is a regression and the veto has to come back
    /// (scoped properly this time).
    ///
    /// So: 4K **Main10** (10-bit, via the packed-RGB10 input path), forced-2 against
    /// single-engine, same bitrate, sub-frame pinned off so only the split variable moves.
    /// Reports rather than asserts — both outcomes are legitimate findings and the point is the
    /// number. Run on the **Ada** box (`.181`) and compare against Blackwell (`.21`):
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_main10_split_ab --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 vs Blackwell .21)"]
    fn nvenc_cuda_main10_split_ab() {
        use std::time::Instant;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 12;
        const MEASURED: u32 = 32;
        // Mode is overridable so the SAME test can be pointed at the configuration the veto was
        // originally measured on — `PF_AB_MODE=5120x1440x240` reproduces the 2.7×-slower datapoint's
        // operating point, which is the one config this change flips behaviour for.
        let (w, h, fps) = std::env::var("PF_AB_MODE")
            .ok()
            .and_then(|s| {
                let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((3840, 2160, 60));

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // 10-bit input: the packed 2:10:10:10 PQ path is how a Main10 session is actually fed here
        // (`bit_depth`/`hdr` are DERIVED from the input format, never trusted from the args).
        let frames: Vec<CapturedFrame> = (0..4).map(|i| rgb10_frame(w, h, i)).collect();

        let run = |split: &str| -> (u128, u8, usize) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::X2Rgb10,
                w,
                h,
                fps,
                BPS,
                true,
                10,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let (mut times, mut bytes) = (Vec::new(), Vec::new());
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                let mut got = 0usize;
                while let Some(au) = enc.poll().expect("poll") {
                    got = au.data.len();
                }
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                    bytes.push(got);
                }
            }
            let depth = enc.bit_depth;
            let opened = enc.split_mode;
            enc.flush().ok();
            times.sort_unstable();
            bytes.sort_unstable();
            println!(
                "    (opened split_mode={opened}, derived bit_depth={depth}, \
                 {} B/AU)",
                bytes[bytes.len() / 2]
            );
            (times[times.len() / 2], depth, bytes[bytes.len() / 2])
        };

        println!(
            "Main10 split A/B @ {w}x{h}@{fps} HEVC 10-bit, {} Mbps:",
            BPS / 1_000_000
        );
        let (single_us, d1, _) = run("0");
        println!("  single-engine : {single_us:>6} us/frame");
        let (split_us, d2, _) = run("2");
        println!("  forced 2-way  : {split_us:>6} us/frame");
        assert_eq!(d1, 10, "leg 1 did not derive a 10-bit session");
        assert_eq!(d2, 10, "leg 2 did not derive a 10-bit session");
        let ratio = single_us as f64 / split_us.max(1) as f64;
        println!(
            "  ⇒ split is {ratio:.2}× the single-engine rate — {}",
            if ratio > 1.15 {
                "split WINS for Main10 here; the 2.7x-slower datapoint does NOT generalise"
            } else if ratio < 0.87 {
                "split LOSES for Main10 — the veto was right and must come back, scoped"
            } else {
                "a wash; neither arm is clearly better for Main10 here"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// ON-HARDWARE — **THE BITS/FRAME CURVE**, the measurement this whole programme has been blind
    /// to (WP0's real deliverable).
    ///
    /// Every other timing here was taken against driver-zeroed buffers, so rate control had
    /// nothing to code (~300 B/AU against an 833 KB quota) and only the PIXEL-proportional half of
    /// the encode cost was ever exercised. But the 4K60 HDR field report was a *bits/frame*
    /// problem — 6.8 Mbit/frame — and the central hypothesis is that split's benefit and the
    /// 10-bit veto's origin both live on that axis. [`noise_nv12_frame`] finally puts real entropy
    /// in front of the encoder.
    ///
    /// Sweeps bitrate at a fixed mode, single-engine vs forced-2, and prints **bytes/AU alongside
    /// every timing** — without that column a run that silently undershoots its quota looks like a
    /// result instead of a non-measurement. Run on both boxes:
    ///   cargo test -p pf-encode --features nvenc -- --ignored --test-threads=1 \
    ///     nvenc_cuda_bits_per_frame_curve --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 / Blackwell .21)"]
    fn nvenc_cuda_bits_per_frame_curve() {
        use std::time::Instant;
        const WARMUP: u32 = 10;
        const MEASURED: u32 = 24;
        let (w, h, fps) = std::env::var("PF_AB_MODE")
            .ok()
            .and_then(|s| {
                let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((3840, 2160, 60));

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Sweep CONTENT DETAIL, not nominal bitrate. Pure noise is incompressible, so a low
        // bitrate target simply overshoots (measured: 719 KB/AU against a 104 KB quota) and every
        // low row lands at the same high bits/frame — the exact blindness this test exists to fix.
        // Blockier content codes cheaper, so detail is what actually moves along the axis, and the
        // x-axis below is the bits/frame the encoder ACTUALLY produced, never the one requested.
        let bps: u64 = 600_000_000;
        println!(
            "bits/frame curve @ {w}x{h}@{fps} HEVC 8-bit, REAL content, {} Mbps cap:",
            bps / 1_000_000
        );
        println!("  detail | ACTUAL bits/frame |   single |  split-2 | ratio");
        for block in [64usize, 32, 16, 8, 4, 1] {
            let frames: Vec<CapturedFrame> =
                (0..4).map(|i| noise_nv12_frame(w, h, i, block)).collect();
            let run = |split: &str| -> (u128, usize) {
                set_env("PUNKTFUNK_SPLIT_ENCODE", split);
                let mut enc = NvencCudaEncoder::open(
                    Codec::H265,
                    PixelFormat::Nv12,
                    w,
                    h,
                    fps,
                    bps,
                    true,
                    8,
                    ChromaFormat::Yuv420,
                    false,
                    4,
                )
                .expect("open NVENC CUDA session");
                let (mut times, mut bytes) = (Vec::new(), Vec::new());
                for i in 0..(WARMUP + MEASURED) {
                    let t0 = Instant::now();
                    enc.submit_indexed(&frames[(i % 4) as usize], i)
                        .expect("submit");
                    let mut got = 0usize;
                    while let Some(au) = enc.poll().expect("poll") {
                        got = au.data.len();
                    }
                    if i >= WARMUP {
                        times.push(t0.elapsed().as_micros());
                        bytes.push(got);
                    }
                }
                enc.flush().ok();
                times.sort_unstable();
                bytes.sort_unstable();
                (times[times.len() / 2], bytes[bytes.len() / 2])
            };
            let (s_us, s_bytes) = run("0");
            let (p_us, _) = run("2");
            println!(
                "  {block:>5}px | {:>10.2} Mbit    | {s_us:>6}us | {p_us:>6}us | {:>4.2}×",
                s_bytes as f64 * 8.0 / 1e6,
                s_us as f64 / p_us.max(1) as f64
            );
        }

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
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
            false,
            4,
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

    fn open_h265() -> NvencCudaEncoder {
        NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            1280,
            720,
            60,
            20_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA encoder")
    }

    /// ON-HARDWARE: the codec-switch lifecycle from the 2026-07 field report ("switching the
    /// codec leaves the host unable to bring the encoder up until a restart") — cycle sessions
    /// across codecs in ONE process, clean drain per leg. Every leg must open and encode. Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_cuda_codec_switch --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_codec_switch_reopen() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        for (leg, codec) in [
            Codec::H265,
            Codec::Av1,
            Codec::H265,
            Codec::H264,
            Codec::H265,
        ]
        .into_iter()
        .enumerate()
        {
            let mut enc = NvencCudaEncoder::open(
                codec,
                PixelFormat::Nv12,
                W,
                H,
                60,
                20_000_000,
                true,
                8,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open");
            for f in 0..4u32 {
                let frame = nv12_frame(W, H, f);
                enc.submit_indexed(&frame, f)
                    .unwrap_or_else(|e| panic!("leg {leg} {codec:?} submit failed: {e:#}"));
                while enc.poll().expect("poll").is_some() {}
            }
            drop(enc);
        }
        println!("nvenc_cuda codec-switch: 5 legs across H265/AV1/H264, all clean");
    }

    /// ON-HARDWARE: dirty teardown — drop encoders with encodes still in flight (what a
    /// mid-stream session kill does), several times, then a fresh session must still open. Guards
    /// the teardown-with-pending path against driver-side session-slot leaks.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_dirty_teardown_reopen() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        for round in 0..3 {
            let mut enc = open_h265();
            for f in 0..4u32 {
                let frame = nv12_frame(W, H, f);
                enc.submit_indexed(&frame, f)
                    .unwrap_or_else(|e| panic!("round {round} submit {f} failed: {e:#}"));
            }
            drop(enc); // teardown with 4 in-flight encodes
        }
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0)
            .expect("reopen after dirty teardowns");
        while enc.poll().expect("poll").is_some() {}
        println!("nvenc_cuda dirty-teardown: 3 dirty drops, reopen clean");
    }

    /// ON-HARDWARE: the session-open failure path end to end — exhaust the driver's concurrent-
    /// session cap with raw opens, assert a real encoder open fails with the actionable error
    /// (and fires the one-shot self-diagnosis), then free the slots and assert the SAME encoder
    /// rebuilds in place and produces an AU. This is the transient the session loop's rebuild
    /// backoff is sized to outlive; on the RTX 5070 Ti (driver 610.43.03) the cap is 12 sessions
    /// and the failure status is `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_open_failure_diagnosis_and_recovery() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        try_api().expect("nvenc api");
        let shared = cuda::context().expect("shared ctx");

        let open_raw = |device: *mut c_void| -> (nv::NVENCSTATUS, *mut c_void) {
            let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device,
                apiVersion: nv::NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc: *mut c_void = ptr::null_mut();
            // SAFETY: live params/out-param across the synchronous call; test-only.
            let st = unsafe { (api().open_encode_session_ex)(&mut params, &mut enc) };
            (st, enc)
        };

        // Exhaust the concurrent-session cap.
        let mut held = Vec::new();
        loop {
            let (st, enc) = open_raw(shared);
            if st != nv::NVENCSTATUS::NV_ENC_SUCCESS {
                if !enc.is_null() {
                    // SAFETY: destroy the failed-open residue per the NVENC docs.
                    unsafe {
                        let _ = (api().destroy_encoder)(enc);
                    }
                }
                break;
            }
            held.push(enc);
        }
        assert!(!held.is_empty(), "expected a finite session cap");

        // A real encoder open must now fail (lazy init → caps probe) with the actionable error.
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        let err = enc
            .submit_indexed(&frame, 0)
            .expect_err("submit must fail while the cap is exhausted");
        println!("at-cap error (self-diagnosis logged alongside): {err:#}");

        // The transient clears (slots freed) → the SAME encoder rebuilds in place and encodes.
        for e in held {
            // SAFETY: e came from a successful raw open above; destroyed exactly once.
            unsafe {
                let _ = (api().destroy_encoder)(e);
            }
        }
        assert!(enc.reset(), "in-place reset must be available");
        let frame = nv12_frame(W, H, 1);
        enc.submit_indexed(&frame, 1)
            .expect("rebuild after the transient cleared");
        let mut got = false;
        while enc.poll().expect("poll").is_some() {
            got = true;
        }
        assert!(got, "recovered encoder must produce an AU");
        println!("nvenc_cuda open-failure recovery: cap hit → diagnosed → recovered in place");
    }

    /// ON-HARDWARE (RTX box `.21`): the stream-ordered submit (latency plan §7 LN2) must ARM on a
    /// default-env session — `NvEncSetIOCudaStreams` accepted, boxed `CUstream` held. Guards
    /// against a silent fallback to blocking copies: a rejected binding still encodes correctly,
    /// just with the per-frame CPU syncs back, which no other test would notice.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_stream_ordered_arms() {
        const W: u32 = 640;
        const H: u32 = 360;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Respect an explicit operator opt-out (or two-thread mode) rather than fail.
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(au.keyframe, "opening AU must be the session IDR");
        assert!(
            enc.stream_ordered,
            "IO-stream binding must arm on a default-env session (NvEncSetIOCudaStreams rejected?)"
        );
        assert!(
            !enc.io_stream.is_null(),
            "the boxed CUstream must be held while armed"
        );
    }

    /// ON-HARDWARE (RTX box `.21`): cursor-bearing frames must KEEP the stream-ordered fast
    /// path — the gamescope 80-fps-on-a-120-session fix. With the timeline-semaphore blend
    /// available, `submit` takes `blend_ref_ordered` (the ticket advances by 2 per frame)
    /// instead of the CPU-synced fence-wait blend, and AUs keep flowing — including across a
    /// cursor-bitmap change (exercises the upload quiesce) and per-frame position moves.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_cursor_blend_stream_ordered() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Respect an explicit operator opt-out (or two-thread mode) rather than fail.
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
            true, // cursor_blend: bring up the Vulkan slot ring + blend
            4,
        )
        .expect("open NVENC CUDA session");
        let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
            x,
            y,
            w: 32,
            h: 32,
            rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
            serial,
            hot_x: 0,
            hot_y: 0,
            visible: true,
        };
        let mut aus = 0usize;
        for i in 0..6u32 {
            let mut frame = nv12_frame(W, H, i);
            // Bitmap serial flips at frame 3 (upload quiesce over in-flight ordered blends);
            // the position moves every frame (push-constant path).
            frame.cursor = Some(cursor(
                if i < 3 { 1 } else { 2 },
                40 + i as i32 * 9,
                60 + i as i32 * 5,
            ));
            enc.submit_indexed(&frame, i).expect("submit cursor frame");
            while enc.poll().expect("poll").is_some() {
                aus += 1;
            }
        }
        assert_eq!(aus, 6, "every cursor frame must deliver an AU");
        assert!(
            enc.stream_ordered,
            "IO-stream binding must arm on a default-env session"
        );
        let vk = enc
            .vk_blend
            .as_ref()
            .expect("Vulkan slot blend must come up on an RTX box");
        assert!(
            vk.ordered_ready(),
            "timeline semaphore must export to CUDA on this driver"
        );
        assert_eq!(
            vk.ordered_ticket(),
            12,
            "all 6 cursor blends must take the ordered path (2 timeline values each)"
        );
        println!(
            "nvenc_cuda cursor stream-ordered: 6 cursor AUs, ticket={}",
            vk.ordered_ticket()
        );
    }

    /// ON-HARDWARE (RTX box `.21`): the §7 LN3 pipelined-retrieve escalation —
    /// `set_pipelined(true)` on a live sync session must rebuild it without the IO-stream
    /// binding, spawn the retrieve thread on the re-open, and keep delivering AUs (the first
    /// post-escalation AU is the re-open's session-opening IDR; pipelined `poll` is
    /// non-blocking, so AUs may ride a later tick).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_pipelined_escalation() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        if async_retrieve_env() == Some(false) {
            println!("skipped: PUNKTFUNK_NVENC_ASYNC=0 vetoes the escalation");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        // Steady sync frames first (stream-ordered mode).
        for i in 0..3u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            enc.poll().expect("poll").expect("AU");
        }
        assert!(enc.async_rt.is_none(), "session starts sync");
        assert!(enc.set_pipelined(true), "escalation must be accepted");
        let mut aus = 0usize;
        let mut first_key = false;
        for i in 3..13u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i)
                .expect("submit post-escalation");
            while let Some(au) = enc.poll().expect("poll") {
                if aus == 0 {
                    first_key = au.keyframe;
                }
                aus += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        // Drain the pipelined tail (bounded).
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while aus < 10 && std::time::Instant::now() < deadline {
            if enc.poll().expect("poll").is_some() {
                aus += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            enc.async_rt.is_some(),
            "retrieve thread must be live after escalation"
        );
        assert!(
            !enc.stream_ordered,
            "IO-stream binding must be gone in pipelined mode"
        );
        assert_eq!(aus, 10, "every post-escalation frame must deliver an AU");
        assert!(first_key, "first post-escalation AU is the re-open IDR");
    }

    /// ON-HARDWARE (RTX box `.21`), MEASUREMENT probe for latency plan §7 LN1 — answers the
    /// go/no-go question for sub-frame slice output: with `PUNKTFUNK_NVENC_SLICES=4` +
    /// `PUNKTFUNK_NVENC_SUBFRAME=1`, do slices become READABLE incrementally while the frame is
    /// still encoding (and with what spacing), or does the driver only publish them at frame
    /// completion? Spins `lock_bitstream(doNotWait)` against the in-flight bitstream and prints a
    /// `(t_us, numSlices, bytes)` timeline. Asserts only the config half (4 slices materialize);
    /// the timeline is the experiment's output — read it with `--nocapture`. Run single-threaded
    /// (env vars are process-global): `-- --ignored --test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_subframe_slice_probe() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                remove_env("PUNKTFUNK_NVENC_SLICES");
                remove_env("PUNKTFUNK_NVENC_SUBFRAME");
            }
        }
        set_env("PUNKTFUNK_NVENC_SLICES", "4");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "1");
        let _guard = EnvGuard;

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
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
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        // Frame 0 opens the session (IDR) — drain it normally.
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit opening frame");
        enc.poll().expect("poll").expect("opening AU");

        // Frame 1: spin doNotWait locks against the in-flight bitstream BEFORE the blocking poll.
        let frame = nv12_frame(W, H, 1);
        enc.submit_indexed(&frame, 1).expect("submit probed frame");
        let bs = enc.pending.back().expect("in-flight entry").0;
        let t0 = std::time::Instant::now();
        let mut timeline: Vec<(u64, nv::NVENCSTATUS, u32, u32)> = Vec::new();
        let mut offsets = [0u32; 32];
        loop {
            let mut lock = nv::NV_ENC_LOCK_BITSTREAM {
                version: nv::NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: bs,
                sliceOffsets: offsets.as_mut_ptr(),
                ..Default::default()
            };
            lock.set_doNotWait(1);
            // SAFETY: `bs` is the pool bitstream the just-submitted `encode_picture` targets and
            // the session is live for the whole test; `lock` (version set, doNotWait) and
            // `offsets` are live stack locals across the synchronous call; a successful lock is
            // unlocked before the next iteration reuses the struct. `reportSliceOffsets` was
            // armed at init so `sliceOffsets` may be written up to `numSlices` ≤ 32 entries
            // (sliceModeData = 4).
            let (status, n, bytes) = unsafe {
                let st = (api().lock_bitstream)(enc.encoder, &mut lock);
                let ok = st == nv::NVENCSTATUS::NV_ENC_SUCCESS;
                let (n, b) = if ok {
                    (lock.numSlices, lock.bitstreamSizeInBytes)
                } else {
                    (0, 0)
                };
                if ok {
                    let _ = (api().unlock_bitstream)(enc.encoder, bs);
                }
                (st, n, b)
            };
            let t_us = t0.elapsed().as_micros() as u64;
            timeline.push((t_us, status, n, bytes));
            // A successful doNotWait lock on a COMPLETE frame reports the final slice count; on
            // LOCK_BUSY the frame is still encoding. Stop once complete (all 4 slices) or after
            // a generous 50 ms safety window.
            if (status == nv::NVENCSTATUS::NV_ENC_SUCCESS && n >= 4) || t_us > 50_000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        println!("subframe probe timeline (t_us, status, numSlices, bytes):");
        for (t, st, n, b) in &timeline {
            println!("  {t:>7} us  {st:?}  slices={n}  bytes={b}");
        }
        // Drain the probed frame through the normal path (lock again + unmap) — proves the probe
        // locks didn't corrupt the session.
        let au = enc.poll().expect("poll probed frame").expect("probed AU");
        assert!(!au.data.is_empty(), "probed AU must carry data");
        let last = timeline.last().expect("at least one sample");
        assert_eq!(
            last.2, 4,
            "4 slices must materialize (PUNKTFUNK_NVENC_SLICES=4 + subframe readback armed)"
        );
        // One more frame end-to-end for session health.
        let frame = nv12_frame(W, H, 2);
        enc.submit_indexed(&frame, 2).expect("submit follow-up");
        enc.poll().expect("poll").expect("follow-up AU");
    }

    /// Every chunk must be cut at an Annex-B NAL boundary (slice starts carry a start code).
    fn starts_with_start_code(d: &[u8]) -> bool {
        d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
    }

    /// ON-HARDWARE (RTX box `.21`): LN1 Phase 1 — the chunked poll end to end, at the Phase-3
    /// DEFAULTS (no env knobs: 4 slices + sub-frame readback arm on their own on a
    /// SUBFRAME_READBACK-capable GPU). `poll_chunk` must (a) report the mode armed, (b) hand
    /// every AU out as chunks whose first chunk opens the AU with the right metadata and whose
    /// `last` closes it, (c) cut every chunk at an Annex-B start code, and (d) reassemble byte-
    /// identically to the finishing blocking lock's AU (enforced by the debug-build shadow check
    /// inside `poll_chunk` — a mismatch errors the test). At least one frame must actually chunk
    /// (>1 chunk) — the 5070 Ti probe shows every frame does. Run single-threaded (env vars are
    /// process-global): `-- --ignored --test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_chunked_poll_end_to_end() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // Defaults under test — make sure another test's knobs aren't leaking in.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
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
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut multi_chunk_frames = 0usize;
        let mut total_chunks = 0usize;
        for i in 0..6u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            assert!(
                enc.supports_chunked_poll(),
                "4 slices + subframe on a sync session must arm chunked poll"
            );
            let mut au = Vec::new();
            let mut chunks = 0usize;
            loop {
                let c = enc
                    .poll_chunk()
                    .expect("poll_chunk")
                    .expect("an AU is in flight — poll_chunk must block, never None");
                if chunks == 0 {
                    assert!(c.first, "the first chunk must open the AU");
                    assert_eq!(
                        c.keyframe,
                        i == 0,
                        "only the session-opening frame is an IDR"
                    );
                }
                assert_eq!(c.pts_ns, i as u64 * 16_666_667, "pts rides every chunk");
                assert!(!c.recovery_anchor, "no RFI happened");
                if !c.data.is_empty() {
                    assert!(
                        starts_with_start_code(&c.data),
                        "chunk cut must land on an Annex-B start code (frame {i}, chunk {chunks})"
                    );
                }
                au.extend_from_slice(&c.data);
                chunks += 1;
                if c.last {
                    break;
                }
            }
            assert!(!au.is_empty(), "frame {i} produced an empty AU");
            assert!(
                enc.chunk.is_none(),
                "chunk state must be cleared once the AU closes"
            );
            if chunks > 1 {
                multi_chunk_frames += 1;
            }
            total_chunks += chunks;
            println!("frame {i}: {chunks} chunks, {} bytes", au.len());
        }
        assert!(
            multi_chunk_frames >= 1,
            "sub-frame readback yielded no multi-chunk frame — incremental slice readback \
             regressed (the probe shows ~200 µs slice spacing on this GPU)"
        );
        println!(
            "nvenc_cuda chunked poll: {total_chunks} chunks over 6 frames, \
             {multi_chunk_frames} frames chunked"
        );

        // Mode-mix across frames is legal: a fully-drained chunked AU leaves poll() usable.
        let frame = nv12_frame(W, H, 6);
        enc.submit_indexed(&frame, 6)
            .expect("submit plain-poll frame");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(!au.data.is_empty());
    }

    /// ON-HARDWARE (RTX box `.21`): a session whose CLIENT ceiling is 1 slice (`max_slices` from
    /// negotiation — no cap bit / Moonlight `videoEncoderSlicesPerFrame:1`, the Chromecast field
    /// regression's fix) must encode single-slice frames with chunked poll disarmed, with NO env
    /// knobs involved. Run with `--test-threads=1` (env vars are process-global).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_single_slice_client_ceiling() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // The ceiling under test is the negotiated one, not the operator override.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
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
            false,
            1, // the client never advertised multi-slice tolerance
        )
        .expect("open NVENC CUDA session");
        for i in 0..4u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            assert_eq!(
                enc.slices, 1,
                "a 1-slice client ceiling must clamp the Phase-3 default"
            );
            assert!(
                !enc.supports_chunked_poll(),
                "single-slice sessions have no boundaries — chunked poll must stay disarmed"
            );
            let au = enc.poll().expect("poll").expect("one AU per sync frame");
            assert!(!au.data.is_empty(), "frame {i} produced an empty AU");
        }
    }

    /// ON-HARDWARE (RTX box `.21`): the Phase-3 default-on ESCAPES — `PUNKTFUNK_NVENC_SLICES=1`
    /// must fully disarm chunked poll (and `poll_chunk` degrades
    /// to exactly one self-closing whole-AU chunk (the default-path contract every non-chunked
    /// session shares). Run with `--test-threads=1` (env vars are process-global).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_chunked_poll_fallback_whole_au() {
        const W: u32 = 1280;
        const H: u32 = 720;
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                remove_env("PUNKTFUNK_NVENC_SLICES");
                remove_env("PUNKTFUNK_NVENC_SUBFRAME");
            }
        }
        let _guard = EnvGuard;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

        // Escape 1: explicit single slice — no boundaries to cut, chunked poll disarmed.
        set_env("PUNKTFUNK_NVENC_SLICES", "1");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        assert!(
            !enc.supports_chunked_poll(),
            "PUNKTFUNK_NVENC_SLICES=1 → chunked poll must not arm"
        );
        let c = enc
            .poll_chunk()
            .expect("poll_chunk")
            .expect("whole-AU chunk");
        assert!(c.first && c.last, "fallback chunk must be self-closing");
        assert!(c.keyframe, "opening AU is the session IDR");
        assert!(!c.data.is_empty());
        assert!(
            enc.poll_chunk().expect("poll_chunk").is_none(),
            "nothing in flight → None"
        );
        drop(enc);

        // Escape 2: sub-frame readback vetoed — slices stay (default 4) but chunked poll
        // disarms and the plain poll path carries the session.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        assert!(
            !enc.supports_chunked_poll(),
            "PUNKTFUNK_NVENC_SUBFRAME=0 → chunked poll must not arm"
        );
        let au = enc.poll().expect("poll").expect("AU");
        assert!(au.keyframe && !au.data.is_empty());
    }
}
