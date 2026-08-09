//! Per-pad Windows resource RAII + the **sealed gamepad channel** broker (DualSense / DualShock 4 /
//! XUSB backends).
//!
//! Each virtual pad owns three OS resources: the **unnamed** DATA section the `pf_gamepad`/`pf_xusb`
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
//! late-bound [`PadBootstrap`] mailbox handshake, the one *named* object left.
//!
//! **What the mailbox is worth to an attacker** (corrected, security-review 2026-07-28). It carries
//! pids and a handle VALUE, and a handle value IS meaningless outside its process — but the mailbox
//! also *chooses that process*, and it is writable by LocalService (the SDDL the driver's WUDFHost
//! needs to open it by name). The delivery gate, [`pf_capture::verify_is_wudfhost`], authenticates an
//! IMAGE PATH, and `%SystemRoot%\System32\WUDFHost.exe` is world-executable: a LocalService principal
//! — the de-privileged plugin runner, or any other compromised LocalService service — can spawn its
//! own suspended WUDFHost, publish that pid, and be handed `SECTION_MAP_READ|WRITE` on this pad's DATA
//! section. That is forged HID input into the interactive desktop (the pf-mouse section drives a real
//! absolute pointer) and a read of the remote user's live controller state — so the earlier claim that
//! mailbox tampering "yields at worst a gamepad DoS, never a read or an injection" was wrong. (The
//! frame channel is NOT exposed this way: its WUDFHost pid arrives over the pf-vdisplay control
//! device, whose DACL is SYSTEM + Administrators.)
//!
//! **How v3 closes it.** The mailbox no longer decides anything. [`PadChannel::pump`] asks the
//! DEVNODE which process is serving it — [`channel_proof`], the host half of
//! `pf_driver_proto::gamepad::ChannelProof` — and duplicates into that answer. Only the driver PnP
//! actually bound to the device we `SwDeviceCreate`d can answer that device's I/O, and the lookup is
//! keyed by the instance id PnP handed back, so neither a spawned WUDFHost nor a planted look-alike
//! devnode is in the running. `driver_pid` survives as a liveness hint and nothing more; a tamperer
//! can still deny a pad, but denial was always available (squat the name) and is not what mattered.
//!
//! Two further rules make the state machine hold up around it:
//! * **One live target.** A delivery stands until its target process EXITS, judged on a retained
//!   `SYNCHRONIZE` handle so a recycled pid cannot fake it. UMDF's genuine
//!   restart-the-host-after-a-driver-crash path still re-attaches; nothing else displaces a live one.
//! * **No unproved delivery.** A pad with no `SwDeviceCreate` devnode (the out-of-band `devgen`
//!   bring-up path) has nothing to ask, so it refuses to deliver rather than fall back to the
//!   mailbox — unless an operator sets [`TRUST_MAILBOX_ENV`], which says so loudly in the log.
//!
//! What is NOT claimed: this authenticates the *devnode*, not the driver binary. An attacker who can
//! already replace the installed, signed driver package owns the endpoint by definition — that is the
//! same floor the frame channel accepts, and it sits above the SECURITY.md admin/SYSTEM boundary.

use super::channel_proof;
/// Re-exported so a pad backend needs only one `use` to wire up its channel.
pub(super) use super::channel_proof::ProofTransport;
use crate::pad_slots::PadCreateFault;
use anyhow::{anyhow, Context, Result};
use pf_driver_proto::gamepad::{PadBootstrap, BOOT_MAGIC, GAMEPAD_PROTO_VERSION};
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::{w, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS, CM_LOCATE_DEVNODE_NORMAL,
    CM_PROB, CR_SUCCESS, DN_DRIVER_LOADED, DN_HAS_PROBLEM, DN_STARTED,
};
use windows::Win32::Devices::Enumeration::Pnp::{SwDeviceClose, HSWDEVICE};
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, LocalFree, SetLastError, DUPLICATE_HANDLE_OPTIONS,
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, SetEvent, WaitForSingleObject, PROCESS_DUP_HANDLE,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
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

