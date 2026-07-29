//! Virtual-monitor model + lifecycle (STEP 4). Monitors are created on demand by the control plane
//! ([`crate::control`], `IOCTL_ADD`): each carries the requested mode (advertised as preferred) plus the
//! `session_id` the host keys it by and the OS target id + render-adapter LUID captured at arrival. Ported
//! from the working upstream virtual-display-rs (`monitor.rs` + `context.rs::create_monitor`), with
//! `guid: u128` → `session_id: u64` for the owned `pf_driver_proto` control plane.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use wdk_sys::{NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding, iddcx};

/// One resolution with the refresh rates it supports.
#[derive(Clone)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_rates: Vec<u32>,
}

/// A single (width, height, refresh) tuple — modes flattened across their refresh rates.
#[derive(Copy, Clone)]
pub struct ModeItem {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

/// Flatten a mode list into per-refresh-rate tuples (the order the mode DDIs emit).
pub fn flatten(modes: &[Mode]) -> impl Iterator<Item = ModeItem> + '_ {
    modes.iter().flat_map(|m| {
        m.refresh_rates.iter().map(|&rr| ModeItem {
            width: m.width,
            height: m.height,
            refresh_rate: rr,
        })
    })
}

/// A live (or pending) virtual monitor.
pub struct MonitorObject {
    /// The IddCx monitor handle, set once `IddCxMonitorCreate` returns (None while pending).
    pub object: Option<iddcx::IDDCX_MONITOR>,
    /// EDID serial / connector index — the key the mode DDIs match on.
    pub id: u32,
    /// Advertised modes (requested mode first, then [`default_modes`]).
    pub modes: Vec<Mode>,
    /// The host's monotonic key (ADD/REMOVE).
    pub session_id: u64,
    /// OS target id + render-adapter LUID from `IDARG_OUT_MONITORARRIVAL` (the ADD reply).
    pub target_id: u32,
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
    /// The live swap-chain drain worker, set by `assign_swap_chain` and dropped (RAII-joins the worker
    /// thread) by `unassign_swap_chain` / departure (STEP 5).
    pub swap_chain_processor: Option<crate::swap_chain_processor::SwapChainProcessor>,
    /// The host's sealed-channel delivery (`IOCTL_SET_FRAME_CHANNEL`) awaiting pickup by the swap-chain
    /// worker ([`take_frame_channel`]). Exactly one owner per delivery: replacing or dropping the entry
    /// closes an unconsumed channel's handles via [`FrameChannel`]'s `Drop`, so no delivery can leak
    /// handles in the WUDFHost table whatever the monitor's fate.
    pub frame_channel: Option<crate::frame_transport::FrameChannel>,
    /// A live [`FramePublisher`](crate::frame_transport::FramePublisher) preserved across a swap-chain
    /// unassign→reassign flap (STEP 6 sibling-join fix). The OS unassigns a monitor's swap-chain
    /// whenever a SIBLING display churns the desktop topology (a second client joining / leaving /
    /// resizing), which drops the swap-chain worker — but the HOST-owned ring (header / event /
    /// textures) the publisher holds stays valid, and the host only re-delivers the frame channel on a
    /// ring RECREATE (a descriptor change), so a fresh worker had nothing to re-attach from and the
    /// first client's stream froze (repeat frames forever). The exiting worker stashes its still-live
    /// publisher here ([`preserve_publisher`]); the next worker on the SAME render adapter takes it back
    /// ([`take_preserved_publisher`]) and resumes publishing into the same ring. Dropped with the
    /// `MonitorObject` on teardown (closing its ring handles) if no worker ever reclaims it.
    pub preserved_publisher: Option<crate::frame_transport::FramePublisher>,
    /// The worker's [`FrameStash`](crate::frame_transport::FrameStash) (the retained last composed
    /// frame — the first-frame guarantee) preserved across a swap-chain unassign→reassign flap,
    /// tagged with the render-adapter LUID its texture lives on: the next worker re-adopts it only
    /// on the SAME adapter (same pooled device — a cross-device texture would be unusable). Dropped
    /// with the `MonitorObject` on teardown (it holds only a driver-private texture, no handles).
    pub preserved_stash: Option<(crate::frame_transport::FrameStash, u32, i32)>,
    /// The host asked for an IddCx hardware cursor (`AddRequest::hw_cursor`, proto v5): declared
    /// once the cursor channel arrives (`IOCTL_SET_CURSOR_CHANNEL`) — see [`set_cursor_channel`].
    pub hw_cursor: bool,
    /// The live cursor query→publish worker (drops = stop+join) — set by [`set_cursor_channel`].
    pub cursor_worker: Option<crate::cursor_worker::CursorWorker>,
    /// The mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`, proto v6): while `false`
    /// the hardware cursor stays UN-declared (DWM composites the pointer — the capture mouse
    /// model) and [`resetup_cursor`] skips its per-mode-commit re-declare. Starts `true` — the
    /// channel delivery declares.
    pub cursor_forward_on: bool,
    /// When the entry was created — the watchdog skips still-initializing monitors.
    pub created_at: Instant,
}
// SAFETY: the raw IddCx monitor handle is framework-managed; access is serialized by MONITOR_MODES.
unsafe impl Send for MonitorObject {}

/// All live monitors. A process-`static` (not a WDFDEVICE-context-owned allocation) BY NECESSITY: the IddCx
/// monitor/mode DDIs receive only an IddCx handle — never the WDFDEVICE or its context — so this state must
/// be reachable without one (the upstream virtual-display-rs is a process-`static` for the same reason).
/// With a single `pf_vdisplay` devnode + `UmdfHostProcessSharing=ProcessSharingDisabled` the host process
/// (and this state) die WITH the device, so it is effectively device-scoped already; a `Box` + `AtomicPtr`
/// "device-owned" variant (audit §2.5) would only add a use-after-free window — the host-gone watchdog
/// thread ([`crate::control::start_watchdog`]) races device cleanup — for no real gain. Cleanup of the
/// heavy per-monitor resources on device removal is instead done explicitly ([`cleanup_for_device_removal`]).
pub static MONITOR_MODES: Mutex<Vec<MonitorObject>> = Mutex::new(Vec::new());

