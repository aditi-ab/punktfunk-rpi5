//! Direct-SDK NVENC encoder (Windows, D3D11 input): zero-copy capture→encode on the GPU.
//!
//! Raw `nvEncodeAPI` through a runtime-loaded [`EncodeApi`]. The crate's `ENCODE_API` /
//! safe `Encoder` stay unused: they are CUDA-only and their static entry points would
//! import `nvEncodeAPI64.dll` at load on the all-vendor binary. Sibling of
//! `encode/linux/nvenc_cuda.rs`. Design: `design/linux-direct-nvenc.md`; recovery:
//! `encoder-recovery-hardening.md`.
//!
//! The session binds the DXGI capturer's `ID3D11Device` and registers each input texture
//! once (cached by pointer); `encode_picture` uses it in place. That holds while the host
//! loop is capture → submit → poll. A pipelined loop must hand a texture ring. Config
//! matches Linux NVENC: CBR + ULL, infinite GOP, P-frames only, forced-IDR for RFI,
//! in-band SPS/PPS.
//!
//! Two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC=1`): the encode thread submits; a retrieve
//! thread waits on per-buffer events and `nvEncLockBitstream`. `submit` blocks on the
//! oldest completion when `POOL - 1` encodes are in flight. Register/map/unmap stay on
//! the encode thread. The DLL resolves at runtime, so an AMD/Intel box fails [`try_api`]
//! and AMF/QSV/`super::sw` carry the session.

// `unsafe_op_in_unsafe_fn` off: this file is raw NVENC/D3D11 calls. Wrapping each one
// would add a SAFETY that only restates the prototype. Exit: delete the empty markers.
#![allow(unsafe_op_in_unsafe_fn)]

use super::nvenc_core::{
    apply_low_latency_config, build_init_params, cached_ceiling, codec_guid, plan_range_recovery,
    resolve_slices, resolve_split_subframe, resolve_subframe, store_ceiling, subframe_env_forced,
    CeilingKey, LowLatencyConfig, NvStatusExt, RangePlan,
};
// Shared with the libav path (builds without the `nvenc` feature). Do not fork this copy.
use super::nvenc_core::{
    cached_split_verdict, store_split_verdict, ArbAction, SplitArbiter, SplitKey,
};
use super::nvenc_status;
use super::{max_forced_split_mode, resolve_split_mode};
use super::{AuChunk, ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

// Runtime-loaded NVENC entry table. A link-time import of `nvEncodeAPI64.dll` would
// refuse to start on AMD/Intel before `main`. Only the two DLL exports resolve by
// name; `NvEncodeAPICreateInstance` fills the rest.

/// NVENC entry table unwrapped at load. Do not touch the sdk crate's `EncodeAPI` lazy static:
/// it calls the statically-declared externs and would demand the import lib at link time.
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
    // Driver GUID list for [`probe_codec_support`]. Missing entries fail the table load.
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
    register_async_event:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_EVENT_PARAMS) -> nv::NVENCSTATUS,
    unregister_async_event:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_EVENT_PARAMS) -> nv::NVENCSTATUS,
    invalidate_ref_frames: unsafe extern "C" fn(*mut c_void, u64) -> nv::NVENCSTATUS,
}

/// Resolve the table once per process. `Err` = no NVIDIA driver/DLL or a driver older than our
/// headers. [`NvencD3d11Encoder::open`] and [`probe_can_encode_444`] gate on it.
fn try_api() -> std::result::Result<&'static EncodeApi, &'static str> {
    static TABLE: std::sync::OnceLock<std::result::Result<EncodeApi, String>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let table = load_api();
            if let Err(e) = &table {
                // Misdetect, or `PUNKTFUNK_ENCODER=nvenc` without a driver.
                tracing::warn!(error = %e, "NVENC API unavailable");
            }
            table
        })
        .as_ref()
        .map_err(|e| e.as_str())
}

/// Loaded table for call sites past a [`try_api`] gate. Lives for the process lifetime.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

fn load_api() -> std::result::Result<EncodeApi, String> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names;
    // `LOAD_LIBRARY_SEARCH_SYSTEM32` excludes a planted DLL. The transmutes are the
    // `nvEncodeAPI.h` prototypes. `GetMaxSupportedVersion` writes one u32; `CreateInstance`
    // fills `list` (version set) only during the call. The module is never freed.
    unsafe {
        let module = LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("nvEncodeAPI64.dll not loadable (no NVIDIA driver?): {e}"))?;
        let get_version = GetProcAddress(module, s!("NvEncodeAPIGetMaxSupportedVersion"))
            .ok_or("nvEncodeAPI64.dll exports no NvEncodeAPIGetMaxSupportedVersion")?;
        let create_instance = GetProcAddress(module, s!("NvEncodeAPICreateInstance"))
            .ok_or("nvEncodeAPI64.dll exports no NvEncodeAPICreateInstance")?;
        let get_version: unsafe extern "C" fn(*mut u32) -> nv::NVENCSTATUS =
            std::mem::transmute(get_version);
        let create_instance: unsafe extern "C" fn(
            *mut nv::NV_ENCODE_API_FUNCTION_LIST,
        ) -> nv::NVENCSTATUS = std::mem::transmute(create_instance);

        let mut version = 0u32;
        get_version(&mut version)
            .nv_ok()
            .map_err(|e| format!("NvEncodeAPIGetMaxSupportedVersion: {e:?}"))?;
        // Same check as the sdk's `assert_versions_match`, but an older driver is `Err`, not a panic.
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
        Ok(EncodeApi {
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
            register_async_event: list.nvEncRegisterAsyncEvent.ok_or(MISSING)?,
            unregister_async_event: list.nvEncUnregisterAsyncEvent.ok_or(MISSING)?,
            invalidate_ref_frames: list.nvEncInvalidateRefFrames.ok_or(MISSING)?,
        })
    }
}

// Max in-flight encodes. Must be ≥ `PUNKTFUNK_ENCODE_DEPTH` (default 4, clamped ≤ 6) so GPU
// scheduling waits overlap instead of serializing.
const POOL: usize = 8;

