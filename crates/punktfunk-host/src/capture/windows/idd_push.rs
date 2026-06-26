//! P2 direct frame push (kill DDA) — HOST side. The pf-vdisplay driver runs in a restricted WUDFHost
//! token that canNOT create named kernel objects, so — exactly like the gamepad UMDF drivers
//! (`inject/dualsense_windows.rs`) — the HOST (privileged) CREATES the shared header + frame-ready
//! event + ring of keyed-mutex textures (`Global\` names, permissive `D:(A;;GA;;;WD)` SDDL) on the
//! discrete render GPU, and the driver only OPENS them and copies frames in. We then consume the ring
//! straight into the zero-copy NVENC path — no DXGI Desktop Duplication, no `win32u` hook. Gated by
//! `PUNKTFUNK_IDD_PUSH`. Driver counterpart: `packaging/windows/drivers/pf-vdisplay/src/
//! frame_transport.rs`. The shared `SharedHeader` layout, `MAGIC`/`VERSION`/`RING_LEN`, the
//! `DRV_STATUS_*` codes, the `Global\` name scheme and the publish token all come from
//! [`pf_driver_proto::frame`] (which OWNS the contract, with `const` size asserts) — both sides
//! `use` it, so drift is a compile error rather than a "must match" comment.

use super::dxgi::{make_device, D3d11Frame, HdrConverter, WinCaptureTarget};
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{bail, Context, Result};
use pf_driver_proto::frame;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
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
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

// The frame-transport contract — `SharedHeader` layout, `MAGIC`/`VERSION`/`RING_LEN`, the
// `DRV_STATUS_*` codes and the `Global\` name helpers — lives in `pf_driver_proto::frame`; both sides
// `use frame::*`, so a layout/name/code drift is a compile error (the proto has `const` size asserts).
use frame::{
    event_name, header_name, texture_name, SharedHeader, DRV_STATUS_NO_DEVICE1, DRV_STATUS_OPENED,
    DRV_STATUS_TEX_FAIL, MAGIC, RING_LEN, VERSION,
};

/// `DXGI_SHARED_RESOURCE_READ | _WRITE` for `CreateSharedHandle`/`OpenSharedResourceByName`. Local (not
/// part of the proto contract — it is a DXGI sharing-API arg, mirrored on the driver side).
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;

/// Host-owned output-ring depth: distinct NVENC-input textures rotated per frame so the in-flight
/// encode of frame N and the convert/copy of frame N+1 never touch the same texture. 3 covers a
/// pipeline depth of 2 with one slot of margin.
const OUT_RING: usize = 3;

/// Bring-up debug block (fixed name) — the host creates it; the driver writes diagnostics into it
/// independent of the per-target header. NOT part of `pf_driver_proto` (a host-side bring-up channel,
/// not the data path); the matching `DebugBlock` lives in the OLD oracle driver's `frame_transport.rs`.
#[repr(C)]
struct DebugBlock {
    magic: u32,
    run_core_entries: u32,
    resolved_target_id: u32,
    header_open_attempts: u32,
    last_open_error: u32,
    header_opened: u32,
    render_luid_low: u32,
    render_luid_high: i32,
    frames_acquired: u32,
    _pad: u32,
}
const DBG_NAME: &str = "Global\\pfvd-dbg";
const DBG_MAGIC: u32 = 0x4742_4450;

/// Monotonic per-process generation: each capturer instance stamps its ring-texture names with a
/// fresh value so a retried/overlapping `open()` never collides with a previous attempt's not-yet-
/// released shared-handle names (`DXGI_ERROR_NAME_ALREADY_EXISTS`). The driver reads it from the header.
static IDD_GENERATION: AtomicU32 = AtomicU32::new(1);

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// RAII wrapper for a file-mapping object + its mapped view: on drop the view is `UnmapViewOfFile`'d,
/// THEN the [`OwnedHandle`] closes the underlying mapping object (order matters — unmap before close).
/// A `header`/`dbg_block` raw pointer borrows into the view via [`ptr`](Self::ptr); the section must
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
    /// The named shared-resource handle, held only to keep the resource alive (the driver opens it by
    /// NAME). An [`OwnedHandle`] so it closes on drop (was a manual `CloseHandle` in a `Drop` impl);
    /// never read directly — its sole purpose is the RAII close.
    #[allow(dead_code)]
    shared: OwnedHandle,
    /// SRV on the slot texture so the HDR path samples the FP16 slot DIRECTLY (no slot→scratch copy);
    /// the convert pass writes the output ring while holding the slot's keyed mutex. Unused for SDR
    /// (which CopyResource's the BGRA slot straight to the output).
    srv: ID3D11ShaderResourceView,
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

