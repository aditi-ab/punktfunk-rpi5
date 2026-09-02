//! Host-side IDD-push capture: unnamed shared ring, consumed as [`FramePayload::D3d11`].
//!
//! The HOST (SYSTEM) creates the header, frame-ready event, and keyed-mutex textures
//! unnamed on the render GPU. [`ChannelBroker`] duplicates those handles into the
//! pf-vdisplay WUDFHost over `IOCTL_SET_FRAME_CHANNEL`. A handle is meaningless
//! outside that process. Evidence: `design/idd-push-security.md`.
//!
//! Sole Windows capture path. Driver:
//! `packaging/windows/drivers/pf-vdisplay/src/frame_transport.rs`. Layout,
//! `MAGIC`/`VERSION`/`RING_LEN`, status codes, and the publish token live in
//! [`pf_driver_proto`] — both sides `use` it, so drift is a compile error.

use super::dxgi::{
    make_device, BgraToYuvPlanes, D3d11Frame, HdrP010Converter, HdrRgb10Converter, PyroFrameShare,
    VideoConverter, WinCaptureTarget,
};
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{bail, Context, Result};
use pf_driver_proto::{control, frame};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    DuplicateHandle, LocalFree, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS,
    DUPLICATE_SAME_ACCESS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LUID, POINT, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

use frame::{
    unpack_opened_detail, SharedHeader, DRV_STATUS_BIND_FAIL, DRV_STATUS_NONE,
    DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED, DRV_STATUS_TEX_FAIL, MAGIC, RING_LEN, VERSION,
};

/// DXGI sharing-API arg (`READ|WRITE`), not part of the proto contract.
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;

/// Map-only on the driver's header duplicate. No OWNER / `WRITE_DAC` / DELETE.
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;
/// Driver only `SetEvent`s; host keeps `SYNCHRONIZE` on its own handle.
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// NVENC-input textures rotated per frame. 3 covers pipeline depth 2 plus one slot of margin.
const OUT_RING: usize = 3;

/// Stamped into the header and every publish token so a recreate rejects a stale-ring publish.
static IDD_GENERATION: AtomicU32 = AtomicU32::new(1);

/// Masked to [`frame::FrameToken::GENERATION_MASK`] and never `0`.
/// `0` is the cleared-`latest` sentinel [`IddPushCapturer::recreate_ring`] stores.
fn next_generation() -> u32 {
    loop {
        let g = IDD_GENERATION.fetch_add(1, Ordering::Relaxed) & frame::FrameToken::GENERATION_MASK;
        if g != 0 {
            return g;
        }
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// File mapping + mapped view. Drop unmaps, then [`OwnedHandle`] closes.
/// [`IddPushCapturer::header`] borrows the view; `section` is declared first so the pointer outlives it.
struct MappedSection {
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl MappedSection {
    /// View base; valid only while this section lives.
    fn ptr<T>(&self) -> *mut T {
        self.view.Value as *mut T
    }
}

impl Drop for MappedSection {
    fn drop(&mut self) {
        // SAFETY: `view` is the live `MapViewOfFile` mapping; unmap before `handle` closes.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

struct HostSlot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    /// Unnamed NT handle: keeps the resource alive and is the only duplication source for the driver.
    shared: OwnedHandle,
    /// HDR samples the FP16 slot directly (no slot→scratch copy) while the keyed mutex is held.
    srv: ID3D11ShaderResourceView,
}

/// Encode-input texture plus, for P010, the two plane RTVs.
/// Built with the slot so HDR convert never creates RTVs under the keyed mutex.
struct OutSlot {
    tex: ID3D11Texture2D,
    /// Plane RTVs. `None` for NV12/BGRA.
    p010: Option<(ID3D11RenderTargetView, ID3D11RenderTargetView)>,
    /// Packed 10-bit RTV for HDR 4:4:4 ([`HdrRgb10Converter`]). `None` otherwise.
    rgb10: Option<ID3D11RenderTargetView>,
}

/// Separate shareable Y + CbCr planes the wavelet encoder imports
/// (`design/pyrowave-windows-host-zerocopy.md`). Rotated like `out_ring`.
struct PyroOutSlot {
    y: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    cbcr: ID3D11Texture2D,
    cbcr_rtv: ID3D11RenderTargetView,
}

/// [`acquire`](Self::acquire) / `Drop` = `AcquireSync` / `ReleaseSync`.
/// Releases on `?` or panic so a leaked key cannot stall the driver on that slot.
struct KeyedMutexGuard<'a> {
    mutex: &'a IDXGIKeyedMutex,
    key: u64,
}

/// Driver died holding the slot; ownership still transferred. SUCCESS-severity,
/// so the windows-rs `Result` wrapper maps it to `Ok` — classify the raw HRESULT.
const WAIT_ABANDONED_HRESULT: i32 = 0x0000_0080;

/// The classified result of a slot acquire — the outcomes have DISJOINT consequences and must not
/// collapse into one "no guard" (they did: a timeout, an abandoned mutex, and a removed device all
/// read as an ordinary no-frame tick, hiding a dead transport behind repeats forever — F4).
enum SlotAcquire<'a> {
    /// Genuinely held — convert/copy under the guard.
    Acquired(KeyedMutexGuard<'a>),
    /// `WAIT_TIMEOUT`: the driver holds the slot mid-copy — skip this tick, retry is correct.
    Busy,
    /// `WAIT_ABANDONED`: the producer died holding the slot. The cleanup ownership the API handed
    /// over was already discharged (best-effort release, no pixel action) — the generation is
    /// poisoned and the caller must fail typed.
    Abandoned,
    /// A fatal HRESULT (negative — device removed / invalid call).
    Fatal(i32),
}

/// Pure classification of an `AcquireSync` HRESULT — shared shape with the driver's publish loop
/// (`frame_transport.rs`), split out so the branch logic is testable without a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcquireClass {
    Acquired,
    Abandoned,
    Busy,
    Fatal,
}

/// What the stale-source watchdog does this tick (see
/// [`IddPushCapturer::stale_source_watchdog`]) — pure over its inputs so the floor/evidence/
/// budget rules are testable without a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleAction {
    None,
    Rebuild,
    Fail,
}

fn stale_source_action(
    gap: Duration,
    floor: Duration,
    cursor_px: u32,
    evidence_px: u32,
    offered_delta: u64,
    trips: u32,
) -> StaleAction {
    if gap < floor {
        return StaleAction::None;
    }
    // Activity evidence: the user's input moved the cursor over the frozen image, or the driver
    // kept offering frames the ring never delivered. Neither = plain idle = no recovery.
    if cursor_px < evidence_px && offered_delta == 0 {
        return StaleAction::None;
    }
    if trips == 0 {
        StaleAction::Rebuild
    } else {
        StaleAction::Fail
    }
}

fn classify_acquire(hr: i32) -> AcquireClass {
    match hr {
        0 => AcquireClass::Acquired,
        WAIT_ABANDONED_HRESULT => AcquireClass::Abandoned,
        hr if hr >= 0 => AcquireClass::Busy, // WAIT_TIMEOUT and other success-severity codes
        _ => AcquireClass::Fatal,
    }
}

impl<'a> KeyedMutexGuard<'a> {
    /// Acquire `mutex` at `key`, waiting up to `timeout_ms`, classifying the outcome.
    fn acquire(mutex: &'a IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> SlotAcquire<'a> {
        // SAFETY: `mutex` is a live `IDXGIKeyedMutex` on this thread's immediate-context device.
        // Raw vtable call, NOT the `Result` wrapper: `.is_err()` treated WAIT_TIMEOUT (positive =
        // `Ok`) as acquired, handing out a guard for a slot the DRIVER still held — converting from
        // a texture mid-copy (torn frame) and `ReleaseSync`ing a key this side never took.
        let hr = unsafe {
            (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, timeout_ms)
        };
        match classify_acquire(hr.0) {
            AcquireClass::Acquired => SlotAcquire::Acquired(KeyedMutexGuard { mutex, key }),
            // The producer died holding the slot: the lock transferred to us, but the surface's
            // consistency is unknown — take NO pixel action, discharge the cleanup ownership
            // (a failed cleanup release does not soften the poisoning), and report it.
            AcquireClass::Abandoned => {
                // SAFETY: abandoned still transfers the lock; best-effort release, exactly once.
                unsafe {
                    let _ = mutex.ReleaseSync(key);
                }
                SlotAcquire::Abandoned
            }
            AcquireClass::Busy => SlotAcquire::Busy,
            AcquireClass::Fatal => SlotAcquire::Fatal(hr.0),
        }
    }

    /// Release the slot, CHECKED — the `Drop` release stays as the early-return fallback, but the
    /// steady path must see a failed release (a slot the producer can never re-acquire) instead
    /// of discarding it.
    fn release(self) -> std::result::Result<(), i32> {
        // SAFETY: we hold `mutex` at `key`; release exactly once (`forget` skips the Drop release).
        let r = unsafe { self.mutex.ReleaseSync(self.key) };
        std::mem::forget(self);
        r.map_err(|e| e.code().0)
    }
}

impl Drop for KeyedMutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: we hold `mutex` at `key` (acquired in `acquire`, never released elsewhere).
        unsafe {
            let _ = self.mutex.ReleaseSync(self.key);
        }
    }
}

