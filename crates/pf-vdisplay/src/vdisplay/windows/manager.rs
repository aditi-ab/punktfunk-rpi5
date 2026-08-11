//! Host-lifetime virtual-display **ownership model** (Goal-1 §2.5). One reference-counted monitor
//! lifecycle, born as the shared half of two Windows backends (SudoVDA + pf-vdisplay) so the two
//! verbatim-duplicated `MGR: Mutex<Mgr>` globals could go; the SudoVDA backend has since been
//! removed, so pf-vdisplay is the sole driver behind the seam.
//!
//! [`VirtualDisplayManager`] owns the earned Idle/Active/Lingering refcount machine + the linger timer +
//! a **typed** [`OwnedHandle`] control device (no more raw `isize` smuggled across the pinger/linger
//! threads). The driver-specific part — the IOCTL protocol and the per-monitor REMOVE key — is the only
//! thing behind the [`VdisplayDriver`] seam; the state machine, the render-adapter pin decision, the
//! GDI/CCD glue (`pf_win_display::win_display`), and the generation-stamped [`MonitorLease`] are driver-neutral.
//!
//! It's a process-wide singleton ([`vdm`]) initialised once with the driver — the host runs exactly
//! one virtual-display backend per process. The session holds a [`MonitorLease`];
//! its `Drop` releases the refcount (a *stale* lease — its monitor was preempted + recreated under it —
//! is a no-op, so it can never tear down the live monitor).

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
    wait_target_departed, SavedConfig,
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
    /// This monitor was ADDed with the v5 hardware-cursor flag (already driver-proto-gated) —
    /// preserved across a re-arrival resize so the recreated monitor keeps the cursor channel.
    hw_cursor: bool,
    /// The ADD reply flagged this monitor's OS target as carrying an IRREVOCABLE hardware-cursor
    /// declare from an earlier session (§8.6): DWM excludes the pointer from its frames forever
    /// (the sticky state survives monitor REMOVE→ADD via the stable per-client target id), so a
    /// session WITHOUT the cursor channel must self-composite — carried into
    /// [`WinCaptureTarget`] for the IDD-push capturer's forced-composite gate.
    cursor_excluded: bool,
    /// The driver's WUDFHost pid (from the ADD reply) — carried into [`WinCaptureTarget`] so the
    /// IDD-push capturer knows where to duplicate the sealed frame channel's handles. The SAME
    /// process for every parallel monitor (one devnode → one WUDFHost hosts all publishers), which
    /// is why WUDFHost death is ALL-slot shared fate.
    wudf_pid: u32,
    gdi_name: Option<String>,
    /// The mode the OS actually COMMITTED for this monitor, not the one the client asked for — all
    /// three paths that write it (create, re-arrival, in-place resize) read it back through
    /// [`committed_mode_or`]. It is what `output_for` hands the capturer as `preferred_mode` and
    /// what `/display/state` reports, so a requested-but-never-committed refresh here mis-paces the
    /// encoder. It is NOT, on its own, the resize discriminator — see `requested_mode` below.
    mode: Mode,
    /// The mode the monitor was ASKED for at its last ADD/mode-set — the client's negotiated mode,
    /// verbatim.
    ///
    /// Kept beside the committed one because [`needs_resize`] is a two-sided question and `mode`
    /// alone cannot answer it. `set_active_mode` deliberately commits the highest advertised refresh
    /// <= the requested one rather than lose the client's resolution, so on a box that will not
    /// advertise the negotiated rate (5120x1440@240 = 1.77 Gpix/s is the documented example) the two
    /// fields PERMANENTLY disagree. If `acquire` diffed the incoming request against `mode` only,
    /// every later acquire at the very mode the session already negotiated would read as a
    /// mid-stream resize — and since `slot_id_for` keys on resolution alone, a refresh-only
    /// divergence stays in the same slot and the divergence re-records itself on each pass. That is
    /// not academic: `build_pipeline_with_retry` takes a retry-hold lease and then EVERY build
    /// attempt re-`create`s the identical mode expecting a refcount++ join, so the slot would take
    /// an in-place-resize attempt and then a full REMOVE→ADD hotplug per attempt — the exact churn
    /// that exhausts the IddCx monitor-slot pool and wedges ADD at 0x80070490.
    requested_mode: Mode,
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
                cursor_excluded: self.cursor_excluded,
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
    /// Whether `ccd_saved` was captured by an EXCLUSIVE isolate (vs `Primary`, which also
    /// snapshots but deliberately keeps the physical displays active) — gates the re-assert
    /// watchdog, which must never "fix" a Primary group's lit panels. Cleared with the restore.
    ccd_exclusive: bool,
}

/// How a mid-stream re-arrival ([`VirtualDisplayManager::re_add`]) ended.
///
/// Three-way on purpose. `re_add` REMOVEs the old driver monitor before it ADDs the new one, so
/// once the ADD fails the old monitor is GONE — and the caller used to answer that by putting its
/// `Monitor` struct back in the slot, complete with a dead `key`, a departed `target_id` and a
/// stale `gdi_name`. Every later `acquire` then joined a monitor that did not exist.
enum ReAdd {
    /// Re-arrived at the requested mode.
    Arrived(Box<Monitor>),
    /// The ADD failed and the OLD mode was re-ADDed instead, so the session keeps streaming what it
    /// already had. The monitor is REAL but new — a re-arrival mints a fresh `target_id` — which is
    /// why the caller must store THIS one and not the struct it handed in.
    RolledBack {
        mon: Box<Monitor>,
        err: anyhow::Error,
    },
    /// The ADD failed and so did the rollback: this slot has no driver monitor at all. The caller
    /// must leave it EMPTY so the next `acquire` creates one, rather than joining a phantom.
    Lost(anyhow::Error),
}

/// What a NON-LAST-member teardown owes the group's topology.
///
/// Split out of [`VirtualDisplayManager::teardown_removed`] so the gate is testable without a driver, a CCD
/// device or a desktop — the Windows half of this crate has no other way to pin a decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShrinkAction {
    /// Exclusive group: re-issue the isolate over the shrunk managed set.
    Reisolate,
    /// Primary group: re-promote a survivor — the departing member may have been holding primary.
    RepromotePrimary,
    /// Extend/Auto, or nothing was ever captured: leave the topology alone.
    Nothing,
}

/// One stage of [`VirtualDisplayManager::resolve_target_gdi`]'s ladder: poll for the target's GDI name until
/// the 3 s ceiling. 50 ms sampling (latency plan P0.5) — a typical activation resolves on an early
/// poll, so finer sampling shaves ~150 ms off every stage crossing.
///
/// Extracted because the ladder ran this loop three times verbatim, and the 2nd and 3rd copies
/// documented themselves as "SAFETY: as the resolve loop above" — a pointer to a proof rather than
/// a proof, which silently rots the moment the block it points at moves.
fn poll_gdi_name(target_id: u32) -> Option<String> {
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(50));
        if let Some(n) = resolve_gdi_name(target_id) {
            return Some(n);
        }
    }
    None
}