impl<'a> KeyedMutexGuard<'a> {
    /// Acquire `mutex` at `key`, waiting up to `timeout_ms`. `None` if the acquire times out / errors
    /// (the caller skips the frame), so the guard is only ever held when the lock is genuinely held.
    fn acquire(
        mutex: &'a IDXGIKeyedMutex,
        key: u64,
        timeout_ms: u32,
    ) -> Option<KeyedMutexGuard<'a>> {
        // SAFETY: `mutex` is a live `IDXGIKeyedMutex` on this thread's immediate-context device.
        if unsafe { mutex.AcquireSync(key, timeout_ms) }.is_err() {
            return None;
        }
        Some(KeyedMutexGuard { mutex, key })
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

/// Creates + owns the shared ring; yields the driver's frames as [`FramePayload::D3d11`].
pub struct IddPushCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    target_id: u32,
    /// Owns the shared-header file mapping + its mapped view (RAII unmap-then-close). Declared BEFORE
    /// `header`, which is a raw pointer borrowed into this view via [`MappedSection::ptr`]. Never read
    /// directly (the `header` pointer is) — held purely so the mapping outlives the capturer.
    #[allow(dead_code)]
    section: MappedSection,
    header: *mut SharedHeader,
    event: OwnedHandle,
    /// Owns the bring-up debug section (mapping + view), or `None` when the debug block wasn't created.
    /// Never read directly (the `dbg_block` pointer is) — held purely for the RAII unmap/close.
    #[allow(dead_code)]
    dbg_section: Option<MappedSection>,
    dbg_block: *mut DebugBlock,
    width: u32,
    height: u32,
    slots: Vec<HostSlot>,
    /// The ring/texture generation, bumped every time the ring is recreated at a new format (the
    /// display's HDR mode flipped). Stamped into the texture names + the header so the driver re-attaches.
    generation: u32,
    /// The CLIENT's advertised 10-bit capability (= negotiated `bit_depth >= 10`). Only used at `open`
    /// to PROACTIVELY enable advanced color (so a 10-bit client gets HDR without a manual toggle); it
    /// does NOT gate the per-frame conversion — that follows the display, like the WGC path (clients
    /// under-report 10-bit yet all decode Main10 + auto-detect PQ from the VUI).
    client_10bit: bool,
    /// The DISPLAY's CURRENT HDR state (from `advanced_color_enabled`) — the user can flip "Use HDR" in
    /// Windows mid-session. Drives the ring format (HDR → FP16 surfaces, SDR → BGRA) and the conversion.
    /// Polled in the capture loop; a change recreates the ring (see [`Self::recreate_ring`]).
    display_hdr: bool,
    /// Throttle for the `advanced_color_enabled` poll (a CCD `QueryDisplayConfig`, ~ms — too costly per
    /// frame at 240 Hz).
    last_acm_poll: Instant,
    /// Set when a display-descriptor change triggered a ring recreate (recovery, game-capture bug GB1);
    /// cleared when a fresh frame resumes. If it stays set past the recovery window, `try_consume` drops
    /// the session (recover-or-drop, no DDA).
    recovering_since: Option<Instant>,
    /// Host-owned ROTATING output ring NVENC encodes (texture + RTV per slot). Rotating it per frame is
    /// the precondition for pipelining the encode loop: while NVENC encodes frame N's texture on the
    /// ASIC, frame N+1's convert/copy writes a DIFFERENT texture on the 3D engine — the two overlap. The
    /// HDR convert and the SDR copy both write into the current slot. Format = `out_format()` (Rgb10a2 in
    /// HDR, Bgra in SDR); rebuilt on a display-mode flip. Built lazily.
    out_ring: Vec<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    out_idx: usize,
    /// FP16 scRGB → `Rgb10a2` BT.2020 PQ converter, used while the display is HDR. Built lazily.
    hdr_conv: Option<HdrConverter>,
    last_seq: u64,
    last_present: Option<(ID3D11Texture2D, PixelFormat)>,
    status_logged: bool,
    _keepalive: Box<dyn Send>,
}
// COM objects used only from the owning (encode) thread.
unsafe impl Send for IddPushCapturer {}

