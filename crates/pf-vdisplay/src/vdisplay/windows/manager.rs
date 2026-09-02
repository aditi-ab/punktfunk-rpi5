//! Host-lifetime virtual-display ownership: one process-wide refcount machine
//! (Idle / Active / Lingering / Pinned), the linger timer, and a typed
//! [`OwnedHandle`] control device.
//!
//! [`VirtualDisplayManager`] is the singleton ([`vdm`]). The session holds a
//! [`MonitorLease`]; `Drop` releases the slot's refcount. A stale lease — its
//! monitor was preempted and recreated under it — is a no-op, so it cannot tear
//! down the live monitor.
//!
//! Driver IOCTL and the per-monitor REMOVE key sit behind [`VdisplayDriver`].
//! Topology, GDI/CCD glue, and generation-stamped leases are driver-neutral.
//! Evidence: `design/display-management.md`.

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
    restore_displays_ccd, set_active_mode, set_virtual_primary_ccd, wait_mode_settled,
    wait_target_departed, CcdTargetKey, SavedConfig,
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

/// One live virtual monitor, owned by manager state (not by a session).
/// No `Drop`: [`teardown_removed`](VirtualDisplayManager::teardown_removed)
/// must fire the REMOVE IOCTL; a bare drop orphans the driver-side monitor.
/// Group topology (CCD snapshot, DDC, PnP) lives on [`GroupState`], not here.
struct Monitor {
    key: MonitorKey,
    target_id: u32,
    /// IddCx DISPLAY adapter LUID from the ADD reply — not the render GPU
    /// (that one is only in the shared frame header). Do not compare picks
    /// against this.
    luid: LUID,
    /// SET_RENDER_ADAPTER pin at ADD (`None` = no GPU selectable). Never
    /// re-issued on reuse — [`warn_if_pick_moved`] compares the current pick
    /// against this.
    render_pin: Option<LUID>,
    /// ADD-time v5 hardware-cursor flag. Survives re-arrival so the new
    /// monitor keeps the cursor channel.
    hw_cursor: bool,
    /// OS target carries an irrevocable hardware-cursor declare: DWM drops
    /// the pointer from frames forever (survives REMOVE→ADD via the stable
    /// target id). A session without the cursor channel must self-composite.
    cursor_excluded: bool,
    /// WUDFHost pid from the ADD reply. One process hosts every slot's
    /// publisher, so its death is all-slot shared fate.
    wudf_pid: u32,
    gdi_name: Option<String>,
    /// Mode the OS committed, not the one the client asked for. Read back
    /// through [`committed_mode_or`]. Not the resize discriminator — see
    /// `requested_mode`.
    mode: Mode,
    /// Mode asked for at the last ADD/mode-set.
    ///
    /// [`needs_resize`] must see this, not only `mode`. `set_active_mode`
    /// commits the highest advertised refresh ≤ requested, so the two fields
    /// can disagree for the monitor's life. Diffing acquire against `mode`
    /// alone turns every rebuild (`build_pipeline_with_retry` re-asks the
    /// negotiated rate) into an in-place resize then REMOVE→ADD, exhausting
    /// the IddCx slot pool (0x80070490).
    requested_mode: Mode,
    /// Driver-resolved monitor id (EDID serial / ConnectorIndex).
    resolved_monitor_id: u32,
    /// Desktop origin from the group layout. `(0,0)` until a multi-slot arrange.
    position: (i32, i32),
    /// Generation stamp; a [`MonitorLease`] releases only if this still matches.
    generation: u64,
}

impl Monitor {
    /// This monitor's complete CCD identity — every display-global helper selects paths by it
    /// (target ids alone are only unique per adapter; `luid` is the IddCx display adapter's).
    fn ccd_key(&self) -> CcdTargetKey {
        CcdTargetKey::from_luid_parts(self.luid.LowPart, self.luid.HighPart, self.target_id)
    }

    /// The capture target handed to a session (`None` until the GDI name resolves on a WDDM GPU).
    fn target(&self) -> Option<pf_frame::dxgi::WinCaptureTarget> {
        self.gdi_name
            .clone()
            .map(|n| pf_frame::dxgi::WinCaptureTarget {
                adapter_luid: pf_frame::dxgi::pack_luid(self.luid),
                gdi_name: n,
                target_id: self.target_id,
                wudf_pid: self.wudf_pid,
                cursor_excluded: self.cursor_excluded,
            })
    }
}

/// Per-slot machine. Idle is absence from the map.
enum SlotState {
    Active {
        mon: Monitor,
        refs: u32,
    },
    Lingering {
        mon: Monitor,
        until: Instant,
    },
    /// `keep_alive = forever`: linger timer never tears this down. Reconnect
    /// still preempts (a reused IddCx swap-chain is dead). Only
    /// `/display/release` or host shutdown frees it.
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

/// Group topology for the one Windows desktop. First slot isolates and
/// captures; last member restores. Per-monitor restore would flash physical
/// panels between sibling sessions.
#[derive(Default)]
struct GroupState {
    /// First slot's pre-isolate snapshot; last-member teardown restores it.
    /// `Some` also means an exclusive isolate is live, so add/remove re-issues
    /// isolate over the grown/shrunk set.
    ccd_saved: Option<SavedConfig>,
    /// Panels that acknowledged DDC/CI off at first isolate. Last-member
    /// teardown wakes them after CCD restore iff > 0.
    ddc_panels_off: u32,
    /// PnP instance ids disabled at first isolate. Last-member teardown
    /// re-enables them before the CCD restore.
    pnp_disabled: Vec<String>,
    /// AMD connector emulation pinned at first isolate. Last-member teardown
    /// owes the unlock; pinned emulation outlives the process (crash journal).
    edid_locked: bool,
    /// `ccd_saved` came from an exclusive isolate. `Primary` also snapshots
    /// but keeps physicals lit — the re-assert watchdog must not "fix" those.
    ccd_exclusive: bool,
}

/// Outcome of a mid-stream re-arrival ([`VirtualDisplayManager::re_add`]).
///
/// Three-way on purpose: `re_add` REMOVEs the old driver monitor before ADD,
/// so a failed ADD leaves no live monitor. Putting the old `Monitor` back
/// would make later `acquire` join a departed `target_id`.
enum ReAdd {
    Arrived(Box<Monitor>),
    /// ADD failed; the old mode was re-ADDed. The monitor is real but new —
    /// a re-arrival mints a fresh `target_id` — so the caller must store this
    /// one, not the struct it handed in.
    RolledBack {
        mon: Box<Monitor>,
        err: anyhow::Error,
    },
    /// ADD and rollback both failed. Leave the slot empty so the next
    /// `acquire` creates one rather than joining a phantom.
    Lost(anyhow::Error),
}

/// Topology work a non-last-member teardown owes the group.
/// Extracted so the gate is testable without a driver or a desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShrinkAction {
    Reisolate,
    /// Departing member may have been holding primary.
    RepromotePrimary,
    Nothing,
}

/// One stage of [`VirtualDisplayManager::resolve_target_gdi`]'s ladder: poll for the target's GDI name until
/// the 3 s ceiling. 50 ms sampling (latency plan P0.5) — a typical activation resolves on an early
/// poll, so finer sampling shaves ~150 ms off every stage crossing.
///
/// Extracted because the ladder ran this loop three times verbatim, and the 2nd and 3rd copies
/// documented themselves as "SAFETY: as the resolve loop above" — a pointer to a proof rather than
/// a proof, which silently rots the moment the block it points at moves.
fn poll_gdi_name(key: CcdTargetKey) -> Option<String> {
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(50));
        if let Some(n) = resolve_gdi_name(key) {
            return Some(n);
        }
    }
    None
}

/// Test-only: fail the next N CCD isolates against the real driver.
/// `#[cfg(test)]` so live hardware tests can reach it without a production
/// knob that could leave isolation silently disabled.
#[cfg(test)]
pub(crate) static FAIL_NEXT_ISOLATES: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// [`isolate_displays_ccd`] with the test seam in front of it. Every call site in this file goes
/// through here so an injected failure exercises the same gates a real one would.
fn isolate_displays_ccd_seam(keep: &[CcdTargetKey]) -> Option<SavedConfig> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        if FAIL_NEXT_ISOLATES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            tracing::warn!(
                keep = ?keep,
                "TEST fault injection: forcing isolate_displays_ccd -> None"
            );
            return None;
        }
    }
    isolate_displays_ccd(keep)
}

/// [`ShrinkAction`] a non-last-member teardown owes the group.
///
/// Discriminator is `ccd_exclusive`, not `has_saved`: `Topology::Primary`
/// also stores a `ccd_saved` snapshot. Keying on the snapshot ran the
/// exclusive isolate on a Primary group and blanked the physical displays
/// Primary exists to keep lit.
fn shrink_action(ccd_exclusive: bool, has_saved: bool) -> ShrinkAction {
    if ccd_exclusive {
        ShrinkAction::Reisolate
    } else if has_saved {
        ShrinkAction::RepromotePrimary
    } else {
        ShrinkAction::Nothing
    }
}

