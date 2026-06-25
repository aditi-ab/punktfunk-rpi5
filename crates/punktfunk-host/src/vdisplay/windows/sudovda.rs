//! Windows virtual-display backend driving **SudoVDA** (the SudoMaker Virtual Display Adapter —
//! the Indirect Display Driver the Apollo Sunshine-fork ships). The Windows analogue of the
//! Linux per-compositor backends: [`create`](VirtualDisplay::create) adds a virtual monitor at the
//! client's exact `WxH@Hz` (the mode is baked into the ADD IOCTL — no EDID seeding), starts the
//! mandatory watchdog ping, and the returned [`VirtualOutput`]'s keepalive `Drop` removes it (RAII).
//!
//! Control surface (verified live against SudoVDA 0.2.1): a device-interface-GUID + `CreateFileW`
//! + `DeviceIoControl` IOCTL protocol. No DLL, no named pipe. See `docs/windows-host.md`.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

/// Monotonic monitor generation. Each [`create_monitor`] stamps the next value onto the [`Monitor`]
/// and its [`MonitorLease`]s, so a lease whose monitor was already torn down + recreated (the IDD-push
/// reconnect-preempt path) is ignored on drop instead of decrementing the NEW monitor's refcount.
// pub(crate) so vdisplay::pf_vdisplay can reuse this shared generation counter (one counter across both
// backends keeps the idd_push stale-ring bail working regardless of which backend is active).
pub(crate) static MON_GEN: AtomicU64 = AtomicU64::new(1);

/// IDD-push mode: a new client connection preempts + recreates the monitor (single-client reconnect),
/// because a REUSED IddCx monitor's swap-chain is dead. Off → monitors are shared across sessions.
fn idd_push_mode() -> bool {
    crate::config::config().idd_push
}
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
// (CCD `Devices::Display` + `Graphics::Gdi` imports moved with the display helpers to `win_display`.)
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use super::{Mode, VirtualDisplay, VirtualOutput};

// SudoVDA device-interface GUID (Common/Include/sudovda-ioctl.h).
const SUVDA_INTERFACE: GUID = GUID::from_u128(0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D);

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0).
const fn ctl(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}
const IOCTL_ADD: u32 = ctl(0x800);
const IOCTL_REMOVE: u32 = ctl(0x801);
const IOCTL_SET_RENDER_ADAPTER: u32 = ctl(0x802); // == 0x0022_2008
const IOCTL_GET_WATCHDOG: u32 = ctl(0x803);
/// pf-vdisplay extension (NOT in SudoVDA): tear down every virtual monitor. Sent once on host startup
/// to reap monitors orphaned by a crashed/killed previous host. SudoVDA returns invalid (ignored).
const IOCTL_CLEAR_ALL: u32 = ctl(0x804);
const IOCTL_DRIVER_PING: u32 = ctl(0x888);
const IOCTL_GET_VERSION: u32 = ctl(0x8FF);

