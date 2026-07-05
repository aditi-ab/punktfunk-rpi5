//! Host-lifetime virtual-display **ownership model** (Goal-1 §2.5). One reference-counted monitor
//! lifecycle, shared by both Windows backends (SudoVDA + pf-vdisplay) instead of the two verbatim-
//! duplicated `MGR: Mutex<Mgr>` globals each backend used to carry.
//!
//! [`VirtualDisplayManager`] owns the earned Idle/Active/Lingering refcount machine + the linger timer +
//! a **typed** [`OwnedHandle`] control device (no more raw `isize` smuggled across the pinger/linger
//! threads). The backend differences — the IOCTL protocol and the per-monitor REMOVE key — are the only
//! thing behind the [`VdisplayDriver`] seam; the state machine, the render-adapter pin decision, the
//! GDI/CCD glue (`crate::win_display`), and the generation-stamped [`MonitorLease`] are backend-neutral.
//!
//! It's a process-wide singleton ([`vdm`]) initialised once with the chosen backend's driver — the
//! host runs exactly one virtual-display backend per process. The session holds a [`MonitorLease`];
//! its `Drop` releases the refcount (a *stale* lease — its monitor was preempted + recreated under it —
//! is a no-op, so it can never tear down the live monitor).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, LUID, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

use super::{Mode, VirtualOutput};
use crate::win_display::{
    force_extend_topology, isolate_displays_ccd, resolve_gdi_name, restore_displays_ccd,
    set_active_mode, set_virtual_primary_ccd, SavedConfig,
};

/// The per-backend REMOVE key the driver stamps on ADD and consumes on REMOVE. SudoVDA keys monitors by
/// a fresh `GUID`; pf-vdisplay keys them by a monotonic `u64` session id.
#[derive(Clone, Copy)]
pub(crate) enum MonitorKey {
    Guid(windows::core::GUID),
    Session(u64),
}

/// What a backend's `add_monitor` returns: the REMOVE key + the OS target id + the render LUID + the
/// driver's WUDFHost pid (the sealed frame channel's handle-duplication target).
pub(crate) struct AddedMonitor {
    pub key: MonitorKey,
    pub target_id: u32,
    pub luid: LUID,
    pub wudf_pid: u32,
}

/// The backend-specific IOCTL surface — the *only* thing that differs between SudoVDA and pf-vdisplay.
/// Everything else (the refcount machine, the linger, the pinger, the CCD/GDI glue) is shared in
/// [`VirtualDisplayManager`]. `Send + Sync` because the manager (and so the boxed driver) is a
/// `&'static` singleton reached from the pinger + linger threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// Find + open the control device, validate it (version handshake), and read the watchdog
    /// timeout. `reap_orphans` (the FIRST open of the process only) additionally `CLEAR_ALL`s
    /// monitors orphaned by a crashed previous host — a REOPEN (after a dead handle was retired)
    /// must NOT, since sessions this process still considers live may be racing it. Returns the
    /// owned handle + watchdog seconds.
    ///
    /// # Safety
    /// Issues setup-API + `DeviceIoControl` calls; runs in the caller's apartment.
    unsafe fn open(&self, reap_orphans: bool) -> Result<(OwnedHandle, u32)>;
    /// ADD a virtual monitor at `mode`, pinning the IDD render GPU to `render_luid` first if `Some`, and
    /// requesting `preferred_monitor_id` (the host's per-client stable id; `0` = auto). Returns the REMOVE
    /// key + target id + the adapter LUID the driver actually used.
    ///
    /// # Safety
    /// `dev` must be the live control handle from [`open`](Self::open).
    unsafe fn add_monitor(
        &self,
        dev: HANDLE,
        mode: Mode,
        render_luid: Option<LUID>,
        preferred_monitor_id: u32,
    ) -> Result<AddedMonitor>;
    /// REMOVE the monitor identified by `key`.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn remove_monitor(&self, dev: HANDLE, key: &MonitorKey) -> Result<()>;
    /// Watchdog keepalive PING (issued every `watchdog/3` from the pinger thread).
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn ping(&self, dev: HANDLE) -> Result<()>;
}

/// The resources backing one live virtual monitor (owned by the [`VirtualDisplayManager`] state, not by
/// any session). No `Drop` impl — [`teardown`](VirtualDisplayManager::teardown) must be called so the
/// REMOVE IOCTL fires (a bare drop would orphan the driver-side monitor).
struct Monitor {
    key: MonitorKey,
    target_id: u32,
    luid: LUID,
    /// The driver's WUDFHost pid (from the ADD reply) — carried into [`WinCaptureTarget`] so the
    /// IDD-push capturer knows where to duplicate the sealed frame channel's handles.
    wudf_pid: u32,
    gdi_name: Option<String>,
    mode: Mode,
    stop: Arc<AtomicBool>,
    pinger: Option<JoinHandle<()>>,
    ccd_saved: Option<SavedConfig>,
    /// Generation stamp; a [`MonitorLease`] only releases if its gen still matches (stale-lease no-op).
    gen: u64,
}

impl Monitor {
    /// The capture target handed to a session (`None` until the GDI name resolves on a WDDM GPU).
    fn target(&self) -> Option<crate::capture::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| crate::capture::dxgi::WinCaptureTarget {
                adapter_luid: crate::capture::dxgi::pack_luid(self.luid),
                gdi_name: n,
                target_id: self.target_id,
                wudf_pid: self.wudf_pid,
            })
    }
}

enum MgrState {
    Idle,
    Active {
        mon: Monitor,
        refs: u32,
    },
    Lingering {
        mon: Monitor,
        until: Instant,
    },
    /// `keep_alive = forever` (gaming-rig): the monitor is kept indefinitely after the last session
    /// leaves — like `Lingering` but the linger timer never tears it down. A reconnect preempts +
    /// recreates it (same as `Lingering`, since a reused IddCx swap-chain is dead); only the mgmt
    /// `/display/release` (or host shutdown) frees it. The physical screens stay off (exclusive) for
    /// the box's life — the §8 release-now escape hatch (`force_release`) is the way back.
    Pinned {
        mon: Monitor,
    },
}