/// Mode `target_id` is actually running, for a caller about to record it.
/// `requested` is the fallback when the read-back cannot be trusted.
///
/// Every store of `Monitor.mode` owes this call. `set_active_mode` commits
/// the highest advertised refresh ≤ requested, and `wait_mode_settled`
/// verifies resolution only — a true settle says nothing about refresh.
///
/// Deliberately narrowed to the REFRESH. A read-back that FAILS, or that reports a different
/// RESOLUTION, keeps `requested`: the create path proceeds even when its settle timed out, so the
/// OS may still be sitting on its own default there, and recording that would hand the capturer +
/// the client a size nobody negotiated. The capturer already re-resolves the live size on its own
/// (`active_resolution` poll, game-capture GB1); the refresh is the field only this read-back can
/// answer.
fn committed_mode_or(key: CcdTargetKey, requested: Mode) -> Mode {
    let Some((width, height, refresh_hz)) = pf_win_display::win_display::active_mode(key) else {
        return requested;
    };
    if (width, height) != (requested.width, requested.height) {
        tracing::warn!(
            target = %key,
            requested = format!("{}x{}", requested.width, requested.height),
            active = format!("{width}x{height}"),
            "the OS is not running the requested resolution after the settle — recording the \
             requested mode (the capturer re-resolves the live size itself)"
        );
        return requested;
    }
    if refresh_hz != requested.refresh_hz {
        tracing::info!(
            target = %key,
            requested_hz = requested.refresh_hz,
            committed_hz = refresh_hz,
            "the OS committed a different refresh than requested (the driver does not advertise \
             it) — recording what the display actually runs"
        );
    }
    Mode {
        width,
        height,
        refresh_hz,
    }
}

/// Whether an acquire for `want` on a live monitor needs a mid-stream resize.
///
/// Matching either `requested` or `committed` is a join: the session re-asks
/// the negotiated mode on every rebuild, and that cannot beat what ADD got.
/// Keying on `committed` alone turns every re-acquire on a refresh-clamping
/// box into an in-place resize then REMOVE→ADD, forever.
fn needs_resize(requested: Mode, committed: Mode, want: Mode) -> bool {
    want != requested && want != committed
}

/// Slot map plus the one group record. One lock for both: every group
/// mutation happens on a slot transition, so splitting them invites lock-order
/// bugs.
#[derive(Default)]
struct MgrInner {
    /// Live/kept slots, keyed by identity slot (`1..=15`) or `0` for
    /// anonymous/GameStream (at most one; no identity to find another slot by).
    slots: BTreeMap<u32, SlotState>,
    group: GroupState,
}

impl MgrInner {
    /// Live target keys in acquire (generation) order — the CCD isolate keep-set + the layout member order.
    fn target_keys(&self) -> Vec<CcdTargetKey> {
        let mut mons: Vec<&Monitor> = self.slots.values().map(SlotState::mon).collect();
        mons.sort_by_key(|m| m.generation);
        mons.iter().map(|m| m.ccd_key()).collect()
    }
}

/// Device-level watchdog pinger, running while any slot lives (any IOCTL
/// bumps the watchdog, so one thread serves N monitors). Same stop+join
/// shape as the exclusive-topology re-assert watchdog.
struct Pinger {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// Control-device cache. A gone-class IOCTL retires the handle; the next
/// [`VirtualDisplayManager::ensure_device`] reopens and re-handshakes.
///
/// Every consumer holds its own `Arc` clone across IOCTLs. Retire drops only
/// the manager's reference; the handle closes when the last in-flight user
/// drains. That close is required: an open control handle vetoes the PnP
/// disable/restart wake-from-sleep recovery uses. Do not keep a retired
/// handle open for the process lifetime.
#[derive(Default)]
struct DeviceSlot {
    current: Option<Arc<OwnedHandle>>,
    /// `CLEAR_ALL` (crashed-host orphan reap) runs only on the first open;
    /// a reopen races sessions this process still considers live.
    opened_once: bool,
}

pub struct VirtualDisplayManager {
    driver: Box<dyn VdisplayDriver>,
    /// Opened on first acquire; reopened after a gone-classified retire
    /// (see [`DeviceSlot`]). Shared via the `&'static` singleton.
    device: Mutex<DeviceSlot>,
    watchdog_s: AtomicU32,
    /// Handshake protocol version (0 until first open). In-place resize
    /// needs `>= 4`; a v3 driver keeps the advertised fast path plus re-arrival.
    driver_proto: AtomicU32,
    /// Latched after UPDATE_MODES failed to make a new mode settable. The OS
    /// pins a monitor's settable set at arrival, so later attempts only waste
    /// ~1 s before the same re-arrival fallback. One attempt per process.
    update_modes_futile: AtomicBool,
    generation: AtomicU64,
    state: Mutex<MgrInner>,
    /// Serializes IDD-push setup (preempt + create) manager-wide: ADD/REMOVE
    /// stay exclusive. 400 ms async-departure settle and the IddCx slot-budget
    /// wedge both want zero concurrent ADD/REMOVE. Held across pipeline build.
    setup_lock: Mutex<()>,
    /// Per-slot IDD-push stop flags. A new connection signals only the session
    /// holding that identity's slot. A different identity is admission, never
    /// a preempt. Entries persist (bounded at 16); signalling a dead flag is
    /// harmless.
    idd_session_stops: Mutex<std::collections::HashMap<u32, Arc<AtomicBool>>>,
    pinger: Mutex<Option<Pinger>>,
    /// Exclusive-topology re-assert watchdog. A verified isolate is not
    /// durable (see [`Self::ensure_exclusive_watch`]).
    exclusive_watch: Mutex<Option<Pinger>>,
    // Per-client monitor ids live on `super::identity::global()`. CREATE
    // resolves via `identity::resolve_slot` so the EDID serial + ConnectorIndex
    // stay stable across reconnects and Windows reapplies saved DPI scaling.
}

static VDM: OnceLock<VirtualDisplayManager> = OnceLock::new();

/// Bumped on every exclusive-topology eviction the re-assert watchdog does.
/// An eviction is a real topology change: the OS recreates the IDD swap-chain
/// while the session's capture ring still waits on the old attachment, so
/// frames stop. Stream loops sample this and rebuild capture in place.
static TOPOLOGY_REASSERT_GEN: AtomicU64 = AtomicU64::new(0);

pub fn topology_reassert_gen() -> u64 {
    TOPOLOGY_REASSERT_GEN.load(Ordering::Relaxed)
}

/// Initialise the process-wide manager with `driver`. Idempotent: first
/// backend wins; a later call ignores its driver.
pub(crate) fn init(driver: Box<dyn VdisplayDriver>) -> &'static VirtualDisplayManager {
    VDM.get_or_init(|| VirtualDisplayManager {
        driver,
        device: Mutex::new(DeviceSlot::default()),
        watchdog_s: AtomicU32::new(3),
        driver_proto: AtomicU32::new(0),
        update_modes_futile: AtomicBool::new(false),
        generation: AtomicU64::new(1),
        state: Mutex::new(MgrInner::default()),
        setup_lock: Mutex::new(()),
        idd_session_stops: Mutex::new(std::collections::HashMap::new()),
        pinger: Mutex::new(None),
        exclusive_watch: Mutex::new(None),
    })
}

/// Process-wide manager. Panics if reached before a backend called [`init`].
pub fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

/// Whether this host's pf-vdisplay driver can run the v5 hardware-cursor
/// channel. Opens the control device once if nothing has this service run,
/// so Welcome never guesses. `false` when the driver is missing or stale.
///
/// The first session's Welcome runs before `vdisplay::open` constructs the
/// backend, so this must not assume an initialised manager. `init` is
/// idempotent and constructing the driver facade is free.
pub fn hw_cursor_capable() -> bool {
    let m = init(Box::new(crate::driver::PfVdisplayDriver));
    let v = m.driver_proto.load(Ordering::Relaxed);
    if v != 0 {
        return v >= 5;
    }
    let _ = m.ensure_device();
    m.driver_proto.load(Ordering::Relaxed) >= 5
}

/// Live control device for IDD-push sealed-channel delivery. The caller
/// (and every closure it builds) holds the `Arc` for as long as it may
/// issue IOCTLs; the handle closes when the last holder drains. `None`
/// before the first backend open.
pub fn control_device_handle() -> Option<Arc<OwnedHandle>> {
    VDM.get().and_then(VirtualDisplayManager::device_handle)
}

/// Retire the cached control handle from outside the manager, for a caller
/// that knows the device died. Takes the `device` mutex: must not run from
/// inside it (not from `VdisplayDriver::open`, which `ensure_device` holds).
/// No-op before any backend opened the device.
pub(crate) fn invalidate_cached_device(why: &str) {
    if let Some(m) = VDM.get() {
        m.invalidate_device(&anyhow::anyhow!("{why}"));
    }
}

/// Re-commit the current display config under the `state` lock (sole
/// topology mutator). The OS reverts a path to software-cursor only on a
/// mode commit, so standing `IOCTL_SET_CURSOR_FORWARD` down needs this
/// nudge. `false` before the first backend open.
pub fn force_recommit() -> bool {
    let Some(m) = VDM.get() else {
        return false;
    };
    let _guard = m.state.lock().unwrap();
    pf_win_display::win_display::force_mode_reenumeration()
}

/// Best-effort "is this WUDFHost pid still alive?" for the JOIN path.
/// `OpenProcess` failing or the process signaled ⇒ dead. Pid reuse could
/// alias a fresh process as alive; the joining session then retries into
/// its rebuild budget — acceptable for a sub-second window.
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

/// True when an IOCTL failure means the control device itself is gone.
/// The cached handle can only keep failing and must be retired.
/// 0x80070490 (ERROR_NOT_FOUND, ADD slot-exhaustion) is not here — it has
/// its own reap-and-retry and the device is alive when it fires.
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

/// Transient raw `HANDLE` of an Arc-held control device. Sound only while
/// the borrowed `Arc` is held — every use site has the owning clone alive
/// across the call, so a concurrent retire cannot close it mid-IOCTL.
fn dev_raw(dev: &OwnedHandle) -> HANDLE {
    HANDLE(dev.as_raw_handle())
}

