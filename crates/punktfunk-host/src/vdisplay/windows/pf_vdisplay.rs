//! Windows virtual-display backend driving **pf-vdisplay** — punktfunk's OWN IddCx Indirect Display
//! Driver (the clean-room replacement for SudoVDA). The Windows analogue of the Linux per-compositor
//! backends: [`create`](VirtualDisplay::create) adds a virtual monitor at the client's exact `WxH@Hz`
//! (the mode is baked into the ADD IOCTL — no EDID seeding), starts the mandatory watchdog ping, and
//! the returned [`VirtualOutput`]'s keepalive `Drop` removes it (RAII).
//!
//! Control surface: a device-interface-GUID + `CreateFileW` + `DeviceIoControl` IOCTL protocol, with
//! the wire contract OWNED by [`pf_vdisplay_proto::control`] (versioned + `#[repr(C)] Pod` structs,
//! NOT the SudoVDA ABI). No DLL, no named pipe. See `docs/windows-host-rewrite.md`.
//!
//! This is a faithful clone of [`super::sudovda`] (the shipping fallback) repointed at the new driver:
//! same reference-counted/lingering monitor lifecycle, same CCD isolation + active-mode forcing — those
//! backend-NEUTRAL helpers are REUSED from `sudovda` (a pf-vdisplay monitor's `target_id` is a real OS
//! target id, so the CCD/DXGI code works unchanged). Only the driver-specific bits (GUID, IOCTL codes,
//! request/reply structs, the version handshake) differ, per `pf_vdisplay_proto`.

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

use pf_vdisplay_proto::control;

use super::{Mode, VirtualDisplay, VirtualOutput};
// Backend-NEUTRAL CCD/DXGI helpers reused from the SudoVDA backend (a pf-vdisplay monitor's target_id
// is a real OS target id, so these operate identically). The shared MON_GEN lease-generation counter is
// reused too, so a stale preempted lease can't tear down the live monitor regardless of which backend is active.
use super::sudovda::MON_GEN;
use crate::win_adapter::resolve_render_adapter_luid;
use crate::win_display::{
    isolate_displays_ccd, resolve_gdi_name, restore_displays_ccd, set_active_mode, SavedConfig,
};

// pf-vdisplay device-interface GUID (pf_vdisplay_proto::PF_VDISPLAY_INTERFACE_GUID_U128). Deliberately
// NOT SudoVDA's `{e5bcc234-…}` — we own this driver, so a private interface GUID signals it and avoids
// any accidental coexistence with a real SudoVDA install.
const PF_VDISPLAY_INTERFACE: GUID =
    GUID::from_u128(pf_vdisplay_proto::PF_VDISPLAY_INTERFACE_GUID_U128);

/// IDD-push mode: a new client connection preempts + recreates the monitor (single-client reconnect),
/// because a REUSED IddCx monitor's swap-chain is dead. Off → monitors are shared across sessions.
fn idd_push_mode() -> bool {
    crate::config::config().idd_push
}

/// Monotonic per-session id keying a pf-vdisplay monitor for `IOCTL_ADD`/`IOCTL_REMOVE`. Unlike
/// SudoVDA's 16-byte GUID + pid-mangling, the proto keys monitors by a plain `u64` — the host-level
/// refcount manager (MGR) owns collision safety (a stale session can never REMOVE a live one), so a
/// simple monotonic counter suffices. Unique per (process, session) within this host's lifetime.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// One `DeviceIoControl` round trip (METHOD_BUFFERED). `input`/`output` may be empty. Identical to the
/// SudoVDA backend's wrapper; struct<->bytes conversion happens at the call sites via `bytemuck`.
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

/// Pin the pf-vdisplay IddCx's RENDER GPU to `luid` (the analogue of Apollo's `SetRenderAdapter`). No
/// output buffer. Issued on the driver handle BEFORE `IOCTL_ADD` to steer which GPU the new target
/// renders on — on a multi-adapter box this stops DXGI from reparenting the virtual output onto a
/// different adapter than the one we duplicate/encode on (the ACCESS_LOST storm).
///
/// NOTE: the pf-vdisplay driver currently returns `STATUS_NOT_IMPLEMENTED` for this IOCTL (a STEP-4
/// stub), so this call WILL fail today. Callers tolerate the `Err` (warn + continue) — exactly as the
/// SudoVDA backend tolerated the driver IGNORING the pin.
unsafe fn set_render_adapter(h: HANDLE, luid: LUID) -> Result<()> {
    let req = control::SetRenderAdapterRequest {
        luid_low: luid.LowPart,
        luid_high: luid.HighPart,
    };
    let mut none: [u8; 0] = [];
    ioctl(
        h,
        control::IOCTL_SET_RENDER_ADAPTER,
        bytemuck::bytes_of(&req),
        &mut none,
    )
    .map(|_| ())
    .context("pf-vdisplay SET_RENDER_ADAPTER")
}

