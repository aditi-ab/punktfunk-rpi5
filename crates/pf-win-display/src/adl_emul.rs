//! AMD ADL connector/EDID emulation — the shared implementation behind the `display-disturb
//! adl-emul` probe AND the `edid_lock` display-policy axis (the software equivalent of an
//! HPD-holding dummy plug).
//!
//! Three field cases (ASUS VG32VQ1B/DP, Odyssey G60SD/DP, LG UltraGear 32GS95UE/HDMI — all
//! RX 9070 XT hosts) share one mechanism: a connected-but-asleep sink whose standby HPD/DDC/link
//! servicing the KMD performs below every OS lever (CCD deactivation, devnode disable and CRU
//! EDID overrides are confirmed no-ops — `design/vdisplay-disturbance-immunity.md` §2a/§3). The
//! one software lever that can stop the servicing at its SOURCE is the driver's own connector
//! emulation: pin the live EDID with `ADL2_Adapter_ConnectionData_Set`, then
//! `ADL2_Adapter_EmulationMode_Set(ADL_EMUL_MODE_ALWAYS)` so the driver stops caring what the
//! physical pins report.
//!
//! [`run`] performs one action across every AMD adapter's connectors and returns the per-op
//! [`OpRecord`]s — the probe tool prints them as bench lines, the host tracing-logs them. The
//! `edid_lock` axis drives [`lock_for_stream`]/[`unlock_after_stream`] at the Exclusive isolate
//! (`pf-vdisplay`'s Windows manager), with a crash journal ([`startup_recover`]) because pinned
//! emulation persists across host restarts — and can persist across REBOOTS, so every lock ships
//! with its unlock (driver reinstall = the escape hatch of last resort).
//!
//! Everything is best-effort by design: no AMD driver (`atiadlxx.dll` absent) means the axis is
//! inert, and each rc is preserved so a field log answers the consumer-vs-Pro gating question
//! (`ADL_ERR_NOT_SUPPORTED(-8)` vs `ADL_OK`).

// FFI mirrors of ADL's C structs — keep AMD's field names verbatim so the header diff is
// mechanical.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::Instant;

use windows::core::{s, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

// ---- ADL constants (adl_defines.h, GPUOpen display-library) ----

const ADL_OK: i32 = 0;
const ADL_MAX_PATH: usize = 256;
const ADL_MAX_DISPLAY_EDID_DATA_SIZE: usize = 1024;
const ADL_MAX_RAD_LINK_COUNT: usize = 15;

const ADL_EMUL_MODE_OFF: i32 = 0;
const ADL_EMUL_MODE_ALWAYS: i32 = 3;

const ADL_QUERY_REAL_DATA: i32 = 0;
const ADL_QUERY_EMULATED_DATA: i32 = 1;

const ADL_EMUL_STATUS_REAL_DEVICE_CONNECTED: i32 = 0x1;
const ADL_EMUL_STATUS_EMULATED_DEVICE_PRESENT: i32 = 0x2;
const ADL_EMUL_STATUS_EMULATED_DEVICE_USED: i32 = 0x4;

const AMD_VENDOR_ID: i32 = 1002;

/// Decode the rc values a field log will actually contain (adl_defines.h) — `-8` vs `-1` is the
/// whole consumer-vs-Pro question, so spell them out.
pub fn rc_str(rc: i32) -> &'static str {
    match rc {
        0 => "ADL_OK",
        1..=4 => "ADL_OK_(warning-class)",
        -1 => "ADL_ERR",
        -2 => "ADL_ERR_NOT_INIT",
        -3 => "ADL_ERR_INVALID_PARAM",
        -5 => "ADL_ERR_INVALID_ADL_IDX",
        -8 => "ADL_ERR_NOT_SUPPORTED",
        -9 => "ADL_ERR_NULL_POINTER",
        -10 => "ADL_ERR_DISABLED_ADAPTER",
        -22 => "ADL_ERR_CALL_TO_INCOMPATIABLE_DRIVER",
        -23 => "ADL_ERR_NO_ADMINISTRATOR_PRIVILEGES",
        _ => "?",
    }
}