/// Owns an SDDL-derived `SECURITY_ATTRIBUTES` **and** the OS-allocated security descriptor its
/// `lpSecurityDescriptor` points at (`ConvertStringSecurityDescriptorToSecurityDescriptorW`
/// `LocalAlloc`s the descriptor). Drop `LocalFree`s it, so a `SecAttr` must outlive every
/// `CreateFileMappingW` that borrows its `sa`: the section copies the security info at create time, so
/// freeing after the create returns is safe — hence [`Shm::create_named`] builds one `SecAttr` before
/// its squat-retry loop and reuses it across attempts instead of re-allocating (and re-leaking) per
/// attempt.
struct SecAttr {
    sa: SECURITY_ATTRIBUTES,
    psd: PSECURITY_DESCRIPTOR,
}

impl Drop for SecAttr {
    fn drop(&mut self) {
        // SAFETY: `psd` is the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW`
        // allocated for us with `LocalAlloc`; release it with the matching `LocalFree`. Every
        // `CreateFileMappingW` that borrowed `self.sa` has already returned (so has copied the
        // security info into its section object), so no live `SECURITY_ATTRIBUTES` still points here.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

/// Build a [`SecAttr`] from an SDDL literal — a `SECURITY_ATTRIBUTES` plus the descriptor it borrows,
/// freed together on drop. The returned owner must outlive every `CreateFileMappingW` that borrows
/// its `sa` (see [`SecAttr`]).
fn sddl_sa(sddl: PCWSTR) -> Result<SecAttr> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is valid; `psd` receives a `LocalAlloc`'d descriptor that `SecAttr`'s
    // `Drop` `LocalFree`s once the section create that borrows it has returned.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
    }
    Ok(SecAttr {
        sa: SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        },
        psd,
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
        // `sa` owns the descriptor and lives to the end of this fn, so it outlives the create.
        Self::create_inner(&sa.sa, PCWSTR::null(), size)
            .context("create unnamed gamepad DATA section")
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
    ///
    /// ⚠️ That squat check only ever sees the collisions we are ALLOWED to see. `CreateFileMappingW`
    /// opens a pre-existing object with full access, so a caller the incumbent's DACL excludes is
    /// refused with `ERROR_ACCESS_DENIED` and never reaches the `ERROR_ALREADY_EXISTS` branch at
    /// all — and that is the collision the field actually produces, because this SDDL grants SYSTEM
    /// and LocalService only, while the host service runs as LocalSystem and a hand-run devtest
    /// runs as an elevated Administrator. So the "another punktfunk-host instance is serving this
    /// pad index" diagnosis below was unreachable for the one pairing that happens: on `.173`
    /// (2026-08-09) it surfaced as a bare `Zugriff verweigert (0x80070005)` under a line telling the
    /// operator to reinstall the drivers. [`classify_named_create_failure`] is what restores it.
    pub(super) fn create_named(name: &HSTRING, size: usize) -> Result<Shm> {
        // Build the descriptor ONCE and reuse it across the squat-retry loop — it (and the OS
        // allocation it owns) lives to the end of this fn, so it outlives every create below.
        // `D:P` (protected) — strip any inherited ACEs so only SYSTEM + LocalService are granted,
        // matching the intent of the other named objects (security-review 2026-07-17).
        let sa = sddl_sa(w!("D:P(A;;GA;;;SY)(A;;GA;;;LS)"))?;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: clearing the thread error slot so ERROR_ALREADY_EXISTS below is unambiguous.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let shm = match Self::create_inner(&sa.sa, PCWSTR(name.as_ptr()), size) {
                Ok(shm) => shm,
                Err(e) => return Err(classify_named_create_failure(name, e)),
            };
            // SAFETY: read immediately after the create; windows-rs only touches the error slot on
            // failure, so a success here preserves CreateFileMappingW's ALREADY_EXISTS signal.
            if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                return Ok(shm);
            }
            // `shm` drops here → unmap + close our handle to the foreign object, then retry.
        }
        // Reached only when we COULD open the incumbent (same account — two hosts both as SYSTEM,
        // or a LocalService squatter). The cross-account case exits through
        // `classify_named_create_failure` above; both carry the same fault, because to everything
        // downstream they are the same event: this index is taken.
        Err(anyhow!(
            "bootstrap mailbox {name} already exists and stayed alive across retries — another \
             punktfunk-host instance is serving this pad index, or a local service is squatting the \
             name (gamepad DoS attempt?)"
        )
        .context(PadCreateFault::IndexOwnedElsewhere))
    }

    fn create_inner(sa: &SECURITY_ATTRIBUTES, name: PCWSTR, size: usize) -> Result<Shm> {
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the caller's SDDL; the
        // descriptor behind `sa` outlives this call (owned by the caller's `SecAttr`, freed only once
        // every create that borrows it has returned).
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

/// Turn a failed NAMED-section create into an error that names the cause, because
/// `CreateFileMappingW` collapses two OPPOSITE situations into one `ERROR_ACCESS_DENIED`
/// (`0x80070005`, and on a German box the entirely unsearchable "Zugriff verweigert" the field
/// report carried):
///
/// * **the name is TAKEN, by someone whose object we may not open.** Creating over an existing name
///   is really an open, and an open is access-checked against the incumbent's DACL. The mailbox
///   SDDL grants SYSTEM + LocalService only, so the exact pairing that occurs on a dev box — the
///   LocalSystem host service holding pad 0 for a live session while an operator runs
///   `punktfunk-host.exe dualsense-windows-test` from an elevated Administrator console — is
///   refused here rather than reported as the squat it is.
/// * **the name is FREE and we may not create it.** `Global\` names need `SeCreateGlobalPrivilege`,
///   which SYSTEM and services hold and an ordinary (even elevated) user token does not.
///
/// `OpenFileMappingW` separates them, because the object-manager lookup happens BEFORE the access
/// check: an absent name is `ERROR_FILE_NOT_FOUND`, a present one we are not in the DACL of is
/// `ERROR_ACCESS_DENIED`. Everything else keeps the original wording.
///
/// The contended case additionally carries a [`PadCreateFault`], which is what stops the pad
/// manager's failure line from telling the operator to reinstall a driver that is working
/// perfectly (see [`crate::pad_slots::PadCreateFault`]).
fn classify_named_create_failure(name: &HSTRING, e: anyhow::Error) -> anyhow::Error {
    let denied = e
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|w| w.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
    if !denied {
        return e.context(format!("create gamepad bootstrap mailbox {name}"));
    }
    if named_section_exists(name) {
        return e
            .context(PadCreateFault::IndexOwnedElsewhere)
            .context(format!(
            "bootstrap mailbox {name} exists and belongs to a process this one may not open — a \
             live session's pad, held by the LocalSystem host service (its mailboxes grant SYSTEM \
             + LocalService only, so an Administrator console sees ACCESS_DENIED, not \
             ALREADY_EXISTS). Nothing is wrong with the drivers"
        ));
    }
    e.context(format!(
        "create gamepad bootstrap mailbox {name}: access denied although the name is FREE — this \
         process may not create Global\\ objects at all (that needs SeCreateGlobalPrivilege, which \
         SYSTEM and services hold and a user token does not)"
    ))
}

/// Whether a section with this name exists right now, as seen from THIS process — the
/// disambiguation [`classify_named_create_failure`] runs on. `true` also when the object is there
/// but closed to us, which is the case that matters: ACCESS_DENIED from an OPEN means the name
/// resolved and only the access check failed.
///
/// Deliberately not a security decision — a hostile squatter can make this say either thing. It
/// only ever chooses which sentence to print.
fn named_section_exists(name: &HSTRING) -> bool {
    // SAFETY: `name` is a live NUL-terminated UTF-16 string for the duration of the call. Ask for
    // the least access there is (`FILE_MAP_READ`): the handle is closed immediately and never
    // mapped — we want the lookup's verdict, not the object.
    let opened = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) };
    match opened {
        Ok(h) => {
            // SAFETY: `h` is the handle just opened here and referenced nowhere else.
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        // ERROR_FILE_NOT_FOUND (and anything else) reads as absent; ACCESS_DENIED is presence.
        Err(e) => e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
    }
}