/// A UNIQUE-per-session SudoVDA monitor GUID. The monitor is keyed by GUID for IOCTL_ADD/REMOVE, so a
/// FIXED GUID makes overlapping sessions (a client reconnecting after a freeze before the old session
/// has torn down, or genuine concurrent sessions) all map to the SAME monitor — then one session's
/// IOCTL_REMOVE on teardown tears the monitor down OUT FROM UNDER a still-live session ("display
/// disconnected" sound + freeze, even with no context change — observed live). Make it unique per
/// (process, session): base GUID with the low 48-bit node = (pid << 16 | session#).
fn next_monitor_guid() -> GUID {
    use std::sync::atomic::AtomicU32;
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id() as u128;
    GUID::from_u128(0x70756E6B_7466_756E_6B30_000000000000u128 | (pid << 16) | (n & 0xFFFF))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddParams {
    width: u32,
    height: u32,
    refresh: u32,
    guid: GUID,
    device_name: [u8; 14],
    serial: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AddOut {
    luid: LUID,
    target_id: u32,
}

// SET_RENDER_ADAPTER input — byte-identical to SudoVDA's `{ LUID AdapterLuid; }` (8 bytes). The
// windows `LUID` is `{ LowPart: u32, HighPart: i32 }` == the C `LUID`, so `#[repr(C)]` is exact.
#[repr(C)]
#[derive(Clone, Copy)]
struct SetRenderAdapterParams {
    luid: LUID,
}

/// Pin the SudoVDA IDD's RENDER GPU to `luid` (Apollo's `SetRenderAdapter`). No output buffer. MUST be
/// issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target renders on — on a
/// multi-adapter box (SudoVDA IDD + a discrete GPU) this stops DXGI from reparenting the virtual
/// output onto a different adapter than the one we duplicate/encode on (the ACCESS_LOST storm).
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let p = SetRenderAdapterParams { luid };
    let bytes = std::slice::from_raw_parts(
        &p as *const _ as *const u8,
        size_of::<SetRenderAdapterParams>(),
    );
    let mut none: [u8; 0] = [];
    ioctl(h, IOCTL_SET_RENDER_ADAPTER, bytes, &mut none)
        .map(|_| ())
        .context("SudoVDA SET_RENDER_ADAPTER")
}

// `resolve_render_adapter_luid` moved to the backend-neutral `crate::win_adapter` (audit §9 / Goal 2:
// it is display-utility, not SudoVDA-specific). Re-exported so this backend's own callers keep the short
// name; external callers (idd_push, pf_vdisplay) use `crate::win_adapter` directly.
pub(crate) use crate::win_adapter::resolve_render_adapter_luid;

#[repr(C)]
struct RemoveParams {
    guid: GUID,
}

/// One `DeviceIoControl` round trip (METHOD_BUFFERED). `input`/`output` may be empty.
unsafe fn ioctl(h: HANDLE, code: u32, input: &[u8], output: &mut [u8]) -> Result<u32> {
    let mut returned = 0u32;
    let inp = (!input.is_empty()).then_some(input.as_ptr() as *const c_void);
    let outp = (!output.is_empty()).then_some(output.as_mut_ptr() as *mut c_void);
    DeviceIoControl(
        h,
        code,
        inp,
        input.len() as u32,
        outp,
        output.len() as u32,
        Some(&mut returned),
        None,
    )
    .with_context(|| format!("DeviceIoControl(code={code:#x})"))?;
    Ok(returned)
}

// The CCD/GDI display helpers (resolve_gdi_name, set_advanced_color, advanced_color_enabled,
// set_active_mode, isolate/restore_displays_ccd) + SavedConfig moved to the backend-neutral
// `crate::win_display` (audit §9 / Goal 2). Re-exported so this backend's own callers keep the short
// names; external callers use `crate::win_display` directly.
pub(crate) use crate::win_display::{
    isolate_displays_ccd, resolve_gdi_name, restore_displays_ccd, set_active_mode, SavedConfig,
};

unsafe fn open_device() -> Result<HANDLE> {
    let hdev = SetupDiGetClassDevsW(
        Some(&SUVDA_INTERFACE),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    )
    .context("SetupDiGetClassDevsW(SudoVDA) — is the SudoVDA driver installed?")?;

    let mut idata = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    SetupDiEnumDeviceInterfaces(hdev, None, &SUVDA_INTERFACE, 0, &mut idata)
        .context("SetupDiEnumDeviceInterfaces(SudoVDA)")?;

    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &idata, None, 0, Some(&mut required), None);
    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    SetupDiGetDeviceInterfaceDetailW(hdev, &idata, Some(detail), required, None, None)
        .context("SetupDiGetDeviceInterfaceDetailW(SudoVDA)")?;

    let handle = CreateFileW(
        PCWSTR((*detail).DevicePath.as_ptr()),
        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(SudoVDA device)")?;
    let _ = SetupDiDestroyDeviceInfoList(hdev);
    Ok(handle)
}

// ── Host-level reference-counted SudoVDA monitor lifecycle ──────────────────────────────────────
//
// The virtual monitor is created on the first session and REUSED across sessions. When the last
// session disconnects the monitor LINGERS for a grace window (PUNKTFUNK_MONITOR_LINGER_MS, default
// 10 s): a reconnect within the window reuses it instantly (no new screen, no PnP connect/disconnect
// chime, no teardown/recreate kernel churn); after the window a background timer REMOVEs it so a
// physical-screen user gets their screen back. Overlapping sessions share one monitor via the
// refcount (teardown only at refs==0 + expired grace), so a stale session can never REMOVE a live
// session's monitor (the earlier collision). The control-device HANDLE is opened once and kept for
// the host lifetime — it's a handle, not a screen, so it creates no phantom display.

/// The resources backing one live SudoVDA monitor (owned by [`MGR`], not by any session).
struct Monitor {
    guid: GUID,
    target_id: u32,
    luid: LUID,
    gdi_name: Option<String>,
    mode: Mode,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    ccd_saved: Option<SavedConfig>,
    /// Generation stamp ([`MON_GEN`]); a [`MonitorLease`] only releases if its gen still matches.
    gen: u64,
}

enum MgrState {
    Idle,
    Active { mon: Monitor, refs: u32 },
    Lingering { mon: Monitor, until: Instant },
}

struct Mgr {
    /// Control-device handle (raw isize; `HANDLE` isn't `Send`). Opened once, kept for the host life.
    device: Option<isize>,
    watchdog_s: u32,
    state: MgrState,
}

static MGR: Mutex<Mgr> = Mutex::new(Mgr {
    device: None,
    watchdog_s: 3,
    state: MgrState::Idle,
});

/// The Windows virtual-display backend. A marker — the monitor lifecycle lives in the global [`MGR`].
pub struct SudoVdaDisplay;

impl SudoVdaDisplay {
    pub fn new() -> Result<Self> {
        // Open the control device once (validates the driver is present) + log version/watchdog.
        let mut g = MGR.lock().unwrap();
        mgr_ensure_device(&mut g)?;
        Ok(Self)
    }
}

impl Drop for SudoVdaDisplay {
    fn drop(&mut self) {
        // Nothing: the control device + monitor lifecycle are host-level (owned by MGR) and
        // deliberately outlive any single session so a reconnect can reuse the monitor.
    }
}

impl VirtualDisplay for SudoVdaDisplay {
    fn name(&self) -> &'static str {
        "sudovda"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Delegate to the host-level manager: create the monitor, reuse a lingering one on reconnect,
        // or join the live one — and hand back a lease whose Drop releases the refcount.
        mgr_acquire(mode)
    }
}