/// The manager's control-device cache. Reopenable: a driver upgrade / WUDFHost restart kills the
/// cached handle (every IOCTL fails with a gone-class code forever), so such a failure RETIRES it and
/// the next [`VirtualDisplayManager::ensure_device`] reopens the (new) device interface, re-running
/// the version handshake. Retired handles are deliberately kept alive — never closed — for the
/// process lifetime: the pinger/linger threads and every capturer's `ChannelBroker` hold BARE
/// `HANDLE` copies whose soundness contract is "never closed"; a retired handle only ever FAILS
/// IOCTLs, which every holder already tolerates. Reopens are rare (a driver restart), so the retained
/// list is bounded in practice.
#[derive(Default)]
struct DeviceSlot {
    current: Option<Arc<OwnedHandle>>,
    /// Never dropped — see the type doc (bare-`HANDLE` holders rely on no-close).
    retired: Vec<Arc<OwnedHandle>>,
    /// `CLEAR_ALL` (crashed-host orphan reap) runs only on the FIRST open of the process; a reopen
    /// races sessions this process still considers live and must not raze them.
    opened_once: bool,
}

/// The host-lifetime virtual-display manager: the single owner of the monitor lifecycle.
pub(crate) struct VirtualDisplayManager {
    driver: Box<dyn VdisplayDriver>,
    /// Control device, opened on first acquire and REOPENED after a gone-classified failure retired
    /// it (see [`DeviceSlot`]). Typed + `Send+Sync`, so the pinger/linger threads share it via the
    /// `&'static` singleton with no raw-handle smuggling.
    device: Mutex<DeviceSlot>,
    watchdog_s: AtomicU32,
    /// Monotonic lease-generation counter (was the `MON_GEN` global).
    gen: AtomicU64,
    state: Mutex<MgrState>,
    /// Serializes IDD-push session SETUP (preempt + monitor create) so a reconnect flood can't run
    /// concurrent monitor create/teardown — held by the session across the pipeline build (was the
    /// `IDD_SETUP_LOCK` global in `punktfunk1`).
    setup_lock: Mutex<()>,
    /// The current IDD-push session's stop flag; a new connection signals the prior one to release its
    /// monitor before the fresh one is created (was the `IDD_SESSION_STOP` global in `punktfunk1`).
    idd_session_stop: Mutex<Option<Arc<AtomicBool>>>,
    // The per-client stable monitor-id map is now the process-wide `super::identity::global()`
    // (shared with the Linux KWin backend's per-slot naming — never same-process). A monitor CREATE
    // resolves the client's id via `identity::resolve_slot`, so it keeps the same EDID serial + IddCx
    // ConnectorIndex across reconnects and Windows reapplies its saved per-monitor DPI scaling.
}

static VDM: OnceLock<VirtualDisplayManager> = OnceLock::new();

/// Initialise the process-wide manager with `driver` (the chosen backend) and return it. Idempotent: the
/// first backend to call wins (the host runs one backend per process), so a later call ignores its driver.
pub(crate) fn init(driver: Box<dyn VdisplayDriver>) -> &'static VirtualDisplayManager {
    VDM.get_or_init(|| VirtualDisplayManager {
        driver,
        device: Mutex::new(DeviceSlot::default()),
        watchdog_s: AtomicU32::new(3),
        gen: AtomicU64::new(1),
        state: Mutex::new(MgrState::Idle),
        setup_lock: Mutex::new(()),
        idd_session_stop: Mutex::new(None),
    })
}

/// The process-wide manager. Panics if reached before a backend called [`init`] — by construction a
/// session is only ever created after `vdisplay::open` constructed the backend (which calls `init`).
pub(crate) fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

/// The live pf-vdisplay control-device handle, for the IDD-push capturer's sealed-channel delivery
/// (`IOCTL_SET_FRAME_CHANNEL`). Safe to hand out as a bare `HANDLE`: cached handles are never closed
/// for the process lifetime — a dead one is RETIRED (kept alive, see [`DeviceSlot`]), so a stale copy
/// can only fail IOCTLs, never dangle. `None` before the first backend open — impossible for a
/// capturer, which only exists on a monitor the manager created.
pub(crate) fn control_device_handle() -> Option<HANDLE> {
    VDM.get().and_then(VirtualDisplayManager::device_handle)
}

/// True when an IOCTL failure means the CONTROL DEVICE itself is gone (driver upgrade, WUDFHost
/// restart, device disable) — the cached handle can only keep failing and must be retired so the
/// next use reopens. The root `windows` error survives anyhow `.context` chains via `downcast_ref`.
/// NOTE: 0x80070490 (ERROR_NOT_FOUND, the ADD slot-exhaustion wedge) is deliberately NOT here — it
/// has its own reap-and-retry handling and the device is alive when it fires.
/// The held single-instance mutex (`None` until claimed). Process-global — not per-manager — so the
/// serve path can claim it EAGERLY at startup, before any session opens the backend: the claim is
/// first-comer-wins, and a lazily-claiming service could otherwise lose its own machine's driver to
/// a stray second host started while the service sat idle (observed on-glass). A failed claim is NOT
/// memoized: once the other instance exits, the next attempt succeeds.
static INSTANCE: Mutex<Option<OwnedHandle>> = Mutex::new(None);

/// Claim (or re-verify) the cross-process single-instance guard. Idempotent; retries after failure.
fn claim_instance() -> Result<()> {
    let mut g = INSTANCE.lock().unwrap();
    if g.is_none() {
        *g = Some(acquire_single_instance()?);
    }
    Ok(())
}

/// Eager startup claim for the serve/service path (Windows): reserves this process as THE
/// pf-vdisplay manager before any client connects. Failure is a loud warning, not fatal — sessions
/// then fail with the same clear in-use error until the other instance exits.
pub(crate) fn claim_instance_eagerly() {
    if let Err(e) = claim_instance() {
        tracing::warn!("pf-vdisplay single-instance claim failed at startup: {e:#}");
    }
}