impl VirtualDisplayManager {
    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
    }

    /// Open and cache the control device; reopen after a gone-classified
    /// retire. Returns an `Arc` clone the caller holds across every IOCTL —
    /// a concurrent retire then drops only the manager's reference.
    fn ensure_device(&self) -> Result<Arc<OwnedHandle>> {
        let mut slot = self.device.lock().unwrap();
        if let Some(d) = &slot.current {
            return Ok(d.clone());
        }
        let reap = !slot.opened_once;
        claim_instance()?;
        // `open` is safe: it discharges FFI inside its body. The `device`
        // mutex serializes racing opens — serialization, not soundness, so
        // this is not `unsafe`.
        let (handle, watchdog_s, driver_proto) = self.driver.open(reap)?;
        slot.opened_once = true;
        self.watchdog_s.store(watchdog_s, Ordering::Relaxed);
        self.driver_proto.store(driver_proto, Ordering::Relaxed);
        let dev = Arc::new(handle);
        slot.current = Some(dev.clone());
        if !reap {
            tracing::info!("virtual-display control device reopened (retired handle replaced)");
        }
        Ok(dev)
    }

    /// Live control device for the pinger/linger threads. `None` before the
    /// first open, or between a retire and the next reopen.
    fn device_handle(&self) -> Option<Arc<OwnedHandle>> {
        self.device.lock().unwrap().current.clone()
    }

    /// Drop the manager's reference after a gone-classified IOCTL. The handle
    /// closes once the last in-flight user drains (see [`DeviceSlot`]). Next
    /// [`ensure_device`](Self::ensure_device) reopens and re-handshakes.
    fn invalidate_device(&self, why: &anyhow::Error) {
        let mut slot = self.device.lock().unwrap();
        if slot.current.take().is_some() {
            tracing::warn!(
                "virtual-display control device retired — closes when its last user drains, \
                 reopening on next use (cause: {why:#})"
            );
        }
    }

    /// Open and initialise the backend (validates the driver is present).
    pub(crate) fn open_backend(&self) -> Result<()> {
        // Hold `state` across the open so two racing backends cannot double-open.
        let _guard = self.state.lock().unwrap();
        self.ensure_device().map(|_| ())
    }

    /// Acquire this client's slot: preempt-recreate under IDD-push, join a live
    /// monitor (refcount++), or create one. `client_fp` keys the slot and gives
    /// a freshly created monitor the client's stable id. The returned
    /// [`MonitorLease`] releases the slot's refcount on drop.
    pub(crate) fn acquire(
        &'static self,
        mode: Mode,
        client_fp: Option<[u8; 32]>,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        hw_cursor: bool,
        quit: Option<Arc<AtomicBool>>,
    ) -> Result<VirtualOutput> {
        // Host outside the active console session cannot drive the display it
        // is about to create (SetDisplayConfig ACCESS_DENIED, GDI from the
        // wrong session). Non-fatal: persistence-DB activation can still work.
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

        // IDD-push: a new connection while THIS slot is Lingering/Pinned is a
        // reconnect. A reused IddCx swap-chain is dead — preempt and create
        // fresh. Do not preempt Active: that is a live lease (build-retry or
        // concurrent session); tearing it churns REMOVE→ADD into 0x80070490.
        if matches!(
            inner.slots.get(&slot),
            Some(SlotState::Lingering { .. } | SlotState::Pinned { .. })
        ) {
            if let Some(SlotState::Lingering { mon, .. } | SlotState::Pinned { mon }) =
                inner.slots.remove(&slot)
            {
                let old_key = mon.ccd_key();
                tracing::info!(
                    slot,
                    old_target = %old_key,
                    "IDD-push reconnect — preempting the kept (lingering/pinned) monitor, recreating a fresh one"
                );
                // SAFETY: `teardown_removed` requires `dev` to be a valid control handle; the `dev`
                // Arc `ensure_device()` returned above is held across this call, so the handle stays
                // open even against a concurrent retire. `mon` was just removed from the map, so it
                // is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
                // Let the OS finish the ASYNC monitor departure before the next ADD; a back-to-back
                // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
                // Verified-state wait, ceiling = the old fixed 400 ms settle (latency plan P0.3).
                let departed = wait_target_departed(old_key, Duration::from_millis(400));
                if !departed {
                    tracing::debug!(
                        old_target = %old_key,
                        "preempted monitor still in the active CCD set after the departure ceiling"
                    );
                }
            }
        }

        // Active whose WUDFHost has exited is dead driver-side. The rebuild
        // re-acquires while the old lease is still held, so the slot is Active.
        // Join would hand it a stale target. Preempt; generation-stamped
        // leases no-op on release. WUDFHost death is all-slot shared fate.
        if matches!(inner.slots.get(&slot), Some(SlotState::Active { mon, .. }) if !wudf_alive(mon.wudf_pid))
        {
            if let Some(SlotState::Active { mon, .. }) = inner.slots.remove(&slot) {
                let old_key = mon.ccd_key();
                tracing::warn!(
                    slot,
                    old_target = %old_key,
                    wudf_pid = mon.wudf_pid,
                    "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
                );
                // SAFETY: `teardown_removed` requires a valid control handle; the `dev` Arc
                // `ensure_device()` returned above is held across this call, so the handle stays
                // open even against a concurrent retire. `mon` was just removed from the map, so it
                // is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
                // Same async-departure settle as the reconnect preempt above (verified wait, P0.3).
                let _ = wait_target_departed(old_key, Duration::from_millis(400));
            }
        }

        // Live monitor on this slot — join (refcount++). Covers concurrent
        // same-client sessions and mid-stream Reconfigure overlap.
        if matches!(inner.slots.get(&slot), Some(SlotState::Active { .. })) {
            // A different mode is a mid-stream resize. Diff against both
            // [`needs_resize`] sides: negotiated and committed. `mon.mode`
            // alone is not the discriminator — a clamped refresh disagrees
            // for the monitor's life and every rebuild would look like resize.
            let (req_mode, cur_mode) = match inner.slots.get(&slot) {
                Some(SlotState::Active { mon, .. }) => (mon.requested_mode, mon.mode),
                _ => unreachable!("just matched Active"),
            };
            if needs_resize(req_mode, cur_mode, mode) {
                // In-place first: an already-advertised resolution is CCD-forced
                // on the same monitor (identity, swap-chain, stash survive).
                // Out-of-list fails fast in `resize_in_place` and falls through
                // to re-arrival.
                {
                    let in_place = {
                        let Some(SlotState::Active { mon, refs }) = inner.slots.get_mut(&slot)
                        else {
                            unreachable!("just matched Active");
                        };
                        // SAFETY: the `dev` Arc `ensure_device()` returned above is held across
                        // this call (so the handle stays open); the CCD waits inside run under
                        // the held `state` lock (this fn's discipline).
                        match unsafe { self.resize_in_place(dev_raw(&dev), mon, mode) } {
                            Ok(()) => {
                                // +1 ref for the new (build-then-drop) lease;
                                // generation untouched so the old lease stays valid.
                                *refs += 1;
                                let refs = *refs;
                                let out = self.output_for(slot, mon, quit.clone());
                                tracing::info!(
                                    slot,
                                    refs,
                                    backend = self.driver.name(),
                                    "virtual monitor resized IN PLACE (identity + swap-chain kept)"
                                );
                                Some(out)
                            }
                            Err(e) => {
                                // First-seen size: OS pins settable modes at
                                // arrival; re-arrival teaches it. Info, not warn.
                                tracing::info!(
                                    slot,
                                    reason = %format!("{e:#}"),
                                    "in-place resize not possible — monitor re-arrival"
                                );
                                None
                            }
                        }
                    };
                    if let Some(out) = in_place {
                        // Width changed — re-arrange so auto-row siblings do
                        // not overlap (no-op for a single member).
                        self.apply_group_layout(&mut inner);
                        return Ok(out);
                    }
                }
                let Some(SlotState::Active { mon, refs }) = inner.slots.remove(&slot) else {
                    unreachable!("just matched Active");
                };
                // SAFETY: the `dev` Arc `ensure_device()` returned above is held across this call
                // (so the handle stays open); `re_add` touches the live topology under the held
                // `state` lock. `mon` is owned here (removed from the map).
                let new_mon = match unsafe {
                    self.re_add(dev_raw(&dev), &mut inner, slot, &mon, mode, client_hdr)
                } {
                    ReAdd::Arrived(m) => *m,
                    ReAdd::RolledBack {
                        mon: recovered,
                        err,
                    } => {
                        // Store the recovered monitor, not the one handed in:
                        // that one's driver monitor was REMOVEd, so key /
                        // target_id / gdi_name are dead. generation/refs kept
                        // so leases stay valid.
                        inner.slots.insert(
                            slot,
                            SlotState::Active {
                                mon: *recovered,
                                refs,
                            },
                        );
                        return Err(err).context("mid-stream resize re-arrival");
                    }
                    ReAdd::Lost(err) => {
                        // Leave the slot empty so the next acquire ADDs cleanly.
                        return Err(err).context("mid-stream resize re-arrival (slot left empty)");
                    }
                };
                // `re_add` preserved generation so both leases match on release.
                // +1 ref for the new (build-then-drop) lease.
                let out = self.output_for(slot, &new_mon, quit);
                inner.slots.insert(
                    slot,
                    SlotState::Active {
                        mon: new_mon,
                        refs: refs + 1,
                    },
                );
                // Width changed — re-arrange so auto-row siblings do not
                // overlap (no-op for a single member).
                self.apply_group_layout(&mut inner);
                tracing::info!(
                    slot,
                    refs = refs + 1,
                    backend = self.driver.name(),
                    "virtual monitor re-arrived for a mid-stream resize"
                );
                return Ok(out);
            }
            // Same mode — concurrent-session join (refcount++), no re-arrival.
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

        // Fail-closed backstop if a session got past admission. `max_displays`
        // counts Active+Lingering+Pinned; one live slot can never trip it.
        let max = crate::policy::prefs().get().effective().max_displays;
        if inner.slots.len() as u32 >= max {
            anyhow::bail!(
                "display budget exhausted: {} display(s) live/kept, max_displays = {max} — freeing \
                 one (session end, linger expiry, or /display/release) admits the next",
                inner.slots.len()
            );
        }

        // SAFETY: `create_monitor` requires `dev` to be a valid control handle; the `dev` Arc
        // `ensure_device()` returned above is held across this call (so the handle stays open even
        // against a concurrent retire), and we hold the `state` lock.
        let mon = match unsafe {
            self.create_monitor(dev_raw(&dev), mode, slot, client_hdr, hw_cursor, &mut inner)
        } {
            // Cached device died under us. Retire, reopen, retry once so a
            // reconnect after driver restart does not burn a failed session.
            Err(e) if is_device_gone(&e) => {
                self.invalidate_device(&e);
                let dev = self.ensure_device()?;
                tracing::info!(
                    "virtual-display control device reopened — retrying the monitor create"
                );
                // SAFETY: the `dev` Arc the reopening `ensure_device` just returned is
                // held across this call, and the `state` lock is still held.
                unsafe {
                    self.create_monitor(
                        dev_raw(&dev),
                        mode,
                        slot,
                        client_hdr,
                        hw_cursor,
                        &mut inner,
                    )?
                }
            }
            r => r?,
        };
        let out = self.output_for(slot, &mon, quit);
        inner.slots.insert(slot, SlotState::Active { mon, refs: 1 });
        // Arrange live members and commit desktop origins in one CCD apply.
        // A single member sits at the origin — this no-ops.
        self.apply_group_layout(&mut inner);
        Ok(out)
    }

    /// [`VirtualOutput`] for `mon` in `slot`: preferred mode, capture target,
    /// generation-stamped lease. `quit` is read by the lease `Drop` (see
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
                generation: mon.generation,
                quit,
            }),
            // Manager owns the monitor lifecycle, so the registry treats it
            // as Owned (it delegates via `vd.create`).
            ownership: DisplayOwnership::Owned,
        }
    }

    /// Start the device-level watchdog pinger if it is not running. One thread
    /// serves every slot — any IOCTL bumps the watchdog.
    fn ensure_pinger(&'static self) {
        let mut guard = self.pinger.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let interval =
            Duration::from_millis(self.watchdog_s.load(Ordering::Relaxed) as u64 * 1000 / 3);
        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("vdisplay-pinger".into())
            .spawn(move || {
                let mut warned = false;
                while !stop_t.load(Ordering::Relaxed) {
                    if let Some(h) = vdm().device_handle() {
                        // SAFETY: `ping` requires `dev` to be a valid control handle. The `h` Arc
                        // from `device_handle()` is held across this call, so the handle stays open
                        // even if it is retired concurrently — at worst the IOCTL fails (the retire
                        // drops only the manager's reference; see `DeviceSlot`). The pinger thread
                        // only spins while the `&'static` manager singleton lives.
                        match unsafe { vdm().driver.ping(dev_raw(&h)) } {
                            Ok(()) => warned = false,
                            Err(e) if is_device_gone(&e) => {
                                // Device is gone. Retire so the next session
                                // reopens; the monitors are already dead
                                // driver-side.
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
        // Not `thread::spawn`: that panics if the OS refuses the thread, and
        // this holds `pinger` plus (via create_monitor ← acquire) `state`.
        // Unwind poisons both locks. A missing pinger degrades to the driver
        // watchdog; a poisoned manager does not.
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not spawn the virtual-display keepalive pinger — the driver's host-gone \
                     watchdog will tear this monitor down when it expires"
                );
                return;
            }
        };
        *guard = Some(Pinger { stop, thread });
    }

    /// Stop and join the device-level pinger (the last slot was just torn down).
    /// Join is bounded by the ping interval (watchdog/3 seconds).
    fn stop_pinger(&self) {
        if let Some(p) = self.pinger.lock().unwrap().take() {
            p.stop.store(true, Ordering::Relaxed);
            let _ = p.thread.join();
        }
    }

    /// Start the exclusive-topology re-assert watchdog (idempotent).
    ///
    /// A verified [`isolate_displays_ccd`] is not durable: the isolated topology
    /// is deliberately not saved to the CCD database (teardown must restore the
    /// user's layout), so a later re-resolution can bring the stored layout back.
    ///
    /// Re-query every [`knobs::exclusive_reassert_ms`]. On a non-managed display,
    /// evict via full isolate — the forced re-commit restarts presentation, and
    /// the session heals the swap-chain bounce off [`topology_reassert_gen`].
    /// Cycles `try_lock` the state lock: teardown stops+joins this thread while
    /// holding that lock, so a blocking `lock()` here would deadlock. Sleep is
    /// sliced so stop+join is bounded by ~250 ms, not a full cycle.
    fn ensure_exclusive_watch(&'static self) {
        let interval = Duration::from_millis(knobs::exclusive_reassert_ms());
        if interval.is_zero() {
            return; // knob 0 = disabled
        }
        let mut guard = self.exclusive_watch.lock().unwrap();
        if guard.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = thread::Builder::new()
            .name("vdisplay-exclusive-watch".into())
            .spawn(move || {
                // Consecutive eviction cycles. Resets when a cycle is clean,
                // so a rare re-add WARNs each time; a fighter escalates once.
                let mut fighting = 0u32;
                'watch: loop {
                    let mut slept = Duration::ZERO;
                    while slept < interval {
                        if stop_t.load(Ordering::Relaxed) {
                            break 'watch;
                        }
                        let slice = Duration::from_millis(250).min(interval - slept);
                        thread::sleep(slice);
                        slept += slice;
                    }
                    let Ok(inner) = vdm().state.try_lock() else {
                        continue;
                    };
                    if inner.group.ccd_saved.is_none() || !inner.group.ccd_exclusive {
                        continue; // no exclusive isolate live right now
                    }
                    let keep = inner.target_keys();
                    if keep.is_empty() {
                        continue;
                    }
                    // A FAILED verification query is UNKNOWN, not "stable": back off to the next
                    // cycle and mutate nothing (the old `unwrap_or(0)` silently called an unknown
                    // topology successfully exclusive).
                    let survivors = match count_other_active(&keep) {
                        Some(n) => n,
                        None => {
                            tracing::debug!(
                                "exclusive re-assert watchdog: CCD verification query failed — \
                                 topology state unknown this cycle, mutating nothing"
                            );
                            continue;
                        }
                    };
                    if survivors == 0 {
                        if fighting > 0 {
                            tracing::info!(
                                reasserts = fighting,
                                "exclusive topology stable again — no non-managed display active"
                            );
                            // Close the churn window now — descriptor-following
                            // resumes instead of waiting out the hold expiry.
                            pf_win_display::topology_churn::release();
                        }
                        fighting = 0;
                        continue;
                    }
                    fighting += 1;
                    // Announce churn before evicting: descriptors the capturer
                    // samples until "stable again" are the transient eviction
                    // state. Acting on them would recreate the ring at a mode
                    // the recovery is about to undo. Window = interval + 3 s.
                    pf_win_display::topology_churn::hold(interval + Duration::from_secs(3));
                    match fighting {
                        1..=3 => tracing::warn!(
                            survivors,
                            round = fighting,
                            "exclusive topology lost — a non-managed display re-activated after \
                             the verified isolate (hybrid-GPU driver / display-poller software \
                             restoring the saved layout?); re-asserting the isolate"
                        ),
                        4 => tracing::error!(
                            survivors,
                            "exclusive topology keeps being re-activated (4 consecutive \
                             re-asserts) — something on this host is fighting the isolate; \
                             continuing to re-assert every cycle, further rounds log at DEBUG"
                        ),
                        _ => tracing::debug!(
                            survivors,
                            round = fighting,
                            "re-asserting exclusive topology"
                        ),
                    }
                    let _ = isolate_displays_ccd_seam(&keep);
                    // Forced re-commit hands the IDD path a fresh swap-chain.
                    // Bump so the session re-attaches capture instead of
                    // streaming a frozen frame.
                    TOPOLOGY_REASSERT_GEN.fetch_add(1, Ordering::Relaxed);
                }
            });
        // Not `.expect()`: this holds `exclusive_watch` and (via the caller)
        // `state`. A panic poisons both. A missing watchdog is a degradation;
        // wedging the manager is not.
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "could not spawn the exclusive re-assert watchdog — the isolate will not be \
                     re-asserted if something re-lights a physical display"
                );
                return;
            }
        };
        *guard = Some(Pinger { stop, thread });
    }

    /// Stop and join the exclusive-topology watchdog. Safe under the state
    /// lock: the watchdog only `try_lock`s it, and sliced sleep bounds the
    /// join by ~250 ms.
    fn stop_exclusive_watch(&self) {
        if let Some(w) = self.exclusive_watch.lock().unwrap().take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.thread.join();
        }
    }

    /// Arrange live slots' desktop origins (`auto-row` default, console `manual`
    /// pins win) and commit them in one CCD apply. No-ops for a single member.
    fn apply_group_layout(&self, inner: &mut MgrInner) {
        use crate::layout::{arrange, Member};
        if inner.slots.len() < 2 {
            return;
        }
        let layout_policy = crate::policy::prefs().get().effective().layout;
        // Members in acquire (generation) order — the auto-row order; identity slot 0 = anonymous (no
        // manual pin can address it, so it always auto-rows). `(slot, generation, key, width)`
        // copied out so the arrangement below can write back through `get_mut`.
        let mut ordered: Vec<(u32, u64, CcdTargetKey, i32)> = inner
            .slots
            .iter()
            .map(|(slot, s)| {
                let m = s.mon();
                (*slot, m.generation, m.ccd_key(), m.mode.width as i32)
            })
            .collect();
        ordered.sort_by_key(|&(_, generation, _, _)| generation);
        let members: Vec<Member> = ordered
            .iter()
            .map(|&(slot, _, _, width)| Member {
                identity_slot: (slot != 0).then_some(slot),
                width,
            })
            .collect();
        let placements = arrange(&members, &layout_policy);
        let positions: Vec<(CcdTargetKey, i32, i32)> = ordered
            .iter()
            .zip(&placements)
            .map(|(&(_, _, target, _), p)| (target, p.x, p.y))
            .collect();
        pf_win_display::win_display::apply_source_positions(&positions);
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

    /// Wait for Windows to auto-activate a freshly-ADDed IDD target into its OWN display path and
    /// return its GDI name — the capture target. Shared by the fresh CREATE and the mid-stream
    /// re-arrival ([`re_add`](Self::re_add)). `None` on a GPU-less box (target added but not
    /// WDDM-activated); the capture backend re-resolves once a GPU is present.
    ///
    /// A three-stage ladder, each stage a real failure mode. Plain poll FIRST — never a forced
    /// topology change: bare `SDC_TOPOLOGY_EXTEND` is ACCESS_DENIED from Session 0 on a headless
    /// box and breaks the auto-activate. force-EXTEND is the integrated-screen FALLBACK: a fresh
    /// IDD there is CLONED onto the panel (shared source, no committed path of its own —
    /// observed on an Optimus laptop, commit 8e87e61), and the EXTEND de-clones it. LAST RESORT
    /// is explicit path activation: a lid-closed laptop defeats both (the clamshell policy
    /// suppresses auto-activation and the EXTEND preset "succeeds" committing nothing) —
    /// `activate_target_path` commits the path directly, which ignores the lid policy.
    ///
    /// CAVEAT (unobserved): textbook CCD also allows a clone with a *shared-source ACTIVE* path
    /// (resolve → `Some`), which the `is_none()` gate would miss; widening needs a
    /// `target_is_cloned` helper plus on-laptop validation.
    ///
    /// Call under the `state` lock — this mutates the live CCD topology, and the manager's
    /// sole-topology-mutator contract keeps two acquires from interleaving path commits (a
    /// serialization requirement, not a soundness one: every helper it calls is a safe fn).
    fn resolve_target_gdi(&self, key: CcdTargetKey) -> Option<String> {
        // 50 ms sampling (latency plan P0.5): the SAME 3 s per-stage ceilings — the 3-stage ladder
        // structure encodes real failure modes (headless auto-activate, integrated-panel clone,
        // lid-closed path activation) and is untouched — but a typical activation resolves on an
        // early poll, so finer sampling shaves ~150 ms off every stage crossing.
        if let Some(n) = poll_gdi_name(key) {
            return Some(n);
        }
        force_extend_topology();
        if let Some(n) = poll_gdi_name(key) {
            return Some(n);
        }
        if pf_win_display::win_display::activate_target_path(key) {
            if let Some(n) = poll_gdi_name(key) {
                return Some(n);
            }
        }
        None
    }

    /// ADD via the driver, start the watchdog pinger, resolve the GDI name, force
    /// the mode, apply group topology (first member isolates and captures restore;
    /// a later member re-issues isolate with the grown set).
    ///
    /// Returned `Monitor.mode` is what the OS committed; `requested_mode` keeps
    /// `mode` verbatim for `acquire`'s join/resize gate.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn create_monitor(
        &'static self,
        dev: HANDLE,
        mut mode: Mode,
        slot: u32,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
        hw_cursor: bool,
        inner: &mut MgrInner,
    ) -> Result<Monitor> {
        // Slot id doubles as the driver-preferred monitor id (EDID serial /
        // ConnectorIndex) so Windows reapplies saved DPI on reconnect; `0`
        // (anonymous) = driver auto-allocates.
        let preferred_id = slot;
        // Negotiated mode, before the post-settle read-back overwrites `mode`.
        // The session re-asks for this on every rebuild (see `requested_mode`).
        let requested_mode = mode;
        let render_pin = resolve_render_pin();
        // Gate on v5 so the capture layer does not create a section nobody
        // will publish into (older drivers ignore the AddRequest field).
        let hw_cursor = hw_cursor && self.driver_proto.load(Ordering::Relaxed) >= 5;
        // PRE-MUTATION baseline for the standby-sink selector (immunity plan WP3a): which targets
        // were part of the desktop before THIS acquire touches anything — the ADD's
        // auto-activation, the resolve ladder's force-EXTEND (which can light a sleeping sink!),
        // and the isolate all mutate the active set, so only a snapshot taken here can tell a
        // pre-dark sink from a display we switched off (or lit) ourselves.
        let baseline_active: Vec<CcdTargetKey> =
            if inner.slots.is_empty() && crate::policy::prefs().standby_sink_neutralise() {
                pf_win_display::win_display::target_inventory()
                    .iter()
                    .filter(|t| t.active)
                    .map(|t| t.key)
                    .collect()
            } else {
                Vec::new()
            };
        // SAFETY: `create_monitor`'s own `# Safety` contract guarantees `dev` is the live control
        // handle; we forward it unchanged to `add_monitor`, whose precondition is exactly that.
        // `render_pin` is an `Option<LUID>` by value (plain `Copy`), so no borrowed memory
        // crosses the call.
        let added = unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, preferred_id, client_hdr, hw_cursor)?
        };
        let added_key =
            CcdTargetKey::from_luid_parts(added.luid.LowPart, added.luid.HighPart, added.target_id);

        // Ping inside the watchdog window or the driver tears all displays
        // down. One device-level pinger; started with the first monitor.
        self.ensure_pinger();

        // Resolve the capture target — wait for Windows to auto-activate the freshly-ADDed IDD into its
        // OWN display path, with the integrated-screen clone fallback (shared by the re-arrival path).
        // Its `state`-lock discipline is satisfied: `acquire` holds the lock across this whole call.
        let gdi_name = self.resolve_target_gdi(added_key);
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    target = %added_key,
                    gdi = %n,
                    "IDD target activated into a display path"
                );
                // ADD only advertises; force the mode so DXGI captures the
                // requested size. Exclusive: first member captures restore;
                // later members re-isolate the grown set (never deactivate a
                // sibling). Primary/Extend leave physicals lit.
                set_active_mode(n, mode);
                use crate::policy::Topology;
                let first_member = inner.slots.is_empty();
                match topology_action() {
                    Topology::Exclusive => {
                        // The managed keep-set: every live sibling + the new monitor.
                        let mut keep = inner.target_keys();
                        keep.push(added_key);
                        if first_member {
                            // DDC off BEFORE isolate: an HMONITOR (and the DDC
                            // channel) exists only while the display is still
                            // active. First member only — physicals are already
                            // dark for a sibling. Evidence: `windows/ddc.rs`.
                            if crate::policy::prefs().ddc_power_off() {
                                inner.group.ddc_panels_off = crate::ddc::panel_off_except(n);
                            }
                            // Pin AMD connector EDID before isolate so an
                            // awake sink still answers the live-EDID read.
                            // Emulation outlives the process. First member
                            // only. Evidence: `pf_win_display::adl_emul`.
                            if crate::policy::prefs().edid_lock() {
                                inner.group.edid_locked =
                                    pf_win_display::adl_emul::lock_for_stream();
                            }
                            inner.group.ccd_saved = isolate_displays_ccd_seam(&keep);
                            // After isolate, disable deactivated monitor PnP
                            // devnodes so standby wake events do not cascade.
                            // Evidence: `windows/monitor_devnode.rs`.
                            if crate::policy::prefs().pnp_disable_monitors() {
                                if let Some(saved) = &inner.group.ccd_saved {
                                    inner.group.pnp_disabled =
                                        pf_win_display::monitor_devnode::disable_for_deactivated(
                                            saved, added_key,
                                        );
                                }
                            }
                            // Verified isolate is not durable — see `ensure_exclusive_watch`.
                            inner.group.ccd_exclusive = inner.group.ccd_saved.is_some();
                            if inner.group.ccd_exclusive {
                                self.ensure_exclusive_watch();
                            }
                        } else {
                            // Re-isolate so the fresh member joins the
                            // composited set. Discard the snapshot unless the
                            // first member's isolate failed — then adopt this
                            // one, or teardown cannot restore the physicals.
                            let snap = isolate_displays_ccd_seam(&keep);
                            if inner.group.ccd_saved.is_none() {
                                if let Some(snap) = snap {
                                    tracing::warn!(
                                        "display isolate (CCD): the first member captured no restore \
                                         snapshot (its isolate failed) — adopting this member's, so \
                                         teardown can still put the physical displays back"
                                    );
                                    inner.group.ccd_saved = Some(snap);
                                    inner.group.ccd_exclusive = true;
                                    self.ensure_exclusive_watch();
                                }
                            }
                        }
                    }
                    Topology::Primary if first_member => {
                        // force-EXTEND only when the virtual is the SOLE active display (the
                        // headless auto-activate): on a lit physical the bare EXTEND preset
                        // re-pulls persistence-DB modes and resets a 120 Hz panel to 60; the
                        // reposition below re-supplies queried modes verbatim. An UNKNOWN
                        // answer (query failed) also skips it — never mutate unverified state.
                        let already_extended = match count_other_active(&[added_key]) {
                            Some(n) => n > 0,
                            None => {
                                tracing::warn!(
                                    "display topology=primary — CCD query failed; skipping the \
                                     force-EXTEND (topology state unknown, mutating nothing extra)"
                                );
                                true
                            }
                        };
                        if already_extended {
                            tracing::info!(
                                "display topology=primary — a physical display is already active; \
                                 skipping force-EXTEND (preserves its refresh) before making the \
                                 virtual primary"
                            );
                        } else {
                            force_extend_topology();
                            thread::sleep(Duration::from_millis(300));
                        }
                        inner.group.ccd_saved = set_virtual_primary_ccd(added_key);
                    }
                    Topology::Primary => {
                        // A sibling already holds primary; the new member
                        // just extends. Group layout arranges it.
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
                // Verified-state wait before capture opens. Ceiling 1500 ms.
                // A rejected mode burns the ceiling.
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added_key, mode, Duration::from_millis(1500));
                tracing::info!(
                    settle_ms = settle_start.elapsed().as_millis() as u64,
                    verified = settled,
                    "topology settle (verified-state wait)"
                );
                // Record what actually COMMITTED, not what was asked for — the same read-back
                // `resize_in_place` does, for the same reason. `set_active_mode` deliberately falls
                // back to the highest advertised refresh <= requested rather than lose the client's
                // resolution, and `wait_mode_settled` verifies the RESOLUTION only, so `settled`
                // says nothing about the refresh. Storing the request would make `mon.mode` claim a
                // rate the display is not running: `output_for` hands the capturer that as
                // `preferred_mode` (the encoder then paces to a rate the output never reaches),
                // `/display/state` reports it, and the next Reconfigure diffs against it — a client
                // re-requesting the rate it actually has would pay a needless resize, while one
                // re-requesting the phantom rate takes the plain JOIN branch and never tries again.
                mode = committed_mode_or(added_key, mode);

                // Connected-but-inactive sinks (standby TV) whose wake events
                // the deactivated-set selector misses. After settle so force-
                // EXTEND physicals are not still mid-activation. First member
                // only; Extend leaves active panels untouched by construction.
                if first_member && crate::policy::prefs().standby_sink_neutralise() {
                    if let Some(rest) =
                        Duration::from_millis(1500).checked_sub(settle_start.elapsed())
                    {
                        thread::sleep(rest);
                    }
                    let mut keep = inner.target_keys();
                    keep.push(added_key);
                    for id in pf_win_display::monitor_devnode::disable_connected_inactive(
                        &keep,
                        &baseline_active,
                    ) {
                        if !inner.group.pnp_disabled.contains(&id) {
                            inner.group.pnp_disabled.push(id);
                        }
                    }
                }
            }
            None => tracing::warn!(
                "virtual-display target {added_key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
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
            requested_mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: (0, 0),
            generation: self.generation.fetch_add(1, Ordering::Relaxed),
            hw_cursor,
            cursor_excluded: added.cursor_excluded,
        })
    }

    /// Mid-stream resize on the same monitor: refresh the advertised mode list
    /// (`IOCTL_UPDATE_MODES`, protocol v4), re-enumerate, CCD/GDI force-set.
    /// Target id, GDI name, DPI, swap-chain worker and frame stash survive.
    /// On failure `mon` is untouched and the caller falls back to [`re_add`].
    ///
    /// # Safety
    /// `dev` must be the live control handle; CCD/GDI runs under the `state` lock.
    unsafe fn resize_in_place(&self, dev: HANDLE, mon: &mut Monitor, mode: Mode) -> Result<()> {
        let gdi = mon
            .gdi_name
            .clone()
            .context("in-place resize needs a resolved GDI name")?;
        let t0 = Instant::now();
        // FAST PATH (driver-independent): the OS already offers this resolution — the monitor's
        // arrival list, which since the driver's mode-history union contains every size this
        // identity ever served — so a plain CCD mode set reaches it with no driver round-trip.
        let mon_key = mon.ccd_key();
        let already = pf_win_display::win_display::wait_mode_advertised(&gdi, mode, Duration::ZERO);
        if !already {
            // Out-of-arrival-list. The OS pins the settable set at arrival,
            // so one bounded UPDATE_MODES attempt per process, then latch
            // futile and fail fast to re-arrival (same-id history then makes
            // this size settable in place).
            if self.driver_proto.load(Ordering::Relaxed) < 4 {
                anyhow::bail!(
                    "{}x{} is not in the advertised mode set (v3 driver: in-place reaches only \
                     arrival-list modes)",
                    mode.width,
                    mode.height
                );
            }
            if self.update_modes_futile.load(Ordering::Relaxed) {
                anyhow::bail!(
                    "{}x{} is not in the advertised mode set (UPDATE_MODES latched futile — the \
                     OS pins settable modes at monitor arrival; the re-arrival teaches this size \
                     to the identity's history)",
                    mode.width,
                    mode.height
                );
            }
            tracing::info!(
                old = format!(
                    "{}x{}@{}",
                    mon.mode.width, mon.mode.height, mon.mode.refresh_hz
                ),
                new = format!("{}x{}@{}", mode.width, mode.height, mode.refresh_hz),
                target = mon.target_id,
                "virtual-display: updating the live monitor's modes for an in-place resize"
            );
            // SAFETY: `dev` is the live control handle (this fn's contract); `update_modes`
            // forwards it to a synchronous IOCTL with owned/borrowed locals only.
            unsafe { self.driver.update_modes(dev, &mon.key, mode) }?;
            pf_win_display::win_display::force_mode_reenumeration();
            if !pf_win_display::win_display::wait_mode_advertised(
                &gdi,
                mode,
                Duration::from_millis(800),
            ) {
                self.update_modes_futile.store(true, Ordering::Relaxed);
                anyhow::bail!(
                    "OS did not advertise {}x{} within {}ms of the driver mode-list update \
                     (offers: {:?}) — latching UPDATE_MODES off for this process",
                    mode.width,
                    mode.height,
                    t0.elapsed().as_millis(),
                    pf_win_display::win_display::advertised_resolutions(&gdi)
                );
            }
        }
        let advertised_ms = t0.elapsed().as_millis() as u64;
        set_active_mode(&gdi, mode);
        // Same committed-state predicate as create. Uncommitted within the
        // ceiling routes to the re-arrival fallback.
        let settle_start = Instant::now();
        let settled = wait_mode_settled(mon_key, mode, Duration::from_millis(1500));
        if !settled {
            anyhow::bail!(
                "in-place mode set did not commit within 1.5s (advertised after {advertised_ms} ms)"
            );
        }
        // Record what actually COMMITTED, not what was asked for — see [`committed_mode_or`], which
        // the fresh-create and re-arrival paths share with this one so all three store the same
        // truth: `mon.mode` is what `/display/state` reports and what the capturer paces to.
        let landed = committed_mode_or(mon_key, mode);
        tracing::info!(
            advertised_ms,
            settle_ms = settle_start.elapsed().as_millis() as u64,
            mode = format!("{}x{}@{}", landed.width, landed.height, landed.refresh_hz),
            "in-place resize committed (verified-state wait)"
        );
        mon.mode = landed;
        // Also store what was asked: the session re-requests this on the next
        // acquire. Dropping it would make every rebuild re-enter this path.
        mon.requested_mode = mode;
        Ok(())
    }

    /// Mid-stream resize by monitor re-arrival.
    ///
    /// The driver freezes the advertised mode list at ADD, so an out-of-list
    /// `ChangeDisplaySettingsExW` returns `DISP_CHANGE_BADMODE`. REMOVE then ADD
    /// at the new mode, reusing the slot's stable id so the OS keeps identity and
    /// saved DPI. Visible cost is one hotplug.
    ///
    /// The rebuilt `Monitor` keeps the old `generation` so outstanding leases
    /// still match. Group restore snapshot is preserved (not a first-member
    /// create). Caller owns the slot's `Monitor` + `refs` across this call.
    ///
    /// # Safety
    /// `dev` must be the live control handle; touches live CCD/GDI topology.
    unsafe fn re_add(
        &'static self,
        dev: HANDLE,
        inner: &mut MgrInner,
        slot: u32,
        old: &Monitor,
        mode: Mode,
        client_hdr: Option<punktfunk_core::quic::HdrMeta>,
    ) -> ReAdd {
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
        // Bare REMOVE (no topology restore, pinger stays up). Frees the
        // preferred id so ADD can reuse it. Best-effort: ADD still proceeds
        // on REMOVE failure (driver reaps a stale same-id monitor anyway).

        // SAFETY: `dev` is the live control handle (this fn's contract); `&old.key` borrows the
        // still-owned `MonitorKey`, alive across the synchronous IOCTL.
        if let Err(e) = unsafe { self.driver.remove_monitor(dev, &old.key) } {
            tracing::warn!(
                old_target = old.target_id,
                "re-arrival REMOVE failed (continuing to ADD): {e:#}"
            );
        }
        // Async departure before ADD. Ceiling 400 ms; the driver's ghost-reap
        // ADD retry is the backstop if CCD reports departed early.
        let depart_start = std::time::Instant::now();
        let departed = wait_target_departed(old.ccd_key(), Duration::from_millis(400));
        tracing::info!(
            depart_ms = depart_start.elapsed().as_millis() as u64,
            verified = departed,
            "re-arrival: old monitor departure settle"
        );
        let render_pin = resolve_render_pin();
        // SAFETY: `dev` is the live control handle; `render_pin`/`client_hdr` are owned `Copy`/`Option`
        // values passed by value — no borrow crosses the call.
        let (added, mut mode, rollback_err) = match unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, slot, client_hdr, old.hw_cursor)
        } {
            Ok(a) => (a, mode, None),
            Err(e) => {
                // Old monitor already REMOVEd. Re-ADD at its requested mode
                // (not committed): those differ when refresh was clamped, and
                // re-ADDing the clamp would make the next acquire at the
                // negotiated rate look like another resize.
                let e = e.context("re-arrival ADD at the new mode");
                tracing::warn!(
                    slot,
                    error = %format!("{e:#}"),
                    "re-arrival ADD failed — rolling back to the previous mode"
                );
                // SAFETY: `dev` is the live control handle; args are owned `Copy`/`Option`.
                match unsafe {
                    self.driver.add_monitor(
                        dev,
                        old.requested_mode,
                        render_pin,
                        slot,
                        client_hdr,
                        old.hw_cursor,
                    )
                } {
                    Ok(a) => (a, old.requested_mode, Some(e)),
                    Err(e2) => {
                        tracing::error!(
                            slot,
                            error = %format!("{e2:#}"),
                            "re-arrival rollback ADD also failed — the slot now has NO monitor"
                        );
                        return ReAdd::Lost(e.context(format!("rollback also failed: {e2:#}")));
                    }
                }
            }
        };
        // What this ADD asked for, before the post-settle read-back overwrites
        // `mode`. Join/resize gate's other side (see `Monitor::requested_mode`).
        let requested_mode = mode;
        self.ensure_pinger();
        let added_key =
            CcdTargetKey::from_luid_parts(added.luid.LowPart, added.luid.HighPart, added.target_id);
        // 3. Resolve the NEW target's GDI name (target_id changes across a re-arrival). Under the
        //    `state` lock, as its topology-mutator discipline requires.
        let gdi_name = self.resolve_target_gdi(added_key);
        match &gdi_name {
            Some(n) => {
                tracing::info!(
                    backend = self.driver.name(),
                    "re-arrival target {added_key} -> {n}"
                );
                // ADD only advertises; force the mode so DXGI/IDD capture the new size.
                set_active_mode(n, mode);
                // 4. Re-isolate the composited set with the NEW target replacing the old — preserving
                //    the group's first-member restore snapshot. Under the `state` lock (the caller
                //    holds it and lent us `inner`), as its topology-mutator discipline requires.
                self.reisolate_after_swap(inner, added_key);
                // Topology settle before capture reopens: verified-state wait, ceiling = the old
                // fixed 1500 ms sleep (latency plan P0.2 — the re-arrival twin).
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added_key, mode, Duration::from_millis(1500));
                tracing::info!(
                    settle_ms = settle_start.elapsed().as_millis() as u64,
                    verified = settled,
                    "re-arrival topology settle (verified-state wait)"
                );
                // Store what COMMITTED, not what was asked for — the settle above verifies the
                // resolution only, so it is no evidence about the refresh (see
                // [`committed_mode_or`]). Doing this here rather than at the `Monitor` construction
                // below keeps it on the arm where a path actually exists: with no GDI name there is
                // no committed mode to read, and the request stands.
                mode = committed_mode_or(added_key, mode);
            }
            None => tracing::warn!(
                "re-arrival target {added_key} not yet an active display path (auto-activate, \
                 EXTEND preset and explicit path activation all failed — GPU-less box?)"
            ),
        }
        // Preserve `generation` (lease continuity) and group-layout `position`.
        // A fresh generation would strand the old session's release.
        let mon = Box::new(Monitor {
            key: added.key,
            target_id: added.target_id,
            luid: added.luid,
            render_pin,
            wudf_pid: added.wudf_pid,
            gdi_name,
            mode,
            requested_mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: old.position,
            generation: old.generation,
            hw_cursor: old.hw_cursor,
            // Fresh from this reply, not `old`: the driver's per-target
            // declare registry is ground truth.
            cursor_excluded: added.cursor_excluded,
        });
        match rollback_err {
            None => ReAdd::Arrived(mon),
            Some(err) => ReAdd::RolledBack { mon, err },
        }
    }

    /// Re-isolate after [`re_add`] put a new target in place of the old one,
    /// without recapturing the group restore snapshot. The old slot is already
    /// gone from the map, so `inner.target_ids()` is surviving siblings.
    ///
    /// Call under the `state` lock — it commits a new CCD topology, so it must not interleave with
    /// another slot transition's commit. A *serialization* requirement, not a soundness one: every
    /// helper it reaches (`isolate_displays_ccd_seam`, `set_virtual_primary_ccd`) is a safe fn, so
    /// this body performs no unsafe operation. (`&mut MgrInner` already proves the lock is held.)
    fn reisolate_after_swap(&self, inner: &mut MgrInner, new_target: CcdTargetKey) {
        use crate::policy::Topology;
        match topology_action() {
            Topology::Exclusive => {
                // Grown-set semantics: isolate to the surviving siblings + the new target. The returned
                // snapshot is DISCARDED — the group keeps the first member's (design §6.1).
                let mut keep = inner.target_keys();
                keep.push(new_target);
                let _ = isolate_displays_ccd_seam(&keep);
            }
            Topology::Primary => {
                // Predecessor held primary. The call recaptures a snapshot, so
                // save/restore the group's around it.
                let keep_saved = inner.group.ccd_saved.take();
                let _ = set_virtual_primary_ccd(new_target);
                inner.group.ccd_saved = keep_saved;
            }
            Topology::Extend | Topology::Auto => {
            }
        }
    }

    /// Tear down `mon`, already removed from `inner.slots`. Last member: stop
    /// the pinger and restore group topology. Non-last: re-issue isolate over
    /// the shrunk set. Then REMOVE. Consumes `mon`.
    ///
    /// # Safety
    /// `dev` must be the live control handle.
    unsafe fn teardown_removed(&self, dev: HANDLE, inner: &mut MgrInner, mon: Monitor) {
        // Runs under the `state` lock, so a REMOVE/CCD-restore that never
        // returns blocks every future `acquire` with nothing in the log.
        // One ERROR after 10 s turns that silent wedge into a diagnosis.
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
            // Last slot: stop the pinger first, then restore first-in/last-out.
            self.stop_pinger();
            // Watchdog must be gone before restore — it would read the restored
            // topology as "lost exclusivity" and re-fight it.
            self.stop_exclusive_watch();
            // Re-enable PnP first and let them re-arrive, so CCD restore
            // finds monitors that exist. Outside the ccd_saved gate: the
            // connected-inactive sweep also runs in Extend/Primary.
            let pnp_disabled = std::mem::take(&mut inner.group.pnp_disabled);
            if !pnp_disabled.is_empty() {
                pf_win_display::monitor_devnode::enable_instances(&pnp_disabled);
                thread::sleep(Duration::from_millis(300));
            }
            // Re-attach detached displays BEFORE REMOVE so the box is never
            // left with zero displays.
            inner.group.ccd_exclusive = false;
            if let Some(saved) = inner.group.ccd_saved.take() {
                restore_displays_ccd(&saved);
            }
            // Clear the isolate crash journal even when there was no snapshot
            // (failed isolate leaves `ccd_saved` None). The group is gone.
            pf_win_display::win_display::isolate_journal::clear();
            // DDC wake outside the `ccd_saved` gate: panels were commanded
            // dark before isolate, which can return None. Nested in that arm
            // the wake never ran and the live link never DPMS-wakes them.
            // 300 ms lets re-activated paths show up in EnumDisplayMonitors.
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
            // Unlock after CCD restore + DDC wake so the driver does not
            // re-probe sinks mid-restore. Outside `ccd_saved` for the same
            // reason as the DDC wake — lock ran before isolate.
            if inner.group.edid_locked {
                pf_win_display::adl_emul::unlock_after_stream();
                inner.group.edid_locked = false;
            }
        } else {
            match shrink_action(inner.group.ccd_exclusive, inner.group.ccd_saved.is_some()) {
                // Re-issue isolate over the shrunk set. Snapshot discarded;
                // the group keeps the first member's.
                ShrinkAction::Reisolate => {
                    let keep = inner.target_keys();
                    let _ = isolate_displays_ccd_seam(&keep);
                }
                // Re-promote a survivor rather than leave primary on a target
                // about to be REMOVEd. Save/restore the snapshot: the call
                // recaptures one and the group must keep the first member's.
                ShrinkAction::RepromotePrimary => {
                    if let Some(&survivor) = inner.target_keys().first() {
                        let keep_saved = inner.group.ccd_saved.take();
                        let _ = set_virtual_primary_ccd(survivor);
                        inner.group.ccd_saved = keep_saved;
                    }
                }
                ShrinkAction::Nothing => {}
            }
        }
        // SAFETY: `teardown_removed`'s own `# Safety` contract guarantees `dev` is the live control
        // handle, and `remove_monitor` requires exactly that. `&mon.key` borrows the `MonitorKey`
        // inside the still-owned `mon`, alive for this synchronous IOCTL, so the pointer the driver
        // reads stays valid.
        if let Err(e) = unsafe { self.driver.remove_monitor(dev, &mon.key) } {
            // Device died under this monitor — retire so the next session reopens.
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
        // Re-arrange survivors. Leaving never called `apply_group_layout`,
        // so CCD origins kept a gap where the departing monitor sat. After
        // REMOVE so the departing path is gone. No-op below two members.
        self.apply_group_layout(inner);
        done.store(true, Ordering::SeqCst);
    }

    /// Release a session's hold. Last session lingers unless `quit_now` (QUIT
    /// code): then tear down immediately so a reconnect finds Idle instead of
    /// the Lingering-preempt REMOVE→ADD. `keep_alive = forever` outranks quit —
    /// only `/display/release` frees a pin. A stale lease is a no-op.
    fn release(&self, slot: u32, generation: u64, quit_now: bool) {
        let mut inner = self.state.lock().unwrap();
        let stale = match inner.slots.get(&slot) {
            Some(s) => s.mon().generation != generation,
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
            // Pin before considering quit: keep_alive=forever means the
            // screen stays alive. A deliberate quit skips linger, never pin.
            SlotState::Active { mon, .. } if keep_alive_forever() => {
                tracing::info!(
                    slot,
                    "virtual-display: last session left — PINNED (keep_alive=forever); free via /display/release"
                );
                inner.slots.insert(slot, SlotState::Pinned { mon });
            }
            // Deliberate quit: tear down now, under the state lock so a racing
            // `acquire` waits rather than ADD into an in-flight REMOVE.
            // `device_handle()` None is impossible with a live monitor; fall
            // back to Lingering (timer retries) rather than leak.
            SlotState::Active { mon, .. } if quit_now => match self.device_handle() {
                Some(dev) => {
                    tracing::info!(
                        slot,
                        "virtual-display: last session left (deliberate quit) — tearing down now, linger skipped"
                    );
                    // SAFETY: `teardown_removed` requires `dev` to be the live control handle; the
                    // `dev` Arc from `device_handle()` (the `Some` checked above) is held across
                    // this call, so the handle stays open. `mon` was moved out of the map under the
                    // `state` lock, so it is exclusively owned here — no aliasing.
                    unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
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
            // Kept slot has no live hold — stale/duplicate release; put it back.
            other => {
                inner.slots.insert(slot, other);
            }
        }
    }

    /// Begin IDD-push session setup. Serializes via the manager-wide setup lock,
    /// registers this session's stop flag on its slot, signals the prior holder
    /// to stop, and waits for release so a reconnect (dead reused swap-chain)
    /// preempts cleanly. A different identity is admission, never a preempt.
    /// Caller holds the guard across pipeline build.
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
                // Prior session still Active. `acquire` preempts Lingering
                // only (so build-retries join), which would JOIN this stuck
                // monitor's dead swap-chain. Force-preempt once here under
                // `setup_lock` — not inside `acquire`, which would re-churn.
                if let Some(dev) = self.device_handle() {
                    let mut inner = self.state.lock().unwrap();
                    let taken = match inner.slots.get(&slot) {
                        Some(SlotState::Active { .. }) => inner.slots.remove(&slot),
                        // Raced to Lingering/empty between the wait and here.
                        _ => None,
                    };
                    if let Some(SlotState::Active { mon, .. }) = taken {
                        tracing::warn!(
                            slot,
                            old_target = mon.target_id,
                            "IDD-push setup: force-preempting the stuck-Active prior monitor (its IddCx swap-chain is dead)"
                        );
                        // SAFETY: `teardown_removed` requires `dev` to be the live control handle;
                        // the `dev` Arc from `device_handle()` (the `Some` checked above) is held
                        // across this call, so the handle stays open. `mon` was moved out of the
                        // map under the `state` lock, so it is exclusively owned here — no aliasing.
                        unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
                        // Async departure before the next ADD (same 400 ms
                        // ceiling as acquire's Lingering-preempt).
                        thread::sleep(Duration::from_millis(400));
                    }
                }
            }
        }
        guard
    }

    /// Wait up to `timeout` for `slot` to leave Active. Used after signalling
    /// the old IDD-push session to stop. `false` ⇒ caller force-preempts.
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

    /// Background timer (started once): tear down a monitor past its linger
    /// deadline so a physical-screen user gets their screen back.
    fn ensure_linger_timer(&'static self) {
        static TIMER: Once = Once::new();
        TIMER.call_once(|| {
            thread::Builder::new()
                .name("vdisplay-linger".into())
                .spawn(move || {
                    loop {
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
                                // Teardown under the state lock. Dropping it
                                // first let a concurrent acquire ADD + isolate
                                // while this REMOVE/restore was in flight; the
                                // late restore then de-isolated the new session.

                                // SAFETY: `teardown_removed` requires a valid control handle; the `dev`
                                // Arc from `self.device_handle()` is held across this call, so the
                                // handle stays open (a concurrent retire drops only the manager's
                                // reference; see `DeviceSlot`). `mon` was moved out of the map under
                                // the lock, so it is exclusively owned here.
                                unsafe { self.teardown_removed(dev_raw(&dev), &mut g, mon) };
                            }
                        }
                    }
                })
                .ok();
        });
    }
}

