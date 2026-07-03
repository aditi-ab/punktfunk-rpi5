//! Virtual-monitor model + lifecycle (STEP 4). Monitors are created on demand by the control plane
//! ([`crate::control`], `IOCTL_ADD`): each carries the requested mode (advertised as preferred) plus the
//! `session_id` the host keys it by and the OS target id + render-adapter LUID captured at arrival. Ported
//! from the working upstream virtual-display-rs (`monitor.rs` + `context.rs::create_monitor`), with
//! `guid: u128` → `session_id: u64` for the owned `pf_driver_proto` control plane.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use wdk_sys::{WDFOBJECT, call_unsafe_wdf_function_binding, iddcx};

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
/// by `preferred_id` (the host's per-client stable id; `0` = auto-allocate). Returns the resolved
/// `(monitor_id, target_id, adapter_luid_low, adapter_luid_high)` for the
/// [`AddReply`](pf_driver_proto::control::AddReply), or `None` on failure (no adapter yet / IddCx error).
pub fn create_monitor(
    session_id: u64,
    width: u32,
    height: u32,
    refresh: u32,
    preferred_id: u32,
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
            created_at: Instant::now(),
        });
        id
    };

    // EDID (serial = id) describes the monitor; the OS calls back into parse_monitor_description.
    let mut edid = crate::edid::Edid::generate_with(id);
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
        let mut lock = lock_monitors();
        if let Some(m) = lock.iter_mut().find(|m| m.id == id) {
            m.target_id = target_id;
            m.adapter_luid_low = luid_low;
            m.adapter_luid_high = luid_high;
        }
    }
    Some((id, target_id, luid_low, luid_high))
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