/// The cross-process single-instance guard for pf-vdisplay management. A SECOND host process's
/// first device open used to fire `IOCTL_CLEAR_ALL` and raze the live host's monitors mid-stream —
/// an admin footgun (run `punktfunk-host serve` while the SCM service streams), masked afterwards
/// because both processes' pings satisfy the shared driver watchdog. The named mutex makes the
/// second process fail its vdisplay open LOUDLY instead. Held, never released, for the process
/// lifetime; the OS reclaims it (and frees the name) when the process exits, however it exits.
fn acquire_single_instance() -> Result<OwnedHandle> {
    const IN_USE: &str = "another punktfunk-host process is already managing pf-vdisplay on this \
         machine — refusing to touch the driver (a second manager's startup CLEAR_ALL would raze \
         the live host's monitors mid-stream). Stop the other instance (e.g. `punktfunk-host \
         service stop`) first.";
    // SAFETY: plain FFI create of a named mutex; the returned handle (checked) is solely owned by
    // the `OwnedHandle`, and `GetLastError` is read immediately after the create — the documented
    // ERROR_ALREADY_EXISTS protocol for pre-existing named objects.
    unsafe {
        let h = match CreateMutexW(None, false, w!("Global\\punktfunk-vdisplay-manager")) {
            Ok(h) => h,
            // The name exists but its creator's DACL denies this token the implicit OPEN (the SCM
            // service creates it as SYSTEM; a second elevated-admin host lands here instead of in
            // the ALREADY_EXISTS branch — validated on-glass). Same meaning: an instance is live.
            Err(e) if e.code().0 == 0x8007_0005u32 as i32 => anyhow::bail!("{IN_USE}"),
            Err(e) => {
                return Err(e).context("CreateMutexW(punktfunk-vdisplay single-instance guard)");
            }
        };
        let already = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = OwnedHandle::from_raw_handle(h.0 as _);
        if already {
            anyhow::bail!("{IN_USE}");
        }
        Ok(owned)
    }
}

/// Best-effort "is this WUDFHost pid still alive?" — the monitor-liveness probe for the JOIN path.
/// `OpenProcess` failing (pid reaped) or the process being signaled ⇒ dead. Pid reuse could
/// theoretically alias a fresh process and read "alive"; the joining session then just retries into
/// its rebuild budget — acceptable for a sub-second reuse window that realistically never hits.
fn wudf_alive(pid: u32) -> bool {
    if pid == 0 {
        return true; // pre-v2 driver reports no pid — never preempt on the probe's account
    }
    // SAFETY: plain FFI probe; the opened handle (checked) is closed exactly once below, and the
    // 0 ms wait only reads its signaled state.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let alive = WaitForSingleObject(h, 0) != WAIT_OBJECT_0;
        let _ = CloseHandle(h);
        alive
    }
}

fn is_device_gone(e: &anyhow::Error) -> bool {
    let Some(w) = e.downcast_ref::<windows::core::Error>() else {
        return false;
    };
    // Win32 codes as HRESULTs: FILE_NOT_FOUND(2), INVALID_HANDLE(6), BAD_COMMAND(22),
    // GEN_FAILURE(31), DEV_NOT_EXIST(55), OPERATION_ABORTED(995), DEVICE_NOT_CONNECTED(1167 =
    // 0x48F — one below the 0x490 wedge), DEVICE_REMOVED(1617).
    const GONE: [i32; 8] = [
        0x8007_0002u32 as i32,
        0x8007_0006u32 as i32,
        0x8007_0016u32 as i32,
        0x8007_001Fu32 as i32,
        0x8007_0037u32 as i32,
        0x8007_03E3u32 as i32,
        0x8007_048Fu32 as i32,
        0x8007_0651u32 as i32,
    ];
    GONE.contains(&w.code().0)
}

