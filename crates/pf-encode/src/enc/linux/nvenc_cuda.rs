//! Direct-SDK NVENC encoder (Linux, CUDA input).
//!
//! Raw `nvEncodeAPI` so this host can do reference-frame invalidation, the recovery-anchor
//! tag, `reset()`, and HDR/Main10 — none of which libavcodec `hevc_nvenc` can express.
//! Sibling of `encode/windows/nvenc.rs`. Design: `design/linux-direct-nvenc.md`; recovery:
//! `encoder-recovery-hardening.md`.
//!
//! Loads `libnvidia-encode.so.1` at runtime (never a link-time import: AMD/Intel boxes still
//! start and fall through to VAAPI/software). Session is `NV_ENC_DEVICE_TYPE_CUDA` on the
//! shared process-wide `CUcontext`. Input is an encoder-owned ring of registered CUDA
//! surfaces; each `FramePayload::Cuda` is device→device copied into a slot. Stream-ordered
//! submit (default; `PUNKTFUNK_NVENC_STREAM_ORDERED=0` reverts) binds IO streams so copy +
//! blend enqueue without a CPU sync.
//!
//! Two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC`: `1` always, `0` never, unset = adaptive via
//! [`Encoder::set_pipelined`]) keeps the session SYNC — Linux has no completion events — and
//! moves blocking `nvEncLockBitstream` off the encode thread. Sub-frame chunked poll (default
//! 4 slices + `SUBFRAME_READBACK`) is mutually exclusive with pipelined retrieve.
//!
//! Compiles GPU-less; [`try_api`] fails cleanly without a driver.

// `unsafe_op_in_unsafe_fn` off: this file is raw CUDA/`nvEncodeAPI` calls. Wrapping each one
// would add a SAFETY that only restates the prototype. Exit: delete the empty markers.
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

// Runtime-loaded NVENC entry table. Never a link-time import: `nvenc` is compiled in
// unconditionally, and a load-time `.so` would refuse to start on AMD/Intel-only boxes.

/// Runtime `NV_ENCODE_API_FUNCTION_LIST` entries. Do not use the crate's `ENCODE_API`: its
/// static externs put a load-time `.so` import on the all-vendor binary.
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
    // GUID enumeration for [`probe_support`]. Present since NVENC 1.0; a hole here means the
    // rest of the table is unusable too.
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
    /// `NvEncSetIOCudaStreams`. The two `NV_ENC_CUSTREAM_PTR` args are pointers *to* `CUstream`
    /// values, not the streams themselves.
    set_io_cuda_streams: unsafe extern "C" fn(
        *mut c_void,
        nv::NV_ENC_CUSTREAM_PTR,
        nv::NV_ENC_CUSTREAM_PTR,
    ) -> nv::NVENCSTATUS,
}

/// Resolve the table once per process. `Err` = no driver, no `.so`, or a driver older than
/// our headers. [`NvencCudaEncoder::open`] gates on it.
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

/// Loaded table. Call only past a [`try_api`] gate; the mapping lives for the process.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

/// Codec / 4:4:4 / 10-bit caps from one throwaway session. Per-field fail direction is on the
/// fields: codecs fail open, 4:4:4 and 10-bit fail closed.
#[derive(Clone, Copy)]
pub(crate) struct ProbedSupport {
    /// Encode GUIDs this chip lists. All-`false` = unanswered; [`crate::CodecSupport::wire_mask`]
    /// turns that into `None` so the caller keeps the static superset (fail open).
    pub codecs: crate::CodecSupport,
    /// HEVC 4:4:4 encode. `false` when unanswered (fail closed: a 4:2:0 session beats a dead
    /// one).
    pub hevc_444: bool,
    /// 10-bit encode per listed codec. Must not go through ffmpeg `hevc_nvenc`: mixing that
    /// client with this direct-SDK session wedges later opens process-wide
    /// (`NV_ENC_ERR_INVALID_VERSION`). `false` when unanswered (fail closed).
    pub ten_bit: crate::CodecSupport,
}

/// Cached [`probe_support_uncached`] — one throwaway session per process.
pub(crate) fn probe_support() -> ProbedSupport {
    static CACHE: std::sync::OnceLock<ProbedSupport> = std::sync::OnceLock::new();
    *CACHE.get_or_init(probe_support_uncached)
}

/// Ask this GPU's driver which codecs it encodes, plus HEVC 4:4:4 / 10-bit.
///
/// Do not open a libav `*_nvenc` to answer: mixing ffmpeg's NVENC client with this direct-SDK
/// session wedges later opens process-wide (`NV_ENC_ERR_INVALID_VERSION`). Same client, same
/// shared CUDA context as live sessions. Failures return "nothing probed"; per-field fail
/// direction is on [`ProbedSupport`].
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
    // SAFETY: `try_api()` Ok ⇒ every fn pointer is a live driver entry. `params`/`enc`/`count`/
    // `written` outlive their sync calls; `device` is the shared CUDA context. `guids` is sized
    // to the reported count. Destroy the session on every path out — a failed open may still
    // hold a slot toward the concurrent-session cap.
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
        // Kernel-module handshake succeeded — same latch `query_caps` sets.
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
        // Cap query for an absent codec is undefined — only against a listed HEVC GUID.
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
        // Same still-open session; skip unlisted GUIDs (cap query for an absent codec is undefined).
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
    // SAFETY: `Library::new` runs the trusted NVIDIA driver's initializers; absence is `Err`.
    // Each `lib.get::<T>` matches the `nvEncodeAPI.h` prototype. Version/create write through
    // live locals. Fn pointers are copied out of `Symbol` before `forget(lib)` leaks the
    // mapping for the process lifetime. OnceLock init — no aliasing.
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
        // Older driver than our headers: clean Err, not a panic.
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
        std::mem::forget(lib); // mapping must outlive the copied fn pointers (process)
        Ok(api)
    }
}

/// Bitstream pool = input-ring depth. Must stay ≥ [`async_inflight_cap`] (≤ `POOL - 1`) so a
/// slot is never reused mid-encode.
const POOL: usize = 8;

/// `PUNKTFUNK_NVENC_ASYNC`: `Some(true)` forces two-thread retrieve (at depth-1 the AU rides
/// the next tick); `Some(false)` vetoes [`Encoder::set_pipelined`]; `None` = adaptive.
/// Linux stays SYNC; only the blocking lock moves, so open cannot reject this.
fn async_retrieve_env() -> Option<bool> {
    match std::env::var("PUNKTFUNK_NVENC_ASYNC") {
        Ok(v) if matches!(v.trim(), "1" | "true" | "yes" | "on") => Some(true),
        Ok(v) if matches!(v.trim(), "0" | "false" | "no" | "off") => Some(false),
        _ => None,
    }
}

/// `PUNKTFUNK_NVENC_ASYNC=1` — two-thread retrieve from session open.
fn async_retrieve_requested() -> bool {
    async_retrieve_env() == Some(true)
}

/// Two-thread in-flight cap (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4, clamped `2..=POOL-1`).
/// Memoized: this is the `submit` backpressure loop condition.
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

/// Stream-ordered submit (default on; `PUNKTFUNK_NVENC_STREAM_ORDERED=0` = blocking copies).
/// Sync retrieve only, and only while `pending` is empty — see [`Encoder::submit`].
fn stream_ordered_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_STREAM_ORDERED")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// In-flight bitstream for the retrieve thread. Pointer as `usize`; the thread is joined
/// before the session is destroyed.
struct RetrieveJob {
    bs: usize,
}

