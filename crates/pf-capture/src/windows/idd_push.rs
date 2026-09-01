//! P2 direct frame push (kill DDA) — HOST side, over the **sealed channel**
//! (`design/idd-push-security.md`). The frame channel carries whole-desktop pixels, so its protection
//! must match DDA's (where capturer and consumer are one process and there is no openable channel at
//! all): the HOST (SYSTEM) creates the shared header + frame-ready event + ring of keyed-mutex textures
//! **UNNAMED** on the discrete render GPU — nothing to enumerate, open by name, or pre-create
//! ("squat") — then DUPLICATES the handles into the pf-vdisplay driver's WUDFHost process
//! ([`ChannelBroker`]; SYSTEM can `DuplicateHandle` into the LocalService host, the reverse is
//! correctly denied, which is why the HOST is the broker) and delivers the handle VALUES over the
//! SYSTEM-only control device (`IOCTL_SET_FRAME_CHANNEL`). A handle value is meaningless outside the
//! target process's handle table, so the bootstrap's ACL is not load-bearing; the only way to reach the
//! frames is to already be one of the two endpoint processes. The driver copies frames in; we consume
//! the ring straight into the zero-copy NVENC path — no DXGI Desktop Duplication, no `win32u` hook.
//! The SOLE Windows capture path. Driver counterpart: `packaging/windows/drivers/pf-vdisplay/src/
//! frame_transport.rs`. The shared `SharedHeader` layout, `MAGIC`/`VERSION`/`RING_LEN`, the
//! `DRV_STATUS_*` codes, the channel-delivery struct and the publish token all come from
//! [`pf_driver_proto`] (which OWNS the contract, with `const` size asserts) — both sides `use` it, so
//! drift is a compile error rather than a "must match" comment.

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

// The frame-transport contract — `SharedHeader` layout, `MAGIC`/`VERSION`/`RING_LEN`, the
// `DRV_STATUS_*` codes and the channel-delivery struct — lives in `pf_driver_proto`; both sides
// `use` it, so a layout/code drift is a compile error (the proto has `const` size asserts).
use frame::{
    unpack_opened_detail, SharedHeader, DRV_STATUS_BIND_FAIL, DRV_STATUS_NONE,
    DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED, DRV_STATUS_TEX_FAIL, MAGIC, RING_LEN, VERSION,
};

/// `DXGI_SHARED_RESOURCE_READ | _WRITE` for `CreateSharedHandle`/`OpenSharedResourceByName`. Local (not
/// part of the proto contract — it is a DXGI sharing-API arg, mirrored on the driver side).
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;

/// Least access the driver needs on the duplicated **header section**: map it read/write (it reads the
/// layout + writes `driver_status`/`driver_render_luid`/the publish token). `SECTION_MAP_READ |
/// SECTION_MAP_WRITE` (== the driver's `FILE_MAP_READ | FILE_MAP_WRITE` map flag). Duplicating with
/// exactly this — instead of `DUPLICATE_SAME_ACCESS`, which would copy the host's full-access creator
/// handle — is the "grant least privilege" discipline for unnamed shared objects (Raymond Chen,
/// *"unnamed objects aren't safe just because they're unnamed"*): a compromised driver's handle can't
/// `WRITE_DAC`/`WRITE_OWNER`/`DELETE` the object, only map it.
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;
/// Least access the driver needs on the duplicated **frame-ready event**: it only `SetEvent`s it, which
/// requires `EVENT_MODIFY_STATE`. (The host holds `SYNCHRONIZE` on its own handle to wait.)
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// Host-owned output-ring depth: distinct NVENC-input textures rotated per frame so the in-flight
/// encode of frame N and the convert/copy of frame N+1 never touch the same texture. 3 covers a
/// pipeline depth of 2 with one slot of margin.
const OUT_RING: usize = 3;

/// Monotonic per-process generation stamped into the header + every publish token, so the host rejects
/// a stale-ring publish and the driver detects a recreate. (With unnamed textures there is no name
/// collision to avoid — the generation's remaining job is the recreate/stale-publish handshake.)
static IDD_GENERATION: AtomicU32 = AtomicU32::new(1);

/// Mint the next ring generation: masked to [`frame::FrameToken::GENERATION_MASK`] and never `0`.
///
/// [`IDD_GENERATION`] is a full `u32`, but the publish token carries only 24 bits of it and
/// `FrameToken::unpack` masks what it reads — so an unmasked `self.generation` stops matching ANY
/// token past 2²⁴ recreates and `try_consume`'s `tok.generation != self.generation` becomes
/// permanently true: every frame rejected, forever. `0` is skipped because it is also the
/// cleared-`latest` sentinel [`IddPushCapturer::recreate_ring`] stores. Latent — 2²⁴ recreates is a
/// long session — but the invariant belongs at the single mint point, not in the comparison.
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

/// RAII wrapper for a file-mapping object + its mapped view: on drop the view is `UnmapViewOfFile`'d,
/// THEN the [`OwnedHandle`] closes the underlying mapping object (order matters — unmap before close).
/// A `header` raw pointer borrows into the view via [`ptr`](Self::ptr); the section must
/// outlive it (it's declared before it in [`IddPushCapturer`], and moving the section doesn't move the
/// OS mapping, so the borrowed pointer stays valid).
struct MappedSection {
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl MappedSection {
    /// The mapped view base as a `*mut T` (a borrow into the section; valid only while it lives).
    fn ptr<T>(&self) -> *mut T {
        self.view.Value as *mut T
    }
}

impl Drop for MappedSection {
    fn drop(&mut self) {
        // SAFETY: `view` is the live view we created with `MapViewOfFile` and have not yet unmapped;
        // unmap it BEFORE `handle` (the OwnedHandle) closes the mapping object — order matters.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

struct HostSlot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    /// The UNNAMED shared-resource NT handle: keeps the resource alive for the session AND is the
    /// source the [`ChannelBroker`] duplicates into the driver's WUDFHost (the ONLY way the driver can
    /// reach this texture — there is no name to open). An [`OwnedHandle`] so it closes on drop.
    shared: OwnedHandle,
    /// SRV on the slot texture so the HDR path samples the FP16 slot DIRECTLY (no slot→scratch copy);
    /// the convert pass writes the output ring while holding the slot's keyed mutex. Unused for SDR
    /// (which converts the BGRA slot → NV12 on the video engine, via its own per-frame input view).
    srv: ID3D11ShaderResourceView,
}

/// One host out-ring slot: the NVENC/AMF/QSV encode-input texture, plus — when the output format is
/// P010 — the two per-plane RTVs the HDR shader passes render through.
///
/// The views live HERE, built once with the slot, because they are a property of the texture and the
/// mode, not of the frame: `HdrP010Converter::convert` used to create both on EVERY HDR frame, inside
/// the ring slot's keyed-mutex hold, so the driver sat blocked on that slot while we allocated views.
struct OutSlot {
    tex: ID3D11Texture2D,
    /// `(luma R16_UNORM, chroma R16G16_UNORM)` plane views. `None` for NV12/BGRA outputs, which the
    /// video processor or a plain `CopyResource` writes without an RTV of ours.
    p010: Option<(ID3D11RenderTargetView, ID3D11RenderTargetView)>,
    /// Plain RTV of the packed 10-bit slot, for the HDR + 4:4:4 output ([`HdrRgb10Converter`]).
    /// `None` for every other format.
    rgb10: Option<ID3D11RenderTargetView>,
}

/// One PyroWave output-ring slot: the two SEPARATE shareable plane textures the wavelet encoder
/// imports (design/pyrowave-windows-host-zerocopy.md) plus their RTVs (the [`BgraToYuvPlanes`] CSC
/// renders into them). Y is full-res `R8_UNORM`, CbCr is half-res `R8G8_UNORM`; both are
/// `SHARED | SHARED_NTHANDLE`. Rotated per frame like `out_ring` so encode N and convert N+1 touch
/// different textures.
struct PyroOutSlot {
    y: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    cbcr: ID3D11Texture2D,
    cbcr_rtv: ID3D11RenderTargetView,
}

/// RAII guard over an [`IDXGIKeyedMutex`]: [`acquire`](Self::acquire) does `AcquireSync(key, timeout)`,
/// `Drop` does `ReleaseSync(key)`. So the lock is released even if the work between acquire and the end
/// of the guard's scope `?`-returns or panics — the "leak the keyed-mutex lock → stall the driver on
/// that slot" footgun the consume loop guards against by hand. Keeps the hot loop free of a raw
/// `ReleaseSync` that a future early-return could skip.
struct KeyedMutexGuard<'a> {
    mutex: &'a IDXGIKeyedMutex,
    key: u64,
}

/// `WAIT_ABANDONED` as an HRESULT: the driver died while holding the slot's keyed mutex — ownership
/// still transferred to this caller. SUCCESS-severity (positive), like `WAIT_TIMEOUT` (0x102): the
/// windows-rs `Result` wrapper erases both (`.ok()` maps every non-negative HRESULT to `Ok(())`), so
/// acquisition MUST be classified on the raw vtable HRESULT. Mirrors the driver's constants
/// (`frame_transport.rs`).
const WAIT_ABANDONED_HRESULT: i32 = 0x0000_0080;

impl<'a> KeyedMutexGuard<'a> {
    /// Acquire `mutex` at `key`, waiting up to `timeout_ms`. `None` if the acquire times out / errors
    /// (the caller skips the frame), so the guard is only ever held when the lock is genuinely held.
    fn acquire(
        mutex: &'a IDXGIKeyedMutex,
        key: u64,
        timeout_ms: u32,
    ) -> Option<KeyedMutexGuard<'a>> {
        // SAFETY: `mutex` is a live `IDXGIKeyedMutex` on this thread's immediate-context device.
        // Raw vtable call, NOT the `Result` wrapper: `.is_err()` treated WAIT_TIMEOUT (positive =
        // `Ok`) as acquired, handing out a guard for a slot the DRIVER still held — converting from
        // a texture mid-copy (torn frame) and `ReleaseSync`ing a key this side never took.
        let hr = unsafe {
            (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, timeout_ms)
        };
        match hr.0 {
            // Acquired — S_OK, or WAIT_ABANDONED (the driver died holding the slot: the lock is
            // OURS now, and refusing the guard would leave the key held forever, wedging the slot).
            0 | WAIT_ABANDONED_HRESULT => Some(KeyedMutexGuard { mutex, key }),
            // WAIT_TIMEOUT (slot busy — the caller skips this frame) or a genuine error: never held.
            _ => None,
        }
    }
}

impl Drop for KeyedMutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: we hold `mutex` at `key` (acquired in `acquire`, never released elsewhere); release it.
        unsafe {
            let _ = self.mutex.ReleaseSync(self.key);
        }
    }
}

