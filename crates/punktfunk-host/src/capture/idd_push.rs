//! P2 direct frame push (kill DDA) — HOST side. The pf-vdisplay driver runs in a restricted WUDFHost
//! token that canNOT create named kernel objects, so — exactly like the gamepad UMDF drivers
//! (`inject/dualsense_windows.rs`) — the HOST (privileged) CREATES the shared header + frame-ready
//! event + ring of keyed-mutex textures (`Global\` names, permissive `D:(A;;GA;;;WD)` SDDL) on the
//! discrete render GPU, and the driver only OPENS them and copies frames in. We then consume the ring
//! straight into the zero-copy NVENC path — no DXGI Desktop Duplication, no `win32u` hook. Gated by
//! `PUNKTFUNK_IDD_PUSH`. Driver counterpart: `packaging/windows/vdisplay-driver/pf-vdisplay/src/
//! frame_transport.rs` — [`SharedHeader`], [`MAGIC`], [`RING_LEN`], the status codes and the `Global\`
//! name scheme are DUPLICATED byte-identically there.

use super::dxgi::{make_device, D3d11Frame, HdrConverter, WinCaptureTarget};
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LUID};
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

// --- kept byte-identical with the driver (frame_transport.rs) ---
pub const MAGIC: u32 = 0x4456_4650;
pub const VERSION: u32 = 1;
/// Ring slots — MUST equal the driver's `RING_LEN` (frame_transport.rs). 6 (was 3) gives ample headroom
/// so the driver's 0 ms-timeout publish always finds a free slot while the host briefly holds one across
/// the convert/copy into its output ring and the depth-2 pipelined encode runs on the rest.
pub const RING_LEN: u32 = 6;
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;

// driver_status codes (the driver writes these; we read+log them).
const DRV_STATUS_OPENED: u32 = 1;
const DRV_STATUS_TEX_FAIL: u32 = 2;
const DRV_STATUS_NO_DEVICE1: u32 = 3;

/// Host-owned output-ring depth: distinct NVENC-input textures rotated per frame so the in-flight
/// encode of frame N and the convert/copy of frame N+1 never touch the same texture. 3 covers a
/// pipeline depth of 2 with one slot of margin.
const OUT_RING: usize = 3;

#[repr(C)]
struct SharedHeader {
    magic: u32,
    version: u32,
    generation: u32,
    ring_len: u32,
    width: u32,
    height: u32,
    dxgi_format: u32,
    _pad: u32,
    latest: u64,
    qpc_pts: u64,
    driver_render_luid_low: u32,
    driver_render_luid_high: i32,
    driver_status: u32,
    driver_status_detail: u32,
}

/// Bring-up debug block (fixed name) — the host creates it; the driver writes diagnostics into it
/// independent of the per-target header. Byte-identical with the driver's `DebugBlock`.
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

fn hdr_name(target_id: u32) -> String {
    format!("Global\\pfvd-hdr-{target_id}")
}
fn evt_name(target_id: u32) -> String {
    format!("Global\\pfvd-evt-{target_id}")
}
fn tex_name(target_id: u32, generation: u32, slot: u32) -> String {
    format!("Global\\pfvd-tex-{target_id}-{generation}-{slot}")
}
// ----------------------------------------------------------------

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

struct HostSlot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    shared: HANDLE,
    /// SRV on the slot texture so the HDR path samples the FP16 slot DIRECTLY (no slot→scratch copy);
    /// the convert pass writes the output ring while holding the slot's keyed mutex. Unused for SDR
    /// (which CopyResource's the BGRA slot straight to the output).
    srv: ID3D11ShaderResourceView,
}

impl Drop for HostSlot {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.shared);
        }
    }
}

/// Creates + owns the shared ring; yields the driver's frames as [`FramePayload::D3d11`].
pub struct IddPushCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    target_id: u32,
    map: HANDLE,
    header: *mut SharedHeader,
    event: HANDLE,
    dbg_map: HANDLE,
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
    /// The monitor generation this capturer was opened for. When the active monitor gen changes (a
    /// reconnect preempted + recreated the monitor), `next_frame` bails immediately so this session
    /// releases its NVENC encoder instead of lingering on the dead ring's 20s deadline.
    my_gen: u64,
    _keepalive: Box<dyn Send>,
}
// COM objects used only from the owning (encode) thread.
unsafe impl Send for IddPushCapturer {}

/// The persistent IDD-push capturer, kept alive for the host lifetime and SHARED across client
/// sessions. The driver's per-session monitor TEARDOWN→RECREATE path is unstable (on session 2 the
/// target-id resolves to 0, `IddCxSwapChainSetDevice` fails `0x80070057`, then an access violation),
/// while the FIRST-session path is solid. So we create the monitor + ring + swap-chain ONCE and hand
/// every later session a thin handle delegating to this one. The persistent capturer holds a monitor
/// lease for the host lifetime, so `VirtualDisplay::create` always JOINs the same live monitor (same
/// target id) and the reuse match always hits — no recreate, no driver crash. Prototype scope:
/// single-client, single-mode (a different mode would need a recreate, the unstable path).
static IDD_PERSIST: Mutex<Option<IddPushCapturer>> = Mutex::new(None);

