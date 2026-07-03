//! Per-pad Windows resource RAII + the **sealed gamepad channel** broker (DualSense / DualShock 4 /
//! XUSB backends).
//!
//! Each virtual pad owns three OS resources: the **unnamed** DATA section the `pf_dualsense`/`pf_xusb`
//! driver works against (`XusbShm`/`PadShm`), the tiny **named** bootstrap mailbox
//! (`pf_driver_proto::gamepad::PadBootstrap`) that hands the driver a duplicated handle to it, and the
//! `SwDeviceCreate`'d software devnode the driver loads on. [`Shm`] and [`SwDevice`] own the resources
//! with RAII; [`PadChannel`] owns the two sections plus the delivery handshake.
//!
//! **Why the channel is sealed** (`design/gamepad-channel-sealing.md`): the DATA section used to be a
//! `Global\pf…-shm-<index>` named section with an SY+LS DACL, which let any *sibling LocalService*
//! process open it by name to read the live controller input or inject/forge input and rumble — the
//! same name-open vector the frame ring closed (`design/idd-push-security.md`). The DATA section is now
//! UNNAMED with a SYSTEM-only DACL and reaches the driver exclusively as a handle this host duplicated
//! into its WUDFHost (a duplicated handle carries the source's access, so no LS ACE is needed). The pad
//! drivers are UMDF HID minidrivers with **no control device** (hidclass owns the stack), so unlike the
//! frame channel there is no IOCTL to deliver the handle or learn the WUDFHost pid — hence the
//! late-bound [`PadBootstrap`] mailbox handshake, the one *named* object left. It carries only pids and
//! a handle VALUE (meaningless outside the target process), so tampering with it yields at worst a
//! gamepad DoS, never a read or an injection; the empirical floor from the frame work holds here too
//! (a LocalService token is DACL-denied `OpenProcess` on a UMDF WUDFHost for every access right).

use anyhow::{anyhow, bail, Context, Result};
use pf_driver_proto::gamepad::{PadBootstrap, BOOT_MAGIC, GAMEPAD_PROTO_VERSION};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS, CM_LOCATE_DEVNODE_NORMAL,
    CM_PROB, CR_SUCCESS, DN_DRIVER_LOADED, DN_HAS_PROBLEM, DN_STARTED,
};
use windows::Win32::Devices::Enumeration::Pnp::{SwDeviceClose, HSWDEVICE};
use windows::Win32::Foundation::{
    DuplicateHandle, GetLastError, SetLastError, DUPLICATE_HANDLE_OPTIONS, ERROR_ALREADY_EXISTS,
    HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Least access the pad driver needs on the duplicated DATA section: it only MAPS it read/write, so
/// `SECTION_MAP_READ | SECTION_MAP_WRITE` (== the driver's `FILE_MAP_RW`). Granted explicitly in
/// [`PadChannel::deliver_to`] instead of `DUPLICATE_SAME_ACCESS` (least privilege for the sealed
/// section — the driver's handle then can't take ownership / change security / delete the object).
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;

/// An anonymous (pagefile-backed) shared section + its mapped read/write view. RAII: drop unmaps the
/// view, then the [`OwnedHandle`] closes the section handle (in that order). Created either
/// [unnamed](Self::create_unnamed) (the sealed DATA section — reachable only by handle duplication) or
/// [named](Self::create_named) (the bootstrap mailbox the driver opens by name).
pub(super) struct Shm {
    /// Owns the section handle (closed on drop). Also the duplication source for the sealed channel —
    /// see [`Shm::raw_handle`].
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

/// Build a `SECURITY_ATTRIBUTES` from an SDDL literal (`psd` is OS-allocated and leaked — acceptable
/// for the handful of pad channels a host creates; it must outlive the returned `SECURITY_ATTRIBUTES`).
fn sddl_sa(sddl: PCWSTR) -> Result<SECURITY_ATTRIBUTES> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is valid; `psd` receives an OS-allocated descriptor (leaked — see above).
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
    }
    Ok(SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    })
}

impl Shm {
    /// Create + zero an **unnamed** `size`-byte section, mapped read/write — the sealed DATA section.
    /// SDDL `D:P(A;;GA;;;SY)` (SYSTEM-only, protected): with no name there is nothing to enumerate,
    /// open, or squat, and the driver reaches it through a duplicated handle, which carries the
    /// source's access without re-checking the object DACL (the exact property the frame ring
    /// validated on-glass — `design/idd-push-security.md`).
    pub(super) fn create_unnamed(size: usize) -> Result<Shm> {
        let sa = sddl_sa(w!("D:P(A;;GA;;;SY)"))?;
        Self::create_inner(&sa, PCWSTR::null(), size).context("create unnamed gamepad DATA section")
    }

