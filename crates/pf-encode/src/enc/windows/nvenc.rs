//! NVENC hardware encoder (Windows, D3D11 input) — zero-copy capture→encode on the GPU.
//!
//! Drives the raw NVENC API via the `nvidia_video_codec_sdk` `sys` types and a **runtime-loaded**
//! entry table ([`EncodeApi`] — the crate's `ENCODE_API`/safe `Encoder` are deliberately unused:
//! the safe wrapper is CUDA-only, and its statically-declared entry points would put a load-time
//! `nvEncodeAPI64.dll` import on the all-vendor binary, killing it on every AMD/Intel-only box).
//! Opens an encode session bound to the **same** `ID3D11Device` as the DXGI
//! capturer (the device is carried on `FramePayload::D3d11`), and **encodes the capturer's texture in
//! place** — it registers each input texture with NVENC once (cached by pointer) and `encode_picture`s
//! it directly, with NO per-frame `CopyResource`. (That's safe because the host encode loop is
//! synchronous — capture → submit → poll, where `poll`/`lock_bitstream` blocks until the encode
//! finishes — so the capturer never overwrites the texture mid-encode; if that loop ever becomes
//! pipelined, the capturer must hand a ring of textures.) Mirrors the Linux NVENC config: CBR +
//! ultra-low-latency, infinite GOP, P-frames only, forced-IDR for RFI, in-band SPS/PPS each keyframe.
//!
//! Needs a real NVIDIA GPU at runtime (session creation fails otherwise) — compiles GPU-less and
//! **starts driver-less** (the DLL resolves at runtime; on an AMD/Intel box [`try_api`] fails
//! cleanly and the AMF/QSV/software backends carry the session). The software encoder
//! (`super::sw`) is the fallback.
//!
//! **Two-thread async retrieve** (`PUNKTFUNK_NVENC_ASYNC=1`, opt-in until on-glass validated —
//! gpu-contention plan §5.B): the NVENC guide mandates that the main thread only *submit*
//! (`nvEncEncodePicture`) while a **secondary thread** waits on per-buffer completion events and
//! does `nvEncLockBitstream`. Today's sync mode does both on one thread, so under a GPU-saturating
//! game the whole pipeline serializes on the WDDM scheduling wait (`1000/17ms ≈ 59 fps` — the
//! depth-1 collapse). In async mode the session is opened `enableEncodeAsync=1`, each output
//! bitstream gets a registered auto-reset event, `submit` returns immediately, and an internal
//! retrieve thread waits + locks + copies + unlocks, handing finished AUs back through a channel
//! that `poll` drains without blocking. All input-resource calls (register/map/unmap) stay on the
//! encode thread; the retrieve thread touches ONLY the event + lock/unlock — the exact split the
//! guide blesses. Backpressure: `submit` blocks on the oldest completion when `POOL - 1` encodes
//! are in flight, so an output buffer is never reused mid-encode. Latency cost when idle ≈ 0 (the
//! AU completes within the same tick and `poll` picks it up); under contention completed frames
//! queue instead of stalling capture — throughput recovers up to the scheduler-granted share.

// Every `unsafe` block / impl in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use super::nvenc_core::{
    apply_low_latency_config, build_init_params, codec_guid, LowLatencyConfig, NvStatusExt, RFI_DPB,
};
use super::nvenc_status;
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
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

// ---------------------------------------------------------------------------------------------
// Runtime-loaded NVENC entry table.
//
// The NVENC entry points live in `nvEncodeAPI64.dll`, which exists ONLY where the NVIDIA driver
// is installed. They must be resolved at runtime (`LoadLibraryExW` + `GetProcAddress`), never as
// a link-time import: the shipped host binary compiles the `nvenc` feature in unconditionally,
// and a load-time DLL import makes the Windows loader refuse to start the process on every
// AMD/Intel-only box ("nvencodeapi64.dll was not found", before `main`) — `encode.rs` never gets
// the chance to dispatch to AMF/QSV. This is the Windows analogue of the Linux host's dlopen'd
// libcuda. Only the two real DLL exports are resolved by name; the rest of the table comes back
// through `NvEncodeAPICreateInstance`.
// ---------------------------------------------------------------------------------------------

/// The `NV_ENCODE_API_FUNCTION_LIST` entries this encoder uses, unwrapped once at load so call
/// sites stay `(api().encode_picture)(…)`. Field names mirror the sdk crate's `EncodeAPI`, whose
/// lazy static must NOT be referenced — it calls the statically-declared externs, which is what
/// demanded the import lib at link time.
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
    register_async_event:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_EVENT_PARAMS) -> nv::NVENCSTATUS,
    unregister_async_event:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_EVENT_PARAMS) -> nv::NVENCSTATUS,
    invalidate_ref_frames: unsafe extern "C" fn(*mut c_void, u64) -> nv::NVENCSTATUS,
}

/// Resolve the table once per process. `Err` = NVENC genuinely unavailable on this machine (no
/// NVIDIA driver/DLL, or a driver older than our headers) — the entry points
/// ([`NvencD3d11Encoder::open`], [`probe_can_encode_444`]) gate on it and the AMF/QSV/software
/// backends carry on.
fn try_api() -> std::result::Result<&'static EncodeApi, &'static str> {
    static TABLE: std::sync::OnceLock<std::result::Result<EncodeApi, String>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let table = load_api();
            if let Err(e) = &table {
                // Once per process. Only reachable when something resolved to NVENC on this box
                // (backend misdetect or a forced PUNKTFUNK_ENCODER=nvenc) — say why it will fail.
                tracing::warn!(error = %e, "NVENC API unavailable");
            }
            table
        })
        .as_ref()
        .map_err(|e| e.as_str())
}

/// The loaded table, for call sites past a [`try_api`] gate — a live session (or the probe's own
/// gate) implies the load succeeded, and the table lives for the process lifetime.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

fn load_api() -> std::result::Result<EncodeApi, String> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names; the
    // System32-only search path keeps a planted DLL out of the SYSTEM-service process. The two
    // transmutes cast the resolved exports to their documented prototypes (nvEncodeAPI.h), the
    // same contract the C SDK's own loader applies. `NvEncodeAPIGetMaxSupportedVersion` writes
    // one u32 through a live pointer; `NvEncodeAPICreateInstance` fills `list`, a stack-local
    // `#[repr(C)]` function list with `version` set, only during the call. The module is never
    // freed, so every extracted function pointer stays valid for the process lifetime.
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
        // The sdk's assert_versions_match, minus the panic: an older driver is a clean Err.
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

// Output bitstream buffers = max in-flight encodes. The helper deep-pipelines (submits several frames
// before locking the oldest) so per-frame GPU-scheduling waits OVERLAP instead of serializing under a
// GPU-saturating game; this must be ≥ the helper's `PUNKTFUNK_ENCODE_DEPTH` (default 4, clamped ≤ 6).
const POOL: usize = 8;