/// Open the IDD-push capturer, reusing the persistent one across sessions (see [`IDD_PERSIST`]).
pub fn open_or_reuse(
    target: WinCaptureTarget,
    preferred: Option<(u32, u32, u32)>,
    client_10bit: bool,
    keepalive: Box<dyn Send>,
) -> Result<Box<dyn Capturer>> {
    let (w, h, _) =
        preferred.context("IDD push needs the negotiated mode (WxH) to size the ring")?;
    let mut slot = IDD_PERSIST.lock().unwrap();
    let reuse = matches!(slot.as_ref(), Some(c) if c.target_id == target.target_id && c.width == w && c.height == h);
    match slot.as_mut() {
        Some(c) if reuse => {
            // Reuse: the persistent capturer already owns the monitor + ring + driver attach. Drop the
            // new per-session monitor lease (the persistent capturer's lease keeps the monitor live).
            // The ring tracks the display, not the client; only the client's 10-bit cap can differ.
            drop(keepalive);
            c.set_client_10bit(client_10bit);
            tracing::info!(
                target_id = target.target_id,
                client_10bit,
                "IDD push: reusing the persistent capturer (no monitor/ring recreate)"
            );
        }
        Some(c) => bail!(
            "IDD-push persistent capturer is {}x{} target {}, this session wants {}x{} target {} — a \
             mode/target change needs a recreate (the driver's recreate path is unstable); not \
             supported in the persistent prototype",
            c.width,
            c.height,
            c.target_id,
            w,
            h,
            target.target_id
        ),
        None => {
            tracing::info!(
                target_id = target.target_id,
                client_10bit,
                "IDD push: creating the persistent capturer (first session)"
            );
            *slot = Some(IddPushCapturer::open(target, preferred, client_10bit, keepalive)?);
        }
    }
    Ok(Box::new(IddReuseHandle))
}

