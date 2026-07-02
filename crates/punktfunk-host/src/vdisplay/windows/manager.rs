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

use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Foundation::{HANDLE, LUID};

use super::{Mode, VirtualOutput};
use crate::win_display::{
    force_extend_topology, isolate_displays_ccd, resolve_gdi_name, restore_displays_ccd,
    set_active_mode, SavedConfig,
};

/// The per-backend REMOVE key the driver stamps on ADD and consumes on REMOVE. SudoVDA keys monitors by
/// a fresh `GUID`; pf-vdisplay keys them by a monotonic `u64` session id.
#[derive(Clone, Copy)]
pub(crate) enum MonitorKey {
    Guid(windows::core::GUID),
    Session(u64),
}

/// What a backend's `add_monitor` returns: the REMOVE key + the OS target id + the render LUID.
pub(crate) struct AddedMonitor {
    pub key: MonitorKey,
    pub target_id: u32,
    pub luid: LUID,
}

/// The backend-specific IOCTL surface — the *only* thing that differs between SudoVDA and pf-vdisplay.
/// Everything else (the refcount machine, the linger, the pinger, the CCD/GDI glue) is shared in
/// [`VirtualDisplayManager`]. `Send + Sync` because the manager (and so the boxed driver) is a
/// `&'static` singleton reached from the pinger + linger threads.
pub(crate) trait VdisplayDriver: Send + Sync {
    fn name(&self) -> &'static str;
    /// Find + open the control device, validate it (version handshake), read the watchdog timeout, and
    /// reap monitors orphaned by a crashed previous host (`CLEAR_ALL`). Returns the owned handle +
    /// watchdog seconds.
    ///
    /// # Safety
    /// Issues setup-API + `DeviceIoControl` calls; runs in the caller's apartment.
    unsafe fn open(&self) -> Result<(OwnedHandle, u32)>;
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
            })
    }
}

enum MgrState {
    Idle,
    Active { mon: Monitor, refs: u32 },
    Lingering { mon: Monitor, until: Instant },
}

/// The host-lifetime virtual-display manager: the single owner of the monitor lifecycle.
pub(crate) struct VirtualDisplayManager {
    driver: Box<dyn VdisplayDriver>,
    /// Control device, opened once on first acquire. Typed + `Send+Sync`, so the pinger/linger threads
    /// share it via the `&'static` singleton with no raw-handle smuggling.
    device: OnceLock<Arc<OwnedHandle>>,
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
    /// Persistent per-client (cert-fingerprint) → stable monitor-id map. A monitor CREATE resolves the
    /// connecting client's id here, so the client keeps the same EDID serial + IddCx ConnectorIndex across
    /// reconnects and Windows reapplies its saved per-monitor config (DPI scaling). See [`super::identity`].
    identity_map: Mutex<super::identity::MonitorIdentityMap>,
}

static VDM: OnceLock<VirtualDisplayManager> = OnceLock::new();

/// Initialise the process-wide manager with `driver` (the chosen backend) and return it. Idempotent: the
/// first backend to call wins (the host runs one backend per process), so a later call ignores its driver.
pub(crate) fn init(driver: Box<dyn VdisplayDriver>) -> &'static VirtualDisplayManager {
    VDM.get_or_init(|| VirtualDisplayManager {
        driver,
        device: OnceLock::new(),
        watchdog_s: AtomicU32::new(3),
        gen: AtomicU64::new(1),
        state: Mutex::new(MgrState::Idle),
        setup_lock: Mutex::new(()),
        idd_session_stop: Mutex::new(None),
        identity_map: Mutex::new(super::identity::MonitorIdentityMap::load()),
    })
}

/// The process-wide manager. Panics if reached before a backend called [`init`] — by construction a
/// session is only ever created after `vdisplay::open` constructed the backend (which calls `init`).
pub(crate) fn vdm() -> &'static VirtualDisplayManager {
    VDM.get()
        .expect("VirtualDisplayManager used before a backend initialised it")
}