    /// Create + zero a **named** `size`-byte section, mapped read/write — the bootstrap mailbox. SDDL
    /// `D:(A;;GA;;;SY)(A;;GA;;;LS)`: SYSTEM (this host) + LocalService (the driver's WUDFHost opens it
    /// by name). Safe to leave name-openable because it carries nothing exploitable (see the module
    /// docs). **Squat-checked**: `Global\` names are creatable by any service holding
    /// `SeCreateGlobalPrivilege` (LocalService has it), so if the name already exists —
    /// `ERROR_ALREADY_EXISTS`, meaning `CreateFileMappingW` silently *opened* a pre-existing object we
    /// don't control — we close and retry briefly (our own driver holds the name for microseconds per
    /// poll tick), then fail loudly rather than run the handshake through an attacker-owned (or
    /// another host instance's) mailbox.
    pub(super) fn create_named(name: &HSTRING, size: usize) -> Result<Shm> {
        let sa = sddl_sa(w!("D:(A;;GA;;;SY)(A;;GA;;;LS)"))?;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: clearing the thread error slot so ERROR_ALREADY_EXISTS below is unambiguous.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let shm = Self::create_inner(&sa, PCWSTR(name.as_ptr()), size)
                .with_context(|| format!("create gamepad bootstrap mailbox {name}"))?;
            // SAFETY: read immediately after the create; windows-rs only touches the error slot on
            // failure, so a success here preserves CreateFileMappingW's ALREADY_EXISTS signal.
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return Ok(shm);
            }
            // `shm` drops here → unmap + close our handle to the foreign object, then retry.
        }
        bail!(
            "bootstrap mailbox {name} already exists and stayed alive across retries — another \
             punktfunk-host instance is serving this pad index, or a local service is squatting the \
             name (gamepad DoS attempt?)"
        );
    }

    fn create_inner(sa: &SECURITY_ATTRIBUTES, name: PCWSTR, size: usize) -> Result<Shm> {
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the caller's SDDL; the
        // descriptor behind `sa` outlives this call (leaked by `sddl_sa`).
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa),
                PAGE_READWRITE,
                0,
                size as u32,
                name,
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
            return Err(anyhow!("MapViewOfFile failed"));
        }
        // SAFETY: `view` points at `size` writable bytes (just mapped).
        unsafe { core::ptr::write_bytes(view.Value as *mut u8, 0, size) };
        Ok(Shm { handle, view })
    }

    /// The mapped section's base pointer. Stable for the `Shm`'s lifetime (moving the `Shm` does not
    /// relocate the OS mapping — the view address is fixed by `MapViewOfFile`).
    pub(super) fn base(&self) -> *mut u8 {
        self.view.Value as *mut u8
    }

    /// The section handle as a borrowed `HANDLE` (the sealed channel's duplication source).
    fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: `view` came from `MapViewOfFile`; unmap it BEFORE the `handle` field closes the
        // section (struct fields drop only after this `Drop::drop` returns).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

// ── The sealed-channel bootstrap broker ─────────────────────────────────────────────────────────

/// Global delivery sequence for [`PadBootstrap::handle_seq`] — host-wide monotonic and never 0, so two
/// consecutive pads on the same index can't hand the (persistent, out-of-band-devnode) driver the same
/// seq twice. Starts at 1.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(1);

/// Hard cap on delivery attempts per pad: each attempt duplicates a handle into a WUDFHost, so a
/// tampered mailbox flapping `driver_pid` must not mint unbounded remote handles (DoS containment).
/// A legitimate pad needs exactly one (a driver restart within one pad lifetime is not a thing —
/// the WUDFHost dies with the devnode).
const MAX_DELIVERY_ATTEMPTS: u32 = 16;