/// Session refcount handle on its slot. `Drop` releases; a stale lease is a no-op.
struct MonitorLease {
    mgr: &'static VirtualDisplayManager,
    slot: u32,
    generation: u64,
    /// Client closed with QUIT (user stop, not a network drop). Read at drop:
    /// a quit tears the monitor down now instead of lingering. `None` = linger.
    quit: Option<Arc<AtomicBool>>,
}

impl Drop for MonitorLease {
    fn drop(&mut self) {
        let quit_now = self.quit.as_ref().is_some_and(|q| q.load(Ordering::SeqCst));
        self.mgr.release(self.slot, self.generation, quit_now);
    }
}

/// Slot id for a client's monitor: stable per-client identity (`1..=15`) or
/// `0` for anonymous/GameStream (at most one; no identity to find another
/// slot by). Shared by `acquire` and [`VirtualDisplayManager::begin_idd_setup`].
pub fn slot_id_for(client_fp: Option<[u8; 32]>, mode: (u32, u32)) -> u32 {
    super::identity::resolve_slot(client_fp, mode, crate::policy::Identity::PerClient).unwrap_or(0)
}

/// Render-GPU pin: IDD-push NVENC runs on the render adapter, so it must be
/// the selected encoder GPU. Selection lives in
/// [`pf_gpu::resolve_render_adapter_luid`].
fn resolve_render_pin() -> Option<LUID> {
    tracing::info!("IDD push: pinning the render GPU (SET_RENDER_ADAPTER)");
    pf_gpu::resolve_render_adapter_luid()
}

