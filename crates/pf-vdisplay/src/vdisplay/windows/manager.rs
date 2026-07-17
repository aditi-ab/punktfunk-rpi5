//! Host-lifetime virtual-display **ownership model** (Goal-1 §2.5). One reference-counted monitor
//! lifecycle, shared by both Windows backends (SudoVDA + pf-vdisplay) instead of the two verbatim-
//! duplicated `MGR: Mutex<Mgr>` globals each backend used to carry.
//!
//! [`VirtualDisplayManager`] owns the earned Idle/Active/Lingering refcount machine + the linger timer +
//! a **typed** [`OwnedHandle`] control device (no more raw `isize` smuggled across the pinger/linger
//! threads). The backend differences — the IOCTL protocol and the per-monitor REMOVE key — are the only
//! thing behind the [`VdisplayDriver`] seam; the state machine, the render-adapter pin decision, the
//! GDI/CCD glue (`pf_win_display::win_display`), and the generation-stamped [`MonitorLease`] are backend-neutral.
//!
//! It's a process-wide singleton ([`vdm`]) initialised once with the chosen backend's driver — the
//! host runs exactly one virtual-display backend per process. The session holds a [`MonitorLease`];
//! its `Drop` releases the refcount (a *stale* lease — its monitor was preempted + recreated under it —
//! is a no-op, so it can never tear down the live monitor).

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use std::collections::BTreeMap;
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

use super::{DisplayOwnership, Mode, VirtualOutput};
use pf_win_display::win_display::{
    count_other_active, force_extend_topology, isolate_displays_ccd, resolve_gdi_name,
    restore_displays_ccd, set_active_mode, set_virtual_primary_ccd, SavedConfig,
};

#[path = "manager/driver.rs"]
mod driver;
pub(crate) use driver::{AddedMonitor, MonitorKey, VdisplayDriver};

#[path = "manager/instance.rs"]
mod instance;
use instance::claim_instance;
pub use instance::claim_instance_eagerly;

#[path = "manager/knobs.rs"]
mod knobs;
use knobs::{keep_alive_forever, linger_ms, topology_action};

/// The resources backing one live virtual monitor (owned by the [`VirtualDisplayManager`] state, not by
/// any session). No `Drop` impl — [`teardown_removed`](VirtualDisplayManager::teardown_removed) must be
/// called so the REMOVE IOCTL fires (a bare drop would orphan the driver-side monitor).
///
/// Since the Stage-W1 slot map, what is GROUP-scoped no longer lives here: the CCD `SavedConfig`,
/// `ddc_panels_off` and `pnp_disabled` moved to [`GroupState`] (first-in captures, last-out restores —
/// `design/display-management.md` §6.1), and the per-monitor watchdog pinger became ONE device-level
/// pinger (any IOCTL bumps the driver watchdog; per-monitor pingers were redundancy, not correctness).
struct Monitor {
    key: MonitorKey,
    target_id: u32,
    /// IddCx DISPLAY adapter LUID from the ADD reply (`IDARG_OUT_MONITORARRIVAL.OsAdapterLuid`) —
    /// NOT the render GPU the driver renders on (the driver reports that one in the shared frame
    /// header only). Do not compare it against render-GPU picks.
    luid: LUID,
    /// The render-GPU pin handed to SET_RENDER_ADAPTER at this monitor's ADD (`None` = no GPU was
    /// selectable). The pin is never re-issued on reuse, so this is what the driver still renders
    /// on — [`warn_if_pick_moved`] compares the CURRENT pick against it.
    render_pin: Option<LUID>,
    /// The driver's WUDFHost pid (from the ADD reply) — carried into [`WinCaptureTarget`] so the
    /// IDD-push capturer knows where to duplicate the sealed frame channel's handles. The SAME
    /// process for every parallel monitor (one devnode → one WUDFHost hosts all publishers), which
    /// is why WUDFHost death is ALL-slot shared fate.
    wudf_pid: u32,
    gdi_name: Option<String>,
    mode: Mode,
    /// The monitor id the driver actually resolved (the EDID serial / ConnectorIndex) — equals the
    /// slot key when the per-client preference was honored, or the auto-allocated id (diagnostics).
    resolved_monitor_id: u32,
    /// This monitor's desktop-space origin from the group layout (`(0,0)` until a multi-slot
    /// arrangement places it) — reported via [`ManagedInfo`].
    position: (i32, i32),
    /// Generation stamp; a [`MonitorLease`] only releases if its gen still matches (stale-lease no-op).
    gen: u64,
}

impl Monitor {
    /// The capture target handed to a session (`None` until the GDI name resolves on a WDDM GPU).
    fn target(&self) -> Option<pf_frame::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| pf_frame::dxgi::WinCaptureTarget {
                adapter_luid: pf_frame::dxgi::pack_luid(self.luid),
                gdi_name: n,
                target_id: self.target_id,
                wudf_pid: self.wudf_pid,
            })
    }
}

/// One slot's state — today's per-monitor machine, per entry (an Idle slot is simply absent from the
/// map; `design/windows-parallel-virtual-displays.md` §4.1 / Stage W1).
enum SlotState {
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

impl SlotState {
    fn mon(&self) -> &Monitor {
        match self {
            SlotState::Active { mon, .. }
            | SlotState::Lingering { mon, .. }
            | SlotState::Pinned { mon } => mon,
        }
    }
}

/// Group-scoped topology state (ONE group on Windows — the shared desktop,
/// `design/display-management.md` §6.1): captured by the FIRST slot's isolate, restored when the LAST
/// member drops. Per-monitor restore would flash the physical panels back between sibling sessions.
#[derive(Default)]
struct GroupState {
    /// The pre-isolate active config (first slot's snapshot) — teardown restores it on last-member
    /// drop. `Some` also marks "an exclusive isolate is live", so slot add/remove re-issues the
    /// isolate with the grown/shrunk managed set.
    ccd_saved: Option<SavedConfig>,
    /// How many physical panels acknowledged the EXPERIMENTAL DDC/CI off command at the group's
    /// first isolate (`ddc_power_off` policy axis) — last-member teardown wakes them after the CCD
    /// restore iff > 0.
    ddc_panels_off: u32,
    /// PnP instance ids of monitor devnodes the EXPERIMENTAL `pnp_disable_monitors` axis disabled at
    /// the group's first isolate — last-member teardown re-enables them BEFORE the CCD restore.
    pnp_disabled: Vec<String>,
}

/// The manager's guarded state: the slot map + the (single) group record. One lock for both — every
/// group mutation happens on a slot transition, so splitting them would only invite lock-order bugs.
#[derive(Default)]
struct MgrInner {
    /// Live/kept slots, keyed by the SLOT id: the client's stable identity slot (`1..=15`,
    /// `identity::resolve_slot`) — what is stable per client across reconnects — or `0` for
    /// anonymous/GameStream sessions (at most one at a time, exactly the pre-slot-map semantics; an
    /// anonymous re-acquire has no identity to find any other slot by).
    slots: BTreeMap<u32, SlotState>,
    group: GroupState,
}

impl MgrInner {
    /// Live target ids in acquire (gen) order — the CCD isolate keep-set + the layout member order.
    fn target_ids(&self) -> Vec<u32> {
        let mut mons: Vec<&Monitor> = self.slots.values().map(SlotState::mon).collect();
        mons.sort_by_key(|m| m.gen);
        mons.iter().map(|m| m.target_id).collect()
    }
}

/// The single device-level watchdog pinger, running while ANY slot lives (any IOCTL bumps the driver
/// watchdog, so one thread serves N monitors).
struct Pinger {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
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
pub struct VirtualDisplayManager {
    driver: Box<dyn VdisplayDriver>,
    /// Control device, opened on first acquire and REOPENED after a gone-classified failure retired
    /// it (see [`DeviceSlot`]). Typed + `Send+Sync`, so the pinger/linger threads share it via the
    /// `&'static` singleton with no raw-handle smuggling.
    device: Mutex<DeviceSlot>,
    watchdog_s: AtomicU32,
    /// Monotonic lease-generation counter (was the `MON_GEN` global).
    gen: AtomicU64,
    state: Mutex<MgrInner>,
    /// Serializes IDD-push session SETUP (preempt + monitor create) — MANAGER-WIDE even with slots:
    /// monitor create/teardown stays serialized (the 400 ms async-departure settle and the IddCx
    /// slot-budget wedge both want zero concurrent ADD/REMOVE). Held by the session across the
    /// pipeline build (was the `IDD_SETUP_LOCK` global in `native`).
    setup_lock: Mutex<()>,
    /// Per-SLOT IDD-push session stop flags: a new connection signals only the stop of a session
    /// holding *that identity's* slot (the same-client zombie-reconnect preempt, slot-scoped since
    /// Stage W1 — a different identity is an ADMISSION question, never a preempt). Entries persist
    /// per slot (bounded at 16); signaling an ended session's flag is harmless.
    idd_session_stops: Mutex<std::collections::HashMap<u32, Arc<AtomicBool>>>,
    /// The device-level watchdog [`Pinger`], running while any slot lives.
    pinger: Mutex<Option<Pinger>>,
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
        state: Mutex::new(MgrInner::default()),
        setup_lock: Mutex::new(()),
        idd_session_stops: Mutex::new(std::collections::HashMap::new()),
        pinger: Mutex::new(None),
    })
}

