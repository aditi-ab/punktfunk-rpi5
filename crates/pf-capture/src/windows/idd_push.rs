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

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::dxgi::{
    make_device, BgraToYuvPlanes, D3d11Frame, HdrP010Converter, PyroFrameShare, VideoConverter,
    WinCaptureTarget,
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
    DuplicateHandle, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS,
    HANDLE, INVALID_HANDLE_VALUE, LUID, POINT, WAIT_OBJECT_0,
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
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM,
    DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
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

/// LAST-RESORT fallback: nudge DWM into composing THE TARGET virtual display. DWM presents a
/// display only when something DIRTIES it — an idle desktop never does, so a freshly-attached ring
/// (session open, or a mid-session ring recreate) can sit at E_PENDING with no first frame even
/// though everything is healthy.
///
/// The PRIMARY first-frame mechanism is the driver's `FrameStash` (frame_transport.rs): the driver
/// retains the last composed frame and republishes it into every freshly-attached ring, so with a
/// stash-capable driver the first frame lands milliseconds after the channel delivery and this kick
/// never fires. It remains for pre-stash drivers and for the empty-stash cold start (a monitor that
/// has NEVER composed — normally the activation compose covers that). Synthetic input is inherently
/// unreliable — blocked on the secure desktop, defeated by a fullscreen game's ClipCursor, and
/// user-visible in the sibling-display case — which is exactly why it was demoted to fallback.
///
/// pf-vdisplay implements no hardware-cursor plane, so a cursor move is composited into
/// the frame — a guaranteed real present onto the IDD swap-chain (empirically what
/// `punktfunk-probe --input-test` always relied on).
///
/// The cursor only dirties the display it is ON — proven on-glass in the Stage-W3 two-display
/// validation: display B's session-open kicks wiggled the cursor on display A and B never composed
/// a first frame. So the kick is per-TARGET: when the cursor already sits inside `target_id`'s
/// desktop region (always true single-display), two net-zero 1 px relative moves (the historical
/// behavior, pointer ends exactly where it started); when it sits on a SIBLING display, jump the
/// cursor to the target's center and straight back (`SetCursorPos` ×2 — each absolute move dirties
/// the cursor layer of the display it lands on, so the target composes at least one frame; the
/// round trip is sub-millisecond and throttled). Best-effort — injection can be unavailable on the
/// secure desktop, where a fresh compose just happened anyway.
///
/// **HID-first**: when the host has registered [`HID_COMPOSE_KICK`] (the resident pf-mouse virtual
/// HID pointer), the kick goes through it INSTEAD of the `SendInput` paths below. A report from a
/// HID device is real input to win32k — delivered regardless of this process's session or the
/// active desktop, it wakes a powered-off display subsystem (lid-closed laptop / display idle-off /
/// modern standby) and counts as user presence — every condition under which `SendInput` is
/// silently impotent (wrong session → wrong input queue; secure desktop → blocked; display off →
/// nothing composes at all). That set is exactly the lid-closed field-report state.
fn kick_dwm_compose(target_id: u32) {
    // Process-GLOBAL throttle (Stage W3): with N parallel capturers each nudging on its own
    // schedule, DWM needs only one dirty per composition window — and the nudge is synthetic INPUT
    // (global, user-visible pointer state), so it must not multiply with capturer count. 50 ms
    // covers every composition interval we ship (≥ 60 Hz) while staying far under the callers' own
    // 600–800 ms per-capturer schedules.
    static LAST_KICK: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST_KICK.lock().unwrap();
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(50)) {
            return;
        }
        *last = Some(now);
    }
    // Where is the cursor, and where does the target display live in desktop space?
    let mut pos = POINT::default();
    // SAFETY: plain FFI; `pos` is a valid out-param for this synchronous call.
    let have_pos = unsafe { GetCursorPos(&mut pos) }.is_ok();
    // SAFETY: `source_desktop_rect` only runs the CCD QueryDisplayConfig FFI over owned local
    // buffers; the `Copy` target id crosses by value.
    let rect = unsafe { pf_win_display::win_display::source_desktop_rect(target_id) };
    // HID-first (see the doc comment): the registered virtual-mouse kick works from any
    // session/desktop and wakes an off display. Both geometries come from CCD (global database),
    // NOT per-session GDI metrics, so the aim is right even from a non-console session. Fall
    // through to SendInput only when the hook isn't registered / the mouse isn't up.
    if let (Some(kick), Some(rect)) = (crate::HID_COMPOSE_KICK.get(), rect) {
        // SAFETY: `desktop_bounds` only runs the CCD QueryDisplayConfig FFI over owned local
        // buffers.
        let bounds = unsafe { pf_win_display::win_display::desktop_bounds() };
        if let Some(bounds) = bounds {
            if kick(rect, bounds) {
                return;
            }
        }
    }
    if let (true, Some((x, y, w, h))) = (have_pos, rect) {
        let inside = pos.x >= x && pos.x < x + w.max(1) && pos.y >= y && pos.y < y + h.max(1);
        if !inside {
            // The cursor is on a sibling display — a wiggle there dirties the WRONG display. Jump
            // to the target's center, DWELL one composition interval, then restore. The dwell is
            // load-bearing (proven on-glass, Stage W3): DWM computes dirty state from the CURRENT
            // cursor position at the next vsync tick, so a sub-tick jump-and-return is invisible
            // and the target never composes — 35 ms covers a 30 Hz tick with margin. The cursor
            // visibly leaves the sibling display for those ~2 frames; kicks only fire during THIS
            // display's session-open / recovery windows (throttled), so the blip is rare and brief.
            // SAFETY: plain FFI; coordinates are plain ints, and the second call restores the
            // observed original position.
            unsafe {
                let _ = SetCursorPos(x + w / 2, y + h / 2);
            }
            std::thread::sleep(Duration::from_millis(35));
            // SAFETY: as above.
            unsafe {
                let _ = SetCursorPos(pos.x, pos.y);
            }
            return;
        }
    }
    let mk = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: plain FFI; the input slice is valid, fully-initialized local data for this synchronous
    // call, and `cbsize` is the true element size.
    unsafe {
        let _ = SendInput(&[mk(1), mk(-1)], std::mem::size_of::<INPUT>() as i32);
    }
}

/// Confirm the process is a genuine system WUDFHost — `%SystemRoot%\System32\WUDFHost.exe` — before a
/// broker duplicates sensitive handles into it. The pid is driver-reported (the frame channel's
/// [`control::AddReply::wudf_pid`], or the gamepad bootstrap's `driver_pid`); a spoofed devnode / a
/// tampered mailbox could name an arbitrary process to receive the channel, so this is the
/// confused-deputy gate. Best-effort image-path identity is proportionate: a fully-compromised REAL
/// driver is already a channel endpoint, and any *other* process (attacker exe, a non-driver pid)
/// fails this WUDFHost image check. `what` names the channel in the error (e.g. `"frame-channel"`);
/// shared with the gamepad sealed channel (`inject/windows/gamepad_raii.rs`).
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
#[path = "idd_push/descriptor.rs"]
mod descriptor;
#[path = "idd_push/stall.rs"]
mod stall;
use channel::ChannelBroker;
use descriptor::{DescriptorPoller, DisplayDescriptor};
use stall::StallWatch;