/// A reused monitor keeps the ADD-time render pin. If the current pick has
/// moved, say so: the session follows the pin; the new pick takes effect on
/// the next CREATE (`/display/release`). Compare against the pin, not
/// `mon.luid` (that is the IddCx display adapter).
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

/// Read-only view of one managed slot for `/display/state`.
pub(crate) struct ManagedInfo {
    pub backend: &'static str,
    pub mode: (u32, u32, u32),
    /// `"active"` | `"lingering"` | `"pinned"`.
    pub state: &'static str,
    /// Milliseconds until linger teardown (`None` when active or pinned).
    pub expires_in_ms: Option<u64>,
    pub sessions: u32,
    /// Generation stamp — the `/display/release` slot arg.
    pub generation: u64,
    pub slot_id: u32,
    pub position: (i32, i32),
}

impl VirtualDisplayManager {
    /// Snapshot managed slots in acquire (generation) order. Empty when none live.
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
                    SlotState::Pinned { mon } => (mon, "pinned", 0u32, None),
                };
                ManagedInfo {
                    backend: self.driver.name(),
                    mode: (mon.mode.width, mon.mode.height, mon.mode.refresh_hz),
                    state,
                    expires_in_ms,
                    sessions,
                    generation: mon.generation,
                    slot_id: *slot,
                    position: mon.position,
                }
            })
            .collect();
        out.sort_by_key(|i| i.generation);
        out
    }

    /// Tear down kept (Lingering or Pinned) monitors now (`/display/release`).
    /// `slot` is a [`ManagedInfo::generation`]; `None` releases every kept one.
    /// Active monitors are refused. Returns the number released.
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
                    if slot.is_none_or(|g| g == mon.generation) =>
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
                // SAFETY: `teardown_removed` needs a live control handle; the `dev` Arc from
                // `device_handle()` is held across this call, so the handle stays open (see
                // `DeviceSlot`). `mon` was moved out of the map under the `state` lock, so it is
                // exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
                released += 1;
            }
        }
        released
    }
}