/// The process-wide manager. Panics if reached before a backend called [`init`] — by construction a
/// session is only ever created after `vdisplay::open` constructed the backend (which calls `init`).
pub fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

/// The live pf-vdisplay control-device handle, for the IDD-push capturer's sealed-channel delivery
/// (`IOCTL_SET_FRAME_CHANNEL`). Safe to hand out as a bare `HANDLE`: cached handles are never closed
/// for the process lifetime — a dead one is RETIRED (kept alive, see [`DeviceSlot`]), so a stale copy
/// can only fail IOCTLs, never dangle. `None` before the first backend open — impossible for a
/// capturer, which only exists on a monitor the manager created.
pub fn control_device_handle() -> Option<HANDLE> {
    VDM.get().and_then(VirtualDisplayManager::device_handle)
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

/// True when an IOCTL failure means the CONTROL DEVICE itself is gone (driver upgrade, WUDFHost
/// restart, device disable) — the cached handle can only keep failing and must be retired so the
/// next use reopens. The root `windows` error survives anyhow `.context` chains via `downcast_ref`.
/// NOTE: 0x80070490 (ERROR_NOT_FOUND, the ADD slot-exhaustion wedge) is deliberately NOT here — it
/// has its own reap-and-retry handling and the device is alive when it fires.
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

    /// Acquire this client's slot for a new session: preempt-recreate under IDD-push, join its live
    /// monitor (refcount++), or create one. `client_fp` (the connecting client's cert fingerprint;
    /// `None` = anonymous/GameStream) keys the SLOT (`slot_id_for`) and gives a freshly CREATED
    /// monitor the client's STABLE per-client id (so Windows reapplies its saved per-monitor
    /// config). One live slot behaves exactly like the pre-slot-map singleton; a second identity
    /// gets its OWN slot → own monitor → own sealed ring (Stage W1/W3). The returned
    /// [`MonitorLease`] releases the slot's refcount on drop.
    pub(crate) fn acquire(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        quit: Option<Arc<AtomicBool>>,
    ) -> Result<VirtualOutput> {
        // Console-session guard: a host outside the ACTIVE console session cannot drive the display
        // it is about to create — every SetDisplayConfig/CDS write fails ERROR_ACCESS_DENIED, GDI
        // reads describe the wrong session's displays, and SendInput compose kicks go nowhere; the
        // session then dies far downstream as "no frame published within 4s" (the lid-closed field
        // report). Name the real problem up front, once per acquire. Non-fatal: the OS-side
        // persistence-DB activation can still succeed, so the attempt proceeds.
        if let Some((own, console)) = pf_win_display::console_session_mismatch() {
            tracing::error!(
                own_session = own,
                console_session = console,
                "punktfunk-host is NOT in the active console session — display activation, \
                 mode-set and capture will fail (disconnected RDP session?). Reconnect the \
                 console (`tscon {own} /dest:console`) or run the host via the installed \
                 service, which follows the console session"
            );
        }
        self.ensure_linger_timer();
        let slot = slot_id_for(client_fp, (mode.width, mode.height));
        let mut inner = self.state.lock().unwrap();
        let dev = self.ensure_device()?;

        // IDD-push: a new connection while THIS SLOT's monitor is kept (LINGERING or PINNED) is a
        // single-client RECONNECT (the prior session fully released). A REUSED IddCx swap-chain is
        // DEAD, so reusing it hands a black screen — PREEMPT: tear the kept monitor down and create a
        // fresh one. The old session's lease is gen-stamped, so its later drop is a no-op. A SIBLING
        // slot's kept monitor is never touched — that's another client's display.
        //
        // ONLY the kept states, NOT Active: an Active monitor still has a lease held — that's the
        // build-retry path (`build_pipeline_with_retry` holds one lease across all attempts) or a
        // concurrent same-client session, NOT a reconnect. Preempting Active would tear a live session
        // down AND churn REMOVE→ADD on every retry — the per-cold-start monitor churn that exhausts
        // the IddCx slot pool and wedges ADD at 0x80070490. Active falls through to the JOIN path
        // below (refcount++, no ADD).
        if matches!(
            inner.slots.get(&slot),
            Some(SlotState::Lingering { .. } | SlotState::Pinned { .. })
        ) {
            if let Some(SlotState::Lingering { mon, .. } | SlotState::Pinned { mon }) =
                inner.slots.remove(&slot)
            {
                tracing::info!(
                    slot,
                    old_target = mon.target_id,
                    "IDD-push reconnect — preempting the kept (lingering/pinned) monitor, recreating a fresh one"
                );
                // SAFETY: `teardown_removed` requires `dev` to be a valid control handle; `dev` is the
                // value `ensure_device()` returned above (cached handles are never closed — a dead one
                // is retired, kept alive; see `DeviceSlot`). `mon` was just removed from the map, so it
                // is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev, &mut inner, mon) };
                // Let the OS finish the ASYNC monitor departure before the next ADD; a back-to-back
                // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
                thread::sleep(Duration::from_millis(400));
            }
        }

        // An ACTIVE monitor whose WUDFHost has EXITED is dead driver-side (driver crash / upgrade):
        // the capturer's driver-death watch failed its session, and that session's in-place rebuild
        // re-acquires here while its old lease is STILL held — so the slot is Active. Joining would
        // hand the rebuild the dead monitor's target (stale wudf_pid) and starve it to the rebuild
        // budget. Preempt instead: best-effort teardown (REMOVE fails harmlessly on a dead/retired
        // device) and fall through to a fresh create on the auto-restarted device. Held leases are
        // gen-stamped, so their eventual release is a no-op. ONE WUDFHost hosts every slot's
        // publisher, so its death is ALL-slot shared fate — but each sibling's session fails through
        // its own capturer watch and rebuilds through this same path; no cross-slot teardown here.
        if matches!(inner.slots.get(&slot), Some(SlotState::Active { mon, .. }) if !wudf_alive(mon.wudf_pid))
        {
            if let Some(SlotState::Active { mon, .. }) = inner.slots.remove(&slot) {
                tracing::warn!(
                    slot,
                    old_target = mon.target_id,
                    wudf_pid = mon.wudf_pid,
                    "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
                );
                // SAFETY: `teardown_removed` requires a valid control handle; `dev` is the value
                // `ensure_device()` returned above (cached handles are never closed — a dead one is
                // retired, kept alive; see `DeviceSlot`). `mon` was just removed from the map, so it
                // is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev, &mut inner, mon) };
                // Same async-departure settle as the reconnect preempt above.
                thread::sleep(Duration::from_millis(400));
            }
        }

        // This slot already has a live monitor — join it (refcount++). Covers same-client concurrent
        // sessions AND the build-then-drop overlap of a mid-stream Reconfigure (the new lease is taken
        // while the old is still held).
        if matches!(inner.slots.get(&slot), Some(SlotState::Active { .. })) {
            // A DIFFERENT mode is a mid-stream resize (Reconfigure). The pf-vdisplay driver freezes its
            // advertised mode list at ADD time, so we can't reach an arbitrary new mode in place — RE-
            // ARRIVE the monitor at the exact mode instead (Fix 1). Own the slot for the swap: `re_add`
            // needs `&mut inner` for the topology re-isolate, which the borrowed `mon` would block.
            let cur_mode = match inner.slots.get(&slot) {
                Some(SlotState::Active { mon, .. }) => mon.mode,
                _ => unreachable!("just matched Active"),
            };
            if cur_mode != mode {
                let Some(SlotState::Active { mon, refs }) = inner.slots.remove(&slot) else {
                    unreachable!("just matched Active");
                };
                // SAFETY: `dev` is the handle `ensure_device()` returned above; `re_add` touches the
                // live topology under the held `state` lock. `mon` is owned here (removed from the map).
                let new_mon =
                    match unsafe { self.re_add(dev, &mut inner, slot, &mon, mode, client_hdr) } {
                        Ok(m) => m,
                        Err(e) => {
                            // The re-arrival failed — put the OLD monitor back so the session keeps
                            // streaming its current mode (the control task already acked the switch; the
                            // rebuild reuses the old target and Fix 2's corrective ack tells the client the
                            // resolution didn't change). Its `gen`/`refs` are intact, so leases stay valid.
                            inner.slots.insert(slot, SlotState::Active { mon, refs });
                            return Err(e).context("mid-stream resize re-arrival");
                        }
                    };
                // `re_add` preserved `gen`, so both the old session's lease and this new one match on
                // release. +1 ref for the new (build-then-drop overlap) lease.
                let out = self.output_for(slot, &new_mon, quit);
                inner.slots.insert(
                    slot,
                    SlotState::Active {
                        mon: new_mon,
                        refs: refs + 1,
                    },
                );
                // The width changed — re-arrange the group so auto-row siblings don't overlap the
                // resized display (no-op for a single member).
                self.apply_group_layout(&mut inner);
                tracing::info!(
                    slot,
                    refs = refs + 1,
                    backend = self.driver.name(),
                    "virtual monitor re-arrived for a mid-stream resize"
                );
                return Ok(out);
            }
            // Same mode — a plain concurrent-session JOIN (refcount++), no re-arrival.
            let Some(SlotState::Active { mon, refs }) = inner.slots.get_mut(&slot) else {
                unreachable!("just matched Active");
            };
            *refs += 1;
            tracing::info!(
                slot,
                refs = *refs,
                backend = self.driver.name(),
                "virtual monitor reused (concurrent session)"
            );
            warn_if_pick_moved(mon);
            return Ok(self.output_for(slot, mon, quit));
        }

        // Display budget (Stage W3): a display we can't afford is DECLINED at admission
        // (`max_displays` across Active+Lingering+Pinned slots; the identity-slot ceiling of 15 is
        // the hard limit behind it) — this is the fail-closed backstop for a session that got past
        // admission anyway. One live slot can never trip it (max_displays >= 1).
        let max = crate::policy::prefs().get().effective().max_displays;
        if inner.slots.len() as u32 >= max {
            anyhow::bail!(
                "display budget exhausted: {} display(s) live/kept, max_displays = {max} — freeing \
                 one (session end, linger expiry, or /display/release) admits the next",
                inner.slots.len()
            );
        }

        // The slot is empty: create a fresh monitor for it.
        // SAFETY: `create_monitor` requires `dev` to be a valid control handle; `dev` is the handle
        // `ensure_device()` returned above (cached handles are never closed — a dead one is retired,
        // kept alive; see `DeviceSlot`), and we hold the `state` lock.
        let mon = match unsafe { self.create_monitor(dev, mode, slot, client_hdr, &mut inner) } {
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
                unsafe { self.create_monitor(dev, mode, slot, client_hdr, &mut inner)? }
            }
            r => r?,
        };
        let out = self.output_for(slot, &mon, quit);
        inner.slots.insert(slot, SlotState::Active { mon, refs: 1 });
        // Multi-slot group layout (§6.2): arrange the live members (auto-row / manual pins) and
        // commit their desktop origins in one CCD apply. A single member sits at the origin and this
        // no-ops — the single-display path issues no positioning at all.
        self.apply_group_layout(&mut inner);
        Ok(out)
    }

    /// Build the [`VirtualOutput`] (preferred mode + capture target + a fresh gen-stamped lease) for
    /// `mon` in `slot`. `quit` is the session's deliberate-quit flag, read by the lease `Drop` (see
    /// [`Self::release`]).
    fn output_for(
        &'static self,
        slot: u32,
        mon: &Monitor,
        quit: Option<Arc<AtomicBool>>,
    ) -> VirtualOutput {
        VirtualOutput {
            node_id: 0,
            preferred_mode: Some((mon.mode.width, mon.mode.height, mon.mode.refresh_hz)),
            win_capture: mon.target(),
            keepalive: Box::new(MonitorLease {
                mgr: self,
                slot,
                gen: mon.gen,
                quit,
            }),
            // The Windows manager owns the monitor lifecycle (refcount/linger/pin), so the registry
            // (which delegates to it via `vd.create`) treats it as Owned.
            ownership: DisplayOwnership::Owned,
        }
    }

    /// Start the device-level watchdog pinger if it isn't running (first slot), so the driver's
    /// host-gone watchdog stays satisfied while ANY monitor lives. One thread serves every slot —
    /// any IOCTL bumps the watchdog, so per-monitor pingers were redundancy, not correctness.
    fn ensure_pinger(&'static self) {
        let mut guard = self.pinger.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let interval =
            Duration::from_millis(self.watchdog_s.load(Ordering::Relaxed) as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let thread = thread::spawn(move || {
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
                            // `ensure_device` reopens; the monitors are already dead driver-side.
                            vdm().invalidate_device(&e);
                        }
                        Err(e) => {
                            if !warned {
                                tracing::warn!(
                                    "virtual-display keepalive PING failed (control handle lost?): {e:#}"
                                );
                                warned = true;
                            }
                        }
                    }
                }
                thread::sleep(interval);
            }
        });
        *guard = Some(Pinger { stop, thread });
    }

    /// Stop + join the device-level pinger (the LAST slot was just torn down). The join is bounded
    /// by the ping interval (watchdog/3 — seconds), same as the old per-monitor pinger join.
    fn stop_pinger(&self) {
        if let Some(p) = self.pinger.lock().unwrap().take() {
            p.stop.store(true, Ordering::Relaxed);
            let _ = p.thread.join();
        }
    }

    /// Arrange the live slots' desktop origins (design §6.2: the pure `vdisplay/layout.rs` engine —
    /// `auto-row` default, console `manual` pins win) and commit them in one CCD apply. No-ops for a
    /// single member (it sits at the origin), so the single-display path issues no positioning at
    /// all. Records each monitor's applied position for the `/display/state` readout.
    fn apply_group_layout(&self, inner: &mut MgrInner) {
        use crate::layout::{arrange, Member};
        if inner.slots.len() < 2 {
            return;
        }
        let layout_policy = crate::policy::prefs().get().effective().layout;
        // Members in acquire (gen) order — the auto-row order; identity slot 0 = anonymous (no
        // manual pin can address it, so it always auto-rows). `(slot, gen, target_id, width)`
        // copied out so the arrangement below can write back through `get_mut`.
        let mut ordered: Vec<(u32, u64, u32, i32)> = inner
            .slots
            .iter()
            .map(|(slot, s)| {
                let m = s.mon();
                (*slot, m.gen, m.target_id, m.mode.width as i32)
            })
            .collect();
        ordered.sort_by_key(|&(_, gen, _, _)| gen);
        let members: Vec<Member> = ordered
            .iter()
            .map(|&(slot, _, _, width)| Member {
                identity_slot: (slot != 0).then_some(slot),
                width,
            })
            .collect();
        let placements = arrange(&members, &layout_policy);
        let positions: Vec<(u32, i32, i32)> = ordered
            .iter()
            .zip(&placements)
            .map(|(&(_, _, target, _), p)| (target, p.x, p.y))
            .collect();
        // SAFETY: `apply_source_positions` only drives the CCD query/apply FFI with owned local
        // buffers, under the `state` lock — the sole topology mutator.
        unsafe { pf_win_display::win_display::apply_source_positions(&positions) };
        for (&(slot, ..), p) in ordered.iter().zip(&placements) {
            if let Some(
                SlotState::Active { mon, .. }
                | SlotState::Lingering { mon, .. }
                | SlotState::Pinned { mon },
            ) = inner.slots.get_mut(&slot)
            {
                mon.position = (p.x, p.y);
            }
        }
    }

    /// Create a fresh monitor at `mode` for `slot` (the client's stable identity slot, `0` = auto):
    /// Wait for Windows to auto-activate a freshly-ADDed IDD target into its OWN display path and
    /// return its GDI name — the capture target. Shared by the fresh CREATE and the mid-stream
    /// re-arrival ([`re_add`](Self::re_add)).
    ///
    /// The IDD comes up EXTENDED alongside any existing/basic display; the caller then promotes it to
    /// primary / isolates it. Returns `None` on a GPU-less box (target added but not WDDM-activated) —
    /// the capture backend re-resolves once a GPU is present.
    ///
    /// We do NOT force a topology change FIRST: the bare `SDC_TOPOLOGY_EXTEND` preset is ACCESS_DENIED
    /// from our Session-0 service context on a headless box and BREAKS this auto-activate (it regressed
    /// the headless path — the IDD then never gets its own path → "not an active display path" → black).
    /// force-EXTEND is only the FALLBACK, for an integrated-screen box (e.g. a laptop panel) where a
    /// fresh IDD is CLONED onto the existing display, sharing its source, so it never gets its own
    /// committed path (observed on an Intel-iGPU + NVIDIA-Optimus laptop, commit 8e87e61):
    /// `resolve_gdi_name` stays None → the `is_none()` fallback force-EXTENDs to de-clone and the
    /// second resolve finds the now-committed path. Headless/extended boxes resolve on the first loop
    /// and skip it — which is the point, since force-EXTEND is ACCESS_DENIED from our service context
    /// there.
    ///
    /// CAVEAT (unobserved for IddCx, untested across GPU/driver/OS): textbook CCD also lets a clone
    /// appear as a *shared-source ACTIVE* path (resolve → Some), which the `is_none()` gate would NOT
    /// catch. If that ever shows up, widen the gate to also fire when the IDD target's source is shared
    /// with another active path (a `target_is_cloned` helper) — needs on-laptop validation first.
    ///
    /// LAST RESORT — explicit path activation: a lid-closed laptop (field report, Intel iGPU) defeats
    /// BOTH stages above — the clamshell lid policy suppresses the new-monitor auto-activation, and
    /// the `SDC_TOPOLOGY_EXTEND` preset "succeeds" without committing a path for the IDD — so the
    /// target stays connected-but-inactive for the session's whole retry budget. `activate_target_path`
    /// commits the target's path directly (supplied-config apply, the same thing display Settings
    /// does), which doesn't consult the lid policy at all.
    ///
    /// # Safety
    /// Runs the CCD (QueryDisplayConfig / SetDisplayConfig) FFI; call under the `state` lock.
    unsafe fn resolve_target_gdi(&self, target_id: u32) -> Option<String> {
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            // SAFETY: `resolve_gdi_name` is `unsafe` for its CCD FFI; it takes a plain `Copy` `u32`
            // target id by value and returns an owned `String`, so no caller memory is borrowed.
            if let Some(n) = unsafe { resolve_gdi_name(target_id) } {
                return Some(n);
            }
        }
        // SAFETY: `force_extend_topology` only calls `SetDisplayConfig` (CCD) with no borrowed memory.
        unsafe { force_extend_topology() };
        for _ in 0..15 {
            thread::sleep(Duration::from_millis(200));
            // SAFETY: as the resolve loop above.
            if let Some(n) = unsafe { resolve_gdi_name(target_id) } {
                return Some(n);
            }
        }
        // SAFETY: `activate_target_path` runs the CCD query/apply FFI with owned local buffers; the
        // `Copy` target id is passed by value, under the `state` lock — the sole topology mutator.
        if unsafe { pf_win_display::win_display::activate_target_path(target_id) } {
            for _ in 0..15 {
                thread::sleep(Duration::from_millis(200));
                // SAFETY: as the resolve loops above.
                if let Some(n) = unsafe { resolve_gdi_name(target_id) } {
                    return Some(n);
                }
            }
        }
        None
    }

    /// ADD via the driver (pinning the discrete render GPU under the usual conditions), ensure the
    /// device-level watchdog pinger, resolve the GDI name, force the mode + apply the GROUP topology
    /// (first member isolates and captures the restore; a later member re-issues the isolate with
    /// the grown managed set — a sibling slot is never deactivated).
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn create_monitor(
        &'static self,
        dev: HANDLE,
        mode: Mode,
        slot: u32,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        inner: &mut MgrInner,
    ) -> Result<Monitor> {
        // The slot id doubles as the driver-preferred monitor id (EDID serial / ConnectorIndex), so
        // Windows reapplies the client's saved per-monitor config (DPI scaling) on reconnect;
        // `0` (anonymous) = the driver auto-allocates the lowest-free id.
        let preferred_id = slot;
        let render_pin = resolve_render_pin();
        // SAFETY: `create_monitor`'s own `# Safety` contract guarantees `dev` is the live control
        // handle; we forward it unchanged to `add_monitor`, whose precondition is exactly that.
        // `render_pin` is an `Option<LUID>` by value (plain `Copy`), so no borrowed memory
        // crosses the call.
        let added = unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, preferred_id, client_hdr)?
        };

        // Mandatory keepalive: ping inside the watchdog window or the driver tears all displays down.
        // ONE device-level pinger serves every slot (any IOCTL bumps the watchdog); started with the
        // first monitor, stopped when the last slot is torn down.
        self.ensure_pinger();

        // Resolve the capture target — wait for Windows to auto-activate the freshly-ADDed IDD into its
        // OWN display path, with the integrated-screen clone fallback (shared by the re-arrival path).
        // SAFETY: `resolve_target_gdi` runs the CCD FFI (a `Copy` `u32` target by value, owned return),
        // under the `state` lock.
        let gdi_name = unsafe { self.resolve_target_gdi(added.target_id) };
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    target_id = added.target_id,
                    gdi = %n,
                    "IDD target activated into a display path"
                );
                // ADD only advertises the mode; force it active so DXGI captures the requested size.
                set_active_mode(n, mode);
                // Apply the display-management topology (Stage 2, GROUP-scoped since Stage W2).
                // `Exclusive` (default) deactivates every non-managed display so the IDD set is the
                // sole composited desktop — an EXTENDED (non-primary) IDD isn't DWM-composited on
                // this box → Desktop Duplication born-losts. The FIRST member captures the restore
                // snapshot (+ the experimental DDC/PnP axes); a LATER member only re-issues the
                // isolate with the GROWN managed set (design §6.1 — never deactivate a sibling
                // slot; the first snapshot is what teardown restores on last-member drop).
                // `Primary` keeps the physical display(s) ACTIVE and makes the FIRST member primary
                // (the group's designated member); later members just extend + get arranged by the
                // group layout. `Extend` leaves it a plain extension. Both isolate + primary go
                // through the atomic CCD path (no MODE_CHANGE storm). Opt out (extend) with
                // PUNKTFUNK_NO_ISOLATE=1 / the console policy.
                use crate::policy::Topology;
                let first_member = inner.slots.is_empty();
                match topology_action() {
                    Topology::Exclusive => {
                        // The managed keep-set: every live sibling + the new monitor.
                        let mut keep = inner.target_ids();
                        keep.push(added.target_id);
                        if first_member {
                            // EXPERIMENTAL `ddc_power_off` policy axis: command the physical panels
                            // dark over DDC/CI BEFORE the isolate — an HMONITOR (and with it the DDC
                            // channel) only exists while the display is still active. A panel that
                            // believes it has an owner skips its no-signal standby probing — the
                            // suspected source of the periodic sole-virtual-display stutter (the
                            // rationale + evidence live in `windows/ddc.rs`). First member only:
                            // the physicals are already dark for a sibling.
                            if crate::policy::prefs().ddc_power_off() {
                                inner.group.ddc_panels_off = crate::ddc::panel_off_except(n);
                            }
                            // SAFETY: `isolate_displays_ccd` is `unsafe` for its CCD topology FFI; it
                            // takes a borrowed slice of `Copy` target ids (alive across the call) and
                            // returns an owned `SavedConfig`, under the `state` lock — the sole
                            // topology mutator.
                            inner.group.ccd_saved = unsafe { isolate_displays_ccd(&keep) };
                            // EXPERIMENTAL `pnp_disable_monitors` policy axis: AFTER the isolate took,
                            // additionally disable the deactivated monitors' PnP devnodes (persistent
                            // across hot-plug re-arrival) so a standby monitor/TV's periodic wake
                            // events no longer trigger the Windows reaction cascade — the suspected
                            // hiccup mechanism (rationale + crash journal in `windows/monitor_devnode.rs`).
                            if crate::policy::prefs().pnp_disable_monitors() {
                                if let Some(saved) = &inner.group.ccd_saved {
                                    inner.group.pnp_disabled =
                                        pf_win_display::monitor_devnode::disable_for_deactivated(
                                            saved,
                                            added.target_id,
                                        );
                                }
                            }
                        } else {
                            // Grown set: re-isolate so the fresh member joins the composited set
                            // (its auto-activate may have lit nothing extra to deactivate, but the
                            // re-commit also drives COMMIT_MODES for the new path). The returned
                            // snapshot is DISCARDED — the group restores the FIRST member's.
                            // SAFETY: as above — borrowed slice of Copy ids, owned return, under the
                            // `state` lock.
                            let _ = unsafe { isolate_displays_ccd(&keep) };
                        }
                    }
                    Topology::Primary if first_member => {
                        // On a headless box the IDD auto-activates as the SOLE display, so a physical
                        // (if present) is deactivated and QueryDisplayConfig sees only the virtual —
                        // force EXTEND to (re)activate every connected display alongside the virtual,
                        // THEN reposition to make the virtual primary. BUT on a box whose physical is
                        // ALREADY active (the IDD came up extended beside it — the common desktop case),
                        // that physical is already lit at its real mode; re-applying the bare
                        // `SDC_TOPOLOGY_EXTEND` preset would only re-pull each display's mode from the
                        // persistence DB, RESETTING a 120 Hz panel to 60 Hz. So force-EXTEND only when the
                        // virtual is currently sole; otherwise skip straight to the reposition, which
                        // re-supplies each physical's QUERIED mode verbatim (preserving its refresh).
                        // SAFETY: `count_other_active` runs the CCD QueryDisplayConfig FFI (borrowed
                        // slice of Copy ids, owned result), under the `state` lock.
                        let already_extended =
                            unsafe { count_other_active(&[added.target_id]) }.unwrap_or(0) > 0;
                        if already_extended {
                            tracing::info!(
                                "display topology=primary — a physical display is already active; \
                                 skipping force-EXTEND (preserves its refresh) before making the \
                                 virtual primary"
                            );
                        } else {
                            // SAFETY: `force_extend_topology` drives the CCD topology FFI (no args, no
                            // borrowed memory), under the `state` lock — the sole topology mutator.
                            unsafe { force_extend_topology() };
                            thread::sleep(Duration::from_millis(300));
                        }
                        // SAFETY: `set_virtual_primary_ccd` takes the `Copy` target id by value and returns
                        // an owned `SavedConfig` (no borrowed memory crosses), under the `state` lock.
                        inner.group.ccd_saved = unsafe { set_virtual_primary_ccd(added.target_id) };
                    }
                    Topology::Primary => {
                        // A sibling already holds primary (the group's designated member) — the new
                        // monitor just extends; the group layout arranges it.
                        tracing::info!(
                            "display topology=primary — sibling slot holds primary; new member extends"
                        );
                    }
                    Topology::Extend | Topology::Auto => {
                        tracing::info!(
                            "display topology=extend — IDD stays extended (no isolate / no primary)"
                        );
                    }
                }
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture opens

                // EXPERIMENTAL `pnp_disable_monitors`, second selector (ANY topology): monitors
                // that are connected but NOT part of the desktop — the standby TV/monitor the
                // deactivated-set selector above structurally misses (it never had an active path
                // to deactivate), yet whose periodic standby wake events drive the same Windows
                // reaction cascade (rationale in `windows/monitor_devnode.rs`). Runs AFTER the
                // settle sleep so the active flags it reads are the committed ones (a display
                // still mid-activation from the primary topology's force-EXTEND must not read as
                // inactive and get disabled); in Extend the active physical panels are untouched
                // by construction. First member only — the sweep is group-scoped like the
                // isolate; later members join an already-swept desktop.
                if first_member && crate::policy::prefs().pnp_disable_monitors() {
                    let mut keep = inner.target_ids();
                    keep.push(added.target_id);
                    for id in pf_win_display::monitor_devnode::disable_connected_inactive(&keep) {
                        if !inner.group.pnp_disabled.contains(&id) {
                            inner.group.pnp_disabled.push(id);
                        }
                    }
                }
            }
            None => tracing::warn!(
                "virtual-display target {} not yet an active display path (auto-activate, EXTEND \
                 preset and explicit path activation all failed — GPU-less box?)",
                added.target_id
            ),
        }

        Ok(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: (0, 0),
            gen: self.gen.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// Mid-stream resize by monitor RE-ARRIVAL (`design/midstream-resolution-resize.md` Fix 1).
    ///
    /// The pf-vdisplay driver freezes a monitor's advertised mode list at `IOCTL_ADD` time (the
    /// requested mode + `default_modes()`), so a plain `ChangeDisplaySettingsExW` can only reach a
    /// mode the monitor advertised on arrival — an out-of-list target (e.g. a session that arrived at
    /// 1080p resizing to 1440p) returns `DISP_CHANGE_BADMODE`. IddCx exposes no live "update modes"
    /// DDI, so to follow the client to an ARBITRARY new mode we REMOVE the driver monitor and ADD a
    /// fresh one at the new mode, reusing the slot's stable per-client id (EDID serial / ConnectorIndex
    /// / ContainerId) so the OS keeps the monitor's identity + saved per-monitor DPI. The visible cost
    /// is one monitor hotplug per switch (the design's accepted "re-arrival for everything").
    ///
    /// Refcount/lease continuity: the rebuilt `Monitor` PRESERVES the old `gen`, so the outstanding
    /// session lease(s) still match on release — the linger/refcount machine is untouched. The group
    /// restore snapshot (`group.ccd_saved` / DDC / PnP) is likewise PRESERVED (a mid-session swap, not
    /// a first-member create): [`reisolate_after_swap`](Self::reisolate_after_swap) re-isolates the new
    /// target without recapturing it. Caller owns the slot's `Monitor` + `refs` across this call.
    ///
    /// # Safety
    /// `dev` must be the live control handle; touches the live display topology via CCD/GDI.
    unsafe fn re_add(
        &'static self,
        dev: HANDLE,
        inner: &mut MgrInner,
        slot: u32,
        old: &Monitor,
        mode: Mode,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    ) -> Result<Monitor> {
        tracing::info!(
            slot,
            old = format!(
                "{}x{}@{}",
                old.mode.width, old.mode.height, old.mode.refresh_hz
            ),
            new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
            old_target = old.target_id,
            "virtual-display: re-arriving monitor for a mid-stream resize (exact mode)"
        );
        // 1. Depart the OLD driver monitor — a bare REMOVE IOCTL (no topology restore, pinger stays
        //    up): the surviving/grown-set re-isolate happens after the new ADD. Frees the preferred id
        //    so the ADD below can reuse the same stable identity. Best-effort — a REMOVE failure still
        //    lets the ADD proceed (the driver reaps a stale same-id monitor on the next create anyway).
        // SAFETY: `dev` is the live control handle (this fn's contract); `&old.key` borrows the
        // still-owned `MonitorKey`, alive across the synchronous IOCTL.
        if let Err(e) = unsafe { self.driver.remove_monitor(dev, &old.key) } {
            tracing::warn!(
                old_target = old.target_id,
                "re-arrival REMOVE failed (continuing to ADD): {e:#}"
            );
        }
        // Let the OS finish the ASYNC monitor departure before the ADD — a back-to-back REMOVE→ADD
        // races the teardown and the ADD is rejected under churn (same 400 ms settle as the reconnect
        // preempt path).
        thread::sleep(Duration::from_millis(400));
        // 2. ADD a fresh monitor at the NEW mode, reusing the slot as the preferred (stable) id.
        let render_pin = resolve_render_pin();
        // SAFETY: `dev` is the live control handle; `render_pin`/`client_hdr` are owned `Copy`/`Option`
        // values passed by value — no borrow crosses the call.
        let added = unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, slot, client_hdr)
                .context("re-arrival ADD at the new mode")?
        };
        self.ensure_pinger();
        // 3. Resolve the NEW target's GDI name (target_id changes across a re-arrival).
        // SAFETY: CCD FFI over a `Copy` target id, under the `state` lock.
        let gdi_name = unsafe { self.resolve_target_gdi(added.target_id) };
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    "re-arrival target {} -> {n}",
                    added.target_id
                );
                // ADD only advertises the mode; force it active so DXGI/IDD captures the new size.
                set_active_mode(n, mode);
                // 4. Re-isolate the composited set with the NEW target replacing the old — preserving
                //    the group's first-member restore snapshot.
                // SAFETY: CCD FFI over borrowed Copy target ids, under the `state` lock.
                unsafe { self.reisolate_after_swap(inner, added.target_id) };
                thread::sleep(Duration::from_millis(1500)); // let the topology settle before capture reopens
            }
            None => tracing::warn!(
                "re-arrival target {} not yet an active display path (auto-activate, EXTEND preset \
                 and explicit path activation all failed — GPU-less box?)",
                added.target_id
            ),
        }
        // 5. Rebuild the Monitor from the ADD reply, PRESERVING `gen` (lease/refcount continuity) and
        //    the group-layout `position`. A fresh `gen` would strand the old session's lease release.
        Ok(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: old.position,
            gen: old.gen,
        })
    }

    /// Re-isolate the composited display set after a mid-stream monitor re-arrival ([`re_add`]) put a
    /// NEW target in place of the old one — WITHOUT recapturing the group restore snapshot (the first
    /// member captured it at session start; teardown restores that, not the mid-session state). The
    /// old slot has already been removed from the map by the caller, so `inner.target_ids()` is the
    /// surviving siblings; the new target joins them.
    ///
    /// # Safety
    /// Drives the CCD topology FFI; call under the `state` lock.
    unsafe fn reisolate_after_swap(&self, inner: &mut MgrInner, new_target: u32) {
        use crate::policy::Topology;
        match topology_action() {
            Topology::Exclusive => {
                // Grown-set semantics: isolate to the surviving siblings + the new target. The returned
                // snapshot is DISCARDED — the group keeps the first member's (design §6.1).
                let mut keep = inner.target_ids();
                keep.push(new_target);
                // SAFETY: borrowed slice of Copy target ids, owned return, under the `state` lock.
                let _ = unsafe { isolate_displays_ccd(&keep) };
            }
            Topology::Primary => {
                // Make the new target primary again (its predecessor held primary), preserving the
                // original restore snapshot: `set_virtual_primary_ccd` recaptures one, so save + restore
                // the group's around the call.
                let keep_saved = inner.group.ccd_saved.take();
                // SAFETY: `Copy` target id by value, owned return, under the `state` lock.
                let _ = unsafe { set_virtual_primary_ccd(new_target) };
                inner.group.ccd_saved = keep_saved;
            }
            Topology::Extend | Topology::Auto => {
                // The re-ADDed target auto-activates extended — nothing to isolate/promote.
            }
        }
    }

    /// Tear down `mon`, which the caller has ALREADY removed from `inner.slots`: on the LAST member
    /// stop the device pinger + restore the group topology; on a non-last member re-issue the
    /// exclusive isolate with the SHRUNK managed set; then REMOVE the monitor. Consumes it.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn teardown_removed(&self, dev: HANDLE, inner: &mut MgrInner, mon: Monitor) {
        // Wedge visibility: this runs synchronously — usually UNDER the `state` lock (linger timer,
        // reconnect preempt, quit-skip), so a REMOVE/CCD-restore that never returns (field signature:
        // Windows AMD reconnects going silently dead) blocks every future `acquire` with NOTHING in the
        // log. One ERROR line after 10 s turns that silent wedge into a diagnosis.
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            let target = mon.target_id;
            thread::Builder::new()
                .name("vdisplay-teardown-watch".into())
                .spawn(move || {
                    thread::sleep(Duration::from_secs(10));
                    if !done.load(Ordering::SeqCst) {
                        tracing::error!(
                            target_id = target,
                            "virtual-display teardown still running after 10s — the driver \
                             REMOVE/CCD restore looks WEDGED; new sessions will block until it returns"
                        );
                    }
                })
                .ok();
        }
        let last_member = inner.slots.is_empty();
        if last_member {
            // The LAST slot is going away: the device-level pinger has nothing left to keep alive
            // (stopped FIRST, as the per-monitor pinger was), and the group's topology restore runs
            // — first-in captured it, last-out restores it (design §6.1).
            self.stop_pinger();
            // EXPERIMENTAL `pnp_disable_monitors` restore: re-enable the devnodes FIRST and let
            // them re-arrive, so a CCD restore below re-activates paths whose monitors exist
            // again (a disabled devnode would leave the restored path modeless/EDID-less).
            // OUTSIDE the ccd_saved gate: the connected-inactive sweep disables devnodes in
            // Extend/Primary sessions too, where no isolate snapshot exists to restore.
            let pnp_disabled = std::mem::take(&mut inner.group.pnp_disabled);
            if !pnp_disabled.is_empty() {
                pf_win_display::monitor_devnode::enable_instances(&pnp_disabled);
                thread::sleep(Duration::from_millis(300));
            }
            // Re-attach detached display(s) BEFORE the REMOVE so the box is never left with zero
            // displays.
            if let Some(saved) = inner.group.ccd_saved.take() {
                restore_displays_ccd(&saved);
                // EXPERIMENTAL `ddc_power_off` wake: the restore re-activated the physical paths, and
                // returning signal alone wakes DPMS-off panels on most firmware — the explicit ON is
                // belt-and-braces for the rest. The brief settle wait lets the re-activated paths show
                // up in EnumDisplayMonitors (no HMONITOR, no DDC channel); teardown is already
                // seconds-scale and watched by the 10 s wedge logger above.
                if inner.group.ddc_panels_off > 0 {
                    thread::sleep(Duration::from_millis(300));
                    let woken = crate::ddc::panel_on_all();
                    tracing::info!(
                        commanded_off = inner.group.ddc_panels_off,
                        woken,
                        "DDC/CI: panel wake commands sent after topology restore"
                    );
                    inner.group.ddc_panels_off = 0;
                }
            }
        } else if inner.group.ccd_saved.is_some() {
            // Siblings remain and an exclusive isolate is live: re-issue it with the SHRUNK managed
            // set (defensive — the departing monitor's path dies with the REMOVE below anyway, but
            // a re-commit keeps the surviving set authoritative if the OS re-lit anything). The
            // returned snapshot is discarded; the group keeps the first member's.
            let keep = inner.target_ids();
            // SAFETY: `isolate_displays_ccd` only drives the CCD query/apply FFI over a borrowed
            // slice of Copy target ids, under the `state` lock — the sole topology mutator.
            let _ = unsafe { isolate_displays_ccd(&keep) };
        }
        // SAFETY: `teardown_removed`'s own `# Safety` contract guarantees `dev` is the live control
        // handle, and `remove_monitor` requires exactly that. `&mon.key` borrows the `MonitorKey`
        // inside the still-owned `mon`, alive for this synchronous IOCTL, so the pointer the driver
        // reads stays valid.
        if let Err(e) = unsafe { self.driver.remove_monitor(dev, &mon.key) } {
            // A gone-classified failure means the device died under this monitor (driver upgrade /
            // WUDFHost restart) — retire the handle so the NEXT session reopens instead of failing.
            if is_device_gone(&e) {
                self.invalidate_device(&e);
            }
            tracing::warn!(
                target_id = mon.target_id,
                "virtual-display REMOVE failed: {e:#}"
            );
        } else {
            tracing::info!(
                backend = self.driver.name(),
                "virtual-display monitor removed"
            );
        }
        done.store(true, Ordering::SeqCst);
    }

    /// Release a session's hold (the [`MonitorLease`] `Drop`): refcount-- ; the last session leaving
    /// LINGERs before teardown — unless `quit_now` (the client closed with the QUIT code, a user
    /// "stop"), which tears the monitor down IMMEDIATELY instead of lingering. That both restores
    /// the physical displays at the moment the user quits and means a follow-up reconnect finds the
    /// manager Idle — a clean fresh ADD with the user's think-time as driver settle — instead of
    /// tripping the Lingering-preempt's back-to-back REMOVE→ADD. `keep_alive = forever` (the
    /// gaming-rig preset) OUTRANKS the quit: its promise is "the screen stays alive", so a
    /// deliberate quit still pins — only `/display/release` frees a pinned monitor. A STALE lease
    /// (its monitor was preempted + recreated under it) is a no-op, so it can't tear down the
    /// CURRENT monitor.
    fn release(&self, slot: u32, gen: u64, quit_now: bool) {
        let mut inner = self.state.lock().unwrap();
        let stale = match inner.slots.get(&slot) {
            Some(s) => s.mon().gen != gen,
            None => true,
        };
        if stale {
            return;
        }
        let Some(entry) = inner.slots.remove(&slot) else {
            return;
        };
        match entry {
            SlotState::Active { mon, refs } if refs > 1 => {
                inner.slots.insert(
                    slot,
                    SlotState::Active {
                        mon,
                        refs: refs - 1,
                    },
                );
            }
            // Last session left this slot: keep the monitor forever (Pinned) under
            // `keep_alive = forever` — checked BEFORE the quit, because the gaming-rig preset's
            // contract is "the screen stays alive": a deliberate quit skips only the linger window,
            // never the pin.
            SlotState::Active { mon, .. } if keep_alive_forever() => {
                tracing::info!(
                    slot,
                    "virtual-display: last session left — PINNED (keep_alive=forever); free via /display/release"
                );
                inner.slots.insert(slot, SlotState::Pinned { mon });
            }
            // Last session left on a deliberate quit: tear down NOW (linger skipped). Teardown
            // runs UNDER the state lock — same shape as the linger timer, and for the same reason: a
            // racing `acquire` must WAIT the teardown out rather than see an empty slot and ADD into
            // the driver's in-flight REMOVE. `device_handle()` is only None if the control device
            // was never opened — impossible with a monitor live — but fall back to Lingering (the
            // timer retries) rather than leak the monitor.
            SlotState::Active { mon, .. } if quit_now => match self.device_handle() {
                Some(dev) => {
                    tracing::info!(
                        slot,
                        "virtual-display: last session left (deliberate quit) — tearing down now, linger skipped"
                    );
                    // SAFETY: `teardown_removed` requires `dev` to be the live control handle; `dev`
                    // is the cached process-lifetime `OwnedHandle` from `device_handle()` (the `Some`
                    // checked above; cached handles are never closed — a dead one is retired, kept
                    // alive). `mon` was moved out of the map under the `state` lock, so it is
                    // exclusively owned here — no aliasing.
                    unsafe { self.teardown_removed(dev, &mut inner, mon) };
                }
                None => {
                    inner.slots.insert(
                        slot,
                        SlotState::Lingering {
                            mon,
                            until: Instant::now() + Duration::from_millis(linger_ms()),
                        },
                    );
                }
            },
            // Last session left, no quit signal: linger for the policy window before the timer
            // tears it down.
            SlotState::Active { mon, .. } => {
                let ms = linger_ms();
                tracing::info!(
                    slot,
                    linger_ms = ms,
                    "virtual-display: last session left — lingering before teardown"
                );
                inner.slots.insert(
                    slot,
                    SlotState::Lingering {
                        mon,
                        until: Instant::now() + Duration::from_millis(ms),
                    },
                );
            }
            // A kept (Lingering/Pinned) slot has no live hold — a release reaching it is
            // stale/duplicate; put it back untouched.
            other => {
                inner.slots.insert(slot, other);
            }
        }
    }

    /// Begin an IDD-push session setup (Goal-1 §2.5 — was the `IDD_SETUP_LOCK` / `IDD_SESSION_STOP` /
    /// `wait_for_monitor_released` dance smeared across `native`). Serializes via the (manager-wide)
    /// setup lock, registers THIS session's stop flag on its SLOT while signalling the prior session
    /// holding that slot to stop, and waits for it to release the slot's monitor — so a reconnect
    /// (whose reused IddCx swap-chain is dead) preempts the stale session cleanly before a fresh
    /// monitor is created. Slot-scoped since Stage W1: a DIFFERENT identity's session is an admission
    /// question, never a preempt. Returns the setup guard; the caller holds it across the pipeline
    /// build, then drops it so the next reconnect can begin (and preempt this one).
    pub fn begin_idd_setup(
        &'static self,
        slot: u32,
        stop: Arc<AtomicBool>,
    ) -> std::sync::MutexGuard<'static, ()> {
        let guard = self.setup_lock.lock().unwrap();
        let prev = self.idd_session_stops.lock().unwrap().insert(slot, stop);
        if let Some(prev_stop) = prev {
            prev_stop.store(true, Ordering::SeqCst);
            if !self.wait_for_slot_released(slot, Duration::from_secs(3)) {
                // TIMEOUT: the prior session is STILL Active on this slot (a wedged/slow teardown).
                // `acquire`'s preempt is Lingering-only (so build-retries JOIN the held monitor
                // instead of churning REMOVE→ADD), which means the upcoming `_retry_hold` acquire
                // would JOIN this stuck monitor and reuse its DEAD IddCx swap-chain → a full-session
                // black screen with no self-heal until this session disconnects. Force-preempt it
                // HERE instead. This runs at most ONCE per session (we hold `setup_lock`), so —
                // unlike preempting inside `acquire` — it does not reintroduce the per-retry churn.
                // The next `acquire` then sees the slot empty and creates a fresh monitor; the stale
                // session's gen-stamped lease release is a no-op.
                if let Some(dev) = self.device_handle() {
                    let mut inner = self.state.lock().unwrap();
                    let taken = match inner.slots.get(&slot) {
                        Some(SlotState::Active { .. }) => inner.slots.remove(&slot),
                        // Raced to Lingering/empty between the wait and here — nothing stuck.
                        _ => None,
                    };
                    if let Some(SlotState::Active { mon, .. }) = taken {
                        tracing::warn!(
                            slot,
                            old_target = mon.target_id,
                            "IDD-push setup: force-preempting the stuck-Active prior monitor (its IddCx swap-chain is dead)"
                        );
                        // SAFETY: `teardown_removed` requires `dev` to be the live control handle;
                        // `dev` is the cached process-lifetime `OwnedHandle` from `device_handle()`
                        // (the `Some` checked above). `mon` was moved out of the map under the
                        // `state` lock, so it is exclusively owned here — no aliasing.
                        unsafe { self.teardown_removed(dev, &mut inner, mon) };
                        // Let the OS finish the ASYNC departure before the next ADD (mirrors the
                        // acquire() Lingering-preempt settle).
                        thread::sleep(Duration::from_millis(400));
                    }
                }
            }
        }
        guard
    }

    /// Wait (up to `timeout`) for `slot` to be RELEASED (no longer `Active`). Used by the IDD-push
    /// reconnect preempt: after signalling the old session to stop, wait here so it tears its monitor
    /// down cleanly before we acquire a fresh one. Returns `true` if it released, `false` on timeout
    /// (the prior session is still `Active` — the caller force-preempts it).
    pub(crate) fn wait_for_slot_released(&self, slot: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !matches!(
                self.state.lock().unwrap().slots.get(&slot),
                Some(SlotState::Active { .. })
            ) {
                return true;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    slot,
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
                    let now = Instant::now();
                    let expired: Vec<u32> = g
                        .slots
                        .iter()
                        .filter_map(|(slot, s)| {
                            matches!(s, SlotState::Lingering { until, .. } if now >= *until)
                                .then_some(*slot)
                        })
                        .collect();
                    for slot in expired {
                        if let Some(SlotState::Lingering { mon, .. }) = g.slots.remove(&slot) {
                            // Teardown UNDER the state lock. Dropping the lock first (the old shape)
                            // let a concurrent `acquire` see the slot empty and run its ADD + CCD
                            // isolate while this monitor's CCD-restore / REMOVE were still in flight
                            // — the late restore then de-isolated (or the REMOVE churn-rejected) the
                            // fresh session at the linger-expiry boundary. Holding the lock makes
                            // the racing acquire WAIT the few teardown seconds instead of failing
                            // its session. Lock order stays state → device (teardown's invalidate
                            // path), same as every other holder; the pinger takes only the device
                            // lock — no inversion.
                            // SAFETY: `teardown_removed` requires a valid control handle; `dev` is
                            // from `self.device_handle()` (cached handles are never closed — a dead
                            // one is retired, kept alive; see `DeviceSlot`). `mon` was moved out of
                            // the map under the lock, so it is exclusively owned here.
                            unsafe { self.teardown_removed(dev, &mut g, mon) };
                        }
                    }
                })
                .ok();
        });
    }
}