pub struct IddPushCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    target_id: u32,
    /// Owns the shared-header file mapping + its mapped view (RAII unmap-then-close). Declared BEFORE
    /// `header`, which is a raw pointer borrowed into this view via [`MappedSection::ptr`]. Also the
    /// duplication source for the driver's header handle on every [`ChannelBroker::send`].
    section: MappedSection,
    header: *mut SharedHeader,
    event: OwnedHandle,
    /// The sealed channel's handle-duplication broker (WUDFHost process + control device); used at open
    /// and again on every ring recreate to deliver fresh duplicates.
    broker: ChannelBroker,
    width: u32,
    height: u32,
    slots: Vec<HostSlot>,
    /// The ring/texture generation, bumped every time the ring is recreated at a new format (the
    /// display's HDR mode flipped). Stamped into the header + each delivery so the driver re-attaches
    /// (and so stale-ring publishes are rejected).
    generation: u32,
    /// The CLIENT's advertised 10-bit capability (= negotiated `bit_depth >= 10`). Gates the
    /// composition depth: a 10-bit client PROACTIVELY enables advanced color at `open` (HDR without a
    /// manual toggle); an SDR-only client forces it OFF and the descriptor poller PINS it there, so a
    /// client that advertised SDR ("HDR off") is never handed the in-band PQ upgrade the pixel-format-
    /// driven encoder would otherwise stamp from an HDR composition. (An HDR-negotiated H.26x session
    /// still follows a host-side "Use HDR" flip; all clients decode Main10 + auto-detect PQ from the VUI.)
    client_10bit: bool,
    /// The DISPLAY's CURRENT HDR state (from `advanced_color_enabled`) — the user can flip "Use HDR" in
    /// Windows mid-session. Drives the ring format (HDR → FP16 surfaces, SDR → BGRA) and the conversion.
    /// Polled in the capture loop; a change recreates the ring (see [`Self::recreate_ring`]).
    display_hdr: bool,
    /// The session negotiated full-chroma 4:4:4: while the display is SDR the BGRA slot passes
    /// THROUGH (a plain copy into the out ring, no NV12 VideoConverter) so NVENC gets full-chroma
    /// RGB and CSCs to 4:4:4 itself — measured on-glass: `chromaFormatIDC=3` + ARGB input yields
    /// TRUE 4:4:4 and the conversion follows the VUI matrix (BT.709 limited, always written).
    /// While the display is HDR this is overridden to the P010 path (no 10-bit 4:4:4 source):
    /// the stream honestly downgrades to 4:2:0 — the encoder's caps cross-check reports it.
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
    /// PyroWave: the fence's persistent shared NT handle (raw), passed on EVERY frame. The encoder
    /// DUPLICATEs + imports it as a Vulkan timeline semaphore whenever it has none (first frame or
    /// after an encoder rebuild), so this original stays valid across rebuilds.
    pyro_fence_handle: Option<isize>,
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
    /// Stall↔OS-event correlation counters for the metronomic warn: how many stalls this session,
    /// and how many had a coinciding a `pf_win_display::display_events` event in their gap window — the
    /// discriminator between "Windows re-enumerates a monitor each cycle" (devnode churn the
    /// `pnp_disable_monitors` axis suppresses) and "the disturbance is below the OS" (GPU driver
    /// servicing a standby sink / display-poller software).
    stalls_seen: u32,
    stalls_with_os_events: u32,
    /// Host-owned ROTATING output ring NVENC encodes (one YUV texture per slot). Rotating it per frame
    /// is the precondition for pipelining the encode loop: while NVENC encodes frame N's texture on the
    /// ASIC, frame N+1's convert writes a DIFFERENT texture — the two overlap. Format = `out_format()`:
    /// NV12 (SDR, BT.709 limited) or P010 (HDR, BT.2020 PQ limited), so NVENC takes native YUV and skips
    /// its internal RGB→YUV CSC on the SM/3D engine the game saturates (plan §5.A). Rebuilt on a
    /// display-mode flip. Built lazily.
    out_ring: Vec<ID3D11Texture2D>,
    out_idx: usize,
    /// BGRA slot → NV12 (BT.709 limited) on the dedicated D3D11 VIDEO engine, used while the display is
    /// SDR — keeps the colour-convert OFF the contended 3D/compute engine. Built lazily; rebuilt on a
    /// size/HDR flip.
    video_conv: Option<VideoConverter>,
    /// FP16 scRGB slot → P010 (BT.2020 PQ limited) via two shader passes, used while the display is HDR
    /// (NVIDIA's VideoProcessor can't do RGB→P010). The passes run on the 3D engine, but it still skips
    /// NVENC's internal SM-side CSC. Built lazily.
    hdr_p010_conv: Option<HdrP010Converter>,
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
// COM interfaces / the broker's bare control `HANDLE`, which is process-global and never closed). It is
// created, used, and dropped by a SINGLE thread — the owning capture/encode thread — never shared: the
// `ID3D11DeviceContext` is the device's IMMEDIATE context (single-threaded by D3D11 contract) and is
// only ever touched from that thread, and the header pointer (into the mapping this struct owns) is
// only dereferenced there. `Send` transfers ownership to one thread at a time with NO concurrent
// access; we do not (and must not) claim `Sync`.
unsafe impl Send for IddPushCapturer {}

/// Build a `SECURITY_ATTRIBUTES` granting GENERIC_ALL to **SYSTEM only** — `D:P(A;;GA;;;SY)`, protected
/// (no inherited ACEs), `bInheritHandle: false`. The sealed channel makes this the strictly-minimal
/// DACL: the objects are UNNAMED and the driver reaches them via **duplicated handles** (which carry the
/// source handle's access — `OpenSharedResourceByName`/`OpenSharedResource1` on a handle does not
/// re-check the object DACL against the opener), so the pf_vdisplay WUDFHost (LocalService) no longer
/// needs a DACL ACE. Dropping the `LS` ACE removes the last theoretical surface where a leaked handle or
/// a name-grown-by-accident could be opened by the (many-service-shared) LocalService SID. Empirically
/// confirmed unreachable regardless: a LocalService token is DACL-denied `OpenProcess` on the WUDFHost
/// (`PROCESS_DUP_HANDLE`/`VM_READ`/even `QUERY_LIMITED` → ACCESS_DENIED, tested on the RTX box
/// 2026-07-03), so it cannot dup the handles out either. History: `Global\`-named + world-openable
/// (`WD`, security-review 2026-06-28 #5) → SY+LS-scoped → nameless → now SY-only. `psd` must outlive
/// `sa`. See `design/idd-push-security.md`.
unsafe fn shared_object_sa() -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
        w!("D:P(A;;GA;;;SY)"),
        SDDL_REVISION_1,
        &mut psd,
        None,
    )
    .context("build SDDL for IDD-push shared objects")?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    Ok((sa, psd))
}