impl VirtualDisplayManager {
    pub(crate) fn backend_name(&self) -> &'static str {
        self.driver.name()
    }

    /// Open + cache the control device (once). Called under the `state` lock so two racing acquires can't
    /// double-open.
    fn ensure_device(&self) -> Result<HANDLE> {
        if let Some(d) = self.device.get() {
            return Ok(HANDLE(d.as_raw_handle()));
        }
        // SAFETY: `VdisplayDriver::open` is `unsafe` only because it issues SetupAPI + `DeviceIoControl`
        // FFI in the caller's apartment; `ensure_device` runs that on the acquiring thread under the
        // `state` lock (callers hold it), so there is no concurrent open. `open` has no handle
        // precondition to uphold, and the `OwnedHandle` it returns is the sole owner of the device.
        let (handle, watchdog_s) = unsafe { self.driver.open()? };
        self.watchdog_s.store(watchdog_s, Ordering::Relaxed);
        let raw = HANDLE(handle.as_raw_handle());
        let _ = self.device.set(Arc::new(handle));
        Ok(raw)
    }

    /// The live control handle for the pinger/linger threads (lock-free: the device never changes once
    /// opened). `None` only before the first acquire opened it.
    fn device_handle(&self) -> Option<HANDLE> {
        self.device.get().map(|d| HANDLE(d.as_raw_handle()))
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

        // IDD-push: a new connection while a monitor is LINGERING is a single-client RECONNECT (the
        // prior session fully released). A REUSED IddCx swap-chain is DEAD, so reusing it hands a black
        // screen — PREEMPT: tear the lingering monitor down (its key/topology are restored) and create a
        // fresh one. The old session's lease is gen-stamped, so its later drop is a no-op.
        //
        // ONLY Lingering, NOT Active: an Active monitor still has a lease held — that's the build-retry
        // path (`build_pipeline_with_retry` holds one lease across all attempts) or a concurrent session,
        // NOT a reconnect. Preempting Active would tear a live session down AND churn REMOVE→ADD on every
        // retry — the per-cold-start monitor churn that exhausts the IddCx slot pool and wedges ADD at
        // 0x80070490. Active falls through to the JOIN path below (refcount++, no ADD).
        if idd_push_mode() && matches!(*state, MgrState::Lingering { .. }) {
            if let MgrState::Lingering { mon, .. } = std::mem::replace(&mut *state, MgrState::Idle)
            {
                tracing::info!(
                    old_target = mon.target_id,
                    "IDD-push reconnect — preempting the lingering monitor, recreating a fresh one"
                );
                // SAFETY: `teardown` requires `dev` to be the live control handle; `dev` is the value
                // `ensure_device()` returned above (the device is cached in the `OnceLock` and never
                // closed for the manager's lifetime). `mon` was moved out of the prior `Lingering`
                // state by `mem::replace`, so it is exclusively owned here — no aliasing.
                unsafe { self.teardown(dev, mon) };
                // Let the OS finish the ASYNC monitor departure before the next ADD; a back-to-back
                // REMOVE→ADD races the teardown and the ADD IOCTL is rejected under reconnect churn.
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

        // Idle or Lingering: repurpose a lingering monitor / create a fresh one → Active{refs:1}.
        let mon = match std::mem::replace(&mut *state, MgrState::Idle) {
            MgrState::Lingering { mut mon, .. } => {
                tracing::info!(
                    backend = self.driver.name(),
                    "virtual monitor reused (reconnect within the linger window)"
                );
                if mon.mode != mode {
                    // SAFETY: `reconfigure` needs an exclusive `&mut Monitor` and only touches the live
                    // display topology. `mon` is the local monitor just moved out of the `Lingering`
                    // state (sole owner), and we hold the `state` lock — no concurrent reconfigure.
                    unsafe { self.reconfigure(&mut mon, mode) };
                }
                mon
            }
            // SAFETY: `create_monitor` requires `dev` to be the live control handle; `dev` is the
            // handle `ensure_device()` returned above (cached in the `OnceLock`, never closed for the
            // manager's lifetime), and we hold the `state` lock.
            MgrState::Idle => unsafe { self.create_monitor(dev, mode, client_fp)? },
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
        // auto-allocates the lowest-free id (the original slot-based behavior).
        let preferred_id = client_fp
            .map(|fp| self.identity_map.lock().unwrap().resolve(fp))
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
                    // SAFETY: `ping` requires `dev` to be the live control handle. `h` is from
                    // `device_handle()` (the `Some` branch) — the `OnceLock<Arc<OwnedHandle>>` that,
                    // once set, is never cleared or closed for the process lifetime, so the handle is
                    // live for this call. The pinger thread only spins while the `&'static` manager
                    // singleton (and thus the device) lives.
                    match unsafe { vdm().driver.ping(h) } {
                        Ok(()) => warned = false,
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
                // Make the virtual display the SOLE active output (default): an EXTENDED (non-primary) IDD
                // isn't DWM-composited on this box → Desktop Duplication born-losts. Deactivating the other
                // display(s) first via the atomic CCD path promotes the IDD to a composited primary with no
                // MODE_CHANGE storm. Opt out with PUNKTFUNK_NO_ISOLATE=1.
                if std::env::var("PUNKTFUNK_NO_ISOLATE").is_err() {
                    // SAFETY: `isolate_displays_ccd` is `unsafe` for its CCD topology FFI; it takes a
                    // `Copy` `u32` by value and returns an owned `SavedConfig` snapshot (no borrowed
                    // memory crosses). It runs under the `state` lock, the sole mutator of the topology.
                    ccd_saved = unsafe { isolate_displays_ccd(added.target_id) };
                } else {
                    tracing::info!("display isolation skipped (PUNKTFUNK_NO_ISOLATE) — IDD stays extended");
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
            MgrState::Active { mon, .. } | MgrState::Lingering { mon, .. } => mon.gen != gen,
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
                    let due = {
                        let g = self.state.lock().unwrap();
                        matches!(&*g, MgrState::Lingering { until, .. } if Instant::now() >= *until)
                    };
                    if !due {
                        continue;
                    }
                    let Some(dev) = self.device_handle() else {
                        continue;
                    };
                    let taken = {
                        let mut g = self.state.lock().unwrap();
                        if matches!(&*g, MgrState::Lingering { until, .. } if Instant::now() >= *until) {
                            if let MgrState::Lingering { mon, .. } =
                                std::mem::replace(&mut *g, MgrState::Idle)
                            {
                                Some(mon)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    };
                    if let Some(mon) = taken {
                        // SAFETY: `teardown` requires `dev` to be the live control handle; `dev` is from
                        // `self.device_handle()` (the `Some` checked just above), i.e. the cached
                        // `OwnedHandle` live for the process lifetime. `mon` was moved out of the
                        // `Lingering` state under the `state` lock, so it is exclusively owned here.
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

/// IDD-push mode: a new client connection preempts + recreates the monitor (single-client reconnect),
/// because a REUSED IddCx monitor's swap-chain is dead. Off → monitors are shared across sessions.
fn idd_push_mode() -> bool {
    crate::config::config().idd_push
}

/// The render-GPU pin decision (backend-neutral): pin the discrete render GPU when explicitly requested,
/// or under IDD-push (the host runs NVENC on the render adapter, so it MUST be the discrete encoder GPU
/// on a hybrid box). `None` = let the IDD use its natural adapter (Apollo parity — avoids the cross-GPU
/// ACCESS_LOST storm SudoVDA hit when pinned).
fn resolve_render_pin() -> Option<LUID> {
    // A web-console manual GPU preference pins exactly like PUNKTFUNK_RENDER_ADAPTER: the whole
    // pipeline (driver render device, capture ring, encoder) must sit on the chosen adapter.
    let manual_pref = crate::gpu::prefs().get().mode == crate::gpu::GpuMode::Manual;
    if crate::config::config().render_adapter.is_some() || manual_pref {
        crate::win_adapter::resolve_render_adapter_luid()
    } else if crate::config::config().idd_push {
        tracing::info!("IDD push: pinning the discrete render GPU (SET_RENDER_ADAPTER)");
        crate::win_adapter::resolve_render_adapter_luid()
    } else {
        tracing::info!(
            "SET_RENDER_ADAPTER skipped (Apollo-parity: no render pin; set PUNKTFUNK_RENDER_ADAPTER=<name> to force one)"
        );
        None
    }
}

/// Linger window before a session-less monitor is torn down (default 10 s; `PUNKTFUNK_MONITOR_LINGER_MS`).
fn linger_ms() -> u64 {
    std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}