impl VirtualDisplayManager {
    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
    }

    /// Open + cache the control device; REOPEN when a gone-classified failure retired the cached one
    /// (driver upgrade / WUDFHost restart). The `device` mutex serializes racing opens.
    fn ensure_device(&self) -> Result<HANDLE> {
        let mut slot = self.device.lock().unwrap();
        if let Some(d) = &slot.current {
            return Ok(HANDLE(d.as_raw_handle()));
        }
        let reap = !slot.opened_once;
        claim_instance()?;
        // SAFETY: `VdisplayDriver::open` is `unsafe` only because it issues SetupAPI + `DeviceIoControl`
        // FFI in the caller's apartment; the `device` mutex (held here) serializes it, so there is no
        // concurrent open. `open` has no handle precondition to uphold, and the `OwnedHandle` it
        // returns is the sole owner of the device.
        let (handle, watchdog_s) = unsafe { self.driver.open(reap)? };
        slot.opened_once = true;
        self.watchdog_s.store(watchdog_s, Ordering::Relaxed);
        let raw = HANDLE(handle.as_raw_handle());
        slot.current = Some(Arc::new(handle));
        if !reap {
            tracing::info!("virtual-display control device reopened (retired handle replaced)");
        }
        Ok(raw)
    }

    /// The live control handle for the pinger/linger threads. `None` before the first acquire opened
    /// it, or between a retire and the next reopen.
    fn device_handle(&self) -> Option<HANDLE> {
        self.device
            .lock()
            .unwrap()
            .current
            .as_ref()
            .map(|d| HANDLE(d.as_raw_handle()))
    }

    /// Retire the cached control handle after a gone-classified IOCTL failure. The handle is retained
    /// un-closed (see [`DeviceSlot`]); the next [`ensure_device`](Self::ensure_device) reopens the
    /// (new) device interface and re-runs the version handshake.
    fn invalidate_device(&self, why: &anyhow::Error) {
        let mut slot = self.device.lock().unwrap();
        if let Some(cur) = slot.current.take() {
            tracing::warn!(
                "virtual-display control device retired — reopening on next use (cause: {why:#})"
            );
            slot.retired.push(cur);
        }
    }

    /// Open + initialise the backend (validates the driver is present). Mirrors the old
    /// `PfVdisplayDisplay::new`.
    pub(crate) fn open_backend(&self) -> Result<()> {
        // Hold the state lock across the open so two racing backends can't double-open the device.
        let _guard = self.state.lock().unwrap();
        self.ensure_device().map(|_| ())
    }

    /// Acquire the shared monitor for a new session: preempt-recreate under IDD-push, join a live one
    /// (refcount++), reuse a lingering one, or create one. `client_fp` (the connecting client's cert
    /// fingerprint; `None` = anonymous/GameStream) gives a freshly CREATED monitor a STABLE per-client id
    /// (so Windows reapplies that client's saved per-monitor config); JOIN and lingering-reuse keep the
    /// existing monitor's id. The returned [`MonitorLease`] releases the refcount on drop.
    pub(crate) fn acquire(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
    ) -> Result<VirtualOutput> {
        self.ensure_linger_timer();
        let mut state = self.state.lock().unwrap();
        let dev = self.ensure_device()?;

        // IDD-push: a new connection while a monitor is kept (LINGERING or PINNED) is a single-client
        // RECONNECT (the prior session fully released). A REUSED IddCx swap-chain is DEAD, so reusing it
        // hands a black screen — PREEMPT: tear the kept monitor down (its key/topology are restored) and
        // create a fresh one. The old session's lease is gen-stamped, so its later drop is a no-op.
        //
        // ONLY the kept states, NOT Active: an Active monitor still has a lease held — that's the
        // build-retry path (`build_pipeline_with_retry` holds one lease across all attempts) or a
        // concurrent session, NOT a reconnect. Preempting Active would tear a live session down AND churn
        // REMOVE→ADD on every retry — the per-cold-start monitor churn that exhausts the IddCx slot pool
        // and wedges ADD at 0x80070490. Active falls through to the JOIN path below (refcount++, no ADD).
        if matches!(*state, MgrState::Lingering { .. } | MgrState::Pinned { .. }) {
            let taken = match std::mem::replace(&mut *state, MgrState::Idle) {
                MgrState::Lingering { mon, .. } | MgrState::Pinned { mon } => Some(mon),
                other => {
                    *state = other;
                    None
                }
            };
            if let Some(mon) = taken {
                tracing::info!(
                    old_target = mon.target_id,
                    "IDD-push reconnect — preempting the kept (lingering/pinned) monitor, recreating a fresh one"
                );
                // SAFETY: `teardown` requires `dev` to be a valid control handle; `dev` is the value
                // `ensure_device()` returned above (cached handles are never closed — a dead one is
                // retired, kept alive; see `DeviceSlot`). `mon` was moved out of the prior `Lingering`
                // state by `mem::replace`, so it is exclusively owned here — no aliasing.
                unsafe { self.teardown(dev, mon) };
                // Let the OS finish the ASYNC monitor departure before the next ADD; a back-to-back
                // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
                thread::sleep(Duration::from_millis(400));
            }
        }

        // An ACTIVE monitor whose WUDFHost has EXITED is dead driver-side (driver crash / upgrade):
        // the capturer's driver-death watch failed its session, and that session's in-place rebuild
        // re-acquires here while its old lease is STILL held — so the state is Active. Joining would
        // hand the rebuild the dead monitor's target (stale wudf_pid) and starve it to the rebuild
        // budget. Preempt instead: best-effort teardown (REMOVE fails harmlessly on a dead/retired
        // device) and fall through to a fresh create on the auto-restarted device. Held leases are
        // gen-stamped, so their eventual release is a no-op.
        if matches!(&*state, MgrState::Active { mon, .. } if !wudf_alive(mon.wudf_pid)) {
            if let MgrState::Active { mon, .. } = std::mem::replace(&mut *state, MgrState::Idle) {
                tracing::warn!(
                    old_target = mon.target_id,
                    wudf_pid = mon.wudf_pid,
                    "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
                );
                // SAFETY: `teardown` requires a valid control handle; `dev` is the value
                // `ensure_device()` returned above (cached handles are never closed — a dead one is
                // retired, kept alive; see `DeviceSlot`). `mon` was moved out of the replaced state,
                // so it is exclusively owned here — no aliasing.
                unsafe { self.teardown(dev, mon) };
                // Same async-departure settle as the reconnect preempt above.
                thread::sleep(Duration::from_millis(400));
            }
        }

        // A live monitor already exists — join it (refcount++). Covers concurrent sessions AND the
        // build-then-drop overlap of a mid-stream Reconfigure (the new lease is taken while the old is
        // still held). Reconfigure the shared monitor if the requested mode differs.
        if let MgrState::Active { mon, refs } = &mut *state {
            *refs += 1;
            if mon.mode != mode {
                // SAFETY: `reconfigure` only manipulates the live display topology via the CCD/GDI
                // helpers and needs an exclusive `&mut Monitor`. `mon` is the `&mut` into the current
                // `Active` state, held under the `state` lock, so nothing else reconfigures it concurrently.
                unsafe { self.reconfigure(mon, mode) };
            }
            tracing::info!(
                refs = *refs,
                backend = self.driver.name(),
                "virtual monitor reused (concurrent / reconfigure session)"
            );
            return Ok(self.output_for(mon));
        }

        // Idle or kept: repurpose a kept monitor / create a fresh one → Active{refs:1}. (In practice a
        // kept Lingering/Pinned monitor was already preempted → Idle above; this arm is the defensive
        // reuse path if a race left one here — it must stay exhaustive over `Pinned` regardless.)
        let mon = match std::mem::replace(&mut *state, MgrState::Idle) {
            MgrState::Lingering { mut mon, .. } | MgrState::Pinned { mut mon } => {
                tracing::info!(
                    backend = self.driver.name(),
                    "virtual monitor reused (reconnect to a kept monitor)"
                );
                if mon.mode != mode {
                    // SAFETY: `reconfigure` needs an exclusive `&mut Monitor` and only touches the live
                    // display topology. `mon` is the local monitor just moved out of the `Lingering`
                    // state (sole owner), and we hold the `state` lock — no concurrent reconfigure.
                    unsafe { self.reconfigure(&mut mon, mode) };
                }
                mon
            }
            // SAFETY: `create_monitor` requires `dev` to be a valid control handle; `dev` is the
            // handle `ensure_device()` returned above (cached handles are never closed — a dead one
            // is retired, kept alive; see `DeviceSlot`), and we hold the `state` lock.
            MgrState::Idle => match unsafe { self.create_monitor(dev, mode, client_fp) } {
                // The cached device died under us (driver upgrade / WUDFHost restart, detected only
                // now — e.g. the host sat idle past the pinger-less window). Retire it, reopen, and
                // retry ONCE so the reconnect-after-driver-restart succeeds first try instead of
                // burning one failed session per restart.
                Err(e) if is_device_gone(&e) => {
                    self.invalidate_device(&e);
                    let dev = self.ensure_device()?;
                    tracing::info!(
                        "virtual-display control device reopened — retrying the monitor create"
                    );
                    // SAFETY: as above — `dev` is the handle the reopening `ensure_device` just
                    // returned, and the `state` lock is still held.
                    unsafe { self.create_monitor(dev, mode, client_fp)? }
                }
                r => r?,
            },
            MgrState::Active { .. } => unreachable!("handled above"),
        };
        let out = self.output_for(&mon);
        *state = MgrState::Active { mon, refs: 1 };
        Ok(out)
    }

    /// Build the [`VirtualOutput`] (preferred mode + capture target + a fresh gen-stamped lease) for `mon`.
    fn output_for(&'static self, mon: &Monitor) -> VirtualOutput {
        VirtualOutput {
            node_id: 0,
            preferred_mode: Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz)),
            win_capture: mon.target(),
            keepalive: Box::new(MonitorLease {
                mgr: self,
                gen: mon.gen,
            }),
        }
    }

    /// Create a fresh monitor at `mode`: ADD via the driver (pinning the discrete render GPU under the
    /// usual conditions), start the watchdog pinger, resolve the GDI name, force the mode + isolate to a
    /// sole composited display.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn create_monitor(
        &'static self,
        dev: HANDLE,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
    ) -> Result<Monitor> {
        // Resolve the connecting client's STABLE per-client monitor id (so Windows reapplies its saved
        // per-monitor config — DPI scaling — on reconnect); `None`/anonymous → 0 = the driver
        // auto-allocates the lowest-free id (the original slot-based behavior). The `identity` policy
        // picks per-client vs per-client-mode; Windows defaults to PerClient (its historical behavior).
        let preferred_id = super::identity::resolve_slot(
            client_fp,
            (mode.width, mode.height),
            crate::vdisplay::policy::Identity::PerClient,
        )
        .unwrap_or(0);
        // SAFETY: `create_monitor`'s own `# Safety` contract guarantees `dev` is the live control
        // handle; we forward it unchanged to `add_monitor`, whose precondition is exactly that.
        // `resolve_render_pin()` returns an `Option<LUID>` by value (plain `Copy`), so no borrowed
        // memory crosses the call.
        let added = unsafe {
            self.driver
                .add_monitor(dev, mode, resolve_render_pin(), preferred_id)?
        };

        // Mandatory keepalive: ping inside the watchdog window or the driver tears all displays down.
        // The pinger reaches the singleton for both the device + the driver — no raw-handle smuggle.
        let stop = Arc::new(AtomicBool::new(false));
        let interval =
            Duration::from_millis(self.watchdog_s.load(Ordering::Relaxed) as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let pinger = thread::spawn(move || {
            let mut warned = false;
            while !stop_t.load(Ordering::Relaxed) {
                if let Some(h) = vdm().device_handle() {
                    // SAFETY: `ping` requires `dev` to be a valid control handle. `h` is from
                    // `device_handle()` (the `Some` branch) — cached handles are NEVER closed for the
                    // process lifetime (a dead one is retired, kept alive; see `DeviceSlot`), so the
                    // handle stays valid for this call even if it was retired concurrently — at worst
                    // the IOCTL fails. The pinger thread only spins while the `&'static` manager
                    // singleton lives.
                    match unsafe { vdm().driver.ping(h) } {
                        Ok(()) => warned = false,
                        Err(e) if is_device_gone(&e) => {
                            // The device itself is gone (driver upgrade / WUDFHost restart) — pings
                            // can only keep failing on this handle. Retire it so the next session's
                            // `ensure_device` reopens; this monitor is already dead driver-side.
                            vdm().invalidate_device(&e);
                        }
                        Err(e) => {
                            if !warned {
                                tracing::warn!("virtual-display keepalive PING failed (control handle lost?): {e:#}");
                                warned = true;
                            }
                        }
                    }
                }
                thread::sleep(interval);
            }
        });

        // Resolve the capture target — wait for Windows to auto-activate the freshly-ADDed IDD into its
        // OWN display path (it comes up EXTENDED alongside any existing/basic display; `set_active_mode`
        // below then promotes it to primary and `isolate_displays_ccd` makes it the sole composited
        // desktop — the proven flow). May be None on a GPU-less box (target added but not WDDM-activated);
        // the capture backend re-resolves once a GPU is present.
        //
        // We do NOT force a topology change FIRST: the bare `SDC_TOPOLOGY_EXTEND` preset is ACCESS_DENIED
        // from our Session-0 service context on a headless box and BREAKS this auto-activate (it regressed
        // the headless path — the IDD then never gets its own path → "not an active display path" → black).
        // force-EXTEND is only the FALLBACK below, for an integrated-screen box where a fresh IDD is CLONED
        // onto the panel (shares its source) instead of getting its own path.
        let mut gdi_name = None;
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            // SAFETY: `resolve_gdi_name` is `unsafe` for its CCD (QueryDisplayConfig) FFI; it takes a
            // plain `Copy` `u32` target id by value and returns an owned `String`, so no caller memory
            // is borrowed across the call.
            if let Some(n) = unsafe { resolve_gdi_name(added.target_id) } {
                gdi_name = Some(n);
                break;
            }
        }

        // Fallback for an integrated-screen box (e.g. a laptop panel): Windows CLONES a freshly-added
        // IDD onto the existing display, sharing its source, so it never gets its own committed path. On
        // the IddCx clone behaviour observed live (commit 8e87e61, an Intel-iGPU + NVIDIA-Optimus laptop)
        // `resolve_gdi_name` then stays None — so this `is_none()` fallback fires, force-EXTENDs to
        // de-clone, and the second resolve finds the now-committed path. Headless/extended boxes already
        // resolved above (the IDD auto-activates with its OWN source) and skip this — which is the whole
        // point, since force-EXTEND's bare preset is ACCESS_DENIED from our service context there.
        //
        // CAVEAT (unobserved for IddCx, untested across GPU/driver/OS): textbook CCD also lets a clone
        // appear as a *shared-source ACTIVE* path (resolve → Some), which this `is_none()` gate would NOT
        // catch. If that ever shows up, widen the gate to also fire when the IDD target's source is shared
        // with another active path (a `target_is_cloned` helper) — needs on-laptop validation first.
        if gdi_name.is_none() {
            // SAFETY: as above — `force_extend_topology` only calls `SetDisplayConfig` (CCD) with no
            // borrowed caller memory, under the `state` lock.
            unsafe { force_extend_topology() };
            for _ in 0..15 {
                thread::sleep(Duration::from_millis(200));
                // SAFETY: as the resolve loop above.
                if let Some(n) = unsafe { resolve_gdi_name(added.target_id) } {
                    gdi_name = Some(n);
                    break;
                }
            }
        }
        let mut ccd_saved: Option<SavedConfig> = None;
        match &gdi_name {
            Some(n) => {
                tracing::info!(backend = self.driver.name(), "target {} -> {n}", added.target_id);
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Apply the display-management topology (Stage 2). `Exclusive` (default) deactivates the
                // other display(s) so the IDD is the SOLE composited primary — an EXTENDED (non-primary)
                // IDD isn't DWM-composited on this box → Desktop Duplication born-losts. `Primary` keeps the
                // physical display(s) ACTIVE and makes the IDD primary (repositioned to origin). `Extend`
                // leaves it a plain extension. Both isolate + primary go through the atomic CCD path (no
                // MODE_CHANGE storm). Opt out (extend) with PUNKTFUNK_NO_ISOLATE=1 / the console policy.
                use crate::vdisplay::policy::Topology;
                match topology_action() {
                    Topology::Exclusive => {
                        // SAFETY: `isolate_displays_ccd` is `unsafe` for its CCD topology FFI; it takes the
                        // `Copy` target id by value and returns an owned `SavedConfig` (no borrowed memory
                        // crosses), under the `state` lock — the sole topology mutator.
                        ccd_saved = unsafe { isolate_displays_ccd(added.target_id) };
                    }
                    Topology::Primary => {
                        // The IDD auto-activates as the SOLE display on a headless box, so the
                        // physical (if present) is deactivated and QueryDisplayConfig sees only the
                        // virtual. Force EXTEND first to (re)activate every CONNECTED display
                        // alongside the virtual, THEN reposition to make the virtual primary — so the
                        // physical stays active. (The bring-up above only force-EXTENDs when the
                        // virtual FAILS to auto-resolve; here it resolved, so we do it explicitly.)
                        // SAFETY: `force_extend_topology` drives the CCD topology FFI (no args, no borrowed
                        // memory), under the `state` lock — the sole topology mutator.
                        unsafe { force_extend_topology() };
                        thread::sleep(Duration::from_millis(300));
                        // SAFETY: `set_virtual_primary_ccd` takes the `Copy` target id by value and returns
                        // an owned `SavedConfig` (no borrowed memory crosses), under the `state` lock.
                        ccd_saved = unsafe { set_virtual_primary_ccd(added.target_id) };
                    }
                    Topology::Extend | Topology::Auto => {
                        tracing::info!(
                            "display topology=extend — IDD stays extended (no isolate / no primary)"
                        );
                    }
                }
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens
            }
            None => tracing::warn!(
                "virtual-display target {} not yet an active display path (needs a WDDM GPU to activate)",
                added.target_id
            ),
        }

        Ok(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            stop,
            pinger: Some(pinger),
            ccd_saved,
            gen: self.gen.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Re-apply a (possibly new) mode to a reused monitor on reconnect, re-resolving its GDI name.
    ///
    /// # Safety
    /// Touches the live display topology via the CCD/GDI helpers.
    unsafe fn reconfigure(&self, mon: &mut Monitor, mode: Mode) {
        tracing::info!(
            old = format!(
                "{}x{}@{}",
                mon.mode.width, mon.mode.height, mon.mode.refresh_hz
            ),
            new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
            "virtual-display: reconfiguring reused monitor to the new client mode"
        );
        // SAFETY: `resolve_gdi_name` is `unsafe` for its CCD FFI; it takes the `Copy` `u32`
        // `mon.target_id` by value and returns an owned `String`, so nothing borrowed crosses the call.
        if let Some(n) = unsafe { resolve_gdi_name(mon.target_id) } {
            mon.gdi_name = Some(n);
        }
        if let Some(n) = &mon.gdi_name {
            set_active_mode(n, mode);
        }
        mon.mode = mode;
    }

    /// Stop the watchdog ping, re-attach the displays we detached, then REMOVE the monitor. Consumes it.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn teardown(&self, dev: HANDLE, mut mon: Monitor) {
        mon.stop.store(true, Ordering::Relaxed);
        if let Some(j) = mon.pinger.take() {
            let _ = j.join();
        }
        // Re-attach detached display(s) BEFORE the REMOVE so the box is never left with zero displays.
        if let Some(saved) = &mon.ccd_saved {
            restore_displays_ccd(saved);
        }
        // SAFETY: `teardown`'s own `# Safety` contract guarantees `dev` is the live control handle, and
        // `remove_monitor` requires exactly that. `&mon.key` borrows the `MonitorKey` inside the
        // still-owned `mon`, alive for this synchronous IOCTL, so the pointer the driver reads stays valid.
        if let Err(e) = unsafe { self.driver.remove_monitor(dev, &mon.key) } {
            // A gone-classified failure means the device died under this monitor (driver upgrade /
            // WUDFHost restart) — retire the handle so the NEXT session reopens instead of failing.
            if is_device_gone(&e) {
                self.invalidate_device(&e);
            }
            tracing::warn!("virtual-display REMOVE failed: {e:#}");
        } else {
            tracing::info!(
                backend = self.driver.name(),
                "virtual-display monitor removed"
            );
        }
    }

    /// Release a session's hold (the [`MonitorLease`] `Drop`): refcount-- ; the last session leaving
    /// LINGERs before teardown. A STALE lease (its monitor was preempted + recreated under it) is a
    /// no-op, so it can't tear down the CURRENT monitor.
    fn release(&self, gen: u64) {
        let mut state = self.state.lock().unwrap();
        let stale = match &*state {
            MgrState::Active { mon, .. }
            | MgrState::Lingering { mon, .. }
            | MgrState::Pinned { mon } => mon.gen != gen,
            MgrState::Idle => true,
        };
        if stale {
            return;
        }
        *state = match std::mem::replace(&mut *state, MgrState::Idle) {
            MgrState::Active { mon, refs } if refs > 1 => MgrState::Active {
                mon,
                refs: refs - 1,
            },
            // Last session left: keep the monitor forever (Pinned) under `keep_alive = forever`,
            // else linger for the policy window before the timer tears it down.
            MgrState::Active { mon, .. } if keep_alive_forever() => {
                tracing::info!(
                    "virtual-display: last session left — PINNED (keep_alive=forever); free via /display/release"
                );
                MgrState::Pinned { mon }
            }
            MgrState::Active { mon, .. } => {
                let ms = linger_ms();
                tracing::info!(
                    linger_ms = ms,
                    "virtual-display: last session left — lingering before teardown"
                );
                MgrState::Lingering {
                    mon,
                    until: Instant::now() + Duration::from_millis(ms),
                }
            }
            other => other,
        };
    }

    /// Begin an IDD-push session setup (Goal-1 §2.5 — was the `IDD_SETUP_LOCK` / `IDD_SESSION_STOP` /
    /// `wait_for_monitor_released` dance smeared across `punktfunk1`). Serializes via the setup lock,
    /// registers THIS session's stop flag while signalling the PRIOR IDD-push session to stop, and waits
    /// for it to release its monitor — so a reconnect (whose reused IddCx swap-chain is dead) preempts the
    /// stale session cleanly before a fresh monitor is created. Returns the setup guard; the caller holds
    /// it across the pipeline build, then drops it so the next reconnect can begin (and preempt this one).
    pub(crate) fn begin_idd_setup(
        &'static self,
        stop: Arc<AtomicBool>,
    ) -> std::sync::MutexGuard<'static, ()> {
        let guard = self.setup_lock.lock().unwrap();
        let prev = self.idd_session_stop.lock().unwrap().replace(stop);
        if let Some(prev_stop) = prev {
            prev_stop.store(true, Ordering::SeqCst);
            if !self.wait_for_monitor_released(Duration::from_secs(3)) {
                // TIMEOUT: the prior session is STILL Active (a wedged/slow teardown). `acquire`'s preempt
                // is now Lingering-only (so build-retries JOIN the held monitor instead of churning
                // REMOVE→ADD), which means the upcoming `_retry_hold` acquire would JOIN this stuck monitor
                // and reuse its DEAD IddCx swap-chain → a full-session black screen with no self-heal until
                // this session disconnects. Force-preempt it HERE instead. This runs at most ONCE per
                // session (we hold `setup_lock`), so — unlike preempting inside `acquire` — it does not
                // reintroduce the per-retry churn. The next `acquire` then sees `Idle` and creates a fresh
                // monitor; the stale session's gen-stamped lease release is a no-op.
                if let Some(dev) = self.device_handle() {
                    let taken = {
                        let mut state = self.state.lock().unwrap();
                        match std::mem::replace(&mut *state, MgrState::Idle) {
                            MgrState::Active { mon, .. } => Some(mon),
                            // Raced to Lingering/Idle between the wait and here — restore + nothing stuck.
                            other => {
                                *state = other;
                                None
                            }
                        }
                    };
                    if let Some(mon) = taken {
                        tracing::warn!(
                            old_target = mon.target_id,
                            "IDD-push setup: force-preempting the stuck-Active prior monitor (its IddCx swap-chain is dead)"
                        );
                        // SAFETY: `teardown` requires `dev` to be the live control handle; `dev` is the
                        // cached process-lifetime `OwnedHandle` from `device_handle()` (the `Some` checked
                        // above). `mon` was moved out of the `Active` state under the `state` lock, so it is
                        // exclusively owned here — no aliasing.
                        unsafe { self.teardown(dev, mon) };
                        // Let the OS finish the ASYNC departure before the next ADD (mirrors the acquire()
                        // Lingering-preempt settle).
                        thread::sleep(Duration::from_millis(400));
                    }
                }
            }
        }
        guard
    }

    /// Wait (up to `timeout`) for the active monitor to be RELEASED (the MGR is no longer `Active`).
    /// Used by the IDD-push reconnect preempt: after signalling the old session to stop, wait here so it
    /// tears its monitor down cleanly before we acquire a fresh one. Returns `true` if it released, `false`
    /// on timeout (the prior session is still `Active` — the caller force-preempts it).
    pub(crate) fn wait_for_monitor_released(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !matches!(*self.state.lock().unwrap(), MgrState::Active { .. }) {
                return true;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    "IDD-push preempt: prior session didn't release the monitor within {timeout:?} — force-preempting"
                );
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Background timer (started once): tear down a monitor that has lingered past its deadline (→ Idle),
    /// so a physical-screen user gets their screen back after they stop streaming.
    fn ensure_linger_timer(&'static self) {
        static TIMER: Once = Once::new();
        TIMER.call_once(|| {
            thread::Builder::new()
                .name("vdisplay-linger".into())
                .spawn(move || loop {
                    thread::sleep(Duration::from_millis(500));
                    let Some(dev) = self.device_handle() else {
                        continue;
                    };
                    let mut g = self.state.lock().unwrap();
                    if !matches!(&*g, MgrState::Lingering { until, .. } if Instant::now() >= *until)
                    {
                        continue;
                    }
                    if let MgrState::Lingering { mon, .. } =
                        std::mem::replace(&mut *g, MgrState::Idle)
                    {
                        // Teardown UNDER the state lock. Dropping the lock first (the old shape) let a
                        // concurrent `acquire` see Idle and run its ADD + CCD isolate while this
                        // monitor's pinger-join / CCD-restore / REMOVE were still in flight — the late
                        // restore then de-isolated (or the REMOVE churn-rejected) the fresh session at
                        // the linger-expiry boundary. Holding the lock makes the racing acquire WAIT
                        // the few teardown seconds instead of failing its session. Lock order stays
                        // state → device (teardown's invalidate path), same as every other holder; the
                        // pinger takes only the device lock — no inversion.
                        // SAFETY: `teardown` requires a valid control handle; `dev` is from
                        // `self.device_handle()` (cached handles are never closed — a dead one is
                        // retired, kept alive; see `DeviceSlot`). `mon` was moved out of the replaced
                        // state under the lock, so it is exclusively owned here.
                        unsafe { self.teardown(dev, mon) };
                    }
                })
                .ok();
        });
    }
}