/// Lock [`MONITOR_MODES`], recovering the guard on poison instead of failing. DEFENSIVE ONLY: this driver
/// workspace builds with `panic = "abort"` (packaging/windows/drivers/Cargo.toml), so a panic while the
/// lock is held aborts the process WITHOUT unwinding — `MutexGuard::drop` never runs, the poison flag is
/// never set, and `.lock()` can never return `Err`. The `into_inner()` arm is therefore currently
/// unreachable; it is retained to consolidate the lock pattern and to stay correct if the panic strategy
/// ever becomes `unwind` (the guarded data is a plain `Vec` with no cross-field invariant a half-completed
/// panic could corrupt, so recovering the guard is sound). NOTE: this does NOT explain the observed ADD
/// 0x80070490 wedge — that is ghost-monitor slot-budget exhaustion (the arrival-failure `WdfObjectDelete`
/// teardown above + the host-side reap), not lock poisoning.
fn lock_monitors() -> std::sync::MutexGuard<'static, Vec<MonitorObject>> {
    MONITOR_MODES.lock().unwrap_or_else(|e| e.into_inner())
}

/// True if any virtual monitor currently exists — the host-gone watchdog only reaps when there's
/// something to reap (see [`crate::control::start_watchdog`]).
pub fn has_monitors() -> bool {
    !lock_monitors().is_empty()
}

/// Depart every monitor that has existed at least `grace` — the host-gone watchdog reap
/// ([`crate::control::start_watchdog`]). The grace skips a just-created monitor (the host adds it, then
/// starts pinging) so a momentarily-stale ping timer can't nuke a brand-new monitor. Returns the count
/// departed. Same lock discipline as [`remove_monitor`]: drop each worker (which RAII-joins its thread)
/// OUTSIDE the `MONITOR_MODES` lock, then depart.
pub fn reap_orphaned(grace: Duration) -> usize {
    let mut drained: Vec<(
        Option<iddcx::IDDCX_MONITOR>,
        Option<crate::swap_chain_processor::SwapChainProcessor>,
    )> = {
        let mut lock = lock_monitors();
        let mut taken = Vec::new();
        let mut i = 0;
        while i < lock.len() {
            if lock[i].created_at.elapsed() >= grace {
                let mut m = lock.remove(i);
                taken.push((m.object, m.swap_chain_processor.take()));
            } else {
                i += 1;
            }
        }
        taken
    };
    let n = drained.len();
    for (_, processor) in &mut drained {
        drop(processor.take());
    }
    for (object, _) in drained {
        if let Some(m) = object {
            // SAFETY: `m` is a live IddCx monitor handle; departure tears it down.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(m) };
        }
    }
    n
}

/// Append `from`'s modes to `into`, skipping resolutions already present, capped at
/// [`MODE_LIST_CAP`] — the accumulate half of the union semantics (see [`update_monitor_modes`]).
fn union_modes(into: &mut Vec<Mode>, from: &[Mode]) {
    for m in from {
        if into.len() >= MODE_LIST_CAP {
            break;
        }
        if !into
            .iter()
            .any(|e| (e.width, e.height) == (m.width, m.height))
        {
            into.push(m.clone());
        }
    }
}

/// The last advertised mode list of a DEPARTED monitor, per monitor id — consumed by the next
/// same-id [`create_monitor`] so a re-arrived monitor's ARRIVAL list already contains every mode
/// its predecessor ever served. The OS pins a monitor's settable set at arrival (see
/// [`update_monitor_modes`]), so this is what makes a windowed↔fullscreen cycle (or any return to
/// a previously-used size) an IN-PLACE mode set instead of another hotplug. In-process only (a
/// WUDFHost restart forgets it — harmless, the next resizes re-teach it); bounded: ≤ 16 ids ×
/// [`MODE_LIST_CAP`] modes.
static MODE_HISTORY: Mutex<Vec<(u32, Vec<Mode>)>> = Mutex::new(Vec::new());

/// Desired cursor-render state per OS TARGET id (`IOCTL_SET_CURSOR_FORWARD`, the mid-stream
/// flip): `false` = composite (do NOT declare the hardware cursor). Kept OUTSIDE the monitor
/// entries because those are churned freely (match-window re-arrival resizes, sibling-session
/// slot re-creates) — a fresh entry inherits this at ARRIVAL, so no generation can resurrect a
/// declare the session turned off. Absent target = default `true` (declare at delivery).
static CURSOR_FORWARD_DESIRED: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());

/// The desired cursor-forward state for `target_id` (default `true`).
fn cursor_forward_desired(target_id: u32) -> bool {
    CURSOR_FORWARD_DESIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(t, _)| *t == target_id)
        .map(|(_, on)| *on)
        .unwrap_or(true)
}

/// OS target ids on which `IddCxMonitorSetupHardwareCursor` ever SUCCEEDED. A declare is
/// IRREVOCABLE (no un-declare DDI; empty-caps re-setup is rejected; even a successful same-mode
/// re-commit never brings DWM's composited pointer back — all proven on-glass,
/// remote-desktop-sweep §8.6) and its EXCLUSION REACH IS THE WHOLE ADAPTER, not the declaring
/// target: a declare on one target leaves every LATER monitor's frames pointer-free too, even a
/// fresh never-declared target id (proven on-glass 2026-07-23: declare on 259, then a GameStream
/// session's fresh 257 streamed a cursor-less desktop). ADD replies therefore report
/// [`AddReply::cursor_excluded`](pf_driver_proto::control::AddReply) from [`any_declared`] —
/// a session that never negotiates the cursor channel must self-composite the pointer instead
/// of streaming a cursor-less desktop. Never cleared: the state's true scope is this WUDFHost's
/// life (`ProcessSharingDisabled` — the process, and with it the adapter and every sticky
/// declare, dies on adapter reset), which is exactly when this static resets too. Per-target
/// ids are kept purely for the dbglog audit trail. Bounded: ≤ 16 ids.
static DECLARED_TARGETS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Record a SUCCESSFUL hardware-cursor declare on `target_id` (see [`DECLARED_TARGETS`]).
fn mark_declared(target_id: u32) {
    let mut declared = DECLARED_TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    if !declared.contains(&target_id) {
        declared.push(target_id);
        dbglog!("[pf-vd] cursor: target {target_id} marked hardware-cursor declared (irrevocable)");
    }
}

/// True if ANY target ever had a hardware cursor declared in this WUDFHost's life — the
/// adapter-wide exclusion reach (see [`DECLARED_TARGETS`]).
pub fn any_declared() -> bool {
    !DECLARED_TARGETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
}