/// Confirm the process is a genuine system WUDFHost — `%SystemRoot%\System32\WUDFHost.exe` — before a
/// broker duplicates sensitive handles into it. The pid is driver-reported (the frame channel's
/// [`control::AddReply::wudf_pid`], or the gamepad bootstrap's `driver_pid`); a spoofed devnode / a
/// tampered mailbox could name an arbitrary process to receive the channel, so this is the
/// confused-deputy gate. `what` names the channel in the error (e.g. `"frame-channel"`); shared with
/// the gamepad sealed channel (`inject/windows/gamepad_raii.rs`).
///
/// # What this does and does NOT prove (security-review 2026-07-28)
///
/// It proves the target's image is the system WUDFHost binary. It does **not** prove the target is
/// *the* WUDFHost hosting our devnode, and it is not an authorization check: `WUDFHost.exe` is
/// world-executable, so anyone who can name a pid to a broker can first spawn their own copy (e.g.
/// `CREATE_SUSPENDED`, which parks it indefinitely with the right image path) and pass this check.
/// It therefore only screens out *non-WUDFHost* pids — an attacker exe, a stale/wrong devnode.
///
/// Whether that is sufficient depends entirely on who can name the pid, so it must be judged at each
/// caller, NOT here:
/// - **Frame channel** — sufficient. The pid is `AddReply::wudf_pid`, which the driver fills with its
///   own `GetCurrentProcessId()` and returns over the pf-vdisplay control device, whose DACL is
///   `D:P(A;;GA;;;SY)(A;;GA;;;BA)`. Only SYSTEM/Administrators can speak on that channel, and both are
///   out of scope by SECURITY.md.
/// - **Gamepad/mouse channel** — NOT sufficient on its own. The pid comes from a `Global\` mailbox
///   that LocalService can write, so this check does not identify the caller; see the corrected
///   analysis and the one-delivery rule in `pf-inject/src/inject/windows/gamepad_raii.rs`.
///
/// Do not add a token/session/account check here expecting it to fix the gamepad case: a genuine
/// UMDF host and a LocalService attacker's spawned WUDFHost are both session 0 and both LocalService,
/// so such checks discriminate nothing while risking a false negative that would silently kill
/// display capture and every virtual pad.
///
/// # Safety
/// `process` must be a live process handle carrying `PROCESS_QUERY_LIMITED_INFORMATION`.
pub unsafe fn verify_is_wudfhost(process: HANDLE, wudf_pid: u32, what: &str) -> Result<()> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` carries QUERY_LIMITED per the contract; `buf`/`len` are a valid out-buffer and
    // its capacity, and on success `len` is updated to the count of UTF-16 units written (no NUL).
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
// The one-time construction half (sweep Phase 5.4): adapter resolve + HDR negotiate + ring/header/
// event creation + channel delivery + the bounded first-frame gate. Read this file when a session
// will not START; read the steady state below when one stops flowing.
#[path = "idd_push/open.rs"]
mod open;
// The synthetic-input DWM compose kick — a fallback for the driver's own frame republish.
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
// Stall attribution (vdisplay-disturbance-immunity Phase A): the DxgKrnl ETW watch (A.3), the
// micro-probe engine (A.2), and the stall watch + verdict matrix that folds them (A.1).
#[path = "idd_push/dxgkrnl_etw.rs"]
mod dxgkrnl_etw;
#[path = "idd_push/probes.rs"]
mod probes;
#[path = "idd_push/stall.rs"]
mod stall;
use channel::ChannelBroker;
use descriptor::{DescriptorPoller, DisplayDescriptor};
use stall::{StallEvidence, StallWatch};

/// Creates + owns the shared ring; yields the driver's frames as [`FramePayload::D3d11`].
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
    /// Owns the shared-header file mapping + its mapped view (RAII unmap-then-close). Declared BEFORE
    /// `header`, which is a raw pointer borrowed into this view via [`MappedSection::ptr`]. Also the
    /// duplication source for the driver's header handle on every [`ChannelBroker::send`].
    section: MappedSection,
    header: *mut SharedHeader,
    event: OwnedHandle,
    /// The sealed channel's handle-duplication broker (WUDFHost process + control device); used at open
    /// and again on every ring recreate to deliver fresh duplicates.
    broker: ChannelBroker,
    /// The v5 hardware-cursor channel's host end (`Some` = delivered; the driver declared the
    /// hardware cursor and seqlock-publishes into it). Survives ring recreates — the section is
    /// independent of the frame ring's generation. With the channel delivered, the driver's
    /// hardware cursor keeps DWM from compositing ANY cursor into the frame; the SHAPE now comes
    /// from [`cursor_poll::CursorPoller`] (the IddCx query is alpha-only — see cursor_poll.rs),
    /// and this shm read is the fallback if that poller dies.
    cursor_shared: Option<cursor::CursorShared>,
    /// The GDI cursor-shape poller (design §8): the overlay source while alive. `Some` when
    /// `cursor_shared` is (both ride the negotiated cursor channel + successful delivery) — or
    /// when `composite_forced` (no channel, but the target's sticky declare needs a blend source).
    cursor_poll: Option<cursor_poll::CursorPoller>,
    /// Retained delivery sender (`IOCTL_SET_CURSOR_CHANNEL`) for RE-delivery: a driver-side
    /// monitor re-arrival (match-window re-arrival resize, a sibling session recreating the
    /// shared slot) destroys the driver's cursor worker — the section here survives, so the
    /// channel is re-delivered on ring recreates.
    cursor_sender: Option<crate::CursorChannelSender>,
    /// The cursor-render flip sender (`IOCTL_SET_CURSOR_FORWARD`) — the secure-desktop guard's
    /// actuator. UAC/Winlogon render only through the OS's software-cursor path (its default on
    /// every mode commit); with our hardware cursor declared (and re-declared on every
    /// swap-chain assign) that path never comes back, and the secure desktop never presents —
    /// the stream freezes on the last normal-desktop frame for the whole UAC/lock interaction.
    /// [`Self::poll_secure_desktop`] flips the declare off/on at the secure-desktop edges.
    cursor_forward: Option<crate::CursorForwardSender>,
    /// The secure-desktop guard's edge state: `true` = the poller reports a secure input
    /// desktop and the declare is currently stood down.
    secure_active: bool,
    /// The CAPTURE mouse model is active — the HOST composites the pointer into the frame
    /// (see cursor_blend.rs for why DWM cannot: a declared IddCx hardware cursor is forever).
    composite_cursor: bool,
    /// This session never negotiated the cursor channel but its target carries an IRREVOCABLE
    /// hardware-cursor declare from an earlier session (`WinCaptureTarget::cursor_excluded`,
    /// §8.6): DWM delivers pointer-free frames and no client draws the cursor, so the ONLY path
    /// to a visible pointer is compositing here. Pins `composite_cursor` on — nothing may turn
    /// it off (there is no channel to hand the pointer to).
    composite_forced: bool,
    /// The cursor-quad blend pass (lazy; per capture device). `None` after a build failure —
    /// composite mode then degrades to pointer-less frames (warned once).
    cursor_blend: Option<cursor_blend::CursorBlendPass>,
    cursor_blend_failed: bool,
    /// Sticky: [`Self::live_cursor`] has fallen back to the driver's shm section. The two sources
    /// keep independent serial namespaces, so once crossed we never go back (see there).
    cursor_shm_latched: bool,
    /// The frame-sized blend scratch (slot copy + cursor quad): texture + SRV + (w, h, fmt)
    /// it was built for — rebuilt when the ring geometry changes.
    blend_scratch: Option<(
        ID3D11Texture2D,
        ID3D11ShaderResourceView,
        u32,
        u32,
        DXGI_FORMAT,
    )>,
    /// The (serial, x, y, visible) of the LAST blended pointer — the composite-regen change
    /// key: pointer-only motion produces no driver publish (the declared hardware cursor
    /// doesn't dirty frames), so `try_consume` regenerates from the last slot when this moves.
    last_blend_key: Option<(u64, i32, i32, bool)>,
    /// The ring slot of the last FRESH publish — the regen source.
    last_slot: Option<usize>,
    /// The target's SDR-white scale (vs 80 nits) for HDR cursor compositing — refreshed on
    /// each blend-scratch rebuild (first use + ring geometry changes). 2.5 ≈ the Windows
    /// SDR-brightness default; without it the composited cursor renders visibly dark on HDR.
    sdr_white_scale: f32,
    width: u32,
    height: u32,
    slots: Vec<HostSlot>,
    /// The ring/texture generation, bumped every time the ring is recreated at a new format (the
    /// display's HDR mode flipped). Stamped into the header + each delivery so the driver re-attaches
    /// (and so stale-ring publishes are rejected).
    generation: u32,
    /// The session negotiated **HDR** (client advertised `VIDEO_CAP_HDR` and the handshake said
    /// yes — no longer merely `bit_depth >= 10`, which the 10-bit SDR path below also reaches).
    /// Gates the composition depth: an HDR session PROACTIVELY enables advanced color at `open`
    /// (HDR without a manual toggle); any other session forces it OFF and the descriptor poller
    /// PINS it there, so a client that did not ask for HDR is never handed the in-band PQ
    /// upgrade the pixel-format-driven encoder would otherwise stamp from an HDR composition.
    /// (An HDR-negotiated H.26x session still follows a host-side "Use HDR" flip; all clients
    /// decode Main10 + auto-detect PQ from the VUI.)
    want_hdr: bool,
    /// The session negotiated 10-bit WITHOUT HDR (`OutputFormat::ten_bit_sdr`): the BGRA slot is
    /// expanded 8→10 bit into the packed RGB10 output ([`PixelFormat::Rgb10a2Sdr`]) so NVENC
    /// encodes Main10 under the ordinary BT.709 SDR VUI. The display's colour state is never
    /// touched — `want_hdr` above stays false, advanced colour stays pinned off.
    ten_bit_sdr: bool,
    /// The DISPLAY's CURRENT HDR state (from `advanced_color_enabled`) — the user can flip "Use HDR" in
    /// Windows mid-session. Drives the ring format (HDR → FP16 surfaces, SDR → BGRA) and the conversion.
    /// Polled in the capture loop; a change recreates the ring (see [`Self::recreate_ring`]).
    display_hdr: bool,
    /// One-shot latch for [`Self::poll_display_hdr`]'s "could not pin the negotiated depth" error —
    /// the poller samples every ~250 ms, and a display that refuses the flip refuses it every time.
    hdr_pin_warned: bool,
    /// Consecutive failed depth-pin attempts ([`Self::poll_display_hdr`]) — past
    /// [`Self::HDR_PIN_EAGER`] the re-pin backs off to every [`Self::HDR_PIN_RETRY_EVERY`]th
    /// descriptor sample: a display that refuses the flip was costing 4 CCD writes + 8 display-
    /// config queries per second, forever, all on the session-global display-config lock.
    hdr_pin_failures: u32,
    /// The session negotiated full-chroma 4:4:4: while the display is SDR the BGRA slot passes
    /// THROUGH (a plain copy into the out ring, no NV12 VideoConverter) so NVENC gets full-chroma
    /// RGB and CSCs to 4:4:4 itself — measured on-glass: `chromaFormatIDC=3` + ARGB input yields
    /// TRUE 4:4:4 and the conversion follows the VUI matrix (BT.709 limited, always written).
    /// While the display is HDR the same idea runs at 10 bits: [`HdrRgb10Converter`] writes packed
    /// BT.2020 PQ RGB and NVENC CSCs that to YUV 4:4:4 (Main 4:4:4 10). Either way the chroma the
    /// Welcome promised is the chroma the wire carries.
    want_444: bool,
    /// A PyroWave (wavelet) session (design/pyrowave-windows-host-zerocopy.md +
    /// design/pyrowave-444-hdr.md). When set, frames come from the separate-plane `pyro_ring`
    /// (shareable Y + CbCr textures the mode-aware [`BgraToYuvPlanes`] CSC writes) and a **shared
    /// fence** is signalled after each convert, so the pyrowave encoder zero-copy-imports the two
    /// textures into its own Vulkan device ordered after the D3D11 convert. The composition is
    /// PINNED to the negotiated depth: SDR sessions force advanced color OFF (8-bit BGRA → R8
    /// planes), 10-bit sessions enable it like H.26x (scRGB FP16 → R16 studio-code planes);
    /// `want_444` sizes the chroma plane full-res.
    pyrowave: bool,
    /// PyroWave: the shared D3D11 timeline fence (created lazily on the first frame, `SHARED` flag).
    /// The capturer `Signal`s it after each frame's GPU convert; the encoder's Vulkan side waits it.
    pyro_fence: Option<ID3D11Fence>,
    /// PyroWave: the fence's persistent shared NT handle, passed (as a raw value) on EVERY frame. The
    /// encoder DUPLICATEs + imports it as a Vulkan timeline semaphore whenever it has none (first
    /// frame or after an encoder rebuild), so this original must stay valid across rebuilds — hence
    /// one handle for the capturer's whole life. An [`OwnedHandle`] so it is CLOSED when the capturer
    /// drops; it used to be a bare `isize` that nothing ever closed, leaking one NT handle per
    /// PyroWave session. Closing ours cannot disturb the encoder: it holds its own duplicate.
    pyro_fence_handle: Option<OwnedHandle>,
    /// PyroWave: the monotonically increasing fence value (one `Signal` per emitted frame).
    pyro_fence_value: u64,
    /// PyroWave: the separate-plane output ring (Y R8 + CbCr R8G8 shareable textures + RTVs), used
    /// INSTEAD of `out_ring` for a pyrowave session. Built lazily; rebuilt on a mode change.
    pyro_ring: Vec<PyroOutSlot>,
    /// PyroWave: the BGRA→YUV-planes CSC (BT.709 limited, matching `rgb2yuv.comp`). Built lazily.
    pyro_conv: Option<BgraToYuvPlanes>,
    /// PyroWave: the last presented (Y, CbCr) textures — the repeat source (analogue of
    /// `last_present` for the two-plane path).
    pyro_last: Option<(ID3D11Texture2D, ID3D11Texture2D)>,
    /// Off-thread display-descriptor sampler (see [`DescriptorPoller`]) — the capture loop reads
    /// its snapshot instead of running CCD queries inline on the frame path.
    desc_poller: DescriptorPoller,
    /// Sequence of the last poller sample the capture loop consumed (0 = none yet).
    desc_seq: u64,
    /// Two-strikes debounce for descriptor changes: the first differing sample arms this; only a
    /// SECOND consecutive sample with the same new descriptor triggers the recreate, so a
    /// single-sample transient (a topology re-probe blip) never tears the ring down.
    pending_desc: Option<DisplayDescriptor>,
    /// Set when a display-descriptor change triggered a ring recreate (recovery, game-capture bug GB1);
    /// cleared when a fresh frame resumes. If it stays set past the recovery window, `try_consume` drops
    /// the session (recover-or-drop, no DDA).
    recovering_since: Option<Instant>,
    /// When the last FRESH driver frame was consumed — feeds the driver-death watch in
    /// [`Self::try_consume`] (a dead WUDFHost is otherwise indistinguishable from an idle desktop:
    /// both stop publishing, and the encode loop would repeat the last frame forever).
    last_fresh: Instant,
    /// Rate-limits the WUDFHost liveness probe (one 0 ms wait per second, and only while stale).
    last_liveness: Instant,
    /// Rate-limits the mid-session [`kick_dwm_compose`] nudge (recovery window only).
    last_kick: Instant,
    /// Capture-stall watch (see [`StallWatch`]): flags multi-hundred-ms DWM composition holes
    /// during active flow and warns when they turn metronomic — the sole-virtual-display
    /// periodic-stutter diagnostic.
    stall_watch: StallWatch,
    /// v2 driver-telemetry trackers feeding [`stall::StallEvidence`]: the header's
    /// `offered_total` as of the last fresh frame, and the stalest the driver's drain heartbeat
    /// ever read (µs) while the host starved since then. Rolled at every fresh frame.
    offered_at_fresh: u64,
    max_hb_age_us: u64,
    /// The damage witness feeding [`stall::StallEvidence::cursor_moved_px`]: the OS cursor's
    /// last sampled position (`None` until `GetCursorPos` first succeeds), the px accumulated
    /// since the last fresh frame, and one pending sample's delta held back a call so the
    /// stall-ending frame's own cursor move never counts into the gap it ended (the fold
    /// happens at the top of the NEXT `try_consume`, by which point this call demonstrably
    /// did not consume a fresh frame — a fresh frame resets both). Sampled at most every
    /// [`Self::CURSOR_WITNESS_INTERVAL`]; user32 reads, never the display-config lock.
    cursor_last: Option<(i32, i32)>,
    cursor_gap_px: u32,
    cursor_pending_px: u32,
    cursor_sampled_at: Instant,
    /// The Phase A.2 micro-probe engine (refcounted process singleton) — its window read rides
    /// every stall report so the verdict matrix can name the disturbance class. `None` when
    /// `PUNKTFUNK_STALL_PROBES=0` opted the box out (the engine costs standing threads); the
    /// verdict matrix already treats an absent probe window as never-stalled, so reports just
    /// lose the corroborating legs, never invent them.
    probes: Option<Arc<probes::ProbeEngine>>,
    /// The Phase A.3 DxgKrnl ETW watch; `None` when the session can't start (non-admin dev run)
    /// — reports then say `etw=unavailable`.
    etw: Option<Arc<dxgkrnl_etw::EtwWatch>>,
    /// Host-owned ROTATING output ring NVENC encodes (one YUV texture per slot). Rotating it per frame
    /// is the precondition for pipelining the encode loop: while NVENC encodes frame N's texture on the
    /// ASIC, frame N+1's convert writes a DIFFERENT texture — the two overlap. Format = `out_format()`:
    /// NV12 (SDR, BT.709 limited) or P010 (HDR, BT.2020 PQ limited), so NVENC takes native YUV and skips
    /// its internal RGB→YUV CSC on the SM/3D engine the game saturates (plan §5.A). Rebuilt on a
    /// display-mode flip. Built lazily.
    out_ring: Vec<OutSlot>,
    out_idx: usize,
    /// BGRA slot → NV12 (BT.709 limited) on the dedicated D3D11 VIDEO engine, used while the display is
    /// SDR — keeps the colour-convert OFF the contended 3D/compute engine. Built lazily; rebuilt on a
    /// size/HDR flip.
    video_conv: Option<VideoConverter>,
    /// FP16 scRGB slot → P010 (BT.2020 PQ limited) via two shader passes, used while the display is
    /// HDR and the session did NOT negotiate 4:4:4 (that case takes [`Self::hdr_rgb10_conv`])
    /// (NVIDIA's VideoProcessor can't do RGB→P010). The passes run on the 3D engine, but it still skips
    /// NVENC's internal SM-side CSC. Built lazily.
    hdr_p010_conv: Option<HdrP010Converter>,
    /// FP16 scRGB slot → packed 10-bit BT.2020 PQ RGB, used while the display is HDR **and** the
    /// session negotiated 4:4:4 — the full-chroma twin of [`Self::hdr_p010_conv`]. Rebuilt with the
    /// ring on a mode/HDR flip.
    hdr_rgb10_conv: Option<HdrRgb10Converter>,
    /// BGRA slot → packed 10-bit RGB by plain 8→10 expansion (`HdrRgb10Converter::new_sdr_expand`
    /// — same sRGB values at UNORM precision), used on a 10-bit **SDR** session
    /// ([`Self::ten_bit_sdr`], both 4:2:0 and 4:4:4 — NVENC does the CSC/subsampling either
    /// way). Built lazily like its HDR twin above.
    sdr_rgb10_conv: Option<HdrRgb10Converter>,
    last_seq: u64,
    last_present: Option<(ID3D11Texture2D, PixelFormat)>,
    status_logged: bool,
    /// Session-lifetime `PowerRequestDisplayRequired` (RAII, `powercfg /requests`-visible): keeps
    /// the console out of display-off while this capturer lives — DWM composes nothing (for ANY
    /// display) once the console's displays power down, so without this a lid-closed/idle box can
    /// go dark mid-stream and the ring runs dry. Prevention only; waking an ALREADY-off display is
    /// the HID compose kick's job ([`crate::HID_COMPOSE_KICK`]). `None` when the kernel refused
    /// (best-effort, the pre-existing behavior).
    _display_wake: Option<pf_frame::session_tuning::DisplayWakeRequest>,
    _keepalive: Box<dyn Send>,
}
// SAFETY: `IddPushCapturer` is `!Send` only because of its `*mut SharedHeader` raw pointer (and the
// COM interfaces; the frame/cursor delivery closures own `Arc` clones of the control device and are
// `Send + Sync` on their own). It is
// created, used, and dropped by a SINGLE thread — the owning capture/encode thread — never shared: the
// `ID3D11DeviceContext` is the device's IMMEDIATE context (single-threaded by D3D11 contract) and is
// only ever touched from that thread, and the header pointer (into the mapping this struct owns) is
// only dereferenced there. `Send` transfers ownership to one thread at a time with NO concurrent
// access; we do not (and must not) claim `Sync`.
unsafe impl Send for IddPushCapturer {}

impl IddPushCapturer {
    /// Failed depth-pin attempts before [`Self::poll_display_hdr`] backs its re-pin off
    /// (≈2 s of eager retries at the descriptor poller's 4 Hz).
    const HDR_PIN_EAGER: u32 = 8;
    /// While backed off, re-attempt the depth pin on every this-many-th descriptor sample
    /// (~4 s at 4 Hz) — self-healing without the 4 Hz CCD-write drumbeat.
    const HDR_PIN_RETRY_EVERY: u64 = 16;

    #[inline]
    fn latest(&self) -> u64 {
        // SAFETY: `self.header` is the live, owned shared-header mapping (page-aligned, sized for a
        // `SharedHeader`). `addr_of!((*self.header).latest)` forms the address of the `latest` field
        // WITHOUT a reference; it is an 8-aligned `u64` (so valid for `AtomicU64`), and the `Acquire` load
        // is the consumer half of the cross-process publish handshake (pairs with the driver's `Release`).
        unsafe {
            (*(std::ptr::addr_of!((*self.header).latest) as *const AtomicU64))
                .load(Ordering::Acquire)
        }
    }

    /// Age of a driver-stamped QPC value in microseconds (QPC is system-wide, so cross-process
    /// comparison is sound); 0 when the stamp reads ahead of us (a benign race with the writer).
    fn qpc_age_us(stamp: u64) -> u64 {
        static FREQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let freq = *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: plain FFI; `f` is a valid local out-param. The frequency is fixed at boot and
            // cannot fail on any OS we run on; 0 would only mean the call failed — guarded below.
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
        // SAFETY: like `latest` — the header stays mapped for the capturer's lifetime, both fields
        // are 8-aligned `u64`s within the v2 layout the host itself created, and Relaxed suffices
        // for best-effort diagnostics (the same contract the driver writes them under).
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

    /// Log the driver's status once it first reports (the only driver-visibility channel we have).
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

    /// The output texture format + the [`PixelFormat`] NVENC encodes, driven by the DISPLAY's HDR
    /// state plus the session's 4:4:4 negotiation: HDR → `P010` (BT.2020 PQ
    /// 10-bit limited) → NVENC Main10, and the client auto-detects PQ from the HEVC VUI; SDR →
    /// `Nv12` (BT.709 8-bit limited), or full-chroma `Bgra` passthrough on a 4:4:4 session (NVENC
    /// CSCs RGB→YUV444 itself, following the BT.709 VUI — the one path that deliberately pays the
    /// SM-side CSC, because the video processor can only produce subsampled output). The
    /// composition depth DOES follow the session's negotiated `want_hdr` — pinned at open
    /// (`open.rs`, the `!want_hdr` force-off and the 10-bit enable) and re-pinned every sample
    /// by [`Self::poll_display_hdr`], because a PQ stream sent to a client that advertised SDR-only
    /// lands on an SDR desktop and blows out. (The older note here claimed the opposite — that the
    /// advertised `VIDEO_CAP_10BIT` was ignored because clients under-report it. That reasoning
    /// survives only in the CODEC choice: an HDR-negotiated H.26x session still follows a host
    /// "Use HDR" flip in either direction.) HDR and 4:4:4 now COMPOSE: an HDR display that
    /// negotiated full chroma emits packed 10-bit BT.2020 PQ RGB (`Rgb10a2`) for NVENC to CSC to
    /// YUV 4:4:4 — HEVC Main 4:4:4 10. (Before, HDR won and the stream silently downgraded to
    /// 4:2:0 *after* the Welcome had already promised 4:4:4.)
    fn out_format(&self) -> (DXGI_FORMAT, PixelFormat) {
        // PyroWave never uses this out-ring (it has its own separate-plane `pyro_ring`); the
        // format here only labels the frame. SDR sessions label NV12 (BT.709 limited), HDR
        // (negotiated 10-bit) sessions P010 — matching the studio-code planes the pyro CSC writes.
        if self.pyrowave {
            return if self.display_hdr {
                (DXGI_FORMAT_P010, PixelFormat::P010)
            } else {
                (DXGI_FORMAT_NV12, PixelFormat::Nv12)
            };
        }
        if self.display_hdr {
            if self.want_444 {
                // HDR + full chroma: packed 10-bit RGB (BT.2020 PQ), which NVENC CSCs to YUV
                // 4:4:4 itself — the HDR twin of the SDR BGRA passthrough below. No subsampling
                // anywhere on this path (see `HdrRgb10Converter`).
                return (DXGI_FORMAT_R10G10B10A2_UNORM, PixelFormat::Rgb10a2);
            }
            (DXGI_FORMAT_P010, PixelFormat::P010)
        } else if self.ten_bit_sdr {
            // 10-bit SDR (either chroma): the BGRA slot expanded 8→10 into packed RGB. The
            // format is the SDR twin of `Rgb10a2` — NVENC ingests it as ABGR10 and encodes
            // Main10 under the BT.709 VUI its CSC follows (the SDR 4:4:4 precedent below).
            (DXGI_FORMAT_R10G10B10A2_UNORM, PixelFormat::Rgb10a2Sdr)
        } else if self.want_444 {
            (DXGI_FORMAT_B8G8R8A8_UNORM, PixelFormat::Bgra)
        } else {
            (DXGI_FORMAT_NV12, PixelFormat::Nv12)
        }
    }

    /// The ring (shared-texture) format for a given HDR state: FP16 when the display composes in
    /// advanced colour, BGRA when SDR. Taken by value so [`Self::recreate_ring`] can size the new
    /// slots for the INCOMING state before committing it (see there).
    fn ring_format_for(hdr: bool) -> DXGI_FORMAT {
        if hdr {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }

    /// The ring format for the display's CURRENT HDR state.
    fn ring_format(&self) -> DXGI_FORMAT {
        Self::ring_format_for(self.display_hdr)
    }

    /// The driver's four best-effort diagnostic words from the shared header:
    /// `(driver_status, detail, render_luid_low, render_luid_high)`.
    ///
    /// SAFETY (once, for all callers): `self.header` points into the live, owned shared-header
    /// mapping (page-aligned, sized `>= size_of::<SharedHeader>()`), so each field read is in-bounds
    /// and aligned, and no reference into the shared region is formed. The driver writes these
    /// cross-process, but an aligned word read cannot tear and these are best-effort DIAGNOSTICS —
    /// the real handshake is the atomic `magic`/`latest`.
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

    /// Recreate the ring at the format for `new_display_hdr` (the user flipped "Use HDR"). Bumps the
    /// generation so the driver re-attaches ([`is_stale`]) to the new-format textures and DELIVERS the
    /// new channel (fresh duplicates of the header + event + the new textures — every delivery is a
    /// self-contained handle set the driver owns); clears the header's `latest` so we don't consume a
    /// stale slot from the old ring; drops the conversion textures so they rebuild at the new format.
    fn recreate_ring(&mut self, new_display_hdr: bool, new_w: u32, new_h: u32) -> Result<()> {
        // BUILD FIRST, COMMIT AFTER. `create_ring_slots` is fallible — VRAM pressure at a large new
        // mode, which is exactly when resizes happen — and the geometry used to be committed BEFORE
        // it ran. A failed recreate then left the capturer emitting frames stamped with the NEW
        // width/height/format against the OLD ring, the old generation and an unchanged header:
        // nothing downstream could detect the mismatch, because every field it would compare against
        // had already been moved. Nothing below this line fails.
        let fmt = Self::ring_format_for(new_display_hdr);
        let new_slots = Self::create_ring_slots(&self.device, new_w, new_h, fmt)?;
        self.display_hdr = new_display_hdr;
        self.width = new_w;
        self.height = new_h;
        let new_gen = next_generation();
        // SAFETY: `self.header` is the live, owned shared-header mapping (page-aligned, sized for a
        // `SharedHeader`). The `latest`/`generation` stores go through `addr_of!`-formed field pointers (no
        // references) of correctly-aligned `u64`/`u32` fields, valid for `AtomicU64`/`AtomicU32`; the
        // `dxgi_format`/`width`/`height` writes are in-bounds raw writes through the pointer (no `&mut`).
        // The `Release` fence + the `Release` `generation` store publish all preceding writes so the driver
        // only re-attaches (`Acquire`) once the new textures + format are in place.
        unsafe {
            // Clear `latest` to the 0 sentinel (generation 0, which try_consume rejects). The real guard
            // against consuming an unwritten new-ring slot is the generation tag in `latest`: a stale
            // old-ring publish racing this recreate carries the OLD generation and is rejected. We wait
            // for the driver's first NEW-generation publish.
            (*(std::ptr::addr_of!((*self.header).latest) as *const AtomicU64))
                .store(0, Ordering::Relaxed);
            (*self.header).dxgi_format = fmt.0 as u32;
            (*self.header).width = new_w;
            (*self.header).height = new_h;
            // Clear the driver's diagnostic words too. `wait_for_attach` — the ONLY classifier of
            // TEX_FAIL/BIND_FAIL and the only source of the render-LUID rebind — runs at open only,
            // so a recreate's re-attach is never classified; a stale `DRV_STATUS_OPENED` left over
            // from the previous generation then made a FAILED re-attach look healthy, and the
            // recover-or-drop bail below reported nothing at all while the driver's real diagnosis
            // sat unread in the header. Cleared, these fields describe THIS generation's attach,
            // which is what that bail now prints.
            (*self.header).driver_status = DRV_STATUS_NONE;
            (*self.header).driver_status_detail = 0;
            // Publish the new generation LAST (Release): when the driver observes it (Acquire) the new
            // textures already exist and the format is already updated.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
                .store(new_gen, Ordering::Release);
        }
        // …and let `log_driver_status_once` report the new generation's attach (or its failure).
        self.status_logged = false;
        self.slots = new_slots; // drops the old slots → closes their shared handles + SRVs
        self.generation = new_gen;
        // Deliver the new generation's channel. The driver's old publisher sees the generation bump
        // (`is_stale`), drops (closing its old handles), and re-attaches from this delivery. On failure
        // the broker already reaped its remote duplicates; the recover-or-drop window in `try_consume`
        // then ends the session cleanly (the driver can never attach to an undelivered ring).
        // SAFETY: `broker.send` requires live `header`/`event` handles of this process — both borrow the
        // owned `self.section.handle`/`self.event` for the duration of the synchronous call.
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
        // Ring recreates ride display churn that can also have re-arrived the MONITOR driver-side
        // (destroying its cursor worker with it) — re-deliver the surviving cursor section so the
        // hardware-cursor declaration follows the CURRENT monitor generation.
        if let (Some(cs), Some(send)) = (self.cursor_shared.as_ref(), self.cursor_sender.as_ref()) {
            let _ = deliver_cursor_channel(&self.broker, self.target_id, cs, send);
        }
        self.blend_scratch = None; // ring geometry/format changed — rebuild at next blend
                                   // The HDR SDR-white reference belongs to the mode we just moved to. Queried HERE, not from
                                   // the blend, which holds the ring slot's keyed mutex (see `refresh_sdr_white_scale`).
        self.refresh_sdr_white_scale();
        self.last_slot = None; // old-ring slot indices are meaningless now
        self.last_seq = 0;
        self.out_ring.clear(); // the output format changed → rebuild lazily at the new format
        self.video_conv = None; // converters are sized + HDR-specific → rebuild at the new mode
        self.hdr_p010_conv = None;
        self.hdr_rgb10_conv = None;
        self.sdr_rgb10_conv = None;
        // The PyroWave CSC is mode-baked too (BgraToYuvPlanes picks different SDR vs HDR shaders
        // and R8/R8G8 vs R16/R16G16 outputs). Without this, a display_hdr flip (Downgrade point D:
        // want_hdr=true but HDR couldn't enable at open) reused the stale SDR converter against
        // the freshly HDR-formatted pyro ring — every frame corrupted. `ensure_pyro_conv` only
        // builds when None, so it must be reset here like its siblings.
        self.pyro_conv = None;
        self.pyro_ring.clear(); // PyroWave two-plane ring is sized → rebuild at the new mode
        self.pyro_last = None;
        self.out_idx = 0;
        self.last_present = None;
        Ok(())
    }

    /// Follow the [`DescriptorPoller`]'s snapshot of the display's live HDR state + resolution;
    /// recreate the ring when the display REALLY changed (a "Use HDR" flip, or a fullscreen game
    /// mode-setting the virtual display out from under the negotiated size — game-capture bug
    /// GB1). Called from the capture loop (incl. while frozen on a format mismatch); cheap — one
    /// mutex read, the CCD queries run off-thread. Two-strikes debounce: a change is acted on
    /// only when TWO consecutive samples agree on the same new descriptor (~½ s), so a
    /// single-sample transient during a topology re-probe never costs a ring recreate.
    fn poll_display_hdr(&mut self) {
        let (mut now, seq) = self.desc_poller.snapshot();
        if seq == self.desc_seq {
            return; // no new sample since last consume
        }
        self.desc_seq = seq;
        // A topology reassert is in flight (the exclusive watchdog announced it): every sample in
        // this window is potentially the TRANSIENT eviction state, and acting on one recreates
        // the ring at a mode the reassert's recovery chain (`recreate_ring_in_place`, keyed off
        // the reassert generation) is about to undo — the field hdr=true→false→true double
        // recreate. Consume the sample, disarm the debounce, act on nothing; a REAL change that
        // races the window survives it (the descriptor still differs once the hold clears, and
        // the poller re-samples in ~250 ms). This also keeps the negotiated-depth pin-back below
        // from issuing a CCD write mid-eviction, where it would fight the reassert itself.
        if pf_win_display::topology_churn::held() {
            self.pending_desc = None;
            return;
        }
        // Two cases re-assert the NEGOTIATED depth instead of following a mid-session "Use HDR"
        // flip — flip the display back and treat the descriptor as the negotiated state (so the ring
        // is never recreated at the wrong format):
        //   - a PyroWave session: its encoder was opened for fixed plane formats (R8 SDR / R16 HDR),
        //     so it can't follow a flip the way H.26x re-inits do;
        //   - ANY SDR-negotiated session (`!want_hdr`, either codec): a host-side flip to HDR
        //     must not promote the stream to P010 PQ behind a client that advertised SDR-only.
        // An HDR-negotiated H.26x session is NOT pinned — it still follows a host "Use HDR" flip in
        // either direction (its encoder re-inits on the depth change).
        if (self.pyrowave || !self.want_hdr) && now.hdr != self.want_hdr {
            let want = self.want_hdr;
            // A display that refuses the pin refuses it on every 250 ms sample — past
            // [`Self::HDR_PIN_EAGER`] consecutive failures the write+read-back re-fires only on
            // every [`Self::HDR_PIN_RETRY_EVERY`]th sample (~4 s), instead of 4 CCD writes +
            // 8 display-config queries per second forever, all on the session-global
            // display-config lock. Skipped samples keep the poller's own advanced-color read,
            // so nothing is fabricated and a display that becomes flippable is re-pinned on
            // the next retry sample.
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
            // No mismatch (or the session follows flips): the pin is healthy — a later refusal
            // starts its eager attempts fresh.
            self.hdr_pin_failures = 0;
        }
        let current = DisplayDescriptor {
            hdr: self.display_hdr,
            width: self.width,
            height: self.height,
        };
        if now == current {
            self.pending_desc = None; // steady (or a blip reverted before its second strike)
            return;
        }
        if self.pending_desc != Some(now) {
            self.pending_desc = Some(now); // first strike — arm, act on confirmation
            return;
        }
        self.pending_desc = None;
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{} hdr={}", self.width, self.height, self.display_hdr),
            to = format!("{}x{} hdr={}", now.width, now.height, now.hdr),
            "IDD push: display descriptor changed — recreating the ring at the new mode"
        );
        // Start the recovery clock (if not already running): if a fresh frame doesn't resume within the
        // window, try_consume drops the session rather than freeze.
        self.recovering_since.get_or_insert_with(Instant::now);
        if let Err(e) = self.recreate_ring(now.hdr, now.width, now.height) {
            tracing::warn!(error = %format!("{e:#}"), "IDD push: ring recreate failed");
        }
    }

    /// Build the host-owned output ring (`OUT_RING` textures at [`Self::out_format`] + RTVs) if not yet
    /// built. Rotated per frame so the in-flight encode of N and the convert/copy of N+1 touch different
    /// textures. Rebuilt (cleared) when the display-mode flip changes the output format.
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
            // RENDER_TARGET: the VIDEO processor (NV12) and the P010 shader passes both write here, and
            // NVENC registers it as encode input. (PyroWave uses its own
            // shareable two-plane `pyro_ring` instead, so this NVENC/AMF/QSV ring stays unshared.)
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..OUT_RING {
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: `CreateTexture2D` is called on `self.device` (the capturer's live D3D11 device);
            // `&desc` is a fully-initialized stack `D3D11_TEXTURE2D_DESC`, the data arg is `None` (no
            // initial data), and `Some(&mut t)` is a live out-parameter the call fills. `?` rejects a failed
            // HRESULT before `t` is unwrapped, and the created texture belongs to `self.device`.
            // `plane_rtv` is `unsafe` for the same COM reason and takes that same device plus the
            // texture we just made; a driver that rejects planar RTVs fails HERE, at ring build, which
            // is where such a failure belongs (it used to surface on the first frame).
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

    /// PyroWave: build the separate-plane output ring (`OUT_RING` × {full-res R8 Y, half-res R8G8
    /// CbCr}, both `SHARED | SHARED_NTHANDLE` + RTV) if not yet built. The wavelet encoder imports the
    /// two SEPARATE textures (a single planar NV12 import is unreliable on NVIDIA); the
    /// [`BgraToYuvPlanes`] CSC renders into their RTVs.
    fn ensure_pyro_ring(&mut self) -> Result<()> {
        if !self.pyro_ring.is_empty() {
            return Ok(());
        }
        let (w, h) = (self.width, self.height);
        // SAFETY: all D3D11 calls target `self.device`; every `&desc` is a fully-initialized stack
        // struct and every `Some(&mut _)` a live out-param; `?` rejects a failed HRESULT before use.
        // The created textures/RTVs belong to `self.device`.
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
            // Plane formats/geometry follow the negotiated session: 16-bit UNORM planes for an
            // HDR (10-bit) session (P010-style studio codes from the pyro HDR CSC), full-res
            // chroma for 4:4:4 (design/pyrowave-444-hdr.md Phase 3).
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

    /// PyroWave: build the (mode-aware) RGB→YUV-planes CSC if not yet built. The mode is
    /// session-fixed: SDR/BGRA vs HDR/scRGB input, half- vs full-res chroma — the composition
    /// is pinned to the negotiated depth (`poll_display_hdr`), so the converter never needs a
    /// mid-session mode swap.
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

    /// Build the per-mode YUV converter if not already built: a VIDEO-engine BGRA→NV12 processor on an
    /// SDR display, or the FP16→P010 shader on an HDR display. Both keep NVENC's RGB→YUV CSC off the SM.
    /// An SDR 4:4:4 session needs NO converter — the BGRA slot passes through (see `out_format`).
    fn ensure_converter(&mut self) -> Result<()> {
        if self.display_hdr && self.want_444 {
            // HDR + full chroma: one full-res pass to packed 10-bit BT.2020 PQ RGB; NVENC does
            // the RGB→YUV444 CSC (there is nothing to subsample, so no second pass).
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
            // 10-bit SDR (4:2:0 AND 4:4:4): one full-res 8→10 expansion pass to packed RGB;
            // NVENC does the RGB→YUV CSC + any subsampling under the BT.709 VUI.
            if self.sdr_rgb10_conv.is_none() {
                self.sdr_rgb10_conv = Some(HdrRgb10Converter::new_sdr_expand(&self.device)?);
            }
        } else if self.want_444 {
            // Full-chroma passthrough — no conversion resources to build.
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

    /// PyroWave: after this frame's GPU convert, `Signal` the shared fence and return the fence
    /// `(handle, value)` for the encoder — the persistent shared handle EVERY frame (the encoder
    /// imports it whenever it has no timeline yet, e.g. after a mode-switch rebuild) + the
    /// incrementing value. `None` for a non-PyroWave session. The fence + its shared handle are
    /// created lazily on the first call. `Flush` submits the queued convert + signal so the encoder's
    /// cross-API Vulkan timeline wait resolves promptly instead of blocking on a still-unsubmitted
    /// signal. The caller pairs the returned fence with the frame's CbCr texture into a
    /// [`PyroFrameShare`].
    ///
    /// # Safety
    /// Runs on the owning capture/encode thread that holds the immediate context; forms no lasting
    /// borrow of `self`'s COM objects.
    unsafe fn pyro_fence_signal(&mut self) -> Result<Option<(Option<isize>, u64)>> {
        // SAFETY: per the contract above this runs on the owning capture/encode thread, which holds
        // the immediate context. Every call is a `?`-checked COM method on `self`'s live device or
        // context (or on a `cast()` of one), and `CreateSharedHandle` yields a fresh NT handle whose
        // raw value is only STORED here — never dereferenced, and never closed on this path.
        unsafe {
            if !self.pyrowave {
                return Ok(None);
            }
            if self.pyro_fence.is_none() {
                let dev5: ID3D11Device5 = self
                    .device
                    .cast()
                    .context("ID3D11Device -> ID3D11Device5 (shared fence)")?;
                // windows-rs returns COM interfaces via an out-param (unlike the HANDLE-returning
                // CreateSharedHandle below).
                let mut fence_out: Option<ID3D11Fence> = None;
                dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence_out)
                    .context("CreateFence(D3D11_FENCE_FLAG_SHARED)")?;
                let fence = fence_out.context("null D3D11 fence")?;
                // GENERIC_ALL (0x1000_0000) — the access the pyrowave interop test hands the handle.
                let handle: HANDLE = fence
                    .CreateSharedHandle(None, 0x1000_0000, PCWSTR::null())
                    .context("ID3D11Fence::CreateSharedHandle")?;
                self.pyro_fence = Some(fence);
                // `handle` is the shared NT handle `CreateSharedHandle` just minted for this fence —
                // a unique, still-open handle owned by this process — so `OwnedHandle` becomes its
                // sole owner and closes it exactly once, when the capturer drops. (Inside this fn's
                // `unsafe` scope; see its SAFETY block.)
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
            // Submit the queued convert + signal so the encoder's Vulkan timeline wait can resolve.
            self.context.Flush();
            // Pass the persistent shared handle EVERY frame (not once): the encoder can be rebuilt on a
            // client mode-switch, and a rebuilt encoder needs to re-import the fence into its fresh Vulkan
            // device. The encoder imports only when it has no timeline yet (and DUPLICATES the handle so
            // this original stays valid for the next rebuild).
            Ok(Some((
                self.pyro_fence_handle
                    .as_ref()
                    .map(|h| h.as_raw_handle() as isize),
                value,
            )))
        }
    }

    /// THE live cursor overlay for this session — the single source every consumer reads
    /// ([`Capturer::cursor`], [`Self::cursor_blend_key`], [`Self::prepare_blend_scratch`]).
    ///
    /// A LIVE poller wins, even while it still reports `None` (pre-first-shape); the driver's shm
    /// section serves only a poller that failed to start or died. The choice is LATCHED for the
    /// session because the two sources maintain INDEPENDENT serial namespaces: interleaving them
    /// mid-session hands the client two different shapes under one serial and poisons its shape
    /// cache. So once the shm has been used, the poller is not re-adopted.
    ///
    /// One function because the three consumers disagreed: `cursor()` degraded poller→shm correctly,
    /// but the BLEND path — `cursor_blend_key` and `prepare_blend_scratch`, the only consumer that
    /// matters in the composite model, since the Windows encode loop never attaches `frame.cursor` —
    /// read `cursor_poll` directly with no `alive()` check and no shm fallback. The documented
    /// fallback and the spawn-failure warning were both untrue for exactly those sessions: a dead
    /// poller meant pointer-less frames, not a degraded pointer.
    fn live_cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        if !self.cursor_shm_latched {
            if let Some(p) = &self.cursor_poll {
                if p.alive() {
                    return p.read();
                }
            }
            // The poller is gone (or never started) and we are about to read the shm — latch, so a
            // poller that somehow reports alive again cannot re-cross the serial namespaces.
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

    /// Refresh [`Self::sdr_white_scale`] — where DWM places SDR white on this HDR desktop, which the
    /// composited cursor must match or it renders visibly dark (~2.5× at the Windows default).
    ///
    /// Called at open and after every ring recreate, NEVER from the blend. `sdr_white_level_scale` is
    /// a CCD query that contends the display-config lock — the exact contention `DescriptorPoller`'s
    /// doc says must stay off the frame path — and it used to run inside `prepare_blend_scratch`,
    /// which holds the ring slot's KEYED MUTEX: the driver's publisher sat blocked on that slot for
    /// the duration of a display-config round-trip. "Only on scratch rebuilds" is not the same as
    /// harmless when the thing being blocked is the producer. A failed query keeps the prior value.
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

    /// The (serial, x, y, visible) of the CURRENT live cursor — the composite-regen change key.
    /// `None` while no source has a shape yet.
    fn cursor_blend_key(&mut self) -> Option<(u64, i32, i32, bool)> {
        self.live_cursor().map(|o| (o.serial, o.x, o.y, o.visible))
    }

    /// Composite the pointer for this convert: ensure the frame-sized blend scratch, copy the
    /// slot into it, and alpha-blend the GDI poller's shape at its polled position. Returns the
    /// scratch (texture + SRV) the conversion should read INSTEAD of the slot; `None` degrades
    /// to the pointer-less slot (scratch/pass creation failed — warned once). A hidden pointer
    /// blends nothing (the plain copy is the correct frame).
    ///
    /// # Safety
    /// D3D11 calls on the owning capture/encode thread's device + immediate context, called
    /// while holding the slot's keyed mutex (the copy reads the slot).
    unsafe fn prepare_blend_scratch(
        &mut self,
        slot_tex: &ID3D11Texture2D,
    ) -> Option<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        // SAFETY: per the contract above, D3D11 calls on the owning thread's device + immediate
        // context while the slot's keyed mutex is held. `CreateTexture2D`/`CreateShaderResourceView`
        // take a fully-initialized stack descriptor plus live out-params and are `.ok()`-checked before
        // use; `CopyResource` moves between our own scratch and the caller's live slot texture, which
        // share format and size by construction (the scratch is rebuilt whenever the ring geometry
        // changes).
        unsafe {
            let fmt = self.ring_format();
            // (Re)build the scratch at the current ring geometry.
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
                        // `sdr_white_scale` is NOT queried here — see `refresh_sdr_white_scale`:
                        // this runs while the ring slot's keyed mutex is held, and a CCD
                        // display-config round-trip has no business blocking the driver's publisher.
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
                        return None;
                    }
                }
            }
            let (tex, srv, ..) = self.blend_scratch.as_ref().expect("just ensured");
            let (tex, srv) = (tex.clone(), srv.clone());
            self.context.CopyResource(&tex, slot_tex);
            // Blend the pointer (visible shapes only; hidden = the copy alone is the frame).
            // Through `live_cursor`, so a dead poller degrades to the shm section HERE too — this
            // is the path that actually draws the pointer in the composite model, and the one that
            // used to read the poller unconditionally.
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
                    // FP16 ring = scRGB linear composition (HDR): linearize the sRGB shape and
                    // scale it to the target's SDR white so it matches the desktop around it.
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

    /// The secure-desktop guard (the 0.18.0 UAC/Winlogon regression). UAC consent and Winlogon
    /// live on the SECURE desktop, which the OS renders through the software-cursor path — its
    /// per-mode-commit default. With this session's IddCx hardware cursor declared (and
    /// re-declared by the driver on every swap-chain assign), that path never materialises, the
    /// secure desktop never presents into our swap-chain, and the stream freezes on the last
    /// normal-desktop frame for the entire UAC/lock interaction. On the poller's secure edge:
    /// stand the declare down (`SET_CURSOR_FORWARD` off — the driver stops its per-assign
    /// re-declare — plus the host facade's forced same-mode re-commit that actualises the
    /// software cursor); on dismissal, re-declare. Runs on the capture/encode thread every tick
    /// (it must keep running while frames are stalled — that is exactly the state it exits).
    fn poll_secure_desktop(&mut self) {
        let Some(fwd) = self.cursor_forward.as_ref() else {
            return;
        };
        // Sessions with a declare possibly in play: the channel session that declared it, and
        // the forced-composite session whose (reused) driver monitor may still run an earlier
        // session's cursor worker. A plain session on a clean target has no poller — no guard.
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
            // Re-declare only for the session that RUNS the cursor channel; a forced-composite
            // session never wanted the declare (leaving the driver's desired state off also
            // stops a reused worker's per-assign re-declares for good — the next channel
            // session's open-time reset re-arms it).
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

    /// Damage-witness sample cadence — coarse enough to cost nothing (two user32 reads at
    /// 125 Hz worst case), fine enough that a ≥150 ms hole gets many samples.
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
    // ponytail: no per-target rect filter (needs a cached CCD rect); add one if parallel-display
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

    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        self.log_driver_status_once();
        // The secure-desktop guard first: while UAC/Winlogon is up there may be NO fresh frames
        // at all — this edge is what brings them back.
        self.poll_secure_desktop();
        // The stall damage witness — before any early return, so gaps of every shape accumulate
        // cursor motion (or certify its absence).
        self.sample_cursor_witness();
        // Follow the display: a "Use HDR" flip recreates the ring at the matching format.
        self.poll_display_hdr();
        // Recover-or-drop (GB1): if a descriptor change triggered a recreate but no fresh frame has resumed
        // within the window, the IDD-push path can't follow the display (e.g. an exclusive-flip) — drop the
        // session cleanly (the loop's `?` ends it → the client reconnects) rather than freeze forever.
        if let Some(since) = self.recovering_since {
            if since.elapsed() > Duration::from_secs(3) {
                // Say WHY, from the driver's own evidence. `recreate_ring` cleared these fields, so
                // whatever they hold now is this generation's re-attach — the one nothing else
                // classifies (`wait_for_attach` runs at open only). Without it this bail was a bare
                // "could not recover", and the driver's real diagnosis (a TEX_FAIL render-adapter
                // mismatch, say, with the adapter it actually renders on) was sitting unread.
                let (st, detail, lo, hi) = self.driver_diag();
                bail!(
                    "IDD-push: display descriptor changed and the ring could not recover within 3s — \
                     dropping the session so the client reconnects. This generation's re-attach: \
                     driver_status={st} detail=0x{detail:08x} driver_render_luid={hi:08x}:{lo:08x} \
                     (0=never attached, 1=attached but published nothing, 2=could not open our \
                     textures — render-adapter mismatch, 4=refused the ring↔monitor binding)"
                );
            }
            // Same idle-desktop stall as the open-time attach gate: after a mid-session ring
            // recreate (HDR flip / mode change) an idle desktop composes nothing, so the fresh ring
            // never sees a frame and the 3 s recover-or-drop above kills a healthy session. A
            // stash-capable driver republishes its retained frame at the re-attach, so this kick
            // is the legacy-driver fallback here too. Nudge DWM (rate-limited) once the natural
            // post-recreate compose (and the stash republish) has had its chance. This is the ONE
            // call site on the live frame path: the kick may BLOCK this (capture/encode) thread
            // ~35 ms on its cursor-on-a-sibling-display branch (see `kick_dwm_compose`'s COST
            // note) — acceptable only because we are already ≥600 ms into a recovery window with
            // no frames arriving, and the 800 ms schedule below bounds the repeat rate.
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
        // Driver-death watch (the SDR path has no other signal): a dead WUDFHost stops publishing,
        // which at the ring is indistinguishable from an idle desktop — the encode loop would repeat
        // the last frame forever (frozen video + live audio) and `next_frame`'s 20 s bail is
        // unreachable once anything ever presented. While no fresh frame is arriving, probe the
        // broker's pinned process handle (rate-limited) and fail the capturer so the session's
        // rebuild path recreates output + ring against the restarted device.
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
        // Stall-attribution evidence (v2 telemetry): record the STALEST the driver's drain
        // heartbeat ever reads between fresh frames. A heartbeat that goes quiet for the hole
        // convicts our worker (starved/dead WUDFHost); one that stays fresh through it acquits the
        // driver and indicts the compose/present path. Two Relaxed loads + a QPC read per consume
        // tick; rolled at every fresh frame below.
        if let Some((hb, _)) = self.telemetry() {
            self.max_hb_age_us = self.max_hb_age_us.max(Self::qpc_age_us(hb));
        }
        let latest = self.latest();
        // `latest` is the proto publish token `(generation << 40) | (seq << 8) | slot`. Reject any publish
        // whose generation isn't our CURRENT ring (a stale old-ring publish racing a recreate, or the 0
        // sentinel we reset to) so we never consume an unwritten new-ring slot — eliminating the
        // toggle-time garbage frame.
        let tok = frame::FrameToken::unpack(latest);
        if tok.generation != self.generation {
            return Ok(None);
        }
        let seq = u64::from(tok.seq);
        let mut slot = tok.slot as usize;
        let fresh = seq != self.last_seq && slot < self.slots.len();
        let mut regen = false;
        if !fresh {
            // Composite cursor model: pointer-only motion produces NO new publish (the declared
            // hardware cursor never dirties the frame), so a static desktop would freeze the
            // blended pointer. Regenerate from the LAST slot whenever the polled cursor state
            // changed — the re-converted out-ring frame carries the pointer's new position.
            let moved = self.composite_cursor
                && self.last_slot.is_some()
                && self.cursor_blend_key() != self.last_blend_key;
            if !moved {
                return Ok(None);
            }
            slot = self.last_slot.expect("checked above");
            if slot >= self.slots.len() {
                return Ok(None); // ring shrank across a recreate — wait for a fresh publish
            }
            regen = true;
        }
        // Build the ring + converter BEFORE acquiring the slot so nothing between Acquire and Release
        // can `?`-return and leak the keyed-mutex lock (which would stall the driver on that slot).
        // PyroWave uses its OWN two-plane ring (`pyro_ring`); everything else the single NV12/BGRA ring.
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

        // Hold the slot's keyed mutex only across the convert/copy into the host out-ring (NOT across the
        // ~3 ms encode — NVENC reads the host out-ring slot, not the keyed-mutex slot), so the driver gets
        // the slot back immediately and the encode of the PREVIOUS frame overlaps this convert.
        // Clone the slot's COM interfaces (an AddRef each) so the guard borrows LOCALS, leaving
        // `self` free for the composite blend prep inside the lock.
        let (slot_tex, slot_srv, slot_mutex) = {
            let s = &self.slots[slot];
            (s.tex.clone(), s.srv.clone(), s.mutex.clone())
        };
        // Acquire the slot's keyed mutex via a RAII guard, scoped to JUST the convert/copy below so it
        // releases at the same point as the old hand-written `ReleaseSync` (the driver gets the slot back
        // immediately, NOT held across the rest of `try_consume`) — but now leak-proof on any early return.
        {
            let Some(_lock) = KeyedMutexGuard::acquire(&slot_mutex, 0, 8) else {
                return Ok(None);
            };
            // SAFETY: convert on the owning (encode) thread's immediate context, holding the slot lock.
            // A `?` here is leak-safe: `_lock` (the KeyedMutexGuard) drops on the early return, releasing
            // the slot back to the driver.
            unsafe {
                // Composite cursor model: divert the convert input through the blend scratch —
                // a slot copy with the pointer quad alpha-blended on top. `None` = compositing
                // off or degraded (the conversion then reads the slot as always).
                let blended = if self.composite_cursor {
                    self.prepare_blend_scratch(&slot_tex)
                } else {
                    None
                };
                if self.pyrowave {
                    // PyroWave: ring slot SRV (BGRA for SDR, scRGB FP16 for HDR) → the two separate
                    // plane textures via the mode-aware CSC; the shared fence signalled just after
                    // (`pyro_fence_signal`) orders the encoder's cross-device Vulkan read after this
                    // convert. The composition format is pinned to the negotiated depth.
                    let (_, y_rtv, _, cbcr_rtv) = pyro_slot.as_ref().expect("pyro slot");
                    if let Some(conv) = self.pyro_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        conv.convert(&self.context, src, y_rtv, cbcr_rtv, self.width, self.height)?;
                    }
                } else if self.display_hdr && self.want_444 {
                    // HDR 4:4:4: FP16 slot SRV → packed 10-bit BT.2020 PQ RGB; NVENC ingests it as
                    // ABGR10 and CSCs to YUV 4:4:4 under FREXT (HEVC Main 4:4:4 10).
                    if let Some(conv) = self.hdr_rgb10_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, _, rtv) = out.as_ref().expect("out ring");
                        let rtv = rtv.as_ref().expect("Rgb10a2 out slot has an RTV");
                        conv.convert(&self.context, src, rtv, self.width, self.height)?;
                    }
                } else if self.display_hdr {
                    // HDR: FP16 slot SRV → P010 (BT.2020 PQ) via the shader; NVENC takes native P010.
                    if let Some(conv) = self.hdr_p010_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, rtvs, _) = out.as_ref().expect("out ring");
                        // The slot's P010 plane views, built once in `ensure_out_ring`.
                        let (y_rtv, uv_rtv) = rtvs.as_ref().expect("P010 out slot has plane RTVs");
                        conv.convert(&self.context, src, y_rtv, uv_rtv, self.width, self.height)?;
                    }
                } else if self.ten_bit_sdr {
                    // 10-bit SDR: BGRA slot → packed 10-bit RGB, a plain 8→10 expansion (same
                    // sRGB values at UNORM precision). NVENC ingests it as ABGR10 and encodes
                    // Main10 under the BT.709 VUI — the CSC and any subsampling are NVENC's,
                    // exactly like the SDR 4:4:4 passthrough below, one bit-depth up.
                    if let Some(conv) = self.sdr_rgb10_conv.as_ref() {
                        let src = blended.as_ref().map(|(_, srv)| srv).unwrap_or(&slot_srv);
                        let (_, _, rtv) = out.as_ref().expect("out ring");
                        let rtv = rtv.as_ref().expect("Rgb10a2Sdr out slot has an RTV");
                        conv.convert(&self.context, src, rtv, self.width, self.height)?;
                    }
                } else if self.want_444 {
                    // SDR 4:4:4: pass the BGRA slot through untouched — NVENC ingests full-chroma
                    // RGB and CSCs to YUV 4:4:4 itself (per the always-written BT.709 VUI). Plain
                    // copy-engine move; the slot releases back to the driver immediately.
                    let src = blended.as_ref().map(|(t, _)| t).unwrap_or(&slot_tex);
                    self.context
                        .CopyResource(&out.as_ref().expect("out ring").0, src);
                } else {
                    // SDR: BGRA slot → NV12 on the VIDEO engine; NVENC takes native NV12, no SM-side CSC.
                    if let Some(conv) = self.video_conv.as_ref() {
                        let src = blended.as_ref().map(|(t, _)| t).unwrap_or(&slot_tex);
                        conv.convert(src, &out.as_ref().expect("out ring").0)?;
                    }
                }
            }
            // `_lock` drops here → `ReleaseSync(0)`.
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
            // A regen re-encodes OLD desktop content at a new pointer position — it is not a
            // fresh driver frame; feeding the freshness/stall bookkeeping would mask a dead
            // driver and pollute stall attribution.
        } else if self.recovering_since.take().is_some() {
            // A fresh frame resumed → recovered. The recovery gap is self-inflicted (ring
            // recreate, already logged by the recreate path) — reset the stall watch so it
            // doesn't read as a DWM stall.
            self.stall_watch.reset();
        } else if let Some(stall) = self.stall_watch.note_fresh(now) {
            // All of the reporting — the OS-event correlation, the two metronomic verdicts and
            // the running correlated/total tally — lives on `StallWatch` (sweep Phase 5.4). It was
            // ~65 lines of log prose inside `try_consume`, which is the hot loop, and its two
            // counters were capturer fields that nothing else touched.
            // One ETW read serves both evidence fields: the prose summary spans the gap plus
            // the same 300 ms lead-in the report's OS-event correlation uses (the disturbance
            // that CAUSED the hole lands just before it), while the discriminator counts span
            // the GAP ONLY — no lead-in: presents from the healthy flow right before the hole
            // would falsely acquit the content (the stall-ending frame's own present lands at
            // the window edge and stays well under the acquit bar). Both halves must come from
            // the same ring snapshot under the same clock anchor, or the prose and the verdict
            // can disagree about the same hole.
            let (etw, etw_counts) = self
                .etw
                .as_ref()
                .and_then(|w| {
                    now.checked_sub(stall.gap)
                        .map(|from| w.window_report(from, now, Duration::from_millis(300)))
                })
                .unzip();
            let evidence = StallEvidence {
                // A publisher re-attach restarts `offered_total` near zero; a ring recreate resets
                // the stall watch before that can matter, but guard the delta anyway (a restarted
                // counter reads as "frames offered since the restart", never as a u64 underflow).
                offered_delta: self.telemetry().map(|(_, offered)| {
                    if offered >= self.offered_at_fresh {
                        offered - self.offered_at_fresh
                    } else {
                        offered
                    }
                }),
                max_heartbeat_age_ms: self.max_hb_age_us / 1_000,
                // The probe read spans the same window the report's OS-event correlation uses
                // (the gap plus a lead-in for the disturbance that CAUSED it).
                probes: now
                    .checked_sub(stall.gap + Duration::from_millis(300))
                    .zip(self.probes.as_deref())
                    .map(|(from, p)| p.window(from, now)),
                etw,
                etw_counts,
                // The gap accumulator only — this call's own pending sample (the stall-ending
                // frame's move) is still unfolded and gets discarded by the reset below.
                cursor_moved_px: self.cursor_last.map(|_| self.cursor_gap_px),
            };
            self.stall_watch.report(&stall, now, &evidence);
        }
        if !regen {
            // A degraded stretch just ended (or was cut by a ring recreate): the one line that
            // makes a sustained ~2 fps phase log-visible — per-hole stall lines gate on prior
            // ACTIVE flow and go quiet exactly while the stretch runs.
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
            // A fresh driver frame: feed the driver-death watch and roll the stall-evidence
            // trackers (a regen re-encodes OLD content — it is not evidence of driver progress).
            self.last_fresh = now;
            if let Some((_, offered)) = self.telemetry() {
                self.offered_at_fresh = offered;
            }
            self.max_hb_age_us = 0;
            // Damage witness rolls with the other per-gap trackers; the pending sample is the
            // ending frame's own move — discarded, never folded (see `sample_cursor_witness`).
            self.cursor_gap_px = 0;
            self.cursor_pending_px = 0;
            // The provenance sequence: a NEW source image, never a regen (which re-encodes the
            // previous one) — the one clock recovery logic may trust for source progress.
            self.source_seq += 1;
        }
        // Build the frame. For PyroWave the encode input is the Y plane
        // (`texture`) + the CbCr plane & fence in `pyro`; signal the shared fence
        // after the convert above. SAFETY: on the owning capture/encode thread.
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
        // Copy the last presented frame into a FRESH rotated out-ring slot so a repeat (static desktop, no
        // new driver frame) never re-hands a slot that may still be encoding under pipeline_depth>1 — the
        // out-ring rotation IS the texture-ownership contract, and repeats must honor it too (audit §5.3).
        // OUT_RING(3) > the max pipeline_depth(2) guarantees the rotated slot is not in flight.
        let i = self.out_idx;
        // PyroWave: copy the last Y+CbCr into a fresh two-plane slot; texture = Y, CbCr + fence in `pyro`.
        if self.pyrowave {
            let (src_y, src_cbcr) = self.pyro_last.clone()?;
            let slot = self.pyro_ring.get(i)?;
            let (dst_y, dst_cbcr) = (slot.y.clone(), slot.cbcr.clone());
            // SAFETY: GPU copies on the owning thread's immediate context; src/dst are our own pyro-ring
            // plane textures of identical format/size.
            unsafe {
                self.context.CopyResource(&dst_y, &src_y);
                self.context.CopyResource(&dst_cbcr, &src_cbcr);
            }
            self.out_idx = (i + 1) % self.pyro_ring.len();
            self.pyro_last = Some((dst_y.clone(), dst_cbcr.clone()));
            // Fence the copies above so the encoder reads completed textures. SAFETY: owning thread.
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
        // SAFETY: GPU copy on the owning thread's immediate context; src/dst are our out-ring textures of
        // identical format/size (src is a previous out-ring slot; dst the next).
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

/// Duplicate `cs`'s section into the driver's WUDFHost and send `IOCTL_SET_CURSOR_CHANNEL`.
/// `true` = the driver adopted it (worker declared per its `cursor_forward_on` state). Shared by
/// the open-time delivery and every RE-delivery (ring recreate / flip NOT_FOUND) — the request is
/// idempotent driver-side (a replaced worker is stopped + joined).
fn deliver_cursor_channel(
    broker: &ChannelBroker,
    target_id: u32,
    cs: &cursor::CursorShared,
    send_cursor: &crate::CursorChannelSender,
) -> bool {
    // SAFETY: `cs.section_handle()` borrows the section mapping `cs` owns (live across this
    // synchronous call); the broker's WUDFHost process handle is live for the broker's lifetime.
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
        // The composite (capture) model is implemented HOST-side: the driver's hardware cursor
        // stays declared for the session's whole life — the only dependable state (there is NO
        // working un-declare; see cursor_blend.rs) — keeping every frame pointer-free, and the
        // capturer blends the GDI poller's shape into the frame itself. No driver round-trip.
        // `composite_forced` (a channel-less session on a sticky-declared target) is pinned ON:
        // with no client drawing, un-compositing would erase the pointer entirely.
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
            // SAFETY: `self.event` is the live frame-ready `OwnedHandle` this capturer owns; its raw value
            // (borrowed for the call, so it outlives this synchronous wait) is a valid auto-reset event
            // handle. `WaitForSingleObject` only reads the handle; the 16 ms timeout bounds the wait.
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
        // Frame-driven trigger (latency plan T1.1): wake the encode loop the moment the driver
        // publishes a frame we haven't consumed. The shared-header token is the truth (state,
        // not edge — the auto-reset event may have been consumed by an earlier wait); the event
        // is just the wakeup, waited in ≤16 ms slices exactly like `next_frame`'s bringup wait.
        loop {
            let tok = frame::FrameToken::unpack(self.latest());
            if tok.generation == self.generation && u64::from(tok.seq) != self.last_seq {
                return; // a fresh publish exists — `try_latest` will consume it
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            let ms = (left.as_millis() as u32).clamp(1, 16);
            // SAFETY: `self.event` is the live frame-ready `OwnedHandle` this capturer owns; its
            // raw value (borrowed for the call, so it outlives this synchronous wait) is a valid
            // auto-reset event handle. `WaitForSingleObject` only reads the handle; the bounded
            // timeout keeps the wait sliced.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), ms) };
        }
    }

    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        // While the display is HDR we emit BT.2020 PQ (Rgb10a2) → the encoder forces HEVC Main10 + the
        // PQ VUI; pair that with a mastering-display SEI so any decoder tone-maps from a real grade. The
        // driver doesn't (yet) forward the OS's IDDCX_HDR10_METADATA, so use the generic HDR10 baseline
        // (the same metadata the native HDR path sends on the 0xCE datagram).
        self.display_hdr.then(pf_frame::hdr::generic_hdr10)
    }

    fn pipeline_depth(&self) -> usize {
        // 2 = one frame deferred: submit N+1 (capture + convert/copy into a fresh out-ring texture) while
        // NVENC encodes N on the ASIC. We hand a rotating `OUT_RING` of output textures, so this is safe.
        // `PUNKTFUNK_IDD_DEPTH` overrides (1 disables pipelining). The ceiling is `OUT_RING - 1`, NOT
        // `OUT_RING`: `d` frames in flight need `d + 1` textures, because the rotation has to hand out a
        // slot that is not one of the `d` still being encoded. Clamping to `OUT_RING` admitted depth 3 on
        // a 3-slot ring, where `repeat_last`'s rotation lands back on the slot NVENC is reading and the
        // convert overwrites it in place — torn frames, silently, with no error anywhere.
        pf_host_config::config().idd_depth.clamp(1, OUT_RING - 1)
    }

    fn capture_target_id(&self) -> Option<u32> {
        Some(self.target_id)
    }

    fn resize_output(&mut self, width: u32, height: u32) -> bool {
        // Host-initiated resize (latency plan P2.3): the session's resize handler has already
        // committed the display's new mode (the manager's in-place mode set), so recreate the ring
        // at the new size NOW — no DescriptorPoller two-strike debounce (that stays, unchanged,
        // for EXTERNAL changes: HDR flips, game mode-sets). The driver re-attaches to the fresh
        // ring and republishes; on an in-place mode set the OS's mode-set full redraw gives the
        // stash/first frame within the recover window. Same recover-or-drop arming as the
        // poller-driven recreate, so a ring that can't re-attach still fails the session cleanly
        // instead of freezing.
        if (width, height) == (self.width, self.height) {
            return true; // already at the requested size (refresh-only change) — nothing to do
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
        // A channel session ending while the secure-desktop guard is engaged must not leave the
        // driver's per-target desired state off — the next session's channel delivery would
        // adopt UNdeclared and silently run the composite model (§8.6's cross-session trap).
        // The open-time reset also covers this (host-crash case); this is the orderly-teardown
        // belt.
        if self.secure_active && self.cursor_shared.is_some() {
            if let Some(fwd) = self.cursor_forward.as_ref() {
                let _ = fwd(true);
            }
        }
        self.slots.clear();
        // The shared header section (`MappedSection`), the frame-ready `event` (`OwnedHandle`) and the
        // broker's WUDFHost process handle free themselves via RAII (unmap view, then close handle) —
        // nothing of this session's channel outlives the capturer on the host side; the driver's
        // duplicates die with its publisher / monitor / WUDFHost (teardown invariant,
        // `design/idd-push-security.md`). _keepalive drops after, REMOVEing the virtual display.
    }
}

#[cfg(test)]
mod tests {
    use super::stall::Stall;
    use super::*;

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
            // The round trip `try_consume` actually performs.
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
        // The wrap really happened (we started 2 below the mask), and produced no duplicate 0.
        assert!(
            seen.contains(&frame::FrameToken::GENERATION_MASK),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|&g| g < 8),
            "the counter should have wrapped: {seen:?}"
        );
    }

    /// The 0 sentinel `recreate_ring` stores must be REJECTED by the generation compare, whatever
    /// the live generation is — that is what stops a consume from the unwritten new ring.
    #[test]
    fn the_cleared_latest_sentinel_never_matches_a_live_generation() {
        let cleared = frame::FrameToken::unpack(0);
        assert_eq!(cleared.generation, 0);
        assert_eq!(cleared.seq, 0);
        for g in [1u32, 2, 0x7F_FFFF, frame::FrameToken::GENERATION_MASK] {
            assert_ne!(cleared.generation, g, "sentinel matched generation {g}");
        }
    }

    /// Feed a [`StallWatch`] fresh frames at the given offsets (ms from a common origin) and
    /// return what each `note_fresh` produced, paired with the metronome's read for the stalls
    /// (fed as non-damage-idle, the way `report` feeds every display-evidence stall).
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

    /// 60 fps flow (16 ms cadence) for `frames` frames starting at `start_ms`, appended to `out`.
    fn flow(out: &mut Vec<u64>, start_ms: u64, frames: u64) {
        out.extend((0..frames).map(|i| start_ms + i * 16));
    }

    #[test]
    fn stall_detected_after_active_flow() {
        // 20 frames of 60 fps flow, then a 300 ms hole — the resuming frame reads as a stall.
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
        // Caret-blink damage: frames ~530 ms apart — the activity gate never opens, so neither
        // the blink gaps nor a long idle hole count.
        let t: Vec<u64> = (0..12).map(|i| i * 530).chain([20_000]).collect();
        assert!(watch_run(&t).iter().all(Option::is_none));
    }

    #[test]
    fn thirty_fps_content_still_qualifies_as_active() {
        // A 30 fps-capped game (33 ms cadence): 8 pre-gap frames span 231 ms ≤ ACTIVE_SPAN, so a
        // 200 ms hole still reads as a stall.
        let mut t: Vec<u64> = (0..10).map(|i| i * 33).collect(); // last at 297 ms
        t.push(497);
        let out = watch_run(&t);
        assert!(out[10].is_some(), "30 fps flow must pass the activity gate");
    }

    /// Feed a [`StallWatch`] frames at the given offsets and return the first degraded-stretch
    /// summary it parks (checking after every frame, the way the capture loop does).
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
        // Active flow, then a ~2 fps phase (10 holes of 500 ms — the sustained-degradation
        // shape whose holes the activity gate keeps out of the per-stall log), then recovery
        // flow. One summary, carrying the whole stretch.
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
        // One hole inside otherwise-healthy flow: its own stall line covers it — a
        // one-hole "stretch" dissolving silently is what keeps the summary meaningful.
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
        // A ring recreate resets the flow history mid-stretch; the holes accumulated BEFORE it
        // are real evidence and must still surface.
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
        // Two degraded holes, then the content plainly STOPS (a 20 s pause — quit to desktop).
        // The summary covers the stretch only; the legitimate pause never joins the tally.
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
        // The field signature: ~300 ms DWM holes every 4 s inside 60 fps flow. Stalls land at the
        // cycle BOUNDARIES (5 cycles → 4 stalls); the 4th completes the metronome streak and
        // reports the ~4 s period.
        let mut t = Vec::new();
        for cycle in 0..5u64 {
            // ~3.7 s of flow, then the hole to the next cycle start.
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

    /// A damage-idle hole must not advance the metronome: the same four evenly-spaced stalls
    /// as [`metronomic_stalls_self_diagnose`], but with one classified damage-idle — the beat
    /// never completes, because a hand/input pause is not display-disturbance evidence.
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
                    // The 2nd stall reads damage-idle (cursor sat still on a dwm-only desktop).
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
        // Active flow, then a ring recreate (reset), then flow resumes 800 ms later — the resume
        // frame must NOT read as a stall, and detection re-arms afterwards.
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

    /// The repeated-stall (non-metronomic) WARN's window arithmetic: fires at the third reported
    /// stall inside 60 s, stays quiet through the 300 s re-warn spacing, and re-arms on a fresh
    /// burst after old entries age out.
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
        // A fresh burst well past the spacing: the old entries have aged out of the window,
        // so it takes a full RATE_MIN_STALLS again — and then warns again.
        assert_eq!(w.note_for_rate_warn(at(400)), None);
        assert_eq!(w.note_for_rate_warn(at(401)), None);
        assert_eq!(
            w.note_for_rate_warn(at(402)),
            Some(3),
            "re-warns after the spacing"
        );
    }

    /// [`stall::attribute`]'s verdict table — the Branch-1/Branch-2 fork, per evidence shape.
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
        // Pre-telemetry driver: no verdict, whatever the heartbeat tracker read.
        assert_eq!(verdict(300, None, 500), StallVerdict::NoTelemetry);
        // Heartbeat silent for most of the hole → the worker starved, wherever the frames were.
        assert_eq!(verdict(600, Some(0), 400), StallVerdict::WorkerStalled);
        assert_eq!(verdict(600, Some(50), 300), StallVerdict::WorkerStalled);
        // A scheduled worker heartbeats every ≤16 ms — 200 ms of silence on a 300 ms gap is under
        // the max(gap/2, 250 ms) bar, so the verdict falls through to the offered-frames fork.
        assert_eq!(verdict(300, Some(1), 200), StallVerdict::ComposeSilence);
        // The stall-ending frame (+ a small resume burst) does not acquit DWM...
        assert_eq!(verdict(300, Some(3), 20), StallVerdict::ComposeSilence);
        // ...but sustained composition through the hole does: the frames existed, WE lost them.
        assert_eq!(verdict(300, Some(8), 20), StallVerdict::DeliveryLeg);
        assert_eq!(verdict(2_000, Some(120), 30), StallVerdict::DeliveryLeg);
        // Long holes scale the worker-stalled bar: 900 ms of silence on a 3 s gap is not half.
        assert_eq!(verdict(3_000, Some(2), 900), StallVerdict::ComposeSilence);
        assert_eq!(verdict(3_000, Some(2), 1_600), StallVerdict::WorkerStalled);
    }

    /// [`stall::classify`]'s verdict matrix — how the micro-probe window and the ETW
    /// present-vs-queue counts refine (or decline to refine) the driver-telemetry verdict into
    /// a named disturbance class.
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
        // Every case below predates the damage witness — `cursor_moved_px = None` keeps the
        // pre-witness classification, which is exactly what these cases assert. The witness's
        // own matrix (the damage-idle split) is `damage_idle_split` below. A nested fn (not a
        // closure): each call site's temporaries need their own lifetime.
        fn classify(
            gap: Duration,
            verdict: &StallVerdict,
            p: Option<&ProbeWindow>,
            c: Option<&EtwWindowCounts>,
        ) -> StallClass {
            super::stall::classify(gap, verdict, p, c, None)
        }
        // The driver's own verdicts win outright — probes can't overrule "we lost the frames".
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
        // No probes: compose-silence alone can't name a class.
        assert_eq!(
            classify(gap, &StallVerdict::ComposeSilence, None, None),
            StallClass::Unattributed
        );
        // Fences stalled ≥ gap/2 → the adapter froze — Class 1 (even without driver telemetry).
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
        // Fences fine (16 ms round-trips) but DWM's tick froze — Class 2; DwmFlush counts too.
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
        // Everything alive + the driver swears E_PENDING, but NO working present witness:
        // the silence cannot be pinned on either side — UNATTRIBUTED, never a guess. (The
        // pre-2026-07-30 default of FRAME-GENERATION here mislabeled benign content pauses.)
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                None
            ),
            StallClass::Unattributed
        );
        // A witness that exists but has never produced an event is NOT a working witness.
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
        // The present witness splits the silence. Presents flowing through the hole while the
        // virtual display's queue starved → the OS display path dropped composed frames: the
        // frame-generation leg, POSITIVELY convicted.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(54, 0))
            ),
            StallClass::FrameGeneration
        );
        // (Essentially) no presents anywhere across the hole — the stall-ending frame and a
        // caret blink stay under the bar → the CONTENT stopped presenting: benign for the
        // display path.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(2, 1))
            ),
            StallClass::ContentSilence
        );
        // A LIVE witness (history true = it demonstrably worked just before the hole) reading
        // an exact zero is the strongest content conviction — the zero is a measurement, not
        // an absence.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(16_000), Some(20_000), Some(30_000))),
                Some(&counts(0, 0))
            ),
            StallClass::ContentSilence
        );
        // The present witness does NOT overrule the driver's own verdicts or the harder
        // classes — it only refines compose-silence.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::ComposeSilence,
                Some(&probes(Some(400_000), Some(400_000), None)),
                Some(&counts(54, 0))
            ),
            StallClass::AdapterFreeze
        );
        // Healthy probes but a pre-telemetry driver: delivery-leg is equally possible — honest.
        assert_eq!(
            classify(
                gap,
                &StallVerdict::NoTelemetry,
                Some(&probes(Some(16_000), Some(20_000), None)),
                Some(&counts(54, 0))
            ),
            StallClass::Unattributed
        );
        // An absent probe (None) never reads as "stalled" — absence is stated, not guessed;
        // with the present witness working, the silence still splits correctly.
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

    /// The damage witness's split of CONTENT-SILENCE (the 2026-08-27 field reattribution): a
    /// present-free hole on a dwm-only desktop with a stationary cursor is an input/hand pause
    /// (DAMAGE-IDLE), a moving cursor keeps it CONTENT-SILENCE (damage existed, nothing
    /// composed — the display-stack conviction), and a game session (flow not dwm-only) is
    /// never demoted regardless of the cursor.
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
        // A game presented in the lookback: its hole is real evidence even with a still cursor.
        assert_eq!(run(false, Some(0)), StallClass::ContentSilence);
        // No witness (pre-witness build / GetCursorPos failing): pre-witness behavior.
        assert_eq!(run(true, None), StallClass::ContentSilence);
        // The witness never overrules a harder conviction: stalled fences stay ADAPTER-FREEZE.
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