/// The session's refcount handle. `Drop` releases the manager's refcount; a stale lease (its monitor was
/// preempted + recreated under it) is a no-op.
struct MonitorLease {
    mgr: &'static VirtualDisplayManager,
    gen: u64,
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        self.mgr.release(self.gen);
    }
}

/// The render-GPU pin (backend-neutral): IDD-push — the sole Windows capture path — runs NVENC on the
/// render adapter, so it must always be pinned to the selected encoder GPU (a hybrid box would
/// otherwise render on the wrong one). The selection itself (web-console preference >
/// `PUNKTFUNK_RENDER_ADAPTER` > max VRAM) lives in [`crate::win_adapter::resolve_render_adapter_luid`].
/// (This was gated on the removed `PUNKTFUNK_IDD_PUSH` knob — a dispatch disagreement, since capture
/// stopped consulting it when DDA/WGC were removed.)
fn resolve_render_pin() -> Option<LUID> {
    tracing::info!("IDD push: pinning the render GPU (SET_RENDER_ADAPTER)");
    crate::win_adapter::resolve_render_adapter_luid()
}

/// A read-only view of the managed monitor for the mgmt `/display/state` endpoint (Goal:
/// display-management registry facade). Backend-neutral; the [`crate::vdisplay::registry`] facade
/// maps it into the wire shape.
pub(crate) struct ManagedInfo {
    pub backend: &'static str,
    pub mode: (u32, u32, u32),
    /// `"active"` | `"lingering"` | `"pinned"`.
    pub state: &'static str,
    /// Milliseconds until a lingering monitor is torn down (`None` when active).
    pub expires_in_ms: Option<u64>,
    /// Live sessions holding the monitor.
    pub sessions: u32,
    /// The monitor's generation stamp — a stable-enough id for the `/display/release` slot arg.
    pub gen: u64,
}