/// Live NVENC session units in this process (plain = 1; forced split = one per engine, 2–3).
/// Admission reads this same counter; other processes are invisible, so we fail closed on our own.
static LIVE_SESSION_UNITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Concurrent-session budget (GeForce 8; pro cards unlimited).
/// `PUNKTFUNK_NVENC_MAX_SESSIONS` overrides.
fn session_cap() -> u32 {
    std::env::var("PUNKTFUNK_NVENC_MAX_SESSIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

/// Whether one more plain (non-split) session fits. AMD/Intel never open NVENC so this passes.
pub(crate) fn can_open_another_session() -> bool {
    LIVE_SESSION_UNITS.load(std::sync::atomic::Ordering::Relaxed) < session_cap()
}

fn split_mode_units(split_mode: u32) -> u32 {
    match split_mode {
        m if m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE as u32 => 3,
        m if m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
            || m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32 =>
        {
            2
        }
        _ => 1,
    }
}

/// Serializes every `nvEncOpenEncodeSessionEx` against [`reap_parked_sessions`]. Overlapping a
/// reap could recycle a zombie's address onto a new session, and the reap would destroy the live
/// one. Held across `init_session` and the standalone probes; admission never takes it.
static DRIVER_SESSION_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Session whose `destroy_encoder` failed ambiguously ([`nvenc_status::destroy_proves_no_session`]).
/// Units stay charged in [`LIVE_SESSION_UNITS`] until [`reap_parked_sessions`] proves the slot free.
struct ParkedSession {
    enc: usize,
    units: u32,
    /// Pins the D3D11 device the session opened against. Teardown drops the texture refs; without
    /// this, a later reap `destroy_encoder` could touch a freed device.
    _device: Option<ID3D11Device>,
}

// SAFETY: the COM ref is the only non-Send field. Capture creates the D3D11 device without
// `D3D11_CREATE_DEVICE_SINGLETHREADED` (pf-frame/src/dxgi.rs), we never call methods on it, and
// Release of a free-threaded COM object from another thread is sound. `enc` is only passed to
// `destroy_encoder` under [`PARKED`] with [`DRIVER_SESSION_GATE`] held.
unsafe impl Send for ParkedSession {}

static PARKED: std::sync::Mutex<Vec<ParkedSession>> = std::sync::Mutex::new(Vec::new());

/// Park a session whose destroy failed ambiguously. Units stay charged until a reap
/// proves the slot free. Age out the oldest entry (and refund) if parked units would
/// exceed the session cap.
fn park_session(enc: usize, units: u32, device: Option<ID3D11Device>) {
    let mut parked = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    while !parked.is_empty() && parked.iter().map(|z| z.units).sum::<u32>() + units > session_cap()
    {
        let old = parked.remove(0);
        LIVE_SESSION_UNITS.fetch_sub(old.units, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            enc = old.enc,
            units = old.units,
            "NVENC parked-session graveyard full — aging out the oldest entry (units refunded)"
        );
    }
    parked.push(ParkedSession {
        enc,
        units,
        _device: device,
    });
}

/// Retry `destroy_encoder` on parked sessions and refund units that now succeed (or prove gone).
/// Runs from `init_session` on the encode thread, never from admission (a wedged driver would
/// block the admission lock).
///
/// # Safety
/// Caller holds [`DRIVER_SESSION_GATE`] and no live NVENC session exists (checked: live units ==
/// parked units), so a recycled handle cannot alias a live session mid-reap. Residual, unprovable
/// from the SDK: a failed destroy leaves the handle intact for retry; NVENC documents neither way.
unsafe fn reap_parked_sessions() {
    let mut parked = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    if parked.is_empty() {
        return;
    }
    let parked_units: u32 = parked.iter().map(|z| z.units).sum();
    if LIVE_SESSION_UNITS.load(std::sync::atomic::Ordering::Relaxed) != parked_units {
        return; // a live session exists — its address space is off limits
    }
    parked.retain(
        |z| match (api().destroy_encoder)(z.enc as *mut c_void).nv_ok() {
            Ok(()) => {
                tracing::info!(
                    enc = z.enc,
                    units = z.units,
                    "NVENC parked session reclaimed — retry-destroy succeeded, budget refunded"
                );
                LIVE_SESSION_UNITS.fetch_sub(z.units, std::sync::atomic::Ordering::Relaxed);
                false
            }
            Err(e) if nvenc_status::destroy_proves_no_session(e) => {
                tracing::info!(
                    enc = z.enc,
                    units = z.units,
                    status = ?e,
                    "NVENC parked session gone on the driver side — budget refunded"
                );
                LIVE_SESSION_UNITS.fetch_sub(z.units, std::sync::atomic::Ordering::Relaxed);
                false
            }
            Err(e) => {
                tracing::debug!(enc = z.enc, status = ?e,
                    "NVENC parked session still refuses destroy — units stay charged");
                true
            }
        },
    );
}

/// Operator asked for two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC` truthy). Combined with
/// `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT` in `init_session`. An async-rejecting config fails the open.
fn async_retrieve_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_ASYNC")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Max in-flight encodes in async mode (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4,
/// clamped `2..=POOL-1`). Never reuse a bitstream mid-encode (`POOL-1`). Encode is
/// in-place, so depth past the capturer ring overwrites a live frame. Read once
/// per process; not latched on the session because `input_ring_depth` can change.
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

/// In-flight encode for the retrieve thread. Pointers travel as `usize` (process-global driver
/// handles); the thread is joined before the session is destroyed.
struct RetrieveJob {
    bs: usize,
    event: usize,
}

/// Finished retrieve (AU or error). `bs` lets the encode thread check FIFO pairing with `pending`.
struct RetrieveDone {
    bs: usize,
    result: std::result::Result<(Vec<u8>, bool), String>,
}

/// Async retrieve: job/done channels, the thread (joined in `teardown` before session destroy),
/// and AUs backpressure already absorbed that `poll` hands out first.
struct AsyncRetrieve {
    work_tx: Option<mpsc::SyncSender<RetrieveJob>>,
    done_rx: mpsc::Receiver<RetrieveDone>,
    join: Option<std::thread::JoinHandle<()>>,
    ready: VecDeque<EncodedFrame>,
}

/// Retrieve thread: wait on each job's completion event, lock/copy/unlock, send back.
/// Exits when the job channel closes. Teardown drops the sender and joins before
/// destroying the session, so `enc`/`bs`/`event` outlive every use. Touches only wait + lock/unlock.
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // After one 5 s timeout, later jobs wait 250 ms. Teardown must drain every queued
    // job (abandoning would destroy/unmap while encoding); the first timeout is already
    // encoder-fatal, so this only shortens teardown. A successful wait resets the latch.
    const WEDGED_DRAIN_WAIT_MS: u32 = 250;
    let mut wedged = false;
    while let Ok(job) = work_rx.recv() {
        let wait_ms = if wedged { WEDGED_DRAIN_WAIT_MS } else { 5000 };
        // SAFETY: `job.event` is an auto-reset event `init_session` registered; `job.bs` is
        // a pool bitstream. Both stay valid until `teardown` joins this thread first.
        // On WAIT_OBJECT_0 the encode is done, so `lock_bitstream` (version set) yields a
        // pointer valid until `unlock_bitstream`; the slice is copied first. Secondary-thread
        // lock/unlock while the encode thread submits is the documented NVENC model.
        let result = unsafe {
            if WaitForSingleObject(HANDLE(job.event as *mut c_void), wait_ms) != WAIT_OBJECT_0 {
                wedged = true;
                Err(format!(
                    "NVENC completion event timeout ({wait_ms} ms) — encoder wedged?"
                ))
            } else {
                wedged = false;
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
                        let _ = (api().unlock_bitstream)(enc as *mut c_void, job.bs as *mut c_void);
                        Ok((data, keyframe))
                    }
                    Err(e) => Err(format!(
                        "lock_bitstream (async): {e:?} — {}",
                        nvenc_status::explain(e)
                    )),
                }
            }
        };
        if done_tx.send(RetrieveDone { bs: job.bs, result }).is_err() {
            break; // encoder gone; teardown drains us via join
        }
    }
}