/// Record a departing monitor's advertised list for its id ([`MODE_HISTORY`]).
fn remember_modes(id: u32, modes: &[Mode]) {
    let mut hist = MODE_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = hist.iter_mut().find(|(i, _)| *i == id) {
        slot.1 = modes.to_vec();
    } else {
        hist.push((id, modes.to_vec()));
    }
}

/// Fallback modes appended after the requested mode, so a topology change still has options.
fn default_modes() -> Vec<Mode> {
    vec![
        Mode {
            width: 1920,
            height: 1080,
            refresh_rates: vec![60, 120],
        },
        Mode {
            width: 1280,
            height: 720,
            refresh_rates: vec![60],
        },
    ]
}

/// `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` for a monitor mode (vSyncFreqDivider = 0, per the DDI contract).
pub fn display_info(
    width: u32,
    height: u32,
    refresh_rate: u32,
) -> wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    // Compute in u64 then saturate the u32 rational numerators: the old u32 `refresh*(h+4)^2` overflows
    // for a large mode (e.g. 8K@240), which panics→aborts the extern-"C" mode DDI in a debug build.
    // Identical for every real mode; only an absurd (also now bounds-rejected) mode saturates.
    let clock_rate: u64 =
        u64::from(refresh_rate) * u64::from(height + 4) * u64::from(height + 4) + 1000;
    let clock_rate_u32 = u32::try_from(clock_rate).unwrap_or(u32::MAX);
    let mut si = pod_init!(wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO);
    si.pixelRate = clock_rate;
    si.hSyncFreq = wdk_sys::DISPLAYCONFIG_RATIONAL {
        Numerator: clock_rate_u32,
        Denominator: height + 4,
    };
    si.vSyncFreq = wdk_sys::DISPLAYCONFIG_RATIONAL {
        Numerator: clock_rate_u32,
        Denominator: (height + 4) * (height + 4),
    };
    si.activeSize = wdk_sys::DISPLAYCONFIG_2DREGION {
        cx: width,
        cy: height,
    };
    si.totalSize = wdk_sys::DISPLAYCONFIG_2DREGION {
        cx: width + 4,
        cy: height + 4,
    };
    // union { AdditionalSignalInfo bitfield | videoStandard:u32 }: videoStandard=255, vSyncFreqDivider=0.
    si.__bindgen_anon_1.videoStandard = 255;
    si.scanLineOrdering =
        wdk_sys::DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    si
}

/// `IDDCX_TARGET_MODE` for a scan-out mode (vSyncFreqDivider = 1, per the DDI contract).
pub fn target_mode(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_TARGET_MODE {
    let region = wdk_sys::DISPLAYCONFIG_2DREGION {
        cx: width,
        cy: height,
    };
    let mut si = pod_init!(wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO);
    si.pixelRate = u64::from(refresh_rate) * u64::from(width) * u64::from(height);
    si.hSyncFreq = wdk_sys::DISPLAYCONFIG_RATIONAL {
        Numerator: refresh_rate * height,
        Denominator: 1,
    };
    si.vSyncFreq = wdk_sys::DISPLAYCONFIG_RATIONAL {
        Numerator: refresh_rate,
        Denominator: 1,
    };
    si.totalSize = region;
    si.activeSize = region;
    si.scanLineOrdering =
        wdk_sys::DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    // videoStandard=255, vSyncFreqDivider=1 (bits 16..21) => 255 | (1<<16).
    si.__bindgen_anon_1.videoStandard = 255 | (1 << 16);
    let mut tm = pod_init!(iddcx::IDDCX_TARGET_MODE);
    tm.Size = core::mem::size_of::<iddcx::IDDCX_TARGET_MODE>() as u32;
    tm.TargetVideoSignalInfo = wdk_sys::DISPLAYCONFIG_TARGET_MODE {
        targetVideoSignalInfo: si,
    };
    tm
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
    let mut w = pod_init!(iddcx::IDDCX_WIRE_BITS_PER_COMPONENT);
    w.Rgb = rgb;
    w.YCbCr444 = iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE;
    w.YCbCr422 = iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE;
    w.YCbCr420 = iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE;
    w
}

/// `IDDCX_TARGET_MODE2` for a scan-out mode (HDR `*2` path): builds the v1 [`target_mode`] and copies its
/// `TargetVideoSignalInfo`, then stamps the `*2` Size + per-mode wire bit-depth ([`wire_bits`]). Rest
/// zeroed.
pub fn target_mode2(width: u32, height: u32, refresh_rate: u32) -> iddcx::IDDCX_TARGET_MODE2 {
    let m1 = target_mode(width, height, refresh_rate);
    let mut tm = pod_init!(iddcx::IDDCX_TARGET_MODE2);
    tm.Size = core::mem::size_of::<iddcx::IDDCX_TARGET_MODE2>() as u32;
    tm.TargetVideoSignalInfo = m1.TargetVideoSignalInfo;
    tm.BitsPerComponent = wire_bits();
    tm
}

/// A monitor's advertised modes (the looked-up entry returns a clone for lock-free mode-DDI fill).
pub fn modes_for_id(id: u32) -> Option<Vec<Mode>> {
    MONITOR_MODES
        .lock()
        .ok()?
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.modes.clone())
}

/// Modes for the monitor whose handle matches (used by `monitor_query_modes`).
pub fn modes_for_object(object: iddcx::IDDCX_MONITOR) -> Option<Vec<Mode>> {
    MONITOR_MODES
        .lock()
        .ok()?
        .iter()
        .find(|m| m.object == Some(object))
        .map(|m| m.modes.clone())
}

/// The OS target id stamped on the monitor whose handle matches (used by `assign_swap_chain` to key the
/// frame-channel stash for its worker). `None` if the monitor isn't found.
pub fn target_id_for_object(object: iddcx::IDDCX_MONITOR) -> Option<u32> {
    MONITOR_MODES
        .lock()
        .ok()?
        .iter()
        .find(|m| m.object == Some(object))
        .map(|m| m.target_id)
}

