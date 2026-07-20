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
//! **Two-thread retrieve** (`PUNKTFUNK_NVENC_ASYNC=1`, the same opt-in knob as the Windows
//! backend — gpu-contention plan §5.B, latency plan T2.2): NVENC *async mode*
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
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise); compiles GPU-less and
//! starts driver-less (the `.so` resolves at runtime — on an AMD/Intel box [`try_api`] fails cleanly
//! and the VAAPI/software backends carry the session).

// Every `unsafe` block / impl in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::nvenc_core::{
    apply_low_latency_config, build_init_params, codec_guid, LowLatencyConfig, NvStatusExt, RFI_DPB,
};
use super::nvenc_status;
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pf_zerocopy::cuda::{self, InputSurface};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// Prebuilt PTX for the cursor-overlay blend kernels (cursor-as-metadata). Source is
/// `cursor_blend.cu` beside this file; regenerate with
/// `nvcc -ptx -arch=compute_75 cursor_blend.cu -o cursor_blend.ptx` after editing. JIT'd by the
/// driver, so it runs on any Turing-or-newer GPU.
const CURSOR_PTX: &[u8] = include_bytes!("cursor_blend.ptx");

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

/// Output bitstream buffers = max in-flight encodes; equals the input-surface ring depth. Must
/// stay ≥ the two-thread retrieve's in-flight cap ([`async_inflight_cap`], ≤ `POOL - 1`) so a
/// bitstream/ring slot is never reused mid-encode.
const POOL: usize = 8;

