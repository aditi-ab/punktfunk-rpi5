//! Virtual monitors: the [`Monitor`] type and the control-plane verbs on it — create + arrive
//! (`IOCTL_ADD`), the in-place mode update, remove, clear, the owner reap, the cursor-channel
//! delivery and the encode session — plus the mode-struct stamping the DDIs fill from. Every
//! verb takes the calling process as `owner` and reaches only the monitors that process added.
//!
//! Ownership: [`crate::registry`] holds the only strong `Arc<Monitor>`; the drain worker holds a
//! `Weak`. Every field a worker, a DDI callback or an IOCTL can race on sits behind its own
//! mutex, held for a field swap and never across a DDI, a join, or a `Drop` that closes a
//! handle. Lock order is `REGISTRY → Monitor.*`, never reversed; workers never take the registry.
//!
//! Removal is two steps with no lock held between them: the registry hands the `Arc` back, then
//! [`Monitor::teardown`] stops the workers (cursor first, then the encode session, the drain
//! worker, the pool), and only then does the caller run `IddCxMonitorDeparture`.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use pf_driver_proto::vdisplay;
use wdk_sys::{NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding, iddcx};

use crate::cursor_cell::CursorCell;
use crate::cursor_worker::CursorChannel;
use crate::encode::section::EncodeSession;
use crate::registry::{self, lock};
use crate::swap_chain_processor::SwapChainProcessor;
use crate::worker::{OwnedHandle, Sendable, Worker};

/// The advertised mode list and its flattening (the order the mode DDIs emit). Both live in
/// [`pf_driver_proto::vdisplay`], where they run under `cargo test` on any OS; re-exported so the
/// mode callbacks keep naming them through this module.
pub use pf_driver_proto::vdisplay::{Mode, flatten};

/// The IddCx monitor handle, set once `IddCxMonitorCreate` returns.
type SendMonitor = Sendable<iddcx::IDDCX_MONITOR>;

/// [`Monitor::state`]: the IddCx lifecycle. `IddCxMonitorDeparture` is legal only on an arrived
/// monitor, so exactly one caller wins the `ARRIVED → DEPARTED` swap and departs it.
const CREATING: u8 = 0;
const ARRIVED: u8 = 1;
const DEPARTED: u8 = 2;

/// What `IddCxMonitorArrival` reported: the OS target id — the key the host addresses every
/// later delivery by — and the render-adapter LUID for the ADD reply.
#[derive(Clone, Copy)]
pub struct Arrival {
    pub target_id: u32,
    pub luid_low: u32,
    pub luid_high: i32,
}

/// The hardware-cursor state (proto v5/v6).
///
/// `data_event` is the OS cursor-data event the hardware cursor is declared against. It is owned
/// here, not by the worker thread: the declare paths copy its raw value out and call the setup
/// DDI after the guard drops, so it may close only after the worker's join — every path that
/// replaces the pair drops the worker first. `forward_on == false` is the composite render
/// mode: the client draws no pointer, so the encode pool blends the worker's shape into every
/// frame DWM excludes it from (`cell.blend`, [`CursorState::set_blend`]).
struct CursorState {
    data_event: Option<OwnedHandle>,
    worker: Option<Worker>,
    forward_on: bool,
    /// Shared with the worker (writer) and the encode pool (reader); one per monitor life.
    cell: Arc<CursorCell>,
}

impl CursorState {
    /// Blend iff the client draws nothing and a hardware cursor is declared on this adapter —
    /// the declare excludes the pointer from every frame for the WUDFHost's life, and the
    /// worker is the only shape source. Armed whenever a blend could turn on, so a clean plate
    /// exists when it does. `excluded` is [`registry::any_declared`], read before the caller
    /// took this monitor's lock.
    fn set_blend(&self, excluded: bool) {
        let armed = self.worker.is_some() && excluded;
        self.cell.set_armed(armed);
        self.cell.set_blend(!self.forward_on && armed);
    }
}