/// Stash a host frame-channel delivery on the monitor with `target_id` (an ARRIVED monitor — a pending
/// entry's `target_id` is still 0, which the host can never send since OS target ids are non-zero).
/// Replacing an unconsumed delivery drops it → its handles close (it WAS adopted by a prior success).
/// `Err(ch)` if no such monitor exists — the caller must NOT close those handles (the host only sees
/// the error status and reaps its remote duplicates itself; closing here too would double-close values
/// the OS may have reused).
pub fn set_frame_channel(
    target_id: u32,
    ch: crate::frame_transport::FrameChannel,
) -> Result<(), crate::frame_transport::FrameChannel> {
    if target_id == 0 {
        return Err(ch);
    }
    let mut lock = lock_monitors();
    if let Some(m) = lock.iter_mut().find(|m| m.target_id == target_id) {
        m.frame_channel = Some(ch);
        Ok(())
    } else {
        Err(ch)
    }
}

/// Take (remove) the pending frame-channel delivery for `target_id`, transferring handle ownership to
/// the caller (the swap-chain worker's attach). `None` until the host delivers one.
pub fn take_frame_channel(target_id: u32) -> Option<crate::frame_transport::FrameChannel> {
    if target_id == 0 {
        return None;
    }
    lock_monitors()
        .iter_mut()
        .find(|m| m.target_id == target_id)?
        .frame_channel
        .take()
}

/// Is a frame-channel delivery pending for `target_id`? The swap-chain worker treats a pending
/// delivery as NEWEST-WINS: it supersedes an attached publisher, because the host only re-delivers
/// after (re)creating the ring — and a retry-created ring is a DIFFERENT header mapping, whose
/// generation bump an old publisher (mapped to the previous header) can never observe.
pub fn has_frame_channel(target_id: u32) -> bool {
    target_id != 0
        && lock_monitors()
            .iter()
            .any(|m| m.target_id == target_id && m.frame_channel.is_some())
}

/// Adopt a hardware-cursor channel delivery (`IOCTL_SET_CURSOR_CHANNEL`, proto v5): declare the
/// hardware cursor to the OS and start the worker. Rejected (`Err(ch)`) when the monitor is
/// unknown, didn't ask for a cursor at ADD, or isn't created yet. A RE-delivery replaces the
/// worker (the old one stops+joins on drop) — the host only re-sends after recreating the
/// section. The IddCx setup runs OUTSIDE the monitors lock (it can call back into mode DDIs).
pub fn set_cursor_channel(
    target_id: u32,
    ch: crate::cursor_worker::CursorChannel,
) -> Result<(), crate::cursor_worker::CursorChannel> {
    if target_id == 0 {
        return Err(ch);
    }
    let (monitor_obj, declare, old_worker) = {
        let mut lock = lock_monitors();
        let Some(m) = lock
            .iter_mut()
            .find(|m| m.target_id == target_id && m.hw_cursor)
        else {
            return Err(ch);
        };
        let Some(obj) = m.object else {
            return Err(ch);
        };
        (obj, m.cursor_forward_on, m.cursor_worker.take())
    };
    drop(old_worker); // stop+join a replaced worker before the new setup
    // `declare = false`: the session is currently in the COMPOSITE render mode (the mid-stream
    // flip) — adopt the channel and spawn the worker WITHOUT declaring the hardware cursor, so
    // DWM keeps compositing; a later enable-flip declares against the worker's event.
    let Some(worker) = crate::cursor_worker::setup_and_spawn(monitor_obj, ch, declare) else {
        // setup_and_spawn consumed + cleaned the channel; report success=false via NOT_FOUND at
        // the dispatch layer is wrong here — the handles are gone, so this is a plain failure.
        return Ok(()); // adopted (handles consumed); the host detects no-publish and moves on
    };
    if declare {
        // The worker only spawns after `IddCxMonitorSetupHardwareCursor` succeeded.
        mark_declared(target_id);
    }
    let mut lock = lock_monitors();
    if let Some(m) = lock.iter_mut().find(|m| m.target_id == target_id) {
        m.cursor_worker = Some(worker);
    }
    // A monitor departed in the window: dropping `worker` here stops it cleanly.
    Ok(())
}

/// Stash a swap-chain worker's still-live [`FramePublisher`](crate::frame_transport::FramePublisher) on
/// its monitor across a swap-chain unassign→reassign flap (STEP 6 sibling-join fix; see the field docs
/// on [`MonitorObject::preserved_publisher`]). Called from the EXITING worker thread — the caller must
/// NOT hold `MONITOR_MODES` (this locks it), matching the same drop-outside-the-lock discipline the
/// processor teardown paths use. Returns `Err(publisher)` when no monitor with `target_id` exists (a
/// genuine teardown, not a flap: the entry was already removed) so the caller drops it, closing the ring
/// handles. Replacing an already-stashed publisher (should not happen — one worker exits at a time)
/// drops the old one, so it can never accumulate. Returning the publisher in the `Err` makes the
/// `Result` itself `#[must_use]`, so a caller can't silently drop the not-preserved publisher; it
/// rides boxed (the struct outgrew clippy's 128-byte `result_large_err` bar with the v2 telemetry
/// fields, and this path runs once per worker exit — the allocation is free).
pub fn preserve_publisher(
    target_id: u32,
    publisher: crate::frame_transport::FramePublisher,
) -> Result<(), Box<crate::frame_transport::FramePublisher>> {
    if target_id == 0 {
        return Err(Box::new(publisher));
    }
    let mut lock = lock_monitors();
    if let Some(m) = lock.iter_mut().find(|m| m.target_id == target_id) {
        m.preserved_publisher = Some(publisher);
        Ok(())
    } else {
        Err(Box::new(publisher))
    }
}

/// Take (remove) a preserved [`FramePublisher`](crate::frame_transport::FramePublisher) for a freshly-
/// (re)assigned swap-chain worker (STEP 6 sibling-join fix). The caller re-adopts it ONLY when the new
/// swap-chain's render adapter matches the publisher's ([`FramePublisher::render_adapter`]) — same
/// pooled device, so its context + opened ring textures are still valid; on a mismatch the caller drops
/// it and waits for a fresh channel delivery instead. `None` until a worker has stashed one.
pub fn take_preserved_publisher(target_id: u32) -> Option<crate::frame_transport::FramePublisher> {
    if target_id == 0 {
        return None;
    }
    lock_monitors()
        .iter_mut()
        .find(|m| m.target_id == target_id)?
        .preserved_publisher
        .take()
}