/// Create a fresh SudoVDA monitor at `mode` on the (host-level) control `device`. The old per-session
/// `create()` body, now owned by the manager: ADD the target, start the watchdog ping, resolve the
/// GDI name, force the client mode + (default) isolate to a sole composited display. Returns the
/// [`Monitor`] resources; the manager tracks its lifecycle (refcount + linger).
unsafe fn create_monitor(device: isize, mode: Mode, watchdog_s: u32) -> Result<Monitor> {
    let dev = HANDLE(device as *mut c_void);
    {
        let mut device_name = [0u8; 14];
        let nm = b"punktfunk";
        device_name[..nm.len()].copy_from_slice(nm);
        // Fresh GUID per created monitor (the manager refcount, not the GUID, prevents the
        // cross-session REMOVE collision now).
        let session_guid = next_monitor_guid();
        let add = AddParams {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_hz,
            guid: session_guid,
            device_name,
            serial: [0u8; 14],
        };
        // SET_RENDER_ADAPTER is OPT-IN. Apollo runs with an EMPTY config and NEVER pins the render
        // adapter, yet captures the SudoVDA cleanly at the client mode on the 4090 (verified live on
        // this exact box: no ACCESS_LOST, no MODE_CHANGE storm). On this box our pin is IGNORED by the
        // driver AND the IDD lands on a DIFFERENT adapter (0x23664) than the one its DXGI output is
        // enumerated under (the 4090, where we make the capture device) — a cross-GPU mismatch that is
        // the real source of the perpetual ACCESS_LOST + MODE_CHANGE_IN_PROGRESS storm. So default to
        // NOT pinning — let the IDD use its natural adapter like Apollo. Opt in with
        // PUNKTFUNK_RENDER_ADAPTER=<name substring> only on a box that genuinely needs steering.
        let pinned = if crate::config::config().render_adapter.is_some() {
            unsafe { resolve_render_adapter_luid() }
        } else if crate::config::config().idd_push {
            // P2 direct frame push: the host opens the driver's shared textures AND runs NVENC on the
            // RENDER adapter, so on a hybrid box (4090 + iGPU) it MUST be the discrete encoder GPU —
            // an iGPU-rendered surface is untouchable by NVENC. pf-vdisplay HONORS SET_RENDER_ADAPTER
            // (SudoVDA ignored it), so pin the discrete GPU. The driver also reports the resulting
            // render LUID in the shared header, so the host binds correctly even if this is overridden.
            tracing::info!("IDD push: pinning the discrete render GPU (SET_RENDER_ADAPTER)");
            unsafe { resolve_render_adapter_luid() }
        } else {
            tracing::info!(
                "SudoVDA SET_RENDER_ADAPTER skipped (Apollo-parity: no render pin — avoids cross-GPU \
                 mismatch; set PUNKTFUNK_RENDER_ADAPTER=<name> to force a specific render GPU)"
            );
            None
        };
        if let Some(luid) = pinned {
            match unsafe { set_render_adapter(dev, luid) } {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                Err(e) => tracing::warn!("SudoVDA SET_RENDER_ADAPTER failed (continuing): {e:#}"),
            }
        }

        let add_bytes = unsafe {
            std::slice::from_raw_parts(&add as *const _ as *const u8, size_of::<AddParams>())
        };
        let mut out = [0u8; size_of::<AddOut>()];
        unsafe { ioctl(dev, IOCTL_ADD, add_bytes, &mut out) }.with_context(|| {
            format!(
                "SudoVDA ADD {}x{}@{}",
                mode.width, mode.height, mode.refresh_hz
            )
        })?;
        let ao = unsafe { *(out.as_ptr() as *const AddOut) };
        tracing::info!(
            "SudoVDA created {}x{}@{} (target_id={}, adapter_luid={:#x})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            ao.target_id,
            ao.luid.LowPart
        );
        if let Some(luid) = pinned {
            if ao.luid.LowPart == luid.LowPart && ao.luid.HighPart == luid.HighPart {
                tracing::info!("SudoVDA ADD render adapter matches the pinned GPU (pin took)");
            } else {
                tracing::warn!(
                    add = format!("{:08x}:{:08x}", ao.luid.HighPart, ao.luid.LowPart),
                    pinned = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "SudoVDA ADD render adapter DIFFERS from pinned — driver ignored SET_RENDER_ADAPTER?"
                );
            }
        }

        // Mandatory keepalive: ping inside the watchdog window or the driver tears all displays down.
        let stop = Arc::new(AtomicBool::new(false));
        let device_raw = device;
        let interval = Duration::from_millis(watchdog_s as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let pinger = thread::spawn(move || {
            let h = HANDLE(device_raw as *mut c_void);
            let mut warned = false;
            while !stop_t.load(Ordering::Relaxed) {
                let mut none: [u8; 0] = [];
                match unsafe { ioctl(h, IOCTL_DRIVER_PING, &[], &mut none) } {
                    Ok(_) => warned = false,
                    // A persistently failing PING means the cached control handle went invalid — the
                    // driver watchdog will then tear the monitor down mid-session. Surface it once
                    // (the old `let _ =` swallowed it, which masked exactly this during the bad-state churn).
                    Err(e) => {
                        if !warned {
                            tracing::warn!(
                                "SudoVDA keepalive PING failed (control handle lost?): {e:#}"
                            );
                            warned = true;
                        }
                    }
                }
                thread::sleep(interval);
            }
        });

        // Resolve the capture target. May be None on a GPU-less box (target added but not activated
        // into a WDDM path); the Windows capture backend will re-resolve once a GPU is present.
        let mut gdi_name = None;
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            if let Some(n) = unsafe { resolve_gdi_name(ao.target_id) } {
                gdi_name = Some(n);
                break;
            }
        }
        let mut ccd_saved: Option<SavedConfig> = None;
        match &gdi_name {
            Some(n) => {
                tracing::info!("SudoVDA target {} -> {n}", ao.target_id);
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Make the SudoVDA the SOLE active display (default). On this box an EXTENDED
                // (non-primary) IDD is NOT DWM-composited → Desktop Duplication gets a born-lost
                // ACCESS_LOST (measured live: MODE_CHANGE storm fixed, but the extended IDD then
                // born-lost). Apollo reaches the same end state ("Virtual Desktop: WxH" — the IDD is the
                // whole desktop, hence primary + composited) via Windows AUTO-promoting the real WDDM
                // display over the box's leftover 1024x768 basic display; Windows does NOT auto-promote
                // for us, so we deactivate the other display(s) explicitly via the clean atomic CCD path.
                // Deactivating FIRST means set_active_mode's primary-promotion has nothing to contest →
                // no MODE_CHANGE_IN_PROGRESS storm (that storm came from promoting primary WHILE the
                // basic display stayed active). Opt out with PUNKTFUNK_NO_ISOLATE=1 (a box with a real
                // second monitor to keep live). The legacy GDI detach is skipped — it misses
                // iGPU-attached monitors on a hybrid box and churns per-device; CCD is atomic.
                if std::env::var("PUNKTFUNK_NO_ISOLATE").is_err() {
                    ccd_saved = unsafe { isolate_displays_ccd(ao.target_id) };
                } else {
                    tracing::info!(
                        "display isolation skipped (PUNKTFUNK_NO_ISOLATE) — IDD stays extended"
                    );
                }
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens
            }
            None => tracing::warn!(
                "SudoVDA target {} not yet an active display path (needs a WDDM GPU to activate)",
                ao.target_id
            ),
        }

        Ok(Monitor {
            guid: session_guid,
            target_id: ao.target_id,
            luid: ao.luid,
            gdi_name,
            mode,
            stop,
            pinger: Some(pinger),
            ccd_saved,
            gen: MON_GEN.fetch_add(1, Ordering::Relaxed),
        })
    }
}