/// Build a permissive (Everyone:GenericAll) `SECURITY_ATTRIBUTES` so the restricted WUDFHost driver
/// can OPEN the host-created objects — the same `D:(A;;GA;;;WD)` SDDL the gamepad shared section uses.
/// The returned `psd` backing must outlive `sa`; both are dropped when the process exits.
unsafe fn permissive_sa() -> Result<(SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR)> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
        w!("D:(A;;GA;;;WD)"),
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
    /// to the display's composition format — FP16 in HDR, BGRA in SDR). Each is shared by the name
    /// `pfvd-tex-<target>-<generation>-<k>` so the driver opens it; a fresh generation gives fresh names
    /// (so a recreate never collides with the old ring's not-yet-released handles).
    unsafe fn create_ring_slots(
        device: &ID3D11Device,
        target_id: u32,
        generation: u32,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<Vec<HostSlot>> {
        let (sa, _psd) = permissive_sa()?;
        let mut slots = Vec::new();
        for k in 0..RING_LEN {
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
                    &HSTRING::from(texture_name(target_id, generation, k)),
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
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
        keepalive: Box<dyn Send>,
    ) -> std::result::Result<Self, (anyhow::Error, Box<dyn Send>)> {
        match Self::open_inner(target, preferred, client_10bit) {
            Ok(mut me) => {
                me._keepalive = keepalive;
                Ok(me)
            }
            Err(e) => Err((e, keepalive)),
        }
    }

    fn open_inner(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
    ) -> Result<Self> {
        let (pw, ph, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the shared ring")?;
        // Size the ring to the display's ACTUAL current resolution if it differs from the negotiated mode:
        // a fullscreen game can hold the virtual display at a different mode (esp. across a reconnect), so
        // matching the actual mode lets the first frame flow instead of being dropped (game-capture bug
        // GB1). Falls back to the negotiated mode when the CCD read is unavailable.
        let (w, h) =
            unsafe { crate::win_display::active_resolution(target.target_id) }.unwrap_or((pw, ph));
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
        // COMMIT_MODES2 colorspace/rgb_bpc log). The user can flip "Use HDR" in Windows at any time, so
        // the ring format must TRACK the display's ACTUAL mode (the driver's format-guard drops a
        // mismatch). We poll the live state here and on every recreate. For a 10-bit-capable client we
        // PROACTIVELY enable advanced color so HDR streams without the user toggling anything; an
        // SDR-only client leaves the display alone (and still gets a tone-mapped picture, never a freeze,
        // if the user does enable HDR).
        unsafe {
            // If we ENABLE advanced color for a 10-bit client, trust it (the driver will compose FP16) and
            // size the ring FP16 directly — don't race the advanced_color_enabled poll, which may not have
            // settled within 250 ms and would size the ring SDR while the driver composes FP16 → a format
            // mismatch → an immediate ring recreate + dropped first frames (audit §5.4).
            let enabled_hdr =
                client_10bit && crate::win_display::set_advanced_color(target.target_id, true);
            if enabled_hdr {
                // Let the colorspace change settle before the driver composes + we size the ring.
                std::thread::sleep(Duration::from_millis(250));
            }
            let display_hdr =
                enabled_hdr || crate::win_display::advanced_color_enabled(target.target_id);
            let ring_fmt = if display_hdr {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            };
            // Create our device on the discrete render GPU (where NVENC runs); the driver must render
            // the swap-chain on the SAME adapter for the shared textures to open (it reports its actual
            // render LUID into the header so we can detect a mismatch).
            let luid = resolve_render_adapter_luid_or(target.adapter_luid);
            let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(luid)
                .context("EnumAdapterByLuid(render adapter) for IDD push")?;
            let (device, context) = make_device(&adapter).context("make_device for IDD push")?;

            let (sa, _psd) = permissive_sa()?;
            let bytes = std::mem::size_of::<SharedHeader>().max(64);

            // Header.
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                bytes as u32,
                &HSTRING::from(header_name(target.target_id)),
            )
            .context("CreateFileMapping(IDD-push header)")?;
            // Own the mapping handle so it (and its view) free via `MappedSection` RAII even on bail.
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle() as *mut c_void),
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

            // Frame-ready event (auto-reset).
            let event = CreateEventW(
                Some(&sa),
                false,
                false,
                &HSTRING::from(event_name(target.target_id)),
            )
            .context("CreateEvent(IDD-push)")?;
            let event = OwnedHandle::from_raw_handle(event.0 as _);

            // Ring of shared keyed-mutex textures, format matched to the display's current mode.
            let slots =
                Self::create_ring_slots(&device, target.target_id, generation, w, h, ring_fmt)?;

            // Bring-up debug block (fixed name) — the driver writes diagnostics here. Best-effort.
            let dbg_bytes = std::mem::size_of::<DebugBlock>();
            let (dbg_section, dbg_block) = match CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                dbg_bytes as u32,
                &HSTRING::from(DBG_NAME),
            ) {
                Ok(dm) => {
                    // Own the mapping handle so it (and its view) free via `MappedSection` RAII.
                    let dm = OwnedHandle::from_raw_handle(dm.0 as _);
                    let dv = MapViewOfFile(
                        HANDLE(dm.as_raw_handle() as *mut c_void),
                        FILE_MAP_ALL_ACCESS,
                        0,
                        0,
                        dbg_bytes,
                    );
                    if dv.Value.is_null() {
                        (None, std::ptr::null_mut()) // `dm` drops → mapping closed
                    } else {
                        let section = MappedSection {
                            handle: dm,
                            view: dv,
                        };
                        let p = section.ptr::<DebugBlock>();
                        std::ptr::write_bytes(p.cast::<u8>(), 0, dbg_bytes);
                        (*p).magic = DBG_MAGIC;
                        (Some(section), p)
                    }
                }
                Err(_) => (None, std::ptr::null_mut()),
            };

            // Publish: magic LAST (Release) — signals the driver the ring is ready to open.
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32))
                .store(MAGIC, Ordering::Release);

            tracing::info!(
                target_id = target.target_id,
                render_luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                mode = format!("{w}x{h}"),
                display_hdr,
                client_10bit,
                ring_fp16 = display_hdr,
                "IDD push(host): created shared ring; waiting for the driver to attach + publish"
            );
            let me = Self {
                device,
                context,
                target_id: target.target_id,
                section,
                header,
                event,
                dbg_section,
                dbg_block,
                width: w,
                height: h,
                slots,
                generation,
                client_10bit,
                display_hdr,
                last_acm_poll: Instant::now(),
                recovering_since: None,
                out_ring: Vec::new(),
                out_idx: 0,
                hdr_conv: None,
                last_seq: 0,
                last_present: None,
                status_logged: false,
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
    /// `docs/windows-host-rewrite.md` §2.5 — the GB1 game-capture fix).
    ///
    /// Requiring the first frame — not just the attach — catches the *reconnect-into-a-broken-state* case:
    /// a fullscreen game can leave the virtual display in a format/size that the driver's `publish()` guard
    /// rejects, so the driver ATTACHES but silently drops every frame; without this the host sails past
    /// `open()` and only dies on `next_frame`'s 20 s deadline (the "reconnect = black + audio" symptom). At
    /// session open the OS activates the virtual display → DWM composites it → a frame arrives within ~1 s,
    /// so this does not false-fail a normal (even idle) open; no frame within the window = genuinely broken.
    fn wait_for_attach(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            // Plain read: the driver writes this u32; an aligned u32 read can't tear (same access as
            // log_driver_status_once).
            let st = unsafe { (*self.header).driver_status };
            if matches!(st, DRV_STATUS_TEX_FAIL | DRV_STATUS_NO_DEVICE1) {
                let detail = unsafe { (*self.header).driver_status_detail };
                bail!(
                    "IDD-push driver failed to attach (driver_status={st} detail=0x{detail:08x} — \
                     render-adapter mismatch?)"
                );
            }
            // Attached AND a frame has been published — the publish token's seq advances past 0.
            if st == DRV_STATUS_OPENED && frame::FrameToken::unpack(self.latest()).seq != 0 {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "IDD-push: driver_status={st} but no frame published within 4s — the virtual display \
                     is likely in a format/size the ring can't match (fullscreen game?); falling back"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[inline]
    fn latest(&self) -> u64 {
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
            other => tracing::warn!(other, render_luid, "IDD push: driver reported an unknown status"),
        }
    }

    /// Log the driver's bring-up diagnostics (the fixed-name debug block) — independent of the
    /// per-target header, so it tells us whether the swap-chain processor ran, what target_id it
    /// resolved, whether the header opened (+ error), and whether frames flowed.
    fn log_debug_block(&self) {
        if self.dbg_block.is_null() {
            tracing::warn!("IDD push DEBUG: no debug block");
            return;
        }
        let d = unsafe { &*self.dbg_block };
        tracing::error!(
            run_core_entries = d.run_core_entries,
            resolved_target_id = d.resolved_target_id,
            header_open_attempts = d.header_open_attempts,
            last_open_error = format!("0x{:08x}", d.last_open_error),
            header_opened = d.header_opened,
            driver_render_luid = format!("{:08x}:{:08x}", d.render_luid_high, d.render_luid_low),
            frames_acquired = d.frames_acquired,
            "IDD push DEBUG: driver-reported diagnostics (run_core_entries=0 ⇒ swap-chain processor \
             never ran; resolved_target_id≠ours ⇒ name mismatch; last_open_error 0x80070002 ⇒ header \
             not found; frames_acquired=0 ⇒ idle display)"
        );
    }

    /// The output texture format + the [`PixelFormat`] it presents as, driven SOLELY by the DISPLAY's
    /// HDR state (like the WGC path): HDR → `Rgb10a2` BT.2020 PQ → NVENC Main10, and the client
    /// auto-detects PQ from the HEVC VUI; SDR → 8-bit `Bgra`. We do NOT gate HDR on the client's
    /// advertised `VIDEO_CAP_10BIT` — clients under-report it (e.g. the Mac advertises 10-bit only when
    /// its OWN display is HDR), yet all decode Main10 + auto-switch, exactly as on the WGC path.
    fn out_format(&self) -> (DXGI_FORMAT, PixelFormat) {
        if self.display_hdr {
            (DXGI_FORMAT_R10G10B10A2_UNORM, PixelFormat::Rgb10a2)
        } else {
            (DXGI_FORMAT_B8G8R8A8_UNORM, PixelFormat::Bgra)
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
    /// generation so the driver re-attaches ([`is_stale`]) to the new-format textures; clears the
    /// header's `latest` so we don't consume a stale slot from the old ring; drops the conversion
    /// textures so they rebuild at the new format.
    fn recreate_ring(&mut self, new_display_hdr: bool, new_w: u32, new_h: u32) -> Result<()> {
        self.display_hdr = new_display_hdr;
        self.width = new_w;
        self.height = new_h;
        let fmt = self.ring_format();
        let new_gen = IDD_GENERATION.fetch_add(1, Ordering::Relaxed);
        let new_slots = unsafe {
            Self::create_ring_slots(
                &self.device,
                self.target_id,
                new_gen,
                self.width,
                self.height,
                fmt,
            )?
        };
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
        self.last_seq = 0;
        self.out_ring.clear(); // the output format changed → rebuild lazily at the new format
        self.out_idx = 0;
        self.last_present = None;
        Ok(())
    }

    /// Throttled poll of the display's live HDR state; recreate the ring if the user flipped "Use HDR".
    /// Called from the capture loop (incl. while frozen on a format mismatch) so a toggle recovers within
    /// a poll interval.
    fn poll_display_hdr(&mut self) {
        if self.last_acm_poll.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_acm_poll = Instant::now();
        let now_hdr = unsafe { crate::win_display::advanced_color_enabled(self.target_id) };
        // Follow the display's ACTUAL resolution too — a fullscreen game can mode-set the virtual display
        // out from under the negotiated size (game-capture bug GB1). Unknown read → keep our current size.
        let (now_w, now_h) = unsafe { crate::win_display::active_resolution(self.target_id) }
            .unwrap_or((self.width, self.height));
        if now_hdr == self.display_hdr && now_w == self.width && now_h == self.height {
            return;
        }
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{} hdr={}", self.width, self.height, self.display_hdr),
            to = format!("{now_w}x{now_h} hdr={now_hdr}"),
            "IDD push: display descriptor changed — recreating the ring at the new mode"
        );
        // Start the recovery clock (if not already running): if a fresh frame doesn't resume within the
        // window, try_consume drops the session rather than freeze.
        self.recovering_since.get_or_insert_with(Instant::now);
        if let Err(e) = self.recreate_ring(now_hdr, now_w, now_h) {
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
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        for _ in 0..OUT_RING {
            let mut t: Option<ID3D11Texture2D> = None;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D(IDD out ring)")?;
                let t = t.context("null out-ring texture")?;
                self.device
                    .CreateRenderTargetView(&t, None, Some(&mut rtv))
                    .context("CreateRenderTargetView(IDD out ring)")?;
                self.out_ring.push((t, rtv.context("null out-ring rtv")?));
            }
        }
        Ok(())
    }

    /// Build the HDR converter if not already built (HDR-display path only — an SDR display is a copy).
    fn ensure_converter(&mut self) -> Result<()> {
        if self.hdr_conv.is_none() {
            self.hdr_conv = Some(unsafe { HdrConverter::new(&self.device)? });
        }
        Ok(())
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
        self.ensure_out_ring()?;
        // Build the HDR converter BEFORE acquiring the slot so nothing between Acquire and Release can
        // `?`-return and leak the keyed-mutex lock (which would stall the driver on that slot).
        if self.display_hdr {
            self.ensure_converter()?;
        }
        let i = self.out_idx;
        let (out, out_rtv) = {
            let (t, rtv) = &self.out_ring[i];
            (t.clone(), rtv.clone())
        };
        let (_, pf) = self.out_format();

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
            // SAFETY: convert/copy on the owning (encode) thread's immediate context, holding the slot lock.
            unsafe {
                if self.display_hdr {
                    // Sample the FP16 slot's SRV directly (no scratch copy) → BT.2020 PQ Rgb10a2.
                    if let Some(conv) = self.hdr_conv.as_ref() {
                        conv.convert(&self.context, &s.srv, &out_rtv, self.width, self.height);
                    }
                } else {
                    // SDR: the slot is already 8-bit BGRA — one copy into the out-ring (hidden by pipelining).
                    self.context.CopyResource(&out, &s.tex);
                }
            }
            // `_lock` drops here → `ReleaseSync(0)`.
        }
        self.out_idx = (i + 1) % self.out_ring.len();
        self.last_seq = seq;
        self.last_present = Some((out.clone(), pf));
        self.recovering_since = None; // a fresh frame resumed → recovered
        Ok(Some(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: pf,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: out,
                device: self.device.clone(),
            }),
        }))
    }

    fn repeat_last(&mut self) -> Option<CapturedFrame> {
        // Copy the last presented frame into a FRESH rotated out-ring slot so a repeat (static desktop, no
        // new driver frame) never re-hands a slot that may still be encoding under pipeline_depth>1 — the
        // out-ring rotation IS the texture-ownership contract, and repeats must honor it too (audit §5.3).
        // OUT_RING(3) > the max pipeline_depth(2) guarantees the rotated slot is not in flight.
        let (src, pf) = self.last_present.clone()?;
        let i = self.out_idx;
        let dst = self.out_ring.get(i)?.0.clone();
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
            }),
        })
    }
}