/// Snapshot managed slots. Empty when no backend has initialised yet.
pub(crate) fn snapshot() -> Vec<ManagedInfo> {
    VDM.get()
        .map(VirtualDisplayManager::snapshot)
        .unwrap_or_default()
}

/// Force-release kept monitors (`slot` = generation stamp, `None` = all).
pub(crate) fn force_release(slot: Option<u64>) -> usize {
    VDM.get().map(|m| m.force_release(slot)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{needs_resize, shrink_action, Mode, ShrinkAction};

    const fn m(width: u32, height: u32, refresh_hz: u32) -> Mode {
        Mode {
            width,
            height,
            refresh_hz,
        }
    }

    /// Non-last-member teardown keys off `ccd_exclusive`, not `has_saved`.
    /// `Primary` also snapshots; the old gate ran exclusive isolate on it.
    #[test]
    fn a_primary_group_shrink_repromotes_instead_of_isolating() {
        // Snapshot without exclusivity: Primary, not isolate.
        assert_eq!(
            shrink_action(false, true),
            ShrinkAction::RepromotePrimary,
            "a Primary group must never run the exclusive isolate on a shrink"
        );
        assert_eq!(shrink_action(true, true), ShrinkAction::Reisolate);
        assert_eq!(shrink_action(false, false), ShrinkAction::Nothing);
    }

    /// Exclusive still re-isolates when the snapshot is missing: physicals
    /// are deactivated either way.
    #[test]
    fn exclusivity_decides_without_a_snapshot() {
        assert_eq!(shrink_action(true, false), ShrinkAction::Reisolate);
    }

    /// Session re-asks the negotiated rate on every rebuild; that must join
    /// even when the OS clamped refresh. Keying on committed alone hotplugs
    /// every attempt.
    #[test]
    fn a_reacquire_at_the_negotiated_mode_joins_even_when_the_os_clamped_the_refresh() {
        let requested = m(5120, 1440, 240);
        let committed = m(5120, 1440, 120);
        assert!(
            !needs_resize(requested, committed, requested),
            "re-asking for the negotiated mode must JOIN, not hotplug the monitor"
        );
        assert!(!needs_resize(requested, committed, committed));
        assert!(needs_resize(requested, committed, m(3840, 2160, 120)));
        assert!(needs_resize(requested, committed, m(5120, 1440, 60)));
    }

    /// No clamp: both fields agree, so the gate is plain mode equality.
    #[test]
    fn without_a_clamp_the_gate_is_plain_mode_equality() {
        let mode = m(1920, 1080, 60);
        assert!(!needs_resize(mode, mode, mode));
        assert!(needs_resize(mode, mode, m(2560, 1440, 60)));
    }
}