// ── The sealed-channel bootstrap broker ─────────────────────────────────────────────────────────

/// Global delivery sequence for [`PadBootstrap::handle_seq`] — host-wide monotonic and never 0, so two
/// consecutive pads on the same index can't hand the (persistent, out-of-band-devnode) driver the same
/// seq twice. Starts at 1.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(1);

/// Hard cap on FAILED delivery attempts per pad: each attempt duplicates a handle into a WUDFHost, so
/// a tampered mailbox flapping `driver_pid` must not mint unbounded remote handles (DoS containment).
/// Only failures spend this budget — a SUCCESSFUL delivery stands until its target process exits
/// ([`PadChannel::delivered`]), so what is left here is purely the retry allowance for a driver that
/// published a pid and then died before we could reach it.
const MAX_DELIVERY_ATTEMPTS: u32 = 16;

/// How often the delivery state machine may ask the devnode for its channel proof while unattached.
/// The proof query opens a device interface and does real I/O, and the pad service pump ticks every
/// few milliseconds — without this the poll would be a hot loop on the HID stack. A driver takes tens
/// of milliseconds to publish its interface after `SwDeviceCreate`, so a quarter second costs nothing
/// perceptible at pad open and keeps the steady-state cost at zero (once delivered, nothing polls).
const PROOF_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// Operator escape hatch for the one case the device stack cannot answer: an **out-of-band devnode**
/// (`devgen`/`devcon`, the driver bring-up path) where `SwDeviceCreate` never ran, so the host has no
/// instance id to look an interface up by. Set it and the channel falls back to the pre-v3 behaviour
/// of trusting the mailbox's `driver_pid`, which is exactly the trust the security review removed —
/// hence opt-in, per-boot, and loud in the log. Never needed for a normal host: every pad and the
/// resident mouse are `SwDeviceCreate`d.
const TRUST_MAILBOX_ENV: &str = "PUNKTFUNK_PAD_CHANNEL_TRUST_MAILBOX";