/// Diagnostic observer (O3.1): create the IDD-push ring + debug block as the SYSTEM host (LocalSystem
/// — proper privileges, the gamepad pattern) ALONGSIDE the normal WGC path, which provides the
/// presentation trigger. Logs whether the driver's `run_core` ran and pushed frames into a
/// host-created ring — resolving the `run_core=0` ambiguity (a user-created ring may be unwritable by
/// the driver). Gated by `PUNKTFUNK_IDD_PUSH_OBSERVE`; spawns a short-lived sampling thread.
pub fn spawn_observer(target: WinCaptureTarget, preferred: Option<(u32, u32, u32)>) {
    std::thread::spawn(move || {
        let tid = target.target_id;
        tracing::info!(
            target_id = tid,
            "IDD push OBSERVER: creating host ring (LocalSystem) + debug block alongside WGC"
        );
        match IddPushCapturer::open(target, preferred, false, Box::new(())) {
            Ok(mut cap) => {
                let mut frames = 0u32;
                for _ in 0..40 {
                    match cap.try_consume() {
                        Ok(Some(_)) => frames += 1,
                        Ok(None) => {}
                        Err(e) => tracing::warn!("IDD push OBSERVER: consume error: {e:#}"),
                    }
                    std::thread::sleep(Duration::from_millis(750));
                }
                tracing::info!(
                    target_id = tid,
                    frames_from_ring = frames,
                    "IDD push OBSERVER: sampling done"
                );
                cap.log_debug_block();
            }
            Err((e, _keep)) => tracing::warn!(
                target_id = tid,
                "IDD push OBSERVER: ring open failed: {e:#}"
            ),
        }
    });
}