/// Finished retrieve. `bs` lets the encode thread cross-check FIFO pairing with `pending`.
struct RetrieveDone {
    bs: usize,
    result: std::result::Result<(Vec<u8>, bool), String>,
}

/// Two-thread retrieve: job/done channels, join handle (joined in `teardown` *before* destroy),
/// and AUs absorbed by backpressure for `poll`.
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

/// Retrieve thread: blocking-lock, copy, unlock, send. Exits when the job channel closes —
/// teardown drops the sender and joins before destroy, so `enc`/`bs` outlive uses here.
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // Shared process-wide CUDA context — same `cuCtxSetCurrent` the encode thread does.
    if let Err(e) = cuda::make_current() {
        tracing::warn!(error = %format!("{e:#}"), "pf-nvenc-out: cuCtxSetCurrent failed");
    }
    let mut jobs: u64 = 0;
    while let Ok(job) = work_rx.recv() {
        // Host `wait_us` wraps a non-blocking poll here, so the encode wait is sampled on this
        // thread (same `PUNKTFUNK_PERF` cadence as submit: every 120).
        let sample = pf_host_config::config().perf && jobs % 120 == 0;
        jobs += 1;
        let t0 = std::time::Instant::now();
        // SAFETY: `job.bs` is a pool bitstream a prior `encode_picture` targeted; teardown
        // joins this thread first. `lock_bitstream` (version set, live stack local) blocks
        // until the encode finishes; `bitstreamBufferPtr` is valid until `unlock_bitstream`.
        // Copy before unlock. Secondary-thread lock/unlock while the encode thread submits is
        // the NVENC guide's threading model.
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
            break; // encode thread dropped the receiver (teardown joins us)
        }
    }
}

/// NVENC buffer format for a captured frame. NV12/YUV444 come from `DeviceBuffer` layout;
/// packed RGB is 4 bytes/px either way, so depth and channel order come from `fmt`, not `buf`.
/// Packed RGB lets NVENC do the CSC (BT.2020 NCL when HDR) — no host CSC, no depth loss.
fn buffer_format(buf: &cuda::DeviceBuffer, fmt: pf_frame::PixelFormat) -> nv::NV_ENC_BUFFER_FORMAT {
    if buf.yuv444 {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
    } else if buf.is_nv12() {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
    } else {
        match fmt {
            // `x:R:G:B` 2:10:10:10 LE = NVENC `ARGB10` (B in the low 10 bits).
            pf_frame::PixelFormat::X2Rgb10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
            // `x:B:G:R` 2:10:10:10 LE = NVENC `ABGR10` (R in the low 10 bits).
            pf_frame::PixelFormat::X2Bgr10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10,
            // Packed BGRA (`copy_device_to_device` fallback); NVENC `ARGB` does the CSC.
            _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        }
    }
}

/// Packed 10-bit RGB input. Bit depth and HDR follow the capture format, not negotiation.
fn is_ten_bit_input(fmt: nv::NV_ENC_BUFFER_FORMAT) -> bool {
    matches!(
        fmt,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10
            | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
    )
}

/// Encoder-owned input surface + NVENC registration (once at init, unregistered at teardown).
struct RingSlot {
    surface: SlotSurface,
    reg: nv::NV_ENC_REGISTERED_PTR,
}

/// Ring-slot backing: Vulkan-imported (cursor-blendable) or pitched CUDA (encode still works,
/// no cursor). Same `(ptr, pitch, height)` for NVENC registration.
enum SlotSurface {
    Cuda(InputSurface),
    /// Backing lives in [`VkSlotBlend`] (`free_slots`); the ref is Copy geometry.
    Vk(VkSlotRef),
}