/// The session's refcount handle on its SLOT. `Drop` releases the slot's refcount; a stale lease (its
/// monitor was preempted + recreated under it) is a no-op.
struct MonitorLease {
    mgr: &'static VirtualDisplayManager,
    slot: u32,
    gen: u64,
    /// The session's deliberate-quit flag (the client closed with the QUIT application code — a user
    /// "stop", not a network drop). Read at drop time: a quit release tears the monitor down NOW
    /// instead of lingering, mirroring the Linux registry's `Linger::Immediate`. `None` = no signal
    /// (GameStream sessions, the mgmt reconfigure path) → the linger policy applies.
    quit: Option<Arc<AtomicBool>>,
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        let quit_now = self.quit.as_ref().is_some_and(|q| q.load(Ordering::SeqCst));
        self.mgr.release(self.slot, self.gen, quit_now);
    }
}

/// The SLOT id keying a client's monitor in the manager: the stable per-client identity slot
/// (`1..=15`, `identity::resolve_slot` — Windows defaults to PerClient, its historical behavior), or
/// `0` for anonymous/GameStream sessions (the driver then auto-allocates the monitor id; at most one
/// anonymous slot at a time — exactly the pre-slot-map semantics, since an anonymous re-acquire has
/// no identity to find any other slot by). Shared by `acquire` and the session setup's
/// [`VirtualDisplayManager::begin_idd_setup`], so both address the same slot.
pub fn slot_id_for(client_fp: Option<[u8; 32]>, mode: (u32, u32)) -> u32 {
    super::identity::resolve_slot(client_fp, mode, crate::policy::Identity::PerClient).unwrap_or(0)
}