unsafe fn open_device() -> Result<HANDLE> {
    let hdev = SetupDiGetClassDevsW(
        Some(&PF_VDISPLAY_INTERFACE),
        PCWSTR::null(),
        None,
        DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
    )
    .context("SetupDiGetClassDevsW(pf-vdisplay) — is the pf-vdisplay driver installed?")?;

    let mut idata = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    SetupDiEnumDeviceInterfaces(hdev, None, &PF_VDISPLAY_INTERFACE, 0, &mut idata)
        .context("SetupDiEnumDeviceInterfaces(pf-vdisplay)")?;

    let mut required = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(hdev, &idata, None, 0, Some(&mut required), None);
    let mut buf = vec![0u8; required as usize];
    let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    SetupDiGetDeviceInterfaceDetailW(hdev, &idata, Some(detail), required, None, None)
        .context("SetupDiGetDeviceInterfaceDetailW(pf-vdisplay)")?;

    let handle = CreateFileW(
        PCWSTR((*detail).DevicePath.as_ptr()),
        0xC000_0000, // GENERIC_READ | GENERIC_WRITE
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        None,
        OPEN_EXISTING,
        FILE_FLAGS_AND_ATTRIBUTES(0),
        None,
    )
    .context("CreateFileW(pf-vdisplay device)")?;
    let _ = SetupDiDestroyDeviceInfoList(hdev);
    Ok(handle)
}

// ── Host-level reference-counted pf-vdisplay monitor lifecycle ───────────────────────────────────
//
// The virtual monitor is created on the first session and REUSED across sessions. When the last
// session disconnects the monitor LINGERS for a grace window (PUNKTFUNK_MONITOR_LINGER_MS, default
// 10 s): a reconnect within the window reuses it instantly (no new screen, no PnP connect/disconnect
// chime, no teardown/recreate kernel churn); after the window a background timer REMOVEs it so a
// physical-screen user gets their screen back. Overlapping sessions share one monitor via the
// refcount (teardown only at refs==0 + expired grace), so a stale session can never REMOVE a live
// session's monitor. The control-device HANDLE is opened once and kept for the host lifetime — it's a
// handle, not a screen, so it creates no phantom display.

/// The resources backing one live pf-vdisplay monitor (owned by [`MGR`], not by any session).
struct Monitor {
    /// Per-session key for `IOCTL_ADD`/`IOCTL_REMOVE` (the proto keys monitors by a plain `u64`).
    session_id: u64,
    target_id: u32,
    luid: LUID,
    gdi_name: Option<String>,
    mode: Mode,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    ccd_saved: Option<SavedConfig>,
    /// Generation stamp (shared [`MON_GEN`]); a [`MonitorLease`] only releases if its gen still matches.
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
    watchdog_s: 10,
    state: MgrState::Idle,
});

/// The Windows pf-vdisplay backend. A marker — the monitor lifecycle lives in the global [`MGR`].
pub struct PfVdisplayDisplay;

impl PfVdisplayDisplay {
    pub fn new() -> Result<Self> {
        // Open the control device once (validates the driver is present + version-matches) + log the
        // watchdog timeout.
        let mut g = MGR.lock().unwrap();
        mgr_ensure_device(&mut g)?;
        Ok(Self)
    }
}

impl Drop for PfVdisplayDisplay {
    fn drop(&mut self) {
        // Nothing: the control device + monitor lifecycle are host-level (owned by MGR) and
        // deliberately outlive any single session so a reconnect can reuse the monitor.
    }
}

impl VirtualDisplay for PfVdisplayDisplay {
    fn name(&self) -> &'static str {
        "pf-vdisplay"
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Delegate to the host-level manager: create the monitor, reuse a lingering one on reconnect,
        // or join the live one — and hand back a lease whose Drop releases the refcount.
        mgr_acquire(mode)
    }
}