impl Monitor {
    /// The capture target handed to a session (`None` until the GDI name resolves).
    fn target(&self) -> Option<crate::capture::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| crate::capture::dxgi::WinCaptureTarget {
                adapter_luid: crate::capture::dxgi::pack_luid(self.luid),
                gdi_name: n,
                // target_id is stable across secure-desktop topology rebuilds; the GDI name is NOT,
                // so capture re-resolves the name from this on every recovery.
                target_id: self.target_id,
            })
    }

    /// Stop the watchdog ping, re-attach the displays we detached, then REMOVE the monitor (by GUID).
    /// `device` is the host-level control handle. Consumes the monitor.
    unsafe fn teardown(mut self, device: isize) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.pinger.take() {
            let _ = j.join();
        }
        // Re-attach detached display(s) BEFORE the REMOVE so the box is never left with zero displays.
        if let Some(saved) = &self.ccd_saved {
            restore_displays_ccd(saved);
        }
        let rp = RemoveParams { guid: self.guid };
        let rp_bytes =
            std::slice::from_raw_parts(&rp as *const _ as *const u8, size_of::<RemoveParams>());
        let mut none: [u8; 0] = [];
        let h = HANDLE(device as *mut c_void);
        if let Err(e) = ioctl(h, IOCTL_REMOVE, rp_bytes, &mut none) {
            tracing::warn!("SudoVDA REMOVE failed: {e:#}");
        } else {
            tracing::info!("SudoVDA monitor removed");
        }
    }
}

