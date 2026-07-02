//! Per-pad Windows resource RAII for the gamepad backends (DualSense / DualShock 4 / XUSB).
//!
//! Each virtual pad owns two OS resources: the named shared-memory section (+ its mapped view) the
//! `pf_dualsense`/`pf_xusb` driver reads, and the `SwDeviceCreate`'d software devnode the driver loads
//! on. Before this module, all three backends hand-rolled the same `CreateFileMappingW` +
//! `MapViewOfFile` and an identical `Drop` doing `SwDeviceClose` + `UnmapViewOfFile` + `CloseHandle` —
//! easy to drift or leak on an error path. [`Shm`] and [`SwDevice`] own those resources with RAII, so a
//! backend just holds them and the cleanup (and ordering) happens by construction.

use anyhow::{anyhow, Result};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS, CM_LOCATE_DEVNODE_NORMAL,
    CM_PROB, CR_SUCCESS, DN_DRIVER_LOADED, DN_HAS_PROBLEM, DN_STARTED,
};
use windows::Win32::Devices::Enumeration::Pnp::{SwDeviceClose, HSWDEVICE};
use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// A named, anonymous (pagefile-backed) shared section + its mapped read/write view. RAII: drop unmaps
/// the view, then the [`OwnedHandle`] closes the section handle (in that order). Replaces the three
/// backends' hand-duplicated `CreateFileMappingW` + `MapViewOfFile` + manual `Drop`.
///
/// SDDL `D:(A;;GA;;;SY)(A;;GA;;;LS)`: GENERIC_ALL to **SYSTEM** (the host creates the section and
/// writes the live HID input report into it) and **LocalService** (the account the UMDF driver's
/// WUDFHost runs under, which reads it). The old SDDL granted **Everyone** (`WD`) — on the (mistaken)
/// assumption the driver needed a restricted token's broad access — letting any local user
/// `OpenFileMapping` the section to inject controller input or tamper the trusted channel
/// (security-review 2026-06-28 #5). Verified on the RTX box (2026-06-29): the WUDFHost token is
/// `S-1-5-19` (LocalService), SYSTEM integrity, with **zero restricted SIDs** — so scoping to SY+LS is
/// sufficient for the driver and excludes normal (medium-IL, non-service) user processes.
pub(super) struct Shm {
    /// Owns the section handle (closed on drop). Held only for ownership — never read after construction.
    _handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl Shm {
    /// Create + zero a `size`-byte section named `name`, mapped read/write. The section handle is owned
    /// immediately, so any failure below (or the returned `Shm`'s drop) closes it.
    pub(super) fn create(name: &HSTRING, size: usize) -> Result<Shm> {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL literal is valid; `psd` receives an OS-allocated descriptor (freed at process
        // exit — acceptable for a host-lifetime object).
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;SY)(A;;GA;;;LS)"),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the SDDL above.
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                size as u32,
                PCWSTR(name.as_ptr()),
            )?
        };
        // SAFETY: `map` is a fresh section handle we own; take ownership immediately so that the early
        // return below (and the eventual drop) closes it. `map` (a `Copy` `HANDLE`) stays usable for the
        // `MapViewOfFile` borrow that follows — `from_raw_handle` only copies the inner pointer.
        let handle = unsafe { OwnedHandle::from_raw_handle(map.0) };
        // SAFETY: `map` is a valid section handle; map the whole thing read/write.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            // `handle` drops here → closes the section. No view to unmap.
            return Err(anyhow!("MapViewOfFile failed for {name}"));
        }
        // SAFETY: `view` points at `size` writable bytes (just mapped).
        unsafe { core::ptr::write_bytes(view.Value as *mut u8, 0, size) };
        Ok(Shm {
            _handle: handle,
            view,
        })
    }

    /// The mapped section's base pointer. Stable for the `Shm`'s lifetime (moving the `Shm` does not
    /// relocate the OS mapping — the view address is fixed by `MapViewOfFile`).
    pub(super) fn base(&self) -> *mut u8 {
        self.view.Value as *mut u8
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: `view` came from `MapViewOfFile`; unmap it BEFORE the `_handle` field closes the
        // section (struct fields drop only after this `Drop::drop` returns).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

/// A `SwDeviceCreate`'d software devnode; drop removes it via `SwDeviceClose`. Replaces the manual
/// `SwDeviceClose` each backend used to call in its `Drop`.
pub(super) struct SwDevice(HSWDEVICE);

impl SwDevice {
    pub(super) fn new(hsw: HSWDEVICE) -> Self {
        SwDevice(hsw)
    }
}

impl Drop for SwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle `SwDeviceCreate` returned; `SwDeviceClose` removes the devnode.
        unsafe { SwDeviceClose(self.0) };
    }
}