fn connector_type_str(t: i32) -> &'static str {
    match t {
        1 => "VGA",
        2 => "DVI-D",
        3 => "DVI-I",
        8 => "HDMI-A",
        9 => "HDMI-B",
        10 => "DP",
        11 => "eDP",
        12 => "miniDP",
        13 => "VIRTUAL",
        14 => "USB-C",
        _ => "unknown",
    }
}

// ---- ADL structs (adl_structures.h, verbatim layouts) ----

#[repr(C)]
struct AdapterInfo {
    iSize: i32,
    iAdapterIndex: i32,
    strUDID: [u8; ADL_MAX_PATH],
    iBusNumber: i32,
    iDeviceNumber: i32,
    iFunctionNumber: i32,
    iVendorID: i32,
    strAdapterName: [u8; ADL_MAX_PATH],
    strDisplayName: [u8; ADL_MAX_PATH],
    iPresent: i32,
    // _WIN32 tail — this tool only builds for Windows.
    iExist: i32,
    strDriverPath: [u8; ADL_MAX_PATH],
    strDriverPathExt: [u8; ADL_MAX_PATH],
    strPNPString: [u8; ADL_MAX_PATH],
    iOSDisplayIndex: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLMSTRad {
    iLinkNumber: i32,
    rad: [u8; ADL_MAX_RAD_LINK_COUNT],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLDevicePort {
    iConnectorIndex: i32,
    aMSTRad: ADLMSTRad,
}

impl ADLDevicePort {
    /// A non-MST port at `connector` (MST RAD all-zero = "DP root / non-DP ignored" per header).
    fn root(connector: i32) -> Self {
        Self {
            iConnectorIndex: connector,
            aMSTRad: ADLMSTRad {
                iLinkNumber: 0,
                rad: [0; ADL_MAX_RAD_LINK_COUNT],
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLConnectionProperties {
    iValidProperties: i32,
    iBitrate: i32,
    iNumberOfLanes: i32,
    iColorDepth: i32,
    iStereo3DCaps: i32,
    iOutputBandwidth: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLConnectionData {
    iConnectionType: i32,
    aConnectionProperties: ADLConnectionProperties,
    iNumberofPorts: i32,
    iActiveConnections: i32,
    iDataSize: i32,
    EdidData: [u8; ADL_MAX_DISPLAY_EDID_DATA_SIZE],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ADLConnectionState {
    iEmulationStatus: i32,
    iEmulationMode: i32,
    iDisplayIndex: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLConnectorInfo {
    iConnectorIndex: i32,
    iConnectorId: i32,
    iSlotIndex: i32,
    iType: i32,
    iOffset: i32,
    iLength: i32,
}

// ---- dynamic binding (atiadlxx.dll ships with every AMD driver; absent elsewhere) ----

type AdlContext = *mut c_void;
type MallocCb = unsafe extern "C" fn(i32) -> *mut c_void;

type FnMainCreate = unsafe extern "C" fn(MallocCb, i32, *mut AdlContext) -> i32;
type FnMainDestroy = unsafe extern "C" fn(AdlContext) -> i32;
type FnNumAdapters = unsafe extern "C" fn(AdlContext, *mut i32) -> i32;
type FnAdapterInfoGet = unsafe extern "C" fn(AdlContext, *mut AdapterInfo, i32) -> i32;
type FnEdidMgmtCaps = unsafe extern "C" fn(AdlContext, i32, *mut i32) -> i32;
type FnBoardLayoutGet = unsafe extern "C" fn(
    AdlContext,
    i32,
    *mut i32,
    *mut i32,
    *mut *mut c_void,
    *mut i32,
    *mut *mut ADLConnectorInfo,
) -> i32;
type FnConnStateGet =
    unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, *mut ADLConnectionState) -> i32;
type FnConnDataGet =
    unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, i32, *mut ADLConnectionData) -> i32;
type FnConnDataSet = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, ADLConnectionData) -> i32;
type FnConnDataRemove = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort) -> i32;
type FnEmulModeSet = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, i32) -> i32;

struct Adl {
    create: FnMainCreate,
    destroy: FnMainDestroy,
    num_adapters: FnNumAdapters,
    adapter_info: FnAdapterInfoGet,
    edid_caps: FnEdidMgmtCaps,
    board_layout: FnBoardLayoutGet,
    conn_state: FnConnStateGet,
    conn_data_get: FnConnDataGet,
    conn_data_set: FnConnDataSet,
    conn_data_remove: FnConnDataRemove,
    emul_mode_set: FnEmulModeSet,
}

/// ADL's application-provided allocator: it hands buffers (board-layout arrays) back through
/// out-pointers and expects the app to own them.
unsafe extern "C" fn adl_malloc(size: i32) -> *mut c_void {
    let size = size.max(1) as usize;
    // Panic-free: a panic here would cross the extern boundary and abort the host. The Err arm
    // is unreachable in practice (size ≤ i32::MAX can't overflow the layout), and ADL treats a
    // null from its allocator as an ordinary failure.
    let Ok(layout) = std::alloc::Layout::from_size_align(size, 16) else {
        return std::ptr::null_mut();
    };
    // SAFETY: non-zero size with a fixed valid alignment; the resulting buffers are deliberately
    // never freed — ADL's contract wants an ADL_Main_Memory_Free symmetry, and leaking the <1 KiB
    // of board-layout arrays in a one-shot probe is simpler than proving allocator parity.
    unsafe { std::alloc::alloc(layout) as *mut c_void }
}

impl Adl {
    fn load() -> Option<Self> {
        // SAFETY: plain LoadLibrary of the AMD-driver-installed ADL runtime by its well-known
        // name; a foreign-DLL search-path attack would require writing to System32.
        let lib: HMODULE = unsafe { LoadLibraryA(s!("atiadlxx.dll")) }.ok()?;
        // One unsafe helper: resolve `name` or bail. Every Fn* type above matches the ADL
        // header's C signature (x64 has a single calling convention, so `extern "C"` is exact).
        unsafe fn sym<T: Copy>(lib: HMODULE, name: PCSTR) -> Option<T> {
            debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
            // SAFETY: caller passes a fn-pointer type T of pointer size (asserted above);
            // GetProcAddress yields the export's address or None.
            let f = unsafe { GetProcAddress(lib, name) }?;
            // SAFETY: reinterpreting one non-null fn pointer as the export's true C signature.
            Some(unsafe { std::mem::transmute_copy::<_, T>(&f) })
        }
        // SAFETY: `lib` is the live module handle from the successful load above.
        unsafe {
            Some(Self {
                create: sym(lib, s!("ADL2_Main_Control_Create"))?,
                destroy: sym(lib, s!("ADL2_Main_Control_Destroy"))?,
                num_adapters: sym(lib, s!("ADL2_Adapter_NumberOfAdapters_Get"))?,
                adapter_info: sym(lib, s!("ADL2_Adapter_AdapterInfo_Get"))?,
                edid_caps: sym(lib, s!("ADL2_Adapter_EDIDManagement_Caps"))?,
                board_layout: sym(lib, s!("ADL2_Adapter_BoardLayout_Get"))?,
                conn_state: sym(lib, s!("ADL2_Adapter_ConnectionState_Get"))?,
                conn_data_get: sym(lib, s!("ADL2_Adapter_ConnectionData_Get"))?,
                conn_data_set: sym(lib, s!("ADL2_Adapter_ConnectionData_Set"))?,
                conn_data_remove: sym(lib, s!("ADL2_Adapter_ConnectionData_Remove"))?,
                emul_mode_set: sym(lib, s!("ADL2_Adapter_EmulationMode_Set"))?,
            })
        }
    }
}

// ---- the library surface ----

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmulAction {
    /// Read-only: caps + layout + per-connector state. Always safe.
    Probe,
    /// Pin live EDID + `ADL_EMUL_MODE_ALWAYS` on occupied (or named) connectors.
    Lock,
    /// `ADL_EMUL_MODE_OFF` + remove pinned EDID on all (or named) connectors.
    Unlock,
}

/// One ADL call's outcome — op name, target, duration, rc (decoded via [`rc_str`]) and the
/// op-specific fields. The probe tool prints these as its bench correlation lines; the host
/// tracing-logs them. The rc IS the deliverable of a field run.
pub struct OpRecord {
    pub op: &'static str,
    pub target: String,
    pub took_ms: u128,
    pub rc: i32,
    pub extra: String,
}

impl OpRecord {
    pub fn ok(&self) -> bool {
        self.rc == ADL_OK
    }
}

impl std::fmt::Display for OpRecord {
    /// The bench line minus the caller's epoch prefix: `op target took_ms ok rc=N(STR) extra`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sep = if self.extra.is_empty() { "" } else { " " };
        write!(
            f,
            "{} {} took_ms={} ok={} rc={}({}){sep}{}",
            self.op,
            self.target,
            self.took_ms,
            self.ok(),
            self.rc,
            rc_str(self.rc),
            self.extra
        )
    }
}

/// How a [`run`] ended: the AMD runtime was absent entirely, died at init (nothing was touched),
/// or walked the connectors (each op's rc in the records — a NOT_SUPPORTED driver still `Done`s).
pub enum RunOutcome {
    /// `atiadlxx.dll` not loadable (or an export missing) — not an AMD driver install; the
    /// emulation lever does not exist on this box.
    NoAdl,
    /// `ADL2_Main_Control_Create` / adapter enumeration failed — records hold the failing rc.
    InitFailed(Vec<OpRecord>),
    /// The connector walk ran; every op's outcome is in the records.
    Done(Vec<OpRecord>),
}

impl RunOutcome {
    pub fn records(&self) -> &[OpRecord] {
        match self {
            RunOutcome::NoAdl => &[],
            RunOutcome::InitFailed(r) | RunOutcome::Done(r) => r,
        }
    }
}

fn c_str(buf: &[u8]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Perform `action` on every AMD adapter's connectors (or only `connector_filter`), returning the
/// per-op records. Read-only for [`EmulAction::Probe`]; [`EmulAction::Lock`] pins occupied
/// connectors only (unless the filter names one), matching an HPD dummy on the cables that exist.
pub fn run(action: EmulAction, connector_filter: Option<i32>) -> RunOutcome {
    let mut recs: Vec<OpRecord> = Vec::new();
    let mut rec = |op: &'static str, target: &str, took_ms: u128, rc: i32, extra: String| {
        recs.push(OpRecord {
            op,
            target: target.to_owned(),
            took_ms,
            rc,
            extra,
        });
    };

    let Some(adl) = Adl::load() else {
        return RunOutcome::NoAdl;
    };

    let mut ctx: AdlContext = std::ptr::null_mut();
    let t = Instant::now();
    // SAFETY: documented init call — our allocator callback, iEnumConnectedAdapters=0 (ALL
    // adapters: an exclusively-isolated streaming host may report no "connected" display on the
    // physical GPU), and a valid out-slot for the context.
    let rc = unsafe { (adl.create)(adl_malloc, 0, &mut ctx) };
    rec(
        "adl-init",
        "atiadlxx",
        t.elapsed().as_millis(),
        rc,
        String::new(),
    );
    if rc != ADL_OK {
        return RunOutcome::InitFailed(recs);
    }

    let mut count = 0i32;
    // SAFETY: live context; valid out-param.
    let rc = unsafe { (adl.num_adapters)(ctx, &mut count) };
    if rc != ADL_OK || count <= 0 {
        rec("adl-num-adapters", "all", 0, rc, format!("count={count}"));
        // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
        let _ = unsafe { (adl.destroy)(ctx) };
        return RunOutcome::InitFailed(recs);
    }
    let mut infos: Vec<AdapterInfo> = (0..count)
        .map(|_| {
            // SAFETY: AdapterInfo is plain ints + byte arrays — the all-zero pattern is valid,
            // and ADL fills the array in place.
            let mut a: AdapterInfo = unsafe { std::mem::zeroed() };
            a.iSize = std::mem::size_of::<AdapterInfo>() as i32;
            a
        })
        .collect();
    let bytes = std::mem::size_of_val(infos.as_slice()) as i32;
    // SAFETY: caller-allocated array of exactly `count` stamped entries, byte size passed as the
    // API's iInputSize contract requires.
    let rc = unsafe { (adl.adapter_info)(ctx, infos.as_mut_ptr(), bytes) };
    rec("adl-adapters", "all", 0, rc, format!("count={count}"));
    if rc != ADL_OK {
        // SAFETY: as above — context teardown, nothing ADL-owned used afterwards.
        let _ = unsafe { (adl.destroy)(ctx) };
        return RunOutcome::InitFailed(recs);
    }

    // What the filter below will see, one record per distinct (bus, vendor, present) — a probe
    // that walks nothing must SAY why (first .173 run: 15 adapters enumerated, zero walked,
    // zero explanation; "silence is not success" applies to the probe itself).
    let mut seen_shapes: Vec<(i32, i32, i32)> = Vec::new();
    for info in &infos {
        let shape = (info.iBusNumber, info.iVendorID, info.iPresent);
        if seen_shapes.contains(&shape) {
            continue;
        }
        seen_shapes.push(shape);
        rec(
            "adl-adapter-seen",
            &format!("bus{}", info.iBusNumber),
            0,
            ADL_OK,
            format!(
                "vendor_id={} present={} name={}",
                info.iVendorID,
                info.iPresent,
                c_str(&info.strAdapterName).trim()
            ),
        );
    }

    // One GPU surfaces as many logical adapters — probe each bus once, AMD-present only.
    let mut seen_buses: Vec<i32> = Vec::new();
    for info in &infos {
        if info.iPresent == 0
            || info.iVendorID != AMD_VENDOR_ID
            || seen_buses.contains(&info.iBusNumber)
        {
            continue;
        }
        seen_buses.push(info.iBusNumber);
        let idx = info.iAdapterIndex;
        let name = c_str(&info.strAdapterName);
        let target = format!("adapter{idx}[{}]", name.trim());

        let mut supported = 0i32;
        let t = Instant::now();
        // SAFETY: live context, adapter index from this enumeration, valid out-param.
        let rc = unsafe { (adl.edid_caps)(ctx, idx, &mut supported) };
        rec(
            "adl-edid-caps",
            &target,
            t.elapsed().as_millis(),
            rc,
            format!("supported={supported}"),
        );

        let (mut valid, mut n_slots, mut n_conn) = (0i32, 0i32, 0i32);
        let mut slots: *mut c_void = std::ptr::null_mut();
        let mut connectors: *mut ADLConnectorInfo = std::ptr::null_mut();
        let t = Instant::now();
        // SAFETY: live context + adapter index; out-pointers valid; ADL allocates the two arrays
        // through `adl_malloc` (deliberately leaked, see there).
        let rc = unsafe {
            (adl.board_layout)(
                ctx,
                idx,
                &mut valid,
                &mut n_slots,
                &mut slots,
                &mut n_conn,
                &mut connectors,
            )
        };
        rec(
            "adl-board-layout",
            &target,
            t.elapsed().as_millis(),
            rc,
            format!("connectors={n_conn} valid_flags={valid:#x}"),
        );
        let connector_list: &[ADLConnectorInfo] =
            if rc == ADL_OK && !connectors.is_null() && n_conn > 0 {
                // SAFETY: ADL just filled `connectors` with `n_conn` entries via our allocator; the
                // (leaked) buffer outlives this borrow.
                unsafe { std::slice::from_raw_parts(connectors, n_conn as usize) }
            } else {
                &[]
            };

        for c in connector_list {
            if connector_filter.is_some_and(|want| want != c.iConnectorIndex) {
                continue;
            }
            let port = ADLDevicePort::root(c.iConnectorIndex);
            let ctarget = format!(
                "adapter{idx}.connector{}[{}]",
                c.iConnectorIndex,
                connector_type_str(c.iType)
            );

            let mut state = ADLConnectionState::default();
            let t = Instant::now();
            // SAFETY: live context; port is a by-value POD naming a connector this adapter just
            // enumerated; valid out-param.
            let rc = unsafe { (adl.conn_state)(ctx, idx, port, &mut state) };
            let real = state.iEmulationStatus & ADL_EMUL_STATUS_REAL_DEVICE_CONNECTED != 0;
            rec(
                "adl-conn-state",
                &ctarget,
                t.elapsed().as_millis(),
                rc,
                format!(
                    "status={:#x} real_connected={} emulated_present={} emulated_used={} mode={} display={}",
                    state.iEmulationStatus,
                    real,
                    state.iEmulationStatus & ADL_EMUL_STATUS_EMULATED_DEVICE_PRESENT != 0,
                    state.iEmulationStatus & ADL_EMUL_STATUS_EMULATED_DEVICE_USED != 0,
                    state.iEmulationMode,
                    state.iDisplayIndex,
                ),
            );
            if rc != ADL_OK {
                continue;
            }

            match action {
                EmulAction::Probe => {
                    // SAFETY: ADLConnectionData is plain ints + a byte array; all-zero is valid
                    // and ADL overwrites it.
                    let mut data: ADLConnectionData = unsafe { std::mem::zeroed() };
                    let t = Instant::now();
                    // SAFETY: live context/port as above; REAL query fills `data` in place.
                    let rc = unsafe {
                        (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_REAL_DATA, &mut data)
                    };
                    rec(
                        "adl-conn-data",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        format!(
                            "type={} edid_bytes={}",
                            data.iConnectionType, data.iDataSize
                        ),
                    );
                }
                EmulAction::Lock => {
                    if !real && connector_filter.is_none() {
                        continue; // nothing to pin — and pinning an EMPTY connector is a different experiment
                    }
                    // SAFETY: as in Probe — zeroed then driver-filled.
                    let mut data: ADLConnectionData = unsafe { std::mem::zeroed() };
                    let t = Instant::now();
                    // SAFETY: live context/port; REAL query first — we pin exactly what the
                    // sink reports today, so the emulated display IS the user's monitor.
                    let mut rc = unsafe {
                        (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_REAL_DATA, &mut data)
                    };
                    if rc != ADL_OK {
                        // Asleep sinks can refuse a live EDID read — fall back to whatever the
                        // driver already has as emulation data (Radeon-Pro-UI parity).
                        // SAFETY: same contract, emulated-data query.
                        rc = unsafe {
                            (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_EMULATED_DATA, &mut data)
                        };
                    }
                    rec(
                        "adl-lock-read",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        format!(
                            "type={} edid_bytes={}",
                            data.iConnectionType, data.iDataSize
                        ),
                    );
                    if rc != ADL_OK || data.iDataSize <= 0 {
                        continue;
                    }
                    let t = Instant::now();
                    // SAFETY: live context/port; `data` passed by value per the ADL signature.
                    let rc = unsafe { (adl.conn_data_set)(ctx, idx, port, data) };
                    rec(
                        "adl-lock-set",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_ALWAYS) };
                    rec(
                        "adl-lock-mode-always",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                }
                EmulAction::Unlock => {
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_OFF) };
                    rec(
                        "adl-unlock-mode-off",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                    let t = Instant::now();
                    // SAFETY: live context/port; removes emulation data set earlier (harmless
                    // where none exists — the rc says so).
                    let rc = unsafe { (adl.conn_data_remove)(ctx, idx, port) };
                    rec(
                        "adl-unlock-remove",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                }
            }
        }
    }

    // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
    let _ = unsafe { (adl.destroy)(ctx) };
    RunOutcome::Done(recs)
}

// ---- the `edid_lock` display-policy axis (host-side) ----

/// The crash-recovery journal: a marker that a lock was applied and not yet unlocked. Pinned
/// emulation outlives the process (and can outlive a reboot), so a host that died mid-stream
/// must unlock on its next start ([`startup_recover`]).
fn journal_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("edid-lock-active.json")
}

/// A non-`ADL_OK` rc that is this call's documented no-op rather than a failure.
///
/// The unlock is deliberately idempotent and runs over EVERY connector — including the ones that
/// were never pinned, and every connector at all on a host recovering from an unclean exit. Some
/// drivers answer `ADL_ERR_NOT_SUPPORTED` to "turn emulation off" where there is no emulation to
/// turn off, so a clean host start emitted one WARN per connector, every time, saying nothing.
/// Four standing warnings are how a log stops being read.
///
/// Scoped to the mode-off call on purpose: `adl-unlock-remove` is the call that actually clears a
/// pin, so its rc is the one that means something, and it keeps its warning.
fn is_expected_noop(r: &OpRecord) -> bool {
    const ADL_ERR_NOT_SUPPORTED: i32 = -8;
    r.op == "adl-unlock-mode-off" && r.rc == ADL_ERR_NOT_SUPPORTED
}

fn tracing_log(prefix: &str, outcome: &RunOutcome) {
    match outcome {
        RunOutcome::NoAdl => tracing::info!(
            "{prefix}: atiadlxx.dll not loadable — not an AMD driver install; the ADL \
             emulation lever does not exist on this box"
        ),
        RunOutcome::InitFailed(recs) | RunOutcome::Done(recs) => {
            for r in recs {
                if r.ok() || is_expected_noop(r) {
                    tracing::info!("{prefix}: {r}");
                } else {
                    tracing::warn!("{prefix}: {r}");
                }
            }
        }
    }
}

/// Apply the `edid_lock` axis at stream bring-up: pin the live EDID + `ADL_EMUL_MODE_ALWAYS` on
/// every occupied AMD connector (the software HPD dummy), journaling first so a crash still
/// unlocks on the next host start. Returns whether a later [`unlock_after_stream`] is owed —
/// true whenever the ADL runtime exists, because even a partially-failed lock may have pinned
/// some connectors (each rc is in the log).
pub fn lock_for_stream() -> bool {
    // Journal BEFORE touching the driver: a crash between the first `ConnectionData_Set` and the
    // journal write would otherwise leave pinned connectors with no startup unlock owed.
    if let Err(e) = std::fs::write(journal_path(), b"{\"locked\":true}") {
        tracing::warn!(
            error = %format!("{e:#}"),
            "edid_lock: crash journal write failed — continuing (the feature degrades to \
             no-crash-journal)"
        );
    }
    let outcome = run(EmulAction::Lock, None);
    tracing_log("edid_lock", &outcome);
    if matches!(outcome, RunOutcome::NoAdl) {
        tracing::info!("edid_lock: enabled but this is not an AMD driver install — axis inert");
        let _ = std::fs::remove_file(journal_path());
        return false;
    }
    let locked = outcome
        .records()
        .iter()
        .filter(|r| r.op == "adl-lock-mode-always" && r.ok())
        .count();
    tracing::info!(
        connectors = locked,
        "edid_lock: connector emulation pinned (software HPD dummy) — unlocked at stream teardown"
    );
    true
}

/// Undo [`lock_for_stream`] at teardown: `ADL_EMUL_MODE_OFF` + remove the pinned EDID on every
/// AMD connector, then clear the crash journal. Idempotent and harmless where nothing is pinned.
pub fn unlock_after_stream() {
    let outcome = run(EmulAction::Unlock, None);
    tracing_log("edid_lock", &outcome);
    let _ = std::fs::remove_file(journal_path());
}

/// Host-startup crash recovery: a previous host that died holding the lock left connector
/// emulation pinned (it persists past the process — and can persist past a reboot). If the
/// journal marker exists, unlock everything and clear it — before any new session touches the
/// topology, mirroring `monitor_devnode::startup_recover`.
pub fn startup_recover() {
    if !journal_path().exists() {
        return;
    }
    tracing::warn!(
        "edid_lock: a previous host left connector emulation pinned (crash/kill) — unlocking"
    );
    unlock_after_stream();
}