/// Preserve an EXITING worker's [`FrameStash`](crate::frame_transport::FrameStash) on its monitor
/// across a swap-chain unassign→reassign flap, tagged with the render-adapter LUID it lives on
/// (see [`MonitorObject::preserved_stash`]). An empty stash, or one for a monitor that no longer
/// exists (genuine teardown), is simply dropped — unlike a publisher it owns no handles, so there
/// is nothing to hand back.
pub fn preserve_stash(
    target_id: u32,
    luid_low: u32,
    luid_high: i32,
    stash: crate::frame_transport::FrameStash,
) {
    if target_id == 0 || stash.texture().is_none() {
        return;
    }
    let mut lock = lock_monitors();
    if let Some(m) = lock.iter_mut().find(|m| m.target_id == target_id) {
        m.preserved_stash = Some((stash, luid_low, luid_high));
    }
}

/// Take (remove) the preserved [`FrameStash`](crate::frame_transport::FrameStash) for a freshly-
/// (re)assigned swap-chain worker — returned only when the worker's render adapter matches the one
/// the stash was preserved on (same pooled device); a mismatched stash is dropped (its texture
/// would be cross-device).
pub fn take_preserved_stash(
    target_id: u32,
    luid_low: u32,
    luid_high: i32,
) -> Option<crate::frame_transport::FrameStash> {
    if target_id == 0 {
        return None;
    }
    let (stash, low, high) = lock_monitors()
        .iter_mut()
        .find(|m| m.target_id == target_id)?
        .preserved_stash
        .take()?;
    ((low, high) == (luid_low, luid_high)).then_some(stash)
}

/// Re-issue the hardware-cursor setup for the monitor identified by its IddCx `object`, if it
/// has a live cursor worker (the M2c cursor channel). Called from `assign_swap_chain`: a mode
/// commit reverts the OS to a software cursor, so the setup must be re-declared on the freshly
/// committed path or `IddCxMonitorQueryHardwareCursor` returns STATUS_NOT_SUPPORTED forever.
/// Reads the worker's data-event UNDER the lock (so the worker is provably alive), then calls
/// the DDI OUTSIDE it (the DDI can re-enter the mode callbacks, which take this lock).
pub fn resetup_cursor(object: iddcx::IDDCX_MONITOR) {
    let data_event = {
        let lock = lock_monitors();
        lock.iter()
            .find(|m| m.object == Some(object))
            // A composite-mode monitor (`cursor_forward_on == false`, the mid-stream flip) must
            // NOT re-declare — the commit's software-cursor default is exactly what it wants.
            .filter(|m| m.cursor_forward_on)
            .and_then(|m| {
                m.cursor_worker
                    .as_ref()
                    .map(|w| (w.data_event(), m.target_id))
            })
    };
    if let Some((ev, target_id)) = data_event {
        let st = crate::cursor_worker::setup_hardware_cursor(object, ev);
        dbglog!("[pf-vd] cursor: re-setup on swap-chain assign -> {st:#x}");
        if wdk_iddcx::nt_success(st) {
            mark_declared(target_id);
        }
    }
}

/// The mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`, proto v6): `enable` declares
/// the hardware cursor again (DWM excludes the pointer; per-mode-commit re-declares resume);
/// disable re-issues the setup with EMPTY caps — asking the OS to route the cursor back to the
/// software path (composited frames) — and stops the per-commit re-declare. `false` when no
/// monitor with `target_id` has a live cursor worker (channel never delivered / gone) — the
/// host logs and keeps its current behavior. Flag update UNDER the lock; DDI call OUTSIDE it
/// (it can re-enter the mode callbacks, which take this lock).
pub fn set_cursor_forward(target_id: u32, enable: bool) -> bool {
    // The flip is STATE, not an edge on one monitor generation: persist the desired state
    // per TARGET (fresh entries inherit it at arrival) and stamp EVERY live entry matching
    // the target — duplicate generations coexist during re-arrival churn, and stamping only
    // the first let a sibling's still-true flag re-declare on its next swap-chain assign
    // (observed on-glass: `re-setup -> 0x0` AFTER `enable=0 stored`). Only a present worker
    // gets the immediate declare DDI call.
    {
        let mut desired = CURSOR_FORWARD_DESIRED
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match desired.iter_mut().find(|(t, _)| *t == target_id) {
            Some(slot) => slot.1 = enable,
            None => desired.push((target_id, enable)),
        }
    }
    let found = {
        let mut lock = lock_monitors();
        let mut any = false;
        let mut declare_on: Option<(Option<iddcx::IDDCX_MONITOR>, Option<isize>)> = None;
        for m in lock
            .iter_mut()
            .filter(|m| m.target_id == target_id && m.hw_cursor)
        {
            any = true;
            m.cursor_forward_on = enable;
            if m.cursor_worker.is_some() {
                declare_on = Some((m.object, m.cursor_worker.as_ref().map(|w| w.data_event())));
            }
        }
        any.then_some(declare_on.unwrap_or((None, None)))
    };
    let Some((object, worker_ev)) = found else {
        return false; // no hw-cursor monitor with this target at all
    };
    match (enable, object, worker_ev) {
        (true, Some(object), Some(ev)) => {
            // Enable declares immediately against the live worker's event (works any time).
            let st = crate::cursor_worker::setup_hardware_cursor(object, ev);
            dbglog!("[pf-vd] cursor: forward flip enable=1 (declare) -> {st:#x}");
            if wdk_iddcx::nt_success(st) {
                mark_declared(target_id);
            }
        }
        (false, _, _) => {
            // There is NO un-declare DDI: re-issuing the setup with empty caps is rejected
            // INVALID_PARAMETER (observed on-glass, 26100). The stored flag alone steers — the
            // HOST forces a same-mode re-commit, whose software-cursor default then STICKS
            // because [`resetup_cursor`] skips this monitor while the flag is off.
            dbglog!(
                "[pf-vd] cursor: forward flip enable=0 stored — awaiting the host's mode \
                 re-commit (software cursor from then on)"
            );
        }
        _ => dbglog!(
            "[pf-vd] cursor: forward flip enable=1 stored (no live worker — applies at the \
             next channel delivery)"
        ),
    }
    true
}

/// Install a swap-chain processor on the monitor whose handle matches, returning any PREVIOUS processor
/// for the caller to drop OUTSIDE the lock. Dropping a processor RAII-joins its worker thread, so it must
/// never happen while holding `MONITOR_MODES` (the worker would block the whole control plane / risk a
/// self-deadlock). `None` returned if the monitor isn't found (the caller should drop `proc` itself).
#[must_use]
pub fn set_swap_chain_processor(
    object: iddcx::IDDCX_MONITOR,
    proc: crate::swap_chain_processor::SwapChainProcessor,
) -> Option<crate::swap_chain_processor::SwapChainProcessor> {
    let mut lock = lock_monitors();
    if let Some(m) = lock.iter_mut().find(|m| m.object == Some(object)) {
        m.swap_chain_processor.replace(proc)
    } else {
        // No such monitor — hand `proc` back so the caller drops it (joins the worker) outside the lock.
        Some(proc)
    }
}