/// Open the control device once + read version/watchdog; cache the handle (raw isize) in `g`.
fn mgr_ensure_device(g: &mut Mgr) -> Result<isize> {
    if let Some(d) = g.device {
        return Ok(d);
    }
    let device = unsafe { open_device()? };
    let mut ver = [0u8; 4];
    if unsafe { ioctl(device, IOCTL_GET_VERSION, &[], &mut ver) }.is_ok() {
        tracing::info!(
            "SudoVDA protocol {}.{}.{} (test={})",
            ver[0],
            ver[1],
            ver[2],
            ver[3]
        );
    }
    let mut wd = [0u8; 8];
    g.watchdog_s = if unsafe { ioctl(device, IOCTL_GET_WATCHDOG, &[], &mut wd) }.is_ok() {
        u32::from_le_bytes([wd[0], wd[1], wd[2], wd[3]]).max(1)
    } else {
        3
    };
    tracing::info!("SudoVDA watchdog timeout {}s", g.watchdog_s);
    // Reap monitors orphaned by a crashed/killed previous host instance before we create ours.
    // pf-vdisplay honors IOCTL_CLEAR_ALL; SudoVDA returns invalid (ignored). Without it an orphan
    // lingers until the driver watchdog fires — but a still-pinging new session keeps resetting that
    // watchdog, so orphans could accumulate (the "5-6 stale monitors that never tear down" failure).
    {
        let mut none: [u8; 0] = [];
        if unsafe { ioctl(device, IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        }
    }
    let raw = device.0 as isize;
    g.device = Some(raw);
    Ok(raw)
}

/// Linger window before a session-less monitor is torn down. A reconnect within it reuses the
/// monitor (no new screen / PnP chime); after it the monitor is REMOVEd so a physical screen returns.
fn linger_ms() -> u64 {
    std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Acquire the shared monitor for a new session: join the live one (refcount++), reuse a lingering
/// one (reconfiguring if the client mode changed), or create one. The returned [`MonitorLease`]
/// releases the refcount on drop.
fn mgr_acquire(mode: Mode) -> Result<VirtualOutput> {
    ensure_linger_timer();
    let mut g = MGR.lock().unwrap();
    let device = mgr_ensure_device(&mut g)?;
    let watchdog_s = g.watchdog_s;

    // IDD-push: a new connection while a monitor is live = a single-client RECONNECT (the prior client
    // is gone — IDD-push is one display, no concurrency). A REUSED IddCx monitor's swap-chain is DEAD,
    // so joining it would hand the new client a black screen until the old session times out. PREEMPT:
    // tear the old monitor down (its Drop restores topology + IOCTL_REMOVEs) and fall through to create
    // a FRESH one. The old session's lease is gen-stamped, so its later drop is ignored (mgr_release
    // no-op) and can't tear down the new monitor.
    if idd_push_mode()
        && matches!(
            g.state,
            MgrState::Active { .. } | MgrState::Lingering { .. }
        )
    {
        if let MgrState::Active { mon, .. } | MgrState::Lingering { mon, .. } =
            std::mem::replace(&mut g.state, MgrState::Idle)
        {
            tracing::info!(
                old_target = mon.target_id,
                "IDD-push reconnect — preempting the prior session, recreating a fresh monitor"
            );
            // teardown() — NOT drop() — sends IOCTL_REMOVE (and restores topology). `Monitor` has NO
            // `Drop` impl, so a bare `drop(mon)` orphaned the IddCx monitor in the driver: it was never
            // departed, so it kept a live D3D device + a stuck swap-chain processor thread, and these
            // accumulated every reconnect (the driver-side churn leak: +1 device, ~36 nvwgf2umx threads,
            // ~50 MB VRAM per session, until it choked). teardown frees it via the driver's do_remove.
            unsafe { mon.teardown(device) };
            // Let the OS finish the ASYNC IddCx monitor departure before the next ADD. A back-to-back
            // REMOVE→ADD races the teardown and the ADD IOCTL is rejected (`DeviceIoControl failed`)
            // under reconnect churn. Held under the MGR lock, but IDD-push setup is already serialized
            // (IDD_SETUP_LOCK), so this only paces the recreate — exactly what a reconnect flood needs.
            thread::sleep(Duration::from_millis(400));
        }
    }

    // A live monitor already exists — join it (refcount++). This covers a concurrent session AND the
    // build-then-drop overlap of a mid-stream Reconfigure / secure-return (the new lease is taken while
    // the old is still held). If the requested mode differs, reconfigure the shared monitor to it so a
    // Reconfigure actually applies (one shared monitor → sessions necessarily share a mode).
    if let MgrState::Active { mon, refs } = &mut g.state {
        *refs += 1;
        let changed = mon.mode.width != mode.width
            || mon.mode.height != mode.height
            || mon.mode.refresh_hz != mode.refresh_hz;
        if changed {
            unsafe { mgr_reconfigure(mon, mode) };
        }
        tracing::info!(
            refs = *refs,
            "SudoVDA monitor reused (concurrent / reconfigure session)"
        );
        let pm = Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz));
        let target = mon.target();
        let gen = mon.gen;
        return Ok(VirtualOutput {
            node_id: 0,
            preferred_mode: pm,
            win_capture: target,
            keepalive: Box::new(MonitorLease { gen }),
        });
    }

    // Idle or Lingering: repurpose/create a monitor → Active{refs:1}.
    let mon = match std::mem::replace(&mut g.state, MgrState::Idle) {
        MgrState::Lingering { mut mon, .. } => {
            tracing::info!("SudoVDA monitor reused (reconnect within the linger window)");
            let changed = mon.mode.width != mode.width
                || mon.mode.height != mode.height
                || mon.mode.refresh_hz != mode.refresh_hz;
            if changed {
                unsafe { mgr_reconfigure(&mut mon, mode) };
            }
            mon
        }
        MgrState::Idle => unsafe { create_monitor(device, mode, watchdog_s)? },
        MgrState::Active { .. } => unreachable!("handled above"),
    };
    let pm = Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz));
    let target = mon.target();
    let gen = mon.gen;
    g.state = MgrState::Active { mon, refs: 1 };
    Ok(VirtualOutput {
        node_id: 0,
        preferred_mode: pm,
        win_capture: target,
        keepalive: Box::new(MonitorLease { gen }),
    })
}