fn slot_fmt_of(fmt: nv::NV_ENC_BUFFER_FORMAT) -> SlotFormat {
    match fmt {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => SlotFormat::Yuv444,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => SlotFormat::Nv12,
        // Geometry matches `Argb` (4 B/px) but blend must unpack 10-bit channels, not bytes.
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

/// `doNotWait` sample interval in [`Encoder::poll_chunk`]. Slice completions are ~200 µs
/// apart; 50 µs stays under one slice without hammering the driver.
const CHUNK_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_micros(50);

/// Chunked-readback progress for the front in-flight AU. [`Encoder::poll`] refuses while this
/// exists — a whole-AU poll would re-emit the already-shipped prefix.
struct ChunkState {
    /// Next chunk start (slice boundary).
    emitted: usize,
    /// Slices already covered by emitted chunks.
    slices_out: u32,
    /// `AuChunk::first` already handed out.
    opened: bool,
    /// Emitted bytes, compared to the finishing lock's AU. A doNotWait
    /// `bitstreamSizeInBytes` can run ahead of flushed slice bytes and ship unwritten
    /// content; the wire stays self-consistent, so only this compare sees it.
    shadow: Vec<u8>,
}

impl ChunkState {
    fn new() -> Self {
        ChunkState {
            emitted: 0,
            slices_out: 0,
            opened: false,
            shadow: Vec::new(),
        }
    }
}

pub struct NvencCudaEncoder {
    encoder: *mut c_void,
    /// Process-wide `CUcontext` this session is bound to.
    cu_ctx: *mut c_void,
    codec: Codec,
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    /// Encoded bit depth. Derived from the captured input ([`is_ten_bit_input`]), not
    /// negotiation. 10-bit rides packed RGB so NVENC does the BT.2020 CSC; the NV12/YUV444
    /// converts write 8-bit planes.
    bit_depth: u8,
    /// HEVC 4:4:4: planar-YUV444 input *and* GPU YUV444 encode.
    chroma_444: bool,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`.
    yuv444_supported: bool,
    /// HDR (BT.2020 PQ). Follows packed 10-bit input, same as `bit_depth`.
    hdr: bool,
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    ring: Vec<RingSlot>,
    next: usize,
    /// Lifetime submit count (never reset, unlike `next`) — `PUNKTFUNK_PERF` sample cadence.
    frames: u64,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// In-flight: (bitstream, mapped input, pts_ns, recovery-anchor, IDR-predicted).
    /// Fourth: first frame after a successful RFI. Fifth: submit-time IDR hint for chunks
    /// emitted before picture type is known (P-only + infinite GOP; finish lock checks).
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64, bool, bool)>,
    /// Next `inputTimeStamp`. [`Encoder::submit_indexed`] pins it to the wire index so RFI
    /// timestamps stay 1:1 across rebuilds. Self-increments for un-indexed callers.
    frame_idx: i64,
    force_kf: bool,
    /// Armed by a successful RFI; next `submit` tags that AU as the recovery anchor (NVENC
    /// applies invalidation at the next `encode_picture`).
    pending_anchor: bool,
    inited: bool,
    /// `nvEncGetEncodeCaps` — probed once before configure.
    rfi_supported: bool,
    custom_vbv: bool,
    /// Split mode the live session opened with. Reconfigure must re-present it (only rate
    /// fields may move). Meaningless while `!inited`.
    split_mode: u32,
    /// Last invalidated range — dedupes repeated RFI for one loss.
    last_rfi_range: Option<(i64, i64)>,
    /// Vulkan SPIR-V cursor blend (`vkslot.rs`). `None` = bring-up failed, ring is plain CUDA,
    /// no cursor. `cursor_tried` is one-shot; `cursor_serial` is the uploaded bitmap.
    vk_blend: Option<VkSlotBlend>,
    /// Cursor overlays expected. Off = skip Vulkan bring-up; embedded-pointer sessions never
    /// carry an overlay.
    blend_wanted: bool,
    cursor_tried: bool,
    cursor_serial: u64,
    /// Blend-warn latch: once per failure streak. A warn on every cursor frame would evict
    /// the log ring.
    cursor_blend_warned: bool,
    /// One-shot [`diagnose_failed_open`](Self::diagnose_failed_open) — a reset burst logs once.
    diagnosed: bool,
    /// Two-thread retrieve. `None` in sync mode. Lives `init_session`→`teardown`.
    async_rt: Option<AsyncRetrieve>,
    /// Pipelined-retrieve escalation. Sticky across rebuilds; switch is at the next drained
    /// point via [`maybe_engage_async`](Self::maybe_engage_async).
    want_async: bool,
    /// De-escalation waiting for a drained point. Distinct from `!want_async`: operator-forced
    /// async (`PUNKTFUNK_NVENC_ASYNC=1`) also has `want_async` false and must not be torn down.
    want_sync: bool,
    /// Heap `CUstream` the IO-stream binding points at (the API takes pointers, this struct
    /// moves). Null when off; freed in `teardown` *after* destroy.
    io_stream: *mut *mut c_void,
    /// Stream-ordered submit armed (sync retrieve). Per-frame gate also requires `pending` empty.
    stream_ordered: bool,
    /// Live slice count ([`resolve_slices`]: env, else 4 clamped to
    /// [`max_slices`](Self::max_slices)). Chunked poll needs ≥ 2. Latched at init so reconfigure
    /// presents the same slicing.
    slices: u32,
    /// Client decoder slice ceiling (`VIDEO_CAP_MULTI_SLICE` / GameStream
    /// `videoEncoderSlicesPerFrame`). 1 = single-slice: decoders that never asked can wedge
    /// on multi-slice AUs. `PUNKTFUNK_NVENC_SLICES` still wins both ways.
    max_slices: u32,
    /// `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK`. Do not force `enableSubFrameWrite` on a GPU
    /// without it — open can fail. `PUNKTFUNK_NVENC_SUBFRAME=1` overrides.
    subframe_cap: bool,
    /// Live sub-frame readback. Every `build_init_params` consumes it so open and reconfigure
    /// present identical init params.
    subframe_on: bool,
    /// `PUNKTFUNK_NVENC_SUBFRAME=1` was explicit. Latched at `query_caps` — no later env
    /// re-reads. Only [`resolve_split_subframe`]'s log severity reads it.
    subframe_forced: bool,
    /// Chunked poll armed: multi-slice + sub-frame + sync retrieve at init.
    subframe_chunks: bool,
    /// Sub-frame published bytes the finished AU disowns. Later `query_caps` on *this*
    /// encoder turns it off so the stall-recovery rebuild does not re-arm and loop into
    /// `MAX_ENCODER_RESETS`. Never cleared; a fresh encoder retests.
    subframe_broken: bool,
    /// `NV_ENC_CAPS_NUM_ENCODER_ENGINES` (`0` = unreadable). The driver accepts a split wider
    /// than the hardware and silently encodes narrower — this is the only honest ceiling.
    encoder_engines: u32,
    /// Submit stamp for the split arbiter (sync depth-1 only).
    last_submit_at: Option<std::time::Instant>,
    /// Host paced-send time (µs). `0` = never reported; the arbiter will not price the
    /// sub-frame trade.
    send_spread_us: u32,
    /// Sub-frame the session *opened* able to run. `subframe_on` moves with the arbiter; this
    /// does not, so a return to non-forced split can restore it without turning it on for a
    /// session that never had it.
    subframe_opened_with: bool,
    /// Live split experiment. `None` = gated off, already decided, or not arbitrable.
    arbiter: Option<SplitArbiter>,
    /// In-progress chunked readback of the front AU. See [`ChunkState`].
    chunk: Option<ChunkState>,
}

// SAFETY: `encoder`, `cu_ctx`, and the raw NVENC pointers in `bitstreams`/`ring`/`pending` are
// `!Send`. The encoder is moved onto the host encode thread once; `submit`/`poll`/`RFI`/`Drop`
// run there. The retrieve thread (when armed) only lock/unlocks bitstreams and is joined in
// `teardown` before destroy. The ownership-transfer move has no NVENC/CUDA call in flight.
unsafe impl Send for NvencCudaEncoder {}

impl NvencCudaEncoder {
    /// Same signature as `super::NvencEncoder::open`. `format`/`cuda` are advisory: real input
    /// comes from the first captured frame. `bit_depth`/`hdr` follow that format, not
    /// negotiation — a 10-bit session whose capture is 8-bit must encode and label 8-bit.
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
        // Fail here, not as an opaque session error on the first frame.
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
            // Provisional until the first frame names the real input (`submit` sets both).
            bit_depth,
            // HEVC-only; confirmed against frame layout + GPU at init.
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
            // A zero caller must not zero the resolver's default arithmetic.
            max_slices: max_slices.max(1),
            subframe_cap: false,
            subframe_on: false,
            subframe_forced: false,
            subframe_chunks: false,
            subframe_broken: false,
            encoder_engines: 0,
            last_submit_at: None,
            send_spread_us: 0,
            subframe_opened_with: false,
            arbiter: None,
            chunk: None,
        })
    }

    /// Engage pipelined retrieve when `pending` is empty: rebuild *without* the IO-stream
    /// binding (its output-stream wait would serialize a pipelined session). Re-open starts
    /// with an IDR. No-op until [`want_async`](Self::want_async).
    fn maybe_engage_async(&mut self) {
        if !self.want_async || self.async_rt.is_some() || !self.pending.is_empty() {
            return;
        }
        if self.inited {
            // SAFETY: encode thread, `pending` empty ⇒ nothing in flight. `teardown` handles
            // this live session; next submit lazily re-inits and spawns the retrieve thread.
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve escalation: rebuilding the session without the \
                 IO-stream binding (stream-ordered submit and two-thread retrieve are mutually \
                 exclusive); next frame opens with an IDR"
            );
        }
    }

    /// Inverse of [`maybe_engage_async`](Self::maybe_engage_async): drain, rebuild, lazy SYNC
    /// re-init restores IO-stream binding and sub-frame chunking. No-op until
    /// [`want_sync`](Self::want_sync).
    fn maybe_disengage_async(&mut self) {
        if !self.want_sync || self.async_rt.is_none() || !self.pending.is_empty() {
            return;
        }
        self.want_sync = false;
        if self.inited {
            // SAFETY: encode thread, `pending` empty ⇒ nothing in flight or queued. `teardown`
            // joins the retrieve thread; next submit lazily re-inits sync.
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve de-escalation: rebuilding the session with the sync \
                 retrieve (IO-stream binding and sub-frame chunking restored); next frame opens \
                 with an IDR"
            );
        }
    }

    /// Destroy the session and pooled resources. Size change and Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Join the retrieve thread first. An in-flight lock returns in ≤ a frame; after join
        // no other thread can touch the session destroyed below.
        if let Some(mut rt) = self.async_rt.take() {
            rt.work_tx.take();
            if let Some(j) = rt.join.take() {
                let _ = j.join();
            }
        }
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
        // A failed destroy can leak a slot toward the concurrent-session cap (per process;
        // only a restart clears it).
        if let Err(e) = (api().destroy_encoder)(self.encoder).nv_ok() {
            tracing::warn!(
                status = ?e,
                "NVENC destroy_encoder failed at teardown — the driver may have leaked this \
                 session's slot toward the concurrent-session cap"
            );
        }
        // IO-stream pointee: free only after destroy. Null so a re-init cannot double-free.
        if !self.io_stream.is_null() {
            drop(Box::from_raw(self.io_stream));
            self.io_stream = ptr::null_mut();
        }
        self.stream_ordered = false;
        // Half-chunked AU dies with the in-flight frame (forfeit); next session re-latches.
        self.subframe_chunks = false;
        self.chunk = None;
        self.ring.clear(); // CUDA InputSurfaces; Vk slots freed just below
        if let Some(vk) = &mut self.vk_blend {
            // Slot memory + CUDA mapping. Device stays up (`cursor_tried` is one-shot).
            vk.free_slots();
        }
        self.bitstreams.clear();
        self.pending.clear();
        self.encoder = ptr::null_mut();
        self.inited = false;
        self.next = 0;
        // New session starts IDR / empty DPB — prior RFI range and pending anchor are stale.
        self.last_rfi_range = None;
        self.pending_anchor = false;
    }

    /// One `NV_ENC_CAPS` value; 0 on error (unqueryable = unsupported).
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

    /// Probe caps on a throwaway session before configure so an out-of-range mode fails
    /// clearly, not as `InvalidParam`.
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
            // Destroy even a failed open: the driver may have taken the slot before erroring.
            // Skipping it leaks toward the concurrent-session cap.
            if !enc.is_null() {
                let _ = (api().destroy_encoder)(enc);
            }
            return Err(nvenc_status::call_err(
                "open_encode_session_ex (caps probe)",
                e,
            ));
        }
        // Handshake succeeded: later `NV_ENC_ERR_INVALID_VERSION` is not header/driver skew.
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
        // Sub-frame / dynamic-slice: stored on `subframe_cap`; `dyn_slice` is log-only.
        let subframe = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK);
        let dyn_slice = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE);
        // Split ceiling. Probe it: the driver accepts a wider split and silently encodes
        // narrower (`max_forced_split_mode`).
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
        // Resolve slices + sub-frame here, before open, so config/init/chunked-poll agree.
        // Clamp to `max_slices`: a client that never asked for multi-slice can wedge on
        // several slice NALs. Caps gate the sub-frame default. Env knobs still override.
        self.slices = resolve_slices(self.codec, 4.min(self.max_slices));
        // `subframe_broken` beats the operator force: this encoder already proved the
        // driver's sub-frame accounting corrupt. Per-encoder; a fresh one retests.
        self.subframe_on = resolve_subframe(self.subframe_cap) && !self.subframe_broken;
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

    /// One-shot open-failure diagnosis. Retries on a fresh CUDA context:
    /// shared-ctx poison vs driver (skew / session-cap / GPU lost) vs CUDA itself.
    /// Log-only; latched so a reset burst logs once.
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
            // SAFETY: `params`/`enc` outlive the sync call; `ctx` is the fresh diagnostic
            // context. Destroy any session the probe opened, including a failed status.
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

    /// P1/ULL `NV_ENC_CONFIG` at `bitrate`. Shared by [`try_open_session`] and
    /// [`Encoder::reconfigure_bitrate`] so an in-place retarget re-authors the same shape.
    unsafe fn build_config(&self, enc: *mut c_void, bitrate: u64) -> Result<nv::NV_ENC_CONFIG> {
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

        // Shared low-latency contract. Linux full-chroma is a YUV444 surface; AV1 input-depth
        // follows the surface (10-bit for packed PQ/BT.2020).
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

    /// Bitrate-ceiling cache key. GPU identity is the shared `CUcontext` pointer — valid
    /// once `cu_ctx` is bound (`init_session`).
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

    /// Open + init one session at `bitrate`/`split_mode`. Destroys the handle on error.
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
            // Failed open may still hold a slot — same destroy-on-error as `query_caps`.
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

    /// Lazy session + ring, keyed off the first frame's format.
    fn init_session(&mut self) -> Result<()> {
        // SAFETY: NVENC calls go through `api()` (gated in `open`). `try_open_session`/
        // `query_caps` return a live handle or `Err`; `destroy_encoder` only on a handle just
        // returned (and `best` only when non-null). Create/register take `enc` and versioned
        // locals that outlive the sync call. `set_io_cuda_streams` points at a boxed `CUstream`
        // freed once: `teardown` after destroy, or `Box::from_raw` on the rejection path.
        // Encode thread only.
        unsafe {
            self.cu_ctx = cuda::context().context("shared CUDA context (Linux direct NVENC)")?;
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;

            if let Err(e) = self.query_caps() {
                // First open of any session — one-shot diagnosis before propagating.
                self.diagnose_failed_open();
                return Err(e);
            }
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // [`resolve_split_mode`]: env / 10-bit / pixel-rate precedence.
            let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
            let mut split_mode: u32 =
                resolve_split_mode(self.codec, self.bit_depth, pixel_rate, self.encoder_engines);
            // Cached verdict wins over the static rule. Operator pin still beats both
            // (`resolve_split_mode`); only consult the cache when the knob is unset.
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
            // Split × sub-frame *before* the ladder, ceiling key, and chunked-poll latch —
            // a drop inside `build_init_params` would leave `poll_chunk` busy-polling.
            let (split_mode, subframe_on) = resolve_split_subframe(
                self.codec,
                split_mode,
                self.subframe_on,
                self.subframe_forced,
            );
            self.subframe_on = subframe_on;
            self.subframe_opened_with = subframe_on;
            const CLAMP_TOL_BPS: u64 = 20_000_000;

            // Known ceiling: open at it instead of a ~6-open binary search on every ABR overshoot.
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
            // Cache is advisory: a stale entry must not wedge the open.
            if probe.is_err() && target_bps < requested_bps {
                target_bps = requested_bps;
                probe = self.try_open_session(requested_bps, split_mode);
            }
            // Split rejection vs bitrate-cap. `used_split` is what actually opened — reconfigure
            // must re-present it, and the ceiling key uses it.
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
                // Only a param/caps rejection is "above the codec ceiling". Transient failures
                // must not steer the search — that would cache a bogus ceiling.
                Err(e) if !nvenc_status::is_param_rejection(&e) => return Err(e),
                Err(_) => {
                    // Above the codec ceiling — binary-search the max accepted.
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
                                // Transient mid-search: do not shrink the window.
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

            // Ring: register once, map per submit. Prefer Vulkan-imported slots so the cursor
            // blend writes the bytes NVENC encodes; any failure falls back to pitched CUDA.
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
            // Full Vulkan ring, else full CUDA. Never mixed (flickering cursor) or short.
            'ring: for use_vk in [self.vk_blend.is_some(), false] {
                if !use_vk && self.vk_blend.is_some() {
                    // Wholesale Vulkan retire before the CUDA retry.
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
                            // Import refused — same wholesale CUDA fallback.
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
                break 'ring;
            }

            self.inited = true;
            // Retrieve thread against the live session. Teardown joins it before destroy.
            if async_retrieve_requested() || self.want_async {
                self.async_rt = Some(AsyncRetrieve::spawn(self.encoder as usize));
                tracing::info!(
                    depth = async_inflight_cap(),
                    escalated = self.want_async,
                    "NVENC two-thread retrieve enabled (submit thread + blocking-lock thread)"
                );
            }
            // Bind IO streams to this thread's copy stream. Same stream both ways so later
            // copies into a reused slot wait for the encode. Sync retrieve only: two-thread
            // mode may recycle the captured buffer after `submit` while the stream still
            // holds the copy.
            if self.async_rt.is_none() && stream_ordered_requested() {
                let stream = cuda::copy_stream_handle();
                if !stream.is_null() {
                    // Driver takes `CUstream` pointers — box; `teardown` frees after destroy.
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
            // Chunked poll: multi-slice + sub-frame + sync retrieve. `build_init_params` arms
            // `enableSubFrameWrite` from `subframe_on` alone; this latch also needs
            // `slices >= 2`. AV1 is 1 slice — `resolve_split_subframe` disarms sub-frame
            // there so the writer and reader agree.
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
                // Final split (post-fallback) at INFO — journals run INFO+.
                split_mode = self.split_mode,
                // Engine count: the driver silently honours an over-wide request, so mode
                // alone cannot be trusted.
                engines = self.encoder_engines,
                subframe = self.subframe_on,
                "NVENC CUDA session ready"
            );
            self.arm_split_arbiter();
            Ok(())
        }
    }

    /// Arm a live split experiment. Opt-in (`PUNKTFUNK_NVENC_SPLIT_ARBITRATE=1`).
    ///
    /// Operator pin (`PUNKTFUNK_SPLIT_ENCODE`) wins. A cached verdict is not re-run. Sync
    /// depth-1 only: pipelined retrieve would mix queue depth into the cost. Needs ≥ 2
    /// engines; never H.264. HEVC forced-split drops sub-frame — the encoder measures
    /// encode time only, so it would prefer split and lose send/encode overlap. Arbitrate
    /// where nothing is traded (sub-frame already off, or AV1).
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
        // Losing sub-frame costs send/encode overlap ≈ `spread × (slices−1)/slices`. Priced
        // here: only the encoder knows `slices`.
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
        // Challenge with the widest forced split unless already there (then DISABLE). Do not
        // challenge AUTO with DISABLE — that parks the session on the slow arm.
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

    /// In-place split change, no IDR (`resetEncoder=0` at the current rate). Restore fields if
    /// the driver refuses so the encoder's idea of the session stays truthful.
    fn apply_split_mode(&mut self, mode: u32) -> bool {
        let (prev_mode, prev_sub, prev_chunks) =
            (self.split_mode, self.subframe_on, self.subframe_chunks);
        // HEVC cannot hold split and sub-frame. Restore only up to `subframe_opened_with`.
        let (mode, subframe) = resolve_split_subframe(
            self.codec,
            mode,
            self.subframe_opened_with,
            self.subframe_forced,
        );
        self.split_mode = mode;
        self.subframe_on = subframe;
        // `reconfigure_bitrate` does not recompute this latch; a stale true makes
        // `poll_chunk` busy-poll while `numSlices` never advances.
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

    fn feed_split_arbiter(&mut self, encode_us: u64) {
        let Some(arb) = self.arbiter.as_mut() else {
            return;
        };
        let action = arb.on_frame(encode_us);
        let done = arb.is_done();
        match action {
            Some(ArbAction::SwitchTo(mode)) => {
                if !self.apply_split_mode(mode) {
                    // Session will not move — abandon rather than compare the same arm twice.
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
            // Switch-back-to-incumbent settles on the mode now live.
            store_split_verdict(self.split_key(), self.split_mode);
            self.arbiter = None;
        }
    }

    /// Device→device copy into the ring slot. `sync` blocks; `!sync` enqueues on the copy
    /// stream (stream-ordered submit — gate in [`Encoder::submit`]).
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
                // NV12: UV at base + pitch*height, same pitch.
                cuda::copy_nv12_to_device(buf, base, pitch, base + pitch as u64 * hh, pitch, sync)
            }
            _ => cuda::copy_device_to_device(buf, base, pitch, sync),
        }
    }

    /// Absorb one retrieve completion: FIFO-check, unmap on the encode thread (retrieve never
    /// touches input resources), queue the AU.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some((bs, map, pts_ns, anchor, _)) = self.pending.pop_front() else {
            bail!("NVENC retrieve: completion with no in-flight frame (pairing bug)");
        };
        if bs as usize != done.bs {
            bail!("NVENC retrieve: completion out of order (pairing bug)");
        }
        // SAFETY: `map` is the mapped input for this completed encode; session live, encode
        // thread. Unmap exactly once, as the sync path's poll does.
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
        self.maybe_engage_async();
        self.maybe_disengage_async();
        // Size or format change (NV12↔YUV444) re-inits.
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
            // SAFETY: encode thread, `inited`, previous frame already polled — nothing mid-encode.
            // Cached ring/bitstreams/pending belong to `self.encoder`.
            unsafe { self.teardown() };
        }
        if !self.inited {
            self.width = captured.width;
            self.height = captured.height;
            self.buffer_fmt = new_fmt;
            // Depth + HDR follow the input, not negotiation — keeps the label and bitstream
            // in step when capture disagrees.
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
            // FREXT only on genuine YUV444; NV12/RGB cannot reconstruct full chroma.
            self.chroma_444 = self.chroma_444 && buf.yuv444;
            // `init_session` publishes `encoder` before later fallible steps. A failure leaves
            // a live session with `inited == false`; the next submit would skip teardown and
            // leak. `teardown` keys off `encoder.is_null()`, so it cleans this half-built state.
            if let Err(e) = self.init_session() {
                // SAFETY: encode thread owns the session; failed init left nothing mid-encode.
                unsafe { self.teardown() };
                return Err(e);
            }
        } else {
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;
        }

        // Opening IDR via `next == 0` (`teardown` zeroes it), not `pts`: `submit_indexed`
        // pins pts to the wire index, non-zero on a mid-session rebuild.
        let opening = self.next == 0;
        // Two-thread backpressure: block on the oldest completion so this slot is free before
        // reuse. Cap-deep instead of 1.
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

        // `PUNKTFUNK_PERF` submit split (~1 line / 2 s at 60 fps). Host `submit_us` folds
        // copy/blend/map/pic; this splits them.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;

        // Stream-ordered only while `pending` is empty: blocking `poll` drained the prior
        // encode, so the reused slot was fully read and the caller still holds this payload
        // across `poll`. Pipelined / two-thread falls back to a blocking copy so an
        // early-recycled source cannot be read late.
        let base_ordered =
            self.stream_ordered && self.async_rt.is_none() && self.pending.is_empty();
        // Cursor stays stream-ordered when the blend can wait a CUDA-held timeline semaphore.
        // Otherwise the fence/CPU path sits between copy and encode.
        let cursor_ordered = base_ordered
            && captured.cursor.is_some()
            && matches!(self.ring[slot].surface, SlotSurface::Vk(_))
            && self.vk_blend.as_ref().is_some_and(|vk| vk.ordered_ready());
        let ordered = base_ordered && (captured.cursor.is_none() || cursor_ordered);
        let t0 = std::time::Instant::now();

        self.copy_into_slot(buf, slot, !ordered)?;
        let t_copy = t0.elapsed();

        // Blend into this slot's owned surface (cursor rect, never the compositor dmabuf).
        // Ordered: copy/dispatch/encode on-device via timeline. Else CUDA copy then
        // fence-waited dispatch, then encode. Failure drops the cursor, never the frame.
        if let Some(ov) = &captured.cursor {
            if let (Some(vk), SlotSurface::Vk(vref)) =
                (self.vk_blend.as_mut(), &self.ring[slot].surface)
            {
                if self.cursor_serial != ov.serial {
                    // Quiesces in-flight ordered blends before touching staging.
                    vk.upload_cursor(ov.rgba.as_slice(), ov.w, ov.h);
                    self.cursor_serial = ov.serial;
                }
                // `surfW` = content width. Pixels past content land in cropped padding.
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
        // SAFETY: `self.encoder` is the live session. `mp` maps the ring registration and is
        // unmapped once in `poll`/teardown. `pic` points at `mp.mappedResource` and
        // `bitstreams[slot]`; SEI scratch outlives the sync `encode_picture`. The slot was
        // just filled (blocking copy, or IO-stream / timeline ordered before this encode)
        // and is not overwritten until POOL submits later, by which time this encode was polled.
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
            // First frame after RFI. A simultaneous forced IDR is itself the re-anchor — drop
            // the tag.
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

            // HDR10 SEI on every IDR. HEVC/H.264 carry SEI; AV1 uses OBUs.
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
                // Chunked-poll IDR hint = `is_idr`. P-only + infinite GOP: the driver never
                // emits an IDR we did not ask for.
                is_idr,
            ));
            // Arbiter stamp. One field, not a sixth `pending` slot: sync depth-1 has at most
            // one encode outstanding.
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
        // Hand the blocking lock to the retrieve thread. `sync_channel(POOL)` cannot fill
        // (in-flight is capped < POOL).
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
            // Latch de-escalation; switch at the next drained point. Caller re-queries until
            // inactive.
            if async_retrieve_env() == Some(true) {
                // Operator pinned async on — do not undo it.
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
            return false; // `PUNKTFUNK_NVENC_ASYNC=0`
        }
        self.want_sync = false; // latest intent wins
        if !self.want_async && self.async_rt.is_none() {
            self.want_async = true;
            self.maybe_engage_async();
        }
        true
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
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
        // Range policy is `nvenc_core::plan_range_recovery`. This backend: session gate +
        // driver loop.
        if self.encoder.is_null() || !self.rfi_supported {
            return false;
        }
        match plan_range_recovery(first, last, self.frame_idx, self.last_rfi_range) {
            // Covering range already invalidated — re-arm the anchor (it may itself have been
            // lost) but skip the driver.
            RangePlan::Covered => {
                self.pending_anchor = true;
                true
            }
            RangePlan::Decline => false,
            RangePlan::Invalidate { first, last } => {
                // `inputTimeStamp` is the wire index (`submit_indexed`), so the client's lost
                // range maps 1:1 onto the timestamps here.
                // SAFETY: live session, encode thread. Each `ts` is clamped to
                // `[oldest_in_dpb, frame_idx - 1]` — a frame still in the DPB. Call takes `u64`.
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
                // Next submit is the re-anchor.
                self.pending_anchor = true;
                true
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Mid-chunked AU: prefix is already with the caller — a whole-AU poll would double-emit.
        if self.chunk.is_some() {
            bail!("NVENC poll() called mid-chunked-AU — drain it via poll_chunk (caller bug)");
        }
        // Two-thread: non-blocking drain. `None` = still in flight; capture does not wait.
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
        // SAFETY: non-empty `pending` ⇒ live session (`teardown` clears both). Encode thread.
        // `lock_bitstream` (version set) blocks until the encode finishes; copy the slice
        // before unlock. `map` is unmapped here, exactly once.
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
            // Submit → AU. Sync depth-1: the lock blocked until the ASIC finished.
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
        // Dynamic: pipelined escalation drops the latch; a per-AU re-query must see that.
        self.subframe_chunks && self.async_rt.is_none()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // Non-chunked session: one whole-AU chunk. Mid-AU state always finishes below.
        if !self.supports_chunked_poll() && self.chunk.is_none() {
            return Ok(self.poll()?.map(AuChunk::whole));
        }
        let Some(&(bs, _, pts_ns, anchor, idr_hint)) = self.pending.front() else {
            return Ok(None);
        };
        // ~2 frame intervals of doNotWait; then the blocking lock. Worst case = sync `poll`.
        let budget = std::time::Duration::from_micros(2_000_000 / self.fps.max(1) as u64);
        let t0 = std::time::Instant::now();
        let mut offsets = [0u32; 32];
        loop {
            let emitted = self.chunk.as_ref().map_or(0, |c| c.emitted);
            let slices_out = self.chunk.as_ref().map_or(0, |c| c.slices_out);
            // SAFETY: `bs` is the front pending bitstream; session live; encode thread.
            // `lock`/`offsets` outlive the sync doNotWait call. `reportSliceOffsets` armed;
            // `numSlices` ≤ 32 (`resolve_slices` clamps 2..=32). Completed-slice bytes are
            // valid until unlock; copy the emitted range first. Unlock every successful lock.
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
                        // All slices readable — finish via blocking lock (`numSlices` is not
                        // trusted across driver branches).
                        let _ = (api().unlock_bitstream)(self.encoder, bs);
                        break;
                    }
                    if n > slices_out && bytes > emitted {
                        // New slices: cut `[emitted..bytes)` — contiguous Annex-B, NAL boundary.
                        let data =
                            std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, bytes)
                                [emitted..]
                                .to_vec();
                        (api().unlock_bitstream)(self.encoder, bs)
                            .nv_ok()
                            .map_err(|e| nvenc_status::call_err("unlock_bitstream (chunk)", e))?;
                        let cs = self.chunk.get_or_insert_with(ChunkState::new);
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
                // LOCK_BUSY / not ready is not an error; the finishing lock owns real failures.
            }
            if t0.elapsed() > budget {
                break;
            }
            std::thread::sleep(CHUNK_SAMPLE_INTERVAL);
        }

        // One blocking lock — completion authority and wedge watchdog (depth-1: tail must
        // not ride a +1 tick). Emits whatever the sampler had not handed out.
        let (bs, map, pts_ns, anchor, idr_hint) =
            self.pending.pop_front().expect("front() checked above");
        // SAFETY: same as `poll`: live session, encode thread, blocking lock. Reads (tail +
        // prefix check) before unlock. Unmap `map` exactly once.
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
            // doNotWait bytes must be a prefix of the finished AU — otherwise the wire is
            // self-consistent and wrong. Latch sub-frame off and bail into stall-recovery
            // (rebuild without sub-frame, IDR). `emitted > total` first: the prefix slice
            // would be ill-formed.
            let diverged = cs.emitted > total || cs.shadow.as_slice() != &full[..cs.emitted];
            if diverged {
                let _ = (api().unlock_bitstream)(self.encoder, bs);
                if !map.is_null() {
                    let _ = (api().unmap_input_resource)(self.encoder, map);
                }
                self.subframe_broken = true;
                tracing::warn!(
                    emitted = cs.emitted,
                    total,
                    "NVENC sub-frame readback diverged from the finished AU — this driver's \
                     early slice publishes cannot be trusted; disarming sub-frame for every \
                     later session open and rebuilding the encoder"
                );
                bail!(
                    "NVENC chunked poll: sub-frame readback diverged from the finished AU \
                     ({} bytes emitted, {} total) — sub-frame disarmed, rebuild required",
                    cs.emitted,
                    total
                );
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
                // P-only + infinite GOP should not diverge; earlier chunks would be mis-flagged.
                tracing::warn!(
                    predicted = idr_hint,
                    actual = keyframe,
                    "NVENC chunked poll: picture type diverged from the submit-time prediction"
                );
            }
            // Chunked path is how a sub-frame session finishes — feed the arbiter here too or
            // the HEVC sub-frame incumbent is invisible.
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
        // SAFETY: encode thread, between submit/poll. `teardown` no-ops a null session.
        unsafe { self.teardown() };
        self.force_kf = true;
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        if !self.inited {
            self.bitrate_bps = bps;
            return true;
        }
        // Clamp to the cached ceiling before the driver call — an overshoot would otherwise
        // rebuild (IDR). Caller reads the clamp via [`Encoder::applied_bitrate_bps`].
        let bps = match cached_ceiling(&self.ceiling_key(self.split_mode)) {
            Some(ceiling) => bps.min(ceiling),
            None => bps,
        };
        // SAFETY: `inited` ⇒ live session, encode thread, between submit/poll. `cfg` outlives
        // the sync reconfigure that points `encodeConfig` at it.
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
            // Keep RC state and DPB: no reset, no IDR. Wire-index prediction survives.
            params.set_resetEncoder(0);
            params.set_forceIDR(0);
            match (api().reconfigure_encoder)(self.encoder, &mut params).nv_ok() {
                Ok(()) => {
                    self.bitrate_bps = bps;
                    true
                }
                Err(e) => {
                    // Above the codec ceiling — caller's rebuild owns the clamp search.
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
        // Post-clamp target: open search and reconfigure cache clamp both write it.
        Some(self.bitrate_bps)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + `frameIntervalP=1`: each submit yields its AU.
    }
}

impl Drop for NvencCudaEncoder {
    fn drop(&mut self) {
        // SAFETY: exclusive owner on the encode thread. `teardown` no-ops a null session;
        // otherwise cached resources belong to that live session. Once.
        unsafe { self.teardown() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
    use pf_zerocopy::cuda::DeviceBuffer;

    /// Env helper for ignored hardware tests. Run `--test-threads=1` — they mutate process env.
    fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: `--test-threads=1` hardware tests only — no concurrent env access.
        unsafe { std::env::set_var(key, val) };
    }

    /// Same single-threaded contract as [`set_env`].
    fn remove_env(key: &str) {
        // SAFETY: as `set_env` — single-threaded, no concurrent env access.
        unsafe { std::env::remove_var(key) };
    }

    /// Wrong NVENC format for packed 2:10:10:10 is silently 8-bit `ARGB` with channels shifted.
    #[test]
    fn ten_bit_rgb_maps_to_the_matching_nvenc_format_and_blend_mode() {
        use nv::NV_ENC_BUFFER_FORMAT as F;
        // `x:R:G:B` = ARGB10; `x:B:G:R` = ABGR10.
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB10));
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ABGR10));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_NV12));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_YUV444));
        // Blend mode must unpack this channel order; a swap tints the pointer.
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

    /// What `resolve_split_mode` actually reads (`query_caps` latch).
    fn self_engines(enc: &NvencCudaEncoder) -> u32 {
        enc.encoder_engines
    }

    /// NV12 with real entropy. Driver-zeroed VRAM under CBR emits ~300 B/AU against an 833 KB
    /// quota, so timings measure only pixel-proportional cost. `block=1` is incompressible
    /// (RC overshoots); larger `block` is the only way to reach the low bits/frame end.
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
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    fn nv12_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        // Uninit VRAM: session/RFI machinery, not picture fidelity.
        let buf = DeviceBuffer::alloc_nv12(w, h).expect("alloc NV12 device buffer");
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    /// Hardware: GUID probe. Every NVENC encodes H.264 — `h264 = false` means enumeration is
    /// broken. Asserted on the uncached fn (the cache would make stability vacuous).
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

        // Warm-up: 8 frames, wire indices 0..7.
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

        // In-DPB range (RFI_DPB=5 ⇒ 3..=7 live). Must be real RFI, not an IDR fallback.
        assert!(
            enc.invalidate_ref_frames(5, 6),
            "invalidate_ref_frames should succeed for an in-DPB range"
        );

        // Re-anchor AU: `recovery_anchor`, not a forced IDR.
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

    /// Packed `X2Rgb10` (NVENC `ARGB10`, no host CSC). Uninit VRAM: session machinery, not
    /// picture fidelity.
    fn rgb10_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        let buf = DeviceBuffer::alloc(w, h).expect("alloc packed RGB device buffer");
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    /// Hardware: packed 10-bit → `ARGB10`. Bit depth and HDR must be derived from the input,
    /// not merely requested — that pair selects Main10 / BT.2020 PQ.
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
            // Dump for out-of-band ffprobe. In-tree we only see the encoder's own config.
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
            // Depth + HDR came from the input format.
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

    /// Hardware: cursor blend on a 10-bit packed slot. An 8-bit fallback would tint the
    /// pointer. Blend correctness is display-referred — not asserted here.
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
            true, // Vulkan slot ring + 10-bit blend
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
            // Serial flip at frame 3 (upload quiesce); position moves every frame.
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

    /// Hardware: HEVC FREXT YUV444 (stacked-plane copy NV12 does not exercise).
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
                provenance: Default::default(),
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

    /// Hardware: in-place rate retarget up and down must not emit an IDR.
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

    /// Hardware: can `splitEncodeMode` move in place (`resetEncoder=0`) without an IDR?
    /// Sub-frame off — HEVC forced-split + sub-frame is unsupported, which would reject for
    /// the wrong reason. Reports the verdict; asserts only that the measurement is valid.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_reconfigure_in_place() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Sub-frame off; open split-disabled so the switch is a real change.
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

        // Session is lazy; reconfigure while `!inited` short-circuits to `true`.
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

        // Forced-2 on a 1-engine GPU is rejected for the wrong reason.
        // SAFETY: live session (`inited`); `get_cap` returns 0 on driver error.
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
        // `resolve_split_mode` reads the latched field, not the live cap.
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

        // Change only `splitEncodeMode`.
        enc.split_mode = two;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1: reconfigure DISABLE→TWO_FORCED accepted = {accepted}");

        let verdict = if !accepted {
            // Live session is still split-disabled — keep the field truthful.
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

        // Reverse only if the forward change was accepted.
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

    /// Hardware: did an accepted in-place split actually take effect? A = fresh DISABLE, B =
    /// fresh TWO_FORCED, C = DISABLE→TWO in place. C ≈ B ⇒ real; C ≈ A ⇒ ignored.
    /// Bytes/AU are load-bearing: uninit VRAM under CBR can collapse every leg.
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
        /// Post-switch discard. Split does not reach steady state on frame 0; 16 was enough
        /// for the switched leg to match a fresh TWO_FORCED session.
        const SETTLE: u32 = 16;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");

        // Rotated buffers do not help: the driver still returns zeroed VRAM. This harness
        // measures pixel-proportional cost only.
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // (early p50 µs, late p50 µs, median B/AU).
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

            // Same measured length; a switched leg starts `SETTLE` frames later.
            let measure_from = if switch_to.is_some() {
                WARMUP + SETTLE
            } else {
                WARMUP
            };
            let (mut times, mut sizes) = (Vec::new(), Vec::new());
            for i in 0..(measure_from + MEASURED) {
                // In-place switch once, after warmup.
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
            // Early vs late: a whole-window median of a settling switch lands between the arms.
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

    /// Hardware: can `(split, sub-frame)` move as a pair in place, IDR-free?
    /// `reconfigure_bitrate` does not recompute `subframe_chunks` — a caller flipping
    /// sub-frame must clear that latch or `poll_chunk` busy-polls.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_subframe_pair_reconfigure() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Split-disabled; sub-frame at the caps-gated default.
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

        // Clear the chunked-poll latch with the sub-frame flag, or `poll_chunk` outlives it.
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

            // Reverse pair (de-escalation).
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

    /// Hardware: does plain AUTO + default-on sub-frame actually split? HEVC split is
    /// unsupported with sub-frame, so AUTO may mean "never split". Time AUTO vs DISABLE vs
    /// TWO_FORCED at 4K (pixel-proportional; VRAM is zeroed).
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

        // `split: None` = unset = plain AUTO. Env `1` is AUTO_FORCED, which disarms sub-frame.
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

        // Unset env: 4K60 8-bit is below SPLIT_FORCE_PIXEL_RATE → plain AUTO. Sub-frame must
        // stay on or this is not the fleet shape.
        let (auto_us, auto_sub) = run(None, None);
        let (dis_us, dis_sub) = run(Some("0"), None);
        let (two_us, two_sub) = run(Some("2"), Some("0"));
        // AUTO with sub-frame off: retiring AUTO would also change that shape.
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

    /// Hardware: split ceiling. A refused mode falls back to DISABLE, not an error. Timing
    /// tells whether an accepted mode actually used more engines.
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

        // (opened mode, p50 µs, engines).
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
            // SAFETY: live session; `get_cap` returns 0 on driver error.
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
        // Env `0` selects DISABLE (enum 15), not the integer 0.
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

    /// Hardware: live split arbitration at 4K. Sub-frame off so the no-trade gate arms.
    /// Must settle with zero extra IDRs and cache a splitting arm.
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
        // Measure + settle + measure, with slack.
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
        // 4K60 is under SPLIT_FORCE_PIXEL_RATE (AUTO vs widest). Single-engine must not win.
        println!(
            "  (incumbent was the static rule's choice; challenger was mode {})",
            max_forced_split_mode(enc_engines)
        );

        remove_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        // Process-global cache would steer later tests that open this config with split unset.
        super::super::nvenc_core::clear_split_verdicts();
    }

    /// Hardware: Main10 split A/B (packed RGB10). Sub-frame off. Reports; both outcomes
    /// are legitimate. `PF_AB_MODE` can retarget the operating point.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 vs Blackwell .21)"]
    fn nvenc_cuda_main10_split_ab() {
        use std::time::Instant;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 12;
        const MEASURED: u32 = 32;
        // `PF_AB_MODE=WxHxFPS` retargets; default 4K60.
        let (w, h, fps) = std::env::var("PF_AB_MODE")
            .ok()
            .and_then(|s| {
                let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((3840, 2160, 60));

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Packed 10-bit: `bit_depth`/`hdr` are derived from the input, not the args.
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

    /// Hardware: bits/frame curve. Zeroed VRAM only measures pixel-proportional cost.
    /// [`noise_nv12_frame`] supplies entropy. Print B/AU next to every timing.
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
        // Sweep spatial detail, not nominal bitrate. Pure noise overshoots any low target;
        // the x-axis is bits/frame actually produced.
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

    /// Pre-session / nonsense RFI declines. Skips if the NVENC `.so` is absent.
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
        // Lazy init: no session yet.
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

    /// Hardware: cycle codecs in one process; every leg must open and encode.
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

    /// Hardware: drop with encodes in flight, then a fresh session must still open.
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
            drop(enc); // pending encodes still in flight
        }
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0)
            .expect("reopen after dirty teardowns");
        while enc.poll().expect("poll").is_some() {}
        println!("nvenc_cuda dirty-teardown: 3 dirty drops, reopen clean");
    }

    /// Hardware: exhaust the concurrent-session cap, assert open fails, free slots, rebuild
    /// in place and produce an AU.
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
            // SAFETY: live params / out-param across the sync call.
            let st = unsafe { (api().open_encode_session_ex)(&mut params, &mut enc) };
            (st, enc)
        };

        // Hold sessions until open fails.
        let mut held = Vec::new();
        loop {
            let (st, enc) = open_raw(shared);
            if st != nv::NVENCSTATUS::NV_ENC_SUCCESS {
                if !enc.is_null() {
                    // SAFETY: destroy failed-open residue (NVENC docs).
                    unsafe {
                        let _ = (api().destroy_encoder)(enc);
                    }
                }
                break;
            }
            held.push(enc);
        }
        assert!(!held.is_empty(), "expected a finite session cap");

        // Caps-probe open must fail while the cap is exhausted.
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        let err = enc
            .submit_indexed(&frame, 0)
            .expect_err("submit must fail while the cap is exhausted");
        println!("at-cap error (self-diagnosis logged alongside): {err:#}");

        // Slots freed → same encoder rebuilds in place.
        for e in held {
            // SAFETY: successful raw open; destroy once.
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

    /// Hardware: stream-ordered submit must arm on a default-env session. A silent fallback
    /// still encodes — no other test would notice.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_stream_ordered_arms() {
        const W: u32 = 640;
        const H: u32 = 360;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Operator opt-out / two-thread mode: skip, don't fail.
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

    /// Hardware: cursor frames stay on the stream-ordered path (`blend_ref_ordered`, ticket
    /// +2 per frame), including a bitmap change and per-frame moves.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_cursor_blend_stream_ordered() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Operator opt-out / two-thread mode: skip, don't fail.
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
            true, // Vulkan slot ring + blend
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
            // Serial flip at frame 3 (upload quiesce); position moves every frame.
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

    /// Hardware: `set_pipelined(true)` rebuilds without IO-stream binding, spawns the
    /// retrieve thread, keeps delivering AUs. First post-escalation AU is the re-open IDR.
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
        // Bounded drain of the pipelined tail.
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

    /// Hardware: do slices become readable mid-encode? Prints a doNotWait timeline; asserts
    /// only that 4 slices materialize. `--test-threads=1`.
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

        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit opening frame");
        enc.poll().expect("poll").expect("opening AU");

        // Spin doNotWait against the in-flight bitstream before the blocking poll.
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
            // SAFETY: live session; `bs` is the just-submitted bitstream. Unlock a successful
            // lock before the next iteration. `reportSliceOffsets` armed; ≤ 32 offsets.
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
            // Complete = 4 slices. LOCK_BUSY = still encoding. 50 ms safety window.
            if (status == nv::NVENCSTATUS::NV_ENC_SUCCESS && n >= 4) || t_us > 50_000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        println!("subframe probe timeline (t_us, status, numSlices, bytes):");
        for (t, st, n, b) in &timeline {
            println!("  {t:>7} us  {st:?}  slices={n}  bytes={b}");
        }
        // Normal poll: probe locks must not have corrupted the session.
        let au = enc.poll().expect("poll probed frame").expect("probed AU");
        assert!(!au.data.is_empty(), "probed AU must carry data");
        let last = timeline.last().expect("at least one sample");
        assert_eq!(
            last.2, 4,
            "4 slices must materialize (PUNKTFUNK_NVENC_SLICES=4 + subframe readback armed)"
        );
        // One more frame — session still healthy.
        let frame = nv12_frame(W, H, 2);
        enc.submit_indexed(&frame, 2).expect("submit follow-up");
        enc.poll().expect("poll").expect("follow-up AU");
    }

    /// Annex-B NAL start code.
    fn starts_with_start_code(d: &[u8]) -> bool {
        d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
    }

    /// Hardware: chunked poll at defaults (4 slices + sub-frame). First/last metadata, Annex-B
    /// cuts, shadow reassembly. At least one multi-chunk frame. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_chunked_poll_end_to_end() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // Defaults under test — no leaked knobs.
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

        // A drained chunked AU leaves `poll()` usable.
        let frame = nv12_frame(W, H, 6);
        enc.submit_indexed(&frame, 6)
            .expect("submit plain-poll frame");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(!au.data.is_empty());
    }

    /// Hardware: client `max_slices=1` must encode single-slice with chunked poll disarmed.
    /// No env knobs. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_single_slice_client_ceiling() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // Negotiated ceiling, not the operator override.
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
            1, // client never advertised multi-slice
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

    /// Hardware: `PUNKTFUNK_NVENC_SLICES=1` disarms chunked poll; `poll_chunk` is one
    /// self-closing whole-AU chunk. `--test-threads=1`.
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

        // Explicit single slice — no boundaries.
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

        // Sub-frame vetoed: slices stay, chunked poll disarms, plain `poll` carries.
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