/// Take (remove) the swap-chain processor from the monitor whose handle matches, returning it for the
/// caller to drop OUTSIDE the lock (see `set_swap_chain_processor`). `None` if none was installed.
#[must_use]
pub fn take_swap_chain_processor(
    object: iddcx::IDDCX_MONITOR,
) -> Option<crate::swap_chain_processor::SwapChainProcessor> {
    MONITOR_MODES
        .lock()
        .ok()?
        .iter_mut()
        .find(|m| m.object == Some(object))?
        .swap_chain_processor
        .take()
}

/// `IOCTL_ADD`: create + arrive a virtual monitor at `width`x`height`@`refresh` for `session_id`, naming it
/// by `preferred_id` (the host's per-client stable id; `0` = auto-allocate) and advertising the
/// CLIENT display's luminance volume in its EDID's CTA HDR block (`client_lum`; all-zero = the
/// built-in defaults). Returns the resolved `(monitor_id, target_id, adapter_luid_low,
/// adapter_luid_high)` for the [`AddReply`](pf_driver_proto::control::AddReply), or `None` on
/// failure (no adapter yet / IddCx error).
pub fn create_monitor(
    session_id: u64,
    width: u32,
    height: u32,
    refresh: u32,
    preferred_id: u32,
    client_lum: crate::edid::ClientLuminance,
    hw_cursor: bool,
) -> Option<(u32, u32, u32, i32)> {
    let adapter = crate::adapter::adapter()?;
    // Single identity per session (E1): if the host re-ADDs a still-live `session_id` (it shouldn't), depart
    // the stale monitor first, so one session maps to exactly one monitor (no duplicate EDID/target lingers).
    if MONITOR_MODES
        .lock()
        .map(|l| l.iter().any(|m| m.session_id == session_id))
        .unwrap_or(false)
    {
        dbglog!(
            "[pf-vd] create_monitor: session {session_id} already live — departing the stale monitor"
        );
        remove_monitor(session_id);
    }
    let mut modes = vec![Mode {
        width,
        height,
        refresh_rates: vec![refresh],
    }];
    modes.extend(default_modes());

    // Register the (pending) monitor so the mode DDIs can find it by EDID-serial id before arrival. The id
    // seeds the EDID serial + IddCx ConnectorIndex + ContainerId — i.e. the monitor's OS IDENTITY. Honor the
    // host's per-client `preferred_id` when it is valid + not currently live, so a given client gets a
    // STABLE identity across reconnects (→ Windows reapplies its saved per-monitor DPI scaling); else fall
    // back to the lowest-free id (auto — the original slot-based behavior). A bounded reused id (vs a
    // monotonic counter) keeps IddCx reusing the same OS target slot rather than leaving a ghost monitor
    // node behind (the slot-exhaustion wedge). Allocated under the lock with the push so two concurrent ADDs
    // can't pick the same id.
    let id = {
        let mut lock = lock_monitors();
        let id = resolve_id(&lock, preferred_id);
        // Same-id mode history (P2 union semantics): a RE-ARRIVED monitor advertises every mode
        // its departed predecessor served, so the OS's arrival-pinned settable set already
        // contains them — a return to any previously-used size is then an IN-PLACE mode set.
        {
            let hist = MODE_HISTORY.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, prev)) = hist.iter().find(|(i, _)| *i == id) {
                union_modes(&mut modes, prev);
            }
        }
        lock.push(MonitorObject {
            object: None,
            id,
            modes,
            session_id,
            target_id: 0,
            adapter_luid_low: 0,
            adapter_luid_high: 0,
            swap_chain_processor: None,
            frame_channel: None,
            preserved_publisher: None,
            preserved_stash: None,
            hw_cursor,
            cursor_worker: None,
            cursor_forward_on: true,
            created_at: Instant::now(),
        });
        id
    };

    // EDID (serial = id) describes the monitor; the OS calls back into parse_monitor_description.
    let mut edid = crate::edid::Edid::generate_with(id, client_lum);
    let mut desc = pod_init!(iddcx::IDDCX_MONITOR_DESCRIPTION);
    desc.Size = core::mem::size_of::<iddcx::IDDCX_MONITOR_DESCRIPTION>() as u32;
    desc.Type = iddcx::IDDCX_MONITOR_DESCRIPTION_TYPE::IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    desc.DataSize = edid.len() as u32;
    // SAFETY: `edid` is a local Vec that outlives this `create_monitor` call; IddCxMonitorCreate (below)
    // reads through `pData` SYNCHRONOUSLY, before `edid` drops — the pointer never escapes the call.
    desc.pData = edid.as_mut_ptr().cast();

    let mut info = pod_init!(iddcx::IDDCX_MONITOR_INFO);
    info.Size = core::mem::size_of::<iddcx::IDDCX_MONITOR_INFO>() as u32;
    info.MonitorContainerId = container_guid(id);
    info.MonitorType =
        wdk_sys::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
    info.ConnectorIndex = id;
    info.MonitorDescription = desc;

    let mut attr = pod_init!(wdk_sys::WDF_OBJECT_ATTRIBUTES);
    attr.Size = core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32;
    attr.ExecutionLevel = wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent;
    attr.SynchronizationScope =
        wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent;

    let create_in = iddcx::IDARG_IN_MONITORCREATE {
        ObjectAttributes: &raw mut attr,
        pMonitorInfo: &raw mut info,
    };
    let mut create_out = pod_init!(iddcx::IDARG_OUT_MONITORCREATE);
    // SAFETY: adapter is a valid IddCx adapter; create_in points to valid local storage read synchronously.
    let st = unsafe { wdk_iddcx::IddCxMonitorCreate(adapter, &create_in, &mut create_out) };
    dbglog!("[pf-vd] IddCxMonitorCreate(id={id}) -> {st:#x}");
    if !wdk_iddcx::nt_success(st) {
        remove_by_id(id);
        return None;
    }
    let monitor = create_out.MonitorObject;
    {
        let mut lock = lock_monitors();
        if let Some(m) = lock.iter_mut().find(|m| m.id == id) {
            m.object = Some(monitor);
        }
    }

    // Tell the OS the monitor is plugged in.
    let mut arrival_out = pod_init!(iddcx::IDARG_OUT_MONITORARRIVAL);
    // SAFETY: `monitor` is the just-created IddCx monitor handle.
    let st = unsafe { wdk_iddcx::IddCxMonitorArrival(monitor, &mut arrival_out) };
    dbglog!("[pf-vd] IddCxMonitorArrival(id={id}) -> {st:#x}");
    if !wdk_iddcx::nt_success(st) {
        // Arrival failed on a monitor we already CREATED. It must be torn down with `WdfObjectDelete`:
        // `IddCxMonitorDeparture` is only valid for an ARRIVED monitor, so departing here would be a
        // no-op that LEAKS the IddCx monitor object and permanently pins its slot against the adapter's
        // `MaxMonitorsSupported` budget — the leak that, asymmetric with the create-failure path just
        // above (which only reclaims the id, having no object to delete), accelerates the ADD 0x80070490
        // wedge. Reclaim the id FIRST (drop the `MONITOR_MODES` entry that still holds this handle) so a
        // concurrent `clear_all`/`reap_orphaned` can't grab + depart the handle we're about to delete,
        // THEN delete the object — `monitor` is a local copy of the handle, valid across both.
        dbglog!(
            "[pf-vd] IddCxMonitorArrival(id={id}) FAILED — reclaiming the id + deleting the created monitor"
        );
        remove_by_id(id);
        // SAFETY: `monitor` is the just-created (not-yet-arrived) IddCx monitor handle, now owned solely
        // here (its `MONITOR_MODES` entry was just removed); `WdfObjectDelete` takes a `WDFOBJECT` (a raw
        // handle cast, as in the swap-chain / device-cleanup teardowns).
        unsafe {
            call_unsafe_wdf_function_binding!(WdfObjectDelete, monitor as WDFOBJECT);
        }
        return None;
    }

    let (target_id, luid_low, luid_high) = (
        arrival_out.OsTargetId,
        arrival_out.OsAdapterLuid.LowPart,
        arrival_out.OsAdapterLuid.HighPart,
    );
    {
        let inherit = cursor_forward_desired(target_id);
        let mut lock = lock_monitors();
        if let Some(m) = lock.iter_mut().find(|m| m.id == id) {
            m.target_id = target_id;
            m.adapter_luid_low = luid_low;
            m.adapter_luid_high = luid_high;
            // The mid-stream render flip survives entry churn: the fresh entry starts at the
            // session's DESIRED state, not the declare-at-delivery default.
            m.cursor_forward_on = inherit;
        }
    }
    Some((id, target_id, luid_low, luid_high))
}