/// Re-apply a (possibly new) mode to a reused monitor on reconnect, re-resolving its GDI name.
unsafe fn mgr_reconfigure(mon: &mut Monitor, mode: Mode) {
    tracing::info!(
        old = format!(
            "{}x{}@{}",
            mon.mode.width, mon.mode.height, mon.mode.refresh_hz
        ),
        new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
        "SudoVDA: reconfiguring reused monitor to the new client mode"
    );
    if let Some(n) = resolve_gdi_name(mon.target_id) {
        mon.gdi_name = Some(n);
    }
    if let Some(n) = &mon.gdi_name {
        set_active_mode(n, mode);
    }
    mon.mode = mode;
}

/// Release a session's hold: refcount-- ; when the last session leaves, LINGER before teardown.
/// `gen` is the lease's monitor generation: a STALE lease (its monitor was already torn down +
/// recreated under it — the IDD-push reconnect-preempt path) does nothing, so it can't decrement the
/// CURRENT (fresh) monitor's refcount and tear it down.
fn mgr_release(gen: u64) {
    let mut g = MGR.lock().unwrap();
    let stale = match &g.state {
        MgrState::Active { mon, .. } | MgrState::Lingering { mon, .. } => mon.gen != gen,
        MgrState::Idle => true,
    };
    if stale {
        return;
    }
    g.state = match std::mem::replace(&mut g.state, MgrState::Idle) {
        MgrState::Active { mon, refs } if refs > 1 => MgrState::Active {
            mon,
            refs: refs - 1,
        },
        MgrState::Active { mon, .. } => {
            let ms = linger_ms();
            tracing::info!(
                linger_ms = ms,
                "SudoVDA: last session left — lingering before teardown"
            );
            MgrState::Lingering {
                mon,
                until: Instant::now() + Duration::from_millis(ms),
            }
        }
        other => other,
    };
}