/// Test-only fault injection for the CCD isolate.
///
/// Every Phase-3 recovery leg in this file fires only when [`isolate_displays_ccd`] returns `None`,
/// and on healthy hardware it never does — which is exactly why those legs shipped unexercised on
/// glass. This counter lets a live test fail the next N isolates against the REAL driver and a REAL
/// panel, so the recovery is observed rather than reasoned about.
///
/// `#[cfg(test)]`-only on purpose: this crate's live hardware tests compile under `cfg(test)`, so
/// the seam is reachable where it is needed WITHOUT shipping a production knob that could leave
/// display isolation silently disabled on an operator's box.
#[cfg(test)]
pub(crate) static FAIL_NEXT_ISOLATES: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// [`isolate_displays_ccd`] with the test seam in front of it. Every call site in this file goes
/// through here so an injected failure exercises the same gates a real one would.
fn isolate_displays_ccd_seam(keep_target_ids: &[u32]) -> Option<SavedConfig> {
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
                keep = ?keep_target_ids,
                "TEST fault injection: forcing isolate_displays_ccd -> None"
            );
            return None;
        }
    }
    isolate_displays_ccd(keep_target_ids)
}

/// Decide the [`ShrinkAction`] a NON-LAST-member teardown owes the group.
///
/// `ccd_exclusive` is the discriminator, NOT `has_saved`: `Topology::Primary` stores a `ccd_saved`
/// snapshot too (from `set_virtual_primary_ccd`), so keying on the snapshot ran the EXCLUSIVE
/// isolate on a Primary group — clearing `DISPLAYCONFIG_PATH_ACTIVE` on every non-kept path, i.e.
/// blanking the very physical displays `Primary` exists to keep lit. (This paragraph had been
/// concatenated onto `poll_gdi_name`'s doc with no blank line between them, so the only written
/// record of the Phase-3.3 gate documented an unrelated polling helper and this fn read as
/// undocumented — a maintainer's invitation to "simplify" it back to the broken predicate.)
fn shrink_action(ccd_exclusive: bool, has_saved: bool) -> ShrinkAction {
    if ccd_exclusive {
        ShrinkAction::Reisolate
    } else if has_saved {
        ShrinkAction::RepromotePrimary
    } else {
        ShrinkAction::Nothing
    }
}