impl IddPushCapturer {
    /// Create the `RING_LEN` shared keyed-mutex textures for one ring generation, at `format` (matched
    /// to the display's composition format — FP16 in HDR, BGRA in SDR). Each is shared through an
    /// UNNAMED NT handle (nothing to open by name — the sealed channel); the driver reaches it only via
    /// the duplicate the [`ChannelBroker`] sends after the ring is published.
    unsafe fn create_ring_slots(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<Vec<HostSlot>> {
        let (sa, _psd) = shared_object_sa()?;
        let mut slots = Vec::new();
        for _ in 0..RING_LEN {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                // Match the OS-composed swap-chain surfaces so the driver's CopyResource into the slot +
                // its format-guard both succeed.
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                    | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D(IDD-push ring slot)")?;
            let tex = tex.context("null ring texture")?;
            let res1: IDXGIResource1 = tex.cast()?;
            let shared = res1
                .CreateSharedHandle(
                    Some(&sa as *const SECURITY_ATTRIBUTES),
                    DXGI_SHARED_RESOURCE_RW,
                    PCWSTR::null(), // UNNAMED — reachable only through the broker's duplicate
                )
                .context("CreateSharedHandle(IDD-push ring slot)")?;
            // Own the shared handle so the slot's `Drop` closes it via RAII (was a manual `CloseHandle`).
            let shared = OwnedHandle::from_raw_handle(shared.0 as _);
            let mutex: IDXGIKeyedMutex = tex.cast()?;
            let mut srv: Option<ID3D11ShaderResourceView> = None;
            device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))
                .context("CreateShaderResourceView(IDD-push ring slot)")?;
            let srv = srv.context("null slot srv")?;
            slots.push(HostSlot {
                tex,
                mutex,
                shared,
                srv,
            });
        }
        Ok(slots)
    }

    /// Open the IDD-push capturer. On success the caller's `keepalive` is attached (the capturer owns the
    /// virtual display); on FAILURE the keepalive is handed BACK so the caller can fall back to DDA
    /// instead of tearing the display down (audit §5.1 — no more 20 s black bail). "Failure" includes the
    /// driver not attaching to the ring within a few seconds (e.g. a hybrid-GPU render mismatch).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
        want_444: bool,
        pyrowave: bool,
        keepalive: Box<dyn Send>,
        sender: crate::FrameChannelSender,
    ) -> std::result::Result<Self, (anyhow::Error, Box<dyn Send>)> {
        // The stall-attribution listener (idempotent): started with the first IDD-push capturer so
        // the stall log can correlate DWM holes with OS display events for the session's lifetime.
        pf_win_display::display_events::spawn_once();
        match Self::open_inner(target, preferred, client_10bit, want_444, pyrowave, sender) {
            Ok(mut me) => {
                me._keepalive = keepalive;
                Ok(me)
            }
            Err(e) => Err((e, keepalive)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_inner(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
        want_444: bool,
        pyrowave: bool,
        sender: crate::FrameChannelSender,
    ) -> Result<Self> {
        // The ring MUST live on the adapter the driver's swap-chain renders on. Primary: the
        // selected render GPU — the same pick SET_RENDER_ADAPTER pinned the driver to at monitor
        // ADD, so on a healthy box they agree, and NVENC gets a device on a real GPU adapter.
        // (`target.adapter_luid` is NOT that adapter: the ADD reply carries
        // `IDARG_OUT_MONITORARRIVAL.OsAdapterLuid` = the IddCx DISPLAY adapter — verified
        // on-glass; it stays a last-resort fallback for a pickerless box only.) When the pick and
        // the driver HAVE drifted — identical twin GPUs whose max-VRAM tie moved between ADD and
        // this open, or a stale kept monitor across an adapter re-init — the driver reports
        // TEX_FAIL plus the adapter it actually renders on, and the rebind below reopens on that.
        let luid = pf_gpu::resolve_render_adapter_luid().unwrap_or(LUID {
            LowPart: (target.adapter_luid & 0xffff_ffff) as u32,
            HighPart: (target.adapter_luid >> 32) as i32,
        });
        match Self::open_on(
            target.clone(),
            preferred,
            client_10bit,
            want_444,
            pyrowave,
            luid,
            sender.clone(),
        ) {
            Ok(me) => Ok(me),
            Err(e) => {
                // Self-heal a render-adapter mismatch ONCE: on TEX_FAIL the driver has reported the
                // adapter its swap-chain ACTUALLY renders on (a stale monitor across an adapter
                // re-init, or a driver that ignored SET_RENDER_ADAPTER). Rebinding the ring to that
                // adapter beats failing the session — the outer pipeline retries would repeat the
                // exact same mismatch.
                let driver_luid = e
                    .downcast_ref::<AttachTexFail>()
                    .map(|tf| tf.driver_luid)
                    .filter(|d| *d != 0 && *d != crate::dxgi::pack_luid(luid));
                let Some(packed) = driver_luid else {
                    return Err(e);
                };
                let drv = LUID {
                    LowPart: (packed & 0xffff_ffff) as u32,
                    HighPart: (packed >> 32) as i32,
                };
                tracing::warn!(
                    ring_adapter = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    driver_adapter = format!("{:08x}:{:08x}", drv.HighPart, drv.LowPart),
                    "IDD push: ring/driver render-adapter mismatch — rebinding the ring to the \
                     driver's reported adapter"
                );
                Self::open_on(
                    target,
                    preferred,
                    client_10bit,
                    want_444,
                    pyrowave,
                    drv,
                    sender,
                )
                .context("IDD-push rebind to the driver's reported render adapter")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn open_on(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
        want_444: bool,
        pyrowave: bool,
        luid: LUID,
        sender: crate::FrameChannelSender,
    ) -> Result<Self> {
        let (pw, ph, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the shared ring")?;
        // Size the ring to the display's ACTUAL current resolution if it differs from the negotiated mode:
        // a fullscreen game can hold the virtual display at a different mode (esp. across a reconnect), so
        // matching the actual mode lets the first frame flow instead of being dropped (game-capture bug
        // GB1). Falls back to the negotiated mode when the CCD read is unavailable.
        // SAFETY: `active_resolution` is an `unsafe fn` (Win32 CCD `QueryDisplayConfig`) that takes only a
        // copy of the plain `u32` CCD target id and returns owned `(w, h)` values; it forms no borrows from
        // us and validates the id internally, returning `None` on any failure (handled by `unwrap_or`).
        let (w, h) = unsafe { pf_win_display::win_display::active_resolution(target.target_id) }
            .unwrap_or((pw, ph));
        if (w, h) != (pw, ph) {
            tracing::info!(
                target_id = target.target_id,
                negotiated = format!("{pw}x{ph}"),
                actual = format!("{w}x{h}"),
                "IDD push: sizing the ring to the display's actual mode (differs from negotiated)"
            );
        }
        // The driver composes the virtual display in FP16 (R16G16B16A16_FLOAT scRGB) when the display is
        // in advanced-color (HDR) mode, and 8-bit BGRA otherwise (per swap_chain_processor.rs + the
        // COMMIT_MODES2 colorspace/rgb_bpc log). For a 10-bit-capable client we PROACTIVELY enable
        // advanced color so HDR streams without the user toggling anything, then TRACK the display's
        // actual mode (a mid-session "Use HDR" flip; the driver's format-guard drops a mismatch), polling
        // the live state here and on every recreate. An SDR-only client instead forces advanced color OFF
        // and is PINNED there (below + the descriptor poller), so the SDR negotiation is honored and the
        // encoder never emits the in-band PQ upgrade to a client that asked for SDR.
        // SAFETY: one block over the whole ring setup; every operation in it is sound:
        // - `set_advanced_color`/`advanced_color_enabled` are `unsafe fn`s taking only a copy of the plain
        //   `u32` target id; they read/flip CCD display config and return owned values, borrowing nothing.
        // - `CreateDXGIFactory1`, `EnumAdapterByLuid`, `make_device`, `shared_object_sa`, `CreateFileMappingW`,
        //   `MapViewOfFile`, `CreateEventW`, and `create_ring_slots` are all `?`-checked, so every returned
        //   interface/handle/view is non-error before use; `&sa`/`&adapter`/`&device` are live borrows that
        //   outlive each synchronous call, and `sa.lpSecurityDescriptor` stays valid because its backing
        //   `_psd` is held in scope for the whole block.
        // - The header mapping is created AND viewed at `bytes == size_of::<SharedHeader>().max(64)`; the
        //   view's null is checked (`bail!` on failure, after which the owned `map` closes the mapping). The
        //   OS view base is page-aligned, so `section.ptr::<SharedHeader>()` is suitably aligned for a
        //   `SharedHeader`, and `write_bytes(.., 0, bytes)` plus the `(*header).field = ..` writes all stay
        //   within those `bytes` and write THROUGH the raw pointer without forming any `&mut`.
        // - The `magic` publish stores through `addr_of!((*header).magic) as *const AtomicU32`: `addr_of!`
        //   takes the field address without a reference; the field is a 4-aligned `u32` (valid for
        //   `AtomicU32`), and the `Release` store after the `Release` fence is the cross-process handshake
        //   that orders all preceding writes before the driver may observe `MAGIC`.
        // - `broker.send` requires live `header`/`event` handles of this process: both borrow the just-
        //   created owned section/event for the duration of that synchronous call.
        // - `header` points into the OS mapping, NOT into the `MappedSection` struct, so moving `section`
        //   into `me` leaves it valid (see the `MappedSection` doc comment).
        unsafe {
            // An SDR-NEGOTIATED session (either codec) must run on an SDR (BGRA) composition, so
            // actively turn advanced color OFF — undoing any leftover HDR state from a prior 10-bit
            // session on a reused/lingering monitor, the driver's default, or the host's global
            // "Use HDR" — and settle before sizing the ring. Non-optional for two reasons:
            //   - PyroWave: its CSC reads 8-bit BGRA and the NVIDIA D3D11 VideoProcessor can't ingest
            //     the FP16 ring at all.
            //   - H.26x: off an HDR composition the capturer emits P010 and the encoder stamps
            //     Main10 + BT.2020 PQ from the pixel format alone (the in-band HDR upgrade), sending a
            //     10-bit PQ stream to a client that advertised SDR-only ("HDR off = never send me
            //     10-bit"). On a client whose monitor is HDR-capable but has "Use HDR" off, that PQ
            //     lands on an SDR desktop and blows out — the composition must honor the negotiation.
            // An HDR-negotiated (10-bit) session instead enables HDR below and rides the FP16 scRGB
            // ring (design/pyrowave-444-hdr.md Phase 3 for PyroWave; the H.26x P010 path otherwise).
            if !client_10bit {
                let _ = pf_win_display::win_display::set_advanced_color(target.target_id, false);
                let settle = Instant::now();
                while settle.elapsed() < Duration::from_millis(250) {
                    if pf_win_display::win_display::advanced_color_enabled(target.target_id)
                        == Some(false)
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                if pf_win_display::win_display::advanced_color_enabled(target.target_id)
                    == Some(true)
                {
                    tracing::error!(
                        target = target.target_id,
                        pyrowave,
                        "IDD push: SDR session but advanced color (HDR) could NOT be turned off on the \
                         virtual display (a physical display forcing HDR?) — PyroWave will likely fail \
                         its first frame; H.26x would emit PQ the SDR-only client never asked for"
                    );
                } else {
                    tracing::info!(
                        target = target.target_id,
                        pyrowave,
                        settle_ms = settle.elapsed().as_millis() as u64,
                        "IDD push: SDR-negotiated session — advanced color forced OFF (SDR/BGRA composition)"
                    );
                }
            }
            // If we ENABLE advanced color for a 10-bit client, trust it (the driver will compose FP16) and
            // size the ring FP16 directly — don't race the advanced_color_enabled poll, which may not have
            // settled within 250 ms and would size the ring SDR while the driver composes FP16 → a format
            // mismatch → an immediate ring recreate + dropped first frames (audit §5.4).
            let enabled_hdr = client_10bit
                && pf_win_display::win_display::set_advanced_color(target.target_id, true);
            if enabled_hdr {
                // Let the colorspace change settle before the driver composes + we size the ring:
                // poll the CCD advanced-color state instead of a fixed sleep (latency plan P0.4),
                // ceiling = the old 250 ms. A read that never flips within the ceiling proceeds
                // exactly like the fixed sleep did — the ring is sized FP16 from `enabled_hdr`
                // either way (the set succeeded; only the driver's compose flip may lag, which the
                // stash/format-guard machinery absorbs).
                let hdr_settle = Instant::now();
                while hdr_settle.elapsed() < Duration::from_millis(250) {
                    if pf_win_display::win_display::advanced_color_enabled(target.target_id)
                        == Some(true)
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                tracing::debug!(
                    target_id = target.target_id,
                    settle_ms = hdr_settle.elapsed().as_millis() as u64,
                    "IDD push: advanced-color (HDR) enable settle"
                );
            }
            // A failed open-time read defaults to SDR (unless the 10-bit path enabled HDR above) —
            // there is no "last known" yet; the descriptor poller corrects a wrong guess mid-session.
            // An SDR-negotiated session (either codec) forced advanced color OFF above and composes
            // SDR unconditionally: `client_10bit` gates HDR so a client that advertised SDR-only is
            // never handed a PQ stream, even if a physical display forces HDR on (the descriptor
            // poller re-asserts OFF; PyroWave's format guard/stash absorbs any lingering FP16 compose).
            let display_hdr = client_10bit
                && (enabled_hdr
                    || pf_win_display::win_display::advanced_color_enabled(target.target_id)
                        .unwrap_or(false));
            // Downgrade point D (design/hdr-10bit-default-and-av1.md item 2d): the session was
            // NEGOTIATED 10-bit (the client was told HDR in the Welcome), but the virtual display
            // could not enable advanced color — the ring sizes SDR and the encoder will emit 8-bit
            // BT.709, so the client's label overstates the stream until the descriptor poller sees
            // HDR come on. Loud, because every frame of this session is affected.
            if client_10bit && !display_hdr {
                tracing::error!(
                    target = target.target_id,
                    "IDD push: 10-bit HDR was negotiated but enabling advanced color on the \
                     virtual display FAILED — encoding 8-bit SDR while the client was told HDR \
                     (check the display driver / Windows HDR support on this box)"
                );
            }
            let ring_fmt = if display_hdr {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            };
            // Our device (ring + zero-copy NVENC) lives on `luid` — the selected render GPU per
            // `open_inner`; the driver must render the swap-chain on the SAME adapter for the
            // shared textures to open (it reports its actual render LUID into the header, which
            // `open_inner` uses to rebind once if this mismatches).
            let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(luid)
                .context("EnumAdapterByLuid(render adapter) for IDD push")?;
            let (device, context) = make_device(&adapter).context("make_device for IDD push")?;

            let (sa, _psd) = shared_object_sa()?;
            let bytes = std::mem::size_of::<SharedHeader>().max(64);

            // Header — UNNAMED (the sealed channel: the driver gets a duplicated handle, not a name).
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                bytes as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(IDD-push header)")?;
            // Own the mapping handle so it (and its view) free via `MappedSection` RAII even on bail.
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                bytes,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for IDD-push header"); // `map` drops → mapping closed
            }
            let section = MappedSection { handle: map, view };
            let generation = IDD_GENERATION.fetch_add(1, Ordering::Relaxed);
            let header = section.ptr::<SharedHeader>();
            std::ptr::write_bytes(header.cast::<u8>(), 0, bytes);
            (*header).version = VERSION;
            (*header).generation = generation;
            (*header).ring_len = RING_LEN;
            (*header).width = w;
            (*header).height = h;
            // Ring format = the display's composition format (FP16 in HDR, BGRA in SDR). The driver
            // reads this into its `ring_format` and drops any surface that doesn't match.
            (*header).dxgi_format = ring_fmt.0 as u32;
            // The ring NAMES its monitor (proto v3, `design/idd-push-security.md` invariant #10) —
            // stamped before the magic (below), never changed for the ring's life (a mid-session
            // recreate reuses this mapping). The driver refuses to attach a ring naming a different
            // monitor, so a stash cross-wire fails closed instead of leaking frames cross-client
            // (fail-closed refusal VALIDATED on-glass 2026-07-10 via a fault-injected build: driver
            // DRV_STATUS_BIND_FAIL + loud host open failure + sibling stream undisturbed).
            (*header).target_id = target.target_id;

            // Frame-ready event (auto-reset) — UNNAMED, like everything on this channel.
            let event = CreateEventW(Some(&sa), false, false, PCWSTR::null())
                .context("CreateEvent(IDD-push)")?;
            let event = OwnedHandle::from_raw_handle(event.0 as _);

            // Ring of shared keyed-mutex textures, format matched to the display's current mode.
            let slots = Self::create_ring_slots(&device, w, h, ring_fmt)?;

            // Publish: magic LAST (Release) — the ring must be fully initialized before the driver
            // (which receives the channel strictly afterwards) can observe MAGIC.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32))
                .store(MAGIC, Ordering::Release);

            // Deliver the sealed channel: duplicate header + event + every slot texture into the
            // driver's WUDFHost and hand it the values over the control device. All-or-nothing (the
            // broker reaps its remote duplicates on failure), and a failure fails the open — without
            // the delivery the driver can never attach.
            let broker = ChannelBroker::open(target.wudf_pid, sender)?;
            broker
                .send(
                    target.target_id,
                    generation,
                    HANDLE(section.handle.as_raw_handle()),
                    HANDLE(event.as_raw_handle()),
                    &slots,
                )
                .context("deliver IDD-push frame channel to the driver")?;

            tracing::info!(
                target_id = target.target_id,
                wudf_pid = target.wudf_pid,
                render_luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                mode = format!("{w}x{h}"),
                display_hdr,
                client_10bit,
                want_444,
                ring_fp16 = display_hdr,
                "IDD push(host): created sealed ring + delivered the channel; waiting for the driver \
                 to attach + publish"
            );
            let me = Self {
                device,
                context,
                target_id: target.target_id,
                section,
                header,
                event,
                broker,
                width: w,
                height: h,
                slots,
                generation,
                client_10bit,
                display_hdr,
                want_444,
                pyrowave,
                pyro_fence: None,
                pyro_fence_handle: None,
                pyro_fence_value: 0,
                pyro_ring: Vec::new(),
                pyro_conv: None,
                pyro_last: None,
                desc_poller: DescriptorPoller::spawn(
                    target.target_id,
                    DisplayDescriptor {
                        hdr: display_hdr,
                        width: w,
                        height: h,
                    },
                ),
                desc_seq: 0,
                pending_desc: None,
                recovering_since: None,
                last_fresh: Instant::now(),
                last_liveness: Instant::now(),
                last_kick: Instant::now(),
                stall_watch: StallWatch::new(),
                stalls_seen: 0,
                stalls_with_os_events: 0,
                out_ring: Vec::new(),
                out_idx: 0,
                video_conv: None,
                hdr_p010_conv: None,
                last_seq: 0,
                last_present: None,
                status_logged: false,
                // Held from BEFORE the first-frame gate (the display must not idle off while we
                // wait for the first compose) until the capturer drops with the session.
                _display_wake: pf_frame::session_tuning::DisplayWakeRequest::new(),
                // Placeholder; `open()` attaches the real keepalive on success, so a FAILED open can hand
                // it back to the caller for the DDA fallback (audit §5.1).
                _keepalive: Box::new(()),
            };
            // Bounded wait for the driver to ATTACH to the ring AND publish a first frame. An attach
            // failure (DRV_STATUS_TEX_FAIL) or an attach-but-no-frames (a game left the display in a
            // format/size the ring can't match) becomes an open failure the caller falls back from (→ DDA),
            // instead of next_frame's 20 s black-then-bail.
            me.wait_for_attach()?;
            Ok(me)
        }
    }

    /// Block (bounded) until the driver has ATTACHED to the host ring (`DRV_STATUS_OPENED`) **and published
    /// a first frame**, else fail so the caller can fall back to DDA (audit §5.1 +
    /// `design/windows-host-rewrite.md` §2.5 — the GB1 game-capture fix).
    ///
    /// Requiring the first frame — not just the attach — catches the *reconnect-into-a-broken-state* case:
    /// a fullscreen game can leave the virtual display in a format/size that the driver's `publish()` guard
    /// rejects, so the driver ATTACHES but silently drops every frame; without this the host sails past
    /// `open()` and only dies on `next_frame`'s 20 s deadline (the "reconnect = black + audio" symptom).
    /// A stash-capable driver republishes its retained desktop frame the moment it attaches (the
    /// first-frame guarantee — `FrameStash`, driver frame_transport.rs), so the normal case clears this
    /// gate in milliseconds even on an idle desktop; failing that, at session open the OS activates the
    /// virtual display → DWM composites it → a frame arrives within ~1 s, plus the compose-kick fallback
    /// below — no frame within the window = genuinely broken.
    fn wait_for_attach(&self) -> Result<()> {
        // Symmetric host-side binding sanity (proto v3 §3.2): OUR header must still name OUR
        // monitor. The stamp is ours and nothing legitimate rewrites it, so a mismatch means a
        // host-side bug (a stash/capturer cross-wire) — the exact class the driver-side check
        // catches from the other end; failing here names the culprit in the same release.
        // SAFETY: in-bounds, aligned u32 read of the live, owned shared-header mapping (same access
        // pattern as the `driver_status` read below); no reference into the shared region is formed.
        let stamped = unsafe { (*self.header).target_id };
        if stamped != self.target_id {
            bail!(
                "IDD-push: our ring header names target {stamped} but this capturer serves target \
                 {} — host-side ring↔monitor cross-wire (bug); failing the open",
                self.target_id
            );
        }
        let deadline = Instant::now() + Duration::from_secs(4);
        // First-frame expectation: a stash-capable driver republishes its retained desktop frame
        // the moment it attaches (`FrameStash`, frame_transport.rs), so on a healthy pairing the
        // gate below clears in milliseconds even on a perfectly idle desktop. The compose-kick
        // schedule is the FALLBACK for pre-stash drivers / an empty stash (a display that has
        // never composed): DWM only presents a display something DIRTIED, so on an idle desktop
        // an attach would otherwise sit at E_PENDING forever and fail this gate — the
        // "idle desktop → no frames" gotcha. Give the natural post-activate compose (and the
        // stash republish) a moment, then nudge; log when we do, so field logs show whether the
        // stash path is working.
        let mut next_kick = Instant::now() + Duration::from_millis(600);
        loop {
            // SAFETY: `self.header` points into the live shared-header mapping this capturer owns (sized
            // `>= size_of::<SharedHeader>()`, page-aligned), so the field read is in-bounds + aligned, and
            // no reference into the shared region is formed. Plain read: the driver writes this `u32`
            // cross-process, but an aligned `u32` read can't tear and `driver_status` is best-effort
            // diagnostics — the real handshake is the atomic `magic`/`latest` (same access as
            // log_driver_status_once).
            let st = unsafe { (*self.header).driver_status };
            if st == DRV_STATUS_TEX_FAIL {
                // SAFETY: as above — in-bounds, aligned word reads of best-effort diagnostic fields
                // through the owned, live header mapping; no reference into the shared region is formed.
                // The driver wrote its render LUID BEFORE attempting the texture opens
                // (frame_transport.rs step 2), so it is valid here.
                let (detail, lo, hi) = unsafe {
                    (
                        (*self.header).driver_status_detail,
                        (*self.header).driver_render_luid_low,
                        (*self.header).driver_render_luid_high,
                    )
                };
                // Typed so `open_inner` can rebind the ring to the driver's adapter once.
                return Err(anyhow::Error::new(AttachTexFail {
                    detail,
                    driver_luid: ((hi as i64) << 32) | (lo as i64 & 0xffff_ffff),
                }));
            }
            if st == DRV_STATUS_NO_DEVICE1 {
                // SAFETY: as above — an in-bounds, aligned `u32` read of a best-effort diagnostic field
                // through the owned, live header mapping; no reference into the shared region is formed.
                let detail = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver failed to attach (driver_status={st} detail=0x{detail:08x} — \
                     the driver has no ID3D11Device1 to open shared resources)"
                );
            }
            if st == DRV_STATUS_BIND_FAIL {
                // SAFETY: as above — an in-bounds, aligned `u32` read of a best-effort diagnostic field
                // through the owned, live header mapping; no reference into the shared region is formed.
                let claimed = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver REFUSED the ring↔monitor binding (DRV_STATUS_BIND_FAIL: the \
                     delivered ring names target {claimed}, the monitor is {}) — host \
                     stash/delivery cross-wire (bug); failing the open loudly (proto v3 §3.2)",
                    self.target_id
                );
            }
            // Attached AND a frame has been published — the publish token's seq advances past 0.
            if st == DRV_STATUS_OPENED && frame::FrameToken::unpack(self.latest()).seq != 0 {
                return Ok(());
            }
            if Instant::now() >= next_kick {
                // Reaching a kick at all means the driver did NOT republish a retained frame
                // (pre-stash driver, or a never-composed display) — worth a line in the field log.
                tracing::debug!(
                    target_id = self.target_id,
                    driver_status = st,
                    "IDD push: no first frame after attach delivery — falling back to a synthetic \
                     compose kick (stash-capable drivers republish instantly; old driver?)"
                );
                kick_dwm_compose(self.target_id);
                next_kick = Instant::now() + Duration::from_millis(800);
            }
            if Instant::now() > deadline {
                bail!(
                    "IDD-push: no frame published within 4s (despite compose kicks) — {}; \
                     falling back",
                    self.no_first_frame_diagnosis(st)
                );
            }
            // Event-driven wait (latency plan P0.6): the driver signals the frame-ready event on
            // every publish, so wake on it instead of a blind sleep — the 20 ms timeout keeps the
            // driver_status polls above live (status writes don't signal the event). Consuming a
            // signal here is fine: `next_frame` re-checks the atomic `latest` token, never the
            // event, for truth.
            // SAFETY: `self.event` is this capturer's owned, live auto-reset event handle;
            // `WaitForSingleObject` only reads the handle and the 20 ms timeout bounds the wait.
            let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), 20) };
        }
    }

    /// Name a first-frame timeout from the driver's own evidence — `driver_status` plus the live
    /// OPENED detail word (proto `pack_opened_detail`) — instead of guessing. The three no-frames
    /// states look identical from the host side but have disjoint causes and fixes; the lid-closed
    /// field report burned days for lack of exactly this line. Appends a console-session hint when
    /// the host itself is in the wrong session (display writes + input kicks can't work from there).
    fn no_first_frame_diagnosis(&self, st: u32) -> String {
        let what = match st {
            // The delivery was never consumed: no swap-chain worker ran for this monitor at all.
            DRV_STATUS_NONE => "the driver never attached — the channel delivery was never \
                 consumed, so the OS ran no swap-chain worker for this monitor (display not \
                 composed at all: console display-off / modern standby, or the mode commit \
                 never reached the adapter)"
                .to_string(),
            DRV_STATUS_OPENED => {
                // SAFETY: in-bounds, aligned u32 read of the live, owned shared-header mapping
                // (same best-effort diagnostic access as the `driver_status` read in the caller);
                // no reference into the shared region is formed.
                let detail = unsafe { (*self.header).driver_status_detail };
                match unpack_opened_detail(detail) {
                    Some((0, _)) => "driver attached with a live swap-chain, but DWM composed \
                         ZERO frames — an undamaged or powered-off desktop, and the compose \
                         kicks didn't bite (synthetic input is blocked on the secure desktop)"
                        .to_string(),
                    Some((offered, mismatched)) => format!(
                        "driver attached and DWM composed {offered} frame(s), but none matched \
                         the ring — {mismatched} dropped for a size/format mismatch (the \
                         display's actual mode differs from what the host sized the ring to: \
                         a mid-open mode-set, a fullscreen game, or a stale GDI view)"
                    ),
                    // A pre-detail driver never stamps the live bit — say so rather than guess.
                    None => "driver attached but published nothing; this pf-vdisplay build \
                         predates attach diagnostics, so the cause can't be named — update the \
                         driver for a precise line here"
                        .to_string(),
                }
            }
            other => format!("driver_status={other} (unexpected at this point)"),
        };
        match pf_win_display::console_session_mismatch() {
            Some((own, console)) => format!(
                "{what} [host is in session {own} but the console is session {console} — display \
                 writes and input kicks cannot work from a non-console session; reconnect the \
                 console or run via the installed service]"
            ),
            None => what,
        }
    }

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

    /// Log the driver's status once it first reports (the only driver-visibility channel we have).
    fn log_driver_status_once(&mut self) {
        if self.status_logged {
            return;
        }
        // SAFETY: four in-bounds, aligned reads of the live, owned shared-header mapping. The driver writes
        // these `u32`/`i32` diagnostic fields cross-process, but aligned word reads can't tear and these are
        // best-effort status (the real handshake is the atomic `magic`/`latest`); no `&`/`&mut` reference
        // into the shared region is formed.
        let (status, detail, lo, hi) = unsafe {
            (
                (*self.header).driver_status,
                (*self.header).driver_status_detail,
                (*self.header).driver_render_luid_low,
                (*self.header).driver_render_luid_high,
            )
        };
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
    /// state (like the WGC path) plus the session's 4:4:4 negotiation: HDR → `P010` (BT.2020 PQ
    /// 10-bit limited) → NVENC Main10, and the client auto-detects PQ from the HEVC VUI; SDR →
    /// `Nv12` (BT.709 8-bit limited), or full-chroma `Bgra` passthrough on a 4:4:4 session (NVENC
    /// CSCs RGB→YUV444 itself, following the BT.709 VUI — the one path that deliberately pays the
    /// SM-side CSC, because the video processor can only produce subsampled output). We do NOT
    /// gate HDR on the client's advertised `VIDEO_CAP_10BIT` — clients under-report it (e.g. the
    /// Mac advertises 10-bit only when its OWN display is HDR), yet all decode Main10 +
    /// auto-switch, exactly as on the WGC path. HDR wins over 4:4:4 (there is no 10-bit
    /// full-chroma source): the stream downgrades to 4:2:0 with a warning.
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
                warn_444_hdr_downgrade_once();
            }
            (DXGI_FORMAT_P010, PixelFormat::P010)
        } else if self.want_444 {
            (DXGI_FORMAT_B8G8R8A8_UNORM, PixelFormat::Bgra)
        } else {
            (DXGI_FORMAT_NV12, PixelFormat::Nv12)
        }
    }

    /// The ring (shared-texture) format, matched to the display's composition format: FP16 when the
    /// display is HDR, BGRA when SDR.
    fn ring_format(&self) -> DXGI_FORMAT {
        if self.display_hdr {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        }
    }

    /// Recreate the ring at the format for `new_display_hdr` (the user flipped "Use HDR"). Bumps the
    /// generation so the driver re-attaches ([`is_stale`]) to the new-format textures and DELIVERS the
    /// new channel (fresh duplicates of the header + event + the new textures — every delivery is a
    /// self-contained handle set the driver owns); clears the header's `latest` so we don't consume a
    /// stale slot from the old ring; drops the conversion textures so they rebuild at the new format.
    fn recreate_ring(&mut self, new_display_hdr: bool, new_w: u32, new_h: u32) -> Result<()> {
        self.display_hdr = new_display_hdr;
        self.width = new_w;
        self.height = new_h;
        let fmt = self.ring_format();
        let new_gen = IDD_GENERATION.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `create_ring_slots` is an `unsafe fn` (it makes D3D11/DXGI COM calls); we pass a live
        // borrow of `self.device` (the capturer's own device, on which the slots are created) plus plain
        // `u32`/`DXGI_FORMAT` values, and `?` propagates any failure before the slots are used. Every
        // returned slot's texture + keyed mutex belongs to that same `self.device`.
        let new_slots =
            unsafe { Self::create_ring_slots(&self.device, self.width, self.height, fmt)? };
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
            // Publish the new generation LAST (Release): when the driver observes it (Acquire) the new
            // textures already exist and the format is already updated.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*self.header).generation) as *const AtomicU32))
                .store(new_gen, Ordering::Release);
        }
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
        self.last_seq = 0;
        self.out_ring.clear(); // the output format changed → rebuild lazily at the new format
        self.video_conv = None; // converters are sized + HDR-specific → rebuild at the new mode
        self.hdr_p010_conv = None;
        // The PyroWave CSC is mode-baked too (BgraToYuvPlanes picks different SDR vs HDR shaders
        // and R8/R8G8 vs R16/R16G16 outputs). Without this, a display_hdr flip (Downgrade point D:
        // client_10bit=true but HDR couldn't enable at open) reused the stale SDR converter against
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
        // Two cases re-assert the NEGOTIATED depth instead of following a mid-session "Use HDR"
        // flip — flip the display back and treat the descriptor as the negotiated state (so the ring
        // is never recreated at the wrong format):
        //   - a PyroWave session: its encoder was opened for fixed plane formats (R8 SDR / R16 HDR),
        //     so it can't follow a flip the way H.26x re-inits do;
        //   - ANY SDR-negotiated session (`!client_10bit`, either codec): a host-side flip to HDR
        //     must not promote the stream to P010 PQ behind a client that advertised SDR-only.
        // An HDR-negotiated H.26x session is NOT pinned — it still follows a host "Use HDR" flip in
        // either direction (its encoder re-inits on the depth change).
        if (self.pyrowave || !self.client_10bit) && now.hdr != self.client_10bit {
            // SAFETY: `set_advanced_color` is `unsafe` (CCD DisplayConfig calls); it takes a plain
            // `u32` target id + bool, forms no lasting borrow, and returns a bool.
            unsafe {
                let _ = pf_win_display::win_display::set_advanced_color(
                    self.target_id,
                    self.client_10bit,
                );
            }
            now.hdr = self.client_10bit;
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
            // NVENC registers it as encode input — matching the WGC YUV ring. (PyroWave uses its own
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
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D(IDD out ring)")?;
                self.out_ring.push(t.context("null out-ring texture")?);
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
            // SAFETY: `BgraToYuvPlanes::new` compiles D3D11 shaders on `self.device`; `?` propagates
            // failure before it is stored.
            self.pyro_conv = Some(unsafe {
                BgraToYuvPlanes::new(&self.device, self.display_hdr, self.want_444)?
            });
        }
        Ok(())
    }

    /// Build the per-mode YUV converter if not already built: a VIDEO-engine BGRA→NV12 processor on an
    /// SDR display, or the FP16→P010 shader on an HDR display. Both keep NVENC's RGB→YUV CSC off the SM.
    /// An SDR 4:4:4 session needs NO converter — the BGRA slot passes through (see `out_format`).
    fn ensure_converter(&mut self) -> Result<()> {
        if self.display_hdr {
            if self.hdr_p010_conv.is_none() {
                // SAFETY: `HdrP010Converter::new` is `unsafe` (it compiles D3D11 shaders + creates
                // resources); we pass a live borrow of `self.device`, the device the converter's resources
                // belong to, and `?` propagates any failure before the converter is stored.
                self.hdr_p010_conv = Some(unsafe { HdrP010Converter::new(&self.device)? });
            }
        } else if self.want_444 {
            // Full-chroma passthrough — no conversion resources to build.
        } else if self.video_conv.is_none() {
            // SAFETY: `VideoConverter::new` is `unsafe` (it sets up the D3D11 VIDEO processor); we pass live
            // borrows of `self.device` + its immediate `self.context` (single-threaded, this thread) plus
            // plain `u32` dimensions, and `?` propagates any failure before it is stored. The converter's
            // resources belong to that same device/context.
            self.video_conv = Some(unsafe {
                VideoConverter::new(&self.device, &self.context, self.width, self.height, false)?
            });
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
            self.pyro_fence_handle = Some(handle.0 as isize);
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
        Ok(Some((self.pyro_fence_handle, value)))
    }

    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        self.log_driver_status_once();
        // Follow the display: a "Use HDR" flip recreates the ring at the matching format.
        self.poll_display_hdr();
        // Recover-or-drop (GB1): if a descriptor change triggered a recreate but no fresh frame has resumed
        // within the window, the IDD-push path can't follow the display (e.g. an exclusive-flip) — drop the
        // session cleanly (the loop's `?` ends it → the client reconnects) rather than freeze forever.
        if let Some(since) = self.recovering_since {
            if since.elapsed() > Duration::from_secs(3) {
                bail!(
                    "IDD-push: display descriptor changed and the ring could not recover within 3s — \
                     dropping the session so the client reconnects"
                );
            }
            // Same idle-desktop stall as the open-time attach gate: after a mid-session ring
            // recreate (HDR flip / mode change) an idle desktop composes nothing, so the fresh ring
            // never sees a frame and the 3 s recover-or-drop above kills a healthy session. A
            // stash-capable driver republishes its retained frame at the re-attach, so this kick
            // is the legacy-driver fallback here too. Nudge DWM (rate-limited) once the natural
            // post-recreate compose (and the stash republish) has had its chance.
            if since.elapsed() > Duration::from_millis(600)
                && self.last_kick.elapsed() > Duration::from_millis(800)
            {
                self.last_kick = Instant::now();
                tracing::debug!(
                    target_id = self.target_id,
                    "IDD push: no frame after ring recreate — falling back to a synthetic compose \
                     kick (stash-capable drivers republish at re-attach; old driver?)"
                );
                kick_dwm_compose(self.target_id);
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
        let slot = tok.slot as usize;
        if seq == self.last_seq || slot >= self.slots.len() {
            return Ok(None);
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
            (Some(self.out_ring[i].clone()), None)
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
        let s = &self.slots[slot];
        // Acquire the slot's keyed mutex via a RAII guard, scoped to JUST the convert/copy below so it
        // releases at the same point as the old hand-written `ReleaseSync` (the driver gets the slot back
        // immediately, NOT held across the rest of `try_consume`) — but now leak-proof on any early return.
        {
            let Some(_lock) = KeyedMutexGuard::acquire(&s.mutex, 0, 8) else {
                return Ok(None);
            };
            // SAFETY: convert on the owning (encode) thread's immediate context, holding the slot lock.
            // A `?` here is leak-safe: `_lock` (the KeyedMutexGuard) drops on the early return, releasing
            // the slot back to the driver.
            unsafe {
                if self.pyrowave {
                    // PyroWave: ring slot SRV (BGRA for SDR, scRGB FP16 for HDR) → the two separate
                    // plane textures via the mode-aware CSC; the shared fence signalled just after
                    // (`pyro_fence_signal`) orders the encoder's cross-device Vulkan read after this
                    // convert. The composition format is pinned to the negotiated depth.
                    let (_, y_rtv, _, cbcr_rtv) = pyro_slot.as_ref().expect("pyro slot");
                    if let Some(conv) = self.pyro_conv.as_ref() {
                        conv.convert(
                            &self.context,
                            &s.srv,
                            y_rtv,
                            cbcr_rtv,
                            self.width,
                            self.height,
                        )?;
                    }
                } else if self.display_hdr {
                    // HDR: FP16 slot SRV → P010 (BT.2020 PQ) via the shader; NVENC takes native P010.
                    if let Some(conv) = self.hdr_p010_conv.as_ref() {
                        conv.convert(
                            &self.device,
                            &self.context,
                            &s.srv,
                            out.as_ref().expect("out ring"),
                            self.width,
                            self.height,
                        )?;
                    }
                } else if self.want_444 {
                    // SDR 4:4:4: pass the BGRA slot through untouched — NVENC ingests full-chroma
                    // RGB and CSCs to YUV 4:4:4 itself (per the always-written BT.709 VUI). Plain
                    // copy-engine move; the slot releases back to the driver immediately.
                    self.context
                        .CopyResource(out.as_ref().expect("out ring"), &s.tex);
                } else {
                    // SDR: BGRA slot → NV12 on the VIDEO engine; NVENC takes native NV12, no SM-side CSC.
                    if let Some(conv) = self.video_conv.as_ref() {
                        conv.convert(&s.tex, out.as_ref().expect("out ring"))?;
                    }
                }
            }
            // `_lock` drops here → `ReleaseSync(0)`.
        }
        self.out_idx = (i + 1) % ring_len;
        self.last_seq = seq;
        if let Some((y, _, cbcr, _)) = pyro_slot.as_ref() {
            self.pyro_last = Some((y.clone(), cbcr.clone()));
        } else {
            self.last_present = Some((out.as_ref().expect("out ring").clone(), pf));
        }
        let now = Instant::now();
        if self.recovering_since.take().is_some() {
            // A fresh frame resumed → recovered. The recovery gap is self-inflicted (ring
            // recreate, already logged by the recreate path) — reset the stall watch so it
            // doesn't read as a DWM stall.
            self.stall_watch.reset();
        } else if let Some(stall) = self.stall_watch.note_fresh(now) {
            // OS display events inside the gap (plus a lead-in margin: the event that CAUSED the
            // hole lands just before DWM stops delivering) — the attribution that turns "DWM
            // stopped composing" into "…because Windows re-enumerated SAMSUNG on HDMI".
            let window = stall.gap + Duration::from_millis(300);
            let events = now
                .checked_sub(window)
                .map(|from| pf_win_display::display_events::events_between(from, now))
                .unwrap_or_default();
            self.stalls_seen = self.stalls_seen.saturating_add(1);
            if !events.is_empty() {
                self.stalls_with_os_events = self.stalls_with_os_events.saturating_add(1);
            }
            // debug (not warn): a single hole also happens when content legitimately pauses;
            // the reportable signal is the metronomic cycle below. Mounjay-class triage runs
            // at debug level, and the web-console debug ring captures these.
            tracing::debug!(
                gap_ms = stall.gap.as_millis() as u64,
                os_display_events = %pf_win_display::display_events::summarize(&events),
                "IDD-push capture stall — the desktop was composing at speed, then DWM \
                 delivered no frame for the gap; the present path stalled below capture"
            );
            if let Some(period) = stall.metronomic {
                let suspects = pf_win_display::display_events::connected_inactive_externals();
                let suspects = if suspects.is_empty() {
                    "none".to_string()
                } else {
                    suspects.join(", ")
                };
                let correlated = format!("{}/{}", self.stalls_with_os_events, self.stalls_seen);
                // Half-or-more of the stalls carrying a coinciding OS event = the reaction
                // cascade is OS-visible; otherwise the disturbance never surfaces above the
                // driver. Different classes, different cures — say which one this box has.
                if self.stalls_with_os_events * 2 >= self.stalls_seen {
                    tracing::warn!(
                        period_s = format!("{:.2}", period.as_secs_f64()),
                        os_correlated = correlated,
                        connected_inactive = %suspects,
                        "capture stalls are METRONOMIC and coincide with Windows monitor \
                         hot-plug/re-enumeration events — a connected display (or its \
                         cable/switch/AVR) re-probes the link on a timer and Windows re-reacts \
                         each time. Cures, best-first: that display's OSD 'auto input \
                         scan/detect' OFF (and on TVs: instant-on/quick-start + CEC off), \
                         unplug its cable at the GPU, an HPD-holding adapter/dummy plug, or \
                         keep it active while streaming; the pnp_disable_monitors policy axis \
                         suppresses the Windows-side reaction (see connected_inactive for the \
                         suspects)"
                    );
                } else {
                    tracing::warn!(
                        period_s = format!("{:.2}", period.as_secs_f64()),
                        os_correlated = correlated,
                        connected_inactive = %suspects,
                        "capture stalls are METRONOMIC with NO coinciding OS display event — \
                         the disturbance is BELOW Windows: the GPU driver servicing a \
                         connected-but-asleep sink (standby HPD/DDC/link probing), \
                         display-poller software (the SteelSeries-GG/SignalRGB class — \
                         correlate 'slow display-descriptor poll' lines), or the DWM present \
                         clock (try a different refresh rate). If connected_inactive lists a \
                         display, its standby probing is the prime suspect: unplug it at the \
                         GPU, disable its OSD auto input scan (TVs: instant-on/quick-start + \
                         CEC off), use an HPD-holding adapter/dummy, or keep it active while \
                         streaming"
                    );
                }
            }
        }
        self.last_fresh = now; // feeds the driver-death watch
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
                }),
            )
        } else {
            (out.expect("out ring texture"), None)
        };
        Ok(Some(CapturedFrame {
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
                    }),
                }),
                cursor: None,
            });
        }
        let (src, pf) = self.last_present.clone()?;
        let dst = self.out_ring.get(i)?.clone();
        // SAFETY: GPU copy on the owning thread's immediate context; src/dst are our out-ring textures of
        // identical format/size (src is a previous out-ring slot; dst the next).
        unsafe {
            self.context.CopyResource(&dst, &src);
        }
        self.out_idx = (i + 1) % self.out_ring.len();
        self.last_present = Some((dst.clone(), pf));
        Some(CapturedFrame {
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

/// `wait_for_attach`'s DRV_STATUS_TEX_FAIL as a typed error: the driver could not open the ring
/// textures, and `driver_luid` (packed, from the shared header) is the adapter its swap-chain
/// ACTUALLY renders on — `open_inner` downcasts to this to rebind the ring there once.
#[derive(Debug)]
struct AttachTexFail {
    detail: u32,
    driver_luid: i64,
}

impl std::fmt::Display for AttachTexFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "IDD-push driver failed to attach (driver_status={DRV_STATUS_TEX_FAIL} \
             detail=0x{:08x}): it could not open the ring textures — its swap-chain renders on \
             adapter {:08x}:{:08x}, not the ring's (render-adapter mismatch)",
            self.detail,
            (self.driver_luid >> 32) as i32,
            (self.driver_luid & 0xffff_ffff) as u32,
        )
    }
}