/// Live NVENC hardware-session units held by THIS host process (a plain session = 1; a forced
/// split-encode session occupies one session per engine = 2–3) — the Stage-W3 encoder budget
/// (`design/windows-parallel-virtual-displays.md` §4.5). Kept in ONE place so admitting a parallel
/// display consults the same accounting every open/teardown maintains; other processes' sessions
/// aren't visible here, but our own consumption is the deterministic part we can enforce
/// fail-closed at admission.
static LIVE_SESSION_UNITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The NVENC concurrent-session cap to budget against: GeForce (consumer) drivers allow 8
/// concurrent encode sessions since R550 (pro cards are effectively unlimited).
/// `PUNKTFUNK_NVENC_MAX_SESSIONS` overrides for older drivers / known-different cards.
fn session_cap() -> u32 {
    std::env::var("PUNKTFUNK_NVENC_MAX_SESSIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

/// Whether one more (plain, non-split) encode session fits the NVENC budget — consulted by
/// admission before admitting a parallel display (`vdisplay::admission`). On a box that never
/// opened NVENC (AMD/Intel/none) the count is 0 and this always passes — the budget seam is
/// NVENC-only until the AMF/QSV equivalents grow their own accounting.
pub(crate) fn can_open_another_session() -> bool {
    LIVE_SESSION_UNITS.load(std::sync::atomic::Ordering::Relaxed) < session_cap()
}

/// Session-unit weight of a chosen split-encode mode (one hardware session per engine).
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

/// Whether the operator asked for the two-thread async retrieve (`PUNKTFUNK_NVENC_ASYNC` truthy).
/// Combined with the GPU's `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT` in `init_session`. Opt-in until
/// on-glass validated; note an async-rejecting config surfaces as a failed session open — unset
/// the env in that case.
fn async_retrieve_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_ASYNC")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Max encodes in flight in async mode (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4, clamped
/// `2..=POOL-1`). Two independent ceilings meet here: the output-bitstream pool (hard, `POOL-1` —
/// a buffer must never be reused mid-encode) and the capturer's texture ring (soft — NVENC encodes
/// the ring textures in place, so in-flight depth beyond the ring lets the capturer overwrite a
/// frame mid-encode: visual corruption, not UB). IDD-push rings are sized around
/// `PUNKTFUNK_IDD_DEPTH`; raise both together if deeper pipelining is needed.
fn async_inflight_cap() -> usize {
    std::env::var("PUNKTFUNK_NVENC_ASYNC_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, POOL - 1)
}

/// One in-flight encode handed to the retrieve thread: the output bitstream to lock once its
/// completion `event` signals. Raw pointers travel as `usize` (the addresses are process-global
/// driver handles; the thread is joined before the session they belong to is destroyed).
struct RetrieveJob {
    bs: usize,
    event: usize,
}

/// A finished retrieve: the locked-and-copied AU (or the retrieve-side error) for the oldest
/// in-flight bitstream. `bs` lets the encode thread cross-check FIFO pairing with `pending`.
struct RetrieveDone {
    bs: usize,
    result: std::result::Result<(Vec<u8>, bool), String>,
}

/// The async-retrieve runtime: the job channel feeding the retrieve thread, the completion channel
/// back, the thread handle (joined in `teardown` BEFORE the session is destroyed), and AUs already
/// absorbed by backpressure that `poll` hands out first.
struct AsyncRetrieve {
    work_tx: Option<mpsc::SyncSender<RetrieveJob>>,
    done_rx: mpsc::Receiver<RetrieveDone>,
    join: Option<std::thread::JoinHandle<()>>,
    ready: VecDeque<EncodedFrame>,
}

/// The retrieve-thread body (gpu-contention plan §5.B): for each submitted frame, wait on its
/// completion event, lock the bitstream, copy the AU out, unlock, and send it back. Exits when the
/// job channel closes (teardown drops the sender and joins BEFORE destroying the session, so
/// `enc`/`bs`/`event` outlive every use here). Touches ONLY the event wait + lock/unlock — the
/// NVENC threading model's sanctioned secondary-thread surface.
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    while let Ok(job) = work_rx.recv() {
        // SAFETY: `job.event` is one of the auto-reset events `init_session` created and
        // registered for exactly this session, and `job.bs` one of its pool bitstreams; both stay
        // valid until `teardown`, which joins this thread first. `WaitForSingleObject` takes the
        // handle by value. On WAIT_OBJECT_0 the driver has completed the encode into `job.bs`, so
        // `lock_bitstream` (version set, struct a live stack local for the synchronous call)
        // yields a CPU-readable `bitstreamBufferPtr`/`bitstreamSizeInBytes` valid until
        // `unlock_bitstream`; the slice is copied (`to_vec`) before the unlock on the same buffer.
        // Lock/unlock from a secondary thread while the encode thread submits is the NVENC
        // guide's documented threading model.
        let result = unsafe {
            if WaitForSingleObject(HANDLE(job.event as *mut c_void), 5000) != WAIT_OBJECT_0 {
                Err("NVENC completion event timeout (5s) — encoder wedged?".to_string())
            } else {
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
            break; // encoder side gone (teardown drains us via join)
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
    /// Full-chroma 4:4:4 (HEVC Range Extensions, `chroma_format_idc = 3`) requested for this session.
    /// NVENC ingests the RGB (ARGB/ABGR10) input and CSCs it to YUV444 internally; the `FREXT` profile
    /// and `chromaFormatIDC = 3` in the encode config carry the chroma. Gated on the GPU's
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` (cleared in `query_caps` on a card that lacks it) and on an
    /// RGB input format (NV12/P010 capture can't reconstruct 4:4:4). HEVC-only.
    chroma_444: bool,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE` from the caps probe — whether this GPU can 4:4:4 encode at
    /// all. `chroma_444` is forced off when this is false (graceful downgrade to 4:2:0).
    yuv444_supported: bool,
    /// HDR: the capturer is delivering BT.2020 PQ 10-bit (`PixelFormat::Rgb10a2`) frames. Sets the
    /// `ABGR10` input format + the BT.2020/PQ colour VUI. Derived per-frame from the capture format
    /// (HDR can toggle mid-session); a change re-inits the session.
    hdr: bool,
    /// The source's static HDR mastering metadata (from the capturer's `GetDesc1`), emitted as
    /// in-band SEI (`mastering_display_colour_volume` + `content_light_level_info`) on each keyframe
    /// when `hdr`. `None` = unknown → no SEI (the VUI still signals BT.2020 PQ). Set per-frame via
    /// [`Encoder::set_hdr_meta`], so a mid-session regrade is picked up on the next keyframe.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Registrations of the capturer's input textures, cached by texture raw pointer — NVENC encodes
    /// them in place (no per-frame copy). The cloned `ID3D11Texture2D` keeps each alive until we
    /// unregister it (the capturer may drop its copy on a device recreate before our teardown runs).
    regs: HashMap<isize, (nv::NV_ENC_REGISTERED_PTR, ID3D11Texture2D)>,
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    /// Async mode: the registered completion event per pool bitstream (raw `HANDLE` as `usize`,
    /// parallel to `bitstreams`); empty in sync mode. Unregistered + closed in `teardown`.
    events: Vec<usize>,
    /// Async mode: the retrieve thread + its channels (`None` = classic same-thread sync retrieve).
    async_rt: Option<AsyncRetrieve>,
    /// The capturer's `pipeline_depth` (`set_input_ring_depth`). This backend encodes the
    /// capturer's textures IN PLACE, so it is a HARD ceiling on async in-flight depth: the
    /// capturer rotates its ring per delivered frame regardless of encode completion, so
    /// pipelining deeper lets it overwrite a texture mid-encode (torn frames). `None` until the
    /// session glue reports it — treated as "unknown, don't pipeline past the env cap".
    input_ring_depth: Option<usize>,
    /// `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT` from the caps probe — gates the async retrieve mode.
    async_supported: bool,
    /// (bitstream, mapped input resource to unmap after retrieval, pts_ns, recovery-anchor) per
    /// in-flight encode. The fourth field tags the first frame encoded after a successful
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) — the clean re-anchor P-frame the
    /// client lifts its post-loss freeze on (see [`EncodedFrame::recovery_anchor`]).
    pending: VecDeque<(nv::NV_ENC_OUTPUT_PTR, nv::NV_ENC_INPUT_PTR, u64, bool)>,
    /// The frame number of the NEXT submission (also its `inputTimeStamp`). Pinned per frame by
    /// [`Encoder::submit_indexed`] to the WIRE frame index the AU will carry, so the DPB timestamps
    /// `invalidate_ref_frames` compares client frame numbers against stay 1:1 with the wire across
    /// encoder rebuilds/resets (an internal counter desyncs on the first adaptive-bitrate rebuild —
    /// RFI then never matches again). Self-increments as a fallback for un-indexed callers (tests).
    frame_idx: i64,
    force_kf: bool,
    /// A successful [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) arms this; the next
    /// `submit` consumes it into `pending` so that AU ships as the recovery anchor. NVENC applies
    /// the invalidation at the next `encode_picture`, so that frame is by construction the first
    /// one coded against only-valid references — without tagging it the client's freeze can only
    /// lift on an IDR, which the session glue suppresses after an RFI success (the cooldown):
    /// a ~1 s frozen stall per loss event on NVIDIA hosts.
    pending_anchor: bool,
    inited: bool,
    /// GPU capabilities probed once via `nvEncGetEncodeCaps` before configuring (Apollo's
    /// `get_encoder_cap`): gates 10-bit/custom-VBV/RFI on what this card actually supports instead
    /// of failing later as an opaque `InvalidParam`. Set by [`query_caps`](Self::query_caps).
    rfi_supported: bool,
    custom_vbv: bool,
    /// The split-encode mode + async-retrieve flag the live session was initialized with —
    /// `reconfigure_bitrate` must present the SAME init params as the open (only the config's
    /// rate fields may move). Meaningless while `inited` is false.
    split_mode: u32,
    session_async: bool,
    /// The last reference-frame range we invalidated — dedupes repeated RFI requests for the same
    /// loss event (the client resends until it sees recovery).
    last_rfi_range: Option<(i64, i64)>,
    /// Raw ptr of the D3D11 device this session was initialized with. The capturer recreates the
    /// device on a desktop switch (normal ↔ Winlogon secure); when a frame carries a new device we
    /// tear down and re-init NVENC against it.
    init_device: *mut c_void,
    /// The hardware-session units THIS encoder holds against [`LIVE_SESSION_UNITS`] (1 plain, 2–3
    /// under forced split-encode — a split session occupies one session per engine). `0` while no
    /// session is open; set by `init_session`, returned by `teardown`.
    session_units: u32,
}

// SAFETY: the `!Send` fields are the raw NVENC session/device handles (`encoder`, `init_device`),
// the raw NVENC bitstream/registered/mapped pointers carried in `bitstreams`/`regs`/`pending`, and
// the `ID3D11Texture2D` COM refs — none of which may be touched concurrently from two threads
// EXCEPT along the NVENC guide's sanctioned split. The encoder object is owned by exactly one
// thread: it is moved onto the host encode thread once at construction, and every method
// (`submit`/`poll`/`invalidate_ref_frames`/`Drop`) runs there. In async mode the internal retrieve
// thread additionally calls `WaitForSingleObject`/`lock_bitstream`/`unlock_bitstream` on the same
// session — the exact two-thread model the NVENC API documents as thread-safe (submit-side vs
// output-side); it never touches registrations, mappings, or D3D11. `teardown` joins that thread
// BEFORE destroying the session, so no retrieve call can outlive the handles. Moving the encoder
// across its single ownership-transfer boundary is sound because no NVENC/D3D11 call is in flight
// during the move — so `Send` introduces no data race on the non-`Send` fields.
unsafe impl Send for NvencD3d11Encoder {}

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
    ) -> Result<Self> {
        // The runtime DLL load is the real "is NVENC possible here" gate: fail the open with a
        // clear reason (backend misdetect / forced PUNKTFUNK_ENCODER=nvenc on a non-NVIDIA box)
        // instead of an opaque session error on the first frame. Every later NVENC call in this
        // file sits behind this gate (or the probe's), so the infallible `api()` is sound.
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
            // 4:4:4 is HEVC-only; the GPU-support gate is applied in `query_caps`.
            chroma_444: chroma.is_444() && codec == Codec::H265,
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
            pending: VecDeque::new(),
            frame_idx: 0,
            force_kf: false,
            pending_anchor: false,
            inited: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32,
            session_async: false,
            last_rfi_range: None,
            init_device: ptr::null_mut(),
            session_units: 0,
        })
    }

    /// Tear down the encode session + pooled resources. Reused on a capture-device change (desktop
    /// switch) and at Drop.
    unsafe fn teardown(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        // Async mode: retire the retrieve thread FIRST — drop the job sender so it finishes every
        // queued job (each references the still-live session) and exits, then join. Only after the
        // join is it sound to unmap/destroy anything the thread might have been touching.
        if let Some(mut rt) = self.async_rt.take() {
            drop(rt.work_tx.take());
            if let Some(j) = rt.join.take() {
                let _ = j.join();
            }
            // Completions the thread produced that poll() never absorbed — their AUs are dropped
            // (the session is going away), but the FIFO pairing stands, so nothing extra to do
            // beyond the pending unmap below.
            while rt.done_rx.try_recv().is_ok() {}
        }
        // Unmap any in-flight inputs, then unregister every cached texture and destroy the bitstreams.
        for (_, map, _, _) in &self.pending {
            if !map.is_null() {
                let _ = (api().unmap_input_resource)(self.encoder, *map);
            }
        }
        for (reg, _tex) in self.regs.values() {
            let _ = (api().unregister_resource)(self.encoder, *reg);
        }
        // Async events: unregister from the session, then close the Win32 handles.
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
        // Return this session's units to the budget (see LIVE_SESSION_UNITS).
        LIVE_SESSION_UNITS.fetch_sub(self.session_units, std::sync::atomic::Ordering::Relaxed);
        self.session_units = 0;
        self.regs.clear(); // drops the texture clones, releasing our refs
        self.bitstreams.clear();
        self.pending.clear();
        self.encoder = ptr::null_mut();
        self.inited = false;
        self.next = 0;
        // The new session starts with an empty DPB (its first frame is an IDR), so any prior
        // invalidation range is meaningless against it — and the IDR is itself the re-anchor,
        // so a pending anchor tag from a pre-teardown RFI is stale too.
        self.last_rfi_range = None;
        self.pending_anchor = false;
    }

    /// Query one `NV_ENC_CAPS` value for this codec on an open session; 0 on any error (treat an
    /// unqueryable cap as "unsupported", the conservative choice).
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

    /// Probe this GPU's real capabilities once (Apollo's `get_encoder_cap`) before the bitrate-probe
    /// loop configures the session: opens a throwaway session, queries the codec's max dimensions +
    /// 10-bit / custom-VBV / ref-pic-invalidation support, destroys it. Rejects an out-of-range mode
    /// up front with a clear error, downgrades 10-bit→8-bit when unsupported, and records the
    /// RFI/custom-VBV flags the config + [`invalidate_ref_frames`](Encoder::invalidate_ref_frames)
    /// gate on. Without this, an unsupported config surfaces only as an opaque `InvalidParam` that
    /// the bitrate-clamp search misreads as "bitrate too high" and binary-searches into the floor.
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
        let _ = (api().destroy_encoder)(enc);

        // Reject an over-range mode with a clear message instead of an opaque InvalidParam.
        if wmax > 0 && hmax > 0 && (self.width as i32 > wmax || self.height as i32 > hmax) {
            bail!(
                "this GPU's NVENC max encode size for {:?} is {wmax}x{hmax}; client requested \
                 {}x{} (lower the client resolution or use a codec/GPU that supports it)",
                self.codec,
                self.width,
                self.height
            );
        }
        // Degrade gracefully rather than fail: no 10-bit encode on this card → 8-bit SDR.
        if self.bit_depth >= 10 && ten_bit == 0 {
            tracing::warn!("NVENC: this GPU can't 10-bit encode — falling back to 8-bit SDR");
            self.bit_depth = 8;
            self.hdr = false;
        }
        // Same for 4:4:4: a card without YUV444 encode falls back to 4:2:0. (The host already probed
        // this via `probe_can_encode_444` before the Welcome, so this is a belt-and-braces guard.)
        self.yuv444_supported = yuv444 != 0;
        if self.chroma_444 && !self.yuv444_supported {
            tracing::warn!("NVENC: this GPU can't 4:4:4 encode — falling back to 4:2:0");
            self.chroma_444 = false;
        }
        self.rfi_supported = rfi != 0;
        self.custom_vbv = custom_vbv != 0;
        self.async_supported = async_enc != 0;
        tracing::info!(
            rfi = self.rfi_supported,
            custom_vbv = self.custom_vbv,
            async_encode = self.async_supported,
            max = %format!("{wmax}x{hmax}"),
            ten_bit = ten_bit != 0,
            "NVENC capabilities probed"
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
        .map_err(|e| nvenc_status::call_err("get_encode_preset_config_ex", e))?;
        let mut cfg = preset.presetCfg;

        // Steps 3-7 (RC/VBV, tier+level, chroma+bit-depth, colour VUI, RFI DPB) are the shared
        // low-latency contract. On Windows the full-chroma input is a packed-RGB surface (NVENC
        // CSCs it internally under FREXT), and AV1's input-depth follows the surface format — 10-bit
        // for an ABGR10 / YUV420_10BIT input, else 8-bit.
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
            },
        );
        Ok(cfg)
    }

    /// Open + configure + initialize ONE NVENC session at `bitrate` (bps) and `split_mode`. Returns
    /// the session handle, or destroys it and returns the error. NVENC has no re-init after a failed
    /// `initialize_encoder`, so the bitrate-clamp search in `init_session` calls this once per probe.
    unsafe fn try_open_session(
        &self,
        device: &ID3D11Device,
        bitrate: u64,
        split_mode: u32,
        enable_async: bool,
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
            enable_async,
        );

        match (api().initialize_encoder)(enc, &mut init).nv_ok() {
            Ok(()) => Ok(enc),
            Err(e) => {
                let _ = (api().destroy_encoder)(enc);
                Err(nvenc_status::call_err("initialize_encoder", e))
            }
        }
    }

    /// Lazily create the session on the first frame's D3D11 device (so capture + encode share it).
    fn init_session(&mut self, device: &ID3D11Device) -> Result<()> {
        // SAFETY: every call below goes through a function pointer resolved once from the
        // runtime-loaded [`EncodeApi`] table (`api()`, gated in `open`), or through this type's own
        // `unsafe fn`s whose contract is met here. `query_caps`/`try_open_session` receive `device`,
        // the live `ID3D11Device` the caller pulled off the first frame; each returns either a valid
        // open NVENC session handle or an `Err`. `destroy_encoder` is only ever called on a handle a
        // `try_open_session` just returned (and `best` only when `!best.is_null()`), so it never frees
        // a dangling or null session. `create_bitstream_buffer` is passed `enc` — the one chosen live
        // session — and `&mut cb`, a `#[repr(C)] NV_ENC_CREATE_BITSTREAM_BUFFER` whose `version` is set
        // to `NV_ENC_CREATE_BITSTREAM_BUFFER_VER`; `cb` lives across the synchronous call and its
        // returned `bitstreamBuffer` is copied into `self.bitstreams` before `cb` drops. No handle
        // escapes the encode thread.
        unsafe {
            // Probe real GPU caps first (max dims / 10-bit / custom-VBV / RFI) so the config below is
            // gated on what this card supports and an out-of-range mode fails with a clear error
            // rather than being misread as a too-high bitrate by the clamp search.
            self.query_caps(device)?;
            // Bitrate clamp (see the search below): NVENC rejects `initialize_encoder` when the bitrate
            // exceeds the GPU's max codec level. We try the requested rate, then binary-search down to
            // the MAX the level accepts and clamp to it — so an over-asking client (e.g. 1 Gbps on HEVC)
            // gets the highest the GPU can actually do, not a coarse fraction of it.
            const FLOOR_BPS: u64 = 10_000_000;
            let requested_bps = self.bitrate_bps;
            // 2-way NVENC split-frame encoding (Ada dual-NVENC) — the high-pixel-rate throughput lever
            // the Linux host enables via libavcodec `split_encode_mode`. A single Ada NVENC session tops
            // out ~0.8 Gpix/s, so at high motion a 5K@240 (1.77 Gpix/s) frame takes ~8 ms to encode and
            // the rate caps ~125 fps; splitting across both engines roughly halves that. Force 2-way
            // above ~1 Gpix/s (matching encode/linux.rs), AUTO below (the ~2% BD-rate cost isn't worth
            // it at low pixel rates). Env override PUNKTFUNK_SPLIT_ENCODE = 0/disable | 1/auto | 2 | 3.
            // HEVC/AV1 only; the init-failure fallback below disables it if a codec/config rejects it.
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
                // Main10 (10-bit / HDR): 2-way split is measurably SLOWER on Ada — at 5120x1440@240
                // Main10, forced-2 took 7.6 ms/frame (~131 fps) vs 2.8 ms (~357 fps) single-engine
                // (the split/merge overhead dominates for 10-bit). A single Ada NVENC engine already
                // handles 5K@240 Main10 well under the 4.17 ms budget, so DON'T split — splitting was
                // the "broken animations in HDR" (the stream capped at ~131 fps). Env still overrides.
                _ if self.bit_depth >= 10 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32
                }
                _ if pixel_rate > 1_000_000_000 => {
                    nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
                }
                _ => nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_MODE as u32,
            };
            tracing::debug!(
                split_mode,
                bit_depth = self.bit_depth,
                pixel_rate,
                "NVENC split-encode mode selected"
            );
            // Find the highest bitrate the GPU's codec LEVEL accepts and CLAMP to it. NVENC rejects
            // `initialize_encoder` (InvalidParam) when the bitrate exceeds the level ceiling (e.g. a
            // 1 Gbps request on HEVC). Strategy: try the requested rate; if the only problem is a forced
            // split-encode mode the codec doesn't support, disable split and retry; if the bitrate
            // itself is too high, binary-search [FLOOR, requested] for the MAX accepted rate and clamp
            // to THAT (don't undershoot — the old ×¾ step-down landed well below the real ceiling).
            const CLAMP_TOL_BPS: u64 = 20_000_000; // stop bisecting within ~20 Mbps of the ceiling

            // Two-thread async retrieve: operator opt-in AND the GPU reports async-encode support
            // (query_caps above). Threaded into every session-open probe so the chosen session is
            // built in the right mode from the start.
            let use_async = self.async_supported && async_retrieve_requested();

            let mut probe = self.try_open_session(device, requested_bps, split_mode, use_async);
            // Disambiguate a forced-split rejection from a bitrate-cap rejection: retry once at the
            // requested rate with split disabled — if THAT succeeds, split was the problem, not bitrate.
            // ANY non-disabled mode can be the rejection — AUTO included: AV1 rejects the whole
            // init with INVALID_PARAM on drivers/configs where auto split isn't valid for it,
            // which then masqueraded as a bitrate cap and failed "even at the floor".
            let split_on =
                split_mode != nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
            if probe.is_err() && split_on {
                let no_split = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                if let Ok(e) = self.try_open_session(device, requested_bps, no_split, use_async) {
                    tracing::warn!("NVENC: split-encode rejected by codec/config — disabled");
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
                    // `lo` is the highest known-good rate (FLOOR is assumed to fit), `hi` the lowest
                    // rejected; `best` holds the live session at `lo` so we end up with the clamped one.
                    let mut lo = FLOOR_BPS;
                    let mut hi = requested_bps;
                    let mut best: *mut c_void = ptr::null_mut();
                    let mut best_bps = 0u64;
                    while hi > lo + CLAMP_TOL_BPS {
                        let mid = lo + (hi - lo) / 2;
                        match self.try_open_session(device, mid, split_mode, use_async) {
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
                        // Nothing in (FLOOR, requested] accepted — fall back to the floor itself, also
                        // trying split-disabled in case a forced split (not the bitrate) is the blocker.
                        let no_split =
                            nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
                        best = self
                            .try_open_session(device, FLOOR_BPS, split_mode, use_async)
                            .or_else(|_| {
                                self.try_open_session(device, FLOOR_BPS, no_split, use_async)
                            })
                            .context(
                                "NVENC initialize_encoder rejected even at the floor bitrate",
                            )?;
                        best_bps = FLOOR_BPS;
                    }
                    tracing::warn!(
                        requested_mbps = requested_bps / 1_000_000,
                        clamped_mbps = best_bps / 1_000_000,
                        "NVENC: requested bitrate above the GPU codec-level ceiling — clamped to the max accepted"
                    );
                    self.bitrate_bps = best_bps;
                    best
                }
            };
            self.encoder = enc;
            // Session init params a later `reconfigure_bitrate` must re-present verbatim. (Best
            // effort: the floor fallback above may have succeeded split-disabled without updating
            // `split_mode` — a reconfigure then presents the forced mode, NVENC rejects it, and
            // the caller's rebuild fallback covers the mismatch.)
            self.split_mode = split_mode;
            self.session_async = use_async;
            // Session-budget accounting (Stage W3): record what this open holds so admission can
            // decline a parallel display the hardware can't afford. Weighted by the FINAL split
            // mode (a split session occupies one hardware session per engine).
            self.session_units = split_mode_units(split_mode);
            LIVE_SESSION_UNITS.fetch_add(self.session_units, std::sync::atomic::Ordering::Relaxed);
            // (The clamp path above already logs the requested→clamped bitrate at warn; no second
            // info line for the same event here.)

            // 5. one output bitstream per in-flight slot. There is NO encoder-owned input pool: the
            // capturer's textures are registered on demand in `submit` and encoded in place.
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
            // Async retrieve: one auto-reset completion event per pool bitstream, registered with
            // the session, plus the retrieve thread the events signal. The thread only ever sees
            // raw addresses; `teardown` joins it before any of them die.
            if use_async {
                for _ in 0..POOL {
                    let ev = CreateEventW(None, false, false, PCWSTR::null())
                        .context("CreateEvent (NVENC completion)")?;
                    let mut ep = nv::NV_ENC_EVENT_PARAMS {
                        version: nv::NV_ENC_EVENT_PARAMS_VER,
                        completionEvent: ev.0,
                        ..Default::default()
                    };
                    (api().register_async_event)(enc, &mut ep)
                        .nv_ok()
                        .map_err(|e| nvenc_status::call_err("register_async_event", e))?;
                    self.events.push(ev.0 as usize);
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
                "NVENC D3D11 session: {}x{}@{} {}-bit{} {} Mbps {:?}",
                self.width,
                self.height,
                self.fps,
                self.bit_depth,
                if self.hdr { " HDR(BT.2020 PQ)" } else { "" },
                self.bitrate_bps / 1_000_000,
                self.codec_guid
            );
            Ok(())
        }
    }

    /// Fold one retrieve-thread completion back into encoder state ON THE ENCODE THREAD: pop the
    /// oldest `pending` entry (completions are FIFO — one retrieve thread, in-order jobs), verify
    /// the bitstream pairing, unmap the input resource, and queue the AU for `poll`. A retrieve
    /// error surfaces AFTER the unmap (the resource is retired either way) so the session glue's
    /// rebuild path starts from clean state.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some((bs, map, pts_ns, anchor)) = self.pending.pop_front() else {
            bail!("NVENC async: completion with no in-flight frame (pairing bug)");
        };
        if bs as usize != done.bs {
            bail!("NVENC async: completion out of order (pairing bug)");
        }
        // SAFETY: `map` is the mapped input `submit` recorded for exactly this now-completed
        // encode; the session is live (`async_rt` exists only between `init_session` and
        // `teardown`) and this runs on the encode thread — the single unmap here mirrors the sync
        // path's poll-side unmap, exactly once per mapping.
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
                bail!("NVENC D3D11 encoder needs a GPU texture frame (use the software encoder for CPU frames)")
            }
        };
        // The capturer recreates its D3D11 device on a desktop switch (secure/Winlogon) and may come
        // back at a different resolution (user session applies its own mode on login). Re-init when the
        // frame arrives on a different device OR at a different size than our session was built on.
        // HDR (BT.2020 PQ 10-bit) when the capturer hands us a 10-bit R10G10B10A2 frame. This can flip
        // mid-session when the user toggles HDR (which arrives as a capture device recreate anyway).
        // HDR (BT.2020 PQ) when the capturer hands a 10-bit frame — either R10G10B10A2 (the legacy
        // shader path) or P010 (the video-processor path). 8-bit NV12/ARGB → SDR.
        let hdr = matches!(captured.format, PixelFormat::Rgb10a2 | PixelFormat::P010);
        let dev_raw = frame.device.as_raw();
        let size_changed =
            self.inited && (self.width != captured.width || self.height != captured.height);
        let hdr_changed = self.inited && self.hdr != hdr;
        if self.inited && (self.init_device != dev_raw || size_changed || hdr_changed) {
            tracing::info!(
                device_changed = self.init_device != dev_raw,
                size_changed,
                hdr_changed,
                hdr,
                new = format!("{}x{}", captured.width, captured.height),
                "NVENC: capture device/size/HDR changed — re-initializing session"
            );
            // SAFETY: `teardown` (an `unsafe fn`) requires the encode thread with no NVENC call in
            // flight and a session whose cached regs/bitstreams/pending all belong to `self.encoder`.
            // All hold: this is the synchronous encode thread, `self.inited` so `self.encoder` is the
            // live session every cached resource was created against, and the previous frame's encode
            // has already been polled (synchronous submit→poll), so nothing is mid-encode.
            unsafe { self.teardown() };
        }
        if !self.inited {
            // Adopt the current frame size + colour so the encoder always matches the capturer output.
            self.width = captured.width;
            self.height = captured.height;
            self.hdr = hdr;
            // Pick the NVENC input format from the captured pixel format. YUV (NV12/P010) is the
            // video-processor path — NVENC encodes it natively (no internal RGB→YUV, which is a hidden
            // 3D/compute step that would fight a GPU-saturating game). RGB (ARGB/ABGR10) is the legacy
            // shader path. 10-bit (P010/ABGR10) forces HEVC Main10 + the BT.2020 PQ VUI.
            self.buffer_fmt = match captured.format {
                PixelFormat::P010 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
                }
                PixelFormat::Rgb10a2 => {
                    self.bit_depth = 10;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
                }
                PixelFormat::Nv12 => {
                    // NV12 is 8-bit 4:2:0. Force 8-bit so a transition from a prior P010 (10-bit) session
                    // — or a 10-bit-negotiated client on an SDR display — re-inits at the matching depth.
                    // Unlike ARGB (which NVENC upconverts to Main10), NV12 cannot feed a 10-bit session:
                    // `register_resource` rejects it as InvalidParam (the HDR→SDR-toggle stream drop).
                    self.bit_depth = 8;
                    nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
                }
                _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            };
            // 4:4:4 honesty: the FREXT/chromaFormatIDC=3 config engages only on an RGB input (a
            // subsampled NV12/P010 source can't reconstruct full chroma). If the capturer handed
            // native YUV despite a 4:4:4 negotiation, this session encodes 4:2:0 — clear the flag
            // NOW so `caps().chroma_444` (and native's post-open cross-check) reports what
            // the stream really carries instead of silently claiming full chroma.
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
            // `init_session` publishes `self.encoder` (and charges LIVE_SESSION_UNITS) BEFORE its
            // last fallible steps, so a failure there leaves a live session with `inited == false`.
            // Every guard on the re-init path keys off `inited`, so without this the next submit
            // would skip teardown and overwrite `self.encoder` — leaking the session permanently
            // (toward the driver's per-process cap) along with its session-budget units.
            // `teardown` keys off `encoder.is_null()`, not `inited`, so it cleans up exactly this
            // half-built state and is a no-op when nothing was opened.
            if let Err(e) = self.init_session(&device) {
                // SAFETY: same contract as the teardown above — the encode thread owns the session,
                // and a failed init leaves nothing mid-encode to race with.
                unsafe { self.teardown() };
                return Err(e);
            }
            self.init_device = dev_raw;
        }
        // The session's opening frame — NVENC emits it as an IDR regardless of pic flags, so the
        // in-band HDR SEI must ride it too. Detected via the still-empty output slot counter
        // (`teardown` zeroes it), NOT via `pts == 0`: `submit_indexed` pins pts to the wire frame
        // index, which is non-zero on a mid-session encoder rebuild's first frame.
        let opening = self.next == 0;
        // Async backpressure: never hand NVENC an output bitstream that is still in flight, and
        // keep in-flight depth within the capturer's texture ring. At the cap, block on the OLDEST
        // completion (the retrieve thread is already waiting on its event) before submitting more —
        // bounding depth exactly like the sync path's per-tick blocking poll, just `cap` deep
        // instead of 1.
        //
        // The ring term is the one that matters for correctness: `async_inflight_cap()` is only the
        // output-bitstream-pool ceiling plus an env knob, and consults NOTHING about the capturer,
        // despite this comment previously claiming otherwise. Since this backend encodes the
        // capturer's textures in place, exceeding the capturer's declared `pipeline_depth` lets it
        // rotate a texture out from under a live encode — torn frames, silently.
        let cap = match self.input_ring_depth {
            Some(d) => async_inflight_cap().min(d.max(1)),
            None => async_inflight_cap(),
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
        // SAFETY: every NVENC call goes through a function pointer from the runtime-loaded `EncodeApi` table
        // and takes `self.encoder`, the live session `init_session` just established (non-null on the
        // path that reaches here). `NV_ENC_REGISTER_RESOURCE rr` has `version =
        // NV_ENC_REGISTER_RESOURCE_VER` and registers `frame.texture` — a D3D11 texture from
        // `frame.device`, which is the SAME device the session was opened against (any device change
        // tears down and re-inits above, so `init_device == frame.device.as_raw()` here); the cloned
        // `ID3D11Texture2D` is kept alive in `regs` so NVENC's registration never outlives the texture.
        // `mp` (`NV_ENC_MAP_INPUT_RESOURCE`, version set) maps that registration and the map is recorded
        // in `pending` to be unmapped exactly once in `poll`/`teardown`. `pic` (`NV_ENC_PIC_PARAMS`,
        // version set) points `inputBuffer` at `mp.mappedResource` and `outputBitstream` at the live
        // pool bitstream `bitstreams[slot]`; the optional SEI scratch (`mastering_sei`/`cll_sei` and the
        // `sei` Vec whose `as_mut_ptr()` is written into the codec union) are stack locals that outlive
        // the synchronous `encode_picture`. Every `#[repr(C)]` param is a live local borrowed `&mut`
        // for the duration of its one synchronous call. (In-place encode without `CopyResource` is
        // sound because the encode loop is synchronous, as the module docs state.)
        unsafe {
            // Register the capturer's texture with NVENC once (cached by raw pointer), then encode it
            // IN PLACE — no `CopyResource` into an encoder-owned pool. This is the zero-copy win: the
            // capturer already produced a stable GPU texture; we just register + map + encode it.
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
            // Recovery anchor (armed by a successful invalidate_ref_frames): THIS frame is the
            // first one encoded after the invalidation — the clean re-anchor. A simultaneous
            // forced IDR is itself the re-anchor, so the tag is dropped in that case.
            let anchor = std::mem::take(&mut self.pending_anchor) && flags == 0;
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
                // Async mode: the event the driver signals when this encode completes (the
                // retrieve thread waits on it). Null in sync mode (`events` is empty).
                completionEvent: self
                    .events
                    .get(slot)
                    .map(|&e| e as *mut c_void)
                    .unwrap_or(ptr::null_mut()),
                ..Default::default()
            };

            // In-band HDR10 SEI on every IDR (a forced keyframe, or the first frame NVENC opens with):
            // `mastering_display_colour_volume` (ST.2086) + `content_light_level_info` (CEA-861.3),
            // built from the source display's metadata. Any decoder — incl. stock Moonlight — then
            // tone-maps from the real grade. HEVC/H.264 carry SEI; AV1 uses metadata OBUs (follow-up).
            // The scratch buffers must outlive `encode_picture`, so they live in this scope.
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
                // Writing a union field is safe; the pointers/len are read during encode_picture.
                match self.codec {
                    Codec::H265 => {
                        pic.codecPicParams.hevcPicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.hevcPicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    Codec::H264 => {
                        pic.codecPicParams.h264PicParams.seiPayloadArray = sei.as_mut_ptr();
                        pic.codecPicParams.h264PicParams.seiPayloadArrayCnt = sei.len() as u32;
                    }
                    // AV1 mastering/CLL ride METADATA OBUs, not SEI — separate follow-up.
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
            ));
            // Async: hand the in-flight encode to the retrieve thread (channel capacity = POOL ≥
            // in-flight, so this send never blocks). The pending entry above pairs with its
            // completion FIFO in `absorb_done`.
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

    /// Pin this submission's frame number (= its `inputTimeStamp`) to the wire frame index the AU
    /// will carry, so the DPB timestamps `invalidate_ref_frames` matches client frame numbers
    /// against are the wire's — 1:1 across every rebuild/reset (see the trait doc). Within a
    /// session the loop's prediction is nondecreasing; a repeat after a reset lands on a fresh
    /// session (teardown cleared the DPB and `last_rfi_range`), so re-pinning is always sound.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn set_input_ring_depth(&mut self, depth: usize) {
        // This backend registers and encodes the capturer's textures in place (no CopyResource),
        // so the capturer's ring depth is a hard ceiling on how deep async may pipeline.
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
        // RFI is probed once at open (`rfi_supported`); HDR SEI rides keyframes whenever the
        // session is in HDR mode. Both are the real capabilities the session glue routes on.
        EncoderCaps {
            supports_rfi: self.rfi_supported,
            // In-band mastering/CLL is attached as keyframe SEI on HEVC/H.264 only — AV1 carries
            // it in METADATA OBUs (`HDR_MDCV`/`HDR_CLL`), which this backend doesn't emit yet
            // (see `submit`); the grade still reaches punktfunk clients out-of-band via the 0xCE
            // datagram. Don't claim a capability the AV1 path doesn't have.
            supports_hdr_metadata: self.hdr && self.codec != Codec::Av1,
            // Reflects what the session actually configured (cleared in `query_caps` if the GPU lacks
            // YUV444 encode), so the glue can confirm 4:4:4 vs the negotiated request.
            chroma_444: self.chroma_444,
            // The direct-NVENC path recovers via real RFI (or a forced IDR), not the Linux
            // libavcodec intra-refresh mode.
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
        }
    }

    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        // Stored and emitted as in-band SEI on the next keyframe (see `submit`). Cheap to call every
        // frame; only changes when the source is regraded or HDR toggles.
        self.hdr_meta = meta;
    }

    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live session, the GPU can't invalidate, or a nonsense range → caller forces a full IDR.
        // (NVENC handles are single-threaded; this runs on the encode thread, like submit/poll.)
        if self.encoder.is_null() || !self.rfi_supported || first < 0 || first > last {
            return false;
        }
        // Already invalidated a covering range for this loss event — no new driver calls needed,
        // no IDR. RE-ARM the anchor though: the client re-asking means the previous recovery
        // anchor AU may itself have been lost, and the next frame is just as clean a re-anchor
        // (it too references only valid frames).
        if let Some((pf, pl)) = self.last_rfi_range {
            if first >= pf && last <= pl {
                self.pending_anchor = true;
                return true;
            }
        }
        // `frame_idx` is the NEXT timestamp to assign, so the last encoded frame is `frame_idx - 1`
        // and the DPB holds `[frame_idx - RFI_DPB, frame_idx - 1]`. A lost frame older than that
        // can't be invalidated, so the only correct recovery is an IDR.
        let oldest_in_dpb = self.frame_idx - RFI_DPB as i64;
        if first < oldest_in_dpb {
            return false;
        }
        // Clamp to frames we've actually encoded (don't invalidate a timestamp we never assigned).
        let last = last.min(self.frame_idx - 1);
        if first > last {
            return false;
        }
        // Each input's `inputTimeStamp` is `frame_idx`, which `submit_indexed` pins to the WIRE
        // frame index the AU carries — so the client's lost-frame range maps 1:1 onto the
        // timestamps NVENC invalidates here, and stays 1:1 across encoder rebuilds/resets (an
        // internal counter would desync on the first adaptive-bitrate rebuild and RFI would then
        // clamp every range into first > last, silently degrading to IDR-only forever).
        // SAFETY: `invalidate_ref_frames` is a function pointer from the runtime-loaded `EncodeApi` table.
        // `self.encoder` was checked non-null at the top of this fn and is the live session; this runs
        // on the encode thread (like submit/poll), so there is no concurrent NVENC use. Each `ts` was
        // clamped to `[oldest_in_dpb, frame_idx - 1]` above, so it names a frame still in the session's
        // DPB; the call passes only that `u64` timestamp (no struct), so there is no struct-size or
        // lifetime concern.
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
        // The next submitted frame is the first one encoded after the invalidation — the clean
        // re-anchor P-frame. Arm the tag so its AU ships with `recovery_anchor` and the client
        // lifts its post-loss freeze on it (instead of waiting ~1 s for the cooldown-suppressed
        // IDR fallback).
        self.pending_anchor = true;
        true
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Async mode: drain whatever the retrieve thread has finished (non-blocking) and hand out
        // the oldest ready AU. `None` = nothing completed yet — the session loop keeps the frame
        // in flight and re-polls next tick, capture never blocks on the WDDM scheduling wait.
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
        // (`teardown` clears `pending` whenever it nulls the handle); all calls below use function
        // pointers from the runtime-loaded `EncodeApi` table on the encode thread. `NV_ENC_LOCK_BITSTREAM lock`
        // (version = `NV_ENC_LOCK_BITSTREAM_VER`) locks `bs`, a pool bitstream a prior `encode_picture`
        // targeted; `lock_bitstream` blocks until that encode finishes, so on success
        // `lock.bitstreamBufferPtr` is non-null and points at `lock.bitstreamSizeInBytes` bytes of
        // NVENC-owned, CPU-readable output valid until `unlock_bitstream`. The `from_raw_parts` slice is
        // only read (copied via `to_vec()`) BEFORE `unlock_bitstream(bs)` — lock and unlock pair on the
        // same buffer — so it never outlives the lock. `map` (the input resource paired with `bs` in
        // `pending`) is unmapped here, after the encode completed, exactly once.
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

    /// Encode-stall recovery: tear the whole session down (the same teardown a capture-device
    /// change uses) and let the next `submit` rebuild it lazily on the current device — the owed
    /// AUs are forfeited and the fresh session opens on an IDR. Gives the encode-stall watchdog a
    /// healing lever on NVENC instead of ending the session. Caveat: the SYNC retrieve mode blocks
    /// inside `lock_bitstream`, so a driver wedge that hangs the lock never returns to the loop
    /// for the watchdog to fire — this lever fully protects the async retrieve mode (5 s event
    /// timeouts surface as poll errors) and the submit-side failure paths.
    fn reset(&mut self) -> bool {
        // SAFETY: `teardown` (an `unsafe fn`) requires the encode thread with no NVENC call in
        // flight and a session whose cached resources belong to `self.encoder` — all hold here
        // (reset is called from the session loop between submit/poll, like every other method),
        // and it early-returns on an already-null session.
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
        // SAFETY: `inited` ⟹ `self.encoder` is the live session and this runs on the encode
        // thread between submit/poll (`nvEncReconfigureEncoder` is a submit-side call, the
        // sanctioned side of the two-thread async split — the retrieve thread only ever locks
        // bitstreams). `build_config` only queries the preset on that session; `cfg` outlives the
        // synchronous reconfigure call whose `reInitEncodeParams.encodeConfig` points at it.
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

impl Drop for NvencD3d11Encoder {
    fn drop(&mut self) {
        // SAFETY: `teardown` (an `unsafe fn`) needs the owning thread with no NVENC call in flight and
        // a session whose cached resources all belong to `self.encoder`. At Drop this encoder is owned
        // exclusively (no other reference can exist), runs on the encode thread it was confined to, and
        // `teardown` early-returns when `self.encoder` is null; otherwise every cached reg/bitstream/
        // pending was created against that live session. It runs exactly once (here).
        unsafe { self.teardown() };
    }
}

/// Probe whether the active NVIDIA GPU can encode HEVC **4:4:4** (`NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`).
/// HEVC-only; the result is cached by the caller ([`crate::can_encode_444`]) and read *before*
/// the Welcome so the host advertises the chroma it can really encode (honest downgrade to 4:2:0 on a
/// card without it). See [`probe_encode_cap`] for the throwaway-session mechanics.
pub fn probe_can_encode_444(codec: Codec) -> bool {
    if codec != Codec::H265 {
        return false;
    }
    probe_encode_cap(codec, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE)
}

/// Probe whether the active NVIDIA GPU can encode `codec` at **10-bit**
/// (`NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` against the codec's own GUID — HEVC Main10 / AV1 10-bit).
/// The result is cached by the caller ([`crate::can_encode_10bit`]) and read *before* the
/// Welcome so the negotiated bit depth — and the HDR label derived from it — matches what NVENC
/// will really emit. The session-open path re-checks the same cap as a belt-and-braces guard
/// ([`NvencD3d11Encoder::probe_caps`]'s 8-bit fallback).
pub fn probe_can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    probe_encode_cap(codec, nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE)
}