/// The render-GPU pin (backend-neutral): IDD-push — the sole Windows capture path — runs NVENC on the
/// render adapter, so it must always be pinned to the selected encoder GPU (a hybrid box would
/// otherwise render on the wrong one). The selection itself (web-console preference >
/// `PUNKTFUNK_RENDER_ADAPTER` > max VRAM) lives in [`pf_gpu::resolve_render_adapter_luid`].
/// (This was gated on the removed `PUNKTFUNK_IDD_PUSH` knob — a dispatch disagreement, since capture
/// stopped consulting it when DDA/WGC were removed.)
fn resolve_render_pin() -> Option<LUID> {
    tracing::info!("IDD push: pinning the render GPU (SET_RENDER_ADAPTER)");
    pf_gpu::resolve_render_adapter_luid()
}

/// A reused monitor keeps the render GPU the driver was pinned to at its ADD — the pin is never
/// re-issued on reuse. When the current pick has moved since then (an operator preference change,
/// or the max-VRAM tie shifting on identical twin GPUs), say so: the session self-heals (the
/// IDD-push open rebinds its ring to the driver's actual adapter on a mismatch), but the new pick
/// only takes effect on the next monitor CREATE, which the mgmt `/display/release` forces.
/// Compares the pick against the ADD-time PIN — `mon.luid` is the IddCx display adapter and must
/// not be compared with render-GPU picks.
fn warn_if_pick_moved(mon: &Monitor) {
    let Some(pin) = mon.render_pin else { return };
    let Some(sel) = pf_gpu::selected_gpu() else {
        return;
    };
    let pick = sel.info.luid();
    if (pick.LowPart, pick.HighPart) != (pin.LowPart, pin.HighPart) {
        tracing::warn!(
            pinned_adapter = format!("{:08x}:{:08x}", pin.HighPart, pin.LowPart),
            current_pick = format!(
                "{:08x}:{:08x} ({}, {})",
                pick.HighPart,
                pick.LowPart,
                sel.info.name,
                sel.source.tag()
            ),
            "reused virtual monitor is pinned to a different render GPU than the current pick — \
             the session follows the pinned GPU; free the display (mgmt /display/release) to \
             recreate it on the picked one"
        );
    }
}