/// How many distinct resolutions a monitor's advertised list may accumulate (the requested head +
/// history + the built-in fallbacks). Bounds the union growth across many resizes; the OLDEST
/// history entries fall off first.
const MODE_LIST_CAP: usize = 12;

/// `IOCTL_UPDATE_MODES` (v4): refresh the LIVE monitor's advertised mode list to lead with a new
/// preferred mode and push the new TARGET mode list to the OS via `IddCxMonitorUpdateModes2` —
/// the in-place mid-stream resize (`design/first-frame-and-resize-latency.md` P2). No departure:
/// the monitor's OS identity, its swap-chain worker and the retained frame stash all survive.
/// The `*2` (HDR) DDI matches the `*2` mode/buffer family this driver already requires
/// (IddCx 1.10), so it adds no new OS floor.
///
/// UNION semantics (on-glass finding, build 26200): the OS re-parses the description AND
/// re-queries target modes after `UpdateModes2` — our callbacks served the fresh list — yet the
/// SETTABLE set stays pruned to the modes known at monitor ARRIVAL (the monitor source-mode set
/// is pinned then). So replacing the list can only ever LOSE settable modes (v1 of this op
/// dropped the arrival mode from the target list, breaking even a resize BACK to it); the update
/// therefore accumulates — new mode first, every previously-advertised mode kept (deduped by
/// resolution, capped at [`MODE_LIST_CAP`]) — and the real payoff is at the NEXT re-arrival,
/// where [`create_monitor`]'s same-id history union makes every previously-used mode settable.
///
/// The stored list is updated FIRST (under the lock) so any OS re-query through the mode DDIs
/// ([`modes_for_object`]/[`modes_for_id`]) sees the new list, and REVERTED if the DDI fails — the
/// OS then still holds the old list and the two stay coherent. The DDI itself is called OUTSIDE
/// the lock (it may re-enter the mode-query callbacks, which lock [`MONITOR_MODES`]).
pub fn update_monitor_modes(session_id: u64, width: u32, height: u32, refresh: u32) -> NTSTATUS {
    // Swap the stored list (union — see above) + grab the live handle under the lock.
    let (object, old_modes, new_modes) = {
        let mut lock = lock_monitors();
        let Some(m) = lock.iter_mut().find(|m| m.session_id == session_id) else {
            return crate::STATUS_NOT_FOUND;
        };
        let Some(object) = m.object else {
            return crate::STATUS_NOT_FOUND; // created but not yet arrived — nothing to update
        };
        let mut new_modes = vec![Mode {
            width,
            height,
            refresh_rates: vec![refresh],
        }];
        union_modes(&mut new_modes, &m.modes);
        let old = core::mem::replace(&mut m.modes, new_modes.clone());
        (object, old, new_modes)
    };

    // The OS's target-mode list for this monitor (the `*2`/HDR shape, like `monitor_query_modes2`).
    let mut targets: Vec<iddcx::IDDCX_TARGET_MODE2> = flatten(&new_modes)
        .map(|item| target_mode2(item.width, item.height, item.refresh_rate))
        .collect();
    let mut in_args = pod_init!(iddcx::IDARG_IN_UPDATEMODES2);
    in_args.Reason = iddcx::IDDCX_UPDATE_REASON::IDDCX_UPDATE_REASON_OTHER;
    in_args.TargetModeCount = targets.len() as u32;
    in_args.pTargetModes = targets.as_mut_ptr();
    // SAFETY: `object` is a live IddCx monitor handle (arrived — checked above; a concurrent REMOVE
    // is serialized by the host, which only ever resizes a monitor its own session holds a lease
    // on). `in_args` points at valid local storage (`targets` outlives the synchronous DDI call).
    let st = unsafe { wdk_iddcx::IddCxMonitorUpdateModes2(object, &in_args) };
    dbglog!(
        "[pf-vd] IddCxMonitorUpdateModes2(session={session_id}, {width}x{height}@{refresh}) -> {st:#x}"
    );
    if !wdk_iddcx::nt_success(st) {
        // Keep the stored list coherent with what the OS actually holds (the old one).
        let mut lock = lock_monitors();
        if let Some(m) = lock.iter_mut().find(|m| m.session_id == session_id) {
            m.modes = old_modes;
        }
        return st;
    }
    crate::STATUS_SUCCESS
}

