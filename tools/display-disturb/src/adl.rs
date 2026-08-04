//! `adl-emul` — the AMD ADL EDID-emulation probe (immunity design doc §3's "probe once, log rc",
//! promoted to a field A/B after the third RX 9070 XT standby-sink case).
//!
//! Three field cases (ASUS VG32VQ1B/DP, Odyssey G60SD/DP, LG UltraGear 32GS95UE/HDMI — all
//! RX 9070 XT hosts) share one mechanism: a connected-but-asleep sink whose standby HPD/DDC/link
//! servicing the KMD performs below every OS lever (CCD deactivation, devnode disable and CRU
//! EDID overrides are confirmed no-ops — design doc §2a/§3). The one software lever that could
//! stop the servicing at its SOURCE is the driver's own connector emulation — the software
//! equivalent of an HPD-holding dummy plug: pin the live EDID with
//! `ADL2_Adapter_ConnectionData_Set`, then `ADL2_Adapter_EmulationMode_Set(ADL_EMUL_MODE_ALWAYS)`
//! so the driver stops caring what the physical pins report. The design doc marked the API
//! "likely Pro-gated" on hearsay; nobody — here or in the community record — ever probed consumer
//! Adrenalin. This subcommand is that probe:
//!
//! * `adl-emul`                       — read-only: caps, board layout, per-connector state.
//! * `adl-emul --lock [--connector N]` — pin the live EDID + `ADL_EMUL_MODE_ALWAYS` (occupied
//!   connectors only unless `--connector` names one).
//! * `adl-emul --unlock [--connector N]` — `ADL_EMUL_MODE_OFF` + remove the pinned EDID.
//!
//! Every call prints the bench's `epoch_ms op target took_ms ok` line plus `rc=` (decoded): the
//! rc IS the deliverable — `ADL2_Adapter_EDIDManagement_Caps` answering `supported=1` with
//! `--lock` returning `ADL_OK` on a consumer RX card kills the Pro-gating assumption, and a
//! locked connector during a stream with the sink asleep is the direct A/B for the metronomic
//! stall. `--unlock` (or a driver reinstall) restores; emulation state can persist across
//! reboots, so a `--lock` run must always be paired with a later `--unlock`.

// FFI mirrors of ADL's C structs — keep AMD's field names verbatim so the header diff is
// mechanical.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
fn rc_str(rc: i32) -> &'static str {
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
    // SAFETY: non-zero size with a fixed valid alignment; the resulting buffers are deliberately
    // never freed — ADL's contract wants an ADL_Main_Memory_Free symmetry, and leaking the <1 KiB
    // of board-layout arrays in a one-shot probe is simpler than proving allocator parity.
    unsafe {
        std::alloc::alloc(std::alloc::Layout::from_size_align(size, 16).expect("tiny ADL alloc"))
            as *mut c_void
    }
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

// ---- the probe ----

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmulAction {
    /// Read-only: caps + layout + per-connector state. Always safe.
    Probe,
    /// Pin live EDID + `ADL_EMUL_MODE_ALWAYS` on occupied (or named) connectors.
    Lock,
    /// `ADL_EMUL_MODE_OFF` + remove pinned EDID on all (or named) connectors.
    Unlock,
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The bench's correlation-line convention (`epoch_ms op target took_ms ok`), plus the decoded
/// rc and any op-specific fields — the rc is what a field report gets read for.
fn line(op: &str, target: &str, took_ms: u128, rc: i32, extra: &str) {
    let sep = if extra.is_empty() { "" } else { " " };
    println!(
        "{} {op} {target} took_ms={took_ms} ok={} rc={rc}({}){sep}{extra}",
        epoch_ms(),
        rc == ADL_OK,
        rc_str(rc),
    );
}

fn c_str(buf: &[u8]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

pub fn run(action: EmulAction, connector_filter: Option<i32>) -> ! {
    let Some(adl) = Adl::load() else {
        eprintln!(
            "atiadlxx.dll not loadable (or an export is missing) — not an AMD driver install; \
             the ADL emulation lever does not exist on this box"
        );
        std::process::exit(2);
    };

    let mut ctx: AdlContext = std::ptr::null_mut();
    let t = Instant::now();
    // SAFETY: documented init call — our allocator callback, iEnumConnectedAdapters=0 (ALL
    // adapters: an exclusively-isolated streaming host may report no "connected" display on the
    // physical GPU), and a valid out-slot for the context.
    let rc = unsafe { (adl.create)(adl_malloc, 0, &mut ctx) };
    line("adl-init", "atiadlxx", t.elapsed().as_millis(), rc, "");
    if rc != ADL_OK {
        std::process::exit(1);
    }

    let mut count = 0i32;
    // SAFETY: live context; valid out-param.
    let rc = unsafe { (adl.num_adapters)(ctx, &mut count) };
    if rc != ADL_OK || count <= 0 {
        line("adl-num-adapters", "all", 0, rc, &format!("count={count}"));
        std::process::exit(1);
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
    line("adl-adapters", "all", 0, rc, &format!("count={count}"));
    if rc != ADL_OK {
        std::process::exit(1);
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
        line(
            "adl-edid-caps",
            &target,
            t.elapsed().as_millis(),
            rc,
            &format!("supported={supported}"),
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
        line(
            "adl-board-layout",
            &target,
            t.elapsed().as_millis(),
            rc,
            &format!("connectors={n_conn} valid_flags={valid:#x}"),
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
            line(
                "adl-conn-state",
                &ctarget,
                t.elapsed().as_millis(),
                rc,
                &format!(
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
                    line(
                        "adl-conn-data",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        &format!(
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
                    line(
                        "adl-lock-read",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        &format!(
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
                    line("adl-lock-set", &ctarget, t.elapsed().as_millis(), rc, "");
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_ALWAYS) };
                    line(
                        "adl-lock-mode-always",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        "",
                    );
                }
                EmulAction::Unlock => {
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_OFF) };
                    line(
                        "adl-unlock-mode-off",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        "",
                    );
                    let t = Instant::now();
                    // SAFETY: live context/port; removes emulation data set earlier (harmless
                    // where none exists — the rc says so).
                    let rc = unsafe { (adl.conn_data_remove)(ctx, idx, port) };
                    line(
                        "adl-unlock-remove",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        "",
                    );
                }
            }
        }
    }

    // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
    let _ = unsafe { (adl.destroy)(ctx) };
    std::process::exit(0);
}