/// Query ONE NVENC capability for `codec`: creates a throwaway hardware D3D11 device + NVENC
/// session on the **selected render adapter**, reads the cap, and tears everything down. `false`
/// on any failure (no loadable NVENC, no device, failed open) — the honest answer for a
/// capability that couldn't be confirmed.
fn probe_encode_cap(codec: Codec, cap: nv::NV_ENC_CAPS) -> bool {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    // No loadable NVENC on this box (non-NVIDIA / no driver) → the honest 4:4:4 answer is "no".
    // This is also the `api()` gate for every NVENC call below.
    if try_api().is_err() {
        return false;
    }
    // SAFETY: a self-contained probe owning every handle it creates. `CreateDXGIFactory1`/
    // `EnumAdapterByLuid` return owned COM objects or err (→ default-adapter fallback).
    // `D3D11CreateDevice` (explicit adapter + UNKNOWN driver type, or NULL adapter + HARDWARE)
    // fills `device` or returns Err (→ false). `open_encode_session_ex` opens an NVENC session
    // against that device's raw pointer (valid while `device` is held) or errors (→ false, after
    // destroying any residue session the failed open left — the docs require it).
    // `get_encode_caps` reads one scalar cap into `val` via the loaded API table.
    // `destroy_encoder` frees the session exactly once; `device`/its context drop with the COM
    // wrappers. No handle escapes this call and nothing runs concurrently.
    unsafe {
        // Probe on the SELECTED render adapter — the GPU the session will actually encode on
        // (web-console preference / PUNKTFUNK_RENDER_ADAPTER / max VRAM). The OS default adapter
        // (NULL) can be the *other* GPU on a hybrid box, answering for hardware we won't use.
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
            return false;
        }
        let Some(device) = device else { return false };
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
            return false;
        }
        let mut param = nv::NV_ENC_CAPS_PARAM {
            version: nv::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: cap,
            reserved: [0; 62],
        };
        let mut val: i32 = 0;
        let ok = (api().get_encode_caps)(enc, codec_guid(codec), &mut param, &mut val)
            .nv_ok()
            .is_ok()
            && val != 0;
        let _ = (api().destroy_encoder)(enc);
        ok
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

    /// The 8 fully-saturated colour bars the matrix analysis samples (RGB). Saturated primaries
    /// separate BT.601 from BT.709 by tens of code points (e.g. pure-green luma 145 vs 173).
    const BARS: [(u8, u8, u8); 8] = [
        (255, 255, 255), // white
        (255, 255, 0),   // yellow
        (0, 255, 255),   // cyan
        (0, 255, 0),     // green
        (255, 0, 255),   // magenta
        (255, 0, 0),     // red
        (0, 0, 255),     // blue
        (0, 0, 0),       // black
    ];

    /// BGRA probe pattern: left half = the 8 colour bars (flat patches → matrix measurement),
    /// right half = alternating 1-px red/blue columns (the chroma-resolution litmus: true 4:4:4
    /// keeps adjacent columns' chroma distinct; an internally-subsampled encode blends them).
    fn probe_pattern(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        let bar_w = (w / 2) / BARS.len();
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = if x < w / 2 {
                    BARS[(x / bar_w).min(BARS.len() - 1)]
                } else if x % 2 == 0 {
                    (255, 0, 0) // red column
                } else {
                    (0, 0, 255) // blue column
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

    /// Encode 30 static pattern frames through the real NVENC session (ARGB input, the exact
    /// production configuration) at the given chroma and write the Annex-B stream to `path`.
    fn encode_pattern(chroma: ChromaFormat, path: &str) {
        const W: u32 = 1280;
        const H: u32 = 720;
        // SAFETY: (test-only) straight-line D3D11/DXGI COM calls on one thread; every out-pointer
        // is checked before use; the texture/device outlive the encoder (dropped at scope end).
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
            )
            .expect("NVENC open");
            let mut out = Vec::new();
            for i in 0..30u64 {
                let frame = CapturedFrame {
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

    /// ON-HARDWARE (RTX box `.173`): the Phase 3.2 in-place rate retarget — encode a few frames,
    /// `reconfigure_bitrate` mid-stream (up AND down), keep encoding, and assert every
    /// post-reconfigure AU is a P-frame (`nvEncReconfigureEncoder` with `resetEncoder=0` /
    /// `forceIDR=0` must NOT restart the stream). The Windows twin of the Linux backend's
    /// `nvenc_cuda_reconfigure_no_idr`. Run:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_reconfigure --nocapture
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
    fn nvenc_reconfigure_no_idr() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        const W: u32 = 1280;
        const H: u32 = 720;
        // SAFETY: (test-only) same straight-line D3D11/DXGI setup as `encode_pattern`.
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
            )
            .expect("NVENC open");

            let submit_and_poll = |enc: &mut NvencD3d11Encoder, range: std::ops::Range<u64>| {
                let mut keyframes = 0usize;
                let mut aus = 0usize;
                for i in range {
                    let frame = CapturedFrame {
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

    /// ON-GLASS (RTX box): the measurement gating the AYUV 4:4:4 work — encodes the probe
    /// pattern through the REAL ARGB-input NVENC session once with `chromaFormatIDC=3`/FREXT
    /// and once as plain 4:2:0, so offline analysis of the two bitstreams answers (1) whether
    /// the FREXT stream is truly full-chroma and (2) which matrix NVENC's internal RGB→YUV CSC
    /// used (BT.601 vs BT.709 — saturated bars differ by tens of code points). Run with:
    ///   cargo test -p punktfunk-host --features nvenc -- --ignored nvenc_444_on_glass --nocapture
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
}