/// One pad's sealed host↔driver channel: the unnamed DATA section (the real `XusbShm`/`PadShm`), the
/// named bootstrap mailbox, and the delivery state machine ([`Self::pump`]) that hands the driver's
/// WUDFHost a duplicated DATA handle once it publishes its pid. Owns both sections (RAII teardown —
/// dropping the channel closes the mailbox, whose *name* then disappears, which is how a persistent
/// (out-of-band-devnode) driver detects the host is gone).
pub(super) struct PadChannel {
    data: Shm,
    boot: Shm,
    boot_name: String,
    /// Last `driver_pid` acted on (delivered or rejected) — never retry the same value, so a failed
    /// verify can't be spun into a hot loop by a static mailbox.
    last_seen_pid: u32,
    attempts: u32,
    delivered: bool,
    warned_proto: bool,
    warned_cap: bool,
}

impl PadChannel {
    /// Create the unnamed DATA section (`data_size` bytes, zeroed — the caller stamps its layout and
    /// magic) plus the named bootstrap mailbox, stamped `host_proto` first and `BOOT_MAGIC` last so a
    /// driver only trusts a fully-initialized mailbox.
    pub(super) fn create(boot_name: String, data_size: usize) -> Result<PadChannel> {
        let data = Shm::create_unnamed(data_size)?;
        let boot = Shm::create_named(
            &HSTRING::from(boot_name.as_str()),
            core::mem::size_of::<PadBootstrap>(),
        )?;
        let base = boot.base();
        // SAFETY: `base` is the live, page-aligned mailbox view (>= size_of::<PadBootstrap>()); the
        // field offsets are pinned by the proto's asserts and naturally aligned, so the atomic views
        // are valid. `host_proto` is published BEFORE `magic` (Release) — a driver that observes the
        // magic (Acquire) sees the version.
        unsafe {
            (*(base.add(core::mem::offset_of!(PadBootstrap, host_proto)) as *const AtomicU32))
                .store(GAMEPAD_PROTO_VERSION, Ordering::Relaxed);
            fence(Ordering::Release);
            (*(base.add(core::mem::offset_of!(PadBootstrap, magic)) as *const AtomicU32))
                .store(BOOT_MAGIC, Ordering::Release);
        }
        Ok(PadChannel {
            data,
            boot,
            boot_name,
            last_seen_pid: 0,
            attempts: 0,
            delivered: false,
            warned_proto: false,
            warned_cap: false,
        })
    }

    /// The DATA section's mapped base (the host side of `XusbShm`/`PadShm`).
    pub(super) fn data_base(&self) -> *mut u8 {
        self.data.base()
    }

    /// The bootstrap mailbox name (log labelling).
    pub(super) fn boot_name(&self) -> &str {
        &self.boot_name
    }

    /// Atomic `u32` load from a mailbox field.
    fn boot_load(&self, off: usize) -> u32 {
        // SAFETY: the mailbox view is live (owned by `self.boot`), page-aligned, and every
        // `PadBootstrap` u32 field offset is 4-aligned (proto asserts), so the atomic view is valid;
        // no reference into the shared region outlives the load.
        unsafe { (*(self.boot.base().add(off) as *const AtomicU32)).load(Ordering::Acquire) }
    }