/// Whether the operator asked for the two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC` truthy — the
/// SAME knob as the Windows backend, so one env drives the split on either host OS). Opt-in
/// until on-glass validated. Unlike Windows this changes NO session parameter (Linux stays sync
/// mode; only the blocking lock moves off the encode thread), so there is no async-rejecting
/// config to fail the open.
fn async_retrieve_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_ASYNC")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Max encodes in flight in two-thread mode (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4, clamped
/// `2..=POOL-1` — a bitstream must never be reused mid-encode, and the input ring is the same
/// depth). Mirrors the Windows knob exactly.
fn async_inflight_cap() -> usize {
    std::env::var("PUNKTFUNK_NVENC_ASYNC_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, POOL - 1)
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
    /// Frames submitted over the encoder's lifetime (never reset, unlike `next`) — drives the
    /// sampled `PUNKTFUNK_PERF` submit-split log cadence, mirroring the VAAPI backend's counter.
    frames: u64,
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
    /// Cursor-as-metadata GPU blend (loaded lazily on the first frame that carries a cursor, once the
    /// CUDA context is current). `None` until then or if the module load fails; `cursor_tried` stops
    /// re-attempting a failed load every frame. `cursor_serial` tracks the uploaded bitmap.
    cursor: Option<cuda::CursorBlend>,
    cursor_tried: bool,
    cursor_serial: u64,
    /// Suppress-until-success latches for the per-frame cursor upload/blend warns: a persistent
    /// failure sits in the submit() hot path, so warn once per failure streak (reset on success)
    /// rather than on every cursor-bearing frame, which would evict the log ring.
    cursor_upload_warned: bool,
    cursor_blend_warned: bool,
    /// One-shot latch for [`diagnose_failed_open`](Self::diagnose_failed_open) so a rebuild-retry
    /// burst (the session loop's bounded encoder resets) logs the diagnosis once, not per attempt.
    diagnosed: bool,
    /// The two-thread retrieve runtime (`PUNKTFUNK_NVENC_ASYNC`) — `None` in the default
    /// single-thread mode and between sessions. Exists only `init_session`→`teardown`.
    async_rt: Option<AsyncRetrieve>,
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
        _format: pf_frame::PixelFormat,
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
            // An HDR (GNOME 50 portal) session never reaches this backend: its X2RGB10 frames ride
            // the CPU/dmabuf paths (no CUDA import for the 10-bit formats yet), so the dispatcher
            // opens the libav P010 path instead. Reaching here 10-bit means a CUDA capture payload
            // on a 10-bit session — not wired; encode 8-bit rather than mislabel.
            tracing::warn!(
                "Linux direct-NVENC: 10-bit requested but the CUDA capture path has no 10-bit \
                 import yet (HDR rides the libav P010 path) — encoding 8-bit SDR"
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
            frames: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            pending_anchor: false,
            cursor: None,
            cursor_tried: false,
            cursor_serial: u64::MAX,
            cursor_upload_warned: false,
            cursor_blend_warned: false,
            diagnosed: false,
            inited: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32,
            last_rfi_range: None,
            async_rt: None,
        })
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
            subframe_readback = subframe != 0,
            dynamic_slice = dyn_slice != 0,
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
        // low-latency contract. On Linux the full-chroma input is a YUV444 surface and the input is
        // 8-bit today, so AV1's input-depth is 0.
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
                av1_input_depth_minus8: 0,
                hdr: self.hdr,
                rfi_supported: self.rfi_supported,
            },
        );
        Ok(cfg)
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
        // live pitched CUDA allocation on the shared context. No handle escapes the encode thread.
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
                    .map_err(|e| nvenc_status::call_err("create_bitstream_buffer", e))?;
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
                    .map_err(|e| nvenc_status::call_err("register_resource (CUDADEVICEPTR)", e))?;
                self.ring.push(RingSlot {
                    surface,
                    reg: rr.registeredResource,
                });
            }

            self.inited = true;
            // Two-thread retrieve (T2.2): spawn the lock thread against the live session. No
            // session parameter differs — teardown/rebuild always stops it before destroy.
            if async_retrieve_requested() {
                self.async_rt = Some(AsyncRetrieve::spawn(self.encoder as usize));
                tracing::info!(
                    depth = async_inflight_cap(),
                    "NVENC two-thread retrieve enabled (submit thread + blocking-lock thread)"
                );
            }
            tracing::info!(
                mode = %format_args!("{}x{}@{}", self.width, self.height, self.fps),
                bit_depth = self.bit_depth,
                mbps = self.bitrate_bps / 1_000_000,
                codec = ?self.codec_guid,
                fmt = ?self.buffer_fmt,
                "NVENC CUDA session ready"
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

    /// Fold one retrieve-thread completion into `ready` (two-thread mode only): pop the oldest
    /// in-flight entry, cross-check FIFO pairing, unmap its input HERE (the encode thread — the
    /// retrieve thread never touches input resources), and queue the finished AU.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some((bs, map, pts_ns, anchor)) = self.pending.pop_front() else {
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
        let t0 = std::time::Instant::now();

        // Copy the captured buffer into this slot's input surface before encoding it.
        self.copy_into_slot(buf, slot)?;
        let t_copy = t0.elapsed();

        // Cursor-as-metadata: blend the overlay into this slot's OWNED input surface (a tiny kernel
        // over the cursor's rect — never the compositor's dmabuf). The PTX module loads lazily on the
        // first cursor frame now that the CUDA context is current; any failure degrades to no cursor,
        // never a dropped frame.
        if let Some(ov) = &captured.cursor {
            if !self.cursor_tried {
                self.cursor_tried = true;
                match cuda::CursorBlend::new(CURSOR_PTX) {
                    Ok(cb) => self.cursor = Some(cb),
                    Err(e) => tracing::warn!(
                        error = %format!("{e:#}"),
                        "NVENC (Linux): cursor blend module load failed — cursor not composited"
                    ),
                }
            }
            if let Some(cb) = &self.cursor {
                if self.cursor_serial != ov.serial {
                    match cb.upload(ov.rgba.as_slice(), ov.w, ov.h) {
                        Ok(()) => {
                            self.cursor_serial = ov.serial;
                            self.cursor_upload_warned = false;
                        }
                        Err(e) => {
                            if !self.cursor_upload_warned {
                                self.cursor_upload_warned = true;
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    serial = ov.serial,
                                    "NVENC (Linux): cursor upload failed — cursor not composited"
                                );
                            }
                        }
                    }
                }
                let s = &self.ring[slot].surface;
                // surfW = content width; surfH = the surface's allocated height (matches
                // `copy_into_slot`'s plane math). Cursor pixels past the content are in cropped
                // padding rows — harmless.
                let (w, h) = (self.width, s.height);
                let r = match self.buffer_fmt {
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => {
                        cb.blend_yuv444(s.ptr, s.pitch, w, h, ov.w, ov.h, ov.x, ov.y)
                    }
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                        cb.blend_nv12(s.ptr, s.pitch, w, h, ov.w, ov.h, ov.x, ov.y)
                    }
                    _ => cb.blend_argb(s.ptr, s.pitch, w, h, ov.w, ov.h, ov.x, ov.y),
                };
                if let Err(e) = r {
                    if !self.cursor_blend_warned {
                        self.cursor_blend_warned = true;
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "NVENC (Linux): cursor blend launch failed — cursor not composited"
                        );
                    }
                } else {
                    self.cursor_blend_warned = false;
                }
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
        // just filled by the (synchronized) device→device copy and is not overwritten until this slot
        // is reused POOL submits later, by which time this encode was polled (POOL ≥ in-flight depth).
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
            ));
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
                reInitEncodeParams: build_init_params(
                    self.codec_guid,
                    self.width,
                    self.height,
                    self.fps,
                    &mut cfg,
                    self.split_mode,
                    false,
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
}