/// Image path is `%SystemRoot%\System32\WUDFHost.exe` before duplicating
/// handles into `process`. `what` names the channel in the error.
///
/// Path only — not our UMDF host, and not authorization. Callers judge
/// sufficiency (`design/idd-push-security.md`). A token/session check
/// false-negatives: genuine host and spawned copy are both session 0
/// LocalService.
///
/// # Safety
/// `process` must carry `PROCESS_QUERY_LIMITED_INFORMATION`.
pub unsafe fn verify_is_wudfhost(process: HANDLE, wudf_pid: u32, what: &str) -> Result<()> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` carries QUERY_LIMITED; `buf`/`len` are a valid out-buffer.
    // On success `len` is the UTF-16 unit count written (no NUL).
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .with_context(|| format!("QueryFullProcessImageNameW on the {what} pid"))?;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let got = path.to_ascii_lowercase().replace('/', "\\");
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let expected = format!("{}\\system32\\wudfhost.exe", sysroot.to_ascii_lowercase());
    if got != expected {
        bail!(
            "{what} pid {wudf_pid} is not the system WUDFHost (image={path:?}, expected \
             {expected:?}) — refusing to duplicate the channel's handles into it (spoofed driver / \
             wrong devnode?)"
        );
    }
    Ok(())
}

#[path = "idd_push/channel.rs"]
mod channel;
// Construction: adapter, HDR, ring, delivery, first-frame gate. Steady state stays below.
#[path = "idd_push/open.rs"]
mod open;
// Synthetic DWM compose kick — fallback when the driver has no stash republish.
#[path = "idd_push/compose_kick.rs"]
mod compose_kick;
use compose_kick::kick_dwm_compose;
#[path = "idd_push/cursor.rs"]
mod cursor;
#[path = "idd_push/cursor_blend.rs"]
mod cursor_blend;
#[path = "idd_push/cursor_poll.rs"]
mod cursor_poll;
#[path = "idd_push/descriptor.rs"]
mod descriptor;
// Stall attribution: DxgKrnl ETW, micro-probes, and the verdict matrix that folds them.
#[path = "idd_push/dxgkrnl_etw.rs"]
mod dxgkrnl_etw;
#[path = "idd_push/probes.rs"]
mod probes;
#[path = "idd_push/stall.rs"]
mod stall;
use channel::ChannelBroker;
use descriptor::{DescriptorPoller, DisplayDescriptor};
use stall::{StallEvidence, StallWatch};

/// Owns the shared ring; yields driver frames as [`FramePayload::D3d11`].
pub struct IddPushCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Driver-protocol target id (ring binding, cursor channel, logs). CCD path selection goes
    /// through `ccd` — a bare id is only unique per adapter.
    target_id: u32,
    /// Complete CCD identity (adapter LUID + target id) for every display-global helper.
    ccd: pf_win_display::win_display::CcdTargetKey,
    /// Monotonic count of NEW source images delivered (`FrameOrigin::Source` only) — the
    /// provenance sequence. Survives ring recreates by construction: it lives on the capturer,
    /// not the ring.
    source_seq: u64,
    /// Consecutive stale-source watchdog trips (WP3b): 1 = the one in-place rebuild ran; a
    /// second trip is terminal. Reset only when CONTINUOUS source flow resumes — a single stash
    /// republish after the rebuild must not re-arm another silent stale floor.
    stale_trips: u32,
    /// Owns the shared-header file mapping + its mapped view (RAII unmap-then-close). Declared BEFORE
    /// `header`, which is a raw pointer borrowed into this view via [`MappedSection::ptr`]. Also the
    /// duplication source for the driver's header handle on every [`ChannelBroker::send`].
    section: MappedSection,
    header: *mut SharedHeader,
    event: OwnedHandle,
    /// Handle-duplication into WUDFHost; used at open and every ring recreate.
    broker: ChannelBroker,
    /// Hardware-cursor shm (`Some` = delivered). Survives ring recreates.
    /// IddCx shape is alpha-only; fallback if [`cursor_poll::CursorPoller`] dies.
    cursor_shared: Option<cursor::CursorShared>,
    /// GDI overlay source while alive. Present with `cursor_shared`, or when
    /// `composite_forced` needs a blend source without a channel.
    cursor_poll: Option<cursor_poll::CursorPoller>,
    /// `IOCTL_SET_CURSOR_CHANNEL` for re-delivery: a monitor re-arrival kills
    /// the driver's cursor worker; this section survives, so recreates re-send.
    cursor_sender: Option<crate::CursorChannelSender>,
    /// `IOCTL_SET_CURSOR_FORWARD`. A declared IddCx hardware cursor blocks the
    /// OS software-cursor path; [`Self::poll_secure_desktop`] stands it down at UAC/Winlogon.
    cursor_forward: Option<crate::CursorForwardSender>,
    /// Poller reports a secure input desktop and the declare is stood down.
    secure_active: bool,
    /// Host blends the pointer in; a declared IddCx hardware cursor is forever
    /// (see `cursor_blend.rs`).
    composite_cursor: bool,
    /// No cursor channel, but the target still has an earlier session's hardware-cursor
    /// declare (`WinCaptureTarget::cursor_excluded`). Pins `composite_cursor` on.
    composite_forced: bool,
    /// Lazy cursor-quad pass. `None` after a build failure (pointer-less, warned once).
    cursor_blend: Option<cursor_blend::CursorBlendPass>,
    cursor_blend_failed: bool,
    /// [`Self::live_cursor`] fell back to shm. Independent serial namespaces — never unlatch.
    cursor_shm_latched: bool,
    /// Slot copy + cursor quad, tagged with the (w, h, fmt) it was built for.
    blend_scratch: Option<(
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        u32,
        u32,
        DXGI_FORMAT,
    )>,
    /// Last blended pointer. Hardware cursor does not dirty frames, so `try_consume`
    /// regenerates from the last slot when this moves.
    last_blend_key: Option<(u64, i32, i32, bool)>,
    /// Last fresh publish slot — regen source.
    last_slot: Option<usize>,
    /// HDR cursor match to desktop SDR white (vs 80 nits). 2.5 ≈ Windows default;
    /// without it the composited cursor is visibly dark.
    sdr_white_scale: f32,
    width: u32,
    height: u32,
    slots: Vec<HostSlot>,
    /// Bumped on recreate; stamped so the driver re-attaches and stale publishes die.
    generation: u32,
    /// Handshake advertised `VIDEO_CAP_HDR` (not merely 10-bit). Pins composition
    /// so an SDR client never gets in-band PQ. HDR H.26x still follows a host "Use HDR" flip.
    want_hdr: bool,
    /// 10-bit SDR: BGRA expanded 8→10 into [`PixelFormat::Rgb10a2Sdr`]. Display colour
    /// is never touched — `want_hdr` stays false.
    ten_bit_sdr: bool,
    /// Live `advanced_color_enabled`. HDR → FP16 ring, SDR → BGRA. A change recreates.
    display_hdr: bool,
    /// One-shot: the display refused the negotiated depth (poller is ~4 Hz).
    hdr_pin_warned: bool,
    /// Failed pin attempts. Past [`Self::HDR_PIN_EAGER`] retry every
    /// [`Self::HDR_PIN_RETRY_EVERY`]th sample — CCD write+query takes the session-global lock.
    hdr_pin_failures: u32,
    /// Full-chroma 4:4:4: SDR copies BGRA through; HDR uses [`HdrRgb10Converter`].
    /// NVENC CSCs to YUV 4:4:4 itself so the Welcome chroma is the wire chroma.
    want_444: bool,
    /// Wavelet session (`design/pyrowave-windows-host-zerocopy.md`). Frames come
    /// from `pyro_ring` and a shared fence; composition is pinned to negotiated depth.
    pyrowave: bool,
    /// Shared D3D11 timeline fence, created lazily; capturer Signals, encoder waits.
    pyro_fence: Option<ID3D11Fence>,
    /// Persistent shared NT handle passed on every frame. The encoder duplicates it
    /// on first import / rebuild; this original must outlive those rebuilds.
    pyro_fence_handle: Option<OwnedHandle>,
    pyro_fence_value: u64,
    /// Separate-plane output (instead of `out_ring`). Lazy; rebuilt on mode change.
    pyro_ring: Vec<PyroOutSlot>,
    /// BGRA→YUV-planes CSC (BT.709 limited, matching `rgb2yuv.comp`). Lazy.
    pyro_conv: Option<BgraToYuvPlanes>,
    /// Last presented (Y, CbCr) — repeat source, analogue of `last_present`.
    pyro_last: Option<(ID3D11Texture2D, ID3D11Texture2D)>,
    /// Off-thread CCD snapshot; the capture loop never runs those queries inline.
    desc_poller: DescriptorPoller,
    /// Last consumed poller sequence (0 = none yet).
    desc_seq: u64,
    /// Two-strikes debounce: act only when a second consecutive sample agrees,
    /// so a topology re-probe blip never tears the ring down.
    pending_desc: Option<DisplayDescriptor>,
    /// Ring recreate in flight; if still set past the window, `try_consume` drops
    /// the session (recover-or-drop, no DDA).
    recovering_since: Option<Instant>,
    /// Last fresh driver frame. A dead WUDFHost and an idle desktop both stop
    /// publishing; without this the encode loop would repeat forever.
    last_fresh: Instant,
    /// One 0 ms wait per second, and only while stale.
    last_liveness: Instant,
    /// Mid-session [`kick_dwm_compose`] (recovery window only).
    last_kick: Instant,
    /// Multi-hundred-ms DWM holes during active flow; warns when they turn metronomic.
    stall_watch: StallWatch,
    /// `offered_total` at last fresh frame, and the stalest drain heartbeat (µs) since.
    offered_at_fresh: u64,
    max_hb_age_us: u64,
    /// Damage witness for [`stall::StallEvidence::cursor_moved_px`]. Pending sample
    /// is held one call so the stall-ending move is not counted into the gap it ended.
    /// Sampled at most every [`Self::CURSOR_WITNESS_INTERVAL`]; user32, never the display-config lock.
    cursor_last: Option<(i32, i32)>,
    cursor_gap_px: u32,
    cursor_pending_px: u32,
    cursor_sampled_at: Instant,
    /// Micro-probe singleton. `None` when `PUNKTFUNK_STALL_PROBES=0`; the matrix
    /// treats a missing window as never-stalled, so reports never invent legs.
    probes: Option<Arc<probes::ProbeEngine>>,
    /// DxgKrnl ETW; `None` when the session cannot start it (reports `etw=unavailable`).
    etw: Option<Arc<dxgkrnl_etw::EtwWatch>>,
    /// Rotating NVENC-input textures. Depth-2 pipelining needs a different slot
    /// for convert N+1 while N encodes. Format = `out_format()`. Lazy; rebuilt on mode flip.
    out_ring: Vec<OutSlot>,
    out_idx: usize,
    /// BGRA→NV12 on the VIDEO engine while SDR, so CSC stays off the 3D engine. Lazy.
    video_conv: Option<VideoConverter>,
    /// FP16→P010 while HDR and not 4:4:4 (NVIDIA's VideoProcessor cannot do RGB→P010).
    hdr_p010_conv: Option<HdrP010Converter>,
    /// FP16→packed 10-bit BT.2020 PQ RGB for HDR 4:4:4. Rebuilt with the ring.
    hdr_rgb10_conv: Option<HdrRgb10Converter>,
    /// BGRA 8→10 expansion for [`Self::ten_bit_sdr`]. NVENC does CSC/subsampling.
    sdr_rgb10_conv: Option<HdrRgb10Converter>,
    last_seq: u64,
    last_present: Option<(ID3D11Texture2D, PixelFormat)>,
    status_logged: bool,
    /// `PowerRequestDisplayRequired` for this capturer's life: DWM composes nothing
    /// once the console goes dark. Waking an already-off display is the HID kick.
    _display_wake: Option<pf_frame::session_tuning::DisplayWakeRequest>,
    _keepalive: Box<dyn Send>,
}
// SAFETY: `!Send` only from `*mut SharedHeader` (and COM). Created, used, and
// dropped on the capture/encode thread: the immediate context is single-threaded
// by D3D11 contract, and the header pointer is only dereferenced there. `Send`
// moves ownership with no concurrent access; we do not claim `Sync`.
unsafe impl Send for IddPushCapturer {}

impl IddPushCapturer {
    /// Failed pins before [`Self::poll_display_hdr`] backs off (≈2 s at 4 Hz).
    const HDR_PIN_EAGER: u32 = 8;
    /// While backed off, re-pin every this-many-th sample (~4 s at 4 Hz).
    const HDR_PIN_RETRY_EVERY: u64 = 16;

    #[inline]
    fn latest(&self) -> u64 {
        // SAFETY: `self.header` is the live mapping. `addr_of!` forms the `latest`
        // field address with no reference; it is 8-aligned `u64` (valid for `AtomicU64`).
        // `Acquire` is the consumer half of the publish handshake (driver `Release`).
        unsafe {
            (*(std::ptr::addr_of!((*self.header).latest) as *const AtomicU64))
                .load(Ordering::Acquire)
        }
    }

    /// Age of a driver QPC stamp in µs (QPC is system-wide). 0 if the stamp is ahead.
    fn qpc_age_us(stamp: u64) -> u64 {
        static FREQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let freq = *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: plain FFI; `f` is a valid local out-param. Frequency is fixed at boot;
            // 0 means the call failed — guarded below.
            let _ = unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(0) as u64
        });
        if freq == 0 {
            return 0;
        }
        let mut now = 0i64;
        // SAFETY: plain FFI; `now` is a valid local out-param.
        if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
            return 0;
        }
        (now as u64).saturating_sub(stamp).saturating_mul(1_000_000) / freq
    }

    /// `GetDeviceRemovedReason` on the host endpoint device, as a raw code (`0` = the device
    /// still reports healthy) — queried after a fatal slot-synchronization result so the typed
    /// error names whether a TDR/removal is behind it.
    fn device_removed_reason(&self) -> i32 {
        // SAFETY: `self.device` is the capturer's live device; the call only reads it.
        unsafe { self.device.GetDeviceRemovedReason() }.map_or_else(|e| e.code().0, |()| 0)
    }

    /// The driver's provenance stamp for the most recent publish — the OS
    /// `PresentDisplayQPCTime` the driver writes into `qpc_pts` (0 = pre-provenance driver, or
    /// the OS reported none). Plausibility-gated: a stamp ahead of the local QPC clock is
    /// nonsense (a torn read, a restarted counter) and reads as unknown rather than as a
    /// timestamp recovery logic might trust.
    fn source_present_qpc(&self) -> u64 {
        // SAFETY: like `telemetry` — the header stays mapped for the capturer's lifetime, the
        // field is an 8-aligned u64, and Relaxed matches the driver's best-effort store.
        let stamp = unsafe {
            (*(std::ptr::addr_of!((*self.header).qpc_pts) as *const AtomicU64))
                .load(Ordering::Relaxed)
        };
        if stamp == 0 {
            return 0;
        }
        let mut now = 0i64;
        // SAFETY: plain FFI; `now` is a valid local out-param.
        if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
            return 0;
        }
        if stamp > now as u64 {
            return 0;
        }
        stamp
    }

    /// The header's v2 telemetry tail — `(drain_heartbeat_qpc, offered_total)`; `None` until a
    /// telemetry-capable driver writes its first heartbeat (the host always creates the v2 layout,
    /// so a zero heartbeat means the attached driver predates it).
    #[inline]
    fn telemetry(&self) -> Option<(u64, u64)> {
        // SAFETY: like `latest` — header mapped for the capturer's life; both
        // fields are 8-aligned `u64`s in the v2 layout. Relaxed is diagnostics.
        let (hb, offered) = unsafe {
            (
                (*(std::ptr::addr_of!((*self.header).drain_heartbeat_qpc) as *const AtomicU64))
                    .load(Ordering::Relaxed),
                (*(std::ptr::addr_of!((*self.header).offered_total) as *const AtomicU64))
                    .load(Ordering::Relaxed),
            )
        };
        (hb != 0).then_some((hb, offered))
    }

    /// First non-zero `driver_status` only — the only driver-visibility channel.
    fn log_driver_status_once(&mut self) {
        if self.status_logged {
            return;
        }
        let (status, detail, lo, hi) = self.driver_diag();
        if status == 0 {
            return;
        }
        self.status_logged = true;
        let render_luid = format!("{hi:08x}:{lo:08x}");
        match status {
            DRV_STATUS_OPENED => tracing::info!(
                render_luid,
                "IDD push: driver attached to the shared ring"
            ),
            DRV_STATUS_TEX_FAIL => tracing::error!(
                render_luid,
                detail = format!("0x{detail:08x}"),
                "IDD push: driver could NOT open our textures — render-adapter mismatch (it renders on \
                 a different GPU than where we created the ring)"
            ),
            DRV_STATUS_NO_DEVICE1 => {
                tracing::error!("IDD push: driver has no ID3D11Device1 to open shared resources")
            }
            DRV_STATUS_BIND_FAIL => tracing::error!(
                ring_claims_target = detail,
                our_target = self.target_id,
                "IDD push: driver REFUSED the ring↔monitor binding (host stash cross-wire?)"
            ),
            other => tracing::warn!(other, render_luid, "IDD push: driver reported an unknown status"),
        }
    }

    /// NVENC input format from display HDR + session 4:4:4.
    /// Composition depth follows `want_hdr`, pinned at open and by [`Self::poll_display_hdr`].
    fn out_format(&self) -> (DXGI_FORMAT, PixelFormat) {
        // PyroWave labels the frame only (`pyro_ring` is separate). Studio-code planes.
        if self.pyrowave {
            return if self.display_hdr {
                (DXGI_FORMAT_P010, PixelFormat::P010)
            } else {
                (DXGI_FORMAT_NV12, PixelFormat::Nv12)
            };
        }
        if self.display_hdr {
            if self.want_444 {
                // Packed RGB; NVENC CSCs to YUV 4:4:4. No subsampling here.
                return (DXGI_FORMAT_R10G10B10A2_UNORM, PixelFormat::Rgb10a2);
            }
            (DXGI_FORMAT_P010, PixelFormat::P010)
        } else if self.ten_bit_sdr {
            // Packed RGB; NVENC encodes Main10 under BT.709.
            (DXGI_FORMAT_R10G10B10A2_UNORM, PixelFormat::Rgb10a2Sdr)
        } else if self.want_444 {
            (DXGI_FORMAT_B8G8R8A8_UNORM, PixelFormat::Bgra)
        } else {
            (DXGI_FORMAT_NV12, PixelFormat::Nv12)
        }
    }

    /// FP16 under advanced colour, BGRA when SDR. By-value so [`Self::recreate_ring`]
    /// can size slots for the incoming state before committing it.
    fn ring_format_for(hdr: bool) -> DXGI_FORMAT {
        if hdr {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }

    fn ring_format(&self) -> DXGI_FORMAT {
        Self::ring_format_for(self.display_hdr)
    }

    /// `(driver_status, detail, render_luid_low, render_luid_high)`.
    ///
    /// SAFETY (all callers): `self.header` is the live mapping; field reads are
    /// in-bounds and form no reference. Aligned word reads cannot tear.
    fn driver_diag(&self) -> (u32, u32, u32, i32) {
        // SAFETY: see the doc comment above.
        unsafe {
            (
                (*self.header).driver_status,
                (*self.header).driver_status_detail,
                (*self.header).driver_render_luid_low,
                (*self.header).driver_render_luid_high,
            )
        }
    }

    /// Rebuild at `new_display_hdr`. Bumps generation, delivers a fresh handle set,
    /// and clears `latest` so an old-ring slot is never consumed.
    fn recreate_ring(&mut self, new_display_hdr: bool, new_w: u32, new_h: u32) -> Result<()> {
        // Build first, commit after. `create_ring_slots` is fallible (VRAM at a large mode).
        let fmt = Self::ring_format_for(new_display_hdr);
        let new_slots = Self::create_ring_slots(&self.device, new_w, new_h, fmt)?;
        self.display_hdr = new_display_hdr;
        self.width = new_w;
        self.height = new_h;
        let new_gen = next_generation();
        // SAFETY: live mapping. `latest`/`generation` stores go through `addr_of!`
        // (no references) of aligned `u64`/`u32` fields; format/size writes are
        // in-bounds and form no `&mut`. `Release` fence + `generation` store
        // publish so the driver Acquire-sees textures + format in place.
        unsafe {
            // 0 sentinel (generation 0, which try_consume rejects). A racing old-ring
            // publish still carries the old generation and is dropped.
            (*(std::ptr::addr_of!((*self.header).latest) as *const AtomicU64))
                .store(0, Ordering::Relaxed);
            (*self.header).dxgi_format = fmt.0 as u32;
            (*self.header).width = new_w;
            (*self.header).height = new_h;
            // `wait_for_attach` runs at open only. Clear so a failed re-attach is visible.
            (*self.header).driver_status = DRV_STATUS_NONE;
            (*self.header).driver_status_detail = 0;
            // Generation last (Release): the driver Acquire-sees textures + format in place.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
                .store(new_gen, Ordering::Release);
        }
        // Let `log_driver_status_once` report this generation's attach.
        self.status_logged = false;
        self.slots = new_slots;
        self.generation = new_gen;
        // Driver sees the bump (`is_stale`), drops, re-attaches.
        // SAFETY: `broker.send` borrows live `self.section.handle`/`self.event` for this call.
        if let Err(e) = unsafe {
            self.broker.send(
                self.target_id,
                new_gen,
                HANDLE(self.section.handle.as_raw_handle()),
                HANDLE(self.event.as_raw_handle()),
                &self.slots,
            )
        } {
            tracing::warn!(
                error = %format!("{e:#}"),
                "IDD push: frame-channel re-delivery failed after ring recreate"
            );
        }
        // Monitor re-arrival can kill the cursor worker; re-deliver the surviving section.
        if let (Some(cs), Some(send)) = (self.cursor_shared.as_ref(), self.cursor_sender.as_ref()) {
            let _ = deliver_cursor_channel(&self.broker, self.target_id, cs, send);
        }
        self.blend_scratch = None;
        // Query here, not from the blend (that holds the slot's keyed mutex).
        self.refresh_sdr_white_scale();
        self.last_slot = None;
        self.last_seq = 0;
        self.out_ring.clear();
        self.video_conv = None;
        self.hdr_p010_conv = None;
        self.hdr_rgb10_conv = None;
        self.sdr_rgb10_conv = None;
        // CSC is mode-baked; `ensure_pyro_conv` only builds when None.
        self.pyro_conv = None;
        self.pyro_ring.clear();
        self.pyro_last = None;
        self.out_idx = 0;
        self.last_present = None;
        Ok(())
    }

    /// Recreate when two consecutive poller samples agree on a new descriptor
    /// (~½ s), so a topology re-probe blip never costs a ring rebuild.
    fn poll_display_hdr(&mut self) {
        let (mut now, seq) = self.desc_poller.snapshot();
        if seq == self.desc_seq {
            return;
        }
        self.desc_seq = seq;
        // Exclusive-watchdog reassert in flight: a sample here is the transient eviction.
        if pf_win_display::topology_churn::held() {
            self.pending_desc = None;
            return;
        }
        // Re-assert negotiated depth instead of following a mid-session flip:
        // PyroWave plane formats are fixed; an SDR session must not promote to
        // P010 PQ. HDR H.26x is not pinned — its encoder re-inits on a flip.
        if (self.pyrowave || !self.want_hdr) && now.hdr != self.want_hdr {
            let want = self.want_hdr;
            if self.hdr_pin_failures < Self::HDR_PIN_EAGER
                || self.desc_seq % Self::HDR_PIN_RETRY_EVERY == 0
            {
                // OBSERVE the flip; never assert it. This used to discard `set_advanced_color`'s
                // `bool` and then write `now.hdr = self.want_hdr` — substituting the DESIRED
                // state for the observed one, which broke in both directions on a display that
                // cannot be flipped (the state this file already logs as "Downgrade point D" at
                // open):
                //   - want HDR, display stays SDR: the fabricated `true` differed from `current`,
                //     so two samples drove `recreate_ring(true, …)` and rebuilt the ring FP16
                //     while the driver composed 8-bit BGRA. Every publish was then dropped by the
                //     driver's format guard, `recovering_since` expired, and `try_consume`
                //     bailed — a permanent 3-second reconnect loop.
                //   - want SDR, display stays HDR: the fabricated `false` MATCHED `current`, so no
                //     recreate ever fired and the ring stayed BGRA against an FP16 composition.
                //     Same dropped-publish outcome, silently.
                // Reading back immediately can catch a flip that has not settled yet; that costs
                // one debounce cycle (the poller re-samples in ~250 ms) and never a wrong ring,
                // which is why this does not block the frame path on a settle poll the way
                // `open_on` does.
                let requested = pf_win_display::win_display::set_advanced_color(self.ccd, want);
                let observed = pf_win_display::win_display::advanced_color_enabled(self.ccd);
                // A failed READ is not evidence of a failed flip — keep the poller's sample then.
                now.hdr = observed.unwrap_or(now.hdr);
                if now.hdr != want {
                    self.hdr_pin_failures = self.hdr_pin_failures.saturating_add(1);
                    if !self.hdr_pin_warned {
                        self.hdr_pin_warned = true;
                        tracing::error!(
                            target_id = self.target_id,
                            want_hdr = want,
                            observed_hdr = ?observed,
                            set_advanced_color_returned = requested,
                            pyrowave = self.pyrowave,
                            want_hdr = self.want_hdr,
                            "IDD push: could not pin the display to the NEGOTIATED depth — following what \
                             it actually composes instead (a physical display forcing HDR, or a driver that \
                             refuses the flip). The stream's depth will not match the negotiation; the \
                             encoder's caps cross-check reports the truth to the client"
                        );
                    }
                } else {
                    self.hdr_pin_failures = 0;
                }
            }
        } else {
            // No mismatch (or the session follows flips): a later refusal starts eager again.
            self.hdr_pin_failures = 0;
        }
        let current = DisplayDescriptor {
            hdr: self.display_hdr,
            width: self.width,
            height: self.height,
        };
        if now == current {
            self.pending_desc = None;
            return;
        }
        if self.pending_desc != Some(now) {
            // First strike — act only when a second consecutive sample agrees.
            self.pending_desc = Some(now);
            return;
        }
        self.pending_desc = None;
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{} hdr={}", self.width, self.height, self.display_hdr),
            to = format!("{}x{} hdr={}", now.width, now.height, now.hdr),
            "IDD push: display descriptor changed — recreating the ring at the new mode"
        );
        // If a fresh frame does not resume, try_consume drops rather than freeze.
        self.recovering_since.get_or_insert_with(Instant::now);
        if let Err(e) = self.recreate_ring(now.hdr, now.width, now.height) {
            tracing::warn!(error = %format!("{e:#}"), "IDD push: ring recreate failed");
        }
    }

    /// Host output ring at [`Self::out_format`] if empty. Rotated so encode N
    /// and convert N+1 never share a texture.
    fn ensure_out_ring(&mut self) -> Result<()> {
        if !self.out_ring.is_empty() {
            return Ok(());
        }
        let (format, _) = self.out_format();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // VIDEO processor and P010 shaders write here; NVENC takes it as encode input.
            // PyroWave uses `pyro_ring`, so this ring stays unshared.
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..OUT_RING {
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: `CreateTexture2D` on the live device; `desc` is initialized,
            // data is `None`, `t` is the out-param. `plane_rtv` uses that texture;
            // a driver that rejects planar RTVs fails at ring build, not first frame.
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D(IDD out ring)")?;
                let tex = t.context("null out-ring texture")?;
                let p010 = if format == DXGI_FORMAT_P010 {
                    Some((
                        HdrP010Converter::plane_rtv(&self.device, &tex, DXGI_FORMAT_R16_UNORM)?,
                        HdrP010Converter::plane_rtv(&self.device, &tex, DXGI_FORMAT_R16G16_UNORM)?,
                    ))
                } else {
                    None
                };
                let rgb10 = if format == DXGI_FORMAT_R10G10B10A2_UNORM {
                    Some(HdrRgb10Converter::rtv(&self.device, &tex)?)
                } else {
                    None
                };
                self.out_ring.push(OutSlot { tex, p010, rgb10 });
            }
        }
        Ok(())
    }

    /// Separate-plane ring if empty. Wavelet import of planar NV12 is unreliable on NVIDIA.
    fn ensure_pyro_ring(&mut self) -> Result<()> {
        if !self.pyro_ring.is_empty() {
            return Ok(());
        }
        let (w, h) = (self.width, self.height);
        // SAFETY: D3D11 on `self.device`; every `&desc` is initialized and every
        // `Some(&mut _)` a live out-param; `?` rejects a failed HRESULT before use.
        unsafe {
            let make = |dev: &ID3D11Device,
                        fmt: DXGI_FORMAT,
                        w: u32,
                        h: u32|
             -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: fmt,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                        | D3D11_RESOURCE_MISC_SHARED.0) as u32,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                dev.CreateTexture2D(&desc, None, Some(&mut tex))
                    .context("CreateTexture2D(pyro plane)")?;
                let tex = tex.context("null pyro plane texture")?;
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                dev.CreateRenderTargetView(&tex, None, Some(&mut rtv))
                    .context("CreateRenderTargetView(pyro plane)")?;
                Ok((tex, rtv.context("null pyro plane rtv")?))
            };
            // 16-bit UNORM for HDR; full-res chroma for 4:4:4 (`design/pyrowave-444-hdr.md`).
            let (yf, cf) = if self.display_hdr {
                (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
            } else {
                (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
            };
            let (cw, ch) = if self.want_444 {
                (w, h)
            } else {
                (w / 2, h / 2)
            };
            for _ in 0..OUT_RING {
                let (y, y_rtv) = make(&self.device, yf, w, h)?;
                let (cbcr, cbcr_rtv) = make(&self.device, cf, cw, ch)?;
                self.pyro_ring.push(PyroOutSlot {
                    y,
                    y_rtv,
                    cbcr,
                    cbcr_rtv,
                });
            }
        }
        Ok(())
    }

    /// Mode-aware RGB→YUV CSC. Composition is pinned, so no mid-session swap.
    fn ensure_pyro_conv(&mut self) -> Result<()> {
        if self.pyro_conv.is_none() {
            self.pyro_conv = Some(BgraToYuvPlanes::new(
                &self.device,
                self.display_hdr,
                self.want_444,
            )?);
        }
        Ok(())
    }

    /// VIDEO-engine BGRA→NV12 or FP16→P010. SDR 4:4:4 needs none — BGRA passes through.
    fn ensure_converter(&mut self) -> Result<()> {
        if self.display_hdr && self.want_444 {
            // One full-res pass to packed 10-bit PQ RGB; NVENC does RGB→YUV444.
            if self.hdr_rgb10_conv.is_none() {
                self.hdr_rgb10_conv = Some(HdrRgb10Converter::new(&self.device)?);
            }
        } else if self.display_hdr {
            if self.hdr_p010_conv.is_none() {
                self.hdr_p010_conv = Some(HdrP010Converter::new(
                    &self.device,
                    self.width,
                    self.height,
                )?);
            }
        } else if self.ten_bit_sdr {
            // NVENC does CSC + any subsampling under BT.709.
            if self.sdr_rgb10_conv.is_none() {
                self.sdr_rgb10_conv = Some(HdrRgb10Converter::new_sdr_expand(&self.device)?);
            }
        } else if self.want_444 {
            // BGRA passthrough — no converter.
        } else if self.video_conv.is_none() {
            self.video_conv = Some(VideoConverter::new(
                &self.device,
                &self.context,
                self.width,
                self.height,
                false,
            )?);
        }
        Ok(())
    }

    /// Signal the shared fence after this convert; return `(handle, value)` for
    /// the encoder. `None` if not PyroWave. `Flush` so the Vulkan wait is not
    /// blocked on an unsubmitted signal.
    ///
    /// # Safety
    /// Owning capture/encode thread; forms no lasting borrow of `self`'s COM objects.
    unsafe fn pyro_fence_signal(&mut self) -> Result<Option<(Option<isize>, u64)>> {
        // SAFETY: owning capture/encode thread holds the immediate context.
        // `?`-checked COM on live device/context; `CreateSharedHandle` yields a
        // fresh NT handle that is stored here, never dereferenced, never closed.
        unsafe {
            if !self.pyrowave {
                return Ok(None);
            }
            if self.pyro_fence.is_none() {
                let dev5: ID3D11Device5 = self
                    .device
                    .cast()
                    .context("ID3D11Device -> ID3D11Device5 (shared fence)")?;
                // COM out-param (unlike HANDLE-returning CreateSharedHandle below).
                let mut fence_out: Option<ID3D11Fence> = None;
                dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence_out)
                    .context("CreateFence(D3D11_FENCE_FLAG_SHARED)")?;
                let fence = fence_out.context("null D3D11 fence")?;
                // GENERIC_ALL (0x1000_0000) — access the pyrowave interop test uses.
                let handle: HANDLE = fence
                    .CreateSharedHandle(None, 0x1000_0000, PCWSTR::null())
                    .context("ID3D11Fence::CreateSharedHandle")?;
                self.pyro_fence = Some(fence);
                // Fresh NT handle; `OwnedHandle` closes it once, when the capturer drops.
                self.pyro_fence_handle = Some(OwnedHandle::from_raw_handle(handle.0 as _));
                self.pyro_fence_value = 0;
            }
            self.pyro_fence_value += 1;
            let value = self.pyro_fence_value;
            let ctx4: ID3D11DeviceContext4 = self
                .context
                .cast()
                .context("ID3D11DeviceContext -> ID3D11DeviceContext4 (fence signal)")?;
            {
                let fence = self.pyro_fence.as_ref().expect("fence just created");
                ctx4.Signal(fence, value)
                    .context("ID3D11 fence Signal after convert")?;
            }
            // Submit the queued convert + signal so the Vulkan timeline wait can resolve.
            self.context.Flush();
            // Every frame: a rebuilt encoder re-imports; it duplicates so this original stays.
            Ok(Some((
                self.pyro_fence_handle
                    .as_ref()
                    .map(|h| h.as_raw_handle() as isize),
                value,
            )))
        }
    }

    /// Overlay source for [`Capturer::cursor`], blend key, and blend scratch.
    ///
    /// A live poller wins even while it still reports `None`. Shm is only for a
    /// dead/missing poller, then latched: the two serial namespaces must not interleave.
    fn live_cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        if !self.cursor_shm_latched {
            if let Some(p) = &self.cursor_poll {
                if p.alive() {
                    return p.read();
                }
            }
            // About to read shm — latch so a revived poller cannot recross serials.
            if self.cursor_shared.is_some() {
                self.cursor_shm_latched = true;
                tracing::warn!(
                    target_id = self.target_id,
                    "cursor: the GDI shape poller is not running — degrading to the driver's \
                     hardware-cursor shm section for the rest of the session (alpha-only shapes: \
                     monochrome/masked cursors will look wrong)"
                );
            }
        }
        self.cursor_shared.as_mut().and_then(|c| c.read())
    }

    /// Where DWM places SDR white on this HDR desktop. 2.5× at the Windows default.
    ///
    /// Open and ring-recreate only — never from the blend. The query takes the
    /// display-config lock, and the blend holds the slot's keyed mutex.
    fn refresh_sdr_white_scale(&mut self) {
        if !self.display_hdr {
            return;
        }
        let queried = pf_win_display::win_display::sdr_white_level_scale(self.ccd);
        self.sdr_white_scale = queried.unwrap_or(self.sdr_white_scale);
        tracing::info!(
            target_id = self.target_id,
            queried = ?queried,
            applied = self.sdr_white_scale,
            "cursor composite: HDR SDR-white scale (1.0 = 80 nits; None = query failed — keeping \
             the prior value)"
        );
    }

    fn cursor_blend_key(&mut self) -> Option<(u64, i32, i32, bool)> {
        self.live_cursor().map(|o| (o.serial, o.x, o.y, o.visible))
    }

    /// (Re)build the blend scratch at the current ring geometry — the ALLOCATION half of the
    /// composite blend, split from [`Self::prepare_blend_scratch`] so it runs BEFORE the slot's
    /// keyed mutex is acquired: the held interval must carry only the ordered read/copy/convert
    /// work, never a `CreateTexture2D` that can stall on the device while the driver waits.
    ///
    /// # Safety
    /// D3D11 calls on the owning thread's device; call with NO slot lock held.
    unsafe fn ensure_blend_scratch(&mut self) {
        // SAFETY: `CreateTexture2D`/`CreateShaderResourceView` take a fully-initialized stack
        // descriptor plus live out-params and are `.ok()`-checked before use.
        unsafe {
            let fmt = self.ring_format();
            let stale = self
                .blend_scratch
                .as_ref()
                .is_none_or(|(_, _, w, h, f)| (*w, *h, *f) != (self.width, self.height, fmt));
            if stale {
                self.blend_scratch = None;
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: fmt,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                let built = self
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut tex))
                    .ok()
                    .and(tex)
                    .and_then(|t| {
                        let mut srv: Option<ID3D11ShaderResourceView> = None;
                        self.device
                            .CreateShaderResourceView(&t, None, Some(&mut srv))
                            .ok()
                            .and(srv)
                            .map(|v| (t, v))
                    });
                match built {
                    Some((t, v)) => {
                        // Not queried here: keyed mutex is held (see `refresh_sdr_white_scale`).
                        self.blend_scratch = Some((t, v, self.width, self.height, fmt));
                    }
                    None => {
                        if !self.cursor_blend_failed {
                            self.cursor_blend_failed = true;
                            tracing::warn!(
                                "cursor blend scratch creation failed — capture-model frames stay \
                             pointer-less this session"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Composite the pointer for this convert: copy the slot into the pre-built blend scratch
    /// and alpha-blend the GDI poller's shape at its polled position. Returns the scratch
    /// (texture + SRV) the conversion should read INSTEAD of the slot; `None` degrades to the
    /// pointer-less slot (no scratch — creation failed, warned once by `ensure_blend_scratch`).
    /// A hidden pointer blends nothing (the plain copy is the correct frame).
    ///
    /// # Safety
    /// D3D11 calls on the owning capture/encode thread's device + immediate context, called
    /// while holding the slot's keyed mutex (the copy reads the slot). Call
    /// [`Self::ensure_blend_scratch`] before the acquire.
    unsafe fn prepare_blend_scratch(
        &mut self,
        slot_tex: &ID3D11Texture2D,
    ) -> Option<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        // SAFETY: per the contract above, D3D11 calls on the owning thread's device + immediate
        // context while the slot's keyed mutex is held. The scratch was allocated by
        // `ensure_blend_scratch` BEFORE the slot acquire; only the ordered copy + blend run here.
        // `CopyResource` moves between our own scratch and the caller's live slot texture, which
        // share format and size by construction.
        unsafe {
            let (tex, srv, ..) = self.blend_scratch.as_ref()?;
            let (tex, srv) = (tex.clone(), srv.clone());
            self.context.CopyResource(&tex, slot_tex);
            // Hidden = the copy alone. Through `live_cursor` so a dead poller degrades here too.
            let overlay = self.live_cursor();
            self.last_blend_key = overlay.as_ref().map(|o| (o.serial, o.x, o.y, o.visible));
            if let Some(ov) = overlay.filter(|o| o.visible) {
                if self.cursor_blend.is_none() && !self.cursor_blend_failed {
                    match cursor_blend::CursorBlendPass::new(&self.device) {
                        Ok(p) => self.cursor_blend = Some(p),
                        Err(e) => {
                            self.cursor_blend_failed = true;
                            tracing::warn!(
                                "cursor blend pass build failed — capture-model frames stay \
                             pointer-less this session: {e:#}"
                            );
                        }
                    }
                }
                if let Some(pass) = self.cursor_blend.as_mut() {
                    // scRGB linear: linearize the sRGB shape and scale to desktop SDR white.
                    let scale = if self.display_hdr {
                        self.sdr_white_scale
                    } else {
                        0.0
                    };
                    if let Err(e) = pass.blend(&self.device, &self.context, &tex, &ov, scale) {
                        if !self.cursor_blend_failed {
                            self.cursor_blend_failed = true;
                            tracing::warn!("cursor blend draw failed — pointer-less frames: {e:#}");
                        }
                    }
                }
            }
            Some((tex, srv))
        }
    }

    /// UAC/Winlogon use the software-cursor path; a declared IddCx hardware cursor
    /// blocks it. Stand the declare down on the secure edge; restore on dismissal.
    /// Must run every tick, including while frames are stalled.
    fn poll_secure_desktop(&mut self) {
        let Some(fwd) = self.cursor_forward.as_ref() else {
            return;
        };
        // Channel session, or forced-composite on a reused monitor that may still
        // run an earlier worker. A clean target has no poller — no guard.
        if self.cursor_shared.is_none() && !self.composite_forced {
            return;
        }
        let secure = self
            .cursor_poll
            .as_ref()
            .is_some_and(|p| p.secure_desktop());
        if secure == self.secure_active {
            return;
        }
        self.secure_active = secure;
        if secure {
            tracing::info!(
                target_id = self.target_id,
                "secure desktop (UAC/Winlogon) active — standing the IddCx hardware-cursor \
                 declare down so the OS software-cursor path can render it"
            );
            if let Err(e) = fwd(false) {
                tracing::warn!(
                    "secure-desktop cursor-forward stand-down failed (secure content may stay \
                     invisible this session): {e:#}"
                );
            }
        } else {
            tracing::info!(
                target_id = self.target_id,
                "secure desktop dismissed — restoring the cursor render model"
            );
            // Only the session that runs the cursor channel. Forced-composite never
            // wanted the declare; leaving desired-state off stops per-assign re-declares.
            if self.cursor_shared.is_some() {
                if let Err(e) = fwd(true) {
                    tracing::warn!(
                        "secure-desktop cursor-forward re-enable failed (client-drawn cursor \
                         may double with a composited one): {e:#}"
                    );
                }
            }
        }
    }

    /// Two user32 reads; 8 ms so a ≥150 ms hole still gets many samples.
    const CURSOR_WITNESS_INTERVAL: Duration = Duration::from_millis(8);

    /// The damage witness (see the `cursor_*` field docs): fold the PREVIOUS call's pending
    /// delta into the gap accumulator — if that call had consumed a fresh frame, the fresh-frame
    /// bookkeeping would have zeroed the pending, so whatever survives belongs to the gap — then
    /// take a fresh rate-limited `GetCursorPos` sample into the pending slot. The one-call lag is
    /// what keeps the stall-ending frame's own cursor move out of the gap it ended.
    ///
    /// `GetCursorPos` is global, not per-display: a delta of 0 therefore proves the cursor sat
    /// still EVERYWHERE (the demotion direction is strict), while a delta > 0 on a
    /// parallel-displays host may be a sibling display's motion — that direction only ever
    /// upholds today's CONTENT-SILENCE labeling, never worsens it.
    // No per-target rect filter yet (needs a cached CCD rect); add one if parallel-display
    // hosts ever show false CONTENT-SILENCE convictions from sibling-cursor motion.
    fn sample_cursor_witness(&mut self) {
        self.cursor_gap_px = self.cursor_gap_px.saturating_add(self.cursor_pending_px);
        self.cursor_pending_px = 0;
        if self.cursor_sampled_at.elapsed() < Self::CURSOR_WITNESS_INTERVAL {
            return;
        }
        self.cursor_sampled_at = Instant::now();
        let mut pos = POINT::default();
        // SAFETY: plain FFI; `pos` is a valid out-param for this synchronous call.
        if unsafe { GetCursorPos(&mut pos) }.is_ok() {
            if let Some((x, y)) = self.cursor_last {
                self.cursor_pending_px = pos.x.abs_diff(x).saturating_add(pos.y.abs_diff(y));
            }
            self.cursor_last = Some((pos.x, pos.y));
        }
    }

    /// Stale floor for the interim stale-source watchdog (WP3b). Anchored ABOVE the recorded
    /// benign vendor-hole envelope — field holes run 1.6–10 s during link/power/modeset servicing
    /// (vdisplay-disturbance-immunity §1), and a transient adapter pause must never trigger
    /// recovery. The staged-recovery classifier (WP12) inherits this value as its starting floor.
    const STALE_SOURCE_FLOOR: Duration = Duration::from_secs(15);
    /// Cursor motion across the gap that counts as ACTIVITY evidence — a couple of real mouse
    /// movements, comfortably above sub-pixel jitter.
    const STALE_EVIDENCE_PX: u32 = 64;

    /// Interim stale-source watchdog (immunity plan WP3b; retired when the WP13 recovery ladder
    /// owns the decision). A wedged-but-ALIVE presentation path answers `Ok(None)` forever — the
    /// 20 s `next_frame` bail is unreachable after a first frame, and the driver-death watch only
    /// catches an EXITED WUDFHost — so a known-active display could stream one stale texture
    /// indefinitely (F1). When no fresh driver frame has arrived through the floor while activity
    /// evidence exists (the user's cursor moved over the frozen image, or the driver kept
    /// offering frames the ring never delivered), run ONE same-mode in-place rebuild; a second
    /// expiry is terminal (typed `RingFault::SourceStalled`). No evidence = plain idle = no
    /// recovery: a static desktop composes nothing, and that is healthy.
    fn stale_source_watchdog(&mut self) -> Result<()> {
        if self.recovering_since.is_some() {
            return Ok(()); // a recreate is in flight — its own 3 s recover-or-drop governs
        }
        let gap = self.last_fresh.elapsed();
        let offered_delta = self.telemetry().map_or(0, |(_, offered)| {
            offered.saturating_sub(self.offered_at_fresh)
        });
        match stale_source_action(
            gap,
            Self::STALE_SOURCE_FLOOR,
            self.cursor_gap_px,
            Self::STALE_EVIDENCE_PX,
            offered_delta,
            self.stale_trips,
        ) {
            StaleAction::None => Ok(()),
            StaleAction::Rebuild => {
                self.stale_trips += 1;
                tracing::warn!(
                    target = %self.ccd,
                    gap_s = gap.as_secs(),
                    cursor_moved_px = self.cursor_gap_px,
                    offered_delta,
                    "IDD push: no source frame through the stale floor with activity evidence — \
                     one same-mode ring rebuild (interim stale-source watchdog)"
                );
                if !self.recreate_ring_in_place() {
                    let fault = crate::RingFault::SourceStalled {
                        secs: gap.as_secs() as u32,
                    };
                    return Err(anyhow::Error::new(fault).context(
                        "IDD-push: stale-source rebuild failed — ending the video plane",
                    ));
                }
                Ok(())
            }
            StaleAction::Fail => {
                self.stale_trips += 1;
                let fault = crate::RingFault::SourceStalled {
                    secs: gap.as_secs() as u32,
                };
                tracing::error!(
                    target = %self.ccd,
                    %fault,
                    trips = self.stale_trips,
                    "IDD push: stale-source watchdog exhausted its one rebuild"
                );
                Err(anyhow::Error::new(fault).context(
                    "IDD-push: a known-active display delivered no source frame through the \
                     stale floor and a rebuild — ending the video plane with a typed error",
                ))
            }
        }
    }

    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        self.log_driver_status_once();
        // Secure-desktop first: UAC/Winlogon may produce no frames until this edge.
        self.poll_secure_desktop();
        // Witness before any early return so every gap shape accumulates cursor motion.
        self.sample_cursor_witness();
        // A "Use HDR" flip recreates the ring at the matching format.
        self.poll_display_hdr();
        // Recover-or-drop: a recreate that never resumes a fresh frame ends the session.
        if let Some(since) = self.recovering_since {
            if since.elapsed() > Duration::from_secs(3) {
                // `recreate_ring` cleared these; whatever they hold is this generation's re-attach.
                let (st, detail, lo, hi) = self.driver_diag();
                bail!(
                    "IDD-push: display descriptor changed and the ring could not recover within 3s — \
                     dropping the session so the client reconnects. This generation's re-attach: \
                     driver_status={st} detail=0x{detail:08x} driver_render_luid={hi:08x}:{lo:08x} \
                     (0=never attached, 1=attached but published nothing, 2=could not open our \
                     textures — render-adapter mismatch, 4=refused the ring↔monitor binding)"
                );
            }
            // Idle desktop after recreate: no compose, recover-or-drop kills a healthy session.
            // Stash-capable drivers republish; this kick is the fallback. Rate-limited; may
            // block ~35 ms on the sibling-display branch — only ≥600 ms into recovery with no frames.
            if since.elapsed() > Duration::from_millis(600)
                && self.last_kick.elapsed() > Duration::from_millis(800)
            {
                self.last_kick = Instant::now();
                tracing::debug!(
                    target_id = self.target_id,
                    "IDD push: no frame after ring recreate — falling back to a synthetic compose \
                     kick (stash-capable drivers republish at re-attach; old driver?)"
                );
                kick_dwm_compose(self.ccd);
            }
        }
        // Dead WUDFHost and idle desktop both stop publishing. Probe while stale
        // so the session rebuilds instead of repeating the last frame forever.
        if self.last_fresh.elapsed() > Duration::from_secs(2)
            && self.last_liveness.elapsed() > Duration::from_secs(1)
        {
            self.last_liveness = Instant::now();
            if !self.broker.driver_alive() {
                bail!(
                    "IDD-push: the pf-vdisplay WUDFHost (pid {}) exited mid-session — driver died; \
                     failing the capturer so the session rebuilds the virtual output",
                    self.broker.wudf_pid
                );
            }
        }
        // Interim stale-source watchdog (WP3b) — after the driver-death watch, so a trip means
        // the WUDFHost is ALIVE and the presentation path is what stopped.
        self.stale_source_watchdog()?;
        // Stall-attribution evidence (v2 telemetry): record the STALEST the driver's drain
        // heartbeat ever reads between fresh frames. A heartbeat that goes quiet for the hole
        // convicts our worker (starved/dead WUDFHost); one that stays fresh through it acquits the
        // driver and indicts the compose/present path. Two Relaxed loads + a QPC read per consume
        // tick; rolled at every fresh frame below.
        if let Some((hb, _)) = self.telemetry() {
            self.max_hb_age_us = self.max_hb_age_us.max(Self::qpc_age_us(hb));
        }
        let latest = self.latest();
        // Reject a publish whose generation is not the current ring (stale race or 0 sentinel).
        let tok = frame::FrameToken::unpack(latest);
        if tok.generation != self.generation {
            return Ok(None);
        }
        let seq = u64::from(tok.seq);
        let mut slot = tok.slot as usize;
        let fresh = seq != self.last_seq && slot < self.slots.len();
        let mut regen = false;
        if !fresh {
            // Pointer-only motion publishes nothing; regenerate from the last slot.
            let moved = self.composite_cursor
                && self.last_slot.is_some()
                && self.cursor_blend_key() != self.last_blend_key;
            if !moved {
                return Ok(None);
            }
            slot = self.last_slot.expect("checked above");
            if slot >= self.slots.len() {
                // Ring shrank across a recreate — wait for a fresh publish.
                return Ok(None);
            }
            regen = true;
        }
        // Ring + converter before Acquire so a `?` cannot leak the keyed mutex.
        let i = self.out_idx;
        let (out, pyro_slot) = if self.pyrowave {
            self.ensure_pyro_ring()?;
            self.ensure_pyro_conv()?;
            let s = &self.pyro_ring[i];
            (
                None,
                Some((
                    s.y.clone(),
                    s.y_rtv.clone(),
                    s.cbcr.clone(),
                    s.cbcr_rtv.clone(),
                )),
            )
        } else {
            self.ensure_out_ring()?;
            self.ensure_converter()?;
            let s = &self.out_ring[i];
            (Some((s.tex.clone(), s.p010.clone(), s.rgb10.clone())), None)
        };
        let (_, pf) = self.out_format();
        let ring_len = if self.pyrowave {
            self.pyro_ring.len()
        } else {
            self.out_ring.len()
        };

        // Mutex only across convert/copy, not the ~3 ms encode (NVENC reads the host
        // out-ring). Clone COM so the guard borrows locals and `self` stays free.
        let (slot_tex, slot_srv, slot_mutex) = {
            let s = &self.slots[slot];
            (s.tex.clone(), s.srv.clone(), s.mutex.clone())
        };
        // Blend RESOURCE construction (scratch texture + SRV) before the acquisition, so the held
        // interval below contains only the ordered read/copy/convert work — never a
        // CreateTexture2D that can stall on the D3D device while the driver waits on the slot.
        if self.composite_cursor {
            // SAFETY: D3D11 resource creation on the owning thread's device; no slot is held.
            unsafe { self.ensure_blend_scratch() };
        }
        // Acquire the slot's keyed mutex via a RAII guard, scoped to JUST the convert/copy below so it
        // releases at the same point as the old hand-written `ReleaseSync` (the driver gets the slot back
        // immediately, NOT held across the rest of `try_consume`) — but now leak-proof on any early return.
        {
            let lock = match KeyedMutexGuard::acquire(&slot_mutex, 0, 8) {
                SlotAcquire::Acquired(l) => l,
                // The driver holds the slot mid-copy — an ordinary tick, retry is correct.
                SlotAcquire::Busy => return Ok(None),
                // FATAL outcomes are typed errors now, never an `Ok(None)` repeat (F4): the ring
                // generation is dead and only a rebuild (or a clean session error) helps.
                SlotAcquire::Abandoned => {
                    let fault = crate::RingFault::Abandoned;
                    tracing::error!(target = %self.ccd, %fault, "IDD push: ring poisoned");
                    return Err(anyhow::Error::new(fault)
                        .context("IDD-push ring generation poisoned (rebuild required)"));
                }
                SlotAcquire::Fatal(hr) => {
                    let removed = self.device_removed_reason();
                    let fault = crate::RingFault::DeviceLost { hr, removed };
                    tracing::error!(target = %self.ccd, %fault, "IDD push: fatal slot acquire");
                    return Err(anyhow::Error::new(fault)
                        .context("IDD-push slot acquire failed fatally (rebuild required)"));
                }
            };
            // SAFETY: convert on the owning (encode) thread's immediate context, holding the slot lock.
            // A `?` here is leak-safe: `lock` (the KeyedMutexGuard) drops on the early return, releasing
            // the slot back to the driver.
            unsafe {
                // Blend scratch as convert input. `None` = compositing off or degraded.
                let blended = if self.composite_cursor {
                    self.prepare_blend_scratch(&slot_tex)
                } else {
                    None
                };
                if self.pyrowave {
                    // Slot SRV → two planes; fence after orders the encoder's Vulkan read.
                    let (_, y_rtv, _, cbcr_rtv) = pyro_slot.as_ref().expect("pyro slot");
                    if let Some(conv) = self.pyro_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        conv.convert(&self.context, src, y_rtv, cbcr_rtv, self.width, self.height)?;
                    }
                } else if self.display_hdr && self.want_444 {
                    // NVENC CSCs packed PQ RGB to YUV 4:4:4 (Main 4:4:4 10).
                    if let Some(conv) = self.hdr_rgb10_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, _, rtv) = out.as_ref().expect("out ring");
                        let rtv = rtv.as_ref().expect("Rgb10a2 out slot has an RTV");
                        conv.convert(&self.context, src, rtv, self.width, self.height)?;
                    }
                } else if self.display_hdr {
                    if let Some(conv) = self.hdr_p010_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, rtvs, _) = out.as_ref().expect("out ring");
                        // Plane views, built once in `ensure_out_ring`.
                        let (y_rtv, uv_rtv) = rtvs.as_ref().expect("P010 out slot has plane RTVs");
                        conv.convert(&self.context, src, y_rtv, uv_rtv, self.width, self.height)?;
                    }
                } else if self.ten_bit_sdr {
                    // NVENC encodes Main10 under BT.709; CSC is NVENC's.
                    if let Some(conv) = self.sdr_rgb10_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, _, rtv) = out.as_ref().expect("out ring");
                        let rtv = rtv.as_ref().expect("Rgb10a2Sdr out slot has an RTV");
                        conv.convert(&self.context, src, rtv, self.width, self.height)?;
                    }
                } else if self.want_444 {
                    // BGRA passthrough; NVENC CSCs to YUV 4:4:4. Copy-engine; slot releases now.
                    let src = blended.as_ref().map(|(t, _)| t).unwrap_or(&slot_tex);
                    self.context
                        .CopyResource(&out.as_ref().expect("out ring").0, src);
                } else {
                    if let Some(conv) = self.video_conv.as_ref() {
                        let src = blended.as_ref().map(|(t, _)| t).unwrap_or(&slot_tex);
                        conv.convert(src, &out.as_ref().expect("out ring").0)?;
                    }
                }
            }
            // CHECKED release on the steady path (the Drop release only backs the `?` returns
            // above): a failed release wedges the slot for the driver and poisons the generation.
            if let Err(hr) = lock.release() {
                let removed = self.device_removed_reason();
                let fault = crate::RingFault::ReleaseFailed { hr, removed };
                tracing::error!(target = %self.ccd, %fault, "IDD push: fatal slot release");
                return Err(anyhow::Error::new(fault)
                    .context("IDD-push slot release failed (rebuild required)"));
            }
        }
        self.out_idx = (i + 1) % ring_len;
        self.last_seq = seq;
        if fresh {
            self.last_slot = Some(slot);
        }
        if let Some((y, _, cbcr, _)) = pyro_slot.as_ref() {
            self.pyro_last = Some((y.clone(), cbcr.clone()));
        } else {
            self.last_present = Some((out.as_ref().expect("out ring").0.clone(), pf));
        }
        let now = Instant::now();
        if regen {
            // Re-encodes old desktop at a new pointer; must not feed freshness/stall bookkeeping.
        } else if self.recovering_since.take().is_some() {
            // Self-inflicted gap (ring recreate). Reset so it is not a DWM stall.
            self.stall_watch.reset();
        } else if let Some(stall) = self.stall_watch.note_fresh(now) {
            // ETW prose uses gap + 300 ms lead-in (the cause lands just before);
            // discriminator counts use the gap only — presents from healthy flow
            // would falsely acquit. Same ring snapshot, same clock.
            let (etw, etw_counts) = self
                .etw
                .as_ref()
                .and_then(|w| {
                    now.checked_sub(stall.gap)
                        .map(|from| w.window_report(from, now, Duration::from_millis(300)))
                })
                .unzip();
            let evidence = StallEvidence {
                // Re-attach restarts `offered_total` near zero; never a u64 underflow.
                offered_delta: self.telemetry().map(|(_, offered)| {
                    if offered >= self.offered_at_fresh {
                        offered - self.offered_at_fresh
                    } else {
                        offered
                    }
                }),
                max_heartbeat_age_ms: self.max_hb_age_us / 1_000,
                // Same window as the report's OS-event correlation (gap + cause lead-in).
                probes: now
                    .checked_sub(stall.gap + Duration::from_millis(300))
                    .zip(self.probes.as_deref())
                    .map(|(from, p)| p.window(from, now)),
                etw,
                etw_counts,
                // Gap accumulator only; this call's pending (ending-frame move) is still unfolded.
                cursor_moved_px: self.cursor_last.map(|_| self.cursor_gap_px),
            };
            self.stall_watch.report(&stall, now, &evidence);
        }
        if !regen {
            // Sustained ~2 fps stretch: per-hole lines gate on prior ACTIVE flow.
            if let Some(r) = self.stall_watch.take_recovery() {
                tracing::info!(
                    degraded_ms = r.degraded.as_millis() as u64,
                    holes = r.holes,
                    hole_time_ms = r.hole_time.as_millis() as u64,
                    worst_hole_ms = r.worst.as_millis() as u64,
                    "IDD-push capture recovered from a degraded stretch — fresh frames arrived \
                     only between stall-sized holes for its whole span; the per-stall lines \
                     above cover at most its first hole"
                );
            }
            // The stale-source watchdog's episode ends only when CONTINUOUS flow resumes: one
            // stash republish after its rebuild arrives as a lone fresh frame at the end of a
            // long gap and must NOT re-arm another silent stale floor over a frozen image.
            if self.last_fresh.elapsed() < Duration::from_secs(5) {
                self.stale_trips = 0;
            }
            // A fresh driver frame: feed the driver-death watch and roll the stall-evidence
            // trackers (a regen re-encodes OLD content — it is not evidence of driver progress).
            self.last_fresh = now;
            if let Some((_, offered)) = self.telemetry() {
                self.offered_at_fresh = offered;
            }
            self.max_hb_age_us = 0;
            // Pending sample is the ending frame's move — discarded, never folded.
            self.cursor_gap_px = 0;
            self.cursor_pending_px = 0;
            // The provenance sequence: a NEW source image, never a regen (which re-encodes the
            // previous one) — the one clock recovery logic may trust for source progress.
            self.source_seq += 1;
        }
        // PyroWave: Y is `texture`; CbCr + fence in `pyro`.
        let (texture, pyro) = if let Some((y, _, cbcr, _)) = pyro_slot {
            // SAFETY: on the owning capture/encode thread holding the immediate context.
            let (fence_handle, fence_value) =
                unsafe { self.pyro_fence_signal() }?.expect("pyrowave session signals its fence");
            (
                y,
                Some(PyroFrameShare {
                    cbcr,
                    fence_handle,
                    fence_value,
                    ring_gen: self.generation,
                }),
            )
        } else {
            (out.expect("out ring texture").0, None)
        };
        Ok(Some(CapturedFrame {
            provenance: if regen {
                pf_frame::Provenance::cursor_regen(self.source_seq)
            } else {
                pf_frame::Provenance::source(self.source_seq, self.source_present_qpc())
            },
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: pf,
            payload: FramePayload::D3d11(D3d11Frame {
                texture,
                device: self.device.clone(),
                pyro,
            }),
            cursor: None,
        }))
    }

    fn repeat_last(&mut self) -> Option<CapturedFrame> {
        // Fresh rotated slot so a repeat never re-hands a texture still encoding.
        // OUT_RING(3) > max pipeline_depth(2) so the rotated slot is not in flight.
        let i = self.out_idx;
        // Copy last Y+CbCr into a fresh two-plane slot; texture = Y, CbCr + fence in `pyro`.
        if self.pyrowave {
            let (src_y, src_cbcr) = self.pyro_last.clone()?;
            let slot = self.pyro_ring.get(i)?;
            let (dst_y, dst_cbcr) = (slot.y.clone(), slot.cbcr.clone());
            // SAFETY: GPU copies on the owning thread; src/dst are our pyro-ring planes.
            unsafe {
                self.context.CopyResource(&dst_y, &src_y);
                self.context.CopyResource(&dst_cbcr, &src_cbcr);
            }
            self.out_idx = (i + 1) % self.pyro_ring.len();
            self.pyro_last = Some((dst_y.clone(), dst_cbcr.clone()));
            // Fence the copies so the encoder reads completed textures.
            // SAFETY: owning capture/encode thread holds the immediate context.
            let (fence_handle, fence_value) = match unsafe { self.pyro_fence_signal() } {
                Ok(Some(f)) => f,
                _ => {
                    tracing::warn!("pyrowave: fence signal failed on a repeat frame — dropping it");
                    return None;
                }
            };
            return Some(CapturedFrame {
                provenance: pf_frame::Provenance::hold(self.source_seq),
                width: self.width,
                height: self.height,
                pts_ns: now_ns(),
                format: self.out_format().1,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: dst_y,
                    device: self.device.clone(),
                    pyro: Some(PyroFrameShare {
                        cbcr: dst_cbcr,
                        fence_handle,
                        fence_value,
                        ring_gen: self.generation,
                    }),
                }),
                cursor: None,
            });
        }
        let (src, pf) = self.last_present.clone()?;
        let dst = self.out_ring.get(i)?.tex.clone();
        // SAFETY: GPU copy on the owning thread; src/dst are our out-ring textures.
        unsafe {
            self.context.CopyResource(&dst, &src);
        }
        self.out_idx = (i + 1) % self.out_ring.len();
        self.last_present = Some((dst.clone(), pf));
        Some(CapturedFrame {
            provenance: pf_frame::Provenance::hold(self.source_seq),
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: pf,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: dst,
                device: self.device.clone(),
                pyro: None,
            }),
            cursor: None,
        })
    }
}

/// Duplicate `cs` into WUDFHost and `IOCTL_SET_CURSOR_CHANNEL`.
/// `true` = adopted. Idempotent driver-side (replaced worker is stopped).
fn deliver_cursor_channel(
    broker: &ChannelBroker,
    target_id: u32,
    cs: &cursor::CursorShared,
    send_cursor: &crate::CursorChannelSender,
) -> bool {
    // SAFETY: `cs.section_handle()` borrows the mapping `cs` owns for this call;
    // the broker's WUDFHost process handle is live for the broker's lifetime.
    let value = match unsafe { broker.dup_into_public(cs.section_handle()) } {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("cursor section duplication failed (composited cursor stays): {e:#}");
            return false;
        }
    };
    let req = pf_driver_proto::control::SetCursorChannelRequest {
        target_id,
        _pad: 0,
        header_handle: value,
    };
    match send_cursor(&req) {
        Ok(()) => {
            tracing::info!(
                target_id,
                "IDD push(host): cursor channel delivered — driver declares the hardware cursor"
            );
            true
        }
        Err(e) => {
            broker.close_remote_public(value);
            tracing::warn!("cursor channel delivery failed (composited cursor stays): {e:#}");
            false
        }
    }
}

impl Capturer for IddPushCapturer {
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.live_cursor()
    }

    fn set_cursor_forward(&mut self, on: bool) {
        // Capture model: hardware cursor stays declared (no working un-declare);
        // host blends. `composite_forced` cannot turn off — no client draws.
        let composite = (!on && self.cursor_shared.is_some()) || self.composite_forced;
        if self.composite_cursor != composite {
            self.composite_cursor = composite;
            self.last_blend_key = None; // regenerate immediately at the current pointer state
            tracing::info!(
                composite,
                "cursor render model: host compositing {}",
                if composite {
                    "ON (capture model — blending the pointer into frames)"
                } else {
                    "OFF (client draws locally)"
                }
            );
        }
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            // SAFETY: `self.event` is the live frame-ready handle; borrowed for this wait.
            // `WaitForSingleObject` only reads it; 16 ms bounds the wait.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), 16) };
            if let Some(f) = self.try_consume()? {
                return Ok(f);
            }
            if let Some(f) = self.repeat_last() {
                return Ok(f);
            }
            if Instant::now() > deadline {
                let (st, detail, lo, hi) = self.driver_diag();
                bail!(
                    "no IDD-push frame within 20s (target {}) — driver_status={st} detail=0x{detail:08x} \
                     driver_render_luid={hi:08x}:{lo:08x}. 0=driver never attached (swap-chain not \
                     assigned / driver not active), 1=attached but no frames (idle desktop?), 2=driver \
                     couldn't open our textures (render-adapter mismatch).",
                    self.target_id
                );
            }
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.try_consume()
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: Instant) {
        // Token is the truth (state, not edge — the auto-reset event may already
        // have been consumed). Event is the wakeup, sliced ≤16 ms like `next_frame`.
        loop {
            let tok = frame::FrameToken::unpack(self.latest());
            if tok.generation == self.generation && u64::from(tok.seq) != self.last_seq {
                return;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let ms = (left.as_millis() as u32).clamp(1, 16);
            // SAFETY: live frame-ready handle, borrowed for this wait. Bounded timeout slices it.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), ms) };
        }
    }

    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        // BT.2020 PQ while HDR. Driver does not forward IDDCX_HDR10_METADATA;
        // send the same generic HDR10 baseline as the native 0xCE path.
        self.display_hdr.then(pf_frame::hdr::generic_hdr10)
    }

    fn pipeline_depth(&self) -> usize {
        // Encode N while capture N+1 writes a different out-ring texture.
        // Ceiling is `OUT_RING - 1`: `d` in flight need `d + 1` slots.
        pf_host_config::config().idd_depth.clamp(1, OUT_RING - 1)
    }

    fn capture_target_id(&self) -> Option<u32> {
        Some(self.target_id)
    }

    fn resize_output(&mut self, width: u32, height: u32) -> bool {
        // Session already committed the new mode. Recreate now — no two-strike
        // debounce (that stays for external HDR/game mode-sets). Same recover-or-drop.
        if (width, height) == (self.width, self.height) {
            return true;
        }
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{}", self.width, self.height),
            to = format!("{width}x{height}"),
            "IDD push: host-initiated resize — recreating the ring at the new mode"
        );
        self.recovering_since.get_or_insert_with(Instant::now);
        if let Err(e) = self.recreate_ring(self.display_hdr, width, height) {
            tracing::warn!(
                error = %format!("{e:#}"),
                "IDD push: host-initiated ring recreate failed — falling back to a full rebuild"
            );
            return false;
        }
        true
    }

    fn recreate_ring_in_place(&mut self) -> bool {
        // Same-mode ring recreate (trait doc: swap-chain bounce recovery) — deliberately NOT
        // routed through `resize_output`, whose same-size fast path would no-op exactly the
        // case this exists for. Same recover-or-drop arming as the resize recreate.
        //
        // Restart OS presentation FIRST: the eviction's topology commit leaves DWM not
        // presenting to this display, so a re-attached ring would only ever receive the
        // driver's stash (measured: new_fps=0 forever after the re-attach). CDS_RESET forces a
        // real mode-set at the CURRENT mode — the same lever bring-up's ADD path relies on —
        // and the ring recreate below then re-attaches after that churn, not before it.
        match pf_win_display::win_display::resolve_gdi_name(self.ccd) {
            Some(gdi) => {
                if !pf_win_display::win_display::force_mode_reset(&gdi) {
                    tracing::warn!(
                        target_id = self.target_id,
                        "IDD push: presentation-restart mode reset failed — re-attaching anyway"
                    );
                }
            }
            None => tracing::warn!(
                target_id = self.target_id,
                "IDD push: no GDI name for the presentation-restart mode reset — re-attaching \
                 anyway"
            ),
        }
        tracing::info!(
            target_id = self.target_id,
            mode = format!("{}x{}", self.width, self.height),
            "IDD push: same-mode ring recreate — re-running the driver attach handshake"
        );
        self.recovering_since.get_or_insert_with(Instant::now);
        if let Err(e) = self.recreate_ring(self.display_hdr, self.width, self.height) {
            tracing::warn!(
                error = %format!("{e:#}"),
                "IDD push: same-mode ring recreate failed"
            );
            return false;
        }
        true
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
        // Must not leave per-target desired-state off: the next session would
        // adopt undeclared and silently run the composite model. Open-time reset
        // covers host crash; this is orderly teardown.
        if self.secure_active && self.cursor_shared.is_some() {
            if let Some(fwd) = self.cursor_forward.as_ref() {
                let _ = fwd(true);
            }
        }
        self.slots.clear();
        // Header, event, and WUDFHost handle free via RAII. Driver duplicates die
        // with publisher/monitor/WUDFHost (`design/idd-push-security.md`).
    }
}

#[cfg(test)]
mod tests {
    use super::stall::Stall;
    use super::*;

    /// The stale-source watchdog's whole contract (WP3b): under the floor nothing happens; over
    /// it, NO evidence stays idle (a static desktop is healthy), evidence buys exactly one
    /// rebuild, and the next expiry is terminal — never an indefinite repeat.
    #[test]
    fn stale_source_watchdog_is_one_rebuild_then_terminal() {
        let (floor, px) = (Duration::from_secs(15), 64u32);
        let s = Duration::from_secs;
        // Under the floor: nothing, whatever the evidence says.
        assert_eq!(
            stale_source_action(s(14), floor, 999, px, 5, 0),
            StaleAction::None
        );
        // Over the floor with no activity evidence: plain idle — no recovery.
        assert_eq!(
            stale_source_action(s(60), floor, 0, px, 0, 0),
            StaleAction::None
        );
        assert_eq!(
            stale_source_action(s(60), floor, px - 1, px, 0, 0),
            StaleAction::None
        );
        // Cursor motion over a frozen image, or undelivered driver offers: one rebuild…
        assert_eq!(
            stale_source_action(s(16), floor, px, px, 0, 0),
            StaleAction::Rebuild
        );
        assert_eq!(
            stale_source_action(s(16), floor, 0, px, 1, 0),
            StaleAction::Rebuild
        );
        // …and the second expiry is terminal.
        assert_eq!(
            stale_source_action(s(16), floor, px, px, 0, 1),
            StaleAction::Fail
        );
    }

    /// Every `AcquireSync` HRESULT class routes to a DISTINCT consequence (F4): timeout retries,
    /// abandoned poisons, negative fails typed — none may collapse into an ordinary no-frame.
    #[test]
    fn acquire_hresults_classify_disjointly() {
        assert_eq!(classify_acquire(0), AcquireClass::Acquired);
        assert_eq!(
            classify_acquire(WAIT_ABANDONED_HRESULT),
            AcquireClass::Abandoned
        );
        assert_eq!(classify_acquire(0x102), AcquireClass::Busy); // WAIT_TIMEOUT
        assert_eq!(
            classify_acquire(0x8007_000E_u32 as i32),
            AcquireClass::Fatal
        ); // E_OUTOFMEMORY
        assert_eq!(
            classify_acquire(0x887A_0005_u32 as i32),
            AcquireClass::Fatal
        ); // DEVICE_REMOVED
        assert_eq!(
            classify_acquire(0x8000_4005_u32 as i32),
            AcquireClass::Fatal
        ); // E_FAIL
    }

    /// The `CcdTargetKey` packing must equal `pf_frame::dxgi::pack_luid` — the capture target's
    /// `adapter_luid` (packed by pf-frame) is what pf-capture builds its CCD keys from, so a
    /// divergence would make every display-global helper miss its own target's paths. This crate
    /// is the lowest one that depends on both, which is why the assertion lives here.
    #[test]
    fn ccd_key_packing_matches_pf_frame_pack_luid() {
        for (low, high) in [
            (0u32, 0i32),
            (0xdead_beef, -2),
            (7, 0x7fff_ffff),
            (u32::MAX, -1),
        ] {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: low,
                HighPart: high,
            };
            assert_eq!(
                pf_win_display::win_display::CcdTargetKey::from_luid_parts(low, high, 1)
                    .adapter_luid,
                pf_frame::dxgi::pack_luid(luid),
                "packing diverged for LUID {high:#x}:{low:#x}"
            );
        }
    }

    /// W14: the mint must stay inside the publish token's 24-bit generation field, and must skip 0.
    ///
    /// `IDD_GENERATION` is a full `u32` while `FrameToken` carries 24 bits and `unpack` MASKS what it
    /// reads, so an unmasked `self.generation` stops matching any token past 2²⁴ recreates and
    /// `try_consume`'s `tok.generation != self.generation` becomes permanently true — every frame
    /// rejected, forever. The counter is parked just below the boundary here so the wrap is what gets
    /// exercised, not the happy path. (Same-module access to the private static; no capturer is
    /// running, and no other test touches it.)
    #[test]
    fn the_ring_generation_survives_the_publish_token() {
        IDD_GENERATION.store(frame::FrameToken::GENERATION_MASK - 2, Ordering::Relaxed);
        let mut seen = Vec::new();
        for _ in 0..8 {
            let g = next_generation();
            assert_ne!(g, 0, "0 also means the cleared-`latest` sentinel");
            assert_eq!(
                g & frame::FrameToken::GENERATION_MASK,
                g,
                "generation {g} does not fit the token's field"
            );
            // The pack/unpack `try_consume` performs.
            let tok = frame::FrameToken {
                generation: g,
                seq: 12345,
                slot: 2,
            };
            let back = frame::FrameToken::unpack(tok.pack());
            assert_eq!(back.generation, g, "generation lost in the token");
            assert_eq!(back.seq, 12345, "seq lost in the token");
            assert_eq!(back.slot, 2, "slot lost in the token");
            seen.push(g);
        }
        // Started 2 below the mask; wrap produced no duplicate 0.
        assert!(
            seen.contains(&frame::FrameToken::GENERATION_MASK),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|&g| g < 8),
            "the counter should have wrapped: {seen:?}"
        );
    }

    /// The 0 sentinel `recreate_ring` stores must never match a live generation.
    #[test]
    fn the_cleared_latest_sentinel_never_matches_a_live_generation() {
        let cleared = frame::FrameToken::unpack(0);
        assert_eq!(cleared.generation, 0);
        assert_eq!(cleared.seq, 0);
        for g in [1u32, 2, 0x7F_FFFF, frame::FrameToken::GENERATION_MASK] {
            assert_ne!(cleared.generation, g, "sentinel matched generation {g}");
        }
    }

    /// Feed [`StallWatch`] at `offsets_ms`; metronome is non-damage-idle, as `report` feeds it.
    fn watch_run(offsets_ms: &[u64]) -> Vec<Option<(Stall, Option<Duration>)>> {
        let base = Instant::now();
        let mut w = StallWatch::new();
        offsets_ms
            .iter()
            .map(|ms| {
                let at = base + Duration::from_millis(*ms);
                w.note_fresh(at).map(|s| {
                    let period = w.cycle(at, false);
                    (s, period)
                })
            })
            .collect()
    }

    fn flow(out: &mut Vec<u64>, start_ms: u64, frames: u64) {
        out.extend((0..frames).map(|i| start_ms + i * 16));
    }

    #[test]
    fn stall_detected_after_active_flow() {
        // 20 frames of 60 fps, then a 300 ms hole — the resuming frame is a stall.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.push(604);
        let out = watch_run(&t);
        assert!(out[..20].iter().all(Option::is_none));
        let (stall, period) = out[20].as_ref().expect("hole after active flow is a stall");
        assert_eq!(stall.gap.as_millis(), 300);
        assert!(period.is_none(), "one stall is not a cycle");
    }

    #[test]
    fn idle_desktop_gaps_are_not_stalls() {
        // ~530 ms caret blink: activity gate never opens.
        let t: Vec<u64> = (0..12).map(|i| i * 530).chain([20_000]).collect();
        assert!(watch_run(&t).iter().all(Option::is_none));
    }

    #[test]
    fn thirty_fps_content_still_qualifies_as_active() {
        // 33 ms cadence: 8 pre-gap frames span 231 ms ≤ ACTIVE_SPAN.
        let mut t: Vec<u64> = (0..10).map(|i| i * 33).collect(); // last at 297 ms
        t.push(497);
        let out = watch_run(&t);
        assert!(out[10].is_some(), "30 fps flow must pass the activity gate");
    }

    /// First degraded-stretch summary, checked after every frame like the capture loop.
    fn watch_recovery(offsets_ms: &[u64]) -> (StallWatch, Option<super::stall::Recovery>) {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut recovery = None;
        for ms in offsets_ms {
            w.note_fresh(base + Duration::from_millis(*ms));
            if let Some(r) = w.take_recovery() {
                recovery.get_or_insert(r);
            }
        }
        (w, recovery)
    }

    #[test]
    fn a_degraded_stretch_summarizes_on_recovery() {
        // ~2 fps phase (10×500 ms holes) after active flow: one summary for the stretch.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.extend((1..=10).map(|i| 304 + i * 500)); // 804..5304: ten 500 ms holes
        t.extend((1..=12).map(|i| 5304 + i * 16)); // sustained flow is back
        let (_, r) = watch_recovery(&t);
        let r = r.expect("a multi-hole degraded stretch summarizes at recovery");
        assert_eq!(r.holes, 10);
        assert_eq!(r.hole_time.as_millis(), 5000);
        assert_eq!(r.worst.as_millis(), 500);
        assert_eq!(r.degraded.as_millis(), 5000);
    }

    #[test]
    fn a_single_stall_never_summarizes() {
        // One hole in healthy flow: its stall line covers it; a one-hole stretch must not summarize.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.push(604); // the lone 300 ms hole
        t.extend((1..=12).map(|i| 604 + i * 16));
        let (_, r) = watch_recovery(&t);
        assert!(
            r.is_none(),
            "single stall must not produce a stretch summary"
        );
    }

    #[test]
    fn a_recreate_cut_stretch_still_summarizes() {
        // Recreate resets flow history mid-stretch; holes before it must still surface.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend((1..=3).map(|i| 304 + i * 500));
        let (mut w, r) = watch_recovery(&t);
        assert!(r.is_none(), "stretch still open — no summary yet");
        w.reset();
        let r = w
            .take_recovery()
            .expect("reset closes and summarizes the open stretch");
        assert_eq!(r.holes, 3);
        assert_eq!(r.hole_time.as_millis(), 1500);
    }

    #[test]
    fn a_content_stop_closes_the_stretch_without_folding_the_pause_in() {
        // Two degraded holes, then a 20 s pause. Summary covers the stretch only.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend([804, 1304, 21_304]);
        let (_, r) = watch_recovery(&t);
        let r = r.expect("the content stop closes the stretch");
        assert_eq!(r.holes, 2);
        assert_eq!(r.hole_time.as_millis(), 1000);
        assert_eq!(r.degraded.as_millis(), 1000);
    }

    #[test]
    fn metronomic_stalls_self_diagnose() {
        // ~300 ms DWM holes every 4 s in 60 fps flow. 5 cycles → 4 stalls; the 4th is the period.
        let mut t = Vec::new();
        for cycle in 0..5u64 {
            // ~3.7 s of flow, then the hole to the next cycle.
            flow(&mut t, cycle * 4_000, 232); // last frame at cycle*4000 + 3696
        }
        let out = watch_run(&t);
        let stalls: Vec<&(Stall, Option<Duration>)> = out.iter().flatten().collect();
        assert_eq!(stalls.len(), 4, "each cycle boundary is one stall");
        assert!(stalls[..3].iter().all(|(_, period)| period.is_none()));
        let period = stalls[3]
            .1
            .expect("the 4th evenly-spaced event completes the metronome streak");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.3,
            "period={period:?}"
        );
    }

    /// Same four evenly-spaced stalls as [`metronomic_stalls_self_diagnose`], one
    /// damage-idle: a hand/input pause is not display-disturbance evidence.
    #[test]
    fn damage_idle_stalls_do_not_feed_the_metronome() {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut periods = Vec::new();
        for cycle in 0..5u64 {
            let mut t = Vec::new();
            flow(&mut t, cycle * 4_000, 232);
            for ms in t {
                let at = base + Duration::from_millis(ms);
                if let Some(_stall) = w.note_fresh(at) {
                    // 2nd stall is damage-idle (cursor still on a dwm-only desktop).
                    let damage_idle = periods.len() == 1;
                    periods.push(w.cycle(at, damage_idle));
                }
            }
        }
        assert_eq!(periods.len(), 4);
        assert!(
            periods.iter().all(Option::is_none),
            "a skipped beat must break the streak: {periods:?}"
        );
    }

    #[test]
    fn reset_swallows_the_recreate_gap() {
        // Recreate, then resume 800 ms later: not a stall; detection re-arms after.
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut w = StallWatch::new();
        for i in 0..20u64 {
            assert!(w.note_fresh(at(i * 16)).is_none());
        }
        w.reset();
        assert!(w.note_fresh(at(1_104)).is_none(), "recreate gap swallowed");
        for i in 1..20u64 {
            assert!(w.note_fresh(at(1_104 + i * 16)).is_none());
        }
        assert!(
            w.note_fresh(at(1_104 + 19 * 16 + 300)).is_some(),
            "detection re-armed after the reset"
        );
    }

    /// Third stall in 60 s warns; quiet through 300 s re-warn spacing; re-arms after age-out.
    #[test]
    fn stall_rate_warn_window_and_rewarn() {
        let base = Instant::now();
        let at = |s: u64| base + Duration::from_secs(s);
        let mut w = StallWatch::new();
        assert_eq!(w.note_for_rate_warn(at(0)), None);
        assert_eq!(w.note_for_rate_warn(at(10)), None);
        assert_eq!(
            w.note_for_rate_warn(at(20)),
            Some(3),
            "third stall in 60 s warns"
        );
        assert_eq!(
            w.note_for_rate_warn(at(30)),
            None,
            "inside the re-warn spacing the arm stays quiet"
        );
        // Past the spacing: old entries aged out, so RATE_MIN_STALLS again then re-warns.
        assert_eq!(w.note_for_rate_warn(at(400)), None);
        assert_eq!(w.note_for_rate_warn(at(401)), None);
        assert_eq!(
            w.note_for_rate_warn(at(402)),
            Some(3),
            "re-warns after the spacing"
        );
    }

    /// [`stall::attribute`] verdict table.
    #[test]
    fn stall_attribution_verdicts() {
        use super::stall::{attribute, StallVerdict};
        let verdict = |gap_ms: u64, offered: Option<u64>, hb_age_ms: u64| {
            attribute(
                Duration::from_millis(gap_ms),
                &StallEvidence {
                    offered_delta: offered,
                    max_heartbeat_age_ms: hb_age_ms,
                    probes: None,
                    etw: None,
                    etw_counts: None,
                    cursor_moved_px: None,
                },
            )
        };
        // Pre-telemetry driver: no verdict.
        assert_eq!(verdict(300, None, 500), StallVerdict::NoTelemetry);
        // Heartbeat silent for most of the hole → worker starved.
        assert_eq!(verdict(600, Some(0), 400), StallVerdict::WorkerStalled);
        assert_eq!(verdict(600, Some(50), 300), StallVerdict::WorkerStalled);
        // ≤16 ms heartbeat; 200 ms silence on a 300 ms gap is under max(gap/2, 250 ms).
        assert_eq!(verdict(300, Some(1), 200), StallVerdict::ComposeSilence);
        // Stall-ending frame (+ a small resume burst) does not acquit DWM.
        assert_eq!(verdict(300, Some(3), 20), StallVerdict::ComposeSilence);
        // Sustained composition through the hole does: frames existed, we lost them.
        assert_eq!(verdict(300, Some(8), 20), StallVerdict::DeliveryLeg);
        assert_eq!(verdict(2_000, Some(120), 30), StallVerdict::DeliveryLeg);
        // Long holes scale the bar: 900 ms silence on a 3 s gap is not half.
        assert_eq!(verdict(3_000, Some(2), 900), StallVerdict::ComposeSilence);
        assert_eq!(verdict(3_000, Some(2), 1_600), StallVerdict::WorkerStalled);
    }

    /// [`stall::classify`]: probes + ETW present/queue counts refine the telemetry verdict.
    #[test]
    fn stall_classification_matrix() {
        use super::dxgkrnl_etw::EtwWindowCounts;
        use super::stall::{ProbeWindow, StallClass, StallVerdict};
        let gap = Duration::from_millis(600);
        let probes = |fence: Option<u64>, dwm: Option<u64>, flush: Option<u64>| ProbeWindow {
            fence_max_us: fence,
            dwm_tick_frozen_us: dwm,
            dwm_flush_max_us: flush,
            ..ProbeWindow::default()
        };
        let counts = |presents: u32, queue_adds: u32| EtwWindowCounts {
            presents,
            queue_adds,
            present_history: true,
            queue_history: true,
            flow_dwm_only: false,
        };
        // `cursor_moved_px = None` is pre-witness classification (`damage_idle_split`
        // owns the witness). Nested fn, not a closure: each call site's temporaries
        // need their own lifetime.
        fn classify(
            gap: Duration,
            verdict: &StallVerdict,
            p: Option<&ProbeWindow>,
            c: Option<&EtwWindowCounts>,
        ) -> StallClass {
            super::stall::classify(gap, verdict, p, c, None)
        }
        // Driver verdicts win — probes cannot overrule "we lost the frames".
        assert_eq!(
            classify(
                gap,
                &StallVerdict::WorkerStalled,
                Some(&probes(Some(500_000), None, None)),
                None
            ),
            StallClass::OursWorker
        );
        assert_eq!(
            classify(gap, &StallVerdict::DeliveryLeg, None, None),
            StallClass::OursDelivery
        );
        // No probes: compose-silence alone cannot name a class.
        assert_eq!(
            classify(gap, &StallVerdict::ComposeSilence, None, None),
            StallClass::Unattributed
        );
        // Fences stalled ≥ gap/2 → adapter froze (even without driver telemetry).
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(400_000), Some(400_000), None)),
                None
            ),
            StallClass::AdapterFreeze
        );
        assert_eq!(
            classify(
                gap,
                &StallVerdict::NoTelemetry,
                Some(&probes(Some(400_000), None, None)),
                None
            ),
            StallClass::AdapterFreeze
        );
        // Fences fine (16 ms) but DWM's tick froze; DwmFlush counts too.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(500_000), None)),
                None
            ),
            StallClass::CompositorBlocked
        );
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(450_000))),
                None
            ),
            StallClass::CompositorBlocked
        );
        // Alive + E_PENDING, but no working present witness: Unattributed, never a guess.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                None
            ),
            StallClass::Unattributed
        );
        // A witness that has never produced an event is not a working witness.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&EtwWindowCounts {
                    present_history: false,
                    ..EtwWindowCounts::default()
                })
            ),
            StallClass::Unattributed
        );
        // Presents through the hole while the virtual queue starved → FrameGeneration.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(54, 0))
            ),
            StallClass::FrameGeneration
        );
        // Stall-ending frame + caret blink stay under the bar → ContentSilence.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(2, 1))
            ),
            StallClass::ContentSilence
        );
        // Live witness (history true) reading exact zero is a measurement, not an absence.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(0, 0))
            ),
            StallClass::ContentSilence
        );
        // Present witness only refines compose-silence; it does not overrule harder classes.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(400_000), Some(400_000), None)),
                Some(&counts(54, 0))
            ),
            StallClass::AdapterFreeze
        );
        // Healthy probes, pre-telemetry driver: delivery-leg is equally possible.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::NoTelemetry,
                Some(&probes(Some(16_000), Some(20_000), None)),
                Some(&counts(54, 0))
            ),
            StallClass::Unattributed
        );
        // Absent probe never reads as stalled; a working present witness still splits.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(None, Some(20_000), Some(30_000))),
                Some(&counts(1, 0))
            ),
            StallClass::ContentSilence
        );
    }

    /// Present-free hole on a dwm-only desktop: still cursor → DamageIdle; moving
    /// cursor → ContentSilence. A game session (not dwm-only) is never demoted.
    #[test]
    fn damage_idle_split() {
        use super::dxgkrnl_etw::EtwWindowCounts;
        use super::stall::{classify, ProbeWindow, StallClass, StallVerdict};
        let gap = Duration::from_millis(600);
        let healthy = ProbeWindow {
            fence_max_us: Some(16_000),
            dwm_tick_frozen_us: Some(20_000),
            dwm_flush_max_us: Some(30_000),
            ..ProbeWindow::default()
        };
        let counts = |dwm_only: bool| EtwWindowCounts {
            presents: 0,
            queue_adds: 0,
            present_history: true,
            queue_history: true,
            flow_dwm_only: dwm_only,
        };
        let run = |dwm_only: bool, moved: Option<u32>| {
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&healthy),
                Some(&counts(dwm_only)),
                moved,
            )
        };
        assert_eq!(run(true, Some(0)), StallClass::DamageIdle);
        assert_eq!(run(true, Some(312)), StallClass::ContentSilence);
        // Game in the lookback: hole is real evidence even with a still cursor.
        assert_eq!(run(false, Some(0)), StallClass::ContentSilence);
        // No witness (`GetCursorPos` failing): pre-witness behavior.
        assert_eq!(run(true, None), StallClass::ContentSilence);
        // Witness never overrules a harder conviction: stalled fences stay AdapterFreeze.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&ProbeWindow {
                    fence_max_us: Some(400_000),
                    ..ProbeWindow::default()
                }),
                Some(&counts(true)),
                Some(0),
            ),
            StallClass::AdapterFreeze
        );
    }
}