/// `IOCTL_REMOVE`: depart + drop the monitor for `session_id`. Returns true if one was removed.
pub fn remove_monitor(session_id: u64) -> bool {
    // Pull out the IddCx handle AND the swap-chain processor under the lock, but drop the processor
    // (which RAII-joins its worker thread) only AFTER the lock guard is released — joining a worker
    // while holding `MONITOR_MODES` would head-block the whole control plane / risk a self-deadlock.
    let (monitor, processor) = {
        let mut lock = lock_monitors();
        let Some(pos) = lock.iter().position(|m| m.session_id == session_id) else {
            return false;
        };
        let mut entry = lock.remove(pos);
        // Keep the departing monitor's advertised list for its id — the next same-id create
        // unions it back in (P2 mode history; see MODE_HISTORY).
        remember_modes(entry.id, &entry.modes);
        (entry.object, entry.swap_chain_processor.take())
    };
    // Drop the worker FIRST (it joins + deletes the swap-chain), THEN depart the monitor.
    drop(processor);
    if let Some(m) = monitor {
        // SAFETY: `m` is a live IddCx monitor handle; departure tears it down.
        unsafe { wdk_iddcx::IddCxMonitorDeparture(m) };
    }
    true
}

/// `IOCTL_CLEAR_ALL`: depart + drop every monitor (host-startup orphan reap).
pub fn clear_all() {
    // Drain every entry under the lock, keeping each (handle, processor); drop the processors (RAII-join
    // their workers) only AFTER releasing the lock, then depart the monitors. See `remove_monitor`.
    let mut drained: Vec<(
        Option<iddcx::IDDCX_MONITOR>,
        Option<crate::swap_chain_processor::SwapChainProcessor>,
    )> = {
        let mut lock = lock_monitors();
        lock.drain(..)
            .map(|mut m| (m.object, m.swap_chain_processor.take()))
            .collect()
    };
    // Drop all workers FIRST (join + delete their swap-chains), THEN depart the monitors.
    for (_, processor) in &mut drained {
        drop(processor.take());
    }
    for (object, _) in drained {
        if let Some(m) = object {
            // SAFETY: `m` is a live IddCx monitor handle.
            unsafe { wdk_iddcx::IddCxMonitorDeparture(m) };
        }
    }
}

/// `EvtCleanupCallback` (device removal, [`crate::callbacks::device_cleanup`]): drop every monitor's heavy
/// resources — the swap-chain processor workers (each RAII-joins its thread + deletes its swap-chain) — and
/// clear the list, WITHOUT `IddCxMonitorDeparture` (the framework tears the IddCx monitors down together
/// with the departing device; departing here would double-tear). Frees our worker threads promptly even
/// though the per-devnode WUDFHost (`ProcessSharingDisabled`) would also reap them when it exits.
pub fn cleanup_for_device_removal() {
    let mut drained: Vec<Option<crate::swap_chain_processor::SwapChainProcessor>> = {
        let mut lock = lock_monitors();
        lock.drain(..)
            .map(|mut m| m.swap_chain_processor.take())
            .collect()
    };
    // Drop the workers (join their threads) AFTER releasing the lock — joining under MONITOR_MODES would
    // head-block the control plane (same discipline as remove_monitor / clear_all).
    for processor in &mut drained {
        drop(processor.take());
    }
}

/// Drop a pending entry by id (create failed before arrival).
fn remove_by_id(id: u32) {
    lock_monitors().retain(|m| m.id != id);
}

/// Resolve the id to name a new monitor by: honor the host's `preferred` per-client id when it is in the
/// valid range (`1..=15`, so the IddCx `ConnectorIndex` = id stays `< MaxMonitorsSupported` = 16) AND not
/// currently live (two live monitors MUST have distinct ids/connectors); otherwise fall back to
/// [`alloc_monitor_id`] (auto, lowest-free). NEVER auto-departs a colliding live monitor — that would tear
/// down an unrelated concurrent client — so the live-uniqueness invariant is preserved even against a host
/// bug. `preferred == 0` (anonymous/TOFU/GameStream) always falls through to auto. Caller holds `MONITOR_MODES`.
fn resolve_id(modes: &[MonitorObject], preferred: u32) -> u32 {
    if (1..=15).contains(&preferred) && !modes.iter().any(|m| m.id == preferred) {
        preferred
    } else {
        alloc_monitor_id(modes)
    }
}

/// The lowest monitor id (≥1) not currently live. Reusing freed ids (instead of a monotonic counter) keeps
/// the connector index / EDID serial / container GUID bounded to the number of concurrent monitors, so a
/// fresh ADD reuses a departed monitor's OS target slot rather than allocating a new one and orphaning the
/// old (the ghost-monitor accumulation that wedges ADD at 0x80070490 ERROR_NOT_FOUND). Caller holds
/// `MONITOR_MODES`. With ≤ N live ids, a free one always exists in `1..=N+1` (pigeonhole).
fn alloc_monitor_id(modes: &[MonitorObject]) -> u32 {
    (1u32..=modes.len() as u32 + 1)
        .find(|id| !modes.iter().any(|m| m.id == *id))
        .unwrap_or(1)
}

/// A deterministic, monitor-unique container GUID (groups targets into a physical device). Derived from
/// `id` so it is stable + collision-free without a random source.
fn container_guid(id: u32) -> wdk_sys::GUID {
    wdk_sys::GUID {
        Data1: 0x7066_7664u32.wrapping_add(id),
        Data2: 0x7044,
        Data3: 0x5350,
        Data4: [
            0xa1,
            0xb2,
            0xc3,
            0xd4,
            0xe5,
            0xf6,
            (id >> 8) as u8,
            id as u8,
        ],
    }
}