impl VirtualDisplayManager {
    /// Snapshot the current monitor for the mgmt `/display/state` endpoint. `None` when Idle.
    pub(crate) fn snapshot(&self) -> Option<ManagedInfo> {
        let st = self.state.lock().unwrap();
        let (mon, state, sessions, expires_in_ms) = match &*st {
            MgrState::Idle => return None,
            MgrState::Active { mon, refs } => (mon, "active", *refs, None),
            MgrState::Lingering { mon, until } => {
                let ms = until.saturating_duration_since(Instant::now()).as_millis() as u64;
                (mon, "lingering", 0u32, Some(ms))
            }
            // Pinned (keep_alive=forever): kept indefinitely, no expiry — the console shows "Pinned".
            MgrState::Pinned { mon } => (mon, "pinned", 0u32, None),
        };
        Some(ManagedInfo {
            backend: self.driver.name(),
            mode: (mon.mode.width, mon.mode.height, mon.mode.refresh_hz),
            state,
            expires_in_ms,
            sessions,
            gen: mon.gen,
        })
    }

    /// Force-tear-down a kept (LINGERING **or** PINNED) monitor now (the `/display/release` endpoint) —
    /// so a physical-screen user gets their screen back without waiting out the linger, and it is the §8
    /// escape hatch that frees a `keep_alive=forever` (Pinned) monitor. An Active monitor is refused
    /// (stopping a live session is session management, not display management). Returns `true` if a kept
    /// monitor was released.
    pub(crate) fn force_release(&self) -> bool {
        let Some(dev) = self.device_handle() else {
            return false;
        };
        let mut st = self.state.lock().unwrap();
        if matches!(&*st, MgrState::Lingering { .. } | MgrState::Pinned { .. }) {
            let mon = match std::mem::replace(&mut *st, MgrState::Idle) {
                MgrState::Lingering { mon, .. } | MgrState::Pinned { mon } => Some(mon),
                other => {
                    *st = other;
                    None
                }
            };
            if let Some(mon) = mon {
                // SAFETY: `teardown` needs a live control handle; `dev` is from `device_handle()`
                // (cached handles are never closed — a dead one is retired, kept alive; see
                // `DeviceSlot`). `mon` was moved out of the kept state under the `state` lock,
                // so it is exclusively owned here — no aliasing.
                unsafe { self.teardown(dev, mon) };
                return true;
            }
        }
        false
    }
}