/// A read-only view of one managed slot for the mgmt `/display/state` endpoint (Goal:
/// display-management registry facade). Backend-neutral; the [`crate::registry`] facade
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
    /// The slot key: the client's stable identity slot (`1..=15`), or `0` = anonymous/auto.
    pub slot_id: u32,
    /// Desktop-space origin from the group layout (`(0,0)` for a single display).
    pub position: (i32, i32),
}

impl VirtualDisplayManager {
    /// Snapshot the managed slots for the mgmt `/display/state` endpoint, in acquire (gen) order.
    /// Empty when no slot lives.
    pub(crate) fn snapshot(&self) -> Vec<ManagedInfo> {
        let inner = self.state.lock().unwrap();
        let mut out: Vec<ManagedInfo> = inner
            .slots
            .iter()
            .map(|(slot, s)| {
                let (mon, state, sessions, expires_in_ms) = match s {
                    SlotState::Active { mon, refs } => (mon, "active", *refs, None),
                    SlotState::Lingering { mon, until } => {
                        let ms = until.saturating_duration_since(Instant::now()).as_millis() as u64;
                        (mon, "lingering", 0u32, Some(ms))
                    }
                    // Pinned (keep_alive=forever): kept indefinitely, no expiry — the console shows
                    // "Pinned".
                    SlotState::Pinned { mon } => (mon, "pinned", 0u32, None),
                };
                ManagedInfo {
                    backend: self.driver.name(),
                    mode: (mon.mode.width, mon.mode.height, mon.mode.refresh_hz),
                    state,
                    expires_in_ms,
                    sessions,
                    gen: mon.gen,
                    slot_id: *slot,
                    position: mon.position,
                }
            })
            .collect();
        out.sort_by_key(|i| i.gen);
        out
    }