impl std::error::Error for AttachTexFail {}

impl Capturer for IddPushCapturer {
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
                // SAFETY: four in-bounds, aligned reads of the live, owned shared-header mapping — the same
                // best-effort diagnostic fields as `log_driver_status_once` (aligned word reads can't tear;
                // no reference into the shared region is formed).
                let (st, detail, lo, hi) = unsafe {
                    (
                        (*self.header).driver_status,
                        (*self.header).driver_status_detail,
                        (*self.header).driver_render_luid_low,
                        (*self.header).driver_render_luid_high,
                    )
                };
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
        // `PUNKTFUNK_IDD_DEPTH` overrides (1 disables pipelining; clamp to ≤ OUT_RING so a frame in flight
        // always has its own texture).
        pf_host_config::config().idd_depth.clamp(1, OUT_RING)
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
}

/// A 4:4:4 session while the display is HDR: there is no 10-bit full-chroma source (the FP16
/// desktop needs the PQ tone curve, which the P010 shader provides at 4:2:0), so the stream
/// honestly downgrades — the encoder's `chroma_444` caps cross-check reports it and the in-band
/// SPS keeps the client decoding correctly. Once per process: the state can flap mid-session.
fn warn_444_hdr_downgrade_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if ONCE.swap(false, Ordering::Relaxed) {
        tracing::warn!(
            "4:4:4 negotiated but the display is HDR — no 10-bit full-chroma source exists; \
             encoding HDR 4:2:0 (P010) instead (disable HDR on the virtual display for 4:4:4)"
        );
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
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

    /// Feed a [`StallWatch`] fresh frames at the given offsets (ms from a common origin) and
    /// return what each `note_fresh` produced.
    fn watch_run(offsets_ms: &[u64]) -> Vec<Option<Stall>> {
        let base = Instant::now();
        let mut w = StallWatch::new();
        offsets_ms
            .iter()
            .map(|ms| w.note_fresh(base + Duration::from_millis(*ms)))
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
        let stall = out[20].as_ref().expect("hole after active flow is a stall");
        assert_eq!(stall.gap.as_millis(), 300);
        assert!(stall.metronomic.is_none(), "one stall is not a cycle");
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
        let stalls: Vec<&Stall> = out.iter().flatten().collect();
        assert_eq!(stalls.len(), 4, "each cycle boundary is one stall");
        assert!(stalls[..3].iter().all(|s| s.metronomic.is_none()));
        let period = stalls[3]
            .metronomic
            .expect("the 4th evenly-spaced event completes the metronome streak");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.3,
            "period={period:?}"
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
}