/// Snapshot the managed monitor, or `None` when no backend has initialised the manager yet (no
/// session has ever run) or it is Idle. Safe to call per management request.
pub(crate) fn snapshot() -> Option<ManagedInfo> {
    VDM.get().and_then(VirtualDisplayManager::snapshot)
}

/// Force-release a lingering monitor now; `false` if nothing was lingering (or the manager is
/// uninitialised).
pub(crate) fn force_release() -> bool {
    VDM.get()
        .map(VirtualDisplayManager::force_release)
        .unwrap_or(false)
}

/// Linger window before a session-less monitor is torn down. The console display-management policy
/// wins when configured (`keep_alive`); otherwise the legacy `PUNKTFUNK_MONITOR_LINGER_MS` env knob,
/// else the 10 s default.
fn linger_ms() -> u64 {
    use crate::vdisplay::policy::{prefs, Linger};
    if let Some(eff) = prefs().configured_effective() {
        return match eff.keep_alive.linger() {
            Linger::Immediate => 0,
            Linger::For(d) => d.as_millis() as u64,
            // `forever` is handled BEFORE this by `keep_alive_forever()` in `release` (→ `Pinned`), so
            // this arm is only reached defensively (e.g. a caller that resolves ms without the pin
            // check) — fall back to the default rather than a huge linger.
            Linger::Forever => 10_000,
        };
    }
    std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Whether the configured console policy's `keep_alive` resolves to **forever** (`Pinned`) — the
/// gaming-rig preset. `release` uses this to keep the last-released monitor indefinitely instead of
/// lingering. Unconfigured hosts are never forever (default is a short linger).
fn keep_alive_forever() -> bool {
    use crate::vdisplay::policy::{prefs, Linger};
    prefs()
        .configured_effective()
        .map(|eff| matches!(eff.keep_alive.linger(), Linger::Forever))
        .unwrap_or(false)
}

/// The effective display topology for a freshly-created monitor (never `Auto`): the console policy's
/// [`effective_topology`](crate::vdisplay::effective_topology) when configured, else the legacy
/// `PUNKTFUNK_NO_ISOLATE` env knob (`Extend`) / `Exclusive` (today's default). `Extend` leaves the IDD
/// extended; `Primary` makes it primary while keeping the physical(s) active; `Exclusive` disables the
/// physical(s) so the IDD is the sole composited desktop.
fn topology_action() -> crate::vdisplay::policy::Topology {
    use crate::vdisplay::policy::Topology;
    if crate::vdisplay::policy::prefs()
        .configured_effective()
        .is_some()
    {
        return crate::vdisplay::effective_topology();
    }
    if std::env::var("PUNKTFUNK_NO_ISOLATE").is_ok() {
        Topology::Extend
    } else {
        Topology::Exclusive
    }
}