// ── Driver health surfacing ─────────────────────────────────────────────────────────────────────
//
// The gamepad drivers have no IOCTL plane (hidclass gates the stack), so the only cross-process
// signal is the shared section itself. The drivers stamp `driver_proto` into their section once
// attached (pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION); [`DriverAttach`] watches that field
// from the host's regular pump and turns silence into actionable WARN/ERROR log lines — the piece
// that used to be missing entirely: a pad could be "created" with no driver installed and nothing
// was ever logged until the user gave up ("host doesn't see my controller" bug reports).

/// How long to give PnP to bind the driver + the driver to stamp the section before warning.
const ATTACH_GRACE: Duration = Duration::from_secs(3);

/// Per-pad driver-attach watcher: feed it the section's `driver_proto` on every service tick; it
/// logs the attach (INFO), a version mismatch (WARN), or — after [`ATTACH_GRACE`] of silence — one
/// diagnosis WARN combining the driver-store check and the devnode problem code. States never
/// repeat their log line, so the pump can call this at full rate.
pub(super) struct DriverAttach {
    /// Driver label for log lines (`pf_xusb` / `pf_dualsense` / `pf_dualshock4`).
    driver: &'static str,
    /// The INF the driver store must hold for this driver (`pf_xusb.inf` / `pf_dualsense.inf`).
    inf: &'static str,
    /// The driver's own debug log, referenced in the diagnosis line.
    driver_log: &'static str,
    /// Section name, for log lines.
    shm_name: String,
    /// PnP instance id of the SwDeviceCreate'd devnode (`None` on the out-of-band fallback path).
    instance_id: Option<String>,
    created: Instant,
    state: AttachState,
}

enum AttachState {
    Waiting,
    /// Diagnosis logged; still watching so a late attach gets its INFO line.
    Warned,
    Attached,
}

impl DriverAttach {
    pub(super) fn new(
        driver: &'static str,
        inf: &'static str,
        driver_log: &'static str,
        shm_name: String,
        instance_id: Option<String>,
    ) -> DriverAttach {
        DriverAttach {
            driver,
            inf,
            driver_log,
            shm_name,
            instance_id,
            created: Instant::now(),
            state: AttachState::Waiting,
        }
    }

    /// `driver_proto` is the section field the driver stamps once attached (0 = not attached).
    pub(super) fn observe(&mut self, driver_proto: u32) {
        match self.state {
            AttachState::Attached => {}
            AttachState::Waiting | AttachState::Warned if driver_proto != 0 => {
                let late = matches!(self.state, AttachState::Warned);
                tracing::info!(
                    driver = self.driver,
                    shm = %self.shm_name,
                    proto = driver_proto,
                    late,
                    "gamepad driver attached to the shared section"
                );
                if driver_proto != pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION {
                    tracing::warn!(
                        driver = self.driver,
                        driver_proto,
                        host_proto = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION,
                        "gamepad driver/host protocol mismatch — update the drivers: punktfunk-host.exe driver install --gamepad"
                    );
                }
                self.state = AttachState::Attached;
            }
            AttachState::Waiting if self.created.elapsed() >= ATTACH_GRACE => {
                self.diagnose();
                self.state = AttachState::Warned;
            }
            _ => {}
        }
    }