/// The discrete render GPU LUID (where NVENC runs), falling back to the monitor's `OsAdapterLuid`.
fn resolve_render_adapter_luid_or(fallback_packed: i64) -> LUID {
    if let Some(l) = unsafe { crate::win_adapter::resolve_render_adapter_luid() } {
        return l;
    }
    LUID {
        LowPart: (fallback_packed & 0xffff_ffff) as u32,
        HighPart: (fallback_packed >> 32) as i32,
    }
}

impl Capturer for IddPushCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let _ = unsafe {
                WaitForSingleObject(HANDLE(self.event.as_raw_handle() as *mut c_void), 16)
            };
            if let Some(f) = self.try_consume()? {
                return Ok(f);
            }
            if let Some(f) = self.repeat_last() {
                return Ok(f);
            }
            if Instant::now() > deadline {
                self.log_debug_block();
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

    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        // While the display is HDR we emit BT.2020 PQ (Rgb10a2) → the encoder forces HEVC Main10 + the
        // PQ VUI; pair that with a mastering-display SEI so any decoder tone-maps from a real grade. The
        // driver doesn't (yet) forward the OS's IDDCX_HDR10_METADATA, so use the generic HDR10 baseline
        // (the same metadata the native HDR path sends on the 0xCE datagram).
        self.display_hdr.then(crate::hdr::generic_hdr10)
    }

    fn pipeline_depth(&self) -> usize {
        // 2 = one frame deferred: submit N+1 (capture + convert/copy into a fresh out-ring texture) while
        // NVENC encodes N on the ASIC. We hand a rotating `OUT_RING` of output textures, so this is safe.
        // `PUNKTFUNK_IDD_DEPTH` overrides (1 disables pipelining; clamp to ≤ OUT_RING so a frame in flight
        // always has its own texture).
        crate::config::config().idd_depth.clamp(1, OUT_RING)
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
        self.slots.clear();
        // The shared header + debug sections (`MappedSection`) and the frame-ready `event`
        // (`OwnedHandle`) free themselves via RAII (each unmaps its view, then closes its handle).
        // _keepalive drops after, REMOVEing the virtual display.
    }
}