/// Wait (up to `timeout`) for the active monitor to be RELEASED — i.e. the MGR is no longer `Active`
/// (the prior session dropped its lease → `Lingering`/`Idle`). Used by the IDD-push reconnect preempt:
/// after signalling the old session to stop, we wait here so it tears its monitor down CLEANLY (while
/// frames still flow) before we acquire a fresh one — instead of dropping the monitor out from under a
/// still-live session, which churns the driver's ADD/REMOVE path and wedges it under rapid reconnects.
pub(crate) fn wait_for_monitor_released(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !matches!(MGR.lock().unwrap().state, MgrState::Active { .. }) {
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "IDD-push preempt: prior session didn't release the monitor within {timeout:?} — \
                 proceeding (mgr_acquire will preempt it)"
            );
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Background timer (started once): tear down a monitor that has lingered past its deadline (→ Idle),
/// so a physical-screen user gets their screen back after they stop streaming.
fn ensure_linger_timer() {
    static TIMER: Once = Once::new();
    TIMER.call_once(|| {
        let _ = thread::Builder::new()
            .name("sudovda-linger".into())
            .spawn(|| loop {
                thread::sleep(Duration::from_millis(500));
                let mut g = MGR.lock().unwrap();
                let due = matches!(&g.state, MgrState::Lingering { until, .. } if Instant::now() >= *until);
                if due {
                    let device = g.device.unwrap_or(0);
                    if let MgrState::Lingering { mon, .. } =
                        std::mem::replace(&mut g.state, MgrState::Idle)
                    {
                        drop(g); // release the lock before the REMOVE IOCTL + display restore
                        unsafe { mon.teardown(device) };
                    }
                }
            });
    });
}

/// A session's lease on the shared monitor. Drop releases the refcount (→ linger when it hits 0),
/// UNLESS the monitor was already torn down + recreated under it (gen mismatch — the IDD-push
/// reconnect-preempt path), in which case the drop is a no-op so it can't tear down the new monitor.
struct MonitorLease {
    gen: u64,
}
impl Drop for MonitorLease {
    fn drop(&mut self) {
        mgr_release(self.gen);
    }
}

/// Readiness probe: can we open the SudoVDA control device?
pub fn probe() -> Result<()> {
    let h = unsafe { open_device()? };
    unsafe {
        let _ = CloseHandle(h);
    }
    Ok(())
}

/// Is the SudoVDA driver present (device interface enumerable)?
pub fn is_available() -> bool {
    unsafe { open_device().map(|h| CloseHandle(h)).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live hardware round trip — skipped unless `PUNKTFUNK_SUDOVDA_LIVE=1` (needs the SudoVDA
    /// driver installed). Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    fn live_create_drop() {
        if std::env::var("PUNKTFUNK_SUDOVDA_LIVE").is_err() {
            return;
        }
        let mut vd = SudoVdaDisplay::new().expect("open SudoVDA");
        let vout = vd
            .create(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create virtual display");
        assert_eq!(vout.preferred_mode, Some((1920, 1080, 60)));
        thread::sleep(Duration::from_secs(3));
        drop(vout); // triggers REMOVE + stops the pinger
    }
}