    /// One-shot WARN with everything the host can find out about WHY the driver isn't attached:
    /// driver-store presence, the devnode's PnP status/problem code, and where to look next.
    fn diagnose(&self) {
        let store = match driver_store_has(self.inf) {
            Some(true) => "driver package present in the driver store",
            Some(false) => {
                "driver package NOT in the driver store — run: punktfunk-host.exe driver install --gamepad"
            }
            None => "driver store could not be queried (pnputil failed)",
        };
        let devnode = match &self.instance_id {
            Some(id) => devnode_status_line(id),
            None => {
                "no per-session devnode (SwDeviceCreate failed earlier — see the warning above)"
                    .to_string()
            }
        };
        tracing::warn!(
            driver = self.driver,
            shm = %self.shm_name,
            grace_secs = ATTACH_GRACE.as_secs(),
            store,
            devnode = %devnode,
            driver_log = self.driver_log,
            "gamepad driver has not attached to the shared section — the virtual pad exists but no \
             driver is serving it (games will not see it); an old (pre-health) driver also reads as \
             not-attached: update with punktfunk-host.exe driver install --gamepad"
        );
    }
}

/// Driver-store inventory (`pnputil /enum-drivers`), lower-cased, fetched once per process — only
/// consulted on the failure path, so the one-off subprocess cost never hits a healthy session.
fn driver_store_inventory() -> &'static str {
    static INV: OnceLock<String> = OnceLock::new();
    INV.get_or_init(|| {
        std::process::Command::new("pnputil")
            .arg("/enum-drivers")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
            .unwrap_or_default()
    })
}

/// Whether the driver store holds `inf` (e.g. `pf_xusb.inf`). `None` = pnputil unavailable/failed.
fn driver_store_has(inf: &str) -> Option<bool> {
    let inv = driver_store_inventory();
    if inv.is_empty() {
        return None;
    }
    Some(inv.contains(&inf.to_ascii_lowercase()))
}

/// Human-readable PnP status of a devnode: driver bound/started or the CM problem code with a hint.
fn devnode_status_line(instance_id: &str) -> String {
    let wide: Vec<u16> = instance_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut devinst = 0u32;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 instance id; `devinst` receives the handle.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return format!(
            "devnode {instance_id} not found (CM_Locate_DevNodeW CR={})",
            cr.0
        );
    }
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    // SAFETY: devinst is the devnode located above; the two out-params receive status + problem.
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS {
        return format!("devnode {instance_id}: status query failed (CR={})", cr.0);
    }
    if status.0 & DN_HAS_PROBLEM.0 != 0 {
        return format!(
            "devnode {instance_id} has PnP problem code {} ({}) [status 0x{:08x}]",
            problem.0,
            cm_problem_hint(problem.0),
            status.0
        );
    }
    format!(
        "devnode {instance_id} status 0x{:08x} (driver_loaded={} started={})",
        status.0,
        status.0 & DN_DRIVER_LOADED.0 != 0,
        status.0 & DN_STARTED.0 != 0,
    )
}

/// The CM_PROB_* codes a virtual-pad devnode realistically hits, with the operator-facing cause.
fn cm_problem_hint(problem: u32) -> &'static str {
    match problem {
        1 => "not configured — no driver bound; install the drivers",
        10 => "device failed to start — driver bound but its start failed; check the driver log",
        18 => "reinstall required — re-run driver install",
        24 => "device not present/working — PnP could not start the virtual devnode",
        28 => "drivers not installed — the pf driver package is missing from the store or its certificate is not trusted",
        31 => "driver failed to load — binding found the package but loading it failed",
        39 => "driver corrupt or missing — reinstall the drivers",
        43 => "reported failure after start — check the driver log",
        52 => "driver signature rejected — certificate not in Root/TrustedPublisher, or blocked by Memory Integrity",
        _ => "see Device Manager for this code",
    }
}