/// A live (or pending) virtual monitor.
///
/// The registry holds the only strong `Arc`; the drain worker holds a `Weak` it upgrades per
/// pass, so a monitor is never kept alive by its own worker. Identity is plain fields, the
/// handle and the arrival record are write-once, and everything else has its own mutex, held
/// for a field swap only. Whatever a swap displaces goes back to the caller, who drops it with
/// no lock held: a join or a handle close under a lock head-blocks the control plane and every
/// mode DDI the OS issues during the same topology change.
///
/// `gone` is set first thing in [`teardown`](Self::teardown): an install landing after it gets
/// its value handed back instead of parking a worker on a monitor nobody will join.
pub struct Monitor {
    /// The process whose ADD created this monitor; every later verb must come from it.
    pub owner: u32,
    /// EDID serial / connector index — the key the mode DDIs match on.
    pub id: u32,
    /// The owner's key (ADD/REMOVE); two owners may use the same value.
    pub session_id: u64,
    /// The host asked for an IddCx hardware cursor at ADD.
    pub hw_cursor: bool,
    /// When the entry was created — the watchdog reap skips a still-initializing monitor.
    pub created_at: Instant,
    object: OnceLock<SendMonitor>,
    arrival: OnceLock<Arrival>,
    /// `CREATING`, `ARRIVED` or `DEPARTED`; see [`Monitor::claim_departure`].
    state: AtomicU8,
    /// Advertised modes (requested mode first, then the proto's fallbacks).
    modes: Mutex<Vec<Mode>>,
    /// The live swap-chain drain worker; dropping it joins the thread.
    swap: Mutex<Option<SwapChainProcessor>>,
    /// The source sequence: advanced only by frames the drain worker hands the pool, and
    /// monotonic across encode sessions — shared with every pool this monitor gets.
    pub source_seq: Arc<AtomicU64>,
    cursor: Mutex<CursorState>,
    /// The live encode session (`SET_ENCODE`); its thread stops with no lock held.
    encode: Mutex<Option<Arc<EncodeSession>>>,
    /// The encode pool, kept across sessions for its retained slot; dropped at teardown.
    pool: Mutex<Option<Arc<crate::encode::pool::Pool>>>,
    /// The newest surface no pool took, which the next pool opens on.
    seed: Arc<crate::encode::pool::Seed>,
    /// Bumped (Release) by every session install or removal, and by every pool change. The
    /// drain loop compares it with its last-seen value and re-reads the slots only then.
    pub encode_gen: AtomicU32,
    /// The render LUID of the last swap-chain assignment, packed; `0` = none yet. The pool
    /// and the encoder open on this device, the one the drain worker acquires from.
    render_luid: std::sync::atomic::AtomicI64,
    gone: AtomicBool,
    /// DDI calls in flight against `object` or the cursor event, held by [`DdiGuard`].
    /// [`teardown`](Self::teardown) waits for zero before closing the event, and the
    /// departure that follows finds no caller still inside IddCx with the handle.
    ddi_inflight: AtomicU32,
}

/// One DDI call against a monitor's handle or cursor event; see [`Monitor::ddi`].
pub struct DdiGuard<'a>(&'a Monitor);

impl Drop for DdiGuard<'_> {
    fn drop(&mut self) {
        self.0.ddi_inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How long [`Monitor::teardown`] waits for in-flight DDI calls. A DDI can re-enter the mode
/// callbacks, which take the registry lock the caller has already released, so it completes.
const DDI_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Take a slot's value with its guard already released, so the caller drops it lock-free.
fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
    lock(slot).take()
}

impl Monitor {
    /// A registered-but-not-created entry; [`create_monitor`] fills the handle and arrival in.
    pub(crate) fn pending(
        owner: u32,
        id: u32,
        session_id: u64,
        hw_cursor: bool,
        modes: Vec<Mode>,
    ) -> Self {
        Self {
            owner,
            id,
            session_id,
            hw_cursor,
            created_at: Instant::now(),
            object: OnceLock::new(),
            arrival: OnceLock::new(),
            state: AtomicU8::new(CREATING),
            modes: Mutex::new(modes),
            swap: Mutex::new(None),
            source_seq: Arc::new(AtomicU64::new(0)),
            cursor: Mutex::new(CursorState {
                data_event: None,
                worker: None,
                forward_on: true,
                cell: Arc::default(),
            }),
            encode: Mutex::new(None),
            pool: Mutex::new(None),
            seed: Arc::new(crate::encode::pool::Seed::new()),
            encode_gen: AtomicU32::new(0),
            render_luid: std::sync::atomic::AtomicI64::new(0),
            gone: AtomicBool::new(false),
            ddi_inflight: AtomicU32::new(0),
        }
    }

    /// Hold while calling a DDI against this monitor's handle or its cursor event. `None`
    /// once [`teardown`](Self::teardown) has started: the increment lands before the `gone`
    /// read, so a teardown that saw zero cannot be passed by a caller that saw `!gone`.
    pub fn ddi(&self) -> Option<DdiGuard<'_>> {
        self.ddi_inflight.fetch_add(1, Ordering::AcqRel);
        if self.gone.load(Ordering::Acquire) {
            self.ddi_inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(DdiGuard(self))
    }

    /// Whether [`teardown`](Self::teardown) has run; a waiter stops rather than block on a
    /// monitor that is going away.
    pub fn gone(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }

    /// Record the render adapter a swap-chain was just assigned on.
    pub fn set_render_luid(&self, luid: windows::Win32::Foundation::LUID) {
        let packed = (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart);
        self.render_luid.store(packed, Ordering::Release);
    }

    /// The render adapter of the last assignment; `None` before the first.
    pub fn render_luid(&self) -> Option<windows::Win32::Foundation::LUID> {
        let packed = self.render_luid.load(Ordering::Acquire);
        (packed != 0).then_some(windows::Win32::Foundation::LUID {
            LowPart: packed as u32,
            HighPart: (packed >> 32) as i32,
        })
    }

    /// The cursor the encode pool blends ([`CursorCell`]); the same `Arc` for the monitor's life.
    pub fn cursor_cell(&self) -> Arc<CursorCell> {
        lock(&self.cursor).cell.clone()
    }