/// Consecutive unanswered proof queries before the debug line escalates to one operator-facing warn.
/// At [`PROOF_PROBE_INTERVAL`] this is ~5 s — comfortably past a healthy driver's attach (tens of
/// milliseconds) and past the eager window, so it only fires when the pad really is not coming up.
const PROOF_FAILURES_BEFORE_WARN: u32 = 20;

/// One pad's sealed host↔driver channel: the unnamed DATA section (the real `XusbShm`/`PadShm`), the
/// named bootstrap mailbox, and the delivery state machine ([`Self::pump`]) that hands the driver's
/// WUDFHost a duplicated DATA handle. Owns both sections (RAII teardown — dropping the channel closes
/// the mailbox, whose *name* then disappears, which is how a persistent (out-of-band-devnode) driver
/// detects the host is gone).
pub(super) struct PadChannel {
    data: Shm,
    boot: Shm,
    boot_name: String,
    /// The devnode to ask for a channel proof, and how ([`Self::bind_devnode`]). `None` until the
    /// caller has a `SwDeviceCreate` instance id — and permanently `None` on the out-of-band devnode
    /// path, which is what [`TRUST_MAILBOX_ENV`] exists for.
    devnode: Option<(String, ProofTransport)>,
    /// The pad index the proof must agree with, so a mis-resolved interface can't cross-wire two pads.
    pad_index: u32,
    /// When the devnode was last asked (throttle — see [`PROOF_PROBE_INTERVAL`]).
    last_probe: Option<Instant>,
    /// Last pid acted on (delivered or rejected) — never retry the same value, so a failed verify
    /// can't be spun into a hot loop.
    last_seen_pid: u32,
    attempts: u32,
    /// The WUDFHost the DATA section was duplicated into. `Some` ⇒ this channel is spoken for and no
    /// other process is served while that one lives (see [`Self::pump`]).
    delivered: Option<Delivered>,
    warned_proto: bool,
    warned_cap: bool,
    warned_takeover: bool,
    warned_unproven: bool,
    proof_failures: u32,
}

/// A completed delivery: the WUDFHost we handed the DATA section to, pinned by a live handle.
/// Liveness is judged on the HANDLE, never the pid — the handle pins the process object, so a
/// recycled pid can never read as "the process we served has exited".
struct Delivered {
    pid: u32,
    /// `SYNCHRONIZE` handle to the target; signaled ⇔ it exited. Same liveness idiom the frame
    /// channel's `ChannelBroker::driver_alive` uses.
    process: OwnedHandle,
}