    /// One tick of the delivery state machine — called from the pad's regular service pump (≤4 ms
    /// cadence) and from [`Self::deliver_eager`]. Cheap when idle: two atomic loads.
    pub(super) fn pump(&mut self) {
        // Version diagnostics: the driver writes its own proto version even when it refuses to
        // publish a pid (host/driver mismatch), so the operator sees WHY the pad never attaches.
        let drv_proto = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_proto));
        if drv_proto != 0 && drv_proto != GAMEPAD_PROTO_VERSION && !self.warned_proto {
            self.warned_proto = true;
            tracing::warn!(
                mailbox = %self.boot_name,
                driver_proto = drv_proto,
                host_proto = GAMEPAD_PROTO_VERSION,
                "gamepad driver/host protocol mismatch on the bootstrap mailbox — update the \
                 drivers: punktfunk-host.exe driver install --gamepad"
            );
        }
        let pid = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_pid));
        if pid == 0 || pid == self.last_seen_pid {
            return;
        }
        self.last_seen_pid = pid;
        if self.attempts >= MAX_DELIVERY_ATTEMPTS {
            if !self.warned_cap {
                self.warned_cap = true;
                tracing::warn!(
                    mailbox = %self.boot_name,
                    attempts = self.attempts,
                    "gamepad channel delivery cap reached — the bootstrap mailbox keeps changing \
                     its driver pid (tampering?); no further handles will be duplicated"
                );
            }
            return;
        }
        self.attempts += 1;
        match self.deliver_to(pid) {
            Ok(seq) => {
                self.delivered = true;
                tracing::info!(
                    mailbox = %self.boot_name,
                    wudf_pid = pid,
                    seq,
                    "sealed gamepad channel delivered (DATA handle duplicated into the driver's \
                     WUDFHost)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    mailbox = %self.boot_name,
                    pid,
                    error = %format!("{e:#}"),
                    "sealed gamepad channel delivery failed — will retry when the mailbox reports \
                     a different driver pid"
                );
            }
        }
    }

    /// Duplicate the DATA section into `pid`'s handle table (after verifying it is a genuine
    /// WUDFHost) and publish the handle value + owning pid, bumping `handle_seq` LAST. The driver
    /// adopts the handle by consuming the delivery; an unconsumed duplicate dies with the target
    /// process (nothing to reap — there is no fallible step after the duplication).
    fn deliver_to(&self, pid: u32) -> Result<u32> {
        // SAFETY: plain FFI; the handle (checked by `?`) is owned solely here and moved into the
        // `OwnedHandle` (single owner, closes on drop); `verify_is_wudfhost` borrows it for the
        // synchronous check and forms no lasting alias.
        let process = unsafe {
            let h = OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
            .context("OpenProcess(PROCESS_DUP_HANDLE) on the mailbox-reported pid")?;
            let process = OwnedHandle::from_raw_handle(h.0 as _);
            crate::capture::idd_push::verify_is_wudfhost(
                HANDLE(process.as_raw_handle()),
                pid,
                "gamepad-channel",
            )?;
            process
        };
        let mut remote = HANDLE::default();
        // SAFETY: `self.data.raw_handle()` is the live section handle this channel owns;
        // `process` is the live PROCESS_DUP_HANDLE target; `&mut remote` is a valid out-param.
        // Least privilege: the pad driver only MAPS the DATA section read/write (its `FILE_MAP_RW` =
        // `SECTION_MAP_READ | SECTION_MAP_WRITE`), so grant exactly that instead of copying our
        // full-access creator handle via `DUPLICATE_SAME_ACCESS` (Chen: don't over-grant unnamed
        // shared objects — a compromised driver's handle then can't `WRITE_DAC`/`DELETE` the section).
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.data.raw_handle(),
                HANDLE(process.as_raw_handle()),
                &mut remote,
                SECTION_MAP_RW,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .context("DuplicateHandle(gamepad DATA section) into the driver's WUDFHost")?;
        }
        let value = remote.0 as usize as u64;
        let base = self.boot.base();
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
        // SAFETY: live, page-aligned mailbox view; `data_handle` is 8-aligned and `handle_pid`/
        // `handle_seq` 4-aligned (proto asserts). The handle value + owning pid are published BEFORE
        // the seq (Release) — a driver that observes the new seq (Acquire) sees a complete delivery.
        unsafe {
            (*(base.add(core::mem::offset_of!(PadBootstrap, data_handle)) as *const AtomicU64))
                .store(value, Ordering::Relaxed);
            (*(base.add(core::mem::offset_of!(PadBootstrap, handle_pid)) as *const AtomicU32))
                .store(pid, Ordering::Relaxed);
            fence(Ordering::Release);
            (*(base.add(core::mem::offset_of!(PadBootstrap, handle_seq)) as *const AtomicU32))
                .store(seq, Ordering::Release);
        }
        Ok(seq)
    }

    /// Bounded wait at pad-open: pump until the mailbox produces a driver pid we act on (delivered or
    /// rejected) or `timeout` passes. Closes the identity race for the DualShock 4 (the driver reads
    /// `device_type` from the DATA section when hidclass asks for descriptors — the channel should be
    /// attached by then); the regular service pump takes over afterwards either way.
    pub(super) fn deliver_eager(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.last_seen_pid != 0 || Instant::now() >= deadline {
                if !self.delivered {
                    tracing::debug!(
                        mailbox = %self.boot_name,
                        "eager gamepad-channel delivery window passed without an attach — the \
                         service pump keeps polling (driver-attach diagnosis follows if it stays \
                         silent)"
                    );
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
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
    /// Bootstrap-mailbox name, for log lines (the DATA section is unnamed).
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
             driver is serving it (games will not see it); an old (pre-sealed-channel) driver also \
             reads as not-attached: update with punktfunk-host.exe driver install --gamepad \
             (driver_log is only written by debug driver builds, or with the PFXUSB_DEBUG_LOG / \
             PFDS_DEBUG_LOG system env var set + the device restarted)"
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