    /// Force-tear-down kept (LINGERING **or** PINNED) monitors now (the `/display/release` endpoint) —
    /// so a physical-screen user gets their screen back without waiting out the linger, and it is the §8
    /// escape hatch that frees a `keep_alive=forever` (Pinned) monitor. `slot` selects one kept monitor
    /// by its [`ManagedInfo::gen`] stamp; `None` releases every kept one. Active monitors are refused
    /// (stopping a live session is session management, not display management). Returns the number
    /// released.
    pub(crate) fn force_release(&self, slot: Option<u64>) -> usize {
        let Some(dev) = self.device_handle() else {
            return 0;
        };
        let mut inner = self.state.lock().unwrap();
        let kept: Vec<u32> = inner
            .slots
            .iter()
            .filter_map(|(k, s)| match s {
                SlotState::Lingering { mon, .. } | SlotState::Pinned { mon }
                    if slot.is_none_or(|g| g == mon.gen) =>
                {
                    Some(*k)
                }
                _ => None,
            })
            .collect();
        let mut released = 0usize;
        for k in kept {
            if let Some(SlotState::Lingering { mon, .. } | SlotState::Pinned { mon }) =
                inner.slots.remove(&k)
            {
                // SAFETY: `teardown_removed` needs a live control handle; `dev` is from
                // `device_handle()` (cached handles are never closed — a dead one is retired, kept
                // alive; see `DeviceSlot`). `mon` was moved out of the map under the `state` lock,
                // so it is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev, &mut inner, mon) };
                released += 1;
            }
        }
        released
    }
}

/// Snapshot the managed slots (empty when no backend has initialised the manager yet — no session
/// has ever run — or none lives). Safe to call per management request.
pub(crate) fn snapshot() -> Vec<ManagedInfo> {
    VDM.get()
        .map(VirtualDisplayManager::snapshot)
        .unwrap_or_default()
}

/// Force-release kept monitors now (`slot` = a [`ManagedInfo::gen`] stamp, `None` = all kept); `0`
/// if nothing was kept (or the manager is uninitialised).
pub(crate) fn force_release(slot: Option<u64>) -> usize {
    VDM.get().map(|m| m.force_release(slot)).unwrap_or(0)
}