impl Delivered {
    fn exited(&self) -> bool {
        // SAFETY: `process` is the live `OwnedHandle` this channel owns (borrowed for this
        // synchronous call); a 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) == WAIT_OBJECT_0 }
    }
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
            devnode: None,
            pad_index: 0,
            last_probe: None,
            last_seen_pid: 0,
            attempts: 0,
            delivered: None,
            warned_proto: false,
            warned_cap: false,
            warned_takeover: false,
            warned_unproven: false,
            proof_failures: 0,
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

    /// Bind this channel to the devnode the caller just `SwDeviceCreate`d, so [`Self::pump`] can ask
    /// it for a channel proof. Call between `create_swdevice` and [`Self::deliver_eager`].
    ///
    /// `instance_id` is `None` when `SwDeviceCreate` failed and the caller fell back to an
    /// out-of-band (`devgen`) devnode: there is then no device to ask, and the channel will refuse to
    /// deliver unless [`TRUST_MAILBOX_ENV`] is set.
    pub(super) fn bind_devnode(
        &mut self,
        pad_index: u32,
        instance_id: Option<String>,
        transport: ProofTransport,
    ) {
        self.pad_index = pad_index;
        self.devnode = instance_id.map(|id| (id, transport));
    }

    /// One tick of the delivery state machine — called from the pad's regular service pump (≤4 ms
    /// cadence) and from [`Self::deliver_eager`]. Cheap when idle: an atomic load, and once the
    /// channel is delivered, one 0 ms wait.
    pub(super) fn pump(&mut self) {
        // Version diagnostics: the driver writes its own proto version even when it refuses the
        // handshake (host/driver mismatch), so the operator sees WHY the pad never attaches.
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

        // A delivery stands until its target process dies. Re-delivering to a *different* live
        // process was the pre-v3 takeover (see the module docs); UMDF restarting a host whose driver
        // crashed is the one legitimate case, and that one is visible here as an exited handle.
        if let Some(d) = self.delivered.as_ref() {
            if !d.exited() {
                return;
            }
            tracing::info!(
                mailbox = %self.boot_name,
                exited_pid = d.pid,
                "the WUDFHost this channel was delivered to has exited (driver crash / host \
                 restart) — re-attaching to the restarted driver"
            );
            self.delivered = None;
            self.attempts = 0; // a genuine restart earns a fresh budget
            self.last_seen_pid = 0;
            self.last_probe = None;
        }
        if self.attempts >= MAX_DELIVERY_ATTEMPTS {
            if !self.warned_cap {
                self.warned_cap = true;
                tracing::warn!(
                    mailbox = %self.boot_name,
                    attempts = self.attempts,
                    "gamepad channel delivery cap reached — no further handles will be duplicated"
                );
            }
            return;
        }
        // Throttle: asking costs a device open + an IOCTL, and this runs on the pad service pump.
        if self
            .last_probe
            .is_some_and(|t| t.elapsed() < PROOF_PROBE_INTERVAL)
        {
            return;
        }
        self.last_probe = Some(Instant::now());

        let Some(pid) = self.resolve_driver_pid() else {
            return;
        };
        if pid == self.last_seen_pid {
            return; // already tried this one and it failed — don't spin on it
        }
        self.last_seen_pid = pid;
        self.attempts += 1;
        match self.deliver_to(pid) {
            Ok((seq, process)) => {
                self.delivered = Some(Delivered { pid, process });
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
                    "sealed gamepad channel delivery failed"
                );
            }
        }
    }

    /// Which process to hand this pad's DATA section to.
    ///
    /// The answer comes from the DEVNODE ([`channel_proof`]), never from the bootstrap mailbox. That
    /// is the whole security property: the mailbox is writable by LocalService, so anything in it —
    /// including `driver_pid` — is attacker-choosable, whereas only the driver PnP actually bound to
    /// the device we created can answer that device's I/O. See the module docs for the attack this
    /// closes.
    fn resolve_driver_pid(&mut self) -> Option<u32> {
        let Some((instance_id, transport)) = self.devnode.clone() else {
            return self.unproven_mailbox_pid("this pad has no SwDeviceCreate devnode to ask");
        };
        match channel_proof::query(&instance_id, transport, self.pad_index) {
            Ok(pid) => {
                self.proof_failures = 0;
                Some(pid)
            }
            Err(e) => {
                self.proof_failures += 1;
                // Chatty at debug (the driver simply may not have started yet); one warn once it is
                // clearly not coming, so a dead pad names its own cause instead of just going quiet.
                // `DriverAttach` prints the operator-facing remedy separately.
                if self.proof_failures == PROOF_FAILURES_BEFORE_WARN {
                    tracing::warn!(
                        mailbox = %self.boot_name,
                        devnode = %instance_id,
                        error = %format!("{e:#}"),
                        "the pad's devnode has not answered a channel proof — the host will NOT \
                         hand the DATA section to a pid it cannot verify, so this pad stays \
                         unattached (an old driver? reinstall: punktfunk-host.exe driver install \
                         --gamepad)"
                    );
                } else {
                    tracing::debug!(
                        mailbox = %self.boot_name,
                        attempt = self.proof_failures,
                        error = %format!("{e:#}"),
                        "no channel proof yet"
                    );
                }
                None
            }
        }
    }

    /// The pre-v3 behaviour — trust the mailbox's `driver_pid` — for the one case that has no
    /// devnode to ask: an out-of-band (`devgen`) devnode created outside `SwDeviceCreate`. Refused
    /// unless [`TRUST_MAILBOX_ENV`] is set, because this is exactly the trust the security review
    /// removed: any LocalService principal can write that field and be handed the pad's live input
    /// surface.
    fn unproven_mailbox_pid(&mut self, why: &str) -> Option<u32> {
        if std::env::var_os(TRUST_MAILBOX_ENV).is_none() {
            if !self.warned_unproven {
                self.warned_unproven = true;
                tracing::warn!(
                    mailbox = %self.boot_name,
                    reason = why,
                    "cannot ask this pad's driver for a channel proof — REFUSING to deliver the \
                     DATA section (the mailbox pid is not trustworthy: any local service can write \
                     it). Set {TRUST_MAILBOX_ENV}=1 to accept the old, unverified handshake on a \
                     driver bring-up box."
                );
            }
            return None;
        }
        let pid = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_pid));
        if pid == 0 {
            return None;
        }
        if !self.warned_unproven {
            self.warned_unproven = true;
            tracing::warn!(
                mailbox = %self.boot_name,
                reason = why,
                "delivering this pad channel on the UNVERIFIED mailbox pid — a local service that \
                 wins the startup race can redirect this pad's input section (forged gamepad input \
                 + a read of pad state). Documented residual; the virtual MOUSE and the XUSB pad are \
                 both proved."
            );
        }
        Some(pid)
    }

    /// Duplicate the DATA section into `pid`'s handle table (after verifying it is a genuine
    /// WUDFHost) and publish the handle value + owning pid, bumping `handle_seq` LAST. The driver
    /// adopts the handle by consuming the delivery; an unconsumed duplicate dies with the target
    /// process (nothing to reap — there is no fallible step after the duplication).
    ///
    /// Returns `(handle_seq, process)` — the process handle is RETAINED by the caller so the
    /// one-delivery rule in [`Self::pump`] can tell "our driver crashed and UMDF restarted it" from
    /// "someone else is claiming this channel" on the handle rather than on a reusable pid.
    fn deliver_to(&self, pid: u32) -> Result<(u32, OwnedHandle)> {
        // SAFETY: plain FFI; the handle (checked by `?`) is owned solely here and moved into the
        // `OwnedHandle` (single owner, closes on drop); `verify_is_wudfhost` borrows it for the
        // synchronous check and forms no lasting alias. `SYNCHRONIZE` is requested so the retained
        // handle doubles as the incumbent-liveness probe ([`Delivered::exited`]) — the same thing the
        // frame channel's `ChannelBroker` asks for.
        let process = unsafe {
            let h = OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .context("OpenProcess(PROCESS_DUP_HANDLE) on the mailbox-reported pid")?;
            let process = OwnedHandle::from_raw_handle(h.0 as _);
            pf_capture::verify_is_wudfhost(
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
        Ok((seq, process))
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
                if self.delivered.is_none() {
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

/// Context for the `SwDeviceCreate` completion callback: an event to signal, the HRESULT it reports,
/// and the PnP instance id PnP assigned (captured for devnode health diagnostics). Shared by every
/// Windows companion backend (XUSB / DualSense / DS4): each `create_swdevice` builds one, hands it to
/// `SwDeviceCreate` alongside [`sw_create_cb`], and reads [`instance_id`](Self::instance_id) once the
/// callback has signalled.
#[repr(C)]
pub(super) struct SwCreateCtx {
    pub(super) event: HANDLE,
    pub(super) result: HRESULT,
    pub(super) instance_id: [u16; 128],
}

/// `SwDeviceCreate` fires this once PnP has enumerated the device; stash the result and wake the
/// creator, which blocks on the event (so there's no concurrent access to `*ctx`).
pub(super) unsafe extern "system" fn sw_create_cb(
    _dev: HSWDEVICE,
    result: HRESULT,
    ctx: *const c_void,
    id: PCWSTR,
) {
    if !ctx.is_null() {
        // SAFETY: ctx is the &mut SwCreateCtx the creator passed; it outlives this callback (the
        // creator blocks on the event). `id` is a NUL-terminated string for the callback's duration.
        unsafe {
            let c = ctx as *mut SwCreateCtx;
            (*c).result = result;
            if !id.is_null() {
                for i in 0..(*c).instance_id.len() - 1 {
                    let ch = *id.0.add(i);
                    (*c).instance_id[i] = ch;
                    if ch == 0 {
                        break;
                    }
                }
            }
            let _ = SetEvent((*c).event);
        }
    }
}

impl SwCreateCtx {
    pub(super) fn instance_id(&self) -> Option<String> {
        let len = self.instance_id.iter().position(|&c| c == 0)?;
        (len > 0).then(|| String::from_utf16_lossy(&self.instance_id[..len]))
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
    ///
    /// Runs on its own thread and returns immediately. The caller is the session's pad service
    /// thread — the one feeding input and rumble — and everything below is slow: the driver-store
    /// check waits up to [`INVENTORY_WAIT`] for a `pnputil` enumeration that can take tens of
    /// seconds, and the devnode lookup is a synchronous PnP call. Blocking there stalled input for
    /// up to two seconds *per unattached pad* (the wait is a deadline, not a one-off: while the
    /// enumeration is still outstanding every pad pays it again), at exactly the moment a session
    /// is already going wrong. Diagnostics must never be able to hurt the thing they diagnose.
    ///
    /// Off the hot path the wait also stops being a compromise — it can afford to be patient and
    /// report what it actually found rather than "still enumerating".
    fn diagnose(&self) {
        let (driver, inf, driver_log) = (self.driver, self.inf, self.driver_log);
        let shm_name = self.shm_name.clone();
        let instance_id = self.instance_id.clone();
        std::thread::Builder::new()
            .name("pf-driver-diagnose".into())
            .spawn(move || diagnose_blocking(driver, inf, driver_log, &shm_name, instance_id))
            .ok();
    }
}

/// The body of [`DriverAttach::diagnose`], on its own thread. Split out rather than inlined into
/// the closure so the blocking calls stay visible as blocking.
fn diagnose_blocking(
    driver: &'static str,
    inf: &'static str,
    driver_log: &'static str,
    shm_name: &str,
    instance_id: Option<String>,
) {
    let store = match driver_store_has(inf) {
        Some(true) => "driver package present in the driver store",
        Some(false) => {
            "driver package NOT in the driver store — run: punktfunk-host.exe driver install --gamepad"
        }
        None => "driver store could not be queried (pnputil failed or still enumerating)",
    };
    let devnode = match &instance_id {
        Some(id) => devnode_status_line(id),
        None => "no per-session devnode (SwDeviceCreate failed earlier — see the warning above)"
            .to_string(),
    };
    tracing::warn!(
        driver,
        shm = %shm_name,
        grace_secs = ATTACH_GRACE.as_secs(),
        store,
        devnode = %devnode,
        driver_log,
        "gamepad driver has not attached to the shared section — the virtual pad exists but no \
         driver is serving it (games will not see it); an old (pre-sealed-channel) driver also \
         reads as not-attached: update with punktfunk-host.exe driver install --gamepad \
         (driver_log is only written by debug driver builds, or with the PFXUSB_DEBUG_LOG / \
         PFGAMEPAD_DEBUG_LOG / PFMOUSE_DEBUG_LOG system env var set + the device restarted)"
    );
}

/// How long [`driver_store_inventory`] waits for the background pnputil query before reporting
/// without it. Only [`diagnose_blocking`] waits, and that has a thread to itself, so this is
/// generous: pnputil routinely takes longer than a couple of seconds on a busy driver store, and
/// the old two-second budget — chosen to limit the damage while this ran on the pad service thread
/// — meant the diagnosis usually gave up and printed "still enumerating", which is the one answer
/// that helps nobody. Nothing waits on this thread, so patience costs only a late log line.
const INVENTORY_WAIT: Duration = Duration::from_secs(30);

/// Driver-store inventory (`pnputil /enum-drivers`), lower-cased, fetched once per process — only
/// consulted on the failure path, so the subprocess cost never hits a healthy session. The query
/// runs on its OWN thread: pnputil can block for tens of seconds on a busy/wedged driver store,
/// and this keeps one wedged query from being re-run per pad. `None` = not available yet (query
/// still running past [`INVENTORY_WAIT`]) or
/// failed; a query that outlives [`INVENTORY_WAIT`] still lands in the cache for later reports.
fn driver_store_inventory() -> Option<&'static str> {
    static INV: OnceLock<String> = OnceLock::new();
    static SPAWN: std::sync::Once = std::sync::Once::new();
    SPAWN.call_once(|| {
        std::thread::spawn(|| {
            // Resolve pnputil by full System32 path — the host runs as SYSTEM and must not trust
            // PATH / the CreateProcess search (which checks the launching EXE's own dir first), or
            // a planted `pnputil.exe` beside the host binary would run elevated (security-review
            // 2026-07-17).
            let pnputil = std::env::var("SystemRoot")
                .map(|r| format!(r"{r}\System32\pnputil.exe"))
                .unwrap_or_else(|_| "pnputil.exe".to_string());
            let inv = std::process::Command::new(&pnputil)
                .arg("/enum-drivers")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
                .unwrap_or_default();
            let _ = INV.set(inv);
        });
    });
    let deadline = Instant::now() + INVENTORY_WAIT;
    loop {
        if let Some(inv) = INV.get() {
            return Some(inv);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether the driver store holds `inf` (e.g. `pf_xusb.inf`). `None` = pnputil unavailable,
/// failed, or still enumerating.
fn driver_store_has(inf: &str) -> Option<bool> {
    let inv = driver_store_inventory()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The one-delivery rule ([`PadChannel::pump`]) rests entirely on [`Delivered::exited`] reading
    /// the retained HANDLE rather than the pid, and both of its answers are load-bearing: "alive"
    /// is what refuses a LocalService takeover of a live pad channel, and "exited" is what still
    /// lets UMDF's restart-the-host-after-a-driver-crash path re-deliver. Neither half is
    /// observable from the pad path without a real WUDFHost, so pin the primitive itself.
    #[test]
    fn delivered_exited_tracks_the_process_object_not_the_pid() {
        // A child that parks long enough to be observed alive (no console needed, unlike `pause`).
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a parked child");
        let pid = child.id();
        // SAFETY: plain FFI on our own child's pid; the returned handle is owned solely by the
        // `OwnedHandle` built from it (single owner, closes on drop).
        let process = unsafe {
            let h = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .expect("OpenProcess(SYNCHRONIZE) on our own child");
            OwnedHandle::from_raw_handle(h.0 as _)
        };
        let delivered = Delivered { pid, process };
        assert!(
            !delivered.exited(),
            "a running target must read as ALIVE — this is what refuses a channel takeover"
        );
        child.kill().expect("kill the child");
        // `wait` reaps, and only returns once the process has really exited — so the handle is
        // signaled by the time we look.
        child.wait().expect("reap the child");
        assert!(
            delivered.exited(),
            "an exited target must read as GONE — this is what lets a crashed driver re-attach"
        );
    }
}