    /// The next publish-token generation: one per `SET_ENCODE`, never 0, and unique for the
    /// driver's life. Per monitor is not enough — a re-arrival keeps the target id and starts a
    /// fresh monitor at zero, so the replacement session took the same token as the one it
    /// replaced and a stale close matched it exactly.
    pub fn next_encode_generation(&self) -> u32 {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Install an encode session and wake the drain worker so an idle display picks it up
    /// now. Returns the session it displaced — `Err(session)` once the monitor is torn down —
    /// for the caller to stop with no lock held.
    pub fn set_encode(
        &self,
        session: Arc<EncodeSession>,
    ) -> Result<Option<Arc<EncodeSession>>, Arc<EncodeSession>> {
        let displaced = {
            let mut slot = lock(&self.encode);
            if self.gone.load(Ordering::Acquire) {
                return Err(session);
            }
            slot.replace(session)
        };
        self.bump_encode_gen();
        Ok(displaced)
    }

    /// The live session, if any.
    pub fn encode(&self) -> Option<Arc<EncodeSession>> {
        lock(&self.encode).clone()
    }

    /// Remove the live session, for the caller to stop with no lock held.
    pub fn take_encode(&self) -> Option<Arc<EncodeSession>> {
        let session = take(&self.encode);
        if session.is_some() {
            self.bump_encode_gen();
        }
        session
    }

    /// Remove the live session only if it IS `generation`, matched under the lock. A proxy
    /// closing the session it owns must never take the one that replaced it: a mid-stream resize
    /// installs the replacement first, so an unconditional take leaves the monitor encoding
    /// nothing while the drain worker keeps filling its pool.
    pub fn take_encode_if(&self, generation: u32) -> Option<Arc<EncodeSession>> {
        let session = {
            let mut slot = lock(&self.encode);
            match slot.as_ref() {
                Some(live) if live.generation == generation => slot.take(),
                _ => None,
            }
        };
        if session.is_some() {
            self.bump_encode_gen();
        }
        session
    }

    /// The encode pool, if one was ever built.
    pub fn pool(&self) -> Option<Arc<crate::encode::pool::Pool>> {
        lock(&self.pool).clone()
    }

    /// The surface the next pool opens on ([`crate::encode::pool::Seed`]).
    pub fn seed(&self) -> Arc<crate::encode::pool::Seed> {
        self.seed.clone()
    }

    /// Install a freshly built pool (the encode thread, once its session's kind is known) and
    /// wake the drain worker. The replaced pool's textures drop after the guard.
    pub fn set_pool(&self, pool: Arc<crate::encode::pool::Pool>) {
        let replaced = lock(&self.pool).replace(pool);
        self.bump_encode_gen();
        drop(replaced);
    }

    /// Bump the encode generation and wake the drain worker (`SetEvent` never blocks, so the
    /// `swap` guard may span it).
    fn bump_encode_gen(&self) {
        self.encode_gen.fetch_add(1, Ordering::Release);
        let swap = lock(&self.swap);
        if let Some(p) = &*swap {
            p.wake();
        }
    }

    /// True once the OS has assigned this monitor a swap chain — the seat bring-up waits on it.
    pub fn has_swap_chain(&self) -> bool {
        lock(&self.swap).is_some()
    }

    /// The IddCx handle — `None` until `IddCxMonitorCreate` returned. It is set before the
    /// arrival, so the mode DDIs the arrival re-enters can match on it.
    pub fn object(&self) -> Option<iddcx::IDDCX_MONITOR> {
        self.object.get().map(|o| o.0)
    }

    /// True between a successful arrival and the departure claim.
    fn arrived(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARRIVED
    }

    /// Mark the monitor departed. `true` only when it had arrived: the caller, alone, then runs
    /// `IddCxMonitorDeparture`. A monitor still creating is left to its creator, which sees the
    /// mark after the arrival and departs it itself.
    fn claim_departure(&self) -> bool {
        self.state.swap(DEPARTED, Ordering::AcqRel) == ARRIVED
    }

    /// The OS target id — 0 until arrival, a value the host never sends (OS target ids are
    /// non-zero), so a pending entry matches no target-keyed lookup.
    pub fn target_id(&self) -> u32 {
        self.arrival.get().map_or(0, |a| a.target_id)
    }

    /// A clone of the advertised list, for a lock-free DDI fill.
    pub fn modes(&self) -> Vec<Mode> {
        lock(&self.modes).clone()
    }

    /// Install the drain worker for a fresh swap-chain assignment. Returns the processor it
    /// displaced — or `proc` itself once the monitor is torn down — for the caller to drop with
    /// no lock held: dropping one joins its thread.
    #[must_use]
    pub fn set_swap(&self, proc: SwapChainProcessor) -> Option<SwapChainProcessor> {
        let mut slot = lock(&self.swap);
        if self.gone.load(Ordering::Acquire) {
            return Some(proc);
        }
        slot.replace(proc)
    }

    /// Take the drain worker out (unassign / reassign); the caller drops it with no lock held.
    #[must_use]
    pub fn take_swap(&self) -> Option<SwapChainProcessor> {
        take(&self.swap)
    }

    /// Install a fresh cursor worker and the event it waits on. Returns what they displaced —
    /// or the pair itself once the monitor is torn down — for the caller to drop with no lock
    /// held, worker first.
    fn set_cursor(
        &self,
        worker: Worker,
        data_event: OwnedHandle,
    ) -> (Option<Worker>, Option<OwnedHandle>) {
        let mut c = lock(&self.cursor);
        if self.gone.load(Ordering::Acquire) {
            return (Some(worker), Some(data_event));
        }
        (c.worker.replace(worker), c.data_event.replace(data_event))
    }

    /// Re-declare the hardware cursor after a swap-chain assign: a mode commit reverts the OS
    /// to a software cursor, and without this `IddCxMonitorQueryHardwareCursor` fails
    /// STATUS_NOT_SUPPORTED for good. No-op without a live worker, and in the composite render
    /// mode on an adapter that never declared, whose software cursor is the point; once any
    /// target declared, the pointer is excluded for good and the worker's shape is the only
    /// one the pool can blend. The event value is copied out under the guard and the DDI runs
    /// after it: the DDI can re-enter the mode callbacks, and the event stays open because the
    /// [`ddi`](Self::ddi) guard holds teardown off until the call returns.
    pub fn resetup_cursor(&self) {
        let Some(object) = self.object() else {
            return;
        };
        let Some(_ddi) = self.ddi() else {
            return;
        };
        let excluded = registry::any_declared();
        let data_event = {
            let c = lock(&self.cursor);
            c.data_event
                .as_ref()
                .filter(|_| (c.forward_on || excluded) && c.worker.is_some())
                .map(|h| h.as_raw().0 as isize)
        };
        if let Some(ev) = data_event {
            let st = crate::cursor_worker::setup_hardware_cursor(object, ev);
            dbglog!("[pf-vd] cursor: re-setup on swap-chain assign -> {st:#x}");
            if wdk_iddcx::nt_success(st) {
                registry::mark_declared(self.target_id());
            }
        }
    }

    /// Stop everything whose drop blocks, each taken under its own guard and dropped after it:
    /// the cursor worker (it can be inside a hardware-cursor query against the handle the
    /// caller departs next), then the event it waited on, then the encode session (its thread
    /// stops within a bound or is detached), the drain worker and the pool. `gone` is set
    /// first, so a racing install hands its value back instead of landing here unjoined. Run
    /// by the caller that removed this monitor, with the registry lock released.
    pub fn teardown(&self) {
        self.gone.store(true, Ordering::Release);
        let started = Instant::now();
        // Every DDI caller took its guard before reading `gone`; wait the live ones out before
        // closing the event they were handed and before the handle departs.
        while self.ddi_inflight.load(Ordering::Acquire) != 0 {
            if started.elapsed() > DDI_DRAIN_BUDGET {
                dbglog!(
                    "[pf-vd] monitor teardown: {} DDI call(s) still in flight after {} ms",
                    self.ddi_inflight.load(Ordering::Acquire),
                    DDI_DRAIN_BUDGET.as_millis()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let (worker, event) = {
            let mut c = lock(&self.cursor);
            (c.worker.take(), c.data_event.take())
        };
        drop(worker);
        drop(event);
        if let Some(session) = take(&self.encode) {
            session.stop();
            drop(session);
        }
        drop(take(&self.swap));
        drop(take(&self.pool));
        let took = started.elapsed();
        if took > Duration::from_millis(250) {
            dbglog!("[pf-vd] monitor teardown took {} ms", took.as_millis());
        }
    }
}

/// Tear every removed monitor down, then depart the ones this call claims
/// ([`Monitor::claim_departure`]): a worker can be inside a DDI against the handle departure
/// destroys, so the joins come first. A monitor still creating is marked, and its creator
/// departs it.
fn depart(removed: Vec<Arc<Monitor>>) {
    for m in &removed {
        m.teardown();
    }
    for m in removed {
        if m.claim_departure()
            && let Some(object) = m.object()
        {
            // SAFETY: `object` arrived, and this call won its one departure claim.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(object) };
        }
    }
}

/// Depart every monitor of `owner` that has existed at least `grace` — the owner-gone reap
/// ([`crate::watchdog`]). The grace skips a just-created monitor (the host adds it, then starts
/// pinging) so a momentarily stale ping sample cannot reap a brand-new one. Returns the count.
pub fn reap_owner(owner: u32, grace: Duration) -> usize {
    let removed = registry::remove(|m| m.owner == owner && m.created_at.elapsed() >= grace);
    let n = removed.len();
    depart(removed);
    n
}

/// Stamp [`vdisplay::signal_info`]'s numbers into the OS `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` —
/// one description for both mode DDI families, differing only in `v_sync_freq_divider` (0 for
/// monitor modes, 1 for target modes, per the DDI contract). Both sync rationals have
/// denominator 1, and total size equals active size: no fabricated blanking.
fn signal_info(
    width: u32,
    height: u32,
    refresh_rate: u32,
    v_sync_freq_divider: u32,
) -> wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    let n = vdisplay::signal_info(width, height, refresh_rate, v_sync_freq_divider);
    let region = wdk_sys::DISPLAYCONFIG_2DREGION {
        cx: width,
        cy: height,
    };
    let mut si = wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
        pixelRate: n.pixel_rate,
        hSyncFreq: wdk_sys::DISPLAYCONFIG_RATIONAL {
            Numerator: n.h_sync_num,
            Denominator: 1,
        },
        vSyncFreq: wdk_sys::DISPLAYCONFIG_RATIONAL {
            Numerator: n.v_sync_num,
            Denominator: 1,
        },
        totalSize: region,
        activeSize: region,
        scanLineOrdering:
            wdk_sys::DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
        ..Default::default()
    };
    // union { AdditionalSignalInfo bitfield | videoStandard:u32 } — the proto packs the
    // vSyncFreqDivider into bits 16..21 of the "other" video standard.
    si.__bindgen_anon_1.videoStandard = n.video_standard;
    si
}

/// `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` for a monitor mode (vSyncFreqDivider = 0, per the DDI contract).
pub fn display_info(
    width: u32,
    height: u32,
    refresh_rate: u32,
) -> wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    signal_info(width, height, refresh_rate, 0)
}

/// `IDDCX_TARGET_MODE` for a scan-out mode (vSyncFreqDivider = 1, per the DDI contract).
pub fn target_mode(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_TARGET_MODE {
    iddcx::IDDCX_TARGET_MODE {
        Size: core::mem::size_of::<iddcx::IDDCX_TARGET_MODE>() as u32,
        TargetVideoSignalInfo: wdk_sys::DISPLAYCONFIG_TARGET_MODE {
            targetVideoSignalInfo: signal_info(width, height, refresh_rate, 1),
        },
        ..Default::default()
    }
}

/// Wire bit-depth advertised per mode in the `*2` (HDR) mode DDIs. STEP 7: advertise BOTH 8 and 10 bpc
/// RGB (so the OS offers HDR10 modes), no YCbCr. The wdk-sys bindgen enum is `ModuleConsts`, so each
/// `IDDCX_BITS_PER_COMPONENT_*` is a plain-int const and the `IDDCX_WIRE_BITS_PER_COMPONENT` fields are
/// plain ints — OR the constants directly (NO newtype `.0` like the oracle's wdf-umdf-sys binding). Field
/// names (Rgb/YCbCr444/YCbCr422/YCbCr420, IDDCX_BITS_PER_COMPONENT_8/_10/_NONE) are the verbatim C header
/// names, identical across both bindings.
pub fn wire_bits() -> iddcx::IDDCX_WIRE_BITS_PER_COMPONENT {
    let rgb = iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_8
        | iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_10;
    iddcx::IDDCX_WIRE_BITS_PER_COMPONENT {
        Rgb: rgb,
        YCbCr444: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr422: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr420: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
    }
}

/// `IDDCX_TARGET_MODE2` for a scan-out mode (HDR `*2` path): builds the v1 [`target_mode`] and copies its
/// `TargetVideoSignalInfo`, then stamps the `*2` Size + per-mode wire bit-depth ([`wire_bits`]). Rest
/// zeroed.
pub fn target_mode2(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_TARGET_MODE2 {
    let m1 = target_mode(width, height, refresh_rate);
    iddcx::IDDCX_TARGET_MODE2 {
        Size: core::mem::size_of::<iddcx::IDDCX_TARGET_MODE2>() as u32,
        TargetVideoSignalInfo: m1.TargetVideoSignalInfo,
        BitsPerComponent: wire_bits(),
        ..Default::default()
    }
}

/// Adopt a hardware-cursor channel delivery (`IOCTL_SET_CURSOR_CHANNEL`, proto v5): create the
/// cursor-data event, declare the hardware cursor to the OS, start the worker. `Err(ch)` when
/// `owner` has no arrived monitor with `target_id`, the event could not be made, or the section
/// failed to map or validate. A re-delivery replaces both — the host only re-sends after
/// recreating the section. A monitor added without `hw_cursor` gets one only because the
/// adapter already excludes the pointer: its client draws nothing, so the channel exists for
/// the pool's blend alone.
///
/// The monitor keeps one data event across deliveries: a re-declare after a swap-chain assign
/// copies its value out and must never find it closed. A replaced worker is joined before the
/// new one waits on it, and both the setup DDI and every join run with no lock held: the DDI can
/// re-enter the mode callbacks, and a join under a lock would head-block the control plane. A
/// delivery that fails leaves no worker, so the pool stops blending its last image.
pub fn set_cursor_channel(
    owner: u32,
    target_id: u32,
    ch: CursorChannel,
) -> Result<(), CursorChannel> {
    if target_id == 0 {
        return Err(ch);
    }
    let Some(m) = registry::find(|m| m.owner == owner && m.target_id() == target_id) else {
        return Err(ch);
    };
    let Some(object) = m.object() else {
        return Err(ch);
    };
    let Some(_ddi) = m.ddi() else {
        return Err(ch);
    };
    let excluded = registry::any_declared();
    let (declare, old_worker, old_event, cell) = {
        let mut c = lock(&m.cursor);
        if !m.hw_cursor {
            c.forward_on = false;
        }
        (
            c.forward_on || excluded,
            c.worker.take(),
            c.data_event.take(),
            c.cell.clone(),
        )
    };
    drop(old_worker); // join a replaced worker BEFORE the new one waits on its event
    // Auto-reset: the OS signals it once per cursor update.
    let Some(data_event) = old_event.or_else(|| OwnedHandle::event(false)) else {
        dbglog!("[pf-vd] cursor: data event creation failed — keeping composited cursor");
        lock(&m.cursor).set_blend(registry::any_declared());
        return Err(ch);
    };
    // `declare = false`: the composite render mode on an adapter that never declared — adopt
    // the channel and spawn the worker WITHOUT declaring, so DWM keeps compositing; a later
    // enable-flip declares against this event. Once anything declared, the pointer is gone
    // from every frame and the worker's shape is what the pool blends, so declare regardless.
    let spawned = crate::cursor_worker::setup_and_spawn(
        object,
        ch,
        declare,
        data_event.as_raw().0 as isize,
        cell,
    );
    let worker = match spawned {
        Ok(worker) => worker,
        Err(unadopted) => {
            // `data_event` drops here. An adopted channel is released already and the host sees
            // no publish; one that failed validation goes back for the host to reap.
            lock(&m.cursor).set_blend(registry::any_declared());
            return unadopted.map_or(Ok(()), Err);
        }
    };
    if declare {
        // The worker only spawns after `IddCxMonitorSetupHardwareCursor` succeeded.
        registry::mark_declared(target_id);
    }
    let (displaced_worker, displaced_event) = m.set_cursor(worker, data_event);
    let excluded = registry::any_declared();
    lock(&m.cursor).set_blend(excluded);
    drop(displaced_worker); // join outside every lock, then close the event it waited on
    drop(displaced_event);
    Ok(())
}

/// The mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`, proto v6): `enable` declares
/// the hardware cursor again (DWM excludes the pointer; per-mode-commit re-declares resume);
/// disable stores the flag, which stops the per-commit re-declare on an adapter that never
/// declared and turns the pool's blend on where one did — there is no un-declare DDI.
/// `false` when `owner` has no monitor with `target_id`, or another owner has one.
///
/// The flip is state, not an edge on one monitor generation: the desired value persists per
/// target in the registry (a fresh entry inherits it at arrival) and is stamped on every live
/// entry matching the target, since duplicate generations coexist during re-arrival churn. The
/// DDI runs after every guard has dropped (it can re-enter the mode callbacks), against the
/// event value copied out of the entry, which closes it only after joining its worker.
pub fn set_cursor_forward(owner: u32, target_id: u32, enable: bool) -> bool {
    // The desired state is keyed by OS target, so a target another owner holds is not this
    // owner's to flip.
    if registry::find(|m| m.owner != owner && m.target_id() == target_id).is_some() {
        return false;
    }
    registry::set_cursor_forward_desired(target_id, enable);
    let matching = registry::find_all(|m| m.owner == owner && m.target_id() == target_id);
    if matching.is_empty() {
        return false; // no monitor with this target at all
    }
    let excluded = registry::any_declared();
    // Only a present worker gets the immediate declare DDI call.
    let mut declare_on: Option<(Arc<Monitor>, Option<isize>)> = None;
    let (mut had_worker, mut blend_now) = (false, false);
    for m in &matching {
        let mut c = lock(&m.cursor);
        c.forward_on = enable;
        c.set_blend(excluded);
        had_worker |= c.worker.is_some();
        blend_now |= c.cell.blends();
        if c.worker.is_some() {
            declare_on = Some((
                Arc::clone(m),
                c.data_event.as_ref().map(|h| h.as_raw().0 as isize),
            ));
        }
    }
    let (declare_m, worker_ev) = match declare_on {
        Some((m, ev)) => (Some(m), ev),
        None => (None, None),
    };
    match (enable, declare_m, worker_ev) {
        (true, Some(m), Some(ev)) => {
            // Enable declares immediately against the live worker's event (works any time),
            // under the DDI guard so a teardown cannot close that event underneath it.
            if let (Some(object), Some(_ddi)) = (m.object(), m.ddi()) {
                let st = crate::cursor_worker::setup_hardware_cursor(object, ev);
                dbglog!("[pf-vd] cursor: forward flip enable=1 (declare) -> {st:#x}");
                if wdk_iddcx::nt_success(st) {
                    registry::mark_declared(target_id);
                }
            }
        }
        (false, _, _) => {
            // Blend needs all three. Naming them is the difference between "the flip did not
            // arrive" and "it arrived and there was nothing to blend from".
            dbglog!(
                "[pf-vd] cursor: forward flip enable=0 — blend={} (forward_on=0 worker={} \
                 declared={}); a false worker/declared means nothing composites",
                u8::from(blend_now),
                u8::from(had_worker),
                u8::from(excluded)
            );
        }
        _ => dbglog!(
            "[pf-vd] cursor: forward flip enable=1 stored (worker={} declared={}) — applies at \
             the next channel delivery",
            u8::from(had_worker),
            u8::from(excluded)
        ),
    }
    true
}

/// The seat placeholder's owner and session. Pid 0 is never a requestor, so the pair cannot
/// collide with a host's. It stays for the session's life: it holds the only display path the
/// remoting stack commits, and a monitor arriving into an empty topology gets none of its own.
pub const SEAT_PLACEHOLDER_OWNER: u32 = 0;
pub const SEAT_PLACEHOLDER_SESSION: u64 = 0;

/// `IOCTL_ADD`: create + arrive `owner`'s virtual monitor at the requested mode, named by
/// `req.preferred_monitor_id` (the host's per-client stable id; `0` = lowest free) and
/// advertising the client display's luminance volume in its EDID (all-zero = the built-in
/// defaults). Returns `(monitor_id, target_id, adapter_luid_low, adapter_luid_high)` for the
/// [`AddReply`](pf_driver_proto::control::AddReply), or `None` (no adapter yet / IddCx error).
/// The caller validates the mode.
///
/// The entry is registered pending before `IddCxMonitorCreate`, so the mode DDIs the create
/// re-enters find it by id; the handle and then the arrival are filled in write-once. A create
/// failure reclaims the id. An arrival failure must also `WdfObjectDelete` the created object:
/// departure is only valid for an arrived monitor, and a leaked object pins its slot against the
/// adapter's monitor budget. A concurrent clear or reap never departs a monitor that has not
/// arrived ([`Monitor::claim_departure`]), so that delete, and the creator's own departure after
/// a mid-create removal, each run exactly once.
pub fn create_monitor(
    owner: u32,
    req: &pf_driver_proto::control::AddRequest,
) -> Option<(u32, u32, u32, i32)> {
    let (session_id, width, height, refresh) =
        (req.session_id, req.width, req.height, req.refresh_hz);
    let preferred_id = req.preferred_monitor_id;
    let hw_cursor = req.hw_cursor != 0;
    let client_lum = pf_driver_proto::edid::ClientLuminance {
        max_nits: req.max_luminance_nits,
        max_frame_avg_nits: req.max_frame_avg_nits,
        min_millinits: req.min_luminance_millinits,
    };
    let adapter = crate::adapter::adapter()?;
    // One identity per owner and session: a re-ADD of a still-live `session_id` departs the
    // stale monitor first, so no duplicate EDID/target lingers. Another owner's same key is
    // a different monitor.
    if registry::find(|m| m.owner == owner && m.session_id == session_id).is_some() {
        dbglog!(
            "[pf-vd] create_monitor: owner {owner} session {session_id} already live — departing the stale monitor"
        );
        remove_monitor(owner, session_id);
    }
    let modes = vdisplay::advertised_modes(
        Mode {
            width,
            height,
            refresh_rates: vec![refresh],
        },
        crate::adapter::is_seat_role(),
        &[],
    );
    let monitor = registry::insert(owner, session_id, hw_cursor, preferred_id, modes);
    let id = monitor.id;

    // EDID (serial = id) describes the monitor; the OS calls back into parse_monitor_description.
    // The session's own mode becomes the preferred-timing DTD when it fits the encoding.
    let mut edid = pf_driver_proto::edid::generate(id, client_lum, Some((width, height, refresh)));
    let desc = iddcx::IDDCX_MONITOR_DESCRIPTION {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_DESCRIPTION>() as u32,
        Type: iddcx::IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
        DataSize: edid.len() as u32,
        // SAFETY: `edid` is a local array that outlives this `create_monitor` call; IddCxMonitorCreate
        // (below) reads through `pData` SYNCHRONOUSLY, before `edid` drops — the pointer never escapes.
        pData: edid.as_mut_ptr().cast(),
    };

    let mut info = iddcx::IDDCX_MONITOR_INFO {
        Size: core::mem::size_of::<iddcx::IDDCX_MONITOR_INFO>() as u32,
        MonitorContainerId: container_guid(id),
        MonitorType:
            wdk_sys::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: id,
        MonitorDescription: desc,
    };

    let mut attr = wdk_sys::WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32,
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    };

    let create_in = iddcx::IDARG_IN_MONITORCREATE {
        ObjectAttributes: &raw mut attr,
        pMonitorInfo: &raw mut info,
    };
    let mut create_out = iddcx::IDARG_OUT_MONITORCREATE::default();
    // SAFETY: adapter is a valid IddCx adapter; create_in points to valid local storage read synchronously.
    let st = unsafe { wdk_iddcx::IddCxMonitorCreate(adapter, &create_in, &mut create_out) };
    dbglog!("[pf-vd] IddCxMonitorCreate(id={id}) -> {st:#x}");
    if !wdk_iddcx::nt_success(st) {
        remove_by_id(id);
        return None;
    }
    let object = create_out.MonitorObject;
    let _ = monitor.object.set(Sendable(object));

    // Tell the OS the monitor is plugged in.
    let mut arrival_out = iddcx::IDARG_OUT_MONITORARRIVAL::default();
    // SAFETY: `object` is the just-created IddCx monitor handle. No one departs it before it
    // arrives: `claim_departure` only marks a monitor still creating.
    let st = unsafe { wdk_iddcx::IddCxMonitorArrival(object, &mut arrival_out) };
    dbglog!("[pf-vd] IddCxMonitorArrival(id={id}) -> {st:#x}");
    if !wdk_iddcx::nt_success(st) {
        dbglog!(
            "[pf-vd] IddCxMonitorArrival(id={id}) FAILED — reclaiming the id + deleting the created monitor"
        );
        monitor.state.store(DEPARTED, Ordering::Release);
        remove_by_id(id);
        // SAFETY: `object` never arrived, so no departure ran on it, and only this thread
        // deletes it; `WdfObjectDelete` takes a `WDFOBJECT` (a raw handle cast, as in the
        // swap-chain teardown).
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, object as WDFOBJECT);
        }
        return None;
    }