/// Create a fresh pf-vdisplay monitor at `mode` on the (host-level) control `device`. ADD the target,
/// start the watchdog ping, resolve the GDI name, force the client mode + (default) isolate to a sole
/// composited display. Returns the [`Monitor`] resources; the manager tracks its lifecycle
/// (refcount + linger).
unsafe fn create_monitor(device: isize, mode: Mode, watchdog_s: u32) -> Result<Monitor> {
    let dev = HANDLE(device as *mut c_void);
    {
        // Fresh session id per created monitor (the manager refcount, not the id, prevents the
        // cross-session REMOVE collision).
        let session_id = next_session_id();
        let add = control::AddRequest {
            session_id,
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            _reserved: 0,
        };
        // SET_RENDER_ADAPTER is OPT-IN. By default we do NOT pin the render adapter — let the IDD use
        // its natural adapter (Apollo-parity; avoids the cross-GPU mismatch ACCESS_LOST storm). Opt in
        // with PUNKTFUNK_RENDER_ADAPTER=<name substring> or the IDD-push path (which MUST run NVENC on
        // the discrete render GPU it pins here). The pf-vdisplay driver now IMPLEMENTS this IOCTL
        // (IddCxAdapterSetRenderAdapter); a failure is still tolerated (the driver also reports its real
        // render LUID in the shared header, so the host binds to the right GPU regardless).
        let pinned = if crate::config::config().render_adapter.is_some() {
            unsafe { resolve_render_adapter_luid() }
        } else if crate::config::config().idd_push {
            // P2 direct frame push: the host opens the driver's shared textures AND runs NVENC on the
            // RENDER adapter, so on a hybrid box (dGPU + iGPU) it MUST be the discrete encoder GPU — an
            // iGPU-rendered surface is untouchable by NVENC. pf-vdisplay now IMPLEMENTS
            // SET_RENDER_ADAPTER, so pin the discrete GPU; the driver also reports the resulting render LUID
            // in the shared header, so the host binds correctly even if this is overridden.
            tracing::info!("IDD push: pinning the discrete render GPU (SET_RENDER_ADAPTER)");
            unsafe { resolve_render_adapter_luid() }
        } else {
            tracing::info!(
                "pf-vdisplay SET_RENDER_ADAPTER skipped (no render pin — avoids cross-GPU mismatch; \
                 set PUNKTFUNK_RENDER_ADAPTER=<name> to force a specific render GPU)"
            );
            None
        };
        if let Some(luid) = pinned {
            match unsafe { set_render_adapter(dev, luid) } {
                Ok(()) => tracing::info!(
                    luid = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    "pf-vdisplay SET_RENDER_ADAPTER: pinned IDD render GPU"
                ),
                // Non-fatal: warn + continue (do NOT propagate). The driver reports its real render LUID
                // in the shared header and the host binds to that, so the natural-adapter path still works.
                Err(e) => tracing::warn!(
                    "pf-vdisplay SET_RENDER_ADAPTER failed (continuing on the natural adapter): {e:#}"
                ),
            }
        }

        let mut out = [0u8; size_of::<control::AddReply>()];
        unsafe { ioctl(dev, control::IOCTL_ADD, bytemuck::bytes_of(&add), &mut out) }
            .with_context(|| {
                format!(
                    "pf-vdisplay ADD {}x{}@{}",
                    mode.width, mode.height, mode.refresh_hz
                )
            })?;
        // `pod_read_unaligned` (NOT `from_bytes`): `out` is a stack `[u8; N]` with no guaranteed
        // 4-byte alignment, and `from_bytes` PANICS on an alignment mismatch. This copies the bytes
        // into a properly-aligned `AddReply` value.
        let reply: control::AddReply =
            bytemuck::pod_read_unaligned(&out[..size_of::<control::AddReply>()]);
        let luid = LUID {
            LowPart: reply.adapter_luid_low,
            HighPart: reply.adapter_luid_high,
        };
        tracing::info!(
            "pf-vdisplay created {}x{}@{} (target_id={}, adapter_luid={:#x})",
            mode.width,
            mode.height,
            mode.refresh_hz,
            reply.target_id,
            luid.LowPart
        );
        if let Some(pin) = pinned {
            if luid.LowPart == pin.LowPart && luid.HighPart == pin.HighPart {
                tracing::info!("pf-vdisplay ADD render adapter matches the pinned GPU (pin took)");
            } else {
                tracing::warn!(
                    add = format!("{:08x}:{:08x}", luid.HighPart, luid.LowPart),
                    pinned = format!("{:08x}:{:08x}", pin.HighPart, pin.LowPart),
                    "pf-vdisplay ADD render adapter DIFFERS from pinned — driver ignored SET_RENDER_ADAPTER?"
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
                match unsafe { ioctl(h, control::IOCTL_PING, &[], &mut none) } {
                    Ok(_) => warned = false,
                    // A persistently failing PING means the cached control handle went invalid — the
                    // driver watchdog will then tear the monitor down mid-session. Surface it once.
                    Err(e) => {
                        if !warned {
                            tracing::warn!(
                                "pf-vdisplay keepalive PING failed (control handle lost?): {e:#}"
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
            if let Some(n) = unsafe { resolve_gdi_name(reply.target_id) } {
                gdi_name = Some(n);
                break;
            }
        }
        let mut ccd_saved: Option<SavedConfig> = None;
        match &gdi_name {
            Some(n) => {
                tracing::info!("pf-vdisplay target {} -> {n}", reply.target_id);
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Make the pf-vdisplay the SOLE active display (default). An EXTENDED (non-primary) IDD
                // is NOT DWM-composited → Desktop Duplication gets a born-lost ACCESS_LOST; deactivating
                // the other display(s) FIRST (CCD, atomic) leaves the virtual output as the sole →
                // primary → composited desktop, so all content (incl. Winlogon) renders to it without a
                // MODE_CHANGE_IN_PROGRESS storm. Opt out with PUNKTFUNK_NO_ISOLATE=1 (a box with a real
                // second monitor to keep live).
                if std::env::var("PUNKTFUNK_NO_ISOLATE").is_err() {
                    ccd_saved = unsafe { isolate_displays_ccd(reply.target_id) };
                } else {
                    tracing::info!(
                        "display isolation skipped (PUNKTFUNK_NO_ISOLATE) — IDD stays extended"
                    );
                }
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens
            }
            None => tracing::warn!(
                "pf-vdisplay target {} not yet an active display path (needs a WDDM GPU to activate)",
                reply.target_id
            ),
        }

        Ok(Monitor {
            session_id,
            target_id: reply.target_id,
            luid,
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

    /// Stop the watchdog ping, re-attach the displays we detached, then REMOVE the monitor (by session
    /// id). `device` is the host-level control handle. Consumes the monitor.
    unsafe fn teardown(mut self, device: isize) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.pinger.take() {
            let _ = j.join();
        }
        // Re-attach detached display(s) BEFORE the REMOVE so the box is never left with zero displays.
        if let Some(saved) = &self.ccd_saved {
            restore_displays_ccd(saved);
        }
        let req = control::RemoveRequest {
            session_id: self.session_id,
        };
        let mut none: [u8; 0] = [];
        let h = HANDLE(device as *mut c_void);
        if let Err(e) = ioctl(
            h,
            control::IOCTL_REMOVE,
            bytemuck::bytes_of(&req),
            &mut none,
        ) {
            tracing::warn!("pf-vdisplay REMOVE failed: {e:#}");
        } else {
            tracing::info!("pf-vdisplay monitor removed");
        }
    }
}

/// Open the control device once + version/watchdog handshake; cache the handle (raw isize) in `g`.
fn mgr_ensure_device(g: &mut Mgr) -> Result<isize> {
    if let Some(d) = g.device {
        return Ok(d);
    }
    let device = unsafe { open_device()? };
    // Single version+watchdog handshake. The proto intends a HARD protocol-version check (unlike
    // SudoVDA's best-effort log) — a mismatched host/driver pair fails loudly here rather than
    // corrupting the IOCTL stream.
    let mut info_buf = [0u8; size_of::<control::InfoReply>()];
    unsafe { ioctl(device, control::IOCTL_GET_INFO, &[], &mut info_buf) }
        .context("pf-vdisplay IOCTL_GET_INFO (version handshake)")?;
    // `pod_read_unaligned` (see the AddReply note): copies out of the unaligned stack buffer.
    let info: control::InfoReply =
        bytemuck::pod_read_unaligned(&info_buf[..size_of::<control::InfoReply>()]);
    if info.protocol_version != pf_vdisplay_proto::PROTOCOL_VERSION {
        // Close the handle before bailing so a retry re-opens cleanly.
        unsafe {
            let _ = CloseHandle(device);
        }
        anyhow::bail!(
            "pf-vdisplay protocol mismatch: host expects {}, driver reports {} — install matching \
             host + driver",
            pf_vdisplay_proto::PROTOCOL_VERSION,
            info.protocol_version
        );
    }
    g.watchdog_s = info.watchdog_timeout_s.max(1);
    tracing::info!(
        "pf-vdisplay protocol {} (watchdog timeout {}s)",
        info.protocol_version,
        g.watchdog_s
    );
    // Reap monitors orphaned by a crashed/killed previous host instance before we create ours. This is
    // a FIRST-CLASS op on pf-vdisplay (the driver returns SUCCESS), NOT a "send-and-hope" hack: without
    // it an orphan lingers until the driver watchdog fires — but a still-pinging new session keeps
    // resetting that watchdog, so orphans could accumulate.
    {
        let mut none: [u8; 0] = [];
        if unsafe { ioctl(device, control::IOCTL_CLEAR_ALL, &[], &mut none) }.is_ok() {
            tracing::info!("cleared orphaned virtual monitors on host startup");
        } else {
            tracing::warn!("pf-vdisplay IOCTL_CLEAR_ALL failed on startup (continuing)");
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
    // tear the old monitor down (its teardown restores topology + IOCTL_REMOVEs) and fall through to
    // create a FRESH one. The old session's lease is gen-stamped, so its later drop is ignored
    // (mgr_release no-op) and can't tear down the new monitor.
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
            // `Drop` impl, so a bare `drop(mon)` would orphan the IddCx monitor in the driver (never
            // departed → leaks a live D3D device + a stuck swap-chain processor thread per reconnect).
            unsafe { mon.teardown(device) };
            // Let the OS finish the ASYNC IddCx monitor departure before the next ADD. A back-to-back
            // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
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
            "pf-vdisplay monitor reused (concurrent / reconfigure session)"
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
            tracing::info!("pf-vdisplay monitor reused (reconnect within the linger window)");
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
        "pf-vdisplay: reconfiguring reused monitor to the new client mode"
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
                "pf-vdisplay: last session left — lingering before teardown"
            );
            MgrState::Lingering {
                mon,
                until: Instant::now() + Duration::from_millis(ms),
            }
        }
        other => other,
    };
}

// NOTE: `wait_for_monitor_released` is NOT redefined here. Its only caller (`punktfunk1.rs`, the
// IDD-push reconnect preempt) reaches it as `crate::vdisplay::sudovda::wait_for_monitor_released`, and
// pf_vdisplay.rs never calls it internally (the preempt is done inline in `mgr_acquire` above), so a
// second copy here would be dead code waiting on the (separate) pf-vdisplay MGR. The two backends keep
// independent MGRs but only one is ever active — see the cross-MGR caveat in the implementation report.

/// Background timer (started once): tear down a monitor that has lingered past its deadline (→ Idle),
/// so a physical-screen user gets their screen back after they stop streaming.
fn ensure_linger_timer() {
    static TIMER: Once = Once::new();
    TIMER.call_once(|| {
        let _ = thread::Builder::new()
            .name("pf-vdisplay-linger".into())
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

/// Readiness probe: can we open the pf-vdisplay control device?
pub fn probe() -> Result<()> {
    let h = unsafe { open_device()? };
    unsafe {
        let _ = CloseHandle(h);
    }
    Ok(())
}

/// Is the pf-vdisplay driver present (device interface enumerable)?
pub fn is_available() -> bool {
    unsafe { open_device().map(|h| CloseHandle(h)).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live hardware round trip — skipped unless `PUNKTFUNK_PF_VDISPLAY_LIVE=1` (needs the pf-vdisplay
    /// driver installed). Exercises the real trait path: open -> create -> hold -> drop (REMOVE).
    #[test]
    fn live_create_drop() {
        if std::env::var("PUNKTFUNK_PF_VDISPLAY_LIVE").is_err() {
            return;
        }
        let mut vd = PfVdisplayDisplay::new().expect("open pf-vdisplay");
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