pub struct NvencD3d11Encoder {
    encoder: *mut c_void,
    codec: Codec,
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    /// Encoded bit depth (8 or 10). 10 → HEVC Main10 (NVENC upconverts the 8-bit ARGB input).
    bit_depth: u8,
    /// Effective 4:4:4 (HEVC FREXT, `chroma_format_idc = 3`). NVENC CSCs RGB internally. Gated
    /// on `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` and an RGB input; NV12/P010 cannot reconstruct 4:4:4.
    chroma_444: bool,
    /// Negotiated 4:4:4, before per-init downgrade. `chroma_444` is the effective value and
    /// clears on subsampled YUV; keeping the request lets a later RGB re-init recover 4:4:4.
    chroma_444_requested: bool,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`. `chroma_444` is forced off when false.
    yuv444_supported: bool,
    /// Effective HDR (BT.2020 PQ 10-bit). Derived per-frame; a change re-inits the session.
    hdr: bool,
    /// HDR the capture format asks for. `hdr` is effective and `query_caps` may clear it; comparing
    /// against `hdr` on a no-10-bit GPU would rebuild the session every P010 frame.
    hdr_requested: bool,
    /// Latched when `query_caps` finds no 10-bit encode, so re-inits do not re-warn.
    hdr_unsupported: bool,
    /// Source mastering metadata, emitted as in-band SEI on each HDR keyframe. `None` = VUI only.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Capturer textures registered with NVENC, cached by pointer (in-place encode). The cloned
    /// `ID3D11Texture2D` keeps each alive until unregister — the capturer may drop its copy first.
    regs: HashMap<isize, (nv::NV_ENC_REGISTERED_PTR, ID3D11Texture2D)>,
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// Async: completion event per pool bitstream (`HANDLE` as `usize`); empty in sync. Closed in `teardown`.
    events: Vec<usize>,
    /// Async retrieve thread + channels. `None` = same-thread sync retrieve.
    async_rt: Option<AsyncRetrieve>,
    /// Capturer `pipeline_depth`. Encode is in-place, so this hard-caps async depth: the capturer
    /// rotates the ring regardless of encode completion. `None` = unknown, do not pipeline past the env cap.
    input_ring_depth: Option<usize>,
    async_supported: bool,
    /// `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK`. Default-on only when the GPU advertises it.
    subframe_cap: bool,
    /// `NV_ENC_CAPS_NUM_ENCODER_ENGINES` (`0` = unprobed). The driver accepts a split wider than
    /// the hardware and silently encodes narrower — this is the only honest width.
    encoder_engines: u32,
    /// Submit stamp for the split arbiter's per-frame cost (sync depth-1 path only).
    last_submit_at: Option<std::time::Instant>,
    /// Whole-AU paced-send time (µs) the host last reported. `0` = never reported, which keeps
    /// the arbiter out of the sub-frame trade it cannot otherwise price.
    send_spread_us: u32,
    /// Sub-frame as opened. Restored on a return to non-forced split; never enabled if it never was.
    subframe_opened_with: bool,
    arbiter: Option<SplitArbiter>,
    /// In-flight encodes: (bitstream, mapped input, pts_ns, recovery-anchor, idr-hint). The
    /// fourth field is the first frame after a successful [`invalidate_ref_frames`].
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64, bool, bool)>,
    /// Next submission's `inputTimeStamp`. [`Encoder::submit_indexed`] pins it to the
    /// wire index so RFI timestamps stay 1:1 across rebuilds.
    frame_idx: i64,
    force_kf: bool,
    /// Armed by a successful [`invalidate_ref_frames`]; the next `submit` consumes it so that AU
    /// is the recovery anchor. NVENC applies invalidation at the next `encode_picture`. Without
    /// the tag the client can only lift on an IDR, which session glue suppresses after RFI.
    pending_anchor: bool,
    inited: bool,
    /// From `query_caps`. Gates RFI instead of failing later as opaque `InvalidParam`.
    rfi_supported: bool,
    custom_vbv: bool,
    /// Split mode the live session opened with. `reconfigure_bitrate` must present the same init
    /// params (only rate fields may move).
    split_mode: u32,
    /// Sub-frame as opened ([`resolve_split_subframe`]). Reconfigure must not re-read the env —
    /// that could flip `enableSubFrameWrite` mid-session.
    subframe_on: bool,
    /// Slice count latched at open so reconfigure presents the same slicing. Chunked poll needs ≥ 2.
    slices: u32,
    max_slices: u32,
    /// Chunked poll armed (`slices ≥ 2` ∧ sub-frame ∧ sync retrieve). Async retrieve owns the
    /// bitstream; a doNotWait sampler here would race it.
    subframe_chunks: bool,
    /// Finish-lock prefix check saw sub-frame publish bytes the finished AU disowns.
    /// Later opens on this encoder resolve sub-frame off. Never cleared: a fresh encoder retests.
    subframe_broken: bool,
    chunk: Option<ChunkState>,
    session_async: bool,
    /// Last invalidated ref range. Dedupes the client's resends of the same loss event.
    last_rfi_range: Option<(i64, i64)>,
    /// D3D11 device this session opened against. Capturer recreates it on a desktop switch; a new
    /// device pointer tears down and re-inits.
    init_device: *mut c_void,
    /// COM ref pinning that device. `init_device` alone pins nothing, and `teardown` releases
    /// texture regs before destroy — a parked retry must not outlive the device. Moved into
    /// [`ParkedSession`] on that path.
    init_device_com: Option<ID3D11Device>,
    /// Units this encoder holds against [`LIVE_SESSION_UNITS`] (1 plain, 2–3 if split). `0` while closed.
    session_units: u32,
}

// SAFETY: the `!Send` fields are the NVENC session/device handles, bitstream/registered/
// mapped pointers, and `ID3D11Texture2D` COM refs. One thread owns the encoder (every
// method runs there). In async mode the retrieve thread only waits/lock/unlock — never
// registrations, mappings, or D3D11 — and `teardown` joins it first. The ownership
// move is sound because no NVENC/D3D11 call is in flight during it.
unsafe impl Send for NvencD3d11Encoder {}

/// doNotWait sample cadence. Slice completions land ~0.5–1 ms apart; 50 µs stays under one
/// slice time without hammering the driver.
const CHUNK_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_micros(50);

/// Chunked readback of the front in-flight AU. `Some` from the first emitted chunk until `last`;
/// [`Encoder::poll`] refuses while it exists (a whole-AU poll would re-emit the shipped prefix).
struct ChunkState {
    emitted: usize,
    slices_out: u32,
    opened: bool,
    /// Shadow of every emitted byte, compared to the finishing blocking lock's full AU. A driver
    /// whose doNotWait `bitstreamSizeInBytes` runs ahead of flushed slice bytes ships unwritten
    /// buffer; the wire stays self-consistent so no client counter moves. One AU-sized copy/compare.
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

impl NvencD3d11Encoder {
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        _format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
        // Client-decoder slice ceiling (`VIDEO_CAP_MULTI_SLICE` / GameStream slices-per-frame).
        // 1 = single-slice — the safe shape toward decoders that never asked (some SoCs wedge).
        max_slices: u32,
    ) -> Result<Self> {
        // DLL load is the real availability gate: fail open with a reason instead of an opaque
        // first-frame session error. Later NVENC calls sit behind this, so `api()` is sound.
        try_api().map_err(|e| anyhow!("NVENC unavailable: {e}"))?;
        Ok(Self {
            encoder: ptr::null_mut(),
            codec,
            codec_guid: codec_guid(codec),
            width,
            height,
            fps,
            bitrate_bps,
            buffer_fmt: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bit_depth,
            // 4:4:4 is HEVC-only; the GPU-support gate is in `query_caps`.
            chroma_444: chroma.is_444() && codec == Codec::H265,
            chroma_444_requested: chroma.is_444() && codec == Codec::H265,
            hdr_requested: false,
            hdr_unsupported: false,
            yuv444_supported: false,
            hdr: false,
            hdr_meta: None,
            regs: HashMap::new(),
            next: 0,
            bitstreams: Vec::new(),
            events: Vec::new(),
            async_rt: None,
            input_ring_depth: None,
            async_supported: false,
            subframe_cap: false,
            encoder_engines: 0,
            last_submit_at: None,
            send_spread_us: 0,
            subframe_opened_with: false,
            arbiter: None,
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            pending_anchor: false,
            inited: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32,
            subframe_on: false,
            slices: 1,
            max_slices: max_slices.max(1),
            subframe_chunks: false,
            subframe_broken: false,
            chunk: None,
            session_async: false,
            last_rfi_range: None,
            init_device: ptr::null_mut(),
            init_device_com: None,
            session_units: 0,
        })
    }

    /// Tear down the session and pooled resources. Reused on a capture-device change and at Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Retire the retrieve thread first: drop the job sender so it finishes queued jobs against
        // the still-live session, then join. Only then is unmap/destroy sound.
        if let Some(mut rt) = self.async_rt.take() {
            drop(rt.work_tx.take());
            if let Some(j) = rt.join.take() {
                let _ = j.join();
            }
            // Completions poll never absorbed. AUs drop with the session; pending unmap below covers maps.
            while rt.done_rx.try_recv().is_ok() {}
        }
        for (_, map, _, _, _) in &self.pending {
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, *map);
            }
        }
        for (reg, _tex) in self.regs.values() {
            let _ = (api().unregister_resource)(self.encoder, *reg);
        }
        for &ev in &self.events {
            let mut ep = nv::NV_ENC_EVENT_PARAMS {
                version: nv::NV_ENC_EVENT_PARAMS_VER,
                completionEvent: ev as *mut c_void,
                ..Default::default()
            };
            let _ = (api().unregister_async_event)(self.encoder, &mut ep);
            let _ = CloseHandle(HANDLE(ev as *mut c_void));
        }
        self.events.clear();
        for &bs in &self.bitstreams {
            let _ = (api().destroy_bitstream_buffer)(self.encoder, bs);
        }
        // Refund units only when the driver proves the slot free (success or a gone-session
        // status). Ambiguous failures park the handle with units still charged;
        // `reap_parked_sessions` retries once nothing is live.
        let dev_pin = self.init_device_com.take();
        match (api().destroy_encoder)(self.encoder).nv_ok() {
            Ok(()) => {
                LIVE_SESSION_UNITS
                    .fetch_sub(self.session_units, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) if nvenc_status::destroy_proves_no_session(e) => {
                tracing::warn!(
                    status = ?e,
                    "NVENC destroy_encoder failed, but the status proves the driver holds no \
                     session (device gone/reset) — budget refunded"
                );
                LIVE_SESSION_UNITS
                    .fetch_sub(self.session_units, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    status = ?e,
                    units = self.session_units,
                    "NVENC destroy_encoder failed ambiguously — the driver may still hold this \
                     session's slot; parking the handle (units stay charged until a reap-destroy \
                     proves the slot free)"
                );
                park_session(self.encoder as usize, self.session_units, dev_pin);
            }
        }
        self.session_units = 0;
        self.regs.clear();
        self.bitstreams.clear();
        self.pending.clear();
        self.chunk = None;
        self.subframe_chunks = false;
        self.encoder = ptr::null_mut();
        self.inited = false;
        self.next = 0;
        // Fresh session, empty DPB, first frame is an IDR. Prior RFI range and pending-anchor tag are stale.
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

    /// Probe GPU caps on a throwaway session before the bitrate-probe loop: max dimensions,
    /// 10-bit / custom-VBV / RFI. Rejects an over-range mode up front; without this an
    /// unsupported config is opaque `InvalidParam` that the clamp search treats as "bitrate too high".
    unsafe fn query_caps(&mut self, device: &ID3D11Device) -> Result<()> {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        if let Err(e) = (api().open_encode_session_ex)(&mut params, &mut enc).nv_ok() {
            // NVENC requires `NvEncDestroyEncoder` even after a failed open — the driver may have
            // allocated the session slot. Skipping it leaks slots toward the concurrent-session cap.
            if !enc.is_null() {
                let _ = (api().destroy_encoder)(enc);
            }
            return Err(nvenc_status::call_err(
                "open_encode_session_ex (caps probe)",
                e,
            ));
        }
        // Kernel-mode handshake succeeded: later `NV_ENC_ERR_INVALID_VERSION` is not driver skew.
        nvenc_status::note_session_opened();
        let wmax = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX);
        let hmax = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX);
        let ten_bit = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE);
        let yuv444 = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE);
        let rfi = self.get_cap(
            enc,
            nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
        );
        let custom_vbv = self.get_cap(
            enc,
            nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE,
        );
        let async_enc = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT);
        let subframe = self.get_cap(enc, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK);
        // Split-encode ceiling. Probe, don't infer from rejection: the driver accepts a split
        // wider than the hardware and silently encodes narrower (`max_forced_split_mode`).
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
        if self.bit_depth >= 10 && ten_bit == 0 {
            if !self.hdr_unsupported {
                tracing::warn!("NVENC: this GPU can't 10-bit encode — falling back to 8-bit SDR");
            }
            // Latch so `submit` compares against `hdr_requested`, not this cleared value.
            self.hdr_unsupported = true;
            self.bit_depth = 8;
            self.hdr = false;
        }
        // 4:4:4: no YUV444 encode → 4:2:0. `probe_can_encode_444` already gated the Welcome.
        self.yuv444_supported = yuv444 != 0;
        if self.chroma_444 && !self.yuv444_supported {
            tracing::warn!("NVENC: this GPU can't 4:4:4 encode — falling back to 4:2:0");
            self.chroma_444 = false;
        }
        self.rfi_supported = rfi != 0;
        self.custom_vbv = custom_vbv != 0;
        self.async_supported = async_enc != 0;
        self.subframe_cap = subframe != 0;
        self.encoder_engines = engines.max(0) as u32;
        tracing::info!(
            rfi = self.rfi_supported,
            custom_vbv = self.custom_vbv,
            async_encode = self.async_supported,
            subframe_readback = self.subframe_cap,
            max = %format!("{wmax}x{hmax}"),
            ten_bit = ten_bit != 0,
            "NVENC capabilities probed"
        );
        Ok(())
    }

    /// Session `NV_ENC_CONFIG` at `bitrate` (bps): P1/ULL preset plus the RC/tier/chroma/VUI/DPB
    /// this backend always runs. Shared by [`try_open_session`] and [`Encoder::reconfigure_bitrate`]
    /// so an in-place retarget moves only bitrate + derived VBV.
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

        // Shared low-latency contract. Windows full-chroma input is packed RGB (NVENC CSCs under
        // FREXT). AV1 input-depth follows the surface: 10-bit for ABGR10 / YUV420_10BIT, else 8.
        let rgb_input = matches!(
            self.buffer_fmt,
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
                | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
        );
        let ten_bit_in = matches!(
            self.buffer_fmt,
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
        );
        apply_low_latency_config(
            &mut cfg,
            LowLatencyConfig {
                codec: self.codec,
                bitrate,
                fps: self.fps,
                custom_vbv: self.custom_vbv,
                chroma_444: self.chroma_444,
                full_chroma_input: rgb_input,
                bit_depth: self.bit_depth,
                av1_input_depth_minus8: if ten_bit_in { 2 } else { 0 },
                hdr: self.hdr,
                rfi_supported: self.rfi_supported,
                // Latched at open so a later reconfigure re-presents the same slicing.
                slices: self.slices,
            },
        );
        Ok(cfg)
    }

    fn split_key(&self) -> SplitKey {
        // Render-adapter LUID, `0` when unresolved. Advisory: a collision costs one re-search.
        let gpu = pf_gpu::resolve_render_adapter_luid()
            .map(|l| ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64)
            .unwrap_or(0);
        SplitKey {
            gpu,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
        }
    }

    /// Move the live session to `mode` without an IDR. `nvEncReconfigureEncoder` accepts a
    /// changed `splitEncodeMode` with `resetEncoder=0` and emits no keyframe.
    fn apply_split_mode(&mut self, mode: u32) -> bool {
        let (prev_mode, prev_sub) = (self.split_mode, self.subframe_on);
        let (mode, subframe) = resolve_split_subframe(
            self.codec,
            mode,
            self.subframe_opened_with,
            subframe_env_forced(),
        );
        self.split_mode = mode;
        self.subframe_on = subframe;
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
                    self.arbiter = None;
                    return;
                }
            }
            Some(ArbAction::Settled(mode)) => store_split_verdict(self.split_key(), mode),
            None => {}
        }
        if done {
            store_split_verdict(self.split_key(), self.split_mode);
            self.arbiter = None;
        }
    }

    /// Arm a live split experiment. Same gates as Linux `arm_split_arbiter`; Windows also
    /// refuses when `async_rt` is set — the submit→AU span then includes queue depth.
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
        let handicap_us = if self.subframe_on && self.codec != Codec::Av1 {
            if self.send_spread_us == 0 || self.slices < 2 {
                return;
            }
            let slices = self.slices as u64;
            self.send_spread_us as u64 * (slices - 1) / slices
        } else {
            0
        };
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
            "NVENC split arbitration armed (Windows) — measuring both arms live (no IDR)"
        );
        self.arbiter = Some(SplitArbiter::with_handicap(
            self.split_mode,
            challenger,
            handicap_us,
        ));
    }

    /// Identity in the process-lifetime bitrate-ceiling cache. GPU is the render adapter LUID
    /// (`0` if unresolved). Advisory: a collision costs one failed open + re-search, never a wrong session.
    fn ceiling_key(&self, split_mode: u32) -> CeilingKey {
        let gpu = pf_gpu::resolve_render_adapter_luid()
            .map(|l| ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64)
            .unwrap_or(0);
        CeilingKey {
            gpu,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
            split_mode,
        }
    }

    /// Open + initialize one session at `bitrate` / `split_mode`. On failure, destroy and return
    /// the error. NVENC has no re-init after a failed `initialize_encoder`.
    unsafe fn try_open_session(
        &self,
        device: &ID3D11Device,
        bitrate: u64,
        split_mode: u32,
        enable_async: bool,
        subframe: bool,
    ) -> Result<*mut c_void> {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        if let Err(e) = (api().open_encode_session_ex)(&mut params, &mut enc).nv_ok() {
            // Destroy-on-failed-open: a failed open may still hold a session slot.
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
            enable_async,
            // Windows: env opt-in only, never a default. Caller already arbitrated vs split;
            // `build_init_params` also refuses sub-frame on an async session.
            subframe,
        );

        match (api().initialize_encoder)(enc, &mut init).nv_ok() {
            Ok(()) => Ok(enc),
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                Err(nvenc_status::call_err("initialize_encoder", e))
            }
        }
    }

    /// Lazily create the session on the first frame's D3D11 device so capture and encode share it.
    fn init_session(&mut self, device: &ID3D11Device) -> Result<()> {
        // Serialize this open (caps + clamp + charge) against other opens and the zombie reap.
        let _gate = DRIVER_SESSION_GATE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: gate held (no open can be handed a recycled zombie address mid-reap) and the
        // reap itself re-checks that no live session exists before touching any parked handle.
        unsafe { reap_parked_sessions() };
        // SAFETY: NVENC calls go through the runtime-loaded [`EncodeApi`] table (`api()`,
        // gated in `open`) or this type's `unsafe fn`s. `query_caps` / `try_open_session`
        // take the live `ID3D11Device` and return a session or `Err`. `destroy_encoder`
        // runs only on a handle just returned (`best` when non-null). `create_bitstream_buffer`
        // fills `cb` (version set); the pointer is copied into `self.bitstreams` before `cb` drops.
        unsafe {
            self.query_caps(device)?;
            // NVENC rejects `initialize_encoder` when bitrate exceeds the GPU's max codec level.
            // Try the request, then binary-search down to the max the level accepts.
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // Split-frame encode: one session tops out ~0.8–1 Gpix/s. See [`resolve_split_mode`].
            // Init-failure fallback below disables it if rejected.
            let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
            let split_mode: u32 =
                resolve_split_mode(self.codec, self.bit_depth, pixel_rate, self.encoder_engines);
            // Multi-slice default 4, clamped by the client ceiling. `PUNKTFUNK_NVENC_SLICES` overrides.
            self.slices = resolve_slices(self.codec, 4.min(self.max_slices));
            // Sub-frame defaults ON where the GPU advertises SUBFRAME_READBACK.
            // `PUNKTFUNK_NVENC_SUBFRAME` is the tri-state override. `subframe_broken`
            // wins over the operator force so a failed prefix check does not re-arm.
            let subframe_req = resolve_subframe(self.subframe_cap) && !self.subframe_broken;
            let (split_mode, subframe_req) =
                resolve_split_subframe(self.codec, split_mode, subframe_req, subframe_env_forced());
            // Highest bitrate the codec LEVEL accepts. If a forced split is the only problem,
            // disable it and retry; if bitrate itself is too high, bisect [FLOOR, requested].
            const CLAMP_TOL_BPS: u64 = 20_000_000; // stop bisecting within ~20 Mbps of the ceiling

            let use_async = self.async_supported && async_retrieve_requested();

            // Prior clamp already found this config's max — open at the ceiling
            // instead of binary-searching on every ABR overshoot.
            let mut target_bps = requested_bps;
            if let Some(ceiling) = cached_ceiling(&self.ceiling_key(split_mode)) {
                if requested_bps > ceiling {
                    tracing::info!(
                        requested_mbps = requested_bps / 1_000_000,
                        ceiling_mbps = ceiling / 1_000_000,
                        "NVENC: requested bitrate above the cached codec-level ceiling — opening \
                         at the ceiling"
                    );
                    target_bps = ceiling;
                }
            }

            let mut probe =
                self.try_open_session(device, target_bps, split_mode, use_async, subframe_req);
            // Cache is advisory: a stale entry must not wedge the open — retry the requested rate.
            if probe.is_err() && target_bps < requested_bps {
                target_bps = requested_bps;
                probe = self.try_open_session(
                    device,
                    requested_bps,
                    split_mode,
                    use_async,
                    subframe_req,
                );
            }
            // Disambiguate forced-split rejection from a bitrate cap: retry once with split
            // disabled. AV1 can reject AUTO split as INVALID_PARAM, which then looks like a
            // bitrate cap and fails even at the floor. `used_split` is what later reconfigure
            // and the ceiling-cache key must re-present.
            let mut used_split = split_mode;
            let split_on =
                split_mode != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
            if probe.is_err() && split_on {
                let no_split = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                if let Ok(e) =
                    self.try_open_session(device, target_bps, no_split, use_async, subframe_req)
                {
                    tracing::warn!("NVENC: split-encode rejected by codec/config — disabled");
                    used_split = no_split;
                    probe = Ok(e);
                }
            }

            let enc = match probe {
                Ok(enc) => {
                    self.bitrate_bps = target_bps;
                    enc
                }
                // Only a param/caps rejection means "bitrate above the codec-level ceiling".
                // Transient failures must not be cached as a bogus ceiling.
                Err(e) if !nvenc_status::is_param_rejection(&e) => return Err(e),
                Err(_) => {
                    // Requested bitrate exceeds the codec-level ceiling. `lo` is highest known-good
                    // (FLOOR assumed to fit), `hi` lowest rejected; `best` holds the live session at `lo`.
                    let mut lo = FLOOR_BPS;
                    let mut hi = target_bps;
                    let mut best: *mut c_void = ptr::null_mut();
                    let mut best_bps = 0u64;
                    while hi > lo + CLAMP_TOL_BPS {
                        let mid = lo + (hi - lo) / 2;
                        match self.try_open_session(
                            device,
                            mid,
                            used_split,
                            use_async,
                            subframe_req,
                        ) {
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
                                // Environmental mid-search failure: don't shrink the search.
                                if !best.is_null() {
                                    let _ = (api().destroy_encoder)(best);
                                }
                                return Err(e);
                            }
                        }
                    }
                    if best.is_null() {
                        // Nothing in (FLOOR, requested] accepted — try the floor, also split-disabled
                        // in case forced split (not bitrate) is the blocker.
                        let no_split =
                            nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                        best = match self.try_open_session(
                            device,
                            FLOOR_BPS,
                            used_split,
                            use_async,
                            subframe_req,
                        ) {
                            Ok(e) => e,
                            Err(_) => {
                                let e = self
                                    .try_open_session(
                                        device,
                                        FLOOR_BPS,
                                        no_split,
                                        use_async,
                                        subframe_req,
                                    )
                                    .context(
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
                        "NVENC: requested bitrate above the GPU codec-level ceiling — clamped to the max accepted"
                    );
                    store_ceiling(self.ceiling_key(used_split), best_bps);
                    self.bitrate_bps = best_bps;
                    best
                }
            };
            self.encoder = enc;
            // Pin the device for the session lifetime and a parked afterlife if destroy fails.
            self.init_device_com = Some(device.clone());
            // Init params a later `reconfigure_bitrate` must re-present verbatim.
            self.split_mode = used_split;
            self.subframe_on = subframe_req;
            self.session_async = use_async;
            // Chunked poll is depth-1 sync; the async retrieve thread owns the bitstream lock.
            self.subframe_chunks = self.slices >= 2 && subframe_req && !use_async;
            if self.subframe_chunks {
                tracing::info!(
                    slices = self.slices,
                    "NVENC sub-frame chunked poll armed (poll_chunk emits slice-boundary AU chunks)"
                );
            }
            // Charge what this open holds so admission can decline a parallel display. Weighted
            // by the final split mode (one hardware session per engine).
            self.session_units = split_mode_units(used_split);
            LIVE_SESSION_UNITS.fetch_add(self.session_units, std::sync::atomic::Ordering::Relaxed);
            // One output bitstream per in-flight slot. No encoder-owned input pool: capturer
            // textures are registered on demand in `submit` and encoded in place.
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
            // One auto-reset completion event per pool bitstream, plus the retrieve thread.
            // The thread only sees raw addresses; `teardown` joins it before any of them die.
            if use_async {
                for _ in 0..POOL {
                    let ev = CreateEventW(None, false, false, PCWSTR::null())
                        .context("CreateEvent (NVENC completion)")?;
                    // Push before registering: teardown only closes handles already in
                    // `self.events`. Unregister of a never-registered event is harmless.
                    self.events.push(ev.0 as usize);
                    let mut ep = nv::NV_ENC_EVENT_PARAMS {
                        version: nv::NV_ENC_EVENT_PARAMS_VER,
                        completionEvent: ev.0,
                        ..Default::default()
                    };
                    (api().register_async_event)(enc, &mut ep)
                        .nv_ok()
                        .map_err(|e| nvenc_status::call_err("register_async_event", e))?;
                }
                let (work_tx, work_rx) = mpsc::sync_channel::<RetrieveJob>(POOL);
                let (done_tx, done_rx) = mpsc::channel::<RetrieveDone>();
                let enc_addr = enc as usize;
                let join = std::thread::Builder::new()
                    .name("punktfunk-nvenc-out".into())
                    .spawn(move || retrieve_loop(enc_addr, work_rx, done_tx))
                    .context("spawn NVENC retrieve thread")?;
                self.async_rt = Some(AsyncRetrieve {
                    work_tx: Some(work_tx),
                    done_rx,
                    join: Some(join),
                    ready: VecDeque::new(),
                });
                tracing::info!(
                    pool = POOL,
                    "NVENC async retrieve active (two-thread encode: submit here, \
                     lock_bitstream on the retrieve thread)"
                );
            }
            self.inited = true;
            tracing::info!(
                // `split_mode` is the final mode (post-fallback) and `engines` the ceiling it was
                // chosen from — either alone is ambiguous. `subframe` because AUTO + sub-frame
                // is a single-engine combination that reads like a split in a log.
                split_mode = self.split_mode,
                engines = self.encoder_engines,
                subframe = self.subframe_on,
                "NVENC D3D11 session: {}x{}@{} {}-bit{} {} Mbps {:?}",
                self.width,
                self.height,
                self.fps,
                self.bit_depth,
                if self.hdr { " HDR(BT.2020 PQ)" } else { "" },
                self.bitrate_bps / 1_000_000,
                self.codec_guid
            );
            self.subframe_opened_with = self.subframe_on;
            self.arm_split_arbiter();
            Ok(())
        }
    }

    /// Fold one retrieve-thread completion into encoder state on the encode thread: pop the
    /// oldest `pending` (FIFO), verify bitstream pairing, unmap, queue the AU. A retrieve error
    /// surfaces after the unmap so the rebuild path starts from clean state.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some((bs, map, pts_ns, anchor, _idr_hint)) = self.pending.pop_front() else {
            bail!("NVENC async: completion with no in-flight frame (pairing bug)");
        };
        if bs as usize != done.bs {
            bail!("NVENC async: completion out of order (pairing bug)");
        }
        // SAFETY: `map` is the mapped input `submit` recorded for this completed encode; the
        // session is live (`async_rt` exists only between `init_session` and `teardown`) and this
        // runs on the encode thread. One unmap, mirroring the sync path's poll-side unmap.
        unsafe {
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, map);
            }
        }
        let (data, keyframe) = done.result.map_err(|e| anyhow!("{e}"))?;
        self.async_rt
            .as_mut()
            .expect("absorb_done is only reachable in async mode")
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

impl Encoder for NvencD3d11Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!(
                    "NVENC D3D11 encoder needs a GPU texture frame (use the software encoder for CPU frames)"
                )
            }
        };
        // Capturer recreates its D3D11 device on a desktop switch and may return a different
        // resolution. Re-init on a different device or size. HDR (BT.2020 PQ) when the capturer
        // hands a 10-bit frame (Rgb10a2 or P010); 8-bit NV12/ARGB is SDR. Can flip mid-session.
        let hdr = matches!(captured.format, PixelFormat::Rgb10a2 | PixelFormat::P010);
        let dev_raw = frame.device.as_raw();
        let size_changed =
            self.inited && (self.width != captured.width || self.height != captured.height);
        // Compare against last-init REQUEST, not effective `self.hdr`: on a no-10-bit GPU
        // `query_caps` clears `self.hdr`, so a P010 capturer would rebuild every frame.
        let hdr_changed = self.inited && self.hdr_requested != hdr;
        if self.inited && (self.init_device != dev_raw || size_changed || hdr_changed) {
            tracing::info!(
                device_changed = self.init_device != dev_raw,
                size_changed,
                hdr_changed,
                hdr,
                new = format!("{}x{}", captured.width, captured.height),
                "NVENC: capture device/size/HDR changed — re-initializing session"
            );
            // SAFETY: `teardown` needs the encode thread with no NVENC call in flight and a session
            // whose cached regs/bitstreams/pending belong to `self.encoder`. All hold: this is the
            // encode thread, `self.inited` so `self.encoder` is the live session, and the previous
            // frame's encode has already been polled.
            unsafe { self.teardown() };
        }
        if !self.inited {
            self.width = captured.width;
            self.height = captured.height;
            self.hdr_requested = hdr;
            // Effective until `query_caps` clears it on a card without 10-bit.
            self.hdr = hdr;
            // Recompute effective 4:4:4 from the negotiation. Overwriting `chroma_444` on the
            // first subsampled-YUV frame would permanently demote the session.
            self.chroma_444 = self.chroma_444_requested;
            // YUV (NV12/P010): native encode, no RGB→YUV CSC. RGB is the shader path.
            // 10-bit forces Main10.
            self.buffer_fmt = match captured.format {
                PixelFormat::P010 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
                }
                PixelFormat::Rgb10a2 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                }
                PixelFormat::Rgb10a2Sdr => {
                    // 10-bit SDR: same packed layout as Rgb10a2, but sRGB — `hdr` is false, so
                    // the session opens Main10 with BT.709 SDR VUI.
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                }
                PixelFormat::Nv12 => {
                    // NV12 is 8-bit 4:2:0. Unlike ARGB, NV12 cannot feed a 10-bit session:
                    // `register_resource` rejects it as InvalidParam.
                    self.bit_depth = 8;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
                }
                _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            };
            // FREXT/chromaFormatIDC=3 only on RGB input. Clear the effective flag so
            // `caps().chroma_444` reports what the stream carries; keep `chroma_444_requested`
            // so a later RGB re-init recovers 4:4:4.
            if self.chroma_444
                && !matches!(
                    self.buffer_fmt,
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
                        | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                )
            {
                tracing::warn!(
                    format = ?captured.format,
                    "4:4:4 negotiated but the capturer delivered subsampled YUV — encoding 4:2:0"
                );
                self.chroma_444 = false;
            }
            let device = frame.device.clone();
            // `init_session` publishes `self.encoder` (and charges units) before its last
            // fallible steps, so a failure leaves a live session with `inited == false`.
            // Re-init guards key off `inited`; teardown here, keyed off `encoder.is_null()`,
            // so the next submit does not overwrite a live handle.
            if let Err(e) = self.init_session(&device) {
                // SAFETY: same contract as the teardown above — encode thread owns the session,
                // and a failed init leaves nothing mid-encode.
                unsafe { self.teardown() };
                return Err(e);
            }
            self.init_device = dev_raw;
        }
        // Opening frame: NVENC emits an IDR regardless of pic flags, so HDR SEI must ride it.
        // Detected via `next == 0` (`teardown` zeroes it), not `pts == 0`: `submit_indexed`
        // pins pts to the wire index, which is non-zero on a mid-session rebuild's first frame.
        let opening = self.next == 0;
        // Never reuse an in-flight bitstream; keep depth within the capturer's texture ring.
        // Encode is in-place: exceeding `pipeline_depth` overwrites a live texture. An
        // unknown ring uses 2 — less pipelining, not corruption. At the cap, block on the oldest.
        const UNCONFIGURED_RING_DEPTH: usize = 2;
        let cap = match self.input_ring_depth {
            Some(d) => async_inflight_cap().min(d.max(1)),
            None => async_inflight_cap().min(UNCONFIGURED_RING_DEPTH),
        };
        while self.async_rt.is_some() && self.pending.len() >= cap {
            let done = {
                let rt = self.async_rt.as_mut().expect("checked in loop condition");
                rt.done_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(|_| anyhow!("NVENC async retrieve stalled (5s) — encoder wedged?"))?
            };
            self.absorb_done(done)?;
        }
        let slot = self.next % POOL;
        self.next += 1;
        // SAFETY: NVENC calls go through the loaded `EncodeApi` table against `self.encoder`
        // (live, non-null). `rr` (version set) registers `frame.texture` from the same
        // device the session opened against; the cloned texture in `regs` keeps it alive.
        // `mp` maps that registration and is recorded in `pending` for one unmap. `pic`
        // points at the mapped resource and `bitstreams[slot]`; SEI scratch outlives encode.
        unsafe {
            let key = frame.texture.as_raw() as isize;
            if !self.regs.contains_key(&key) {
                let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType:
                        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                    width: self.width,
                    height: self.height,
                    pitch: 0,
                    resourceToRegister: frame.texture.as_raw(),
                    bufferFormat: self.buffer_fmt,
                    bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                (api().register_resource)(self.encoder, &mut rr)
                    .nv_ok()
                    .map_err(|e| nvenc_status::call_err("register_resource", e))?;
                self.regs
                    .insert(key, (rr.registeredResource, frame.texture.clone()));
            }
            let reg = self.regs[&key].0;

            let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
                version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: reg,
                ..Default::default()
            };
            (api().map_input_resource)(self.encoder, &mut mp)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("map_input_resource", e))?;

            let pts = self.frame_idx as u64;
            self.frame_idx += 1;
            let flags = if std::mem::take(&mut self.force_kf) {
                nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            };
            // Recovery anchor (armed by a successful invalidate_ref_frames): this frame is the
            // first encoded after invalidation. A simultaneous forced IDR is itself the re-anchor.
            let anchor = std::mem::take(&mut self.pending_anchor) && flags == 0;
            // Chunked poll must flag early chunks before the driver reports `pictureType`.
            // Under P-only + infinite GOP, IDRs happen only when forced, or on the opening
            // frame (NVENC emits IDR regardless). Without `opening`, frame 1's early chunks go unflagged.
            let idr_hint = flags != 0 || opening;
            let mut pic = nv::NV_ENC_PIC_PARAMS {
                version: nv::NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.width,
                inputHeight: self.height,
                inputPitch: 0,
                inputBuffer: mp.mappedResource,
                bufferFmt: mp.mappedBufferFmt,
                outputBitstream: self.bitstreams[slot],
                pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputTimeStamp: pts,
                encodePicFlags: flags as u32,
                // Async: event the driver signals when this encode completes. Null in sync.
                completionEvent: self
                    .events
                    .get(slot)
                    .map(|&e| e as *mut c_void)
                    .unwrap_or(ptr::null_mut()),
                ..Default::default()
            };

            // In-band HDR10 SEI on every IDR: ST.2086 mastering + CEA-861.3 CLL.
            // HEVC/H.264 carry SEI; AV1 uses metadata OBUs. Scratch outlives `encode_picture`.
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
                // Union write: pointers/len are read during encode_picture (scratch outlives it).
                match self.codec {
                    Codec::H265 => {
                        pic.codecPicParams.hevcPicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.hevcPicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    Codec::H264 => {
                        pic.codecPicParams.h264PicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.h264PicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    // AV1 mastering/CLL ride METADATA OBUs, not SEI.
                    Codec::Av1 => {}
                    Codec::PyroWave => {
                        unreachable!("PyroWave never opens the direct-NVENC backend")
                    }
                }
            }
            (api().encode_picture)(self.encoder, &mut pic)
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("encode_picture", e))?;
            self.pending.push_back((
                self.bitstreams[slot],
                mp.mappedResource,
                captured.pts_ns,
                anchor,
                idr_hint,
            ));
            // Split-arbiter cost; only meaningful on the sync depth-1 path `arm_split_arbiter` allows.
            self.last_submit_at = Some(std::time::Instant::now());
            // Channel capacity = POOL ≥ in-flight, so this send never blocks.
            if let Some(rt) = &self.async_rt {
                let job = RetrieveJob {
                    bs: self.bitstreams[slot] as usize,
                    event: self.events[slot],
                };
                if rt.work_tx.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
                    bail!("NVENC retrieve thread gone — rebuilding the session");
                }
            }
        }
        Ok(())
    }

    /// Pin this submission's frame number (`inputTimeStamp`) to the wire index the AU will
    /// carry, so RFI timestamps stay 1:1 across rebuilds. A repeat after a reset lands on a
    /// fresh session (teardown cleared the DPB), so re-pinning is always sound.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn set_input_ring_depth(&mut self, depth: usize) {
        // Encode is in-place, so the capturer's ring depth hard-caps async pipeline depth.
        self.input_ring_depth = Some(depth);
        tracing::debug!(
            depth,
            env_cap = async_inflight_cap(),
            "NVENC: capturer input-ring depth reported — async in-flight bounded by the smaller"
        );
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn caps(&self) -> EncoderCaps {
        // RFI is probed once at open. In-band HDR SEI needs no cap: it rides HEVC/H.264 keyframes.
        EncoderCaps {
            // Windows capture composites the pointer; this backend never reads `frame.cursor`.
            blends_cursor: false,
            supports_rfi: self.rfi_supported,
            // What the session actually configured (cleared in `query_caps` if the GPU lacks YUV444).
            chroma_444: self.chroma_444,
            // Direct-NVENC recovers via real RFI (or a forced IDR), not libavcodec intra-refresh.
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.hdr_meta = meta;
    }

    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live session or the GPU can't invalidate → caller forces a full IDR.
        // Range policy is `nvenc_core::plan_range_recovery` for both direct-NVENC backends.
        if self.encoder.is_null() || !self.rfi_supported {
            return false;
        }
        match plan_range_recovery(first, last, self.frame_idx, self.last_rfi_range) {
            // Covering range already invalidated. Re-arm the anchor: the previous recovery AU
            // may itself have been lost, and the next frame is equally clean.
            RangePlan::Covered => {
                self.pending_anchor = true;
                true
            }
            RangePlan::Decline => false,
            RangePlan::Invalidate { first, last } => {
                // `inputTimeStamp` is the wire index, so the client's lost-frame range maps 1:1
                // onto NVENC timestamps across rebuilds.
                // SAFETY: `invalidate_ref_frames` is a loaded `EncodeApi` pointer. `self.encoder`
                // is the live session (checked non-null) on the encode thread. Each `ts` is
                // clamped to `[oldest_in_dpb, frame_idx - 1]`, so it names a frame still in the DPB.
                unsafe {
                    for ts in first..=last {
                        if (api().invalidate_ref_frames)(self.encoder, ts as u64)
                            .nv_ok()
                            .is_err()
                        {
                            return false; // any failure → fall back to IDR
                        }
                    }
                }
                self.last_rfi_range = Some((first, last));
                // Next submitted frame is the first encoded after invalidation. Tag it so the
                // client lifts its freeze here instead of waiting for the cooldown-suppressed IDR.
                self.pending_anchor = true;
                true
            }
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // A partially-chunked AU must finish through `poll_chunk`; a whole-AU poll would double-emit.
        if self.chunk.is_some() {
            bail!("NVENC poll() called mid-chunked-AU — drain it via poll_chunk (caller bug)");
        }
        // Drain finished retrieves without blocking. `None` = still in flight; capture never
        // waits on the WDDM scheduling wait.
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
        let Some((bs, map, pts_ns, anchor, _idr_hint)) = self.pending.pop_front() else {
            return Ok(None);
        };
        // SAFETY: non-empty `pending` implies `submit` ran, so `self.encoder` is live
        // (`teardown` clears `pending` when it nulls the handle). `lock` (version set)
        // targets a pool bitstream; `lock_bitstream` blocks until the encode finishes,
        // so `bitstreamBufferPtr` is CPU-readable until unlock. Copy first; unmap `map` once.
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
        // Dynamic: a rebuild can land in a different mode and `teardown` drops the latch.
        self.subframe_chunks && self.async_rt.is_none()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // Not a chunked session: degrade to one whole-AU chunk. Mid-AU state must still finish below.
        if !self.supports_chunked_poll() && self.chunk.is_none() {
            return Ok(self.poll()?.map(AuChunk::whole));
        }
        let Some(&(bs, _, pts_ns, anchor, idr_hint)) = self.pending.front() else {
            return Ok(None);
        };
        // If this driver never publishes intermediate slices, stop after ~2 frame intervals and
        // finish through the blocking lock.
        let budget = std::time::Duration::from_micros(2_000_000 / self.fps.max(1) as u64);
        let t0 = std::time::Instant::now();
        let mut offsets = [0u32; 32];
        loop {
            let emitted = self.chunk.as_ref().map_or(0, |c| c.emitted);
            let slices_out = self.chunk.as_ref().map_or(0, |c| c.slices_out);
            // SAFETY: `bs` is the front `pending` pool bitstream (`teardown` clears `pending`
            // when it nulls the session). `lock` (version set, doNotWait) and `offsets` are
            // live stack locals; the driver may write up to 32 offsets. On a successful
            // sub-frame lock, `bitstreamBufferPtr` is valid until unlock; copy first.
            // Every successful lock is unlocked exactly once.
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
                        // Every slice readable — finish through the blocking lock (`numSlices`
                        // alone is not trusted across driver branches).
                        let _ = (api().unlock_bitstream)(self.encoder, bs);
                        break;
                    }
                    if n > slices_out && bytes > emitted {
                        // New completed slice(s): cut `[emitted..bytes)`. Contiguous Annex-B, so
                        // the cut lands on a NAL boundary.
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
                // LOCK_BUSY = not ready. The finishing blocking lock owns real failures.
            }
            if t0.elapsed() > budget {
                break;
            }
            std::thread::sleep(CHUNK_SAMPLE_INTERVAL);
        }

        // One blocking lock — the completion authority. The AU tail must not ride a +1 tick
        // (depth-1 pump contract).
        let (bs, map, pts_ns, anchor, idr_hint) =
            self.pending.pop_front().expect("front() checked above");
        // SAFETY: same contract as `poll`'s blocking lock: `bs` is the popped in-flight pool
        // bitstream on the live session (encode thread); blocking `lock_bitstream` (version set)
        // yields CPU-readable bytes valid until `unlock_bitstream` — every read happens first.
        // `map` is unmapped here, after completion, exactly once.
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
            // Completion authority vs sampler: doNotWait bytes must be a byte-exact prefix of
            // the finished AU, or the wire already carries undetectable corruption. On
            // divergence, latch sub-frame off and bail into stall-recovery (rebuild without
            // sub-frame, force IDR). Check `emitted > total` first — the prefix slice is ill-formed.
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
                // Can't happen under P-only + infinite GOP; if it does, earlier chunks had the wrong flag.
                tracing::warn!(
                    predicted = idr_hint,
                    actual = keyframe,
                    "NVENC chunked poll: picture type diverged from the submit-time prediction"
                );
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

    /// Encode-stall recovery: tear the session down; the next `submit` rebuilds (fresh
    /// session, IDR). Sync retrieve blocks inside `lock_bitstream`, so a hung lock never
    /// returns; this covers async retrieve (5 s event timeouts) and submit-side failures.
    fn reset(&mut self) -> bool {
        // SAFETY: `teardown` needs the encode thread with no NVENC call in flight and a session
        // whose cached resources belong to `self.encoder` — all hold (called between submit/poll).
        unsafe { self.teardown() };
        self.force_kf = true;
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        if !self.inited {
            // No live session yet — lazy init opens at the new rate.
            self.bitrate_bps = bps;
            return true;
        }
        // Clamp to the cached codec-level ceiling before the driver call so an overshoot
        // retargets in place instead of bouncing into a full rebuild (IDR + session churn).
        let bps = match cached_ceiling(&self.ceiling_key(self.split_mode)) {
            Some(ceiling) => bps.min(ceiling),
            None => bps,
        };
        // SAFETY: `inited` ⟹ `self.encoder` is the live session and this runs on the encode
        // thread between submit/poll (`nvEncReconfigureEncoder` is a submit-side call — the
        // retrieve thread only locks bitstreams). `cfg` outlives the synchronous reconfigure
        // call whose `reInitEncodeParams.encodeConfig` points at it.
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
                    self.session_async,
                    // Session's recorded state, not a fresh env read: reconfigure must present
                    // the open's init params (an env re-read could flip enableSubFrameWrite).
                    self.subframe_on,
                ),
                ..Default::default()
            };
            // Keep RC state and the reference chain: no reset, no IDR.
            params.set_resetEncoder(0);
            params.set_forceIDR(0);
            match (api().reconfigure_encoder)(self.encoder, &mut params).nv_ok() {
                Ok(()) => {
                    self.bitrate_bps = bps;
                    true
                }
                Err(e) => {
                    // New rate above the codec-level ceiling — caller rebuild owns the clamp search.
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
        // `bitrate_bps` is post-clamp: both open and reconfigure write what the session targets.
        Some(self.bitrate_bps)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + frameIntervalP=1: each submit yields its AU; no internal queue.
    }
}

impl Drop for NvencD3d11Encoder {
    fn drop(&mut self) {
        // SAFETY: `teardown` needs the owning thread with no NVENC call in flight and a session
        // whose cached resources belong to `self.encoder`. At Drop this encoder is owned
        // exclusively on the encode thread; `teardown` early-returns when `self.encoder` is null.
        unsafe { self.teardown() };
    }
}

/// Probe HEVC 4:4:4 (`NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`). Cached by [`crate::can_encode_444`]
/// and read before Welcome so the host advertises the chroma it can really encode.
pub fn probe_can_encode_444(codec: Codec) -> bool {
    if codec != Codec::H265 {
        return false;
    }
    probe_encode_cap(codec, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)
}

/// Probe 10-bit encode (`NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` on the codec GUID). Cached by
/// [`crate::can_encode_10bit`] and read before Welcome so negotiated depth matches NVENC.
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    probe_encode_cap(codec, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE)
}

/// One NVENC cap for `codec` on a throwaway session. `false` on any failure — unconfirmed = no.
fn probe_encode_cap(codec: Codec, cap: nv::NV_ENC_CAPS) -> bool {
    with_probe_session(|enc| {
        let mut param = nv::NV_ENC_CAPS_PARAM {
            version: nv::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: cap,
            reserved: [0; 62],
        };
        let mut val: i32 = 0;
        // SAFETY: `get_encode_caps` reads one scalar cap into `val` (live locals) for live
        // session `enc` via the loaded API table (`with_probe_session` sits past `try_api`).
        unsafe {
            (api().get_encode_caps)(enc, codec_guid(codec), &mut param, &mut val)
                .nv_ok()
                .is_ok()
                && val != 0
        }
    })
    .unwrap_or(false)
}

/// Codecs this GPU's NVENC can encode (`nvEncGetEncodeGUIDs`) on a throwaway session,
/// probing the selected render adapter. Failure returns "nothing probed", which
/// [`crate::CodecSupport::wire_mask`] turns into `None` so a broken probe cannot
/// narrow an NVIDIA host to nothing. Cached per GPU by [`crate::windows_codec_support`].
pub(crate) fn probe_codec_support() -> crate::CodecSupport {
    let unknown = crate::CodecSupport {
        h264: false,
        h265: false,
        av1: false,
    };
    with_probe_session(|enc| {
        // SAFETY: all NVENC calls go through the loaded API table against live session `enc`;
        // `count`/`written` are live locals, and `guids` is sized to the count the driver just
        // reported, its pointer valid for that many `GUID`s.
        unsafe {
            let mut count = 0u32;
            let counted = (api().get_encode_guid_count)(enc, &mut count)
                .nv_ok()
                .is_ok();
            let mut guids = vec![nv::GUID::default(); count as usize];
            let mut written = 0u32;
            let listed = counted
                && count > 0
                && (api().get_encode_guids)(enc, guids.as_mut_ptr(), count, &mut written)
                    .nv_ok()
                    .is_ok();
            if !listed {
                tracing::warn!(
                    "NVENC codec probe: driver listed no encode GUIDs — keeping the static \
                     advertisement"
                );
                return unknown;
            }
            guids.truncate(written as usize);
            crate::CodecSupport {
                h264: guids.contains(&codec_guid(Codec::H264)),
                h265: guids.contains(&codec_guid(Codec::H265)),
                av1: guids.contains(&codec_guid(Codec::Av1)),
            }
        }
    })
    .unwrap_or(unknown)
}

/// Open a throwaway NVENC session on a fresh hardware D3D11 device, hand it to `f`, tear down.
/// `None` = no loadable NVENC / no device / failed open. Shared by [`probe_encode_cap`] and
/// [`probe_codec_support`].
fn with_probe_session<T>(f: impl FnOnce(*mut c_void) -> T) -> Option<T> {
    // Same exclusion as `init_session`: a throwaway open must not overlap a zombie reap.
    let _gate = DRIVER_SESSION_GATE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    // No loadable NVENC → nothing to confirm. Also the `api()` gate for every call below and in `f`.
    if try_api().is_err() {
        return None;
    }
    // SAFETY: this probe owns every handle it creates. `CreateDXGIFactory1` /
    // `EnumAdapterByLuid` return owned COM or err. `D3D11CreateDevice` fills `device`
    // or returns Err. `open_encode_session_ex` opens against that device's raw pointer
    // (valid while `device` is held); a failed open destroys any residue session.
    // `destroy_encoder` runs once after `f` returns. No handle escapes.
    unsafe {
        // Probe the selected render adapter — the GPU the session will encode on. The OS default
        // can be the other GPU on a hybrid box.
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
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
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
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
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
        let device = device?;
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        if (api().open_encode_session_ex)(&mut params, &mut enc)
            .nv_ok()
            .is_err()
        {
            // Destroy-on-failed-open: a failed open may still hold a session slot.
            if !enc.is_null() {
                let _ = (api().destroy_encoder)(enc);
            }
            return None;
        }
        // A real session open: the driver accepted this build's version word (see `nvenc_status`).
        nvenc_status::note_session_opened();
        let out = f(enc);
        let _ = (api().destroy_encoder)(enc);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::{dxgi::D3d11Frame, CapturedFrame, FramePayload};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    /// Saturated primaries separate BT.601 from BT.709 by tens of code points (pure-green luma 145 vs 173).
    const BARS: [(u8, u8, u8); 8] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
    ];

    /// Left half: colour bars (matrix measurement). Right half: 1-px red/blue columns (true 4:4:4
    /// keeps adjacent chroma distinct; a subsampled encode blends them).
    fn probe_pattern(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        let bar_w = (w / 2) / BARS.len();
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if x < w / 2 {
                    BARS[(x / bar_w).min(BARS.len() - 1)]
                } else if x % 2 == 0 {
                    (255, 0, 0)
                } else {
                    (0, 0, 255)
                };
                let o = (y * w + x) * 4;
                px[o] = b;
                px[o + 1] = g;
                px[o + 2] = r;
                px[o + 3] = 255;
            }
        }
        px
    }

    /// Encode 30 static pattern frames through a real NVENC session (ARGB, production config).
    fn encode_pattern(chroma: ChromaFormat, path: &str) {
        const W: u32 = 1280;
        const H: u32 = 720;
        // SAFETY: test-only D3D11/DXGI COM calls on one thread; every out-pointer is checked
        // before use; the texture/device outlive the encoder.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
            let mut adapter = None;
            for i in 0.. {
                let Ok(a) = factory.EnumAdapters1(i) else {
                    break;
                };
                let desc = a.GetDesc1().expect("adapter desc");
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    adapter = Some(a);
                    break;
                }
            }
            let adapter = adapter.expect("no hardware DXGI adapter");
            let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");

            let bytes = probe_pattern(W as usize, H as usize);
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr() as *const _,
                SysMemPitch: W * 4,
                SysMemSlicePitch: 0,
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // NVENC registration requires RENDER_TARGET on D3D11 input textures.
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .expect("pattern texture");
            let tex = tex.expect("null pattern texture");

            let mut enc = NvencD3d11Encoder::open(
                Codec::H265,
                PixelFormat::Bgra,
                W,
                H,
                60,
                100_000_000, // high rate: the 1-px stripes must survive quantization
                8,
                chroma,
                1,
            )
            .expect("NVENC open");
            let mut out = Vec::new();
            for i in 0..30u64 {
                let frame = CapturedFrame {
                    provenance: Default::default(),
                    width: W,
                    height: H,
                    pts_ns: i * 16_666_667,
                    format: PixelFormat::Bgra,
                    payload: FramePayload::D3d11(D3d11Frame {
                        texture: tex.clone(),
                        device: device.clone(),
                        pyro: None,
                    }),
                    cursor: None,
                };
                enc.submit(&frame).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    out.extend_from_slice(&au.data);
                }
            }
            enc.flush().ok();
            while let Ok(Some(au)) = enc.poll() {
                out.extend_from_slice(&au.data);
            }
            assert!(!out.is_empty(), "no AUs produced");
            let caps444 = enc.caps().chroma_444;
            std::fs::write(path, &out).expect("write bitstream");
            println!(
                "wrote {path}: {} bytes, requested {chroma:?}, caps.chroma_444={caps444}",
                out.len()
            );
        }
    }

    /// Encode a few frames, `reconfigure_bitrate` mid-stream (up and down), and assert
    /// every post-reconfigure AU is a P-frame (`resetEncoder=0` / `forceIDR=0` must not
    /// restart the stream). Windows counterpart of Linux `nvenc_cuda_reconfigure_no_idr`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
    fn nvenc_reconfigure_no_idr() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        const W: u32 = 1280;
        const H: u32 = 720;
        // SAFETY: test-only, same D3D11/DXGI setup as `encode_pattern`.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
            let mut adapter = None;
            for i in 0.. {
                let Ok(a) = factory.EnumAdapters1(i) else {
                    break;
                };
                let desc = a.GetDesc1().expect("adapter desc");
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                    adapter = Some(a);
                    break;
                }
            }
            let adapter = adapter.expect("no hardware DXGI adapter");
            let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");

            let bytes = probe_pattern(W as usize, H as usize);
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr() as *const _,
                SysMemPitch: W * 4,
                SysMemSlicePitch: 0,
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .expect("pattern texture");
            let tex = tex.expect("null pattern texture");

            let mut enc = NvencD3d11Encoder::open(
                Codec::H265,
                PixelFormat::Bgra,
                W,
                H,
                60,
                20_000_000,
                8,
                ChromaFormat::Yuv420,
                1,
            )
            .expect("NVENC open");

            let submit_and_poll = |enc: &mut NvencD3d11Encoder, range: std::ops::Range<u64>| {
                let mut keyframes = 0usize;
                let mut aus = 0usize;
                for i in range {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: W,
                        height: H,
                        pts_ns: i * 16_666_667,
                        format: PixelFormat::Bgra,
                        payload: FramePayload::D3d11(D3d11Frame {
                            texture: tex.clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit_indexed(&frame, i as u32).expect("submit");
                    while let Some(au) = enc.poll().expect("poll") {
                        aus += 1;
                        keyframes += au.keyframe as usize;
                    }
                }
                enc.flush().ok();
                while let Ok(Some(au)) = enc.poll() {
                    aus += 1;
                    keyframes += au.keyframe as usize;
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

            println!("nvenc (Windows) reconfigure smoke: 20→60→10 Mbps in place, zero IDRs");
        }
    }

    /// Check that `nvEncReconfigureEncoder` accepts a changed `splitEncodeMode` with
    /// `resetEncoder=0` and emits no IDR on D3D11. Also checks `query_caps` latches
    /// `NUM_ENCODER_ENGINES` and that an over-ask (3-way split on a 2-engine card)
    /// is why the clamp exists. Reports rather than asserts: both outcomes are findings.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX Windows box"]
    fn nvenc_split_reconfigure_in_place() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // SAFETY: this ignored hardware test is run alone; no other thread touches the env.
        unsafe {
            std::env::set_var("PUNKTFUNK_NVENC_SUBFRAME", "0");
            std::env::set_var("PUNKTFUNK_SPLIT_ENCODE", "0");
        }

        // SAFETY: test-only, same D3D11/DXGI setup as `nvenc_reconfigure_no_idr`.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
            let mut adapter = None;
            for i in 0.. {
                let Ok(a) = factory.EnumAdapters1(i) else {
                    break;
                };
                if a.GetDesc1().expect("adapter desc").Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32
                    == 0
                {
                    adapter = Some(a);
                    break;
                }
            }
            let adapter = adapter.expect("no hardware DXGI adapter");
            let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");
            let bytes = probe_pattern(W as usize, H as usize);
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr() as *const _,
                SysMemPitch: W * 4,
                SysMemSlicePitch: 0,
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .expect("pattern texture");
            let tex = tex.expect("null pattern texture");

            let mut enc = NvencD3d11Encoder::open(
                Codec::H265,
                PixelFormat::Bgra,
                W,
                H,
                60,
                BPS,
                8,
                ChromaFormat::Yuv420,
                1,
            )
            .expect("NVENC open");

            let submit_and_poll = |enc: &mut NvencD3d11Encoder, range: std::ops::Range<u64>| {
                let (mut aus, mut keyframes) = (0usize, 0usize);
                for i in range {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: W,
                        height: H,
                        pts_ns: i * 16_666_667,
                        format: PixelFormat::Bgra,
                        payload: FramePayload::D3d11(D3d11Frame {
                            texture: tex.clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit_indexed(&frame, i as u32).expect("submit");
                    while let Some(au) = enc.poll().expect("poll") {
                        aus += 1;
                        keyframes += au.keyframe as usize;
                    }
                }
                (aus, keyframes)
            };

            let (aus, kfs) = submit_and_poll(&mut enc, 0..6);
            assert!(aus > 0 && kfs == 1, "opening IDR then steady P-frames");
            println!(
                "S1(win): engines={} (latched by query_caps), opened split_mode={}",
                enc.encoder_engines, enc.split_mode
            );
            assert!(
                enc.encoder_engines >= 2,
                "this GPU reports {} NVENC engine(s) — S1 is not interpretable here",
                enc.encoder_engines
            );
            assert_eq!(enc.split_mode, disable, "must open split-disabled");

            // Change only splitEncodeMode, in place, same bitrate.
            enc.split_mode = two;
            let accepted = enc.reconfigure_bitrate(BPS);
            println!("S1(win): reconfigure DISABLE→TWO_FORCED accepted = {accepted}");
            if accepted {
                let (aus, kfs) = submit_and_poll(&mut enc, 6..12);
                assert!(aus > 0, "no AUs after the accepted reconfigure");
                println!(
                    "S1(win) VERDICT: {}",
                    if kfs == 0 {
                        "PASS — accepted with NO IDR on D3D11: Windows arbitration is buildable"
                    } else {
                        "FAIL — accepted but forced an IDR, which is the same as a rejection"
                    }
                );
                enc.split_mode = disable;
                let back = enc.reconfigure_bitrate(BPS);
                println!("S1(win): reverse accepted = {back}");
            } else {
                enc.split_mode = disable;
                println!(
                    "S1(win) VERDICT: FAIL — the D3D11 path REFUSES an in-place split change. \
                     Windows arbitration is not buildable; the Linux result does not transfer."
                );
            }
            enc.flush().ok();
        }

        // SAFETY: single-threaded manual test; no concurrent env access.
        unsafe {
            std::env::remove_var("PUNKTFUNK_SPLIT_ENCODE");
            std::env::remove_var("PUNKTFUNK_NVENC_SUBFRAME");
        }
    }

    /// Encode the probe pattern as FREXT 4:4:4 and as 4:2:0 so offline analysis can tell whether
    /// the FREXT stream is full-chroma and which matrix the RGB→YUV CSC used (BT.601 vs BT.709).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box"]
    fn nvenc_444_on_glass_probe() {
        encode_pattern(
            ChromaFormat::Yuv444,
            "C:\\Users\\Public\\nvenc444_probe.h265",
        );
        encode_pattern(
            ChromaFormat::Yuv420,
            "C:\\Users\\Public\\nvenc420_probe.h265",
        );
    }

    /// Codec-advertisement probe against the real driver. Every NVENC GPU encodes H.264
    /// (`false` means enumeration is broken). Must be stable: one cached answer drives
    /// every negotiation. `--release` is required on Windows: debug `/OPT:NOREF` keeps
    /// the sdk crate's unused lazy loader and its NvEncodeAPI imports (LNK2019).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
    fn nvenc_codec_probe_reports_real_gpu_support() {
        let caps = probe_codec_support();
        eprintln!(
            "NVENC (Windows) probe: h264={} h265={} av1={}",
            caps.h264, caps.h265, caps.av1
        );
        assert!(
            caps.h264,
            "every NVENC generation encodes H.264 — a false here means the GUID enumeration \
             failed, which would narrow the host's codec advertisement"
        );
        let again = probe_codec_support();
        assert_eq!(
            (caps.h264, caps.h265, caps.av1),
            (again.h264, again.h265, again.av1),
            "the probe must be stable — it is cached once and drives every later negotiation"
        );
    }
}