/// The mode `target_id` is ACTUALLY running, for a caller about to RECORD it, with `requested` as
/// the fallback whenever the read-back cannot be trusted.
///
/// Every path that stores a `Monitor.mode` owes this call: `set_active_mode` deliberately commits
/// the highest advertised refresh <= the requested one rather than lose the client's resolution, and
/// the `wait_mode_settled` that precedes every store verifies the RESOLUTION only — so a `true`
/// settle is no evidence at all about the refresh. `active_mode`'s own doc states the contract:
/// "Callers that RECORD a mode must record this, or they claim a refresh the display is not
/// running."
///
/// Deliberately narrowed to the REFRESH. A read-back that FAILS, or that reports a different
/// RESOLUTION, keeps `requested`: the create path proceeds even when its settle timed out, so the
/// OS may still be sitting on its own default there, and recording that would hand the capturer +
/// the client a size nobody negotiated. The capturer already re-resolves the live size on its own
/// (`active_resolution` poll, game-capture GB1); the refresh is the field only this read-back can
/// answer.
fn committed_mode_or(target_id: u32, requested: Mode) -> Mode {
    let Some((width, height, refresh_hz)) = pf_win_display::win_display::active_mode(target_id)
    else {
        return requested;
    };
    if (width, height) != (requested.width, requested.height) {
        tracing::warn!(
            target_id,
            requested = format!("{}x{}", requested.width, requested.height),
            active = format!("{width}x{height}"),
            "the OS is not running the requested resolution after the settle — recording the \
             requested mode (the capturer re-resolves the live size itself)"
        );
        return requested;
    }
    if refresh_hz != requested.refresh_hz {
        tracing::info!(
            target_id,
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

/// Does an acquire for `want` on a live monitor need a mid-stream resize, or can it JOIN?
///
/// Two modes describe one monitor and the caller may legitimately name either: `requested` is what
/// the session negotiated and re-asks for on every rebuild attempt, `committed` is what the OS
/// actually runs (they differ exactly when the driver would not advertise the negotiated refresh —
/// see [`committed_mode_or`]). Matching EITHER means there is nothing to do: asking again for the
/// negotiated mode cannot get a better result than the ADD already got, and asking for the mode the
/// display is already running is satisfied by definition. Only a genuinely NEW mode is a resize.
///
/// Keying on `committed` alone was the trap: it turned every same-mode re-acquire on a
/// refresh-clamping box into an in-place-resize attempt followed by a REMOVE→ADD hotplug, forever
/// (nothing ever re-records the negotiated rate, so the mismatch is self-perpetuating).
fn needs_resize(requested: Mode, committed: Mode, want: Mode) -> bool {
    want != requested && want != committed
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
/// watchdog, so one thread serves N monitors). Also reused as the stop+join pair for the
/// exclusive-topology re-assert watchdog (same lifecycle shape).
struct Pinger {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

/// The manager's control-device cache. Reopenable: a driver upgrade / WUDFHost restart kills the
/// cached handle (every IOCTL fails with a gone-class code forever), so such a failure RETIRES it and
/// the next [`VirtualDisplayManager::ensure_device`] reopens the (new) device interface, re-running
/// the version handshake.
///
/// Ownership is `Arc` all the way out: every consumer — `acquire`'s IOCTL runs, the pinger/linger
/// threads, the capture layer's delivery closures — holds its OWN clone across its use, so retiring
/// here merely drops the manager's reference and the handle CLOSES when the last in-flight user
/// drains. That close is load-bearing, not housekeeping: an open control handle is exactly what
/// vetoes the PnP disable/restart the wake-from-sleep recovery leans on (field 2026-08-08 — every
/// reload REFUSED `Generic failure`; `reset-pf-vdisplay.ps1` stops the whole host service precisely
/// to get its handles closed, and Arc ownership buys the same release without dying). The previous
/// contract kept retired handles open for the process lifetime because bare `HANDLE` copies were
/// smuggled into threads and closures; those copies are gone, and nothing may rely on a dead
/// handle staying open again.
#[derive(Default)]
struct DeviceSlot {
    current: Option<Arc<OwnedHandle>>,
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
    /// The driver's handshake-reported protocol version (0 until the first open). The in-place
    /// resize (latency plan P2) gates its UPDATE_MODES attempt on `>= 4`; a v3 driver keeps the
    /// already-advertised fast path + the re-arrival fallback.
    driver_proto: AtomicU32,
    /// Latched `true` after an UPDATE_MODES round-trip failed to make the new mode settable —
    /// on-glass (build 26200) the OS pins a monitor's settable set at ARRIVAL (it re-parses our
    /// description + re-queries target modes, then ignores both), so every further attempt for an
    /// out-of-arrival-list mode would only waste ~1 s per resize before the same re-arrival
    /// fallback. One attempt per process, in case a future OS build honors the refresh.
    update_modes_futile: AtomicBool,
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
    /// The exclusive-topology re-assert watchdog (same stop+join shape as [`Pinger`]), running
    /// while an EXCLUSIVE group isolate is live — a verified isolate is not durable on hybrid
    /// boxes (see [`Self::ensure_exclusive_watch`]).
    exclusive_watch: Mutex<Option<Pinger>>,
    // The per-client stable monitor-id map is now the process-wide `super::identity::global()`
    // (shared with the Linux KWin backend's per-slot naming — never same-process). A monitor CREATE
    // resolves the client's id via `identity::resolve_slot`, so it keeps the same EDID serial + IddCx
    // ConnectorIndex across reconnects and Windows reapplies its saved per-monitor DPI scaling.
}

static VDM: OnceLock<VirtualDisplayManager> = OnceLock::new();

/// Bumped on every exclusive-topology EVICTION the re-assert watchdog performs. An eviction is a
/// real topology change, and the OS drives COMMIT_MODES on every live path for those — the IDD
/// display's swap-chain is recreated driver-side while the session's capture ring keeps waiting
/// on the old attachment, so frames stop silently (on-glass: panel evicted, stream frozen). The
/// manager can only announce; the SESSION owns the ring + encoder — stream loops sample this at
/// start and rebuild their capture attachment in place when it advances.
static TOPOLOGY_REASSERT_GEN: AtomicU64 = AtomicU64::new(0);

/// The current exclusive-eviction generation (see [`TOPOLOGY_REASSERT_GEN`]).
pub fn topology_reassert_gen() -> u64 {
    TOPOLOGY_REASSERT_GEN.load(Ordering::Relaxed)
}

/// Initialise the process-wide manager with `driver` (the chosen backend) and return it. Idempotent: the
/// first backend to call wins (the host runs one backend per process), so a later call ignores its driver.
pub(crate) fn init(driver: Box<dyn VdisplayDriver>) -> &'static VirtualDisplayManager {
    VDM.get_or_init(|| VirtualDisplayManager {
        driver,
        device: Mutex::new(DeviceSlot::default()),
        watchdog_s: AtomicU32::new(3),
        driver_proto: AtomicU32::new(0),
        update_modes_futile: AtomicBool::new(false),
        gen: AtomicU64::new(1),
        state: Mutex::new(MgrInner::default()),
        setup_lock: Mutex::new(()),
        idd_session_stops: Mutex::new(std::collections::HashMap::new()),
        pinger: Mutex::new(None),
        exclusive_watch: Mutex::new(None),
    })
}

/// The process-wide manager. Panics if reached before a backend called [`init`] — by construction a
/// session is only ever created after `vdisplay::open` constructed the backend (which calls `init`).
pub fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

/// Can this host's pf-vdisplay driver run the v5 hardware-cursor channel? Reads the
/// handshake-latched protocol version, opening the control device once if no session has
/// opened it yet this service run (the same open every session performs anyway) — so the
/// Welcome-time capability decision never guesses. `false` when the driver is missing/stale.
///
/// The FIRST session's Welcome precedes any backend construction (`vdisplay::open` runs at
/// display prep, after the Welcome), so this must not assume an initialised manager —
/// `init` is idempotent and constructing the driver facade is free (on-glass finding: the
/// `vdm()` expect panicked the very first handshake of a fresh service).
pub fn hw_cursor_capable() -> bool {
    let m = init(Box::new(crate::driver::PfVdisplayDriver));
    let v = m.driver_proto.load(Ordering::Relaxed);
    if v != 0 {
        return v >= 5;
    }
    let _ = m.ensure_device();
    m.driver_proto.load(Ordering::Relaxed) >= 5
}

/// The live pf-vdisplay control device, for the IDD-push capturer's sealed-channel delivery
/// (`IOCTL_SET_FRAME_CHANNEL`) — an `Arc` clone the caller (and every closure it builds) holds for
/// as long as it may issue IOCTLs: the handle stays open while any holder lives and closes when the
/// last drains, which is what lets the wake-from-sleep recovery's PnP disable proceed once the
/// manager retires it (see [`DeviceSlot`]). `None` before the first backend open — impossible for a
/// capturer, which only exists on a monitor the manager created.
pub fn control_device_handle() -> Option<Arc<OwnedHandle>> {
    VDM.get().and_then(VirtualDisplayManager::device_handle)
}

/// Retire the cached control handle from OUTSIDE the manager, for a caller that KNOWS the device
/// died — the adapter-reload recovery in [`crate::driver`], which tears the driver stack down and
/// back up. Without it the stale handle survives into the next session's `IOCTL_ADD` and is only
/// recovered by the gone-classified retry one failed IOCTL later.
///
/// Takes the `device` mutex, so it must NOT be called from inside it (notably not from
/// `VdisplayDriver::open`, which `ensure_device` invokes while holding it). No-op before any backend
/// opened the device.
pub(crate) fn invalidate_cached_device(why: &str) {
    if let Some(m) = VDM.get() {
        m.invalidate_device(&anyhow::anyhow!("{why}"));
    }
}

/// Re-commit the CURRENT display config under the manager `state` lock (the sole-topology-mutator
/// contract of [`force_mode_reenumeration`]). The secure-desktop guard's actuator: the OS only
/// reverts a path to its software-cursor default ON a mode commit, so standing the hardware-cursor
/// declare down (`IOCTL_SET_CURSOR_FORWARD` off) needs this nudge for UAC/Winlogon to actually
/// render. `false` (no-op) before the first backend open — no monitors exist to re-commit for.
pub fn force_recommit() -> bool {
    let Some(m) = VDM.get() else {
        return false;
    };
    let _guard = m.state.lock().unwrap();
    pf_win_display::win_display::force_mode_reenumeration()
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

/// The transient raw `HANDLE` view of an Arc-held control device, for the backend IOCTL surface.
/// Sound only while the `Arc` it borrows from is held — which the borrow makes structural: every
/// use site necessarily has the owning clone alive across the call, so a concurrent retire (which
/// now really closes the handle once its users drain — see [`DeviceSlot`]) can never close it
/// mid-IOCTL.
fn dev_raw(dev: &OwnedHandle) -> HANDLE {
    HANDLE(dev.as_raw_handle())
}

impl VirtualDisplayManager {
    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
    }

    /// Open + cache the control device; REOPEN when a gone-classified failure retired the cached one
    /// (driver upgrade / WUDFHost restart). The `device` mutex serializes racing opens. Returns an
    /// `Arc` clone the caller holds across every IOCTL it derives from it — a concurrent retire then
    /// drops only the manager's reference and closes nothing under the caller (see [`DeviceSlot`]).
    fn ensure_device(&self) -> Result<Arc<OwnedHandle>> {
        let mut slot = self.device.lock().unwrap();
        if let Some(d) = &slot.current {
            return Ok(d.clone());
        }
        let reap = !slot.opened_once;
        claim_instance()?;
        // `open` is a SAFE fn: it discharges every FFI precondition inside its own body (it opens the
        // handle it then IOCTLs) and returns an `OwnedHandle` that is the sole owner of the device.
        // The `device` mutex held here serializes racing opens — a *serialization* requirement, not
        // a soundness one, which is exactly why it is not expressed as `unsafe`.
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

    /// The live control device for the pinger/linger threads — an `Arc` clone the caller holds
    /// across its IOCTLs. `None` before the first acquire opened it, or between a retire and the
    /// next reopen.
    fn device_handle(&self) -> Option<Arc<OwnedHandle>> {
        self.device.lock().unwrap().current.clone()
    }

    /// Retire the cached control handle after a gone-classified IOCTL failure: drop the manager's
    /// reference, so the handle CLOSES once the last in-flight user drains (see [`DeviceSlot`]) —
    /// the release the wake-from-sleep recovery needs before it can cycle the adapter devnode. The
    /// next [`ensure_device`](Self::ensure_device) reopens the (new) device interface and re-runs
    /// the version handshake.
    fn invalidate_device(&self, why: &anyhow::Error) {
        let mut slot = self.device.lock().unwrap();
        if slot.current.take().is_some() {
            tracing::warn!(
                "virtual-display control device retired — closes when its last user drains, \
                 reopening on next use (cause: {why:#})"
            );
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
        hw_cursor: bool,
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
                let old_target = mon.target_id;
                tracing::info!(
                    slot,
                    old_target,
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
                let departed = wait_target_departed(old_target, Duration::from_millis(400));
                if !departed {
                    tracing::debug!(
                        old_target,
                        "preempted monitor still in the active CCD set after the departure ceiling"
                    );
                }
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
                let old_target = mon.target_id;
                tracing::warn!(
                    slot,
                    old_target,
                    wudf_pid = mon.wudf_pid,
                    "virtual monitor's WUDFHost is gone — preempting the dead monitor, recreating"
                );
                // SAFETY: `teardown_removed` requires a valid control handle; the `dev` Arc
                // `ensure_device()` returned above is held across this call, so the handle stays
                // open even against a concurrent retire. `mon` was just removed from the map, so it
                // is exclusively owned here — no aliasing.
                unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
                // Same async-departure settle as the reconnect preempt above (verified wait, P0.3).
                let _ = wait_target_departed(old_target, Duration::from_millis(400));
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
            // Diff against BOTH of the slot's modes ([`needs_resize`]): the negotiated one the
            // session keeps re-asking for and the one the OS actually committed. `mon.mode` alone is
            // not the discriminator — on a box that clamps the negotiated refresh the two disagree
            // for the monitor's whole life, and a request for the negotiated mode would then look
            // like a resize on every acquire (`build_pipeline_with_retry` makes exactly that request
            // once per build attempt, expecting a refcount++ join).
            let (req_mode, cur_mode) = match inner.slots.get(&slot) {
                Some(SlotState::Active { mon, .. }) => (mon.requested_mode, mon.mode),
                _ => unreachable!("just matched Active"),
            };
            if needs_resize(req_mode, cur_mode, mode) {
                // IN-PLACE mode set first (latency plan P2): an already-advertised resolution
                // (arrival list + the driver's same-id mode history) is CCD-forced on the SAME
                // monitor — no REMOVE→ADD, so the monitor's OS identity (saved per-monitor DPI),
                // the driver-side swap-chain machinery and the retained frame stash all survive,
                // and the whole hotplug cost (departure settle + activation ladder + re-isolate)
                // disappears. An out-of-list mode fails FAST (see `resize_in_place`) and falls
                // through to the proven re-arrival below.
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
                                // Same join semantics as the re-arrival: +1 ref for the new
                                // (build-then-drop overlap) lease; `gen` untouched, so the old
                                // session's lease stays valid.
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
                                // Expected-normal for a first-seen arbitrary size (the OS pins
                                // settable modes at arrival; the re-arrival teaches it) — info,
                                // not warn.
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
                        // The width changed — re-arrange the group so auto-row siblings don't
                        // overlap the resized display (no-op for a single member).
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
                        // The session keeps streaming its current mode (the control task already
                        // acked the switch; Fix 2's corrective ack tells the client the resolution
                        // did not change). Store the RECOVERED monitor, not the one handed in:
                        // that one's driver monitor was REMOVEd before the failed ADD, so its
                        // `key`/`target_id`/`gdi_name` are all dead. `gen`/`refs` are preserved,
                        // so leases stay valid either way.
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
                        // No driver monitor for this slot. Leave it EMPTY so the next acquire
                        // does a clean fresh ADD rather than joining a phantom.
                        return Err(err).context("mid-stream resize re-arrival (slot left empty)");
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
        // SAFETY: `create_monitor` requires `dev` to be a valid control handle; the `dev` Arc
        // `ensure_device()` returned above is held across this call (so the handle stays open even
        // against a concurrent retire), and we hold the `state` lock.
        let mon = match unsafe {
            self.create_monitor(dev_raw(&dev), mode, slot, client_hdr, hw_cursor, &mut inner)
        } {
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
                // SAFETY: as above — the `dev` Arc the reopening `ensure_device` just returned is
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
                                // The device itself is gone (driver upgrade / WUDFHost restart) —
                                // pings can only keep failing on this handle. Retire it so the next
                                // session's `ensure_device` reopens; the monitors are already dead
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
        // NOT `thread::spawn` (which PANICS when the OS refuses the thread), for the same reason
        // `ensure_exclusive_watch` was moved off it: this runs holding `pinger` and — via
        // `create_monitor` ← `acquire` — the manager `state` guard, so an unwind here poisons the
        // two locks the whole manager runs on, and every later `acquire`/`release`/`snapshot`
        // `.lock().unwrap()` panics for the rest of the process. A missing pinger degrades to the
        // driver's watchdog tearing the displays down (recoverable, and loud); a poisoned manager is
        // neither. It also gains the thread name its two siblings already have.
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

    /// Stop + join the device-level pinger (the LAST slot was just torn down). The join is bounded
    /// by the ping interval (watchdog/3 — seconds), same as the old per-monitor pinger join.
    fn stop_pinger(&self) {
        if let Some(p) = self.pinger.lock().unwrap().take() {
            p.stop.store(true, Ordering::Relaxed);
            let _ = p.thread.join();
        }
    }

    /// Start the exclusive-topology re-assert watchdog (idempotent). A verified
    /// [`isolate_displays_ccd`] is NOT durable: the isolated topology is deliberately not saved to
    /// the CCD database (teardown must restore the user's layout), so any later topology
    /// re-resolution — our own post-isolate resize/HDR commit, the deactivated panel's own driver,
    /// display-poller software — can bring the stored layout back. Field-proven on a hybrid
    /// Intel+NVIDIA box (`.221`): seconds after the commit-time verify reported "sole active
    /// desktop", CCD showed the internal panel ACTIVE again beside the virtual display, and the
    /// host never noticed. That silently un-does exclusive mode: cursor + windows can land
    /// off-stream, and the secure desktop / lock screen can land on the physical panel.
    ///
    /// The watchdog re-queries every [`knobs::exclusive_reassert_ms`] and, when a non-managed
    /// display crept back, evicts it via the full [`isolate_displays_ccd`] — the forced
    /// re-commit is required (it restarts OS presentation to the virtual display), and the
    /// swap-chain bounce it causes is healed by the SESSION's in-place capture re-attach, keyed
    /// off [`topology_reassert_gen`]. Each cycle takes the state lock via `try_lock` —
    /// teardown stops+joins this thread while HOLDING the state lock, so a blocking `lock()` here
    /// would deadlock; a contended cycle just skips (the slot transition that owns the lock
    /// re-isolates itself). The sleep is sliced so that stop+join is bounded by ~250 ms, not a
    /// full cycle.
    fn ensure_exclusive_watch(&'static self) {
        let interval = Duration::from_millis(knobs::exclusive_reassert_ms());
        if interval.is_zero() {
            return; // knob 0 = disabled (the pre-watchdog behavior)
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
                // Consecutive cycles that found (and evicted) a non-managed display — resets when a
                // cycle comes up clean, so an actor that re-adds the panel every few minutes logs a
                // WARN each time, while one fighting every cycle escalates once and then goes quiet.
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
                    let keep = inner.target_ids();
                    if keep.is_empty() {
                        continue;
                    }
                    let survivors = count_other_active(&keep).unwrap_or(0);
                    if survivors == 0 {
                        if fighting > 0 {
                            tracing::info!(
                                reasserts = fighting,
                                "exclusive topology stable again — no non-managed display active"
                            );
                            // Close the churn window early — descriptor-following resumes now
                            // instead of waiting out the hold's self-expiry.
                            pf_win_display::topology_churn::release();
                        }
                        fighting = 0;
                        continue;
                    }
                    fighting += 1;
                    // Announce the churn BEFORE evicting: every descriptor the capturer's poller
                    // samples from here until "stable again" (or self-expiry) is potentially the
                    // TRANSIENT eviction state — acting on it would recreate the ring at a mode
                    // the recovery chain is about to undo (the field hdr=true→false→true double
                    // recreate). Window = watchdog interval + recovery/debounce margin, refreshed
                    // every fighting round.
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
                    // That same forced re-commit hands the live IDD path a fresh swap-chain,
                    // orphaning the session's capture ring — announce it so the session rebuilds
                    // its capture attachment (same-mode ring recreate + driver re-attach + fresh
                    // encoder) instead of silently streaming a frozen frame. The two halves are
                    // a pair: eviction restarts presentation, the session re-attaches capture.
                    TOPOLOGY_REASSERT_GEN.fetch_add(1, Ordering::Relaxed);
                }
            });
        // NOT `.expect()`: this runs holding BOTH `exclusive_watch` and (via the caller) `state`, so
        // a panic here poisons the two locks the whole manager runs on — every later `acquire` and
        // every mgmt read would then panic on `.lock().unwrap()`. A thread we could not spawn means
        // no re-assert watchdog, which is a degradation; wedging the manager is not.
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

    /// Stop + join the exclusive-topology watchdog (the group's exclusive isolate is being
    /// restored). Safe under the state lock: the watchdog only ever `try_lock`s it, and its sliced
    /// sleep bounds the join by ~250 ms.
    fn stop_exclusive_watch(&self) {
        if let Some(w) = self.exclusive_watch.lock().unwrap().take() {
            w.stop.store(true, Ordering::Relaxed);
            let _ = w.thread.join();
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
    /// Call under the `state` lock: this mutates the LIVE CCD topology (force-EXTEND, explicit path
    /// activation), and the manager's sole-topology-mutator contract is what keeps two acquires from
    /// interleaving path commits. A *serialization* requirement, not a soundness one — every CCD
    /// helper it calls is a safe fn in `pf_win_display::win_display`, so this function performs no
    /// unsafe operation at all. It was an `unsafe fn` back when the FFI was inline here, and stayed
    /// one after the FFI moved out: three call sites then carried `unsafe {}` blocks whose SAFETY
    /// proofs asserted things about FFI that is no longer in the body.
    fn resolve_target_gdi(&self, target_id: u32) -> Option<String> {
        // 50 ms sampling (latency plan P0.5): the SAME 3 s per-stage ceilings — the 3-stage ladder
        // structure encodes real failure modes (headless auto-activate, integrated-panel clone,
        // lid-closed path activation) and is untouched — but a typical activation resolves on an
        // early poll, so finer sampling shaves ~150 ms off every stage crossing.
        if let Some(n) = poll_gdi_name(target_id) {
            return Some(n);
        }
        force_extend_topology();
        if let Some(n) = poll_gdi_name(target_id) {
            return Some(n);
        }
        if pf_win_display::win_display::activate_target_path(target_id) {
            if let Some(n) = poll_gdi_name(target_id) {
                return Some(n);
            }
        }
        None
    }

    /// ADD via the driver (pinning the discrete render GPU under the usual conditions), ensure the
    /// device-level watchdog pinger, resolve the GDI name, force the mode + apply the GROUP topology
    /// (first member isolates and captures the restore; a later member re-issues the isolate with
    /// the grown managed set — a sibling slot is never deactivated).
    ///
    /// The returned `Monitor.mode` is what the OS COMMITTED, which need not be `mode` — see the
    /// read-back after the settle. `Monitor.requested_mode` keeps `mode` verbatim, because that is
    /// what the session re-asks for and `acquire`'s join/resize gate has to recognise.
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
        // The slot id doubles as the driver-preferred monitor id (EDID serial / ConnectorIndex), so
        // Windows reapplies the client's saved per-monitor config (DPI scaling) on reconnect;
        // `0` (anonymous) = the driver auto-allocates the lowest-free id.
        let preferred_id = slot;
        // The client's negotiated mode, before the post-settle read-back below overwrites `mode`
        // with what the OS committed. Both end up on the `Monitor`: the session re-asks for THIS one
        // on every rebuild attempt, so it — not the committed one — is what `acquire`'s join/resize
        // gate must recognise (see `Monitor::requested_mode`).
        let requested_mode = mode;
        let render_pin = resolve_render_pin();
        // Hardware cursor only against a driver that implements the v5 channel: an older driver
        // ignores the AddRequest field anyway (composited cursor), but gating here keeps the
        // capture layer from creating + delivering a section nobody will ever publish into.
        let hw_cursor = hw_cursor && self.driver_proto.load(Ordering::Relaxed) >= 5;
        // SAFETY: `create_monitor`'s own `# Safety` contract guarantees `dev` is the live control
        // handle; we forward it unchanged to `add_monitor`, whose precondition is exactly that.
        // `render_pin` is an `Option<LUID>` by value (plain `Copy`), so no borrowed memory
        // crosses the call.
        let added = unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, preferred_id, client_hdr, hw_cursor)?
        };

        // Mandatory keepalive: ping inside the watchdog window or the driver tears all displays down.
        // ONE device-level pinger serves every slot (any IOCTL bumps the watchdog); started with the
        // first monitor, stopped when the last slot is torn down.
        self.ensure_pinger();

        // Resolve the capture target — wait for Windows to auto-activate the freshly-ADDed IDD into its
        // OWN display path, with the integrated-screen clone fallback (shared by the re-arrival path).
        // Its `state`-lock discipline is satisfied: `acquire` holds the lock across this whole call.
        let gdi_name = self.resolve_target_gdi(added.target_id);
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
                            inner.group.ccd_saved = isolate_displays_ccd_seam(&keep);
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
                            // The verified isolate above is not durable — watch and re-assert
                            // while the exclusive group lives (see `ensure_exclusive_watch`).
                            inner.group.ccd_exclusive = inner.group.ccd_saved.is_some();
                            if inner.group.ccd_exclusive {
                                self.ensure_exclusive_watch();
                            }
                        } else {
                            // Grown set: re-isolate so the fresh member joins the composited set
                            // (its auto-activate may have lit nothing extra to deactivate, but the
                            // re-commit also drives COMMIT_MODES for the new path).
                            let snap = isolate_displays_ccd_seam(&keep);
                            // Normally DISCARDED — the group restores the FIRST member's snapshot.
                            // But if the first member's isolate FAILED, there is no first snapshot,
                            // and this one just deactivated the physicals with nothing able to put
                            // them back: `teardown_removed`'s restore is gated on `ccd_saved`, and
                            // the re-assert watchdog never started either. Adopt the first
                            // SUCCESSFUL isolate as the group's snapshot instead.
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
                        let already_extended =
                            count_other_active(&[added.target_id]).unwrap_or(0) > 0;
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
                        inner.group.ccd_saved = set_virtual_primary_ccd(added.target_id);
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
                // Topology settle before capture opens: verified-state wait (latency plan P0.2) —
                // poll until the target's path + active mode are committed, ceiling = the old fixed
                // 1500 ms sleep (a rejected mode / slow third-party CCD-lock holder burns the
                // ceiling and proceeds, exactly like the sleep it replaces).
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added.target_id, mode, Duration::from_millis(1500));
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
                mode = committed_mode_or(added.target_id, mode);

                // EXPERIMENTAL `pnp_disable_monitors`, second selector (ANY topology): monitors
                // that are connected but NOT part of the desktop — the standby TV/monitor the
                // deactivated-set selector above structurally misses (it never had an active path
                // to deactivate), yet whose periodic standby wake events drive the same Windows
                // reaction cascade (rationale in `windows/monitor_devnode.rs`). Runs AFTER the
                // settle so the active flags it reads are the committed ones (a display still
                // mid-activation from the primary topology's force-EXTEND must not read as
                // inactive and get disabled) — and since the verified wait above only confirms
                // OUR target (not a physical still lighting up from force-EXTEND), this opt-in
                // sweep keeps the old FULL settle as its floor before reading those flags.
                // In Extend the active physical panels are untouched by construction. First
                // member only — the sweep is group-scoped like the isolate; later members join
                // an already-swept desktop.
                if first_member && crate::policy::prefs().pnp_disable_monitors() {
                    if let Some(rest) =
                        Duration::from_millis(1500).checked_sub(settle_start.elapsed())
                    {
                        thread::sleep(rest);
                    }
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
            requested_mode,
            resolved_monitor_id: added.resolved_monitor_id,
            position: (0, 0),
            gen: self.gen.fetch_add(1, Ordering::Relaxed),
            hw_cursor,
            cursor_excluded: added.cursor_excluded,
        })
    }

    /// Mid-stream resize IN PLACE (latency plan P2): the driver refreshes the LIVE monitor's
    /// advertised target-mode list to lead with `mode` (`IOCTL_UPDATE_MODES` →
    /// `IddCxMonitorUpdateModes2`, protocol v4), the OS re-enumerates the target's settable modes
    /// (waited on, bounded), and the usual CCD/GDI force-set + verified settle (P0.2) commit it —
    /// on the SAME monitor: target id, GDI name, saved per-monitor DPI, the driver's swap-chain
    /// worker and its retained frame stash all survive (the OS reassigns the swap-chain across a
    /// mode set; the preserved-publisher/stash hand-off covers that flap — what it was built for).
    /// On success `mon.mode` is updated in place; any failure leaves `mon` untouched (still at the
    /// old mode) and the caller falls back to [`re_add`](Self::re_add).
    ///
    /// # Safety
    /// `dev` must be the live control handle; runs the CCD/GDI FFI under the `state` lock.
    unsafe fn resize_in_place(&self, dev: HANDLE, mon: &mut Monitor, mode: Mode) -> Result<()> {
        let gdi = mon
            .gdi_name
            .clone()
            .context("in-place resize needs a resolved GDI name")?;
        let t0 = Instant::now();
        // FAST PATH (driver-independent): the OS already offers this resolution — the monitor's
        // arrival list, which since the driver's mode-history union contains every size this
        // identity ever served — so a plain CCD mode set reaches it with no driver round-trip.
        let already = pf_win_display::win_display::wait_mode_advertised(&gdi, mode, Duration::ZERO);
        if !already {
            // Out-of-arrival-list mode. On-glass (build 26200) the OS re-parses our description
            // AND re-queries target modes after UpdateModes2 — our callbacks served the fresh
            // list — yet the SETTABLE set stays pruned to the arrival list: the monitor
            // source-mode set is pinned at arrival. So one bounded UPDATE_MODES attempt per
            // process (in case a future build honors the refresh), then latch it futile and fail
            // fast to the re-arrival — whose same-id history union makes THIS size settable in
            // place from then on.
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
        // Verified-state settle (P0.2): the same committed-state predicate as the create paths. A
        // mode set that did not commit within the ceiling routes to the re-arrival fallback.
        let settle_start = Instant::now();
        let settled = wait_mode_settled(mon.target_id, mode, Duration::from_millis(1500));
        if !settled {
            anyhow::bail!(
                "in-place mode set did not commit within 1.5s (advertised after {advertised_ms} ms)"
            );
        }
        // Record what actually COMMITTED, not what was asked for — see [`committed_mode_or`], which
        // the fresh-create and re-arrival paths share with this one so all three store the same
        // truth: `mon.mode` is what `/display/state` reports and what the capturer paces to.
        let landed = committed_mode_or(mon.target_id, mode);
        tracing::info!(
            advertised_ms,
            settle_ms = settle_start.elapsed().as_millis() as u64,
            mode = format!("{}x{}@{}", landed.width, landed.height, landed.refresh_hz),
            "in-place resize committed (verified-state wait)"
        );
        mon.mode = landed;
        // …and separately what was ASKED for, because that is what the session will re-request on
        // its next acquire (a build retry, a build-then-drop overlap). Dropping it here would leave
        // the slot only knowing a clamped refresh, and every such re-acquire would re-enter this
        // function — or, once `wait_mode_advertised` refuses the un-advertised rate, the re-arrival
        // hotplug below it.
        mon.requested_mode = mode;
        Ok(())
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
        // races the teardown and the ADD is rejected under churn. Verified departure wait, ceiling =
        // the old fixed 400 ms settle (latency plan P0.3); the driver's ghost-reap ADD retry remains
        // the backstop for a departure the CCD reports early.
        let depart_start = std::time::Instant::now();
        let departed = wait_target_departed(old.target_id, Duration::from_millis(400));
        tracing::info!(
            depart_ms = depart_start.elapsed().as_millis() as u64,
            verified = departed,
            "re-arrival: old monitor departure settle"
        );
        // 2. ADD a fresh monitor at the NEW mode, reusing the slot as the preferred (stable) id.
        let render_pin = resolve_render_pin();
        // SAFETY: `dev` is the live control handle; `render_pin`/`client_hdr` are owned `Copy`/`Option`
        // values passed by value — no borrow crosses the call.
        // SAFETY (both ADDs): `dev` is the live control handle; `render_pin`/`client_hdr` are owned
        // `Copy`/`Option` values passed by value — no borrow crosses the call.
        let (added, mut mode, rollback_err) = match unsafe {
            self.driver
                .add_monitor(dev, mode, render_pin, slot, client_hdr, old.hw_cursor)
        } {
            Ok(a) => (a, mode, None),
            Err(e) => {
                // The old monitor is already REMOVEd, so there is nothing to "keep". Re-ADD it at
                // the mode it had: the resize fails, but the session keeps streaming instead of
                // being handed a slot whose driver monitor does not exist.
                //
                // At its REQUESTED mode, not its committed one — those differ exactly when the OS
                // clamped the negotiated refresh, and this ADD is meant to replay the original one
                // (same advertised mode list, same identity). Re-ADDing at the clamped rate would
                // also make the clamp the slot's new negotiated mode, so the session's next acquire
                // at the rate it still believes it has would read as yet another resize.
                let e = e.context("re-arrival ADD at the new mode");
                tracing::warn!(
                    slot,
                    error = %format!("{e:#}"),
                    "re-arrival ADD failed — rolling back to the previous mode"
                );
                // SAFETY: as the ADD above — live control handle, owned `Copy`/`Option` args.
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
        // What the surviving ADD actually asked for (the new mode, or the old monitor's on a
        // rollback) — pinned before the post-settle read-back below overwrites `mode` with the
        // committed one. The session re-asks for THIS on its next acquire, so it is the join/resize
        // gate's side of the pair (see `Monitor::requested_mode`).
        let requested_mode = mode;
        self.ensure_pinger();
        // 3. Resolve the NEW target's GDI name (target_id changes across a re-arrival). Under the
        //    `state` lock, as its topology-mutator discipline requires.
        let gdi_name = self.resolve_target_gdi(added.target_id);
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
                //    the group's first-member restore snapshot. Under the `state` lock (the caller
                //    holds it and lent us `inner`), as its topology-mutator discipline requires.
                self.reisolate_after_swap(inner, added.target_id);
                // Topology settle before capture reopens: verified-state wait, ceiling = the old
                // fixed 1500 ms sleep (latency plan P0.2 — the re-arrival twin).
                let settle_start = std::time::Instant::now();
                let settled = wait_mode_settled(added.target_id, mode, Duration::from_millis(1500));
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
                mode = committed_mode_or(added.target_id, mode);
            }
            None => tracing::warn!(
                "re-arrival target {} not yet an active display path (auto-activate, EXTEND preset \
                 and explicit path activation all failed — GPU-less box?)",
                added.target_id
            ),
        }
        // 5. Rebuild the Monitor from the ADD reply, PRESERVING `gen` (lease/refcount continuity) and
        //    the group-layout `position`. A fresh `gen` would strand the old session's lease release.
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
            gen: old.gen,
            hw_cursor: old.hw_cursor,
            // Fresh from THIS reply, not `old`: the driver's per-target declare registry is the
            // ground truth (this session may itself have declared since the original ADD).
            cursor_excluded: added.cursor_excluded,
        });
        match rollback_err {
            None => ReAdd::Arrived(mon),
            Some(err) => ReAdd::RolledBack { mon, err },
        }
    }

    /// Re-isolate the composited display set after a mid-stream monitor re-arrival ([`re_add`]) put a
    /// NEW target in place of the old one — WITHOUT recapturing the group restore snapshot (the first
    /// member captured it at session start; teardown restores that, not the mid-session state). The
    /// old slot has already been removed from the map by the caller, so `inner.target_ids()` is the
    /// surviving siblings; the new target joins them.
    ///
    /// Call under the `state` lock — it commits a new CCD topology, so it must not interleave with
    /// another slot transition's commit. A *serialization* requirement, not a soundness one: every
    /// helper it reaches (`isolate_displays_ccd_seam`, `set_virtual_primary_ccd`) is a safe fn, so
    /// this body performs no unsafe operation. (`&mut MgrInner` already proves the lock is held.)
    fn reisolate_after_swap(&self, inner: &mut MgrInner, new_target: u32) {
        use crate::policy::Topology;
        match topology_action() {
            Topology::Exclusive => {
                // Grown-set semantics: isolate to the surviving siblings + the new target. The returned
                // snapshot is DISCARDED — the group keeps the first member's (design §6.1).
                let mut keep = inner.target_ids();
                keep.push(new_target);
                let _ = isolate_displays_ccd_seam(&keep);
            }
            Topology::Primary => {
                // Make the new target primary again (its predecessor held primary), preserving the
                // original restore snapshot: `set_virtual_primary_ccd` recaptures one, so save + restore
                // the group's around the call.
                let keep_saved = inner.group.ccd_saved.take();
                let _ = set_virtual_primary_ccd(new_target);
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
            // The exclusive re-assert watchdog must be gone BEFORE the restore below — it would
            // read the restored (multi-display) topology as "lost exclusivity" and re-fight it.
            // (Belt-and-braces: its cycles also gate on `ccd_exclusive`, cleared below.)
            self.stop_exclusive_watch();
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
            inner.group.ccd_exclusive = false;
            if let Some(saved) = inner.group.ccd_saved.take() {
                restore_displays_ccd(&saved);
            }
            // Drop the isolate's crash-recovery marker even when there was no snapshot to restore
            // (a failed `isolate_displays_ccd` leaves `ccd_saved` None, and `restore_displays_ccd`
            // — which clears it itself — then never runs). The group is gone either way, so no
            // future host start owes this desk a force-EXTEND.
            pf_win_display::win_display::isolate_journal::clear();
            // EXPERIMENTAL `ddc_power_off` wake. OUTSIDE the `ccd_saved` gate, for the same reason
            // `pnp_disabled` is above it: the panels were commanded dark BEFORE the isolate, and
            // the isolate can return `None` (its `query_active_config` failed). Nested inside that
            // arm, the wake then never ran — and because the link stays live, there is no returning
            // signal to trigger a DPMS wake either, so the panels stayed dark for the rest of the
            // host's life. The brief settle wait lets re-activated paths show up in
            // EnumDisplayMonitors (no HMONITOR, no DDC channel); teardown is already seconds-scale
            // and watched by the 10 s wedge logger above.
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
        } else {
            match shrink_action(inner.group.ccd_exclusive, inner.group.ccd_saved.is_some()) {
                // Re-issue the isolate over the shrunk set (defensive — the departing monitor's
                // path dies with the REMOVE below anyway, but a re-commit keeps the surviving set
                // authoritative if the OS re-lit anything). The returned snapshot is discarded;
                // the group keeps the first member's.
                ShrinkAction::Reisolate => {
                    let keep = inner.target_ids();
                    let _ = isolate_displays_ccd_seam(&keep);
                }
                // Re-promote a survivor rather than leaving the desktop's primary on a target that
                // is about to be REMOVEd. Same save/restore-the-snapshot dance as
                // `reisolate_after_swap`, and for the same reason: `set_virtual_primary_ccd`
                // recaptures one and the group must keep the FIRST member's.
                ShrinkAction::RepromotePrimary => {
                    if let Some(&survivor) = inner.target_ids().first() {
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
        // Re-arrange whatever is LEFT. `apply_group_layout` had only ever been called from
        // `acquire` (fresh create, re-arrival, in-place resize), so a member leaving never
        // triggered it: the survivors kept the CCD origins computed when the departing monitor was
        // still between them, leaving a dead gap in the desktop that nothing closed until the next
        // acquire. Here rather than at the six call sites because this is where the slot map has
        // already shrunk, and after the REMOVE so the departing path is genuinely gone. It no-ops
        // for a group of fewer than two.
        self.apply_group_layout(inner);
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
                        // the `dev` Arc from `device_handle()` (the `Some` checked above) is held
                        // across this call, so the handle stays open. `mon` was moved out of the
                        // map under the `state` lock, so it is exclusively owned here — no aliasing.
                        unsafe { self.teardown_removed(dev_raw(&dev), &mut inner, mon) };
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
                            // SAFETY: `teardown_removed` requires a valid control handle; the `dev`
                            // Arc from `self.device_handle()` is held across this call, so the
                            // handle stays open (a concurrent retire drops only the manager's
                            // reference; see `DeviceSlot`). `mon` was moved out of the map under
                            // the lock, so it is exclusively owned here.
                            unsafe { self.teardown_removed(dev_raw(&dev), &mut g, mon) };
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

    /// The gate a non-last-member teardown keys off. It used to be `ccd_saved.is_some()`, which is
    /// true for BOTH topologies — so a `Primary` group shrinking ran the exclusive isolate and
    /// blanked the physical displays that `Primary` exists to keep lit.
    #[test]
    fn a_primary_group_shrink_repromotes_instead_of_isolating() {
        // Primary: a snapshot exists (from `set_virtual_primary_ccd`) but exclusivity was never
        // asserted. This is the case the old gate got wrong.
        assert_eq!(
            shrink_action(false, true),
            ShrinkAction::RepromotePrimary,
            "a Primary group must never run the exclusive isolate on a shrink"
        );
        // Exclusive: re-isolate over the shrunk managed set, as before.
        assert_eq!(shrink_action(true, true), ShrinkAction::Reisolate);
        // Extend/Auto captured nothing — leave the operator's topology alone.
        assert_eq!(shrink_action(false, false), ShrinkAction::Nothing);
    }

    /// Exclusivity is the discriminator on its own: a group that asserted it still re-isolates even
    /// if the snapshot went missing, because the physicals are deactivated either way.
    #[test]
    fn exclusivity_decides_without_a_snapshot() {
        assert_eq!(shrink_action(true, false), ShrinkAction::Reisolate);
    }

    /// The join/resize gate on a box that CLAMPED the negotiated refresh (the driver would not
    /// advertise 240 Hz at that pixel rate, so the OS committed 120). The session still re-requests
    /// its negotiated mode on every build attempt — that must JOIN. Keying the gate on the committed
    /// mode alone (which is what recording the read-back into `Monitor.mode` without keeping the
    /// request amounts to) makes each of those a resize, i.e. an in-place attempt that fails on an
    /// un-advertised rate and then a REMOVE→ADD hotplug, once per attempt, forever.
    #[test]
    fn a_reacquire_at_the_negotiated_mode_joins_even_when_the_os_clamped_the_refresh() {
        let requested = m(5120, 1440, 240);
        let committed = m(5120, 1440, 120);
        assert!(
            !needs_resize(requested, committed, requested),
            "re-asking for the negotiated mode must JOIN, not hotplug the monitor"
        );
        // The other side of the pair: a client that re-asks for the rate the display actually runs
        // has nothing to change either.
        assert!(!needs_resize(requested, committed, committed));
        // A genuinely new mode is still a resize — that is the branch's whole reason to exist.
        assert!(needs_resize(requested, committed, m(3840, 2160, 120)));
        assert!(needs_resize(requested, committed, m(5120, 1440, 60)));
    }

    /// The ordinary box (the OS advertises and commits exactly what was asked): both fields agree,
    /// so the gate behaves exactly as the single-field one did.
    #[test]
    fn without_a_clamp_the_gate_is_plain_mode_equality() {
        let mode = m(1920, 1080, 60);
        assert!(!needs_resize(mode, mode, mode));
        assert!(needs_resize(mode, mode, m(2560, 1440, 60)));
    }
}