    // A removal that ran while the create was in flight either marked the monitor departed
    // (clear, reap, remove) or only unlinked it (device cleanup). Either way nothing else will
    // depart it, and it would stay plugged in for the device's life: undo it here.
    let arrived = monitor
        .state
        .compare_exchange(CREATING, ARRIVED, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    let linked = registry::find(|m| m.id == id).is_some_and(|m| Arc::ptr_eq(&m, &monitor));
    if !arrived || !linked {
        dbglog!("[pf-vd] create_monitor(id={id}): cleared mid-create — departing the new monitor");
        monitor.teardown();
        // A lost swap means a removal marked it and left the departure here; an unlinked one
        // is claimed like any other removal, so a remover still in `depart` skips it.
        if !arrived || monitor.claim_departure() {
            // SAFETY: `object` arrived successfully just above, and this is its one departure.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(object) };
        }
        return None;
    }

    let arrival = Arrival {
        target_id: arrival_out.OsTargetId,
        luid_low: arrival_out.OsAdapterLuid.LowPart,
        luid_high: arrival_out.OsAdapterLuid.HighPart,
    };
    // The render flip survives entry churn: start at the session's desired state, stamped
    // before the arrival makes this entry findable by target.
    lock(&monitor.cursor).forward_on = registry::cursor_forward_desired(arrival.target_id);
    let _ = monitor.arrival.set(arrival);
    Some((id, arrival.target_id, arrival.luid_low, arrival.luid_high))
}

/// `IOCTL_UPDATE_MODES` (v4): lead the live monitor's mode list with a new preferred mode and
/// push the target list to the OS via `IddCxMonitorUpdateModes2` — the in-place mid-stream
/// resize. No departure: OS identity, drain worker and encode session all survive.
///
/// The list is a union — new mode first, every previously advertised mode kept (deduped, capped
/// at [`vdisplay::MODE_LIST_CAP`]) — because the OS pins a monitor's settable set at arrival, so
/// a replacement could only lose settable modes; the union pays off at the next same-id
/// re-arrival, where the registry's mode history makes every previously used mode settable.
///
/// The stored list changes first, so an OS re-query through the mode DDIs sees the new one, and
/// is reverted if the DDI fails so the two stay coherent. The DDI runs with no lock held: it can
/// re-enter the mode-query callbacks. `STATUS_NOT_FOUND` unless `owner` holds `session_id`.
pub fn update_monitor_modes(
    owner: u32,
    session_id: u64,
    width: u32,
    height: u32,
    refresh: u32,
) -> NTSTATUS {
    let Some(m) = registry::find(|m| m.owner == owner && m.session_id == session_id) else {
        return crate::STATUS_NOT_FOUND;
    };
    let Some(object) = m.object().filter(|_| m.arrived()) else {
        return crate::STATUS_NOT_FOUND; // not arrived, or departing — nothing to update
    };
    let Some(_ddi) = m.ddi() else {
        return crate::STATUS_NOT_FOUND; // torn down under us
    };
    let (old_modes, new_modes) = {
        let mut modes = lock(&m.modes);
        let new_modes = vdisplay::advertised_modes(
            Mode {
                width,
                height,
                refresh_rates: vec![refresh],
            },
            crate::adapter::is_seat_role(),
            &modes,
        );
        (
            core::mem::replace(&mut *modes, new_modes.clone()),
            new_modes,
        )
    };

    // The OS's target-mode list for this monitor (the `*2`/HDR shape, like `monitor_query_modes2`).
    let mut targets: Vec<iddcx::IDDCX_TARGET_MODE2> = flatten(&new_modes)
        .map(|item| target_mode2(item.width, item.height, item.refresh_rate))
        .collect();
    let in_args = iddcx::IDARG_IN_UPDATEMODES2 {
        Reason: iddcx::IDDCX_UPDATE_REASON::IDDCX_UPDATE_REASON_OTHER,
        TargetModeCount: targets.len() as u32,
        pTargetModes: targets.as_mut_ptr(),
    };
    // SAFETY: `object` is a live IddCx monitor handle (arrived — checked above) and `_ddi` holds
    // its teardown and departure off until this call returns. `in_args` points at valid local
    // storage (`targets` outlives the synchronous DDI call).
    let st = unsafe { wdk_iddcx::IddCxMonitorUpdateModes2(object, &in_args) };
    dbglog!(
        "[pf-vd] IddCxMonitorUpdateModes2(session={session_id}, {width}x{height}@{refresh}) -> {st:#x}"
    );
    if !wdk_iddcx::nt_success(st) {
        // Keep the stored list coherent with what the OS actually holds (the old one).
        *lock(&m.modes) = old_modes;
        return st;
    }
    crate::STATUS_SUCCESS
}

/// `IOCTL_REMOVE`: unlink, tear down and depart `owner`'s monitor for `session_id`, recording
/// its mode list for the next same-id create. Returns true if one was removed.
pub fn remove_monitor(owner: u32, session_id: u64) -> bool {
    let Some(m) = registry::remove_session(owner, session_id) else {
        return false;
    };
    depart(vec![m]);
    true
}

/// `IOCTL_CLEAR_ALL`: tear down and depart every monitor `owner` holds. A crashed
/// predecessor's monitors are not the caller's to clear: [`crate::watchdog`] departs those when
/// the dead process's handles close, so a restarted host finds its connectors free already.
pub fn clear_all(owner: u32) {
    depart(registry::remove(|m| m.owner == owner));
}

/// `EvtCleanupCallback` (device removal, [`crate::callbacks::device_cleanup`]): empty the
/// registry and release every monitor's heavy resources — drain workers, cursor workers and
/// encode sessions — WITHOUT `IddCxMonitorDeparture`: the framework tears the IddCx monitors
/// down with the departing device, and departing here would double-tear.
pub fn cleanup_for_device_removal() {
    for m in registry::remove(|_| true) {
        m.teardown();
    }
}

/// Drop a pending entry by id (create failed before arrival). A pending entry normally holds
/// nothing, but a concurrent delivery may already have stocked it, so it is torn down anyway.
fn remove_by_id(id: u32) {
    for m in registry::remove(|m| m.id == id) {
        m.teardown();
    }
}

/// Rebuild [`vdisplay::container_guid`]'s fields as the OS `GUID` — the container id that groups a
/// monitor's targets into one physical device. `pf_driver_proto` is `no_std` and has no `GUID` type,
/// so it hands back the four fields.
fn container_guid(id: u32) -> wdk_sys::GUID {
    let (d1, d2, d3, d4) = vdisplay::container_guid(id);
    wdk_sys::GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    }
}