/// Thin per-session handle: every method delegates to the single persistent [`IddPushCapturer`].
/// Dropping it (session end) does NOT tear down the ring/monitor — that's the whole point.
struct IddReuseHandle;
impl Capturer for IddReuseHandle {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        IDD_PERSIST
            .lock()
            .unwrap()
            .as_mut()
            .context("IDD-push persistent capturer missing")?
            .next_frame()
    }
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        IDD_PERSIST
            .lock()
            .unwrap()
            .as_mut()
            .context("IDD-push persistent capturer missing")?
            .try_latest()
    }
    fn set_active(&self, active: bool) {
        if let Some(c) = IDD_PERSIST.lock().unwrap().as_ref() {
            c.set_active(active);
        }
    }
    fn hdr_meta(&self) -> Option<punktfunk_core::quic::HdrMeta> {
        IDD_PERSIST
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.hdr_meta())
    }
}

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
                    &HSTRING::from(tex_name(target_id, generation, k)),
                )
                .context("CreateSharedHandle(IDD-push ring slot)")?;
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

    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        client_10bit: bool,
        keepalive: Box<dyn Send>,
    ) -> Result<Self> {
        let (w, h, _hz) = preferred
            .context("IDD push needs the negotiated mode (WxH) to size the shared ring")?;
        // The driver composes the virtual display in FP16 (R16G16B16A16_FLOAT scRGB) when the display is
        // in advanced-color (HDR) mode, and 8-bit BGRA otherwise (per swap_chain_processor.rs + the
        // COMMIT_MODES2 colorspace/rgb_bpc log). The user can flip "Use HDR" in Windows at any time, so
        // the ring format must TRACK the display's ACTUAL mode (the driver's format-guard drops a
        // mismatch). We poll the live state here and on every recreate. For a 10-bit-capable client we
        // PROACTIVELY enable advanced color so HDR streams without the user toggling anything; an
        // SDR-only client leaves the display alone (and still gets a tone-mapped picture, never a freeze,
        // if the user does enable HDR).
        unsafe {
            if client_10bit && crate::vdisplay::sudovda::set_advanced_color(target.target_id, true)
            {
                // Let the colorspace change settle before the driver composes + we size the ring.
                std::thread::sleep(Duration::from_millis(250));
            }
            let display_hdr = crate::vdisplay::sudovda::advanced_color_enabled(target.target_id);
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
                &HSTRING::from(hdr_name(target.target_id)),
            )
            .context("CreateFileMapping(IDD-push header)")?;
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, bytes);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                bail!("MapViewOfFile failed for IDD-push header");
            }
            let generation = IDD_GENERATION.fetch_add(1, Ordering::Relaxed);
            let header = view.Value.cast::<SharedHeader>();
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
                &HSTRING::from(evt_name(target.target_id)),
            )
            .context("CreateEvent(IDD-push)")?;

            // Ring of shared keyed-mutex textures, format matched to the display's current mode.
            let slots =
                Self::create_ring_slots(&device, target.target_id, generation, w, h, ring_fmt)?;

            // Bring-up debug block (fixed name) — the driver writes diagnostics here. Best-effort.
            let dbg_bytes = std::mem::size_of::<DebugBlock>();
            let (dbg_map, dbg_block) = match CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                dbg_bytes as u32,
                &HSTRING::from(DBG_NAME),
            ) {
                Ok(dm) => {
                    let dv = MapViewOfFile(dm, FILE_MAP_ALL_ACCESS, 0, 0, dbg_bytes);
                    if dv.Value.is_null() {
                        let _ = CloseHandle(dm);
                        (HANDLE::default(), std::ptr::null_mut())
                    } else {
                        let p = dv.Value.cast::<DebugBlock>();
                        std::ptr::write_bytes(p.cast::<u8>(), 0, dbg_bytes);
                        (*p).magic = DBG_MAGIC;
                        (dm, p)
                    }
                }
                Err(_) => (HANDLE::default(), std::ptr::null_mut()),
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
            Ok(Self {
                device,
                context,
                target_id: target.target_id,
                map,
                header,
                event,
                dbg_map,
                dbg_block,
                width: w,
                height: h,
                slots,
                generation,
                client_10bit,
                display_hdr,
                last_acm_poll: Instant::now(),
                out_ring: Vec::new(),
                out_idx: 0,
                hdr_conv: None,
                last_seq: 0,
                last_present: None,
                status_logged: false,
                my_gen: crate::vdisplay::sudovda::CURRENT_MON_GEN.load(Ordering::Relaxed),
                _keepalive: keepalive,
            })
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

    /// Update the client's 10-bit capability (the reuse path). Only affects whether a fresh `open`
    /// proactively enables advanced color; the per-frame conversion follows the display, not the client.
    fn set_client_10bit(&mut self, client_10bit: bool) {
        self.client_10bit = client_10bit;
    }

    /// Recreate the ring at the format for `new_display_hdr` (the user flipped "Use HDR"). Bumps the
    /// generation so the driver re-attaches ([`is_stale`]) to the new-format textures; clears the
    /// header's `latest` so we don't consume a stale slot from the old ring; drops the conversion
    /// textures so they rebuild at the new format.
    fn recreate_ring(&mut self, new_display_hdr: bool) -> Result<()> {
        self.display_hdr = new_display_hdr;
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
        let now_hdr = unsafe { crate::vdisplay::sudovda::advanced_color_enabled(self.target_id) };
        if now_hdr == self.display_hdr {
            return;
        }
        tracing::info!(
            target_id = self.target_id,
            display_hdr = now_hdr,
            client_10bit = self.client_10bit,
            "IDD push: display HDR mode flipped — recreating the ring at the new format"
        );
        if let Err(e) = self.recreate_ring(now_hdr) {
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
        let latest = self.latest();
        // `latest` = (generation << 40) | (seq << 8) | slot. Reject any publish whose generation isn't
        // our CURRENT ring (a stale old-ring publish racing a recreate, or the 0 sentinel we reset to) so
        // we never consume an unwritten new-ring slot — eliminating the toggle-time garbage frame.
        if (latest >> 40) as u32 != self.generation {
            return Ok(None);
        }
        let seq = (latest >> 8) & 0xFFFF_FFFF;
        let slot = (latest & 0xff) as usize;
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
        if unsafe { s.mutex.AcquireSync(0, 8) }.is_err() {
            return Ok(None);
        }
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
            let _ = s.mutex.ReleaseSync(0);
        }
        self.out_idx = (i + 1) % self.out_ring.len();
        self.last_seq = seq;
        self.last_present = Some((out.clone(), pf));
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

    fn repeat_last(&self) -> Option<CapturedFrame> {
        self.last_present.as_ref().map(|(tex, pf)| CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: *pf,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: tex.clone(),
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
            Err(e) => tracing::warn!(
                target_id = tid,
                "IDD push OBSERVER: ring open failed: {e:#}"
            ),
        }
    });
}

/// The discrete render GPU LUID (where NVENC runs), falling back to the monitor's `OsAdapterLuid`.
fn resolve_render_adapter_luid_or(fallback_packed: i64) -> LUID {
    if let Some(l) = unsafe { crate::vdisplay::sudovda::resolve_render_adapter_luid() } {
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
            let _ = unsafe { WaitForSingleObject(self.event, 16) };
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
        std::env::var("PUNKTFUNK_IDD_DEPTH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(2)
            .clamp(1, OUT_RING)
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
        self.slots.clear();
        unsafe {
            if !self.dbg_block.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.dbg_block.cast(),
                });
            }
            if !self.dbg_map.is_invalid() {
                let _ = CloseHandle(self.dbg_map);
            }
            if !self.header.is_null() {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.header.cast(),
                });
            }
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
        }
        // _keepalive drops after, REMOVEing the virtual display.
    }
}
